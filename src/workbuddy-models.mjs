/**
 * WorkBuddy 模型目录
 *
 * 模型清单内嵌于 WorkBuddy 内置 CLI 的 product.json（v5.3.14）。
 * 这里收录主要对话/多模态模型，作为 /v1/models 响应与默认模型回退。
 *
 * 运行时权威来源：
 *   GET {endpoint}/v3/config            （Bearer + 官方 UA）→ data.models
 *   GET {console}/enterprises/{id}/config/models （企业定制模型）
 *
 * 注意：服务端按官方客户端 UA 区分来源，缺失 UA 时 /v3/config 的
 * models 可能返回 null，因此 refresh 时必须带 User-Agent。
 */

import { WORKBUDDY_USER_AGENT } from './workbuddy-endpoints.mjs';
import { proxyFetch } from './workbuddy-proxy.mjs';

const DEFAULT_UA = WORKBUDDY_USER_AGENT;

/**
 * 下游可见模型白名单：/v1/models 与路由解析只暴露这些模型。
 * 空数组 = 不限制，全量放行（/v3/config 拉回的真实清单，约 51 个模型）。
 * 需要收窄时填入模型 id，如 ['deepseek-v4.1-flash']。
 */
export const MODEL_ALLOWLIST = [];

/**
 * 内置模型目录（id → 元数据）。
 * credits 字段为桌面端展示的单价，仅作参考。
 */
const BUILTIN_MODELS = [
  { id: 'auto', name: 'Auto', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 168000, supportsToolCall: true, supportsImages: true, isDefault: true, descriptionZh: '平衡效果与速度' },
  { id: 'default', name: 'Default', vendor: 'v', credits: 'x2.00 credits', maxOutputTokens: 24000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: false },

  { id: 'deepseek-v4-pro', name: 'Deepseek-V4-Pro', vendor: 'f', credits: 'x0.16 credits', maxOutputTokens: 50000, maxInputTokens: 1000000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true, descriptionZh: 'DeepSeek 旗舰模型，支持 1M 上下文窗口' },
  { id: 'deepseek-v4-flash', name: 'Deepseek-V4-Flash', vendor: 'f', credits: 'x0.06 credits', maxOutputTokens: 50000, maxInputTokens: 1000000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true, descriptionZh: 'DeepSeek 旗舰模型，支持 1M 上下文窗口' },
  // deepseek-v4.1-flash 不在 5.3.14 内置清单/旧版 /v3/config 里，但上游实测可路由；
  // 作为远程清单不可用时的兜底定义（远程正常时以远程元数据为准）。
  { id: 'deepseek-v4.1-flash', name: 'Deepseek-V4.1-Flash', vendor: 'f', credits: 'x0.03 credits', maxOutputTokens: 50000, maxInputTokens: 1000000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true, descriptionZh: 'DeepSeek 快速模型，支持 1M 上下文窗口' },
  { id: 'deepseek-v3-2-volc', name: 'DeepSeek-V3.2', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 96000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },

  { id: 'glm-5.1', name: 'GLM-5.1', vendor: 'e', credits: '', maxOutputTokens: 48000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'glm-5.0', name: 'GLM-5.0', vendor: 'f', credits: '', maxOutputTokens: 48000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'glm-5.0-turbo', name: 'GLM-5.0-Turbo', vendor: 'e', credits: '', maxOutputTokens: 48000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'glm-5v-turbo', name: 'GLM-5v-Turbo', vendor: 'e', credits: '', maxOutputTokens: 38000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'glm-4.7', name: 'GLM-4.7', vendor: 'f', credits: '', maxOutputTokens: 48000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'glm-4.6', name: 'GLM-4.6', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 168000, supportsToolCall: true, supportsImages: true, supportsReasoning: true },
  { id: 'glm-4.6v', name: 'GLM-4.6V', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 128000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },

  { id: 'minimax-m2.5', name: 'MiniMax-M2.5', vendor: 'f', credits: '', maxOutputTokens: 48000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'minimax-m2.7', name: 'MiniMax-M2.7', vendor: 'f', credits: '', maxOutputTokens: 48000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'minimax-m3-play', name: 'MiniMax-M3', vendor: 'f', credits: 'x0.25 credits', maxOutputTokens: 48000, maxInputTokens: 512000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true, descriptionZh: '原生多模态，擅长代码、智能体任务' },

  { id: 'kimi-k2.6', name: 'Kimi-K2.6', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 256000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'kimi-k2.5', name: 'Kimi-K2.5', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 256000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'kimi-k2-thinking', name: 'Kimi-K2-Thinking', vendor: 'f', credits: '', maxOutputTokens: 32000, maxInputTokens: 256000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'kimi-k2-instruct-taiji', name: 'Kimi-K2', vendor: 'f', credits: '', maxOutputTokens: 8192, maxInputTokens: 31000, supportsToolCall: true, supportsImages: true },

  { id: 'hy3-preview', name: 'Hy3 preview', vendor: 'j', credits: '', maxOutputTokens: 64000, maxInputTokens: 192000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'hy3-preview-agent', name: 'Hy3 preview', vendor: 'j', credits: 'x0.04 credits', maxOutputTokens: 64000, maxInputTokens: 192000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'hunyuan-chat', name: 'Hunyuan-Turbos', vendor: 'j', credits: '', maxOutputTokens: 8192, maxInputTokens: 128000, supportsToolCall: true, supportsImages: true, descriptionZh: '腾讯自研的轻量、快速的通用模型' },
  { id: 'hunyuan-2.0-instruct', name: 'Hunyuan-2.0-Instruct', vendor: 'j', credits: '', maxOutputTokens: 16000, maxInputTokens: 128000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'hunyuan-2.0-thinking', name: 'Hunyuan-2.0-Thinking', vendor: 'e', credits: '', maxOutputTokens: 24000, maxInputTokens: 128000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },

  { id: 'default-1.1', name: 'Claude-3.7-Sonnet', vendor: 'e', credits: '', maxOutputTokens: 8192, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },
  { id: 'default-1.2', name: 'Claude-4.0-Sonnet', vendor: 'e', credits: '', maxOutputTokens: 24000, maxInputTokens: 200000, supportsToolCall: true, supportsImages: true, supportsReasoning: true, onlyReasoning: true },

  // 图像 / 视频生成（非对话模型，/v1/models 里也列出便于选择）
  { id: 'hunyuan-image-v3.0', name: 'Hunyuan-Image-V3', vendor: 'j', credits: '', kind: 'image' },
  { id: 'kling-v3-t2v', name: 'Kling-V3-T2V', vendor: 'k', credits: '', kind: 'video' },
  { id: 'kling-v3-i2v', name: 'Kling-V3-I2V', vendor: 'k', credits: '', kind: 'video' },
];

export class ModelRouteError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ModelRouteError';
  }
}

/**
 * 对话模型判定：网关只服务对话模型，非对话项（补全/内部工具/图像小模型）
 * 不进目录 —— /v1/models、/api/session 与路由解析三者保持一致。
 * 判定依据来自 /v3/config 下发的模型元数据（实测 2026-09-10）：
 *   - 内部补全/工具模型带 supportsExtra，输出上限 ≤8192
 *   - 图像模型没有 maxOutputTokens（只有 id/name/tags）
 *   - 旧一代小输出对话模型（r1-0528 / v3-0324 / kimi-k2-instruct / Claude 内部 id）
 *     由黑名单排除，官方客户端 UI 也不展示它们
 */
const CHAT_MODEL_EXCLUDE_RE =
  /^(completion-|codewise-|hunyuan-image|hunyuan-3b|hunyuan-7b|deepseek-r1-0528|deepseek-v3-0324|kimi-k2-instruct|default-1\.)/;

export function isChatModel(model = {}) {
  if (CHAT_MODEL_EXCLUDE_RE.test(String(model.id || ''))) return false;
  if (model.supportsExtra) return false;
  if (typeof model.maxOutputTokens !== 'number') return false;
  return model.maxOutputTokens >= 16000;
}

/** 模型 id 归一化：小写 + 去掉分隔符，deepseek-v4.1-flash → deepseekv41flash */
function normalizeModelId(value) {
  return String(value ?? '').toLowerCase().replace(/[^a-z0-9]/g, '');
}

/** 经典 Levenshtein 距离（模型 id 都很短，O(nm) 足够） */
function editDistance(a, b) {
  if (a === b) return 0;
  if (!a.length) return b.length;
  if (!b.length) return a.length;
  let prev = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    const cur = [i];
    for (let j = 1; j <= b.length; j++) {
      cur[j] = Math.min(
        prev[j] + 1,
        cur[j - 1] + 1,
        prev[j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1),
      );
    }
    prev = cur;
  }
  return prev[b.length];
}

function commonPrefixLength(a, b) {
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  return i;
}

export function createModelCatalog({ verbose = () => {}, fallbackModels } = {}) {
  /** 目录准入过滤：对话模型判定 + 白名单（空白名单全量放行） */
  const applyFilters = models => models
    .filter(m => isChatModel(m))
    .filter(m => (MODEL_ALLOWLIST.length ? MODEL_ALLOWLIST.includes(m.id) : true));
  // 白名单模型必须有可用定义：远程/传入清单缺失时回退内置定义
  const allowlistFallback = () => BUILTIN_MODELS.filter(m => MODEL_ALLOWLIST.includes(m.id) && isChatModel(m));

  let catalog = (() => {
    const base = Array.isArray(fallbackModels) && fallbackModels.length ? fallbackModels : BUILTIN_MODELS;
    const allowed = applyFilters(base);
    return allowed.length ? allowed : allowlistFallback();
  })();
  let lastRefreshedAt = 0;
  let remoteRefreshed = false;

  function get(id) {
    if (!id) return null;
    const lower = String(id).toLowerCase();
    return catalog.find(m => m.id.toLowerCase() === lower)
      || catalog.find(m => String(m.name || '').toLowerCase() === lower)
      || null;
  }

  function has(id) {
    return !!get(id);
  }

  /**
   * 解析模型 id：只接受目录里真实存在的 id（大小写不敏感）。
   * 未知名直接抛 ModelRouteError —— 绝不静默改写/回退：
   * 用户点名要 A 就必须路由到 A，不存在就报错让下游自己改，
   * 静默换成别的模型（哪怕同族近似）只会造成"请求 4.1 实跑 V4"这类事故。
   */
  function resolve(id) {
    const hit = get(id);
    if (hit) return hit.id;
    throw new ModelRouteError(`模型不存在: ${id}，可用模型见 GET /v1/models`);
  }

  /**
   * 仅供 400 报错信息用的"你是不是想要"提示：列出目录里与请求名最相近的 id。
   * 结果不参与路由 —— 路由永远只认精确存在的模型。
   */
  function suggest(id, limit = 5) {
    const target = normalizeModelId(id);
    if (!target) return [];
    return catalog
      .map(m => {
        const cand = normalizeModelId(m.id);
        return {
          id: m.id,
          dist: editDistance(target, cand),
          prefix: commonPrefixLength(target, cand),
          startsWith: cand.startsWith(target) || target.startsWith(cand),
        };
      })
      .filter(x => x.prefix >= 4
        && (x.startsWith || x.dist <= Math.max(3, Math.floor(target.length * 0.4))))
      .sort((a, b) => a.dist - b.dist || b.prefix - a.prefix)
      .slice(0, limit)
      .map(x => x.id);
  }

  function list() {
    return catalog.slice();
  }

  function listResponse() {
    return {
      object: 'list',
      data: catalog.map(m => ({
        id: m.id,
        object: 'model',
        created: 0,
        owned_by: 'workbuddy',
        name: m.name,
        credits: m.credits || '',
        max_output_tokens: m.maxOutputTokens,
        max_input_tokens: m.maxInputTokens,
        supports_tool_call: !!m.supportsToolCall,
        supports_images: !!m.supportsImages,
        supports_reasoning: !!m.supportsReasoning,
        is_default: !!m.isDefault,
        kind: m.kind || 'chat',
      })),
      meta: { source: remoteRefreshed ? 'remote' : 'builtin', lastRefreshedAt, count: catalog.length },
    };
  }

  /**
   * 远程配置刷新（与桌面端行为一致）：
   *   1) GET {endpoint}/v3/config  → data.models 为运行时清单（含 auto 等服务端下发模型）
   *   2) 企业账号再取 /console/enterprises/{id}/config/models
   * 两者都不可用时保留内置目录。拉回的清单先过 MODEL_ALLOWLIST 白名单，
   * 白名单模型缺失时不采用远程清单（保留内置定义），保证 /v1/models 稳定可见。
   */
  async function refresh({ endpoint, enterpriseId, headers = {}, userAgent = DEFAULT_UA, fetchImpl = globalThis.fetch, proxy = null } = {}) {
    // 出网统一走 proxyFetch（undici）以便按账号挂代理；单测替身 fetchImpl 优先
    const send = (fetchImpl && fetchImpl !== globalThis.fetch) ? fetchImpl : proxyFetch;
    const base = String(endpoint || '').replace(/\/+$/, '');
    if (base && headers.Authorization) {
      try {
        const resp = await send(`${base}/v3/config`, {
          headers: { Accept: 'application/json', 'User-Agent': userAgent, ...headers },
        }, proxy);
        const payload = await resp.json();
        const models = payload?.data?.models;
        const allowed = Array.isArray(models) ? applyFilters(models) : [];
        if (allowed.length) {
          // 默认模型统一为 auto（远程可能把 fast-model 等档位模型标为默认，与网关默认不一致）
          catalog = allowed.map(m => ({
            ...m,
            isDefault: String(m.id || '').toLowerCase() === 'auto',
            modelType: String(m.id || '').toLowerCase() === 'auto' ? 'auto' : 'built-in',
          }));
          lastRefreshedAt = Date.now();
          remoteRefreshed = true;
          return { refreshed: true, count: allowed.length, source: 'v3/config' };
        }
        verbose('[Models]', `远程清单(${Array.isArray(models) ? models.length : 0} 个)不含可用对话模型，保留内置目录`);
      } catch (error) {
        verbose('[Models]', `远程配置拉取失败: ${error.message}`);
      }
    }
    if (base && enterpriseId) {
      const url = `${base}/console/enterprises/${encodeURIComponent(enterpriseId)}/config/models`;
      try {
        const resp = await send(url, { headers }, proxy);
        const payload = await resp.json();
        const models = payload?.data;
        const allowed = Array.isArray(models) ? applyFilters(models) : [];
        if (allowed.length) {
          catalog = allowed.map(m => ({
            ...m,
            isDefault: String(m.id || '').toLowerCase() === 'auto',
            modelType: String(m.id || '').startsWith('custom:') ? 'enterprise' : 'built-in',
          }));
          lastRefreshedAt = Date.now();
          remoteRefreshed = true;
          return { refreshed: true, count: allowed.length, source: 'enterprise' };
        }
        return { refreshed: false, reason: '企业模型列表为空' };
      } catch (error) {
        verbose('[Models]', `企业模型目录刷新失败: ${error.message}`);
        return { refreshed: false, reason: error.message };
      }
    }
    return { refreshed: false, reason: '缺少登录态，使用内置模型目录' };
  }

  return {
    get,
    has,
    resolve,
    suggest,
    list,
    listResponse,
    refresh,
    get lastRefreshedAt() { return lastRefreshedAt; },
    get isRemote() { return remoteRefreshed; },
  };
}

/** 供 CLI/调试：直接拿到内置清单 */
export function builtinModels() {
  return BUILTIN_MODELS.slice();
}
