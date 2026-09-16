#!/usr/bin/env node
/**
 * WorkBuddy Local Proxy — OpenAI 兼容 API 网关 + 积分/签到接口
 *
 * 把腾讯 WorkBuddy 桌面端的登录态包装成本地 OpenAI 兼容接口，
 * 透明转发到官方 LLM 网关，并额外暴露积分查询与每日签到接口。
 *
 *   OpenAI 客户端 (HTTP/SSE)
 *       ↓ POST /v1/chat/completions   (OpenAI 兼容)
 *   本网关 server.mjs（无头登录、token 刷新、请求头复刻）
 *       ↓ HTTPS
 *   https://copilot.tencent.com/v2/chat/completions
 *
 * 端点分三类，前缀规则不同：
 *   登录/账号  {endpoint}/v2/plugin/...     ← 带 prefixPath
 *   计费/签到  {endpoint}/v2/billing/...    ← 不带
 *   LLM 对话   {endpoint}/v2/chat/completions ← 不带
 *
 * 登录方式（任选其一）：
 *   1. node server.mjs --login   打印登录 URL，浏览器完成登录后自动获取 token
 *      （逆向桌面端流程：POST /v2/plugin/auth/state?platform=workbuddy
 *        → 浏览器 → 轮询 GET /v2/plugin/auth/token?state=…）
 *   2. 环境变量 WORKBUDDY_TOKEN / WORKBUDDY_REFRESH_TOKEN / WORKBUDDY_USER_ID
 *
 * 登录态持久化: ~/.workbuddy-proxy/auth.json，token 临期（5 分钟窗口）自动刷新。
 *
 * 用法:
 *   node server.mjs --port 3065
 *   node server.mjs --login
 *   node server.mjs --checkin          签到并打印最新积分
 *   node server.mjs --usage            只查积分/额度
 *   node server.mjs --status           查看登录态
 */

import http from 'node:http';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { argv, exit } from 'node:process';
import { pathToFileURL } from 'node:url';
import {
  createWorkBuddyAuth,
  WorkBuddyAuthError,
} from './src/workbuddy-auth.mjs';
import { createAccountStore, AccountStoreError } from './src/workbuddy-account-store.mjs';
import { setupAccountRoutes } from './src/workbuddy-account-routes.mjs';
import {
  createWorkBuddyUpstreamClient,
  WorkBuddyUpstreamError,
  parseQuotaResetAt,
} from './src/workbuddy-upstream-client.mjs';
import { createModelCatalog, ModelRouteError, MODEL_ALLOWLIST } from './src/workbuddy-models.mjs';
import {
  createWorkBuddyBilling,
  WorkBuddyBillingError,
} from './src/workbuddy-billing.mjs';
import { createDesensitizer, DEFAULT_ROLES } from './src/workbuddy-desensitize.mjs';
import { setupDesensitizeRoutes } from './src/workbuddy-desensitize-routes.mjs';
import { createLogStore } from './src/workbuddy-logs.mjs';
import { setupLogRoutes } from './src/workbuddy-log-routes.mjs';
import {
  DEFAULT_PREFIX_PATH,
  DEFAULT_EDITION,
  EDITIONS,
  WORKBUDDY_CLIENT_VERSION,
  resolveEdition,
  userAgentForEdition,
} from './src/workbuddy-endpoints.mjs';
import { clashProxyOptions, closeDispatchers } from './src/workbuddy-proxy.mjs';
import { printStartupBanner } from './src/workbuddy-banner.mjs';
import {
  printUsageReport,
  runCheckinCommand,
  runEndpointsCommand,
  runStatusCommand,
  logStartupConfig,
} from './src/workbuddy-cli.mjs';

// ─── 命令行参数 ──────────────────────────────────────────

function parseArgs() {
  const args = argv.slice(2);
  const opts = {
    port: 3065,
    host: '127.0.0.1',
    apiKey: process.env.WORKBUDDY_PROXY_API_KEY || null,
    edition: process.env.WORKBUDDY_EDITION || null,
    endpoint: process.env.WORKBUDDY_ENDPOINT || null,
    prefixPath: process.env.WORKBUDDY_PREFIX_PATH,
    defaultModel: process.env.WORKBUDDY_DEFAULT_MODEL || 'auto',
    verbose: process.env.WORKBUDDY_VERBOSE === '1',
    mode: 'serve',
    locale: process.env.WORKBUDDY_LOCALE || 'zh-CN',
    // 内容脱敏默认开启；undefined 表示沿用持久化配置，--desensitize / --no-desensitize 为本次运行的强制覆盖
    desensitize: process.env.WORKBUDDY_DESENSITIZE === '0' ? false
      : process.env.WORKBUDDY_DESENSITIZE === '1' ? true
      : undefined,
  };
  for (let i = 0; i < args.length; i++) {
    switch (args[i]) {
      case '--port':          opts.port = parseInt(args[++i]) || 3065; break;
      case '--host':          opts.host = args[++i] || '127.0.0.1'; break;
      case '--api-key':       opts.apiKey = args[++i]; break;
      case '--edition':       opts.edition = args[++i]; break;
      case '--endpoint':      opts.endpoint = args[++i]; break;
      case '--prefix-path':   opts.prefixPath = args[++i]; break;
      case '--default-model': opts.defaultModel = args[++i]; break;
      case '--locale':        opts.locale = args[++i]; break;
      case '--desensitize':   opts.desensitize = true; break;
      case '--no-desensitize': opts.desensitize = false; break;
      case '--strict-models': break; // 已废弃：未知模型直接报错现在是默认且唯一行为，保留参数仅为兼容旧启动脚本
      case '--login':         opts.mode = 'login'; break;
      case '--logout':        opts.mode = 'logout'; break;
      case '--status':        opts.mode = 'status'; break;
      case '--usage':         opts.mode = 'usage'; break;
      case '--checkin':       opts.mode = 'checkin'; break;
      case '--endpoints':     opts.mode = 'endpoints'; break;
      case '--verbose':
      case '-v':              opts.verbose = true; break;
      case '--help':
      case '-h':
        console.log(`
WorkBuddy Local Proxy（OpenAI 兼容网关 + 积分/签到 · 腾讯 WorkBuddy）

用法: node server.mjs [options]

选项:
  --port <port>           监听端口 (默认 3065)
  --host <address>        监听地址 (默认 127.0.0.1)
  --api-key <key>         API Key 认证（非回环地址监听时建议开启）
  --endpoint <url>        上游端点 (默认按 --edition：国内版 copilot.tencent.com / 国际版 www.workbuddy.ai)
  --edition <cn|intl>     账号版本：cn=国内版 / intl=国际版（默认 cn；多账号时以账号记录为准）
  --prefix-path <path>    鉴权路径前缀 (默认 /plugin；国际版为空)
  --default-model <id>    客户端未指定模型时使用的默认模型 (默认 auto)
  --locale <locale>       计费接口 Accept-Language (默认 zh-CN → zh)
                          （未知模型一律直接返回 400 报错，绝不静默改写/回退）
  --desensitize           开启内容脱敏（默认为开启，此参数用于覆盖已持久化的关闭状态）
  --no-desensitize        关闭内容脱敏（等价于桌面端关掉开关，不影响持久化配置）
  --login                 发起无头登录：打印登录 URL，浏览器完成后自动保存 token
  --logout                清除本地登录态
  --status                查看当前登录态与 token 过期时间
  --usage                 查询积分/额度（临时命令，不启动服务）
  --checkin               执行每日签到并打印最新积分
  --endpoints             打印已逆向出的全部接口清单
  -v, --verbose           详细日志
  -h, --help              显示帮助

上游环境变量:
  WORKBUDDY_ENDPOINT           上游端点
  WORKBUDDY_TOKEN              直接注入 accessToken（优先级高于 auth.json）
  WORKBUDDY_REFRESH_TOKEN      配合 WORKBUDDY_TOKEN 的刷新凭证
  WORKBUDDY_USER_ID            配合 WORKBUDDY_TOKEN 的用户 ID（X-User-Id 头）
  WORKBUDDY_DEFAULT_MODEL      默认模型
  WORKBUDDY_PROXY_API_KEY      网关自身 API Key
  WORKBUDDY_DESENSITIZE        1 开启 / 0 关闭内容脱敏（未设置则用词表文件里的开关）
  WORKBUDDY_PROXY_HOME         配置目录 (默认 ~/.workbuddy-proxy)
`);
        exit(0);
    }
  }
  return opts;
}

const opts = parseArgs();

// ─── 网关自身配置（API Key 持久化）────────────────────────

const CONFIG_DIR = process.env.WORKBUDDY_PROXY_HOME || join(homedir(), '.workbuddy-proxy');
const CONFIG_FILE = join(CONFIG_DIR, 'config.json');

// ─── 日志 ──────────────────────────────────────────────────
//
// 双通道输出：控制台（保持原有格式）+ 运行日志库（桌面端「日志」页）。
// 级别由标签推断：tag 以 ❌/⚠️ 开头分别记 error/warn，其余 info；
// verbose 记 debug（只有 --verbose 时才会上报入库，避免刷屏）。

/** 标签 → 日志分类，与桌面端筛选下拉一致 */
const TAG_CATEGORY = Object.freeze({
  '[Server]': 'server',
  '[Init]': 'server',
  '[Config]': 'config',
  '[Security]': 'config',
  '[Login]': 'auth',
  '[Auth]': 'auth',
  '[Accounts]': 'account',
  '[Models]': 'model',
  '[Model]': 'model',
  '[Upstream]': 'upstream',
  '[Desensitize]': 'desensitize',
  '[Logs]': 'server',
  '[Billing]': 'upstream',
  '[Activity]': 'upstream',
  '[HTTP]': 'server',
});

const logStore = createLogStore({ directory: CONFIG_DIR, log: (...args) => console.log(...args) });

/** 从标签与消息推断级别：错误/警告前缀优先 */
function inferLevel(tag, text) {
  if (/^❌|\b失败\b|失败:/.test(text) || /^❌/.test(tag)) return 'error';
  if (/^⚠️/.test(text) || /^⚠️/.test(tag)) return 'warn';
  return 'info';
}

function emit(tag, args, { level: forcedLevel } = {}) {
  const ts = new Date().toISOString().slice(11, 23);
  console.log(`[${ts}] ${tag}`, ...args);
  if (!logStore) return;
  const text = args
    .map(arg => (typeof arg === 'string' ? arg : (() => {
      try { return JSON.stringify(arg); } catch { return String(arg); }
    })()))
    .join(' ')
    .trim();
  if (!text) return;
  logStore.append({
    level: forcedLevel || inferLevel(tag, text),
    category: TAG_CATEGORY[tag] || 'server',
    message: `${tag} ${text}`.trim(),
  });
}

function log(tag, ...args) {
  emit(tag, args);
}
function verbose(tag, ...args) {
  if (opts.verbose) emit(tag, args, { level: 'debug' });
}

/**
 * 结构化事件上报：429 自动切换这类需要带字段的事件走这里，
 * 桌面端日志页能按 category/data 精确展示与筛选。
 */
function logEvent({ level = 'info', category = 'server', message, data } = {}) {
  if (!message) return null;
  return logStore?.append({ level, category, message, data }) || null;
}

function loadConfig() {
  try {
    if (existsSync(CONFIG_FILE)) return JSON.parse(readFileSync(CONFIG_FILE, 'utf8'));
  } catch { /* 缺失或损坏用默认值 */ }
  return {};
}

function saveConfig(config) {
  try {
    mkdirSync(CONFIG_DIR, { recursive: true });
    writeFileSync(CONFIG_FILE, JSON.stringify(config, null, 2), 'utf8');
    return true;
  } catch (error) {
    log('[Config]', `保存失败: ${error.message}`);
    return false;
  }
}

function applyConfig(config) {
  if (config.apiKey && !opts.apiKey) opts.apiKey = config.apiKey;
  if (config.locale && !process.env.WORKBUDDY_LOCALE) opts.locale = config.locale;
}
applyConfig(loadConfig());

// ─── HTTP 工具 ─────────────────────────────────────────────

const MAX_BODY_SIZE = 32 * 1024 * 1024;

function readRawBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', chunk => {
      size += chunk.length;
      if (size > MAX_BODY_SIZE) {
        reject(new Error('Body too large'));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

function sendCORS(res) {
  res.setHeader('Access-Control-Allow-Origin', '*');
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS, DELETE');
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type, Authorization, x-api-key');
}

function sendJson(res, status, payload) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(payload));
}

function checkApiKey(req) {
  if (!opts.apiKey) return true;
  const auth = req.headers['authorization'] || '';
  const bearer = auth.replace(/^Bearer\s+/i, '');
  const apiKeyHeader = req.headers['x-api-key'] || '';
  return bearer === opts.apiKey || apiKeyHeader === opts.apiKey;
}

function unauthorized(res) {
  sendJson(res, 401, {
    error: { message: 'Unauthorized: invalid or missing API key', type: 'invalid_api_key' },
  });
}

function errorPayload(error) {
  const statusCode = Number.isInteger(error?.statusCode) ? error.statusCode : 500;
  // 账号限额（429/code 6004）：按 OpenAI 风格 rate_limit_exceeded 返回，附带恢复时间
  const isQuota = statusCode === 429 || Number(error?.upstreamCode) === 6004;
  const resetAt = isQuota ? parseQuotaResetAt(error?.message || '') : 0;
  return {
    statusCode,
    body: {
      error: {
        message: error instanceof Error ? error.message : String(error),
        type: isQuota ? 'rate_limit_exceeded'
          : statusCode === 401 ? 'authentication_error'
            : statusCode < 500 ? 'invalid_request_error'
              : 'proxy_error',
        ...(error?.upstreamCode !== undefined ? { upstream_code: error.upstreamCode } : {}),
        ...(resetAt ? { reset_at: resetAt, reset_at_text: new Date(resetAt).toLocaleString('zh-CN', { hour12: false }) } : {}),
      },
    },
  };
}

// ─── 核心组件 ──────────────────────────────────────────────

const accountStore = createAccountStore({ directory: CONFIG_DIR, log });

const auth = createWorkBuddyAuth({
  edition: opts.edition,
  endpoint: opts.endpoint,
  prefixPath: opts.prefixPath,
  directory: CONFIG_DIR,
  accountStore,
  log,
  verbose,
});

// 旧版单账号 auth.json 迁移（仅当账号列表为空时导入一次）
accountStore.importLegacySession(auth.loadStoredSession());

// 优先级去重迁移：历史数据里并列的优先级会被重新编号成连续序号，
// 相对顺序保持不变（所以升级后实际转发顺序不变）。详见 account-store。
accountStore.migratePriorities();

const modelCatalog = createModelCatalog({ verbose });
const upstreamClient = createWorkBuddyUpstreamClient({ auth, accountStore, log, verbose, logEvent });
const billing = createWorkBuddyBilling({ auth, log, verbose });

/**
 * 最近一次实际转发的模型（含默认值填充后的结果）。
 * 账号页的「模型」筛选默认选它 —— 用户最关心「我正在用的这个模型会走哪些账号」，
 * 而不是一个抽象的全局顺序。
 *
 * 取值优先序：
 *   1. config.json 里上次记下的模型（值变化才写盘，不用每个请求都动文件）；
 *   2. 上一次入站请求的调试落盘（debug/last-request.json）—— 升级到本版本后
 *      首次启动时 config 里还没有记录，用它兜底就能直接显示用户在用的模型；
 *   3. 都没有则由前端回落到启动配置的 defaultModel / 目录首项。
 */
function readLastRequestModel() {
  const fromConfig = String(loadConfig().lastRequestModel || '');
  if (fromConfig) return fromConfig;
  try {
    const debugFile = join(CONFIG_DIR, 'debug', 'last-request.json');
    if (!existsSync(debugFile)) return '';
    const body = JSON.parse(readFileSync(debugFile, 'utf8'));
    return typeof body?.model === 'string' ? body.model : '';
  } catch {
    return '';
  }
}

let lastRequestModel = readLastRequestModel();

/** 记住本次请求用的模型；仅在变化时写盘，避免每个请求都动文件 */
function rememberRequestModel(model) {
  const id = String(model || '');
  if (!id || id === lastRequestModel) return;
  lastRequestModel = id;
  saveConfig({ ...loadConfig(), lastRequestModel: id });
}

/**
 * 内容脱敏：默认开启。词表与开关持久化在 {CONFIG_DIR}/desensitize.json，
 * 由 readConfig → createDesensitizer 读入；--desensitize / --no-desensitize
 * 只覆盖本次运行，不写回文件（避免启动脚本顺带改掉用户在前端的设置）。
 */
const desensitizer = createDesensitizer({ directory: CONFIG_DIR, log });
if (opts.desensitize !== undefined) desensitizer.setEnabled(opts.desensitize, { persist: false });

const accountRoutes = setupAccountRoutes({
  store: accountStore,
  auth,
  billing,
  log,
  verbose,
  checkApiKey,
  unauthorized,
  readRawBody,
});

const desensitizeRoutes = setupDesensitizeRoutes({
  desensitizer,
  checkApiKey,
  unauthorized,
  readRawBody,
  log,
});

const logRoutes = setupLogRoutes({
  logStore,
  checkApiKey,
  unauthorized,
  log,
});

/** 模型目录动态刷新：GET /v3/config → data.models（端点/UA/出口按当前账号走） */
async function refreshModelCatalog() {
  try {
    // 当前账号 = 队首的可用账号（由 store 派生，与转发默认使用的账号一致），
    // 这里直接用它的凭证与出口，保证模型目录与转发看到的是同一个账号
    const entry = accountStore.getCurrentEntry();
    const session = entry?.session ?? await auth.getCurrentSession();
    if (!session) return;
    const result = await modelCatalog.refresh({
      endpoint: session.endpoint || auth.baseUrl,
      enterpriseId: session.account?.enterpriseId,
      headers: auth.buildAuthHeaders(session),
      userAgent: userAgentForEdition(session.edition),
      proxy: entry?.proxy || null,
    });
    if (result.refreshed) {
      log('[Models]', `✅ 模型目录已更新（${result.count} 个，来源 ${result.source}）`);
    } else {
      verbose('[Models]', `模型目录未更新: ${result.reason}`);
    }
  } catch (error) {
    verbose('[Models]', `模型目录刷新失败: ${error.message}`);
  }
}

// ─── 路由处理 ──────────────────────────────────────────────

async function handleModelRequest(req, res, rawBody, kind, path) {
  let body;
  try {
    body = rawBody.length ? JSON.parse(rawBody.toString('utf8')) : null;
    if (!body || typeof body !== 'object' || Array.isArray(body)) throw new Error('not object');
  } catch {
    sendJson(res, 400, { error: { message: '请求体必须是 JSON 对象', type: 'invalid_request_error' } });
    return;
  }
  if (kind === 'chat' && !Array.isArray(body.messages)) {
    sendJson(res, 400, { error: { message: '缺少 messages 数组', type: 'invalid_request_error' } });
    return;
  }

  verbose('[Model]',
    `← ${req.method} ${path} model=${body.model || '(未指定)'} `
    + `stream=${body.stream === true ? 'true' : 'false'} `
    + `msgs=${Array.isArray(body.messages) ? body.messages.length : '-'} `
    + `bytes=${rawBody.length} ua=${req.headers['user-agent'] || '-'}`);

  // 调试落盘：保存最近一次入站请求体，供重放分析（覆盖写）
  try {
    const debugDir = join(CONFIG_DIR, 'debug');
    mkdirSync(debugDir, { recursive: true });
    writeFileSync(join(debugDir, 'last-request.json'), rawBody);
    writeFileSync(join(debugDir, 'last-request.meta.txt'),
      `${new Date().toISOString()} ${req.method} ${path} ${rawBody.length}B ua=${req.headers['user-agent'] || '-'}`);
  } catch { /* 调试落盘失败不影响请求 */ }

  // 模型路由：点名的模型必须是上游目录里真实存在的 id。
  // 不做任何静默改写/回退 —— 请求 4.1 就必须路由到 4.1，
  // 不存在就 400 报错（附近似名提示），让下游自己改配置。
  if (kind === 'chat' && !body.model) {
    // 客户端没点名 → 网关默认；默认值被白名单收窄掉时，落到目录里 isDefault 的模型
    body.model = modelCatalog.has(opts.defaultModel)
      ? opts.defaultModel
      : modelCatalog.list().find(m => m.isDefault)?.id ?? modelCatalog.list()[0]?.id;
  }
  if (body.model && !modelCatalog.has(body.model)) {
    const hint = modelCatalog.suggest(body.model, 5);
    sendJson(res, 400, {
      error: {
        message: `模型不存在: ${body.model}`
          + (hint.length ? `（目录里相近的模型: ${hint.join('、')}）` : '')
          + '。完整列表见 GET /v1/models',
        type: 'invalid_request_error',
        code: 'model_not_found',
      },
    });
    return;
  }
  // 记录本次实际用的模型（默认值已填充完毕），供账号页筛选默认选中
  if (body.model) rememberRequestModel(body.model);

  // 内容脱敏：默认开启，命中词插零宽空格后再送上游（缓解后端审核对合规声明的误拦）。
  // 只处理 messages 的文本内容，不影响 model / stream 等其他字段。
  if (kind === 'chat' || kind === 'completions') {
    const result = desensitizer.processBody(body);
    if (result.changed) {
      body = result.body;
      verbose('[Desensitize]', `命中 ${result.hits} 处（${(result.matched || []).join('、')}）`);
    }
  }

  const controller = new AbortController();
  let responseCompleted = false;
  const abortRequest = () => controller.abort(new Error('客户端已断开'));
  const onResponseClose = () => {
    if (!responseCompleted && !res.writableEnded) abortRequest();
  };
  req.on('aborted', abortRequest);
  res.on('close', onResponseClose);
  try {
    if (kind === 'chat') {
      // 请求体哈希作为去重键：相同 body 的快速重试在代理内排队（防风控）
      const dedupeKey = createHash('sha256').update(rawBody).digest('hex');
      await upstreamClient.forwardChatCompletions({ req, res, body, controller, dedupeKey });
    } else if (kind === 'completions') {
      await upstreamClient.forwardCompletions({ req, res, body, controller });
    } else if (kind === 'images') {
      await upstreamClient.forwardImageGenerations({ req, res, body, controller });
    } else if (kind === 'videos') {
      await upstreamClient.forwardVideoGenerations({ req, res, body, controller });
    } else {
      await upstreamClient.forwardEmbeddings({ req, res, body, controller });
    }
    responseCompleted = true;
  } catch (error) {
    if (controller.signal.aborted) return;
    const result = errorPayload(error);
    log('[Model]', `❌ ${error instanceof Error ? `${error.message}\n${(error.stack || '').split('\n').slice(1, 4).join('\n')}` : String(error)}`);
    if (!res.headersSent) {
      sendJson(res, result.statusCode, result.body);
    } else if (!res.writableEnded) {
      try { res.write(`data: ${JSON.stringify({ error: result.body.error })}\n\n`); } catch { /* 已断开 */ }
      try { res.write('data: [DONE]\n\n'); } catch { /* 已断开 */ }
      res.end();
    }
  } finally {
    req.removeListener('aborted', abortRequest);
    res.removeListener('close', onResponseClose);
  }
}

async function handleHealth(req, res) {
  const summary = await upstreamClient.getConfigSummary();
  const status = await auth.getStatus();
  const healthy = summary.configured === true;
  sendJson(res, 200, {
    status: healthy ? 'ok' : 'degraded',
    transport: 'upstream-api',
    product: 'WorkBuddy',
    upstreamConfigured: healthy,
    upstreamBaseUrl: summary.baseUrl || null,
    authApiBase: summary.authApiBase || null,
    chatApiUrl: summary.chatApiUrl || null,
    authSource: summary.authSource || null,
    currentAccountId: summary.currentAccountId || null,
    tokenExpiresAt: summary.tokenExpiresAt || null,
    canRefresh: summary.canRefresh === true,
    login: status,
    unavailableReason: healthy ? null : summary.unavailableReason || null,
    authRequired: !!opts.apiKey,
    defaultModel: opts.defaultModel,
    models: modelCatalog.list().length,
  });
}

// 无头登录：打印 URL → 等浏览器登录 → 自动保存 token
async function runLogin(res, edition) {
  const e = resolveEdition(edition ?? opts.edition ?? DEFAULT_EDITION);
  log('[Login]', `发起${e.label}登录（${e.endpoint}）…`);
  const session = await auth.loginInteractive({
    edition: e.id,
    onAuthUrl: (url) => {
      log('[Login]', '请在浏览器中打开以下链接并完成登录：');
      console.log('');
      console.log(`  ${url}`);
      console.log('');
      log('[Login]', '登录完成后本网关将自动获取并保存 token…');
    },
  });
  if (res && !res.writableEnded) {
    sendJson(res, 200, {
      success: true,
      accountUid: session.account?.uid || null,
      nickname: session.account?.nickname || null,
      edition: session.edition || e.id,
      authFile: auth.authFile,
    });
  }
  return session;
}

// ─── 登录任务管理（供 /api/session/login/* 使用）────────────

const loginTasks = new Map(); // state -> task

function startLoginTask(edition) {
  const e = resolveEdition(edition ?? opts.edition ?? DEFAULT_EDITION);
  const controller = new AbortController();
  const task = {
    state: null, authUrl: null, done: false, error: null, session: null,
    edition: e.id, startedAt: Date.now(), controller,
  };
  const promise = auth.loginInteractive({
    edition: e.id,
    signal: controller.signal,
    onAuthUrl: (url, state) => {
      task.authUrl = url;
      task.state = state;
      if (state) loginTasks.set(state, task);
    },
  });
  promise.then(
    session => {
      task.done = true;
      task.session = {
        accountUid: session.account?.uid || null,
        nickname: session.account?.nickname || null,
        edition: session.edition || e.id,
      };
      log('[Login]', `✅ 登录任务完成（${e.label}，账号 ${task.session.accountUid || '未知'}）`);
    },
    error => {
      task.done = true;
      task.error = error instanceof Error ? error.message : String(error);
      log('[Login]', task.canceled ? '登录任务已取消' : `❌ 登录任务失败: ${task.error}`);
    },
  ).finally(() => {
    setTimeout(() => { if (task.state) loginTasks.delete(task.state); }, 10 * 60 * 1000);
  });
  return task;
}

/**
 * 取消登录任务：中止后端轮询并立即把任务标记为结束。
 * 用户在浏览器里放弃登录、或关掉桌面端弹窗时调用，避免任务空转到超时。
 */
function cancelLoginTask(state) {
  if (!state) return false;
  const task = loginTasks.get(state);
  if (!task) return false;
  task.canceled = true;
  task.done = true;
  task.error = task.error || '登录已取消';
  try { task.controller?.abort(); } catch { /* 已结束 */ }
  loginTasks.delete(state);
  log('[Login]', '登录任务已取消（用户放弃等待）');
  return true;
}

// ─── HTTP 服务器 ───────────────────────────────────────────

export function createRequestHandler() {
  return async (req, res) => {
    sendCORS(res);
    if (req.method === 'OPTIONS') { res.writeHead(204); res.end(); return; }

    const url = new URL(req.url, `http://${req.headers.host}`);
    const path = url.pathname;

    // 详细模式下记录每个入站请求（--verbose），便于观察桌面端/客户端实际调用了哪些接口
    if (opts.verbose && !path.startsWith('/v1/')) {
      verbose('[HTTP]', `← ${req.method} ${path}${url.search}`);
    }

    try {
      if (req.method === 'GET' && path === '/health') return await handleHealth(req, res);

      // ── 接口清单（把逆向结果直接暴露出来，便于排查）──
      if (req.method === 'GET' && path === '/api/endpoints') {
        const { AUTH, LLM, BILLING, ACTIVITY, CONFIG, CLOUD_AGENT, SIDECAR } = await import('./src/workbuddy-endpoints.mjs');
        sendJson(res, 200, {
          success: true,
          data: {
            baseUrl: auth.baseUrl,
            prefixPath: auth.prefixPath,
            platform: auth.platform,
            edition: auth.edition,
            editions: EDITIONS,
            clientVersion: WORKBUDDY_CLIENT_VERSION,
            note: '登录/账号带 prefixPath；计费/签到/LLM 不带；多账号时端点按账号 edition 走',
            auth: AUTH,
            llm: LLM,
            billing: BILLING,
            activity: ACTIVITY,
            config: CONFIG,
            cloudAgent: CLOUD_AGENT,
            sidecar: SIDECAR,
          },
        });
        return;
      }

      // ── 多账号管理 API（列表/添加/切换/修改/刷新/删除）──
      if (await accountRoutes.tryHandle(req, res, path)) return;

      // ── 出网代理（Clash Verge 可选项 / 连通性测试）──
      if (await accountRoutes.tryHandleProxies(req, res, path)) return;

      // ── 敏感词脱敏（词表维护 / 开关 / 命中统计）──
      if (await desensitizeRoutes.tryHandle(req, res, path, url)) return;

      // ── 运行日志（查询 / 统计 / 导出 / 清空）──
      if (await logRoutes.tryHandle(req, res, path, url)) return;

      // ── 会话管理 API ──
      if (req.method === 'GET' && path === '/api/session') {
        const [summary, status] = await Promise.all([
          upstreamClient.getConfigSummary(),
          auth.getStatus(),
        ]);
        const accountsSnapshot = accountStore.listAccounts();
        sendJson(res, 200, {
          success: true,
          data: {
            health: {
              upstreamConfigured: summary.configured === true,
              upstreamBaseUrl: summary.baseUrl || null,
              authSource: summary.authSource || null,
              unavailableReason: summary.configured ? null : summary.unavailableReason || null,
            },
            session: status,
            // accounts.currentAccountId 即首选账号（由优先级派生，见 account-store）
            accounts: accountsSnapshot,
            // 最近一次实际请求用的模型：账号页的「模型」筛选默认选它，
            // 这样打开页面看到的就是「我正在用的这个模型会走哪些账号」
            lastRequestModel: lastRequestModel || null,
            defaultModel: opts.defaultModel,
            proxies: (() => {
              const clash = clashProxyOptions();
              return { clashAvailable: clash.available, clashError: clash.error, optionCount: clash.options.length };
            })(),
            models: modelCatalog.list().map(m => ({
              id: m.id, name: m.name, isDefault: !!m.isDefault, credits: m.credits || '',
            })),
            desensitize: (() => {
              const d = desensitizer.getState();
              return { enabled: d.enabled, termCount: d.termCount, roles: d.roles };
            })(),
          },
        });
        return;
      }

      if (req.method === 'POST' && path === '/api/session/login/start') {
        if (!checkApiKey(req)) return unauthorized(res);
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let edition = null;
        try { edition = JSON.parse(raw || '{}')?.edition ?? null; } catch { edition = null; }
        const task = startLoginTask(edition);
        const deadline = Date.now() + 15000;
        while (!task.authUrl && !task.done && Date.now() < deadline) {
          await new Promise(resolve => setTimeout(resolve, 100));
        }
        if (!task.authUrl) {
          sendJson(res, 502, { success: false, error: task.error || '上游登录服务未返回登录链接' });
          return;
        }
        sendJson(res, 200, { success: true, data: { state: task.state, authUrl: task.authUrl, edition: task.edition } });
        return;
      }

      if (req.method === 'GET' && path === '/api/session/login/wait') {
        if (!checkApiKey(req)) return unauthorized(res);
        const state = url.searchParams.get('state') || '';
        const task = loginTasks.get(state);
        if (!task) {
          sendJson(res, 404, { success: false, error: '登录任务不存在或已过期' });
          return;
        }
        if (!task.done) sendJson(res, 200, { success: true, data: { pending: true } });
        else if (task.error) sendJson(res, 200, { success: true, data: { done: true, error: task.error } });
        else sendJson(res, 200, { success: true, data: { done: true, session: task.session } });
        return;
      }

      // 取消登录：用户在浏览器里放弃或关掉弹窗时调用，立刻结束任务
      if (req.method === 'POST' && path === '/api/session/login/cancel') {
        if (!checkApiKey(req)) return unauthorized(res);
        const raw = (await readRawBody(req)).toString('utf8') || '{}';
        let state = '';
        try { state = String(JSON.parse(raw || '{}')?.state || ''); } catch { state = ''; }
        const canceled = cancelLoginTask(state);
        sendJson(res, 200, { success: true, data: { canceled } });
        return;
      }

      if (req.method === 'POST' && path === '/api/session/refresh') {
        if (!checkApiKey(req)) return unauthorized(res);
        await auth.refreshStoredSession();
        sendJson(res, 200, { success: true, data: { session: await auth.getStatus() } });
        return;
      }

      if (req.method === 'POST' && path === '/api/session/logout') {
        if (!checkApiKey(req)) return unauthorized(res);
        auth.clearSession();
        sendJson(res, 200, { success: true });
        return;
      }

      // ── 积分 / 额度查询 ──
      if (req.method === 'GET' && path === '/api/usage') {
        if (!checkApiKey(req)) return unauthorized(res);
        sendJson(res, 200, { success: true, data: await billing.queryCreditsSummary({ locale: opts.locale }) });
        return;
      }

      // ── 每日签到 ──
      if (req.method === 'GET' && path === '/api/checkin/status') {
        if (!checkApiKey(req)) return unauthorized(res);
        sendJson(res, 200, { success: true, data: await billing.getCheckinStatus() });
        return;
      }

      if (req.method === 'POST' && path === '/api/checkin') {
        if (!checkApiKey(req)) return unauthorized(res);
        const result = await billing.claimDailyCheckin();
        sendJson(res, result.success ? 200 : 409, { success: result.success, data: result });
        return;
      }

      // 签到 + 查余额的组合动作
      if (req.method === 'POST' && path === '/api/checkin/claim-and-report') {
        if (!checkApiKey(req)) return unauthorized(res);
        const result = await billing.checkinAndReport({ locale: opts.locale });
        sendJson(res, 200, { success: true, data: result });
        return;
      }

      // ── 运营活动 ──
      if (req.method === 'GET' && path === '/api/activity/banner') {
        if (!checkApiKey(req)) return unauthorized(res);
        sendJson(res, 200, { success: true, data: await billing.getActivityBanner() });
        return;
      }

      if (req.method === 'GET' && path === '/api/activity/ambassador') {
        if (!checkApiKey(req)) return unauthorized(res);
        sendJson(res, 200, { success: true, data: await billing.getAmbassadorStatus() });
        return;
      }

      // ── 网关配置（API Key）──
      if (req.method === 'GET' && path === '/api/config') {
        if (!checkApiKey(req)) return unauthorized(res);
        const d = desensitizer.getState();
        sendJson(res, 200, {
          success: true,
          data: {
            apiKey: opts.apiKey ? `${opts.apiKey.slice(0, 6)}...${opts.apiKey.slice(-4)}` : null,
            apiKeySet: !!opts.apiKey,
            locale: opts.locale,
            defaultModel: opts.defaultModel,
            desensitize: { enabled: d.enabled, termCount: d.termCount, roles: d.roles },
          },
        });
        return;
      }

      if (req.method === 'POST' && path === '/api/config') {
        if (!checkApiKey(req)) return unauthorized(res);
        const body = JSON.parse((await readRawBody(req)).toString('utf8') || '{}');
        const config = loadConfig();
        if (body.apiKey !== undefined) {
          if (body.apiKey !== null && (typeof body.apiKey !== 'string' || body.apiKey.length < 8)) {
            sendJson(res, 400, { success: false, error: 'API Key 至少需要 8 个字符' });
            return;
          }
          if (body.apiKey === null) {
            delete config.apiKey;
            opts.apiKey = null;
          } else {
            config.apiKey = body.apiKey;
            opts.apiKey = body.apiKey;
          }
        }
        if (typeof body.locale === 'string' && body.locale) {
          config.locale = body.locale;
          opts.locale = body.locale;
        }
        saveConfig(config);
        sendJson(res, 200, { success: true, data: { apiKeySet: !!opts.apiKey, locale: opts.locale } });
        return;
      }

      if (req.method === 'GET' && path === '/v1/models') {
        sendJson(res, 200, modelCatalog.listResponse());
        // 异步刷新模型目录，不阻塞响应
        void refreshModelCatalog();
        return;
      }

      // 登录管理
      if (req.method === 'POST' && path === '/auth/login') {
        if (!checkApiKey(req)) return unauthorized(res);
        await runLogin(res);
        return;
      }
      if (req.method === 'POST' && path === '/auth/logout') {
        if (!checkApiKey(req)) return unauthorized(res);
        auth.clearSession();
        sendJson(res, 200, { success: true });
        return;
      }

      // 模型请求
      const modelRoutes = {
        '/v1/chat/completions': 'chat',
        '/v1/completions': 'completions',
        '/v1/embeddings': 'embeddings',
        '/v1/images/generations': 'images',
        '/v1/videos/generations': 'videos',
      };
      if (req.method === 'POST' && modelRoutes[path]) {
        if (!checkApiKey(req)) return unauthorized(res);
        const rawBody = await readRawBody(req);
        return await handleModelRequest(req, res, rawBody, modelRoutes[path], path);
      }

      sendJson(res, 404, { error: { message: `Not found: ${req.method} ${path}` } });
    } catch (err) {
      log('[HTTP]', `请求错误: ${err.message}`);
      const result = errorPayload(err);
      if (!res.headersSent) sendJson(res, result.statusCode, result.body);
      else if (!res.writableEnded) res.end();
    }
  };
}

export function createServer() {
  return http.createServer(createRequestHandler());
}

// ─── CLI 子命令 ────────────────────────────────────────────

async function main() {
  // ─── 非 serve 子命令（纯 CLI，不启动 HTTP 服务）──
  if (opts.mode === 'endpoints') {
    await runEndpointsCommand({ auth, edition: opts.edition });
    return;
  }
  if (opts.mode === 'logout') {
    auth.clearSession();
    console.log('✅ 已清除本地登录态');
    return;
  }
  if (opts.mode === 'status') {
    const status = await runStatusCommand({ auth });
    if (!status.loggedIn) exit(1);
    return;
  }
  if (opts.mode === 'usage') {
    printUsageReport(await billing.queryCreditsSummary({ locale: opts.locale }));
    return;
  }
  if (opts.mode === 'checkin') {
    await runCheckinCommand({ billing, locale: opts.locale });
    return;
  }
  if (opts.mode === 'login') {
    await runLogin(null);
    return;
  }

  // ─── serve：启动自检 → 监听 ───
  logStartupConfig({ opts, auth, store: accountStore, desensitizer, log });

  const summary = await upstreamClient.getConfigSummary();
  if (summary.configured) {
    log('[Init]', `✅ 凭证来源: ${summary.authSource}（账号 ${summary.currentAccountId ?? '未知'}）`);
    if (summary.tokenExpiresAt) {
      const left = summary.tokenExpiresAt - Date.now();
      log('[Init]', `   token ${left > 0 ? `${Math.round(left / 60e3)} 分钟后过期` : '已过期（将自动刷新）'}`);
    }
    void refreshModelCatalog();
  } else {
    log('[Init]', `⚠️  暂无可用登录态: ${summary.unavailableReason}`);
    log('[Init]', '   执行 node server.mjs --login 完成登录后再启动服务');
  }


  const server = createServer();
  // 退出时关掉缓存的代理连接，避免 shutdown 被挂起的 socket 拖住
  const closeAll = () => { try { closeDispatchers(); } catch { /* 已退出 */ } };
  process.once('SIGINT', closeAll);
  process.once('SIGTERM', closeAll);
  server.listen(opts.port, opts.host, () => {
    console.log('');
    log('[Server]', `✅ API 服务已启动: http://${opts.host}:${opts.port}`);
    if (opts.host !== '127.0.0.1' && opts.host !== '::1' && opts.host !== 'localhost') {
      log('[Security]', '⚠️ API 监听在非本机地址，请确认已启用 API Key 认证');
    }
    printStartupBanner(opts.host, opts.port, { apiKey: opts.apiKey });
  });
}

// PM2 fork 模式下 argv[1] 是 ProcessContainerFork.js 而非本文件，
// 因此 PM2 场景由 WORKBUDDY_PROXY_STANDALONE=1 兜底触发（与 raccoon 项目同款方案）。
const isDirectRun = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isDirectRun || process.env.WORKBUDDY_PROXY_STANDALONE === '1') {
  main().catch(error => {
    if (error instanceof WorkBuddyAuthError
      || error instanceof WorkBuddyUpstreamError
      || error instanceof WorkBuddyBillingError
      || error instanceof ModelRouteError
      || error instanceof AccountStoreError) {
      console.error('[Fatal]', error.message);
    } else {
      console.error('[Fatal]', error);
    }
    exit(1);
  });
}
