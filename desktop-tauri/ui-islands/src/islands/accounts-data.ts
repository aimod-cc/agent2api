/**
 * 账号页的**动作层与对外契约**（非岛：`.ts` 不被 import.meta.glob 当岛加载）。
 *
 * 替换 ui/accounts-view.js 的有状态部分 + ui/accounts-filters.js 的交互 +
 * ui/usage-actions.js + ui/proxy-form.js 的 Clash 缓存。分四层：
 *   · accounts-domain.ts  纯逻辑（provider 能力表 / 判定 / 筛选口径 / 队列位置）
 *   · accounts-store.ts   状态 store（快照 / 筛选 / 勾选 / 展开态 / 账号名隐藏）
 *   · 本文件              动作（余额 / 签到 / 连接数 / Clash 缓存 / 行内动作 / 弹窗）
 *                         + `window.wbAccountsView` / `wbAccountsModel` / `wbAccountPanel`
 *   · accounts-page.tsx / accounts-panels.tsx / accounts-dialogs.tsx  视图
 * 状态对外只有一条读路径（`export * from './accounts-store'`），视图侧的 import 路径
 * 因此只有本文件一处。
 *
 * ── 对外契约（调用点逐个 grep 确认过）──────────────────────────
 *   · `wbAccountsView`  app.js:147 syncConnections / :575 render / :622 refreshCaches /
 *                       :811 与 tasks-panel.tsx 的 syncBalancesSnapshot / providers.js:138 render；
 *                       tasks-panel.tsx 的「立即签到一次」还调 refreshUsageAfterCheckin
 *                       —— 那条路径的签到不在账号页里，但积分同样会变，余额得跟着刷
 *   · `wbAccountsModel` app.js:178 isRateLimited / :663 isDesktopAccount / report.js:307
 *                       editionSuffix / models-fetch-modal.tsx:245 providerFeatures、:255 byPriorityOrder
 *   · `wbAccountPanel`  app.js:623 invalidate / :645 open
 * 注册必须在**模块求值时**完成：app.js 的 refresh() 是异步的（首个 await 之后才轮到渲染），
 * 但 report.js / 别的同步脚本紧随其后执行 —— 拿不到就会退化成静默不生效。
 *
 * ── 全局一条队列 ─────────────────────────────────────────────
 * 优先级是全局唯一的一条队列（见 accounts-domain.ts 的模块头）。行序 = 优先级升序；
 * ↑/↓ 与全局相邻账号交换；「设为首选」只把账号移到全局第一位，不改变启用状态。
 */

import {
  errorMessage, POOL_VALUE_PREFIX, shared, toast,
  type AccountRecord, type ClashSnapshot, type PanelKind, type PoolItem, type UsageEntry,
} from './accounts-shared'
import {
  checkinableAccounts, claimedPlanIdsToday, displayNameOf, isDesktopAccount, isEnabled, isRateLimited,
  supportsCheckin, supportsUsage,
} from './accounts-domain'
import * as domain from './accounts-domain'
import { clampPriority, priorityOf } from './accounts-columns'
import {
  allAccounts, bump, findAccount, getStore, isPicked, openPanelsFor, panelOpen, patch,
} from './accounts-store'

// 状态层原样再导出：视图侧（accounts-page / panels / dialogs）只认本文件这一条路径
export * from './accounts-store'

/**
 * 「未配置查询凭证」的标记（与后端 `providers::adapter::USAGE_NOT_CONFIGURED_CODE` 逐字
 * 一致）—— 这是一个**前后端契约常量**：改一边必须改另一边，不一致的后果是那种情况
 * 退回红色「查询失败」 */
export const NOT_CONFIGURED_CODE = 'usage_not_configured'

/**
 * 余额的结果缓存：**原地可变**的 Map，改完调 bump() 通知。
 * 为什么不像其余状态那样每次换新对象：写 `null`（「查询中」）这个中间态必须让视图立刻
 * 看到，而它是高频小改动（每次点按钮 / 每轮批量），每次拷贝一份纯属浪费。
 */
const usageMap = new Map<string, UsageEntry>()

export const usageEntries = (): ReadonlyMap<string, UsageEntry> => usageMap

/**
 * 上一次签到的**失败原因**（按账号 id）。
 *
 * 签到没有明细面板，结果只落在两处：成功/已领取由行上那颗按钮自己的状态表达
 * （`checkedInToday` → 「已签到」）＋ 一条 toast；失败则要留下可复看的原因 ——
 * toast 几秒后就没了，而「为什么没签上」正是需要复看的信息，所以记在这里，
 * 由按钮的 title 读出来（见 `checkinErrorOf`）。
 *
 * 成功与「今天已领取」都不进这张表：前者按钮会变「已签到」，后者是上游的正常状态，
 * 不该在按钮上挂一个「失败」提示。
 */
const checkinErrors = new Map<string, string>()

/** 该账号上一次签到的失败原因（没有则空串）—— 行上「签到」按钮的 title 读它 */
export const checkinErrorOf = (id: string): string => checkinErrors.get(id) || ''

/* ─── Clash 出口缓存（只剩代理表单的「Clash Verge」档在用）─────
 *
 * 代理列不再列 Clash 出口（出口统一走代理池，见 accounts-panels 的 ProxyCell），
 * 所以这里没有「列上没就绪就补拉」那条自愈链了 —— 唯一的消费者是账号设置
 * 弹窗里的 **Clash 直引档**（只在账号当前就是这种存量配置时出现），
 * 它挂载时自己拉一次、失败还有「重新读取」按钮。
 * 缓存仍放 store（`clash` 字段）：一次弹窗打开内多实例共享，避免重复请求。 */

let clashInflight: Promise<ClashSnapshot> | null = null

/**
 * 读一次 Clash 出口列表（模块级缓存；并发调用合并成一次）。
 * 返回值必须带 `clash` 对象，否则同样按失败处理：桥异常时可能 **resolve 出
 * `undefined`（而不是 reject）**——不校验的话，这种失败会伪装成「没有出口」。
 */
export async function clashOptions(options: { force?: boolean } = {}): Promise<ClashSnapshot> {
  const cached = getStore().clash
  if (!options.force && cached) return cached
  if (!clashInflight) {
    clashInflight = (async () => {
      try {
        const data = await shared().workbuddyDesktop?.getProxies?.()
        if (!data || typeof data !== 'object' || !data.clash) {
          throw new Error(`代理列表响应异常（${typeof data}）`)
        }
        return data.clash
      } finally {
        clashInflight = null
      }
    })()
  }
  const clash = await clashInflight
  patch({ clash })
  return clash
}

/**
 * 失效重读用：清掉缓存（下一次调用会真正重新读取）。
 *
 * 对外契约 `wbAccountPanel.invalidate`（app.js 在账号列表刷新后调用）。
 * 现在唯一的读者是代理表单的 Clash 直引档：清掉之后，下一次打开账号设置
 * 弹窗会重新读 Clash 端口 —— 那类记录的端口由 Clash 实时决定，可能刚被改过。
 */
export function invalidateClashCache(): void {
  patch({ clash: null })
}

/* ─── 代理池（「网络代理」页的命名代理，账号页的代理下拉）─── */

/**
 * 代理池列表缓存。与 clash 那份（放 store 里）不同：它**只有代理表单用**
 * （账号表的代理列读的是账号自己的 proxy 描述，不需要池），所以不进 store，
 * 一个模块变量 + 在途 Promise 就够。
 */
let poolCache: PoolItem[] | null = null
let poolInflight: Promise<PoolItem[]> | null = null

/** 已缓存的池列表（`null` = 还没读过）；表单首帧用它避免下拉先空一拍 */
export function proxyPoolSnapshot(): PoolItem[] | null {
  return poolCache
}

/** 清掉缓存（表单里的「重新读取」用） */
export function invalidateProxyPoolCache(): void {
  poolCache = null
}

/**
 * 读一次代理池（模块级缓存；并发调用合并成一次）。
 * 与 clashOptions 同一条纪律：桥异常时可能 resolve 出 `undefined`（而不是 reject），
 * 那种情况按失败处理 —— 不校验的话会伪装成「池里一条都没有」。
 */
export async function proxyPoolOptions(options: { force?: boolean } = {}): Promise<PoolItem[]> {
  if (!options.force && poolCache) return poolCache
  if (!poolInflight) {
    poolInflight = (async () => {
      try {
        const data = await shared().workbuddyDesktop?.getProxyPool?.()
        if (!data || typeof data !== 'object' || !Array.isArray(data.items)) {
          throw new Error(`代理池响应异常（${typeof data}）`)
        }
        return data.items
      } finally {
        poolInflight = null
      }
    })()
  }
  const items = await poolInflight
  poolCache = items
  return items
}

/** 代理池读取失败的原因（成功后清空）—— 代理列据此显示「读取失败」的说明项 */
let lastPoolError: string | null = null

export function poolError(): string | null {
  return lastPoolError
}

/**
 * 代理列的池条目自愈：没读到就补拉（节流 30 秒，与 `ensureClashOptions` 同一套
 * 理由 —— 没有它，「失败 → 重画 → 又拉」会变成死循环；有它，网络 / 桥恢复后
 * 最多半分钟自己补上）。
 */
let poolRetryAt = 0
let poolWarned = false

export function ensureProxyPoolOptions(): void {
  if (poolCache || Date.now() < poolRetryAt) return
  if (typeof shared().workbuddyDesktop?.getProxyPool !== 'function') {
    // 桥没挂上（加载失败等）：本页面会话内不再尝试，并告警一次
    if (!poolWarned) {
      poolWarned = true
      console.warn('[accounts] 桥未提供 getProxyPool：代理列的「已保存的代理」选项不可用')
    }
    poolRetryAt = Infinity
    return
  }
  poolRetryAt = Date.now() + 30_000
  void proxyPoolOptions()
    .then(() => {
      poolRetryAt = 0
      lastPoolError = null
      // 读到了要重画一次：这一格渲染时读的是同步快照，没有订阅者通知它
      bump()
    })
    .catch(error => {
      lastPoolError = errorMessage(error)
      bump()
    })
}

/* ─── 弹窗 ─────────────────────────────────── */

export function openSettingsDialog(id: string): void {
  if (!findAccount(id)) { toast('账号不存在，请刷新后重试', 'err'); return }
  patch({ dialog: { kind: 'settings', id } })
}

export function closeDialog(): void {
  patch({ dialog: null })
}

export function openBatchDialog(ids: string[], action = 'enable'): void {
  const selected = (Array.isArray(ids) ? ids : []).filter(id => findAccount(id))
  if (!selected.length) { toast('请先勾选要操作的账号', 'err'); return }
  patch({ dialog: { kind: 'batch', ids: selected, action } })
}

/* ─── 删除账号后的缓存清理 ───────────────────── */

/** 清掉已删除账号的本地缓存（app.js 每次 refresh 后调用） */
export function refreshCaches(validIds: Set<string>): void {
  let touched = false
  for (const id of [...usageMap.keys()]) if (!validIds.has(id)) { usageMap.delete(id); touched = true }
  for (const id of [...checkinErrors.keys()]) if (!validIds.has(id)) { checkinErrors.delete(id); touched = true }
  const panels = new Map(getStore().panels)
  for (const id of [...panels.keys()]) if (!validIds.has(id)) { panels.delete(id); touched = true }
  const connections = new Map(getStore().connections)
  for (const id of [...connections.keys()]) if (!validIds.has(id)) { connections.delete(id); touched = true }
  const inflight = new Set(getStore().usageInflight)
  for (const id of [...inflight]) if (!validIds.has(id)) { inflight.delete(id); touched = true }
  const selected = new Set(getStore().selected)
  for (const id of [...selected]) if (!validIds.has(id)) { selected.delete(id); touched = true }
  if (touched) patch({ panels, connections, usageInflight: inflight, selected })
}

/* ─── 连接数（实时，2 秒一轮）────────────────────
 * 口径与 OmniProxy 上游管理页的「连接」列一致：**此刻正在使用这个账号的请求数**
 * （一个请求在账号间轮换时计数跟着走，见后端 core::upstream::connections）。
 * 单独一条 2 秒轮询而不是跟着 app.js 那 20 秒一轮：连接数的全部意义就在「现在」。
 * 后端那边是进程内计数（不读盘、不出网），所以这条链路的代价足够低。 */

/** 2 秒：够快看得出「正在跑」，又不至于让 DevTools 的网络面板刷屏 */
export const CONNECTIONS_POLL_MS = 2000

export function connectionsOf(id: string): number {
  return getStore().connections.get(id) || 0
}

/**
 * 拉一次连接数。失败静默：2 秒一次的轮询，后端不可达时 toast 会变成刷屏；
 * 且缓存保留上一轮的值比清空更贴近事实（用户看到的是「刚才还在跑」）。
 */
export async function syncConnections(): Promise<boolean> {
  try {
    const data = await shared().workbuddyDesktop?.getAccountConnections?.()
    const counts = data?.counts && typeof data.counts === 'object' ? data.counts : {}
    const connections = new Map<string, number>()
    for (const [id, value] of Object.entries(counts)) {
      const count = Number(value) || 0
      if (count > 0) connections.set(id, count)
    }
    patch({ connections })
    return true
  } catch {
    // 静默：下一次轮询自然重试（与 app.js 的 refresh / syncLogsBadge 同一取舍）
    return false
  }
}

/**
 * 起轮询定时器（只起一次）。只在**账号页可见**时真发请求：切到别的页、或窗口被
 * 最小化时不打后端 —— 与 requests-page / logs-panel 的轮询同一取舍。
 */
let connectionsTimer: ReturnType<typeof setInterval> | null = null
export function startConnectionsPolling(): void {
  if (connectionsTimer) return
  connectionsTimer = setInterval(() => {
    if (document.hidden || shared().wbApp?.currentPage !== 'accounts') return
    void syncConnections()
  }, CONNECTIONS_POLL_MS)
}

/* ─── 余额 / 签到：动作层 ───────────────────── */

/**
 * 缓存条目 → 失败描述的**唯一入口**（余额列的摘要渲染与这里的 toast 播报共用同一份
 * 判据，两处不会一个说红一个说灰）。返回 null 表示这不是失败（还在查询中 / 是结果）。
 *
 * 「未配置」为什么是中性的：CatPaw 的余额接口要一个单独的网页会话凭证（token2），
 * 没配置时后端返回 `code: "usage_not_configured"`。那不是故障：账号本身完全正常、
 * 转发照跑，只是用户还没告诉网关那个凭证长什么样。把它渲染成红色的「查询失败」会让
 * 人去排查一个不存在的故障，所以判据用后端给的 `code` **而不是匹配文案**。
 */
export function usageFailureOf(entry: UsageEntry): { message: string; notConfigured: boolean } | null {
  if (entry === undefined || entry === null) return null
  if (typeof entry === 'string') return { message: entry, notConfigured: false }
  if (typeof entry !== 'object') return null
  const error = entry.error
  if (!error) return null
  return { message: String(error), notConfigured: entry.code === NOT_CONFIGURED_CODE }
}

/** 后端结果行 → 缓存条目（成功给 `usage`，失败给 `{error, code?}`）。
 *  失败行的 `code` 必须留住：`usageFailureOf` 按它区分「未配置查询」（中性提示）
 *  与真正的失败（红色）—— 只存 error 字符串会丢掉这个判据。 */
function cacheEntryOf(row: Record<string, unknown>): UsageEntry {
  if (row.usage) return row.usage as Record<string, unknown>
  return { error: row.error ? String(row.error) : '余额响应为空', code: row.code }
}

/** 把一批余额结果写进列表缓存（定时快照、批量查询与外部调用共用）。返回写入条数。 */
export function applyBalances(balances: { results?: Array<Record<string, unknown>> } | null | undefined): number {
  const rows = Array.isArray(balances?.results) ? balances.results : []
  let applied = 0
  for (const row of rows) {
    if (!row?.id) continue
    usageMap.set(String(row.id), cacheEntryOf(row))
    applied++
  }
  if (applied) bump()
  return applied
}

/**
 * 拉一次「定时查询积分」的结果快照并写进缓存，返回是否应用了新的一轮。
 *
 * 余额查询在后端有条定时任务（默认每 10 分钟查全部账号），结果存在后端快照里。
 * 界面不点按钮时也要跟着它更新 —— 否则定时任务在后台跑得好好的，用户看到的还是启动
 * 那一次的旧余额，那正是「定时查询」最容易让人觉得「没生效」的地方。
 * `at` 是那一刻的毫秒时间戳，用它判断「这一轮我应用过了没」：时间戳没变就直接返回，
 * 不做无谓的重绘。**失败的行同样会被应用**（后端快照里就带着它们），于是账号页会
 * 明确显示「查询失败」而不是悄悄留着上一个成功的旧值。
 * 失败静默（不 toast）：它是 20 秒一次的轮询，网关长时间不可用会变成刷屏。
 */
let lastSnapshotAt = 0
export async function syncBalancesSnapshot(): Promise<boolean> {
  try {
    const data = await shared().workbuddyDesktop?.getBalancesSnapshot?.()
    const at = Number(data?.at) || 0
    // at = 0 表示本进程还没定时查过（刚启动、或任务被关掉）—— 不覆盖已有缓存
    if (!at || at === lastSnapshotAt) return false
    lastSnapshotAt = at
    if (!applyBalances(data)) return false
    return true
  } catch {
    // 静默：下一次轮询自然重试；账号页保持上一轮的结果不变
    return false
  }
}

/**
 * 查询余额。`id` 缺省 = 全部（后端批量目标集合）；给了 id 则**带 `?id=` 请求**。
 *
 * 为什么单查要走 `?id=` 而不是「取一批后筛一条」：后端批量路径的目标集合是「全部
 * **启用**账号」，但用户手点某一行账号的「余额」按钮问的是另一个问题：「这个账号现在
 * 还剩多少」。按启用状态把它挡掉，结果里就没有这一行，界面只能兜底成「未返回余额数据」
 * —— 用户分不清是禁用了还是上游挂了。所以单查带 id 走后端那条**不看启用状态**的分支。
 * 批量（`id` 缺省）仍是「全部启用账号」，行为与改造前一致。
 */
export async function queryUsageFor(id?: string | null): Promise<{ results?: Array<Record<string, unknown>> } | null | undefined> {
  const data = await shared().workbuddyDesktop?.getAllBalances?.(id || undefined)
  const rows = Array.isArray(data?.results) ? data.results : []
  const returned = new Set<string>()
  for (const row of rows) {
    if (!row?.id) continue
    const rowId = String(row.id)
    if (id && rowId !== id) continue
    returned.add(rowId)
    usageMap.set(rowId, cacheEntryOf(row))
  }
  if (id) {
    // 后端返回了 0 行才是真的「没数据」（账号刚被删、或 provider 不认这个 id）
    if (!returned.has(id)) usageMap.set(id, '未返回余额数据')
    bump()
    return data
  }
  // 只给**批量目标集合内的**账号补「未返回」：后端的目标集合是「启用 + 有余额概念」，
  // 缺失一行才是异常。禁用账号不在集合里，补它等于把「这行没参与本轮查询」说成
  // 「上游没给数据」—— 与单查那个 bug 同源。
  for (const account of allAccounts()) {
    if (!returned.has(account.id) && supportsUsage(account) && account.enabled !== false) {
      usageMap.set(account.id, '未返回余额数据')
    }
  }
  bump()
  return data
}

/**
 * 批量查询全部可查询账号的余额（工具条「查询余额」）。
 * 目标集合只含「有余额概念 + 启用」：已禁用账号后端同样会跳过，界面若把它算进分母，
 * 播报的「已更新 N/M」会与真实条数对不上。
 * 每次点击都是一次查询（明细面板已取消，没有「第二次点击收起」那套语义）。
 */
export async function queryAllUsage(): Promise<void> {
  if (getStore().usageBusy) return
  const targets = allAccounts().filter(account => supportsUsage(account) && account.enabled !== false)
  if (!targets.length) { toast('暂无可查询余额的账号', 'err'); return }
  patch({ usageBusy: true })
  // 先写「查询中」再重绘：余额列立刻显示查询中，结果回来了直接换成读数
  targets.forEach(account => usageMap.set(account.id, null))
  bump()
  try {
    const rows = (await queryUsageFor(null))?.results || []
    const ok = rows.filter(row => row.usage).length
    // 「未配置查询」不算失败：那些账号成功返回了、只是缺一个可选的凭证
    const skipped = rows.filter(row => row.code === NOT_CONFIGURED_CODE).length
    const suffix = skipped ? `（${skipped} 个未配置查询凭证）` : ''
    const failed = rows.length - ok - skipped
    toast(ok + skipped === rows.length
      ? `✅ 已更新 ${ok} 个账号的余额${suffix}`
      : `已更新 ${ok}/${rows.length} 个账号，${failed} 个失败`, failed ? 'err' : 'ok')
  } catch (error) {
    const message = errorMessage(error)
    targets.forEach(account => usageMap.set(account.id, `查询失败：${message}`))
    bump()
    toast(`余额查询失败：${message}`, 'err')
  } finally {
    patch({ usageBusy: false })
  }
}

/**
 * 签到。`id` 缺省 = 全部可签到账号串行签到；指定 id = 单账号签到。
 * 目标集合只用「有签到概念 + 国内版」的账号（后端同样只把国际版排除在外，
 * **不看启用状态**）。
 *
 * 只发请求、不动任何界面缓存：结果的呈现由调用方决定（行上按钮的状态 + toast，
 * 以及顺带一次余额刷新，见 `runCheckin` / `checkinAll`）—— 签到**没有明细面板**，
 * 别在这里挂面板状态。
 */
export async function checkinFor(id?: string | null): Promise<{
  results?: Array<Record<string, unknown>>
  succeeded?: number
  total?: number
  skipped?: number
} | null | undefined> {
  return shared().workbuddyDesktop?.checkinAllAccounts?.(id || null)
}

/**
 * 一行签到结果的分类。三种结局互斥，判据只看 claim 的两个布尔：
 *   - `ok`：本次真的领到了（`claim.success === true`）；
 *   - `already`：上游说今天已经领过了（`claim.alreadyCompleted === true`）——
 *     **不是失败**：一天里大部分时候点签到都是这个结果，报红会把正常状态说成故障；
 *   - `failed`：其余（`row.error` 后端分派层报的错、`claim.success === false` 且
 *     不是已领取、完全没返回结果），原因取 msg。
 */
type CheckinOutcome = { kind: 'ok' | 'already' | 'failed'; reason: string }

function checkinOutcomeOf(row: Record<string, unknown> | undefined): CheckinOutcome {
  if (!row) return { kind: 'failed', reason: '未返回签到结果' }
  if (row.error) return { kind: 'failed', reason: String(row.error) }
  const claim = row.claim as Record<string, unknown> | null | undefined
  if (!claim) return { kind: 'failed', reason: '签到响应为空' }
  if (claim.success === true) return { kind: 'ok', reason: '' }
  if (claim.alreadyCompleted === true) return { kind: 'already', reason: '' }
  return { kind: 'failed', reason: String(claim.msg || '未领取') }
}

/**
 * 批量签到（工具条「全部签到」）。结果只走 toast —— 没有明细面板可展开。
 * 签到完还会静默刷新一次余额：签到发的积分 / 权益本来就要落到余额列上。
 */
export async function checkinAll(): Promise<void> {
  if (getStore().checkinBusy) return
  const targets = checkinableAccounts(allAccounts())
  if (!targets.length) {
    toast('暂无可签到的账号（签到仅限 WorkBuddy 国内版 / 小浣熊 / AutoClaw / Qoder 中国版 / Trae）', 'err')
    return
  }
  if (!(await shared().wbConfirm?.ask?.({
    title: '批量签到',
    html: `将对 <strong>${targets.length}</strong> 个账号串行签到，可能需要一点时间。继续？`,
    okText: '继续',
    bodyClass: '',
  }))) return
  patch({ checkinBusy: true })
  try {
    const data = await checkinFor(null)
    const rows = Array.isArray(data?.results) ? data.results : []
    const byId = new Map(rows.map(row => [String(row?.id || ''), row]))
    let ok = 0
    let already = 0
    const failed: string[] = []
    for (const account of targets) {
      const outcome = checkinOutcomeOf(byId.get(account.id))
      if (outcome.kind === 'ok') {
        ok += 1
        checkinErrors.delete(account.id)
      } else if (outcome.kind === 'already') {
        already += 1
        checkinErrors.delete(account.id)
      } else {
        failed.push(`${displayNameOf(account) || account.id}：${outcome.reason}`)
        checkinErrors.set(account.id, outcome.reason)
      }
    }
    bump()
    // 失败详情：个数 + 第一条原因（各账号自己的原因记进按钮 title，可逐个悬停复看）
    const parts = [`成功领取 ${ok} 个`]
    if (already) parts.push(`今日已领取 ${already} 个`)
    if (failed.length) parts.push(`未领取 ${failed.length} 个（首个：${failed[0]}）`)
    const skipped = Number(data?.skipped) || 0
    toast(`签到完成：${parts.join('，')}`
      + (skipped ? `；跳过 ${skipped} 个国际版 / 所属家无签到的账号` : ''), failed.length ? 'err' : 'ok')
    // 签到会改变余额读数（签到发的就是积分 / 权益）：静默再查一遍，余额列直接落到
    // 新读数（不 await、不播报，理由见 refreshUsageAfterCheckin）
    void refreshUsageAfterCheckin()
  } catch (error) {
    const message = errorMessage(error)
    targets.forEach(account => checkinErrors.set(account.id, message))
    bump()
    toast(`签到失败：${message}`, 'err')
  } finally {
    patch({ checkinBusy: false })
    // 重拉账号状态：签到时间由后端落盘，行上的签到按钮据此变成「已签到」。
    // 不重拉的话按钮要等下一轮 20 秒轮询才跟上，那期间还显示成可点。
    void shared().wbApp?.refresh?.()
  }
}

/**
 * 单个账号签到：请求 → toast 结果 → 重拉账号状态（`checkinAt` 由后端落盘）。
 *
 * 三种结局各自的落点（见 `checkinOutcomeOf`）：
 *   - 成功 → toast ✅，按钮随即变成「已签到」；
 *   - 今日已领取 → 中性 toast（正常状态，不是故障），按钮同样变「已签到」；
 *   - 未领取 → toast 原因 + 把它记进按钮 title（toast 会消失，原因要能复看）。
 *
 * 请求正常返回（三种结局都算）后顺带**静默查一次该账号的余额** —— 签到会改变余额
 * 读数，见 `refreshUsageAfterCheckin`。请求本身抛错时不查：那时后端多半不可达。
 */
export async function runCheckin(id: string): Promise<void> {
  const label = displayNameOf(findAccount(id)) || id
  try {
    const data = await checkinFor(id)
    const rows = Array.isArray(data?.results) ? data.results : []
    const row = rows.find(item => item?.id === id) || rows[0]
    const outcome = checkinOutcomeOf(row)
    if (outcome.kind === 'failed') {
      checkinErrors.set(id, outcome.reason)
      toast(`签到失败：${label}：${outcome.reason}`, 'err')
    } else {
      checkinErrors.delete(id)
      toast(outcome.kind === 'already' ? `${label}：今日已领取` : `✅ ${label} 签到成功`, 'ok')
    }
    bump()
    // 签到会改变余额读数：此刻刷新余额（静默，见 refreshUsageAfterCheckin）。
    // 只查这一行 —— 用户点的是这个账号，别的行没动过
    void refreshUsageAfterCheckin(id)
    void shared().wbApp?.refresh?.()
  } catch (error) {
    const message = errorMessage(error)
    checkinErrors.set(id, message)
    bump()
    toast(`签到失败：${label}：${message}`, 'err')
  }
}

/**
 * 查询一个账号余额的执行体：在途去重 + 「查询中」中间态。异常**照原样抛给调用方**
 * —— 失败怎么落缓存、要不要播报由调用方定，两个调用点的口径不同：
 *   · 行上「余额」按钮（`queryUsageOnce`）→ 失败落进余额列 + 红色 toast；
 *   · 签到后的自动刷新（`refreshUsageAfterCheckin`）→ 只落缓存，不播报。
 */
async function runUsageQuery(id: string): Promise<void> {
  if (getStore().usageInflight.has(id)) return
  const inflight = new Set(getStore().usageInflight)
  inflight.add(id)
  usageMap.set(id, null)
  patch({ usageInflight: inflight })
  try {
    await queryUsageFor(id)
  } finally {
    const next = new Set(getStore().usageInflight)
    next.delete(id)
    patch({ usageInflight: next })
  }
}

/** 行上「余额」按钮：在途去重（同一账号同时发几份一模一样的上游请求，界面上看不出区别） */
export async function queryUsageOnce(id: string): Promise<void> {
  if (getStore().usageInflight.has(id)) return
  try {
    await runUsageQuery(id)
    // 缓存的四种形态（见 usageFailureOf）：undefined/null/字符串/对象，对象里再分
    // 「未配置」与「失败」—— 提示语要跟着这个分叉走
    const failure = usageFailureOf(usageMap.get(id))
    if (failure?.notConfigured) toast(failure.message, 'ok')
    else if (failure) toast(`余额查询失败：${failure.message}`, 'err')
    else toast('✅ 已更新余额')
  } catch (error) {
    usageMap.set(id, `查询失败：${errorMessage(error)}`)
    bump()
    toast(`余额查询失败：${errorMessage(error)}`, 'err')
  }
}

/**
 * 动作之后的余额刷新：**这些动作会改变余额读数**（小浣熊与 AutoClaw 的签到直接
 * 发积分、Qoder 的签到发权益、ZCode 的「领套餐」领到的就是 token 额度），
 * 而余额列读的是缓存里的旧读数 —— 不刷新的话，用户要再点一次「余额」才看得到
 * 刚领到的那笔。所以动作一结束就把读数重新拉一遍。
 *
 * 名字里的 checkin 是历史（这条链最早只服务签到）；领套餐那条也走它，
 * 差别只在「刷哪一行」由调用方给定 `id`。
 *
 * ── 为什么静默（不 toast、也不复用 queryUsageOnce / queryAllUsage 的播报）──
 * toast 是单例，后一条会把前一条**顶掉**：签到结果才是用户刚点那个动作的结果，
 * 不能被一条「已更新余额」挤掉。反馈改由界面自己给 —— 余额列先显示「查询中」、
 * 再落到新读数；批量那条同时把工具条的「查询中…」点亮（`usageBusy`，顺带挡住
 * 用户在刷新期间重复点「查询余额」）。
 *
 * 目标集合：`id` 给定 = 该账号（后端单查路径不看 `enabled`，与行上那颗「余额」
 * 按钮同一条）；缺省 = 与工具条「查询余额」逐字相同的集合（有余额概念 + 启用），
 * 免得自动刷新比手动查询还「多查一批」。没有余额概念的账号直接跳过 ——
 * 签到范围的几家都有余额概念，这一条是留给将来新增 provider 的兜底。
 *
 * 失败只写缓存（余额列显示失败原因）、不播报：签到请求成功而余额查询失败时，
 * 用户需要的是「这行为什么没有读数」，而那条原因就在列上。
 */
export async function refreshUsageAfterCheckin(id?: string): Promise<void> {
  if (id) {
    const account = findAccount(id)
    if (!account || !supportsUsage(account)) return
    try {
      await runUsageQuery(id)
    } catch (error) {
      usageMap.set(id, `查询失败：${errorMessage(error)}`)
      bump()
    }
    return
  }
  if (getStore().usageBusy) return
  const targets = allAccounts().filter(account => supportsUsage(account) && account.enabled !== false)
  if (!targets.length) return
  patch({ usageBusy: true })
  targets.forEach(account => usageMap.set(account.id, null))
  bump()
  try {
    await queryUsageFor(null)
  } catch (error) {
    const message = errorMessage(error)
    targets.forEach(account => usageMap.set(account.id, `查询失败：${message}`))
    bump()
  } finally {
    patch({ usageBusy: false })
  }
}

/* ─── 行内动作（优先级 / 启用 / 顺序 / 限流 / 代理）───── */

/**
 * 提交一个优先级。失败（409 冲突）时**必须还原显示值**：留着用户输的数字会让人以为
 * 存进去了，而实际的转发顺序没变 —— 下一次刷新时它会悄悄跳回去，那比当场报错更让人
 * 困惑。后端的 409 文案已经带上了占位者的姓名，直接透出即可（前端不知道谁是占位者，
 * 除非再算一遍，而两套算法迟早会分叉）。
 */
export async function commitPriority(id: string, raw: string): Promise<number | null> {
  const account = findAccount(id)
  if (!account) return null
  const current = priorityOf(account)
  const text = String(raw ?? '').trim()
  const parsed = text ? Number(text) : NaN
  // 非法值 / 未改动：只把显示复原，不发请求（改成同一个值后端也会返回空 changes）
  if (!Number.isFinite(parsed)) return current
  const next = clampPriority(parsed)
  if (next === current) return current
  try {
    await shared().workbuddyDesktop?.updateAccount?.(id, { priority: next })
    toast(`✅ 优先级已改为 ${next}`)
    // 改完顺序会变，必须重拉：只改本地状态的话行不会重排，看起来「没生效」
    await shared().wbApp?.refresh?.()
    return next
  } catch (error) {
    toast(`优先级未保存：${errorMessage(error)}`, 'err')
    // 冲突可能来自别处已经改过的数据（比如另一端刚占了号），补一次刷新让列表回到事实
    void shared().wbApp?.refresh?.()
    return current
  }
}

/**
 * 启用 / 禁用单个账号（状态列的开关与 ⋯ 菜单的第一项走同一条链）。
 *
 * 为什么不用 app.js 的 runAccountAction：那个入口的 switch 分支只认
 * switch / refresh / remove，别的 action 会静默走完不做事。
 * 用 updateAccount 走 PATCH —— 后端 apply_patch 只改显式传入的字段，这里只传 enabled。
 */
export async function setAccountEnabled(id: string, enabled: boolean): Promise<void> {
  try {
    await shared().workbuddyDesktop?.updateAccount?.(id, { enabled })
    await shared().wbApp?.refresh?.()
    toast(enabled ? '✅ 已启用' : '✅ 已禁用')
  } catch (error) {
    toast(`操作失败：${errorMessage(error)}`, 'err')
    // 失败时把开关拨回去：界面上不能留一个「已改」的假象
    void shared().wbApp?.refresh?.()
  }
}

/**
 * 与全局相邻账号交换优先级。
 *
 * 方向与语义的对应（**极易搞反**）：优先级数值越小越先用，所以
 *   · up   = 与队列里的**上一个**账号交换 = 排得更靠前 = 数值变小
 *   · down = 与队列里的**下一个**账号交换 = 排得更靠后 = 数值变大
 * 来自后端 `move_account(id, "up" | "down")` 的语义（down 找 index + 1，up 找 index - 1）。
 */
export async function moveAccount(id: string, direction: 'up' | 'down'): Promise<void> {
  try {
    await shared().workbuddyDesktop?.moveAccount?.(id, direction)
    await shared().wbApp?.refresh?.()
  } catch (error) {
    toast(`调整顺序失败：${errorMessage(error)}`, 'err')
  }
}

/** 清除限流标记：单条（model）或全部。后端返回新快照，交给全局刷新对齐。 */
export async function clearLimits(id: string, model?: string): Promise<void> {
  try {
    await shared().workbuddyDesktop?.clearRateLimits?.(id, model || null)
    toast(model ? `✅ 已清除 ${model} 的限流标记` : '✅ 已清除该账号全部限流标记')
    await shared().wbApp?.refresh?.()
  } catch (error) {
    toast(`清除失败：${errorMessage(error)}`, 'err')
  }
}

/**
 * 代理列下拉的保存。值只有两种：空串 = 直连（proxy 传 null），否则是 Clash 出口 uid
 * （`{source:'clash', listenerUid}`）——「自定义代理…」不是值而是动作：把下拉恢复成
 * 原选中，再打开账号设置弹窗（完整代理表单在那里）。成功后用后端回报的变更说明播报，
 * 与后端文案保持一份事实。
 */
/**
 * 代理列选中即保存（`value` 是下拉的值）。下拉的选项只有三类：
 *   ''                     → 直连（proxy = null）
 *   `pool:<proxyId>`       → 引用「网络代理」页的池条目
 *   两个 `__proxy_*__` 占位值 → 「当前值的显示项」与「打开设置弹窗」，
 *                             都不是一次修改（在前两个分支里拦掉）
 *
 * 出口统一走代理池之后，这里**不再**产生 `{source:'clash'}` —— 存量里那种
 * 记录仍照原样转发，用户在这个下拉里选一条池条目或切回直连就会改写它。
 */
export async function applyProxyPick(id: string, value: string, fallback: string): Promise<void> {
  if (value === PROXY_CUSTOM_EDIT) {
    // 动作项：开弹窗（下拉的原值由调用方在渲染层恢复 —— 它是受控的，重绘即回原值）
    openSettingsDialog(id)
    return
  }
  // 当前自定义配置的显示项：选它自己不是一次修改
  if (value === PROXY_CUSTOM_CURRENT) return
  if (value === fallback) return
  try {
    const proxy = value === ''
      ? null
      : { source: 'pool', proxyId: value.slice(POOL_VALUE_PREFIX.length) }
    const result = await shared().workbuddyDesktop?.updateAccount?.(id, { proxy })
    const changes = result?.changes
    const change = Array.isArray(changes) && changes.length ? changes[0] : '代理已更新'
    toast(`✅ ${change}`)
  } catch (error) {
    toast(`保存失败：${errorMessage(error)}`, 'err')
  } finally {
    // 成败都重拉：成功让这格显示落库后的值，失败把下拉拨回原值
    await shared().wbApp?.refresh?.()
  }
}

/** 代理下拉里两个「不是出口 uid」的特殊值：代表当前自定义配置的显示项、打开设置弹窗的动作项 */
export const PROXY_CUSTOM_CURRENT = '__proxy_custom__'
export const PROXY_CUSTOM_EDIT = '__proxy_custom_edit__'

/**
 * 行上「领套餐」：整条流程（探测 → 确认 → 验证码 → 领取）在 ui/zcode-claim.js，
 * 这里负责发起与**收尾**。
 *
 * ── 为什么要把「今天领过的套餐」传给脚本 ──────────────────────
 * 一个账号可能同时挂着几份可领套餐，上游的「已领取过」是**按套餐**判的
 * （见 `claimedPlanIdsToday`）。台账在本页（账号记录的 `claimPlans`），
 * 判定规则在域层，脚本只负责把它们画进弹窗、并只让选还没领的那几份 ——
 * 日界的算法因此仍然只有域层一处。
 *
 * ── 收尾为什么在本文件而不是那支 legacy 脚本里 ────────────────
 * 领到的是 token 额度：余额列的读数立刻就变了，而那颗按钮的悬停提示也跟着变
 * （后端落的领取台账由重拉账号拿到）。两件事都是本页的 store / 动作
 * （legacy 脚本拿不到），所以脚本只把结果交回来，由这里刷新 ——
 * 与 CodeArts 那条（脚本自己调 `wbApp.refresh()`）不同，是因为这条还得顺带刷余额。
 *
 * `already_claimed` 同样算「已领」：上游说这份套餐已经被领掉了（可能是另一台
 * 设备领的），台账该补上它，否则用户会一直点它、每次拿回同一句话。
 */
export async function startZcodeClaim(id: string): Promise<void> {
  const account = findAccount(id) || undefined
  const result = (await shared().wbZcodeClaim?.start?.(
    account,
    claimedPlanIdsToday(account),
  )) as { ok?: boolean; failure?: string } | undefined
  const settled = result?.ok === true || result?.failure === 'already_claimed'
  if (!settled) return
  // 余额静默刷新（不 await、不播报：领取结果那条 toast 不能被顶掉，
  // 理由见 refreshUsageAfterCheckin）
  void refreshUsageAfterCheckin(id)
  // 重拉账号状态：领取台账是后端落盘的，弹窗与悬停提示据此更新「哪几份已领」
  void shared().wbApp?.refresh?.()
}

/**
 * CodeArts 的「领福利」：整条流程（只读探测 → 用户确认 → 领取 → 等官方回读）
 * 住在 legacy 脚本 `ui/codearts-welfare.js` 里，这里只把账号对象递过去。
 *
 * 与上面那颗「领套餐」是**两件事**（判据位 `welfare` vs `claim`、本家不要验证码、
 * 端点也不同），所以是另一个全局对象而不是 `wbZcodeClaim` 的一个参数。
 * 台账刷新由那侧负责（它领完自己调 `wbApp.refresh()`）。
 */
export async function startCodeArtsWelfare(id: string): Promise<void> {
  await shared().wbCodeArtsWelfare?.start?.(findAccount(id) || undefined)
}

/* ─── 对外契约（window）────────────────────── */

/** 一行账号的渲染上下文（视图层按它取位置、勾选、余额与连接数） */
export type RowContext = {
  seat: { position: number; total: number }
  picked: boolean
  usageEntry: UsageEntry
  limitsOpen: boolean
  connections: number
}

export function rowContext(account: AccountRecord, seatMap: Map<string, { position: number; total: number }>): RowContext {
  return {
    seat: seatMap.get(account.id) || { position: 1, total: 1 },
    picked: isPicked(account.id),
    usageEntry: usageMap.get(account.id),
    limitsOpen: panelOpen(account.id, 'limits'),
    connections: connectionsOf(account.id),
  }
}

export type AccountsViewApi = {
  render(): void
  refreshCaches(validIds: Set<string>): void
  openPanels(ids: string[], kind: PanelKind): void
  syncConnections(): Promise<boolean>
  applyBalances(balances: { results?: Array<Record<string, unknown>> } | null | undefined): number
  syncBalancesSnapshot(): Promise<boolean>
  queryUsageFor(id?: string | null): Promise<unknown>
  queryAllUsage(): Promise<void>
  checkinFor(id?: string | null): Promise<unknown>
  checkinAll(): Promise<void>
  refreshUsageAfterCheckin(id?: string | null): Promise<void>
  checkinableAccounts: typeof checkinableAccounts
  supportsCheckin: typeof supportsCheckin
  supportsUsage: typeof supportsUsage
  isDesktopAccount: typeof isDesktopAccount
  isEnabled: typeof isEnabled
  isRateLimited: typeof isRateLimited
}

export type AccountsPanelApi = {
  open(id: string): void
  close(): void
  openBatch(ids: string[], action?: string): void
  closeBatch(): void
  syncAddProvider(): void
  invalidate(): void
}

/**
 * `window.wbAccountsModel` 的公开面：与原 ui/accounts-model.js 的**方法清单逐字一致**，
 * 只少了五个**生成 HTML 字符串**的成员（statusTag / accountTags / moreMenuHtml /
 * limitPanelHtml / checkinPanelHtml）—— 它们随表格一起变成了 React 组件，页外无任何
 * 引用（见最终报告）。accountTags 的等价物在域层（返回结构化标签，由视图渲染成 Badge）。
 */
type AccountsModelApi = {
  DEFAULT_PROVIDER_ID: string
  RACCOON_PROVIDER_ID: string
  providerOf: typeof domain.providerOf
  providerFeatures: typeof domain.providerFeatures
  providerSummaries: typeof domain.providerSummaries
  identifierOf: typeof domain.identifierOf
  tokenExpiryOf: typeof domain.tokenExpiryOf
  supportsUsage: typeof domain.supportsUsage
  isDesktopAccount: typeof domain.isDesktopAccount
  supportsChat: typeof domain.supportsChat
  byPriorityOrder: typeof domain.byPriorityOrder
  typeLabel: typeof domain.typeLabel
  isEnabled: typeof domain.isEnabled
  isRateLimited: typeof domain.isRateLimited
  accountEdition: typeof domain.accountEdition
  supportsCheckin: typeof domain.supportsCheckin
  supportsClaim: typeof domain.supportsClaim
  checkedInToday: typeof domain.checkedInToday
  checkinableAccounts: typeof domain.checkinableAccounts
  matchProvider: typeof domain.matchProvider
  matchEnabled: typeof domain.matchEnabled
  matchLimit: typeof domain.matchLimit
  visibleAccounts: typeof domain.visibleAccounts
  filterCounts: typeof domain.filterCounts
  positionMap: typeof domain.positionMap
  activeLimits: typeof domain.activeLimits
  editionSuffix: typeof domain.editionSuffix
  formatResetText: typeof domain.formatResetText
}

declare global {
  interface Window {
    /** 账号列表视图（替换 ui/accounts-view.js；app.js / providers.js / tasks-panel 的调用点逐字保留） */
    wbAccountsView?: AccountsViewApi
    /** 账号的领域判定（替换 ui/accounts-model.js；app.js / report.js / models-fetch-modal 的调用点逐字保留） */
    wbAccountsModel?: AccountsModelApi
    /** 账号设置 / 批量操作弹窗（替换 ui/account-panel.js；app.js:623 / :645 的调用点逐字保留） */
    wbAccountPanel?: AccountsPanelApi
  }
}

/** 域层的对外面（方法清单同上；显式列名而不是 `export *`，免得把内部辅助也挂上去） */
const ACCOUNTS_MODEL_API: AccountsModelApi = {
  DEFAULT_PROVIDER_ID: domain.DEFAULT_PROVIDER_ID,
  RACCOON_PROVIDER_ID: domain.RACCOON_PROVIDER_ID,
  providerOf: domain.providerOf,
  providerFeatures: domain.providerFeatures,
  providerSummaries: domain.providerSummaries,
  identifierOf: domain.identifierOf,
  tokenExpiryOf: domain.tokenExpiryOf,
  supportsUsage: domain.supportsUsage,
  isDesktopAccount: domain.isDesktopAccount,
  supportsChat: domain.supportsChat,
  byPriorityOrder: domain.byPriorityOrder,
  typeLabel: domain.typeLabel,
  isEnabled: domain.isEnabled,
  isRateLimited: domain.isRateLimited,
  accountEdition: domain.accountEdition,
  supportsCheckin: domain.supportsCheckin,
  supportsClaim: domain.supportsClaim,
  checkedInToday: domain.checkedInToday,
  checkinableAccounts: domain.checkinableAccounts,
  matchProvider: domain.matchProvider,
  matchEnabled: domain.matchEnabled,
  matchLimit: domain.matchLimit,
  visibleAccounts: domain.visibleAccounts,
  filterCounts: domain.filterCounts,
  positionMap: domain.positionMap,
  activeLimits: domain.activeLimits,
  editionSuffix: domain.editionSuffix,
  formatResetText: domain.formatResetText,
}

/**
 * 注册对外契约。**必须在模块求值时完成**：app.js 的 refresh() 是异步的（首个 await
 * 之后才轮到渲染），但 report.js / add-account.js 这些同步脚本紧随其后执行 —— 它们
 * 拿不到就会退化成静默不生效。
 */
export function installAccountsApi(): void {
  window.wbAccountsView = {
    render: () => bump(),
    refreshCaches,
    openPanels: openPanelsFor,
    syncConnections,
    applyBalances,
    syncBalancesSnapshot,
    queryUsageFor,
    queryAllUsage,
    checkinFor,
    checkinAll,
    refreshUsageAfterCheckin,
    checkinableAccounts,
    supportsCheckin,
    supportsUsage,
    isDesktopAccount,
    isEnabled,
    isRateLimited,
  }
  window.wbAccountsModel = ACCOUNTS_MODEL_API
  window.wbAccountPanel = {
    open: openSettingsDialog,
    close: closeDialog,
    openBatch: openBatchDialog,
    closeBatch: closeDialog,
    /** 「添加账号」弹窗打开时可用：按 providers 摘要重建选项并复位到 WorkBuddy */
    syncAddProvider: () => shared().wbAccountAddForms?.syncAddProvider?.(),
    /** 账号列表刷新后调用：Clash 端口可能已在 Clash 侧改过 */
    invalidate: invalidateClashCache,
  }
}
