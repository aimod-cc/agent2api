/* Agent2API · 账号列表视图（全局一条队列的一张表 + 行内操作） */
/* global workbuddyDesktop, wbApp, wbAccountsModel, wbAccountsTable, wbAccountsColumns, wbUsageActions, wbAccountsFilters */

/**
 * 账号页的**有状态**那一半：表格重绘、批量选择、行内面板展开、⋯ 菜单定位、
 * 优先级行内编辑、事件委托。
 *
 * ── 全局一条队列（优先级不再按 provider 分段）────────────────────
 * 优先级在后端是全局唯一的：四家账号混排在同一条队列里，转发时按优先级从小到大
 * 逐个尝试，跳过禁用 / 不支持该模型 / 该模型限流中的账号（见 priority.rs 与
 * rotate.rs）。所以这张表：
 *   · 行序 = 优先级升序（不再按 provider 分块，也没有组首分隔线）；
 *   · 「设为首选」只调整全局队列顺序，不改变启用状态；
 *   · 不根据请求模型或选路结果标记账号；
 *   · ↑/↓ 与全局相邻账号交换（对方可能是另一家的账号）；
 *   · 「先用哪一家」由队列里排最前的支持该模型的账号决定，
 *     provider 只再是账号的一个属性（不再有独立的「转发路由」设置）。
 *
 * ── 文件分工 ──────────────────────────────────────────────────
 *   · `usage-actions.js`    余额 / 签到：发请求、写缓存、播报（不碰 DOM）
 *   · `accounts-table.js`   一行长什么样：列定义与单元格 HTML（纯展示）
 *   · `accounts-columns.js` 列宽拖动与持久化
 *   · `accounts-filters.js` 筛选维度：状态、动态注入的下拉、分段计数徽标
 *   · 本文件                重绘编排、批量选择、事件委托、优先级行内编辑
 * 缓存 Map（usageMap / checkinMap）由 usage-actions 持有并**按引用**导出，
 * 因为「查询中」（写 null）这个中间态要让视图立刻看到，拷贝一份就对不上了。
 * 筛选状态同理：`wbAccountsFilters.state` 是本文件读的那一份对象本身。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { toast } = wbApp;
  // 领域判定与标签渲染抽到 accounts-model.js（纯逻辑，无状态），这里直接引用
  const {
    providerOf,
    supportsUsage,
    supportsCheckin,
    checkinableAccounts,
    visibleAccounts,
    positionMap,
    moreMenuHtml,
    usagePanelHtml: usagePanelHtmlOf,
    limitPanelHtml: limitPanelHtmlOf,
    checkinPanelHtml: checkinPanelHtmlOf,
    byPriorityOrder,
  } = wbAccountsModel;
  // 余额 / 签到动作层（本次从本文件拆出），缓存按引用取用
  const actions = wbUsageActions;
  const { usageMap, checkinMap } = actions;
  // 表格渲染层：列定义、行、明细行
  const table = wbAccountsTable;
  // 列宽：渲染时要喂 colgroup，交互（拖动/双击还原）由它自己委托
  const columns = wbAccountsColumns;
  // 筛选维度（状态与它自己的那部分 DOM）在 accounts-filters.js —— state 是同一个对象
  const filters = wbAccountsFilters;
  const accountFilter = filters.state;

  /** 批量选择的账号 id（只作用于当前勾选，不随筛选变化自动增减） */
  const selectedIds = new Set();
  /** 最近一次渲染出的可见账号 id（「全选」只作用于这批） */
  let lastVisibleIds = [];
  /** 正在编辑中的优先级输入框（重绘前记下、重绘后放回，见 accounts-table.js） */
  let editingSnapshot = null;
  /**
   * 重绘中标志：整张表被 innerHTML 换掉时，浏览器可能对**被移除的**输入框补发一个
   * focusout —— 那个输入框已经脱离文档，但 dataset.prio 与用户敲了一半的值都还在，
   * 于是「失焦提交」会被冤枉地触发一次 PATCH（把用户根本没打算提交的中间值存下去）。
   * 重绘期间把它置真、结束置假，commitPriority 见真就跳过。
   */
  let rendering = false;
  /** accountId -> 活跃请求数（只留 >0 的；见下方「连接数（实时）」一节） */
  const connectionsMap = new Map();

  const accounts = () => wbApp.getState()?.accounts?.accounts || [];
  const snapshot = () => wbApp.getState()?.accounts;

  // ─── 行内面板：展开状态 ─────────────────────
  // 面板按需展开（用户点过「限流」/「积分」/「签到」或批量操作带出结果之后），收起时 HTML 为空串。
  /** 已展开明细的账号：accountId -> Set<'limits' | 'usage' | 'checkin'> */
  const openPanels = new Map();

  const panelOpen = (accountId, kind) => openPanels.get(accountId)?.has(kind) === true;

  /** 展开 / 收起某账号的某块明细 */
  function setPanelOpen(accountId, kind, open) {
    if (!accountId) return;
    const set = openPanels.get(accountId) || new Set();
    if (open) set.add(kind); else set.delete(kind);
    if (set.size) openPanels.set(accountId, set);
    else openPanels.delete(accountId);
  }

  /** 批量展开入口：余额 / 签到的批量动作在 usage-actions.js，展开态归本文件管 */
  function openPanelsFor(ids, kind) {
    for (const id of ids) setPanelOpen(id, kind, true);
  }

  /** 该账号当前要渲染的行内明细（未展开时为空串） */
  function panelsHtml(account) {
    let html = '';
    if (panelOpen(account.id, 'limits')) html += limitPanelHtmlOf(account);
    if (panelOpen(account.id, 'usage')) html += usagePanelHtmlOf(account, usageMap.get(account.id));
    if (panelOpen(account.id, 'checkin')) html += checkinPanelHtmlOf(account, checkinMap.get(account.id));
    return html;
  }

  // ─── 列表渲染 ──────────────────────────────

  /**
   * 一张表：所有可见账号按**全局优先级升序**排成一条队列（与后端选路同构）。
   *
   * 每行的 ctx 都是现算的：位置表（序号与 ↑/↓ 边界）、各自的展开态。
   * 明细行紧跟在各自账号行之后（整宽内容，放进单元格会被那一列锁死）。
   */
  function render() {
    const list = $('account-list');
    if (!list) return;
    const snap = snapshot();
    const all = Array.isArray(snap?.accounts) ? snap.accounts : [];
    // 先归一化筛选状态（限流的可用性依赖状态已归一，顺序不能换）
    filters.syncAll(all);
    $('accounts-count').textContent = String(all.length);
    // 余额查询：四家都支持（各由自己的适配器实现），有任一账号有这个概念就可点
    $('btn-query-usage').disabled = !all.some(supportsUsage);
    $('btn-checkin-all').disabled = !checkinableAccounts(all).length;

    // 四个维度各自独立（提供商 / 版本 / 启用状态 / 限流），判定函数在 accounts-groups.js，
    // 可见列表与分段计数共用同一份口径。
    const visible = visibleAccounts(all, accountFilter);
    lastVisibleIds = visible.map(a => a.id);
    renderBatchBar(all, lastVisibleIds);

    if (!all.length) {
      list.innerHTML = '<div class="empty">暂无账号，请点击右上角「登录 / 添加账号」</div>';
      return;
    }
    if (!visible.length) {
      list.innerHTML = '<div class="empty">当前筛选条件下没有账号</div>';
      return;
    }
    const positions = positionMap(all);

    // 逐行渲染（priorityUsage 之类的提示数据不再需要：冲突只有后端一处判）
    const body = visible.slice().sort(byPriorityOrder).map(account => {
      const row = table.rowHtml(account, {
        seat: positions.get(account.id) || { position: 1, total: 1 },
        picked: selectedIds.has(account.id),
        usageEntry: usageMap.get(account.id),
        usageOpen: panelOpen(account.id, 'usage'),
        limitsOpen: panelOpen(account.id, 'limits'),
        // 连接数取实时计数缓存（缺失 = 0，connectionsHtml 会渲染成空）
        connections: connectionsOf(account.id),
      });
      const panels = panelsHtml(account);
      return row + (panels ? table.panelsRowHtml(account, panels) : '');
    }).join('');

    // 重绘会换掉整张表：正在编辑的优先级输入框要先记下、画完再放回原位
    editingSnapshot = table.captureEditing();
    rendering = true;
    list.className = 'acct-scroll';
    list.innerHTML = table.tableHtml(body, columns.widths());
    table.restoreEditing(editingSnapshot);
    rendering = false;
    // 表头那个「全选」是新画出来的节点，状态必须在它进 DOM **之后**再同步：
    // renderBatchBar 里那次同步发生在 innerHTML 赋值之前，只能改到上一版表格里的
    // 那个复选框（此刻已被换掉），所以这里补一次。
    syncSelectAllBoxes(lastVisibleIds);
  }

  /**
   * 批量操作栏：常驻显示，避免「必须先勾选才能全选」的死循环；未勾选时按钮禁用。
   * 全选只覆盖当前筛选结果。visibleIds 传 id 而非账号对象，便于就地刷新操作栏。
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

    const label = $('batch-select-label');
    if (label) label.textContent = visibleIds.length ? `全选当前筛选结果（${visibleIds.length} 个）` : '没有可全选的账号';

    for (const id of ['btn-batch-open', 'btn-batch-clear']) {
      const button = $(id);
      if (button) button.disabled = !active;
    }
    syncSelectAllBoxes(visibleIds);
  }

  /** 「全选」的两个入口（批量栏 + 表头）保持同一状态：勾满 / 半选 / 不可点 */
  function syncSelectAllBoxes(visibleIds) {
    const ids = visibleIds || lastVisibleIds;
    const allPicked = ids.length > 0 && ids.every(id => selectedIds.has(id));
    const somePicked = ids.some(id => selectedIds.has(id));
    for (const id of ['batch-select-all', 'acct-select-all']) {
      const box = $(id);
      if (!box) continue;
      box.checked = allPicked;
      box.indeterminate = !allPicked && somePicked;
      box.disabled = !ids.length;
    }
  }

  /**
   * 把选中态同步到已有 DOM：勾选框、行高亮与操作栏。
   * 就地更新而不是重绘整张表 —— 重绘会丢掉滚动位置，勾选时还会丢失输入焦点。
   */
  function syncSelectionUi() {
    document.querySelectorAll('#account-list input[data-pick]').forEach(box => {
      const picked = selectedIds.has(box.dataset.pick);
      box.checked = picked;
      box.closest('tr.acct-row')?.classList.toggle('selected', picked);
    });
    renderBatchBar(accounts(), lastVisibleIds);
  }

  /** 导航上的账号数徽标（总数：这是「我总共有几个登录态」，与家数无关） */
  function renderNavCount() {
    const all = accounts();
    const badge = $('nav-count-accounts');
    if (!badge) return;
    badge.textContent = String(all.length);
    badge.classList.toggle('muted', all.length === 0);
  }

  /** 清掉已删除账号的本地缓存（缓存本体在 usage-actions.js，展开态在本文件） */
  function refreshCaches(validIds) {
    actions.refreshCaches(validIds);
    for (const id of openPanels.keys()) if (!validIds.has(id)) openPanels.delete(id);
    for (const id of connectionsMap.keys()) if (!validIds.has(id)) connectionsMap.delete(id);
  }

  // ─── 连接数（实时） ─────────────────────────
  //
  // 口径与 OmniProxy 上游管理页的「连接」列一致：**此刻正在使用这个账号的请求数**
  // （一个请求在账号间轮换时计数跟着走，见后端 core::upstream::connections）。
  //
  // 为什么单独一条 2 秒轮询、而不是跟着 app.js 那 20 秒一轮：连接数的全部意义
  // 就在「现在」，20 秒的滞后会让它变成一串没什么信息量的历史值。后端那边是
  // 进程内计数（不读盘、不出网），所以这条链路的代价足够低。

  /** 2 秒：够快看得出「正在跑」，又不至于让 DevTools 的网络面板刷屏 */
  const CONNECTIONS_POLL_MS = 2000;

  const connectionsOf = id => connectionsMap.get(id) || 0;

  /**
   * 拉一次连接数并**就地更新**那一列，不重绘整张表。
   *
   * ── 为什么不走 render() ──────────────────────────────────
   * 整表重绘会打断正在编辑的优先级输入框、把用户展开的明细行重排，而这个数字
   * 每 2 秒就可能变一次 —— 用重绘去追它，页面会一直在抖。连接数格子里没有交互，
   * 宽度又由 colgroup 定死（不参与自适应），所以改它的 innerHTML 是安全的。
   *
   * 失败静默：2 秒一次的轮询，后端不可达时 toast 会变成刷屏；且缓存保留上一轮的
   * 值比清空更贴近事实（用户看到的是「刚才还在跑」，而不是「突然全没了」）。
   */
  async function syncConnections() {
    try {
      const data = await api.getAccountConnections?.();
      const counts = data?.counts && typeof data.counts === 'object' ? data.counts : {};
      // 先更新缓存：即便下面的 DOM 更新因为表格正在重绘而落空，
      // 紧接着的那次 render() 也会用上新值（rowHtml 读的就是这份缓存）
      connectionsMap.clear();
      for (const [id, value] of Object.entries(counts)) {
        const count = Number(value) || 0;
        if (count > 0) connectionsMap.set(id, count);
      }
      paintConnections();
      return true;
    } catch {
      // 静默：下一次轮询自然重试（与 app.js 的 refresh / syncLogsBadge 同一取舍）
      return false;
    }
  }

  /** 把缓存里的连接数写进现有表格的对应格子（没有账号行时是空操作） */
  function paintConnections() {
    const list = $('account-list');
    if (!list) return;
    list.querySelectorAll('tr.acct-row').forEach(row => {
      const cell = row.querySelector('td.cell-connections');
      if (!cell) return;
      const html = table.connectionsHtml(connectionsOf(row.dataset.id));
      // 内容没变就不碰 DOM：2 秒一次无谓的 innerHTML 赋值会把鼠标悬停在数字上
      // 时那个 title 提示打断（表现为提示反复闪烁）
      if (cell.innerHTML !== html) cell.innerHTML = html;
    });
  }

  /**
   * 起轮询定时器（只起一次）。
   *
   * 只在**账号页可见**时真发请求：切到别的页、或窗口被最小化时不打后端 ——
   * 与 requests-panel / logs-panel 的轮询同一取舍。页面上没有账号行时
   * （空列表 / 筛选后为空）`paintConnections` 会自己空转，不做特判。
   */
  let connectionsTimer = null;
  function startConnectionsPolling() {
    if (connectionsTimer) return;
    connectionsTimer = setInterval(() => {
      if (document.hidden || wbApp.currentPage !== 'accounts') return;
      void syncConnections();
    }, CONNECTIONS_POLL_MS);
  }

  // ─── 优先级行内编辑 ─────────────────────────
  //
  // 输入框一直可编辑（不是双击才变控件，理由见 accounts-table.js 的 priorityCell）。
  // 提交时机是**失焦**：读 input.value → 归一 → 与当前值相同就只把显示复原，
  // 不同才发 PATCH。

  const DEFAULT_PRIORITY = table.PRIORITY_DEFAULT;

  /** 显示值 → 归一后的数字；非法（空 / NaN）返回 null，由调用方还原显示 */
  function readPriorityInput(input) {
    const raw = String(input.value ?? '').trim();
    if (!raw) return null;
    const value = Number(raw);
    if (!Number.isFinite(value)) return null;
    return table.clampPriority(value);
  }

  /**
   * 提交一个优先级输入框。
   *
   * 失败（409 冲突）时**必须还原显示值**：留着用户输的数字会让人以为存进去了，
   * 而实际的转发顺序没变 —— 下一次刷新时它会悄悄跳回去，那比当场报错更让人困惑。
   * 后端的 409 文案已经带上了占位者的姓名（`优先级 100 已被账号「X」占用…`），
   * 直接透出即可，不必在前端另写一套判据（前端不知道谁是占位者，除非再算一遍，
   * 而两套算法迟早会分叉）。
   */
  async function commitPriority(input) {
    // 重绘拆掉旧输入框时浏览器补发的那个 focusout 不算用户提交（见 rendering 的说明）
    if (rendering) return;
    const id = input.dataset.prio;
    const account = accounts().find(item => item.id === id);
    if (!account) return;
    const current = table.priorityOf(account);
    const next = readPriorityInput(input);
    // 非法值 / 未改动：只把显示复原，不发请求（改成同一个值后端也会返回空 changes）
    if (next === null || next === current) {
      input.value = String(current);
      return;
    }
    input.disabled = true;
    try {
      await api.updateAccount(id, { priority: next });
      input.value = String(next);
      toast(`✅ 优先级已改为 ${next}`);
      // 改完顺序会变，必须重拉：只改本地 DOM 的话行不会重排，看起来「没生效」
      await wbApp.refresh?.();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      // 还原成**账号当前的真实值**，不是用户输入的值
      input.value = String(current);
      toast(`优先级未保存：${message}`, 'err');
      // 冲突可能来自别处已经改过的数据（比如另一端刚占了号），补一次刷新让列表回到事实
      void wbApp.refresh?.();
    } finally {
      input.disabled = false;
    }
  }

  /** 清除限流标记：单条（model）或全部。后端返回新快照，交给全局刷新对齐。 */
  async function clearLimits(id, model) {
    try {
      await api.clearRateLimits(id, model || null);
      toast(model ? `✅ 已清除 ${model} 的限流标记` : '✅ 已清除该账号全部限流标记');
      await wbApp.refresh?.();
    } catch (error) {
      toast(`清除失败：${error instanceof Error ? error.message : String(error)}`, 'err');
    }
  }

  // ─── ⋯ 菜单（点击才插入 DOM）─────────────────

  let openMenu = null;          // 当前展开的菜单元素
  let menuScrollHost = null;    // 菜单展开期间挂着 scroll 监听的滚动容器
  let menuScrollHandler = null; // 与之配对的监听函数，收起时用来摘掉

  function closeMoreMenu() {
    if (openMenu) { openMenu.remove(); openMenu = null; }
    if (menuScrollHost && menuScrollHandler) {
      menuScrollHost.removeEventListener('scroll', menuScrollHandler);
    }
    menuScrollHost = null;
    menuScrollHandler = null;
  }

  /**
   * 决定菜单往下弹还是往上弹（加 / 摘 .more-menu.flip）。
   *
   * 判定口径：菜单下沿越过「滚动容器可视底」与「视口底」里更靠上的那个即为放不下。
   * 表格住在 .acct-scroll 这个滚动容器里，列表底部的行下方已没有可视空间，
   * 固定往下弹会超出容器底部被裁掉（既看不见也点不到）；行在列表顶部时又该正常往下弹
   * —— 方向只能按当下几何量出来判。
   */
  function applyMenuDirection(menu) {
    // 列表重绘会把菜单连同行一起换掉，此时元素已脱离文档，量不出有意义的值
    if (!menu.isConnected) return;
    // 先摘掉翻转态再量：带着 flip 量到的是「向上」的矩形，用它判定会一直得出
    // 「下方有空间」，下一次滚动就把翻转撤销了 —— 必须量默认的向下位置。
    menu.classList.remove('flip');
    const rect = menu.getBoundingClientRect();
    const scroller = menu.closest('.acct-scroll');
    const bottomLimit = Math.min(
      scroller ? scroller.getBoundingClientRect().bottom : Infinity,
      document.documentElement.clientHeight,
    );
    menu.classList.toggle('flip', rect.bottom > bottomLimit);
  }

  /**
   * 打开「⋯」菜单：点击时才把菜单项插入 DOM，收起时移除。
   * 之所以不是常驻隐藏（display:none），是因为布局探针会把「常驻但隐藏」的
   * 按钮算进行内按钮集合，导致测量结果与实际可见操作不一致。
   * 菜单项本身由 accounts-model.js 生成（启用/禁用、刷新 Token、删除账号）。
   *
   * 宿主是操作单元格 `.cell-actions`（它设了 `position: relative`）：
   * 菜单贴着按钮组弹出、随列表滚动一起移动。
   */
  function toggleMoreMenu(button, account) {
    const already = openMenu && openMenu.dataset.for === account.id;
    closeMoreMenu();
    if (already) return;

    const menu = document.createElement('div');
    menu.className = 'more-menu';
    menu.dataset.for = account.id;
    menu.innerHTML = moreMenuHtml(account);

    const host = button.closest('.cell-actions') || button.closest('tr.acct-row');
    host.appendChild(menu);
    openMenu = menu;

    // 插入后立刻按当下几何定一次方向：列表底部的行改用向上弹
    applyMenuDirection(menu);

    // 菜单开着时盯着滚动：滚动会带着行和菜单一起移动，原本放得下的方向可能变得
    // 放不下（反之亦然），所以持续重测、只增删 flip 类，不关菜单。监听用 passive 且
    // 只读几何，不干扰滚动性能；closeMoreMenu 时统一摘掉。
    menuScrollHost = menu.closest('.acct-scroll');
    if (menuScrollHost) {
      menuScrollHandler = () => {
        // 列表被整体重绘（后台推送刷新等）时，菜单会随旧行一起被换掉。
        // 这里自行收尾，别把监听留在滚动容器上引用一个已脱离文档的节点。
        if (!menu.isConnected) { closeMoreMenu(); return; }
        applyMenuDirection(menu);
      };
      menuScrollHost.addEventListener('scroll', menuScrollHandler, { passive: true });
    }
  }

  // ─── 事件绑定 ──────────────────────────────

  /**
   * 账号行按钮与各级筛选的绑定。
   * 事件绑定与脚本加载顺序绑定（本模块在 app.js 之后加载，wbApp 已就绪），
   * 因此在这里自持注册，而不依赖 app.js 回调过来。
   */
  function bindEvents() {
    $('account-list').addEventListener('click', async event => {
      // 明细条上的「收起」按钮
      const closer = event.target.closest('button[data-panel-close]');
      if (closer) {
        setPanelOpen(closer.closest('tr[data-panels-for]')?.dataset.panelsFor, closer.dataset.panelClose, false);
        render();
        return;
      }
      // 限流明细里的「清除标记」（单条 / 全部）
      const clearAll = event.target.closest('button[data-limit-clear-all]');
      if (clearAll) {
        const id = clearAll.closest('tr[data-panels-for]')?.dataset.panelsFor;
        if (id) await clearLimits(id, '');
        return;
      }
      const clearOne = event.target.closest('button[data-limit-clear]');
      if (clearOne) {
        const id = clearOne.closest('tr[data-panels-for]')?.dataset.panelsFor;
        if (id) await clearLimits(id, clearOne.dataset.limitClear);
        return;
      }
      // ⋯ 菜单里的菜单项（启用/禁用 / 刷新 Token / 删除账号）
      const menuItem = event.target.closest('button[data-menu-action]');
      if (menuItem) {
        // 置灰项不响应；后端也会拒绝，这里先挡住
        if (menuItem.disabled) return;
        const { menuAction, id } = menuItem.dataset;
        closeMoreMenu();
        // 启用/禁用在这里自行消化，不交给 app.js 的 runAccountAction ——
        // 那个入口只处理 switch / refresh / remove，未知 action 会被静默忽略。
        // 走 updateAccount（PATCH /api/accounts/<id>），与状态列的开关、设置弹窗里
        // 勾选「启用」保存是同一条链路，语义一致。
        if (menuAction === 'enable' || menuAction === 'disable') {
          await toggleAccountEnabled(id, menuAction === 'enable');
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
      if (action === 'limits') {
        // 点「限流」徽章即展开明细；已展开时再点则收起（当成开关用）
        setPanelOpen(id, 'limits', !panelOpen(id, 'limits'));
        render();
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
          await actions.queryUsageFor(id);
          // 缓存的四种形态（见 usage-panel.js）：undefined/null/字符串/对象，
          // 对象里再分「未配置」与「失败」—— 提示语要跟着这个分叉走
          const failure = actions.usageFailureOf(usageMap.get(id));
          if (failure?.notConfigured) toast(failure.message, 'ok');
          else if (failure) toast(`余额查询失败：${failure.message}`, 'err');
          else toast('✅ 已更新余额');
        } catch (error) {
          usageMap.set(id, `查询失败：${error.message}`);
          render();
          toast(`余额查询失败：${error.message}`, 'err');
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

    /**
     * 启用 / 禁用单个账号（状态列的开关与 ⋯ 菜单的第一项走同一条链）。
     *
     * 为什么放在这里而不是 app.js 的 runAccountAction：那个入口的 switch 分支只认
     * switch / refresh / remove，别的 action 会静默走完不做事。
     *
     * 用 updateAccount 走 PATCH —— 后端 apply_patch 只改显式传入的字段，这里只传
     * enabled，别的一概不动。成功后 refresh() 会重新拉 getState 并 render()，
     * 所以状态徽章（已禁用）、行内按钮（签到按启用状态显隐）会立即跟着变。
     */
    async function toggleAccountEnabled(id, enabled) {
      try {
        await api.updateAccount(id, { enabled });
        await wbApp.refresh?.();
        toast(enabled ? '✅ 已启用' : '✅ 已禁用');
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        toast(`操作失败：${message}`, 'err');
        // 失败时把开关拨回去：界面上不能留一个「已改」的假象
        void wbApp.refresh?.();
      }
    }

    /** 单个账号签到：展开明细 → 串行请求 → 就地刷新结果 */
    async function runCheckin(id) {
      checkinMap.set(id, null);
      render();
      try {
        await actions.checkinFor(id);
        const entry = checkinMap.get(id);
        if (typeof entry === 'string') toast(`签到失败：${entry}`, 'err');
        else if (entry?.success) toast('✅ 该账号签到成功');
        else if (entry?.msg) toast(entry.msg, 'err');
        // 重新拉一次账号状态：签到时间（checkinAt）是**后端**落的，
        // 不重拉的话这一行的按钮要等下一轮 20 秒轮询才会变成「已签到」，
        // 而这期间它还是可点的 —— 正好是这次改动要消除的「看起来还能再签一次」。
        // 失败路径也拉：上游说「今天已签到」同样会落时间（见后端 checkin_completed_today），
        // 所以那条也不是纯粹的失败。
        void wbApp.refresh?.();
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

    // 优先级输入框：失焦提交。
    // 用 focusout（冒泡）而不是 blur（不冒泡）：事件委托挂在列表上，一个监听覆盖
    // 所有行 —— 每渲染一次就逐行绑一次的话，重绘后那些监听就随旧节点一起没了。
    // 也不监听 change：数字框在「清空后失焦」时不一定派发 change，那样输入框会
    // 停在一个既没保存、也不是原值的状态上。
    //
    // isConnected 这道门是必须的：整张表被 innerHTML 换掉时，被移除的那个输入框
    // 可能补发一个 focusout（见 rendering 的说明），而它此刻已不在文档里 ——
    // 那是重绘的副产品，不是用户提交。
    $('account-list').addEventListener('focusout', event => {
      const input = event.target.closest?.('input[data-prio]');
      if (!input || input.disabled || !input.isConnected) return;
      void commitPriority(input);
    });
    // 回车 = 提交（失焦即走上面那条路）；Esc = 放弃这次输入、还原成当前值
    $('account-list').addEventListener('keydown', event => {
      const input = event.target.closest?.('input[data-prio]');
      if (!input) return;
      if (event.key === 'Enter') {
        event.preventDefault();
        input.blur();
      } else if (event.key === 'Escape') {
        const account = accounts().find(item => item.id === input.dataset.prio);
        input.value = String(table.priorityOf(account) ?? DEFAULT_PRIORITY);
        input.blur();
      }
    });

    // 版本 / 启用状态 / 限流 / 提供商：筛选维度的绑定在 accounts-filters.js
    // （状态与它自己的那部分 DOM 都在那边），这里只把「变了要重绘」这件事传过去
    filters.bind(render);

    // 列宽拖动 / 双击还原：委托在 accounts-columns.js，这里把容器交给它
    columns.bind($('account-list'));

    // 行首复选框 / 状态开关（change 事件委托）
    $('account-list').addEventListener('change', event => {
      const box = event.target.closest('input[data-pick]');
      if (box) {
        const id = box.dataset.pick;
        if (box.checked) selectedIds.add(id);
        else selectedIds.delete(id);
        // 就地更新，不重绘整张表，避免丢掉滚动位置
        syncSelectionUi();
        return;
      }
      const toggle = event.target.closest('input[data-toggle]');
      if (toggle) void toggleAccountEnabled(toggle.dataset.toggle, toggle.checked);
    });

    // 全选 / 全不选：只作用于当前筛选结果（批量栏与表头两个入口同一条链）
    for (const id of ['batch-select-all', 'acct-select-all']) {
      $(id)?.addEventListener('change', event => {
        if (event.target.checked) for (const id of lastVisibleIds) selectedIds.add(id);
        else for (const id of lastVisibleIds) selectedIds.delete(id);
        syncSelectionUi();
      });
    }

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

    $('btn-query-usage').addEventListener('click', actions.queryAllUsage);
    $('btn-checkin-all').addEventListener('click', actions.checkinAll);
  }

  // 追加式注入必须在 bindEvents 之前完成：提供商下拉是动态插进工具条的节点，
  // 它的 change 监听（filters.bind）在 bindEvents 里才挂得上。
  filters.mount();
  bindEvents();
  // 连接数轮询：定时器自持「页面可见才发请求」的判定（见 startConnectionsPolling）。
  // 这里不立刻拉一次 —— 首屏那次 refresh() 渲染出的表格里连接数本来就是空的，
  // 两秒后第一轮轮询会把它填上；抢跑一次只会与首屏的状态请求撞在一起。
  startConnectionsPolling();

  window.wbAccountsView = {
    render,
    renderNavCount,
    refreshCaches,
    openPanels: openPanelsFor,
    // 连接数：切页面回来时视图侧主动补一次（轮询只认「当时在账号页」，
    // 切走的这两分钟里数据已经过期了）
    syncConnections,
    // 余额 / 签到的动作与缓存都在 usage-actions.js，这里只做转发：
    // app.js 与外部仍按原有的 wbAccountsView 名字调用，引用路径一行不用改
    applyBalances: actions.applyBalances,
    // 定时查询积分的结果快照轮询（见 usage-actions.js 的 syncSnapshot）
    syncBalancesSnapshot: actions.syncSnapshot,
    queryUsageFor: actions.queryUsageFor,
    queryAllUsage: actions.queryAllUsage,
    checkinFor: actions.checkinFor,
    checkinAll: actions.checkinAll,
    checkinableAccounts,
    supportsCheckin,
    supportsUsage,
    isDesktopAccount: wbAccountsModel.isDesktopAccount,
    isEnabled: wbAccountsModel.isEnabled,
    isRateLimited: wbAccountsModel.isRateLimited,
  };
})();
