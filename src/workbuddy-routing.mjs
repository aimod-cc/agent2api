/**
 * WorkBuddy 账号选路 —— 严格优先级排队
 *
 * 优先级是「主备序号」：优先级必须全局唯一（由 account-store 在写入侧保证），
 * 不允许多个账号并列 —— 并列会让「同级」语义失效（退化成永远只取加入最早的
 * 那个，其余账号形同虚设），这一点与 OmniProxy 的「同级按权重轮询」不同。
 *
 * 规则：
 *   1. 候选 = 启用中（enabled !== false）且未对「该模型」处于限额冷却期的账号；
 *   2. 取候选里优先级数值最小的那个（优先级即主备顺序，数值小的先用）；
 *   3. 本次已尝试过的账号（429 降级）从候选中排除，避免回环；
 *   4. 全部候选都不可用时，由调用方决定是降级重试还是透传错误。
 *
 * 同级兜底：若磁盘数据被手工编辑出并列（正常路径被写入校验挡住），
 * 这里按加入时间取最早的，保证选路结果稳定可预期。
 *
 * 「当前账号」不是独立的手动选择，而是本模块选路结果在「不限模型」下的那个账号
 * （account-store 的 getCurrentEntry 按同样的判据派生）。因此界面上的「当前」
 * 与转发默认使用谁始终一致；仅当账号对某具体模型限额时，该模型的请求才会
 * 临时降级到下一个候选 —— 那是按模型的一次性决策，不改写「当前账号」。
 */

import { byPriorityOrder, normalizePriority } from './workbuddy-account-store.mjs';

/** 账号对某模型是否处于限额冷却期 */
export function isRateLimited(account, model, now = Date.now()) {
  const key = String(model || '');
  const limit = account?.rateLimits?.[key];
  if (!limit) return false;
  return Number(limit.resetAt) > now;
}

/** 限额恢复时间（未限额返回 0） */
export function rateLimitResetAt(account, model, now = Date.now()) {
  const key = String(model || '');
  const limit = account?.rateLimits?.[key];
  if (!limit) return 0;
  const resetAt = Number(limit.resetAt) || 0;
  return resetAt > now ? resetAt : 0;
}

/**
 * 账号是否可用于转发：启用 + 未被该模型限额。
 * 返回 { usable, reason }，reason ∈ 'disabled' | 'rate-limited' | null。
 */
export function accountUsability(account, model, now = Date.now()) {
  if (account?.enabled === false) return { usable: false, reason: 'disabled' };
  if (isRateLimited(account, model, now)) return { usable: false, reason: 'rate-limited' };
  return { usable: true, reason: null };
}

/**
 * 按优先级挑选本次请求使用的账号。
 * accounts 为 store.listAccounts().accounts 的公开形态（含 priority/enabled/rateLimits）。
 * excludeIds 是本次请求已尝试过的账号 id（429 降级用）。
 * 返回账号对象，或 null（没有可用账号）。
 */
export function pickAccountByPriority(accounts, { model = '', excludeIds = [], now = Date.now() } = {}) {
  const excluded = new Set(excludeIds);
  const candidates = (Array.isArray(accounts) ? accounts : []).filter(account => {
    if (!account?.id || excluded.has(account.id)) return false;
    return accountUsability(account, model, now).usable;
  });
  if (!candidates.length) return null;
  // 优先级唯一（写入侧保证），排序后取首位即为唯一答案；
  // 并列属手工编辑出来的异常数据，按加入时间兜底，结果依旧稳定
  candidates.sort(byPriorityOrder);
  return candidates[0];
}

/**
 * 选路决策的完整快照（供日志展示与排障）：
 *   { picked, priority, group, blocked: [{ id, name, reason }], total }
 */
export function describeRouteDecision(accounts, { model = '', excludeIds = [], now = Date.now() } = {}) {
  const list = Array.isArray(accounts) ? accounts : [];
  const excluded = new Set(excludeIds);
  const blocked = [];
  const candidates = [];
  for (const account of list) {
    if (!account?.id) continue;
    if (excluded.has(account.id)) {
      blocked.push({ id: account.id, name: account.name || account.id, reason: 'tried' });
      continue;
    }
    const { usable, reason } = accountUsability(account, model, now);
    if (usable) candidates.push(account);
    else blocked.push({ id: account.id, name: account.name || account.id, reason });
  }
  const picked = pickAccountByPriority(list, { model, excludeIds, now });
  return {
    picked,
    priority: picked ? normalizePriority(picked.priority) : null,
    candidateCount: candidates.length,
    blocked,
    total: list.length,
  };
}
