/**
 * 冒烟测试：验证上游客户端的 URL / 请求头复刻 / SSE 聚合逻辑。
 * 用假 auth + 假 fetch，不触网。
 *
 * 运行: node smoke-test.mjs
 */

import {
  createWorkBuddyUpstreamClient,
  parseQuotaResetAt,
  isQuotaLimitError,
  ensureLeadingSystemMessage,
  DEFAULT_SYSTEM_PROMPT,
} from '../src/workbuddy-upstream-client.mjs';
import { createWorkBuddyBilling } from '../src/workbuddy-billing.mjs';
import { createWorkBuddyAuth } from '../src/workbuddy-auth.mjs';
import { WORKBUDDY_USER_AGENT } from '../src/workbuddy-endpoints.mjs';
import { createModelCatalog } from '../src/workbuddy-models.mjs';
import { createAccountStore } from '../src/workbuddy-account-store.mjs';
import { createLogStore, MAX_ENTRIES } from '../src/workbuddy-logs.mjs';
import {
  createDesensitizer,
  compileTerms,
  desensitizeBody,
  desensitizeText,
  normalizeTerms,
  normalizeRoles,
  DEFAULT_TERMS,
  DEFAULT_ROLES,
  ZWSP,
} from '../src/workbuddy-desensitize.mjs';
import { setupDesensitizeRoutes } from '../src/workbuddy-desensitize-routes.mjs';
import { mkdtempSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

let failures = 0;
function check(label, actual, expected) {
  const ok = actual === expected;
  if (!ok) failures++;
  console.log(`${ok ? '✅' : '❌'} ${label}: ${JSON.stringify(actual)}${ok ? '' : ` (期望 ${JSON.stringify(expected)})`}`);
}

const SSE = [
  'data: {"id":"cmpl-1","object":"chat.completion.chunk","model":"auto","choices":[{"index":0,"delta":{"role":"assistant","content":"你"}}]}',
  '',
  'data: {"id":"cmpl-1","model":"auto","choices":[{"index":0,"delta":{"content":"好"}}]}',
  '',
  'data: {"id":"cmpl-1","model":"auto","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_","arguments":"{\\"a"}}]}}]}',
  '',
  'data: {"id":"cmpl-1","model":"auto","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"time","arguments":"\\":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}',
  '',
  'data: [DONE]',
  '',
].join('\n');

const session = {
  auth: { accessToken: 'TEST-TOKEN', refreshToken: 'RT', expiresAt: Date.now() + 3600_000, domain: 'copilot.tencent.com' },
  account: { uid: 'uid-123', enterpriseId: 'ent-1', nickname: 'tester', type: 'personal' },
};

function makeAuth(captured) {
  return {
    baseUrl: 'https://copilot.tencent.com',
    prefixPath: '/plugin',
    buildAuthHeaders: () => ({
      Authorization: `Bearer ${session.auth.accessToken}`,
      'X-User-Id': session.account.uid,
      'X-Enterprise-Id': session.account.enterpriseId,
      'X-Tenant-Id': session.account.enterpriseId,
      'X-Domain': session.auth.domain,
    }),
    getCurrentSession: async () => session,
  };
}

function makeRes() {
  return {
    headersSent: false,
    writableEnded: false,
    status: 0,
    headers: null,
    payload: null,
    writeHead(s, h) { this.headersSent = true; this.status = s; this.headers = h; },
    end(b) { this.writableEnded = true; this.payload = b; },
    write() { return true; },
  };
}

// ─── 1. chat/completions：URL + 头 + 非流式聚合 ────────────

console.log('\n━━━ 1. chat/completions 转发 ━━━');
{
  const captured = [];
  const client = createWorkBuddyUpstreamClient({
    auth: makeAuth(captured),
    fetchImpl: async (url, init) => {
      captured.push({ url, init });
      return new Response(SSE, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
    },
  });

  const res = makeRes();
  await client.forwardChatCompletions({
    req: {}, res,
    body: { model: 'auto', stream: false, messages: [{ role: 'user', content: 'hi' }] },
    controller: new AbortController(),
    dedupeKey: 'k1',
  });

  const { url, init } = captured[0];
  const h = init.headers;
  check('URL', url, 'https://copilot.tencent.com/v2/chat/completions');
  check('method', init.method, 'POST');
  check('Authorization', h.Authorization, 'Bearer TEST-TOKEN');
  check('X-User-Id', h['X-User-Id'], 'uid-123');
  check('X-Enterprise-Id', h['X-Enterprise-Id'], 'ent-1');
  check('X-Tenant-Id', h['X-Tenant-Id'], 'ent-1');
  check('X-Domain', h['X-Domain'], 'copilot.tencent.com');
  check('User-Agent', h['User-Agent'], WORKBUDDY_USER_AGENT);
  check('X-IDE-Type', h['X-IDE-Type'], 'WorkBuddy');
  check('X-Product', h['X-Product'], 'WorkBuddy');
  check('X-Agent-Intent', h['X-Agent-Intent'], 'craft');
  check('Accept', h.Accept, 'text/event-stream');
  check('有 X-Request-ID', typeof h['X-Request-ID'] === 'string' && h['X-Request-ID'].length > 0, true);
  // 上游只支持流式：即使客户端要 stream=false，也必须以 stream=true 请求
  check('上游 stream 强制 true', JSON.parse(init.body).stream, true);

  const out = JSON.parse(res.payload);
  check('聚合 object', out.object, 'chat.completion');
  check('聚合 content', out.choices[0].message.content, '你好');
  check('聚合 tool_calls 名称拼接', out.choices[0].message.tool_calls[0].function.name, 'get_time');
  check('聚合 tool_calls 参数拼接', out.choices[0].message.tool_calls[0].function.arguments, '{"a":1}');
  check('聚合 finish_reason', out.choices[0].finish_reason, 'tool_calls');
  check('聚合 usage', out.usage.total_tokens, 13);
  check('内部字段 _chunkCount 不外泄', '_chunkCount' in out, false);
}

// ─── 2. 首条 system 消息注入 ───────────────────────────────

console.log('\n━━━ 2. 首条 system 消息注入 ━━━');
{
  // 上游要求首条消息是 system prompt，否则 400。纯函数分支：
  const plain = { model: 'auto', messages: [{ role: 'user', content: '你好' }] };
  const injected = ensureLeadingSystemMessage(plain);
  check('纯 user 开头 → 注入', injected.messages.length, 2);
  check('注入的首条是 system', injected.messages[0].role, 'system');
  check('注入内容为默认提示词', injected.messages[0].content, DEFAULT_SYSTEM_PROMPT);
  check('默认提示词文案', DEFAULT_SYSTEM_PROMPT, '你是一个得力助手');
  check('原消息顺序不变', injected.messages[1].role, 'user');
  check('不改原 body（新引用）', injected !== plain, true);
  check('原 body.messages 未被改动', plain.messages.length, 1);

  // 已是 system 开头 / 无 messages / 空 messages：原样返回（同一引用）
  const withSystem = { model: 'auto', messages: [{ role: 'system', content: 'X' }, { role: 'user', content: 'hi' }] };
  check('system 开头不注入', ensureLeadingSystemMessage(withSystem), withSystem);
  const noMessages = { model: 'auto' };
  check('无 messages 不注入', ensureLeadingSystemMessage(noMessages), noMessages);
  const emptyMessages = { model: 'auto', messages: [] };
  check('空 messages 不注入（交给上游报错）', ensureLeadingSystemMessage(emptyMessages), emptyMessages);

  // 转发链路上的实际效果：客户端不带 system 时，出站请求体首条应为注入的 system
  const captured = [];
  const client = createWorkBuddyUpstreamClient({
    auth: makeAuth([]),
    fetchImpl: async (url, init) => {
      captured.push({ url, init });
      return new Response(SSE, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
    },
  });
  const res = makeRes();
  await client.forwardChatCompletions({
    req: {}, res,
    body: { model: 'auto', stream: false, messages: [{ role: 'user', content: '你好' }] },
    controller: new AbortController(),
    dedupeKey: 'sys-inject',
  });
  const sent = JSON.parse(captured[0].init.body);
  check('出站首条为 system', sent.messages[0].role, 'system');
  check('出站注入内容正确', sent.messages[0].content, DEFAULT_SYSTEM_PROMPT);
  check('出站保留原 user 消息', sent.messages[1].content, '你好');
  check('出站消息数 +1', sent.messages.length, 2);
}

// ─── 3. 各转发端点 URL ─────────────────────────────────────

console.log('\n━━━ 3. 各模型端点 URL（均不带 /plugin）━━━');
{
  const urls = [];
  const client = createWorkBuddyUpstreamClient({
    auth: makeAuth([]),
    fetchImpl: async (url) => { urls.push(url); return new Response('{}', { status: 200 }); },
  });
  const res = makeRes();
  const controller = new AbortController();
  await client.forwardCompletions({ req: {}, res, body: {}, controller });
  await client.forwardEmbeddings({ req: {}, res, body: {}, controller });
  await client.forwardImageGenerations({ req: {}, res, body: {}, controller });
  await client.forwardVideoGenerations({ req: {}, res, body: {}, controller });
  check('completions', urls[0], 'https://copilot.tencent.com/v2/completions');
  check('embeddings', urls[1], 'https://copilot.tencent.com/v2/embeddings');
  check('images/generations', urls[2], 'https://copilot.tencent.com/v2/images/generations');
  check('videos/generations', urls[3], 'https://copilot.tencent.com/v2/videos/generations');
}

// ─── 3. 计费 / 签到：URL + 头 + 归一化 ─────────────────────

console.log('\n━━━ 4. 计费 / 签到（不带 /plugin）━━━');
{
  const calls = [];
  const billingBodies = {
    '/v2/billing/meter/checkin-activity-status': {
      code: 0, data: { checked_in: false, continuous_days: 4, total_days: 12, points: 100 },
    },
    '/v2/billing/meter/daily-checkin': {
      code: 0, data: { checked_in: true, continuous_days: 5, points: 100 },
    },
    '/v2/billing/meter/get-user-resource': {
      code: 0,
      data: {
        Response: {
          Data: {
            Accounts: [
              { ResourceId: 'r1', PackageCode: 'TCACA_code_001_PqouKr6QWV', CycleCapacitySizePrecise: 1000, CycleCapacityRemainPrecise: 700, CycleEndTime: '2030-01-01T00:00:00Z' },
              { ResourceId: 'r2', PackageCode: 'TCACA_code_002_AkiJS3ZHF5', CycleCapacitySizePrecise: 5000, CycleCapacityRemainPrecise: 4200, DeductionEndTime: '2030-06-01T00:00:00Z', CycleEndTime: '2030-06-01T00:00:00Z', AutoRenewFlag: 1 },
              { ResourceId: 'r3', PackageCode: 'TCACA_code_028_NtpWi0jzXs', CycleCapacitySizePrecise: 800, CycleCapacityRemainPrecise: 500, DeductionEndTime: '2030-12-01T00:00:00Z' },
            ],
          },
        },
      },
    },
    '/v2/billing/meter/get-enterprise-user-usage': { code: 0, data: { limitNum: 8000, credit: 1500, cycleResetTime: '2030-03-01T00:00:00Z' } },
    '/v2/activity/workbuddy/banner': { code: 0, data: { activity_id: 'a1', banner_content: 'hello', status: 'None', activity_online_status: true } },
    '/v2/activity/ambassador/status': { code: 0, data: { isAmbassador: true } },
  };

  const billing = createWorkBuddyBilling({
    auth: makeAuth(calls),
    fetchImpl: async (url, init) => {
      const path = new URL(url).pathname;
      calls.push({ url, path, headers: init.headers, body: init.body });
      const payload = billingBodies[path] ?? { code: 0, data: {} };
      return new Response(JSON.stringify(payload), { status: 200, headers: { 'Content-Type': 'application/json' } });
    },
  });

  const status = await billing.getCheckinStatus();
  check('签到状态 checkedIn', status.checkedIn, false);
  check('签到状态 continuousDays', status.continuousDays, 4);
  check('签到状态 totalDays', status.totalDays, 12);
  check('签到接口 URL', calls[0].path, '/v2/billing/meter/checkin-activity-status');
  check('签到接口 Authorization', calls[0].headers.Authorization, 'Bearer TEST-TOKEN');
  check('签到接口 X-User-Id', calls[0].headers['X-User-Id'], 'uid-123');
  check('签到接口 X-Domain', calls[0].headers['X-Domain'], 'copilot.tencent.com');
  check('签到接口 body 为 {}', calls[0].body, '{}');

  const claim = await billing.claimDailyCheckin();
  check('签到领取 success', claim.success, true);
  check('签到领取 continuousDays', claim.data.continuousDays, 5);
  check('签到领取 URL', calls[1].path, '/v2/billing/meter/daily-checkin');

  const usage = await billing.getPersonalUsage(session, { locale: 'zh-CN' });
  check('个人额度 kind', usage.kind, 'personal');
  check('个人额度 usageTotal', usage.usageTotal, '6800');
  check('个人额度 usageLeft', usage.usageLeft, '5400');
  check('个人额度 usageUsed', usage.usageUsed, '1400');
  check('个人额度 editionType（识别到专业版包月）', usage.editionType, 'pro');
  check('个人额度 isPro', usage.isPro, true);
  check('个人额度包名（中文标签）', usage.resources[0].name, '专业版包月');
  check('日额度包标记 isDaily', usage.resources.find(r => r.packageCode === 'TCACA_code_001_PqouKr6QWV')?.isDaily, true);
  check('个人额度资源数', usage.resources.length, 3);
  // 订阅类排在日额度包之前（桌面端套餐优先级）
  check('套餐优先级排序：专业版排首位', usage.resources[0].packageCode, 'TCACA_code_002_AkiJS3ZHF5');
  check('计费接口 Accept-Language 归一化', calls[2].headers['Accept-Language'], 'zh');
  check('个人额度 URL', calls[2].path, '/v2/billing/meter/get-user-resource');
  check('个人额度 body ProductCode', JSON.parse(calls[2].body).ProductCode, 'p_tcaca');

  // 积分简报：三个数（总剩余 / 套餐基础 / 平台奖励）
  const personalSession = { ...session, account: { ...session.account, enterpriseId: '' } };
  const summary = await billing.queryCreditsSummary({ session: personalSession, locale: 'zh-CN' });
  check('简报 kind', summary.kind, 'personal');
  check('简报总剩余（含全部资源包）', summary.totalLeft, 5400);
  check('简报套餐基础（专业版包月+免费日额度）', summary.planLeft, 4900);
  check('简报平台奖励（赠送积分 28）', summary.bonusLeft, 500);
  check('简报不含明细字段', Object.keys(summary).sort().join(','), 'bonusLeft,kind,planLeft,totalLeft,unlimited');

  const entSummary = await billing.queryCreditsSummary({
    ...session, account: { ...session.account, enterpriseId: 'ent-1', type: 'exclusive' },
  });
  check('企业简报 totalLeft', entSummary.totalLeft, 6500);
  check('企业简报 planLeft/bonusLeft 为 null', entSummary.planLeft === null && entSummary.bonusLeft === null, true);

  const entUsage = await billing.getEnterpriseUsage({
    ...session, account: { ...session.account, enterpriseId: 'ent-1', type: 'exclusive' },
  });
  check('企业额度 kind', entUsage.kind, 'enterprise');
  check('企业额度 usageLeft', entUsage.usageLeft, '6500');
  check('企业额度 usageTotal', entUsage.usageTotal, '8000');
  check('企业额度 editionType', entUsage.editionType, 'exclusive');
  check('企业额度 URL', calls.find(c => c.path === '/v2/billing/meter/get-enterprise-user-usage')?.path,
    '/v2/billing/meter/get-enterprise-user-usage');

  const banner = await billing.getActivityBanner();
  const bannerCall = calls.find(c => c.path === '/v2/activity/workbuddy/banner');
  check('banner id', banner.id, 'a1');
  check('banner 白名单 X-Product', bannerCall?.headers['X-Product'], 'WorkBuddy');
  check('banner 白名单 User-Agent', bannerCall?.headers['User-Agent'], WORKBUDDY_USER_AGENT);
  check('banner URL', bannerCall?.path, '/v2/activity/workbuddy/banner');

  const amb = await billing.getAmbassadorStatus();
  check('ambassador', amb.isAmbassador, true);
}

// ─── 4. 企业不限量哨兵 ─────────────────────────────────────

console.log('\n━━━ 5. 企业不限量（limitNum = -1）━━━');
{
  // 企业账号：fetchAvailableCredits 内部复用 queryUsage，需要 enterpriseId 才走企业链路
  const entSession = {
    ...session,
    account: { ...session.account, enterpriseId: 'ent-1', type: 'exclusive' },
  };
  const billing = createWorkBuddyBilling({
    auth: makeAuth([]),
    fetchImpl: async () => new Response(
      JSON.stringify({ code: 0, data: { limitNum: -1, credit: 999 } }),
      { status: 200, headers: { 'Content-Type': 'application/json' } },
    ),
  });
  const usage = await billing.getEnterpriseUsage(session);
  check('unlimited 标记', usage.unlimited, true);
  check('usageLeft 哨兵', usage.usageLeft, 'unlimited');
  check('fetchAvailableCredits → Infinity', await billing.fetchAvailableCredits({ session: entSession }), Infinity);
}

// ─── 5. 匿名登录 URL（不触网，仅校验拼装）──────────────────

console.log('\n━━━ 6. 鉴权 URL 拼装 ━━━');
{
  const captured = [];
  const auth = createWorkBuddyAuth({
    endpoint: 'https://copilot.tencent.com',
    fetchImpl: async (url, init) => {
      captured.push({ url, headers: init.headers, method: init.method });
      return new Response(JSON.stringify({ code: 0, data: { state: 'st-1', authUrl: 'https://x/login' } }), {
        status: 200, headers: { 'Content-Type': 'application/json' },
      });
    },
  });
  check('prefixPath', auth.prefixPath, '/plugin');
  const ctrl = new AbortController();
  await auth.loginInteractive({
    signal: ctrl.signal, pollIntervalMs: 10_000,
    onAuthUrl: () => ctrl.abort(),
  }).catch(() => {});
  check('auth/state URL 带 prefixPath', captured[0].url, 'https://copilot.tencent.com/v2/plugin/auth/state?platform=workbuddy');
  check('auth/state method', captured[0].method, 'POST');
  check('auth/state 匿名头 X-No-Authorization', captured[0].headers['X-No-Authorization'], 'true');
  check('auth/state 匿名头 X-No-Department-Info', captured[0].headers['X-No-Department-Info'], 'true');
  check('auth/state 无 Authorization', 'Authorization' in captured[0].headers, false);

  // 国际版登录：端点 workbuddy.ai + platform=workbuddy-ai
  const intlCaptured = [];
  const intlAuth = createWorkBuddyAuth({
    edition: 'intl',
    fetchImpl: async (url, init) => {
      intlCaptured.push({ url, headers: init.headers });
      return new Response(JSON.stringify({ code: 0, data: { state: 'st-intl', authUrl: 'https://www.workbuddy.ai/login' } }), {
        status: 200, headers: { 'Content-Type': 'application/json' },
      });
    },
  });
  check('国际版 auth.baseUrl', intlAuth.baseUrl, 'https://www.workbuddy.ai');
  check('国际版 platform', intlAuth.platform, 'workbuddy-ai');
  const intlCtrl = new AbortController();
  await intlAuth.loginInteractive({
    edition: 'intl', signal: intlCtrl.signal, pollIntervalMs: 10_000,
    onAuthUrl: () => intlCtrl.abort(),
  }).catch(() => {});
  check('国际版登录 URL', intlCaptured[0].url, 'https://www.workbuddy.ai/v2/plugin/auth/state?platform=workbuddy-ai');

  // token 刷新：refreshToken 走头
  const refreshCalls = [];
  const auth2 = createWorkBuddyAuth({
    fetchImpl: async (url, init) => {
      refreshCalls.push({ url, headers: init.headers, body: init.body });
      return new Response(JSON.stringify({ code: 0, data: { accessToken: 'NEW', refreshToken: 'RT2', expiresIn: 3600 } }), {
        status: 200, headers: { 'Content-Type': 'application/json' },
      });
    },
  });
  const next = await auth2.refreshSession({ accessToken: 'OLD', refreshToken: 'RT', domain: 'copilot.tencent.com' });
  check('refresh URL', refreshCalls[0].url, 'https://copilot.tencent.com/v2/plugin/auth/token/refresh');
  check('refresh X-Refresh-Token', refreshCalls[0].headers['X-Refresh-Token'], 'RT');
  check('refresh X-Auth-Refresh-Source', refreshCalls[0].headers['X-Auth-Refresh-Source'], 'plugin');
  check('refresh body 为 {}', refreshCalls[0].body, '{}');
  check('refresh 新 token', next.accessToken, 'NEW');
  check('refresh 计算 expiresAt', next.expiresAt > Date.now(), true);

  // 国际版账号按自己的端点续期（多账号混版时不能打到国内端点）
  const intlRefreshCalls = [];
  const auth3 = createWorkBuddyAuth({
    fetchImpl: async (url) => {
      intlRefreshCalls.push({ url });
      return new Response(JSON.stringify({ code: 0, data: { accessToken: 'NEW-INTL', expiresIn: 3600 } }), {
        status: 200, headers: { 'Content-Type': 'application/json' },
      });
    },
  });
  await auth3.refreshSession(
    { accessToken: 'OLD', refreshToken: 'RT-INTL' },
    auth3.makeContext({ edition: 'intl' }),
  );
  check('国际版 refresh URL', intlRefreshCalls[0].url, 'https://www.workbuddy.ai/v2/plugin/auth/token/refresh');
}

// ─── 6. 错误码处理 ─────────────────────────────────────────

console.log('\n━━━ 7. 上游错误码 ━━━');
{
  // 11217：登录轮询中，应继续等待而不是抛错
  let pollCount = 0;
  const auth = createWorkBuddyAuth({
    fetchImpl: async (url) => {
      if (url.includes('auth/state')) {
        return new Response(JSON.stringify({ code: 0, data: { state: 'st-1', authUrl: 'https://x' } }), { status: 200 });
      }
      if (url.includes('auth/token?')) {
        pollCount++;
        if (pollCount < 3) return new Response(JSON.stringify({ code: 11217, message: 'pending' }), { status: 200 });
        return new Response(JSON.stringify({ code: 0, data: { accessToken: 'T', expiresIn: 60 } }), { status: 200 });
      }
      if (url.includes('/login/account')) {
        return new Response(JSON.stringify({ code: 0, data: { uid: 'u1', nickname: 'n' } }), { status: 200 });
      }
      return new Response(JSON.stringify({ code: 0, data: { accounts: [] } }), { status: 200 });
    },
    directory: mkdtempSync(join(tmpdir(), 'wb-proxy-test-')),
    log: () => {},
  });
  const sess = await auth.loginInteractive({ pollIntervalMs: 5 });
  check('11217 期间持续轮询，最终拿到 token', sess.auth.accessToken, 'T');
  check('轮询次数 >= 3', pollCount >= 3, true);
  check('最终账号 uid', sess.account.uid, 'u1');
}

// ─── 7. 签到幂等（非 0 code）──────────────────────────────

console.log('\n━━━ 8. 签到幂等（已领取时非 0 code）━━━');
{
  const billing = createWorkBuddyBilling({
    auth: makeAuth([]),
    fetchImpl: async () => new Response(
      JSON.stringify({ code: 41001, msg: '今日已签到', requestId: 'req-1' }),
      { status: 200, headers: { 'Content-Type': 'application/json' } },
    ),
  });
  const claim = await billing.claimDailyCheckin();
  check('已签到 success=false', claim.success, false);
  check('已签到 code', claim.code, 41001);
  check('已签到 msg', claim.msg, '今日已签到');
  // 状态接口在 code!=0 时返回 null（与桌面端一致）
  check('状态接口 code!=0 返回 null', await billing.getCheckinStatus(), null);
}

// ─── 8. 模型目录 ───────────────────────────────────────────

console.log('\n━━━ 9. 模型目录 ━━━');
{
  const cat = createModelCatalog({});
  check('内置模型数 > 20', cat.list().length > 20, true);
  check('deepseek-v4.1-flash 内置兜底存在', cat.has('deepseek-v4.1-flash'), true);
  check('大小写不敏感', cat.has('AUTO'), true);
  check('已知模型原样返回', cat.resolve('deepseek-v4-pro'), 'deepseek-v4-pro');
  // 未知模型必须抛错而不是静默回退：请求 A 就必须路由到 A，不存在就报错
  const throwName = (() => {
    try { cat.resolve('no-such-model-xyz'); return null; }
    catch (error) { return error.name; }
  })();
  check('未知模型 resolve 抛 ModelRouteError', throwName, 'ModelRouteError');
  check('listResponse 带 credits 倍率字段', typeof cat.listResponse().data[0].credits === 'string', true);
  // suggest 仅用于 400 提示，不参与路由
  const hint = cat.suggest('deepseek-v4.1-flashx', 3);
  check('suggest 提示同系近似名', hint.includes('deepseek-v4.1-flash'), true, hint.join(','));
  check('suggest 不含无关模型', cat.suggest('completely-unrelated-xyz', 5).length === 0,
    true, cat.suggest('completely-unrelated-xyz', 5).join(','));
  const list = cat.listResponse();
  check('listResponse object', list.object, 'list');
  check('meta.source = builtin', list.meta.source, 'builtin');
  check('data[0].owned_by', list.data[0].owned_by, 'workbuddy');
}

// ─── 9. 账号存储 ───────────────────────────────────────────

console.log('\n━━━ 10. 账号存储（临时目录）━━━');
{
  const dir = mkdtempSync(join(tmpdir(), 'wb-store-test-'));
  try {
    const store = createAccountStore({ directory: dir, log: () => {} });
    const pub = store.addAccount({
      auth: { accessToken: 'AT-1234', refreshToken: 'RT-1', expiresAt: 123 },
      account: { uid: 'uid-1', nickname: '小明', enterpriseId: '' },
    });
    check('账号 id', pub.id, 'user-uid-1');
    check('账号昵称', pub.nickname, '小明');
    check('tokenTail 脱敏', pub.tokenTail, '1234');
    check('hasRefreshToken', pub.hasRefreshToken, true);
    // 当前账号由优先级派生（队首），这里只有一个账号
    check('列表当前账号', store.listAccounts().currentAccountId, 'user-uid-1');
    check('当前会话 prefixPath', store.getCurrentEntry().session.prefixPath, '/plugin');
    check('当前会话 uid', store.getCurrentEntry().session.account.uid, 'uid-1');
    check('默认版本 = 国内版', pub.edition, 'cn');
    check('国内版端点', pub.endpoint, 'https://copilot.tencent.com');
    store.updateAccountTokens('user-uid-1', { accessToken: 'AT-9999' });
    check('回写后 tokenTail', store.getCredentialsById('user-uid-1').accessToken, 'AT-9999');
    // 国际版账号：端点 / platform / edition 按版本落定
    const intl = store.addAccount({
      auth: { accessToken: 'AT-INTL', refreshToken: 'RT-INTL', expiresAt: 456 },
      account: { uid: 'uid-intl', nickname: 'Intl User' },
      edition: 'intl',
    });
    check('国际版账号 edition', intl.edition, 'intl');
    check('国际版端点', intl.endpoint, 'https://www.workbuddy.ai');
    check('国际版凭据端点', store.getCredentialsById('user-uid-intl').endpoint, 'https://www.workbuddy.ai');
    // 新增账号不改变当前选择：仍是首个账号，会话按当前账号走
    check('新增账号不抢占当前账号', store.listAccounts().currentAccountId, 'user-uid-1');
    check('当前会话仍是国内版', store.getCurrentEntry().session.edition, 'cn');
    check('国际版账号会话 platform', store.getSessionById('user-uid-intl').session.platform, 'workbuddy-ai');
    check('国际版账号会话 edition', store.getSessionById('user-uid-intl').session.edition, 'intl');
    // 显式 endpoint 优先于版本默认值
    const custom = store.addAccount({
      auth: { accessToken: 'AT-CUSTOM' },
      account: { uid: 'uid-custom' },
      edition: 'intl',
      endpoint: 'https://staging.workbuddy.ai',
    });
    check('显式端点优先', custom.endpoint, 'https://staging.workbuddy.ai');
    store.removeAccount('user-uid-intl');
    store.removeAccount('user-uid-custom');
    let threw = false;
    try { store.addAccount({ auth: {} }); } catch { threw = true; }
    check('缺 accessToken 报错', threw, true);
    let threw2 = false;
    try { store.addAccount({ auth: { accessToken: 'x' } }); } catch { threw2 = true; }
    check('缺 uid 报错', threw2, true);
    store.removeAccount('user-uid-1');
    check('删除后账号数', store.listAccounts().accounts.length, 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// ─── 10. 敏感词脱敏 ─────────────────────────────────────────

console.log('\n━━━ 11. 敏感词脱敏 ━━━');
{
  // 短词不多吃长词："C2 frameworks" 必须整体命中，而不是被 "C2 framework" 的短前缀切碎
  const pattern = compileTerms(['C2 framework', 'C2 frameworks', 'DoS']);
  check('长词优先（frameworks 完整命中）',
    desensitizeText('C2 frameworks here', pattern), `C${ZWSP}2 frameworks here`);
  check('DoS 命中插零宽',
    desensitizeText('Refuse DoS attacks', pattern), `Refuse D${ZWSP}oS attacks`);
  check('大小写不敏感', desensitizeText('AND DOS', compileTerms(['DoS'])), `AND D${ZWSP}OS`);
  check('未命中保持原样', desensitizeText('hello world', pattern), 'hello world');
  check('非字符串原样返回', desensitizeText(null, pattern), null);

  // 词边界：避免 0day 命中 100days、XSS 命中 XSSRF
  const boundary = compileTerms(['0day', 'XSS']);
  check('词边界：100days 不命中', desensitizeText('100days', boundary), '100days');
  check('词边界：ddos 不因 DoS 命中', desensitizeText('ddos', compileTerms(['DoS'])), 'ddos');
  check('词边界：XSSRF 不命中', desensitizeText('XSSRF', boundary), 'XSSRF');
  check('词边界：独立 XSS 命中', desensitizeText('an XSS bug', boundary), `an X${ZWSP}SS bug`);

  // 空词表 / 空 pattern
  check('空词表编译为 null', compileTerms([]), null);
  check('空词表不脱敏', desensitizeText('DoS attack', compileTerms([])), 'DoS attack');

  // 词表清洗：去空白 / 去重（忽略大小写）/ 丢弃非字符串
  check('清洗去重', normalizeTerms([' DoS ', 'dos', '', null, 'XSS']).join(','), 'DoS,XSS');
  check('清洗丢弃非字符串', normalizeTerms(['DoS', 123, {}]).join(','), 'DoS');
  check('清洗丢弃超长词', normalizeTerms(['a'.repeat(201), 'ok']).join(','), 'ok');
  check('角色白名单', normalizeRoles(['user', 'system', 'bogus']).join(','), 'system,user');
  check('角色空回退默认', normalizeRoles([]).join(','), DEFAULT_ROLES.join(','));
  check('角色非法类型回退默认', normalizeRoles('system').join(','), DEFAULT_ROLES.join(','));

  // messages 脱敏：只动指定角色，且不修改原对象
  const original = {
    model: 'auto',
    messages: [
      { role: 'system', content: 'Refuse requests for DoS attacks and malware.' },
      { role: 'user', content: 'explain XSS' },
      { role: 'assistant', content: 'about malware' },
    ],
  };
  const scrubbed = desensitizeBody(original, { pattern: compileTerms(DEFAULT_TERMS), roles: ['system', 'user'] });
  check('system 命中', scrubbed.messages[0].content.includes(ZWSP), true);
  check('user 命中', scrubbed.messages[1].content.includes(ZWSP), true);
  check('assistant 不动', scrubbed.messages[2].content, 'about malware');
  check('原对象未被修改', original.messages[0].content.includes(ZWSP), false);
  check('model 等其他字段保留', scrubbed.model, 'auto');
  check('未命中角色保持引用', scrubbed.messages[2] === original.messages[2], true);

  // 多模态 content 数组
  const multi = desensitizeBody({
    messages: [{ role: 'user', content: [{ type: 'text', text: 'reverse shell' }, { type: 'image_url', image_url: { url: 'x' } }] }],
  }, { pattern: compileTerms(['reverse shell']) });
  check('多模态文本块命中', multi.messages[0].content[0].text.includes(ZWSP), true);
  check('多模态图片块原样', multi.messages[0].content[1].type, 'image_url');

  // 无 messages 的请求体（embeddings / images）不受影响
  const noMessages = { input: 'DoS attack' };
  check('无 messages 原样返回', desensitizeBody(noMessages, { pattern }), noMessages);
}

console.log('\n━━━ 12. 脱敏器：开关 / 持久化 / 统计 ━━━');
{
  const dir = mkdtempSync(join(tmpdir(), 'wb-desens-test-'));
  try {
    const d = createDesensitizer({ directory: dir, log: () => {} });
    check('默认开启', d.enabled, true);
    check('默认词表', d.terms.length, DEFAULT_TERMS.length);

    // 命中统计
    const body = { messages: [{ role: 'user', content: 'malware and malware and botnet' }] };
    const r1 = d.processBody(body);
    check('命中后 changed', r1.changed, true);
    check('命中次数', r1.hits, 3);
    check('返回新 body', r1.body === body, false);
    check('原始 body 未被改', body.messages[0].content, 'malware and malware and botnet');
    const stats = d.getState().stats;
    check('统计 requests', stats.requests, 1);
    check('统计 changedRequests', stats.changedRequests, 1);
    check('统计 totalHits', stats.totalHits, 3);
    check('统计 topTerms 首位', stats.topTerms[0].term, 'malware');
    check('统计 topTerms 计数', stats.topTerms[0].count, 2);

    // 未命中不计入 changed
    d.processBody({ messages: [{ role: 'user', content: 'nothing here' }] });
    check('未命中不计 changed', d.getState().stats.changedRequests, 1);
    check('未命中 requests 仍累计', d.getState().stats.requests, 2);

    // 关闭后不处理
    d.setEnabled(false);
    const off = d.processBody({ messages: [{ role: 'user', content: 'malware' }] });
    check('关闭后不处理', off.changed, false);
    check('关闭后 body 原样', off.body.messages[0].content, 'malware');

    // 持久化：reload 后开关与词表都还原
    const file = join(dir, 'desensitize.json');
    check('配置文件已生成', existsSync(file), true);
    d.setEnabled(true);
    d.setTerms(['acme-corp']);
    const reloaded = createDesensitizer({ directory: dir, log: () => {} });
    check('reload 词表', reloaded.terms.join(','), 'acme-corp');
    check('reload 开关', reloaded.enabled, true);

    // 增删词（忽略大小写）
    reloaded.addTerms(['DoS', 'dos', 'XSS']);
    check('增词去重（忽略大小写）', reloaded.terms.length, 3);
    reloaded.removeTerms(['ACME-CORP']);
    check('删词（忽略大小写）', reloaded.terms.includes('acme-corp'), false);
    check('删词后剩余', reloaded.terms.length, 2);

    // 恢复默认词表
    reloaded.resetTerms();
    check('恢复默认词表', reloaded.terms.length, DEFAULT_TERMS.length);

    // 角色可改
    reloaded.setRoles(['system']);
    check('角色改为仅 system', reloaded.getState().roles.join(','), 'system');
    const onlySystem = reloaded.processBody({
      messages: [
        { role: 'system', content: 'malware' },
        { role: 'user', content: 'malware' },
      ],
    });
    check('仅 system 时 user 不动', onlySystem.body.messages[1].content, 'malware');
    check('仅 system 时 system 命中', onlySystem.body.messages[0].content.includes(ZWSP), true);

    // 统计重置
    reloaded.resetStats();
    check('统计已重置', reloaded.getState().stats.totalHits, 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

console.log('\n━━━ 13. 脱敏 HTTP 路由 ━━━');
{
  const dir = mkdtempSync(join(tmpdir(), 'wb-desens-route-'));
  try {
    const desensitizer = createDesensitizer({ directory: dir, log: () => {} });
    const routes = setupDesensitizeRoutes({
      desensitizer,
      readRawBody: async req => Buffer.from(JSON.stringify(req._body || {})),
      log: () => {},
    });
    const call = async (method, path, body, search = '') => {
      const res = {
        status: 0, payload: null, headersSent: false,
        writeHead(s) { this.status = s; this.headersSent = true; },
        end(b) { this.payload = JSON.parse(b); },
      };
      const url = new URL(`http://x${path}${search}`);
      const handled = await routes.tryHandle({ method, _body: body }, res, url.pathname, url);
      return { handled, status: res.status, payload: res.payload };
    };

    check('非本模块路径不接管', (await call('GET', '/api/accounts')).handled, false);

    const initial = await call('GET', '/api/desensitize');
    check('GET 状态 200', initial.status, 200);
    check('GET 状态 enabled', initial.payload.data.enabled, true);
    check('GET 状态含词表', Array.isArray(initial.payload.data.terms), true);

    const add = await call('POST', '/api/desensitize/terms', { terms: ['acme-secret'] });
    check('追加词 200', add.status, 200);
    check('追加后词数', add.payload.data.termCount, DEFAULT_TERMS.length + 1);

    const dup = await call('POST', '/api/desensitize/terms', { term: 'ACME-SECRET' });
    check('重复词不增加', dup.payload.data.termCount, DEFAULT_TERMS.length + 1);

    const badType = await call('POST', '/api/desensitize/terms', { terms: 'not-array' });
    check('非法词表 400', badType.status, 400);
    check('非法词表错误信息', badType.payload.success, false);

    const empty = await call('PUT', '/api/desensitize/terms', { terms: [] });
    check('空词表 400', empty.status, 400);

    const put = await call('PUT', '/api/desensitize/terms', { terms: ['only-this'] });
    check('PUT 全量替换', put.payload.data.terms.join(','), 'only-this');

    const del = await call('DELETE', '/api/desensitize/terms', { terms: ['only-this'] });
    check('DELETE 后词表为空', del.payload.data.termCount, 0);

    const viaQuery = await call('DELETE', '/api/desensitize/terms', null, '?term=foo');
    check('DELETE 支持 query 传参 200', viaQuery.status, 200);

    const off = await call('POST', '/api/desensitize/enabled', { enabled: false });
    check('关闭开关', off.payload.data.enabled, false);
    const badEnabled = await call('POST', '/api/desensitize/enabled', { enabled: 'yes' });
    check('enabled 非布尔 400', badEnabled.status, 400);

    const roles = await call('POST', '/api/desensitize/roles', { roles: ['system', 'user', 'assistant'] });
    check('设置角色', roles.payload.data.roles.join(','), 'system,user,assistant');

    const reset = await call('POST', '/api/desensitize/reset');
    check('恢复默认词表', reset.payload.data.termCount, DEFAULT_TERMS.length);

    const statsReset = await call('POST', '/api/desensitize/stats/reset');
    check('清空统计 200', statsReset.status, 200);

    const notFound = await call('GET', '/api/desensitize/unknown');
    check('未知子路径 404', notFound.status, 404);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// ─── 13. 账号限额轮换（429 / code 6004）───────────────────

console.log('\n━━━ 14. 账号限额轮换 ━━━');
{
  // 恢复时间解析（msg 里的 UTC+8 文本）
  const QUOTA_MSG = '您的使用量已超出频率限制，将在 2026-09-11 19:43:46 UTC+8 重置，您也可以切换其他模型继续使用。';
  check('parseQuotaResetAt 解析 UTC+8 时间',
    parseQuotaResetAt(QUOTA_MSG), Date.parse('2026-09-11T19:43:46+08:00'));
  check('parseQuotaResetAt 无时间文本返回 0', parseQuotaResetAt('限额了'), 0);
  check('isQuotaLimitError HTTP 429', isQuotaLimitError({ statusCode: 429 }), true);
  check('isQuotaLimitError code 6004', isQuotaLimitError({ upstreamCode: 6004 }), true);
  check('非限额错误 false', isQuotaLimitError({ statusCode: 500, upstreamCode: 11128 }), false);

  // 账号标记：写入 / 公开字段 / 清除
  const dir = mkdtempSync(join(tmpdir(), 'wb-quota-test-'));
  try {
    const store = createAccountStore({ directory: dir, log: () => {} });
    store.addAccount({ auth: { accessToken: 'AT-A' }, account: { uid: 'uid-a', nickname: 'A' } });
    store.addAccount({ auth: { accessToken: 'AT-B' }, account: { uid: 'uid-b', nickname: 'B' } });
    store.promoteToFront('user-uid-a');
    // 这里用相对的未来时间戳：markRateLimited 对「已过去」的 resetAt 会回退成
    // now + 10 分钟，写成固定日期会随时间流逝而失败（时间炸弹）。
    const FUTURE_RESET_AT = Date.now() + 3600_000;
    const entry = store.markRateLimited('user-uid-a', 'deepseek-v4.1-flash', {
      status: 429, code: 6004, resetAt: FUTURE_RESET_AT, message: QUOTA_MSG,
    });
    check('markRateLimited 返回 resetAt', entry.resetAt, FUTURE_RESET_AT);
    const pub = store.listAccounts().accounts.find(a => a.id === 'user-uid-a');
    check('公开字段含限额状态', pub.rateLimits['deepseek-v4.1-flash'].status, 429);
    check('公开字段含 code', pub.rateLimits['deepseek-v4.1-flash'].code, 6004);
    check('clearRateLimit 清除', store.clearRateLimit('user-uid-a', 'deepseek-v4.1-flash'), true);
    check('清除后无记录', store.listAccounts().accounts.find(a => a.id === 'user-uid-a').rateLimits['deepseek-v4.1-flash'], undefined);

    // 端到端：A 限额 → 自动切换 B 成功
    const calls = [];
    const storeAuth = {
      baseUrl: 'https://copilot.tencent.com',
      prefixPath: '/plugin',
      getCurrentSession: async () => store.getCurrentEntry()?.session || null,
      buildAuthHeaders: loaded => ({
        Authorization: `Bearer ${loaded.auth.accessToken}`,
        'X-User-Id': loaded.account.uid,
      }),
    };
    const client = createWorkBuddyUpstreamClient({
      auth: storeAuth,
      accountStore: store,
      log: () => {},
      fetchImpl: async (url, init) => {
        calls.push(init.headers);
        if (calls.length === 1) {
          return new Response(JSON.stringify({ code: 6004, msg: QUOTA_MSG, requestId: 'r1' }),
            { status: 429, headers: { 'Content-Type': 'application/json' } });
        }
        return new Response(SSE, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
      },
    });
    store.promoteToFront('user-uid-a');
    const res = makeRes();
    await client.forwardChatCompletions({
      req: {}, res,
      body: { model: 'deepseek-v4.1-flash', stream: false, messages: [{ role: 'user', content: 'hi' }] },
      controller: new AbortController(),
      dedupeKey: 'quota-failover-1',
    });
    check('切换后请求成功 200', res.status, 200);
    check('发起两次上游请求', calls.length, 2);
    check('第二次用备用账号 B', calls[1]['X-User-Id'], 'uid-b');
    // 当前账号是模型无关的派生值，不会因为某模型的限额而降级改写；
    // 降级只发生在请求层（见上面的「第二次用备用账号 B」）
    check('当前账号仍是队首 A', store.listAccounts().currentAccountId, 'user-uid-a');
    const marked = store.listAccounts().accounts.find(a => a.id === 'user-uid-a');
    check('A 账号留下 429 标记', marked.rateLimits['deepseek-v4.1-flash'].status, 429);
    const out = JSON.parse(res.payload);
    check('聚合内容正常', out.choices[0].message.content, '你好');

    // 全部账号限额：透传 429 错误
    store.clearRateLimit('user-uid-a', 'deepseek-v4.1-flash');
    store.promoteToFront('user-uid-a');
    let secondCalls = 0;
    const client2 = createWorkBuddyUpstreamClient({
      auth: storeAuth,
      accountStore: store,
      log: () => {},
      fetchImpl: async () => {
        secondCalls++;
        return new Response(JSON.stringify({ code: 6004, msg: QUOTA_MSG, requestId: 'r2' }),
          { status: 429, headers: { 'Content-Type': 'application/json' } });
      },
    });
    let thrown = null;
    try {
      await client2.forwardChatCompletions({
        req: {}, res: makeRes(),
        body: { model: 'deepseek-v4.1-flash', stream: true, messages: [{ role: 'user', content: 'hi' }] },
        controller: new AbortController(),
        dedupeKey: 'quota-failover-2',
      });
    } catch (error) { thrown = error; }
    check('全部限额时抛错', Boolean(thrown), true);
    check('错误码保持 429', thrown?.statusCode, 429);
    check('错误信息标注全部账号', /所有候选账号/.test(thrown?.message || ''), true);
    check('两个账号各试一次', secondCalls, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// ─── 14. 运行日志 ──────────────────────────────────────────

console.log('\n━━━ 15. 运行日志 ━━━');
{
  const dir = mkdtempSync(join(tmpdir(), 'wb-logs-test-'));
  try {
    const store = createLogStore({ directory: dir, log: () => {} });
    check('初始为空', store.size, 0);

    store.append({ level: 'info', category: 'server', message: '服务已启动' });
    store.append({ level: 'warn', category: 'account', message: '账号已限额', data: { model: 'm1', from: 'A', to: 'B' } });
    store.append({ level: 'error', category: 'upstream', message: '上游 502' });
    check('写入 3 条', store.size, 3);

    const all = store.query({});
    check('查询倒序（最新在前）', all.entries[0].message, '上游 502');
    check('查询总数', all.total, 3);
    check('条目含 id/ts/level', typeof all.entries[0].id === 'number' && all.entries[0].ts > 0, true);

    // 级别筛选是"含以上"
    check('level=warn 含 warn+error', store.query({ level: 'warn' }).matched, 2);
    check('level=error 仅 error', store.query({ level: 'error' }).matched, 1);
    check('category 筛选', store.query({ category: 'account' }).matched, 1);
    check('keyword 筛选', store.query({ keyword: '限额' }).matched, 1);
    check('sinceId 只取更新的', store.query({ sinceId: all.entries[0].id }).matched, 0);

    // 结构化数据（429 切换记录的核心）
    const limitEntry = store.query({ category: 'account' }).entries[0];
    check('429 记录保留 data.from', limitEntry.data?.from, 'A');
    check('429 记录保留 data.to', limitEntry.data?.to, 'B');
    check('429 记录保留 model', limitEntry.data?.model, 'm1');

    // 统计
    const stats = store.stats();
    check('stats.total', stats.total, 3);
    check('stats.byLevel.warn', stats.byLevel.warn, 1);
    check('stats.byCategory.account', stats.byCategory.account, 1);
    check('stats.lastId 为最大 id', stats.lastId, all.entries[0].id);

    // 非法级别/分类回落默认值，不产生脏数据
    store.append({ level: 'nonsense', category: 'nope', message: '回落到默认' });
    const fallback = store.query({}).entries[0];
    check('非法 level 回落 info', fallback.level, 'info');
    check('非法 category 回落 server', fallback.category, 'server');

    // 空消息不入库
    const before = store.size;
    store.append({ level: 'info', message: '   ' });
    check('空消息不入库', store.size, before);

    // 导出 JSONL
    const jsonl = store.toJsonl();
    check('导出为 JSONL（行数=条数）', jsonl.trim().split('\n').length, store.size);
    check('导出内容可解析', (() => { try { JSON.parse(jsonl.trim().split('\n')[0]); return true; } catch { return false; } })(), true);

    // 清空
    store.clear();
    check('清空后为空', store.size, 0);
    check('清空后统计归零', store.stats().total, 0);

    // 环形保留：写超上限只留最近 MAX_ENTRIES 条
    for (let i = 0; i < MAX_ENTRIES + 30; i++) {
      store.append({ level: 'info', category: 'server', message: `第 ${i} 条` });
    }
    check(`环形保留 ${MAX_ENTRIES} 条`, store.size, MAX_ENTRIES);
    check('保留的是最新的', store.query({}).entries[0].message, `第 ${MAX_ENTRIES + 29} 条`);

    // 重启后可查（从磁盘重新载入）
    const reloaded = createLogStore({ directory: dir, log: () => {} });
    check('重启后仍可查到历史', reloaded.size, MAX_ENTRIES);
    check('重启后 id 继续递增', reloaded.stats().lastId >= MAX_ENTRIES, true);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// ─── 15. 429 切换写入运行日志 ──────────────────────────────

console.log('\n━━━ 16. 429 切换日志 ━━━');
{
  const dir = mkdtempSync(join(tmpdir(), 'wb-logs-429-'));
  try {
    const store = createAccountStore({ directory: dir, log: () => {} });
    store.addAccount({
      auth: { accessToken: 'AT-a', refreshToken: 'RT-a', expiresAt: Date.now() + 3600_000 },
      account: { uid: 'uid-a', nickname: '账号A' },
    });
    store.addAccount({
      auth: { accessToken: 'AT-b', refreshToken: 'RT-b', expiresAt: Date.now() + 3600_000 },
      account: { uid: 'uid-b', nickname: '账号B' },
    });
    store.promoteToFront('user-uid-a');

    const QUOTA = '当前模型使用量已达上限，将在 2026-09-11 18:00:00 UTC+8 重置，您也可以切换其他模型继续使用。';
    // 日志库与账号库同目录，便于共用临时目录；先建好再供 logEvent 闭包引用
    const logStore = createLogStore({ directory: dir, log: () => {} });
    let calls = 0;
    const client = createWorkBuddyUpstreamClient({
      auth: {
        getCurrentSession: async () => store.getCurrentEntry()?.session || null,
        buildAuthHeaders: () => ({ Authorization: 'Bearer X', 'X-User-Id': 'u' }),
        get baseUrl() { return 'https://copilot.tencent.com'; },
      },
      accountStore: store,
      log: () => {},
      logEvent: event => logStore.append(event),
      fetchImpl: async () => {
        calls++;
        if (calls === 1) {
          return new Response(JSON.stringify({ code: 6004, msg: QUOTA }), { status: 429 });
        }
        return new Response(SSE, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
      },
    });

    await client.forwardChatCompletions({
      req: {}, res: makeRes(),
      // stream:false 走聚合路径（与第 13 节同款 mock），避免需要真实的可管道流
      body: { model: 'deepseek-v4.1-flash', stream: false, messages: [{ role: 'user', content: 'hi' }] },
      controller: new AbortController(),
      dedupeKey: 'log-429',
    });

    const events = logStore.query({ category: 'account' }).entries;
    // 降级日志的判定用「降级」：早期实现文案是「自动切换」，后来改成
    // 「按优先级降级」（与账号页的优先级语义一致），断言需跟着更新
    const switched = events.find(item => item.level === 'warn' && /降级/.test(item.message));
    check('429 切换写入运行日志', Boolean(switched), true);
    check('日志含原账号', switched?.data?.from, '账号A');
    check('日志含目标账号', switched?.data?.to, '账号B');
    check('日志含模型', switched?.data?.model, 'deepseek-v4.1-flash');
    check('日志含限额状态码', switched?.data?.status, 429);
    check('日志含恢复时间戳', Number(switched?.data?.resetAt) > Date.now(), true);
    check('日志消息含恢复时间文本', /恢复/.test(switched?.message || ''), true);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

console.log(`\n${failures === 0 ? '🎉 全部通过' : `⚠️ ${failures} 项失败`}\n`);
process.exit(failures === 0 ? 0 : 1);
