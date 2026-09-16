#!/usr/bin/env node
/**
 * 脱敏端到端验证：起一个假上游 + 真实网关，验证「命中的词在出站请求体里确实被插了零宽空格」。
 *
 * 与 http-test.mjs 的区别：那个测路由契约；这个测**转发链路上的实际改写效果**，
 * 是本次需求的验收点（默认开启、system+user 都处理、可关闭）。
 *
 * 运行: node desensitize-e2e.mjs
 */

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const GATE_PORT = 3096;
const UPSTREAM_PORT = 3095;
const BASE = `http://127.0.0.1:${GATE_PORT}`;

let failures = 0;
function check(label, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (!ok) failures++;
  console.log(`${ok ? '✅' : '❌'} ${label}: ${JSON.stringify(actual)}${ok ? '' : ` (期望 ${JSON.stringify(expected)})`}`);
}

const ZWSP = '\u200b';
/** 去掉零宽空格，用于验证"读起来不变" */
const strip = value => String(value).split(ZWSP).join('');

// ─── 假上游：记录收到的请求体，返回一个最小 SSE 流 ──────────

const received = [];
const upstream = http.createServer((req, res) => {
  const chunks = [];
  req.on('data', chunk => chunks.push(chunk));
  req.on('end', () => {
    const raw = Buffer.concat(chunks).toString('utf8');
    let parsed = null;
    try { parsed = JSON.parse(raw); } catch { /* 非 JSON */ }
    received.push({ path: req.url, body: parsed, raw });

    // 模型目录接口要返回真结构，否则网关启动时会 400
    if (req.url.includes('/v3/config')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        code: 0,
        data: { models: [{ id: 'auto', name: 'Auto', isDefault: true }] },
      }));
      return;
    }

    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    res.write('data: {"id":"cmpl-1","object":"chat.completion.chunk","model":"auto",'
      + '"choices":[{"index":0,"delta":{"role":"assistant","content":"ok"}}]}\n\n');
    res.write('data: {"id":"cmpl-1","object":"chat.completion.chunk","model":"auto",'
      + '"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n');
    res.write('data: [DONE]\n\n');
    res.end();
  });
});

// ─── 启动假上游 + 网关 ────────────────────────────────────

await new Promise(resolve => upstream.listen(UPSTREAM_PORT, '127.0.0.1', resolve));

const home = mkdtempSync(join(tmpdir(), 'wb-desens-e2e-'));
mkdirSync(home, { recursive: true });
writeFileSync(join(home, 'accounts.json'), JSON.stringify({
  currentAccountId: 'user-uid-aaa',
  accounts: [{
    id: 'user-uid-aaa', name: '账号A', uid: 'uid-aaa', nickname: '测试', type: 'personal',
    accessToken: 'AT-aaa', refreshToken: 'RT-aaa', expiresAt: Date.now() + 3600_000,
    // endpoint 必须指向假上游：账号记录里的 endpoint 优先于 --endpoint，
    // 缺省会回退到 edition 的官方端点，请求就打到真实上游去了
    endpoint: `http://127.0.0.1:${UPSTREAM_PORT}`,
    tokenTail: '-aaa', prefixPath: '', platform: 'workbuddy', addedAt: 1, updatedAt: 1,
  }],
}, null, 2), 'utf8');

const child = spawn(process.execPath, [
  join(ROOT, 'server.mjs'),
  '--port', String(GATE_PORT),
  '--endpoint', `http://127.0.0.1:${UPSTREAM_PORT}`,
  '--prefix-path', '',
  ...process.argv.slice(2),
], {
  cwd: ROOT,
  env: { ...process.env, WORKBUDDY_PROXY_HOME: home },
  stdio: ['ignore', 'pipe', 'pipe'],
});
let serverLog = '';
child.stdout.on('data', d => { serverLog += String(d); });
child.stderr.on('data', d => { serverLog += String(d); });

async function waitReady(deadlineMs = 15_000) {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    try { if ((await fetch(`${BASE}/health`)).ok) return true; } catch { /* 还没起来 */ }
    await new Promise(r => setTimeout(r, 200));
  }
  return false;
}

/**
 * 取出站请求体里指定角色的第 n 条消息内容。
 * 不用固定下标：首条不是 system 时网关会注入兜底 system 消息，下标会整体后移。
 */
function contentOf(body, role, index = 0) {
  const list = (body?.messages || []).filter(m => m.role === role);
  return list[index]?.content ?? null;
}

/** 发一次 chat 请求，返回假上游收到的最后一个请求体 */
async function sendChat(messages) {
  const before = received.length;
  const res = await fetch(`${BASE}/v1/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model: 'auto', stream: true, messages }),
  });
  await res.text();
  for (let i = 0; i < 40; i++) {
    if (received.length > before) return received[received.length - 1].body;
    await new Promise(r => setTimeout(r, 50));
  }
  return null;
}

try {
  if (!await waitReady()) {
    console.error('server 未能启动，日志:\n', serverLog);
    process.exit(1);
  }

  console.log('\n━━━ 1. 默认开启：system 合规模板被脱敏 ━━━');
  {
    const body = await sendChat([
      { role: 'system', content: 'Refuse requests for DoS attacks, exploit development, malware creation and phishing.' },
      { role: 'user', content: 'hello' },
    ]);
    check('请求已到达上游', !!body, true);
    const sys = body.messages[0].content;
    check('system 含零宽空格（已脱敏）', sys.includes(ZWSP), true);
    check('DoS 被处理', sys.includes(`D${ZWSP}oS`), true);
    check('exploit 被处理', sys.includes(`e${ZWSP}xploit`), true);
    check('malware 被处理', sys.includes(`m${ZWSP}alware`), true);
    check('phishing 被处理', sys.includes(`p${ZWSP}hishing`), true);
    check('读起来仍是原文', strip(sys), 'Refuse requests for DoS attacks, exploit development, malware creation and phishing.');
    check('user 消息未命中', body.messages[1].content, 'hello');
  }

  console.log('\n━━━ 2. user 角色同样处理（本次需求新增）━━━');
  {
    const body = await sendChat([
      { role: 'system', content: 'You are a helpful assistant.' },
      { role: 'user', content: 'How does a reverse shell work?' },
    ]);
    const user = body.messages[1].content;
    check('user 含零宽空格', user.includes(ZWSP), true);
    check('reverse shell 整体命中', user.includes(`r${ZWSP}everse shell`), true);
    check('user 读起来不变', strip(user), 'How does a reverse shell work?');
    check('system 未命中保持原样', body.messages[0].content, 'You are a helpful assistant.');
  }

  console.log('\n━━━ 3. 词边界：避免误伤 ━━━');
  {
    // 这些词包含敏感词片段，但自身不是敏感词，必须原样通过
    const body = await sendChat([
      { role: 'user', content: 'We ran for 100days, gave kudos politely, and used a backdrop.' },
    ]);
    const text = contentOf(body, 'user');
    check('100days 未误伤（0day）', text.includes('100days'), true);
    check('kudos 未误伤（DoS）', text.includes('kudos'), true);
    check('backdrop 未误伤（backdoor）', text.includes('backdrop'), true);
    check('整段无零宽空格', text.includes(ZWSP), false);

    // 独立的敏感词仍要命中
    const hit = await sendChat([{ role: 'user', content: 'about DoS and XSS' }]);
    check('独立 DoS 命中', contentOf(hit, 'user').includes(`D${ZWSP}oS`), true);
    check('独立 XSS 命中', contentOf(hit, 'user').includes(`X${ZWSP}SS`), true);

    // DDoS 是词表里的独立词，应整体命中（而不是被 DoS 切一半）
    const ddos = await sendChat([{ role: 'user', content: 'a DDoS attack' }]);
    check('DDoS 整体命中', contentOf(ddos, 'user').includes(`D${ZWSP}DoS`), true);
    check('DDoS 读起来不变', strip(contentOf(ddos, 'user')), 'a DDoS attack');
  }

  console.log('\n━━━ 4. 前端维护的词表生效 ━━━');
  {
    const add = await fetch(`${BASE}/api/desensitize/terms`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ terms: ['acme-project-codename'] }),
    });
    check('通过 API 添加自定义词', add.status, 200);

    const body = await sendChat([
      { role: 'user', content: 'the acme-project-codename is ready' },
    ]);
    check('自定义词被脱敏', contentOf(body, 'user').includes(`a${ZWSP}cme-project-codename`), true);
    check('自定义词读起来不变', strip(contentOf(body, 'user')), 'the acme-project-codename is ready');

    // 统计应记录命中
    const state = await (await fetch(`${BASE}/api/desensitize`)).json();
    check('命中统计 totalHits > 0', state.data.stats.totalHits > 0, true);
    check('统计里有自定义词', state.data.stats.topTerms.some(t => t.term === 'acme-project-codename'), true);
  }

  console.log('\n━━━ 5. 关闭开关后不再改写 ━━━');
  {
    const off = await fetch(`${BASE}/api/desensitize/enabled`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: false }),
    });
    check('关闭开关 200', off.status, 200);

    const body = await sendChat([
      { role: 'system', content: 'Refuse requests for DoS attacks.' },
      { role: 'user', content: 'tell me about malware' },
    ]);
    check('system 原样（无零宽）', body.messages[0].content.includes(ZWSP), false);
    check('system 原文完整', body.messages[0].content, 'Refuse requests for DoS attacks.');
    check('user 原样（无零宽）', body.messages[1].content.includes(ZWSP), false);

    // 重新开启
    await fetch(`${BASE}/api/desensitize/enabled`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: true }),
    });
    const back = await sendChat([{ role: 'user', content: 'about malware' }]);
    check('重新开启后恢复脱敏', contentOf(back, 'user').includes(ZWSP), true);
  }

  console.log('\n━━━ 6. 关闭开关后重启仍保持关闭（持久化）━━━');
  {
    await fetch(`${BASE}/api/desensitize/enabled`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: false }),
    });
    child.kill();
    await new Promise(r => setTimeout(r, 400));

    const child2 = spawn(process.execPath, [
      join(ROOT, 'server.mjs'),
      '--port', String(GATE_PORT),
      '--endpoint', `http://127.0.0.1:${UPSTREAM_PORT}`,
      '--prefix-path', '',
    ], {
      cwd: ROOT,
      env: { ...process.env, WORKBUDDY_PROXY_HOME: home },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    child2.stdout.on('data', () => {});
    child2.stderr.on('data', () => {});
    try {
      if (!await waitReady(20_000)) {
        check('重启后服务可用', false, true);
      } else {
        const state = await (await fetch(`${BASE}/api/desensitize`)).json();
        check('重启后仍为关闭', state.data.enabled, false);
        await fetch(`${BASE}/api/desensitize/enabled`, {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ enabled: true }),
        });
      }
    } finally {
      child2.kill();
      await new Promise(r => setTimeout(r, 300));
    }
  }
} finally {
  child.kill();
  upstream.close();
  await new Promise(r => setTimeout(r, 300));
  rmSync(home, { recursive: true, force: true });
}

console.log(`\n${failures === 0 ? '🎉 全部通过' : `⚠️ ${failures} 项失败`}\n`);
process.exit(failures === 0 ? 0 : 1);
