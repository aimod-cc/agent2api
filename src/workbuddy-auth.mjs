#!/usr/bin/env node
/**
 * WorkBuddy 鉴权模块 — 登录态存储、无头登录、token 自动刷新
 *
 * 逆向自 WorkBuddy 桌面端 v5.3.14（app.asar → main/common.js，
 * ExternalLinkAuthenticationProvider）与其内置 CLI（cli/product.json）。
 *
 * 登录流程（与桌面端 createSession 逐步对齐）：
 *   ① POST {endpoint}/v2{prefixPath}/auth/state?platform=workbuddy   （匿名）
 *        → data.authUrl + data.state
 *   ② 浏览器打开 authUrl 完成登录
 *   ③ GET  {endpoint}/v2{prefixPath}/auth/token?state=<state>        （匿名，轮询）
 *        → data.accessToken / refreshToken / expiresIn / refreshExpiresIn
 *        未完成时上游返回 code=11217，需继续轮询
 *   ④ GET  {endpoint}/v2{prefixPath}/login/account?state=<state>     （Bearer，轮询）
 *        → data 为账号对象；未就绪时 code=12151
 *   ⑤ GET  {endpoint}/v2{prefixPath}/accounts                        （Bearer）
 *        → data.accounts[]
 *
 * token 续期：
 *   POST {endpoint}/v2{prefixPath}/auth/token/refresh   body {}
 *   headers: X-Refresh-Token: <refreshToken>
 *            X-Auth-Refresh-Source: plugin
 *
 * 关键区别（易错）：
 *   - 登录/账号接口带 prefixPath（国内版 /v2/plugin/...）
 *   - 计费/签到/LLM 接口不带 prefixPath（直接 /v2/...）
 *
 * 登录态持久化在 ~/.workbuddy-proxy/auth.json，结构与桌面端
 * FileAuthenticationStorage 落盘的 session 对象一致（auth + account + accounts）。
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import {
  DEFAULT_ENDPOINT,
  DEFAULT_PREFIX_PATH,
  WORKBUDDY_PLATFORM,
  RESPONSE_CODE_OK,
  SERVER_CODES,
  defaultPrefixPath,
  normalizeEndpoint,
  anonymousHeaders,
  DOMAINS,
  resolveEdition,
} from './workbuddy-endpoints.mjs';
import { proxyFetch } from './workbuddy-proxy.mjs';

/** token 临期刷新窗口：过期前 5 分钟视为需要刷新 */
const REFRESH_WINDOW_MS = 5 * 60 * 1000;

/** 登录轮询间隔与总超时（桌面端 SIGN_IN_FETCH_INTERVAL / SIGN_IN_PENDING_TIMEOUT） */
const LOGIN_POLL_INTERVAL_MS = 3000;
const LOGIN_TIMEOUT_MS = 5 * 60 * 1000;

export { DEFAULT_ENDPOINT, DEFAULT_PREFIX_PATH, WORKBUDDY_PLATFORM };

export class WorkBuddyAuthError extends Error {
  constructor(message, { statusCode, code } = {}) {
    super(message);
    this.name = 'WorkBuddyAuthError';
    this.statusCode = statusCode;
    this.upstreamCode = code;
  }
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

export function createWorkBuddyAuth({
  endpoint,
  prefixPath,
  platform,
  edition,
  directory,
  accountStore,
  log = () => {},
  verbose = () => {},
  fetchImpl = globalThis.fetch,
  httpFetch = null,
} = {}) {
  /**
   * 出网函数，签名 (url, init, proxy)：
   *   默认用 workbuddy-proxy 的 proxyFetch —— 它是 undici fetch，可按请求挂
   *   dispatcher（代理）；Node 内置 fetch 不支持该选项，所以不能用它。
   *   单测注入 fetchImpl 替身时以替身为准（第三个代理参数会被忽略）。
   */
  const sendRequest = (fetchImpl && fetchImpl !== globalThis.fetch)
    ? fetchImpl
    : (httpFetch || proxyFetch);

  /**
   * 请求上下文：{ baseUrl, prefix, platform, edition }。
   * 模块级默认上下文来自构造参数；多账号场景按账号记录构造
   * （国内版/国际版端点与 platform 不同）。
   * prefix 优先级：显式传入 > 显式版本的 prefixPath > 端点推断（保持旧行为）。
   */
  function makeContext({ endpoint: ep, prefixPath: pp, platform: pf, edition: ed } = {}) {
    const explicitEdition = ed !== undefined && ed !== null && ed !== '';
    const e = resolveEdition(ed);
    const resolvedEndpoint = normalizeEndpoint(ep || e.endpoint);
    const resolvedPrefix = pp !== undefined && pp !== null
      ? pp
      : (explicitEdition ? e.prefixPath : defaultPrefixPath(resolvedEndpoint));
    return {
      baseUrl: resolvedEndpoint,
      prefix: resolvedPrefix,
      platform: pf || e.platform,
      edition: e.id,
    };
  }

  /** 按账号会话构造上下文（账号记录里存有 endpoint/prefixPath/platform/edition） */
  function contextFromSession(session) {
    if (!session) return defaultContext;
    return makeContext({
      endpoint: session.endpoint,
      prefixPath: session.prefixPath,
      platform: session.platform,
      edition: session.edition,
    });
  }

  const defaultContext = makeContext({ endpoint, prefixPath, platform, edition });
  const dir = directory || join(homedir(), '.workbuddy-proxy');
  const authFile = join(dir, 'auth.json');

  if (typeof fetchImpl !== 'function') {
    throw new WorkBuddyAuthError('当前 Node 版本缺少全局 fetch，请使用 Node >= 18');
  }

  // ─── 登录态存储 ─────────────────────────────────────────

  function loadStoredSession() {
    try {
      if (!existsSync(authFile)) return null;
      const raw = JSON.parse(readFileSync(authFile, 'utf8'));
      if (!raw || !raw.auth || !raw.auth.accessToken) return null;
      return raw;
    } catch (error) {
      log('[Auth]', `读取登录态失败: ${error.message}`);
      return null;
    }
  }

  function persistSession(session) {
    mkdirSync(dir, { recursive: true });
    writeFileSync(authFile, JSON.stringify(session, null, 2), 'utf8');
  }

  function clearSession() {
    if (accountStore) {
      const entry = accountStore.getCurrentEntry();
      if (entry) accountStore.removeAccount(entry.id);
      return;
    }
    try {
      if (existsSync(authFile)) writeFileSync(authFile, JSON.stringify({}, null, 2), 'utf8');
    } catch (error) {
      log('[Auth]', `清理登录态失败: ${error.message}`);
    }
  }

  // ─── 上游管理接口基础请求 ───────────────────────────────

  /** 带 prefixPath 的鉴权接口 URL（/v2/plugin/...） */
  function authUrl(path, ctx = defaultContext) {
    return `${ctx.baseUrl}/v2${ctx.prefix}${path}`;
  }

  function api(path, { method = 'GET', body, headers = {}, withPrefix = true, proxy = null } = {}, ctx = defaultContext) {
    const url = withPrefix ? authUrl(path, ctx) : `${ctx.baseUrl}${path}`;
    const init = {
      method,
      headers: { Accept: 'application/json', ...headers },
    };
    if (body !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    // proxy 作为第三个参数传给 proxyFetch（undici dispatcher）；替身 fetchImpl 会忽略它
    return sendRequest(url, init, proxy);
  }

  /** 管理接口响应包为 { code, data }，code === 0 才算成功 */
  async function unwrap(response, apiName) {
    const text = await response.text();
    let payload;
    try {
      payload = text ? JSON.parse(text) : null;
    } catch {
      throw new WorkBuddyAuthError(`${apiName} 返回非 JSON 响应（HTTP ${response.status}）`, {
        statusCode: response.status,
      });
    }
    if (!response.ok) {
      const message = payload?.error?.message || payload?.message || payload?.msg || `HTTP ${response.status}`;
      throw new WorkBuddyAuthError(`${apiName} 失败: ${message}`, {
        statusCode: response.status,
        code: payload?.code,
      });
    }
    if (payload && typeof payload.code === 'number' && payload.code !== RESPONSE_CODE_OK) {
      throw new WorkBuddyAuthError(
        `${apiName} 失败: code=${payload.code} ${payload.message || payload.msg || ''}`.trim(),
        { statusCode: response.status, code: payload.code },
      );
    }
    return payload?.data;
  }

  // ─── 企业/域名头 ────────────────────────────────────────

  /**
   * 企业扩展头。WorkBuddy 桌面端 enterpriseHeaders() 总是带 X-Domain
   * （取 auth.domain，缺失时回退端点 authority）。
   */
  function enterpriseHeaders(auth, ctx = defaultContext) {
    const headers = {};
    const enterpriseId = auth?.account?.enterpriseId;
    if (enterpriseId) {
      headers['X-Enterprise-Id'] = enterpriseId;
      headers['X-Tenant-Id'] = enterpriseId;
    }
    const domain = auth?.domain || safeAuthority(ctx.baseUrl);
    if (domain) headers['X-Domain'] = domain;
    return headers;
  }

  function safeAuthority(url) {
    try {
      return new URL(url).host;
    } catch {
      return '';
    }
  }

  // ─── 无头登录 ───────────────────────────────────────────

  /**
   * 发起登录：POST auth/state 拿 authUrl + state，
   * 用户在浏览器完成登录后轮询 auth/token 直到拿到 token。
   * edition 指定国内版（cn）/国际版（intl），决定端点与 platform 参数。
   */
  async function loginInteractive({
    pollIntervalMs = LOGIN_POLL_INTERVAL_MS,
    timeoutMs = LOGIN_TIMEOUT_MS,
    onAuthUrl,
    signal,
    edition,
    endpoint: loginEndpoint,
  } = {}) {
    const ctx = (edition || loginEndpoint) ? makeContext({ edition, endpoint: loginEndpoint }) : defaultContext;
    const stateResp = await api(
      `/auth/state?platform=${encodeURIComponent(ctx.platform)}`,
      { method: 'POST', body: {}, headers: anonymousHeaders() },
      ctx,
    );
    const stateData = await unwrap(stateResp, 'auth/state');
    const state = stateData?.state || stateData?.authState;
    const authUrlValue = stateData?.authUrl;
    if (!state && !authUrlValue) {
      throw new WorkBuddyAuthError('auth/state 未返回 state/authUrl，无法发起登录');
    }
    if (authUrlValue && onAuthUrl) onAuthUrl(authUrlValue, state);

    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (signal?.aborted) throw new WorkBuddyAuthError('登录已取消');
      await sleep(pollIntervalMs);
      let tokenData = null;
      try {
        const resp = await api(`/auth/token?state=${encodeURIComponent(state)}`, {
          method: 'GET',
          headers: anonymousHeaders(),
        }, ctx);
        tokenData = await unwrap(resp, 'auth/token');
      } catch (error) {
        // 未完成登录时上游返回 code=11217（RetryFetchToken），轮询期间一律继续等待
        if (error?.upstreamCode === SERVER_CODES.RetryFetchToken) {
          verbose('[Auth]', '等待浏览器登录完成…');
          continue;
        }
        verbose('[Auth]', `登录轮询中: ${error.message}`);
        continue;
      }
      if (tokenData?.accessToken) {
        const session = await buildSessionFromToken(tokenData, { state, ctx });
        if (!session.account?.uid) {
          throw new WorkBuddyAuthError('登录成功但获取账号信息失败（缺少 uid），请重试');
        }
        if (accountStore) {
          const saved = accountStore.addAccount(session);
          log('[Auth]', `✅ 登录成功：${saved.name}（${saved.uid.slice(0, 8)}…），已加入账号列表`);
        } else {
          persistSession(session);
          log('[Auth]', `✅ 登录成功（账号 ${session.account?.uid || '未知'}），已写入 ${authFile}`);
        }
        return session;
      }
      verbose('[Auth]', '等待浏览器登录完成…');
    }
    throw new WorkBuddyAuthError(`登录轮询超时（${Math.round(timeoutMs / 60000)} 分钟）`);
  }

  /** 拿到 token 后拉账号：优先 /login/account?state=，再回退 /accounts */
  async function buildSessionFromToken(auth, { state, ctx = defaultContext } = {}) {
    const enriched = withExpiresAt({ ...auth });
    let account = {};
    let accounts = [];

    if (state) {
      try {
        account = (await fetchLoginAccount(enriched, state, ctx)) || {};
      } catch (error) {
        verbose('[Auth]', `login/account 拉取失败: ${error.message}`);
      }
    }
    try {
      accounts = await fetchAccounts(enriched, ctx);
    } catch (error) {
      log('[Auth]', `拉取账号列表失败（不影响登录）: ${error.message}`);
    }
    if (!account.uid) account = accounts.find(item => item.lastLogin) || accounts[0] || {};

    return {
      endpoint: ctx.baseUrl,
      prefixPath: ctx.prefix,
      platform: ctx.platform,
      edition: ctx.edition,
      auth: enriched,
      account,
      accounts,
      lastRefreshTime: Date.now(),
    };
  }

  /** ④ GET /v2{prefix}/login/account?state= —— 登录后账号详情 */
  async function fetchLoginAccount(auth, state, ctx = defaultContext) {
    const headers = {
      ...enterpriseHeaders(auth, ctx),
      Authorization: `Bearer ${auth.accessToken}`,
      'X-No-User-Id': 'true',
      'X-No-Enterprise-Id': 'true',
      'X-No-Department-Info': 'true',
    };
    const resp = await api(`/login/account?state=${encodeURIComponent(state)}`, { method: 'GET', headers }, ctx);
    return unwrap(resp, 'login/account');
  }

  function withExpiresAt(auth) {
    const now = Date.now();
    const expiresIn = Number(auth.expiresIn) || 0;
    const refreshExpiresIn = Number(auth.refreshExpiresIn) || 0;
    return {
      ...auth,
      expiresAt: auth.expiresAt || (expiresIn ? now + expiresIn * 1000 : 0),
      refreshExpiresAt: auth.refreshExpiresAt || (refreshExpiresIn ? now + refreshExpiresIn * 1000 : 0),
      lastRefreshTime: auth.lastRefreshTime || now,
    };
  }

  // ─── 账号列表 ───────────────────────────────────────────

  async function fetchAccounts(auth, ctx = defaultContext) {
    const headers = {
      ...enterpriseHeaders(auth, ctx),
      Authorization: `Bearer ${auth.accessToken}`,
    };
    const resp = await api('/accounts', { method: 'GET', headers }, ctx);
    const data = await unwrap(resp, 'accounts');
    const accounts = Array.isArray(data?.accounts) ? data.accounts : [];
    return accounts;
  }

  // ─── 切换账号（个人/企业版）─────────────────────────────

  /**
   * 桌面端 switchAccount：企业账号走 /login/enterprise/{enterpriseId}，
   * 个人账号走 /login/enterprise；返回新的 token。
   */
  async function switchAccount(auth, { enterpriseId = '', type } = {}, ctx = defaultContext) {
    const isEnterprise = type === 'enterprise' || Boolean(enterpriseId);
    const path = isEnterprise && enterpriseId
      ? `/login/enterprise/${encodeURIComponent(enterpriseId)}`
      : '/login/enterprise';
    const headers = {
      ...enterpriseHeaders(auth, ctx),
      'X-Refresh-Token': auth.refreshToken || '',
      Authorization: `Bearer ${auth.accessToken}`,
    };
    if (enterpriseId) {
      headers['X-Enterprise-Id'] = enterpriseId;
      headers['X-Tenant-Id'] = enterpriseId;
    }
    const resp = await api(path, { method: 'POST', body: {}, headers }, ctx);
    const data = await unwrap(resp, 'login/enterprise');
    if (!data?.accessToken) throw new WorkBuddyAuthError('切换账号响应缺少 accessToken');
    return withExpiresAt({ ...data, domain: data.domain ?? auth.domain });
  }

  // ─── token 刷新 ─────────────────────────────────────────

  function isTokenExpiring(auth) {
    const expiresAt = Number(auth?.expiresAt) || 0;
    if (!expiresAt) return false;
    return expiresAt - REFRESH_WINDOW_MS <= Date.now();
  }

  /**
   * token 续期。ctx 决定向哪个端点续期 —— 多账号（国内版/国际版）必须
   * 用该账号自己的 endpoint，否则会拿 A 站点的 refreshToken 打 B 站点。
   * proxy 为该账号的出网代理（null=直连），与转发走同一个出口，
   * 否则会出现「能转发但刷不了 token」这种一半通的情况。
   */
  async function refreshSession(auth, ctx = defaultContext, proxy = null) {
    if (!auth?.refreshToken) {
      throw new WorkBuddyAuthError('缺少 refreshToken，无法续期，请重新登录');
    }
    const resp = await api('/auth/token/refresh', {
      method: 'POST',
      body: {},
      headers: {
        ...enterpriseHeaders(auth, ctx),
        'X-Refresh-Token': auth.refreshToken,
        'X-Auth-Refresh-Source': 'plugin',
      },
      proxy,
    }, ctx);
    const data = await unwrap(resp, 'auth/token/refresh');
    if (!data?.accessToken) throw new WorkBuddyAuthError('token 刷新响应缺少 accessToken');
    return withExpiresAt(data);
  }

  /** 手动刷新：当前账号（store 模式）或已存储会话（auth.json 模式） */
  async function refreshStoredSession() {
    if (accountStore) {
      const entry = accountStore.getSessionById(accountStore.getCurrentEntry()?.id ?? '')
        || accountStore.getCurrentEntry();
      if (!entry?.session?.auth?.refreshToken) throw new WorkBuddyAuthError('当前没有可刷新的登录态');
      const nextAuth = await refreshSession(entry.session.auth, contextFromSession(entry.session), entry.proxy || null);
      accountStore.updateAccountTokens(entry.id, {
        accessToken: nextAuth.accessToken,
        refreshToken: nextAuth.refreshToken,
        expiresAt: nextAuth.expiresAt,
        refreshExpiresAt: nextAuth.refreshExpiresAt,
      });
      return { ...entry.session, auth: nextAuth };
    }
    const stored = loadStoredSession();
    if (!stored?.auth?.refreshToken) throw new WorkBuddyAuthError('当前没有可刷新的登录态');
    const nextAuth = await refreshSession(stored.auth, contextFromSession(stored));
    const next = { ...stored, auth: nextAuth, lastRefreshTime: Date.now() };
    persistSession(next);
    return next;
  }

  // ─── 对外凭证接口 ───────────────────────────────────────

  /**
   * 返回当前可用凭证；临期自动刷新并回写。
   * 优先级：环境变量 WORKBUDDY_TOKEN > 账号存储当前账号 > auth.json。
   */
  async function getCurrentSession() {
    if (process.env.WORKBUDDY_TOKEN) {
      return {
        endpoint: defaultContext.baseUrl,
        prefixPath: defaultContext.prefix,
        platform: defaultContext.platform,
        edition: defaultContext.edition,
        auth: withExpiresAt({
          accessToken: process.env.WORKBUDDY_TOKEN,
          refreshToken: process.env.WORKBUDDY_REFRESH_TOKEN || '',
          tokenType: 'Bearer',
        }),
        account: { uid: process.env.WORKBUDDY_USER_ID || '' },
      };
    }
    if (accountStore) {
      const entry = accountStore.getCurrentEntry();
      if (!entry) return null;
      const session = entry.session;
      if (isTokenExpiring(session.auth) && session.auth.refreshToken) {
        verbose('[Auth]', `token 临期，自动刷新（账号 ${entry.id}）…`);
        // 续期也走该账号自己的出口，否则「能转发但刷不了 token」
        const nextAuth = await refreshSession(session.auth, contextFromSession(session), session.proxy || null);
        accountStore.updateAccountTokens(entry.id, {
          accessToken: nextAuth.accessToken,
          refreshToken: nextAuth.refreshToken,
          expiresAt: nextAuth.expiresAt,
          refreshExpiresAt: nextAuth.refreshExpiresAt,
        });
        return { ...session, auth: nextAuth };
      }
      return session;
    }
    const stored = loadStoredSession();
    if (!stored) return null;
    if (isTokenExpiring(stored.auth) && stored.auth.refreshToken) {
      verbose('[Auth]', 'token 临期，自动刷新…');
      const auth = await refreshSession(stored.auth, contextFromSession(stored));
      const next = { ...stored, auth, lastRefreshTime: Date.now() };
      persistSession(next);
      return next;
    }
    return stored;
  }

  /** 转发 LLM 请求所需的完整鉴权头（对齐桌面端 buildAuthHeaders(true,false,false)） */
  function buildAuthHeaders(session) {
    const { auth, account } = session || {};
    const headers = {};
    if (account?.uid) headers['X-User-Id'] = account.uid;
    if (auth?.accessToken) headers.Authorization = `Bearer ${auth.accessToken}`;
    if (account?.enterpriseId) {
      headers['X-Enterprise-Id'] = account.enterpriseId;
      headers['X-Tenant-Id'] = account.enterpriseId;
    }
    if (auth?.domain) headers['X-Domain'] = auth.domain;
    if (account?.departmentFullName) headers['X-Department-Info'] = account.departmentFullName;
    return headers;
  }

  async function getStatus() {
    const session = await getCurrentSession().catch(() => null);
    if (!session) {
      return {
        loggedIn: false,
        endpoint: defaultContext.baseUrl,
        prefixPath: defaultContext.prefix,
        edition: defaultContext.edition,
        platform: defaultContext.platform,
        authFile: accountStore ? accountStore.file : authFile,
        reason: '尚未登录或凭证缺失',
      };
    }
    const expiresAt = Number(session.auth?.expiresAt) || 0;
    return {
      loggedIn: true,
      endpoint: session.endpoint || defaultContext.baseUrl,
      prefixPath: session.prefixPath ?? defaultContext.prefix,
      edition: session.edition || defaultContext.edition,
      platform: session.platform || defaultContext.platform,
      accountUid: session.account?.uid || null,
      nickname: session.account?.nickname || null,
      accountType: session.account?.type || 'personal',
      enterpriseId: session.account?.enterpriseId || null,
      currentAccountId: accountStore ? accountStore.getCurrentEntry()?.id ?? null : null,
      tokenExpiresAt: expiresAt || null,
      canRefresh: !!session.auth?.refreshToken,
      authFile: accountStore ? accountStore.file : authFile,
    };
  }

  return {
    get baseUrl() { return defaultContext.baseUrl; },
    get prefixPath() { return defaultContext.prefix; },
    get authFile() { return authFile; },
    get platform() { return defaultContext.platform; },
    get edition() { return defaultContext.edition; },
    makeContext,
    contextFromSession,
    loginInteractive,
    getCurrentSession,
    refreshSession,
    refreshStoredSession,
    fetchAccounts,
    fetchLoginAccount,
    switchAccount,
    buildAuthHeaders,
    enterpriseHeaders,
    getStatus,
    clearSession,
    loadStoredSession,
  };
}
