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
import { t } from '../i18n'

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
  addButton: t('添加小浣熊账号'),
  // 网页登录（后端 providers::raccoon::oauth）：官方登录页 + 一次性授权码回调。
  // 桌面端仍优先用内嵌窗口；网页端 / Docker 远程面板会在授权完成后
  // 提示用户粘贴 office-raccoon:// 回调地址，不依赖本机协议注册。
  webLogin: {
    noteHtml: t('在<strong>内嵌窗口</strong>里完成官方登录，成功后自动加入账号列表。'),
    button: t('打开网页登录'),
    hint: t('内嵌窗口打开；完成后自动加入列表，关窗即取消等待'),
    busyText: t('等待小浣熊登录完成…'),
  },
  manualTitle: t('粘贴 token / refreshToken'),
  manualNoteHtml: t('refreshToken 可选，填了到期可自动续期；两者都能从<a href="#" class="raccoon-hint-link" data-raccoon-hint>小浣熊客户端登录态文件</a>里取到。'),
  fields: [
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用凭证里的账号名') },
    { key: 'token', label: 'token', rows: 3, placeholder: t('粘贴 access_token（一长串 JWT）') },
    { key: 'refreshToken', inputKey: 'refresh', label: 'refreshToken', rows: 2, optional: true, placeholder: t('可选') },
  ],
  desktopNote: t('读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后点「刷新 Token」同步。'),
}

const CATPAW: ProviderConfig = {
  // catpaw_accounts::add_catpaw_account：token / accessToken / access_token / auth_token
  // （即 X-Passport-Token）、uid / userId / loginName、name；没有刷新机制
  provider: 'catpaw',
  label: 'CatPaw',
  // 网页登录：美团 passport 授权页 + loopback 回调；服务端同时轮询 poll-token，
  // 因此 Docker / 远程面板不依赖浏览器访问容器内的 loopback 端口。
  webLogin: {
    noteHtml: t('用 CatPaw 账号完成登录（美团 passport），成功后自动加入账号列表。'),
    button: t('打开 CatPaw 网页登录'),
    busyText: t('等待 CatPaw 登录完成…'),
    modes: [
      { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('内嵌窗口打开；完成后自动加入列表，关窗即取消等待') },
      { value: 'external', label: t('系统浏览器'), hint: t('系统浏览器打开（复用已登录的美团账号）；完成后自动加入列表') },
    ],
  },
  manualNote: t('token 是登录态 Cookie；CatPaw 没有刷新机制，过期后需在客户端重新登录。'),
  fields: [
    { key: 'token', label: 'token', rows: 3, placeholder: t('CatPaw 的 X-Passport-Token（登录态 Cookie）') },
    { key: 'uid', label: 'uid', placeholder: t('必填，CatPaw 账号标识') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用登录名或 uid') },
  ],
  desktopNote: t('读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后重新导入即可。'),
  desktopHint: t('读取 ~/.meituan-catpaw/auth.json，需已在 CatPaw 客户端登录'),
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
  label: t('AutoClaw 国内版'),
  manualNote: t('token 支持 enc: 前缀（Windows 上自动解密）。未填 refreshToken 无法自动续期。'),
  // 国内版**唯一**的官方登录方式（它的登录页不渲染 OAuth 按钮，已核对构建产物）
  smsLogin: {
    noteHtml: t('用绑定的手机号登录：点「获取验证码」后填入即可。这是国内版官方唯一的登录方式。'),
  },
  fields: [
    { key: 'token', label: 'token', rows: 3, placeholder: t('明文 JWT 或 auth.json 里的 enc: 加密值（自动解密）') },
    { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: t('没有则无法自动续期') },
    { key: 'deviceId', label: 'deviceId', optional: true, placeholder: t('可选，续期时带上') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用 userId') },
  ],
  desktopNote: t('读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。'),
  desktopHint: t('读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows'),
  // 登录态是 Electron safeStorage 密文，解密要走 DPAPI（仅 Windows），
  // 因此 macOS 上整段收起（理由见 desktopImportAvailable）
  desktopWindowsOnly: true,
}

const AUTOCLAW_INTL: ProviderConfig = {
  // AutoClaw 国际版（autoglm-api.autoglm.ai）：与国内版同一套协议与签名指纹
  // （appId/appKey 两地逐字相同，已实测），只有站点不同。
  provider: 'autoclaw-intl',
  label: t('AutoClaw 国际版'),
  manualNote: t('国际版与国内版账号体系独立，请填国际版的凭证。未填 refreshToken 无法自动续期。'),
  // OAuth 网页登录：国际版**唯一**的登录方式（登录页只渲染 Zai / Google 两个按钮）。
  // 与另外五家的差别：授权地址前有一次强制风控验证码（阿里云滑块），
  // 必须在浏览器里跑完才能拿地址，因此点按钮后会先在本弹窗里弹滑块
  // （见 ui/autoclaw-oauth.js 的文件头）。
  oauthLogin: {
    title: t('网页登录（Zai / Google）'),
    noteHtml: t('点按钮后先过一次滑块验证（官方风控），随后打开登录页，登录完成即自动添加账号。<br>这是国际版官方唯一的登录方式；已在客户端登录过的，用「导入桌面端登录态」更快。'),
    modes: [
      { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('内嵌窗口打开；关窗即取消等待（Google 账号被拒时改用系统浏览器）') },
      { value: 'external', label: t('系统浏览器'), hint: t('系统浏览器打开（复用已登录的 Zai / Google 账号）；完成后自动加入列表') },
    ],
  },
  fields: [
    { key: 'token', label: 'token', rows: 3, placeholder: t('明文 JWT（国际版账号的 access token）') },
    { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: t('没有则无法自动续期') },
    { key: 'deviceId', label: 'deviceId', optional: true, placeholder: t('可选，续期时带上') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用 userId') },
  ],
  // 桌面端登录态导入两个地区都给：auth.json 没有地区标记、本机判断不了，
  // 用户在哪一项下点导入就得到哪一家的账号，猜错的后果是可见的上游 401。
  desktopNote: t('读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。用 Zai / Google 登录客户端的用户走这一条。'),
  desktopHint: t('读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows'),
  desktopWindowsOnly: true,
}

/**
 * Qoder 两个地区（中国版 / 国际版）：同一套设备授权协议，只有站点主机不同。
 *
 * 为什么不是一个配置带一个「地区」下拉（拆家，2026-10）：与 AutoClaw / Accio /
 * ZCode 同一思路 —— 地区是 **provider 身份**而不是账号属性。拆家前两地区账号
 * 混在一个「模型来源」下拉里、一次「获取模型」只能刷到队首账号所属地区的目录；
 * 按两家建模后各自有独立的账号、清单与启停（除 id / 标签 / 文案外同构）。
 */
function qoderForm(spec: { provider: string; label: string; siteNote: string }): ProviderConfig {
  const { provider, label, siteNote } = spec
  return {
    provider,
    label,
    desktop: false,
    // 地区写死在 provider 身份里（后端按 provider id 反查），不再走地区分段
    webLogin: {
      // 两站是同一套 PKCE 设备授权协议，只有站点主机不同；edition 由 provider
      // 身份决定（后端按 provider id 反查地区，请求里的 edition 只是回显字段）
      noteHtml: t('打开官方授权页完成设备码授权，登录的是{label}（{site}）的账号。', { label, site: siteNote }),
      button: t('打开 Qoder 网页登录'),
      busyText: t('等待 Qoder 授权完成…'),
      edition: provider === 'qoder' ? 'cn' : 'global',
      modes: [
        { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('内嵌窗口用全新环境，多账号互不影响；关窗即取消等待') },
        { value: 'external', label: t('系统浏览器'), hint: t('系统浏览器打开（复用已登录账号）；完成后自动加入列表') },
      ],
    },
    manualTitle: t('使用个人访问令牌（PAT）'),
    manualNote: t('在{label}的 Qoder 账号设置 → Integrations 生成 PAT（别填 Google / GitHub 的令牌）。两个地区的账号与凭证不通用。', { label }),
    fields: [
      { key: 'pat', label: 'Qoder PAT', rows: 3, placeholder: t('粘贴 Qoder 个人访问令牌（pt-…）') },
      { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空使用账号昵称或邮箱') },
    ],
  }
}

const QODER = qoderForm({ provider: 'qoder', label: t('中国版'), siteNote: 'qoder.com.cn' })
const QODER_INTL = qoderForm({ provider: 'qoder-intl', label: t('国际版'), siteNote: 'qoder.com' })

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
      noteHtml: t('打开授权页完成确认，账号自动加入 <b>{label}</b>{note}', { label, note: poolNote }),
      button: t('打开 Cline 授权页'),
      busyText: t('等待 Cline 授权确认…'),
      modes: [
        { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('内嵌窗口打开；完成后自动加入列表，关窗即取消等待') },
        { value: 'external', label: t('系统浏览器'), hint: t('系统浏览器打开（复用已登录的 Cline 账号）；完成后自动加入列表') },
      ],
    },
    manualTitle: t('填写凭证'),
    manualNote: t('refreshToken 可选，填了可自动续期；两者都能从客户端登录态文件取到。同一个 Cline 账号两个池都能用，这里归入 {label}。', { label }),
    fields: [
      { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: t('粘贴 workos:… 开头的令牌（不带前缀也会自动补上）') },
      { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: t('可选，没有则无法自动续期') },
      { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空使用账号姓名、邮箱或令牌指纹') },
    ],
    desktopNote: t('读本机客户端当前登录态，每次实时读取（删掉这条记录不影响客户端登录态）。这一条归入 {label}。', { label }),
    desktopHint: t('读取 ~/.cline/data/settings/providers.json，需已在 Cline 客户端登录'),
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
    // OAuth 2.0 授权码 + PKCE：同机直接接收 loopback；Docker / 远程面板
    // 由网页 shim 提示粘贴最终回调地址（与 CodeArts 同一兜底形态）。
    webLogin: {
      noteHtml: t('打开 Accio <b>{label}</b>的官方登录页（<code>{site}</code>）并用你的 Accio 账号登录：登录成功后官方页面会跳回本机；同机自动完成，Docker / 远程面板按提示粘贴回调地址即可（授权码只在本机传给网关，界面不显示明文 token）。', { label, site }),
      button: t('打开 Accio {label}登录页', { label }),
      busyText: t('等待 Accio {label}登录完成…', { label }),
      modes: [
        { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待') },
        { value: 'external', label: t('系统浏览器'), hint: t('将用系统默认浏览器打开登录页（会复用浏览器里已登录的 Accio 账号）；完成登录后自动加入账号列表，关掉弹窗即取消等待') },
      ],
    },
    manualTitle: t('填写凭证'),
    manualNoteHtml: t('accessToken 是 Accio 的登录凭证（一长串不透明 token，不是 JWT）；refreshToken 可选，填了之后到期能自动续期。请填写 <b>{label}</b>账号的凭证 —— {siteNote}（最容易拿到的办法：直接用上方的「网页登录」，不需要手工找 token。）', { label, siteNote }),
    fields: [
      { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: t('粘贴 Accio 的 accessToken') },
      { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: t('可选，没有则无法自动续期') },
      { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空使用账号昵称或邮箱') },
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
      noteHtml: t('打开 ZCode <b>{label}</b>的官方授权页并用你的账号登录。授权结果由 ZCode 服务端记录，网关在后台自动取回凭证并加入账号列表。', { label }),
      button: t('打开 ZCode {label}授权页', { label }),
      busyText: t('等待 ZCode {label}授权完成…', { label }),
      modes: [
        { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('将打开内嵌窗口；授权完成后自动加入账号列表。关掉窗口即取消等待。授权页最后可能提示无法打开 zcode:// 链接，这是正常的 —— 结果已由服务端记下') },
        { value: 'external', label: t('系统浏览器'), hint: t('将用系统默认浏览器打开授权页（会复用浏览器里已登录的 ZCode 账号）；授权完成后自动加入账号列表。页面最后可能提示无法打开 zcode:// 链接，属正常现象') },
      ],
    },
    manualTitle: t('填写凭证'),
    manualNoteHtml: t('本家有两个**互不替代**的凭证，按你要用的功能填，至少填一个：<b>编码套餐 API Key</b> 用于转发推理（打 <code>{site}</code>）与查询窗口限额（每 N 小时 / 每周的剩余比例）；<b>jwt</b> 用于领取套餐与查询余额（打 <code>zcode.z.ai</code> 的 billing 网关，官方叫 Coding Plan JWT，是一串三段点分的字符串）。余额两把都能查、但读到的东西不同：jwt 读到余额桶与套餐到期，API Key 读到窗口限额；而领取只认 jwt、转发只认 API Key。请填写 <b>{label}</b>账号的凭证 —— {plan}（最容易拿到的办法：直接用上方的「网页登录」—— 它会替你把这个 API Key 换好，两个凭证一起拿到。）', { site, label, plan: planNote }),
    fields: [
      {
        key: 'accessToken',
        label: t('编码套餐 API Key'),
        rows: 3,
        optional: true,
        // 这里不能填 OAuth 登录态：那个串拿去转发会被上游按「OAuth 令牌」那条路
        // 校验并回 401。官方客户端与「网页登录」这条链都是先换成编码套餐 API Key 再用。
        placeholder: t('用于转发；形如 apiKey.secret 两段点分（不填则这个账号不能转发）'),
      },
      { key: 'jwt', label: 'Coding Plan JWT', inputKey: 'jwt', rows: 3, optional: true, placeholder: t('用于领取套餐与查询余额；不填则这个账号不能领取与查余额') },
      { key: 'userId', label: t('用户 ID'), optional: true, placeholder: t('可选；用于生成账号 id 与展示名') },
      { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空自动生成') },
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
 *   · Docker / 远程面板直接把地址栏中的最终回调地址粘贴回网页 shim —— 带 `code` 就直接落账；
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
    noteHtml: t('打开华为云 CodeArts 的官方授权页登录：登录完成后官方页面会把浏览器带回<b>网关自己的</b> <code>/oauth/callback</code>，网关用一次性授权码换取临时凭据并加入账号列表。<br>网关跑在另一台机器上时，网页端会提示把地址栏中的最终回调地址直接粘贴回面板。'),
    button: t('打开 CodeArts 授权页'),
    busyText: t('等待 CodeArts 登录完成…'),
    modes: [
      {
        value: 'embedded',
        label: t('内嵌窗口（推荐）'),
        hint: t('将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待'),
      },
      {
        value: 'external',
        label: t('系统浏览器'),
        hint: t('将用系统默认浏览器打开授权页（会复用浏览器里已登录的华为云账号）；浏览器与网关不在同一台机器时，按上方说明把最终回调地址粘贴回面板'),
      },
    ],
  },
  manualTitle: t('粘贴登录凭据'),
  manualNoteHtml: t('整份粘贴官方插件 / CLIProxyAPI 落盘的凭据 JSON（形如 <code>{"codearts_provider_credential":{…}}</code>，铺平的也行）。<br>必填：<code>access_key_id</code>、<code>secret_access_key</code>、<code>security_token</code>。<b>要能自动续期，必须连 <code>refresh_token</code> 与 <code>oauth_context</code> 一起粘</b> —— 临时凭据约一小时到期，缺这半块就续不回来，只能重新登录。'),
  fields: [
    {
      key: 'credentialJson',
      label: t('凭据 JSON'),
      rows: 8,
      // 解析后按字段铺开进请求体（后端 `Credential::from_payload` 嵌套/平铺都认）
      jsonExpand: true,
      placeholder: '{"codearts_provider_credential":{"access_key_id":"HSTA…","secret_access_key":"…",'
        + '"security_token":"…","expires_at":"2026-09-27T16:17:00.327Z","domain_id":"…","user_id":"…",'
        + '"user_name":"…","refresh_token":"eyJ…","oauth_context":{…}}}',
    },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空使用凭据里的 user_name') },
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
    noteHtml: t('打开 <b>Trae SOLO</b> 的官方授权页（<code>trae.cn</code>）并用你的 Trae 账号登录：授权完成后官方页面会跳回<b>本机</b>的一个临时端口；同机部署自动接收，Docker / 远程面板请按提示复制地址栏回调地址。'),
    button: t('打开 Trae 授权页'),
    busyText: t('等待 Trae 授权完成…'),
    modes: [
      {
        value: 'embedded',
        label: t('内嵌窗口（推荐）'),
        hint: t('将打开内嵌窗口；授权完成后自动加入账号列表。远程面板可粘贴回调地址。关掉窗口即取消等待。链接 5 分钟内有效，超时或未点就会作废（可重新发起）'),
      },
      {
        value: 'external',
        label: t('系统浏览器'),
        hint: t('将用系统默认浏览器打开授权页（会复用浏览器里已登录的 Trae 账号）；同机完成后自动加入，Docker / 远程面板按提示粘贴地址栏回调地址。'),
      },
    ],
  },
  manualTitle: t('填写凭证'),
  manualNoteHtml: t('accessToken 是 Trae 的 <code>Cloud-IDE-JWT</code>（三段点分），refreshToken 用于到期自动续期（本家 <b>refreshToken 一次一换</b>：换发一次旧的即作废，所以两份程序别同时刷同一个账号）。<br>手工粘贴时请连 <b>machineId / deviceId</b> 一起填：上游把它们与登录时上传的设备公钥绑在一起判设备，凭空换一对会撞 <code>2xxxx</code> 那族设备绑定拒绝。<br>没有这三样时的正路是用上方的「网页登录」；容器 / 远程部署形态按提示粘贴地址栏回调地址即可。'),
  fields: [
    { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: t('Cloud-IDE-JWT（三段点分）') },
    { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: t('可选；填了才能到期自动续期') },
    { key: 'uid', label: t('用户 ID'), optional: true, placeholder: t('可选；用于去重与展示名（留空时网关会问一次上游）') },
    { key: 'machineId', label: 'machineId', optional: true, placeholder: t('可选；UUID 形态，与凭据同生共死') },
    { key: 'deviceId', label: 'deviceId', optional: true, placeholder: t('可选；纯数字串（上游只收 8–24 位数字）') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空自动用昵称') },
  ],
}

/**
 * Loomy（讯飞）：短信登录 + 粘贴 session。
 *
 * ── 三处与别家不同（都有上游依据，别照抄别家）────────────────
 *   1. 短信链路是**另一套**（端点 `/api/session/login/loomy/sms/*`、中间态字段
 *      叫 `msgid` 而不是 `deviceId`、请求带 HMAC-SHA1 签名头）——
 *      由 `ui/sms-login.js` 的 SMS_PROFILES 与壳侧桥接按 provider 分派；
 *   2. 粘贴的是**登录 session**（它同时是模型网关的 token，不是 JWT）；
 *   3. **没有续期**：session 约 14 天，过期只能重新登录（所以不渲染
 *      「刷新 Token」那一类控件）。
 */
const LOOMY: ProviderConfig = {
  provider: 'loomy',
  label: 'Loomy',
  smsLogin: {
    noteHtml: t('用 Loomy 账号绑定的手机号登录：点「获取验证码」后填入即可。这是本家最省事的入口。'),
  },
  manualNote: t('粘贴的是登录 session（同时是模型网关的 token）。Loomy 没有续期接口，session 有效期约 14 天，过期后重新登录即可。'),
  fields: [
    { key: 'session', label: 'session', rows: 3, placeholder: t('登录 session（一长串）') },
    { key: 'userId', label: 'userId', optional: true, placeholder: t('可选，讯飞侧 userid') },
    { key: 'phone', label: t('手机号'), optional: true, placeholder: t('可选，展示用') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用脱敏手机号或 userId') },
  ],
  // 登录态在客户端自己的加密存储里（没有 auth.json 那种稳定可读的文件形态），
  // 不提供「导入桌面端登录态」（与 Accio / ZCode 同一处境，理由见各自配置注释）。
  desktop: false,
}

/**
 * KukuAI（百度文库「库库 AI / GenFlowPro」）：一家一个 provider，没有地区之分。
 *
 * ── 凭证形态 ────────────────────────────────────────────────
 * 三个 Cookie 就能认证：`BDUSS` / `STOKEN` / `gfprotpl=genflowpro`（后一个
 * 恒附加，不用填）。粘贴支持三种形态（与后端 `kuku::credentials` 同口径）：
 *   1. 整段 Cookie 头（`BDUSS=…; STOKEN=…; …`）；
 *   2. Cookie 编辑器导出的 `[{name, value}, …]` JSON 数组；
 *   3. `{BDUSS, STOKEN}` JSON 对象。
 *
 * ── 导入桌面端 ──────────────────────────────────────────────
 * 客户端是 Electron 套壳，登录态在 `%APPDATA%\baidugenflowpro\Network\Cookies`
 * （SQLite **明文**，无需解密）。客户端运行时该文件被独占锁，导入失败时
 * 提示先关闭客户端（占用冲突的两段式处置与 CatPaw 导入同一交互）。
 *
 * ── 没有刷新机制 ────────────────────────────────────────────
 * BDUSS 是百度通行证登录态，上游没有刷新接口，过期后重新导入/重新粘贴即可
 * （与 CatPaw 同一处境，所以不渲染「刷新 Token」那一类控件）。
 */
const KUKU: ProviderConfig = {
  provider: 'kuku',
  label: 'KukuAI',
  // 主站登录（最终方案）：打开 kuku.baidu.com，用户像平时用网页版一样登录
  // （手机验证码 / 扫码都由官方页面自己处理风控 —— 不再在登录页上做任何
  // 自动化）。登录成功后壳侧检测到 Cookie 里出现 BDUSS，自动读取**完整会话**
  // （含 HttpOnly 的 BAIDUID_BFESS 等）交回网关，窗口自动关闭、账号落列表。
  webLogin: {
    noteHtml: t('在打开的窗口里用百度账号登录 KukuAI 主站（手机验证码 / 扫码均可），登录成功后自动加入账号列表。网关会向百度换发 KukuAI 业务会话令牌（依赖本机已安装库库AI 客户端）。'),
    button: t('打开网页登录'),
    busyText: t('等待 KukuAI 登录完成…'),
  },
  manualTitle: t('粘贴 Cookie'),
  manualNote: t('BDUSS 必填；请粘贴**完整 Cookie**（Cookie 编辑器一键导出即可）—— 其中 **PTOKEN 必须保留**，网关要用它向百度换发 KukuAI 业务会话令牌，缺了它模型刷新会报「未登录」。支持整段 Cookie 头、Cookie 编辑器导出的 JSON 数组，或 {BDUSS, STOKEN} 对象。KukuAI 没有刷新机制，登录态过期后重新粘贴即可。'),
  fields: [
    { key: 'cookie', label: 'Cookie', rows: 3, placeholder: t('BDUSS=…; STOKEN=…（或 Cookie 编辑器导出的 JSON）') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用账号 uk') },
  ],
  desktop: true,
  desktopHint: t('读取本机 KukuAI 客户端登录态（%APPDATA%\\baidugenflowpro\\Network\\Cookies），需已在 KukuAI 客户端登录'),
  desktopNote: t('读本机客户端当前登录态（明文 Cookie，无需解密）。客户端运行时 Cookies 文件被独占占用，导入前请先关闭 KukuAI 客户端。'),
}

/**
 * MonkeyCode（长亭科技）两个站点：国内版 `monkeycode-ai.com` / 国际版
 * `monkeycode-ai.net`。
 *
 * ── 为什么两个站点 = 两家 provider ─────────────────────────
 * 与 AutoClaw / Qoder / ZCode 同一思路：两站是同一套协议、同一个 session
 * cookie 名、同一套任务流，只有站点不同；地区是 **provider 身份**而不是账号
 * 属性（后端 `monkeycode::Region` 是「provider id → 站点」的唯一映射）。
 * 拆家后两站各自有独立的账号与启停、界面上各占一个分组 —— 因此两块用同一个
 * 工厂生成（表单相同、站点提示不同，照 `qoderForm` 的写法）。
 *
 * ── 粘贴 session、没有续期 ─────────────────────────────────
 * 粘贴浏览器里 `monkeycode_ai_session` cookie 的值（整段 Cookie 直接粘也行，
 * 网关会自己剥出值）。本家的密码登录要图形验证码、OAuth 要短信验证，两条都
 * 不适合网关代跑，粘贴 session 是唯一可靠的入口。上游**没有 refresh 接口**：
 * session 是约 30 天的硬限制，过期只能重新登录网页再粘贴（与 CatPaw /
 * KukuAI 同一处境，所以不渲染「刷新 Token」那一类控件）。
 *
 * ── imageId 为什么可以留空 ─────────────────────────────────
 * `imageId`（任务镜像 id）是发起对话的必需字段，但它不在登录响应里 —— 表单
 * 允许留空，由网关在添加时从该账号**已有任务**里 best-effort 自动发现（老
 * 用户一般能拿到）；新账号一个任务都没有、发现不到时**不阻断添加**，之后
 * 手动补即可。
 *
 * ── 没有「导入桌面端登录态」────────────────────────────────
 * 登录态就是浏览器 cookie，没有客户端 `auth.json` 那种稳定可读的文件形态
 * （与 Accio / ZCode 同一处境），给了入口只会稳定失败。
 */
function monkeycodeForm(spec: { provider: string; label: string; siteNote: string }): ProviderConfig {
  const { provider, label, siteNote } = spec
  return {
    provider,
    label,
    // 登录态是浏览器 cookie、没有可读的落盘文件（理由见上方注释）
    desktop: false,
    manualTitle: t('粘贴 session cookie'),
    manualNoteHtml: t('粘贴网页登录态 cookie <code>monkeycode_ai_session</code> 的值（在浏览器里复制整段 Cookie 直接粘贴也行，网关会自己剥出值）。<br>{siteNote}<br>MonkeyCode <b>没有续期接口</b>：session 有效期约 30 天（硬限制），过期后重新登录网页、再粘贴一次即可。', { siteNote }),
    fields: [
      { key: 'session', label: 'session', rows: 3, placeholder: t('monkeycode_ai_session 的值（也可直接粘贴整段 Cookie）') },
      { key: 'imageId', label: t('任务镜像 id'), optional: true, placeholder: t('可选：发起对话必需，留空则由网关从该账号已有任务里自动发现，发现不到时之后手动补') },
      { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用上游返回的账号名或账号 id') },
    ],
  }
}

const MONKEYCODE = monkeycodeForm({
  provider: 'monkeycode',
  label: t('MonkeyCode'),
  siteNote: t('这里填国内站（monkeycode-ai.com）的账号：请粘贴国内站的 session。国际站（monkeycode-ai.net）是另一家 provider、另一套账号，两边的凭证不通用。'),
})

const MONKEYCODE_INTL = monkeycodeForm({
  provider: 'monkeycode-intl',
  label: t('MonkeyCode 国际版'),
  siteNote: t('这里填国际站（monkeycode-ai.net）的账号：请粘贴国际站的 session。国内站（monkeycode-ai.com）是另一家 provider、另一套账号，两边的凭证不通用。'),
})

/**
 * Command Code（`api.commandcode.ai`）：一家一个 provider，没有地区之分
 * （规格 §9：参考里只有这一个主机，没有 cn / intl 双域名、没有 region 头）。
 *
 * ── 凭证是一枚 `user_` 前缀的 API Key，粘贴式 ─────────────────
 * 来源是官方 CLI 的 `~/.commandcode/auth.json` 或网页 studio
 * （commandcode.ai/studio）。粘贴形态很杂（裸 key / 带成对引号 /
 * `Authorization: Bearer …` 整行 / key 后面还跟着别的字段），网关会自己扫描
 * `user_` 剥成裸 key —— 整行直接粘就行，不用手工裁剪。
 *
 * ── 无续期、无桌面端导入 ───────────────────────────────────
 * key 是静态的：上游没有 refresh 接口、也不会过期，失效只能重新获取（所以
 * 不渲染「刷新 Token」那一类控件）。本机 `~/.commandcode/auth.json` 是 CLI
 * 自己的凭证文件，不是可导入的「客户端登录态」，没有那条路径（与 Loomy /
 * MonkeyCode 同一处境）。
 *
 * ── 添加时的一次线上探活 ───────────────────────────────────
 * 网关在添加时会打一次**轻量探活**（`GET /alpha/billing/credits`）：只有拿到
 * 401/403 才判 key 无效，网络失败 / 5xx / 未知一律放行 —— 失败不阻断添加，
 * 只作提示（不让一次上游抖动把用户挡在门外）。
 */
const COMMANDCODE: ProviderConfig = {
  provider: 'commandcode',
  label: t('Command Code'),
  desktop: false,
  manualTitle: t('粘贴 API Key'),
  manualNote: t('粘贴 Command Code 的 API Key（user_ 开头；从官方 CLI 的 ~/.commandcode/auth.json 或网页 studio 获取）。整行 / 带引号 / 带 Bearer 前缀地粘都行，网关会自己剥成裸 key。key 不过期、也没有续期接口，失效后重新获取即可；添加时网关会做一次轻量探活，失败不影响添加。'),
  fields: [
    { key: 'apiKey', label: 'apiKey', rows: 3, placeholder: t('user_ 开头的 API Key（也可直接粘贴 Authorization: Bearer … 整行）') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用 key 尾号') },
  ],
}

/**
 * Antigravity（Google 的 AI IDE，本次接它提供的 Gemini 模型）。
 *
 * ── 网页登录（Google OAuth 授权码 + loopback）───────────────
 * 后端适配器本地拼授权地址（`accounts.google.com`，6 个 scope +
 * `access_token=offline` / `prompt=consent`），完成后浏览器回到**网关自己的**
 * loopback 端口 `/oauth-callback`（redirect_uri 就是网关端口拼出来的，见
 * `providers::antigravity::oauth`）。回调落在网关，因此内嵌窗口与系统浏览器
 * 两种方式都能用（与 CatPaw 同款）；远程 / Docker 场景下浏览器到不了容器内
 * 的 loopback 端口时，可把地址栏整条回调 URL 粘回登录弹窗。
 *
 * ── 粘贴 Google refresh token（仍保留的手工入口）──────────────
 * 已从别处（官方 IDE / 参考实现）拿到 refresh_token 的用户可以直接粘贴；
 * 令牌以 `1//` 开头，直接粘贴含 `refresh_token=` 的整行也行（网关会归一化，
 * `1//` 本体留着）。
 *
 * ── 有自动续期 ─────────────────────────────────────────────
 * access_token 约一小时过期（Google 侧的 expires_in），refresh_token 是长寿命
 * 的主凭证：网页登录与粘贴式都会在添加时打一次 Google token 端点做**真实校验**
 * 并顺手拿到 access token，之后到期由网关自动续期 —— 令牌丢了只能重新走
 * Google 授权。
 *
 * ── projectId 可以留空 ─────────────────────────────────────
 * `projectId`（`cloudaicompanionProject`）是聊天请求必需、但每个 Google 账号
 * 各不相同的值（不在 token 里）：留空时网关会 best-effort 自动发现
 * （`loadCodeAssist` → 无则 `onboardUser`），失败不阻断添加，刷模型清单时会
 * 再试。`email` 同样可选，只用于展示与身份识别（网页登录会自己取 userinfo）。
 *
 * ── 没有「导入桌面端登录态」────────────────────────────────
 * 登录态在 Antigravity IDE 自己的存储里，没有 `auth.json` 那种稳定可读的
 * 形态（与 Accio / ZCode / Loomy 同一处境）。
 */
const ANTIGRAVITY: ProviderConfig = {
  provider: 'antigravity',
  label: t('Antigravity'),
  desktop: false,
  // 网页登录：Google 账号授权页 + loopback 回调。回调落在网关自己的端口，
  // 所以两种打开方式都可用（会复用系统浏览器里已登录的 Google 账号）。
  webLogin: {
    noteHtml: t('在打开的窗口里登录 Google 账号（Antigravity 的授权页），授权完成后自动加入账号列表。'),
    button: t('打开 Antigravity 网页登录'),
    busyText: t('等待 Google 授权完成…'),
    modes: [
      { value: 'embedded', label: t('内嵌窗口（推荐）'), hint: t('内嵌窗口打开；完成后自动加入列表，关窗即取消等待') },
      { value: 'external', label: t('系统浏览器'), hint: t('系统浏览器打开（复用已登录的 Google 账号）；完成后自动加入列表') },
    ],
  },
  manualTitle: t('粘贴 Google refresh token'),
  manualNoteHtml: t('粘贴 Antigravity 的 Google refresh token（以 <code>1//</code> 开头；直接粘贴含 <code>refresh_token=</code> 的整行也行）。<br>这是网页登录之外的手工入口（例如从已登录的官方 IDE 里导出）。refresh token 不会过期，access token 到期由网关自动续期；令牌丢了只能重新走 Google 授权，请妥善保存。'),
  fields: [
    { key: 'refreshToken', label: 'refreshToken', rows: 2, placeholder: t('1// 开头的 Google refresh token（也可直接粘贴 refresh_token=… 整行）') },
    { key: 'projectId', label: 'projectId', optional: true, placeholder: t('可选（cloudaicompanionProject）：留空由网关自动发现') },
    { key: 'email', label: 'email', optional: true, placeholder: t('可选：账号邮箱，用于展示与身份识别（换新令牌也能认回同一账号）') },
    { key: 'name', label: t('备注名'), optional: true, placeholder: t('可选，留空则用邮箱或令牌尾号') },
  ],
}

/**
 * OfficeAce（华为云果办 / OfficeClaw）：一家一个 provider，没有地区之分。
 *
 * ── 两条来源（都落在后端 `add_officeace_account`）──────────────
 * ① **浏览器登录（自助 OAuth）**：网关向云端要 state、拼授权地址，用户在浏览器里
 *    登华为云，网关**轮询云端**取授权码并换到网关凭据 —— **不需要装 OfficeAce
 *    桌面端**（云端回调页不往本机跳，所以壳侧也不用开回调监听）。
 * ② **粘贴凭据**：从别的机器（或桌面端 `models.json`）拿 `baseUrl` 与
 *    `modelAppKey`/`modelAppSecret` 贴进来。只给这三个也能转发（网关凭据不过期）；
 *    控制面临时凭据（`accessKeyId` 等）可选。
 *
 * ── 没有「导入桌面端登录态」这条后端路径 ──────────────────────
 * 与 CodeArts 同：凭据只能靠网页登录或粘贴，所以显式 `desktop: false`
 * （不写的话桌面壳里会多出一个选了之后什么都没有的分段）。
 */
const OFFICEACE: ProviderConfig = {
  provider: 'officeace',
  label: 'OfficeAce 果办',
  desktop: false,
  webLogin: {
    noteHtml: '打开华为云授权页登录：登录完成后网关会**轮询云端**取回授权码并加入账号列表'
      + '（不需要装 OfficeAce 桌面端，也无需手工回填回调地址）。',
    button: '打开授权页',
    busyText: '等待 OfficeAce 登录完成…',
  },
  manualTitle: '粘贴网关凭据',
  manualNoteHtml: '从别的机器（或 OfficeAce 桌面端 <code>models.json</code>）取三项贴进来：'
    + '模型网关基址（<code>baseUrl</code>，形如 <code>https://modelgw-…/v2</code>）与'
    + ' <code>modelAppKey</code> / <code>modelAppSecret</code>。'
    + '这三项就能转发（网关凭据不过期）；控制面临时凭据（<code>accessKeyId</code> 等）可选，'
    + '留空则额度/自动续期不可用，但不影响转发。',
  fields: [
    { key: 'baseUrl', label: '模型网关基址', placeholder: 'https://modelgw-…huaweicloud-agentarts.com/v2' },
    { key: 'modelAppKey', label: '网关 Key', placeholder: 'model_app_key' },
    { key: 'modelAppSecret', label: '网关 Secret', placeholder: 'model_app_secret' },
    { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空用「OfficeAce 果办」' },
  ],
}

/** 内置家的表单块，顺序与旧 ADD_FORMS 一致（只影响 DOM 里的块顺序，不影响界面） */
export const BUILTIN_CONFIGS: ProviderConfig[] = [
  RACCOON,
  CATPAW,
  AUTOCLAW,
  AUTOCLAW_INTL,
  // Qoder 两个地区相邻（拆家后是两家 provider，与 AutoClaw / Accio / ZCode
  // 同一理由）：中国版在前，与存量账号的归属一致
  QODER,
  QODER_INTL,
  // Cline 顺序即界面上「提供商」分段的顺序：免费池在前（无门槛，更常用）
  clineForm({ provider: 'cline-free', label: 'Cline Free', poolNote: t('（免费额度池，模型名带 cline-free/ 前缀）。') }),
  clineForm({ provider: 'cline-pass', label: 'Cline Pass', poolNote: t('（订阅池，模型名带 cline-pass/ 前缀，需要账号有对应订阅）。') }),
  // Accio 顺序：国际版在前（默认安装的版本）
  accioForm({ provider: 'accio', label: t('国际版'), site: 'www.accio.com', siteNote: t('国际版与国内版是**两套独立的账号**（同一账号体系的两个站点），凭证不通用。') }),
  accioForm({ provider: 'accio-cn', label: t('国内版'), site: 'www.accio-ai.com', siteNote: t('国内版的登录站点是 www.accio-ai.com，与国际版不是同一站。') }),
  // ZCode 顺序：国内版在前（国内网络环境下更常被添加的那个，与后端注册表 PROVIDERS 的排列一致）
  zcodeForm({ provider: 'zcode', label: t('国内版'), site: 'open.bigmodel.cn', planNote: t('国内版与**国际版是两套独立的账号与套餐**，凭证与领取的套餐都不通用。') }),
  zcodeForm({ provider: 'zcode-intl', label: t('国际版'), site: 'api.z.ai', planNote: t('国际版的推理站点是 api.z.ai，与国内版不是同一站；套餐也各自独立。') }),
  // CodeArts（华为云 AI 代码助手）：一家一个 provider，没有地区/额度池之分
  // （region 写死 cn-north-4，与 token 签发地必须一致）。
  CODEARTS,
  // Trae 只有 SOLO 那一家（没有地区分叉，理由见 TRAE 上方那段）
  TRAE,
  // Loomy（讯飞）：单一入口（手机验证码登录），排在末尾 —— 与后端注册表
  // PROVIDERS 的排列一致（新增的一家加在表尾）
  LOOMY,
  // KukuAI（百度文库库库 AI）：粘贴 Cookie / 导入本机登录态，排在末尾
  // （与后端注册表 PROVIDERS 的排列一致，2026-10 接入）
  KUKU,
  // MonkeyCode（长亭科技）的两个站点相邻（与 AutoClaw / Qoder 同一拆法）：
  // 国内版在前 —— 与后端注册表 PROVIDERS 的追加顺序一致（2026-10 接入）。
  MONKEYCODE,
  MONKEYCODE_INTL,
  // Command Code / Antigravity：单一入口、没有地区伴生，同样按后端注册表
  // PROVIDERS 的追加顺序排在表尾（后到居后，新增的家加在末尾）。
  COMMANDCODE,
  ANTIGRAVITY,
  // OfficeAce（华为云果办 / OfficeClaw）：浏览器登录（自助 OAuth）或粘贴网关
  //凭据，排在末尾（与后端注册表 PROVIDERS 的排列一致，2026-10 接入）
  OFFICEACE,
]

/** WorkBuddy 的块 id（结构特殊，单独一个组件） */
export const WORKBUDDY_PROVIDER = 'workbuddy'
/** 自定义提供商的块 id（后注册块的 provider 取值） */
export const CUSTOM_PROVIDER = 'custom'

/**
 * WorkBuddy 在**添加账号入口**上的展示名：中性品牌名，不带地区。
 *
 * ── 为什么不用注册表里的名字（拆家后是「WorkBuddy 国内版」）──────
 * 这个入口是**两版共用**的：第 1 步只有一张卡，卡片里的「账号版本」分段负责
 * 切国内版 / 国际版（同一个 `WorkBuddyBlock` 的两个选项，见 add-provider-pick
 * 的说明）。名字带「国内版」而用户当场就能切到国际版，读起来就是自相矛盾 ——
 * 卡片与弹窗标题都因此用品牌名。
 *
 * 两处**都**走这个常量而不是各写一份：卡片与标题是同一个入口的两个步骤，
 * 名字不一致时用户会以为点了别的东西。
 *
 * ── 别处仍用注册表名 ────────────────────────────────────────
 * 账号页徽章、模型页、Key 页用的是注册表名（带地区）：那些地方要能分清两家，
 * 而这里一家代表两版。别家不需要这一层 —— AutoClaw / Accio / ZCode 的两版
 * 是**两张卡**，各自的注册名就是各自的身份。
 */
export const WORKBUDDY_ENTRY_LABEL = 'WorkBuddy'

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
  { id: 'oauth', label: t('网页登录（Zai / Google）') },
  { id: 'sms', label: t('手机验证码登录') },
  { id: 'web', label: t('网页登录') },
  { id: 'manual', label: t('填写凭证') },
  { id: 'desktop', label: t('导入桌面端登录态') },
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
export const manualTitleOf = (config: ProviderConfig): string => config.manualTitle || t('填写凭证添加')
/** 说明中的行内标记优先，否则按纯文本渲染（React 的文本节点天然转义） */
export const manualNoteOf = (config: ProviderConfig): { html?: string; text?: string } =>
  (config.manualNoteHtml ? { html: config.manualNoteHtml } : { text: config.manualNote || '' })

/** 手填段主按钮文案 */
export const addButtonTextOf = (config: ProviderConfig): string =>
  config.addButton || t('添加 {name} 账号', { name: config.label })

/** 字段的 DOM id（inputKey 保留小浣熊既有的 refresh-input id，请求体键仍为 refreshToken） */
export const fieldIdOf = (config: ProviderConfig, field: FieldSpec): string =>
  `${config.provider}-${field.inputKey || field.key}-input`
