/* Agent2API · 系统事件日志面板（筛选 / 分页 / 导出 / 清空） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的日志面板模块：自持筛选条件、页码与轮询定时器。
 * 网关的模型请求明细已拆到「请求日志」页（requests-panel.js），本文件只管系统事件。
 *
 * 与 desensitize-panel.js 同构：依赖 window.wbApp 的 esc / toast / updateLogsBadge，
 * 通过 window.wbLogsPanel 暴露 load 给 app.js（切到日志页时立即刷新）。
 *
 * ── 数据口径 ────────────────────────────────────────────────
 * 系统事件（GET /api/logs）：内存里最多 500 条，一次拉满后端上限即可，
 * 翻页与「不看脱敏」都在前端算 —— 交互即时，也不会和轮询抢状态。
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
   * 兜底值 10 秒 = 改造前的硬编码值：后端拿不到配置时行为与改造前一致。
   */
  const DEFAULT_AUTO_REFRESH_MS = 10_000;
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

  /** 事件日志每页条数 */
  const PAGE_SIZE = 50;
  /** 系统事件单次拉取上限（后端上限）：一次拿全，总页数才对得上真实结果 */
  const FETCH_LIMIT = 500;
  /** 「不看脱敏」的持久化键：读不到 / 存储不可用时都按默认「隐藏」处理。
   *  前缀沿用项目既有的 workbuddy-desktop-*（主题、日志已读标记用的是同一套） */
  const HIDE_KEY = 'workbuddy-desktop-logs-hide-desensitize';
  /** 事件日志时间范围的持久化键（请求日志页的档位在 requests-panel.js，两键独立） */
  const EVENT_RANGE_KEY = 'workbuddy-desktop-logs-range';
  const CAT_DESENSITIZE = 'desensitize';

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
  let page = 1;            // 事件日志当前页码（1 起，指向过滤后的结果）
  let filtered = [];       // 事件日志最近一次结果经「不看脱敏」过滤后的条目

  // ─── 开关与档位状态 ──────────────────────────

  /** 只有明确存过 '0' 才算关闭；其余情况（无值 / 读取抛错）都是默认开启 */
  function readHide() {
    try {
      return localStorage.getItem(HIDE_KEY) !== '0';
    } catch {
      return true;
    }
  }

  function persistHide(enabled) {
    try {
      localStorage.setItem(HIDE_KEY, enabled ? '1' : '0');
    } catch {
      // 存储不可用时只影响下次打开，不影响本次会话内的表现
    }
  }

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
      // 同 persistHide：存储不可用不该影响本次会话
    }
  }

  /** 开关与档位的当前值以内存为准：localStorage 只负责跨次启动恢复 */
  let hideOn = readHide();
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

  /** 秒级时间：日志看的是“刚刚发生”，不需要日期 */
  function formatLogTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    const pad = n => String(n).padStart(2, '0');
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
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
   * 「不看脱敏」这层只能在前端叠加：级别 / 分类 / 关键词 / 时间区间都由后端过滤，
   * 后端不认识这个开关，也不该为它加参数。过滤放在分页之前，总页数才是对的。
   */
  function localFilter(entries) {
    if (!hideOn) return { rows: entries, hidden: 0 };
    const rows = [];
    let hidden = 0;
    for (const entry of entries) {
      if (entry.category === CAT_DESENSITIZE) hidden += 1;
      else rows.push(entry);
    }
    return { rows, hidden };
  }

  /**
   * 徽标：正常情况下沿用「N 条 / M / N 条」，
   * 一旦藏过日志就把隐藏条数一并写出，否则用户会以为日志丢了。
   *
   * 分母有讲究：带隐藏时改成「命中条数」，写总数会让人以为丢了 400 条，
   * 而括号里只解释了其中几条 —— 「97 / 100 条（已隐藏 3 条脱敏）」才是自洽的读数。
   * 「要不要分母」仍按原口径（是否筛过）决定，无筛选时给出「498 条（已隐藏 2 条脱敏）」。
   */
  function renderBadge(badge, { total, matched, hidden }) {
    if (!badge) return;
    const visible = Math.max(0, matched - hidden);
    const denom = hidden ? matched : total;
    const base = matched === total ? `${visible} 条` : `${visible} / ${denom} 条`;
    badge.className = 'badge';
    badge.textContent = hidden ? `${base}（已隐藏 ${hidden} 条脱敏）` : base;
  }

  function renderPager(pageCount) {
    const info = $('logs-page-info');
    if (info) info.textContent = `第 ${page} / ${pageCount} 页`;
    const prev = $('btn-logs-prev');
    const next = $('btn-logs-next');
    if (prev) prev.disabled = page <= 1;
    if (next) next.disabled = page >= pageCount;
  }

  /** 空态文案沿用原口径，只把「全被开关藏起来」单独说明，免得看着像筛选出错 */
  function emptyText(total, matched, hidden) {
    if (!total) return '暂无日志';
    if (matched && hidden >= matched) return `已按「不看脱敏」隐藏 ${hidden} 条日志，取消勾选即可查看`;
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

    const { rows, hidden } = localFilter(entries);
    filtered = rows;

    // 总页数按过滤后的条数算；日志被清空或筛选变严后页码会越界，回落到最后一页
    const pageCount = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
    page = Math.min(Math.max(1, page), pageCount);

    renderBadge($('logs-badge'), { total, matched, hidden });
    if (current?.file) $('logs-file').textContent = current.file;
    renderPager(pageCount);

    const start = (page - 1) * PAGE_SIZE;
    const pageRows = rows.slice(start, start + PAGE_SIZE);
    if (!pageRows.length) {
      list.innerHTML = `<div class="log-empty">${esc(emptyText(total, matched, hidden))}</div>`;
      return;
    }

    list.innerHTML = pageRows.map(rowHtml).join('');
  }

  /** 分类下拉选项由后端返回的字典填充（只填一次；含「脱敏」，关掉开关后可专门看它） */
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
      // 否则每 10 秒就把正在看日志的人踢回页首。换筛选 / 页码被夹回则一律回顶。
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
      void load({ silent: true });
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

  /** 翻页只重绘：整窗口的数据已经在 filtered 里，不必再打一次接口 */
  function gotoPage(target) {
    const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
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

  async function clearLogs() {
    if (!confirm('确定清空运行日志？清空后无法恢复。')) return;
    await guard($('btn-logs-clear'), '清空中…', async () => {
      await api.clearLogs();
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

  // 开关默认勾选写在 HTML 里，只有存过「关闭」才在首屏改掉（避免先亮后灭的闪动）
  const hideBox = $('logs-hide-desensitize');
  if (hideBox) hideBox.checked = hideOn;

  // 时间档位的默认值（「全部」）也写在 HTML 里，存过的值在这里纠正
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

  $('btn-logs-refresh').addEventListener('click', () => load().then(() => toast('日志已刷新')));
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
  hideBox?.addEventListener('change', event => {
    hideOn = event.target.checked;
    persistHide(hideOn);
    page = 1;
    render();   // 纯前端过滤，改完重绘即可
    setListScroll('log-list');
    toast(hideOn ? '已隐藏脱敏日志' : '已显示全部日志');
  });
  window.wbLogsPanel = {
    load,
    render,
    lastStats: () => stats,
    // 「定时任务」页改完间隔后推给本面板（见 applyAutoRefresh 的说明）
    applyAutoRefresh,
  };

  // 首屏自持加载：即便 app.js 的 refresh 失败，日志页也能独立显示真实状态。
  // 自动刷新的间隔先按兜底值起一次（页面立刻有轮询），同时异步读配置校准 ——
  // 不 await：一次本地接口调用不该拖住首屏，读到后 applyAutoRefresh 会重启定时器。
  void load({ silent: true });
  startAuto();
  void syncAutoRefresh();
})();
