/**
 * WorkBuddy 接口清单（唯一事实来源）
 *
 * 逆向自 WorkBuddy 桌面端 v5.3.14 的 app.asar：
 *   - main/workbuddy-auth-product-coordinator.js  → AuthService（计费/签到/活动）
 *   - main/common.js                              → ExternalLinkAuthenticationProvider（登录）
 *   - cli/dist/codebuddy.js                       → ModelProviderImpl（LLM 转发）
 *   - cli/product.json                            → endpoint / prefixPath / platform
 *
 * ── 产品配置（cli/product.json）──────────────────────────────
 *   国内版（WorkBuddy）   : endpoint https://copilot.tencent.com  / platform workbuddy
 *   国际版（WorkBuddy AI）: endpoint https://www.workbuddy.ai     / platform workbuddy-ai
 *   两版 dataFolderName 分别为 .workbuddy / .workbuddy-ai，
 *   prefixPath 均为 /plugin，接口协议完全一致（见 EDITIONS）。
 *
 * ── 三类鉴权路径前缀（务必区分）──────────────────────────────
 *   1. 登录/账号：  {endpoint}/v2{prefixPath}/...  即 /v2/plugin/...  ← prefixPath 生效
 *   2. 计费/签到：  {endpoint}/v2/...             不带 /plugin     ← 直接挂在根上
 *   3. LLM 对话：   {endpoint}/v2/chat/completions                 ← 不带 /plugin
 *
 * ── 统一响应包 ───────────────────────────────────────────────
 *   管理/计费接口：{ code: 0, data: {...}, msg?, requestId? }，code === 0 为成功。
 *   LLM 接口：OpenAI 兼容（chat.completion / SSE chunk）。
 */

// ─── 端点与版本 ─────────────────────────────────────────────

export const DEFAULT_ENDPOINT = 'https://copilot.tencent.com';
export const STAGING_ENDPOINT = 'https://staging-copilot.tencent.com';
/** 国际版（CodeBuddy 海外站），prefixPath 为空 */
export const GLOBAL_ENDPOINT = 'https://www.codebuddy.ai';

/**
 * 鉴权相关域名白名单（cli/product.json → authentication.attributes）。
 * internalDomain 走国内端点，externalDomain 走海外端点。
 */
export const DOMAINS = {
  internal: [
    'copilot.tencent.com',
    'staging-copilot.tencent.com',
    'www.codebuddy.cn',
    'staging.codebuddy.cn',
    'www.workbuddy.cn',
    'staging.workbuddy.cn',
  ],
  external: ['www.codebuddy.ai', 'staging-codebuddy.tencent.com'],
  ioa: [
    'tencent.sso.copilot.tencent.com',
    'tencent.sso.copilot-staging.tencent.com',
    'tencent.sso.codebuddy.cn',
    'tencent.staging-sso.codebuddy.cn',
  ],
};

/** 桌面端版本（X-IDE-Version / User-Agent 用） */
export const WORKBUDDY_CLIENT_VERSION = '5.5.4';
export const WORKBUDDY_PRODUCT = 'WorkBuddy';
/**
 * 桌面端内置 CLI 版本（User-Agent 扩展段，还原客户端 UserAgentHttpInterceptor
 * 的 `CLI/<版本>` 追加行为）。服务端按该段识别桌面 CLI 通道：缺失时 /v3/config
 * 只下发旧版兼容模型清单（37 个，无 v4.1/5.3/hy4/modelPromotions），
 * 带上才下发完整清单（51 个）。实测 2026-09-10。
 */
export const WORKBUDDY_CLI_VERSION = '2.137.1';
/** 鉴权platform 参数（auth/state?platform=…） */
export const WORKBUDDY_PLATFORM = 'workbuddy';
/** 鉴权路径前缀（国内版 /plugin，国际版空串） */
export const DEFAULT_PREFIX_PATH = '/plugin';

/**
 * 版本（edition）配置：国内版 / 国际版协议完全一致，仅端点与客户端身份不同。
 * 数据取自各自安装包的 cli/product.json（实测 2026-09-11）：
 *   国内版 WorkBuddy（D:\Program Files\WorkBuddy）
 *     endpoint copilot.tencent.com / prefix /plugin / platform workbuddy    / UA 产品名 WorkBuddy     / v5.5.4
 *   国际版 WorkBuddy AI（C:\Program Files\WorkBuddyAI）
 *     endpoint www.workbuddy.ai    / prefix /plugin / platform workbuddy-ai / UA 产品名 WorkBuddy AI / v5.5.2
 * 两版 CLI 版本相同（2.137.1），UA 的 platform 段两版都用 "WorkBuddy"。
 */
export const EDITIONS = Object.freeze({
  cn: {
    id: 'cn',
    label: '国内版',
    endpoint: 'https://copilot.tencent.com',
    stagingEndpoint: 'https://staging-copilot.tencent.com',
    prefixPath: '/plugin',
    platform: 'workbuddy',
    uaPlatform: 'WorkBuddy',
    productName: 'WorkBuddy',
    clientVersion: WORKBUDDY_CLIENT_VERSION,
    cliVersion: WORKBUDDY_CLI_VERSION,
    dataFolderName: '.workbuddy',
  },
  intl: {
    id: 'intl',
    label: '国际版',
    endpoint: 'https://www.workbuddy.ai',
    stagingEndpoint: 'https://staging-codebuddy.tencent.com',
    prefixPath: '/plugin',
    platform: 'workbuddy-ai',
    uaPlatform: 'WorkBuddy',
    productName: 'WorkBuddy AI',
    clientVersion: '5.5.2',
    cliVersion: WORKBUDDY_CLI_VERSION,
    dataFolderName: '.workbuddy-ai',
  },
});

export const DEFAULT_EDITION = 'cn';

/** 宽松解析版本 id：接受 cn/intl 及一些常见写法，未知值回落国内版 */
export function resolveEdition(edition) {
  const id = String(edition ?? '').trim().toLowerCase();
  if (!id) return EDITIONS[DEFAULT_EDITION];
  if (EDITIONS[id]) return EDITIONS[id];
  if (['international', 'global', 'ai', 'workbuddy-ai', 'workbuddyai', 'en'].includes(id)) return EDITIONS.intl;
  if (['china', 'mainland', 'workbuddy', 'zh', 'cn-intl'].includes(id)) return EDITIONS.cn;
  return EDITIONS[DEFAULT_EDITION];
}

/** 带 CLI 扩展段的完整 UA（服务端按 CLI 段识别桌面通道；产品名/版本按版本区分） */
export function userAgentForEdition(edition) {
  const e = resolveEdition(edition);
  return `${e.uaPlatform}/${e.clientVersion} `
    + `${e.productName}/${e.clientVersion} CLI/${e.cliVersion}`;
}

/** 国内版 UA（兼容旧引用；等价 userAgentForEdition('cn')） */
export const WORKBUDDY_USER_AGENT = userAgentForEdition('cn');

/** 管理/计费接口成功码 */
export const RESPONSE_CODE_OK = 0;

// ─── 1. 鉴权：登录 / 账号 / token ──────────────────────────
// 全部带 prefixPath（国内版 = /v2/plugin/...）

/**
 * 登录流程（device-flow 风格，与桌面端 createSession 完全一致）：
 *   ① POST /v2{prefix}/auth/state?platform=workbuddy   （匿名）→ { state, authUrl }
 *   ② 浏览器打开 authUrl 完成登录
 *   ③ GET  /v2{prefix}/auth/token?state=<state>        （匿名）→ accessToken / refreshToken
 *   ④ GET  /v2{prefix}/login/account?state=<state>     （Bearer）→ 账号信息
 *   ⑤ GET  /v2{prefix}/accounts                        （Bearer）→ 账号列表
 */
export const AUTH = {
  /** ① 发起登录，返回 { state, authUrl }（匿名请求） */
  state: {
    method: 'POST',
    path: (prefix) => `/v2${prefix}/auth/state`,
    query: { platform: WORKBUDDY_PLATFORM },
    anonymous: true,
    note: '匿名：带 X-No-Authorization / X-No-User-Id / X-No-Enterprise-Id / X-No-Department-Info',
  },
  /** ③ 轮询取 token（匿名请求）；未完成登录时上游返回 code=11217，需继续轮询 */
  token: {
    method: 'GET',
    path: (prefix) => `/v2${prefix}/auth/token`,
    query: { state: '<state>' },
    anonymous: true,
    retryCode: 11217,
  },
  /** ④ 轮询取账号（Bearer）；账号未就绪时上游返回 code=12151 */
  account: {
    method: 'GET',
    path: (prefix) => `/v2${prefix}/login/account`,
    query: { state: '<state>' },
    retryCode: 12151,
  },
  /** token 续期：headers 带 X-Refresh-Token + X-Auth-Refresh-Source: plugin */
  refresh: {
    method: 'POST',
    path: (prefix) => `/v2${prefix}/auth/token/refresh`,
    headers: { 'X-Auth-Refresh-Source': 'plugin' },
    body: {},
    note: 'refreshToken 放在 X-Refresh-Token 头，不在 body',
  },
  /** 账号列表 */
  accounts: {
    method: 'GET',
    path: (prefix) => `/v2${prefix}/accounts`,
    note: '返回 data.accounts[]，含 uid / nickname / enterpriseId / type / pluginEnabled',
  },
  /** 切换企业/个人版本：个人版 /login/enterprise，企业版 /login/enterprise/{enterpriseId} */
  switchAccount: {
    method: 'POST',
    path: (prefix, enterpriseId = '') => `/v2${prefix}/login/enterprise${enterpriseId ? `/${encodeURIComponent(enterpriseId)}` : ''}`,
    body: {},
  },
  /** 微信侧登录：tmpCode 换 token（WorkBuddy 桌面端不用，保留备查） */
  wxToken: {
    method: 'POST',
    path: (prefix) => `/v2${prefix}/auth/token/wxide`,
    query: { tmpCode: '<code>' },
  },
};

// ─── 2. LLM：对话 / 补全 / 多模态 ───────────────────────────
// 不带 prefixPath，直接挂 {endpoint}/v2/...

export const LLM = {
  /** 对话（OpenAI 兼容，SSE 流式）；WorkBuddy 主链路 */
  chatCompletions: {
    method: 'POST',
    path: '/v2/chat/completions',
    accept: 'text/event-stream',
    note: 'body 为 OpenAI 兼容结构；上游仅支持 stream:true（stream:false 会返回 code=11101）',
  },
  /** 代码补全（旧版 VS Code 扩展链路，WorkBuddy 桌面端默认不走） */
  completions: { method: 'POST', path: '/v2/completions' },
  embeddings: { method: 'POST', path: '/v2/embeddings' },
  /** 文生图（插件 ImageGen 用，endpoint 同源） */
  imageGenerations: { method: 'POST', path: '/v2/images/generations' },
  imageEdits: { method: 'POST', path: '/v2/images/edits' },
  /** 视频生成：提交任务 + 轮询 */
  videoGenerations: { method: 'POST', path: '/v2/videos/generations' },
};

// ─── 3. 计费 / 积分 / 签到 ─────────────────────────────────
// 不带 prefixPath

export const BILLING = {
  /**
   * 签到活动状态：GET 之前的每日签到信息。
   * 返回 data 原样透出（含今日是否已签到、连续天数等，字段随活动配置变化）。
   */
  checkinStatus: {
    method: 'POST',
    path: '/v2/billing/meter/checkin-activity-status',
    body: {},
    note: 'AuthService.getCheckinStatus()；成功为 code===0 且 data 非空',
  },
  /** 领取每日签到积分。幂等：已领取时上游返回非 0 code + msg */
  dailyCheckin: {
    method: 'POST',
    path: '/v2/billing/meter/daily-checkin',
    body: {},
    note: 'AuthService.claimDailyCheckin()；返回 { code, msg, requestId }',
  },
  /**
   * 个人账号积分/资源：返回生效中的各类积分包（日额度/月会员/加油包…）。
   * body 固定常量（ProductCode=p_tcaca 为 WorkBuddy 产品码）。
   */
  userResource: {
    method: 'POST',
    path: '/v2/billing/meter/get-user-resource',
    body: {
      PageNumber: 1,
      PageSize: 100,
      ProductCode: 'p_tcaca',
      Status: [0, 3],
      OnlyValidPeriod: true,
    },
    note: 'AuthService.getPersonalUsage()；结果路径 data.Response.Data.Accounts[]',
  },
  /** 企业账号额度：limitNum === -1 表示不限量，credit 为已用 */
  enterpriseUsage: {
    method: 'POST',
    path: '/v2/billing/meter/get-enterprise-user-usage',
    body: {},
    note: 'AuthService.getEnterpriseUsage()；limitNum + credit + cycleResetTime',
  },
  /** 用量提示（CLI 端 productFeatures.BillingNotice 开启时轮询） */
  dosageNotify: {
    method: 'POST',
    path: '/v2/billing/meter/get-dosage-notify',
    body: {},
  },
};

// ─── 4. 活动 / 用户 ────────────────────────────────────────

export const ACTIVITY = {
  /** 运营 banner；需白名单头（X-IDE-Type / X-Product / User-Agent） */
  workbuddyBanner: {
    method: 'GET',
    path: '/v2/activity/workbuddy/banner',
    whitelistHeaders: true,
    note: 'AuthService.getActivityBanner()；缺 X-Product 等头会被白名单拦截',
  },
  /** 大使（推广）状态 */
  ambassadorStatus: { method: 'GET', path: '/v2/activity/ambassador/status' },
  /** 修改昵称（微信侧用户资料） */
  updateNickname: {
    method: 'POST',
    path: '/v2/as/wechatmp/user/profile/update',
    body: { nickname: '<new>' },
  },
};

// ─── 5. 配置 / 模型目录 ────────────────────────────────────

export const CONFIG = {
  /** 运行时配置 v3（含 models / productFeatures）——模型目录权威来源 */
  v3: { method: 'GET', path: '/v3/config' },
  /** 旧版配置 v2 */
  v2: { method: 'GET', path: '/v2/config' },
  /** 企业定制模型：{endpoint}/console/enterprises/{id}/config/models */
  enterpriseModels: {
    method: 'GET',
    path: (_prefix, enterpriseId) => `/console/enterprises/${encodeURIComponent(enterpriseId)}/config/models`,
  },
  /** 功能开关（灰度） */
  featureFlag: { method: 'GET', path: '/v2/feature-flag/api/product-config' },
  /** 更新检查 */
  update: { method: 'GET', path: '/v2/update', query: { platform: 'workbuddy-win32-x64', version: '<ver>' } },
};

// ─── 6. 云智能体 / 其他 ────────────────────────────────────

export const CLOUD_AGENT = {
  entitlement: { method: 'GET', path: '/v2/user/cloudagent/entitlement' },
  quota: { method: 'GET', path: '/v2/user/cloudagent/quota' },
  listAgents: { method: 'GET', path: '/v2/user/cloudagent/agents' },
  listInstances: { method: 'GET', path: '/v2/user/cloudagent/instances' },
  listConversations: { method: 'GET', path: '/v2/user/cloudagent/conversations' },
  listTasks: { method: 'GET', path: '/v2/user/cloudagent/tasks' },
};

// ─── 7. 本地 CLI sidecar（不是上游接口）────────────────────
// 桌面端 spawn `codebuddy --serve`，监听 127.0.0.1 随机端口，
// 由 CODEBUDDY_GATEWAY_PASSWORD 的 Bearer 保护。

export const SIDECAR = {
  acp: { method: 'POST', path: '/api/v1/acp', note: 'ACP 主通道，走 loopback 豁免' },
  health: { method: 'GET', path: '/api/v1/health' },
  llmCompletions: { method: 'POST', path: '/api/v1/llm/completions', note: '内部摘要生成，不经 requireAuth' },
};

// ─── 请求头常量 ────────────────────────────────────────────

/**
 * 转发 LLM 请求所必须的头（对齐 CLI ModelProviderImpl + 桌面端 buildHeaders）。
 */
export const AUTH_HEADERS = {
  USER_ID: 'X-User-Id',
  ENTERPRISE_ID: 'X-Enterprise-Id',
  TENANT_ID: 'X-Tenant-Id',
  DEPARTMENT_INFO: 'X-Department-Info',
  DOMAIN: 'X-Domain',
};

/** 客户端身份头（X-IDE-* / X-Product），服务端按此做客户端识别与白名单 */
export const CLIENT_HEADERS = {
  IDE_TYPE: 'X-IDE-Type',
  IDE_NAME: 'X-IDE-Name',
  IDE_VERSION: 'X-IDE-Version',
  PRODUCT: 'X-Product',
  PRODUCT_VERSION: 'X-Product-Version',
  AGENT_INTENT: 'X-Agent-Intent',
  API_KEY: 'X-API-Key',
};

/** 链路追踪 / 会话头（CLI 会注入，代理侧可选复刻） */
export const TRACE_HEADERS = {
  TRACE_ID: 'X-Trace-ID',
  REQUEST_ID: 'X-Request-ID',
  ROOT_REQUEST_ID: 'X-Root-Request-ID',
  CONVERSATION_ID: 'X-Conversation-ID',
  SESSION_ID: 'X-Session-ID',
  CONVERSATION_REQUEST_ID: 'X-Conversation-Request-ID',
  CONVERSATION_MESSAGE_ID: 'X-Conversation-Message-ID',
};

/** 匿名登录请求的跳过鉴权头 */
export function anonymousHeaders() {
  return {
    'X-No-Authorization': 'true',
    'X-No-User-Id': 'true',
    'X-No-Enterprise-Id': 'true',
    'X-No-Department-Info': 'true',
  };
}

// ─── 积分包商品码（账单里 PackageCode 的含义）──────────────

export const COMMODITY_CODES = {
  free: 'TCACA_code_001_PqouKr6QWV',
  proMon: 'TCACA_code_002_AkiJS3ZHF5',
  proYear: 'TCACA_code_003_FAnt7lcmRT',
  proMonPlus: 'TCACA_code_005_maRGyrHhw1',
  gift: 'TCACA_code_006_DbXS0lrypC',
  activity: 'TCACA_code_007_nzdH5h4Nl0',
  freeMon: 'TCACA_code_008_cfWoLwvjU4',
  extra: 'TCACA_code_009_0XmEQc2xOf',
  youth: 'TCACA_code_023_4xbGhMrE6q',
  advanced: 'TCACA_code_026_BaESVICNoi',
  flagship: 'TCACA_code_027_0FCGVA6vSa',
  bonus28: 'TCACA_code_028_NtpWi0jzXs',
  bonus29: 'TCACA_code_029_6wCGEWquYy',
  bonus30: 'TCACA_code_030_BjSt89qTvr',
  freeMonIntl: 'TCACA_code_035_ArVxJcGDsm',
  extraIntl: 'TCACA_code_036_lupO5WgNdG',
  bonusIntl: 'TCACA_code_037_WxOD3MpI2o',
  extra38: 'TCACA_code_038_OhvqZtiPKr',
  proTrialMon: 'TCACA_code_039_KRcQj7wUat',
  proTrialYear: 'TCACA_code_040_mi9rCYg46x',
};

/** 商品码 → 展示名 */
export const COMMODITY_LABELS = {
  [COMMODITY_CODES.free]: '免费版每日额度',
  [COMMODITY_CODES.proMon]: '专业版包月',
  [COMMODITY_CODES.proYear]: '专业版包年',
  [COMMODITY_CODES.proMonPlus]: '专业版包月（升级包）',
  [COMMODITY_CODES.gift]: '专业版体验',
  [COMMODITY_CODES.activity]: '成长计划',
  [COMMODITY_CODES.freeMon]: '专业版日额度',
  [COMMODITY_CODES.extra]: '积分加油包',
  [COMMODITY_CODES.youth]: '青春版',
  [COMMODITY_CODES.advanced]: '进阶版',
  [COMMODITY_CODES.flagship]: '旗舰版',
  [COMMODITY_CODES.bonus28]: '赠送积分（28）',
  [COMMODITY_CODES.bonus29]: '赠送积分（29）',
  [COMMODITY_CODES.bonus30]: '赠送积分（30）',
  [COMMODITY_CODES.freeMonIntl]: '国际版月额度',
  [COMMODITY_CODES.extraIntl]: '国际版加油包',
  [COMMODITY_CODES.bonusIntl]: '国际版赠送积分',
  [COMMODITY_CODES.extra38]: '积分加油包（38）',
  [COMMODITY_CODES.proTrialMon]: '专业版试用（月）',
  [COMMODITY_CODES.proTrialYear]: '专业版试用（年）',
};

/** 按日刷新的积分包（额度每天重置，无过期时间） */
export const DAILY_CREDITS = [COMMODITY_CODES.free];

/** 企业不限量哨兵（与桌面端 UNLIMITED_USAGE_SENTINEL 一致） */
export const UNLIMITED_USAGE_SENTINEL = 'unlimited';

/** 上游错误码（桌面端 ServerErrorCode） */
export const SERVER_CODES = {
  RetryFetchToken: 11217,
  RetryFetchAccount: 12151,
  LicenseSeatLimit: 12005,
  LicenseExpired: 11212,
  TrialExpired: 11216,
  IpLimit: 10081,
  NonStreamNotSupported: 11101,
  RateLimited: 11128,
};

/** 根据端点推断鉴权前缀（国内 /plugin，海外空） */
export function defaultPrefixPath(endpoint) {
  const host = String(endpoint || '').replace(/\/+$/, '');
  if (host === GLOBAL_ENDPOINT || host.startsWith('https://staging-codebuddy')) return '';
  return DEFAULT_PREFIX_PATH;
}

export function normalizeEndpoint(endpoint) {
  return String(endpoint || DEFAULT_ENDPOINT).replace(/\/+$/, '');
}
