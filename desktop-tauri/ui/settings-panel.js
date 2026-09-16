/* WorkBuddy 本地代理 · 设置面板（启动与托盘 / 账号导入导出 / 网关地址） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的面板模块：自持「应用设置」状态，
 * 通过 window.wbSettingsPanel 暴露 load 给 app.js（切到设置页时加载）。
 *
 * 与 desensitize-panel.js / logs-panel.js 同构，依赖 window.wbApp 的 esc / toast / refresh。
 *
 * 主进程契约（本文件只按此调用，不另造方法名）：
 *   getAppSettings()        → { closeToTray, autostart }
 *   saveAppSettings(patch)  → patch 为全量覆盖：{ closeToTray, autostart }
 *   exportAccounts()        → { canceled } | { count, file? }
 *   importAccounts()        → { canceled } | { total, added, updated, skipped, failed, errors }
 *   getBackendStatus()      → { ready, port }（已存在，用于展示网关地址）
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  let panelBusy = false;
  let current = null; // 最近一次从主进程读到的应用设置

  // ─── 渲染：启动与托盘 ────────────────────────

  function renderSettings(data) {
    if (data !== undefined) current = data;
    const badge = $('settings-badge');
    const tray = $('settings-close-to-tray');
    const autostart = $('settings-autostart');
    if (!badge || !tray || !autostart) return;

    if (!current || typeof current !== 'object') {
      badge.className = 'badge bad';
      badge.textContent = '不可用';
      $('settings-state').textContent = '主进程未返回启动设置，请更新桌面端后重试';
      return;
    }

    // 主进程返回的字段一律按「严格 true」判定，缺字段时按关闭处理（与后端默认值一致）
    tray.checked = current.closeToTray === true;
    autostart.checked = current.autostart === true;
    badge.className = 'badge ok';
    badge.textContent = '已应用';
    $('settings-state').textContent = tray.checked
      ? '关闭窗口时程序不退出，转发继续在后台运行；退出请用托盘菜单'
      : '关闭窗口即退出程序，后台转发随之中断';
  }

  /**
   * 保存返回值归一化：后端按契约应回传完整设置，但只认它是布尔才采纳，
   * 缺字段时沿用本次提交的值，避免把开关误渲染成「关闭」。
   */
  function normalizeSettings(saved, fallback) {
    const result = { ...fallback };
    if (saved && typeof saved === 'object') {
      if (typeof saved.closeToTray === 'boolean') result.closeToTray = saved.closeToTray;
      if (typeof saved.autostart === 'boolean') result.autostart = saved.autostart;
    }
    return result;
  }

  async function loadSettings() {
    if (panelBusy) return;
    try {
      renderSettings(await api.getAppSettings());
    } catch (error) {
      console.warn('读取应用设置失败:', error.message);
      renderSettings(null);
    }
  }

  // ─── 渲染：网关地址 ─────────────────────────

  /** 端口拿不到时统一展示占位符，不让页面因为后端未起而报错 */
  function renderGatewayOffline(text) {
    const badge = $('settings-gateway-badge');
    if (badge) { badge.className = 'badge warn'; badge.textContent = text; }
    $('settings-gateway-base').textContent = '—';
    $('settings-gateway-chat').textContent = '—';
    $('settings-gateway-port').textContent = '—';
  }

  async function loadGateway() {
    try {
      const status = await api.getBackendStatus();
      const port = Number(status?.port) || 0;
      if (!port) { renderGatewayOffline('端口未知'); return; }
      const base = `http://127.0.0.1:${port}`;
      $('settings-gateway-base').textContent = `${base}/v1`;
      $('settings-gateway-chat').textContent = `POST ${base}/v1/chat/completions`;
      $('settings-gateway-port').textContent = String(port);
      const badge = $('settings-gateway-badge');
      if (badge) {
        badge.className = `badge ${status?.ready ? 'ok' : 'warn'}`;
        badge.textContent = status?.ready ? '运行中' : '未就绪';
      }
    } catch (error) {
      console.warn('读取网关状态失败:', error.message);
      renderGatewayOffline('不可用');
    }
  }

  /** 设置页数据入口（app.js 切入该页时调用） */
  async function load() {
    await Promise.all([
      loadSettings(),
      loadGateway(),
      loadAutoCheckin(),
      window.wbUpdatePanel?.load?.(),
    ]);
  }

  // ─── 操作：开关 ─────────────────────────────

  /** 两个开关的当前勾选值就是全量 patch（契约要求全量覆盖，不做增量猜测） */
  function readToggles() {
    return {
      closeToTray: $('settings-close-to-tray').checked,
      autostart: $('settings-autostart').checked,
    };
  }

  /** 保存失败时把开关拨回真实状态：已知状态按状态回滚，未知状态只把刚切的这项切回去 */
  function revertToggles(toggled) {
    const tray = $('settings-close-to-tray');
    const autostart = $('settings-autostart');
    if (current && typeof current === 'object') {
      tray.checked = current.closeToTray === true;
      autostart.checked = current.autostart === true;
      return;
    }
    toggled.checked = !toggled.checked;
  }

  async function saveToggles(toggled) {
    // 其它操作进行中时不受理：把这一下拨动还原，避免界面显示成已改但实际没保存
    if (panelBusy) { revertToggles(toggled); return; }
    panelBusy = true;
    const tray = $('settings-close-to-tray');
    const autostart = $('settings-autostart');
    tray.disabled = true;
    autostart.disabled = true;
    const patch = readToggles();
    const label = toggled === autostart ? '开机自动启动' : '关闭窗口时最小化到托盘';
    try {
      const saved = await api.saveAppSettings(patch);
      // 以主进程返回的设置为准渲染，避免界面与真实状态不一致
      renderSettings(normalizeSettings(saved, patch));
      toast(`✅ 已更新「${label}」`);
    } catch (error) {
      revertToggles(toggled);
      toast(`保存失败：${error.message}`, 'err');
    } finally {
      panelBusy = false;
      tray.disabled = false;
      autostart.disabled = false;
    }
  }

  // ─── 操作：账号导入 / 导出 ──────────────────

  /** 统一忙碌守卫：按钮禁用 + 文案切换，避免重复点击（与 account-panel 的 panelBusy 同思路） */
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

  function setIoResult(html) {
    const box = $('settings-io-result');
    if (box) box.innerHTML = html || '';
  }

  async function exportAccounts() {
    setIoResult('');
    await guard($('btn-settings-export'), '导出中…', async () => {
      const result = await api.exportAccounts();
      if (result?.canceled) { toast('已取消导出'); return; }
      const count = Number(result?.count) || 0;
      if (!count) { toast('没有可导出的账号', 'err'); return; }
      toast(`✅ 已导出 ${count} 个账号${result?.file ? ` 到 ${result.file}` : ''}`);
    });
  }

  async function importAccounts() {
    setIoResult('');
    await guard($('btn-settings-import'), '导入中…', async () => {
      const result = await api.importAccounts();
      if (result?.canceled) { toast('已取消导入'); return; }

      const added = Number(result?.added) || 0;
      const updated = Number(result?.updated) || 0;
      const skipped = Number(result?.skipped) || 0;
      const failed = Number(result?.failed) || 0;
      const errors = Array.isArray(result?.errors) ? result.errors : [];

      const extras = [];
      if (skipped) extras.push(`跳过 ${skipped} 个`);
      if (failed) extras.push(`失败 ${failed} 个`);
      const suffix = extras.length ? `，${extras.join('、')}` : '';
      const summary = `新增 ${added} 个、更新 ${updated} 个${suffix}`;

      if (failed) {
        toast(`导入完成：${summary}`, 'err');
        // 失败明细只列前 3 条，与账号页批量操作的展示密度保持一致
        const detail = errors.slice(0, 3)
          .map(item => `${item?.id ?? '未知账号'}（${item?.message ?? '未知原因'}）`)
          .join('；');
        setIoResult(`<span style="color:var(--danger)">失败 ${failed} 个：${esc(detail)}${
          errors.length > 3 ? ' 等' : ''}</span>`);
      } else {
        toast(`✅ 导入完成：${summary}`);
      }

      // 账号被改动（新增/更新）后让主界面立刻反映：账号列表、导航计数与首选账号等
      await wbApp.refresh?.();
    });
  }

  // ─── 渲染：自动签到 ─────────────────────────

  /** 把「下次执行」化成一句人话；后端没给时间时退回只描述开关状态 */
  function describeNextRun(state) {
    if (!state?.enabled) return '定时签到未开启，账号需要手动签到。';
    const nextAt = Number(state.nextRunAt) || 0;
    if (!nextAt) return `每天 ${state.time} 自动签到。`;
    const next = new Date(nextAt);
    const today = new Date();
    const sameDay = next.toDateString() === today.toDateString();
    const stamp = `${String(next.getHours()).padStart(2, '0')}:${String(next.getMinutes()).padStart(2, '0')}`;
    return `每天 ${state.time} 自动签到，下次执行：${sameDay ? '今天' : '明天'} ${stamp}`
      + `${state.lastFiredToday ? '（今天已执行）' : ''}`;
  }

  /** 上次执行结果写成一行摘要；失败明细只列前两条，与账号页的密度一致 */
  function describeLastResult(result) {
    if (!result) return '';
    const when = result.at ? new Date(Number(result.at)).toLocaleString('zh-CN', { hour12: false }) : '';
    const head = `${when ? `${when} ` : ''}上次执行（${result.reason || '定时'}）：`
      + `${Number(result.succeeded) || 0}/${Number(result.total) || 0} 个账号成功领取`;
    const extras = [];
    if (Number(result.skipped)) extras.push(`跳过 ${result.skipped} 个`);
    if (Number(result.failedCount)) extras.push(`失败 ${result.failedCount} 个`);
    const suffix = extras.length ? `，${extras.join('、')}` : '';
    const failed = Array.isArray(result.failed) && result.failed.length
      ? `（${result.failed.slice(0, 2).join('；')}${result.failed.length > 2 ? ' 等' : ''}）`
      : '';
    return `${head}${suffix}${failed}`;
  }

  function renderAutoCheckin(state) {
    const toggle = $('settings-auto-checkin');
    const time = $('settings-auto-checkin-time');
    const badge = $('checkin-badge');
    if (!toggle || !time || !badge) return;

    if (!state || typeof state !== 'object') {
      badge.className = 'badge bad';
      badge.textContent = '不可用';
      $('settings-checkin-state').textContent = '后端未返回自动签到设置';
      return;
    }

    toggle.checked = state.enabled === true;
    // 只在用户没在编辑时回填时间：否则轮询/重载会把正在输入的值冲掉
    if (document.activeElement !== time && typeof state.time === 'string') time.value = state.time;

    badge.className = `badge ${state.enabled ? 'ok' : ''}`.trim();
    badge.textContent = state.enabled ? (state.lastFiredToday ? '今日已执行' : '已开启') : '未开启';

    const lines = [describeNextRun(state), describeLastResult(state.lastResult)].filter(Boolean);
    $('settings-checkin-state').textContent = lines.join('　·　');
  }

  async function loadAutoCheckin() {
    try {
      renderAutoCheckin(await api.getAutoCheckin());
    } catch (error) {
      console.warn('读取自动签到设置失败:', error.message);
      renderAutoCheckin(null);
    }
  }

  /** 保存自动签到设置（开关与时刻一起提交，契约要求显式给出改动的字段） */
  async function saveAutoCheckin(patch, label) {
    const toggle = $('settings-auto-checkin');
    const time = $('settings-auto-checkin-time');
    if (toggle) toggle.disabled = true;
    if (time) time.disabled = true;
    try {
      const saved = await api.saveAutoCheckin(patch);
      renderAutoCheckin(saved);
      toast(`✅ 已更新「${label}」`);
    } catch (error) {
      toast(`保存失败：${error.message}`, 'err');
      await loadAutoCheckin(); // 回滚到后端的真实状态
    } finally {
      if (toggle) toggle.disabled = false;
      if (time) time.disabled = false;
    }
  }

  async function runCheckinNow() {
    await guard($('btn-checkin-now'), '签到中…', async () => {
      const result = await api.runAutoCheckinNow();
      const succeeded = Number(result?.succeeded) || 0;
      const total = Number(result?.total) || 0;
      const failed = Number(result?.failedCount) || 0;
      if (failed) {
        toast(`签到完成：${succeeded}/${total} 成功，${failed} 个失败`, 'err');
      } else {
        toast(`✅ 签到完成：${succeeded}/${total} 个账号成功领取`);
      }
      if (result?.state) renderAutoCheckin(result.state);
      // 积分可能已变化，顺带刷新账号页的余额展示
      await wbApp.refresh?.();
    });
  }

  // ─── 事件绑定 ──────────────────────────────

  $('settings-close-to-tray').addEventListener('change', event => saveToggles(event.target));
  $('settings-autostart').addEventListener('change', event => saveToggles(event.target));
  $('btn-settings-export').addEventListener('click', exportAccounts);
  $('btn-settings-import').addEventListener('click', importAccounts);
  $('btn-settings-refresh').addEventListener('click', () => loadGateway().then(() => toast('网关地址已刷新')));

  $('settings-auto-checkin')?.addEventListener('change', event =>
    saveAutoCheckin({ enabled: event.target.checked, time: $('settings-auto-checkin-time').value },
      '自动签到开关'));
  // 时间用 change 而不是 input：拖动时间选择器时不该每动一下就发一次请求
  $('settings-auto-checkin-time')?.addEventListener('change', event =>
    saveAutoCheckin({ enabled: $('settings-auto-checkin').checked, time: event.target.value },
      '签到触发时刻'));
  $('btn-checkin-now')?.addEventListener('click', runCheckinNow);

  window.wbSettingsPanel = { load, render: renderSettings, loadGateway, renderAutoCheckin };

  // 首屏自持加载：app.js 的 showPage 在脚本加载前已执行过，
  // 若上次停留在设置页，这里补一次加载，避免徽标一直停在「检测中…」
  if (wbApp.currentPage === 'settings') void load();
})();
