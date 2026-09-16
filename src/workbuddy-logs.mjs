/**
 * workbuddy-logs — 本地网关运行日志
 *
 * 记录"系统事件"级别的日志（登录、token 刷新、账号增删切换、模型目录刷新、
 * 429 限额自动切换、上游错误、脱敏命中、配置变更），供桌面端「日志」页查阅。
 * 每个请求的明细不在这里（那属于 --verbose 的调试输出，只在控制台）。
 *
 * 存储：{directory}/logs.jsonl，一行一条 JSON，追加写；
 * 内存里保留最近 MAX_ENTRIES 条，超限时把内存快照重写回文件（环形保留）。
 * 启动时载入历史，因此重启后仍能查到之前的记录。
 *
 * 与 workbuddy-desensitize.mjs 同构：纯逻辑集中在 createLogStore，
 * 路由层（workbuddy-log-routes.mjs）只做 HTTP 适配。
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

export const FILE_NAME = 'logs.jsonl';

/** 内存/文件里保留的最大条数（环形保留，超出丢最旧的） */
export const MAX_ENTRIES = 500;

/** 日志级别（数值越大越严重，用于筛选） */
export const LEVELS = Object.freeze(['debug', 'info', 'warn', 'error']);

/** 分类：与桌面端筛选下拉一致 */
export const CATEGORIES = Object.freeze({
  server: '服务',
  auth: '登录',
  account: '账号',
  model: '模型',
  upstream: '上游',
  desensitize: '脱敏',
  config: '配置',
});

const MAX_MESSAGE_LENGTH = 1000;
const MAX_DATA_KEYS = 24;

function normalizeLevel(value) {
  const level = String(value || '').toLowerCase();
  return LEVELS.includes(level) ? level : 'info';
}

function normalizeCategory(value) {
  const category = String(value || '').toLowerCase();
  return Object.prototype.hasOwnProperty.call(CATEGORIES, category) ? category : 'server';
}

/** 附加数据裁剪：只留可序列化的浅层键值，避免把整个请求体塞进日志 */
function normalizeData(data) {
  if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
  const out = {};
  let keys = 0;
  for (const [key, value] of Object.entries(data)) {
    if (keys >= MAX_DATA_KEYS) break;
    if (value === undefined) continue;
    let item = value;
    if (item !== null && typeof item === 'object') {
      try {
        item = JSON.parse(JSON.stringify(item));
      } catch {
        item = String(item);
      }
    }
    if (typeof item === 'string' && item.length > MAX_MESSAGE_LENGTH) {
      item = `${item.slice(0, MAX_MESSAGE_LENGTH)}…`;
    }
    out[key] = item;
    keys++;
  }
  return Object.keys(out).length ? out : null;
}

export function createLogStore({ directory, log = () => {} } = {}) {
  const file = join(directory, FILE_NAME);

  /** @type {Array<{id:number,ts:number,level:string,category:string,message:string,data:object|null}>} */
  let entries = [];
  let nextId = 1;
  let dirty = false;
  // 满额后文件会略微超过上限（追加不立即重写），攒够 COMPACT_STEP 条再收敛一次
  let appendsSinceCompact = 0;
  const COMPACT_STEP = 100;

  function load() {
    try {
      if (!existsSync(file)) return;
      const lines = readFileSync(file, 'utf8').split('\n');
      const parsed = [];
      for (const line of lines) {
        const text = line.trim();
        if (!text) continue;
        try {
          const item = JSON.parse(text);
          if (!item || typeof item !== 'object') continue;
          parsed.push({
            id: Number(item.id) || parsed.length + 1,
            ts: Number(item.ts) || 0,
            level: normalizeLevel(item.level),
            category: normalizeCategory(item.category),
            message: String(item.message || '').slice(0, MAX_MESSAGE_LENGTH),
            data: normalizeData(item.data),
          });
        } catch { /* 跳过损坏行，不影响其余日志 */ }
      }
      entries = parsed.slice(-MAX_ENTRIES);
      nextId = entries.reduce((max, item) => Math.max(max, item.id), 0) + 1;
      if (parsed.length > MAX_ENTRIES) dirty = true; // 文件比内存上限大，下次写入时收敛
      log('[Logs]', `已载入运行日志 ${entries.length} 条`);
    } catch (error) {
      log('[Logs]', `日志读取失败，从空日志开始: ${error.message}`);
    }
  }

  /** 落盘：追加一行；启动时发现超限、或满额后攒够一批，才整文件重写（环形保留） */
  function persist(entry) {
    try {
      mkdirSync(directory, { recursive: true });
      if (dirty || appendsSinceCompact >= COMPACT_STEP) {
        writeFileSync(file, `${entries.map(item => JSON.stringify(item)).join('\n')}\n`, 'utf8');
        dirty = false;
        appendsSinceCompact = 0;
        return;
      }
      writeFileSync(file, `${JSON.stringify(entry)}\n`, { flag: 'a' });
      if (entries.length >= MAX_ENTRIES) appendsSinceCompact++;
    } catch (error) {
      log('[Logs]', `日志写入失败: ${error.message}`);
    }
  }

  /**
   * 追加一条日志。debug 级别由调用方（server.mjs）决定是否上报，
   * 这里只负责存储。
   */
  function append({ level, category, message, data, ts } = {}) {
    const text = String(message ?? '').replace(/\s+/g, ' ').trim();
    if (!text) return null;
    const entry = {
      id: nextId++,
      ts: Number(ts) || Date.now(),
      level: normalizeLevel(level),
      category: normalizeCategory(category),
      message: text.slice(0, MAX_MESSAGE_LENGTH),
      data: normalizeData(data),
    };
    entries.push(entry);
    if (entries.length > MAX_ENTRIES) entries = entries.slice(-MAX_ENTRIES);
    persist(entry);
    return entry;
  }

  /**
   * 查询：按级别（含以上）、分类、关键词、起始 id 过滤，倒序返回最新在前。
   * @returns {{ entries: object[], total: number, matched: number }}
   */
  function query({ limit = 200, level, category, keyword, sinceId } = {}) {
    let list = entries.slice();
    if (level && LEVELS.includes(String(level).toLowerCase())) {
      const min = LEVELS.indexOf(String(level).toLowerCase());
      list = list.filter(item => LEVELS.indexOf(item.level) >= min);
    }
    if (category && Object.prototype.hasOwnProperty.call(CATEGORIES, String(category).toLowerCase())) {
      list = list.filter(item => item.category === String(category).toLowerCase());
    }
    const kw = String(keyword || '').trim().toLowerCase();
    if (kw) list = list.filter(item => item.message.toLowerCase().includes(kw));
    if (Number.isFinite(Number(sinceId))) {
      const from = Number(sinceId);
      list = list.filter(item => item.id > from);
    }
    const matched = list.length;
    const size = Math.min(Math.max(Number(limit) || 200, 1), MAX_ENTRIES);
    return { entries: list.slice(-size).reverse(), total: entries.length, matched };
  }

  /** 各级别 / 分类计数（桌面端导航徽标与筛选下拉用） */
  function stats() {
    const byLevel = Object.fromEntries(LEVELS.map(level => [level, 0]));
    const byCategory = Object.fromEntries(Object.keys(CATEGORIES).map(key => [key, 0]));
    for (const item of entries) {
      byLevel[item.level] = (byLevel[item.level] || 0) + 1;
      byCategory[item.category] = (byCategory[item.category] || 0) + 1;
    }
    return {
      total: entries.length,
      max: MAX_ENTRIES,
      byLevel,
      byCategory,
      lastId: entries.length ? entries[entries.length - 1].id : 0,
      file,
    };
  }

  function clear() {
    entries = [];
    nextId = 1;
    try {
      mkdirSync(directory, { recursive: true });
      writeFileSync(file, '', 'utf8');
    } catch (error) {
      log('[Logs]', `日志清空失败: ${error.message}`);
    }
    return stats();
  }

  /** 导出为 JSONL 文本（桌面端「导出」按钮用） */
  function toJsonl() {
    return entries.length ? `${entries.map(item => JSON.stringify(item)).join('\n')}\n` : '';
  }

  load();

  return {
    append,
    query,
    stats,
    clear,
    toJsonl,
    get size() { return entries.length; },
    file,
  };
}
