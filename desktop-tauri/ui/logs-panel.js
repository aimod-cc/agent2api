/* Agent2API · 系统事件日志面板（筛选 / 分页 / 导出 / 清空） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的日志面板模块：自持筛选条件、页码与轮询定时器。
 * 网关的模型请求已拆到「请求日志」页（requests-panel.js），本文件只管系统事件。
 *
 * 依赖 window.wbApp 的 esc / toast / updateLogsBadge，
 * 通过 window.wbLogsPanel 暴露 load 给 app.js（切到日志页时立即刷新）。
 *
 * ── 数据口径 ────────────────────────────────────────────────
 * 系统事件（GET /api/logs）：最多 500 条（后端 `logs_store::MAX_ENTRIES`），
 * 一次拉满后端上限即可，翻页纯在前端算 —— 交互即时，也不会和轮询抢状态。
 *
 * ── 「脱敏」分类为什么还在下拉里 ──────────────────────────────
 * 那一类曾是敏感词模块打的 `[Desensitize]` 应用日志。该模块已随规则集换成
 * 硬编码指纹脱敏而删除（现在的指纹脱敏日志走 `[Config]`），因此**不再有新条目**；
 * 分类字典保留它（前后端两侧都保留）是为了让**历史条目**仍能按分类筛出来看。
 * 逐请求的命中明细一直不在这个页面，而在请求日志的「敏」标签里
 * （`sensitiveHits` 字段 → 悬停看命中了哪几条规则）。
 *
 * ── 自动刷新的间隔从哪来 ────────────────────────────────────
 * 不在本文件写死：由「定时任务」页（tasks-panel.js）配置，落在 config.json 的
 * `scheduledTasks.logsAutoRefresh`。本文件启动时自读一次、之后接受那边推送
 * （`applyAutoRefresh`）。页头原先那个开关已随之移除 —— 开关与间隔是同一件事
 * （`enabled: false` 就是不刷），分在两个页面反而更容易出现「关了还在刷」。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast, updateLogsBadge } = wbApp;

  /**
   * 自动刷新间隔（毫秒），由「定时任务」页配置（`scheduledTasks.logsAutoRefresh`）。
   *
   * 改造前这个节奏写死在这里（`AUTO_REFRESH_MS = 10_000`），并在页头放一个
   * 开关控制它；现在两者都归「定时任务」页 —— 那个页面改完会调
   * `applyAutoRefresh` 把新值推过来，本面板启动时也自己拉一次
   * （见 `syncAutoRefresh`），于是无论用户先开哪一页都对得上。
   *
   * 兜底值 1 秒 = 后端的默认间隔（`DEFAULT_LOGS_AUTO_REFRESH_SECONDS`）：
   * 后端拿不到配置时（冷启动首次请求失败）与用户没改过时的行为一致。
   */
  const DEFAULT_AUTO_REFRESH_MS = 1_000;
  let autoRefreshMs = DEFAULT_AUTO_REFRESH_MS;
  /** 任务关闭时置 false：定时器不跑（区别于「间隔很大」） */
  let autoEnabled = true;
  /**
   * 是否已经从后端读到过间隔配置。
   *
   * 两个作用：① 自读只做一次，切页面不重复请求；② 「定时任务」页推过来的值
   * 也算同步过（见 `applyAutoRefresh`），避免一次迟到的失败自读把用户刚改好的
   * 间隔覆盖回兜底值。
   */
  let autoSynced = false;
  /**
   * 是否有一次轮询触发的拉取还在途中。
   *
   * 定时器是 `setInterval`（不等上一次完成），而间隔可以调到 1 秒 ——
   * 一次慢响应就会与后来的几拍叠在一起，各自带着自己的页码 / 筛选快照乱序落地，
   * 列表会来回跳。所以轮询这一拍撞上在途请求时直接跳过，由下一拍补上
   * （间隔本来就是「最多晚一拍」，不累积延迟）。
   *
   * 只挡轮询：用户翻页 / 换筛选是有意操作，不该被上一次自动刷新挡掉。
   */
  let polling = false;

  /** 事件日志每页条数 */
  const PAGE_SIZE = 50;
  /** 系统事件单次拉取上限（后端上限）：一次拿全，总页数才对得上真实结果 */
  const FETCH_LIMIT = 500;
  /** 事件日志时间范围的持久化键（请求日志页的档位在 requests-panel.js，两键独立） */
  const EVENT_RANGE_KEY = 'workbuddy-desktop-logs-range';

  /** 合法的时间档位与后端 /api/stats/summary 的白名单同字面量（报表页也是这一组）。
   *  默认「全部」而不是报表页的 7 天：日志页原先没有任何时间条件，
   *  加筛选时默认必须落在「行为不变」的那一档上 */
  const RANGES = ['today', '7', '30', 'month', 'all'];
  const DEFAULT_RANGE = 'all';
  const RANGE_LABEL = { today: '今天', 7: '近 7 天', 30: '近 30 天', month: '本月', all: '全部' };

  let panelBusy = false;
  let current = null;      // 最近一次事件日志查询结果
  let stats = null;        // 最近一次统计（导航徽标用）
  let timer = null;
  let page = 1;            // 事件日志当前页码（1 起）

  // ─── 档位状态 ────────────────────────────────

  /** 只有明确存过合法档位才采纳；无值 / 读取抛错 / 值被改坏一律回落「全部」 */
  function readRange(key) {
    try {
      const saved = localStorage.getItem(key);
      return RANGES.includes(saved) ? saved : DEFAULT_RANGE;
    } catch {
      return DEFAULT_RANGE;
    }
  }

  function persistRange(key, value) {
    try {
      localStorage.setItem(key, value);
    } catch {
      // 存储不可用只影响下次打开，不影响本次会话内的表现
    }
  }

  /** 档位的当前值以内存为准：localStorage 只负责跨次启动恢复 */
  let eventRange = readRange(EVENT_RANGE_KEY);

  // ─── 时间档位 → start 参数 ────────────────────

  /** 取某个时刻的本地零点毫秒值 */
  function midnight(date) {
    return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  }

  /**
   * 档位对应的毫秒下界（闭区间起点），口径与报表页 `range_bounds` 逐日一致：
   * 「N 天」= 含今天在内的 N 个自然日，所以往前推 N-1 天。
   *
   * `new Date(y, m, d)` 走的是本地时区构造，因此跨月、跨年、闰月都由 Date 自己算，
   * 不用手写进位；也正因为是本地构造，夏令时地区不会出现「零点偏移一小时」。
   *
   * 「全部」返回 null（不传 start）：这与加筛选之前的行为完全相同。
   */
  function rangeStart(range) {
    const now = new Date();
    switch (range) {
      case 'today': return midnight(now);
      case '7': return midnight(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 6));
      case '30': return midnight(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 29));
      case 'month': return midnight(new Date(now.getFullYear(), now.getMonth(), 1));
      default: return null;
    }
  }

  // ─── 事件日志：渲染 ──────────────────────────

  /**
   * 时间 + 日期：日志按保留期存盘（最长可到 3650 天），只有时刻会对不出
   * 「哪一天」。当年省年份（主要看「刚刚发生」），跨年带全日期。
   */
  function formatLogTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    const pad = n => String(n).padStart(2, '0');
    const clock = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    const date = `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
    if (d.getFullYear() === new Date().getFullYear()) return `${date} ${clock}`;
    return `${d.getFullYear()}-${date} ${clock}`;
  }

  const LEVEL_LABEL = { debug: '调试', info: '信息', warn: '警告', error: '错误' };

  /** 附加数据渲染成简短后缀：429 切换显示「账号 A → 账号 B · 恢复时间」 */
  function extraText(entry) {
    const data = entry.data;
    if (!data) return '';
    const parts = [];
    if (data.from || data.to) {
      parts.push([data.from, data.to].filter(Boolean).join(' → '));
    }
    if (data.model) parts.push(`模型 ${data.model}`);
    if (data.resetAtText) parts.push(`${data.resetAtText} 恢复`);
    else if (data.status && !data.from && !data.to) parts.push(`HTTP ${data.status}`);
    return parts.length ? parts.join(' · ') : '';
  }

  /**
   * 徽标：`N 条`（无筛选时）或 `M / N 条`（筛过时）。
   *
   * 改造前这里还有一层「已隐藏 K 条脱敏」的读数 —— 随「不看脱敏」开关一起移除
   * （见模块头的说明）。现在这两个数字就是后端给的原值，不再有「可见条数」
   * 与「匹配条数」的差别，括号里那层解释也就不需要了。
   */
  function renderBadge(badge, { total, matched }) {
    if (!badge) return;
    badge.className = 'badge';
    badge.textContent = matched === total ? `${total} 条` : `${matched} / ${total} 条`;
  }

  function renderPager(pageCount) {
    const info = $('logs-page-info');
    if (info) info.textContent = `第 ${page} / ${pageCount} 页`;
    const prev = $('btn-logs-prev');
    const next = $('btn-logs-next');
    if (prev) prev.disabled = page <= 1;
    if (next) next.disabled = page >= pageCount;
  }

  /** 空态文案（改造前还有一条「全被开关藏起来」的说明，随开关移除） */
  function emptyText(total, matched) {
    if (!total) return '暂无日志';
    // matched 有值却一条都没渲染出来，只可能是页码越界（清空 / 筛选变严后
    // 停在旧页）；render 会把页码夹回有效范围，所以这里只需给出通用文案
    void matched;
    return '没有符合筛选条件的日志';
  }

  function rowHtml(entry) {
    const extra = extraText(entry);
    const level = esc(entry.level);
    return `<div class="log-row ${level}">
        <span class="log-rail"><span class="log-dot ${level}"></span></span>
        <div class="log-main">
          <div class="log-line">
            <span class="log-cat">${esc(current.categories?.[entry.category] || entry.category)}</span>
            <span class="log-lvl ${level}">${esc(LEVEL_LABEL[entry.level] || entry.level)}</span>
            <span class="msg">${esc(entry.message)}</span>
          </div>
          ${extra ? `<div class="log-extra-wrap"><span class="extra">${esc(extra)}</span></div>` : ''}
        </div>
        <span class="time">${esc(formatLogTime(entry.ts))}</span>
      </div>`;
  }

  function render(data) {
    if (data !== undefined) current = data;
    const list = $('log-list');
    if (!list) return;

    const entries = Array.isArray(current?.entries) ? current.entries : [];
    const total = Number(current?.total) || 0;
    const matched = Number(current?.matched) || 0;

    // 总页数按条目数算；日志被清空或筛选变严后页码会越界，回落到最后一页
    const pageCount = Math.max(1, Math.ceil(entries.length / PAGE_SIZE));
    page = Math.min(Math.max(1, page), pageCount);

    renderBadge($('logs-badge'), { total, matched });
    if (current?.file) $('logs-file').textContent = current.file;
    renderPager(pageCount);

    const start = (page - 1) * PAGE_SIZE;
    const pageRows = entries.slice(start, start + PAGE_SIZE);
    if (!pageRows.length) {
      list.innerHTML = `<div class="log-empty">${esc(emptyText(total, matched))}</div>`;
      return;
    }

    list.innerHTML = pageRows.map(rowHtml).join('');
  }

  /** 分类下拉选项由后端返回的字典填充（只填一次；含「脱敏」，可专门筛出这一类看） */
  function fillCategories(categories) {
    const select = $('logs-category');
    if (!select || !categories || select.dataset.filled === '1') return;
    for (const [value, label] of Object.entries(categories)) {
      const option = document.createElement('option');
      option.value = value;
      option.textContent = label;
      select.appendChild(option);
    }
    select.dataset.filled = '1';
  }

  // ─── 加载 ──────────────────────────────────

  function queryParams() {
    const params = new URLSearchParams();
    const level = $('logs-level')?.value;
    const category = $('logs-category')?.value;
    const keyword = $('logs-keyword')?.value.trim();
    const start = rangeStart(eventRange);
    if (level) params.set('level', level);
    if (category) params.set('category', category);
    if (keyword) params.set('keyword', keyword);
    // 时间条件与级别 / 分类 / 关键词是「与」关系（后端逐层收紧过滤链）。
    // 只传下界不传上界：闭开区间 [start, end) 的上界缺省即「到此刻为止」，
    // 这比拿 Date.now() 当 end 更稳 —— 不会因为本地时钟比服务端快几百毫秒
    // 把刚刚落盘的那条日志挡在窗口外。
    if (start !== null) params.set('start', String(start));
    params.set('limit', String(FETCH_LIMIT));
    return params.toString();
  }

  /** 导航徽标的数据源：徽标只统计系统事件里的 error（见 wbApp.updateLogsBadge） */
  function applyStats(nextStats) {
    stats = nextStats || null;
    updateLogsBadge?.(stats);
  }

  async function load({ silent = false, resetPage = false } = {}) {
    // 筛选条件换了就该从第 1 页看起；普通刷新（含轮询）保持当前页
    if (resetPage) page = 1;
    // 兜一次间隔配置：冷启动时首次读取可能撞上「后端还没起来」而失败，
    // 那时会把兜底值一直用下去（同步过一次就立刻返回，无额外开销）
    void syncAutoRefresh();
    try {
      const [result, nextStats] = await Promise.all([
        api.getLogs(queryParams()),
        api.getLogStats(),
      ]);
      // 重写 innerHTML 会把列表弹回顶部：轮询刷新时把读到的位置还回去，
      // 否则每隔一个刷新周期就把正在看日志的人踢回页首。换筛选 / 页码被夹回则一律回顶。
      const pageBefore = page;
      const keepTop = resetPage ? 0 : ($('log-list')?.scrollTop || 0);
      if (result) fillCategories(result.categories);
      render(result);
      setListScroll('log-list', page === pageBefore ? keepTop : 0);
      applyStats(nextStats);
    } catch (error) {
      if (!silent) {
        console.warn('读取运行日志失败:', error.message);
        render(null);
      }
    }
  }

  /**
   * 起 / 重起轮询定时器。
   *
   * 三个前置条件缺一不可：任务已开启（`autoEnabled`）、间隔为正、
   * 页面可见时才有意义。任务被关掉时不排定时器（而不是排一个永不触发的），
   * 否则「关掉了但定时器还在跑」会让「间隔改了却像没生效」变得难排查。
   */
  function startAuto() {
    stopAuto();
    if (!autoEnabled || autoRefreshMs <= 0) return;
    timer = setInterval(() => {
      // 只在日志页可见时轮询，避免后台无谓请求
      if (document.hidden || wbApp.currentPage !== 'logs') return;
      // 上一轮还没回来就跳过这一拍（见 `polling` 的说明）
      if (polling) return;
      polling = true;
      void load({ silent: true }).finally(() => { polling = false; });
    }, autoRefreshMs);
  }

  function stopAuto() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  /**
   * 应用「定时任务」页推来的新配置（也用于启动时自读，见 `syncAutoRefresh`）。
   *
   * `task` 的形状 = `/api/scheduled-tasks` 里的一条（`{enabled, interval, unit}`）。
   * 传 null / 形状不符时退回默认值 —— 界面不该因为一个读不到的配置就
   * 完全停止刷新（那看起来像功能坏了）。
   */
  function applyAutoRefresh(task) {
    // 配置已经由推送方给过，标上已同步：待重试的自读就不必再跑（更糟的是，
    // 那次读若失败会把用户刚在定时任务页改好的间隔覆盖回兜底值）
    autoSynced = true;
    const interval = Number(task?.interval);
    const valid = task && typeof task === 'object'
      && Number.isFinite(interval) && interval > 0
      && (task.unit === 'seconds' || task.unit === 'minutes');
    if (!valid) {
      autoEnabled = true;
      autoRefreshMs = DEFAULT_AUTO_REFRESH_MS;
    } else {
      autoEnabled = task.enabled !== false;
      autoRefreshMs = task.unit === 'minutes' ? interval * 60_000 : interval * 1000;
    }
    startAuto();
  }

  /**
   * 启动时自己拉一次配置。
   *
   * 为什么本面板要自己拉而不是等「定时任务」页推：用户完全可能直接打开日志页
   * （上次停留的页），而从未进过定时任务页 —— 那样推的动作永远不会发生，
   * 间隔就一直是兜底值。加载顺序上本文件排在 tasks-panel.js 之前，
   * 所以这里用自己的接口调用，不依赖对方已就绪。
   *
   * 读取失败**不**标记为已同步，于是切回日志页时（`load` 里的重试）会再来一次
   * —— 首次读取失败最常见的原因就是「后端还没起来」（冷启动），
   * 一次失败就永久用兜底值，用户会以为「我在定时任务页改的间隔没生效」。
   */
  async function syncAutoRefresh() {
    if (autoSynced) return;
    try {
      const list = await api.getScheduledTasks();
      const task = (list?.tasks || []).find(item => item.id === 'logsAutoRefresh');
      applyAutoRefresh(task || null);
      // 请求成功就标记同步过（哪怕这一条不在清单里 —— 那是后端版本旧，
      // 再重试也不会有，标记下来免得每次切页面都白跑一次请求）
      autoSynced = Array.isArray(list?.tasks);
    } catch (error) {
      // 读不到就用兜底值继续跑（见 applyAutoRefresh 的说明），下次切进本页再试
      console.warn('读取日志自动刷新间隔失败，按默认 10 秒:', error.message);
      applyAutoRefresh(null);
    }
  }

  // ─── 操作 ──────────────────────────────────

  /**
   * 翻页只重绘：整窗口的数据已经在 `current.entries` 里，不必再打一次接口。
   *
   * 条数直接取 `current.entries`（改造前取的是「经前端过滤后」的那份 `filtered`；
   * 那层过滤随「不看脱敏」开关一起移除了，见模块头）。
   */
  function gotoPage(target) {
    const entries = Array.isArray(current?.entries) ? current.entries : [];
    const pageCount = Math.max(1, Math.ceil(entries.length / PAGE_SIZE));
    const next = Math.min(Math.max(1, target), pageCount);
    if (next === page) return;
    page = next;
    render();
    setListScroll('log-list');   // 新一页从顶部开始读，否则会停在上一页的滚动位置
  }

  /** 列表滚动定位：不带参数即回顶部；元素缺失时静默跳过 */
  function setListScroll(id, top = 0) {
    const list = $(id);
    if (list) list.scrollTop = top;
  }

  async function guard(button, label, action) {
    if (panelBusy) return;
    panelBusy = true;
    const original = button?.textContent;
    if (button) { button.disabled = true; if (label) button.textContent = label; }
    try {
      await action();
    } catch (error) {
      toast(`操作失败：${error.message}`, 'err');
    } finally {
      panelBusy = false;
      if (button) { button.disabled = false; if (label) button.textContent = original; }
    }
  }

  /**
   * 清空用的筛选参数：与 queryParams 同一套条件（不含 limit）。
   * 「不看脱敏」不参与 —— 那是纯前端的显示开关，不是后端筛选条件；
   * 清空永远按后端筛选口径执行，页面上被隐藏的命中条目也会一并清掉。
   *
   * 返回**查询串**（与 queryParams 一致）而不是 URLSearchParams 对象：
   * 桥接层的 toQuery 只认字符串 / 普通对象，传对象会静默变成「没有参数」，
   * 那样筛选清空就变成了全清 —— 这个 bug 已经踩过一次，别再踩。
   */
  function clearParams() {
    const params = new URLSearchParams();
    const level = $('logs-level')?.value;
    const category = $('logs-category')?.value;
    const keyword = $('logs-keyword')?.value.trim();
    const start = rangeStart(eventRange);
    if (level) params.set('level', level);
    if (category) params.set('category', category);
    if (keyword) params.set('keyword', keyword);
    if (start !== null) params.set('start', String(start));
    return params.toString();
  }

  async function clearLogs() {
    const query = clearParams();
    // 提示语按「有没有筛选」分开说：带筛选删的是筛选结果，不带筛选才是全清 ——
    // 用户必须知道即将删掉的是哪一批。条数用后端 matched（含被「不看脱敏」
    // 藏起来的命中项，它们同样会被删，文案不给「比实际少」的数）
    const hasFilters = query.length > 0;
    const message = hasFilters
      ? `确定清空当前筛选出的 <strong>${Number(current?.matched) || 0}</strong> 条日志？清空后无法恢复。`
      : '确定清空<strong>全部</strong>运行日志？清空后无法恢复。';
    // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行（等于没有确认），
    // 危险确认一律走自绘弹窗（wbConfirm，见 confirm-dialog.js）
    if (!(await window.wbConfirm?.ask?.({
      title: '清空运行日志',
      html: message,
      okText: '清空',
      okClass: 'danger',
    }))) return;
    await guard($('btn-logs-clear'), '清空中…', async () => {
      // 无筛选时显式带 all=1：后端要求「清空全部」必须显式声明，
      // 免得哪天参数漏传又被当成全清（前端写错一次就是全部数据没了）
      await api.clearLogs(hasFilters ? query : 'all=1');
      // 清空后没有「当前页」可言：回到第 1 页并把滚动位置一起归零
      await load({ resetPage: true });
      toast('运行日志已清空');
    });
  }

  async function exportLogs() {
    await guard($('btn-logs-export'), '导出中…', async () => {
      const result = await api.exportLogs();
      if (result?.canceled) return;
      if (result?.count === 0) { toast('暂无日志可导出', 'err'); return; }
      toast(`✅ 已导出 ${result.count} 条日志到 ${result.file}`);
    });
  }

  // ─── 事件绑定 ──────────────────────────────

  // 时间档位的默认值（「全部」）写在 HTML 里，存过的值在这里纠正
  $('logs-range')?.querySelectorAll('.seg-item[data-range]').forEach(item => {
    item.classList.toggle('active', item.dataset.range === eventRange);
  });

  $('logs-range')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-range]');
    if (!item) return;
    const next = RANGES.includes(item.dataset.range) ? item.dataset.range : DEFAULT_RANGE;
    if (next === eventRange) return;
    eventRange = next;
    persistRange(EVENT_RANGE_KEY, next);
    $('logs-range')?.querySelectorAll('.seg-item[data-range]').forEach(node => {
      node.classList.toggle('active', node.dataset.range === next);
    });
    void load({ resetPage: true });
  });

  $('btn-logs-clear').addEventListener('click', clearLogs);
  $('btn-logs-export').addEventListener('click', exportLogs);
  $('btn-logs-prev').addEventListener('click', () => gotoPage(page - 1));
  $('btn-logs-next').addEventListener('click', () => gotoPage(page + 1));
  // 级别 / 分类 / 关键词都会换掉结果集，页码必须回到第 1 页，否则停的位置没有意义
  $('logs-level').addEventListener('change', () => load({ resetPage: true }));
  $('logs-category').addEventListener('change', () => load({ resetPage: true }));
  // 关键词输入做防抖，避免每敲一个字就打一次接口
  let keywordTimer = null;
  $('logs-keyword').addEventListener('input', () => {
    clearTimeout(keywordTimer);
    keywordTimer = setTimeout(() => load({ resetPage: true }), 300);
  });
  /**
   * 从别的页面跳转过来并把分类筛选预设好（定时任务页「查看签到日志」按钮用）。
   *
   * 分类下拉的选项由后端字典填充、且只填一次（见 fillCategories）——
   * 用户可能还没打开过日志页，这里先确保下拉已就位（没就位就先跑一次加载，
   * 它会顺带填充），再设值并按新分类重拉。最后切页面交给调用方之外的
   * showPage：本方法只负责「页面加载完就是这个筛选」，切页由任务页自己调
   * wbApp.showPage（见 tasks-panel 的按钮处理）—— 但为了这个按钮一次点击
   * 就到位，这里把切页也包进来（showPage 在 app.js 的 wbApp 上）。
   */
  async function showCategory(category) {
    const select = $('logs-category');
    if (!select) return;
    if (select.dataset.filled !== '1' || !select.querySelector(`option[value="${category}"]`)) {
      await load({ silent: true });
    }
    if (!$('logs-category').querySelector(`option[value="${category}"]`)) return;
    $('logs-category').value = category;
    // 程序赋值不派发 change，增强外壳（select.js）的触发器文本也不会自己跟上
    // —— 与 models-panel.js 设置映射弹窗下拉后显式 sync 同一个既有模式
    window.wbSelect?.sync?.(select);
    page = 1;
    await load({ resetPage: true });
    window.wbApp?.showPage?.('logs', { persist: true });
  }

  window.wbLogsPanel = {
    load,
    render,
    lastStats: () => stats,
    // 「定时任务」页改完间隔后推给本面板（见 applyAutoRefresh 的说明）
    applyAutoRefresh,
    // 跳转入口：预设分类并切页（目前只有「查看签到日志」用）
    showCategory,
  };

  // 首屏自持加载：即便 app.js 的 refresh 失败，日志页也能独立显示真实状态。
  // 自动刷新的间隔先按兜底值起一次（页面立刻有轮询），同时异步读配置校准 ——
  // 不 await：一次本地接口调用不该拖住首屏，读到后 applyAutoRefresh 会重启定时器。
  void load({ silent: true });
  startAuto();
  void syncAutoRefresh();
})();
