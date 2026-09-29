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
    /** `source === 'pool'` 时的池条目 id（见 PoolItem） */
    proxyId?: string
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
  /** ZCode：这一行当前走哪条上游通道（`coding-plan` / `start-plan`，见 accounts-domain 的 ZCODE_PLAN_*） */
  zcodePlan?: string
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
  /** 代理池列表（「网络代理」页维护的命名代理）：账号代理表单的
   *  「已保存的代理」下拉读它；写侧（增删改）只有那一页用，不在这份桥里 */
  getProxyPool(): Promise<{ items?: PoolItem[] } | null | undefined>
  /** 测一条池条目（账号表单里选中池引用时的「测试出口」）：
   *  结果会记进那条代理的「上次测试」，与代理页那颗按钮同一个动作 */
  testProxyPoolItem(id: string): Promise<{
    success?: boolean
    ip?: string
    durationMs?: unknown
    error?: string
  } | null | undefined>
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
  /**
   * ZCode 活动套餐通道的**验证码令牌池**概况（见后端 `providers/zcode/captcha.rs`）。
   *
   * 活动套餐的推理端点每条请求都要一个当次铸的阿里云验证码令牌，令牌由界面后台
   * 静默铸造（`ui/zcode-captcha-pool.js`）。这里读的是「当前库存 / 目标 / 是否
   * 正在缺货」—— 账号设置里选活动套餐时用它给用户一个可查的状态，
   * 否则「转发失败但不知道为什么」只能靠日志。
   */
  zcodeCaptchaStats?(): Promise<{
    ready?: number
    target?: number
    startPlanAccounts?: number
    rejected?: number
    minted?: number
    consumed?: number
    ttlMs?: number
  } | null | undefined>
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
 * 「网络代理」页（代理池）的一条条目 —— 只列账号表单用到的字段，
 * 完整形态见 ui-islands/src/islands/proxies-page.tsx 的 PoolItem。
 * 账号的 proxy 可以按 id 引用它（`{source:'pool', proxyId}`）。
 */
export type PoolItem = {
  id: string
  name?: string
  /** manual | clash（表单里按它标「手动 / Clash Verge」） */
  source?: string
  enabled?: boolean
  /** 解析后的出口（Clash 条目的端口在这里是实时值）；不受控字段按 unknown 收 */
  resolved?: { protocol?: string; host?: string; port?: number | null; label?: string } | null
  /** 解析失败的原因（引用了已删除的 Clash 监听器等） */
  resolveError?: string | null
  /** 引用它的账号（后端在池列表里带的） */
  usedBy?: Array<{ id?: string; name?: string; enabled?: boolean }>
}

/**
 * 池条目的地址串：`HTTP 127.0.0.1:7910`（协议大写 + 主机 + 端口）。
 *
 * 与 OmniProxy 的名称列第二行同一口径（它的 ProxiesPage 就在名字下面写
 * `${protocol.toUpperCase()} ${host}:${port}`）。用解析后的 host/port 而不是
 * `resolved.label`：label 对 Clash 条目是「监控器名（:端口）」、对混合端口是
 * 「Clash 混合端口 7892」—— 都不含主机，且形态随来源变。这里统一成同一串，
 * 用户在下拉里能直接核对「这条连的是哪儿」。
 *
 * 取不到（解析失败 / 字段缺失）时回落 `resolved.label`，再没有就是空串。
 */
export function poolItemAddress(item: PoolItem): string {
  const resolved = item.resolved
  if (resolved?.protocol && resolved.host && resolved.port) {
    return `${String(resolved.protocol).toUpperCase()} ${resolved.host}:${resolved.port}`
  }
  return resolved?.label || ''
}

/**
 * 池条目在下拉里的展示名：`香港HK-A（HTTP 127.0.0.1:7909）` /
 * `香港HK-A（HTTP 127.0.0.1:7909 · 已禁用）` / `香港HK-A（解析失败）`。
 *
 * 三个地方共用（账号表代理列 / 账号弹窗的代理表单 / 更新设置的出网线路）——
 * 各写一份迟早会漂移成本次这种事（有一处忘了写地址，用户下拉里只能看到名字）。
 * 条目的来源（手动 / Clash）不在这里标：下拉里已经有地址可比对，来源在
 * 「网络代理」页的类型列上。
 */
export function poolItemLabel(item: PoolItem): string {
  const name = item.name || item.id
  const address = item.resolveError ? '解析失败' : poolItemAddress(item)
  const parts = [address, item.enabled === false ? '已禁用' : ''].filter(Boolean)
  return parts.length ? `${name}（${parts.join(' · ')}）` : name
}

/**
 * 代理下拉里「池条目」这一类的值前缀：`pool:<proxyId>`。
 *
 * 为什么需要前缀而不是直接用 proxyId：同一个下拉里还列着 Clash 的出口 uid
 * （来自 verge.yaml，形态不受我们控制，`px_...` 撞上它不是不可能），
 * 以及几个 `__proxy_*__` 占位值。前缀把三类值域彻底分开，判定只看开头即可。
 * 两个消费方（账号页代理列 / 更新设置的出网线路）读的是同一个常量 ——
 * 一边改了前缀、另一边没改，症状是「选了代理池条目却报不支持」，很难查。
 */
export const POOL_VALUE_PREFIX = 'pool:'

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
  /**
   * ZCode「领套餐」流程（ui/zcode-claim.js，本页把账号对象与「今天领过的套餐 id」
   * 递过去）。第二个参数是**逐份**的领取状态：一个账号可能同时挂着几份可领套餐，
   * 而上游的「已领取过」是按套餐判的 —— 弹窗据此把已领的那几份标出来、只让选没领的。
   */
  wbZcodeClaim?: { start?: (account: AccountRecord | undefined, claimedPlanIds?: string[]) => Promise<unknown> }
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
