/* WorkBuddy 本地代理 · 运行日志面板（系统事件 / 筛选 / 导出 / 清空） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的日志面板模块：自持筛选条件与轮询定时器。
 *
 * 与 desensitize-panel.js 同构：依赖 window.wbApp 的 esc / toast / updateLogsBadge，
 * 通过 window.wbLogsPanel 暴露 load 给 app.js（切到日志页时立即刷新）。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast, updateLogsBadge } = wbApp;

  const AUTO_REFRESH_MS = 10_000;

  let panelBusy = false;
  let current = null;      // 最近一次查询结果
  let stats = null;        // 最近一次统计（导航徽标用）
  let timer = null;

  // ─── 渲染 ──────────────────────────────────

  /** 秒级时间：日志看的是"刚刚发生"，不需要日期 */
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

  function render(data) {
    if (data !== undefined) current = data;
    const list = $('log-list');
    const badge = $('logs-badge');
    if (!list) return;

    const entries = Array.isArray(current?.entries) ? current.entries : [];
    const total = Number(current?.total) || 0;
    const matched = Number(current?.matched) || 0;

    if (badge) {
      badge.className = 'badge';
      badge.textContent = matched === total
        ? `${total} 条`
        : `${matched} / ${total} 条`;
    }
    if (current?.file) $('logs-file').textContent = current.file;

    if (!entries.length) {
      list.innerHTML = `<div class="log-empty">${total ? '没有符合筛选条件的日志' : '暂无日志'}</div>`;
      return;
    }

    list.innerHTML = entries.map(entry => {
      const extra = extraText(entry);
      return `<div class="log-row ${esc(entry.level)}">
        <span class="time">${esc(formatLogTime(entry.ts))}</span>
        <span class="log-lvl ${esc(entry.level)}">${esc(LEVEL_LABEL[entry.level] || entry.level)}</span>
        <span class="cat">${esc(current.categories?.[entry.category] || entry.category)}</span>
        <span class="msg">${esc(entry.message)}${extra ? ` <span class="extra">— ${esc(extra)}</span>` : ''}</span>
      </div>`;
    }).join('');
  }

  /** 分类下拉选项由后端返回的字典填充（只填一次） */
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
    params.set('limit', '200');
    return params.toString();
  }

  async function load({ silent = false } = {}) {
    try {
      const [result, nextStats] = await Promise.all([
        api.getLogs(queryParams()),
        api.getLogStats(),
      ]);
      if (result) fillCategories(result.categories);
      render(result);
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
      await load();
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

  $('btn-logs-refresh').addEventListener('click', () => load().then(() => toast('日志已刷新')));
  $('btn-logs-clear').addEventListener('click', clearLogs);
  $('btn-logs-export').addEventListener('click', exportLogs);
  $('logs-level').addEventListener('change', () => load());
  $('logs-category').addEventListener('change', () => load());
  // 关键词输入做防抖，避免每敲一个字就打一次接口
  let keywordTimer = null;
  $('logs-keyword').addEventListener('input', () => {
    clearTimeout(keywordTimer);
    keywordTimer = setTimeout(() => load(), 300);
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
