/**
 * Agent2API · 「登录 / 添加账号」弹窗：内置家的表单块（视图 + 三个登录引擎的接线）。
 *
 * 替换旧实现里 add-provider-forms.js 的 buildProviderBlock / mountWebLogin /
 * mountSmsLogin / mountOauthLogin / addProviderManual / addProviderDesktop 与
 * add-account.js 里 WorkBuddy 的那一段（账号版本 + 打开方式 + 第三方入口开关）。
 *
 * ── 与旧实现的一处结构差异（刻意的）──────────────────────────
 * 旧实现把「添加方式」做成互斥显隐的分段，但**所有段落始终在 DOM 里**（只切
 * 显隐），因为三个登录引擎都把状态放在自己的闭包里、事件也按 id 绑在那些
 * 节点上（sms-login.js 在 create() 时 addEventListener）。岛上沿用同一手法：
 * 每家的块整块常驻（只有当前选中那家的块可见），块内各段落也只切显隐。
 * 于是「发码 → 切去别的方式 → 切回来 → 提交」时 deviceId 仍在，行为与旧实现一致。
 *
 * 显隐一律切 `hidden` 属性，**不是**行内 `display:none`：这些节点都是组件库的
 * DialogSection / Button，自带带 `!important` 的 `flex` / `inline-flex` 工具类，
 * 行内样式与单类名都压不过它（兜底规则见组件库 globals.css 的 `[hidden][hidden]`）。
 *
 * ── 引擎的容器契约（一个字都不能改）──────────────────────────
 *   web-login.js：按 id 找按钮 / 取消 / 提示三个节点，自己写 disabled / innerHTML /
 *                hidden —— 这三个节点的 id 与旧实现逐字一致，
 *                且**文本子节点是常量**（React 不会去覆盖引擎写进去的内容）。
 *   sms-login.js：create() 时按 id 绑 click，之后按 id 现读输入框的 .value ——
 *                六个 id 逐字一致，输入框是非受控的（React 从不写回 value）。
 *   autoclaw-oauth.js：create() 时抓两个变体按钮的文案、绑 click，同样按 id 读写。
 * 三个引擎建的 DOM 都不进 React 树，也不试图 React 化。
 */

import * as React from 'react'
import {
  Button,
  DialogSection,
  Input,
  SegmentedControl,
  Switch,
  Textarea,
} from '@ui'

import { OrcaModelSelect } from './orca-model-select'
import {
  ADD_METHODS,
  addButtonTextOf,
  desktopImportAvailable,
  fieldIdOf,
  manualNoteOf,
  manualTitleOf,
  maxLengthOf,
  methodsOf,
  type FieldSpec,
  type MethodId,
  type ProviderConfig,
} from './add-account-configs'
import {
  afterAdd,
  addedLabelOf,
  clearFields,
  closeAddModals,
  describeError,
  draftProps,
  loginPrefs,
  postAccount,
  readField,
  shared,
  toast,
  type AccountRecord,
  type Edition,
  type LoginMode,
  type OauthController,
  type WebLoginController,
} from './add-account-bridge'

/**
 * 弹窗里的分段比页面上大一号（旧 #add-modal .add-seg 的 28px 项高 / 12.5px 字号，
 * 那条规则挂在 #add-modal 这个 ID 作用域上，迁到岛后弹窗不再有这个 id）。
 * 用任意变体就地复刻，避免弹窗里的分段掉回页面档位。
 */
export const ADD_SEG_CLASS =
  'max-w-full flex-wrap [&_[data-slot=segmented-item]]:h-7 [&_[data-slot=segmented-item]]:px-3 [&_[data-slot=segmented-item]]:text-[12.5px]'

/** 小浣熊手填段里那个「登录态文件在哪」的链接点了要显示的路径 */
const RACCOON_AUTH_PATH =
  '登录态文件路径：~/.box-agent/config/auth.json（Windows：C:\\Users\\<你的用户名>\\.box-agent\\config\\auth.json）'

/** 手填按钮的忙态：全局一把锁（旧实现的 addBusy 就是模块级，弹窗内互斥） */
let addBusy = false

async function runAdd(setBusy: (value: boolean) => void, task: () => Promise<void>): Promise<void> {
  if (addBusy) return
  addBusy = true
  setBusy(true)
  try {
    await task()
  } finally {
    addBusy = false
    setBusy(false)
  }
}

/** 带行内标记的说明：调用方给的是可信常量（与旧实现的 innerHTML 同口径），纯文本走文本节点 */
function Note({ html, text }: { html?: string; text?: string }): React.ReactElement {
  if (html) return <p dangerouslySetInnerHTML={{ __html: html }} />
  return <p>{text || ''}</p>
}

/* ─── 手填字段 ─────────────────────────────── */

/**
 * `jsonExpand` 字段：解析并把键并进取到的请求体（`false` = 就地拦下，别把
 * 「你粘的不是 JSON」变成后端签名/凭据模块吐出来的一条远端错误）。
 *
 * 只挡两层：解析失败与非对象（`"abc"` / `5` / `[]`）。粘错的常见形态是「多选了
 * 外面的花括号」或「少粘一层」，这两类都在这一层被挡住。
 */
function expandJsonField(field: FieldSpec, value: string, payload: Record<string, unknown>): boolean {
  let parsed: unknown
  try {
    parsed = JSON.parse(value)
  } catch (error) {
    toast(`${field.label} 不是合法 JSON：${describeError(error)}`, 'err')
    return false
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    toast(`${field.label}要粘一个 JSON 对象（形如 {"codearts_provider_credential":{…}}）`, 'err')
    return false
  }
  Object.assign(payload, parsed as Record<string, unknown>)
  return true
}

function ManualSection({
  config,
  region,
  visible,
}: {
  config: ProviderConfig
  region: string
  visible: boolean
}): React.ReactElement {
  const prefix = config.provider
  const [busy, setBusy] = React.useState(false)
  const [hint, setHint] = React.useState('')
  const note = manualNoteOf(config)
  const fieldIds = config.fields.map(field => fieldIdOf(config, field))

  async function submit(): Promise<void> {
    const payload: Record<string, unknown> = { provider: config.provider }
    // 地区（Qoder）：整块共用一个分段控件
    if (config.regionOptions?.length) payload.mode = region
    for (const field of config.fields) {
      const value = readField(fieldIdOf(config, field))
      if (!field.optional && !value) {
        toast(`请填写 ${field.label}`, 'err')
        return
      }
      if (!value) continue
      if (field.jsonExpand) {
        if (!expandJsonField(field, value, payload)) return
        continue
      }
      payload[field.key] = value
    }
    await runAdd(setBusy, async () => {
      try {
        const data = (await postAccount(payload)) as { account?: AccountRecord } | null
        clearFields(fieldIds)
        setHint('')
        await afterAdd(addedLabelOf(data?.account), config.label)
      } catch (error) {
        toast(`添加失败：${describeError(error)}`, 'err')
      }
    })
  }

  return (
    <DialogSection
      hidden={!visible}
      // 小浣熊那条「登录态文件」链接：旧实现是 document 上的委托，这里收在段落上
      onClick={event => {
        if ((event.target as HTMLElement).closest('[data-raccoon-hint]')) {
          event.preventDefault()
          setHint(RACCOON_AUTH_PATH)
        }
      }}
    >
      <h3>{manualTitleOf(config)}</h3>
      <Note {...note} />
      {config.fields.map(field => {
        const id = fieldIdOf(config, field)
        // 必填只在标签上标出（弹窗不是 <form>，原生 required 不生效），真正的拦截在 submit
        const marker = field.optional ? '' : '（必填）'
        return (
          <div key={id} className={`field-row${field.rows ? ' stack' : ''}`}>
            <label htmlFor={id}>
              {field.label}
              {marker}
            </label>
            {field.rows ? (
              <Textarea id={id} rows={field.rows} placeholder={field.placeholder} {...draftProps(id)} />
            ) : (
              <Input
                id={id}
                // 敏感字段渲染成密码框：**只影响显示**（值照常提交、照常被草稿之外
                // 的路径清空）。口令式输入让「密钥是否被遮住」成为一条可自动断言
                // 的界面事实，见 `FieldSpec.secret` 的说明。
                type={field.secret ? 'password' : 'text'}
                data-testid={`${config.provider}-${field.key}`}
                autoComplete={field.secret ? 'off' : undefined}
                maxLength={maxLengthOf(field)}
                placeholder={field.placeholder}
                {...draftProps(id)}
              />
            )}
          </div>
        )
      })}
      <div className='field-row'>
        <Button id={`${prefix}-add-button`} onClick={() => { void submit() }} disabled={busy}>
          {busy ? '添加中…' : addButtonTextOf(config)}
        </Button>
        {/* 引擎不碰这一格：忙态文案在按钮上，这一格只放链接给出的路径与提交失败以外的说明 */}
        <span className='detail' id={`${prefix}-add-hint`}>{hint}</span>
      </div>
    </DialogSection>
  )
}

/* ─── 从本机导入桌面端登录态 ─────────────────── */

function DesktopSection({
  config,
  visible,
}: {
  config: ProviderConfig
  visible: boolean
}): React.ReactElement | null {
  const prefix = config.provider
  const [busy, setBusy] = React.useState(false)
  if (!desktopImportAvailable(config) || !config.desktopNote) return null

  async function submit(): Promise<void> {
    await runAdd(setBusy, async () => {
      try {
        const data = (await postAccount({ provider: config.provider, importDesktop: true })) as
          | { account?: AccountRecord }
          | null
        await afterAdd(data?.account?.name || '', config.label)
      } catch (error) {
        // 读不到客户端登录态时后端给 400 + 明确原因，原样透出即可
        toast(`导入失败：${describeError(error)}`, 'err')
      }
    })
  }

  return (
    <DialogSection hidden={!visible}>
      <h3>从本机导入桌面端登录态</h3>
      <p>{config.desktopNote}</p>
      <div className='field-row'>
        <Button
          id={`${prefix}-desktop-button`}
          title={config.desktopHint}
          onClick={() => { void submit() }}
          disabled={busy}
        >
          {busy ? '导入中…' : '从本机导入桌面端登录态'}
        </Button>
      </div>
    </DialogSection>
  )
}

/* ─── 网页登录（web-login.js）─────────────────── */

function WebLoginSection({
  config,
  mode,
  onModeChange,
  region,
  visible,
}: {
  config: ProviderConfig
  mode: string
  onModeChange: (value: string) => void
  region: string
  visible: boolean
}): React.ReactElement | null {
  const web = config.webLogin
  const prefix = config.provider
  const modeRef = React.useRef(mode)
  const regionRef = React.useRef(region)
  const controllerRef = React.useRef<WebLoginController | null>(null)
  modeRef.current = mode
  regionRef.current = region

  React.useEffect(() => {
    const engine = shared().wbWebLogin
    if (!engine || !web) return
    const modes = web.modes
    // 控制器在块挂载时就建（与旧实现的加载期 create 同一取向）：隐藏状态不影响
    // 主进程状态推送落到它身上的时机，切到这一家时禁用态与文案已经是对的
    controllerRef.current = engine.create({
      provider: prefix,
      buttonId: `${prefix}-web-button`,
      cancelId: `${prefix}-web-cancel`,
      hintId: `${prefix}-web-hint`,
      busyText: web.busyText,
      texts: () => {
        const active = modes?.find(item => item.value === modeRef.current)
        return { button: web.button, hint: active?.hint || web.hint || '' }
      },
      // 传哪个 edition 给壳侧：写死的 edition → 地区分段当前值（Qoder 两站都要登录）
      // → 'cn'（其余家既没有地区级，后端那条链也不读这个字段）
      start: () => shared().workbuddyDesktop?.startLogin(
        web.edition || regionRef.current || 'cn',
        modeRef.current || 'embedded',
        prefix,
      ),
      onSuccess: async () => {
        closeAddModals()
        await shared().wbApp?.refresh?.()
        toast(`✅ ${config.label}账号已添加`)
      },
    }) ?? null
  }, [])

  // 切换打开方式只影响文案（方法列表与所选方法都不变）
  React.useEffect(() => {
    controllerRef.current?.syncTexts()
  }, [mode])

  if (!web) return null
  return (
    <DialogSection hidden={!visible}>
      <h3>网页登录</h3>
      <Note html={web.noteHtml} />
      {web.modes?.length ? (
        <div className='flex flex-col gap-2.5 border-l-2 border-border pl-[11px]'>
          <span className='text-[12.5px] text-subtle'>打开方式</span>
          <SegmentedControl
            aria-label={`${config.label} 网页登录的打开方式`}
            options={web.modes}
            value={mode}
            onValueChange={onModeChange}
          />
        </div>
      ) : null}
      <div className='field-row'>
        {/* 按钮与提示的文本子节点必须是常量：引擎会直接改它们的 textContent */}
        <Button
          id={`${prefix}-web-button`}
          // OrcaRouter 的这条入口在文案上就叫「Connect with OrcaRouter」（配置里给的
          // button），自动化按 data-testid 找它来断言「两种认证方式并列可用」。
          data-testid={`${prefix}-connect`}
          onClick={() => controllerRef.current?.start()}
        >
          {web.button}
        </Button>
        {/* 收起时靠 hidden 属性（不是行内 display）：引擎在 waiting 与空闲之间
            切的就是它（见 web-login.js 的 applyState）。按钮是组件库的 Button，
            inline-flex 工具类带 !important，行内样式压不过 */}
        <Button
          id={`${prefix}-web-cancel`}
          variant='outline'
          hidden
          onClick={() => controllerRef.current?.cancel()}
        >
          取消等待
        </Button>
        <span className='detail' id={`${prefix}-web-hint`}>
          {web.hint || web.modes?.[0]?.hint || ''}
        </span>
      </div>
    </DialogSection>
  )
}

/* ─── 手机验证码登录（sms-login.js）─────────────── */

function SmsSection({
  config,
  visible,
}: {
  config: ProviderConfig
  visible: boolean
}): React.ReactElement | null {
  const sms = config.smsLogin
  const prefix = config.provider

  React.useEffect(() => {
    if (!sms) return
    // DOM 由下面的 JSX 拼好（id 与旧实现逐字一致），交互全在引擎里
    shared().wbSmsLogin?.create({
      provider: prefix,
      onSuccess: data => afterAdd(addedLabelOf((data as { account?: AccountRecord } | null)?.account), config.label),
    })
  }, [])

  if (!sms) return null
  return (
    <DialogSection hidden={!visible}>
      <h3>手机验证码登录</h3>
      <Note
        html={sms.noteHtml}
        text={'用 AutoClaw 账号绑定的手机号登录：点击「获取验证码」，收到短信后填入下方并登录。验证码由本机直接提交给官方接口，界面不显示 token。'}
      />
      <div className='field-row'>
        <label htmlFor={`${prefix}-sms-phone`}>手机号（必填）</label>
        <Input
          id={`${prefix}-sms-phone`}
          type='text'
          maxLength={11}
          placeholder='11 位大陆手机号'
          {...draftProps(`${prefix}-sms-phone`)}
        />
        {/* 引擎在 create() 时就绑上这一颗的 click（文案与禁用态也归它管） */}
        <Button id={`${prefix}-sms-send`} variant='outline'>获取验证码</Button>
      </div>
      <div className='field-row'>
        <label htmlFor={`${prefix}-sms-code`}>验证码（必填）</label>
        {/* 验证码**不进草稿**：引擎在登录成功后会把它清掉（codeNode.value = ''），
            草稿若留着，下次挂载会把这个已经用过的码填回去 —— 手机号与备注名照旧留 */}
        <Input
          id={`${prefix}-sms-code`}
          type='text'
          maxLength={6}
          placeholder='6 位数字验证码'
        />
      </div>
      <div className='field-row'>
        <label htmlFor={`${prefix}-sms-name`}>备注名</label>
        <Input
          id={`${prefix}-sms-name`}
          type='text'
          placeholder='可选，留空则用脱敏手机号'
          {...draftProps(`${prefix}-sms-name`)}
        />
      </div>
      <div className='field-row'>
        <Button id={`${prefix}-sms-submit`}>登录并添加</Button>
        <span className='detail' id={`${prefix}-sms-hint`} />
      </div>
    </DialogSection>
  )
}

/* ─── AutoClaw 国际版的 OAuth 网页登录（autoclaw-oauth.js）─── */

function OauthSection({
  config,
  mode,
  onModeChange,
  visible,
}: {
  config: ProviderConfig
  mode: string
  onModeChange: (value: string) => void
  visible: boolean
}): React.ReactElement | null {
  const oauth = config.oauthLogin
  const prefix = config.provider
  const modeRef = React.useRef(mode)
  const controllerRef = React.useRef<OauthController | null>(null)
  modeRef.current = mode

  const hintOf = (): string =>
    oauth?.modes?.find(item => item.value === modeRef.current)?.hint || oauth?.hint || ''

  React.useEffect(() => {
    const engine = shared().wbAutoclawOauth
    if (!engine || !oauth) return
    controllerRef.current = engine.create({
      provider: prefix,
      // 打开方式现读（用户可能在跑验证码期间又切了那一级），快照会让最后一次切换失效
      mode: () => modeRef.current || 'embedded',
      hint: hintOf,
      // 全程挂着的取消按钮：验证码阶段点它 = 作废滑块等待，等登录阶段点它 = 撤掉壳侧那一轮
      cancelId: `${prefix}-oauth-cancel`,
      // 不传账号名：这条链的账号由网关侧在回调里落库（壳只回 {ok:true}），
      // 传 provider 名当 name 会得到「AutoClaw 国际版账号已添加：AutoClaw 国际版」这种同义重复
      onSuccess: () => afterAdd('', config.label),
    }) ?? null
  }, [])

  React.useEffect(() => {
    controllerRef.current?.syncTexts()
  }, [mode])

  if (!oauth) return null
  return (
    <DialogSection hidden={!visible}>
      <h3>{oauth.title || '网页登录'}</h3>
      <Note html={oauth.noteHtml} />
      {oauth.modes?.length ? (
        <div className='flex flex-col gap-2.5 border-l-2 border-border pl-[11px]'>
          <span className='text-[12.5px] text-subtle'>打开方式</span>
          <SegmentedControl
            aria-label={`${config.label} 网页登录的打开方式`}
            options={oauth.modes}
            value={mode}
            onValueChange={onModeChange}
          />
        </div>
      ) : null}
      <div className='field-row'>
        {/* 两个变体各一颗：上游是两个独立端点、两套账号体系，用下拉会让用户猜。
            点击处理**不在这里**：autoclaw-oauth.js 的 create() 会自己给这三颗绑
            click（旧实现同一分工），这里再绑一次会让 start() 被调两遍 */}
        <Button id={`${prefix}-oauth-zai`}>使用 Zai 账号登录</Button>
        <Button id={`${prefix}-oauth-google`} variant='outline'>使用 Google 账号登录</Button>
        {/* 取消按钮的显隐由 autoclaw-oauth.js 的 paintCancel 管（同样是 hidden 属性） */}
        <Button id={`${prefix}-oauth-cancel`} variant='outline' hidden>
          取消
        </Button>
      </div>
      <div className='field-row'>
        <span className='detail' id={`${prefix}-oauth-hint`}>
          {oauth.modes?.[0]?.hint || oauth.hint || ''}
        </span>
      </div>
    </DialogSection>
  )
}

/* ─── 一家的整块（地区 / 添加方式 / 各段落）─────────── */

export function ProviderBlock({
  config,
  active,
}: {
  config: ProviderConfig
  active: boolean
}): React.ReactElement {
  const initialRegion = config.regionOptions?.[0]?.value ?? ''
  const [region, setRegion] = React.useState(initialRegion)
  const [method, setMethod] = React.useState<MethodId>(() => methodsOf(config, initialRegion)[0])
  const [webMode, setWebMode] = React.useState(config.webLogin?.modes?.[0]?.value ?? '')
  const [oauthMode, setOauthMode] = React.useState(config.oauthLogin?.modes?.[0]?.value ?? '')

  // 地区切换后方法列表重算：收起的那一项可能正是当前选中项，此时退到第一项
  //（否则屏幕上会出现「所有段都藏着、一个可见的选中项也没有」）
  const methods = methodsOf(config, region)
  const effective = methods.includes(method) ? method : methods[0]
  const methodOptions = React.useMemo(
    () => ADD_METHODS.filter(item => methods.includes(item.id)).map(item => ({
      value: item.id,
      label: item.label,
    })),
    [methods.join(',')],
  )

  // OrcaRouter 的两条接入方式是**并列**的（粘贴 API Key / Connect with OrcaRouter）：
  // 同一把 Key 的两种拿法，不该让用户先在一个分段控件里二选一再看另一半。
  // 其余各家照旧走「添加方式」分段。
  const dualAuth = config.provider === 'orcarouter'

  return (
    <div className='add-provider-block' hidden={!active}>
      <div className='contents'>
        {dualAuth ? null : config.regionOptions?.length ? (
          <DialogSection>
            <h3>地区</h3>
            <SegmentedControl
              aria-label={`${config.label} 账号地区`}
              className={ADD_SEG_CLASS}
              options={config.regionOptions}
              value={region}
              onValueChange={setRegion}
            />
          </DialogSection>
        ) : null}

        {dualAuth ? null : (
          <DialogSection>
            <h3>添加方式</h3>
            <SegmentedControl
              aria-label={`${config.label} 账号的添加方式`}
              className={ADD_SEG_CLASS}
              options={methodOptions}
              value={effective}
              onValueChange={value => setMethod(value as MethodId)}
            />
          </DialogSection>
        )}

        {/* 两格并列的宿主：既是两条接入方式的共同锚点（data-testid），
            也是「仪表盘 + 目录」的布局容器。非双认证的家用 `contents`，
            子节点直接参与外层竖排，间距与改动前逐字一致。 */}
        <div
          className={dualAuth ? 'grid grid-cols-[1fr_1fr] items-start gap-4' : 'contents'}
          data-testid={dualAuth ? `${config.provider}-auth-methods` : undefined}
        >
          <WebLoginSection
            config={config}
            mode={webMode}
            onModeChange={setWebMode}
            region={region}
            visible={dualAuth || effective === 'web'}
          />
          <ManualSection config={config} region={region} visible={dualAuth || effective === 'manual'} />
        </div>

        <OauthSection
          config={config}
          mode={oauthMode}
          onModeChange={setOauthMode}
          visible={effective === 'oauth'}
        />
        <SmsSection config={config} visible={effective === 'sms'} />
        <DesktopSection config={config} visible={effective === 'desktop'} />

        {/* 账号级目录：跟着上面那把 Key 走，与「添加方式」那一格同层并列 */}
        {dualAuth ? <OrcaModelSelect provider={config.provider} visible={active} /> : null}
      </div>
    </div>
  )
}

/* ─── WorkBuddy（账号版本 + 网页登录 + 第三方入口开关）───────
 *
 * 结构与其余各家不同：它有两处静态分段（账号版本 / 打开方式）与一个第三方入口
 * 开关，且网页登录的文案随打开方式变。旧实现里这段 DOM 写在 index.html 里、
 * 交互在 add-account.js；迁到岛上后整块在这里，index.html 的那 47 行骨架随之删除。 */

export function WorkBuddyBlock({ active }: { active: boolean }): React.ReactElement {
  // 三处偏好的初值来自模块级 loginPrefs：旧实现里它们住在静态 DOM 上，
  // 关掉弹窗再打开仍是上次的选择（resetAddStep 只复位提供商与步骤）
  const [edition, setEdition] = React.useState<Edition>(loginPrefs.edition)
  const [loginMode, setLoginMode] = React.useState<LoginMode>(loginPrefs.loginMode)
  const [socialRestore, setSocialRestore] = React.useState(loginPrefs.socialRestore)
  const editionRef = React.useRef(edition)
  const modeRef = React.useRef(loginMode)
  const socialRef = React.useRef(socialRestore)
  const controllerRef = React.useRef<WebLoginController | null>(null)
  editionRef.current = edition
  modeRef.current = loginMode
  socialRef.current = socialRestore

  React.useEffect(() => {
    const engine = shared().wbWebLogin
    if (!engine) return
    controllerRef.current = engine.create({
      provider: 'workbuddy',
      buttonId: 'web-login-button',
      cancelId: 'web-login-cancel',
      hintId: 'web-login-hint',
      busyText: '等待网页登录…',
      texts: () => {
        const external = modeRef.current === 'external'
        return {
          button: external ? '在浏览器中打开登录页' : '打开网页登录',
          hint: external
            ? '系统浏览器打开（复用已登录账号）；完成后自动加入列表'
            : '内嵌窗口打开；完成后自动加入列表，关窗即取消等待',
        }
      },
      start: () => shared().workbuddyDesktop?.startLogin(
        editionRef.current,
        modeRef.current,
        'workbuddy',
        socialRef.current,
      ),
      onSuccess: async () => {
        const editionLabel = editionRef.current === 'intl' ? '国际版' : '国内版'
        closeAddModals()
        await shared().wbApp?.refresh?.()
        toast(`✅ 登录成功，${editionLabel}账号已加入列表`)
      },
    }) ?? null
  }, [])

  // 打开方式影响登录按钮与提示的文案（等待中不覆盖，交给引擎的 applyState）
  React.useEffect(() => {
    controllerRef.current?.syncTexts()
  }, [edition, loginMode])

  /** 切版本要联动打开方式：国际版默认系统浏览器、国内版默认内嵌窗口（沿用改造前的联动） */
  function pickEdition(next: Edition): void {
    const mode: LoginMode = next === 'intl' ? 'external' : 'embedded'
    setEdition(next)
    loginPrefs.edition = next
    setLoginMode(mode)
    loginPrefs.loginMode = mode
  }

  function pickLoginMode(next: string): void {
    const mode: LoginMode = next === 'external' ? 'external' : 'embedded'
    setLoginMode(mode)
    loginPrefs.loginMode = mode
  }

  /**
   * 「恢复 Google / GitHub 入口」的可用条件：**国际版 + 内嵌窗口**。
   * 系统浏览器模式下页面跑在用户自己的浏览器里，我们没有注入能力；国内版登录页
   * 根本没有这两个入口（它用微信 / 手机号 / 邮箱 / SSO），壳侧也会忽略这个值。
   * 不满足时置灰，避免给出一个「选了不生效」的假选项。
   */
  const intl = edition === 'intl'
  const embedded = loginMode === 'embedded'
  const socialUsable = intl && embedded
  const socialTitle = socialUsable
    ? '国际版登录页默认只显示邮箱登录，勾选后恢复 Google / GitHub / X 入口'
    : !intl
      ? '国内版登录页没有 Google / GitHub 入口（它用微信 / 手机号 / 邮箱登录）'
      : '只有「内嵌窗口」能恢复第三方入口：系统浏览器里我们无法改动登录页'

  return (
    <div className='add-provider-block' hidden={!active}>
      <DialogSection>
        <h3>账号版本</h3>
        <p>两版账号可同时保存，按账号自动路由。</p>
        <SegmentedControl
          aria-label='账号版本'
          className={ADD_SEG_CLASS}
          options={[
            { value: 'cn', label: '国内版（WorkBuddy）' },
            { value: 'intl', label: '国际版（WorkBuddy AI）' },
          ]}
          value={edition}
          onValueChange={value => pickEdition(value as Edition)}
        />
      </DialogSection>

      <DialogSection>
        <h3>网页登录</h3>
        <p>完成登录后自动加入账号列表，昵称取自上游。</p>
        <div className='field-row'>
          <span className='text-[12.5px] whitespace-nowrap text-subtle'>打开方式</span>
          <SegmentedControl
            aria-label='网页登录的打开方式'
            className={ADD_SEG_CLASS}
            options={[
              { value: 'embedded', label: '内嵌窗口' },
              { value: 'external', label: '系统默认浏览器' },
            ]}
            value={loginMode}
            onValueChange={pickLoginMode}
          />
          <label className='flex items-center gap-2' title={socialTitle}>
            <Switch
              checked={socialRestore}
              disabled={!socialUsable}
              onCheckedChange={next => {
                // 勾选本身只影响发起登录时传给壳侧的值，不需要重建界面
                setSocialRestore(next)
                loginPrefs.socialRestore = next
              }}
            />
            <span className='text-[12.5px] text-subtle'>恢复 Google / GitHub 入口</span>
          </label>
        </div>
        <p>系统浏览器可复用已有登录态（国际版推荐）；内嵌窗口更干净。</p>
        <div className='field-row'>
          {/* 按钮与提示的文本子节点必须是常量：引擎会直接改它们的 textContent */}
          <Button id='web-login-button' onClick={() => controllerRef.current?.start()}>
            打开网页登录
          </Button>
          <Button
            id='web-login-cancel'
            variant='outline'
            hidden
            onClick={() => controllerRef.current?.cancel()}
          >
            取消等待
          </Button>
          <span className='detail' id='web-login-hint'>
            内嵌窗口打开；完成后自动加入列表，关窗即取消等待
          </span>
        </div>
      </DialogSection>
    </div>
  )
}
