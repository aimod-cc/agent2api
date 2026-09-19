/* Agent2API · 设置面板（启动与托盘 / 账号导入导出 / 软件更新 / 数据保留） */
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
 *
 * 例外一：本文件还管着两类「后端说了算」的数据，它们不走上面的主进程契约，
 * 也不进 localStorage，而是经 HTTP 桥收发（workbuddyDesktop 里的 call 系列）：
 *   · 软件更新 —— 读回来展示、改完写回去（实际由 update-panel.js 负责，这里只调它的 load）；
 *   · 数据保留天数（getRetention / saveRetention → /api/retention）——
 *     三项保留期存在后端 config.json 里，改小会让后端**立即删除**超出的历史数据
 *     （接口语义见 server/api/stats_api.rs），所以它在保存前多一道二次确认。
 *   两类共用同一套姿势：读失败降级展示、写成功以接口返回值为准重读。
 *
 * 定时任务（自动签到 + 四条间隔型任务）**不在本文件**：它们已迁到独立的
 * 「定时任务」页（tasks-panel.js）—— 那是「到点自动干活」的一类东西，
 * 混在设置页里既不好找，也没法和同类任务对照着调。
 *
 * 例外二：「设置页当前分类」是纯前端偏好
 * （localStorage + DOM 属性 / class），与主题切换同类，不经主进程，
 * 因此不参与下面的加载 / 保存流程。
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

  // ─── 界面偏好：设置页当前分类 ────────────────

  // 纯前端偏好（localStorage + DOM class），主进程不参与，
  // 与主题同套命名。它只决定「设置页进来时展开哪一类」，
  // 不改变任何设置值本身，所以不进下面的加载 / 保存流程。
  const SETTINGS_CAT_KEY = 'workbuddy-desktop-settings-cat';

  /**
   * 切换分类：导航项与内容栏按 data-cat 通用匹配，不写死具体分类 ——
   * 后续加「数据」等新分类时，只要各加一个 .settings-nav-item 与一个
   * .settings-pane（同一个 data-cat），这里一行都不用动。
   */
  function showCategory(cat) {
    const items = [...document.querySelectorAll('#settings-nav .settings-nav-item')];
    // 传进来的值可能来自 localStorage、也可能来自被改过的 DOM：
    // 当前导航里不存在就回落到第一个，保证任何时候都有一类是展开的
    const target = items.some(item => item.dataset.cat === cat) ? cat : items[0]?.dataset.cat;
    if (!target) return;

    items.forEach(item => item.classList.toggle('active', item.dataset.cat === target));
    document.querySelectorAll('.settings-pane').forEach(pane => {
      pane.classList.toggle('active', pane.dataset.cat === target);
    });
    // 切回同一页时把内容滚回顶部：否则上一类的滚动位置会带到新分类上，
    // 打开「关于」却停在半截
    const panes = document.querySelector('.settings-panes');
    if (panes) panes.scrollTop = 0;
  }

  /** 按 localStorage 恢复上次所在的分类（非法值由 showCategory 兜底） */
  function restoreCategory() {
    showCategory(localStorage.getItem(SETTINGS_CAT_KEY));
  }

  // ─── 界面偏好：计量单位 ──────────────────────

  /**
   * 「中文单位」与主题、当前分类同属于纯前端偏好：值与该存哪、默认是什么
   * 由 `units.js` 一个地方说了算（报表页也读它），这里只负责把开关画成
   * 当前状态、并在拨动时写回去。
   *
   * 拨一下就立即生效：不用重新加载页面、也不用重新拉数据 —— 报表页订阅了
   * `wb-units-changed`，收到后用手里那份数据原地重绘（见 report.js）。
   */
  function renderUnits() {
    const toggle = $('settings-chinese-units');
    if (!toggle) return;
    const on = window.wbUnits?.isChinese?.() !== false;
    toggle.checked = on;
    $('units-state').textContent = on
      ? '当前显示为「1.2亿 / 8400万」这类中文量级。'
      : '当前显示为「1.20M / 8.4k」这类英文缩写。';
  }

  function applyUnits(on) {
    window.wbUnits?.setChinese?.(on);
    renderUnits();
    toast(on ? '✅ 已改用中文单位（亿 / 万）' : '✅ 已改用英文单位（M / k）');
  }

  // ─── 渲染 ───────────────────────────────────

  /** 设置页数据入口（app.js 切入该页时调用） */
  async function load() {
    restoreCategory();
    renderUnits();
    await Promise.all([
      loadSettings(),
      loadRetention(),
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
    if (!box) return;
    box.innerHTML = html || '';
    // 空结果整块收起，避免页面上留一个空框
    box.style.display = html ? '' : 'none';
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

      // 账号被改动（新增/更新）后让主界面立刻反映：账号列表、导航计数等
      await wbApp.refresh?.();
    });
  }

  // ─── 数据保留：三项保留天数 ────────────────

  /**
   * 三项保留期的字段名 / 控件 id / 展示名只在这里对齐一次：
   * 字段名必须与后端 `config.rs` 的 KEY_*_RETENTION_DAYS 完全一致（大小写也一样），
   * 否则 PUT 会被当成「不认识的键」静默忽略 —— 界面提示保存成功，值却没变。
   */
  const RETENTION_FIELDS = [
    { key: 'logRetentionDays', inputId: 'settings-retention-log', label: '事件日志保留天数' },
    { key: 'requestRetentionDays', inputId: 'settings-retention-request', label: '请求明细保留天数' },
    { key: 'dailyRetentionDays', inputId: 'settings-retention-daily', label: '按天聚合保留天数' },
  ];
  // 与后端 RETENTION_MIN_DAYS / RETENTION_MAX_DAYS 同源（非法值后端会 400）
  const RETENTION_MIN = 1;
  const RETENTION_MAX = 3650;

  /** 最近一次从后端读到的生效值；为 null 表示后端不可用（此时输入框保持禁用） */
  let retention = null;
  /** 确认弹窗的 Promise resolver；非空即表示弹窗开着 */
  let retentionConfirm = null;

  function retentionInputs() {
    return RETENTION_FIELDS.map(field => $(field.inputId)).filter(Boolean);
  }

  /**
   * 按后端返回值回填。分两种情况：
   *   · 传 null（读取失败）→ 整块标「不可用」并锁住输入，避免用户对着空框改动；
   *   · 传对象（GET / PUT 的响应）→ 只采纳「范围内的整数」，缺字段 / null / 字符串
   *     一概沿用上一轮的有效值：不把线上没给的项清空，也不写 "undefined" 进输入框
   *     （那会被当成非法值，用户一点保存就报错，看着像设置坏了）。
   */
  function renderRetention(data) {
    const badge = $('retention-badge');
    const inputs = retentionInputs();
    if (!badge || inputs.length !== RETENTION_FIELDS.length) return;

    if (!data || typeof data !== 'object') {
      retention = null;
      badge.className = 'badge bad';
      badge.textContent = '不可用';
      inputs.forEach(input => { input.value = ''; input.disabled = true; });
      return;
    }

    // 先与上一轮的值合并：响应里没给（或给得不像样）的项保持原样
    const next = { ...(retention || {}) };
    RETENTION_FIELDS.forEach(field => {
      const value = Number(data[field.key]);
      if (Number.isInteger(value) && value >= RETENTION_MIN && value <= RETENTION_MAX) {
        next[field.key] = value;
      }
    });

    // 三项都拿不到有效值（换壳后接口形状变了之类）：按不可用处理，
    // 不让三只空输入框留在页面上
    if (!RETENTION_FIELDS.some(field => Number.isInteger(next[field.key]))) {
      renderRetention(null);
      return;
    }

    retention = next;
    RETENTION_FIELDS.forEach((field, index) => {
      const input = inputs[index];
      const value = next[field.key];
      // 这一项这一轮仍没有有效值：保持输入框原样（含禁用态），
      // 别把一个空框解禁 —— 空值过不了校验，点保存只会连着报错
      if (!Number.isInteger(value)) return;
      input.disabled = false;
      // 正在编辑的那一项不回填：否则一次重读会把用户刚敲到一半的数字冲掉
      if (document.activeElement !== input) input.value = String(value);
    });
    badge.className = 'badge ok';
    badge.textContent = '已生效';
  }

  async function loadRetention() {
    try {
      renderRetention(await api.getRetention());
    } catch (error) {
      console.warn('读取数据保留设置失败:', error.message);
      renderRetention(null);
    }
  }

  /** 回滚到已知的生效值；从未成功读到过就留空，不编一个假值填回去 */
  function revertRetentionInput(field) {
    const input = $(field.inputId);
    if (!input) return;
    const known = retention?.[field.key];
    input.value = Number.isInteger(known) ? String(known) : '';
  }

  /**
   * 前端校验：必须是 1–3650 的整数（后端也会挡，这里先挡省一次往返，且提示更短）。
   * 用 /^\d+$/ 而不是 Number() + Number.isInteger()：后者会把 "1e2" 认成 100、
   * "0x10" 认成 16，这些都不是「用户填了几天」的直觉答案，不如直接判非法。
   */
  function parseRetentionDays(raw) {
    const text = String(raw ?? '').trim();
    // 空串要单独挡：空值过不了下面的正则，但提示语不同（「不能为空」比「必须是整数」更准）
    if (!text) return { ok: false, message: `天数不能为空（可填 ${RETENTION_MIN}–${RETENTION_MAX}）` };
    if (!/^\d+$/.test(text)) return { ok: false, message: '天数必须是整数' };
    const days = Number(text);
    if (days < RETENTION_MIN || days > RETENTION_MAX) {
      return { ok: false, message: `天数必须在 ${RETENTION_MIN}–${RETENTION_MAX} 之间（当前填的是 ${text}）` };
    }
    return { ok: true, days };
  }

  // ── 确认弹窗 ──
  // 用项目既有的 .modal-mask / .modal 外壳（与账号设置弹窗同一套观感），
  // 而不是删除账号那种原生 confirm()：设置页是正式界面，且这里要把
  // 「哪一项、从多少改到多少、删什么」摊开说，原生弹窗只能挤一行纯文本。
  // 代价是几十行开关逻辑 —— 用一个 Promise 包住「开窗 → 等按钮」，调用处仍是
  // `if (!await askShrink(...)) return;` 的线性写法，与 confirm() 一样好读。

  /** 关窗并把结果交给等待者（重复调用无副作用：resolver 取走即置空） */
  function resolveRetentionConfirm(accepted) {
    const resolve = retentionConfirm;
    retentionConfirm = null;
    $('retention-modal')?.classList.remove('open');
    if (resolve) resolve(accepted);
  }

  /** 弹确认框，返回 Promise<boolean>：确认继续为 true，取消 / 关窗 / Esc 为 false */
  function askRetentionShrink(html) {
    return new Promise(resolve => {
      // 理论上同时只有一个问题（saveRetentionField 已挡掉重入），这里仍兜一层：
      // 万一有第二个问题挤进来，先把旧的按「取消」收尾，而不是让它的 Promise 永远挂着
      if (retentionConfirm) resolveRetentionConfirm(false);
      retentionConfirm = resolve;
      $('retention-modal-text').innerHTML = html;
      $('retention-modal').classList.add('open');
      // 焦点落在「取消」而不是危险键：这是不可恢复的删除操作，
      // 敲回车不该等于同意删除（要确认得先 Tab 或直接点过去）
      $('retention-modal-cancel').focus();
    });
  }

  /** 改小保留期会立即删数据，文案必须点名「删的是哪一档」 */
  function shrinkPrompt(field, previous, days) {
    const head = previous === null
      ? `没能读到「${field.label}」的当前值，改为 ${days} 天可能会删除超出的历史数据。`
      : `「${field.label}」将从 ${previous} 天改为 ${days} 天。`;
    return `${esc(head)}<br><strong>超出的历史数据会被立即删除，且不可恢复。</strong>确定继续？`;
  }

  /**
   * 提交一项保留期：只传变化的那一个字段 —— 后端支持部分字段（未出现的项保持原值），
   * 整份回传会把另外两项也卷进「是否改小」的确认范围，白白多弹一次窗。
   * shrinking 表示这次是改小（后端会顺手清理），决定提示语要不要提「已清理」。
   */
  async function commitRetention(field, input, days, shrinking) {
    if (panelBusy) { revertRetentionInput(field); return; }
    panelBusy = true;
    // 乐观写入待保存的值，再禁用输入框。理由：Chromium 里「让正在聚焦的输入框
    // disabled」会触发一次 blur，而 blur 可能补发 change —— 那个重入的处理器
    // 会走 panelBusy 分支调 revertRetentionInput，把框里的数字写回旧值。
    // 若不先把新值记进 retention，用户就会看到「刚改的数字闪回旧值、过一下又变回来」。
    // 真失败了下面的 catch 会重读后端覆盖，所以这个乐观值不会被留在界面上。
    if (retention) retention = { ...retention, [field.key]: days };
    const inputs = retentionInputs();
    inputs.forEach(element => { element.disabled = true; });
    try {
      const saved = await api.saveRetention({ [field.key]: days });
      // PUT 契约上返回生效后的**三项**值（见 stats_api.rs 的 put_retention），
      // 所以正常情况下用响应刷新即可，不必再跑一趟 GET
      renderRetention(saved);
      const applied = retention?.[field.key];
      if (RETENTION_FIELDS.every(item => Number.isInteger(retention?.[item.key]))) {
        // 显式回填一次做规范化：用户可能敲了 "07" 或前后带空格，显示要与后端一致。
        // 不能只靠 renderRetention —— 它跳过「正在编辑」的输入框（避免冲掉用户输入），
        // 而 change 与 blur 的先后顺序各内核并不一致，此刻 activeElement 可能还是这个框
        input.value = String(applied);
        toast(shrinking ? `✅ 已保留 ${applied} 天，超出部分已清理` : `✅ 已保留 ${applied} 天`);
        return;
      }
      // 响应里三项没齐（换壳后接口形状变了之类）：退回一次 GET 补齐，
      // 宁可多跑一趟，也不能停在「界面说改了、其实没读到真值」的状态
      await loadRetention();
      const latest = retention?.[field.key];
      input.value = Number.isInteger(latest) ? String(latest) : '';
      // 这里提示用户填的值：GET 也没读到真值时，报后端返回的值反而更让人困惑
      toast(shrinking ? `✅ 已保留 ${days} 天，超出部分已清理` : `✅ 已保留 ${days} 天`);
    } catch (error) {
      // 400 的 message（点名哪个字段、超出多少）比自造一句更指向具体问题
      toast(`保存失败：${error.message}`, 'err');
      await loadRetention(); // 回滚到后端的真实值
      // 显式覆盖一次：重读会跳过正在编辑的那一项，而失败时焦点多半还在输入框上
      revertRetentionInput(field);
    } finally {
      panelBusy = false;
      // 逐个按「有没有已知值」解禁：后端不可用时 renderRetention 会把它们留在禁用态，
      // 这里无脑全开会露出一个空输入框（见 renderRetention 的注释）
      RETENTION_FIELDS.forEach(item => {
        if (Number.isInteger(retention?.[item.key])) {
          const element = $(item.inputId);
          if (element) element.disabled = false;
        }
      });
    }
  }

  /** 单个输入框的提交流程：校验 → （改小时）确认 → 提交，任一步失败都回滚原值 */
  async function saveRetentionField(field, input) {
    if (panelBusy) { revertRetentionInput(field); return; }
    // 确认框开着时不再受理新的编辑：一次只问一个问题，否则第二个问题会把第一个
    // 顶掉（resolver 只能存一个），那个输入框就会在没人点过「取消」的情况下被回滚。
    // 遮罩已经挡住了页面，走到这里只剩键盘 Tab 之类的少数路径，挡一下成本极低。
    if (retentionConfirm) { revertRetentionInput(field); return; }

    const parsed = parseRetentionDays(input.value);
    if (!parsed.ok) {
      toast(parsed.message, 'err');
      revertRetentionInput(field);
      return;
    }

    const known = retention?.[field.key];
    const previous = Number.isInteger(known) ? known : null;
    // 值与后端一致就不发请求：数字框里换个写法（如 007）也会触发 change
    if (previous !== null && parsed.days === previous) { input.value = String(previous); return; }

    // 读不到旧值时无从判断是否改小 —— 只有改小才会删数据，所以这里宁可多问一次：
    // 白弹一次确认的代价，远小于静默删掉用户的历史数据
    const shrinking = previous === null || parsed.days < previous;
    if (shrinking && !await askRetentionShrink(shrinkPrompt(field, previous, parsed.days))) {
      revertRetentionInput(field);
      return;
    }

    await commitRetention(field, input, parsed.days, shrinking);
  }

  // ─── 事件绑定 ──────────────────────────────

  $('settings-close-to-tray').addEventListener('change', event => saveToggles(event.target));
  $('settings-autostart').addEventListener('change', event => saveToggles(event.target));
  // 单位开关不进 readToggles 的 patch：它不走主进程，是纯本地偏好（见上）
  $('settings-chinese-units')?.addEventListener('change', event => applyUnits(event.target.checked));
  $('btn-settings-export').addEventListener('click', exportAccounts);
  $('btn-settings-import').addEventListener('click', importAccounts);

  // 分类切换同样是纯本地偏好：绑定挂在导航容器上（事件委托），
  // 这样以后新增分类不必再补一行绑定
  $('settings-nav')?.addEventListener('click', event => {
    const item = event.target.closest('.settings-nav-item');
    if (!item) return;
    showCategory(item.dataset.cat);
    localStorage.setItem(SETTINGS_CAT_KEY, item.dataset.cat);
  });

  // 保留天数用 change 而不是 input：数字框每敲一位都会触发 input，
  // 那样「3」「30」会连着问两次确认；change 只在失焦或回车提交时来一次。
  // 绑定同样走循环：三项结构一致，新增档位只改 RETENTION_FIELDS 一处
  RETENTION_FIELDS.forEach(field => {
    const input = $(field.inputId);
    if (!input) return;
    input.addEventListener('change', event => saveRetentionField(field, event.target));
    // 回车等价于「失焦提交」：不同内核里 Enter 是否派发 change 并不一致，
    // 这里主动 blur 一次把它统一成「值已提交」这一条路径（值没改则不会触发 change）
    input.addEventListener('keydown', event => {
      if (event.key === 'Enter') input.blur();
    });
  });

  // 确认弹窗：确认 / 取消 / 右上角 ✕ / 点遮罩 / Esc 五条出口都收口到 resolver，
  // 任何一条都对应「继续」或「取消」两个明确结果（关窗即视为取消）
  $('retention-modal-ok')?.addEventListener('click', () => resolveRetentionConfirm(true));
  $('retention-modal-cancel')?.addEventListener('click', () => resolveRetentionConfirm(false));
  $('retention-modal-close')?.addEventListener('click', () => resolveRetentionConfirm(false));
  $('retention-modal')?.addEventListener('click', event => {
    if (event.target === $('retention-modal')) resolveRetentionConfirm(false);
  });
  // Esc 关窗：app.js 也挂了一个全局 Esc（只关「添加账号」弹窗，没收开窗状态就不动作），
  // 这里按「确认框是否开着」判断，两者互不干扰
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && retentionConfirm) resolveRetentionConfirm(false);
  });

  $('btn-retention-refresh')?.addEventListener('click', () => loadRetention().then(() => toast('保留天数已刷新')));

  window.wbSettingsPanel = { load, render: renderSettings, renderRetention };

  // 保留天数在首次读到后端值之前保持禁用：空输入框既能被误改，也没法参与
  // 「新值是否小于旧值」的判断（见 saveRetentionField 里 previous 为 null 的分支）。
  // 读成功后由 renderRetention 解禁，读失败则维持禁用并挂上「不可用」徽标
  retentionInputs().forEach(input => { input.disabled = true; });
  // 分类同理：首屏就按上次的选择展开，不必等 load() 回来（load 里再校准一次，
  // 覆盖「页面切回来时 DOM 被重置」的情况）
  restoreCategory();

  // 首屏自持加载：app.js 的 showPage 在脚本加载前已执行过，
  // 若上次停留在设置页，这里补一次加载，避免徽标一直停在「检测中…」
  if (wbApp.currentPage === 'settings') void load();
})();
