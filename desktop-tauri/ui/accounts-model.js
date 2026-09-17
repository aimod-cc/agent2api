/* WorkBuddy 本地代理 · 账号领域判定与标签渲染 */
/* global wbApp */

/**
 * 从 accounts-view.js 抽出的「纯逻辑 + 标签 HTML」层：
 * 判定账号是否可用 / 是否限流、生成状态标签与第二行小字。
 * 这些函数不读写模块状态、不碰事件，只依赖 window.wbApp 的 esc / formatTime /
 * formatShortTime，因此单独成文件，让账号视图聚焦在渲染与交互上。
 *
 * 通过 window.wbAccountsModel 暴露；accounts-view.js 与 app.js（顶栏状态）都会用到。
 */
(() => {
  const { esc, formatTime, formatShortTime } = wbApp;

  // ─── 基础判定 ──────────────────────────────

  function typeLabel(type) {
    if (type === 'enterprise') return '企业';
    if (type === 'ultimate') return '旗舰';
    return '个人';
  }

  /**
   * 转发顺序排序键：优先级升序，并列时按加入时间。
   * 与后端 src/workbuddy-account-store.mjs 的 byPriorityOrder 保持一致
   * （渲染层无法 import 后端 ESM，只能同构实现；改一处必须同步另一处）。
   * 优先级在写入侧强制唯一，并列只会出现在手工编辑的账号文件里。
   */
  function byPriorityOrder(a, b) {
    const diff = Number(a?.priority ?? 100) - Number(b?.priority ?? 100);
    if (diff !== 0) return diff;
    return (Number(a?.addedAt) || 0) - (Number(b?.addedAt) || 0);
  }

  /** 账号是否启用（禁用账号不参与转发） */
  function isEnabled(account) {
    return account?.enabled !== false;
  }

  /**
   * 账号是否处于限流状态（存在未到恢复时间的限额记录）。
   * 传入 model 时只判定该模型 —— 限额是按模型记的，这是「模型筛选」的核心：
   * 一个账号可能对 A 模型限额、对 B 模型完全正常。
   */
  function isRateLimited(account, model = '') {
    const limits = account?.rateLimits || {};
    const now = Date.now();
    if (model) return Number(limits[model]?.resetAt) > now;
    return Object.values(limits).some(info => Number(info?.resetAt) > now);
  }

  /** 该账号此刻能否承接指定模型的请求（与后端 accountUsability 同构） */
  function usableForModel(account, model) {
    if (!isEnabled(account)) return false;
    return !isRateLimited(account, model);
  }

  /** 账号所属版本：cn=国内 / intl=国际（缺省视为国内，兼容旧账号记录） */
  function accountEdition(account) {
    return account?.edition === 'intl' ? 'intl' : 'cn';
  }

  /** 国际版没有签到活动，签到相关入口对其隐藏 */
  function supportsCheckin(account) {
    return accountEdition(account) !== 'intl';
  }

  /** 可参与签到的账号（一键签到只用这批：启用 + 国内版） */
  function checkinableAccounts(list) {
    return (list || []).filter(account => isEnabled(account) && supportsCheckin(account));
  }

  // ─── 状态标签 ───────────────────────────────

  function statusTag(text, kind, title) {
    const cls = ['badge', 'tag', kind].filter(Boolean).join(' ');
    return `<span class="${cls}"${title ? ` title="${esc(title)}"` : ''}>${esc(text)}</span>`;
  }

  /**
   * 限额标签：状态码 + 恢复时间。
   *
   * 选了模型时只看该模型的限额（列表已是该模型的队列，别的模型的限额与本视图无关，
   * 显示出来只会造成「标着 429 却还能用」的困惑）；未选模型时汇总展示。
   */
  function rateLimitTag(account, model = '') {
    const limits = account.rateLimits || {};
    const now = Date.now();
    const active = Object.entries(limits)
      .map(([id, info]) => ({ model: id, ...info }))
      .filter(item => Number(item.resetAt) > now)
      .filter(item => !model || item.model === model)
      .sort((a, b) => a.resetAt - b.resetAt);
    if (!active.length) return '';
    const first = active[0];
    const extra = active.length > 1 ? ` +${active.length - 1}` : '';
    const title = [
      '已达上限的模型（其它模型不受影响）：',
      ...active.map(item => `${item.model}: ${item.status}，${formatTime(item.resetAt)} 恢复`),
    ].join('\n');
    return statusTag(`${first.status} · ${formatShortTime(first.resetAt)} 恢复${extra}`, 'warn', title);
  }

  /**
   * 状态标签集合：这一区只表达**健康状态**。
   *
   * 「首选」不在这里 —— 它表达的是转发顺序（队首），不是账号健康状况。
   * 卡片底部已有 ★ 与「已是首选」实心块表达同一件事，再在状态区重复一遍
   * 只会让这一列没法收窄，也混淆「正常 / 限流 / 禁用」的语义。
   *
   * 只标「需要关注的状态」；一切正常时给一个「正常」标签 ——
   * 否则整片都是空占位，反而看不出这个账号到底能不能用。
   */
  function accountTags(account, currentAccountId, model = '') {
    const enabled = isEnabled(account);
    const tags = [
      enabled ? '' : statusTag('已禁用', 'bad', '该账号已禁用，不参与转发'),
      // 代理配了解析不出来时明确标出：转发会回退直连，属于需要留意的情况
      account.proxy?.error ? statusTag('代理异常', 'bad', `${account.proxy.error}（转发时会回退直连）`) : '',
      account.available === false ? statusTag('不可用', 'bad', account.reason || '账号不可用') : '',
      rateLimitTag(account, model),
    ].filter(Boolean);
    if (!tags.length) return statusTag('正常', 'ok', '该账号可用，未触发限额');
    return tags.join('');
  }

  // ─── 单元格内容 ─────────────────────────────

  /**
   * 有效期文案（第二行小字用）。
   * 返回纯文本而不是徽章 —— 有效期不是「需要盯」的状态，
   * 做成徽章只会和真正重要的状态标签抢注意力。
   */
  function expiryText(expiresAt) {
    if (!expiresAt) return '未提供过期时间';
    const left = Number(expiresAt) - Date.now();
    if (left <= 0) return '已过期';
    if (left < 3600e3) return `${Math.max(1, Math.round(left / 60e3))} 分钟后过期`;
    if (left < 48 * 3600e3) return `${(left / 3600e3).toFixed(1)} 小时后过期`;
    return `${Math.floor(left / 24 / 3600e3)} 天后过期`;
  }

  /** 第二行小字：额度类型 / 有效期 / 尾号 / 出口 */
  function metaLine(account) {
    const parts = [
      `<span title="该账号的额度类型">${esc(typeLabel(account.type))}</span>`,
      `<span>${esc(expiryText(account.expiresAt))}</span>`,
    ];
    if (account.tokenTail) parts.push(`<span>尾号 <span class="tail">${esc(account.tokenTail)}</span></span>`);
    if (account.proxy && !account.proxy.error) {
      parts.push(`<span class="proxy" title="该账号经此出口访问上游">出口 ${esc(account.proxy.label || '已设置')}</span>`);
    }
    return parts.join('<span class="sep">·</span>');
  }

  /** 版本徽章：国内 / 国际，配色固定（类名 edition-* 被样式与批量徽章共用） */
  function editionCell(account) {
    const edition = accountEdition(account);
    const label = account.editionLabel || (edition === 'intl' ? '国际版' : '国内版');
    return `<span class="badge edition-${edition}">${esc(label)}</span>`;
  }

  // ─── 行内面板（积分 / 签到） ─────────────────
  //
  // 面板按需展开：调用方只在用户点过「积分」/「签到」后才渲染。
  // 这里的函数是纯展示，不判断展开状态 —— entry 由调用方从缓存里取：
  //   undefined = 尚未查询，null = 查询中，string = 出错，对象 = 结果

  /** 明细条右上角的关闭按钮 */
  const panelClose = kind =>
    `<button class="panel-close" data-panel-close="${kind}" title="收起">✕</button>`;

  function usagePanelHtml(account, entry) {
    if (entry === undefined) {
      return `<div class="row-panel">积分尚未查询，请点击该卡片上的「积分」按钮。${panelClose('usage')}</div>`;
    }
    if (entry === null) {
      return `<div class="row-panel"><span class="badge warn">正在查询积分…</span>${panelClose('usage')}</div>`;
    }
    if (typeof entry === 'string') {
      return `<div class="row-panel error"><span class="badge bad">查询失败</span> ${esc(entry)}${panelClose('usage')}</div>`;
    }
    // 三字段简报：总剩余 / 套餐基础 / 平台奖励（企业账号后两项为 null）
    const totalLeft = entry.unlimited ? '∞' : (entry.totalLeft ?? '—');
    const planLeft = entry.planLeft ?? '—';
    const bonusLeft = entry.bonusLeft ?? '—';
    const kindTag = entry.kind === 'enterprise' ? '<span>（企业账号）</span>' : '';
    return `<div class="row-panel">`
      + `<span>总剩余 <b class="big">${esc(totalLeft)}</b></span>`
      + `<span>套餐 <b>${esc(planLeft)}</b></span>`
      + `<span>奖励 <b>${esc(bonusLeft)}</b></span>`
      + kindTag + panelClose('usage')
      + `</div>`;
  }

  function checkinPanelHtml(account, entry) {
    const close = panelClose('checkin');
    if (!supportsCheckin(account)) {
      return `<div class="row-panel">国际版暂无签到活动，该账号不参与签到。${close}</div>`;
    }
    if (!isEnabled(account)) {
      return `<div class="row-panel">账号已禁用，不参与批量签到；如需签到请先启用。${close}</div>`;
    }
    if (entry === undefined) {
      return `<div class="row-panel">签到状态未查询，请点击该卡片上的「签到」按钮。${close}</div>`;
    }
    if (entry === null) {
      return `<div class="row-panel"><span class="badge warn">正在签到…</span>${close}</div>`;
    }
    if (typeof entry === 'string') {
      return `<div class="row-panel error"><span class="badge bad">签到失败</span> ${esc(entry)}${close}</div>`;
    }
    if (!entry || entry.success === undefined) {
      return `<div class="row-panel">未获取到签到结果${close}</div>`;
    }
    if (entry.success) {
      const d = entry.data || {};
      const parts = ['<span class="badge ok">✅ 签到成功</span>'];
      if (Number.isFinite(d.points) && d.points) parts.push(`<span>本次 +${esc(d.points)} 积分</span>`);
      if (Number.isFinite(d.continuousDays)) parts.push(`<span>连续 ${esc(d.continuousDays)} 天</span>`);
      if (Number.isFinite(d.totalDays)) parts.push(`<span>累计 ${esc(d.totalDays)} 天</span>`);
      return `<div class="row-panel">${parts.join('')}${close}</div>`;
    }
    return `<div class="row-panel"><span class="badge warn">${esc(entry.msg || '未领取')}</span>`
      + `<span>code=${esc(entry.code ?? '?')}</span>${close}</div>`;
  }

  window.wbAccountsModel = {
    byPriorityOrder,
    typeLabel,
    isEnabled,
    isRateLimited,
    usableForModel,
    accountEdition,
    supportsCheckin,
    checkinableAccounts,
    statusTag,
    rateLimitTag,
    accountTags,
    expiryText,
    metaLine,
    editionCell,
    usagePanelHtml,
    checkinPanelHtml,
  };
})();
