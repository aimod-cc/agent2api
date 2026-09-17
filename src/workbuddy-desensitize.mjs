/**
 * workbuddy-desensitize — 敏感词脱敏（本地网关侧）
 *
 * 思路对齐 codebuddy 的 codebuddy-desensitize.mjs（以及 codebuddy2openai 的 desensitize.py）：
 * 命中词内部插入零宽空格（U+200B），打断上游后端的关键词匹配，
 * 人眼与模型读到的仍是原词（"DoS" → "Do\u200bS"）。
 *
 * 与 codebuddy 版的差异：
 * - 默认开启（可用 --no-desensitize / 桌面端开关关闭）
 * - 作用角色默认 system + user（客户端合规模板与用户输入都处理）
 * - 词表可在桌面端维护，持久化到 {directory}/desensitize.json
 *
 * 纯函数（compileTerms / desensitizeText / desensitizeMessages）无状态、可单独测试；
 * createDesensitizer 负责词表读写与命中统计。
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

/** 零宽空格：插在词内部打断关键词匹配，不影响阅读 */
export const ZWSP = '\u200b';

export const FILE_NAME = 'desensitize.json';

/** 默认处理这两种角色：system 是客户端合规模板的集中地，user 是用户真实输入 */
export const DEFAULT_ROLES = Object.freeze(['system', 'user']);
const VALID_ROLES = Object.freeze(['system', 'user', 'assistant', 'tool', 'developer']);

export const MAX_TERMS = 2000;
export const MAX_TERM_LENGTH = 200;

/**
 * 默认词表：客户端固定 system 模板里的合规声明高频词。
 * 这些词取自真实被上游审核拦下的模板（"拒绝协助 DoS 攻击 / 漏洞利用开发 /
 * 凭证测试 / C2 框架…"），属于「拒绝作恶」的声明而非有害输入，却会被误判。
 *
 * 末条 `Main branch (you will usually use this for PRs)` 是客户端 system 模板里被上游
 * 审核整句误拦的真实文案（Git 工作流说明，本身无害）——整句作为一个词条，不要拆开。
 */
export const DEFAULT_TERMS = Object.freeze([
  'DoS',
  'DDoS',
  'exploit',
  'credential testing',
  'credential stuffing',
  'supply chain compromise',
  'supply-chain compromise',
  'detection evasion',
  'C2 frameworks',
  'C2 framework',
  'command and control',
  'malicious purposes',
  'malicious intent',
  'mass targeting',
  'brute force',
  'brute-force',
  'privilege escalation',
  'reverse shell',
  'remote code execution',
  'SQL injection',
  'XSS',
  'CSRF',
  'phishing',
  'malware',
  'ransomware',
  'keylogger',
  'rootkit',
  'backdoor',
  'botnet',
  'zero-day',
  '0day',
  'Main branch (you will usually use this for PRs)',
]);

/** 默认词表当前版本：每次往 DEFAULT_TERMS 追加新词条时 +1，并在下面迁移表登记 */
export const DEFAULT_TERMS_VERSION = 2;

/**
 * 默认词表补词迁移表：版本号 → 该版本新增的默认词条。
 *
 * 老用户的词表持久化在 desensitize.json，默认词表更新后不会自动生效，
 * 因此按版本做一次性合并：文件里记录已合并到哪个版本（defaultsVersion），
 * 启动加载时把缺失的新词条按忽略大小写补进去，合并完落盘并更新版本标记。
 *
 * 用"只补登记词条"而不是"与 DEFAULT_TERMS 求并集"，是为了保护用户的删除权：
 * 用户主动删掉的词条（无论新旧默认词）不会在下次启动时被加回来。
 * 未来往 DEFAULT_TERMS 加新词条时：版本 +1、登记一条迁移即可，老用户自动生效。
 *
 * 注意：词条写法必须与 DEFAULT_TERMS 中的条目逐字符一致（零宽空格是运行时由
 * zeroWidthSplit 插入的，词表本身是纯文本），否则忽略大小写比对会认成两个词。
 */
export const DEFAULT_TERM_MIGRATIONS = Object.freeze([
  { version: 2, terms: ['Main branch (you will usually use this for PRs)'] },
]);

// ─── 词表清洗与编译 ────────────────────────────────────────

/**
 * 清洗词表：去空白、丢弃空词与超长词、按忽略大小写去重（保留首次出现的写法）、截断到上限。
 */
export function normalizeTerms(input) {
  if (!Array.isArray(input)) return [];
  const seen = new Set();
  const result = [];
  for (const raw of input) {
    if (typeof raw !== 'string') continue;
    const term = raw.trim();
    if (!term || term.length > MAX_TERM_LENGTH) continue;
    const key = term.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    result.push(term);
    if (result.length >= MAX_TERMS) break;
  }
  return result;
}

/** 角色白名单过滤，保持 DEFAULT_ROLES 的顺序无关，仅做去重与合法性校验 */
export function normalizeRoles(input) {
  if (!Array.isArray(input)) return [...DEFAULT_ROLES];
  const roles = VALID_ROLES.filter(role => input.includes(role));
  return roles.length ? roles : [...DEFAULT_ROLES];
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** 纯 ASCII 词可加词边界；含中文等非 ASCII 的词只能做子串匹配（中文没有词边界） */
const ASCII_TERM = /^[\x20-\x7e]+$/;

/**
 * 词表 → 正则（忽略大小写）。长词优先，避免短词先把长词吃掉。
 * ASCII 词加 (?<![A-Za-z0-9]) / (?![A-Za-z0-9]) 边界，
 * 否则 "0day" 会命中 "100days"、"XSS" 会命中 "XSSRF" 这类误伤。
 * 词表为空返回 null（调用方据此跳过脱敏）。
 */
export function compileTerms(terms) {
  const list = normalizeTerms(terms);
  if (!list.length) return null;
  const parts = list
    .slice()
    .sort((a, b) => b.length - a.length)
    .map(term => (ASCII_TERM.test(term)
      ? `(?<![A-Za-z0-9])${escapeRegExp(term)}(?![A-Za-z0-9])`
      : escapeRegExp(term)));
  return new RegExp(parts.join('|'), 'gi');
}

/** 在词内插入零宽空格。单字符词直接追加（无内部位置可插） */
function zeroWidthSplit(term) {
  if (term.length <= 1) return term + ZWSP;
  return term[0] + ZWSP + term.slice(1);
}

// ─── 纯函数 ────────────────────────────────────────────────

/**
 * 单段文本脱敏；pattern 为 compileTerms 的产物（null 时原样返回）。
 * counter 可选，用于累计命中词（{ total, terms: Map }）。
 */
export function desensitizeText(text, pattern, counter) {
  if (!text || typeof text !== 'string' || !pattern) return text;
  return text.replace(pattern, match => {
    if (counter) {
      counter.total += 1;
      counter.terms.set(match, (counter.terms.get(match) || 0) + 1);
    }
    return zeroWidthSplit(match);
  });
}

/** 处理 OpenAI content：字符串，或 [{type:'text', text}, ...] 多模态数组 */
function scrubContent(content, pattern, counter) {
  if (typeof content === 'string' || !Array.isArray(content)) {
    return desensitizeText(content, pattern, counter);
  }
  let touched = false;
  const next = content.map(block => {
    if (!block || typeof block !== 'object' || block.type !== 'text' || typeof block.text !== 'string') {
      return block;
    }
    const text = desensitizeText(block.text, pattern, counter);
    if (text === block.text) return block;
    touched = true;
    return { ...block, text };
  });
  return touched ? next : content;
}

/**
 * 对指定角色的消息做脱敏，返回新数组（不修改原对象）。
 * 未命中时逐条返回原引用，便于调用方用 === 判断是否需要换 body。
 */
export function desensitizeMessages(messages, { pattern, roles = DEFAULT_ROLES, counter } = {}) {
  if (!Array.isArray(messages) || !pattern) return messages;
  let touched = false;
  const next = messages.map(message => {
    if (!message || typeof message !== 'object') return message;
    if (!roles.includes(message.role)) return message;
    const content = scrubContent(message.content, pattern, counter);
    if (content === message.content) return message;
    touched = true;
    return { ...message, content };
  });
  return touched ? next : messages;
}

/** 对请求体做脱敏（浅拷贝）；无 messages 或未命中时原样返回 */
export function desensitizeBody(body, options = {}) {
  if (!body || !Array.isArray(body.messages)) return body;
  const messages = desensitizeMessages(body.messages, options);
  return messages === body.messages ? body : { ...body, messages };
}

// ─── 状态化：词表读写 + 命中统计 ────────────────────────────

/**
 * 创建脱敏器。enabled 默认 true（无配置文件时即默认开启）。
 *
 * @param {object} options
 * @param {string} options.directory 配置目录（与 accounts.json 同级）
 * @param {Function} [options.log]
 */
export function createDesensitizer({ directory, log = () => {} } = {}) {
  const file = join(directory, FILE_NAME);

  let enabled = true;
  let terms = [...DEFAULT_TERMS];
  let roles = [...DEFAULT_ROLES];
  let pattern = compileTerms(terms);
  // 已合并到的默认词表版本：无配置文件（新用户）时直接视为最新，无需迁移
  let defaultsVersion = DEFAULT_TERMS_VERSION;

  const stats = {
    requests: 0,
    changedRequests: 0,
    totalHits: 0,
    termHits: new Map(),
  };

  function rebuild() {
    pattern = compileTerms(terms);
  }

  /**
   * 默认词表补词迁移（一次性）：把 defaultsVersion 之后各版本登记的新默认词条补进当前词表。
   *
   * 只在 load() 里、读盘之后调用。合并完无论是否真的补了词，都把版本标记推进到最新并落盘，
   * 避免每次启动重复比对、重复写盘。
   * 用户主动删掉的词条不会被补回：这里只处理"比用户已合并版本更新"的登记词条。
   */
  function migrateDefaults() {
    const pending = DEFAULT_TERM_MIGRATIONS
      .filter(item => item.version > defaultsVersion)
      .sort((a, b) => a.version - b.version);
    if (!pending.length) return;

    const known = new Set(terms.map(term => term.toLowerCase()));
    const added = [];
    for (const item of pending) {
      for (const term of item.terms) {
        const key = term.toLowerCase();
        if (known.has(key)) continue;
        known.add(key);
        added.push(term);
      }
    }

    terms = normalizeTerms([...terms, ...added]);
    defaultsVersion = DEFAULT_TERMS_VERSION;
    save();
    log('[Desensitize]', added.length
      ? `默认词表已更新到 v${DEFAULT_TERMS_VERSION}，自动补充词条 ${added.length} 个：${added.join('、')}`
      : `默认词表已是最新（v${DEFAULT_TERMS_VERSION}，无需补充词条）`);
  }

  function load() {
    try {
      if (!existsSync(file)) return;
      const json = JSON.parse(readFileSync(file, 'utf8'));
      if (typeof json.enabled === 'boolean') enabled = json.enabled;
      if (Array.isArray(json.terms)) terms = normalizeTerms(json.terms);
      if (Array.isArray(json.roles)) roles = normalizeRoles(json.roles);
      // 老版本写出的文件没有该字段，视为版本 1（只有初版默认词表）
      defaultsVersion = Number(json.defaultsVersion) >= 1 ? Number(json.defaultsVersion) : 1;
      migrateDefaults();
      rebuild();
      log('[Desensitize]', `已加载词表：${terms.length} 个词，${enabled ? '启用' : '停用'}`);
    } catch (error) {
      log('[Desensitize]', `词表读取失败，回退默认词表: ${error.message}`);
    }
  }

  function save() {
    try {
      mkdirSync(directory, { recursive: true });
      writeFileSync(
        file,
        `${JSON.stringify({ enabled, terms, roles, defaultsVersion }, null, 2)}\n`,
        'utf8',
      );
      return true;
    } catch (error) {
      log('[Desensitize]', `词表保存失败: ${error.message}`);
      return false;
    }
  }

  /**
   * 主入口：处理一次入站请求体。
   * 返回 { body, changed, hits, matched, termCounts }；未命中时 body 为原引用。
   * matched 为命中的词列表（无计数，保持向后兼容），
   * termCounts 为按命中次数降序的 [{ term, count }, ...]，供运行日志展示每词次数。
   */
  function processBody(body) {
    stats.requests += 1;
    if (!enabled || !pattern || !body || !Array.isArray(body.messages)) {
      return { body, changed: false, hits: 0 };
    }
    const counter = { total: 0, terms: new Map() };
    const next = desensitizeBody(body, { pattern, roles, counter });
    if (!counter.total || next === body) return { body, changed: false, hits: 0 };

    stats.changedRequests += 1;
    stats.totalHits += counter.total;
    for (const [term, count] of counter.terms) {
      stats.termHits.set(term, (stats.termHits.get(term) || 0) + count);
    }
    return {
      body: next,
      changed: true,
      hits: counter.total,
      matched: [...counter.terms.keys()],
      termCounts: [...counter.terms.entries()]
        .sort((a, b) => b[1] - a[1])
        .map(([term, count]) => ({ term, count })),
    };
  }

  function statsSnapshot({ top = 20 } = {}) {
    const topTerms = [...stats.termHits.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, top)
      .map(([term, count]) => ({ term, count }));
    return {
      requests: stats.requests,
      changedRequests: stats.changedRequests,
      totalHits: stats.totalHits,
      topTerms,
    };
  }

  function resetStats() {
    stats.requests = 0;
    stats.changedRequests = 0;
    stats.totalHits = 0;
    stats.termHits.clear();
  }

  function getState() {
    return {
      enabled,
      terms: [...terms],
      roles: [...roles],
      termCount: terms.length,
      defaults: {
        enabled: true,
        terms: [...DEFAULT_TERMS],
        roles: [...DEFAULT_ROLES],
        version: DEFAULT_TERMS_VERSION,
      },
      file,
      stats: statsSnapshot(),
    };
  }

  /** 全量替换词表 */
  function setTerms(next) {
    terms = normalizeTerms(next);
    rebuild();
    save();
    return getState();
  }

  /** 追加词（已存在的按忽略大小写跳过） */
  function addTerms(list) {
    return setTerms([...terms, ...(Array.isArray(list) ? list : [list])]);
  }

  /** 删除词（忽略大小写） */
  function removeTerms(list) {
    const drop = new Set((Array.isArray(list) ? list : [list])
      .filter(item => typeof item === 'string')
      .map(item => item.trim().toLowerCase()));
    if (!drop.size) return getState();
    return setTerms(terms.filter(term => !drop.has(term.toLowerCase())));
  }

  function setEnabled(value, { persist = true } = {}) {
    enabled = value !== false;
    if (persist) save();
    return getState();
  }

  function setRoles(next) {
    roles = normalizeRoles(next);
    save();
    return getState();
  }

  /** 恢复默认词表（不动 enabled） */
  function resetTerms() {
    return setTerms([...DEFAULT_TERMS]);
  }

  load();

  return {
    processBody,
    getState,
    setTerms,
    addTerms,
    removeTerms,
    setEnabled,
    setRoles,
    resetTerms,
    resetStats,
    get enabled() { return enabled; },
    get terms() { return [...terms]; },
    get pattern() { return pattern; },
    file,
  };
}
