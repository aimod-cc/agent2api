/* ZCode 添加配置（**两个地区**：国内版 / 国际版）；由 add-provider-forms.js 统一渲染并绑定。 */
(() => {
  /**
   * ── 为什么是两个表单配置而不是一个配置带一个「地区」下拉 ──────
   * ZCode 的两个地区共用同一个 zcode 平面（登录 / 领取都在 `zcode.z.ai`，
   * 两地的 OAuth 只是 `provider` 取值不同），只有**推理站点**不同
   * （国内 `open.bigmodel.cn` / 国际 `api.z.ai`）。
   *
   * 按「一家的一个字段」建模的后果与 Cline 两池 / AutoClaw 两地区 / Accio
   * 两站点那三次一模一样：地区成了**账号的属性**，界面上混在一起，
   * 而「哪个账号走哪个站点」在列表里看不出来，账号库里的记录也无法按地区
   * 隔离。因此这里输出两份配置（除 id、标签与站点文案外完全同构）。
   *
   * 转发与账号侧对应 `zcode` / `zcode-intl` 两家 provider（见后端
   * `providers::zcode::region::Region`）。
   */

  /**
   * 拼一份 ZCode 表单配置。
   *
   * @param {object} spec
   * @param {string} spec.provider provider id（`zcode` / `zcode-intl`）
   * @param {string} spec.label    展示名
   * @param {string} spec.site     推理站点（写进提示文案）
   * @param {string} spec.planNote 套餐说明（两地的套餐不通用）
   */
  function zcodeForm({ provider, label, site, planNote }) {
    return {
      provider,
      label,
      // 本家没有「桌面端实时登录态」可导入：ZCode 客户端的凭证在它自己的
      // 加密存储里，没有 auth.json 那种稳定可读的形态（与 Accio 同一处境）
      desktop: false,
      webLogin: {
        // ── 授权地址由**服务端**给，且不回本机（这一家独有的形态）────
        // 其余各家的网页登录都在本机等一个回调（loopback 端口或自定义协议深链），
        // ZCode 不是：网关拿到的是一张一次性授权地址，用户在浏览器里授权后
        // **由 ZCode 服务端记录结果**，网关在后台轮询取回凭证。
        // 因此内嵌窗口与系统浏览器两条路都走得通，且浏览器在哪台设备上都行
        // （与 Accio 同构）；但有一点必须提前告诉用户，否则会被当成失败 ——
        // 见下面 modes 的 hint。
        noteHtml: `打开 ZCode <b>${label}</b>的官方授权页并用你的账号登录。`
          + '授权结果由 ZCode 服务端记录，网关在后台自动取回凭证并加入账号列表。',
        button: `打开 ZCode ${label}授权页`,
        busyText: `等待 ZCode ${label}授权完成…`,
        modes: [
          {
            value: 'embedded',
            label: '内嵌窗口（推荐）',
            hint: '将打开内嵌窗口；授权完成后自动加入账号列表。关掉窗口即取消等待。'
              + '授权页最后可能提示无法打开 zcode:// 链接，这是正常的 —— 结果已由服务端记下',
          },
          {
            value: 'external',
            label: '系统浏览器',
            hint: '将用系统默认浏览器打开授权页（会复用浏览器里已登录的 ZCode 账号）；'
              + '授权完成后自动加入账号列表。页面最后可能提示无法打开 zcode:// 链接，属正常现象',
          },
        ],
      },
      manualTitle: '填写凭证',
      manualNoteHtml: '本家有两个**互不替代**的凭证，按你要用的功能填，至少填一个：'
        + '<b>accessToken</b> 用于转发推理（打 <code>' + site + '</code>）；'
        + '<b>jwt</b> 用于领取套餐（打 <code>zcode.z.ai</code>，官方叫 Coding Plan JWT，'
        + '是一串三段点分的字符串）。只填 jwt 的账号能领套餐但不能转发，反之亦然。'
        + `请填写 <b>${label}</b>账号的凭证 —— ${planNote}`
        + '（最容易拿到的办法：直接用上方的「网页登录」，两个凭证会一起拿到。）',
      fields: [
        { key: 'accessToken', label: 'accessToken', rows: 3, optional: true, placeholder: '用于转发；不填则这个账号不能转发' },
        { key: 'jwt', label: 'Coding Plan JWT', inputKey: 'jwt', rows: 3, optional: true, placeholder: '用于领取套餐；不填则这个账号不能领取' },
        { key: 'userId', label: '用户 ID', optional: true, placeholder: '可选；用于生成账号 id 与展示名' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空自动生成' },
      ],
    };
  }

  // 顺序即界面上「提供商」分段的顺序：国内版在前（国内网络环境下更常被添加的那个，
  // 与后端注册表 PROVIDERS 的排列一致）
  window.wbZcodeAddForms = [
    zcodeForm({
      provider: 'zcode',
      label: '国内版',
      site: 'open.bigmodel.cn',
      planNote: '国内版与**国际版是两套独立的账号与套餐**，凭证与领取的套餐都不通用。',
    }),
    zcodeForm({
      provider: 'zcode-intl',
      label: '国际版',
      site: 'api.z.ai',
      planNote: '国际版的推理站点是 api.z.ai，与国内版不是同一站；套餐也各自独立。',
    }),
  ];
})();
