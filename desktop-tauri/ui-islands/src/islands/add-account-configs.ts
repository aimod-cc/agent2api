/**
 * Agent2API · 「登录 / 添加账号」弹窗里各家的表单配置（数据层，无 JSX）。
 *
 * 逐字合并旧实现的三份来源：add-provider-forms.js 的 ADD_FORMS、以及
 * add-qoder.js / add-cline.js / add-accio.js / add-zcode.js 四份**纯常量**配置
 * （它们各自是独立文件、注册到 window 上，只被 add-provider-forms.js 读 ——
 * 迁到岛上后同一份数据在一个模块里更省事，因此不再挂 window，见最终报告）。
 *
 * 文案、字段名、上限、方法与裁剪规则都逐字保留：这些是能跑通上游的既有实现，
 * 迁的只是界面外壳。WorkBuddy 不在这张表里 —— 它的块结构特殊（账号版本 +
 * 打开方式 + 第三方入口开关），单独一个组件，见 add-provider-blocks.tsx。
 */

import { shared } from './add-account-bridge'

/* ─── 类型 ───────────────────────────────── */

/** 一个手填字段：key 直接对应请求体键名（inputKey 只改 DOM id 的中间段） */
export type FieldSpec = {
  key: string
  inputKey?: string
  label: string
  optional?: boolean
  rows?: number
  placeholder: string
  /**
   * 整段是一个 JSON（CodeArts 的凭据就是这么落到用户手上的），提交时**逐键并进
   * 请求体**、原始那串文本不留。为什么在前端展开而不是把整串交给后端：
   * `POST /api/accounts` 的各家分派读的是平铺字段（`Credential::from_payload`
   * 认「带外层包装」与「铺平」两种形状，但不认「一个字符串里装着 JSON」），
   * 让后端再多一种形状 = 给所有家共用的路径加一个只有一家走到的分支。
   */
  jsonExpand?: boolean
  /**
   * 敏感字段：渲染成 `type="password"`（**只影响显示**，值照常提交）。
   *
   * 为什么要有这个开关而不是让各家自己传 `type`：字段的 DOM 属性在这里统一
   * 生成（见 `ManualSection`），否则「哪一家哪个字段是密码」这件事会散到视图里；
   * 而它同时是一条**可核验的界面事实** —— 自动化证据据此断言「密钥是被遮住的」，
   * 而不是靠肉眼认。当前只有 OrcaRouter 的 `apiKey` 用它。
   */
  secret?: boolean
}

export type RegionOption = { value: string; label: string }
export type MethodMode = { value: string; label: string; hint?: string }

export type WebLoginConfig = {
  /** 说明里的行内标记（原样注入，与旧实现的 noteOf 同口径） */
  noteHtml?: string
  button: string
  hint?: string
  busyText?: string
  /** 有这一级时多一个「打开方式」分段（没有的家只给内嵌窗口） */
  modes?: MethodMode[]
  /** 这一家的网页登录只在某个地区可用（当前没有家用它，结构上保留） */
  region?: string
  /** 写死的变体选择器（传给壳侧 start_login 的第一个参数） */
  edition?: string
}

export type SmsLoginConfig = { noteHtml?: string }

export type OauthLoginConfig = {
  title?: string
  noteHtml?: string
  hint?: string
  modes?: MethodMode[]
}

export type ProviderConfig = {
  provider: string
  label: string
  /** 手填段的主按钮文案（缺省「添加 X 账号」） */
  addButton?: string
  manualTitle?: string
  manualNote?: string
  manualNoteHtml?: string
  fields: FieldSpec[]
  webLogin?: WebLoginConfig
  smsLogin?: SmsLoginConfig
  oauthLogin?: OauthLoginConfig
  regionOptions?: RegionOption[]
  /** false = 这一家没有「从本机导入桌面端登录态」这一条来源 */
  desktop?: boolean
  desktopNote?: string
  desktopHint?: string
  /** 登录态是 Electron safeStorage 密文、解密要走 DPAPI（仅 Windows） */
  desktopWindowsOnly?: boolean
}

/* ─── 长度上限（与后端逐条对齐，前端先挡一次）────────────── */

/** 备注名长度上限（与后端 truncate_chars(name, 100) 一致） */
export const MAX_NAME_LENGTH = 100
/** uid / deviceId 长度上限（与后端 MAX_USER_UID_LENGTH / MAX_IDENTITY_LENGTH 一致） */
export const MAX_IDENTITY_LENGTH = 256
/** 单行控件的长度上限：备注名按 100，其余标识字段按 256 */
export const maxLengthOf = (field: FieldSpec): number =>
  (field.key === 'name' ? MAX_NAME_LENGTH : MAX_IDENTITY_LENGTH)

/* ─── 各家配置 ─────────────────────────────── */

const RACCOON: ProviderConfig = {
  // raccoon_accounts::add_raccoon_account：token / access_token、refreshToken / refresh_token、name
  provider: 'raccoon',
  label: '小浣熊',
  addButton: '添加小浣熊账号',
  // 网页登录（后端 providers::raccoon::oauth）：官方登录页 + 一次性授权码回调。
  // 只给内嵌窗口，没有「打开方式」这一级 —— 回调是自定义协议深链
  // （office-raccoon://auth/callback），系统浏览器模式下要靠系统注册该协议才回得来
  // （那是小浣熊官方客户端装的，装了才有），给了这个选项只会让用户选完永远等不到回调。
  webLogin: {
    noteHtml: '在<strong>内嵌窗口</strong>里完成官方登录，成功后自动加入账号列表。',
    button: '打开网页登录',
    hint: '内嵌窗口打开；完成后自动加入列表，关窗即取消等待',
    busyText: '等待小浣熊登录完成…',
  },
  manualTitle: '粘贴 token / refreshToken',
  manualNoteHtml: 'refreshToken 可选，填了到期可自动续期；两者都能从<a href="#" class="raccoon-hint-link" data-raccoon-hint>小浣熊客户端登录态文件</a>里取到。',
  fields: [
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用凭证里的账号名' },
    { key: 'token', label: 'token', rows: 3, placeholder: '粘贴 access_token（一长串 JWT）' },
    { key: 'refreshToken', inputKey: 'refresh', label: 'refreshToken', rows: 2, optional: true, placeholder: '可选' },
  ],
  desktopNote: '读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后点「刷新 Token」同步。',
}

const CATPAW: ProviderConfig = {
  // catpaw_accounts::add_catpaw_account：token / accessToken / access_token / auth_token
  // （即 X-Passport-Token）、uid / userId / loginName、name；没有刷新机制
  provider: 'catpaw',
  label: 'CatPaw',
  // 网页登录：美团 passport 授权页 + 上游把 token 推回本机网关的 loopback 回调。
  // 回调落点与「哪个浏览器」无关，因此系统浏览器完全走得通，而且它是内嵌窗口走不通
  // 时的兜底（美团 passport 的扫码 / 第三方账号登录在部分环境下会拒绝内嵌窗口）。
  webLogin: {
    noteHtml: '用 CatPaw 账号完成登录（美团 passport），成功后自动加入账号列表。',
    button: '打开 CatPaw 网页登录',
    busyText: '等待 CatPaw 登录完成…',
    modes: [
      { value: 'embedded', label: '内嵌窗口（推荐）', hint: '内嵌窗口打开；完成后自动加入列表，关窗即取消等待' },
      { value: 'external', label: '系统浏览器', hint: '系统浏览器打开（复用已登录的美团账号）；完成后自动加入列表' },
    ],
  },
  manualNote: 'token 是登录态 Cookie；CatPaw 没有刷新机制，过期后需在客户端重新登录。',
  fields: [
    { key: 'token', label: 'token', rows: 3, placeholder: 'CatPaw 的 X-Passport-Token（登录态 Cookie）' },
    { key: 'uid', label: 'uid', placeholder: '必填，CatPaw 账号标识' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用登录名或 uid' },
  ],
  desktopNote: '读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后重新导入即可。',
  desktopHint: '读取 ~/.meituan-catpaw/auth.json，需已在 CatPaw 客户端登录',
}

/**
 * AutoClaw 两个地区各占一项（与 Cline 两池同一手法）：`autoclaw`（国内版，id 不变
 * —— 存量账号的落盘契约）与 `autoclaw-intl`（国际版）。两项相邻排列。
 *
 * 两地的差别：域名（后端 `autoclaw::region`，前端不体现）；桌面端导入两地都给
 * （auth.json 两地共用、没有地区标记，地区由用户在哪一项下点导入决定）；
 * **登录方式完全不同** —— 国内版只有手机验证码，国际版只有 Zai / Google OAuth。
 */
const AUTOCLAW: ProviderConfig = {
  provider: 'autoclaw',
  label: 'AutoClaw 国内版',
  manualNote: 'token 支持 enc: 前缀（Windows 上自动解密）。未填 refreshToken 无法自动续期。',
  // 国内版**唯一**的官方登录方式（它的登录页不渲染 OAuth 按钮，已核对构建产物）
  smsLogin: {
    noteHtml: '用绑定的手机号登录：点「获取验证码」后填入即可。这是国内版官方唯一的登录方式。',
  },
  fields: [
    { key: 'token', label: 'token', rows: 3, placeholder: '明文 JWT 或 auth.json 里的 enc: 加密值（自动解密）' },
    { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '没有则无法自动续期' },
    { key: 'deviceId', label: 'deviceId', optional: true, placeholder: '可选，续期时带上' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用 userId' },
  ],
  desktopNote: '读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。',
  desktopHint: '读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows',
  // 登录态是 Electron safeStorage 密文，解密要走 DPAPI（仅 Windows），
  // 因此 macOS 上整段收起（理由见 desktopImportAvailable）
  desktopWindowsOnly: true,
}

const AUTOCLAW_INTL: ProviderConfig = {
  // AutoClaw 国际版（autoglm-api.autoglm.ai）：与国内版同一套协议与签名指纹
  // （appId/appKey 两地逐字相同，已实测），只有站点不同。
  provider: 'autoclaw-intl',
  label: 'AutoClaw 国际版',
  manualNote: '国际版与国内版账号体系独立，请填国际版的凭证。未填 refreshToken 无法自动续期。',
  // OAuth 网页登录：国际版**唯一**的登录方式（登录页只渲染 Zai / Google 两个按钮）。
  // 与另外五家的差别：授权地址前有一次强制风控验证码（阿里云滑块），
  // 必须在浏览器里跑完才能拿地址，因此点按钮后会先在本弹窗里弹滑块
  // （见 ui/autoclaw-oauth.js 的文件头）。
  oauthLogin: {
    title: '网页登录（Zai / Google）',
    noteHtml: '点按钮后先过一次滑块验证（官方风控），随后打开登录页，登录完成即自动添加账号。'
      + '<br>这是国际版官方唯一的登录方式；已在客户端登录过的，用「导入桌面端登录态」更快。',
    modes: [
      { value: 'embedded', label: '内嵌窗口（推荐）', hint: '内嵌窗口打开；关窗即取消等待（Google 账号被拒时改用系统浏览器）' },
      { value: 'external', label: '系统浏览器', hint: '系统浏览器打开（复用已登录的 Zai / Google 账号）；完成后自动加入列表' },
    ],
  },
  fields: [
    { key: 'token', label: 'token', rows: 3, placeholder: '明文 JWT（国际版账号的 access token）' },
    { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '没有则无法自动续期' },
    { key: 'deviceId', label: 'deviceId', optional: true, placeholder: '可选，续期时带上' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用 userId' },
  ],
  // 桌面端登录态导入两个地区都给：auth.json 没有地区标记、本机判断不了，
  // 用户在哪一项下点导入就得到哪一家的账号，猜错的后果是可见的上游 401。
  desktopNote: '读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。用 Zai / Google 登录客户端的用户走这一条。',
  desktopHint: '读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows',
  desktopWindowsOnly: true,
}

/** Qoder：没有「桌面端实时登录态」可导入，因此不生成那一段 */
const QODER: ProviderConfig = {
  provider: 'qoder',
  label: 'Qoder',
  desktop: false,
  regionOptions: [
    { value: 'global', label: '国际版' },
    { value: 'cn', label: '中国版' },
  ],
  webLogin: {
    // 国际版 / 中国版都有网页登录：两站是同一套 PKCE 设备授权协议，只有站点主机不同。
    // 因此不设 region 限制 —— 地区分段切到哪一站，这段就登录哪一站。
    noteHtml: '打开官方授权页完成设备码授权，登录的是「地区」所选那一站的账号。',
    button: '打开 Qoder 网页登录',
    busyText: '等待 Qoder 授权完成…',
    modes: [
      { value: 'embedded', label: '内嵌窗口（推荐）', hint: '内嵌窗口用全新环境，多账号互不影响；关窗即取消等待' },
      { value: 'external', label: '系统浏览器', hint: '系统浏览器打开（复用已登录账号）；完成后自动加入列表' },
    ],
  },
  manualTitle: '使用个人访问令牌（PAT）',
  manualNote: '在 Qoder 账号设置 → Integrations 生成 PAT（别填 Google / GitHub 的令牌）。',
  fields: [
    { key: 'pat', label: 'Qoder PAT', rows: 3, placeholder: '粘贴 Qoder 个人访问令牌（pt-…）' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空使用账号昵称或邮箱' },
  ],
}

/**
 * Cline 是两个提供商（Cline Free / Cline Pass）。
 *
 * 为什么不是一个配置带一个「额度池」下拉：上游模型带计费通道前缀
 * （`cline-free/…` / `cline-pass/…`），池是**账号的属性**，混在一起会让
 * 「哪个池能用」在界面上看不出来、同一模型的启停规则跨池互相影响。
 * 按两家建模后各占一个分组、各有一套账号与模型清单。除 id / 标签 / 文案外同构。
 */
function clineForm(spec: { provider: string; label: string; poolNote: string }): ProviderConfig {
  const { provider, label, poolNote } = spec
  return {
    provider,
    label,
    // 桌面登录态本身不属于任何池，两家都能导入（想要两个池都用就在两家各导入一次）
    desktop: true,
    webLogin: {
      // 设备授权登录（WorkOS RFC 8628）：没有回调、没有自定义协议，
      // 就是「打开授权页 → 用户确认 → 网关轮询拿到令牌」，因此两种打开方式都可行。
      noteHtml: '打开授权页完成确认，账号自动加入 <b>' + label + '</b>' + poolNote,
      button: '打开 Cline 授权页',
      busyText: '等待 Cline 授权确认…',
      modes: [
        { value: 'embedded', label: '内嵌窗口（推荐）', hint: '内嵌窗口打开；完成后自动加入列表，关窗即取消等待' },
        { value: 'external', label: '系统浏览器', hint: '系统浏览器打开（复用已登录的 Cline 账号）；完成后自动加入列表' },
      ],
    },
    manualTitle: '填写凭证',
    manualNote: 'refreshToken 可选，填了可自动续期；两者都能从客户端登录态文件取到。'
      + `同一个 Cline 账号两个池都能用，这里归入 ${label}。`,
    fields: [
      { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: '粘贴 workos:… 开头的令牌（不带前缀也会自动补上）' },
      { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '可选，没有则无法自动续期' },
      { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空使用账号姓名、邮箱或令牌指纹' },
    ],
    desktopNote: '读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。'
      + `这一条归入 ${label}。`,
    desktopHint: '读取 ~/.cline/data/settings/providers.json，需已在 Cline 客户端登录',
  }
}

/**
 * Accio 两个地区（国际版 / 国内版）：同一个网关、同一套接口，只有登录站点与
 * x-package-region 头不同。地区不是账号的字段而是身份，因此输出两份配置。
 */
function accioForm(spec: { provider: string; label: string; site: string; siteNote: string }): ProviderConfig {
  const { provider, label, site, siteNote } = spec
  return {
    provider,
    label,
    // OAuth 2.0 授权码 + PKCE：回调落在本机 loopback 端口，与浏览器在哪无关，
    // 因此内嵌窗口与系统浏览器两条路都走得通（与 CatPaw / AutoClaw 国际版同一形态）
    webLogin: {
      noteHtml: `打开 Accio <b>${label}</b>的官方登录页（<code>${site}</code>）并用你的 Accio 账号登录：`
        + '登录成功后官方页面会跳回本机，网关自动用一次性授权码换取凭证并加入账号列表'
        + '（授权码只在本机传给网关，界面不显示明文 token）。',
      button: `打开 Accio ${label}登录页`,
      busyText: `等待 Accio ${label}登录完成…`,
      modes: [
        { value: 'embedded', label: '内嵌窗口（推荐）', hint: '将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待' },
        { value: 'external', label: '系统浏览器', hint: '将用系统默认浏览器打开登录页（会复用浏览器里已登录的 Accio 账号）；完成登录后自动加入账号列表，关掉弹窗即取消等待' },
      ],
    },
    manualTitle: '填写凭证',
    manualNoteHtml: 'accessToken 是 Accio 的登录凭证（一长串不透明 token，不是 JWT）；'
      + 'refreshToken 可选，填了之后到期能自动续期。'
      + `请填写 <b>${label}</b>账号的凭证 —— ${siteNote}`
      + '（最容易拿到的办法：直接用上方的「网页登录」，不需要手工找 token。）',
    fields: [
      { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: '粘贴 Accio 的 accessToken' },
      { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '可选，没有则无法自动续期' },
      { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空使用账号昵称或邮箱' },
    ],
  }
}

/**
 * ZCode 两个地区（国内版 / 国际版）：同一个 zcode 平面（登录 / 领取都在
 * zcode.z.ai），只有推理站点不同，因此输出两份配置。
 */
function zcodeForm(spec: { provider: string; label: string; site: string; planNote: string }): ProviderConfig {
  const { provider, label, site, planNote } = spec
  return {
    provider,
    label,
    // 本家没有「桌面端实时登录态」可导入：凭证在它自己的加密存储里，
    // 没有 auth.json 那种稳定可读的形态（与 Accio 同一处境）
    desktop: false,
    webLogin: {
      // 授权地址由**服务端**给且不回本机：网关拿到一次性授权地址，用户在浏览器里
      // 授权后由 ZCode 服务端记录结果，网关在后台轮询取回凭证。因此两种打开方式
      // 都走得通（与 Accio 同构），但「页面最后提示无法打开 zcode:// 链接」是正常的。
      noteHtml: `打开 ZCode <b>${label}</b>的官方授权页并用你的账号登录。`
        + '授权结果由 ZCode 服务端记录，网关在后台自动取回凭证并加入账号列表。',
      button: `打开 ZCode ${label}授权页`,
      busyText: `等待 ZCode ${label}授权完成…`,
      modes: [
        { value: 'embedded', label: '内嵌窗口（推荐）', hint: '将打开内嵌窗口；授权完成后自动加入账号列表。关掉窗口即取消等待。授权页最后可能提示无法打开 zcode:// 链接，这是正常的 —— 结果已由服务端记下' },
        { value: 'external', label: '系统浏览器', hint: '将用系统默认浏览器打开授权页（会复用浏览器里已登录的 ZCode 账号）；授权完成后自动加入账号列表。页面最后可能提示无法打开 zcode:// 链接，属正常现象' },
      ],
    },
    manualTitle: '填写凭证',
    manualNoteHtml: '本家有两个**互不替代**的凭证，按你要用的功能填，至少填一个：'
      + '<b>编码套餐 API Key</b> 用于转发推理（打 <code>' + site + '</code>）；'
      + '<b>jwt</b> 用于领取套餐与查询余额（打 <code>zcode.z.ai</code> 的 billing 网关，'
      + '官方叫 Coding Plan JWT，是一串三段点分的字符串）。只填 jwt 的账号能领套餐、'
      + '能看余额但不能转发，反之亦然。'
      + `请填写 <b>${label}</b>账号的凭证 —— ${planNote}`
      + '（最容易拿到的办法：直接用上方的「网页登录」—— 它会替你把这个 API Key 换好，两个凭证一起拿到。）',
    fields: [
      {
        key: 'accessToken',
        label: '编码套餐 API Key',
        rows: 3,
        optional: true,
        // 这里不能填 OAuth 登录态：那个串拿去转发会被上游按「OAuth 令牌」那条路
        // 校验并回 401。官方客户端与「网页登录」这条链都是先换成编码套餐 API Key 再用。
        placeholder: '用于转发；形如 apiKey.secret 两段点分（不填则这个账号不能转发）',
      },
      { key: 'jwt', label: 'Coding Plan JWT', inputKey: 'jwt', rows: 3, optional: true, placeholder: '用于领取套餐与查询余额；不填则这个账号不能领取与查余额' },
      { key: 'userId', label: '用户 ID', optional: true, placeholder: '可选；用于生成账号 id 与展示名' },
      { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空自动生成' },
    ],
  }
}

/**
 * CodeArts（华为云 AI 代码助手 / snap-access）。
 *
 * ── 网页登录（OAuth 授权码 + PKCE）────────────────────────
 * 授权地址由网关拼（`providers::codearts::oauth::authorize_url`，参数与官方扩展
 * 逐字对过向量），portal 完成后把浏览器送回**网关自己的端口**上的
 * `http://127.0.0.1:<port>/oauth/callback` —— 路径不是我们能挑的：portal 只认授权
 * 地址里给的 `port`。所以浏览器与网关在同一台机器时两次回调（先 secret+redirect、
 * 再 code）都能落地；不在同一台时浏览器跳的是它自己的 127.0.0.1，到不了网关。
 * 那种部署（网关跑在 NAS / 服务器上）的用法写在下面的提示里，两条通道都通：
 *   · 地址栏里那条 localhost 地址**改成网关地址**再回车 —— 带 `code` 就直接落账；
 *   · 只带 `secret` 时网关会转去走 ticket 轮询通道，代价是**拿不到 refresh token**
 *     （约一小时后要重新登录一次），界面上这句提示是照实说的。
 *
 * ── 凭据为什么是一整块、且只有一个 JSON 框 ──────────────────
 * CodeArts 的凭据是一次 OAuth/PKCE 登录换来的**临时三元组**（AK/SK/STS，实测约
 * 一小时到期），续期还要出示**当初那次登录的 PKCE verifier 与 DPoP 私钥**，
 * 所以少搬一半到期就刷不回来。而用户手上这份数据的**原生形状就是一段 JSON**
 * （官方插件写 `codearts_provider_credential`、CLIProxyAPI 的 auth 文件整份包在
 * 里面）：拆成六个框会把「粘哪一格」变成六次出错机会，`oauth_context` 本身还是
 * 嵌套对象、框里塞不下。因此一个框整份粘，由 `jsonExpand` 解析后铺开。
 */
const CODEARTS: ProviderConfig = {
  provider: 'codearts',
  label: 'CodeArts',
  // 本家**没有**「读本机客户端登录态」这条后端路径（凭据只能靠网页登录或粘贴）。
  // 不显式关掉的话，桌面壳里会出现一个选了之后什么都没有的分段：chip 只看
  // `desktop !== false`，而下面的 desktopNote 缺席就返回空串。
  // 浏览器面板看不到这个洞（platform()==='web' 时整段收起），只有 App 里会露。
  desktop: false,
  webLogin: {
    noteHtml: '打开华为云 CodeArts 的官方授权页登录：登录完成后官方页面会把浏览器带回<b>网关自己的</b> '
      + '<code>/oauth/callback</code>，网关用一次性授权码换取临时凭据并加入账号列表。'
      + '<br>网关跑在另一台机器上时（NAS / 服务器），浏览器跳的是它自己的 127.0.0.1 —— '
      + '把地址栏里那条地址的主机端口改成网关的（如 <code>http://192.168.1.58:3065/oauth/callback?…</code>）再回车即可。',
    button: '打开 CodeArts 授权页',
    busyText: '等待 CodeArts 登录完成…',
    modes: [
      {
        value: 'embedded',
        label: '内嵌窗口（推荐）',
        hint: '将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待',
      },
      {
        value: 'external',
        label: '系统浏览器',
        hint: '将用系统默认浏览器打开授权页（会复用浏览器里已登录的华为云账号）；'
          + '浏览器与网关不在同一台机器时，按上方说明把回调地址改成网关地址再走一遍',
      },
    ],
  },
  manualTitle: '粘贴登录凭据',
  manualNoteHtml: '整份粘贴官方插件 / CLIProxyAPI 落盘的凭据 JSON（形如 '
    + '<code>{"codearts_provider_credential":{…}}</code>，铺平的也行）。'
    + '<br>必填：<code>access_key_id</code>、<code>secret_access_key</code>、<code>security_token</code>。'
    + '<b>要能自动续期，必须连 <code>refresh_token</code> 与 <code>oauth_context</code> 一起粘</b>'
    + ' —— 临时凭据约一小时到期，缺这半块就续不回来，只能重新登录。',
  fields: [
    {
      key: 'credentialJson',
      label: '凭据 JSON',
      rows: 8,
      // 解析后按字段铺开进请求体（后端 `Credential::from_payload` 嵌套/平铺都认）
      jsonExpand: true,
      placeholder: '{"codearts_provider_credential":{"access_key_id":"HSTA…","secret_access_key":"…",'
        + '"security_token":"…","expires_at":"2026-09-27T16:17:00.327Z","domain_id":"…","user_id":"…",'
        + '"user_name":"…","refresh_token":"eyJ…","oauth_context":{…}}}',
    },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空使用凭据里的 user_name' },
  ],
}

/**
 * Trae（字节 AI IDE 的 SOLO 通道）。
 *
 * ── 为什么只有一张卡、没有「地区」下拉 ──────────────────────
 * AutoClaw / Accio / ZCode 的两地是**同一套协议换域名**，所以按地区参数化。
 * Trae 不是：国内 SOLO 走 `trae-api-cn.mchost.guru` 的 `llm_utils_chat`
 * （自定义信封 + 一套自己的 SSE 方言），国际版走 `chat_sessions` → `events`
 * 两步握手、另一个 Origin、流是累积式要还原增量 —— 那是**两套协议**而不是一个
 * 地区的两种拼法。把 `region` 做成这一家的字段，等于让"用哪套协议解释这个账号"
 * 变成账号属性（本仓反复拒绝的那类坑）。国际版将来接入时另立 provider id。
 *
 * ── 为什么回调必须落在本机 ────────────────────────────────
 * 上游对回调地址是**逐字正则校验**的（`^http://127.0.0.1:<port>/authorize$`），
 * 且回调里**没有 state** —— 一轮登录只能靠端口认回来。所以浏览器必须与网关同机；
 * 容器 / 远程部署形态收不到回调，只能在有浏览器的机器上登录后走粘贴那条路。
 */
const TRAE: ProviderConfig = {
  provider: 'trae',
  label: 'Trae',
  // Trae 桌面端的登录态在它自己的加密存储里（与 Accio / ZCode 同一处境），
  // 没有 auth.json 那种稳定可读的形态 —— 给了入口只会稳定失败。
  desktop: false,
  webLogin: {
    noteHtml: '打开 <b>Trae SOLO</b> 的官方授权页（<code>trae.cn</code>）并用你的 Trae 账号登录：'
      + '授权完成后官方页面会跳回<b>本机</b>的一个临时端口，网关自动用一次性授权码换取凭证并加入账号列表'
      + '（授权码只在本机传给网关，界面不显示明文 token）。',
    button: '打开 Trae 授权页',
    busyText: '等待 Trae 授权完成…',
    modes: [
      {
        value: 'embedded',
        label: '内嵌窗口（推荐）',
        hint: '将打开内嵌窗口；授权完成后自动加入账号列表。关掉窗口即取消等待。'
          + '链接 5 分钟内有效，超时或未点就会作废（可重新发起）',
      },
      {
        value: 'external',
        label: '系统浏览器',
        hint: '将用系统默认浏览器打开授权页（会复用浏览器里已登录的 Trae 账号）；'
          + '完成后自动加入账号列表。<b>浏览器必须与网关在同一台机器上</b>'
          + '（回调地址被上游钉成 127.0.0.1 的一个本机端口）',
      },
    ],
  },
  manualTitle: '填写凭证',
  manualNoteHtml: 'accessToken 是 Trae 的 <code>Cloud-IDE-JWT</code>（三段点分），refreshToken 用于到期自动续期'
    + '（本家 <b>refreshToken 一次一换</b>：换发一次旧的即作废，所以两份程序别同时刷同一个账号）。'
    + '<br>手工粘贴时请连 <b>machineId / deviceId</b> 一起填：上游把它们与登录时上传的设备公钥绑在一起判设备，'
    + '凭空换一对会撞 <code>2xxxx</code> 那族设备绑定拒绝。'
    + '<br>没有这三样时的正路是用上方的「网页登录」；容器 / 远程部署形态本机收不到回调，'
    + '只能在有浏览器的机器上登录后把凭据粘进来。',
  fields: [
    { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: 'Cloud-IDE-JWT（三段点分）' },
    { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '可选；填了才能到期自动续期' },
    { key: 'uid', label: '用户 ID', optional: true, placeholder: '可选；用于去重与展示名（留空时网关会问一次上游）' },
    { key: 'machineId', label: 'machineId', optional: true, placeholder: '可选；UUID 形态，与凭据同生共死' },
    { key: 'deviceId', label: 'deviceId', optional: true, placeholder: '可选；纯数字串（上游只收 8–24 位数字）' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空自动用昵称' },
  ],
}

/**
 * OrcaRouter（`orcarouter`）—— 一个 OpenAI 兼容的聚合网关：一个端点后面路由
 * 多家提供方（模型 id 是 `vendor/model` 形态，原样转发）。
 *
 * ── 这一家在界面上是**唯一一家两种认证入口并列**的 provider ─────────
 * 「手填 API Key」与「Connect with OrcaRouter（OAuth 2.0 + PKCE）」两条路
 * 都在这里，且都指向同一个后端落账号入口（`add_orcarouter_account`）。
 * 因此这里的 `fields` 与 `webLogin` 是**并列的两件事**，不是「二选一」——
 * 用户有 Key 就粘，没有就点 Connect（规范要求两条入口都能独立使用）。
 *
 * 两条路最终换回的是同一件东西（一把属于用户账号的长期 API Key），
 * 所以界面上的差别只有 `source` 字段（manual / oauth）与 PKCE 才带的
 * userId / scope。前端的 `secret_masked` 判据就是下面 apiKey 那个
 * `type: 'password'`（新加的 `secret` 字段）—— 密钥永不进草稿、也不进日志。
 */
const ORCAROUTER: ProviderConfig = {
  provider: 'orcarouter',
  label: 'OrcaRouter',
  addButton: '添加 OrcaRouter 账号',
  // 「Connect with OrcaRouter」：后端拼 PKCE 授权地址（S256），浏览器授权完成后
  // 302 回本机 loopback 端口（`/auth/callback-orcarouter`），网关用一次性授权码换
  // 长期 Key 并落账号。这与 Accio 那条链是同一种形态（同一条通用网页登录入口）。
  webLogin: {
    noteHtml: '在你的浏览器里打开 <b>OrcaRouter</b> 的授权页（<code>www.orcarouter.ai/auth</code>）'
      + '并用 OrcaRouter 账号登录：授权完成后同意页会跳回<b>本机</b>的一个临时端口，'
      + '网关用一次性授权码（PKCE S256）换取一把长期 API Key 并加入账号列表'
      + '（授权码只在本机交给网关，界面不显示明文 Key）。'
      + '<br>没有浏览器、或更习惯自己贴 Key 的话，用下面的「粘贴 API Key」——'
      + '两条路等价，换回的都是同一把 Key。',
    button: 'Connect with OrcaRouter',
    hint: '浏览器打开授权页；完成后自动加入列表，取消即放弃等待',
    busyText: '等待 OrcaRouter 授权完成…',
  },
  manualTitle: '粘贴 API Key',
  // 两条入口并列展示：这一段是「有现成 Key 的用户」那条路，明确点出另一条
  manualNoteHtml: '在 OrcaRouter 控制台的密钥管理页创建一把 API Key（形如 <code>sk-orca-…</code>）后粘贴到这里。'
    + '没有现成的 Key、或想用账号授权直接换一把的话，用上方的 '
    + '<b>Connect with OrcaRouter</b>（OAuth 2.0 + PKCE）。'
    + '<br>密钥只在网关侧使用（模型目录与推理都由服务端持 Key 发请求），'
    + '浏览器不会拿到明文；输入框按密码处理，账号列表里也只显示尾号。',
  fields: [
    { key: 'apiKey', label: 'API Key', secret: true, placeholder: 'sk-orca-…' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用 Key 尾号' },
  ],
  // OrcaRouter 是纯云端服务，没有桌面端登录态可读（与 Accio / ZCode 同一处境）
  desktop: false,
}

/** 内置家的表单块，顺序与旧 ADD_FORMS 一致（只影响 DOM 里的块顺序，不影响界面） */
export const BUILTIN_CONFIGS: ProviderConfig[] = [
  RACCOON,
  CATPAW,
  AUTOCLAW,
  AUTOCLAW_INTL,
  QODER,
  // Cline 顺序即界面上「提供商」分段的顺序：免费池在前（无门槛，更常用）
  clineForm({ provider: 'cline-free', label: 'Cline Free', poolNote: '（免费额度池，模型名带 cline-free/ 前缀）。' }),
  clineForm({ provider: 'cline-pass', label: 'Cline Pass', poolNote: '（订阅池，模型名带 cline-pass/ 前缀，需要账号有对应订阅）。' }),
  // Accio 顺序：国际版在前（默认安装的版本）
  accioForm({ provider: 'accio', label: '国际版', site: 'www.accio.com', siteNote: '国际版与国内版是**两套独立的账号**（同一账号体系的两个站点），凭证不通用。' }),
  accioForm({ provider: 'accio-cn', label: '国内版', site: 'www.accio-ai.com', siteNote: '国内版的登录站点是 www.accio-ai.com，与国际版不是同一站。' }),
  // ZCode 顺序：国内版在前（国内网络环境下更常被添加的那个，与后端注册表 PROVIDERS 的排列一致）
  zcodeForm({ provider: 'zcode', label: '国内版', site: 'open.bigmodel.cn', planNote: '国内版与**国际版是两套独立的账号与套餐**，凭证与领取的套餐都不通用。' }),
  zcodeForm({ provider: 'zcode-intl', label: '国际版', site: 'api.z.ai', planNote: '国际版的推理站点是 api.z.ai，与国内版不是同一站；套餐也各自独立。' }),
  // CodeArts（华为云 AI 代码助手）：一家一个 provider，没有地区/额度池之分
  // （region 写死 cn-north-4，与 token 签发地必须一致）。
  CODEARTS,
  // Trae 只有 SOLO 那一家（没有地区分叉，理由见 TRAE 上方那段）
  TRAE,
  // OrcaRouter 排在末尾：它是**聚合网关**（一个端点后面是多家的模型），
  // 与前面每一家「一个上游」的形态都不同，造型上也让分组层次自然
  // —— 与后端注册表 PROVIDERS 的排列一致。
  ORCAROUTER,
]

/** WorkBuddy 的块 id（结构特殊，单独一个组件） */
export const WORKBUDDY_PROVIDER = 'workbuddy'
/** 自定义提供商的块 id（后注册块的 provider 取值） */
export const CUSTOM_PROVIDER = 'custom'

export function configOf(providerId: string): ProviderConfig | undefined {
  return BUILTIN_CONFIGS.find(item => item.provider === providerId)
}

/* ─── 添加方式（分段项）─────────────────────────
 *
 * 文案要短（并排一行，太长会把弹窗挤到换行），详细说明留在各段自己的标题与正文里。
 * 露出哪些项由 methodsOf 按各家配置裁剪。
 *
 * `oauth` 单独一项而不并进「网页登录」：对用户来说两者都是「跳去官方页面登录」，
 * 但交互不同 —— 网页登录点一下就开窗口（等待态由 web-login.js 统一管），而
 * AutoClaw 国际版要先在本弹窗里弹一个阿里云滑块让用户拖完才能拿到授权地址。
 * 并进去会让那个引擎多出「有些家要先跑一段验证码」的分支，且两者的发起时序与
 * 按钮布局都不一样（这一项是两个变体各一个按钮）。 */
export const ADD_METHODS = [
  { id: 'oauth', label: '网页登录（Zai / Google）' },
  { id: 'sms', label: '手机验证码登录' },
  { id: 'web', label: '网页登录' },
  { id: 'manual', label: '填写凭证' },
  { id: 'desktop', label: '导入桌面端登录态' },
] as const

export type MethodId = (typeof ADD_METHODS)[number]['id']

/**
 * 壳的编译目标平台（'macos' / 'windows' / 'linux' / 'web'），由桥接脚本注入。
 *
 * 浏览器直开（没有壳）时拿不到它，退回空串 —— 此时按「不裁剪」处理：
 * 浏览器直连网关本来就用不了这些壳侧功能，多显示一个选项不会误导谁，
 * 而误裁掉一个**本来可用**的功能会让 Windows 用户莫名其妙少一项。
 */
export const platform = (): string => shared().workbuddyDesktop?.platform || ''

/**
 * 桌面端导入在这一家、这个平台是否可用。
 *
 * AutoClaw 要按平台裁掉：它读 %APPDATA%/AutoClaw/auth.json，而那个文件里的 token
 * 是 Electron safeStorage 的密文，要先过 DPAPI（CryptUnprotectData）才能解出密钥
 * —— DPAPI 只有 Windows 有。另外三家（小浣熊 ~/.box-agent、CatPaw ~/.meituan-catpaw、
 * Cline ~/.cline）读的都是 HOME 下的明文 JSON，macOS 上照样能导入，所以只裁 AutoClaw。
 */
export function desktopImportAvailable(config: ProviderConfig): boolean {
  if (config.desktop === false) return false
  // 网页端（headless 托管面板注入 platform='web'）：没有本机桌面客户端可读
  if (platform() === 'web') return false
  if (config.desktopWindowsOnly && platform() === 'macos') return false
  return true
}

/** 网页登录那一段只在某个地区露出时的判据（当前没有家用，结构上保留） */
export function methodsOf(config: ProviderConfig, region: string): MethodId[] {
  return ADD_METHODS.filter(method => {
    if (method.id === 'oauth') return Boolean(config.oauthLogin)
    if (method.id === 'sms') return Boolean(config.smsLogin)
    if (method.id === 'web') return Boolean(config.webLogin) && (!config.webLogin?.region || region === config.webLogin.region)
    if (method.id === 'desktop') return desktopImportAvailable(config)
    return true
  }).map(method => method.id)
}

/** 手填表单标题：默认「填写凭证添加」，小浣熊沿用原文案 */
export const manualTitleOf = (config: ProviderConfig): string => config.manualTitle || '填写凭证添加'
/** 说明中的行内标记优先，否则按纯文本渲染（React 的文本节点天然转义） */
export const manualNoteOf = (config: ProviderConfig): { html?: string; text?: string } =>
  (config.manualNoteHtml ? { html: config.manualNoteHtml } : { text: config.manualNote || '' })

/** 手填段主按钮文案 */
export const addButtonTextOf = (config: ProviderConfig): string =>
  config.addButton || `添加 ${config.label} 账号`

/** 字段的 DOM id（inputKey 保留小浣熊既有的 refresh-input id，请求体键仍为 refreshToken） */
export const fieldIdOf = (config: ProviderConfig, field: FieldSpec): string =>
  `${config.provider}-${field.inputKey || field.key}-input`
