/* Agent2API · 账号标签与行内面板渲染（provider 维度的判定与队列已拆到 accounts-groups.js） */
/* global wbApp */

/**
 * 账号的「标签 + 行内面板」渲染层：状态标签、版本徽章、⋯ 菜单、行内限流 / 积分 / 签到面板。
 *
 * ── 与 accounts-groups.js 的分工（按职责拆分后）─────────────────
 * provider 能力表与摘要归一化、可用性 / 限流判定、筛选口径与分段计数、全局队列的
 * 位置表 —— 这些**纯逻辑**在 accounts-groups.js（挂 window.wbAccountsGroups），
 * 本文件按名解构回来，并在文件末尾**原样再导出**。于是 window.wbAccountsModel 的
 * 公开面与拆分前同构，消费方（accounts-view.js / app.js）不必改引用路径 ——
 * 与本项目既有的转发做法一致（accounts-view.js 也把 accounts-model 的函数再导出给
 * app.js）。加载顺序由 index.html 保证：accounts-groups.js 必须先于本文件加载。
 *
 * 卡片时代的渲染函数（单卡 HTML / 卡片提示条 / 组头等）已随卡片视图一起删除：
 * 表格化之后它们不再被任何调用方引用，留着只会让人误改到一份死代码。
 *
 * 这些函数不读写模块状态、不碰事件，只依赖 window.wbApp 的 esc / formatTime。
 */
(() => {
  const { esc, formatTime } = wbApp;

  /**
   * 纯逻辑与分组全部来自 accounts-groups.js（index.html 保证它先于本文件加载）。
   * 按名解构而不是每次写 window.wbAccountsGroups.xxx：调用点保持拆分前的裸函数名，
   * 搬移后函数体一行未改；万一该文件没加载，这里会直接抛错（首屏即暴露），
   * 比静默少一个按钮更容易发现。
   */
  const {
    DEFAULT_PROVIDER_ID,
    RACCOON_PROVIDER_ID,
    providerOf,
    providerFeatures,
    providerSummaries,
    identifierOf,
    tokenExpiryOf,
    supportsUsage,
    isDesktopAccount,
    supportsChat,
    byPriorityOrder,
    typeLabel,
    isEnabled,
    isRateLimited,
    accountEdition,
    supportsCheckin,
    checkedInToday,
    checkinableAccounts,
    matchProvider,
    matchEnabled,
    matchLimit,
    visibleAccounts,
    filterCounts,
    positionMap,
    activeLimits,
  } = window.wbAccountsGroups;

  // ─── 状态标签 ───────────────────────────────

  function statusTag(text, kind, title) {
    const cls = ['badge', 'tag', kind].filter(Boolean).join(' ');
    return `<span class="${cls}"${title ? ` title="${esc(title)}"` : ''}>${esc(text)}</span>`;
  }

  /**
   * 状态标签集合：这一区只表达**健康状态**。
   *
   * 「设为首选」是排序操作，不是账号健康状况。
   * 「限流」也不在这里 —— 限额按模型记，它有自己的一列（点开看具体模型），
   * 挤在状态列里只能给一个没有信息量的「限流」两个字。
   * **只标「需要关注的状态」，一切正常时返回空串** ——
   * 启用 / 禁用由开关自身表达（轨道位置 + 滑块），再补一枚「启用」徽章
   * 是在同一格里说第二遍同一件事，所以不补。
   */
  function accountTags(account) {
    const tags = [
       // 没有转发能力的家：它的启用开关对转发没有意义，这里如实说明，
       // 而不是留一片空白让人以为「没标记就是好的」。判据是后端的 chatSupported
       // （适配器的 supports_chat() 声明），当前五家都能转发，因此这条在
       // 正常配置下不会出现 —— 留着是为了「将来某家处于只有账号管理的
       // 过渡期」时界面能自己说清楚，而不用再改这里。
       supportsChat(account)
        ? '' : statusTag('仅账号管理', 'plain', '该提供商的推理转发尚未接入，账号不参与转发'),
      // 代理配了解析不出来时明确标出：转发会回退直连，属于需要留意的情况
      account.proxy?.error ? statusTag('代理异常', 'bad', `${account.proxy.error}（转发时会回退直连）`) : '',
      account.available === false ? statusTag('不可用', 'bad', account.reason || '账号不可用') : '',
    ].filter(Boolean);
    // 已禁用的账号这里也不再补「禁用」徽章：开关是关着的，那本身就是标识。
    return tags.join('');
  }

  /**
   * 版本后缀：国内 / 国际。只返回文字，由调用方拼进提供商徽章 ——
   * 「WorkBuddy 国际版」是**一枚**徽章，与 AutoClaw 那种「名字自带版本」的
   * 家看起来是同一种标签，不再两枚并排。无版本的家园地返回空串。
   */
  function editionSuffix(account) {
    if (!providerFeatures(providerOf(account)).edition) return '';
    const edition = accountEdition(account);
    return account.editionLabel || (edition === 'intl' ? '国际版' : '国内版');
  }

  // ─── 行内面板（积分 / 签到） ─────────────────
  //
  // 面板按需展开：调用方只在用户点过「积分」/「签到」后才渲染。
  // 这里的函数是纯展示，不判断展开状态 —— entry 由调用方从缓存里取：
  //   undefined = 尚未查询，null = 查询中，string = 出错，对象 = 结果
  //
  // 余额面板（usage）的实现搬到了 `usage-panel.js`：它现在要渲染**两套形状**
  // （workbuddy 既有形状 + 三家统一形状）与「未配置查询」的中性态，篇幅放不进
  // 本文件（见那个文件的模块头）。这里按名转调，消费方拿到的还是同一个
  // `wbAccountsModel.usagePanelHtml`。

  /** 明细条右上角的关闭按钮 */
  const panelClose = kind =>
    `<button class="panel-close" data-panel-close="${kind}" title="收起">✕</button>`;

  /**
   * 限流明细面板：这个账号**当前限流中的模型**逐行列出 —— 模型名、恢复时间、
   * 上游给的原因，以及「清除标记」动作（单条）与「全部清除」。
   *
   * 为什么值得一整块面板而不是悬浮提示：限流是「按模型」的（一个账号完全可能
   * A 模型限流、B 模型正常），把模型名列出来才能回答「到底是谁把我限了」；
   * 而「清除标记」是真实动作，悬浮层里放不下也点不稳。
   *
   * 「清除标记」只作用于本机这份冷却标记：下一次请求若上游仍限流会再次被标记，
   * 所以它是安全且可逆的，不需要二次确认。已展开但记录恰好全部过期时给一句
   * 中性说明 —— 数据是两次读盘之间变了的，不该渲染成一块空面板。
   */
  function limitPanelHtml(account) {
    const close = panelClose('limits');
    const entries = activeLimits(account);
    if (!entries.length) {
      return `<div class="row-panel limit-panel">当前没有限流中的模型。${close}</div>`;
    }
    const rows = entries.map(entry => {
      const reset = formatResetText(entry.resetAt);
      const resetText = reset === RESET_UNKNOWN ? '恢复时间未知' : `${reset} 恢复`;
      const reason = entry.message || (entry.status ? `上游返回 ${entry.status}` : '');
      return `<div class="lp-row">`
        + `<span class="lp-model" title="${esc(entry.model)}">${esc(entry.model)}</span>`
        + `<span class="badge tag warn">限流中</span>`
        + `<span class="lp-reset" title="到恢复时间后自动解除，无需手动操作">${esc(resetText)}</span>`
        + `<span class="lp-reason" title="${esc(reason)}">${esc(reason)}</span>`
        + `<button data-limit-clear="${esc(entry.model)}" title="清掉本机的限流标记，立刻重新尝试该模型（上游若仍在限流会再次被标记）">清除标记</button>`
        + `</div>`;
    }).join('');
    return `<div class="row-panel limit-panel">${close}`
      + `<div class="lp-head"><b>${esc(account.nickname || account.name || account.id)}</b>`
      + `<span class="muted">${entries.length} 个模型限流中 · 记录来自上游 429 / 限额码，到恢复时间自动解除</span>`
      + `<button data-limit-clear-all title="清掉该账号全部模型的限流标记">全部清除</button></div>`
      + rows
      + `</div>`;
  }

  /** 余额 / 积分明细（实现见 usage-panel.js；两套形状的渲染与判据在那里） */
  const usagePanelHtml = (account, entry) =>
    window.wbUsagePanel.usagePanelHtml(account, entry);

  function checkinPanelHtml(account, entry) {
    const close = panelClose('checkin');
    if (!providerFeatures(providerOf(account)).checkin) {
      return `<div class="row-panel">该提供商没有签到活动，此账号不参与签到。${close}</div>`;
    }
    if (!supportsCheckin(account)) {
      return `<div class="row-panel">国际版暂无签到活动，该账号不参与签到。${close}</div>`;
    }
    // 禁用账号不再拦在这里：签到与转发是两件事，禁用只表示「别用它转发」。
    // 面板照常走到下面的「已签到 / 签到中 / 结果」分支，与后端一致。
    if (entry === undefined) {
      // 今天已签过时按钮是「已签到」（不可点），所以这句提示不能再叫用户去点它 ——
      // 那会把人引到一个点不动的按钮上。已签到的事实来自后端落盘的时间，
      // 不依赖本界面有没有查询过（自动签到那条路径界面从未参与）。
      if (checkedInToday(account)) {
        const at = Number(account?.checkinAt) || 0;
        const clock = at > 0 ? new Date(at).toTimeString().slice(0, 5) : '';
        return `<div class="row-panel"><span class="badge ok">✅ 今天已签到</span>`
          + `${clock ? `<span>${esc(clock)}</span>` : ''}`
          + `<span>签到按自然日重置，明天 0 点后可再签</span>${close}</div>`;
      }
      return `<div class="row-panel">签到状态未查询，请点击该行的「签到」按钮。${close}</div>`;
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
      // 积分字段两家口径不同：WorkBuddy 在 `data.points`（计费接口的嵌套结构），
      // 小浣熊与 AutoClaw 在顶层 `rewardPoints`（它们的 claim 是自造的扁平形状，
      // 没有 data 这一层）。两个都认，缺一不可 —— 只认前者会让后两家的
      // 「+N 积分」凭空消失，用户看不到签到到底领到了什么。
      const points = Number.isFinite(d.points) ? d.points : entry.rewardPoints;
      if (Number.isFinite(points) && points) parts.push(`<span>本次 +${esc(points)} 积分</span>`);
      if (Number.isFinite(d.continuousDays)) parts.push(`<span>连续 ${esc(d.continuousDays)} 天</span>`);
      if (Number.isFinite(d.totalDays)) parts.push(`<span>累计 ${esc(d.totalDays)} 天</span>`);
      return `<div class="row-panel">${parts.join('')}${close}</div>`;
    }
    // 未领取：`code` 只有 WorkBuddy 的 claim 带（上游业务码）；小浣熊与 AutoClaw
    // 的 claim 不带它，此时只显示 msg，不要露出一个 `code=?`
    const code = entry.code === undefined ? '' : `<span>code=${esc(entry.code)}</span>`;
    return `<div class="row-panel"><span class="badge warn">${esc(entry.msg || '未领取')}</span>`
      + `${code}${close}</div>`;
  }

  // ─── 卡片 ──────────────────────────────────

  /**
   * ⋯ 菜单项（点击时才插入 DOM，这里只生成 HTML）。
   *
   * 菜单按「对转发的影响面」从大到小排：启用/禁用会改变这个账号是否参与转发，
   * 是最重的一项，故放在最前；「设为首选」只改队列顺序（不改启用状态），
   * 排在它之后；「并发上限」是账号属性（标签里带当前值，0 = 不限），点击弹
   * 小对话框（见 accounts-view.js）；删除账号同样是最重的改动，排在最后
   * 并由 <hr> 隔开。首尾两项都标 danger：它们会立刻改变转发可用性。
   *
   * ── 「设为首选」本次从行上搬进来 ──────────────────────────────
   * 它原先在操作列里占一颗 62px 的按钮（四颗里最宽的一颗），而它回答的
   * 「这个账号排第几」隔壁的优先级列已经写着。搬进来之后行上只剩四颗按钮，
   * 操作列得以从 250px 收到 200px，省下的宽度归账号列（见 accounts-table.js
   * 的 actionsCell）。可达性不变 —— 它现在是菜单的第二项，紧跟在启用/禁用之后。
   *
   * `ctx.atFront`（账号是否已在全局队列第一位）由调用方给：菜单项要按它
   * **置灰**。判据必须来自视图侧的位置表（accounts-groups 的 positionMap，
   * 与行上的序号、↑ 按钮的边界同源），本文件是纯逻辑、手里没有账号全集。
   * 缺省 false（= 可点）是刻意的降级方向：后端对「已经在第一位」返回
   * `changed: false`，调用方会把那句「账号已在全局队列第一位」透出来 ——
   * 漏传只是少一次置灰，不会变成一个点了没反应的按钮。
   *
   * 「桌面端实时登录态」账号**也可以删除**了（曾经置灰不可删，现已放开）：
   * 它是「导入桌面端登录态」建出来的一条账号记录，删除只作用于这条记录 ——
   * 客户端的登录态文件我们从不去写、也不会删，因此安全且可逆（想再用，
   * 重新导入一次就加回来）。置灰挡掉的其实是用户「我不要这条」的正当选择：
   * 禁用后记录仍占着列表与优先级序号，等于把人锁死。菜单项保留一句 title
   * 说明「删的是这条记录、不是客户端里的登录态」，免得误以为删掉就是退登。
   *
   * 「刷新 Token」仍按 `hasRefreshToken` 决定（数据驱动，与改造前一致）：
   * 桌面端账号的记录里不落 refreshToken（凭证在客户端文件里、转发时临期会自动刷新），
   * 所以这一项对它不出现 —— 手动刷新走的是「不过期就原样返回」的路径，
   * 点了只会得到一句「已刷新」而实际什么都没做，不如不给这个入口。
   */
  function moreMenuHtml(account, ctx = {}) {
    const atFront = ctx.atFront === true;
    const items = [];
    items.push(isEnabled(account)
      ? { action: 'disable', label: '禁用', danger: true }
      : { action: 'enable', label: '启用', danger: true });
    // 队首时置灰：与它还在行上时的处理逐字一致（那时是 disabled 属性）
    items.push({
      action: 'switch',
      label: '设为首选',
      disabled: atFront,
      title: atFront ? '已在全局队列第一位' : '仅将优先级调整到全局第一位，不改变启用状态',
    });
    // 并发上限：**所有家通用**的账号属性（内置 + custom 都渲染这一项），
    // 标签里带当前值 —— 0（含后端缺键，公开形态恒输出 0）显示「不限」，
    // >0 显示具体数字。点击弹小对话框（处理在 accounts-view.js）。
    // 判定口径与后端选路一致：`Number(x) || 0`，非数字脏值一律按不限算。
    const maxConcurrent = Number(account.maxConcurrent) || 0;
    items.push({
      action: 'maxConcurrent',
      label: maxConcurrent > 0 ? `并发上限：${maxConcurrent}` : '并发上限：不限',
      title: '设置该账号同时最多处理的请求数（0 = 不限制）',
    });
    if (account.hasRefreshToken) {
      items.push({ action: 'refresh', label: '刷新 Token' });
    }
    items.push(isDesktopAccount(account)
      ? { action: 'remove', label: '删除账号', danger: true, title: '删除这条账号记录（不会影响客户端自己的登录态；之后可再点「导入桌面端登录态」加回来）' }
      : { action: 'remove', label: '删除账号', danger: true });
    // 菜单内不重复行上已有的操作（签到 / 余额 / 设置都留在行上）；
    // 「设为首选」是反向的那一条 —— 它从行上搬进了菜单，见上面的说明。
    return items.map((item, index) => {
      const hr = item.danger && index > 0 ? '<hr>' : '';
      const attrs = [
        `data-menu-action="${item.action}"`,
        `data-id="${esc(account.id)}"`,
        item.danger ? 'class="danger"' : '',
        item.disabled ? 'disabled' : '',
        item.title ? `title="${esc(item.title)}"` : '',
      ].filter(Boolean).join(' ');
      return `${hr}<button ${attrs}>${esc(item.label)}</button>`;
    }).join('');
  }

  /** 无有效恢复时间时的退化文案：它本身就是完整一句，调用方据此不再拼「，恢复时间：」 */
  const RESET_UNKNOWN = '已限流';

  /** 某时刻所在自然日的零点（本地时区），用于按「日历天」计算今天 / 明天 */
  const startOfDay = value => new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();

  /**
   * 限流恢复时间文案：今天 HH:mm / 明天 HH:mm / M月d日 HH:mm。
   *
   * 为什么带「今天 / 明天」而不是相对毫秒数或完整时间戳：限流是自动解除的，
   * 用户扫过卡片时最关心「到点了没、还要等多久」——「明天 01:04」比
   * 「09-19 01:04」少一步换算，也不会像「6 小时后」那样一过夜就说不清是哪天。
   *
   * 无有效时间戳（缺失 / 非法 / 已过，后者说明数据异常）时返回 RESET_UNKNOWN，
   * 由调用方退化成只输出这一句。
   */
  function formatResetText(resetAt) {
    const time = Number(resetAt);
    if (!Number.isFinite(time) || time <= 0 || time <= Date.now()) return RESET_UNKNOWN;
    const date = new Date(time);
    const clock = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false });
    // 按自然日求差而不是按 24 小时：今晚 23:50 到明天 00:10 只差 20 分钟，
    // 但用户嘴里它就是「明天」，按毫秒差算会显示成「今天」，与直觉相反。
    const days = Math.round((startOfDay(date) - startOfDay(new Date())) / 86400e3);
    if (days === 0) return `今天 ${clock}`;
    if (days === 1) return `明天 ${clock}`;
    return `${date.getMonth() + 1}月${date.getDate()}日 ${clock}`;
  }

  window.wbAccountsModel = {
    // provider 维度
    DEFAULT_PROVIDER_ID,
    RACCOON_PROVIDER_ID,
    providerOf,
    providerFeatures,
    providerSummaries,
    identifierOf,
    tokenExpiryOf,
    supportsUsage,
    isDesktopAccount,
    supportsChat,
    // 基础判定
    byPriorityOrder,
    typeLabel,
    isEnabled,
    isRateLimited,
    accountEdition,
    supportsCheckin,
    checkedInToday,
    checkinableAccounts,
    // 筛选与队列
    matchProvider,
    matchEnabled,
    matchLimit,
    visibleAccounts,
    filterCounts,
    positionMap,
    activeLimits,
    // 标签与面板
    statusTag,
    accountTags,
    editionSuffix,
    moreMenuHtml,
    limitPanelHtml,
    formatResetText,
    usagePanelHtml,
    checkinPanelHtml,
  };
})();
