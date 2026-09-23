/* Agent2API · 账号页的筛选维度（状态 + 筛选器 UI） */
/* global wbApp, wbAccountsModel */

/**
 * 账号页筛选维度的**状态与它自己的那部分 DOM**，从 accounts-view.js 按职责拆出。
 *
 * ── 为什么单独成文件 ────────────────────────────────────────────
 * accounts-view.js 行数吃紧。筛选项里有几块天然自成一体的东西：**动态注入的提供商
 * 下拉与计数摘要**、**分段控件的计数徽标与联动禁用**。它们与「列表怎么画」没有
 * 关系，改筛选口径不该碰到表格渲染，于是整段搬到这里。
 *
 * ── 状态归本文件，判定仍归 accounts-model.js ────────────────────
 * 本文件只持有 `state`（用户选了什么）并把它交给 accounts-model 的
 * `visibleAccounts` / `filterCounts` 去算 —— 口径只有那一份实现，
 * 「可见列表」与「分段计数」不会各算各的。视图侧读 `wbAccountsFilters.state`。
 *
 * ── 三个维度各自独立 ──────────────────────────────────────────
 *   provider = all|providerId    所属提供商（选项来自 providers 摘要）
 *   enabled  = all|enabled|disabled  启用状态
 *   limit    = all|normal|limited    限流状态（该账号任一模型限流中即算「有限流」）
 *
 * 曾经的「版本」维度已下线：国内 / 国际只作为账号属性出现在行上的徽章里
 * （accounts-model 的 editionCell），不再单独给一个筛选入口。
 * 曾经的「模型」维度已下线：它只是 WorkBuddy 单上游时代的遗留 —— 限额按模型记，
 * 每个账号「具体哪个模型限流」现在由行上的「限流」列直接展开（见 accounts-table.js），
 * 不必再让整张表跟着一个模型下拉切换口径。
 */
(() => {
  const $ = id => document.getElementById(id);
  const { esc } = wbApp;
  const { providerSummaries, filterCounts } = wbAccountsModel;

  const state = { provider: 'all', enabled: 'all', limit: 'all' };

  const snapshot = () => wbApp.getState()?.accounts;
  /** providers 摘要（后端注册表顺序；缺失时由账号列表派生，见 accounts-model） */
  const summaries = () => providerSummaries(snapshot());

  // ─── 动态注入：提供商筛选器与计数摘要 ───────
  //
  // 用 select 而不是像状态/限额那样的分段按钮：提供商数量是**动态**的（后端注册表
  // 加一家就多一项），分段按钮会随家数增长把工具条挤成一团；选项里带账号数
  // （`WorkBuddy（14）`），于是「哪家有账号、各有多少」不用切页就能看到。

  const PROVIDER_FILTER_ID = 'account-provider-filter';
  const PROVIDER_SUMMARY_ID = 'accounts-provider-summary';

  /** 把提供商筛选器插进工具条最前（「状态」组之前） */
  function mountProviderFilter() {
    if ($(PROVIDER_FILTER_ID)) return;
    const anchor = $('account-enabled-filter')?.closest('.group');
    const toolbar = anchor?.closest('.toolbar');
    if (!toolbar || !anchor) return;
    const group = document.createElement('div');
    group.className = 'group';
    group.dataset.providerGroup = '1';
    group.innerHTML = `<span class="label">提供商</span>`
      + `<select id="${PROVIDER_FILTER_ID}" class="model-select" aria-label="按提供商筛选账号"></select>`;
    // 落点：工具条是「提供商 | 状态 | 限额 | 操作」，提供商是第一个维度，
    // 于是插在「状态」组之前、并紧跟一条分隔线。
    toolbar.insertBefore(group, anchor);
    const divider = document.createElement('div');
    divider.className = 'divider';
    divider.dataset.providerDivider = '1';
    toolbar.insertBefore(divider, anchor);
  }

  /** 把「按提供商计数」摘要插进批量栏（「共 N 个」之后）：回答「分别是几家的几个」 */
  function mountProviderSummary() {
    const count = $('accounts-count');
    if (!count || $(PROVIDER_SUMMARY_ID)) return;
    const span = document.createElement('span');
    span.id = PROVIDER_SUMMARY_ID;
    span.className = 'provider-summary';
    count.insertAdjacentElement('afterend', span);
  }

  /** 两处追加式注入必须在 bind 之前完成（提供商下拉的 change 要挂得上） */
  function mount() {
    mountProviderFilter();
    mountProviderSummary();
  }

  // ─── 每次重绘前归一化 ───────────────────────

  /** 刷新提供商维度相关的界面：筛选器选项（含各家账号数）与计数摘要 */
  function syncProviderUi(all) {
    const list = summaries();
    // 摘要里已不存在的 provider（账号被删光且后端注册表也移除了）复位成「全部」
    if (state.provider !== 'all' && !list.some(item => item.id === state.provider)) {
      state.provider = 'all';
    }
    const select = $(PROVIDER_FILTER_ID);
    if (select) {
      const options = [{ id: 'all', label: `全部（${all.length}）` }]
        .concat(list.map(item => ({ id: item.id, label: `${item.label}（${item.count}）` })));
      const signature = options.map(item => `${item.id}:${item.label}`).join('|');
      if (select.dataset.signature !== signature) {
        select.dataset.signature = signature;
        select.innerHTML = options
          .map(item => `<option value="${esc(item.id)}">${esc(item.label)}</option>`).join('');
      }
      if (select.value !== state.provider) select.value = state.provider;
    }
    const summary = $(PROVIDER_SUMMARY_ID);
    if (summary) summary.textContent = all.length
      ? list.map(item => `${item.label} ${item.count}`).join(' · ')
      : '';
  }

  /**
   * 限流维度与启用状态联动：状态筛成「禁用」时正常/有限流都不存在，于是把限流复位为
   * 「全部」并禁用该组分段。
   */
  function syncLimitAvailability() {
    const disabledOnly = state.enabled === 'disabled';
    if (disabledOnly && state.limit !== 'all') state.limit = 'all';
    const group = $('account-limit-filter');
    if (!group) return;
    group.querySelectorAll('.seg-item').forEach(item => {
      item.disabled = disabledOnly && item.dataset.limit !== 'all';
      item.classList.toggle('active', item.dataset.limit === state.limit);
    });
  }

  /**
   * 更新各筛选分段的计数徽标：key 取自 HTML 上的 data-count（各维度的「全部」
   * 是 enabledAll / limitAll，代表「另外几个维度已选条件下的合计」）。
   * 口径来自 accounts-model 的 filterCounts —— 与「可见列表」同一份实现。
   */
  function syncCounts(all) {
    const counts = filterCounts(all, state, summaries());
    document.querySelectorAll('.seg-item').forEach(item => {
      const badge = item.querySelector('.seg-count');
      const key = badge?.dataset.count;
      if (!badge || !key) return;
      const value = counts[key] ?? 0;
      badge.textContent = String(value);
      item.classList.toggle('zero', value === 0);
    });
  }

  /** 重绘前的一次性归一化：三段顺序不能换（限流的可用性依赖状态已归一） */
  function syncAll(all) {
    syncLimitAvailability();
    syncProviderUi(all);
    syncCounts(all);
  }

  // ─── 事件绑定 ───────────────────────────────

  /**
   * 筛选控件的事件绑定。`onChange` 由调用方（accounts-view.js）传入 ——
   * 筛选条件一变就要重绘列表，而重绘入口在那边；本文件不反向引用视图。
   */
  function bind(onChange) {
    // 启用状态 / 限流：两个分段按钮维度，可任意组合
    const bindSeg = (containerId, attr) => {
      $(containerId)?.addEventListener('click', event => {
        const item = event.target.closest(`.seg-item[data-${attr}]`);
        if (!item) return;
        state[attr] = item.dataset[attr];
        document.querySelectorAll(`#${containerId} .seg-item`).forEach(node => {
          node.classList.toggle('active', node === item);
        });
        onChange();
      });
    };
    bindSeg('account-enabled-filter', 'enabled');
    bindSeg('account-limit-filter', 'limit');

    // 提供商下拉（动态注入的节点，所以在这里显式绑定；select.js 负责外观增强）
    $(PROVIDER_FILTER_ID)?.addEventListener('change', event => {
      state.provider = event.target.value || 'all';
      onChange();
    });
  }

  window.wbAccountsFilters = { state, mount, syncAll, bind, summaries };
})();
