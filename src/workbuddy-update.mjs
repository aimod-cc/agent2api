/**
 * 软件更新 —— GitHub Release 检测与安装包下载
 *
 * 为什么检测/下载放在 Node 后端而不是 Tauri 壳：
 *   壳侧 reqwest 为省掉 TLS 依赖关掉了默认特性（只能访问本机网关的明文 HTTP），
 *   发不出 GitHub 的 HTTPS 请求。因此这里负责联网，壳只做两件事：
 *   把「当前版本」传进来比较、把下载好的安装包跑起来。
 *
 * 出口策略：直连优先，失败后若 Clash Verge 混合端口可用则重试一次。
 *   国内访问 GitHub 常需代理，但用户不一定给账号配了代理 ——
 *   直连能通就别打扰，通不了再借 Clash 的混合端口兜一次。
 *
 * 安全：下载地址只接受 github.com / *.githubusercontent.com，
 *   避免这个接口被当成任意 URL 的下载器（SSRF / 被拿去拉不可信文件）。
 */

import { createWriteStream, existsSync, mkdirSync, rmSync, statSync } from 'node:fs';
import { join, basename } from 'node:path';
import { Readable } from 'node:stream';

import { CLASH_MIXED_UID, proxyFetch, proxyErrorDetail, resolveAccountProxy } from './workbuddy-proxy.mjs';

/** 默认仓库（本项目的上游）；fork 后可自行覆盖 */
const DEFAULT_REPO = 'aimod-cc/workbuddy-proxy';
const GITHUB_API = 'https://api.github.com';

/** 允许下载的域名：Release 资产实际会落在 github.com 或 *.githubusercontent.com */
const ALLOWED_HOSTS = ['github.com', 'www.github.com', 'objects.githubusercontent.com', 'codeload.github.com'];
const ALLOWED_HOST_SUFFIX = '.githubusercontent.com';

/** 安装包体积上限：只用于拦明显异常的响应，避免把磁盘写满 */
const MAX_INSTALLER_BYTES = 600 * 1024 * 1024;

/** 检测与下载的请求超时（下载本身不设总超时，只在建连阶段给足时间） */
const REQUEST_TIMEOUT_MS = 30_000;

export class UpdateError extends Error {
  constructor(message, { statusCode = 502 } = {}) {
    super(message);
    this.name = 'UpdateError';
    this.statusCode = statusCode;
  }
}

// ─── 版本比较 ───────────────────────────────────────────────

/**
 * 解析版本号为数字段。接受 `1.2.3` / `v1.2.3` / `1.2.3-beta.1`，
 * 非语义化版本返回 null（此时不做「有新版本」判断，只展示原文）。
 */
export function parseVersion(text) {
  const matched = String(text ?? '').trim().replace(/^v/i, '').match(/^(\d+)\.(\d+)\.(\d+)/);
  if (!matched) return null;
  return [Number(matched[1]), Number(matched[2]), Number(matched[3])];
}

/** 语义化比较：a > b 返回 1，相等 0，a < b 返回 -1；无法解析时返回 null */
export function compareVersions(a, b) {
  const left = parseVersion(a);
  const right = parseVersion(b);
  if (!left || !right) return null;
  for (let i = 0; i < 3; i += 1) {
    if (left[i] > right[i]) return 1;
    if (left[i] < right[i]) return -1;
  }
  return 0;
}

// ─── 出口选择 ───────────────────────────────────────────────

/**
 * 候选出口列表：直连优先，Clash 混合端口作为兜底。
 * Clash 未安装或未启用混合端口时只有一个候选，行为与纯直连一致。
 */
function resolveEgressCandidates() {
  const candidates = [{ label: '直连', proxy: null }];
  try {
    const mixed = resolveAccountProxy({ source: 'clash', listenerUid: CLASH_MIXED_UID });
    if (mixed && !mixed.error && mixed.port) {
      candidates.push({ label: mixed.label || 'Clash 混合端口', proxy: mixed });
    }
  } catch { /* Clash 配置不可读：只用直连 */ }
  return candidates;
}

/**
 * 依次尝试各出口，返回第一个成功的响应（以及所用出口的描述）。
 * 只有「网络层失败」才切换出口；HTTP 状态码类错误直接交给调用方判断，
 * 否则 404（仓库没有 Release）会被误当成「需要换代理」而白试一轮。
 */
async function fetchWithEgress(url, init, { log, verbose } = {}) {
  const candidates = resolveEgressCandidates();
  let lastError = null;
  for (const candidate of candidates) {
    try {
      const response = await proxyFetch(url, init, candidate.proxy);
      if (candidates.length > 1 && candidate.proxy) {
        verbose?.('[Update]', `经 ${candidate.label} 访问成功`);
      }
      return { response, egress: candidate.label };
    } catch (error) {
      lastError = error;
      verbose?.('[Update]', `经 ${candidate.label} 访问失败: ${proxyErrorDetail(error)}`);
    }
  }
  throw new UpdateError(`无法连接 GitHub：${proxyErrorDetail(lastError)}`, { statusCode: 502 });
}

/** 域名白名单校验：只允许 https 且落在 GitHub 资产域名下 */
function assertDownloadable(url) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    throw new UpdateError('下载地址不是合法 URL', { statusCode: 400 });
  }
  if (parsed.protocol !== 'https:') {
    throw new UpdateError('下载地址必须是 https', { statusCode: 400 });
  }
  const host = parsed.hostname.toLowerCase();
  const allowed = ALLOWED_HOSTS.includes(host) || host.endsWith(ALLOWED_HOST_SUFFIX);
  if (!allowed) {
    throw new UpdateError(`下载地址域名不在允许列表内: ${host}`, { statusCode: 400 });
  }
  return parsed;
}

/** 从 Release 资产里挑安装包：优先名字带 setup 的 exe，其次任意 exe */
function pickInstaller(assets) {
  const list = Array.isArray(assets) ? assets : [];
  const exes = list.filter(item => /\.exe$/i.test(String(item?.name || '')));
  if (!exes.length) return null;
  const installer = exes.find(item => /setup|install/i.test(String(item.name))) || exes[0];
  return {
    name: String(installer.name || ''),
    size: Number(installer.size) || 0,
    url: String(installer.browser_download_url || ''),
  };
}

/** 文件名安全化：只取基名，去掉路径分隔符与可疑字符 */
function safeFileName(name) {
  const base = basename(String(name || '')).replace(/[\\/:*?"<>|]/g, '_').trim();
  return base || 'workbuddy-update.exe';
}

// ─── 更新管理器 ─────────────────────────────────────────────

export function createUpdateManager({ directory, log = () => {}, verbose = () => {}, repo = null } = {}) {
  const repository = String(repo || process.env.WORKBUDDY_UPDATE_REPO || DEFAULT_REPO).trim();
  const downloadDir = join(directory, 'updates');

  /** 下载任务状态（同一时刻只允许一个，重复请求直接复用进行中的任务） */
  let task = null;

  /** 最近一次拉到的 Release（内存缓存，避免重复打 GitHub） */
  let latest = null;

  function repoApi(path) {
    return `${GITHUB_API}/repos/${repository}${path}`;
  }

  function githubHeaders() {
    // GitHub API 要求带 UA；配了 token 则限额更高（可选，不配也能用）
    const token = (process.env.WORKBUDDY_GITHUB_TOKEN || process.env.GITHUB_TOKEN || '').trim();
    const headers = {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'workbuddy-local-proxy',
      'X-GitHub-Api-Version': '2022-11-28',
    };
    if (token) headers.Authorization = `Bearer ${token}`;
    return headers;
  }

  /**
   * 检查是否有新版本。
   * currentVersion 由桌面端传入（后端不知道自己被哪个壳打包）；
   * 缺省时只回报最新版本，不做「是否有更新」的判断。
   */
  async function check({ currentVersion = '' } = {}) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(new Error('请求超时')), REQUEST_TIMEOUT_MS);
    try {
      await refreshLatest({ controller });
    } finally {
      clearTimeout(timer);
    }
    return buildCheckResult(currentVersion);
  }

  /** 拉取最新 Release；404 表示仓库还没有发布任何版本，按「无更新」处理 */
  async function refreshLatest({ controller }) {
    const { response } = await fetchWithEgress(repoApi('/releases/latest'), {
      headers: githubHeaders(),
      signal: controller?.signal,
    }, { log, verbose });

    if (response.status === 404) {
      latest = null;
      return;
    }
    if (response.status === 403 || response.status === 429) {
      throw new UpdateError(
        'GitHub 接口访问受限（可能是请求频率超限）。稍后再试，或设置 WORKBUDDY_GITHUB_TOKEN 提高限额',
        { statusCode: 502 },
      );
    }
    if (!response.ok) {
      throw new UpdateError(`GitHub 返回 HTTP ${response.status}`, { statusCode: 502 });
    }

    const payload = await response.json();
    latest = {
      tag: String(payload.tag_name || '').replace(/^v/i, ''),
      name: String(payload.name || payload.tag_name || ''),
      notes: String(payload.body || '').slice(0, 4000),
      publishedAt: String(payload.published_at || ''),
      pageUrl: String(payload.html_url || `https://github.com/${repository}/releases`),
      prerelease: payload.prerelease === true,
      asset: pickInstaller(payload.assets),
    };
  }

  function buildCheckResult(currentVersion) {
    const version = String(currentVersion || '').trim();
    const comparison = latest && version ? compareVersions(latest.tag, version) : null;
    const asset = latest?.asset && latest.asset.url ? latest.asset : null;
    return {
      currentVersion: version || null,
      latestVersion: latest?.tag || null,
      hasUpdate: comparison === null ? null : comparison > 0,
      // 无法解析版本号时不谎报「有更新」，界面按 null 显示为「无法比较」
      comparison: comparison === null ? null : comparison > 0 ? 'newer' : comparison < 0 ? 'older' : 'same',
      releaseName: latest?.name || null,
      notes: latest?.notes || null,
      publishedAt: latest?.publishedAt || null,
      pageUrl: latest?.pageUrl || null,
      prerelease: latest?.prerelease === true,
      asset,
      repository,
      downloadSupported: !!asset,
      downloadDir,
      task: taskSummary(),
    };
  }

  function taskSummary() {
    if (!task) return null;
    return {
      active: task.active,
      done: task.done,
      canceled: task.canceled,
      error: task.error,
      filename: task.filename,
      path: task.path,
      received: task.received,
      total: task.total,
      percent: task.total > 0 ? Math.min(100, Math.round((task.received / task.total) * 100)) : 0,
    };
  }

  /** 下载进度（前端轮询） */
  function getProgress() {
    return taskSummary();
  }

  /** 取消进行中的下载并清理半截文件 */
  function cancelDownload() {
    if (!task || !task.active) return { canceled: false };
    task.canceled = true;
    task.controller.abort(new Error('已取消'));
    return { canceled: true };
  }

  /**
   * 启动安装包下载。同一时刻只跑一个：已有进行中的任务时直接返回它，
   * 避免用户连点导致多个 200MB 的安装包同时落盘。
   */
  async function startDownload({ url, name } = {}) {
    if (task?.active) return taskSummary();

    const target = assertDownloadable(String(url || ''));
    const filename = safeFileName(name || target.pathname.split('/').pop());
    mkdirSync(downloadDir, { recursive: true });
    const filePath = join(downloadDir, filename);

    // 同版本重复下载时直接覆盖，不留一堆历史安装包
    if (existsSync(filePath)) {
      try { rmSync(filePath, { force: true }); } catch { /* 占用中则让下面的写入自己报错 */ }
    }

    task = {
      active: true, done: false, canceled: false, error: null,
      filename, path: filePath, received: 0, total: 0,
      controller: new AbortController(),
    };

    // 后台跑：接口立刻返回，进度由前端轮询
    void runDownload(target, filePath);
    return taskSummary();
  }

  async function runDownload(target, filePath) {
    const current = task;
    let written = null;
    try {
      const { response } = await fetchWithEgress(target.href, {
        headers: { 'User-Agent': 'workbuddy-local-proxy', Accept: 'application/octet-stream' },
        signal: current.controller.signal,
      }, { log, verbose });

      if (!response.ok) throw new UpdateError(`下载失败：GitHub 返回 HTTP ${response.status}`);
      const declared = Number(response.headers.get('content-length')) || 0;
      if (declared > MAX_INSTALLER_BYTES) {
        throw new UpdateError(`安装包体积异常（${Math.round(declared / 1024 / 1024)} MB），已中止`);
      }
      current.total = declared;
      log('[Update]', `开始下载 ${current.filename}${declared ? `（${(declared / 1024 / 1024).toFixed(1)} MB）` : ''}`);

      const body = Readable.fromWeb(response.body);
      written = createWriteStream(filePath);
      await new Promise((resolve, reject) => {
        body.on('data', chunk => {
          current.received += chunk.length;
          if (current.received > MAX_INSTALLER_BYTES) {
            reject(new UpdateError('安装包超过体积上限，已中止'));
            body.destroy();
            return;
          }
          // 未声明 content-length 时用已收字节估算总量，保证进度条不是死的
          if (!current.total) current.total = current.received;
        });
        body.on('error', reject);
        written.on('error', reject);
        written.on('finish', resolve);
        body.pipe(written);
      });

      // 磁盘写入完成后再核一次大小，避免「进度 100% 但文件不完整」
      const size = existsSync(filePath) ? statSync(filePath).size : 0;
      if (declared && size !== declared) {
        throw new UpdateError(`安装包不完整（期望 ${declared} 字节，实际 ${size} 字节）`);
      }
      current.done = true;
      current.received = size;
      current.total = size;
      log('[Update]', `✅ 安装包已就绪: ${filePath}（${(size / 1024 / 1024).toFixed(1)} MB）`);
    } catch (error) {
      // 取消与失败都要清掉半截文件：留着会被误当成可运行的安装包
      try { written?.destroy(); } catch { /* 已关闭 */ }
      try { if (existsSync(filePath)) rmSync(filePath, { force: true }); } catch { /* 占用中就算了 */ }
      current.path = null;
      if (current.canceled) {
        log('[Update]', '下载已取消');
      } else {
        current.error = error instanceof UpdateError ? error.message : `下载失败：${proxyErrorDetail(error)}`;
        log('[Update]', `❌ ${current.error}`);
      }
    } finally {
      current.active = false;
    }
  }

  return { check, getProgress, startDownload, cancelDownload, repository };
}
