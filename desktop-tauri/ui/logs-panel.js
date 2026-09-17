/* WorkBuddy 本地代理 · 运行日志面板（系统事件 / 筛选 / 分页 / 导出 / 清空） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的日志面板模块：自持筛选条件、页码与轮询定时器。
 *
 * 与 desensitize-panel.js 同构：依赖 window.wbApp 的 esc / toast / updateLogsBadge，
 * 通过 window.wbLogsPanel 暴露 load 给 app.js（切到日志页时立即刷新）。
 *
 * 过滤与分页都放在前端：接口一次拉满后端上限（500 条，恰好也是内存里保留的条数上限），
 * 之后翻页、切「不看脱敏」都不再打接口 —— 交互即时，也不会和 10 秒轮询抢状态。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast, updateLogsBadge } = wbApp;

  const AUTO_REFRESH_MS = 10_000;
  /** 每页条数 */
  const PAGE_SIZE = 50;
  /** 单次拉取上限（后端上限）：一次拿全，总页数才对得上真实结果 */
  const FETCH_LIMIT = 500;
  /** 「不看脱敏」的持久化键：读不到 / 存储不可用时都按默认「隐藏」处理。
   *  前缀沿用项目既有的 workbuddy-desktop-*（主题、日志已读标记用的是同一套） */
  const HIDE_KEY = 'workbuddy-desktop-logs-hide-desensitize';
  const CAT_DESENSITIZE = 'desensitize';

  let panelBusy = false;
  let current = null;      // 最近一次查询结果
  let stats = null;        // 最近一次统计（导航徽标用）
  let timer = null;
  let page = 1;            // 当前页码（1 起，指向过滤后的结果）
  let filtered = [];       // 最近一次结果经「不看脱敏」过滤后的条目

  // ─── 开关状态 ──────────────────────────────

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

  /** 开关的当前值以内存为准：localStorage 只负责跨次启动恢复 */
  let hideOn = readHide();

  // ─── 渲染 ──────────────────────────────────

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
   * 「不看脱敏」这层只能在前端叠加：级别 / 分类 / 关键词都由后端过滤，
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
      list.innerHTML = `<div class="log-empty">${emptyText(total, matched, hidden)}</div>`;
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
    if (level) params.set('level', level);
    if (category) params.set('category', category);
    if (keyword) params.set('keyword', keyword);
    params.set('limit', String(FETCH_LIMIT));
    return params.toString();
  }

  async function load({ silent = false, resetPage = false } = {}) {
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
      setListScroll(page === pageBefore ? keepTop : 0);
      stats = nextStats || null;
      updateLogsBadge?.(stats);
    } catch (error) {
      if (!silent) {
        console.warn('读取运行日志失败:', error.message);
        render(null);
      }
    }
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

  // ─── 操作 ──────────────────────────────────

  /** 翻页只重绘：整窗口的数据已经在 filtered 里，不必再打一次接口 */
  function gotoPage(target) {
    const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
    const next = Math.min(Math.max(1, target), pageCount);
    if (next === page) return;
    page = next;
    render();
    setListScroll();   // 新一页从顶部开始读，否则会停在上一页的滚动位置
  }

  /** 列表滚动定位：不带参数即回顶部；元素缺失时静默跳过 */
  function setListScroll(top = 0) {
    const list = $('log-list');
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
    setListScroll();
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
