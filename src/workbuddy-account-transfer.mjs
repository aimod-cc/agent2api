/**
 * WorkBuddy 账号导入 / 导出（从 workbuddy-account-store.mjs 拆出，避免单文件超长）。
 *
 * 这里是纯逻辑模块：不碰磁盘、不持状态，读盘/落盘/日志/号段分配全部由 store 注入，
 * 所以它既不依赖 store，也不会与 store 形成循环依赖。
 *
 * 导出形态刻意保留 accessToken / refreshToken：换机器后靠它们直接沿用登录态，
 * 不必再走一遍登录流程。反之 rateLimits 是本机的运行时限额标记、下划线开头的是
 * 内部字段，换机器都没有意义，一律不导出。
 *
 * 导入目前只有 merge 一种模式：按 uid 匹配本机账号 —— 命中则更新凭证与运营字段，
 * 未命中则追加为队尾新账号。**priority 与 addedAt 一律取本机值**：本机的转发顺序
 * 由本机决定，回灌一份导出文件不该把别人的顺序搬过来。
 */

import { resolveEdition } from './workbuddy-endpoints.mjs';
import { normalizeAccountProxy } from './workbuddy-proxy.mjs';

/** 导出文件格式版本（导入端据此兼容后续格式变化） */
export const EXPORT_VERSION = 1;

/**
 * 单条账号记录 → 导出形态：原样输出全部业务字段，剔除 rateLimits（本机运行时限额
 * 标记）与下划线内部字段。
 */
function toExportRecord(record) {
  const item = {};
  for (const [key, value] of Object.entries(record)) {
    if (key.startsWith('_') || key === 'rateLimits') continue;
    item[key] = value === undefined ? null : value;
  }
  // 统一形状：token 与代理即使缺失也显式给出，方便导入端与用户核对
  item.accessToken = record.accessToken || '';
  item.refreshToken = record.refreshToken || '';
  item.proxy = record.proxy ?? null;
  return item;
}

/**
 * 导入记录的字段归一（merge 的「更新」与「新增」分支共用）。
 * 只认导入文件里出现过的键：出现才覆盖，未出现或为空则沿用 before（新增分支传 {}）——
 * 导出文件回流即原样还原，只写了 token 的残缺文件也不会把备注名、代理洗掉。
 * priority / addedAt 刻意不在其中：本机转发顺序由本机决定，导入值一律忽略。
 * 取值阶段的错误在此抛出，保证调用方不会把记录改到一半。
 */
function normalizeImported(item, before = {}) {
  const has = key => Object.prototype.hasOwnProperty.call(item, key);
  const text = (key, fallback) => {
    if (!has(key) || item[key] === null || item[key] === undefined) return fallback;
    return String(item[key]).trim();
  };
  const num = (key, fallback) => {
    const value = Number(item[key]);
    return has(key) && value > 0 ? value : fallback;
  };
  const edition = resolveEdition(item.edition ?? before.edition);
  // token 与备注名以「非空」为准：导入值为空时保留本机旧值，避免账号被洗成空壳
  const accessToken = text('accessToken', '') || before.accessToken || '';
  return {
    accessToken,
    refreshToken: text('refreshToken', '') || before.refreshToken || '',
    // tokenTail 导入记录没有就按最终 accessToken 末 4 位重算
    tokenTail: text('tokenTail', '') || accessToken.slice(-4),
    edition: edition.id,
    prefixPath: item.prefixPath ?? before.prefixPath ?? edition.prefixPath,
    endpoint: item.endpoint ?? before.endpoint ?? edition.endpoint,
    platform: item.platform ?? before.platform ?? edition.platform,
    name: text('name', before.name || '').slice(0, 100) || before.name || '',
    nickname: text('nickname', before.nickname || ''),
    type: text('type', before.type || '') || 'personal',
    enterpriseId: text('enterpriseId', before.enterpriseId || ''),
    enterpriseName: text('enterpriseName', before.enterpriseName || ''),
    domain: text('domain', before.domain || ''),
    expiresAt: num('expiresAt', before.expiresAt ?? null),
    refreshExpiresAt: num('refreshExpiresAt', before.refreshExpiresAt ?? null),
    enabled: has('enabled') ? item.enabled !== false : before.enabled !== false,
    proxy: has('proxy') ? normalizeAccountProxy(item.proxy) : (before.proxy ?? null),
  };
}

/**
 * 构造导入 / 导出能力，注入 store 的读写与领域约束。
 *
 *   load / save        账号文件的读写（store 持有文件路径）
 *   AccountStoreError  错误类型由 store 注入，路由侧 instanceof 判断才不会失真
 *   nextFreePriority   号段分配与 addAccount 共用，留在 store（避免两处规则漂移）
 *   maxAccounts        账号数上限；maxTokenLength token 长度上限
 */
export function createAccountTransfer({
  load,
  save,
  log = () => {},
  AccountStoreError,
  nextFreePriority,
  maxAccounts,
  maxTokenLength,
}) {
  /** 导出全部账号（换机器后导入继续用） */
  function exportAccounts() {
    const state = load();
    return {
      version: EXPORT_VERSION,
      exportedAt: Date.now(),
      accounts: state.accounts.map(toExportRecord),
    };
  }

  /**
   * 导入账号（导出文件的回流），merge 语义见文件头。
   * 单条坏数据（缺 id / 缺 uid / 两个 token 都没有）只记一条 failed 并继续，
   * 不让一条脏数据毁掉整批导入。
   * 返回 { total, added, updated, skipped, failed, errors }。
   */
  function importAccounts(payload) {
    if (!payload || typeof payload !== 'object' || Array.isArray(payload)) {
      throw new AccountStoreError('导入内容必须是 JSON 对象');
    }
    if (!Array.isArray(payload.accounts)) throw new AccountStoreError('缺少 accounts 数组');
    if (!payload.accounts.length) throw new AccountStoreError('accounts 为空，没有可导入的账号');

    const state = load();
    // 按 uid 匹配本机账号；导入中新增的账号也进表，这样同批多条相同 uid
    // 会变成「第一条新增、后续更新同一条」
    const byUid = new Map();
    for (const item of state.accounts) if (item.uid) byUid.set(String(item.uid), item);
    const takenIds = new Set(state.accounts.map(item => item.id));

    const errors = [];
    let added = 0;
    let updated = 0;
    for (const item of payload.accounts) {
      const valid = Boolean(item) && typeof item === 'object' && !Array.isArray(item);
      const itemId = valid && typeof item.id === 'string' ? item.id.trim() : '';
      const uid = valid && typeof item.uid === 'string' ? item.uid.trim() : '';
      try {
        if (!valid) throw new AccountStoreError('账号记录必须是 JSON 对象');
        if (!itemId) throw new AccountStoreError('缺少 id');
        if (!uid) throw new AccountStoreError('缺少 uid（无法标识账号）');
        const accessToken = typeof item.accessToken === 'string' ? item.accessToken.trim() : '';
        const refreshToken = typeof item.refreshToken === 'string' ? item.refreshToken.trim() : '';
        if (!accessToken && !refreshToken) {
          throw new AccountStoreError('缺少 accessToken 与 refreshToken');
        }
        if (accessToken.length > maxTokenLength || refreshToken.length > maxTokenLength) {
          throw new AccountStoreError('token 过长');
        }

        const existing = byUid.get(uid);
        if (existing) {
          // 本机的 priority / addedAt / id 一律保留，只回写凭证与运营字段
          Object.assign(existing, normalizeImported(item, existing), { updatedAt: Date.now() });
          updated++;
          continue;
        }
        if (state.accounts.length >= maxAccounts) {
          throw new AccountStoreError(`最多保存 ${maxAccounts} 个账号`);
        }
        const now = Date.now();
        // id 沿用导入值，撞了别人的 id 就按 addAccount 的约定回退成 user-<uid>；
        // priority 一律取队尾（忽略导入文件里的值），不打乱本机转发顺序
        const record = {
          ...normalizeImported(item),
          id: itemId && !takenIds.has(itemId) ? itemId : `user-${uid}`,
          uid,
          priority: nextFreePriority(state.accounts),
          addedAt: now,
          updatedAt: now,
        };
        if (!record.name) record.name = record.nickname || `账号 ${uid.slice(0, 8)}`;
        state.accounts.push(record);
        byUid.set(uid, record);
        takenIds.add(record.id);
        added++;
      } catch (error) {
        errors.push({ id: itemId || uid, message: error.message });
      }
    }

    save(state);
    log('[Accounts]',
      `📥 账号导入完成: 共 ${payload.accounts.length} 条，新增 ${added} 个，更新 ${updated} 个`
      + `${errors.length ? `，跳过 ${errors.length} 条（数据不完整）` : ''}`);
    return { total: payload.accounts.length, added, updated, skipped: 0, failed: errors.length, errors };
  }

  return { exportAccounts, importAccounts };
}
