/* WorkBuddy 本地代理 · 账号列表视图与行内操作（积分 / 签到 / 徽章） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的账号视图模块：账号列表的渲染（含优先级 / 启用 / 代理徽章）
 * 与行内批量操作（积分查询、签到）集中在这里，主脚本只负责账号级的切换/删除/设置。
 *
 * 依赖 window.wbApp：esc / toast / formatTime / formatShortTime / getState，
 * 通过 window.wbAccountsView 暴露给 app.js：
 *   render()                     重绘账号列表（筛选、分组、徽章、行内面板）
 *   renderNavCount()             导航上的账号数徽标
 *   refreshCaches(validIds)      清理已删除账号的本地缓存
 *   queryAllUsage()/queryFor(id) 积分
 *   checkinAll()/checkinFor(id)  签到
 *   checkinableAccounts(list)    可签到账号（启用 + 国内版）
 *   bindEvents()                 绑定筛选器与账号行按钮事件
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast, formatTime } = wbApp;
  // 领域判定与标签渲染抽到 accounts-model.js（纯逻辑，无状态），这里直接引用
  const {
    byPriorityOrder,
    isEnabled,
    isRateLimited,
    usableForModel,
    accountEdition,
    supportsCheckin,
    checkinableAccounts,
    accountTags,
    metaLine,
    editionCell,
    usagePanelHtml: usagePanelHtmlOf,
    checkinPanelHtml: checkinPanelHtmlOf,
  } = wbAccountsModel;

  const usageMap = new Map();   // accountId -> usage | error string | null(loading)
  const checkinMap = new Map(); // accountId -> claimResult | error string | null(loading)
  /**
   * 四个独立筛选维度，可任意组合：
   *   edition = all|cn|intl     账号版本
   *   enabled = all|enabled|disabled  启用状态
   *   limit   = all|normal|limited    限额状态（只对启用中的账号有意义）
   *   model   = ''|模型 id       只看该模型的可用账号（限额按模型记，所以限额维度随它变）
   */
  const accountFilter = { edition: 'all', enabled: 'all', limit: 'all', model: '' };
  /** 用户是否手动选过模型：没选过时跟随「最近一次请求的模型」 */
  let modelPicked = false;
  /** 批量选择的账号 id（只作用于当前勾选，不随筛选变化自动增减） */
  const selectedIds = new Set();
  /** 最近一次渲染出的可见账号 id（「全选」只作用于这批） */
  let lastVisibleIds = [];

  let usageBusy = false;
  let checkinBusy = false;

  const accounts = () => wbApp.getState()?.accounts?.accounts || [];
  const snapshot = () => wbApp.getState()?.accounts;

  // ─── 行内面板：展开状态 ─────────────────────
  //
  // 面板按需展开：只有用户点过「积分」/「签到」（或批量操作带出结果）之后才渲染。
  // 收起状态下面板 HTML 为空串，行高保持一行。
  // 面板内容本身由 accounts-model.js 渲染（纯展示，入参是缓存里的 entry）。

  /** 已展开明细的账号：accountId -> Set<'usage' | 'checkin'> */
  const openPanels = new Map();

  function panelOpen(accountId, kind) {
    return openPanels.get(accountId)?.has(kind) === true;
  }

  /** 展开 / 收起某账号的某块明细 */
  function setPanelOpen(accountId, kind, open) {
    const set = openPanels.get(accountId) || new Set();
    if (open) set.add(kind); else set.delete(kind);
    if (set.size) openPanels.set(accountId, set);
    else openPanels.delete(accountId);
  }

  function usagePanelHtml(account) {
    if (!panelOpen(account.id, 'usage')) return '';
    return usagePanelHtmlOf(account, usageMap.get(account.id));
  }

  function checkinPanelHtml(account) {
    if (!panelOpen(account.id, 'checkin')) return '';
    return checkinPanelHtmlOf(account, checkinMap.get(account.id));
  }

  // ─── 卡片渲染 ──────────────────────────────

  /**
   * 单张账号卡渲染（不含分组标题）。
   * position / total 为该账号在全局转发顺序里的位置，用于显示 P 值与
   * 决定 ↑/↓ 按钮的可用性（队首不能上移、队尾不能下移）。
   * routedId 为「当前视图下实际会被选中的账号 id」：选了模型时是该模型队列的
   * 第一个可用账号，未选模型时是全局首选。
   * model 为当前筛选的模型 id（空串 = 不限模型）。
   *
   * 卡片结构（自上而下，信息密度递减）：
   *   ① 主体：勾选 / 账号名 + 版本徽章 / 状态徽章（贴账号名右侧）
   *   ② 明细：额度类型 · 有效期 · 尾号 · 出口
   *   ③ 提示：仅异常时出现的一行说明（429 恢复时间 / 代理异常）
   *   ④ 底部：转发顺序在左，操作按钮在右
   * 频繁用到的操作留在卡内；刷新 Token 与删除收进「⋯」菜单（点击才生成内容）。
   */
  function accountCardHtml(account, routedId, position, total, model = '') {
    const isCurrent = account.id === routedId;
    const enabled = isEnabled(account);
    const picked = selectedIds.has(account.id);
    const title = [
      account.uid ? `uid ${account.uid}` : '',
      account.updatedAt ? `更新于 ${formatTime(account.updatedAt)}` : '',
    ].filter(Boolean).join('；');

    const actions = [
      `<span class="order-btns">`
        + `<button data-action="move-up" data-id="${esc(account.id)}" title="与上一个账号交换优先级"${position <= 1 ? ' disabled' : ''}>↑</button>`
        + `<button data-action="move-down" data-id="${esc(account.id)}" title="与下一个账号交换优先级"${position >= total ? ' disabled' : ''}>↓</button>`
        + `</span>`,
      `<button data-action="settings" data-id="${esc(account.id)}">设置</button>`,
      isCurrent
        ? '<button class="is-current" disabled title="已是转发顺序第一位，无需再置顶">已是首选</button>'
        : `<button class="wide" data-action="switch" data-id="${esc(account.id)}" title="把该账号移到转发顺序第一位，并启用它">设为首选</button>`,
      `<button data-action="usage" data-id="${esc(account.id)}" title="查询该账号剩余积分">积分</button>`,
      // 签到按钮保持可见（国际版无签到活动，故仅国内版显示）
      enabled && supportsCheckin(account)
        ? `<button data-action="checkin" data-id="${esc(account.id)}" title="为该账号签到">签到</button>`
        : '',
      `<button data-action="more" data-id="${esc(account.id)}" title="更多操作">⋯</button>`,
    ].filter(Boolean).join('');

    const cardCls = [
      'acct-card',
      enabled ? '' : 'disabled',
      picked ? 'selected' : '',
      isCurrent ? 'is-first' : '',
    ].filter(Boolean).join(' ');

    return `<article class="${cardCls}" data-id="${esc(account.id)}">
      <div class="card-head">
        <span class="pick" title="勾选后可批量操作">
          <input type="checkbox" data-pick="${esc(account.id)}"${picked ? ' checked' : ''}>
        </span>
        <div class="who">
          <div class="who-name"${title ? ` title="${esc(title)}"` : ''}>
            <span class="name">${esc(account.nickname || account.name || account.uid || '未命名账号')}</span>
            ${editionCell(account)}
          </div>
          <div class="who-meta">${metaLine(account, position)}</div>
        </div>
        <div class="card-state">${accountTags(account, routedId, model) || '<span class="badge tag plain">—</span>'}</div>
      </div>
      ${cardNoteHtml(account, model)}
      <div class="card-foot">
        <span class="rank${isCurrent ? ' is-first' : ''}" title="转发顺序：数值越小越先用${position ? `；当前第 ${position} 位` : ''}">${isCurrent ? '<span class="star">★</span>' : ''}<span class="p">P</span>${Number(account.priority ?? 100)}</span>
        <div class="card-actions">${actions}</div>
      </div>
      <div class="row-panels">${usagePanelHtml(account)}${checkinPanelHtml(account)}</div>
    </article>`;
  }

  /**
   * 异常提示条：状态徽章只说「是什么」（429 / 代理异常），这一行补「怎么办」。
   * 只在有异常时返回内容，正常卡片不占这行高度 —— 否则整片卡片都被拉高，
   * 却只为了重复一遍「一切正常」。
   */
  function cardNoteHtml(account, model = '') {
    const notes = [];
    if (isEnabled(account) && isRateLimited(account, model)) {
      notes.push('该模型已达上限，期间的请求会自动改用后面的账号');
    }
    if (account.proxy?.error) {
      notes.push(`代理不可用（${esc(account.proxy.error)}），转发时会回退直连`);
    }
    if (account.available === false) {
      notes.push(esc(account.reason || '账号当前不可用'));
    }
    if (!notes.length) return '';
    // 代理/不可用属于必须处理的故障，用红；纯限额是会自动绕过的，用黄
    const bad = Boolean(account.proxy?.error) || account.available === false;
    return `<div class="card-note ${bad ? 'bad' : 'warn'}"><span class="ico">⚠</span><span>${notes.join('；')}</span></div>`;
  }

  /**
   * 打开「⋯」菜单：点击时才把菜单项插入 DOM，收起时移除。
   * 之所以不是常驻隐藏（display:none），是因为布局探针会把「常驻但隐藏」的
   * 按钮算进行内按钮集合，导致测量结果与实际可见操作不一致。
   */
  let openMenu = null;   // 当前展开的菜单元素

  function closeMoreMenu() {
    if (openMenu) { openMenu.remove(); openMenu = null; }
  }

  function toggleMoreMenu(button, account) {
    const already = openMenu && openMenu.dataset.for === account.id;
    closeMoreMenu();
    if (already) return;

    const menu = document.createElement('div');
    menu.className = 'more-menu';
    menu.dataset.for = account.id;

    const items = [];
    if (account.hasRefreshToken) {
      items.push({ action: 'refresh', label: '刷新 Token' });
    }
    if (isEnabled(account) && supportsCheckin(account)) {
      items.push({ action: 'checkin', label: '签到' });
    }
    if (!account.desktop) {
      items.push({ action: 'remove', label: '删除账号', danger: true });
    }
    // 菜单内不再重复行内已有的操作（设置/积分/设为首选都留在行上）
    menu.innerHTML = items.map((item, index) => {
      const hr = item.danger && index > 0 ? '<hr>' : '';
      return `${hr}<button data-menu-action="${item.action}" data-id="${esc(account.id)}"`
        + `${item.danger ? ' class="danger"' : ''}>${esc(item.label)}</button>`;
    }).join('');

    // 挂在卡片底部操作区里，菜单就贴着按钮组弹出（.card-foot 是定位祖先），
    // 随列表滚动一起移动，不会浮到别的卡片上
    const host = button.closest('.card-foot') || button.closest('.acct-card');
    host.appendChild(menu);
    openMenu = menu;
  }

  /** 转发顺序下的位置表：accountId → 第几位（1 起） */
  function positionMap() {
    const order = accounts().slice().sort(byPriorityOrder);
    const map = new Map();
    order.forEach((account, index) => map.set(account.id, index + 1));
    return { map, total: order.length };
  }

  /**
   * 更新各筛选分段的计数徽标。key 取自 HTML 上的 data-count：
   * 三个维度的「全部」分别是 editionAll / enabledAll / limitAll，
   * 各自代表「另外两个维度已选条件下的合计」，所以必须用不同的 key。
   */
  function updateSegCounts(counts) {
    document.querySelectorAll('.seg-item').forEach(item => {
      const badge = item.querySelector('.seg-count');
      const key = badge?.dataset.count;
      if (!badge || !key) return;
      const value = counts[key] ?? 0;
      badge.textContent = String(value);
      item.classList.toggle('zero', value === 0);
    });
  }

  function render() {
    const list = $('account-list');
    if (!list) return;
    // 先归一化筛选状态：状态选「禁用」时限额维度无意义，会被复位到「全部」
    syncLimitAvailability();
    syncModelFilter();
    const snap = snapshot();
    const all = Array.isArray(snap?.accounts) ? snap.accounts : [];
    // 当前筛选的模型：为空表示「不限模型」（限额取任一模型的记录）
    const model = accountFilter.model;
    $('accounts-count').textContent = String(all.length);
    $('btn-query-usage').disabled = !all.length;
    $('btn-checkin-all').disabled = !checkinableAccounts(all).length;

    // 四个维度各自独立：版本 / 启用状态 / 限额 / 模型。
    // 判定函数抽出来给「可见列表」和「分段计数」共用，保证徽标数字与点进去看到的结果永远一致。
    const matchEdition = account => accountFilter.edition === 'all'
      || accountEdition(account) === accountFilter.edition;
    const matchEnabled = account => {
      if (accountFilter.enabled === 'all') return true;
      return accountFilter.enabled === 'enabled' ? isEnabled(account) : !isEnabled(account);
    };
    // 已禁用账号既不算「正常」也不算「已限流」：限流状态只对参与转发的账号有意义
    // 限额判定按所选模型走 —— 这正是「模型筛选」的意义
    const matchLimit = account => {
      if (accountFilter.limit === 'all') return true;
      if (!isEnabled(account)) return false;
      return accountFilter.limit === 'limited' ? isRateLimited(account, model) : !isRateLimited(account, model);
    };

    const visible = all.filter(account => matchEdition(account) && matchEnabled(account) && matchLimit(account));
    lastVisibleIds = visible.map(a => a.id);

    // 计数：某分段显示的数字 = 「另外几个维度保持当前选择、本维度取该值」的账号数
    const forEdition = all.filter(account => matchEnabled(account) && matchLimit(account));
    const forEnabled = all.filter(account => matchEdition(account) && matchLimit(account));
    const forLimit = all.filter(account => matchEdition(account) && matchEnabled(account));
    updateSegCounts({
      editionAll: forEdition.length,
      cn: forEdition.filter(a => accountEdition(a) === 'cn').length,
      intl: forEdition.filter(a => accountEdition(a) === 'intl').length,
      enabledAll: forEnabled.length,
      enabled: forEnabled.filter(isEnabled).length,
      disabled: forEnabled.filter(a => !isEnabled(a)).length,
      limitAll: forLimit.length,
      normal: forLimit.filter(a => isEnabled(a) && !isRateLimited(a, model)).length,
      limited: forLimit.filter(a => isEnabled(a) && isRateLimited(a, model)).length,
    });

    renderBatchBar(all, visible.map(a => a.id));

    if (!all.length) {
      list.innerHTML = '<div class="empty">暂无账号，请点击右上角「登录 / 添加账号」</div>';
      return;
    }
    if (!visible.length) {
      list.innerHTML = '<div class="empty">当前筛选条件下没有账号</div>';
      return;
    }

    // 版本选「全部」时按版本分组展示，选具体版本时只列该组
    const groups = accountFilter.edition === 'all'
      ? [
        { key: 'cn', title: '国内账号', items: visible.filter(a => accountEdition(a) === 'cn') },
        { key: 'intl', title: '国际账号', items: visible.filter(a => accountEdition(a) === 'intl') },
      ].filter(group => group.items.length)
      : [{ key: accountFilter.edition, title: '', items: visible }];

    // 组内按转发顺序排列，与 P 值和 ↑/↓ 的方向保持一致
    const { map: positions, total } = positionMap();
    // 选了模型时，该模型实际会用哪个账号 —— 这是「这个模型的账号队列」的答案：
    // 队列按优先级排，第一个「启用且对该模型未限流」的即为实际选中项。
    const routedId = model ? pickForModel(all, model)?.id ?? null : snap.currentAccountId;
    // 卡片网格：分组标题也是网格项，横跨整行
    const body = groups.map(group => {
      const title = group.title
        ? `<div class="acct-group-title">${esc(group.title)} · ${group.items.length}</div>`
        : '';
      const items = group.items.slice().sort(byPriorityOrder);
      return title + items
        .map(a => accountCardHtml(a, routedId, positions.get(a.id) ?? 0, total, model))
        .join('');
    }).join('');
    list.className = 'acct-scroll';
    list.innerHTML = `<div class="acct-grid">${body}</div>`;
  }

  /**
   * 某个模型的选路结果：优先级升序里第一个「启用且对该模型未限流」的账号。
   * 与后端 pickAccountByPriority 的判定保持一致（不含 429 重试的排除列表）。
   */
  function pickForModel(list, model) {
    const candidates = (list || [])
      .filter(account => usableForModel(account, model))
      .sort(byPriorityOrder);
    return candidates[0] || null;
  }

  /**
   * 批量操作栏：常驻显示，避免「必须先勾选才能全选」的死循环。
   * 未勾选时按钮禁用；全选只覆盖当前筛选结果，不会选中被筛掉的账号。
   * visibleIds 传的是 id 而不是账号对象，方便勾选时就地刷新操作栏而不重绘列表。
   */
  function renderBatchBar(all, visibleIds) {
    const bar = $('batch-bar');
    if (!bar) return;
    // 清掉已删除账号的勾选，避免残留幽灵选择
    const existing = new Set(all.map(a => a.id));
    for (const id of [...selectedIds]) if (!existing.has(id)) selectedIds.delete(id);
    const visibleSet = new Set(visibleIds);
    const count = selectedIds.size;
    // 被筛选隐藏但仍在勾选中的账号：操作会作用于它们，所以必须显式提示，不能静默生效
    const hiddenByFilter = [...selectedIds].filter(id => !visibleSet.has(id)).length;
    const active = count > 0;

    bar.classList.toggle('active', active);
    $('batch-count').textContent = String(count);

    const hint = $('batch-hidden-hint');
    if (hint) {
      hint.style.display = hiddenByFilter ? '' : 'none';
      hint.textContent = hiddenByFilter ? `另有 ${hiddenByFilter} 个已勾选账号被当前筛选隐藏，仍会参与操作` : '';
    }

    const box = $('batch-select-all');
    const allPicked = visibleIds.length > 0 && visibleIds.every(id => selectedIds.has(id));
    box.checked = allPicked;
    box.indeterminate = !allPicked && visibleIds.some(id => selectedIds.has(id));
    box.disabled = !visibleIds.length;
    const label = $('batch-select-label');
    if (label) label.textContent = visibleIds.length ? `全选当前筛选结果（${visibleIds.length} 个）` : '没有可全选的账号';

    for (const id of ['btn-batch-open', 'btn-batch-clear']) {
      const button = $(id);
      if (button) button.disabled = !active;
    }
  }

  /**
   * 把选中态同步到已有 DOM：勾选框、行高亮与操作栏。
   * 就地更新而不是重绘整个列表 —— 重绘会丢掉滚动位置，勾选时还会丢失输入焦点。
   */
  function syncSelectionUi() {
    document.querySelectorAll('#account-list input[data-pick]').forEach(box => {
      const picked = selectedIds.has(box.dataset.pick);
      box.checked = picked;
      box.closest('.acct-card')?.classList.toggle('selected', picked);
    });
    renderBatchBar(accounts(), lastVisibleIds);
  }

  /**
   * 模型筛选下拉：填充选项并保持选中项与 accountFilter.model 一致。
   *
   * 默认值优先取「最近一次实际请求的模型」——用户打开账号页最想知道的是
   * 「我正在用的这个模型会走哪些账号」，而不是一个抽象的全局顺序。
   * 用户手动选过之后（modelPicked）就不再自动跟随，避免抢走选择权。
   */
  function syncModelFilter() {
    const select = $('account-model-filter');
    if (!select) return;
    const state = wbApp.getState() || {};
    const models = Array.isArray(state.models) ? state.models : [];
    const fallback = state.lastRequestModel || state.defaultModel || '';
    // 未手动选过时持续跟随「最近一次请求的模型」：
    // 用户关心的是「我现在用的这个模型走哪些账号」，换模型后就该跟着变。
    // 手动选过之后（modelPicked）不再抢占他的选择。
    if (!modelPicked && fallback) accountFilter.model = fallback;

    // 选项只在模型目录变化时重建，避免每次渲染都重置滚动位置
    const signature = models.map(m => m.id).join('|');
    if (select.dataset.signature !== signature) {
      select.dataset.signature = signature;
      select.innerHTML = models.map(m => `<option value="${esc(m.id)}">${esc(m.name || m.id)}${m.isDefault ? '（默认）' : ''}</option>`).join('');
    }
    // 目录里没有当前选中值（如旧 id）时回落到第一项，避免出现空选择
    if (accountFilter.model && !models.some(m => m.id === accountFilter.model)) {
      accountFilter.model = models[0]?.id || '';
    }
    if (select.value !== accountFilter.model) select.value = accountFilter.model;
  }

  /**
   * 限额维度与启用状态联动：状态筛成「禁用」时，正常/已限流都不存在，
   * 于是把限额复位为「全部」并禁用该组分段，避免出现一个点了永远空的分段。
   */
  function syncLimitAvailability() {
    const disabledOnly = accountFilter.enabled === 'disabled';
    if (disabledOnly && accountFilter.limit !== 'all') accountFilter.limit = 'all';
    const group = $('account-limit-filter');
    if (!group) return;
    group.querySelectorAll('.seg-item').forEach(item => {
      item.disabled = disabledOnly && item.dataset.limit !== 'all';
      item.classList.toggle('active', item.dataset.limit === accountFilter.limit);
    });
  }

  /** 导航上的账号数徽标 */
  function renderNavCount() {
    const all = accounts();
    const badge = $('nav-count-accounts');
    if (!badge) return;
    badge.textContent = String(all.length);
    badge.classList.toggle('muted', all.length === 0);
  }

  /** 清掉已删除账号的本地缓存 */
  function refreshCaches(validIds) {
    for (const id of usageMap.keys()) if (!validIds.has(id)) usageMap.delete(id);
    for (const id of checkinMap.keys()) if (!validIds.has(id)) checkinMap.delete(id);
    for (const id of openPanels.keys()) if (!validIds.has(id)) openPanels.delete(id);
  }

  /**
   * 把一批余额结果写进列表缓存（启动自动维护与批量查询共用）。
   * 返回写入条数，调用方据此决定要不要重绘。
   */
  function applyBalances(balances) {
    const rows = Array.isArray(balances?.results) ? balances.results : [];
    let applied = 0;
    for (const row of rows) {
      if (!row?.id) continue;
      usageMap.set(row.id, row.usage ? row.usage : (row.error ? String(row.error) : '余额响应为空'));
      applied++;
    }
    return applied;
  }

  // ─── 积分 ──────────────────────────────────

  async function queryUsageFor(id) {
    // 后端批量接口：给 id 也走同一接口（返回全部，这里按需取用）
    const data = await api.getAllBalances();
    const rows = Array.isArray(data?.results) ? data.results : [];
    const returned = new Set();
    for (const row of rows) {
      if (!row?.id) continue;
      if (id && row.id !== id) continue;
      returned.add(row.id);
      usageMap.set(row.id, row.usage ? row.usage : (row.error ? String(row.error) : '余额响应为空'));
    }
    if (id) {
      if (!returned.has(id)) usageMap.set(id, '未返回余额数据');
    } else {
      for (const acc of accounts()) if (!returned.has(acc.id)) usageMap.set(acc.id, '未返回余额数据');
    }
    render();
    return data;
  }

  async function queryAllUsage() {
    if (usageBusy) return;
    const all = accounts();
    if (!all.length) { toast('暂无可查询的账号', 'err'); return; }
    usageBusy = true;
    const button = $('btn-query-usage');
    button.disabled = true;
    button.textContent = '查询中…';
    // 已禁用账号不参与批量查询（后端同样会跳过）
    const targets = all.filter(isEnabled);
    targets.forEach(a => usageMap.set(a.id, null));
    // 批量查询是「我要看所有人的余额」，所以顺手把明细展开 —— 否则结果无处可看
    targets.forEach(a => setPanelOpen(a.id, 'usage', true));
    render();
    try {
      const result = await queryUsageFor(null);
      const rows = result?.results || [];
      const ok = rows.filter(r => r.usage).length;
      toast(ok === rows.length
        ? `✅ 已更新 ${ok} 个账号的积分`
        : `已更新 ${ok}/${rows.length} 个账号，部分失败`, ok !== rows.length ? 'err' : 'ok');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      targets.forEach(a => usageMap.set(a.id, `查询失败：${message}`));
      render();
      toast(`积分查询失败：${message}`, 'err');
    } finally {
      usageBusy = false;
      button.disabled = false;
      button.textContent = '查询积分';
    }
  }

  // ─── 签到 ──────────────────────────────────

  async function checkinFor(id) {
    // id 缺省 = 全部账号串行签到；指定 id = 单账号签到
    const data = await api.checkinAllAccounts(id || null);
    const rows = Array.isArray(data?.results) ? data.results : [];
    if (id) {
      const row = rows.find(r => r.id === id) || rows[0];
      if (!row) checkinMap.set(id, '未返回签到结果');
      else if (row.error) checkinMap.set(id, String(row.error));
      else if (row.claim) checkinMap.set(id, row.claim);
      else checkinMap.set(id, '签到响应为空');
    } else {
      for (const row of rows) {
        if (!row?.id) continue;
        if (row.error) checkinMap.set(row.id, String(row.error));
        else if (row.claim) checkinMap.set(row.id, row.claim);
        else checkinMap.set(row.id, '签到响应为空');
      }
      const returned = new Set(rows.map(r => r.id));
      for (const acc of checkinableAccounts(accounts())) {
        if (!returned.has(acc.id)) checkinMap.set(acc.id, '未返回签到结果');
      }
    }
    render();
    return data;
  }

  async function checkinAll() {
    if (checkinBusy) return;
    const targets = checkinableAccounts(accounts());
    if (!targets.length) { toast('暂无可签到的账号（国际版无签到活动）', 'err'); return; }
    if (!confirm(`将对 ${targets.length} 个国内版账号串行签到，可能需要一点时间。继续？`)) return;
    checkinBusy = true;
    const button = $('btn-checkin-all');
    button.disabled = true;
    button.textContent = '签到中…';
    targets.forEach(a => checkinMap.set(a.id, null));
    // 同批量积分：结果要看得见，所以批量签到也把明细展开
    targets.forEach(a => setPanelOpen(a.id, 'checkin', true));
    render();
    try {
      const data = await checkinFor(null);
      const succeeded = data?.succeeded ?? 0;
      const total = data?.total ?? 0;
      const skipped = Number(data?.skipped) || 0;
      toast(`签到完成：${succeeded}/${total} 个账号成功领取`
        + (skipped ? `（跳过 ${skipped} 个已禁用/国际版账号）` : ''), succeeded < total ? 'err' : 'ok');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      targets.forEach(a => checkinMap.set(a.id, `签到失败：${message}`));
      render();
      toast(`签到失败：${message}`, 'err');
    } finally {
      checkinBusy = false;
      button.disabled = false;
      button.textContent = '全部签到';
    }
  }

// ─── 事件绑定 ──────────────────────────────

/**
 * 账号行按钮与两级筛选的绑定。
 * 事件绑定与脚本加载顺序绑定（本模块在 app.js 之后加载，wbApp 已就绪），
 * 因此在这里自持注册，而不依赖 app.js 回调过来。
 */
function bindEvents() {
  $('account-list').addEventListener('click', async event => {
    // 明细条上的「收起」按钮
    const closer = event.target.closest('button[data-panel-close]');
    if (closer) {
      const host = closer.closest('.acct-card');
      setPanelOpen(host?.dataset.id, closer.dataset.panelClose, false);
      render();
      return;
    }
    // ⋯ 菜单里的菜单项（刷新 Token / 签到 / 删除）
    const menuItem = event.target.closest('button[data-menu-action]');
    if (menuItem) {
      const { menuAction, id } = menuItem.dataset;
      closeMoreMenu();
      if (menuAction === 'checkin') {
        setPanelOpen(id, 'checkin', true);
        await runCheckin(id);
        return;
      }
      window.wbApp.runAccountAction?.(menuAction, id);
      return;
    }

    const button = event.target.closest('button[data-action]');
    if (!button) { closeMoreMenu(); return; }
    const { action, id } = button.dataset;

    if (action === 'more') {
      const account = accounts().find(a => a.id === id);
      if (account) toggleMoreMenu(button, account);
      return;
    }
    closeMoreMenu();

    if (action === 'move-up' || action === 'move-down') {
      button.disabled = true;
      try {
        await api.moveAccount(id, action === 'move-down' ? 'down' : 'up');
        await wbApp.refresh?.();
      } catch (error) {
        toast(`调整顺序失败：${error.message}`, 'err');
        button.disabled = false;
      }
      return;
    }
    if (action === 'usage') {
      // 点「积分」即展开明细；已展开时再点则收起（当成开关用）
      const wasOpen = panelOpen(id, 'usage');
      setPanelOpen(id, 'usage', !wasOpen);
      if (wasOpen) { render(); return; }
      usageMap.set(id, null);
      render();
      try {
        await queryUsageFor(id);
        const entry = usageMap.get(id);
        if (typeof entry === 'string') toast(`积分查询失败：${entry}`, 'err');
        else toast('✅ 已更新积分');
      } catch (error) {
        usageMap.set(id, error instanceof Error ? error.message : String(error));
        render();
        toast(`积分查询失败：${error.message}`, 'err');
      }
      return;
    }
    if (action === 'checkin') {
      setPanelOpen(id, 'checkin', true);
      await runCheckin(id);
      return;
    }
    // 切换 / 刷新 / 删除 / 设置：交给 app.js 的统一入口
    window.wbApp.runAccountAction?.(action, id);
  });

  /** 单个账号签到：展开明细 → 串行请求 → 就地刷新结果 */
  async function runCheckin(id) {
    checkinMap.set(id, null);
    render();
    try {
      await checkinFor(id);
      const entry = checkinMap.get(id);
      if (typeof entry === 'string') toast(`签到失败：${entry}`, 'err');
      else if (entry?.success) toast('✅ 该账号签到成功');
      else if (entry?.msg) toast(entry.msg, 'err');
    } catch (error) {
      checkinMap.set(id, error instanceof Error ? error.message : String(error));
      render();
      toast(`签到失败：${error.message}`, 'err');
    }
  }

  // 点击空白处收起 ⋯ 菜单（菜单是动态插入的，所以监听在 document 上）
  document.addEventListener('click', event => {
    if (!openMenu) return;
    if (event.target.closest('.more-menu') || event.target.closest('button[data-action="more"]')) return;
    closeMoreMenu();
  });

  // 三级筛选：版本 / 启用状态 / 限额状态，三者独立组合
  const bindFilter = (containerId, attr, key) => {
    $(containerId).addEventListener('click', event => {
      const item = event.target.closest(`.seg-item[data-${attr}]`);
      if (!item) return;
      accountFilter[key] = item.dataset[attr];
      document.querySelectorAll(`#${containerId} .seg-item`).forEach(node => {
        node.classList.toggle('active', node === item);
      });
      render();
    });
  };
  bindFilter('account-edition-filter', 'edition', 'edition');
  bindFilter('account-enabled-filter', 'enabled', 'enabled');
  bindFilter('account-limit-filter', 'limit', 'limit');

  // 模型下拉：切换后整个列表按该模型的账号队列重新判定
  // （限额维度跟着该模型走，首选徽章也标到该模型实际会选中的账号上）
  $('account-model-filter').addEventListener('change', event => {
    accountFilter.model = event.target.value;
    modelPicked = true;   // 用户手动选过，不再自动跟随最近请求的模型
    render();
  });

  // 行首复选框：勾选 / 取消批量选择（就地更新，不重绘列表，避免丢掉滚动位置）
  $('account-list').addEventListener('change', event => {
    const box = event.target.closest('input[data-pick]');
    if (!box) return;
    const id = box.dataset.pick;
    if (box.checked) selectedIds.add(id);
    else selectedIds.delete(id);
    syncSelectionUi();
  });

  // 全选 / 全不选：只作用于当前筛选结果
  $('batch-select-all').addEventListener('change', event => {
    if (event.target.checked) for (const id of lastVisibleIds) selectedIds.add(id);
    else for (const id of lastVisibleIds) selectedIds.delete(id);
    syncSelectionUi();
  });

  $('btn-batch-clear').addEventListener('click', () => {
    selectedIds.clear();
    syncSelectionUi();
  });
  // 单个「批量操作」按钮：打开弹窗，具体动作在弹窗里用单选切换（默认「启用」）
  $('btn-batch-open').addEventListener('click', () => openBatch());

  /** 把当前勾选与动作交给账号面板的批量弹窗；不传动作时默认选中「启用」 */
  function openBatch(action = 'enable') {
    if (!selectedIds.size) { toast('请先勾选要操作的账号', 'err'); return; }
    window.wbAccountPanel?.openBatch([...selectedIds], action);
  }

  $('btn-query-usage').addEventListener('click', queryAllUsage);
  $('btn-checkin-all').addEventListener('click', checkinAll);
}

bindEvents();

window.wbAccountsView = {
  render,
  renderNavCount,
  refreshCaches,
  applyBalances,
  queryUsageFor,
  queryAllUsage,
  checkinFor,
  checkinAll,
  checkinableAccounts,
  supportsCheckin,
  isEnabled,
  isRateLimited,
};
})();
