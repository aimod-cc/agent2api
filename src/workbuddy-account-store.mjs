/**
 * WorkBuddy 多账号存储（对齐 raccoon-local-proxy 的 account-store 模式）。
 *
 * 持久化文件 ~/.workbuddy-proxy/accounts.json：
 *   { accounts: [{ id, name, uid, nickname, type,
 *     enterpriseId, accessToken, refreshToken, expiresAt, refreshExpiresAt,
 *     domain, tokenTail, prefixPath, endpoint, platform, edition, addedAt, updatedAt,
 *     priority, enabled, proxy, rateLimits: { [modelId]: { status, code, resetAt, message, at } } }] }
 *
 * edition 标识账号所属版本：cn=国内版（copilot.tencent.com）、intl=国际版（www.workbuddy.ai）。
 * 端点/prefixPath/platform 在入账时按 edition 落定，token 刷新与转发都按账号记录走。
 *
 * priority 决定转发时的账号选择顺序（数值小的先用，主备式），全局唯一。
 * 「当前账号」不是独立存储的手动选择，而是**由优先级派生**：
 * 即转发顺序里第一个「已启用且有凭证」的账号 —— 与转发默认使用的账号一致，
 * 所以不存在「手动选了 A、实际用 B」这种分叉（按模型的限额降级除外，见下）。
 * 想换当前账号就把目标账号置顶（promoteToFront，把它移到队首）。
 *
 * 注意区分两种「不一致」：
 *   1. 手动选择 vs 转发 —— 已消除（当前账号就是转发首选）；
 *   2. 当前账号 vs 某次实际请求 —— 该账号对某模型限额时，那个模型的请求
 *      会临时降级到下一个账号。当前账号是模型无关的，不会跟着变。
 * 详见 workbuddy-routing.mjs。
 *
 * enabled=false 的账号不参与转发与批量操作（但仍可手动查询/刷新/签到）。
 * proxy 为账号级出网代理（null=直连，形态见 workbuddy-proxy.mjs）。
 *
 * rateLimits 记录账号×模型的限额状态（上游 429/code 6004，msg 里带 UTC+8 重置时间），
 * 供转发层自动切换账号、桌面端展示状态码与恢复时间。
 *
 * 账号来源：
 *   1. 无头登录（loginInteractive 完成 → addAccount）
 *   2. 手动上传凭证（POST /api/accounts，accessToken/refreshToken JSON）
 *   3. 旧版单账号 auth.json 迁移（importLegacySession，仅首次启动导入）
 *
 * 新增账号取当前最大优先级 +1（排在队尾），因此不会抢占当前账号。
 *
 * 「当前账号」不落盘：它由上面那条派生规则实时算出，所以升级前遗留的
 * currentAccountId 字段会被忽略（读盘时不再解析），下次保存即自然消失。
 *
 * 所有读取实时走磁盘，手动编辑或外部修改下次请求即生效。
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { DEFAULT_EDITION, resolveEdition } from './workbuddy-endpoints.mjs';
import { describeAccountProxy, normalizeAccountProxy, resolveAccountProxy } from './workbuddy-proxy.mjs';
import { createAccountTransfer } from './workbuddy-account-transfer.mjs';
// 导入 / 导出实现已移到该模块（单文件行数约定），这里原样再导出，对外契约不变
export { EXPORT_VERSION } from './workbuddy-account-transfer.mjs';

const MAX_ACCOUNTS = 20;
const MAX_TOKEN_LENGTH = 8192;

/** 默认优先级：首个账号从这里开始编号，后续账号依次追加 */
export const DEFAULT_PRIORITY = 100;
const MIN_PRIORITY = 0;
const MAX_PRIORITY = 9999;

export class AccountStoreError extends Error {
  constructor(message, statusCode = 400) {
    super(message);
    this.name = 'AccountStoreError';
    this.statusCode = statusCode;
  }
}

function pickToken(payload, ...keys) {
  for (const key of keys) {
    if (typeof payload[key] === 'string' && payload[key].trim()) {
      return payload[key].trim().replace(/^Bearer\s+/i, '');
    }
  }
  return '';
}

/** 优先级归一：非法值回落到默认值，并夹在允许区间内 */
export function normalizePriority(value, fallback = DEFAULT_PRIORITY) {
  if (value === null || value === undefined || value === '') return fallback;
  const num = Number(value);
  if (!Number.isFinite(num)) return fallback;
  return Math.min(MAX_PRIORITY, Math.max(MIN_PRIORITY, Math.round(num)));
}

/**
 * 选路顺序：优先级升序，同优先级按加入时间（与 pickAccountByPriority 一致）。
 * 唯一性由写入侧保证，这里的次级排序键只是手工编辑文件时的兜底。
 */
export function byPriorityOrder(a, b) {
  return normalizePriority(a.priority) - normalizePriority(b.priority)
    || (Number(a.addedAt) || 0) - (Number(b.addedAt) || 0);
}

/**
 * 优先级必须全局唯一 —— 与 OmniProxy 不同，本项目的「优先级」是严格主备序号，
 * 允许并列会让「同级」语义失效（退化成永远只取加入最早的那个，其余账号形同虚设）。
 * 因此：
 *   - 写入侧（新增/修改）遇到冲突一律拒绝（409），由用户显式选一个空闲值；
 *   - 启动时对既有数据做一次性去重迁移（normalizePriorities），保住原有选路顺序；
 *   - 校验范围含已禁用账号 —— 否则禁用期间让出号段，重新启用就会撞车。
 */
function findPriorityHolder(accounts, priority, excludeId = null) {
  return accounts.find(item => item.id !== excludeId
    && normalizePriority(item.priority) === priority) || null;
}

/** 下一个可用优先级：排在现有账号之后（新增账号不抢占已有转发顺序） */
function nextFreePriority(accounts) {
  const used = new Set(accounts.map(item => normalizePriority(item.priority)));
  const max = accounts.reduce((acc, item) => Math.max(acc, normalizePriority(item.priority)), DEFAULT_PRIORITY - 1);
  let candidate = max + 1;
  while (used.has(candidate) && candidate <= MAX_PRIORITY) candidate++;
  if (candidate <= MAX_PRIORITY) return candidate;
  // 极端情况：号段用满，从默认值开始找第一个空位
  for (let p = MIN_PRIORITY; p <= MAX_PRIORITY; p++) if (!used.has(p)) return p;
  return DEFAULT_PRIORITY;
}

/**
 * 把已排好序的账号重新连续编号，保持这个顺序不变。
 *
 * 起点优先沿用原有最小优先级（这样只是整体平移，相对顺序与「第 N 位」都不变）；
 * 若从那里起排不下（号段顶到 MAX_PRIORITY），就整体下移到刚好放得下的位置 ——
 * 绝不能简单地对每个值取 Math.min(MAX_PRIORITY, base + i)，那样会把
 * 靠后的账号压成同一个数值，直接破坏「优先级全局唯一」这条不变量。
 *
 * 返回 [{ id, name, from, to }] 变更清单（未变化的项不列入）。
 */
function renumberConsecutively(ordered) {
  if (!ordered.length) return [];
  const values = ordered.map(item => normalizePriority(item.priority));
  const base = Math.min(...values);
  const span = values.length - 1;
  const start = base + span <= MAX_PRIORITY
    ? base
    : Math.max(MIN_PRIORITY, MAX_PRIORITY - span);

  const assignments = [];
  ordered.forEach((record, index) => {
    const next = Math.min(MAX_PRIORITY, Math.max(MIN_PRIORITY, start + index));
    const from = normalizePriority(record.priority);
    if (from !== next) {
      record.priority = next;
      assignments.push({ id: record.id, name: record.name, from, to: next });
    }
  });
  return assignments;
}

/**
 * 一次性去重迁移：把并列的优先级重新编号成连续序号，保持原有选路顺序。
 * 同名次内部按加入时间排（与历史行为一致，所以升级后实际转发顺序不变）。
 * 只在检测到重复时写盘，返回 { changed, assignments }。
 */
function normalizePriorities(state) {
  const priorityOf = item => normalizePriority(item.priority);
  const values = state.accounts.map(priorityOf);
  if (new Set(values).size === values.length) return { changed: false, assignments: [] };

  const ordered = state.accounts.slice().sort(byPriorityOrder);
  const assignments = renumberConsecutively(ordered);
  state.accounts = ordered;
  return { changed: true, assignments };
}

export function createAccountStore({
  directory = join(homedir(), '.workbuddy-proxy'),
  log = () => {},
} = {}) {
  const filePath = join(directory, 'accounts.json');

  function load() {
    try {
      const json = JSON.parse(readFileSync(filePath, 'utf8'));
      if (!json || typeof json !== 'object' || Array.isArray(json)) throw new Error('bad root');
      return {
        accounts: Array.isArray(json.accounts)
          ? json.accounts.filter(item => item && typeof item === 'object' && typeof item.id === 'string')
          : [],
      };
    } catch {
      return { accounts: [] };
    }
  }

  function save(state) {
    try {
      mkdirSync(directory, { recursive: true });
      writeFileSync(filePath, JSON.stringify(state, null, 2), 'utf8');
    } catch (error) {
      throw new AccountStoreError(`账号文件保存失败: ${error.message}`, 500);
    }
  }

  // 导入 / 导出在独立模块里，这里注入读写与号段规则（避免两处各写一份分配规则）；
  // 该模块只接收这些依赖、不反向 import 本模块，所以不会形成循环依赖
  const { exportAccounts, importAccounts } = createAccountTransfer({
    load, save, log, AccountStoreError,
    nextFreePriority, maxAccounts: MAX_ACCOUNTS, maxTokenLength: MAX_TOKEN_LENGTH,
  });

  function toPublicAccount(record) {
    const edition = resolveEdition(record.edition);
    return {
      id: record.id,
      name: record.name,
      uid: record.uid || '',
      nickname: record.nickname || '',
      type: record.type || 'personal',
      enterpriseId: record.enterpriseId || '',
      enterpriseName: record.enterpriseName || '',
      tokenTail: record.tokenTail || '',
      expiresAt: record.expiresAt || null,
      hasRefreshToken: Boolean(record.refreshToken),
      prefixPath: record.prefixPath ?? edition.prefixPath,
      endpoint: record.endpoint || edition.endpoint,
      edition: edition.id,
      editionLabel: edition.label,
      priority: normalizePriority(record.priority),
      enabled: record.enabled !== false,
      addedAt: record.addedAt || 0,
      updatedAt: record.updatedAt || 0,
      proxy: describeAccountProxy(record.proxy || null),
      rateLimits: record.rateLimits || {},
      available: true,
    };
  }

  /**
   * 添加/更新账号。接受 session 形态（auth + account）或裸凭证形态
   * （accessToken/refreshToken，可含 uid/nickname/expiresAt/domain）。
   * edition 决定端点/prefixPath/platform 默认值（cn 国内版 / intl 国际版），
   * 显式传入的 endpoint/prefixPath/platform 优先。
   * priority/enabled/proxy 仅在显式传入时改写，其余沿用已有值（新账号用默认值）。
   * 新账号的优先级取「现有最大值 + 1」即排在队尾，所以新增账号不会抢占
   * 当前账号（当前账号 = 队首）。返回公开形态。
   */
  function addAccount(payload, name) {
    if (!payload || typeof payload !== 'object' || Array.isArray(payload)) {
      throw new AccountStoreError('账号内容必须是 JSON 对象');
    }
    const auth = payload.auth && typeof payload.auth === 'object' ? payload.auth : payload;
    const account = payload.account && typeof payload.account === 'object' ? payload.account : {};
    const accessToken = pickToken(auth, 'accessToken', 'token', 'access_token');
    const refreshToken = pickToken(auth, 'refreshToken', 'refresh_token');
    if (!accessToken) throw new AccountStoreError('缺少 accessToken');
    if (accessToken.length > MAX_TOKEN_LENGTH || refreshToken.length > MAX_TOKEN_LENGTH) {
      throw new AccountStoreError('token 过长');
    }
    const uid = String(account.uid || payload.uid || '').trim();
    if (!uid) throw new AccountStoreError('缺少 uid（无法标识账号）');

    const state = load();
    const id = `user-${uid}`;
    const existing = state.accounts.find(item => item.id === id);
    const others = state.accounts.filter(item => item.id !== id);
    if (!existing && others.length >= MAX_ACCOUNTS) {
      throw new AccountStoreError(`最多保存 ${MAX_ACCOUNTS} 个账号`);
    }
    // 代理/优先级校验放在写盘前：非法输入直接报错，不留下半成品记录
    const resolvedProxy = payload.proxy !== undefined
      ? normalizeAccountProxy(payload.proxy)
      : (existing?.proxy ?? null);
    const edition = resolveEdition(payload.edition ?? existing?.edition ?? DEFAULT_EDITION);
    // 新账号默认排在末尾，避免凭空插队改变现有转发顺序；显式指定则校验唯一
    const priority = payload.priority === undefined
      ? (existing ? normalizePriority(existing.priority) : nextFreePriority(others))
      : normalizePriority(payload.priority, normalizePriority(existing?.priority));
    const holder = findPriorityHolder(others, priority);
    if (holder) {
      throw new AccountStoreError(
        `优先级 ${priority} 已被账号「${holder.name}」占用，请换一个（优先级需全局唯一）`,
        409,
      );
    }
    const record = {
      id,
      name: (typeof name === 'string' && name.trim() ? name.trim() : '').slice(0, 100)
        || account.nickname || payload.nickname || existing?.name
        || `账号 ${uid.slice(0, 8)}`,
      uid,
      nickname: account.nickname || payload.nickname || existing?.nickname || '',
      type: account.type || payload.type || existing?.type || 'personal',
      enterpriseId: account.enterpriseId || payload.enterpriseId || existing?.enterpriseId || '',
      enterpriseName: account.enterpriseName || payload.enterpriseName || existing?.enterpriseName || '',
      accessToken,
      refreshToken: refreshToken || existing?.refreshToken || '',
      tokenTail: accessToken.slice(-4),
      expiresAt: Number(auth.expiresAt ?? payload.expiresAt) || existing?.expiresAt || null,
      refreshExpiresAt: Number(auth.refreshExpiresAt ?? payload.refreshExpiresAt) || existing?.refreshExpiresAt || null,
      domain: auth.domain || existing?.domain || '',
      edition: edition.id,
      prefixPath: payload.prefixPath ?? existing?.prefixPath ?? edition.prefixPath,
      endpoint: payload.endpoint ?? existing?.endpoint ?? edition.endpoint,
      platform: payload.platform ?? existing?.platform ?? edition.platform,
      priority,
      enabled: payload.enabled !== undefined ? payload.enabled !== false : (existing?.enabled !== false),
      proxy: resolvedProxy,
      addedAt: existing?.addedAt || Date.now(),
      updatedAt: Date.now(),
    };
    state.accounts = [...others, record];
    // 不需要维护「当前账号」：它由优先级派生，而新账号取的是最大优先级 +1，
    // 天然排在队尾，因此添加账号不会抢占正在使用的账号。
    save(state);
    log('[Accounts]', `✅ 账号已保存: ${record.name}（${uid.slice(0, 8)}…，${edition.label}，优先级 ${record.priority}）`);
    return toPublicAccount(record);
  }

  function removeAccount(id) {
    const state = load();
    const before = state.accounts.length;
    state.accounts = state.accounts.filter(item => item.id !== id);
    if (state.accounts.length === before) throw new AccountStoreError('账号不存在', 404);
    save(state);
  }

  /**
   * 置顶账号：把它变成转发顺序第一位，也就是「当前账号」。
   *
   * 这是「设为当前」的实际动作 —— 优先级唯一的前提下，与其让用户手工去猜
   * 一个比所有人都小的数，不如直接把目标移到队首再整队连续编号，
   * 其余账号相对顺序保持不变（只是整体让位）。
   *
   * 目标账号若处于禁用状态会一并启用：这个动作的语义是「现在开始用它」，
   * 一个禁用的账号不可能成为当前账号，只置顶不启用只会让人以为没生效。
   */
  function promoteToFront(id) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record) throw new AccountStoreError('账号不存在', 404);

    const changes = [];
    if (record.enabled === false) {
      record.enabled = true;
      changes.push('已启用');
    }

    const ordered = state.accounts.slice().sort(byPriorityOrder);
    const index = ordered.findIndex(item => item.id === id);
    if (index > 0) {
      ordered.splice(index, 1);
      ordered.unshift(record);
      // 整队重新连续编号（而非只改目标账号）：优先级唯一的前提下，
      // 把目标插到队首后必须把其余账号整体让位，否则会撞号
      renumberConsecutively(ordered);
      changes.push(`优先级 → ${normalizePriority(record.priority)}（置顶）`);
    }

    if (!changes.length) {
      return { id, changed: false, reason: '已是当前账号', currentAccountId: deriveCurrentId(state) };
    }
    record.updatedAt = Date.now();
    state.accounts = ordered;
    save(state);
    const currentAccountId = deriveCurrentId(state);
    log('[Accounts]', `⬆️  账号已置顶: ${record.name}（${changes.join('，')}）`);
    return { id, changed: true, changes, currentAccountId, list: listAccounts() };
  }

  /**
   * 把 patch 应用到单条记录（纯内存操作：不落盘、不动 state）。
   * 返回变更描述列表；非法取值按错误抛出。
   * updateAccount 与 batchUpdate 共用这里，保证单账号与批量的语义完全一致。
   */
  function applyAccountPatch(record, patch, state) {
    const changes = [];
    if (patch.name !== undefined) {
      const next = typeof patch.name === 'string' ? patch.name.trim().slice(0, 100) : '';
      if (next !== record.name) {
        if (!next) throw new AccountStoreError('备注名不能为空');
        record.name = next;
        changes.push(`备注名 → ${next}`);
      }
    }
    if (patch.priority !== undefined) {
      const next = normalizePriority(patch.priority, normalizePriority(record.priority));
      if (next !== normalizePriority(record.priority)) {
        const holder = findPriorityHolder(state.accounts, next, record.id);
        if (holder) {
          throw new AccountStoreError(
            `优先级 ${next} 已被账号「${holder.name}」占用，请换一个（优先级需全局唯一）`,
            409,
          );
        }
        record.priority = next;
        changes.push(`优先级 → ${next}`);
      }
    }
    if (patch.enabled !== undefined) {
      const next = patch.enabled !== false;
      if (next !== (record.enabled !== false)) {
        record.enabled = next;
        changes.push(next ? '已启用' : '已禁用');
      }
    }
    if (patch.proxy !== undefined) {
      const next = normalizeAccountProxy(patch.proxy);
      const before = JSON.stringify(record.proxy ?? null);
      if (JSON.stringify(next) !== before) {
        record.proxy = next;
        changes.push(next ? `代理 → ${describeAccountProxy(next)?.label || '已设置'}` : '代理 → 无代理（直连）');
      }
    }
    return changes;
  }

  /**
   * 修改账号的运营属性：备注名 / 优先级 / 启用状态 / 代理。
   * 只处理显式传入的字段（patch 语义），未传字段保持不变。
   * 返回 { account, changes }，changes 为字段变化列表（供日志与前端提示）。
   */
  function updateAccount(id, patch = {}) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record) throw new AccountStoreError('账号不存在', 404);

    const changes = applyAccountPatch(record, patch, state);
    if (!changes.length) return { account: toPublicAccount(record), changes };
    record.updatedAt = Date.now();
    save(state);
    log('[Accounts]', `✏️  账号已更新: ${record.name}（${changes.join('，')}）`);
    return { account: toPublicAccount(record), changes };
  }

  function listAccounts() {
    const state = load();
    return {
      currentAccountId: deriveCurrentId(state),
      accounts: state.accounts.map(toPublicAccount),
    };
  }

  /**
   * 把账号沿优先级顺序上移/下移一位（与相邻账号交换优先级数值）。
   * 优先级唯一的前提下，交换能让用户点两下就完成重排序，
   * 不用手工去猜一个空闲数字 —— 这是唯一性约束下的主要调整入口。
   * direction: 'up' = 更靠前（优先级变小）；'down' = 更靠后。
   * 已在队首/队尾时返回 { moved: false }，不算错误。
   */
  function moveAccount(id, direction = 'up') {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record) throw new AccountStoreError('账号不存在', 404);

    const ordered = state.accounts.slice().sort(byPriorityOrder);
    const index = ordered.findIndex(item => item.id === id);
    const targetIndex = direction === 'down' ? index + 1 : index - 1;
    if (targetIndex < 0 || targetIndex >= ordered.length) {
      return { moved: false, reason: direction === 'down' ? '已是最后一位' : '已是第一位' };
    }

    const other = ordered[targetIndex];
    const mine = normalizePriority(record.priority);
    const theirs = normalizePriority(other.priority);
    record.priority = theirs;
    other.priority = mine;
    record.updatedAt = Date.now();
    other.updatedAt = Date.now();
    save(state);
    log('[Accounts]', `↕️  优先级交换: ${record.name}(P${mine}) ⇄ ${other.name}(P${theirs})`);
    return {
      moved: true,
      id: record.id,
      newPriority: record.priority,
      swappedWith: { id: other.id, name: other.name, newPriority: other.priority },
      list: listAccounts(),
    };
  }

  /**
   * 批量修改账号（目前支持启用状态与代理）。
   * 语义上是「对每个选中账号各跑一次 updateAccount」，但整批只落盘一次，
   * 避免 N 个账号触发 N 次全量写文件。
   *
   * 单个账号失败不中断整批：结果里分别记 ok / failed（含原因），
   * 前端据此提示「成功 N 个、失败 M 个」。合法的改动会照常生效。
   * 返回 { ok: [...], failed: [...], list }。
   */
  function batchUpdate(ids, patch = {}) {
    if (!Array.isArray(ids) || !ids.length) throw new AccountStoreError('缺少要操作的账号 id');
    if (!patch || typeof patch !== 'object' || Array.isArray(patch)) {
      throw new AccountStoreError('批量修改内容必须是 JSON 对象');
    }
    // 批量场景只开放「启用状态」与「代理」：优先级必须唯一，逐账号指定才有意义，
    // 备注名逐个改也说不通，都留给单账号设置面板
    const allowed = {};
    if (patch.enabled !== undefined) allowed.enabled = patch.enabled !== false;
    if (patch.proxy !== undefined) allowed.proxy = patch.proxy;
    if (!Object.keys(allowed).length) {
      throw new AccountStoreError('批量修改目前只支持启用状态与代理');
    }
    if (Object.prototype.hasOwnProperty.call(allowed, 'proxy')) {
      allowed.proxy = normalizeAccountProxy(allowed.proxy);
    }

    const state = load();
    const ok = [];
    const failed = [];
    let dirty = false;
    for (const id of ids) {
      const record = state.accounts.find(item => item.id === id);
      if (!record) {
        failed.push({ id, error: '账号不存在' });
        continue;
      }
      try {
        // 传入整个 state 以便优先级校验（批量暂不改优先级，保持接口一致而已）
        const changes = applyAccountPatch(record, allowed, state);
        if (changes.length) {
          record.updatedAt = Date.now();
          dirty = true;
        }
        ok.push({ id, name: record.name, changes });
      } catch (error) {
        failed.push({ id, name: record.name, error: error.message });
      }
    }
    if (dirty) save(state);
    const summary = Object.keys(allowed).map(key => (key === 'enabled'
      ? `启用状态 → ${allowed.enabled ? '启用' : '禁用'}`
      : `代理 → ${allowed.proxy ? (describeAccountProxy(allowed.proxy)?.label || '已设置') : '无代理（直连）'}`));
    log('[Accounts]',
      `🔀 批量修改 ${ids.length} 个账号（${summary.join('，')}）: `
      + `成功 ${ok.filter(item => item.changes.length).length} 个`
      + `${failed.length ? `，失败 ${failed.length} 个` : ''}`);
    return { ok, failed, list: listAccounts() };
  }

  /**
   * 批量删除账号。不存在的 id 记入 failed 而不是整体报错，
   * 这样用户重复点删除（或列表已刷新）不会看到莫名其妙的失败。
   * 当前账号由优先级派生，删掉它之后自动变成下一个可用账号，无需额外处理。
   */
  function batchRemove(ids) {
    if (!Array.isArray(ids) || !ids.length) throw new AccountStoreError('缺少要删除的账号 id');
    const state = load();
    const wanted = new Set(ids);
    const removed = [];
    const failed = [];
    for (const id of ids) {
      const record = state.accounts.find(item => item.id === id);
      if (!record) failed.push({ id, error: '账号不存在' });
      else removed.push({ id, name: record.name });
    }
    if (!removed.length) return { removed, failed, list: listAccounts() };

    state.accounts = state.accounts.filter(item => !wanted.has(item.id));
    save(state);
    log('[Accounts]',
      `🗑️  批量删除 ${removed.length} 个账号: ${removed.map(item => item.name).join('、').slice(0, 200)}`
      + `${failed.length ? `（${failed.length} 个不存在，已跳过）` : ''}`);
    return { removed, failed, list: listAccounts() };
  }

  /**
   * 账号记录 → auth 模块会话形态（端点/prefixPath/platform 按账号的 edition 兜底）。
   * 出网代理一并放进 session.proxy —— 计费/签到等「拿着 session 直接发请求」的
   * 调用方无需再单独传代理参数，天然与转发走同一个出口；无代理时为 null（直连）。
   * proxyError 非空表示配置的代理解析失败，调用方应回退直连并提示。
   */
  function sessionFromRecord(record) {
    const edition = resolveEdition(record.edition);
    const { proxy, error: proxyError } = resolveProxyFor(record);
    return {
      endpoint: record.endpoint || edition.endpoint,
      prefixPath: record.prefixPath ?? edition.prefixPath,
      platform: record.platform || edition.platform,
      edition: edition.id,
      proxy,
      proxyError,
      auth: {
        accessToken: record.accessToken,
        refreshToken: record.refreshToken || '',
        tokenType: 'Bearer',
        expiresAt: record.expiresAt || 0,
        refreshExpiresAt: record.refreshExpiresAt || 0,
        domain: record.domain || '',
      },
      account: {
        uid: record.uid,
        nickname: record.nickname,
        type: record.type,
        enterpriseId: record.enterpriseId,
        enterpriseName: record.enterpriseName,
      },
    };
  }

  /**
   * 当前账号 = 转发顺序里第一个「已启用且有凭证」的账号。
   *
   * 与转发选路共用同一套判据（routing 模块），所以界面标注的「当前账号」
   * 就是网关默认使用的账号 —— 不再存在「手动选了 A、转发却用 B」这种分叉。
   *
   * 唯一的例外是**按模型**的限额：当前账号对某个模型处于限额冷却时，
   * 该模型的请求会降级到下一个账号，而当前账号是模型无关的、不会跟着变
   * （换了别的模型它又是首选）。这与「手动选择与转发不一致」是两回事。
   *
   * 没有可用账号时返回 null（而不是回落到某个账号）：此时转发确实不可用，
   * 界面就不该给任何账号打「当前」标记 —— 宁可空着，也不能标一个用不了的账号。
   */
  function pickCurrentRecord(accounts) {
    const list = Array.isArray(accounts) ? accounts : [];
    return list
      .filter(item => item?.accessToken && item.enabled !== false)
      .sort(byPriorityOrder)[0] || null;
  }

  /** 当前账号 id（无账号或全部不可用时为 null） */
  function deriveCurrentId(state) {
    const record = pickCurrentRecord((state || load()).accounts);
    return record?.id || null;
  }

  /**
   * 当前账号的凭证（供请求实时取用）；没有任何可用账号时返回 null。
   * 返回 { id, session } —— session 为 auth 模块的会话形态。
   * 「当前账号」由优先级派生（队首的可用账号），与转发默认使用的账号一致。
   */
  function getCurrentEntry() {
    const state = load();
    const record = pickCurrentRecord(state.accounts);
    if (!record?.accessToken) return null;
    return { id: record.id, session: sessionFromRecord(record) };
  }

  /** 解析账号的出网代理：返回 { proxy, error }（无代理时 proxy 为 null） */
  function resolveProxyFor(record) {
    if (!record?.proxy) return { proxy: null, error: null };
    const resolved = resolveAccountProxy(record.proxy);
    if (resolved?.error) return { proxy: null, error: resolved.error };
    return { proxy: resolved, error: null };
  }

  /**
   * 实际生效的账号（供没有显式账号上下文时使用：计费默认查询、状态展示等）。
   * 与 getCurrentEntry 是同一套判据（当前账号即优先级最高的可用账号），
   * 保留独立入口是为了语义清晰：调用方要的是「兜底的活跃账号」。
   */
  function getActiveEntry() {
    return getCurrentEntry();
  }

  function getCredentialsById(id) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record?.accessToken) return null;
    const edition = resolveEdition(record.edition);
    const { proxy, error: proxyError } = resolveProxyFor(record);
    return {
      id: record.id,
      name: record.name,
      uid: record.uid,
      accessToken: record.accessToken,
      refreshToken: record.refreshToken || '',
      expiresAt: record.expiresAt || null,
      endpoint: record.endpoint || edition.endpoint,
      prefixPath: record.prefixPath ?? edition.prefixPath,
      platform: record.platform || edition.platform,
      edition: edition.id,
      priority: normalizePriority(record.priority),
      enabled: record.enabled !== false,
      proxy,
      proxyError,
    };
  }

  /**
   * 指定账号的完整会话形态（供计费接口等需要 enterpriseId / domain / prefixPath
   * 的调用方使用）。账号不存在时返回 null。
   * proxy 为该账号解析好的出网代理（null=直连）；解析失败时 proxyError 说明原因，
   * 调用方据此回退直连并记日志。
   */
  function getSessionById(id) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record?.accessToken) return null;
    const { proxy, error: proxyError } = resolveProxyFor(record);
    return { id: record.id, session: sessionFromRecord(record), proxy, proxyError };
  }

  /** 账号刷新成功后回写新 token。 */
  function updateAccountTokens(id, { accessToken, refreshToken, expiresAt, refreshExpiresAt }) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record) return false;
    if (accessToken) {
      record.accessToken = accessToken;
      record.tokenTail = accessToken.slice(-4);
    }
    if (refreshToken) record.refreshToken = refreshToken;
    if (Number(expiresAt) > 0) record.expiresAt = Number(expiresAt);
    if (Number(refreshExpiresAt) > 0) record.refreshExpiresAt = Number(refreshExpiresAt);
    record.updatedAt = Date.now();
    save(state);
    return true;
  }

  /**
   * 旧版单账号 auth.json 迁移：仅当 accounts.json 尚无任何账号时导入。
   */
  function importLegacySession(legacy) {
    if (!legacy?.auth?.accessToken) return null;
    const state = load();
    if (state.accounts.length) return null;
    const uid = legacy.account?.uid || '';
    if (!uid) {
      log('[Accounts]', '旧登录态缺少 uid，跳过迁移');
      return null;
    }
    return addAccount({
      auth: legacy.auth,
      account: legacy.account,
      prefixPath: legacy.prefixPath,
      endpoint: legacy.endpoint,
      platform: legacy.platform,
      edition: legacy.edition,
    }, legacy.account?.nickname);
  }

  /**
   * 记录账号对某模型的限额状态（上游 429 / code 6004）。
   * resetAt 为恢复时间戳（msg 里 "将在 YYYY-MM-DD HH:mm:ss UTC+8 重置" 解析而来），
   * 缺失时给 10 分钟兜底冷却，避免短时间内反复撞限额。
   */
  function markRateLimited(id, model, { status = 429, code, resetAt, message } = {}) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record) return null;
    const now = Date.now();
    const entry = {
      status: Number(status) || 429,
      code: code ?? null,
      resetAt: Number(resetAt) > now ? Number(resetAt) : now + 10 * 60 * 1000,
      message: String(message || '').slice(0, 300),
      at: now,
    };
    record.rateLimits = { ...(record.rateLimits || {}), [String(model || '')]: entry };
    record.updatedAt = now;
    save(state);
    return entry;
  }

  /** 清除账号对某模型的限额记录（该模型请求成功时调用） */
  function clearRateLimit(id, model) {
    const state = load();
    const record = state.accounts.find(item => item.id === id);
    if (!record?.rateLimits) return false;
    const key = String(model || '');
    if (!(key in record.rateLimits)) return false;
    delete record.rateLimits[key];
    if (!Object.keys(record.rateLimits).length) delete record.rateLimits;
    record.updatedAt = Date.now();
    save(state);
    return true;
  }

  /**
   * 启动时执行一次优先级去重迁移（仅在磁盘数据存在并列时才写盘）。
   * 由 server.mjs 在装配阶段调用，返回值供启动日志展示。
   */
  function migratePriorities() {
    const state = load();
    const result = normalizePriorities(state);
    if (!result.changed) return result;
    save(state);
    log('[Accounts]', `🔢 优先级已去重（${result.assignments.length} 个账号重新编号，转发顺序保持不变）`);
    return result;
  }

  return {
    get file() { return filePath; },
    addAccount,
    removeAccount,
    promoteToFront,
    updateAccount,
    batchUpdate,
    batchRemove,
    moveAccount,
    promoteToFront,
    migratePriorities,
    listAccounts,
    getCurrentEntry,
    getActiveEntry,
    getCredentialsById,
    getSessionById,
    updateAccountTokens,
    markRateLimited,
    clearRateLimit,
    importLegacySession,
    exportAccounts,
    importAccounts,
    existsSync: () => existsSync(filePath),
  };
}
