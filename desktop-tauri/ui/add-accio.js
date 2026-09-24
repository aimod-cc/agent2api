/* Accio 添加配置（**两个地区**：国际版 / 国内版）；由 add-provider-forms.js 统一渲染并绑定。 */
(() => {
  /**
   * ── 为什么是两个表单配置而不是一个配置带一个「地区」下拉 ──────
   * Accio 的两个地区共用同一个网关与同一套接口（`phoenix-gw.alibaba.com`、
   * `client_id=accio-work`），只有**登录站点**（`www.accio.com` /
   * `www.accio-ai.com`）与 `x-package-region` 头不同。按「一家的一个字段」
   * 建模的后果与 Cline 两池 / AutoClaw 两地区那两次一模一样：地区成了**账号的
   * 属性**，界面上混在一起，而「哪个账号走哪个站点」在列表里看不出来，
   * 账号库里的记录也无法按地区隔离。因此这里输出两份配置（除 id、标签与站点
   * 文案外完全同构，共享字段走同一个工厂）。
   *
   * 转发与账号侧对应 `accio` / `accio-cn` 两家 provider（见后端
   * `providers::accio::endpoints::Region`）。
   */

  /**
   * 拼一份 Accio 表单配置。
   *
   * @param {object} spec
   * @param {string} spec.provider provider id（`accio` / `accio-cn`）
   * @param {string} spec.label    展示名
   * @param {string} spec.site     登录站点（写进提示文案，让用户知道去哪登录）
   * @param {string} spec.siteNote 地区说明（拼进各段说明里）
   */
  function accioForm({ provider, label, site, siteNote }) {
    return {
      provider,
      label,
      // 网页登录：OAuth 2.0 授权码 + PKCE（后端 `providers::accio::oauth`）。
      // 授权地址由网关拼，回调落在**本机 loopback 端口**（浏览器 302 回来，
      // 网关用一次性授权码换凭证）—— 与浏览器在哪无关，因此内嵌窗口与系统
      // 浏览器两条路都走得通（与 CatPaw / AutoClaw 国际版同一形态）。
      webLogin: {
        noteHtml: `打开 Accio <b>${label}</b>的官方登录页（<code>${site}</code>）并用你的 Accio 账号登录：`
          + '登录成功后官方页面会跳回本机，网关自动用一次性授权码换取凭证并加入账号列表'
          + '（授权码只在本机传给网关，界面不显示明文 token）。',
        button: `打开 Accio ${label}登录页`,
        busyText: `等待 Accio ${label}登录完成…`,
        modes: [
          {
            value: 'embedded',
            label: '内嵌窗口（推荐）',
            hint: '将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待',
          },
          {
            value: 'external',
            label: '系统浏览器',
            hint: '将用系统默认浏览器打开登录页（会复用浏览器里已登录的 Accio 账号）；'
              + '完成登录后自动加入账号列表，关掉弹窗即取消等待',
          },
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
    };
  }

  // 顺序即界面上「提供商」分段的顺序：国际版在前（默认安装的版本）
  window.wbAccioAddForms = [
    accioForm({
      provider: 'accio',
      label: '国际版',
      site: 'www.accio.com',
      siteNote: '国际版与国内版是**两套独立的账号**（同一账号体系的两个站点），凭证不通用。',
    }),
    accioForm({
      provider: 'accio-cn',
      label: '国内版',
      site: 'www.accio-ai.com',
      siteNote: '国内版的登录站点是 www.accio-ai.com，与国际版不是同一站。',
    }),
  ];
})();
