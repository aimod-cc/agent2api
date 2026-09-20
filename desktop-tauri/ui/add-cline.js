/* Cline 添加配置（**两个提供商**：Cline Free / Cline Pass）；由 add-provider-forms.js 统一渲染并绑定。 */
(() => {
  /**
   * ── 为什么是两个表单配置而不是一个配置带一个「额度池」下拉 ──────
   * Cline 的上游模型带**计费通道前缀**（`cline-free/...` / `cline-pass/...`），
   * 前缀是它的计费通道选择器，必须原样发给上游。早先的实现把两个前缀当成
   * **同一个账号上的两个额度池**：界面上是一家 Cline + 一个池下拉，池存在
   * 账号记录里，模型列表按「账号库里有哪些池」过滤。
   *
   * 那套做法的问题：池是**账号的属性**，于是模型列表随账号变化、
   * 「哪个池能用」在界面上看不出来、同一个模型的启停规则还会跨池互相影响。
   * 现在按**两家提供商**建模 —— 各占一个分组、各有一套账号与模型清单，
   * 加了哪家才显示哪家的模型。因此这里输出两份配置：除了 id、标签与提示文案
   * 之外完全同构（共享字段走同一个工厂，避免两处各写一遍之后不对称）。
   *
   * 转发侧不变：上游模型 id 的池前缀仍是原样发的通道选择器，
   * 「去前缀」的友好名仍由 `model_rules` 的默认映射种子给出。
   */

  /**
   * 拼一份 Cline 表单配置。
   *
   * @param {object} spec
   * @param {string} spec.provider provider id（`cline-free` / `cline-pass`）
   * @param {string} spec.label    展示名（英文原样，两个池在界面上靠它区分）
   * @param {string} spec.poolNote 这一家收哪个通道的模型（拼进各段说明里）
   */
  function clineForm({ provider, label, poolNote }) {
    return {
      provider,
      label,
      // Cline 有「桌面端实时登录态」可导入（~/.cline/data/settings/providers.json）。
      // 桌面登录态本身**不属于任何池**，所以两家都能导入 —— 想要两个池都用，
      // 就在两家各导入一次（各得一条记录）。
      desktop: true,
      webLogin: {
        // ── 设备授权登录（WorkOS RFC 8628）──────────────────────
        // 与另外几家的「网页登录」形态不同：这里没有回调、没有自定义协议，
        // 就是「打开授权页 → 用户确认 → 网关轮询拿到令牌」。
        // 因此**两种打开方式都可行**（回调与浏览器无关），默认给内嵌窗口。
        //
        // 设备授权与池无关（只有 api.cline.bot 一台站点、一套协议），
        // 登录只决定落哪一家的账号 —— 所以两家这段文案只有落点不同。
        noteHtml: '打开 Cline 授权页并完成确认，网关会自动取回凭证并加入账号列表。'
          + `登录成功的账号会归入 <b>${label}</b>${poolNote}`,
        button: '打开 Cline 授权页',
        busyText: '等待 Cline 授权确认…',
        modes: [
          {
            value: 'embedded',
            label: '内嵌窗口（推荐）',
            hint: '将打开 Cline 授权页，确认后自动加入账号列表。关掉窗口即取消等待',
          },
          {
            value: 'external',
            label: '系统浏览器',
            hint: '将用系统默认浏览器打开授权页（会复用浏览器里已登录的 Cline 账号）；确认后自动加入列表。关掉弹窗即取消等待',
          },
        ],
      },
      manualTitle: '填写凭证',
      manualNote: 'accessToken 是 Cline 的登录凭证（形如 workos:eyJ…，可只填这一段）；'
        + 'refreshToken 可选，填了之后到期能自动续期。两者都可以从 Cline 的登录态文件里取到'
        + '（~/.cline/data/settings/providers.json 的 providers.cline.settings.auth）。'
        + `同一个 Cline 账号两个额度池都能用，这里添加的账号归入 ${label}。`,
      fields: [
        { key: 'accessToken', label: 'accessToken', rows: 3, placeholder: '粘贴 workos:… 开头的令牌（不带前缀也会自动补上）' },
        { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '可选，没有则无法自动续期' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空使用账号姓名、邮箱或令牌指纹' },
      ],
      desktopNote: '读本机 Cline 客户端当前的登录态建一个「桌面端实时登录态」账号：'
        + '凭证不落账号文件、每次实时读取（删掉这条记录不影响 Cline 客户端登录态）。'
        + '客户端重新登录后网关立刻跟上，不需要手动同步。'
        + `桌面登录态两个额度池都能建，这一条归入 ${label}。`,
      desktopHint: '读取 ~/.cline/data/settings/providers.json，需已在 Cline 客户端登录',
    };
  }

  // 顺序即界面上「提供商」分段的顺序：免费池在前（无门槛，更常用）
  window.wbClineAddForms = [
    clineForm({
      provider: 'cline-free',
      label: 'Cline Free',
      poolNote: '（免费额度池，模型名带 cline-free/ 前缀）。',
    }),
    clineForm({
      provider: 'cline-pass',
      label: 'Cline Pass',
      poolNote: '（订阅池，模型名带 cline-pass/ 前缀，需要账号有对应订阅）。',
    }),
  ];
})();
