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
 * 曾经的「版本」维度已下线：国内 / 国际只作为账号属性出现在提供商徽章的文字里
 * （accounts-model 的 editionSuffix，拼成「WorkBuddy 国际版」一枚徽章），
 * 不再单独给一个筛选入口。
 * 曾经的「模型」维度已下线：它只是 WorkBuddy 单上游时代的遗留 —— 限额按模型记，
 * 每个账号「具体哪个模型限流」现在由行上的「限流」列直接展开（见 accounts-table.js），
 * 不必再让整张表跟着一个模型下拉切换口径。
 */
(() => {
  const $ = id => document.getElementById(id);
  const { esc } = wbApp;
  const { providerSummaries, filterCounts } = wbAccountsModel;

  /**
   * 三个维度的当前值。初值来自上次会话的存盘（wbFilterMemory）——
   * 「筛选条件记住上次的选择」，重启后照常生效；存坏的键由 load 落回「全部」。
   */
  const FILTERS_KEY = 'workbuddy-desktop-accounts-filters';
  const state = window.wbFilterMemory
    ? window.wbFilterMemory.load(FILTERS_KEY, { provider: 'all', enabled: 'all', limit: 'all' })
    : { provider: 'all', enabled: 'all', limit: 'all' };
  /** 当前条件整体落盘（load/save 都按整份对象走，不必逐维度记改动） */
  const persist = () => window.wbFilterMemory?.save(FILTERS_KEY, state);

  const snapshot = () => wbApp.getState()?.accounts;
  /** providers 摘要（后端注册表顺序；缺失时由账号列表派生，见 accounts-model） */
  const summaries = () => providerSummaries(snapshot());

  /** 两个分段维度的岛句柄（见 mountSegs）；挂载失败时留 null，后面走可选链 */
  let enabledIsland = null;
  let limitIsland = null;
  /** 筛选变化后的重绘入口。岛在 bind() 之前就挂载了，所以先存一层转发 */
  let notifyChange = () => {};

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

  /** 两处追加式注入与岛挂载都必须在 bind 之前完成（前者的 change、后者的 onChange 要接得上） */
  function mount() {
    mountProviderFilter();
    mountProviderSummary();
    mountSegs();
  }

  /**
   * 两个维度的选项。计数取自 filterCounts —— 与「可见列表」同一份实现。
   * 限流组的「正常 / 已限流」在启用状态筛成「禁用」时不存在，置灰（岛渲染成
   * disabled）；counts 传 null（首屏数据还没到）时一律画 0，与改造前 HTML 预置的 0 一致。
   */
  function segOptions(counts, limitOff) {
    const n = key => counts?.[key] ?? 0;
    return {
      enabled: [
        { value: 'all', label: '全部', count: n('enabledAll') },
        { value: 'enabled', label: '启用', count: n('enabled') },
        { value: 'disabled', label: '禁用', count: n('disabled') },
      ],
      limit: [
        { value: 'all', label: '全部', count: n('limitAll') },
        { value: 'normal', label: '正常', count: n('normal'), disabled: limitOff },
        { value: 'limited', label: '已限流', count: n('limited'), disabled: limitOff },
      ],
    };
  }

  /**
   * 挂两个分段岛（ui/islands/ui.js）。取值与计数都以本文件为准 ——
   * 岛完全受控，这里只负责灌值与回灌。首屏计数先按 0 画，随后由 syncAll 推上真实值。
   */
  function mountSegs() {
    if (!window.wbSegmented) return;
    const initial = segOptions(null, false);
    const enabledHost = $('account-enabled-filter');
    if (enabledHost) {
      enabledIsland = window.wbSegmented.mount(enabledHost, {
        options: initial.enabled,
        value: state.enabled,
        ariaLabel: '启用状态',
        onChange: value => pickSeg('enabled', value),
      });
    }
    const limitHost = $('account-limit-filter');
    if (limitHost) {
      limitIsland = window.wbSegmented.mount(limitHost, {
        options: initial.limit,
        value: state.limit,
        ariaLabel: '限额状态',
        onChange: value => pickSeg('limit', value),
      });
    }
  }

  /** 岛上切了档位：落盘 + 重绘（重绘入口由 bind 传入） */
  function pickSeg(key, value) {
    state[key] = value;
    persist();
    notifyChange();
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
    // 摘要只列**有账号**的家：这一段回答的是「账号分别落在谁家」，
    // 而「Cline Pass 0」这类只占宽度、不提供信息（尤其注册表里家数一多，
    // 半行都被这些 0 挤掉）。完整清单仍在下拉里，含 0 的家，筛选口径不变。
    const summary = $(PROVIDER_SUMMARY_ID);
    if (summary) {
      summary.textContent = list
        .filter(item => item.count > 0)
        .map(item => `${item.label} ${item.count}`)
        .join(' · ');
    }
  }

  /**
   * 限流维度与启用状态联动：状态筛成「禁用」时正常/有限流都不存在，于是把限流复位为
   * 「全部」。置灰不在这里做 —— 它由 segOptions 的 disabled 表达，随选项一起推给岛。
   */
  function syncLimitAvailability() {
    if (state.enabled === 'disabled' && state.limit !== 'all') state.limit = 'all';
  }

  /**
   * 把最新的计数与禁用态推给两个岛，顺带把归一化过的 state 回灌一次
   * （syncLimitAvailability 可能改过 state.limit，不回灌控件会停在旧档位上）。
   * 计数口径来自 accounts-model 的 filterCounts —— 与「可见列表」同一份实现。
   */
  function syncSegOptions(all) {
    const options = segOptions(filterCounts(all, state, summaries()), state.enabled === 'disabled');
    enabledIsland?.setOptions(options.enabled);
    limitIsland?.setOptions(options.limit);
    enabledIsland?.setValue(state.enabled);
    limitIsland?.setValue(state.limit);
  }

  /** 重绘前的一次性归一化：三段顺序不能换（限流的可用性依赖状态已归一） */
  function syncAll(all) {
    syncLimitAvailability();
    syncProviderUi(all);
    syncSegOptions(all);
  }

  // ─── 事件绑定 ───────────────────────────────

  /**
   * 筛选控件的事件绑定。`onChange` 由调用方（accounts-view.js）传入 ——
   * 筛选条件一变就要重绘列表，而重绘入口在那边；本文件不反向引用视图。
   */
  function bind(onChange) {
    // 两个分段维度的交互在岛上（见 mountSegs），这里只把重绘入口转交给它
    notifyChange = onChange;

    // 提供商下拉（动态注入的节点，所以在这里显式绑定；select.js 负责外观增强）
    $(PROVIDER_FILTER_ID)?.addEventListener('change', event => {
      state.provider = event.target.value || 'all';
      persist();
      onChange();
    });
  }

  window.wbAccountsFilters = { state, mount, syncAll, bind, summaries };
})();
