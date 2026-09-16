/**
 * WorkBuddy 上游 LLM 客户端 — 请求头复刻 + 透明转发（JSON / SSE）
 *
 * 端点（不带 prefixPath，直接挂 {endpoint}/v2/...）：
 *   POST {endpoint}/v2/chat/completions   对话（OpenAI 兼容 body，SSE）
 *
 * 只实现对话这一条链路：其他 OpenAI 兼容路径（completions / embeddings /
 * images / videos）虽然出现在逆向出的接口清单里，但没有实测验证过上游是否
 * 真的开放、body 结构是否一致，因此不对外暴露，也不在这里实现转发。
 *
 * 鉴权头（对齐桌面端 AuthService.buildHeaders + CLI ModelProviderImpl）：
 *   Authorization: Bearer <accessToken>
 *   X-User-Id: <uid>
 *   X-Enterprise-Id / X-Tenant-Id: <enterpriseId>   （企业账号）
 *   X-Domain: <domain>
 *   User-Agent / X-IDE-Type / X-IDE-Name / X-IDE-Version / X-Product
 *
 * 会话与链路头（CLI 会注入，这里按需复刻，便于服务端侧统计与排障）：
 *   X-Conversation-ID / X-Session-ID
 *   X-Conversation-Request-ID / X-Conversation-Message-ID
 *   X-Request-ID / X-Root-Request-ID
 *   X-Agent-Intent: craft
 *
 * 流式响应：SSE 透传，唯一改写是把上游 1-2 词一帧的 reasoning 小分片
 * 合并成大帧（见 REASONING_COALESCE_CHARS），避免客户端把思考渲染成碎块。
 * 客户端断开时通过 AbortController 取消上游请求。
 *
 * 账号与出网：
 *   转发前按「优先级 → 加入顺序」挑选账号（禁用/限额中的账号不参与），
 *   429 时标记限额并降级到下一个候选；每个账号可用自己的代理出网
 *   （账号 proxy 为空则直连），出网统一走 workbuddy-proxy 的 proxyFetch，
 *   因为 Node 内置 fetch 不接受 dispatcher（无法按请求指定代理）。
 */

import { Readable, Transform } from 'node:stream';
import { randomUUID } from 'node:crypto';
import { resolveEdition, userAgentForEdition } from './workbuddy-endpoints.mjs';
import { proxyFetch, proxyErrorDetail } from './workbuddy-proxy.mjs';
import { pickAccountByPriority, rateLimitResetAt } from './workbuddy-routing.mjs';

/**
 * 上游风控码 11128 = Illegal API invocation from an unapproved channel。
 * 实测为频率风控的临时冷却窗口：客户端失败自动重试会形成重试风暴，
 * 且拉黑期间的每次重试都会给黑名单续期。因此退避间隔必须足够长，
 * 仅重试 2 次，让黑名单自然过期。
 */
const RATE_LIMIT_CODE = 11128;
const WAF_RETRY_DELAYS_MS = [10_000, 25_000];

/** 上游仅支持流式：stream:false 会返回 code=11101 */
const NON_STREAM_NOT_SUPPORTED_CODE = 11101;

/**
 * 上游要求首条消息必须是 system prompt，否则返回 400
 * （first message is not system prompt）。客户端没带 system 消息时，
 * 注入一条兜底系统消息，让纯 user 开头的请求也能正常转发。
 */
export const DEFAULT_SYSTEM_PROMPT = '你是一个得力助手';

/**
 * 首条消息不是 system 时，在头部补一条系统消息（不改原数组，返回新 body）。
 * 已经是 system 开头、messages 为空/缺失时原样返回（同一引用，便于调用方判断是否改写）——
 * 空 messages 属客户端异常输入，交给上游按原样报错，不在这里改写。
 */
export function ensureLeadingSystemMessage(body, prompt = DEFAULT_SYSTEM_PROMPT) {
  if (!body || typeof body !== 'object' || !Array.isArray(body.messages) || !body.messages.length) return body;
  const firstRole = String(body.messages[0]?.role ?? '').toLowerCase();
  if (firstRole === 'system') return body;
  return { ...body, messages: [{ role: 'system', content: prompt }, ...body.messages] };
}

/**
 * 上游限额码 6004（HTTP 429）：账号×模型维度的用量限额，
 * msg 形如 "您的使用量已超出频率限制，将在 2026-09-11 19:43:46 UTC+8 重置，
 * 您也可以切换其他模型继续使用。" —— 恢复时间只在文本里，需正则解析。
 */
const QUOTA_LIMIT_CODE = 6004;

/** 从限额提示文本解析恢复时间戳（UTC+8）；解析失败返回 0 */
export function parseQuotaResetAt(text) {
  const m = /(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2}):(\d{2})\s*UTC\+8/.exec(String(text || ''));
  if (!m) return 0;
  const at = Date.parse(`${m[1]}-${m[2]}-${m[3]}T${m[4]}:${m[5]}:${m[6]}+08:00`);
  return Number.isFinite(at) ? at : 0;
}

/** 是否为账号限额错误（HTTP 429 或上游 code 6004） */
export function isQuotaLimitError(error) {
  if (!error) return false;
  if (Number(error.statusCode) === 429) return true;
  return Number(error.upstreamCode) === QUOTA_LIMIT_CODE;
}

/**
 * 思考（reasoning_content）帧合并阈值：上游把思考拆成 1-2 个词一帧的小分片，
 * 部分客户端按 SSE 帧渲染思考块，会把一句思考碎成几十个小块。
 * 透传时把相邻 reasoning 帧攒到 ≥ 该字符数（或遇到非 reasoning 事件/流结束）再下发，
 * 只影响分帧粒度，不改变任何内容。
 */
const REASONING_COALESCE_CHARS = 60;

function sseFrame(obj) {
  return Buffer.from(`data: ${JSON.stringify(obj)}\n\n`, 'utf8');
}

/**
 * SSE reasoning 帧合并 Transform。
 * - reasoning_content 增量按帧累积，攒满 REASONING_COALESCE_CHARS 或
 *   遇到 content/tool_calls/finish/usage/[DONE] 等事件时冲刷为一个合并帧；
 * - 其余事件原样透传；帧元数据（id/model/created）取自上游最近一帧。
 */
function createReasoningCoalescingStream() {
  let acc = '';
  let meta = { id: null, model: null, created: null };
  let tail = '';

  return new Transform({
    transform(chunk, _enc, cb) {
      tail += chunk.toString('utf8');
      const lines = tail.split('\n');
      tail = lines.pop() ?? '';
      try {
        for (const raw of lines) {
          const line = raw.replace(/\r$/, '');
          if (!line.trim()) continue;
          if (!line.startsWith('data:')) { this.push(Buffer.from(line + '\n\n', 'utf8')); continue; }
          const data = line.slice(5).trim();
          if (data === '[DONE]') {
            if (acc) { this.push(sseFrame(coalescedChunk())); acc = ''; }
            this.push(Buffer.from('data: [DONE]\n\n', 'utf8'));
            continue;
          }
          let chunkObj;
          try { chunkObj = JSON.parse(data); } catch { this.push(Buffer.from(line + '\n\n', 'utf8')); continue; }
          if (chunkObj && typeof chunkObj === 'object') {
            if (chunkObj.id) meta.id = chunkObj.id;
            if (chunkObj.model) meta.model = chunkObj.model;
            if (chunkObj.created) meta.created = chunkObj.created;
          }
          const delta = chunkObj?.choices?.[0]?.delta;
          const reasoning = delta && typeof delta.reasoning_content === 'string' ? delta.reasoning_content : null;
          const otherDelta = delta && (typeof delta.content === 'string' && delta.content.length > 0
            || (delta.tool_calls?.length)
            || delta.function_call);
          if (reasoning !== null && !otherDelta) {
            acc += reasoning;
            if (acc.length >= REASONING_COALESCE_CHARS) {
              this.push(sseFrame(coalescedChunk()));
              acc = '';
            }
            continue;
          }
          if (acc) { this.push(sseFrame(coalescedChunk())); acc = ''; }
          this.push(Buffer.from(line + '\n\n', 'utf8'));
        }
        cb();
      } catch (error) { cb(error); }
    },
    flush(cb) {
      if (acc) { this.push(sseFrame(coalescedChunk())); acc = ''; }
      cb();
    },
  });

  function coalescedChunk() {
    return {
      id: meta.id || `wb-coalesce-${Date.now()}`,
      object: 'chat.completion.chunk',
      created: meta.created || Math.floor(Date.now() / 1000),
      model: meta.model || '',
      choices: [{ index: 0, delta: { reasoning_content: acc }, finish_reason: null }],
    };
  }
}

function sleep(ms, controller) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    controller?.signal?.addEventListener('abort', () => {
      clearTimeout(timer);
      reject(new Error('aborted'));
    }, { once: true });
  });
}

export class WorkBuddyUpstreamError extends Error {
  constructor(message, { statusCode, upstreamCode, body } = {}) {
    super(message);
    this.name = 'WorkBuddyUpstreamError';
    this.statusCode = statusCode;
    this.upstreamCode = upstreamCode;
    this.body = body;
  }
}

export function createWorkBuddyUpstreamClient({
  auth,
  accountStore = null,
  log = () => {},
  verbose = () => {},
  logEvent = null,
  fetchImpl = globalThis.fetch,
} = {}) {
  if (!auth) throw new Error('createWorkBuddyUpstreamClient 需要传入 auth 提供者');

  /**
   * 账号轮换：请求遇 429/code 6004 时给该账号打上模型级限额标记（含恢复时间），
   * 再按优先级挑选下一个可用账号重试；所有候选都不可用时透传错误。
   * 标记只影响选路，不改写「当前账号」（当前账号由优先级派生，与模型无关）。
   * 依赖 accountStore 提供 id/状态；无 store（如单账号 auth.json）时保持原行为。
   */
  function pickNextAccount(model, triedIds) {
    if (!accountStore) return null;
    try {
      const { accounts } = accountStore.listAccounts();
      return pickAccountByPriority(accounts, { model, excludeIds: triedIds });
    } catch (error) {
      verbose('[Upstream]', `账号列表读取失败: ${error.message}`);
      return null;
    }
  }

  /**
   * 本次请求使用的账号：多账号时按优先级选，无账号列表时回落到默认登录态。
   * 返回 { accountId, account, proxy, proxyError }；accountId 为 null 表示用默认会话
   * （环境变量 WORKBUDDY_TOKEN 或 auth.json 单账号模式）。
   *
   * 三级顺序，保证「禁用」语义不被绕过：
   *   1. 按优先级选（跳过禁用与限额冷却中的账号）；
   *   2. 启用中的账号都在该模型限额期内 → 挑恢复最早的一个试一次，
   *      把上游真实的 429（含恢复时间）透传给客户端；
   *   3. 全部禁用 → 明确报错，绝不回退到已禁用账号。
   */
  function selectTargetAccount(model, triedIds) {
    if (!accountStore) return { accountId: null, account: null, proxy: null, proxyError: null };

    // 逐个候选向前找：优先级升序，跳过没有可用凭证的记录（避免选到空账号）
    const excluded = [...triedIds];
    for (;;) {
      const picked = pickNextAccount(model, excluded);
      if (!picked) break;
      const entry = accountStore.getSessionById(picked.id);
      if (entry) return withProxyNotice(picked, entry, picked.id);
      excluded.push(picked.id);
    }

    const all = accountStore.listAccounts().accounts;
    if (!all.length) return { accountId: null, account: null, proxy: null, proxyError: null };
    const enabled = all.filter(account => account.enabled !== false);
    if (!enabled.length) {
      throw new WorkBuddyUpstreamError(
        '所有账号均已禁用，无账号可转发：请在账号页启用至少一个账号',
        { statusCode: 503 },
      );
    }

    // 启用中的账号都在限额冷却期内：仍用恢复最早的那个试一次，
    // 上游若已实际解除限额可直接成功，否则把真实 429 与恢复时间返回给客户端
    const bestEffort = enabled
      .filter(account => !triedIds.includes(account.id))
      .sort((a, b) => (rateLimitResetAt(a, model) || Infinity) - (rateLimitResetAt(b, model) || Infinity))[0];
    if (bestEffort) {
      const entry = accountStore.getSessionById(bestEffort.id);
      if (entry) return withProxyNotice(bestEffort, entry, bestEffort.id);
    }
    return { accountId: null, account: null, proxy: null, proxyError: null };
  }

  /** 组装选路结果，代理解析失败时记一条日志（本次回退直连） */
  function withProxyNotice(account, entry, accountId) {
    if (entry.proxyError) {
      log('[Upstream]', `⚠️ 账号「${account.name || accountId}」代理不可用，本次直连: ${entry.proxyError}`);
    }
    return { accountId, account, proxy: entry.proxy, proxyError: entry.proxyError };
  }

  /** 记录限额并返回可读的恢复时间文本 */
  function markAccountLimited(accountId, model, error) {
    if (!accountStore || !accountId) return '';
    const detail = error?.body ? (() => { try { return JSON.parse(error.body); } catch { return null; } })() : null;
    const message = detail?.msg || detail?.message || error?.message || '';
    const resetAt = parseQuotaResetAt(message);
    const entry = accountStore.markRateLimited(accountId, model, {
      status: error?.statusCode || 429,
      code: error?.upstreamCode ?? detail?.code ?? null,
      resetAt,
      message,
    });
    return entry?.resetAt ? new Date(entry.resetAt).toLocaleString('zh-CN', { hour12: false }) : '';
  }

  /**
   * 429 限额事件上报到运行日志（桌面端「日志」页）。
   * 结构化 data 让前端能直接展示「账号 A → 账号 B」的切换链路与恢复时间。
   */
  function reportLimitEvent({ message, level = 'warn', from, to, model, error, resetAt, priority }) {
    if (!logEvent) return;
    const detail = error?.body ? (() => { try { return JSON.parse(error.body); } catch { return null; } })() : null;
    logEvent({
      level,
      category: 'account',
      message,
      data: {
        model: String(model || ''),
        from: from?.label || '',
        to: to?.label || '',
        priority: priority ?? null,
        status: error?.statusCode || 429,
        code: error?.upstreamCode ?? detail?.code ?? null,
        resetAt: Number(resetAt) || 0,
        resetAtText: Number(resetAt) ? new Date(resetAt).toLocaleString('zh-CN', { hour12: false }) : '',
      },
    });
  }

  function chatCompletionsUrl(session) {
    return `${session?.endpoint || auth.baseUrl}/v2/chat/completions`;
  }

  async function requireSession() {
    const session = await auth.getCurrentSession();
    if (!session?.auth?.accessToken) {
      throw new WorkBuddyUpstreamError(
        '当前没有可用登录态：先执行 node server.mjs --login，或设置 WORKBUDDY_TOKEN',
        { statusCode: 401 },
      );
    }
    return session;
  }

  /**
   * 客户端身份 + 鉴权 + 会话追踪头。
   * requestId 用于把同一轮对话串起来，便于上游排障。
   * 身份（UA / X-IDE-* / X-Product）按账号版本区分：国内版 WorkBuddy、国际版 WorkBuddy AI。
   */
  function buildHeaders(session, { accept, requestId, conversationId } = {}) {
    const id = requestId || randomUUID();
    const e = resolveEdition(session?.edition);
    const headers = {
      'Content-Type': 'application/json',
      // 完整三段 UA（含 CLI 扩展段）：与桌面客户端一致，服务端按此识别通道
      'User-Agent': userAgentForEdition(e.id),
      // 客户端身份头：服务端按此做客户端识别与白名单校验
      'X-IDE-Type': e.uaPlatform,
      'X-IDE-Name': e.productName,
      'X-IDE-Version': e.clientVersion,
      'X-Product': e.productName,
      'X-Agent-Intent': 'craft',
      // 会话追踪
      'X-Request-ID': id,
      'X-Conversation-Request-ID': id,
      'X-Conversation-ID': conversationId || id,
      'X-Session-ID': conversationId || id,
      ...auth.buildAuthHeaders(session),
    };
    if (accept) headers.Accept = accept;
    return headers;
  }

  /**
   * 构建一次上游请求：优先用指定账号的会话（转发选路结果），
   * 没有账号记录时回落到默认登录态（环境变量 / auth.json）。
   * 返回 { session, init, proxy } —— proxy 为该账号解析好的出网代理（null=直连）。
   */
  async function buildRequestInit({ body, signal, accept, requestId, conversationId, accountId = null, proxy = null }) {
    let session = accountId ? accountStore?.getSessionById(accountId)?.session : null;
    if (!session) session = await requireSession();
    return {
      session,
      proxy,
      init: {
        method: 'POST',
        headers: buildHeaders(session, { accept, requestId, conversationId }),
        body: JSON.stringify(body),
        signal,
      },
    };
  }

  /** 读取上游错误响应为文本，尽量解析出 { code, message } */
  async function readUpstreamError(response) {
    const text = await response.text().catch(() => '');
    let code;
    let message = text.slice(0, 500);
    try {
      const payload = text ? JSON.parse(text) : null;
      code = payload?.code;
      message = payload?.message || payload?.msg || payload?.error?.message || message;
    } catch { /* 非 JSON，保留原文 */ }
    return { code, message, body: text };
  }

  /**
   * 带风控退避的上游请求：11128 拦截时按 10s/25s 退避重试（最多 2 次），
   * 其余错误原样抛出。
   * 按账号代理出网 —— proxy 为 null 时直连（仍走 undici，保证流式/超时行为一致）。
   */
  async function requestWithWafRetry(url, init, controller, proxy = null) {
    for (let attempt = 0; ; attempt++) {
      const response = await fetchViaProxy(url, init, proxy);
      if (response.ok) return response;
      const detail = await readUpstreamError(response);
      if (Number(detail.code) === RATE_LIMIT_CODE && attempt < WAF_RETRY_DELAYS_MS.length) {
        const delay = WAF_RETRY_DELAYS_MS[attempt];
        log('[Upstream]', `⚠️ 上游风控拦截(11128)，${delay / 1000}s 后重试（第 ${attempt + 1}/${WAF_RETRY_DELAYS_MS.length} 次）`);
        await sleep(delay, controller);
        continue;
      }
      log('[Upstream]', `上游错误 HTTP ${response.status}: ${detail.message}`);
      throw new WorkBuddyUpstreamError(
        `上游返回 ${response.status}: ${detail.message}`,
        { statusCode: response.status, upstreamCode: detail.code, body: detail.body },
      );
    }
  }

  /**
   * 出网：走 undici（可按账号挂代理），网络层错误统一收敛成可读信息。
   * 注入了 fetchImpl（测试替身）时直接用它，代理参数不参与 —— 单测不打真实网络。
   */
  async function fetchViaProxy(url, init, proxy) {
    if (fetchImpl !== globalThis.fetch) return fetchImpl(url, init);
    try {
      return await proxyFetch(url, init, proxy);
    } catch (error) {
      if (init?.signal?.aborted) throw error;
      // 代理/网络层错误（ECONNREFUSED、代理鉴权失败等）：带上根因，便于判断是不是代理配错了
      const detail = proxyErrorDetail(error);
      const via = proxy ? `经代理 ${proxy.label || proxy.host}` : '直连';
      throw new WorkBuddyUpstreamError(`上游请求失败（${via}）: ${detail}`, { statusCode: 502 });
    }
  }

  // ─── 请求合并（single-flight）──────────────────────────
  // 客户端失败后常对同一 body 快速重试，高频重复请求会触发上游 11128
  // 频率风控并互相续期。相同请求体在代理内排队串行，把重试风暴压成逐个执行。
  const inFlight = new Map();
  const INFLIGHT_WAIT_MS = 45_000;

  async function waitForInFlight(key) {
    const existing = key && inFlight.get(key);
    if (!existing) return;
    log('[Upstream]', '⏳ 检测到相同请求正在处理，排队等待（防重试风暴）');
    await Promise.race([
      existing.catch(() => {}),
      new Promise(resolve => setTimeout(resolve, INFLIGHT_WAIT_MS)),
    ]);
  }

  function trackInFlight(key, promise) {
    if (!key) return;
    inFlight.set(key, promise.finally(() => inFlight.delete(key)).catch(() => {}));
  }

  /**
   * 转发 OpenAI 兼容的 chat/completions。
   * 上游仅支持流式（stream:false 返回 code=11101）：
   *   - 客户端 stream=true  → SSE 原样透传
   *   - 客户端 stream=false → 代理内部以流式请求上游，聚合成完整 JSON 返回
   */
  async function forwardChatCompletions({ req, res, body, controller, dedupeKey }) {
    await waitForInFlight(dedupeKey);
    const promise = doForwardChatCompletions({ req, res, body, controller });
    trackInFlight(dedupeKey, promise.catch(() => {}));
    return promise;
  }

  async function doForwardChatCompletions({ req, res, body, controller }) {
    const stream = body.stream === true;
    // 无论客户端要不要流式，上游都必须以 stream:true 请求
    let upstreamBody = { ...body, stream: true };
    // 上游要求首条消息是 system prompt：客户端没带时补一条兜底系统消息
    const withSystem = ensureLeadingSystemMessage(upstreamBody);
    if (withSystem !== upstreamBody) {
      upstreamBody = withSystem;
      verbose('[Upstream]', `首条消息非 system，已注入系统消息：${DEFAULT_SYSTEM_PROMPT}`);
    }
    const model = body.model || '';
    const triedIds = [];
    let response = null;
    let session = null;
    let activeProxy = null;

    // 选路循环：按优先级选账号 → 429 标记限额 → 降级到下一候选重试（每次重建鉴权头与出口）
    for (;;) {
      const target = selectTargetAccount(model, triedIds);
      const accountId = target.accountId;
      activeProxy = target.proxy;
      const { session: currentSession, init } = await buildRequestInit({
        body: upstreamBody,
        signal: controller.signal,
        accept: 'text/event-stream',
        accountId,
        proxy: activeProxy,
      });
      session = currentSession;
      // 端点按账号走：国内版账号 → copilot.tencent.com，国际版账号 → www.workbuddy.ai
      const url = chatCompletionsUrl(session);
      verbose('[Upstream]',
        `POST ${url} model=${model || '(默认)'} stream=${stream} uid=${session.account?.uid || '-'} `
        + `priority=${target.account?.priority ?? '-'} 出口=${activeProxy ? activeProxy.label : '直连'} `
        + `msgs=${Array.isArray(body.messages) ? body.messages.length : '?'}`);

      const startedAt = Date.now();
      try {
        response = await requestWithWafRetry(url, init, controller, activeProxy);
      } catch (error) {
        if (controller.signal.aborted) return;
        if (!(error instanceof WorkBuddyUpstreamError)) {
          throw new WorkBuddyUpstreamError(`上游请求失败: ${error.message}`, { statusCode: 502 });
        }
        // 账号限额（429/code 6004）：标记后降级到下一个候选；全部都不可用时透传错误
        if (accountId && isQuotaLimitError(error) && !triedIds.includes(accountId)) {
          triedIds.push(accountId);
          const resetText = markAccountLimited(accountId, model, error);
          const limitEntry = accountStore?.listAccounts?.().accounts
            .find(item => item.id === accountId)?.rateLimits?.[String(model || '')];
          const next = pickNextAccount(model, triedIds);
          const fromLabel = target.account?.name || session.account?.nickname || accountId;
          if (next) {
            log('[Upstream]',
              `⚠️ 账号 ${fromLabel} 对模型 ${model} 已限额`
              + `${resetText ? `（${resetText} 恢复）` : ''}，按优先级降级 → ${next.name || next.id}`
              + `（优先级 ${next.priority}）`);
            reportLimitEvent({
              message: `账号「${fromLabel}」对模型 ${model || '(默认)'} 已限额`
                + `${resetText ? `（${resetText} 恢复）` : ''}，按优先级降级 → 「${next.name || next.id}」`,
              from: { label: fromLabel },
              to: { label: next.name || next.id },
              model,
              error,
              resetAt: limitEntry?.resetAt,
              priority: next.priority,
            });
            continue;
          }
          error.message = `${error.message}（所有候选账号对模型 ${model} 均已限额或禁用）`;
          reportLimitEvent({
            level: 'error',
            message: `模型 ${model || '(默认)'} 在所有候选账号均已限额或禁用，无法继续转发（尝试过 ${triedIds.length} 个账号）`,
            from: { label: fromLabel },
            model,
            error,
            resetAt: limitEntry?.resetAt,
          });
        }
        throw error;
      }
      // 请求成功：该账号对该模型的限额标记（如有）已失效，清除
      if (accountId && accountStore) {
        const hadLimit = accountStore.listAccounts().accounts
          .find(item => item.id === accountId)?.rateLimits?.[String(model || '')];
        accountStore.clearRateLimit(accountId, model);
        // 仅当之前确实处于限额状态才记一条，避免每次成功请求都刷日志
        if (hadLimit) {
          reportLimitEvent({
            level: 'info',
            message: `账号「${target.account?.name || session.account?.nickname || accountId}」对模型 ${model || '(默认)'} 已恢复可用`,
            to: { label: target.account?.name || session.account?.nickname || accountId },
            model,
          });
        }
      }
      verbose('[Upstream]', `上游响应 HTTP ${response.status}（${Date.now() - startedAt}ms）`);
      break;
    }

    if (stream) {
      res.writeHead(response.status, {
        'Content-Type': 'text/event-stream; charset=utf-8',
        'Cache-Control': 'no-cache',
        Connection: 'keep-alive',
      });
      await pipeSse(response.body, res, controller);
      return;
    }

    const completion = await aggregateSseCompletion(response, controller);
    verbose('[Upstream]',
      `聚合完成: chunks=${completion._chunkCount} `
      + `content=${completion.choices[0].message.content?.length || 0} 字符 `
      + `finish=${completion.choices[0].finish_reason}`);
    const { _chunkCount, ...clean } = completion;
    res.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8' });
    res.end(JSON.stringify(clean));
  }

  /**
   * 读取上游 SSE 流并聚合为完整的 OpenAI chat.completion 结构。
   * 拼接 delta.content / delta.reasoning_content，按 index 合并 tool_calls，
   * usage 取最后一次出现（上游在末尾 chunk 下发）。
   */
  async function aggregateSseCompletion(response, controller) {
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let id = '', model = '', created = 0, role = '';
    let content = '', reasoning = '', finish = '';
    let usage = null, chunkCount = 0;
    const toolCalls = new Map();

    const handleChunk = chunk => {
      if (!chunk || typeof chunk !== 'object') return;
      if (chunk.error) {
        throw new WorkBuddyUpstreamError(chunk.error.message || '上游流式返回错误', { statusCode: 502 });
      }
      chunkCount++;
      id = chunk.id || id;
      model = chunk.model || model;
      created = chunk.created || created;
      if (chunk.usage && typeof chunk.usage === 'object') usage = chunk.usage;
      const choice = chunk.choices?.[0];
      if (!choice) return;
      const delta = choice.delta || {};
      role = delta.role || role;
      if (typeof delta.content === 'string') content += delta.content;
      if (typeof delta.reasoning_content === 'string') reasoning += delta.reasoning_content;
      if (choice.finish_reason) finish = choice.finish_reason;
      for (const tc of delta.tool_calls || []) {
        const i = Number.isInteger(tc.index) ? tc.index : 0;
        const cur = toolCalls.get(i) || { id: '', type: 'function', function: { name: '', arguments: '' } };
        if (tc.id) cur.id = tc.id;
        if (tc.type) cur.type = tc.type;
        if (tc.function?.name) cur.function.name += tc.function.name;
        if (tc.function?.arguments) cur.function.arguments += tc.function.arguments;
        toolCalls.set(i, cur);
      }
    };

    for (;;) {
      if (controller.signal.aborted) throw new WorkBuddyUpstreamError('客户端已断开', { statusCode: 499 });
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let idx;
      while ((idx = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, idx).trim();
        buffer = buffer.slice(idx + 1);
        if (!line.startsWith('data:')) continue;
        const data = line.slice(5).trim();
        if (!data || data === '[DONE]') continue;
        try {
          handleChunk(JSON.parse(data));
        } catch (error) {
          if (error instanceof WorkBuddyUpstreamError) throw error;
          // 非 JSON 行忽略
        }
      }
    }

    const message = { role: role || 'assistant', content };
    if (reasoning) message.reasoning_content = reasoning;
    if (toolCalls.size) message.tool_calls = [...toolCalls.values()];
    return {
      id: id || `wb-agg-${Date.now()}`,
      object: 'chat.completion',
      created: created || Math.floor(Date.now() / 1000),
      model,
      choices: [{ index: 0, message, finish_reason: finish || 'stop' }],
      usage,
      _chunkCount: chunkCount,
    };
  }

  /** SSE 透传：思考小分片合并后推给客户端，其余事件原样；客户端断开时取消上游 */
  function pipeSse(upstreamBody, res, controller) {
    return new Promise((resolve, reject) => {
      const upstream = Readable.fromWeb(upstreamBody);
      const coalescer = createReasoningCoalescingStream();
      upstream.pipe(coalescer).pipe(res);
      upstream.on('error', error => {
        if (controller.signal.aborted) return resolve();
        reject(new WorkBuddyUpstreamError(`上游流中断: ${error.message}`, { statusCode: 502 }));
      });
      coalescer.on('error', error => reject(error));
      res.on('finish', () => resolve());
      res.on('close', () => {
        if (!res.writableEnded) controller.abort(new Error('客户端已断开'));
        resolve();
      });
    });
  }

  async function getConfigSummary() {
    // 有账号列表时以「优先级最高的可用账号」为准展示转发目标
    let accountId = null;
    if (accountStore) accountId = pickNextAccount('', [])?.id ?? null;
    const session = accountId
      ? accountStore.getSessionById(accountId)?.session
      : await auth.getCurrentSession().catch(error => ({ authError: error }));
    if (!session) {
      return {
        configured: false,
        baseUrl: auth.baseUrl,
        unavailableReason: '尚未登录（node server.mjs --login）',
      };
    }
    if (session.authError) {
      return {
        configured: false,
        baseUrl: auth.baseUrl,
        unavailableReason: `登录态不可用: ${session.authError.message}`,
        currentAccountId: session.authError.upstreamCode,
      };
    }
    const expiresAt = Number(session.auth?.expiresAt) || 0;
    return {
      configured: true,
      baseUrl: session.endpoint || auth.baseUrl,
      authApiBase: `${auth.baseUrl}/v2${auth.prefixPath}`,
      chatApiUrl: `${auth.baseUrl}/v2/chat/completions`,
      authSource: session.auth?.refreshToken ? 'auth.json（可自动刷新）' : '环境变量/静态 token',
      currentAccountId: session.account?.uid || null,
      tokenExpiresAt: expiresAt || null,
      canRefresh: !!session.auth?.refreshToken,
    };
  }

  return {
    chatCompletionsUrl,
    forwardChatCompletions,
    getConfigSummary,
    NON_STREAM_NOT_SUPPORTED_CODE,
  };
}
