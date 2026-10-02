/**
 * 账号表的**行与单元格**（替换 ui/accounts-table.js 的单元格渲染 + ui/accounts-model.js
 * 的 ⋯ 菜单 / 行内面板 HTML + ui/accounts-view.js 的菜单交互）。
 *
 * ── 为什么还留着 legacy 类名 ─────────────────────────────────
 * 表格的每一格都有 page-accounts-table.css 里量出来的尺寸与排版（列宽预算、`.prio` 的
 * 22+84 算式、`.acct-actions` 的四颗按钮预算…）。迁移只换**控件**，布局类名原样保留：
 * 换掉的控件走组件库（Button / Badge / Switch / Checkbox / Select / Popover），
 * 而 `.prio-stepper`（数字框 + 两枚箭头的合并控件）、`.pbadge`（按 provider 上色的
 * 徽章）、`.usage-sum`（读数，不可点）这三处组件库没有对应件，保持原标记形态 ——
 * 见最终报告的「组件库缺口」。
 *
 * ── 优先级控件的方向语义（**极易搞反，改动时先读这里**）──────────
 * 优先级数值越小越先用（全局一条队列），所以：
 *   · 上箭头（↑）= 与队列里的**上一个**账号交换 = 排得更靠前 = **数值变小**
 *   · 下箭头（↓）= 与队列里的**下一个**账号交换 = 排得更靠后 = **数值变大**
 * 来自后端 `move_account(id, "up" | "down")` 的语义。视觉上「↓ 在左、↑ 在右」与
 * 「左降右升」的横排直觉一致。
 */

import * as React from 'react'
import {
  Badge,
  BadgeDot,
  Button,
  Checkbox,
  Popover,
  PopoverContent,
  PopoverTrigger,
  Progress,
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
  Switch,
  cn,
} from '@ui'
import { formatTime, poolItemLabel, POOL_VALUE_PREFIX, shared, type AccountRecord, type UsageEntry } from './accounts-shared'
import {
  accountTags, activeLimits, checkedInToday, checkinDoneTitle, claimDoneTitle, claimedToday,
  displayNameOf, editionSuffix, expiryMillis, formatResetText, identifierOf, isDesktopAccount, isEnabled,
  providerFeatures, providerOf, RESET_UNKNOWN, supportsCheckin, supportsClaim, supportsUsage,
  supportsWelfare, welfareDoneTitle, welfareStateOf, welfareTodoTitle,
} from './accounts-domain'
import { PRIORITY_MAX, PRIORITY_MIN, priorityOf } from './accounts-columns'
import {
  PROXY_CUSTOM_CURRENT, PROXY_CUSTOM_EDIT, applyProxyPick, checkinErrorOf,
  commitPriority, connectionsOf, maskName, moveAccount, openSettingsDialog, poolError,
  proxyPoolSnapshot, queryUsageOnce, runCheckin, setAccountEnabled, setPanelOpen,
  startCodeArtsWelfare, startZcodeClaim, toggleNamesHidden, usageEntries, usageFailureOf,
} from './accounts-data'
/** 图标（icons.js 的内联 SVG 串）：整站共用一份图标集，这里只做注入 */
function iconHtml(name: string, size: number): string {
  return shared().wbIcons?.icon?.(name, size) || ''
}

/* ─── 优先级列 ──────────────────────────────── */

/**
 * 优先级：全局序号 + 「↓ 数字 ↑」合并控件。
 *
 * 序号（#N）回答「第几位」，控件里的数字回答「队列值」—— 两者是同一个事实的两种读法。
 * 数字一直可编辑（不是双击才变输入框）：这一列的主用途就是改顺序，双击先要用户发现
 * 「这里能双击」；而两枚箭头已经覆盖了最常用的「挪一位」。保存时机是**失焦 / 回车**
 * 而不是 input 事件 —— 每敲一位就发一次请求会让「改成 250」变成三次 PATCH。
 *
 * 草稿态住在这个组件里（而不是像旧实现那样「重绘前 captureEditing / 重绘后
 * restoreEditing」）：React 不会重建同一个 key 的输入框，用户的输入与光标天然保住；
 * 只有「不在编辑中」时才把服务端的最新值同步进草稿。
 */
export function PriorityStepper({ account, seat }: {
  account: AccountRecord
  seat: { position: number; total: number }
}) {
  const serverValue = priorityOf(account)
  const [draft, setDraft] = React.useState(String(serverValue))
  const [editing, setEditing] = React.useState(false)
  /** 交换在途：连点两下会发出两次交换（后端按相邻位置找，第二次会换到别人身上） */
  const [moving, setMoving] = React.useState(false)

  React.useEffect(() => {
    if (!editing) setDraft(String(serverValue))
  }, [serverValue, editing])

  async function commit(): Promise<void> {
    setEditing(false)
    const next = await commitPriority(account.id, draft)
    // commitPriority 在「非法 / 未改动 / 失败」时返回账号的**真实当前值** ——
    // 留着用户输的数字会让人以为存进去了，而下一次刷新它又会悄悄跳回去
    if (next !== null) setDraft(String(next))
  }

  async function move(direction: 'up' | 'down'): Promise<void> {
    if (moving) return
    setMoving(true)
    try {
      await moveAccount(account.id, direction)
    } finally {
      setMoving(false)
    }
  }

  const atFront = seat.position <= 1
  const atEnd = seat.position >= seat.total

  return (
    <div className='prio'>
      <span className='seat' title={`全局队列第 ${seat.position} 位，共 ${seat.total} 位`}>#{seat.position}</span>
      <span className='prio-stepper'>
        <button type='button' className='prio-arrow' disabled={atEnd || moving}
          title='与队列里的下一个账号交换优先级（可能是另一家的账号）'
          onClick={() => void move('down')}
          dangerouslySetInnerHTML={{ __html: iconHtml('arrowDown', 14) }} />
        <input className='prio-input' type='number' min={PRIORITY_MIN} max={PRIORITY_MAX} step={1}
          aria-label='优先级' title='全局唯一：所有提供商的账号都不能重号，数值越小越先用'
          value={draft}
          onChange={event => setDraft(event.currentTarget.value)}
          onFocus={() => setEditing(true)}
          onBlur={() => { void commit() }}
          onKeyDown={event => {
            if (event.key === 'Enter') {
              event.preventDefault()
              // 回车 = 提交（失焦即走上面那条路）
              event.currentTarget.blur()
            } else if (event.key === 'Escape') {
              // 放弃这次输入、还原成当前值
              setDraft(String(serverValue))
              setEditing(false)
              event.currentTarget.blur()
            }
          }} />
        <button type='button' className='prio-arrow' disabled={atFront || moving}
          title='与队列里的上一个账号交换优先级（可能是另一家的账号）'
          onClick={() => void move('up')}
          dangerouslySetInnerHTML={{ __html: iconHtml('arrowUp', 14) }} />
      </span>
    </div>
  )
}

/* ─── 状态列 ────────────────────────────────── */

/**
 * 状态：启用 / 禁用开关（+ 需要留意时的健康徽章）。
 * 开关直接落 `PATCH { enabled }`，不做二次确认 —— 这个动作可逆，且关掉后账号记录仍在
 * 列表里。徽章只在需要留意时出现（一切正常时 accountTags 返回空）—— 启用状态由开关的
 * 轨道位置与滑块表达，再补一枚「启用」是同一格里的第二次说明。
 */
export function StatusCell({ account }: { account: AccountRecord }) {
  const enabled = isEnabled(account)
  const tags = accountTags(account)
  const who = displayNameOf(account) || account.id
  return (
    <>
      <Switch checked={enabled} aria-label={`${enabled ? '禁用' : '启用'}${who}`}
        title={enabled ? '已启用，点击禁用（不参与转发）' : '已禁用，点击启用'}
        onCheckedChange={next => void setAccountEnabled(account.id, next)} />
      {tags.length ? (
        <div className='status-tags'>
          {tags.map(tag => (
            <Badge key={tag.text} shape='tag' variant={tag.kind === 'bad' ? 'destructive' : 'secondary'}
              title={tag.title}>{tag.text}</Badge>
          ))}
        </div>
      ) : null}
    </>
  )
}

/* ─── 限流列 ────────────────────────────────── */

/**
 * 限流：这个账号**当前限流中的模型**。限额在后端按「账号 × 模型」记，四家通用 ——
 * 所以这一列对四家都成立，不再需要「选个模型看队列」的筛选器。
 * 有限流时是一枚可点的黄色徽章（点开 / 收起行下的明细面板），正常时是绿点「正常」。
 */
export function LimitsCell({ account, open }: { account: AccountRecord; open: boolean }) {
  const entries = activeLimits(account)
  if (!entries.length) {
    return (
      <Badge variant='success' shape='tag' title='当前没有任何模型处于限流中'>
        <BadgeDot />正常
      </Badge>
    )
  }
  const soonest = formatResetText(entries[0].resetAt)
  return (
    <>
      <Badge variant='warning' shape='tag' render={<button type='button' />}
        className={open ? 'ring-2 ring-warning-soft' : undefined}
        title={`点击${open ? '收起' : '查看'}各模型的限流明细`}
        onClick={() => setPanelOpen(account.id, 'limits', !open)}>
        {entries.length} 个模型 ▾
      </Badge>
      <span className='lim-sub' title='最早恢复'>
        最早 {soonest === RESET_UNKNOWN ? '待定' : soonest} 恢复
      </span>
    </>
  )
}

/* ─── 有效期 / 连接数 / 余额列 ──────────────── */

/**
 * 有效期：按「这家有没有版本概念」选字段（workbuddy 是 expiresAt，其余是
 * tokenExpiresAt），与域层的 tokenExpiryOf 同口径。文案收短成「30 天后」，
 * 完整句留在 title —— 列宽有限。
 *
 * 读数经域层 `expiryMillis` **先归一到毫秒**再判：落盘的到期值在秒与毫秒之间漂过
 * （Trae 的凭据是从 CPA 的 auth 文件与手工粘贴进来的，那里是 10 位秒；别家与上游
 * 刷新响应都是 13 位毫秒），不归一时 `1791009732` 当毫秒读就是 1970-01-21，账号一进
 * 面板就红着显示「已过期」。归一规则与理由写在 accounts-domain.ts 那一处，
 * 后端 `providers::trae::Credential::expires_at_ms` 是同一个口径。
 */
export function ExpiryCell({ account }: { account: AccountRecord }) {
  const features = providerFeatures(providerOf(account))
  const expiresAt = expiryMillis(features.edition ? account.expiresAt : account[features.expiry])
  if (!expiresAt) return <span className='muted' title='记录里没有过期时间'>—</span>
  const left = expiresAt - Date.now()
  if (left <= 0) return <Badge variant='destructive' shape='tag' title='凭证已过期，转发时会先刷新'>已过期</Badge>
  const text = left < 3600e3 ? `${Math.max(1, Math.round(left / 60e3))} 分钟后`
    : left < 48 * 3600e3 ? `${(left / 3600e3).toFixed(1)} 小时后`
      : `${Math.floor(left / 24 / 3600e3)} 天后`
  // 完整时间点只在解析得出时补进 title：formatTime 对非法时间戳返回空串，
  // 直接拼会留下一个空的「（）」
  const full = formatTime(expiresAt)
  return <span title={full ? `${text}过期（${full}）` : `${text}过期`}>{text}</span>
}

/**
 * 连接数：此刻正在使用这个账号的请求数（2 秒一轮的实时计数）。
 * 口径与 OmniProxy 上游管理页的「连接」列一致 —— 有连接时显示数字、为 0 时**什么都不
 * 显示**（留空）。满屏的 0 会把少数几个真正在跑的账号淹没；要看「谁是 0」时空白本身
 * 就是答案。计数缺失（还没拉到、后端不可达）与 0 同样处理：把一个尚未知的值渲染成 0
 * 会读成「这个账号没在用」，而事实可能是「数据还没到」。
 */
export function ConnectionsCell({ account }: { account: AccountRecord }) {
  const value = connectionsOf(account.id)
  if (value <= 0) return null
  return (
    <span className='conn-count' title={`${value} 个请求正在使用该账号（含还在下发内容的流式请求）`}>
      {value}
    </span>
  )
}

/** 数值 → 展示串（与余额列摘要同口径，取不到给「—」） */
function numberText(value: unknown): string {
  if (value === null || value === undefined || value === '') return '—'
  const number = Number(value)
  return Number.isFinite(number) ? String(number) : String(value)
}

/**
 * 订阅信息 → 摘要 title 里的一段文字（统一形状的 `subscription`）。
 * `expireAt` 各家的类型不同（小浣熊给的是上游原样的字符串日期，AutoClaw 可能给时间戳）：
 * 能解析成日期的按本地时间格式化，否则原样显示 —— 不猜、不丢。
 */
function subscriptionText(subscription: unknown): string {
  if (!subscription || typeof subscription !== 'object') return ''
  const info = subscription as Record<string, unknown>
  const parts: string[] = []
  if (info.planName) parts.push(`套餐 ${String(info.planName)}`)
  if (info.status) parts.push(`状态 ${String(info.status)}`)
  const expireAt = info.expireAt
  if (expireAt !== null && expireAt !== undefined && expireAt !== '') {
    const asNumber = Number(expireAt)
    const text = Number.isFinite(asNumber) && asNumber > 1e11 ? formatTime(asNumber) : String(expireAt)
    if (text) parts.push(`到期 ${text}`)
  }
  if (Number.isFinite(Number(info.remainQuota))) parts.push(`余量 ${numberText(info.remainQuota)}`)
  if (Number.isFinite(Number(info.totalQuota))) parts.push(`总量 ${numberText(info.totalQuota)}`)
  return parts.join(' ')
}

/**
 * 余额结果 → 一行摘要（`{text, kind, title}`）。
 *
 * 形状探测按**字段**而不是按 provider（`totalLeft` 键 = workbuddy 既有形状，否则看
 * `available` / `wallets`）：provider 只决定「谁去查」，不决定「查回来长什么样」。
 * 失败与「未配置」的分流走 usageFailureOf（判据的唯一入口，与查询动作那边的 toast 同源）。
 * 摘要文案刻意压到「数字 + 单位」，完整句（各钱包 / 套餐 / 到期）放进 title ——
 * 余额列是这张表里最窄的几列之一。
 */
function usageSummary(entry: UsageEntry): { text: string; kind: string; title: string } {
  if (entry === undefined) return { text: '未查询', kind: 'muted', title: '尚未查询该账号的余额' }
  if (entry === null) return { text: '查询中…', kind: 'muted', title: '正在查询' }
  const failure = usageFailureOf(entry)
  if (failure) {
    return failure.notConfigured
      ? { text: '未配置', kind: 'muted', title: `${failure.message}（去该账号的「设置」里填上查询凭证即可）` }
      : { text: '查询失败', kind: 'bad', title: failure.message }
  }
  if (typeof entry !== 'object' || entry === null) return { text: '无数据', kind: 'muted', title: String(entry) }
  const data = entry as Record<string, unknown>
  if (Object.prototype.hasOwnProperty.call(data, 'totalLeft')) {
    const total = data.unlimited ? '∞' : numberText(data.totalLeft)
    return {
      text: `可用 ${total}`,
      kind: 'ok',
      title: `总剩余 ${total} · 套餐 ${numberText(data.planLeft)} · 奖励 ${numberText(data.bonusLeft)}`,
    }
  }
  if (Object.prototype.hasOwnProperty.call(data, 'available') || Array.isArray(data.wallets)) {
    const unit = String(data.unit || '积分')
    const wallets = Array.isArray(data.wallets) ? data.wallets as Array<Record<string, unknown>> : []
    // 上游给的展示串优先（带千分位 / 单位的格式化），没有才按数值拼
    const detail = wallets
      .map(wallet => `${wallet?.displayName || wallet?.type || '明细'} `
        + `${wallet?.balanceView ? String(wallet.balanceView) : numberText(wallet?.balance)}`)
      .join(' · ')
    const subscription = subscriptionText(data.subscription)
    // 展示串优先：ZCode 的额度单位是 token（1 亿 = 9 位数字），而余额列只有几十
    // 像素宽 —— 后端因此给了 `availableView`（`1亿 token` 这类紧凑串）。没有这个
    // 字段的家（其余全部）走的仍是「数值 + 单位」那条老路，行为一字未变。
    const available = data.availableView
      ? String(data.availableView)
      : `可用 ${numberText(data.available)} ${unit}`
    // ── 部分失败：一份账读到了、另一份没读到 ────────────────────
    // CodeArts 的余额是**两台网关**（订阅统计 + 福利网关，见后端
    // `providers::codearts::balance` 的模块头），后端把失败的一侧写进
    // `statisticsError` / `benefitError` 而不是整次失败。这时读数是真的、
    // 但**不完整**：显示成一片绿「可用 —」会被读成「额度用完了」，
    // 而实际是那半边根本没读到。判据仍然只在 `usageFailureOf` 那一处
    // （整次失败的入口），这里只补「半次失败」。
    const missing = [
      data.statisticsError ? `订阅统计未读到：${String(data.statisticsError)}` : '',
      data.benefitError ? `福利网关未读到：${String(data.benefitError)}` : '',
    ].filter(Boolean)
    // ── 「没有福利」不是「没读到」────────────────────────────────
    // 上游对没有福利池的账号（福利按限时活动下发，Free 账号常常没有）回
    // `4004 benefit not found`，后端把它翻成 `benefitAbsent` 而不是错误。
    // 这里只在中性说明里提一句 —— 不动 kind、不加 ⚠：它回答的是「为什么
    // 这行没有福利读数」，不是一个需要用户去查的问题。
    const absent = data.benefitAbsent
      ? ['该账号没有福利模型额度（福利按活动下发，不是每个账号都有）']
      : []
    return {
      text: available + (missing.length ? ' ⚠' : ''),
      kind: missing.length ? 'warn' : 'ok',
      title: [available, detail, subscription, ...absent, ...missing].filter(Boolean).join(' · '),
    }
  }
  return { text: '无数据', kind: 'muted', title: '未返回可识别的余额数据' }
}

/**
 * 余额里的「主额度桶」：界面用它画套餐名与进度条（`UsageCell` 的两行形态）。
 *
 * ── 为什么取「总量最大的那个桶」──────────────────────────────
 * 一个账号常常同时挂着好几份额度：活动发的大额包（ZCode Trust Build 的 1 亿）
 * 与每天续发的小额包（Start Plan 的 300 万）。用户点开这一列想看的是**大额那份**
 * 还剩多少，而「总量最大」正是它 —— 也顺带避开了每日桶在一天之内反复回满
 * 导致进度条乱跳。总量缺一个都不参与比较（没有总量就画不出进度），
 * 于是没有结构化验数据的家（其余全部）自然退回上面那套纯读数呈现。
 *
 * ── 缺失一律当「不知道」─────────────────────────────────────
 * `remainingPercent` 缺失时**不画进度条**（但名字与读数照给），
 * 不拿 0 或 100 冒充 —— 一条满格的进度条与一条空进度条读起来是相反的意思，
 * 而它们都可能是错的。
 */
function usagePool(entry: UsageEntry): { planName: string; text: string; percent: number | null } | null {
  if (!entry || typeof entry !== 'object' || Array.isArray(entry)) return null
  const data = entry as Record<string, unknown>
  const wallets = Array.isArray(data.wallets) ? data.wallets as Array<Record<string, unknown>> : []
  let lead: Record<string, unknown> | null = null
  for (const wallet of wallets) {
    const total = Number(wallet?.total)
    if (!Number.isFinite(total) || total <= 0) continue
    if (!lead || total > Number(lead.total)) lead = wallet
  }
  if (!lead) return null
  const subscription = (data.subscription && typeof data.subscription === 'object'
    ? data.subscription
    : {}) as Record<string, unknown>
  // 套餐名优先取这个桶自己的（`wallet.planName`），退回账号级那份 ——
  // 桶认不出归属时（上游没给 plan_id）至少还有订阅里的名字可显示
  const planName = String(lead.planName || subscription.planName || '')
  const percent = numberOrNull(lead.remainingPercent)
  return {
    planName,
    text: String(lead.balanceView || numberText(lead.balance)),
    percent: percent === null ? null : Math.max(0, Math.min(100, percent)),
  }
}

/**
 * 数值字段 → number 或 null。
 *
 * 必须显式判 null/undefined/空串：`Number(null)` 是 0（不是 NaN），照直转换会把
 * 「上游没给这个数」变成「这个数是 0」—— 进度条会画成一条空条，与「剩余 0%」
 * 读起来一模一样，而那是两件事（见 `usagePool` 的缺失口径）。
 */
function numberOrNull(value: unknown): number | null {
  if (value === null || value === undefined || value === '') return null
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : null
}

/**
 * 余额列：**只放读数**（不可点）—— 查询按钮住在操作列，这一列纯粹是
 * 「一眼看出还剩多少」。刻意不换成组件库的 Badge：它是读数而不是状态徽章，
 * 样式全在 `.usage-sum` 里（四档语义色：ok / bad / muted / warn —— `warn` 是
 * 「读到了但不完整」那一档，见上面 `usageSummary` 的半次失败分支）。
 *
 * ── 两行形态（有声明的总额度桶时）───────────────────────────
 * 第一行是套餐名（用户问得最多的一句是「这 1 亿是哪个活动给的」），
 * 第二行是进度条 + 「剩余 / 总量」。进度条画的是**剩余**比例 ——
 * 与相邻的读数同一方向，否则「条快满了」与「剩 8800 万」会互相打架。
 * 完整明细（每个桶、到期、可用合计）仍在悬停提示里，这里只抢最基本的两个问题：
 * 哪个套餐、还剩多少。
 */
export function UsageCell({ account }: { account: AccountRecord }) {
  if (!supportsUsage(account)) {
    return <span className='muted' title='该提供商没有余额查询'>—</span>
  }
  const entry = usageEntries().get(account.id)
  const summary = usageSummary(entry)
  // 失败 / 未配置那些档不画进度条：读数本身就不是「还剩多少」，
  // 给它配个进度条会把一句错误装饰成一条可信的读数
  const pool = summary.kind === 'ok' || summary.kind === 'warn' ? usagePool(entry) : null
  if (!pool) {
    return <span className={`usage-sum ${summary.kind}`} title={summary.title}>{summary.text}</span>
  }
  return (
    <span className='usage-pool' title={summary.title}>
      {pool.planName ? <span className='usage-pool-name'>{pool.planName}</span> : null}
      <span className='usage-pool-line'>
        {pool.percent !== null ? <Progress value={pool.percent} className='usage-pool-bar' /> : null}
        <span className={`usage-pool-view ${summary.kind}`}>{pool.text}</span>
      </span>
    </span>
  )
}

/* ─── 账号 / 提供商 / 代理列 ─────────────────── */

/**
 * 账号：第一行名称，第二行邮箱（有才渲染），第三行只在异常时出现（代理不可用原因）。
 *
 * 标识（UID / userId）与 Token 尾号**不再上屏**（对「这条账号能不能用」没有信息量，
 * 却把副标题占掉大半），仍留在账号名的悬停提示里。健康说明放这一列而不是「状态」列：
 * 状态列只有几十像素，放不下必须读全的文案；账号列是唯一随列宽变化伸缩的一列。
 *
 * 第二行是**邮箱**（不是「桌面端」标签）：这一列要回答的是「这是谁的号」，而
 * AutoClaw 国际版这类网页登录建出来的账号，名字可能只是上游昵称，邮箱才认得出是谁。
 * 名字本身就是邮箱时不重复渲染。Qoder / AutoClaw 国际版反过来：邮箱当**主名**
 * （features.emailAsName），昵称不再占一行（要看就悬停）。
 */
export function AccountCell({ account, namesHidden }: { account: AccountRecord; namesHidden: boolean }) {
  const ident = identifierOf(account)
  const features = providerFeatures(providerOf(account))
  const name = displayNameOf(account) || '未命名账号'
  const email = String(account.email || '').trim()
  const emailAsName = features.emailAsName && email ? email : ''
  const title = [
    ident ? `${features.identifier} ${ident}` : '',
    // 邮箱顶掉了名字的位置，名字（昵称 / 备注名）改从这里看；隐藏账号名开关打开时
    // 连这里也不给 —— 否则悬停一下就能绕过打码，那个开关就白开了
    emailAsName && name !== email && !namesHidden ? `账号名 ${name}` : '',
    isDesktopAccount(account) ? '桌面端实时登录态（凭证每次从客户端登录态文件读取）' : '',
    account.tokenTail ? `Token 尾号 ${account.tokenTail}` : '',
    account.updatedAt ? `更新于 ${formatTime(account.updatedAt)}` : '',
    account.source ? `来源 ${account.source === 'imported' ? '旧数据导入' : '手动添加'}` : '',
  ].filter(Boolean).join('；')

  const showEmail = !emailAsName && email && email !== name
  const primary = emailAsName || name
  const shown = namesHidden ? maskName(primary) : primary
  const proxyError = account.proxy?.error
  return (
    <>
      <div className='acct-name' title={title || undefined}>
        <span className='name'>{shown}</span>
      </div>
      {showEmail ? (
        <div className='acct-sub'>
          <span className='acct-email' title='账号邮箱'>{namesHidden ? maskName(email) : email}</span>
        </div>
      ) : null}
      {proxyError ? (
        <div className='acct-note bad' title={`代理不可用：${proxyError}`}>代理不可用：{proxyError}</div>
      ) : null}
    </>
  )
}

/**
 * 提供商：一枚徽章，带版本后缀（「WorkBuddy 国际版」）—— 与 AutoClaw 那种「名字自带
 * 版本」的家同一种形态，不再提供商、版本两枚并排。
 * 配色按 provider id 生成（`p-<id>` 类），未登记的家落到 CSS 里的中性兜底 ——
 * 加一家时不必改样式表，也不会显示成空白（所以这里不换组件库的 Badge：它没有按
 * provider 上色的档位，见最终报告的组件库缺口）。
 */
export function ProviderCell({ account }: { account: AccountRecord }) {
  const provider = providerOf(account)
  const label = shared().wbProviders?.labelOf?.(provider) || provider
  const edition = providerFeatures(provider).edition ? editionSuffix(account) : ''
  const text = edition ? `${label} ${edition}` : label
  return (
    <div className='pv'>
      <span className={`pbadge p-${provider}`} title={`提供商：${text}`}>{text}</span>
    </div>
  )
}

/**
 * 代理：这个账号出网走哪条线路。一格一个下拉，**选中即保存**。
 *
 * 选项**只有**直连 + 「网络代理」页的代理池条目（值是 `pool:<proxyId>`）——
 * 出口统一在那一页配 / 命名 / 测试（Clash 的出口由「同步 Clash Verge」整体
 * 镜像进池），这里只做「选哪一条」。「自定义代理…」是**动作项**（不是一种
 * 配置）—— 选中它打开账号设置弹窗，下拉随即恢复原值（它是受控的，重绘即回原值）。
 * 池列表来自 accounts-data 的模块级缓存（同步读，纯渲染不发请求）；没就绪时
 * 先只有「直连 + 当前值 + 自定义…」，页面的自愈 effect 补拉一次再重画。
 *
 * 存量记录（直接引用 Clash 出口的 `{source:'clash', listenerUid}`、或自定义
 * 形状）在这里显示为**补位项**（带来源前缀），照原样转发；用户在这个下拉里
 * 选一条池条目或切回直连就会改写它。
 */
export function ProxyCell({ account }: { account: AccountRecord }) {
  const proxy = account.proxy
  const source = proxy?.config?.source || proxy?.source
  const label = proxy?.label || (source === 'custom' ? '自定义代理' : '已设置')
  const broken = proxy?.error
  const pool = proxyPoolSnapshot()
  const poolItems = Array.isArray(pool) ? pool : []

  // 当前值映射回下拉的值域：池条目用 `pool:<id>`、其余（Clash 直引 / custom /
  // 坏形状）落到补位项 —— 绝不能回落成「直连」：那会把「配置坏了」显示成「没配」
  let current = ''
  if (source === 'pool' && proxy?.config?.proxyId) current = `${POOL_VALUE_PREFIX}${proxy.config.proxyId}`
  else if (proxy) current = PROXY_CUSTOM_CURRENT

  const items: Array<{ value: string; label: string; disabled?: boolean }> = [{ value: '', label: '直连' }]
  for (const item of poolItems) {
    // 文案是「名字（协议 主机:端口）」—— 名字是用户在「网络代理」页起的，
    // 地址是后端解析出来的实时值（见 poolItemLabel）。同一格里多条目同名时
    // 地址是唯一能分辨它们的读数，不能只写名字
    items.push({ value: `${POOL_VALUE_PREFIX}${item.id}`, label: poolItemLabel(item) })
  }
  if (pool === null) {
    // 还没读到（首帧 / 自愈 effect 尚未跑完）：给一句「读取中」而不是
    // 「还没有代理」—— 后者会让用户以为池是空的
    items.push({ value: '__hint_pool__', label: '正在读取代理列表…', disabled: true })
  } else if (!poolItems.length) {
    // 池为空 / 读取失败各说明一句：后者是故障（页面侧在节流重试），前者是
    // 「还没配」—— 两种都不该静默成「只有直连」
    items.push({
      value: '__hint_pool__',
      label: poolError() ? '代理列表读取失败（重试中）' : '还没有代理（去「网络代理」页添加）',
      disabled: true,
    })
  }
  if (current === PROXY_CUSTOM_CURRENT) {
    // 补位项：不带动任何写操作（PROXY_CUSTOM_CURRENT 在 applyProxyPick 里被忽略），
    // 只是把当前值原样显示出来
    const prefix = source === 'clash' ? 'Clash 出口：' : source === 'custom' ? '自定义：' : ''
    items.push({ value: PROXY_CUSTOM_CURRENT, label: `${prefix}${label}${broken ? '（不可用）' : ''}` })
  } else if (source === 'pool' && !poolItems.some(item => `${POOL_VALUE_PREFIX}${item.id}` === current)) {
    // 池引用但条目已不在池里（被删 / Clash 侧删了出口）：补位显示当前值
    items.push({ value: current, label: `${label}${broken ? '（不可用）' : ''}` })
  }
  items.push({ value: PROXY_CUSTOM_EDIT, label: '自定义代理…' })

  const title = broken
    ? `代理不可用：${proxy?.error}（转发时会回退直连）；选「自定义代理…」去修改`
    : source === 'clash' || source === 'custom'
      ? `当前：${label}（未经过代理池 —— 可在「网络代理」页把出口同步进池后来这里改选）`
      : `当前：${proxy ? label : '直连'}；选项来自「网络代理」页；「自定义代理…」打开完整设置`
  const selected = items.find(item => item.value === current)

  return (
    <Select value={current} onValueChange={value => void applyProxyPick(account.id, String(value), current)}>
      {/* 代理解析失败时整格标红：旧实现靠 `.cell-proxy.err .select-trigger` 那条 CSS，
          换成组件库的 Select 之后触发器没有 .select-trigger 这个类名（它走 data-slot），
          所以把描边色直接写在工具类上（见最终报告里变死的 CSS） */}
      <SelectTrigger className={cn('w-full', broken && 'border-destructive-bd')}
        title={title} aria-label='出网代理'>
        <SelectValue>{selected?.label || '直连'}</SelectValue>
      </SelectTrigger>
      <SelectContent>
        {items.map(item => (
          <SelectItem key={item.value} value={item.value} disabled={item.disabled}>{item.label}</SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

/* ─── 操作列 ────────────────────────────────── */

/**
 * 操作：签到 / 领套餐 / 领福利 / 余额 / 设置 / ⋯，顺序固定。
 *
 * 顺序按「点的频次」排，签到排头：它是这张表里唯一**每天都会做一次**的动作，
 * 排在第一位让手指有固定的落点 —— 按钮的显隐会随账号状态变，但**顺序不跟着变**。
 * 「设为首选」不在这里：它在 ⋯ 菜单的第二项（行上留一颗按钮去重复隔壁优先级列的
 * 信息，代价是操作列多留 50px，而那 50px 全是从账号列挤出来的）。
 *
 * 签到今天已签过时显示为**「已签到」并置灰**（这天再点也只能拿到上游「今天已签到」）。
 * `disabled` 是真的禁用属性：这才同时挡住点击与键盘操作，也让读屏念出「不可用」。
 * **禁用账号也渲染签到按钮**：签到与转发是两件事，后端单账号签到路径同样不看 enabled。
 */
export function ActionsCell({ account, atFront }: { account: AccountRecord; atFront: boolean }) {
  const [claimBusy, setClaimBusy] = React.useState(false)
  const [welfareBusy, setWelfareBusy] = React.useState(false)
  const [usageBusy, setUsageBusy] = React.useState(false)
  const checkedIn = checkedInToday(account)
  const canCheckin = supportsCheckin(account)
  const canUsage = supportsUsage(account)
  const canClaim = supportsClaim(account)
  const canWelfare = supportsWelfare(account)
  const welfareTaken = welfareStateOf(account)
  const checkinFailed = checkinErrorOf(account.id)

  async function claim(): Promise<void> {
    // 一次领取要拖一次滑块，重复点击会开出第二个验证码流程（共用的求解器一次只允许
    // 一个，后发起的那轮会把前一轮作废）—— 流程期间禁用这颗按钮
    setClaimBusy(true)
    try {
      await startZcodeClaim(account.id)
    } finally {
      setClaimBusy(false)
    }
  }

  async function welfare(): Promise<void> {
    // 领取是外部服务的**写操作**：流程期间全程禁用，否则连点会发两次 claim。
    setWelfareBusy(true)
    try {
      await startCodeArtsWelfare(account.id)
    } finally {
      setWelfareBusy(false)
    }
  }

  return (
    <div className='acct-actions'>
      {canCheckin ? (
        checkedIn ? (
          <Button variant='outline' size='xs' disabled title={checkinDoneTitle(account)}>已签到</Button>
        ) : (
          // 上一次失败的原因挂在这颗按钮的 title 上（toast 几秒就没了，
          // 而「为什么没签上」要能复看）—— 签到没有明细面板，见 accounts-data.ts
          <Button variant='outline' size='xs'
            title={checkinFailed ? `上次签到失败：${checkinFailed}（点此重试）` : '为该账号签到'}
            onClick={() => void runCheckin(account.id)}>
            签到
          </Button>
        )
      ) : null}
      {canClaim ? (
        // 按钮**不因「今天领过」置灰**：同一个账号可能同时挂着几份可领套餐
        // （活动大额包 + 每日包），而上游的「已领取过」是按套餐判的 —— 领了 A
        // 之后 B 照样能领。今天领过没落在悬停提示里，逐份的状态（哪几份已领、
        // 还能选哪份）由弹窗给出，见 ui/zcode-claim.js。
        <Button variant='outline' size='xs' disabled={claimBusy}
          title={claimedToday(account)
            ? claimDoneTitle(account)
            : '探测并领取官方限时体验套餐（每天一期，需要过一次人机验证）'}
          onClick={() => void claim()}>领套餐</Button>
      ) : null}
      {/* CodeArts 的「领福利」：与上面那颗「领套餐」是**两件事**（判据位不同、流程也不同
          —— 本家不要验证码，但领取前有一次只读探测、领取后有一次回读确认）。
          今天已经到账（台账 `accepted`）时显示「已领」并置灰，与签到那颗同一套语义：
          再点也只是让后端回一句「已领取并确认」，留着可点会让人以为还能再领一次。
          **试过但没到账**不置灰 —— 手动点击在后端是绕过限流闸的（那条闸只管自动那一类），
          幂等键按活动存而不是按轮次存，重试不会变成第二笔领取。 */}
      {canWelfare ? (
        welfareTaken.today && welfareTaken.accepted ? (
          <Button variant='outline' size='xs' disabled title={welfareDoneTitle(welfareTaken)}>已领</Button>
        ) : (
          <Button variant='outline' size='xs' disabled={welfareBusy}
            title={welfareTodoTitle(welfareTaken)}
            onClick={() => void welfare()}>领福利</Button>
        )
      ) : null}
      {canUsage ? (
        <Button variant='outline' size='xs' disabled={usageBusy}
          title='查询该账号剩余余额（读数显示在余额列）'
          onClick={() => {
            setUsageBusy(true)
            void queryUsageOnce(account.id).finally(() => setUsageBusy(false))
          }}>余额</Button>
      ) : null}
      <Button variant='outline' size='xs' title='备注名 / 启用 / 代理'
        onClick={() => openSettingsDialog(account.id)}>设置</Button>
      <MoreMenu account={account} atFront={atFront} />
    </div>
  )
}

/* ─── ⋯ 菜单 ───────────────────────────────── */

/**
 * ⋯ 菜单（原先是「点击才把 div.more-menu 插进 .cell-actions」的命令式实现，还要自己
 * 量几何决定向上还是向下弹）。现在走组件库的 Popover：锚点、翻转、贴边、点外部关闭、
 * Esc、焦点归位都由 Base UI 的 floating-ui 那层负责，滚动时自动跟位。
 *
 * 菜单按「对转发的影响面」从大到小排：启用/禁用最重，故在最前；「设为首选」只改队列
 * 顺序（不改启用状态），排在它之后；「并发上限」是账号属性（标签里带当前值）；
 * 「删除账号」同样最重，排在最后并加一条分隔线。首尾两项都标 danger：它们会立刻改变
 * 转发可用性。
 * 「刷新 Token」按 `hasRefreshToken` 决定（桌面端账号的记录里不落 refreshToken，
 * 所以这一项对它不出现 —— 手动刷新走的是「不过期就原样返回」的路径）。
 */
export function MoreMenu({ account, atFront }: { account: AccountRecord; atFront: boolean }) {
  const [open, setOpen] = React.useState(false)
  const enabled = isEnabled(account)
  const maxConcurrent = Number(account.maxConcurrent) || 0
  const itemClass = 'w-full justify-start px-2 font-normal'
  const dangerClass = `${itemClass} text-destructive hover:text-destructive`

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger render={<Button variant='outline' size='xs' title='更多操作' />}>⋯</PopoverTrigger>
      <PopoverContent align='end' sideOffset={4} className='w-[172px] p-1.5'>
        <div className='flex flex-col gap-0.5'>
          <Button variant='ghost' size='sm' className={dangerClass}
            onClick={() => { setOpen(false); void setAccountEnabled(account.id, !enabled) }}>
            {enabled ? '禁用' : '启用'}
          </Button>
          <Button variant='ghost' size='sm' className={itemClass} disabled={atFront}
            title={atFront ? '已在全局队列第一位' : '仅将优先级调整到全局第一位，不改变启用状态'}
            onClick={() => { setOpen(false); shared().wbApp?.runAccountAction?.('switch', account.id) }}>
            设为首选
          </Button>
          <Button variant='ghost' size='sm' className={itemClass}
            title='设置该账号同时最多处理的请求数（0 = 不限制）'
            onClick={() => { setOpen(false); shared().wbAccountConcDialog?.open?.(account) }}>
            并发上限：{maxConcurrent > 0 ? maxConcurrent : '不限'}
          </Button>
          {account.hasRefreshToken ? (
            <Button variant='ghost' size='sm' className={itemClass}
              onClick={() => { setOpen(false); shared().wbApp?.runAccountAction?.('refresh', account.id) }}>
              刷新 Token
            </Button>
          ) : null}
          <div className='my-1 h-px bg-hairline' />
          <Button variant='ghost' size='sm' className={dangerClass}
            title={isDesktopAccount(account)
              ? '删除这条账号记录（不会影响客户端自己的登录态；之后可再点「导入桌面端登录态」加回来）'
              : undefined}
            onClick={() => { setOpen(false); shared().wbApp?.runAccountAction?.('remove', account.id) }}>
            删除账号
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  )
}

/* ─── 展开的明细行（限流 / 签到）───────────────── */

/**
 * 限流明细面板：这个账号**当前限流中的模型**逐行列出 —— 模型名、恢复时间、上游给的
 * 原因，以及「清除标记」动作（单条）与「全部清除」。
 *
 * 为什么值得一整块面板：限流是按模型的（一个账号完全可能 A 模型限流、B 模型正常），
 * 把模型名列出来才能回答「到底是谁把我限了」；而「清除标记」是真实动作，悬浮层里
 * 放不下也点不稳。「清除标记」只作用于本机这份冷却标记（下一次请求若上游仍限流会
 * 再次被标记），所以它是安全且可逆的，不需要二次确认。
 */
function LimitPanel({ account, onClose, onClear }: {
  account: AccountRecord
  onClose: () => void
  onClear: (model: string) => void
}) {
  const entries = activeLimits(account)
  const close = (
    <Button variant='ghost' size='icon-xs' className='panel-close' title='收起' onClick={onClose}>✕</Button>
  )
  if (!entries.length) {
    // 已展开但记录恰好全部过期时给一句中性说明 —— 数据是两次读盘之间变了的，
    // 不该渲染成一块空面板
    return <div className='row-panel limit-panel'>当前没有限流中的模型。{close}</div>
  }
  return (
    <div className='row-panel limit-panel'>
      {close}
      <div className='lp-head'>
        <b>{displayNameOf(account)}</b>
        <span className='muted'>{entries.length} 个模型限流中 · 记录来自上游 429 / 限额码，到恢复时间自动解除</span>
        <Button variant='outline' size='xs' title='清掉该账号全部模型的限流标记'
          onClick={() => onClear('')}>全部清除</Button>
      </div>
      {entries.map(entry => {
        const reset = formatResetText(entry.resetAt)
        const reason = entry.message || (entry.status ? `上游返回 ${entry.status}` : '')
        return (
          <div className='lp-row' key={entry.model}>
            <span className='lp-model' title={entry.model}>{entry.model}</span>
            <Badge variant='warning' shape='tag'>限流中</Badge>
            <span className='lp-reset' title='到恢复时间后自动解除，无需手动操作'>
              {reset === RESET_UNKNOWN ? '恢复时间未知' : `${reset} 恢复`}
            </span>
            <span className='lp-reason' title={reason}>{reason}</span>
            <Button variant='outline' size='2xs'
              title='清掉本机的限流标记，立刻重新尝试该模型（上游若仍在限流会再次被标记）'
              onClick={() => onClear(entry.model)}>清除标记</Button>
          </div>
        )
      })}
    </div>
  )
}

export function PanelsRow({ account, colSpan, limitsOpen, onClear }: {
  account: AccountRecord
  colSpan: number
  /** 展开态由页面从 store 读出来传进来（这一层只负责画） */
  limitsOpen: boolean
  onClear: (id: string, model: string) => void
}) {
  if (!limitsOpen) return null
  return (
    <tr className='acct-panels' data-panels-for={account.id}>
      <td colSpan={colSpan}>
        <LimitPanel account={account} onClose={() => setPanelOpen(account.id, 'limits', false)}
          onClear={model => onClear(account.id, model)} />
      </td>
    </tr>
  )
}

/* ─── 表头 ──────────────────────────────────── */

/**
 * 表头行。**字面量 JSX、顺序固定、不随任何状态变化**：列的显隐与顺序由
 * `wbColSettings.syncStaticHead` 就地重排既有元素（隐藏的列是从 DOM 里摘掉而不是
 * display:none），不能按状态重建 —— `<col>` 上带着列宽拖拽的 inline 宽度，`<th>` 里
 * 插着列宽把手，重建会把两者一起丢掉。React 只在「同一位置、同一类型的子节点」上做
 * 属性 diff，这些 th 的 props 与文本逐字不变，重渲染时一次 DOM 写都不会发生。
 *
 * 各列的对齐类（ta-*）由 syncStaticHead 统一贴（它按用户配置给），所以这里不写 ——
 * 写了反而会与它的结果打架。数据行相反：完全按 visibleColumns() 逐列渲染。
 */
export function TableHead({ namesHidden, allPicked, somePicked, disabled, onToggleAll }: {
  namesHidden: boolean
  allPicked: boolean
  somePicked: boolean
  disabled: boolean
  onToggleAll: (picked: boolean) => void
}) {
  const grip = <span className='col-grip' title='拖动调整列宽（双击还原）' />
  return (
    <thead>
      <tr>
        <th className='cell-pick' data-col='pick'>
          {/* 表头这颗「全选」与批量栏那颗是**同一个选择**（表头入口是表格化之后补的） */}
          <Checkbox id='acct-select-all' checked={allPicked} indeterminate={!allPicked && somePicked}
            disabled={disabled} aria-label='全选当前筛选结果'
            title='全选 / 取消全选当前筛选结果（与批量栏同一个选择）'
            onCheckedChange={next => onToggleAll(next)} />
        </th>
        <th className='cell-priority' data-col='priority'>
          <span className='th-label' title='全局一条队列：数值越小越先用，不分提供商'>
            优先级<span className='th-hint'>全局队列</span>
          </span>
          {grip}
        </th>
        <th className='cell-provider' data-col='provider'>
          <span className='th-label'>提供商</span>{grip}
        </th>
        <th className='cell-account' data-col='account'>
          <span className='th-label'>
            账号
            <NameEyeButton hidden={namesHidden} />
          </span>
          {grip}
        </th>
        <th className='cell-proxy' data-col='proxy'>
          <span className='th-label' title='该账号出网走的代理（Clash 出口 / 自定义 / 直连）；点击可修改'>代理</span>
          {grip}
        </th>
        <th className='cell-connections' data-col='connections'>
          <span className='th-label' title='此刻正在使用这个账号的请求数（含还在下发内容的流式请求）；为 0 时不显示'>连接数</span>
          {grip}
        </th>
        <th className='cell-status' data-col='status'>
          <span className='th-label'>状态</span>{grip}
        </th>
        <th className='cell-limits' data-col='limits'>
          <span className='th-label' title='该账号当前限流中的模型；点徽章看明细'>
            限流<span className='th-hint'>按模型</span>
          </span>
          {grip}
        </th>
        <th className='cell-expiry' data-col='expiry'>
          <span className='th-label'>有效期</span>{grip}
        </th>
        <th className='cell-usage' data-col='usage'>
          <span className='th-label'>余额</span>{grip}
        </th>
        {/* 最后一列不给把手：它绝对定位在右缘，钉在表格右缘会顶出一条横向滚动条 */}
        <th className='cell-actions' data-col='actions'><span className='th-label'>操作</span></th>
      </tr>
    </thead>
  )
}

/**
 * 表头的「隐藏账号名」眼睛（只在账号列出现）：点一下整列名字变星号，
 * 截图 / 演示时不必逐个打码。图标随状态换睁眼 / 闭眼，隐藏生效时常亮主色 ——
 * 「现在处于打码状态」不用悬停就能看出来。
 */
export function NameEyeButton({ hidden }: { hidden: boolean }) {
  const label = hidden ? '显示账号名' : '隐藏账号名（名字显示为星号）'
  return (
    <button type='button' className={`name-eye${hidden ? ' on' : ''}`} title={label} aria-label={label}
      aria-pressed={hidden} onClick={() => toggleNamesHidden()}
      dangerouslySetInnerHTML={{ __html: iconHtml(hidden ? 'eyeOff' : 'eye', 13) }} />
  )
}
