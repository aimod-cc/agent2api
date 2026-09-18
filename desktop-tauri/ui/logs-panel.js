/* WorkBuddy 本地代理 · 日志面板（系统事件 / 模型请求 · 筛选 / 分页 / 导出 / 清空） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的日志面板模块：自持两个视图、各自筛选条件、页码与轮询定时器。
 *
 * 与 desensitize-panel.js 同构：依赖 window.wbApp 的 esc / toast / updateLogsBadge，
 * 通过 window.wbLogsPanel 暴露 load 给 app.js（切到日志页时立即刷新）。
 *
 * ── 两个视图的数据口径不同，所以分页也用了两种做法 ────────────────
 *   · 系统事件（GET /api/logs）：内存里最多 500 条，一次拉满后端上限即可，
 *     翻页与「不看脱敏」都在前端算 —— 交互即时，也不会和 10 秒轮询抢状态。
 *   · 模型请求（GET /api/stats/requests）：明细按保留期（默认 30 天）存盘，
 *     条数没有上限，一次拉全不现实，所以走后端 offset/limit 真分页；
 *     每页 50 条（后端默认值，这里显式传，页数才可算），上限 500 条由后端夹紧。
 * 两边的筛选与页码各自独立，切视图互不影响；共用页头的「自动刷新」与一次 load() 入口。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast, updateLogsBadge } = wbApp;

  const AUTO_REFRESH_MS = 10_000;
  /** 事件日志每页条数 */
  const PAGE_SIZE = 50;
  /** 系统事件单次拉取上限（后端上限）：一次拿全，总页数才对得上真实结果 */
  const FETCH_LIMIT = 500;
  /** 请求明细每页条数：与后端 DEFAULT_LIMIT 一致 */
  const REQ_PAGE_SIZE = 50;
  /** 「不看脱敏」的持久化键：读不到 / 存储不可用时都按默认「隐藏」处理。
   *  前缀沿用项目既有的 workbuddy-desktop-*（主题、日志已读标记用的是同一套） */
  const HIDE_KEY = 'workbuddy-desktop-logs-hide-desensitize';
  /** 两个视图的时间范围各自持久化：需求要求状态互不影响，共用一键会让两边的档位互相踩 */
  const EVENT_RANGE_KEY = 'workbuddy-desktop-logs-range';
  const REQ_RANGE_KEY = 'workbuddy-desktop-logs-requests-range';
  const CAT_DESENSITIZE = 'desensitize';

  /** 合法的时间档位与后端 /api/stats/summary 的白名单同字面量（报表页也是这一组）。
   *  默认「全部」而不是报表页的 7 天：日志页原先没有任何时间条件，
   *  加筛选时默认必须落在「行为不变」的那一档上 */
  const RANGES = ['today', '7', '30', 'month', 'all'];
  const DEFAULT_RANGE = 'all';
  const RANGE_LABEL = { today: '今天', 7: '近 7 天', 30: '近 30 天', month: '本月', all: '全部' };
  const VIEWS = ['events', 'requests'];
  const DEFAULT_VIEW = 'events';

  let view = DEFAULT_VIEW;
  let panelBusy = false;
  let current = null;      // 最近一次事件日志查询结果
  let stats = null;        // 最近一次统计（导航徽标用）
  let timer = null;
  let page = 1;            // 事件日志当前页码（1 起，指向过滤后的结果）
  let filtered = [];       // 事件日志最近一次结果经「不看脱敏」过滤后的条目

  // 请求明细的独立状态：offset 由后端分页决定，不用前端页码
  let reqEntries = [];
  let reqTotal = 0;        // 明细总量（未过滤）
  let reqMatched = 0;      // 命中筛选条件的条数
  let reqOffset = 0;
  let reqSeq = 0;          // 请求序号：连点翻页时只认最新一次响应

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
  let reqRange = readRange(REQ_RANGE_KEY);

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

  // ─── 请求明细：渲染 ──────────────────────────

  /** 表头是渲染出来的一部分（不是常驻节点）：空态时列表里只有一条 .log-empty，
   *  才能命中「唯一子元素居中」那条规则，空态观感与事件日志一致 */
  const REQ_HEAD = `<div class="req-head">
      <span class="req-time">时间</span>
      <span>模型</span>
      <span>账号</span>
      <span>状态</span>
      <span class="req-dur">耗时</span>
      <span class="req-tok">Token</span>
    </div>`;

  /** 明细跨天（最多 30 天），只给时分秒会分不清哪天，所以带上月日 */
  function formatReqTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    const pad = n => String(n).padStart(2, '0');
    return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }

  /** 耗时：秒以内给毫秒（本地请求常常只有几百毫秒，秒制的 0.3 反而看不出差别） */
  function formatDuration(ms) {
    const value = Number(ms) || 0;
    if (value < 1000) return `${value}ms`;
    if (value < 60_000) return `${(value / 1000).toFixed(1)}s`;
    return `${Math.floor(value / 60_000)}分${Math.round((value % 60_000) / 1000)}秒`;
  }

  /** Token 数：一万以内给精确千分位，超过才缩写 —— 与报表页同一口径（那里也是这么分的） */
  function formatTokens(value) {
    const num = Number(value) || 0;
    if (num < 10_000) return num.toLocaleString('zh-CN');
    if (num < 1_000_000) return `${Number((num / 1000).toFixed(1))}k`;
    return `${Number((num / 1_000_000).toFixed(2))}M`;
  }

  function reqRowHtml(entry) {
    const status = Number(entry.status) || 0;
    const ok = status >= 200 && status < 300;
    const attempts = Number(entry.attempts) || 1;
    const tokens = Number(entry.totalTokens) || 0;
    const cache = Number(entry.cacheReadTokens) || 0;
    // 账号为空 = 请求在选定账号之前就失败了（后端契约：无账号时给空串），
    // 这种行的「失败」多半不是上游拒绝，所以账号列必须显式给破折号而不是留空
    const account = entry.accountName
      ? `<span class="req-acct" title="${esc(entry.accountId || entry.accountName)}">${esc(entry.accountName)}</span>`
      : '<span class="req-acct is-empty" title="请求在任何账号接手之前就失败了">—</span>';
    // 状态列直接用项目既有的 .badge.tag（成功 / 危险两档），不另造一套状态标签。
    // status 为 0 是后端契约里的「还没发出请求就失败」，直接写 0 会被读成 HTTP 状态码，
    // 所以退成「失败」两个字
    const statusCell = `<span><span class="badge tag ${ok ? 'ok' : 'bad'}">${status || '失败'}</span></span>`;
    // 重试次数只有 >1 才有信息量，成功一次就完成的请求不必多挂一个「×1」
    const duration = `${esc(formatDuration(entry.durationMs))}${attempts > 1 ? `<span class="req-sub">×${attempts} 次</span>` : ''}`;
    // 缓存读取是 Token 的附加说明（明细里能看出「这次省了多少」），为 0 时不写
    const token = `${esc(formatTokens(tokens))}${cache ? `<span class="req-sub">缓存 ${esc(formatTokens(cache))}</span>` : ''}`;
    return `<div class="req-row${ok ? '' : ' failed'}">
        <span class="req-time">${esc(formatReqTime(entry.ts))}</span>
        <span class="req-model" title="${esc(entry.model)}">${esc(entry.model || '—')}</span>
        ${account}
        ${statusCell}
        <span class="req-num req-dur">${duration}</span>
        <span class="req-num req-tok">${token}</span>
        ${entry.error ? `<span class="req-error">${esc(entry.error)}</span>` : ''}
      </div>`;
  }

  function reqEmptyText() {
    if (!reqTotal) return '暂无请求明细，网关还没有转发过请求';
    return '没有符合筛选条件的请求';
  }

  /** 页脚读数：范围与状态都写出来，「为什么只有这几条」一眼可查 */
  function renderReqSummary() {
    const box = $('req-summary');
    if (!box) return;
    const status = $('req-status')?.value;
    const parts = [RANGE_LABEL[reqRange] || '全部'];
    if (status === 'ok') parts.push('只看成功');
    else if (status === 'error') parts.push('只看失败');
    box.textContent = parts.join(' · ');
  }

  function renderReqBadge() {
    const badge = $('req-badge');
    if (!badge) return;
    badge.className = 'badge';
    badge.textContent = reqMatched === reqTotal
      ? `${reqTotal} 条`
      : `${reqMatched} / ${reqTotal} 条`;
  }

  function renderReqPager() {
    const pageCount = Math.max(1, Math.ceil(reqMatched / REQ_PAGE_SIZE));
    const currentPage = Math.min(pageCount, Math.floor(reqOffset / REQ_PAGE_SIZE) + 1);
    const info = $('req-page-info');
    if (info) info.textContent = `第 ${currentPage} / ${pageCount} 页`;
    const prev = $('btn-req-prev');
    const next = $('btn-req-next');
    if (prev) prev.disabled = reqOffset <= 0;
    if (next) next.disabled = reqOffset + REQ_PAGE_SIZE >= reqMatched;
  }

  /**
   * 渲染请求明细。errorText 有值时列表位置显示错误文案，**不动**计数与页码 ——
   * 那组读数是上一次成功加载的结果，写 0 会让人以为明细被删了；
   * 徽标退成「—」表示「现在这个读数不可信」，比给一个假数字诚实。
   */
  function renderRequests(errorText) {
    const list = $('req-list');
    if (!list) return;
    if (errorText) {
      const badge = $('req-badge');
      if (badge) { badge.className = 'badge'; badge.textContent = '—'; }
      list.innerHTML = `<div class="log-empty">${esc(errorText)}</div>`;
      return;
    }
    renderReqBadge();
    renderReqSummary();
    renderReqPager();
    if (!reqEntries.length) {
      list.innerHTML = `<div class="log-empty">${esc(reqEmptyText())}</div>`;
      return;
    }
    list.innerHTML = REQ_HEAD + reqEntries.map(reqRowHtml).join('');
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

  function reqParams() {
    const params = new URLSearchParams();
    const start = rangeStart(reqRange);
    const status = $('req-status')?.value;
    if (start !== null) params.set('start', String(start));
    if (status) params.set('status', status);
    params.set('offset', String(reqOffset));
    params.set('limit', String(REQ_PAGE_SIZE));
    return params.toString();
  }

  /** 导航徽标的数据源与当前视图无关：未读数只统计系统事件，两个视图都要顺手刷新它 */
  function applyStats(nextStats) {
    stats = nextStats || null;
    updateLogsBadge?.(stats);
  }

  async function loadEvents({ silent = false, resetPage = false } = {}) {
    // 筛选条件换了就该从第 1 页看起；普通刷新（含轮询）保持当前页
    if (resetPage) page = 1;
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

  async function loadRequests({ silent = false, resetPage = false } = {}) {
    if (resetPage) reqOffset = 0;
    // 连点翻页时不做互斥锁，只认最后一次响应：用锁会把后面的点击直接吞掉
    const token = ++reqSeq;
    try {
      const [result, nextStats] = await Promise.all([
        api.getStatsRequests(reqParams()),
        api.getLogStats(),
      ]);
      if (token !== reqSeq) return;
      reqEntries = Array.isArray(result?.entries) ? result.entries : [];
      reqTotal = Number(result?.total) || 0;
      reqMatched = Number(result?.matched) || 0;
      // 明细被清空或被保留期裁掉后，停在第 5 页会看到一片空白：
      // 先把 offset 夹回最后一页再取一次。夹完必然落在合法页（lastOffset 是
      // 本次响应算出来的），所以不会来回递归；用 return 把这次重取并进同一个 Promise，
      // 调用方（刷新按钮）的 then 才会等到真正拿到数据之后才弹提示。
      const lastOffset = Math.max(0, (Math.ceil(reqMatched / REQ_PAGE_SIZE) - 1) * REQ_PAGE_SIZE);
      if (reqOffset > lastOffset) {
        reqOffset = lastOffset;
        return loadRequests({ silent });
      }
      renderRequests();
      applyStats(nextStats);
    } catch (error) {
      // 静默（轮询）时保留上一屏数据，只当没刷过：把读数清成 0 会让人以为明细被删了
      if (!silent) {
        console.warn('读取请求明细失败:', error.message);
        renderRequests('读取请求明细失败，详见控制台');
      }
    }
  }

  /** 唯一入口：切页（app.js）、刷新按钮、轮询都走这里，内部再按当前视图分发 */
  function load(options = {}) {
    return view === 'requests' ? loadRequests(options) : loadEvents(options);
  }

  function startAuto() {
    stopAuto();
    if (!$('logs-auto')?.checked) return;
    timer = setInterval(() => {
      // 只在日志页可见时轮询，避免后台无谓请求
      if (document.hidden || wbApp.currentPage !== 'logs') return;
      void load({ silent: true });
    }, AUTO_REFRESH_MS);
  }

  function stopAuto() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  // ─── 视图切换 ──────────────────────────────

  function syncViewButtons() {
    document.querySelectorAll('#logs-view .seg-item[data-view]').forEach(item => {
      item.classList.toggle('active', item.dataset.view === view);
    });
  }

  /** 两个面板整体 hidden：事件日志的列表要吃掉剩余高度，用 opacity / visibility
   *  藏起来的话它会继续参与布局，切到请求视图后高度被一个看不见的面板占走 */
  function paintView() {
    const events = $('logs-panel-events');
    const requests = $('logs-panel-requests');
    if (events) events.hidden = view !== 'events';
    if (requests) requests.hidden = view !== 'requests';
    syncViewButtons();
  }

  /**
   * 切视图：页码与筛选一律保留（两边状态本来就各自独立），但数据重新拉一次 ——
   * 待在另一个视图期间，轮询刷的是那个视图，本视图的数据已经落后了。
   *
   * 视图本身不落 localStorage：需求只要求两侧的**筛选状态**独立持久化，
   * 视图归属没提；进日志页默认落在「系统事件」与导航未读徽标（只统计事件日志）
   * 的口径一致，而它又占了两侧筛选之外的第三个状态位，多存一个键不值得。
   */
  function setView(next) {
    const value = VIEWS.includes(next) ? next : DEFAULT_VIEW;
    if (value === view) return;
    view = value;
    paintView();
    void load({ silent: true });
  }

  /** 切时间档位：写内存 + 落盘 + 重绘按钮 + 重新查询（页码回到第 1 页）。
   *  两个视图的档位各走各的加载函数，不经过 load() 的分发 —— 控件与数据源一一对应，
   *  将来给某一个视图加筛选时也不会因为「当前视图」这个中间变量而串到另一边。 */
  function setRange(target, value) {
    const requests = target === 'requests';
    const next = RANGES.includes(value) ? value : DEFAULT_RANGE;
    if (requests) {
      if (next === reqRange) return;
      reqRange = next;
    } else {
      if (next === eventRange) return;
      eventRange = next;
    }
    persistRange(requests ? REQ_RANGE_KEY : EVENT_RANGE_KEY, next);
    const box = requests ? $('req-range') : $('logs-range');
    box?.querySelectorAll('.seg-item[data-range]').forEach(item => {
      item.classList.toggle('active', item.dataset.range === next);
    });
    void (requests ? loadRequests({ resetPage: true }) : loadEvents({ resetPage: true }));
  }

  function syncRangeButtons() {
    const paint = (box, value) => {
      box?.querySelectorAll('.seg-item[data-range]').forEach(item => {
        item.classList.toggle('active', item.dataset.range === value);
      });
    };
    paint($('logs-range'), eventRange);
    paint($('req-range'), reqRange);
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

  /** 请求明细翻页要真打接口（offset 是后端口径），到边界直接不发请求 */
  function gotoReqPage(target) {
    const pageCount = Math.max(1, Math.ceil(reqMatched / REQ_PAGE_SIZE));
    const next = Math.min(Math.max(1, target), pageCount);
    const offset = (next - 1) * REQ_PAGE_SIZE;
    if (offset === reqOffset) return;
    reqOffset = offset;
    setListScroll('req-list');
    void load();
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
  syncRangeButtons();
  paintView();

  $('logs-view')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-view]');
    if (item) setView(item.dataset.view);
  });
  $('logs-range')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-range]');
    if (item) setRange('events', item.dataset.range);
  });
  $('req-range')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-range]');
    if (item) setRange('requests', item.dataset.range);
  });

  $('btn-logs-refresh').addEventListener('click', () => load().then(() => toast('日志已刷新')));
  $('btn-logs-clear').addEventListener('click', clearLogs);
  $('btn-logs-export').addEventListener('click', exportLogs);
  $('btn-logs-prev').addEventListener('click', () => gotoPage(page - 1));
  $('btn-logs-next').addEventListener('click', () => gotoPage(page + 1));
  $('btn-req-refresh')?.addEventListener('click', () => load().then(() => toast('请求明细已刷新')));
  $('btn-req-prev')?.addEventListener('click', () => gotoReqPage(Math.floor(reqOffset / REQ_PAGE_SIZE)));
  $('btn-req-next')?.addEventListener('click', () => gotoReqPage(Math.floor(reqOffset / REQ_PAGE_SIZE) + 2));
  // 级别 / 分类 / 关键词 / 状态都会换掉结果集，页码必须回到第 1 页，否则停的位置没有意义
  $('logs-level').addEventListener('change', () => loadEvents({ resetPage: true }));
  $('logs-category').addEventListener('change', () => loadEvents({ resetPage: true }));
  $('req-status')?.addEventListener('change', () => loadRequests({ resetPage: true }));
  // 关键词输入做防抖，避免每敲一个字就打一次接口
  let keywordTimer = null;
  $('logs-keyword').addEventListener('input', () => {
    clearTimeout(keywordTimer);
    keywordTimer = setTimeout(() => loadEvents({ resetPage: true }), 300);
  });
  hideBox?.addEventListener('change', event => {
    hideOn = event.target.checked;
    persistHide(hideOn);
    page = 1;
    render();   // 纯前端过滤，改完重绘即可
    setListScroll('log-list');
    toast(hideOn ? '已隐藏脱敏日志' : '已显示全部日志');
  });
  $('logs-auto').addEventListener('change', event => {
    if (event.target.checked) { startAuto(); toast('已开启日志自动刷新'); }
    else { stopAuto(); toast('已关闭日志自动刷新'); }
  });

  window.wbLogsPanel = {
    load,
    render,
    lastStats: () => stats,
  };

  // 首屏自持加载：即便 app.js 的 refresh 失败，日志页也能独立显示真实状态
  void load({ silent: true });
  startAuto();
})();
