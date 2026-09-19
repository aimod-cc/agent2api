/* Agent2API · 账号卡片的余额 / 积分明细面板（两套形状的渲染） */
/* global wbApp, wbAccountsModel */

/**
 * 账号行内「积分 / 余额」明细的渲染层，从 accounts-model.js 拆出
 * （accounts-view.js 已到 800 行上限，新增的渲染分支没有地方放）。
 *
 * ── 为什么要渲染**两套形状** ────────────────────────────────────
 * 四家都支持余额查询，但它们的返回形状不同：
 *   1. **workbuddy 的既有形状**（改造前就有，前端面板一直在用）：
 *      `{kind, unlimited, totalLeft, planLeft, bonusLeft}`。
 *      这套形状**一个字都不能改** —— 改它等于让那个已经上线的面板显示退化。
 *   2. **三家的统一形状**（本次移植时定稿，契约见
 *      `ProviderAdapter::query_usage` 的文档）：
 *      `{available, unit, wallets[], subscription}`。四家的适配器都按它实现，
 *      workbuddy 是唯一的例外（它透出的是既有形状）。
 *
 * ── 形状探测的判据（为什么按字段而不是按 provider）──────────────
 * 按 **`totalLeft` 这个键存在与否**（workbuddy 形状的判别键）分流，而不是按
 * `account.provider`：
 *   · 后端已经按 provider 分派过适配器，前端再判一次 provider 是把同一条知识
 *     写两遍 —— 将来某家换了形状（或第五家出现），按 provider 的写法要同步改
 *     前端，按字段的写法自动跟上；
 *   · 更实际的一点：**探测的是「响应长什么样」，而这正是渲染要回答的问题**。
 *     provider 只决定「谁去查」，不决定「查回来是什么形状」。
 *   · 两个键同时缺失的兜底：显示「未返回可识别的余额数据」——它比凭 provider
 *     猜一种形状去渲染更诚实（猜错会把 undefined 渲染成一片「—」）。
 */

(() => {
  const { esc, formatTime } = wbApp;

  /**
   * 「未配置查询凭证」的标记（与后端 `providers::adapter::USAGE_NOT_CONFIGURED_CODE`
   * 逐字一致）。这是一个**前后端契约常量**：改一边必须改另一边，
   * 不一致的后果是那种情况退回红色「查询失败」。
   */
  const NOT_CONFIGURED_CODE = 'usage_not_configured';

  /** 明细条右上角的关闭按钮（与 accounts-model.js 里的同形，样式共用 .panel-close） */
  const panelClose = kind =>
    `<button class="panel-close" data-panel-close="${kind}" title="收起">✕</button>`;

  /** 数值 → 展示串（调用方负责 `esc`；`null`/`undefined`/空串 → 「—」）。
   *  不可解析的值原样透出文本 —— 显示上游原文比显示「—」更利于排障。 */
  function numberText(value) {
    if (value === null || value === undefined || value === '') return '—';
    const number = Number(value);
    return Number.isFinite(number) ? String(number) : String(value);
  }

  /**
   * 订阅信息块（统一形状的 `subscription`）。
   *
   * `expireAt` 各家的类型不同（小浣熊给的是上游原样的字符串日期，AutoClaw 可能
   * 给时间戳）：能解析成日期的按本地时间格式化，否则原样显示 —— 不猜、不丢。
   */
  function subscriptionHtml(subscription) {
    if (!subscription || typeof subscription !== 'object') return '';
    const parts = [];
    const plan = subscription.planName ? esc(String(subscription.planName)) : '';
    if (plan) parts.push(`<span>套餐 <b>${plan}</b></span>`);
    if (subscription.status) parts.push(`<span>状态 ${esc(String(subscription.status))}</span>`);
    const expireAt = subscription.expireAt;
    if (expireAt !== null && expireAt !== undefined && expireAt !== '') {
      const asNumber = Number(expireAt);
      const text = Number.isFinite(asNumber) && asNumber > 1e11
        ? formatTime(asNumber)
        : String(expireAt);
      if (text) parts.push(`<span>到期 ${esc(text)}</span>`);
    }
    if (Number.isFinite(Number(subscription.remainQuota))) {
      parts.push(`<span>余量 <b>${esc(numberText(subscription.remainQuota))}</b></span>`);
    }
    if (Number.isFinite(Number(subscription.totalQuota))) {
      parts.push(`<span>总量 ${esc(numberText(subscription.totalQuota))}</span>`);
    }
    return parts.join('');
  }

  /**
   * 统一形状（`available` / `unit` / `wallets` / `subscription`）的渲染。
   * `wallets` 逐条列明细；`subscription` 有就附加一行。
   */
  function unifiedPanelHtml(entry) {
    const unit = esc(String(entry.unit || '积分'));
    const wallets = Array.isArray(entry.wallets) ? entry.wallets : [];
    const head = `<span>可用 <b class="big">${esc(numberText(entry.available))}</b> ${unit}</span>`;
    const details = wallets
      .map(wallet => {
        const name = esc(String(wallet?.displayName || wallet?.type || '明细'));
        // 上游给的展示串优先（带千分位/单位的格式化），没有才按数值拼
        const view = esc(wallet?.balanceView
          ? String(wallet.balanceView)
          : numberText(wallet?.balance));
        return `<span>${name} <b>${view}</b></span>`;
      })
      .join('');
    const subscription = subscriptionHtml(entry.subscription);
    return `<div class="row-panel">${head}${details}${subscription}${panelClose('usage')}</div>`;
  }

  /** workbuddy 既有形状（`kind` / `unlimited` / `totalLeft` / `planLeft` / `bonusLeft`） */
  function legacyPanelHtml(entry) {
    // 三字段简报：总剩余 / 套餐基础 / 平台奖励（企业账号后两项为 null）
    const totalLeft = entry.unlimited ? '∞' : esc(numberText(entry.totalLeft));
    const planLeft = esc(numberText(entry.planLeft));
    const bonusLeft = esc(numberText(entry.bonusLeft));
    const kindTag = entry.kind === 'enterprise' ? '<span>（企业账号）</span>' : '';
    return `<div class="row-panel">`
      + `<span>总剩余 <b class="big">${totalLeft}</b></span>`
      + `<span>套餐 <b>${planLeft}</b></span>`
      + `<span>奖励 <b>${bonusLeft}</b></span>`
      + kindTag + panelClose('usage')
      + `</div>`;
  }

  /**
   * 缓存条目 → 失败描述的**唯一入口**（面板渲染与 toast 提示共用同一份判据）。
   *
   * 返回 null 表示这不是失败（还在查询中 / 是结果）；否则 `{message, notConfigured}`。
   * `notConfigured` 由后端给的 `code` 判定，**不是匹配文案** —— 见 `usagePanelHtml`。
   */
  function usageFailureOf(entry) {
    if (entry === undefined || entry === null) return null;
    if (typeof entry === 'string') return { message: entry, notConfigured: false };
    if (typeof entry !== 'object') return null;
    if (!entry.error) return null;
    return {
      message: String(entry.error),
      notConfigured: entry.code === NOT_CONFIGURED_CODE,
    };
  }

  /**
   * 余额明细的入口。
   *
   * `entry` 由调用方从缓存里取：
   *   undefined = 尚未查询，null = 查询中，
   *   `{error, code?}` = 失败或未配置（见下），对象 = 结果。
   *
   * ── 「未配置」为什么是中性的 ───────────────────────────────────
   * CatPaw 的余额接口要一个**单独的**网页会话凭证（token2），没配置时后端返回
   * `code: "usage_not_configured"`。那不是故障：账号本身完全正常、转发照跑，
   * 只是用户还没告诉网关那个凭证长什么样。把它渲染成红色的「查询失败」会让人
   * 去排查一个不存在的故障，所以判据用后端给的 `code` **而不是匹配文案**
   * （措辞一改，按文案的写法就会静默退回红色）。
   */
  function usagePanelHtml(account, entry) {
    if (entry === undefined) {
      return `<div class="row-panel">余额尚未查询，请点击该卡片上的「积分」按钮。${panelClose('usage')}</div>`;
    }
    if (entry === null) {
      return `<div class="row-panel"><span class="badge warn">正在查询…</span>${panelClose('usage')}</div>`;
    }
    // 失败行：`code` 是 usage_not_configured 时走中性样式（不标红、不叫「失败」）
    const failure = usageFailureOf(entry);
    if (failure) {
      if (failure.notConfigured) {
        return `<div class="row-panel"><span class="badge tag plain">未配置查询</span> `
          + `${esc(failure.message)}${panelClose('usage')}</div>`;
      }
      return `<div class="row-panel error"><span class="badge bad">查询失败</span> `
        + `${esc(failure.message)}${panelClose('usage')}</div>`;
    }
    if (typeof entry !== 'object') {
      return `<div class="row-panel"><span class="badge warn">未返回可识别的余额数据</span>`
        + `${panelClose('usage')}</div>`;
    }
    // 形状探测：`totalLeft` 是 workbuddy 既有形状的判别键（见模块头）
    if (Object.prototype.hasOwnProperty.call(entry, 'totalLeft')) {
      return legacyPanelHtml(entry);
    }
    if (Object.prototype.hasOwnProperty.call(entry, 'available')
      || Array.isArray(entry.wallets)) {
      return unifiedPanelHtml(entry);
    }
    return `<div class="row-panel"><span class="badge warn">未返回可识别的余额数据</span>`
      + `${panelClose('usage')}</div>`;
  }

  window.wbUsagePanel = { usagePanelHtml, usageFailureOf, NOT_CONFIGURED_CODE };
})();
