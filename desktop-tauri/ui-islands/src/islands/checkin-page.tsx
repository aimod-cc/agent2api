import { createRoot } from 'react-dom/client'
import * as React from 'react'
import {
  Badge, Button, Checkbox, Input, Progress, Spinner, Switch,
} from '@ui'
import { PROVIDER_ICONS } from './add-provider-pick'
import { formatTime, shared, type OnboardingTask } from './accounts-shared'
import { claimedPlanIdsToday } from './accounts-domain'
import {
  claimOnboarding, getCheckinStore, groupTone, historyLine, loadCheckinCenter,
  nextRunText, onboardingExpanded, queryOnboarding, runAllCheckin, saveAutoCheckin,
  setAutoTimeDraft, signSingleAccount, subscribeCheckinStore, submitAutoTime,
  toggleAutoProvider, toggleOnboardingExpand, toggleProviderExpand,
  type AutoCheckinState, type CheckinProviderGroup, type CheckinStore,
} from './checkin-state'

/**
 * 签到中心（React 岛）。
 *
 * ── 这页管什么 ──────────────────────────────────────────────
 * 全部签到类动作的统一入口：每日签到（提供商清单与「能不能签」的判定都在后端，
 * 见 `core::auto_checkin::CHECKIN_PROVIDERS`）、自动签到设置
 * （自定时任务页迁入）、签到历史时间线、新手任务（Loomy，惰性查询）与活动福利
 * （CodeArts / ZCode，沿用既有领取流程）。原型见 prototype/checkin-center.html。
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

/** 各提供商的一句链路说明（与 core::auto_checkin 模块头的口径一致） */
const PROVIDER_DESC: Record<string, string> = {
  workbuddy: '腾讯每日签到接口 · 仅国内站',
  raccoon: '桌面端每日积分链路',
  autoclaw: '官方客户端的每日签到任务',
  'autoclaw-intl': '与国内版同一套任务接口 · 站点不同',
  qoder: '活动（campaign）领取 · 每天 10:00 刷新 · 仅中国版',
  trae: 'SOLO 的 checkin_credits 领取 · 按自然日 0 点刷新',
  loomy: '无独立签到接口 · 每天替账号打一次首次登录积分',
  kuku: '「免费领积分」的每日任务 · 逐个领取',
}

/** 问号提示全文（沿用 tasks-panel 的 CHECKIN_DESC，签到口径没变） */
const AUTO_CHECKIN_DESC =
  '到点后自动签到勾选提供商的可用账号（WorkBuddy 走每日签到接口，仅限国内版；' +
  '小浣熊走桌面端每日积分链路；AutoClaw 走官方客户端的每日签到任务；' +
  'Qoder 中国版走活动领取，没被下发活动的账号会得到中性提示；' +
  'Trae 领 SOLO 的每日签到积分（按自然日 0 点重置），上游没对该账号开活动、' +
  '或凭据里没有设备号时得到的是「未领取 + 具体原因」而不是报错；' +
  'Loomy 每天替账号打一次首次登录积分；KukuAI 领「免费领积分」的每日任务）。' +
  '错过时点开机后会自动补签，不会因为当时没开机而漏掉。' +
  '各家签到接口都是幂等的，重复执行不会重复领取。'

/* ─── 小组件 ───────────────────────────────── */

function useCheckinStore(): CheckinStore {
  const [store, setStore] = React.useState(getCheckinStore())
  React.useEffect(() => subscribeCheckinStore(() => setStore(getCheckinStore())), [])
  return store
}

/** 提供商行的状态徽章（全部已签 / 部分 / 待签 / 无账号） */
function GroupBadge({ group }: { group: CheckinProviderGroup }) {
  const tone = groupTone(group)
  if (tone === 'none') return <Badge variant='outline' shape='tag'>无账号</Badge>
  if (tone === 'done') return <Badge variant='success' shape='tag'>已签</Badge>
  if (tone === 'part') return <Badge variant='warning' shape='tag'>部分</Badge>
  return <Badge variant='brand' shape='tag'>待签</Badge>
}

/** 提供商图标：收录过的用真实图标，否则首字母徽章（与添加账号弹窗同一回落） */
function ProviderLogo({ id, label }: { id: string; label: string }) {
  const icon = PROVIDER_ICONS[id]
  return (
    <span className='ck-prov-logo'>
      {icon ? <img src={icon} alt='' /> : (label.slice(0, 1) || '·')}
    </span>
  )
}

/** 展开区里的账号明细行（每日签到） */
function AccountRows({ group }: { group: CheckinProviderGroup }) {
  const signing = getCheckinStore().signing
  return (
    <table className='ck-table'>
      <thead>
        <tr>
          <th>账号</th>
          <th>今日签到</th>
          <th>上次签到时间</th>
          <th aria-label='操作' />
        </tr>
      </thead>
      <tbody>
        {group.accounts.map(account => (
          <tr key={account.id}>
            <td>{account.name || account.id}</td>
            <td>
              {account.checkedInToday
                ? <Badge variant='success' shape='tag'>已签</Badge>
                : <Badge variant='brand' shape='tag'>待签</Badge>}
            </td>
            <td className='text-subtle'>{account.checkinAt ? formatTime(account.checkinAt) : '—'}</td>
            <td className='text-right'>
              <Button
                size='sm'
                variant='ghost'
                disabled={signing.has(account.id)}
                onClick={() => void signSingleAccount(account.id)}
              >
                {signing.has(account.id) ? '签到中…' : account.checkedInToday ? '重签' : '签到'}
              </Button>
            </td>
          </tr>
        ))}
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
            {hasAccounts ? `${group.totalCount} 账号 · ${group.doneCount} 已签` : '—'}
          </span>
          <GroupBadge group={group} />
          {hasAccounts ? <span className='ck-chev' aria-hidden>›</span> : null}
        </div>
      </div>
      {expanded ? (
        <div className='ck-prov-detail'>
          <AccountRows group={group} />
        </div>
      ) : null}
    </div>
  )
}

/** 新手任务一行（含惰性查询 / 一键领取 / 可展开收起的行级任务清单） */
function OnboardingRow({ row }: { row: { id: string; name: string } }) {
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
        <ProviderLogo id='loomy' label='Loomy' />
        <div className='ck-prov-info'>
          <div className='ck-prov-name'>{row.name || row.id}</div>
          <div className='ck-prov-desc'>
            {cache?.status === 'loaded'
              ? `已领 ${tasks.length - unclaimed}/${tasks.length} · 累计 ${cache.earned}${cache.total ? ` / ${cache.total}` : ''} 积分`
              : '尚未查询任务状态'}
            {cache?.checkedAt ? ` · 查询于 ${formatTime(cache.checkedAt)}` : ''}
          </div>
        </div>
        {/* stopPropagation：右侧按钮不触发行的展开 / 收起（点「查询任务」不该顺手折起清单） */}
        <div className='ck-prov-right' onClick={event => event.stopPropagation()}>
          {cache?.status === 'loaded' && unclaimed > 0
            ? <Badge variant='warning' shape='tag'>{unclaimed} 项待领</Badge>
            : cache?.status === 'loaded'
              ? <Badge variant='success' shape='tag'>全部领取</Badge>
              : cache?.status === 'error'
                ? <Badge variant='destructive' shape='tag'>查询失败</Badge>
                : null}
          <Button
            size='sm'
            variant='outline'
            disabled={cache?.status === 'loading' || cache?.claiming === true}
            onClick={() => void queryOnboarding(row.id, { expand: true })}
          >
            {cache?.status === 'loading' ? '查询中…' : '查询任务'}
          </Button>
          <Button
            size='sm'
            variant='outline'
            disabled={cache?.status !== 'loaded' || unclaimed === 0 || cache?.claiming === true}
            onClick={() => void claimOnboarding(row.id)}
          >
            {cache?.claiming ? '领取中…' : `一键领取（${unclaimed}）`}
          </Button>
          {hasDetail ? <span className='ck-chev' aria-hidden>›</span> : null}
        </div>
      </div>
      {cache?.status === 'error' ? (
        <div className='ck-fold-note'>查询失败：{cache.error}</div>
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
                  ? <Badge variant='warning' shape='tag'>领取中</Badge>
                  : task.done
                    ? <Badge variant='success' shape='tag'>已领取</Badge>
                    : task.error
                      ? <Badge variant='destructive' shape='tag'>{task.error}</Badge>
                      : <Badge variant='brand' shape='tag'>可领取</Badge>}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}

/** 活动福利一行（CodeArts / ZCode；领取沿用既有流程，本页只提供入口） */
function WelfareRow({ row, kind }: {
  row: { id: string; name: string; claimAt?: number | null; claimPlans?: Record<string, number> | null }
  kind: 'welfare' | 'plan'
}) {
  /** 领取完成后刷新快照与主状态（账号页的余额读数也在那一轮里跟上） */
  const start = async () => {
    const account = { id: row.id, name: row.name }
    if (kind === 'welfare') {
      await shared().wbCodeArtsWelfare?.start?.(account as never)
    } else {
      // 「今天领过哪几份」逐份比对（claimPlans 的日界判定复用账号页的同款实现）
      const claimed = claimedPlanIdsToday({ claimPlans: row.claimPlans } as never)
      await shared().wbZcodeClaim?.start?.(account as never, claimed)
    }
    await loadCheckinCenter()
    void shared().wbApp?.refresh?.()
  }
  return (
    <div className='ck-welfare-row'>
      <ProviderLogo id={kind === 'welfare' ? 'codearts' : 'zcode'} label={kind === 'welfare' ? 'CodeArts' : 'ZCode'} />
      <div className='ck-prov-info'>
        <div className='ck-prov-name'>{row.name || row.id}</div>
        <div className='ck-prov-desc'>
          {kind === 'welfare'
            ? '运营活动交付（领取 → 确认 → 回读核实）· 领的是套餐赠送积分'
            : row.claimAt
              ? `上次领取 ${formatTime(row.claimAt)} · 领取需通过滑块验证码`
              : '限时体验套餐（start-plan），活动期内每天一份 · 领取需滑块验证码'}
        </div>
      </div>
      <div className='ck-prov-right'>
        <Button size='sm' variant='outline' onClick={() => void start()}>
          {kind === 'welfare' ? '去领取' : '去领取（需验证码）'}
        </Button>
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
          ? <><br /><span className='fail'>{entry.failed.join('；')}</span></>
          : null}
      </div>
    </div>
  )
}

/** 自动签到设置卡（自 tasks-panel 迁入：开关 / 时刻 / 范围 / 上次执行） */
function AutoCheckinCard({ store }: { store: CheckinStore }) {
  const auto: AutoCheckinState | null = store.snapshot?.auto ?? null
  const enabled = auto?.enabled === true
  const options = Array.isArray(auto?.providerOptions) ? auto!.providerOptions! : []
  const picked = Array.isArray(auto?.providers) ? auto!.providers! : []
  const locked = !auto || store.autoSaving
  const checkinTime = store.autoTimeDraft ?? auto?.time ?? ''
  const last = auto?.lastResult
  return (
    <section className='panel'>
      <div className='panel-head'>
        <h2>自动签到</h2>
        <span className='tip-q' data-tip={AUTO_CHECKIN_DESC}></span>
        {/* id 保留：app.js 的 renderTopbarStatus 会按 id 镜像这枚徽标（data-tone 传语义色） */}
        <Badge
          id='checkin-badge'
          className='ml-auto'
          variant={!auto ? 'destructive' : enabled ? 'success' : 'outline'}
          data-tone={!auto ? 'bad' : enabled ? 'ok' : ''}
        >
          {!auto ? '不可用' : enabled ? (auto?.lastFiredToday ? '今日已执行' : '已开启') : '已关闭'}
        </Badge>
      </div>
      <div className='p-4'>
        <div className='ck-set-line'>
          <span className='ck-set-k'>开关</span>
          <span className='ck-set-v'>
            <Switch
              checked={enabled}
              disabled={locked}
              // 开关一起提交当前时刻（带上未提交的编辑草稿，与旧实现同款）
              onCheckedChange={next => void saveAutoCheckin({ enabled: next, time: checkinTime }, '自动签到开关')}
            />
          </span>
        </div>
        <div className='ck-set-line'>
          <span className='ck-set-k'>每天时刻</span>
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
            <span className='text-subtle text-xs'>错过时点开机后会自动补签</span>
          </span>
        </div>
        <div className='ck-set-line'>
          <span className='ck-set-k'>签到范围</span>
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
          <span className='ck-set-k'>下次执行</span>
          <span className='ck-set-v'>{nextRunText(auto)}</span>
        </div>
        {last ? (
          <div className='ck-set-line'>
            <span className='ck-set-k'>上次执行</span>
            <span className='ck-set-v text-xs leading-6'>
              {last.at ? formatTime(last.at) : '—'}
              {last.reason ? ` · ${last.reason}` : ''}
              {` —— 成功 ${Number(last.succeeded) || 0}`}
              {(Number(last.skipped) || 0) > 0 ? ` · 跳过 ${Number(last.skipped) || 0}` : ''}
              {(Number(last.failedCount) || 0) > 0
                ? <span className='text-destructive'> · 失败 {Number(last.failedCount) || 0}</span>
                : ''}
            </span>
          </div>
        ) : null}
        <div className='ck-note'>
          当天去重 + 幂等领取：重复执行只会拿到「已领取」，不会重复加分。
          自动签到的设置以这里为准，「定时任务」页只保留间隔型任务。
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
          <Badge variant='outline' shape='tag'>无账号</Badge>
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
        <span>正在读取签到数据…</span>
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
          <span>读取签到数据失败：{store.loadError}</span>
          <Button size='sm' variant='outline' onClick={() => void loadCheckinCenter()}>重试</Button>
        </div>
      ) : null}

      {/* ── 总览卡条 ── */}
      <div className='ck-stats'>
        <div className='ck-stat'>
          <div className='ck-stat-label'>今日签到进度</div>
          <div className='ck-stat-value'>
            {daily ? <> {daily.todayDone}<small> / {daily.todayEligible}</small></> : <span className='ck-pending'>—</span>}
          </div>
          <Progress className='ck-stat-bar' value={daily && daily.todayEligible > 0 ? (daily.todayDone / daily.todayEligible) * 100 : 0} />
          <div className='ck-stat-foot'>{outCount > 0 ? `另有 ${outCount} 个账号不参与每日签到` : '全部账号均可签到'}</div>
        </div>
        <div className='ck-stat'>
          <div className='ck-stat-label'>签到范围</div>
          <div className='ck-stat-value'>
            {auto ? <>{picked.length}<small> / {options.length} 家</small></> : <span className='ck-pending'>—</span>}
          </div>
          <div className='ck-stat-foot'>
            {picked.length
              ? options.filter(option => picked.includes(option.id)).map(option => option.label).join('、')
              : '未勾选任何提供商'}
          </div>
        </div>
        <div className='ck-stat'>
          <div className='ck-stat-label'>下次自动签到</div>
          <div className='ck-stat-value'>
            {auto?.enabled === true
              ? <>{auto.time}</>
              : <span className='ck-pending'>未开启</span>}
          </div>
          <div className='ck-stat-foot'>
            {auto?.enabled === true
              ? (auto.lastFiredToday ? '今天已执行' : '到点自动执行 · 错过会补签')
              : '账号需要手动签到'}
          </div>
        </div>
        <div className='ck-stat'>
          <div className='ck-stat-label'>待领新手任务</div>
          <div className='ck-stat-value'>
            {onboardingRows.length
              ? <>{onboardingUnclaimed}<small> 项</small></>
              : <span className='ck-pending'>无</span>}
          </div>
          <div className='ck-stat-foot'>
            {onboardingRows.length
              ? `已查 ${onboardingChecked.length} / 共 ${onboardingRows.length} 个 Loomy 账号`
              : '没有 Loomy 账号'}
          </div>
        </div>
      </div>

      <div className='ck-layout'>
        {/* ── 左列：三类签到任务 ── */}
        <div className='ck-main'>
          <section className='panel'>
            <div className='panel-head'>
              <h2>每日签到</h2>
              <span className='panel-sub'>点击行展开账号明细 · 单账号签到不受范围限制</span>
              <Button
                size='sm'
                variant='default'
                className='ml-auto'
                disabled={store.runningAll || (auto?.running === true)}
                onClick={() => void runAllCheckin()}
              >
                {store.runningAll || auto?.running === true ? '签到中…' : '立即全部签到'}
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
                  不参与每日签到：{daily!.outOfScope.map(item => `${item.label} ×${item.count}（${item.reason}）`).join('；')}
                  。CodeArts 与 ZCode 的福利领取见下方「活动福利」。
                </div>
              ) : null}
            </div>
          </section>

          <section className='panel'>
            <div className='panel-head'>
              <h2>新手任务</h2>
              <span className='panel-sub'>一次性福利 · 签到后自动查询并领取 · 点击行展开任务清单</span>
              {onboardingRows.length > 0 ? (
                <Button
                  size='sm'
                  variant='outline'
                  className='ml-auto'
                  onClick={() => { for (const row of onboardingRows) void queryOnboarding(row.id) }}
                >
                  全部查询
                </Button>
              ) : null}
            </div>
            {onboardingRows.length ? (
              <div>
                {onboardingRows.map(row => <OnboardingRow key={row.id} row={row} />)}
              </div>
            ) : (
              <div className='ck-tl-empty'>没有 Loomy 账号 —— 新手任务目前只有 Loomy 一家提供。</div>
            )}
          </section>

          <section className='panel'>
            <div className='panel-head'>
              <h2>活动福利</h2>
              <span className='panel-sub'>独立链路 · 只提供手动入口 · 不进自动签到</span>
            </div>
            {welfareRows.length + planRows.length > 0 ? (
              <div>
                {welfareRows.map(row => <WelfareRow key={row.id} row={row} kind='welfare' />)}
                {planRows.map(row => <WelfareRow key={row.id} row={row} kind='plan' />)}
              </div>
            ) : (
              <div className='ck-tl-empty'>没有可领福利的账号（CodeArts / ZCode 未添加）。</div>
            )}
          </section>
        </div>

        {/* ── 右列：自动签到设置 + 最近记录 ── */}
        <div className='ck-side'>
          <AutoCheckinCard store={store} />
          <section className='panel'>
            <div className='panel-head'>
              <h2>最近签到记录</h2>
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
                查看日志
              </Button>
            </div>
            {history.length ? (
              <div className='ck-tl'>
                {history.map((entry, index) => <HistoryItem key={entry.at ?? index} entry={entry} />)}
              </div>
            ) : (
              <div className='ck-tl-empty'>还没有批量签到记录 —— 点右上角「立即全部签到」，或等自动签到到点执行。</div>
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
