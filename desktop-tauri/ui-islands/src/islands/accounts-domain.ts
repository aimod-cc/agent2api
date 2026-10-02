/**
 * 账号页的**纯逻辑层**：provider 能力表、基础判定、筛选口径、全局队列位置、限流文案。
 *
 * 替换 ui/accounts-groups.js + ui/accounts-model.js 里的纯逻辑部分（那两个文件里的
 * 「标签 / 面板 HTML」字符串生成器随表格一起变成 React 组件，见 accounts-page.tsx）。
 * 本文件不碰 DOM、不读写模块状态，只依赖 window.wbProviders 的 label（运行期读）。
 *
 * ── 对外契约（必须原样保留的调用点）────────────────────────────
 *   · app.js:178  `wbAccountsModel.isRateLimited`（顶栏「已限流」计数）
 *   · app.js:663  `wbAccountsModel.isDesktopAccount`（删除确认框的补充说明）
 *   · report.js:307 `wbAccountsModel.editionSuffix`（报表里账号名后的版本后缀）
 *   · models-fetch-modal.tsx:245 `wbAccountsModel.providerFeatures(...).emailAsName`
 *   · models-fetch-modal.tsx:255 `wbAccountsModel.byPriorityOrder`
 * 其余成员只被账号页自己用（`wbAccountsTable` / `wbAccountsColumns` /
 * `wbAccountsFilters` / `wbAccountsGroups` 四个对象在页外无任何引用，已随本页合并
 * 进岛里不再挂 window —— 见最终报告）。
 *
 * ── 全局一条队列（优先级不再按 provider 分段）────────────────────
 * 优先级在后端是**全局唯一**的一条队列：四家账号混排，转发时按优先级从小到大逐个
 * 尝试，跳过禁用 / 不支持该模型 / 该模型限流中的账号（见 priority.rs 与 rotate.rs）。
 * 所以筛选与计数都按这一条队列算，positionMap 的序号就是整张表的行序。
 */

import { shared, formatTime, type AccountRecord, type AccountsSnapshot, type RateLimitInfo } from './accounts-shared'

/** 缺省 provider id（后端注册表的默认项；旧账号记录没有该字段时的兜底） */
export const DEFAULT_PROVIDER_ID = 'workbuddy'
/** 小浣熊 provider id（只有它需要「桌面端实时登录态」这类专属标记） */
export const RACCOON_PROVIDER_ID = 'raccoon'

type ProviderFeatures = {
  /** 这一家有没有余额 / 积分查询概念 */
  usage: boolean
  /** 这一家有没有签到活动 */
  checkin: boolean
  /** 有没有国内 / 国际版概念（决定提供商徽章是否拼版本后缀、有效期读哪个字段） */
  edition: boolean
  /** 账号标识落在记录里的哪个键（uid / userId / account） */
  identifier: string
  /** 有效期落在记录里的哪个键（expiresAt / tokenExpiresAt） */
  expiry: string
  /** 「这家的账号就该以邮箱报名字」（Qoder / AutoClaw 国际版），见 accountCell 的说明 */
  emailAsName?: boolean
  /** 有没有「领体验套餐」这个动作（只有 ZCode 两家） */
  claim?: boolean
  /**
   * 有没有「使用哪个套餐（上游通道）」这个设置（只有 ZCode 两家）。
   *
   * 它回答的是「这个账号的请求走哪条上游通道」：编码套餐（自己买的订阅，
   * 开放平台的 OpenAI 端点）还是活动套餐（官方限时发放的额度，`zcode.z.ai`
   * 的 Anthropic 端点）。两者是**两份独立额度**，而「套餐已到期」这类拒绝
   * 只由其中一条给出 —— 所以它是设置项而不是自动探测（见后端
   * `providers::zcode::plan` 的模块头）。
   */
  planChannel?: boolean
  /**
   * 有没有「领福利」这个动作（只有 CodeArts）。**刻意不与 ZCode 的 `claim` 合并**：
   * 那家的领取要过一次阿里云验证码、且判据看后端给的 `canClaim`（账号得带套餐令牌），
   * 本家两样都没有 —— 共用一个位会让两边的按钮判据互相污染。
   */
  welfare?: boolean
  /**
   * 这一家「并发上限」的默认值（>0 = 本家**没有**「不限」这一档）。
   * CodeArts 的 3 是上游硬顶（超过直接回 HTTP 400，且那是账号级冲突、不降级换号），
   * 所以那一家把 `0` 解释成「按默认 3」而不是「不做并发过滤」；后端同一口径写在两处
   * （公开形态把没配过的报成 3、准入闸按 3 判），界面上说了就得对上。
   */
  concurrencyDefault?: number
}

/**
 * provider 能力表：决定行上出现哪些按钮、哪行明细显示什么。
 *
 * 为什么是「按 provider 查表」而不是在渲染处写 if：账号页的每个分支（余额按钮、
 * 签到按钮、版本后缀、标识字段名）都要问同一个问题 ——「这家有没有这个概念」。
 * 散在各处写 if 的话，加一家就要翻一遍全文件，漏掉一处不报错、只静默少一个按钮。
 *
 * usage 各家都是 true（余额查询已扩到全部提供商），各由自己的适配器实现；前端只回答
 * 「这一家有没有这个概念」。CatPaw 的余额接口要单独配一个网页会话凭证（token2），
 * 没配置时后端返回可识别的「未配置」、余额列显示成中性提示 —— 所以它的按钮照样渲染，
 * 用户才有「去配置」的入口。
 */
const PROVIDER_FEATURES: Record<string, ProviderFeatures> = {
  workbuddy: { usage: true, checkin: true, edition: true, identifier: 'uid', expiry: 'expiresAt' },
  raccoon: { usage: true, checkin: true, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt' },
  catpaw: { usage: true, checkin: false, edition: false, identifier: 'uid', expiry: 'tokenExpiresAt' },
  // AutoClaw 两个地区能力完全一致，差别只在域名；两项都必须登记 —— 漏了哪一项，
  // 那一家就会掉进 GENERIC_FEATURES（症状：余额按钮消失、标识列显示成空）
  autoclaw: { usage: true, checkin: true, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt' },
  'autoclaw-intl': { usage: true, checkin: true, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt', emailAsName: true },
  // Qoder 的签到**只有中国版有**（国际版这个地区没有签到计划，见 providers::qoder::checkin）。
  // 能力位照样写 true —— 国际版账号由 supportsCheckin 的第二道判据（edition !== 'intl'，
  // Qoder 的公开形态带该字段）单独排除，明细面板给出「国际版暂无签到活动」的说明；
  // 中国版里没有被下发活动的账号（Free 套餐实测如此）会在点签到后得到一条中性提示。
  qoder: { usage: true, checkin: true, edition: true, identifier: 'userId', expiry: 'expiresAt', emailAsName: true },
  // Cline 两条键：同一家上游按计费通道拆成两个 provider，账号形态完全一样（见
  // providers::cline::models）。查表按 id 精确匹配，只登记一个会让另一家掉进兜底
  'cline-free': { usage: true, checkin: false, edition: false, identifier: 'account', expiry: 'expiresAt' },
  'cline-pass': { usage: true, checkin: false, edition: false, identifier: 'account', expiry: 'expiresAt' },
  // Accio 两个地区：额度可查（上游只给用量百分比）、没有签到、有地区概念
  accio: { usage: true, checkin: false, edition: true, identifier: 'userId', expiry: 'expiresAt', emailAsName: true },
  'accio-cn': { usage: true, checkin: false, edition: true, identifier: 'userId', expiry: 'expiresAt', emailAsName: true },
  // ZCode 两个地区：**没有签到**，替代它的是「限时套餐领取」（claim 位）。
  // `usage: true` 对应 providers::zcode::balance —— 余额读的是 billing 网关的
  //   `/zcode-plan/billing/balance`，认**套餐 JWT**（与转发用的 accessToken 不是
  //   一套凭证）。账号只粘了 accessToken 时后端回可识别的「未配置」，余额列显示成
  //   中性提示而不是一片红，所以这颗按钮照样渲染。
  // `claim: true` 就是那颗「领套餐」：2026-09-28 起那期（ZCode Trust Build）是
  //   **每天一份新套餐**（plan_id 带日期段），领过之后按钮当天显示「今日已领」、
  //   次日自动恢复 —— 见 `claimedToday`。
  // expiry 取 expiresAt 是给 add_zcode_account 的契约（落账号时要写访问令牌的过期时间）
  zcode: { usage: true, checkin: false, claim: true, planChannel: true, edition: true, identifier: 'userId', expiry: 'expiresAt' },
  'zcode-intl': { usage: true, checkin: false, claim: true, planChannel: true, edition: true, identifier: 'userId', expiry: 'expiresAt' },
  // CodeArts（华为云 AI 代码助手）。各位各有出处，别照着别家抄：
  // `usage: true` —— 余额是**两份账**（订阅统计 + 福利网关，见 providers::codearts::balance），
  //   界面上「读到 0」与「没读到」必须能分开，后端因此把失败的一侧写进 statisticsError /
  //   benefitError 而不是整次失败（半次失败的呈现见 accounts-panels 的 usageSummary）。
  // `welfare: true` —— 本家没有「每日签到」，对应物是 ops 福利领取（探测 → 确认 →
  //   领取 → 回读二次确认），是用户点一下才走的独立按钮。
  // `edition: false` —— 没有版本/地区概念：region 固定在 cn-north-4 且必须与 token
  //   签发地一致，不是用户可选项；`login_type`（WEB/IDE）也不是版本，别塞进这一列。
  // `expiry: 'expiresAt'` —— 临时凭据约一小时到期，这一列对本家**是主要信息**。
  codearts: {
    usage: true, checkin: false, welfare: true, edition: false,
    identifier: 'userId', expiry: 'expiresAt',
    concurrencyDefault: 3,
  },
  // Trae（只有 SOLO 那一家）：`edition: false` 是事实 —— 国内 SOLO 与国际版是**两套协议**
  // 而不是一个地区的两种拼法，国际版将来接入时另立 provider id，不把它做成账号字段。
  // `usage: true` 对应 providers::trae::usage（上游两份账：ide_user_ent_usage 的权益包/
  // 积分池 + ide_user_pay_status 的快请求与 SOLO 并发）。
  // `checkin: false` 同样不是省事：签到那条链在参考实现里有把出口 IP 打进封禁的前科，
  // 且它记的「签到钱包」与模型调用真正扣的积分池是两笔钱 —— 不给按钮，免得给一个
  // 点了必然报错（或报出一个对不上官方数字的余额）的入口。
  trae: { usage: true, checkin: false, edition: false, identifier: 'uid', expiry: 'expiresAt' },
}

/**
 * 未登记 provider 的兜底能力：不显示余额 / 签到 / 版本 —— 这三个都是 provider 私有
 * 概念，未知的家不该被假定拥有。标识字段假定成 userId，取不到时明细行自动少一项。
 */
const GENERIC_FEATURES: ProviderFeatures = {
  usage: false, checkin: false, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt',
  emailAsName: false,
}

/** 账号所属 provider（字段缺失 / 非字符串按默认 provider 兜底，与后端 store 口径一致） */
export function providerOf(account: AccountRecord | null | undefined): string {
  const id = account?.provider
  return typeof id === 'string' && id ? id : DEFAULT_PROVIDER_ID
}

/** provider id → 能力表（未登记的家走 GENERIC_FEATURES） */
export function providerFeatures(providerId: string | undefined): ProviderFeatures {
  return (providerId && PROVIDER_FEATURES[providerId]) || GENERIC_FEATURES
}

/**
 * providers 摘要归一化：`{ providers: [{id,label,count}] }`。
 *
 * 两处兜底是刻意的（摘要缺一项就让整页空白，代价远大于一个小偏差）：
 *   · 后端没给摘要（旧版 / 首屏 state 尚未到达）→ 按现有账号派生；
 *   · 摘要里没有、但账号里出现的 provider → 补在末尾，计数按现有账号算。
 * label 优先问共享的 wbProviders 目录，都取不到时退化成 id 本身。
 */
export function providerSummaries(snapshot: AccountsSnapshot | null | undefined): Array<{ id: string; label: string; count: number }> {
  const list = Array.isArray(snapshot?.providers) ? snapshot.providers : []
  const accounts = Array.isArray(snapshot?.accounts) ? snapshot.accounts : []
  const counts = new Map<string, number>()
  accounts.forEach(account => {
    const id = providerOf(account)
    counts.set(id, (counts.get(id) || 0) + 1)
  })
  const known = new Map<string, { id: string; label: string; count: number }>()
  list.forEach(item => {
    if (!item?.id) return
    known.set(String(item.id), {
      id: String(item.id),
      label: String(item.label || item.id),
      count: Number(item.count) || 0,
    })
  })
  counts.forEach((count, id) => {
    if (known.has(id)) return
    known.set(id, { id, label: shared().wbProviders?.labelOf?.(id) || id, count })
  })
  if (!known.size) known.set(DEFAULT_PROVIDER_ID, { id: DEFAULT_PROVIDER_ID, label: 'WorkBuddy', count: 0 })
  return [...known.values()]
}

/** 账号标识（workbuddy 是 uid，小浣熊是 userId）；取不到返回空串 */
export function identifierOf(account: AccountRecord | null | undefined): string {
  const key = providerFeatures(providerOf(account)).identifier
  return String(account?.[key] || '')
}

/**
 * 到期时刻**归一到毫秒**：库里落盘的单位在秒与毫秒之间漂过 —— CPA 的 auth 文件与
 * 手工粘贴进来的是 10 位秒，别家与上游刷新响应都是 13 位毫秒。不归一的后果不是
 * "少三位精度"而是整条账号被判死：`1791009732`（秒）当毫秒读是 1970-01-21，
 * 有效期那一列直接显示「已过期」，而它的令牌其实还有几天。
 *
 * 1e11 这个分界与后端 `providers::trae::Credential::expires_at_ms` **同一个口径**
 * （1e11 秒 ≈ 公元 5138 年，真正的毫秒时间戳不可能小于它），两处注释互相指着对方，
 * 改数值时要一起改。0 与负数原样返回：那代表"记录里没给到期时刻"，不能捏成
 * 一个 1970 年的时间戳。
 */
export function expiryMillis(value: unknown): number {
  const raw = Number(value) || 0
  if (raw <= 0) return 0
  return raw < 1e11 ? raw * 1000 : raw
}

/** token 过期时间戳（毫秒，0 表示记录里没有这个字段） */
export function tokenExpiryOf(account: AccountRecord | null | undefined): number {
  const key = providerFeatures(providerOf(account)).expiry
  return expiryMillis(account?.[key])
}

/** 该账号所属 provider 是否有余额概念（没有就不渲染余额按钮，也不参与批量查询） */
export function supportsUsage(account: AccountRecord | null | undefined): boolean {
  return providerFeatures(providerOf(account)).usage
}

/** 是否为「桌面端实时登录态」账号（凭证实时读客户端文件；可禁用、也可删除） */
export function isDesktopAccount(account: AccountRecord | null | undefined): boolean {
  return account?.desktop === true
}

/**
 * 该账号所属的 provider 是否能承接推理转发（后端公开形态的 `chatSupported`）。
 * 字段缺失（旧版后端）按「能转发」处理：宁可让界面显示一个正常账号，也不要因为
 * 少了一个字段就把所有账号标成「仅账号管理」。
 */
export function supportsChat(account: AccountRecord | null | undefined): boolean {
  return account?.chatSupported !== false
}

export function typeLabel(type: string | undefined): string {
  if (type === 'enterprise') return '企业'
  if (type === 'ultimate') return '旗舰'
  return '个人'
}

/**
 * 转发顺序排序键：优先级升序，并列时按加入时间。
 * 与后端 workbuddy-account-store.mjs 的 byPriorityOrder 保持一致（渲染层无法 import
 * 后端 ESM，只能同构实现；改一处必须同步另一处）。优先级在写入侧强制唯一，
 * 并列只会出现在手工编辑的账号文件里。
 */
export function byPriorityOrder(a: AccountRecord, b: AccountRecord): number {
  const diff = Number(a?.priority ?? 100) - Number(b?.priority ?? 100)
  if (diff !== 0) return diff
  return (Number(a?.addedAt) || 0) - (Number(b?.addedAt) || 0)
}

/** 账号是否启用（禁用账号不参与转发） */
export function isEnabled(account: AccountRecord | null | undefined): boolean {
  return account?.enabled !== false
}

/**
 * 账号是否处于限流状态（存在未到恢复时间的限额记录）。
 * 传入 model 时只判定该模型 —— 限额是按模型记的，一个账号可能对 A 模型限额、
 * 对 B 模型完全正常。
 */
export function isRateLimited(account: AccountRecord | null | undefined, model = ''): boolean {
  const limits = account?.rateLimits || {}
  const now = Date.now()
  if (model) return Number(limits[model]?.resetAt) > now
  return Object.values(limits).some(info => Number(info?.resetAt) > now)
}

/** 账号所属版本：cn=国内 / intl=国际（缺省视为国内，兼容旧账号记录） */
export function accountEdition(account: AccountRecord | null | undefined): 'cn' | 'intl' {
  return account?.edition === 'intl' ? 'intl' : 'cn'
}

/**
 * 该账号是否参与签到：所属家**有签到活动**，且不是国际版。
 *
 * 版本限定对 WorkBuddy 与 Qoder 两家实际生效 —— 它们的签到活动只在国内站
 * （Qoder 国际版这个地区根本没有签到计划）。判断按「非 intl」写而不是逐个
 * provider 特判：另几家没有 edition 字段，accountEdition 会把缺省值归一成 cn，
 * 因此这个条件对它们是恒真的。
 *
 * 与后端同源同口径：`billing::checkin::supports_checkin` 也是这条判据，
 * 两处任一改动都要同时改（批量签到的目标集合由后端算，前端这处只决定按钮）。
 */
export function supportsCheckin(account: AccountRecord | null | undefined): boolean {
  if (!providerFeatures(providerOf(account)).checkin) return false
  return accountEdition(account) !== 'intl'
}

/**
 * 这个账号能不能「领取体验套餐」（ZCode 独有的动作）。两道判据缺一不可：
 *   ① 能力位（claim）—— 只有 ZCode 那两家登记了它；
 *   ② `canClaim` —— 后端公开形态给的字段，表示这个账号确实带着套餐令牌（jwt）。
 * 只填了 accessToken 的账号没有 jwt，界面上就不该给一个点了必然 400 的按钮。
 * `canClaim` 缺省按 true：取不到时宁可让按钮出现、由后端如实报错，
 * 那比「按钮消失且没有任何解释」更容易排查。
 */
export function supportsClaim(account: AccountRecord | null | undefined): boolean {
  if (!providerFeatures(providerOf(account)).claim) return false
  return account?.canClaim !== false
}

/** ZCode 的两条上游通道取值（与后端 `providers::zcode` 的常量逐字一致） */
export const ZCODE_PLAN_CODING = 'coding-plan'
export const ZCODE_PLAN_START = 'start-plan'

/**
 * 这个账号能不能选「使用哪个套餐」（ZCode 独有的设置）。
 *
 * 判据只有能力位：**没有 jwt 也照样给这个设置** —— 那个账号只能选编码套餐，
 * 但用户看得到「有这回事」并在换账号后回来改，比让这个设置凭空消失好。
 * 「没有套餐登录态就别选活动套餐」由对话框里的禁用态说明（见 accounts-dialogs）。
 */
export function supportsPlanChannel(account: AccountRecord | null | undefined): boolean {
  return Boolean(providerFeatures(providerOf(account)).planChannel)
}

/**
 * 该账号当前走哪条通道（非 ZCode 账号返回空串）。
 *
 * 后端公开形态**总是**给这个字段（缺失时它自己就按默认给 `coding-plan`，
 * 见 `to_zcode_public_account`），所以这里只在字段真缺失时兜默认值 ——
 * 两处都兜同一个默认，是为了让「老版本后端 + 新版本界面」也不显示空白。
 */
export function zcodePlanOf(account: AccountRecord | null | undefined): string {
  if (!supportsPlanChannel(account)) return ''
  const raw = String(account?.zcodePlan || '').trim()
  return raw === ZCODE_PLAN_START ? ZCODE_PLAN_START : ZCODE_PLAN_CODING
}

/**
 * 通道的展示名（与后端 `zcode::plan_label` 同一套措辞）。
 *
 * 后端保存成功后会回一句「套餐通道 → 活动套餐（Start Plan）」，两处措辞若
 * 不一致，用户会以为设置里选的与提示里说的不是同一件事。
 */
export function zcodePlanLabel(plan: string | undefined): string {
  return plan === ZCODE_PLAN_START ? '活动套餐（Start Plan）' : '编码套餐（Coding Plan）'
}

/**
 * 今天**已经领过的套餐 id**（北京时间自然日）。
 *
 * 读数来自后端落盘的领取台账 `claimPlans`（`{planId: 毫秒}`，见
 * `AccountStore::mark_zcode_claim`）。**逐份**给状态是必需的：同一个账号可能
 * 同时挂着几份可领套餐（活动大额包 + 每日包），而上游的「已领取过」又是**按套餐**
 * 判的 —— 领了 A 之后 B 照样能领。只给一个「今天领过了」会把整颗按钮按住，
 * 用户就再也领不了剩下那几份。
 *
 * 日界用北京时间，与签到 / 福利同一口径：上游的活动按中国时间换期
 * （那期 Trust Build 的套餐 id 就带日期段，每天换一个）。
 */
export function claimedPlanIdsToday(account: AccountRecord | null | undefined): string[] {
  const ledger = account?.claimPlans
  if (!ledger || typeof ledger !== 'object' || Array.isArray(ledger)) return []
  const day = beijingDay()
  return Object.entries(ledger as Record<string, unknown>)
    .filter(([, at]) => Number(at) > 0 && beijingDay(Number(at)) === day)
    .map(([planId]) => planId)
}

/**
 * 今天是否领过至少一份。**只用于文案与悬停提示，不用来禁用按钮** ——
 * 「还有别的套餐能领吗」只有在弹窗里逐份比对才判得准（见 `claimedPlanIdsToday`）。
 */
export function claimedToday(account: AccountRecord | null | undefined): boolean {
  return claimedPlanIdsToday(account).length > 0
}

/** 「今天领过 N 份」的悬停说明：列出领过的套餐，并说清还能继续领别的 */
export function claimDoneTitle(account: AccountRecord | null | undefined): string {
  const planIds = claimedPlanIdsToday(account)
  const names = planIds.length ? `（${planIds.join('、')}）` : ''
  return `今天（北京时间 ${beijingDay()}）已领取 ${planIds.length} 份${names}；`
    + '还有其他可领套餐时，点这里可以继续领；活动按自然日发新套餐，明天可再领'
}

/**
 * 本家有没有「领福利」这个动作。
 *
 * 只看能力位，**没有**第二道 `canClaim` 判据：ZCode 那道闸是因为它的账号可能
 * 只粘了转发用的 accessToken、没有套餐令牌；CodeArts 的领取用的就是账号自己那份
 * 凭据，能路由就一定能领（真领不了由后端如实报错）。
 */
export function supportsWelfare(account: AccountRecord | null | undefined): boolean {
  return Boolean(providerFeatures(providerOf(account)).welfare)
}

/**
 * 今天的**北京时间**自然日（`YYYY-MM-DD`）。
 *
 * ⚠️ 不能按浏览器本地日算：后端那条自然日界是 `welfare::today`（UTC+8，中国无夏令时），
 * 台账里的 `day` 就是它写进去的字符串。界面若在别的时区按本地日判会出现两种错：
 * 北京 0 点前本地已是新一天 → 把昨天的「已领」显示成今天的（按钮被误置灰）；
 * 反过来则今天的读数被当成昨天（该灰不灰）。所以这里比的是**同一个字符串**，
 * 判据只有一份定义。
 */
export function beijingDay(at: number = Date.now()): string {
  return new Date(at + 8 * 3600 * 1000).toISOString().slice(0, 10)
}

/** 领取台账 → 按钮要用的读数（后端写在 `account.welfare` 上） */
export type WelfareState = { known: boolean; today: boolean; day: string; accepted: boolean; attempts: number; confirmed: number }

/**
 * 台账读数（`{known, today, day, accepted, attempts, confirmed}`）。
 *
 * ── 读不懂的台账一律按「今天没有读数」──────────────────────
 * 缺字段 / 日期不是今天 / 压根没领过，三种情况在这里都是 `today: false` 且
 * `attempts`/`accepted`/`confirmed` 归零：按钮照常可点，由后端如实报错（它那份
 * 校验比这里严，见 `ledger_of`）。这里**不复制**那份校验逻辑，否则同一件事有两处
 * 判据、改一处漏一处。**昨天的台账与今天无关**（后端也是整份重来），所以它只留下
 * `known: true`（「这台机器上有过台账」）而不带昨天的数字。
 *
 * `confirmed` 是**条数**而不是台账上的某个字段：后端把每条活动的进度存在
 * `campaigns` 字典里（`{idempotentKey, claimed, confirmed}`），台账顶层没有
 * `confirmed` 这个键 —— 悬停里那句「已确认到账 N 项」数的就是这里的项。
 */
export function welfareStateOf(account: AccountRecord | null | undefined): WelfareState {
  const day = beijingDay()
  const empty: WelfareState = { known: false, today: false, day, accepted: false, attempts: 0, confirmed: 0 }
  const ledger = account?.welfare
  if (!ledger || typeof ledger !== 'object' || Array.isArray(ledger)) return empty
  const row = ledger as Record<string, unknown>
  const campaigns = (row.campaigns && typeof row.campaigns === 'object' ? row.campaigns : {}) as Record<string, { confirmed?: unknown }>
  const state: WelfareState = {
    known: true,
    today: row.day === day,
    day,
    accepted: row.accepted === true,
    attempts: Number(row.attempts) || 0,
    confirmed: Object.values(campaigns).filter(item => item?.confirmed === true).length,
  }
  return state.today ? state : { ...empty, known: true }
}

/** 「已领」的悬停说明：说清哪一天、领到哪一份额度、什么时候能再领。 */
export function welfareDoneTitle(state: WelfareState): string {
  // 「不增加福利模型的 token 池」是**故意留在这里**的：这一家有两份账，领到的积分进的
  // 是套餐赠送积分，而用户点完最可能问的下一句就是「那我的福利模型怎么还是没额度」——
  // 答案放在这颗按钮的悬停里，不必再去余额列上猜（后端 usage 文档的 note 同口径）。
  return `今天（北京时间 ${state.day}）已由官方确认到账 ${state.confirmed} 项；`
    + '领到的是套餐赠送积分，不增加福利模型的 token 池；按自然日重置，明天可再领'
}

/** 「领福利」的悬停说明：把台账里已有的读数带上，回答「今天第几次了」。 */
export function welfareTodoTitle(state: WelfareState): string {
  const tried = state.today && state.attempts > 0
    ? `今天（北京时间 ${state.day}）已试过 ${state.attempts} 次但官方尚未确认到账，`
    : ''
  return `${tried}探测并领取官方每日登录赠送的套餐积分（到账进套餐积分，不增加福利模型 token 池）`
}

/**
 * 某时刻所在**本地自然日**的零点。自然日的判定统一走这里，与限流恢复时间的
 * 「今天 / 明天」（formatResetText）同一口径。
 */
const startOfLocalDay = (value: number): number => {
  const date = new Date(value)
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime()
}

/**
 * 该账号**今天是否已签到**（后端落盘的 `checkinAt` 落在本地今天）。
 *
 * 签到按自然日幂等（上游按天重置额度），所以「签过没有」不能只看有没有这个时间戳，
 * 必须比自然日 —— 过了 0 点同一个字段自然失效，**不需要任何定时器去重置**：判定是
 * 每次渲染现算的，跨零点后下一次重绘按钮就自己变回可点。
 * 看的是后端字段而不是界面缓存：自动签到的执行者是后端（定时任务），界面缓存里
 * 根本没有那次签到的结果。
 */
export function checkedInToday(account: AccountRecord | null | undefined): boolean {
  const at = Number(account?.checkinAt) || 0
  if (at <= 0) return false
  // 时间戳比现在还晚（改过系统时钟、或手工编辑过账号文件）时仍按「今天」算：
  // 它只可能来自一次真实的签到，宁可显示已签到也不要让按钮一直亮着
  if (at > Date.now()) return true
  return startOfLocalDay(at) === startOfLocalDay(Date.now())
}

/**
 * 可参与签到的账号（一键签到只用这批：所属家有签到活动 + 非国际版）。
 *
 * **不看 `enabled`**：禁用只表示「别用它转发」，签到是另一件事 —— 一个被禁用的账号
 * 依然可以每天签到攒积分。此处与后端 `core::billing::checkin` 的批量路径过滤链
 * 口径一致，否则界面上的「将签到 N 个账号」会与实际执行数对不上。
 */
export function checkinableAccounts(list: AccountRecord[] | null | undefined): AccountRecord[] {
  return (list || []).filter(account => supportsCheckin(account))
}

/* ─── 筛选维度（provider / enabled / limit 三维各自独立）───── */

export type AccountFilter = { provider: string; enabled: string; limit: string }

export function matchProvider(account: AccountRecord, filter: AccountFilter): boolean {
  return filter.provider === 'all' || providerOf(account) === filter.provider
}

export function matchEnabled(account: AccountRecord, filter: AccountFilter): boolean {
  if (filter.enabled === 'all') return true
  return filter.enabled === 'enabled' ? isEnabled(account) : !isEnabled(account)
}

/** 已禁用账号既不算「正常」也不算「已限流」：限流状态只对参与转发的账号有意义 */
export function matchLimit(account: AccountRecord, filter: AccountFilter): boolean {
  if (filter.limit === 'all') return true
  if (!isEnabled(account)) return false
  return filter.limit === 'limited' ? isRateLimited(account) : !isRateLimited(account)
}

/** 当前筛选条件下的可见账号（三个维度同时生效） */
export function visibleAccounts(all: AccountRecord[] | null | undefined, filter: AccountFilter): AccountRecord[] {
  return (all || []).filter(account => matchProvider(account, filter)
    && matchEnabled(account, filter)
    && matchLimit(account, filter))
}

/**
 * 分段计数：某分段显示的数字 = 「其余维度保持当前选择、本维度取该值」的账号数。
 * 「可见列表」与「分段计数」共用这一份口径 —— 否则徽标数字与点进去看到的结果会各算各的。
 */
export function filterCounts(
  all: AccountRecord[] | null | undefined,
  filter: AccountFilter,
  summaries: Array<{ id: string }> | null | undefined,
): Record<string, number> {
  const list = all || []
  const scope = (except: 'provider' | 'enabled' | 'limit') => list.filter(account =>
    (except === 'provider' || matchProvider(account, filter))
    && (except === 'enabled' || matchEnabled(account, filter))
    && (except === 'limit' || matchLimit(account, filter)))

  const forEnabled = scope('enabled')
  const forLimit = scope('limit')
  const forProvider = scope('provider')
  const counts: Record<string, number> = {
    enabledAll: forEnabled.length,
    enabled: forEnabled.filter(isEnabled).length,
    disabled: forEnabled.filter(a => !isEnabled(a)).length,
    limitAll: forLimit.length,
    normal: forLimit.filter(a => isEnabled(a) && !isRateLimited(a)).length,
    limited: forLimit.filter(a => isEnabled(a) && isRateLimited(a)).length,
    providerAll: forProvider.length,
  }
  // 摘要里每一家都要有键（没有账号的家显示 0 并置灰），否则它的徽标会停在旧数字上
  ;(summaries || []).forEach(item => { counts[`p-${item.id}`] = 0 })
  forProvider.forEach(account => {
    const key = `p-${providerOf(account)}`
    counts[key] = (counts[key] || 0) + 1
  })
  return counts
}

/**
 * 账号**当前生效**的限流记录：`[{model, status, code, resetAt, message}]`，
 * 按恢复时间升序（最早恢复的排最前）。过期记录视为不存在（冷却自然结束）。
 * 「限流」列与展开的明细面板共用这一份口径 —— 列上的数字与点开看到的条数不会对不上。
 */
export function activeLimits(account: AccountRecord | null | undefined): Array<RateLimitInfo & { model: string }> {
  const limits = account?.rateLimits
  if (!limits || typeof limits !== 'object') return []
  const now = Date.now()
  return Object.entries(limits)
    .map(([model, info]) => ({ model, ...(info && typeof info === 'object' ? info : {}) }))
    .filter(item => Number(item.resetAt) > now)
    .sort((a, b) => Number(a.resetAt) - Number(b.resetAt))
}

/**
 * 转发顺序位置表：accountId → `{ position, total }`（1 起）。
 *
 * **全局一条队列**：四家账号按优先级混排，序号就是整张表的行序，与后端选路
 * （全局优先级）、↑/↓ 的边界同源 —— 否则会出现「界面上不是第一位、但下移按钮
 * 已经点不动」这种对不上的情况。
 */
export function positionMap(all: AccountRecord[] | null | undefined): Map<string, { position: number; total: number }> {
  const ordered = (all || []).slice().sort(byPriorityOrder)
  const map = new Map<string, { position: number; total: number }>()
  ordered.forEach((account, index) => map.set(account.id, { position: index + 1, total: ordered.length }))
  return map
}

/* ─── 展示派生 ─────────────────────────────── */

/** 账号展示名（昵称优先，退化到备注名 / 标识 / id） */
export function displayNameOf(account: AccountRecord | null | undefined): string {
  return account?.nickname || account?.name || identifierOf(account) || account?.id || ''
}

/** 无有效恢复时间时的退化文案：它本身就是完整一句，调用方据此不再拼「，恢复时间：」 */
export const RESET_UNKNOWN = '已限流'

/** 某时刻所在自然日的零点（本地时区），用于按「日历天」计算今天 / 明天 */
const startOfDay = (value: Date): number => new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime()

/**
 * 限流恢复时间文案：今天 HH:mm / 明天 HH:mm / M月d日 HH:mm。
 *
 * 为什么带「今天 / 明天」而不是相对毫秒数或完整时间戳：限流是自动解除的，用户扫过
 * 列表时最关心「到点了没、还要等多久」——「明天 01:04」比「09-19 01:04」少一步换算，
 * 也不会像「6 小时后」那样一过夜就说不清是哪天。
 * 无有效时间戳（缺失 / 非法 / 已过）时返回 RESET_UNKNOWN，由调用方退化成只输出这一句。
 */
export function formatResetText(resetAt: unknown): string {
  const time = Number(resetAt)
  if (!Number.isFinite(time) || time <= 0 || time <= Date.now()) return RESET_UNKNOWN
  const date = new Date(time)
  const clock = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })
  // 按自然日求差而不是按 24 小时：今晚 23:50 到明天 00:10 只差 20 分钟，但用户嘴里
  // 它就是「明天」，按毫秒差算会显示成「今天」，与直觉相反
  const days = Math.round((startOfDay(date) - startOfDay(new Date())) / 86400e3)
  if (days === 0) return `今天 ${clock}`
  if (days === 1) return `明天 ${clock}`
  return `${date.getMonth() + 1}月${date.getDate()}日 ${clock}`
}

/**
 * 版本后缀：国内 / 国际。只返回文字，由调用方拼进提供商徽章 ——「WorkBuddy 国际版」
 * 是**一枚**徽章，与 AutoClaw 那种「名字自带版本」的家看起来是同一种标签。
 *
 * 名字里已经带地区的不再拼一遍：`zcode` / `zcode-intl` / `accio-cn` 这几个 provider 的
 * 注册名本身就以地区结尾（「ZCode 国内版」），拼出来是「ZCode 国内版 国内版」。判据取
 * 「注册名是否已含这个后缀串」，而不是再列一张名单 —— 名单会随新增地区漏项。
 * 别用 `edition` 能力位去关：它还兼着「有效期列读哪个字段」的判据，置 false 会把
 * 有效期列改读 tokenExpiresAt，而那对这几家是错的字段。
 */
export function editionSuffix(account: AccountRecord | null | undefined): string {
  const provider = providerOf(account)
  if (!providerFeatures(provider).edition) return ''
  const edition = accountEdition(account)
  const suffix = account?.editionLabel || (edition === 'intl' ? '国际版' : '国内版')
  const label = shared().wbProviders?.labelOf?.(provider) || ''
  return label.includes(suffix) ? '' : suffix
}

/** 健康标签（结构化；原来由 accountTags 直接拼 HTML，现在交给 React 渲染成 Badge） */
export type AccountTag = { text: string; kind: 'plain' | 'bad'; title: string }

/**
 * 状态标签集合：这一区只表达**健康状态**。
 *
 * 「限流」不在这里 —— 限额按模型记，它有自己的一列。**只标「需要关注的状态」，
 * 一切正常时返回空数组**：启用 / 禁用由开关自身表达，再补一枚「启用」徽章是在同一格
 * 里说第二遍同一件事。「不可用」也不再渲染（`available` 把手动禁用也算进去，
 * 禁用的账号开关明明是关着的，再标一枚是把同一件事说两遍）。
 */
export function accountTags(account: AccountRecord): AccountTag[] {
  return [
    // 没有转发能力的家：它的启用开关对转发没有意义，这里如实说明，而不是留一片空白
    // 让人以为「没标记就是好的」。判据是后端的 chatSupported，正常配置下不会出现 ——
    // 留着是为了「将来某家处于只有账号管理的过渡期」时界面能自己说清楚
    supportsChat(account)
      ? null : { text: '仅账号管理', kind: 'plain' as const, title: '该提供商的推理转发尚未接入，账号不参与转发' },
    // 代理配了解析不出来时明确标出：转发会回退直连，属于需要留意的情况
    account.proxy?.error
      ? { text: '代理异常', kind: 'bad' as const, title: `${account.proxy.error}（转发时会回退直连）` }
      : null,
    // 走活动套餐通道时标出来：它不是默认值，而「这条请求到底花的是哪份额度」
    // 恰恰是用户在这个页面上要回答的问题（行上的余额列也可能同时挂着两份）
    zcodePlanOf(account) === ZCODE_PLAN_START
      ? {
        text: '活动套餐',
        kind: 'plain' as const,
        title: '转发走活动套餐通道（zcode.z.ai 的 Anthropic 端点，用账号里领到的额度）；在账号设置里可切回编码套餐',
      }
      : null,
  ].filter((tag): tag is AccountTag => tag !== null)
}

/**
 * 「下次什么时候能再签」的说明（已签到按钮的悬停说明与明细面板共用一句）。
 *
 * 两家口径不同：
 *   - WorkBuddy / 小浣熊 / AutoClaw：按**自然日**重置，明天 0 点后可再签；
 *   - Qoder：每日权益是一个**活动窗口**（当天 10:00 → 次日 10:00），
 *     所以 0 点后不一定能签 —— 说「0 点后可再签」会让人白点一次。
 */
export function checkinResetHint(account: AccountRecord | null | undefined): string {
  return providerOf(account) === 'qoder'
    ? 'Qoder 的每日权益按 10:00 → 次日 10:00 的活动窗口发放，新窗口开放后可再领'
    : '签到按自然日重置，明天 0 点后可再签'
}

/** 「已签到」按钮的悬停说明：给出签到时刻与重置时机，回答「为什么点不动、什么时候能再签」 */
export function checkinDoneTitle(account: AccountRecord | null | undefined): string {
  const at = Number(account?.checkinAt) || 0
  const clock = at > 0 ? `今天 ${new Date(at).toTimeString().slice(0, 5)}` : '今天'
  return `${clock} 已签到；${checkinResetHint(account)}`
}

/** 签到明细里「今天已签到」那一刻的时钟串（0 返回空串） */
export function checkinClock(account: AccountRecord | null | undefined): string {
  const at = Number(account?.checkinAt) || 0
  return at > 0 ? new Date(at).toTimeString().slice(0, 5) : ''
}

/** 更新时刻文案（时间戳非法时返回空串，调用处据此省略那半句） */
export const formatUpdatedAt = formatTime
