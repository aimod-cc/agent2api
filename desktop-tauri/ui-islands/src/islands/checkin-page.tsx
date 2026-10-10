import { createRoot } from 'react-dom/client'
import * as React from 'react'
import {
  Badge, Button, Checkbox, Input, Progress, Spinner, Switch,
} from '@ui'
import { PROVIDER_ICONS } from './add-provider-pick'
import { formatTime, shared, type OnboardingTask } from './accounts-shared'
import { claimedPlanIdsToday, welfareDoneTitle, welfareStateOf, welfareTodoTitle } from './accounts-domain'
import {
  claimOnboarding, getCheckinStore, groupTone, historyLine, loadCheckinCenter,
  nextRunText, onboardingExpanded, queryOnboarding, runAllCheckin, saveAutoCheckin,
  setAutoTimeDraft, setKeepaliveDraft, signSingleAccount, submitAutoTime,
  submitKeepaliveModels, subscribeCheckinStore, toggleAutoProvider,
  toggleOnboardingExpand, toggleProviderExpand,
  type AutoCheckinState, type CheckinProviderGroup, type CheckinStore,
} from './checkin-state'
import { openZcodePlans } from './accounts-data'
import { t } from '../i18n'

/**
 * 签到中心（React 岛）。
 *
 * ── 这页管什么 ──────────────────────────────────────────────
 * 全部签到类动作的统一入口：每日签到（提供商清单与「能不能签」的判定都在后端，
 * 见 `core::auto_checkin::CHECKIN_PROVIDERS`）、自动签到设置（含 WorkBuddy
 * 国际版日活保活的模型链）、签到历史时间线、新手任务（Loomy / 小浣熊，
 * 惰性查询）与活动福利（CodeArts / ZCode，沿用既有领取流程）。
 * 原型见 prototype/checkin-center.html。
 *
 * ── 数据从哪来 ──────────────────────────────────────────────
 * 快照走 `GET /api/checkin-center` 一次拉全（load）；新手任务的任务状态是上游
 * 查询，拿到账号清单后按需逐账号查询（queryOnboarding）并缓存（checkin-state）。
 * 页面零轮询：签到动作完成后由动作层重拉快照，主状态刷新交给 wbApp.refresh。
 *
 * ── 挂载：面板岛模式，root 直接建在既有的页面区块上 ──────────────
 * 与 accounts-page / docs-page 同一手法（见那两处的文件头）。对外契约
 * `window.wbCheckinPanel = { load }`：app.js 切到本页时调，保证切入这次刷新落地。
 */

/* ─── 常量 ─────────────────────────────────── */

/**
 * 各提供商的一句链路说明（与 core::auto_checkin 模块头的口径一致）。
 * 值是界面文案（中文即键）：在定义处 t()，每日签到行与空账号占位行共用同一份译法。
 */
const PROVIDER_DESC: Record<string, string> = {
  workbuddy: t('腾讯每日签到接口 · 仅国内版'),
  'workbuddy-intl': t('每日活跃任务 · 领取才加积分，保活只维持活跃 · 两者可分开执行'),
  raccoon: t('桌面端每日积分链路'),
  autoclaw: t('官方客户端的每日签到任务'),
  'autoclaw-intl': t('与国内版同一套任务接口 · 站点不同'),
  qoder: t('活动（campaign）领取 · 每天 10:00 刷新'),
  'qoder-intl': t('带设备风控身份领取「每日 100 Credits」 · 需要 UMID 组件'),
  trae: t('SOLO 的 checkin_credits 领取 · 按自然日 0 点刷新'),
  loomy: t('无独立签到接口 · 每天替账号打一次首次登录积分'),
  kuku: t('「免费领积分」的每日任务 · 逐个领取'),
}

/**
 * Qoder 国际版的 UMID 组件提示（展开后的分组详情顶部）。
 *
 * 国际版签到依赖本机的设备风控身份（`Cosy-MachineToken` 三件套），组件来源
 * 三种：本机 Qoder 客户端、qodercli 解压、网关数据目录安装（Linux 一键装）。
 * 这里只读一次状态：装了就说明来源，没装且平台支持安装就给按钮（Linux/Docker
 * 用户唯一能自助的路径）；不支持安装的平台只说明怎么补 —— Windows/macOS 装
 * 客户端或 qodercli 后无需重启网关（组件发现按次执行）。
 */
function UmidHint(): React.ReactElement | null {
  const [state, setState] = React.useState<{ available?: boolean; source?: string | null; installSupported?: boolean; installing?: boolean } | null>(null)
  const load = React.useCallback(async (): Promise<void> => {
    try {
      const response = await fetch('/api/qoder-umid')
      if (response.ok) setState(await response.json())
    } catch { /* 状态是锦上添花，拉不到不挡签到 */ }
  }, [])
  React.useEffect(() => { void load() }, [load])
  if (!state || state.available) return null
  const install = async (): Promise<void> => {
    try {
      setState(prev => prev ? { ...prev, installing: true } : prev)
      const response = await fetch('/api/qoder-umid/install', { method: 'POST' })
      if (!response.ok) {
        const payload = await response.json().catch(() => null)
        shared().wbApp?.toast?.(t('UMID 组件安装失败：{error}', { error: String(payload?.error ?? response.status) }), 'err')
      } else {
        shared().wbApp?.toast?.(t('✅ UMID 组件已安装，下轮签到即可领取国际版积分'))
      }
    } catch (error) {
      shared().wbApp?.toast?.(t('UMID 组件安装失败：{error}', { error: error instanceof Error ? error.message : String(error) }), 'err')
    } finally {
      void load()
    }
  }
  return (
    <div className='flex flex-wrap items-center gap-2 px-[2px] pb-1 text-[12px] text-muted-foreground'>
      <span>{t('国际版签到需要 UMID 组件生成设备风控身份（本机未检测到）。')}</span>
      {state.installSupported
        ? <Button variant='outline' size='sm' disabled={state.installing === true} onClick={() => { void install() }}>
            {state.installing ? t('安装中…') : t('一键安装组件')}
          </Button>
        : <span>{t('安装 Qoder 客户端或 qodercli 后即可（无需重启网关）。')}</span>}
    </div>
  )
}

/**
 * 问号提示全文（沿用 tasks-panel 的 CHECKIN_DESC，签到口径没变）。
 * 整段是一个键：扫描器只认字符串字面量（拼接串 / 模板串提不到键），所以不拆行。
 */
const AUTO_CHECKIN_DESC = t(
  '到点后自动签到勾选提供商的可用账号（WorkBuddy 国内版走每日签到接口，国际版走每日活跃任务：探测活动、领日活奖励、再用免费模型保活；小浣熊走桌面端每日积分链路；AutoClaw 走官方客户端的每日签到任务；Qoder 中国版走活动领取，没被下发活动的账号会得到中性提示；Qoder 国际版要先带设备风控身份（本机 Qoder 客户端 / qodercli 的 UMID 组件生成）才能看到「每日 100 Credits」活动，缺组件时会得到明确的说明而不是报错；Trae 领 SOLO 的每日签到积分（按自然日 0 点重置），上游没对该账号开活动、或凭据里没有设备号时得到的是「未领取 + 具体原因」而不是报错；Loomy 每天替账号打一次首次登录积分；KukuAI 领「免费领积分」的每日任务；OfficeAce 果办一次领当天全部奖励（每日签到 + 新人礼））。错过时点开机后会自动补签，不会因为当时没开机而漏掉。各家签到接口都是幂等的，重复执行不会重复领取。',
)

/* ─── 小组件 ───────────────────────────────── */

/** 新手任务分组的提供商展示名（缺省回落 provider id 本身） */
function onboardingProviderLabel(provider: string): string {
  return provider === 'raccoon' ? '小浣熊' : provider === 'loomy' ? 'Loomy'
    : provider === 'codearts' ? 'CodeArts'
    : provider === 'officeace' ? 'OfficeAce 果办' : provider
}

function useCheckinStore(): CheckinStore {
  const [store, setStore] = React.useState(getCheckinStore())
  React.useEffect(() => subscribeCheckinStore(() => setStore(getCheckinStore())), [])
  return store
}

/** 提供商行的状态徽章（全部已签 / 部分 / 待签 / 无账号） */
function GroupBadge({ group }: { group: CheckinProviderGroup }) {
  const tone = groupTone(group)
  if (tone === 'none') return <Badge variant='outline' shape='tag'>{t('无账号')}</Badge>
  if (tone === 'done') return <Badge variant='success' shape='tag'>{t('已签')}</Badge>
  if (tone === 'part') return <Badge variant='warning' shape='tag'>{t('部分')}</Badge>
  return <Badge variant='brand' shape='tag'>{t('待签')}</Badge>
}

/** 提供商图标：收录过的用真实图标，否则首字母徽章（与添加账号弹窗同一回落） */
function ProviderLogo({ id, label }: { id: string; label: string }) {
  // WorkBuddy 国际版没有专属图标文件，回落到同品牌那张（两家本来就是一条产品线）
  const icon = PROVIDER_ICONS[id === 'workbuddy-intl' ? 'workbuddy' : id]
  return (
    <span className='ck-prov-logo'>
      {icon ? <img src={icon} alt='' /> : (label.slice(0, 1) || '·')}
    </span>
  )
}

/** 展开区里的账号明细行（每日签到） */
function AccountRows({ group }: { group: CheckinProviderGroup }) {
  const signing = getCheckinStore().signing
  // 在途判定按 `${id}:${mode}`：三颗按钮各自的转圈互不牵连
  const busy = (id: string, mode: string) => signing.has(`${id}:${mode}`)
  return (
    <table className='ck-table'>
      <thead>
        <tr>
          <th>{t('账号')}</th>
          <th>{t('今日签到')}</th>
          <th>{t('上次签到时间')}</th>
          <th aria-label={t('操作')} />
        </tr>
      </thead>
      <tbody>
        {group.accounts.map(account => {
          // WorkBuddy 国际版执行的是「每日活跃任务」：三颗按钮对应三档手动粒度
          // （保活 / 领取 / 保活+领取）—— 领取才会加积分，保活只维持活跃，
          // 分开是当初一颗按钮混做两件事留下的教训。
          // 判据用**分组 id** 而不是账号的 `edition`：Qoder 国际版拆家后公开
          // 形态同样带 `edition: "intl"`，按 edition 判会把它的账号派去
          // WorkBuddy 的日活接口（上游稳定 400「该账号不是 WorkBuddy 国际版
          // 账号」）。只有 `workbuddy-intl` 这一组执行日活链。
          const dailyActivity = group.id === 'workbuddy-intl'
          return (
            <tr key={account.id}>
              <td>{account.name || account.id}</td>
              <td>
                {account.checkedInToday
                  ? <Badge variant='success' shape='tag'>{t('已签')}</Badge>
                  : <Badge variant='brand' shape='tag'>{t('待签')}</Badge>}
              </td>
              <td className='text-subtle'>{account.checkinAt ? formatTime(account.checkinAt) : '—'}</td>
              <td className='text-right'>
                {dailyActivity ? (
                  <span className='inline-flex items-center gap-1'>
                    <Button
                      size='sm' variant='ghost'
                      disabled={busy(account.id, 'keepalive')}
                      title={t('用免费模型维持账号活跃 · 不领取奖励')}
                      onClick={() => void signSingleAccount(account.id, 'keepalive')}
                    >
                      {busy(account.id, 'keepalive') ? t('保活中…') : t('保活')}
                    </Button>
                    <Button
                      size='sm' variant='ghost'
                      disabled={busy(account.id, 'claim')}
                      title={t('探测活动并领取日活奖励 · 不做保活')}
                      onClick={() => void signSingleAccount(account.id, 'claim')}
                    >
                      {busy(account.id, 'claim') ? t('领取中…') : t('领取')}
                    </Button>
                    <Button
                      size='sm' variant='ghost'
                      disabled={busy(account.id, 'full')}
                      title={t('探测 + 领取 + 保活 · 与定时签到同一套组合')}
                      onClick={() => void signSingleAccount(account.id, 'full')}
                    >
                      {busy(account.id, 'full') ? t('执行中…') : t('保活+领取')}
                    </Button>
                  </span>
                ) : (
                  <Button
                    size='sm'
                    variant='ghost'
                    disabled={busy(account.id, 'checkin')}
                    onClick={() => void signSingleAccount(account.id)}
                  >
                    {busy(account.id, 'checkin')
                      ? t('签到中…')
                      : account.checkedInToday ? t('重签') : t('签到')}
                  </Button>
                )}
              </td>
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}

/** 每日签到卡的一行（可展开看账号明细） */
function ProviderRow({ group, expanded }: { group: CheckinProviderGroup; expanded: boolean }) {
  const hasAccounts = group.totalCount > 0
  return (
    <div className={expanded ? 'ck-prov-row open' : 'ck-prov-row'}>
      <div
        className='ck-prov-main'
        onClick={() => hasAccounts && toggleProviderExpand(group.id)}
        onKeyDown={event => {
          if (hasAccounts && (event.key === 'Enter' || event.key === ' ')) {
            event.preventDefault()
            toggleProviderExpand(group.id)
          }
        }}
        role={hasAccounts ? 'button' : undefined}
        tabIndex={hasAccounts ? 0 : -1}
        aria-expanded={hasAccounts ? expanded : undefined}
      >
        <ProviderLogo id={group.id} label={group.label} />
        <div className='ck-prov-info'>
          <div className='ck-prov-name'>{group.label}</div>
          <div className='ck-prov-desc'>{PROVIDER_DESC[group.id] ?? ''}</div>
        </div>
        <div className='ck-prov-right'>
          <span className='ck-prov-count'>
            {hasAccounts ? t('{total} 账号 · {done} 已签', { total: group.totalCount, done: group.doneCount }) : '—'}
          </span>
          <GroupBadge group={group} />
          {hasAccounts ? <span className='ck-chev' aria-hidden>›</span> : null}
        </div>
      </div>
      {expanded ? (
        <div className='ck-prov-detail'>
          {group.id === 'qoder-intl' ? <UmidHint /> : null}
          <AccountRows group={group} />
        </div>
      ) : null}
    </div>
  )
}

/** 新手任务一行（含惰性查询 / 一键领取 / 可展开收起的行级任务清单） */
function OnboardingRow({ row }: { row: { id: string; name: string; provider?: string } }) {
  const provider = row.provider ?? 'loomy'
  const cache = getCheckinStore().onboarding.get(row.id)
  const tasks: OnboardingTask[] = cache?.tasks ?? []
  const unclaimed = cache?.unclaimed ?? 0
  const hasDetail = cache?.status === 'loaded' && tasks.length > 0
  const expanded = hasDetail && onboardingExpanded(row.id)
  return (
    <div className={expanded ? 'ck-prov-row open' : 'ck-prov-row'}>
      <div
        className='ck-prov-main'
        onClick={hasDetail ? () => toggleOnboardingExpand(row.id) : undefined}
        onKeyDown={event => {
          if (hasDetail && (event.key === 'Enter' || event.key === ' ')) {
            event.preventDefault()
            toggleOnboardingExpand(row.id)
          }
        }}
        role={hasDetail ? 'button' : undefined}
        tabIndex={hasDetail ? 0 : -1}
        aria-expanded={hasDetail ? expanded : undefined}
      >
        <ProviderLogo id={provider} label={onboardingProviderLabel(provider)} />
        <div className='ck-prov-info'>
          <div className='ck-prov-name'>{row.name || row.id}</div>
          <div className='ck-prov-desc'>
            {cache?.status === 'loaded'
              ? t('已领 {claimed}/{total} · 累计 {earned} 积分', {
                claimed: tasks.length - unclaimed,
                total: tasks.length,
                earned: cache.total ? `${cache.earned} / ${cache.total}` : cache.earned,
              })
              : t('尚未查询任务状态')}
            {/* 记忆路径没有「查询于」（那是后端结算时刻，不是这一次查询）：
                一次性福利领完后按「结算于」如实交代这份结果从哪来 */}
            {cache?.settled && cache.settledAt
              ? ` · ${t('结算于 {time}', { time: formatTime(cache.settledAt) })}`
              : cache?.checkedAt ? ` · ${t('查询于 {time}', { time: formatTime(cache.checkedAt) })}` : ''}
          </div>
        </div>
        {/* stopPropagation：右侧按钮不触发行的展开 / 收起（点「查询任务」不该顺手折起清单） */}
        <div className='ck-prov-right' onClick={event => event.stopPropagation()}>
          {cache?.status === 'loaded' && unclaimed > 0
            ? <Badge variant='warning' shape='tag'>{t('{n} 项待领', { n: unclaimed })}</Badge>
            : cache?.status === 'loaded'
              ? <Badge variant='success' shape='tag'>{t('全部领取')}</Badge>
              : cache?.status === 'error'
                ? <Badge variant='destructive' shape='tag'>{t('查询失败')}</Badge>
                : null}
          {/* 手点「查询任务」= 强制实查（refresh）：结算过的账号进页面走记忆、
              零上游，这颗按钮是唯一的"现在去问一次上游"入口（见 checkin-state） */}
          <Button
            size='sm'
            variant='outline'
            disabled={cache?.status === 'loading' || cache?.claiming === true}
            onClick={() => void queryOnboarding(row.id, { expand: true, refresh: true })}
          >
            {cache?.status === 'loading' ? t('查询中…') : t('查询任务')}
          </Button>
          <Button
            size='sm'
            variant='outline'
            disabled={cache?.status !== 'loaded' || unclaimed === 0 || cache?.claiming === true}
            onClick={() => void claimOnboarding(row.id)}
          >
            {cache?.claiming ? t('领取中…') : t('一键领取（{n}）', { n: unclaimed })}
          </Button>
          {hasDetail ? <span className='ck-chev' aria-hidden>›</span> : null}
        </div>
      </div>
      {cache?.status === 'error' ? (
        <div className='ck-fold-note'>{t('查询失败：{error}', { error: cache.error ?? '' })}</div>
      ) : null}
      {expanded ? (
        <div className='ck-prov-detail'>
          {tasks.map(task => (
            <div className='ck-task-line' key={task.key}>
              <span className={task.done ? 'ck-task-tick done' : 'ck-task-tick'}>
                {task.done ? '✓' : '·'}
              </span>
              <span>{task.title}</span>
              <span className='ck-task-pts'>+{task.points}</span>
              <span className='ck-task-st'>
                {task.claiming
                  ? <Badge variant='warning' shape='tag'>{t('领取中')}</Badge>
                  : task.done
                    ? <Badge variant='success' shape='tag'>{t('已领取')}</Badge>
                    : task.blocked
                      ? <Badge variant='outline' shape='tag'>{t('暂不可领')}</Badge>
                      : task.error
                        ? <Badge variant='destructive' shape='tag'>{task.error}</Badge>
                        : <Badge variant='brand' shape='tag'>{t('可领取')}</Badge>}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}

/**
 * 活动福利一行（CodeArts / ZCode；领取沿用既有流程，本页只提供入口）。
 *
 * ── 「已领取」标记（CodeArts）───────────────────────────────
 * 快照的福利行带着本地领取台账（`row.welfare`，后端 checkin_center 落盘事实），
 * 用账号页同款的 `welfareStateOf` 判「今天已领取且已受理」—— 判据只有一处，
 * 两页不会一个说领了一个说没领。已领时按钮置灰、行上加绿徽章：再点也只是
 * 让后端回一句「已领取并确认」，留着可点会让人以为还能再领一次。
 * **试过但没到账不置灰**：手动点击在后端是绕过限流闸的，重试不会变成第二笔领取。
 *
 * ── ZCode 那行的状态也是逐份的（`claimPlans`）───────────────
 * 「已领取过」在上游是按**套餐**判的（领了 A 之后 B 照样能领），所以这一行不按
 * 「今天领过没」把入口堵死，而是：行上标出**今天已领几份 / 今天未领**（台账判据
 * 复用账号页域层的 `claimedPlanIdsToday`，北京时间的日界只在那一处），按钮始终
 * 开着 —— 点开套餐明细弹窗（`wbZcodePlans`）逐份看：可领取的、名下的（含未生效
 * 与已过期）都在那里，领取也在那里。旧版点这里直接进原生确认框，既看不出逐份
 * 状态、也显示不了刚领到还没生效的套餐。
 */
function WelfareRow({ row, kind }: {
  row: { id: string; name: string; welfare?: unknown; claimAt?: number | null; claimPlans?: Record<string, number> | null }
  kind: 'welfare' | 'plan'
}) {
  const state = kind === 'welfare'
    ? welfareStateOf({ welfare: row.welfare } as never)
    : null
  const taken = state !== null && state.today && state.accepted
  // ZCode 的逐份领取台账（只算今天、按北京时间判；`row.claimPlans` 是后端落盘事实）
  const claimedIds = kind === 'plan'
    ? claimedPlanIdsToday({ claimPlans: row.claimPlans } as never)
    : []
  const claimed = claimedIds.length > 0
  /** 领取完成后刷新快照与主状态（账号页的余额读数也在那一轮里跟上） */
  const start = async () => {
    const account = { id: row.id, name: row.name }
    if (kind === 'welfare') {
      await shared().wbCodeArtsWelfare?.start?.(account as never)
      await loadCheckinCenter()
      void shared().wbApp?.refresh?.()
      return
    }
    // ZCode：打开套餐明细弹窗，领取由弹窗内的逐份按钮发起
    // （账台与余额读数弹窗自己从账号页状态里读，见 zcode-plans-modal.tsx）
    openZcodePlans(row.id, row.name)
  }
  return (
    <div className='ck-welfare-row'>
      <ProviderLogo id={kind === 'welfare' ? 'codearts' : 'zcode'} label={kind === 'welfare' ? 'CodeArts' : 'ZCode'} />
      <div className='ck-prov-info'>
        <div className='ck-prov-name'>
          {row.name || row.id}
          {taken ? <Badge variant='success' shape='tag' title={welfareDoneTitle(state!)}>{t('已领取')}</Badge> : null}
          {kind === 'plan' ? (
            claimed
              ? <Badge variant='success' shape='tag' title={t('本地台账里今天（北京时间）领过的套餐：{plans}', { plans: claimedIds.join(t('、')) })}>
                {t('今天已领 {n} 份', { n: claimedIds.length })}
              </Badge>
              : <Badge variant='outline' shape='tag' title={t('本地台账里今天还没领过；活动期内每天发一份新套餐')}>{t('今天未领')}</Badge>
          ) : null}
        </div>
        <div className='ck-prov-desc'>
          {kind === 'welfare'
            ? taken
              ? t('今天（北京时间 {day}）已领取 · 官方确认到账 {n} 项 · 明天可再领', { day: state!.day, n: state!.confirmed })
              : t('运营活动交付（领取 → 确认 → 回读核实）· 领的是套餐赠送积分')
            : claimed
              ? t('点「套餐明细」看领到了哪几份、什么时候生效；活动期内每天可再领')
              : row.claimAt
                ? t('上次领取 {time} · 领取需通过滑块验证码', { time: formatTime(row.claimAt) })
                : t('限时体验套餐（start-plan），活动期内每天一份 · 领取需滑块验证码')}
        </div>
      </div>
      <div className='ck-prov-right'>
        {kind === 'welfare' && taken ? (
          <Button size='sm' variant='outline' disabled title={welfareDoneTitle(state!)}>{t('已领取')}</Button>
        ) : (
          <Button size='sm' variant='outline'
            title={state && !taken ? welfareTodoTitle(state) : undefined}
            onClick={() => void start()}>
            {kind === 'welfare' ? t('去领取') : claimed ? t('套餐明细') : t('去领取')}
          </Button>
        )}
      </div>
    </div>
  )
}

/** 时间线一条 */
function HistoryItem({ entry }: {
  entry: {
    at?: number | null
    reason?: string | null
    succeeded?: number | null
    active?: number | null
    total?: number | null
    failedCount?: number | null
    failed?: string[] | null
  }
}) {
  const failed = Number(entry.failedCount) || 0
  return (
    <div className={failed > 0 ? 'ck-tl-item err' : Number(entry.succeeded) > 0 ? 'ck-tl-item ok' : 'ck-tl-item'}>
      <div className='ck-tl-head'>
        <span>{entry.at ? formatTime(entry.at) : '—'}</span>
        {entry.reason ? <span className='ck-tl-why'>{entry.reason}</span> : null}
      </div>
      <div className='ck-tl-sub'>
        {historyLine(entry)}
        {failed > 0 && Array.isArray(entry.failed) && entry.failed.length > 0
          ? <><br /><span className='fail'>{entry.failed.join(t('；'))}</span></>
          : null}
      </div>
    </div>
  )
}

/** 自动签到设置卡（自 tasks-panel 迁入：开关 / 时刻 / 范围 / 保活链 / 上次执行） */
function AutoCheckinCard({ store }: { store: CheckinStore }) {
  const auto: AutoCheckinState | null = store.snapshot?.auto ?? null
  const enabled = auto?.enabled === true
  const options = Array.isArray(auto?.providerOptions) ? auto!.providerOptions! : []
  const picked = Array.isArray(auto?.providers) ? auto!.providers! : []
  const locked = !auto || store.autoSaving
  const checkinTime = store.autoTimeDraft ?? auto?.time ?? ''
  const last = auto?.lastResult
  // 保活模型链：WorkBuddy 国际版日活任务用的免费模型，按顺序回退；
  // 清空提交 = 恢复缺省链（提示里写明，别让用户以为清空是关掉保活）
  const keepalive = store.snapshot?.keepalive
  const keepaliveModels = Array.isArray(keepalive?.models) ? keepalive!.models : []
  const defaultModels = Array.isArray(keepalive?.defaultModels) ? keepalive!.defaultModels : []
  const keepaliveText = store.keepaliveDraft ?? keepaliveModels.join(t('、'))
  return (
    <section className='panel'>
      <div className='panel-head'>
        <h2>{t('自动签到')}</h2>
        <span className='tip-q' data-tip={AUTO_CHECKIN_DESC}></span>
        {/* id 保留：app.js 的 renderTopbarStatus 会按 id 镜像这枚徽标（data-tone 传语义色） */}
        <Badge
          id='checkin-badge'
          className='ml-auto'
          variant={!auto ? 'destructive' : enabled ? 'success' : 'outline'}
          data-tone={!auto ? 'bad' : enabled ? 'ok' : ''}
        >
          {!auto ? t('不可用') : enabled ? (auto?.lastFiredToday ? t('今日已执行') : t('已开启')) : t('已关闭')}
        </Badge>
      </div>
      <div className='p-4'>
        <div className='ck-set-line'>
          <span className='ck-set-k'>{t('开关')}</span>
          <span className='ck-set-v'>
            <Switch
              checked={enabled}
              disabled={locked}
              // 开关一起提交当前时刻（带上未提交的编辑草稿，与旧实现同款）
              onCheckedChange={next => void saveAutoCheckin({ enabled: next, time: checkinTime }, t('自动签到开关'))}
            />
          </span>
        </div>
        <div className='ck-set-line'>
          <span className='ck-set-k'>{t('每天时刻')}</span>
          <span className='ck-set-v flex items-center gap-2'>
            <Input
              type='time'
              className='w-[96px] max-w-[96px] font-mono tabular-nums'
              value={checkinTime}
              disabled={locked}
              onChange={event => setAutoTimeDraft(event.currentTarget.value)}
              onBlur={event => void submitAutoTime(event.currentTarget.value)}
              onKeyDown={event => {
                if (event.key === 'Enter') event.currentTarget.blur()
              }}
            />
            <span className='text-subtle text-xs'>{t('错过时点开机后会自动补签')}</span>
          </span>
        </div>
        <div className='ck-set-line'>
          <span className='ck-set-k'>{t('签到范围')}</span>
          <span className='ck-set-v ck-range'>
            {options.map(option => (
              <label className='inline-flex items-center gap-1.5 text-[12.5px]' key={option.id}>
                <Checkbox
                  checked={picked.includes(option.id)}
                  disabled={locked}
                  // 提交完整勾选清单（后端校验至少一家），顺序取 providerOptions
                  onCheckedChange={next => void toggleAutoProvider(option.id, next === true)}
                />
                <span>{option.label}</span>
              </label>
            ))}
          </span>
        </div>
        <div className='ck-set-line'>
          <span className='ck-set-k'>{t('下次执行')}</span>
          <span className='ck-set-v'>{nextRunText(auto)}</span>
        </div>
        <div className='ck-set-line'>
          <span className='ck-set-k'>{t('保活模型链')}</span>
          <span className='ck-set-v flex items-center gap-2'>
            <Input
              type='text'
              className='max-w-[300px] font-mono text-xs'
              placeholder={t('WorkBuddy 国际版保活模型，按顺序回退')}
              value={keepaliveText}
              disabled={store.keepaliveSaving}
              onChange={event => setKeepaliveDraft(event.currentTarget.value)}
              onBlur={event => void submitKeepaliveModels(event.currentTarget.value)}
              onKeyDown={event => {
                if (event.key === 'Enter') event.currentTarget.blur()
              }}
            />
          </span>
        </div>
        <div className='ck-note'>
          {t('「保活模型链」是 WorkBuddy 国际版领日活时用来保活的免费模型，按顺序逐个尝试、 第一个成功即止；清空提交恢复缺省（{defaults}）。 当天去重 + 幂等领取：重复执行只会拿到「已领取」，不会重复加分。 自动签到的设置以这里为准，「定时任务」页只保留间隔型任务。', {
            defaults: defaultModels.join(t('、')),
          })}
        </div>
        {last ? (
          <div className='ck-set-line'>
            <span className='ck-set-k'>{t('上次执行')}</span>
            <span className='ck-set-v text-xs leading-6'>
              {last.at ? formatTime(last.at) : '—'}
              {last.reason ? ` · ${last.reason}` : ''}
              {` —— ${t('成功 {n}', { n: Number(last.succeeded) || 0 })}`}
              {(Number(last.active) || 0) > 0 ? ` · ${t('保活 {n}', { n: Number(last.active) })}` : ''}
              {(Number(last.skipped) || 0) > 0 ? ` · ${t('跳过 {n}', { n: Number(last.skipped) || 0 })}` : ''}
              {(Number(last.failedCount) || 0) > 0
                ? <span className='text-destructive'> · {t('失败 {n}', { n: Number(last.failedCount) || 0 })}</span>
                : ''}
            </span>
          </div>
        ) : null}
        <div className='ck-note'>
          {t('当天去重 + 幂等领取：重复执行只会拿到「已领取」，不会重复加分。 自动签到的设置以这里为准，「定时任务」页只保留间隔型任务。')}
        </div>
      </div>
    </section>
  )
}

/** 空账号家的占位行（没有账号时该行不可展开） */
function EmptyProviderRow({ group }: { group: CheckinProviderGroup }) {
  return (
    <div className='ck-prov-row'>
      <div className='ck-prov-main' style={{ cursor: 'default' }}>
        <ProviderLogo id={group.id} label={group.label} />
        <div className='ck-prov-info'>
          <div className='ck-prov-name'>{group.label}</div>
          <div className='ck-prov-desc'>{PROVIDER_DESC[group.id] ?? ''}</div>
        </div>
        <div className='ck-prov-right'>
          <Badge variant='outline' shape='tag'>{t('无账号')}</Badge>
        </div>
      </div>
    </div>
  )
}

/* ─── 页面 ─────────────────────────────────── */

function CheckinPage() {
  const store = useCheckinStore()
  const snapshot = store.snapshot

  // 挂载即拉一次（app.js 切页钩子是主要入口，这里兜「直接刷新落在本页」的场景）
  React.useEffect(() => { void loadCheckinCenter() }, [])

  // 顶栏那块是本页徽标的镜像（app.js 的 renderTopbarStatus 读 #checkin-badge）：
  // 数据一更新就让它跟上，否则要等下一次主状态轮询（20 秒）才同步 —— 与 tasks-panel 同款
  React.useEffect(() => {
    // renderTopbarStatus 不在 accounts-shared 的 SharedWindow.wbApp 声明里，窄读
    const app = window as unknown as { wbApp?: { renderTopbarStatus?: () => void } }
    app.wbApp?.renderTopbarStatus?.()
  }, [store.version])

  if (!store.loaded) {
    return (
      <div className='ck-loading'>
        <Spinner className='size-5' />
        <span>{t('正在读取签到数据…')}</span>
      </div>
    )
  }

  const daily = snapshot?.daily
  const auto = snapshot?.auto
  const history = snapshot?.history ?? []
  const outCount = (daily?.outOfScope ?? []).reduce((sum, item) => sum + (Number(item.count) || 0), 0)
  const picked = auto?.providers ?? []
  const options = auto?.providerOptions ?? []
  const onboardingRows = snapshot?.extras.onboarding ?? []
  const onboardingChecked = onboardingRows.filter(row => store.onboarding.get(row.id)?.status === 'loaded')
  const onboardingUnclaimed = onboardingChecked.reduce((sum, row) => sum + (store.onboarding.get(row.id)?.unclaimed ?? 0), 0)
  const welfareRows = snapshot?.extras.welfare ?? []
  const planRows = snapshot?.extras.plans ?? []

  return (
    <div>
      {store.loadError ? (
        <div className='ck-page-error'>
          <span>{t('读取签到数据失败：{error}', { error: store.loadError })}</span>
          <Button size='sm' variant='outline' onClick={() => void loadCheckinCenter()}>{t('重试')}</Button>
        </div>
      ) : null}

      {/* ── 总览卡条 ── */}
      <div className='ck-stats'>
        <div className='ck-stat'>
          <div className='ck-stat-label'>{t('今日签到进度')}</div>
          <div className='ck-stat-value'>
            {daily ? <> {daily.todayDone}<small> / {daily.todayEligible}</small></> : <span className='ck-pending'>—</span>}
          </div>
          <Progress className='ck-stat-bar' value={daily && daily.todayEligible > 0 ? (daily.todayDone / daily.todayEligible) * 100 : 0} />
          <div className='ck-stat-foot'>{outCount > 0 ? t('另有 {n} 个账号不参与每日签到', { n: outCount }) : t('全部账号均可签到')}</div>
        </div>
        <div className='ck-stat'>
          <div className='ck-stat-label'>{t('签到范围')}</div>
          <div className='ck-stat-value'>
            {auto ? <>{picked.length}<small> / {t('{n} 家', { n: options.length })}</small></> : <span className='ck-pending'>—</span>}
          </div>
          <div className='ck-stat-foot'>
            {picked.length
              ? options.filter(option => picked.includes(option.id)).map(option => option.label).join(t('、'))
              : t('未勾选任何提供商')}
          </div>
        </div>
        <div className='ck-stat'>
          <div className='ck-stat-label'>{t('下次自动签到')}</div>
          <div className='ck-stat-value'>
            {auto?.enabled === true
              ? <>{auto.time}</>
              : <span className='ck-pending'>{t('未开启')}</span>}
          </div>
          <div className='ck-stat-foot'>
            {auto?.enabled === true
              ? (auto.lastFiredToday ? t('今天已执行') : t('到点自动执行 · 错过会补签'))
              : t('账号需要手动签到')}
          </div>
        </div>
        <div className='ck-stat'>
          <div className='ck-stat-label'>{t('待领新手任务')}</div>
          <div className='ck-stat-value'>
            {onboardingRows.length
              ? <>{onboardingUnclaimed}<small> {t('项')}</small></>
              : <span className='ck-pending'>{t('无')}</span>}
          </div>
          <div className='ck-stat-foot'>
            {onboardingRows.length
              ? t('已有结果 {checked} / 共 {total} 个账号', { checked: onboardingChecked.length, total: onboardingRows.length })
              : t('没有支持新手任务的账号')}
          </div>
        </div>
      </div>

      <div className='ck-layout'>
        {/* ── 左列：三类签到任务 ── */}
        <div className='ck-main'>
          <section className='panel'>
            <div className='panel-head'>
              <h2>{t('每日签到')}</h2>
              <span className='panel-sub'>{t('点击行展开账号明细 · 单账号签到不受范围限制')}</span>
              <Button
                size='sm'
                variant='default'
                className='ml-auto'
                disabled={store.runningAll || (auto?.running === true)}
                onClick={() => void runAllCheckin()}
              >
                {store.runningAll || auto?.running === true ? t('签到中…') : t('立即全部签到')}
              </Button>
            </div>
            <div>
              {(daily?.providers ?? []).map(group =>
                group.totalCount > 0
                  ? <ProviderRow key={group.id} group={group} expanded={store.expanded.has(group.id)} />
                  : <EmptyProviderRow key={group.id} group={group} />,
              )}
              {(daily?.outOfScope ?? []).length > 0 ? (
                <div className='ck-fold-note'>
                  {t('不参与每日签到：{list}。CodeArts 的一次性奖励在上方「新手任务」，它与 ZCode 的每日/套餐福利领取见下方「活动福利」。', {
                    // 列表项里的 provider 名与原因都是后端数据（snapshot 的 outOfScope），原样透出
                    list: daily!.outOfScope
                      .map(item => t('{label} ×{count}（{reason}）', { label: item.label, count: item.count, reason: item.reason }))
                      .join(t('；')),
                  })}
                </div>
              ) : null}
            </div>
          </section>

          <section className='panel'>
            <div className='panel-head'>
              <h2>{t('新手任务')}</h2>
              <span className='panel-sub'>
                {t('一次性福利 · 签到后自动查询并领取，领完记在本机不再查上游 · 点击行展开任务清单')}
              </span>
              {onboardingRows.length > 0 ? (
                <Button
                  size='sm'
                  variant='outline'
                  className='ml-auto'
                  onClick={() => {
                    for (const row of onboardingRows) {
                      // 已结算的账号（快照带回了记忆）不进这一轮：它们的结论不会变，
                      // 「全部查询」的语义是「把还没查的查一遍」。真要重问上游，
                      // 逐行那颗「查询任务」才是强制实查的入口（refresh）。
                      if (row.settled && typeof row.settled === 'object') continue
                      void queryOnboarding(row.id)
                    }
                  }}
                >
                  {t('全部查询')}
                </Button>
              ) : null}
            </div>
            {onboardingRows.length ? (
              <div>
                {onboardingRows.map(row => <OnboardingRow key={row.id} row={row} />)}
              </div>
            ) : (
              <div className='ck-tl-empty'>{t('没有支持新手任务的账号 —— 目前有 Loomy、小浣熊、CodeArts 三家。')}</div>
            )}
          </section>

          <section className='panel'>
            <div className='panel-head'>
              <h2>{t('活动福利')}</h2>
              <span className='panel-sub'>{t('独立链路 · 只提供手动入口 · 不进自动签到')}</span>
            </div>
            {welfareRows.length + planRows.length > 0 ? (
              <div>
                {welfareRows.map(row => <WelfareRow key={row.id} row={row} kind='welfare' />)}
                {planRows.map(row => <WelfareRow key={row.id} row={row} kind='plan' />)}
              </div>
            ) : (
              <div className='ck-tl-empty'>{t('没有可领福利的账号（CodeArts / ZCode 未添加）。')}</div>
            )}
          </section>
        </div>

        {/* ── 右列：自动签到设置 + 最近记录 ── */}
        <div className='ck-side'>
          <AutoCheckinCard store={store} />
          <section className='panel'>
            <div className='panel-head'>
              <h2>{t('最近签到记录')}</h2>
              <Button
                size='sm'
                variant='ghost'
                className='ml-auto'
                onClick={() => {
                  // 预设「自动签到」分类再切页（wbLogsPanel 未进 accounts-shared 的
                  // SharedWindow，这里窄读；与旧定时任务卡的「查看签到日志」同款）
                  const logs = window as unknown as {
                    wbLogsPanel?: { showCategory?: (category: string) => unknown }
                  }
                  void logs.wbLogsPanel?.showCategory?.('checkin')
                }}
              >
                {t('查看日志')}
              </Button>
            </div>
            {history.length ? (
              <div className='ck-tl'>
                {history.map((entry, index) => <HistoryItem key={entry.at ?? index} entry={entry} />)}
              </div>
            ) : (
              <div className='ck-tl-empty'>{t('还没有批量签到记录 —— 点右上角「立即全部签到」，或等自动签到到点执行。')}</div>
            )}
          </section>
        </div>
      </div>
    </div>
  )
}

/* ─── 挂载：接管 index.html 里既有的页面区块 ─────── */

const PAGE_SELECTOR = '.page[data-page="checkin"]'

let mounted = false

/** 把 React root 直接建在页面区块上（不套宿主 div，理由见文件头） */
function mount(): void {
  if (mounted) return
  const section = document.querySelector<HTMLElement>(PAGE_SELECTOR)
  if (!section) return
  mounted = true
  section.replaceChildren()
  createRoot(section).render(<CheckinPage />)
}

declare global {
  interface Window {
    /** 签到中心页（app.js 切页时调 load；数据自持，不随主状态轮询） */
    wbCheckinPanel?: { load(): Promise<void> }
  }
}

// 对外契约：app.js 切到本页时调 load（保证「切入这次刷新一定落地」）
const load = (): Promise<void> => loadCheckinCenter()
window.wbCheckinPanel = { load }

// 脚本排在页面骨架之后（islands/ui.js 在各 section 之后），正常直接挂；
// 万一将来被挪到前面，退化成等 DOM 解析完再挂。
if (document.querySelector(PAGE_SELECTOR)) mount()
else document.addEventListener('DOMContentLoaded', mount, { once: true })

// 首屏自持加载：app.js 的 showPage 在本脚本加载前就执行过（那时 wbCheckinPanel
// 还不存在，切页那次调用落空），用户上次若停在本页，这里补一次 —— 与 tasks-panel 同款
if (shared().wbApp?.currentPage === 'checkin') void load()
