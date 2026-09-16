/**
 * WorkBuddy 出网代理支持 —— Clash Verge 同步 + 按账号代理 + 出口测试
 *
 * 背景：默认所有请求都直连上游（copilot.tencent.com / www.workbuddy.ai）。
 * 当本地网络需要经代理才能访问上游时，可以给每个账号单独指定出口，
 * 默认「无代理」（保持原有直连行为，零影响）。
 *
 * 账号记录里的 proxy 字段形态（null 表示无代理）：
 *   { source: 'clash',  listenerUid: '__mixed__' | '<Clash 节点名>' }
 *   { source: 'custom', protocol: 'http' | 'socks5', host, port, username?, password? }
 *
 * Clash Verge 侧的代理项是「严格镜像」：端口只在 Clash Verge 里改，
 * 这里每次都从 verge.yaml 实时读取，账号只记 listenerUid。
 * Clash 里删掉了某个监听器，引用它的账号会解析失败并回退直连（记日志提醒）。
 *
 * 出网实现：Node 内置 fetch 不接受外部 dispatcher，必须用 npm undici 的
 * fetch + ProxyAgent/Socks5ProxyAgent。dispatcher 按出口缓存复用，
 * 否则每个请求都会新建一条到代理的连接。
 */

import { existsSync, readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { parse as parseYaml } from 'yaml';
import { Agent, ProxyAgent, Socks5ProxyAgent, fetch as undiciFetch } from 'undici';

/** Clash Verge 全局混合端口在账号配置里的保留 uid（与真实节点名不会冲突） */
export const CLASH_MIXED_UID = '__mixed__';

const CLASH_APP_ID = 'io.github.clash-verge-rev.clash-verge-rev';
const MAX_HOST_LENGTH = 255;
const MAX_USER_LENGTH = 200;
const MAX_LABEL_LENGTH = 100;

/** 上游是 SSE 长连接：header 超时给足，body 超时交给调用方的 signal 控制 */
const CONNECT_TIMEOUT_MS = 30_000;
const HEADERS_TIMEOUT_MS = 600_000;

export class ProxyConfigError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ProxyConfigError';
    this.statusCode = 400;
  }
}

// ─── Clash Verge 配置读取 ───────────────────────────────────

export function clashConfigDir() {
  const candidates = [];
  const appdata = process.env.APPDATA?.trim();
  if (appdata) candidates.push(join(appdata, CLASH_APP_ID));
  candidates.push(join(homedir(), 'AppData', 'Roaming', CLASH_APP_ID));
  candidates.push(join(homedir(), '.config', CLASH_APP_ID));
  return candidates.find(dir => existsSync(join(dir, 'verge.yaml'))) || null;
}

function readYaml(path) {
  try {
    if (!existsSync(path)) return null;
    const parsed = parseYaml(readFileSync(path, 'utf8'));
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function isValidPort(value) {
  const port = Number(value);
  return Number.isInteger(port) && port >= 1 && port <= 65535;
}

/**
 * 读取 Clash Verge 的监听器快照。
 * 返回 { available, dir, mixedPort, socksPort, httpPort, listeners[], currentProfileUid, error }。
 * Clash 未安装或配置不可读时 available=false，不抛异常。
 */
export function readClashVergeConfig() {
  const dir = clashConfigDir();
  if (!dir) {
    return { available: false, dir: null, mixedPort: null, listeners: [], currentProfileUid: null, error: '未找到 Clash Verge 配置' };
  }
  const verge = readYaml(join(dir, 'verge.yaml'));
  if (!verge) {
    return { available: false, dir, mixedPort: null, listeners: [], currentProfileUid: null, error: 'verge.yaml 解析失败' };
  }

  const raw = Array.isArray(verge.verge_mixed_listeners) ? verge.verge_mixed_listeners : [];
  const listeners = [];
  for (const item of raw) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) continue;
    const proxy = typeof item.proxy === 'string' ? item.proxy.trim() : '';
    if (!proxy || !isValidPort(item.port)) continue;
    const uid = typeof item.uid === 'string' && item.uid.trim() ? item.uid.trim() : `${item.port}:${proxy}`;
    listeners.push({
      uid,
      name: (typeof item.name === 'string' && item.name.trim()) || proxy,
      proxy,
      port: Number(item.port),
      profileUid: typeof item.profile_uid === 'string' && item.profile_uid.trim() ? item.profile_uid.trim() : null,
      enabled: item.enabled !== false,
    });
  }

  const profiles = readYaml(join(dir, 'profiles.yaml'));
  return {
    available: true,
    dir,
    mixedPort: isValidPort(verge.verge_mixed_port) ? Number(verge.verge_mixed_port) : null,
    socksPort: isValidPort(verge.verge_socks_port) ? Number(verge.verge_socks_port) : null,
    httpPort: isValidPort(verge.verge_port) ? Number(verge.verge_port) : null,
    listeners,
    currentProfileUid: profiles && typeof profiles.current === 'string' && profiles.current.trim()
      ? profiles.current.trim()
      : null,
    error: null,
  };
}

/**
 * 监听器快照缓存：每次解析账号代理都要读 verge.yaml，桌面端列表也会频繁刷新，
 * 加一层短 TTL 缓存避免请求热路径上反复解析 YAML（Clash 改端口最多延迟 3 秒生效）。
 */
const CLASH_CACHE_TTL_MS = 3000;
let clashCache = { at: 0, snapshot: null };

export function clashSnapshot({ ttlMs = CLASH_CACHE_TTL_MS, force = false } = {}) {
  const now = Date.now();
  if (!force && clashCache.snapshot && now - clashCache.at < ttlMs) return clashCache.snapshot;
  clashCache = { at: now, snapshot: readClashVergeConfig() };
  return clashCache.snapshot;
}

/**
 * Clash 代理可选项（桌面端下拉用）：全局混合端口 + 各节点监听器。
 * profileActive 表示该监听器所属订阅就是当前激活订阅（端口实际生效）。
 */
export function clashProxyOptions(snapshot = clashSnapshot()) {
  if (!snapshot.available) {
    return { available: false, error: snapshot.error, dir: snapshot.dir, options: [] };
  }
  const options = [];
  if (snapshot.mixedPort) {
    options.push({
      uid: CLASH_MIXED_UID,
      name: `Clash 混合端口（按规则分流）`,
      port: snapshot.mixedPort,
      kind: 'mixed',
      enabled: true,
      profileActive: true,
    });
  }
  for (const listener of snapshot.listeners) {
    options.push({
      uid: listener.uid,
      name: listener.name,
      port: listener.port,
      kind: 'listener',
      enabled: listener.enabled,
      profileActive: listener.profileUid === null || listener.profileUid === snapshot.currentProfileUid,
    });
  }
  return { available: true, error: null, dir: snapshot.dir, options };
}

// ─── 账号代理配置归一与解析 ─────────────────────────────────

function cleanString(value, max) {
  return typeof value === 'string' ? value.trim().slice(0, max) : '';
}

/**
 * 归一账号代理配置（来自 API 的原始输入）。
 * 返回 null（无代理）或标准形态；非法输入抛 ProxyConfigError（→HTTP 400）。
 */
export function normalizeAccountProxy(input) {
  if (input === null || input === undefined || input === '') return null;
  if (typeof input !== 'object' || Array.isArray(input)) {
    throw new ProxyConfigError('代理配置必须是对象或 null');
  }
  // source 缺省时按字段推断，便于手工编辑 accounts.json
  const source = cleanString(input.source, 20).toLowerCase()
    || (input.listenerUid ? 'clash' : input.host ? 'custom' : '');
  if (!source) throw new ProxyConfigError('代理配置缺少 source（clash / custom）');

  if (source === 'clash') {
    const listenerUid = cleanString(input.listenerUid, MAX_LABEL_LENGTH);
    if (!listenerUid) throw new ProxyConfigError('缺少 Clash 监听器 uid');
    return { source: 'clash', listenerUid };
  }
  if (source !== 'custom') {
    throw new ProxyConfigError(`不支持的代理来源: ${source}`);
  }

  const protocol = cleanString(input.protocol, 10).toLowerCase() || 'http';
  if (protocol !== 'http' && protocol !== 'socks5') {
    throw new ProxyConfigError('代理协议只支持 http 或 socks5');
  }
  const host = cleanString(input.host, MAX_HOST_LENGTH);
  if (!host) throw new ProxyConfigError('缺少代理主机地址');
  if (!isValidPort(input.port)) throw new ProxyConfigError('代理端口必须是 1-65535 的整数');
  const proxy = {
    source: 'custom',
    protocol,
    host,
    port: Number(input.port),
    username: cleanString(input.username, MAX_USER_LENGTH),
    password: cleanString(input.password, MAX_USER_LENGTH),
    label: cleanString(input.label, MAX_LABEL_LENGTH),
  };
  if (!proxy.label) {
    proxy.label = `${protocol}://${host}:${proxy.port}`;
  }
  return proxy;
}

function buildProxyUrl(proxy) {
  const auth = proxy.username
    ? `${encodeURIComponent(proxy.username)}${proxy.password ? `:${encodeURIComponent(proxy.password)}` : ''}@`
    : '';
  const host = proxy.host.includes(':') && !proxy.host.startsWith('[') ? `[${proxy.host}]` : proxy.host;
  return `${proxy.protocol}://${auth}${host}:${proxy.port}`;
}

/**
 * 把账号里存的代理配置解析成可用的连接信息。
 * clash 类型从 Clash Verge 快照实时取端口，取不到时返回 { error }（调用方据此回退直连）。
 */
export function resolveAccountProxy(config, { clash = clashSnapshot() } = {}) {
  if (!config) return null;
  if (config.source === 'custom') {
    return {
      source: 'custom',
      protocol: config.protocol === 'socks5' ? 'socks5' : 'http',
      host: config.host,
      port: Number(config.port),
      username: config.username || '',
      password: config.password || '',
      label: config.label || `${config.protocol}://${config.host}:${config.port}`,
    };
  }
  if (config.source !== 'clash') {
    return { error: `不支持的代理来源: ${config.source}` };
  }
  if (!clash.available) {
    return { error: `Clash Verge 配置不可用${clash.error ? `（${clash.error}）` : ''}` };
  }
  if (config.listenerUid === CLASH_MIXED_UID) {
    if (!clash.mixedPort) return { error: 'Clash Verge 未启用混合端口' };
    return {
      source: 'clash',
      protocol: 'http',
      host: '127.0.0.1',
      port: clash.mixedPort,
      username: '',
      password: '',
      label: `Clash 混合端口 ${clash.mixedPort}`,
    };
  }
  const listener = clash.listeners.find(item => item.uid === config.listenerUid);
  if (!listener) {
    return { error: `Clash Verge 中找不到监听器「${config.listenerUid}」（可能已在 Clash 中删除）` };
  }
  if (!listener.enabled) {
    return { error: `Clash Verge 监听器「${listener.name}」已禁用` };
  }
  return {
    source: 'clash',
    protocol: 'http',
    host: '127.0.0.1',
    port: listener.port,
    username: '',
    password: '',
    label: `${listener.name}（:${listener.port}）`,
  };
}

// ─── dispatcher 缓存与出网 ──────────────────────────────────

/** key = `protocol://host:port`（不含凭证，避免把密码放进缓存键） */
const dispatchers = new Map();
const MAX_DISPATCHERS = 24;
let idleAgent = null;

function createDispatcher(proxy) {
  if (proxy.protocol === 'socks5') {
    return new Socks5ProxyAgent(buildProxyUrl(proxy));
  }
  return new ProxyAgent({
    uri: buildProxyUrl(proxy),
    connectTimeout: CONNECT_TIMEOUT_MS,
    headersTimeout: HEADERS_TIMEOUT_MS,
    bodyTimeout: 0,
  });
}

function destroyDispatcher(dispatcher) {
  try {
    dispatcher?.close?.();
  } catch { /* 关闭失败不影响后续请求 */ }
  try {
    dispatcher?.destroy?.();
  } catch { /* 同上 */ }
}

function idleDispatcher() {
  if (!idleAgent) {
    idleAgent = new Agent({
      connectTimeout: CONNECT_TIMEOUT_MS,
      headersTimeout: HEADERS_TIMEOUT_MS,
      bodyTimeout: 0,
    });
  }
  return idleAgent;
}

/** 取（或创建）某出口对应的 dispatcher；超出上限时关掉最久未用的 */
export function dispatchFor(proxy) {
  if (!proxy) return idleDispatcher();
  const key = `${proxy.protocol}://${proxy.host}:${proxy.port}`;
  const hit = dispatchers.get(key);
  if (hit) {
    // Map 保持插入序：命中时挪到末尾，末尾即最近使用
    dispatchers.delete(key);
    dispatchers.set(key, hit);
    return hit;
  }
  const created = createDispatcher(proxy);
  dispatchers.set(key, created);
  while (dispatchers.size > MAX_DISPATCHERS) {
    const oldestKey = dispatchers.keys().next().value;
    if (oldestKey === key) break;
    destroyDispatcher(dispatchers.get(oldestKey));
    dispatchers.delete(oldestKey);
  }
  return created;
}

/** 关闭所有缓存的 dispatcher（配置变更/退出时用） */
export function closeDispatchers() {
  for (const dispatcher of dispatchers.values()) destroyDispatcher(dispatcher);
  dispatchers.clear();
  destroyDispatcher(idleAgent);
  idleAgent = null;
}

/**
 * 带代理的 fetch。与 globalThis.fetch 同签名，额外接受解析好的代理信息：
 *   proxy 为 null/undefined → 直连（仍走 undici，保证流式与超时行为一致）
 * undici 的 fetch 才能接受 dispatcher —— Node 内置 fetch 会直接报 fetch failed。
 */
export function proxyFetch(url, init = {}, proxy = null) {
  return undiciFetch(url, { ...init, dispatcher: dispatchFor(proxy) });
}

/** 从错误链里取最能说明问题的原因（连接类错误常挂在 cause 上） */
export function proxyErrorDetail(error) {
  const parts = [];
  const seen = new Set();
  let current = error;
  while (current && typeof current === 'object' && !seen.has(current)) {
    seen.add(current);
    const code = String(current.code || '').trim();
    const message = String(current.message || '').trim();
    if (message) parts.push(code && !message.includes(code) ? `${message}（${code}）` : message);
    else if (code) parts.push(code);
    current = current.cause;
  }
  return parts.length ? parts.join(' → ') : String(error ?? '');
}

/**
 * 账号代理的展示描述（公开形态，不带密码）。
 * 解析成功后返回 { label, protocol, host, port, source, error }，
 * 无代理返回 null。解析失败时带上 error 让前端提示「已回退直连」。
 */
export function describeAccountProxy(config, { clash = clashSnapshot() } = {}) {
  if (!config) return null;
  const resolved = resolveAccountProxy(config, { clash });
  if (resolved?.error) {
    return { source: config.source || '', label: '解析失败', error: resolved.error, config };
  }
  return {
    source: resolved.source,
    protocol: resolved.protocol,
    host: resolved.host,
    port: resolved.port,
    label: resolved.label,
    error: null,
    config,
  };
}

// ─── 出口连通性测试 ────────────────────────────────────────

/** 出口 IP 查询服务：拿不到只当作附加信息缺失，不影响连通性结论 */
const IP_ECHO_URL = 'https://api.ipify.org?format=json';

async function readExitIp(proxy, timeoutMs) {
  try {
    const response = await proxyFetch(IP_ECHO_URL, { signal: AbortSignal.timeout(timeoutMs) }, proxy);
    if (!response.ok) return '';
    const text = await response.text().catch(() => '');
    try {
      return String(JSON.parse(text)?.ip || '');
    } catch {
      return text.trim().slice(0, 64);
    }
  } catch {
    return '';
  }
}

/**
 * 测试某个出口是否可用 —— 判据是「能否连上 WorkBuddy 上游」，而不是能否访问
 * 第三方站点：上游在国内可直连、国外站点常常恰恰相反，用第三方站点当判据
 * 会把能用的配置报成失败。
 *
 *   proxy 非空 → 经该代理访问上游；proxy 为空 → 直连访问上游
 *
 * 只要能拿到 HTTP 响应（含 401/404 这类未鉴权响应）就算通；出口 IP 是附加信息，
 * 单独查 ipify，查不到不代表出口不可用。
 * 返回 { success, status?, ip?, durationMs, error? }，不抛异常。
 */
export async function testProxyConnectivity(proxy, { timeoutMs = 15_000, targetUrl = null } = {}) {
  const started = Date.now();
  const url = targetUrl || 'https://copilot.tencent.com/v2/config';
  try {
    const response = await proxyFetch(url, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      signal: AbortSignal.timeout(timeoutMs),
    }, proxy);
    // 只关心「连得上」，不读响应体
    try { await response.body?.cancel?.(); } catch { /* 已结束 */ }
    const durationMs = Date.now() - started;
    const ip = await readExitIp(proxy, Math.min(timeoutMs, 6000));
    return { success: true, status: response.status, ip, durationMs };
  } catch (error) {
    const detail = proxyErrorDetail(error);
    return {
      success: false,
      durationMs: Date.now() - started,
      error: error?.name === 'TimeoutError' ? `连接超时（${Math.round(timeoutMs / 1000)} 秒）` : detail,
    };
  }
}
