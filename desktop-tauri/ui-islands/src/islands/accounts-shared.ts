/**
 * 账号页各文件共用的**类型与全局桥读取**（非岛：`.ts` 不被 import.meta.glob 当岛加载）。
 *
 * 账号页按「视图层 .tsx + 数据 / 状态层 .ts」拆成了好几个文件，它们都要读同一批
 * window 上的既有全局（workbuddyDesktop / wbApp / wbColSettings …）。若每个文件各写
 * 一份窄类型，接口合并会因同名属性类型不一致直接报 TS2717 —— 并行迁移时必然互相撞车
 * （见 table-col-settings.tsx 的说明）。所以这里**只此一份**：声明 + 读取函数都在这。
 *
 * 这里只声明本页用到的那些桥，且**不往 Window 上加共享属性**：账号页独占的四个对象
 * （wbAccountsView / wbAccountsModel / wbUsageActions / wbAccountPanel）在
 * accounts-data.ts 里 declare 一次（只有一个文件声明，不会撞车）。
 */

/* ─── 后端账号形态（只列本页读到的字段，其余按 unknown 收）───── */

/** 出网代理配置（与后端 workbuddy-proxy.mjs 的形状一致） */
export type ProxyConfig = {
  /** 旧版平铺形状里的 source；新版在 config.source 下，两处都读 */
  source?: string
  label?: string
  /** 解析失败的原因：转发时回退直连，界面上整格标红 */
  error?: string
  config?: {
    source?: string
    listenerUid?: string
    protocol?: string
    host?: string
    port?: number
    username?: string
    password?: string
  } | null
}

/** 按模型记的限流记录（`rateLimits[model]`） */
export type RateLimitInfo = {
  status?: number | string
  code?: string | number
  resetAt?: number
  message?: string
}

/**
 * 账号记录。后端公开形态字段很多、且随 provider 不同（identifier / expiry 的键名
 * 由能力表决定），所以留一条索引签名兜底 —— 能力表指向的字段（uid / userId / account）
 * 只能按动态键读。
 */
export type AccountRecord = {
  id: string
  provider?: string
  name?: string
  nickname?: string
  email?: string
  priority?: number
  enabled?: boolean
  /** 桌面端实时登录态（凭证每次从客户端登录态文件读取） */
  desktop?: boolean
  edition?: string
  editionLabel?: string
  chatSupported?: boolean
  canClaim?: boolean
  hasRefreshToken?: boolean
  hasBalanceToken?: boolean
  maxConcurrent?: number
  checkinAt?: number
  addedAt?: number
  updatedAt?: number
  tokenTail?: string
  tokenExpiresAt?: number
  expiresAt?: number
  source?: string
  proxy?: ProxyConfig | null
  rateLimits?: Record<string, RateLimitInfo>
  [key: string]: unknown
}

/** `wbApp.getState()?.accounts` 的形状（providers 摘要 + 账号清单） */
export type AccountsSnapshot = {
  accounts?: AccountRecord[]
  providers?: Array<{ id?: string; label?: string; count?: number }>
}

/** 余额缓存的四种形态：undefined 未查 / null 查询中 / string 失败 / 对象结果 */
export type UsageEntry = null | string | Record<string, unknown> | undefined

/** 列的对齐档（与 table-col-settings 的 Align 同一套取值） */
export type Align = 'left' | 'center' | 'right'

/**
 * 行内明细面板的 kind。**只剩「限流明细」一种**：签到曾经也有一个明细面板，
 * 已按要求删除 —— 签到的结果现在只落在行上那颗按钮（状态 + title 里的失败原因）
 * 与一条 toast 上，见 accounts-data.ts 的 `checkinErrors`。
 */
export type PanelKind = 'limits'

/* ─── 全局桥 ─────────────────────────────────── */

type ConfirmOptions = {
  title?: string
  /** 正文，允许 <strong> 等少量标记；内容由调用方负责转义 */
  html?: string
  okText?: string
  /** danger = 不可恢复的危险操作（确认键走红） */
  okClass?: string
  bodyClass?: string
}

export type AccountsBridge = {
  getAccountConnections(): Promise<{ counts?: Record<string, unknown> } | null | undefined>
  updateAccount(id: string, patch: Record<string, unknown>): Promise<{ changes?: string[] } | null | undefined>
  moveAccount(id: string, direction: 'up' | 'down'): Promise<unknown>
  clearRateLimits(id: string, model?: string | null): Promise<unknown>
  batchAccounts(payload: { action: string; ids: string[]; proxy?: unknown }): Promise<{
    ok?: Array<{ id?: string; changes?: string[] }>
    removed?: unknown[]
    failed?: Array<{ id?: string; error?: string }>
  } | null | undefined>
  getAllBalances(id?: string): Promise<{ results?: Array<Record<string, unknown>> } | null | undefined>
  getBalancesSnapshot(): Promise<{ at?: number; results?: Array<Record<string, unknown>> } | null | undefined>
  checkinAllAccounts(id?: string | null): Promise<{
    results?: Array<Record<string, unknown>>
    succeeded?: number
    active?: number
    total?: number
    skipped?: number
  } | null | undefined>
  getProxies(): Promise<{ clash?: ClashSnapshot } | null | undefined>
  testProxy(payload: { proxy: unknown }): Promise<{
    success?: boolean
    status?: unknown
    ip?: string
    durationMs?: unknown
    error?: string
  } | null | undefined>
  switchAccount(id: string): Promise<{ changed?: boolean } | null | undefined>
  refreshAccountToken(id: string): Promise<unknown>
  removeAccount(id: string): Promise<unknown>
}

/** `/api/proxies` 里 clash 那一段（出口列表 + 可用性） */
export type ClashSnapshot = {
  available?: boolean
  error?: string
  dir?: string
  options?: Array<{
    uid?: string
    name?: string
    port?: number
    profileActive?: boolean
    enabled?: boolean
  }>
}

/**
 * window 上由别的脚本 / 别的岛挂载的共享桥。**只声明本页用到的成员**，
 * 且用「局部窄类型 + 转型」读，不 declare global（见文件头）。
 */
export type SharedWindow = {
  workbuddyDesktop?: AccountsBridge
  wbApp?: {
    esc?: (value: unknown) => string
    toast?: (message: string, kind?: 'err' | 'ok') => void
    formatTime?: (value: unknown) => string
    showPage?: (page: string) => void
    refresh?: () => Promise<unknown> | unknown
    runAccountAction?: (action: string, id: string) => void
    getState?: () => { accounts?: AccountsSnapshot } | null | undefined
    readonly currentPage?: string
  }
  wbConfirm?: { ask?: (options: ConfirmOptions) => Promise<boolean> }
  wbProviders?: {
    labelOf?: (id: string) => string
    all?: () => Array<{ id?: string; label?: string }>
    customList?: () => Array<{ id?: string; name?: string; protocol?: string; baseUrl?: string }>
    customRequest?: (method: string, path: string, body?: unknown) => Promise<unknown>
    PROTOCOL_OPTIONS?: Array<{ value: string; label: string }>
  }
  wbFilterMemory?: {
    load<T extends Record<string, string>>(key: string, defaults: T): T
    save(key: string, patch: Record<string, string>): void
  }
  wbIcons?: { icon?: (name: string, size?: number) => string }
  wbColSettings?: {
    register(spec: ColSettingsSpec): ColSettingsHandle
    apply<C extends { key: string }>(id: string, columns: C[]): Array<C & { align: Align }>
    configOf(id: string): Array<{ key: string; visible: boolean; align: Align }> | null
    syncStaticHead(id: string, table: Element | null | undefined, viewHidden?: ReadonlySet<string> | null): void
    close(): void
  }
  /** 自定义提供商目录（查一家 / 改一家 / 删一家），账号设置弹窗里的「提供商」一段用它 */
  wbCustomProvidersUi?: {
    find?: (id: string) => Promise<{ id: string; name?: string; protocol?: string; baseUrl?: string } | null | undefined>
    update?: (patch: { id: string; name: string; protocol: string; baseUrl: string }) => Promise<unknown>
    remove?: (id: string) => Promise<boolean>
  }
  /** 并发上限小对话框（已迁的岛，见 conc-dialog.tsx） */
  wbAccountConcDialog?: { open?: (account: AccountRecord) => void; close?: () => void }
  /** ZCode「领套餐」流程（ui/zcode-claim.js，本页只把账号对象递过去） */
  wbZcodeClaim?: { start?: (account: AccountRecord | undefined) => Promise<unknown> }
  /** CodeArts「领福利」流程（ui/codearts-welfare.js：只读探测 → 确认 → 领取 → 回读） */
  wbCodeArtsWelfare?: { start?: (account: AccountRecord | undefined) => Promise<unknown> }
  /** 「添加账号」弹窗（归另一个代理，本页只调它的 open） */
  wbAddAccountModal?: { open?: () => void; close?: () => void }
  /** 添加表单的步骤复位（打开弹窗后按 providers 摘要重画卡片） */
  wbAccountAddForms?: { syncAddProvider?: () => void }
}

/** 列设置的登记项（照 table-col-settings.tsx 的 TableSpec 收窄成本页用到的那几个字段） */
export type ColSettingsSpec = {
  id: string
  label?: string
  columns: Array<{ key: string; label: string; align: Align; legacyAlign?: Align }>
  mount?: () => Element | null
  onChange?: () => void
}

export type ColSettingsHandle = {
  apply<C extends { key: string }>(columns: C[]): Array<C & { align: Align }>
  config(): Array<{ key: string; visible: boolean; align: Align }>
}

export function shared(): SharedWindow {
  return window as unknown as SharedWindow
}

/* ─── 小工具（跨文件共用的三件：播报 / 转义 / 时间）───── */

export function toast(message: string, kind?: 'err' | 'ok'): void {
  shared().wbApp?.toast?.(message, kind)
}

/**
 * HTML 转义：跨文件一律走 wbApp（app.js 里的全局单份实现），只有它还没就绪时才自己
 * 转一遍 —— wbConfirm.ask 收的是 HTML 片段，插值漏出去就是注入。
 */
export function esc(value: unknown): string {
  const fn = shared().wbApp?.esc
  if (fn) return fn(value)
  return String(value ?? '').replace(/[&<>"']/g, char => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char] || char
  ))
}

/** 时间戳 → 本地时间串（0 与非法值返回空串，调用处据此省略那半句提示） */
export function formatTime(value: unknown): string {
  const fn = shared().wbApp?.formatTime
  if (fn) return fn(value)
  const time = Number(value)
  if (!Number.isFinite(time) || time <= 0) return ''
  return new Date(time).toLocaleString('zh-CN')
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
