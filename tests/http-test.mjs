/**
 * 后端 HTTP 端到端测试：多账号 + 积分/签到路由。
 *
 * 用两个假账号 seed 到临时配置目录，起真实 server 进程，
 * 通过 HTTP 调用各路由，验证契约（不触上游，因为 token 是假的）。
 *
 * 运行: node http-test.mjs
 */

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const PORT = 3097;
const BASE = `http://127.0.0.1:${PORT}`;
const API_KEY = 'testkey-1234567890';

let failures = 0;
function check(label, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (!ok) failures++;
  console.log(`${ok ? '✅' : '❌'} ${label}: ${JSON.stringify(actual)}${ok ? '' : ` (期望 ${JSON.stringify(expected)})`}`);
}

const home = mkdtempSync(join(tmpdir(), 'wb-http-test-'));

// 预置两个账号（未给 priority，迁移后按加入时间编号 100/101，故第一个即当前账号）
mkdirSync(home, { recursive: true });
writeFileSync(join(home, 'accounts.json'), JSON.stringify({
  currentAccountId: 'user-uid-aaa',
  accounts: [
    {
      id: 'user-uid-aaa', name: '账号A', uid: 'uid-aaa', nickname: '小明', type: 'personal',
      accessToken: 'AT-aaa', refreshToken: 'RT-aaa', expiresAt: Date.now() + 3600_000,
      tokenTail: '-aaa', prefixPath: '/plugin', platform: 'workbuddy', addedAt: 1, updatedAt: 1,
    },
    {
      id: 'user-uid-bbb', name: '账号B', uid: 'uid-bbb', nickname: '小红', type: 'enterprise',
      enterpriseId: 'ent-bbb', accessToken: 'AT-bbb', refreshToken: 'RT-bbb',
      expiresAt: Date.now() + 3600_000, tokenTail: '-bbb', prefixPath: '/plugin',
      platform: 'workbuddy', addedAt: 2, updatedAt: 2,
    },
  ],
}, null, 2), 'utf8');
writeFileSync(join(home, 'config.json'), JSON.stringify({ apiKey: API_KEY }), 'utf8');

const child = spawn(process.execPath, [
  join(ROOT, 'server.mjs'), '--port', String(PORT),
], {
  cwd: ROOT,
  env: { ...process.env, WORKBUDDY_PROXY_HOME: home },
  stdio: ['ignore', 'pipe', 'pipe'],
});
let serverLog = '';
child.stdout.on('data', d => { serverLog += String(d); });
child.stderr.on('data', d => { serverLog += String(d); });

async function req(method, path, { body, key = API_KEY } = {}) {
  const headers = { Accept: 'application/json' };
  if (key) headers['x-api-key'] = key;
  const init = { method, headers };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    init.body = typeof body === 'string' ? body : JSON.stringify(body);
  }
  const res = await fetch(`${BASE}${path}`, init);
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* 非 JSON */ }
  return { status: res.status, json, text };
}

/** 等 server 就绪 */
async function waitReady(deadlineMs = 15_000) {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(`${BASE}/health`);
      if (r.ok) return true;
    } catch { /* 还没起来 */ }
    await new Promise(r => setTimeout(r, 200));
  }
  return false;
}

try {
  if (!await waitReady()) {
    console.error('server 未能启动，日志:\n', serverLog);
    process.exit(1);
  }

  console.log('\n━━━ 1. 鉴权 ━━━');
  {
    const noKey = await req('GET', '/api/accounts', { key: null });
    check('无 API Key → 401', noKey.status, 401);
    const bearer = await fetch(`${BASE}/api/accounts`, {
      headers: { Authorization: `Bearer ${API_KEY}` },
    });
    check('Bearer 形式也可用 → 200', bearer.status, 200);
  }

  console.log('\n━━━ 2. 账号列表 ━━━');
  {
    const r = await req('GET', '/api/accounts');
    check('状态码', r.status, 200);
    check('账号数', r.json.data.accounts.length, 2);
    check('当前账号', r.json.data.currentAccountId, 'user-uid-aaa');
    const a = r.json.data.accounts.find(x => x.id === 'user-uid-aaa');
    check('token 已脱敏（无明文 accessToken）', 'accessToken' in a, false);
    check('tokenTail', a.tokenTail, '-aaa');
    check('hasRefreshToken', a.hasRefreshToken, true);
    const b = r.json.data.accounts.find(x => x.id === 'user-uid-bbb');
    check('企业账号 enterpriseId', b.enterpriseId, 'ent-bbb');
    check('企业账号 type', b.type, 'enterprise');
  }

  console.log('\n━━━ 3. 会话总览 ━━━');
  {
    const r = await req('GET', '/api/session');
    check('状态码', r.status, 200);
    check('loggedIn', r.json.data.session.loggedIn, true);
    check('nickname', r.json.data.session.nickname, '小明');
    check('accountType', r.json.data.session.accountType, 'personal');
    check('模型数 > 20', r.json.data.models.length > 20, true);
    check('模型含 credits 字段', 'credits' in r.json.data.models[0], true);
  }

  console.log('\n━━━ 4. 切换当前账号 ━━━');
  {
    const r = await req('POST', '/api/accounts/current', { body: { id: 'user-uid-bbb' } });
    check('状态码', r.status, 200);
    check('返回新当前账号', r.json.data.currentAccountId, 'user-uid-bbb');
    const after = await req('GET', '/api/accounts');
    check('持久化生效', after.json.data.currentAccountId, 'user-uid-bbb');
    const sess = await req('GET', '/api/session');
    check('会话跟随切换', sess.json.data.session.accountUid, 'uid-bbb');
    check('切换后类型为企业', sess.json.data.session.accountType, 'enterprise');
    // 切回，方便后续断言
    await req('POST', '/api/accounts/current', { body: { id: 'user-uid-aaa' } });
  }

  console.log('\n━━━ 5. 切换账号的参数校验 ━━━');
  {
    const missing = await req('POST', '/api/accounts/current', { body: {} });
    check('缺 id → 400', missing.status, 400);
    const notFound = await req('POST', '/api/accounts/current', { body: { id: 'nope' } });
    check('不存在的 id → 404', notFound.status, 404);
  }

  console.log('\n━━━ 6. 多账号批量积分（假 token → 上游 401）━━━');
  {
    const r = await req('GET', '/api/accounts/usage');
    check('状态码 200（单账号失败不拖垮整体）', r.status, 200);
    check('返回 2 个结果', r.json.data.results.length, 2);
    check('每个结果都带 id', r.json.data.results.every(x => typeof x.id === 'string'), true);
    check('假 token 下 usage 为 null', r.json.data.results.every(x => x.usage === null), true);
    check('失败原因已记录', r.json.data.results.every(x => typeof x.error === 'string' && x.error.length > 0), true);
    console.log(`   （实际错误：${r.json.data.results[0].error.slice(0, 60)}…）`);
  }

  console.log('\n━━━ 7. 多账号批量签到（假 token）━━━');
  {
    const r = await req('POST', '/api/accounts/checkin', { body: {} });
    check('状态码 200', r.status, 200);
    check('total', r.json.data.total, 2);
    check('succeeded=0（假 token）', r.json.data.succeeded, 0);
    check('逐账号返回结果', r.json.data.results.length, 2);
  }

  console.log('\n━━━ 8. 指定单个账号签到 ━━━');
  {
    const r = await req('POST', '/api/accounts/checkin', { body: { id: 'user-uid-aaa' } });
    check('只处理 1 个账号', r.json.data.total, 1);
    check('返回该账号 id', r.json.data.results[0].id, 'user-uid-aaa');
    const bad = await req('POST', '/api/accounts/checkin', { body: { id: 'nope' } });
    check('不存在的 id → 404', bad.status, 404);
  }

  console.log('\n━━━ 9. 当前账号积分 / 签到 ━━━');
  {
    const usage = await req('GET', '/api/usage');
    check('假 token → 401', usage.status, 401);
    const status = await req('GET', '/api/checkin/status');
    check('签到状态 → 401', status.status, 401);
    const claim = await req('POST', '/api/checkin', { body: {} });
    check('签到 → 401', claim.status, 401);
  }

  console.log('\n━━━ 10. 新增账号 / 参数校验 ━━━');
  {
    const added = await req('POST', '/api/accounts', {
      body: { auth: { accessToken: 'AT-ccc', refreshToken: 'RT-ccc' }, account: { uid: 'uid-ccc', nickname: '小刚' } },
    });
    check('新增状态码', added.status, 200);
    check('新增后账号数', added.json.data.list.accounts.length, 3);
    // 新账号排在队尾，不会抢占当前账号
    check('新增不抢占当前账号', added.json.data.list.currentAccountId, 'user-uid-aaa');

    const badJson = await req('POST', '/api/accounts', { body: '{not-json' });
    check('非法 JSON → 400', badJson.status, 400);
    const noToken = await req('POST', '/api/accounts', { body: { account: { uid: 'x' } } });
    check('缺 accessToken → 400', noToken.status, 400);
    const noUid = await req('POST', '/api/accounts', { body: { auth: { accessToken: 'x' } } });
    check('缺 uid → 400', noUid.status, 400);
  }

  console.log('\n━━━ 11. 刷新 token（假 refreshToken）━━━');
  {
    const r = await req('POST', '/api/accounts/refresh', { body: { id: 'user-uid-aaa' } });
    check('上游拒绝 → 非 2xx', r.status >= 400, true);
    check('返回 success:false', r.json.success, false);
    const noRt = await req('POST', '/api/accounts/refresh', { body: { id: 'user-uid-ccc' } });
    check('有 refreshToken 但被拒 → 非 2xx', noRt.status >= 400, true);
  }

  console.log('\n━━━ 12. 删除账号 ━━━');
  {
    const del = await req('DELETE', '/api/accounts/user-uid-ccc');
    check('删除状态码', del.status, 200);
    check('删除后账号数', del.json.data.list.accounts.length, 2);
    const again = await req('DELETE', '/api/accounts/user-uid-ccc');
    check('重复删除 → 404', again.status, 404);
  }

  console.log('\n━━━ 12b. 国际版账号不参与签到 ━━━');
  {
    // 加一个国际版账号，验证批量签到跳过它、单账号签到明确报错
    const added = await req('POST', '/api/accounts', {
      body: {
        auth: { accessToken: 'AT-intl', refreshToken: 'RT-intl' },
        account: { uid: 'uid-intl', nickname: '国际用户' },
        edition: 'intl',
      },
    });
    check('国际版账号已加入', added.status, 200);

    const all = await req('POST', '/api/accounts/checkin', { body: {} });
    check('批量签到只处理国内账号', all.json.data.total, 2);
    check('跳过 1 个国际版账号', all.json.data.skipped, 1);
    check('结果不含国际版账号', all.json.data.results.some(r => r.id === 'user-uid-intl'), false);

    const single = await req('POST', '/api/accounts/checkin', { body: { id: 'user-uid-intl' } });
    check('单账号签到国际版 → 400', single.status, 400);
    check('错误信息说明原因', /国际版/.test(single.json.error), true);

    await req('DELETE', '/api/accounts/user-uid-intl');
  }

  console.log('\n━━━ 13. 接口清单 / 网关配置 ━━━');
  {
    const ep = await req('GET', '/api/endpoints');
    check('状态码', ep.status, 200);
    check('prefixPath', ep.json.data.prefixPath, '/plugin');
    check('platform', ep.json.data.platform, 'workbuddy');
    check('含 billing.checkinStatus', !!ep.json.data.billing.checkinStatus, true);
    check('含 llm.chatCompletions', !!ep.json.data.llm.chatCompletions, true);

    const cfg = await req('GET', '/api/config');
    check('apiKeySet', cfg.json.data.apiKeySet, true);
    check('apiKey 已掩码', /^testke\.\.\./.test(cfg.json.data.apiKey), true);
    check('defaultModel', cfg.json.data.defaultModel, 'auto');
  }

  console.log('\n━━━ 14. 模型列表 ━━━');
  {
    const r = await req('GET', '/v1/models');
    check('状态码', r.status, 200);
    check('object', r.json.object, 'list');
    check('owned_by', r.json.data[0].owned_by, 'workbuddy');
    check('auto 为默认', r.json.data.find(m => m.id === 'auto').is_default, true);
  }

  console.log('\n━━━ 15. 未知模型直接 400 ━━━');
  {
    // 目录里没有的模型必须报错，绝不静默改写/回退（请求 4.1 实跑别的模型就是事故）。
    // 注意 deepseek-v4.1-flash 是内置清单里的合法模型，不能用它测未知模型。
    const r = await req('POST', '/v1/chat/completions', {
      body: { model: 'deepseek-v4.1-flashX', messages: [{ role: 'user', content: 'hi' }] },
    });
    check('状态码 400', r.status, 400);
    check('错误 code', r.json.error?.code, 'model_not_found');
    check('报错含请求的模型名', r.json.error?.message.includes('deepseek-v4.1-flashX'), true);
    check('报错提示相近模型', r.json.error?.message.includes('deepseek-v4.1-flash'), true);
    check('不会给出模型响应体', r.json.object ?? null, null);
    check('不会打到上游（无 chat id）', r.json.id ?? null, null);

    // 完全无关的模型名：有错误但不硬编相近提示
    const bare = await req('POST', '/v1/chat/completions', {
      body: { model: 'gpt-4o', messages: [{ role: 'user', content: 'hi' }] },
    });
    check('无关模型名 → 400', bare.status, 400);
    check('无关模型名 code', bare.json.error?.code, 'model_not_found');
  }

  console.log('\n━━━ 16. 敏感词脱敏（HTTP）━━━');
  {
    const initial = await req('GET', '/api/desensitize');
    check('状态码', initial.status, 200);
    check('默认开启', initial.json.data.enabled, true);
    check('默认作用角色 system+user', initial.json.data.roles.join(','), 'system,user');
    check('默认词表非空', initial.json.data.terms.length > 0, true);
    check('词表文件路径', initial.json.data.file.endsWith('desensitize.json'), true);

    // 追加自定义词，并验证去重
    const added = await req('POST', '/api/desensitize/terms', {
      body: { terms: ['acme-internal-secret', 'acme-internal-secret'] },
    });
    check('追加词 200', added.status, 200);
    check('追加后包含新词', added.json.data.terms.includes('acme-internal-secret'), true);
    const before = initial.json.data.terms.length;
    check('同请求内去重', added.json.data.termCount, before + 1);

    // /api/config 与 /api/session 里的摘要
    const cfg = await req('GET', '/api/config');
    check('config 带脱敏摘要', cfg.json.data.desensitize.enabled, true);
    check('config 脱敏词数', cfg.json.data.desensitize.termCount, before + 1);
    const sess = await req('GET', '/api/session');
    check('session 带脱敏摘要', sess.json.data.desensitize.enabled, true);

    // 开关：关闭后重新读取应保持关闭
    const off = await req('POST', '/api/desensitize/enabled', { body: { enabled: false } });
    check('关闭 200', off.status, 200);
    check('关闭后 enabled=false', off.json.data.enabled, false);
    const reread = await req('GET', '/api/desensitize');
    check('关闭状态已持久化', reread.json.data.enabled, false);
    check('关闭后词表仍在', reread.json.data.terms.includes('acme-internal-secret'), true);

    // 重新开启，便于后续验证
    await req('POST', '/api/desensitize/enabled', { body: { enabled: true } });

    // 删词
    const removed = await req('DELETE', '/api/desensitize/terms', { body: { terms: ['acme-internal-secret'] } });
    check('删词 200', removed.status, 200);
    check('删词后不含该词', removed.json.data.terms.includes('acme-internal-secret'), false);

    // 非法入参
    const bad = await req('POST', '/api/desensitize/terms', { body: { terms: [] } });
    check('空词表 → 400', bad.status, 400);
    const badEnabled = await req('POST', '/api/desensitize/enabled', { body: { enabled: 'true' } });
    check('enabled 非布尔 → 400', badEnabled.status, 400);

    // 恢复默认
    const reset = await req('POST', '/api/desensitize/reset', { body: {} });
    check('恢复默认 200', reset.status, 200);
    check('恢复后词数', reset.json.data.termCount, initial.json.data.terms.length);

    // 命中统计接口
    const stats = await req('POST', '/api/desensitize/stats/reset', { body: {} });
    check('清空统计 200', stats.status, 200);
    check('清空后命中为 0', stats.json.data.stats.totalHits, 0);

    // 鉴权
    const noKey = await req('GET', '/api/desensitize', { key: null });
    check('无 API Key → 401', noKey.status, 401);
  }

  console.log('\n━━━ 17. 运行日志接口 ━━━');
  {
    // 启动期已产生若干日志（Config/Init/Server）
    const list = await req('GET', '/api/logs');
    check('日志列表 200', list.status, 200);
    check('返回 entries 数组', Array.isArray(list.json.data.entries), true);
    check('返回分类字典', typeof list.json.data.categories.account === 'string', true);
    check('返回级别列表', Array.isArray(list.json.data.levels), true);
    check('启动期已有日志', list.json.data.total > 0, true);
    const newest = list.json.data.entries[0];
    check('条目含 id/ts/level/category/message',
      typeof newest.id === 'number' && newest.ts > 0 && typeof newest.level === 'string'
      && typeof newest.category === 'string' && typeof newest.message === 'string', true);

    // 统计
    const stats = await req('GET', '/api/logs/stats');
    check('统计 200', stats.status, 200);
    check('统计 total 与列表一致', stats.json.data.total, list.json.data.total);
    check('统计含 byLevel', typeof stats.json.data.byLevel.info === 'number', true);
    check('统计含 lastId', typeof stats.json.data.lastId === 'number', true);

    // 筛选
    const filtered = await req('GET', '/api/logs?category=config&limit=10');
    check('category 筛选 200', filtered.status, 200);
    check('筛选结果均为 config',
      filtered.json.data.entries.every(e => e.category === 'config'), true);
    const warnOnly = await req('GET', '/api/logs?level=error');
    check('level=error 筛选生效',
      warnOnly.json.data.entries.every(e => e.level === 'error'), true);
    const noMatch = await req('GET', '/api/logs?keyword=绝不可能出现的词xyz');
    check('keyword 无匹配返回空', noMatch.json.data.entries.length, 0);
    check('无匹配时 total 仍为全量', noMatch.json.data.total, list.json.data.total);

    // 导出
    const download = await fetch(`${BASE}/api/logs/download`, { headers: { 'X-API-Key': API_KEY } });
    check('导出 200', download.status, 200);
    check('导出为 ndjson', download.headers.get('content-type')?.includes('ndjson'), true);
    const text = await download.text();
    check('导出内容非空', text.trim().length > 0, true);
    check('导出行数 = total', text.trim().split('\n').length, list.json.data.total);

    // 鉴权
    const noKey = await req('GET', '/api/logs', { key: null });
    check('无 API Key → 401', noKey.status, 401);

    // 清空（放最后，避免影响上面的断言）
    const cleared = await req('DELETE', '/api/logs');
    check('清空 200', cleared.status, 200);
    check('清空后 total=0', cleared.json.data.total, 0);
    // 清空动作本身会记一条（"[Logs] 运行日志已清空"），所以列表里还剩这 1 条
    const afterClear = await req('GET', '/api/logs');
    check('清空后仅剩清空动作自身记录', afterClear.json.data.entries.length, 1);
    check('残留条目为清空记录', /已清空/.test(afterClear.json.data.entries[0].message), true);
  }

  console.log('\n━━━ 18. 登录取消 ━━━');
  {
    // 取消不存在的 state：幂等返回 canceled=false，不报错
    const missing = await req('POST', '/api/session/login/cancel', { body: { state: 'no-such-state' } });
    check('取消未知 state 返回 200', missing.status, 200);
    check('未知 state 的 canceled=false', missing.json.data.canceled, false);
    const noState = await req('POST', '/api/session/login/cancel', { body: {} });
    check('缺 state 也不报错', noState.status, 200);
    check('缺 state 的 canceled=false', noState.json.data.canceled, false);
    // 等待未知 state 的登录进度仍按原逻辑 404
    const wait = await req('GET', '/api/session/login/wait?state=no-such-state');
    check('等待未知 state → 404', wait.status, 404);
    // 鉴权
    const noKey = await req('POST', '/api/session/login/cancel', { body: { state: 'x' }, key: null });
    check('取消接口无 Key → 401', noKey.status, 401);
  }

  console.log('\n━━━ 19. 404 ━━━');
  {
    const r = await req('GET', '/api/nope');
    check('未知路由 → 404', r.status, 404);
    const acc = await req('GET', '/api/accounts/nope/xyz');
    check('账号子路径未知 → 404', acc.status, 404);
  }
} finally {
  child.kill();
  await new Promise(r => setTimeout(r, 300));
  rmSync(home, { recursive: true, force: true });
}

console.log(`\n${failures === 0 ? '🎉 全部通过' : `⚠️ ${failures} 项失败`}\n`);
process.exit(failures === 0 ? 0 : 1);
