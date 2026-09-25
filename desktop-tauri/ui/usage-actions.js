/* Agent2API · 账号的余额 / 签到动作层（请求 + 结果缓存） */
/* global workbuddyDesktop, wbApp, wbAccountsModel, wbAccountsView */

/**
 * 账号「余额查询 / 签到」的**有状态动作层**，从 accounts-view.js 按职责拆出。
 *
 * ── 为什么要拆 ────────────────────────────────────────────────
 * accounts-view.js 原先把「列表渲染」「行内面板展开态」「⋯ 菜单」「余额/签到动作」
 * 四件事混在一个 IIFE 里，已到 800 行上限。账号列表从卡片网格改成表格后渲染代码
 * 只增不减，所以先把**与渲染无关**的一块（网络请求 + 结果缓存 + toast 播报）搬出来。
 * 本文件只做「发请求、把结果写进缓存、把结论播报出来」，一行 DOM 都不碰 ——
 * 重绘由 accounts-view.js 通过 `wbAccountsView.render()` 回调触发（见下）。
 *
 * ── 与 accounts-view.js 的分工 ─────────────────────────────────
 * 缓存（usageMap / checkinMap）与面板展开态（openPanels）分属两侧：
 *   · usageMap / checkinMap 住在本文件：它们是**请求结果**，写入者只有本文件的
 *     query/checkin 四条路径、`syncSnapshot`（定时查询那一轮的快照），
 *     以及外部的 `applyBalances`（经 wbAccountsView 转发）。
 *   · openPanels 留在 accounts-view.js：它表达的是「用户展开过哪一行」，是**视图状态**，
 *     表格重绘时要跟着行一起算，搬过来只会让两边互相回调。
 *     **余额已经没有明细行了**（读数就在余额列上，见 usageCell），所以那份展开态
 *     现在只服务「限流 / 签到」两块面板，本文件只在批量签到时请求展开一次。
 * 因此本文件需要重绘时**调 `window.wbAccountsView?.render?.()`** 而不是自己 import ——
 * 这与项目既有的跨文件约定一致（accounts-table.js 只做渲染、app.js 用可选链委托）。
 * 反向依赖（accounts-view.js 读缓存）走本文件导出的 Map 引用：**导出的是 Map 本身而不是
 * 副本**，因为「查询中」这个中间态（写 null）必须让视图立刻看见，拷贝一份就对不上了。
 *
 * ── 为什么后端语义一行不改 ─────────────────────────────────────
 * 五家都支持余额查询（各自的适配器实现），但**有签到活动的只有三家**
 * （WorkBuddy 国内版 / 小浣熊 / AutoClaw）。这个不对称不是界面取舍而是上游事实，
 * 所以签到那条链的目标集合（启用 + 非国际版 + 所属家有签到）在后端与前端都按
 * 同一口径过滤；改动它会让「提示签了 3 个、实际签了 2 个」。
 * 本文件因此原样承接拆分前的判定与文案，只换存放位置。
 */
(() => {
  const api = workbuddyDesktop;
  const { toast } = wbApp;
  const { supportsUsage, checkinableAccounts } = wbAccountsModel;

  /**
   * 「未配置查询凭证」的标记（与后端 `providers::adapter::USAGE_NOT_CONFIGURED_CODE`
   * 逐字一致）。这是一个**前后端契约常量**：改一边必须改另一边，
   * 不一致的后果是那种情况退回红色「查询失败」。
   */
  const NOT_CONFIGURED_CODE = 'usage_not_configured';

  /**
   * 缓存条目 → 失败描述的**唯一入口**（余额列的摘要渲染与这里的 toast 播报共用
   * 同一份判据，两处不会一个说红一个说灰）。
   *
   * 返回 null 表示这不是失败（还在查询中 / 是结果）；否则 `{message, notConfigured}`。
   *
   * ── 「未配置」为什么是中性的 ───────────────────────────────────
   * CatPaw 的余额接口要一个**单独的**网页会话凭证（token2），没配置时后端返回
   * `code: "usage_not_configured"`。那不是故障：账号本身完全正常、转发照跑，
   * 只是用户还没告诉网关那个凭证长什么样。把它渲染成红色的「查询失败」会让人
   * 去排查一个不存在的故障，所以判据用后端给的 `code` **而不是匹配文案**
   * （措辞一改，按文案的写法就会静默退回红色）。
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

  /** accountId -> usage | {error, code?} | string | null(查询中) */
  const usageMap = new Map();
  /** accountId -> claimResult | error string | null(签到中) */
  const checkinMap = new Map();

  /** 两个批量动作各自的并发闸：同一条链在飞时不允许再发一次（按钮也会置灰） */
  let usageBusy = false;
  let checkinBusy = false;

  const accounts = () => wbApp.getState()?.accounts?.accounts || [];

  /**
   * 请求归来后的重绘入口。
   *
   * 用可选链而不是直接引用 `wbAccountsView.render`：本文件必须在 accounts-view.js
   * **之前**加载（后者加载期就要解构这边的缓存 Map），所以执行期它必然已就绪；
   * 但脚本顺序若被谁改乱，这里退化成「不重绘」而不是抛错把整条请求链带崩 ——
   * 下一次轮询刷新仍会把结果画上去。
   */
  const repaint = () => window.wbAccountsView?.render?.();

  /** 清掉已删除账号的本地缓存（app.js 每次 refresh 后调用）。返回是否清掉了东西。 */
  function refreshCaches(validIds) {
    let removed = 0;
    for (const id of usageMap.keys()) if (!validIds.has(id)) { usageMap.delete(id); removed++; }
    for (const id of checkinMap.keys()) if (!validIds.has(id)) { checkinMap.delete(id); removed++; }
    return removed;
  }

  /** 后端结果行 → 缓存条目（成功给 `usage`，失败给 `{error, code?}`）。
   *  失败行的 `code` 必须留住：`usageFailureOf` 按它区分「未配置查询」（中性提示）
   *  与真正的失败（红色）—— 只存 error 字符串会丢掉这个判据。 */
  function cacheEntryOf(row) {
    return row.usage
      ? row.usage
      : { error: row.error ? String(row.error) : '余额响应为空', code: row.code };
  }

  /** 把一批余额结果写进列表缓存（定时快照、批量查询与外部调用共用）。返回写入条数。 */
  function applyBalances(balances) {
    const rows = Array.isArray(balances?.results) ? balances.results : [];
    let applied = 0;
    for (const row of rows) {
      if (!row?.id) continue;
      usageMap.set(row.id, cacheEntryOf(row));
      applied++;
    }
    return applied;
  }

  /**
   * 拉一次「定时查询积分」的结果快照并写进缓存，返回是否应用了新的一轮。
   *
   * ── 为什么要有这条 ────────────────────────────────────────
   * 余额查询在后端有条定时任务（默认每 10 分钟查全部账号），结果存在后端快照里。
   * 界面不点按钮时也要跟着它更新 —— 否则定时任务在后台跑得好好的，用户看到的
   * 还是启动那一次的旧余额，那正是「定时查询」最容易让人觉得「没生效」的地方。
   *
   * `at` 是那一刻的毫秒时间戳，用它判断「这一轮我应用过了没」：
   * 轮询时快照的时间戳没变就直接返回，不做无谓的整表重绘。
   * **失败的行同样会被应用**（后端快照里就带着它们），于是账号页会明确显示
   * 「查询失败」而不是悄悄留着上一个成功的旧值 —— 这是这条链路的既定口径。
   *
   * 失败静默（不 toast）：它是 20 秒一次的轮询，网关长时间不可用会变成刷屏 ——
   * 与 `app.js` 里 refresh / syncLogsBadge 那两条轮询同一取舍。上游的账号级失败
   * 不走这里，它们由快照内的失败行表达，界面上有明确标记。
   */
  let lastSnapshotAt = 0;
  async function syncSnapshot() {
    try {
      const data = await api.getBalancesSnapshot?.();
      const at = Number(data?.at) || 0;
      // at = 0 表示本进程还没定时查过（刚启动、或任务被关掉）—— 不覆盖已有缓存
      if (!at || at === lastSnapshotAt) return false;
      lastSnapshotAt = at;
      if (!applyBalances(data)) return false;
      repaint();
      return true;
    } catch {
      // 静默：下一次轮询自然重试；账号页保持上一轮的结果不变
      return false;
    }
  }

  /**
   * 查询余额。`id` 缺省 = 全部（后端批量目标集合）；
   * 给了 id 则**带 `?id=` 请求**，后端只查那一个账号。
   *
   * ── 为什么单查要走 `?id=` 而不是「取一批后筛一条」────────────
   * 后端批量路径的目标集合是「全部**启用**账号」—— 禁用是「别用它转发」的意思，
   * 定时那一轮不该为它们发请求。但用户手点某一行账号的「余额」按钮问的是另一个
   * 问题：「这个账号现在还剩多少」。按启用状态把它挡掉，结果里就没有这一行，
   * 界面只能兜底成「未返回余额数据」—— 用户分不清是禁用了还是上游挂了。
   * 所以单查带 id 走后端那条**不看启用状态**的分支（见 core::usage_query）。
   *
   * **签到那边本次改成了同样的口径**（`checkin.rs` 的单账号路径不再看
   * `enabled`，只拒国际版）：余额查询是只读的，「这个禁用账号还剩多少」是
   * 合理且无害的问题；签到虽然会消耗上游额度、写回 `checkinAt`，但用户对
   * 某个账号显式点「签到」本身就是明确意图，替他拦下来反而多余 ——
   * 禁用管的是「别让它承接转发」，不该顺手管到积分签到。
   *
   * 批量（`id` 缺省）仍是「全部启用账号」，行为与改造前一致。
   */
  async function queryUsageFor(id) {
    const data = await api.getAllBalances(id || undefined);
    const rows = Array.isArray(data?.results) ? data.results : [];
    const returned = new Set();
    for (const row of rows) {
      if (!row?.id) continue;
      if (id && row.id !== id) continue;
      returned.add(row.id);
      usageMap.set(row.id, cacheEntryOf(row));
    }
    if (id) {
      // 后端返回了 0 行才是真的「没数据」（账号刚被删、或 provider 不认这个 id）；
      // 禁用账号走到这里是正常的 —— 单查不过滤启用状态。
      if (!returned.has(id)) usageMap.set(id, '未返回余额数据');
      repaint();
      return data;
    }
    // 只给**批量目标集合内的**账号补「未返回」：后端的目标集合是「启用 +
    // 有余额概念」，缺失一行才是异常。禁用账号不在集合里，补它等于把
    // 「这行没参与本轮查询」说成「上游没给数据」—— 与单查那个 bug 同源。
    for (const acc of accounts()) {
      if (!returned.has(acc.id) && supportsUsage(acc) && acc.enabled !== false) {
        usageMap.set(acc.id, '未返回余额数据');
      }
    }
    repaint();
    return data;
  }

  /**
   * 批量查询全部可查询账号的余额（工具条「查询余额」）。
   *
   * 目标集合只含「有余额概念 + 启用」：已禁用账号后端同样会跳过，
   * 界面若把它算进分母，播报的「已更新 N/M」会与真实条数对不上。
   *
   * ── 每次点击都是一次查询（本次改造）──────────────────────────
   * 原先这颗按钮第二次点击是「收起全部明细行」。余额的明细面板已经取消
   * （读数就在余额列上，见 accounts-table.js 的 usageCell），所以「展开态」
   * 这个概念对它不再存在 —— 点几下就是查几次，与单账号那颗「余额」按钮同口径。
   */
  async function queryAllUsage() {
    if (usageBusy) return;
    const targets = accounts().filter(account => supportsUsage(account) && account.enabled !== false);
    if (!targets.length) { toast('暂无可查询余额的账号', 'err'); return; }
    usageBusy = true;
    const button = document.getElementById('btn-query-usage');
    if (button) { button.disabled = true; button.textContent = '查询中…'; }
    // 先写「查询中」再重绘：余额列立刻显示查询中，结果回来了直接换成读数
    targets.forEach(a => usageMap.set(a.id, null));
    repaint();
    try {
      const rows = (await queryUsageFor(null))?.results || [];
      const ok = rows.filter(r => r.usage).length;
      // 「未配置查询」不算失败：那些账号成功返回了、只是缺一个可选的凭证
      const skipped = rows.filter(r => r.code === NOT_CONFIGURED_CODE).length;
      const suffix = skipped ? `（${skipped} 个未配置查询凭证）` : '';
      const failed = rows.length - ok - skipped;
      toast(ok + skipped === rows.length
        ? `✅ 已更新 ${ok} 个账号的余额${suffix}`
        : `已更新 ${ok}/${rows.length} 个账号，${failed} 个失败`, failed ? 'err' : 'ok');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      targets.forEach(a => usageMap.set(a.id, `查询失败：${message}`));
      repaint();
      toast(`余额查询失败：${message}`, 'err');
    } finally {
      usageBusy = false;
      if (button) { button.disabled = false; button.textContent = '查询余额'; }
    }
  }

  /**
   * 签到。`id` 缺省 = 全部可签到账号串行签到；指定 id = 单账号签到。
   *
   * 目标集合、按钮可用性与确认文案三处都只用「有签到概念 + 国内版」的账号
   * （后端 core::billing::checkin 同样只把国际版排除在外，**不看启用状态**）。
   */
  async function checkinFor(id) {
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
    repaint();
    return data;
  }

  /** 批量签到（工具条「全部签到」）。结果要看得见，所以同时把明细行展开。 */
  async function checkinAll() {
    if (checkinBusy) return;
    const targets = checkinableAccounts(accounts());
    if (!targets.length) { toast('暂无可签到的账号（签到仅限 WorkBuddy 国内版 / 小浣熊 / AutoClaw）', 'err'); return; }
    if (!(await window.wbConfirm?.ask?.({
      title: '批量签到',
      html: `将对 <strong>${targets.length}</strong> 个账号串行签到，可能需要一点时间。继续？`,
      okText: '继续',
      bodyClass: '',
    }))) return;
    checkinBusy = true;
    const button = document.getElementById('btn-checkin-all');
    if (button) { button.disabled = true; button.textContent = '签到中…'; }
    targets.forEach(a => checkinMap.set(a.id, null));
    window.wbAccountsView?.openPanels?.(targets.map(a => a.id), 'checkin');
    repaint();
    try {
      const data = await checkinFor(null);
      const succeeded = data?.succeeded ?? 0;
      const total = data?.total ?? 0;
      const skipped = Number(data?.skipped) || 0;
      toast(`签到完成：${succeeded}/${total} 个账号成功领取`
        + (skipped ? `（跳过 ${skipped} 个国际版 / 所属家无签到的账号）` : ''), succeeded < total ? 'err' : 'ok');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      targets.forEach(a => checkinMap.set(a.id, `签到失败：${message}`));
      repaint();
      toast(`签到失败：${message}`, 'err');
    } finally {
      checkinBusy = false;
      if (button) { button.disabled = false; button.textContent = '全部签到'; }
      // 重拉账号状态：签到时间由后端落盘，行上的签到按钮据此变成「已签到」。
      // 与单个签到的处理同理（见 accounts-view.js 的 runCheckin）——
      // 不重拉的话按钮要等下一轮 20 秒轮询才跟上，那期间还显示成可点。
      void wbApp.refresh?.();
    }
  }

  window.wbUsageActions = {
    // 两个缓存 Map 按**引用**导出：视图侧读它们渲染余额列读数与签到面板，
    // 而「查询中」（写 null）这个中间态要让视图立刻看到，拷贝一份就对不上了
    usageMap,
    checkinMap,
    refreshCaches,
    applyBalances,
    syncSnapshot,
    queryUsageFor,
    queryAllUsage,
    checkinFor,
    checkinAll,
    usageFailureOf,
    NOT_CONFIGURED_CODE,
  };
})();
