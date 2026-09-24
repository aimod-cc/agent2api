/* Agent2API · 请求日志的「清理」弹窗（列表头那颗「清理」按钮的本体） */
/* global workbuddyDesktop, wbApp, wbConfirm */

/**
 * 交互形态参考 OmniProxy 的 ClearLogsModal（D:\Code\OmniProxy 的
 * src/components/requestLogs/ClearLogsModal.tsx）：
 *   · 两种清理方式单选卡片 —— 「全部删除」（mode=all，默认）/「仅清空报文原文」
 *     （mode=raw）；两种方式的差别（删不删统计行、动不动日报）写在卡片说明里，
 *     让用户在选择的那一刻就看到后果；
 *   · 打开时按当前列表的筛选拉 clear-preview 预览（将删条数 / 带原文条数 / 库占用）；
 *   · 「压缩数据库」是独立入口（不清理数据，只回收磁盘）：后台 VACUUM，
 *     前端每 3 秒轮询 vacuumRunning 直到收尾；
 *   · 「执行清理」走 wbConfirm 二次确认，DELETE 带 mode 与列表同款筛选。
 *
 * ── 数据链路（三个接口，全部已就绪，见 stats_api.rs 的模块头）─────
 *   GET    /api/stats/requests/clear-preview   预览统计 {all, raw, dbBytes, vacuumRunning, lastVacuum}
 *   POST   /api/stats/requests/compact         压缩（重复触发 409）
 *   DELETE /api/stats/requests?mode=raw|all    清理（不带筛选必须显式 all=1）
 *
 * ── 筛选参数为什么找 wbRequestsPanel 要 ───────────────────────
 * 「预览说删 N 条」与「确认删掉的那批」必须是同一个集合，所以两者都用
 * `wbRequestsPanel.clearParams()` —— 与列表 GET 同一个来源（filterParams）。
 * 后端 GET / DELETE / clear-preview 三条路由共用同一份筛选解析
 *（stats_api::filter_from_params），前端也不另起一套口径。
 *
 * 弹窗按需创建、关闭即移除（与 confirm-dialog.js / custom-provider-ui.js
 * 同一手法）：不往 index.html 里常驻空弹窗。
 */
(() => {
  const api = workbuddyDesktop;
  const { esc, toast } = wbApp;
  const $ = id => document.getElementById(id);

  const MODAL_ID = 'request-clear-modal';

  /**
   * 两种清理方式。说明文案与后端语义逐条对齐（见 stats_api.rs 的
   * clear_stats_requests）：「按天聚合报表不受影响」是带筛选删除的刻意取舍 ——
   * 日报是全量聚合，无法按筛选部分重算；全量清空则连报表一起归零。
   */
  const MODES = {
    all: {
      title: '全部删除',
      desc: '删除日志记录本身。当前有筛选时只删命中的条目，按天聚合报表不受影响；'
        + '无筛选时清空全部并重置报表。此方式无法恢复。',
    },
    raw: {
      title: '仅清空报文原文',
      desc: '保留统计行与报表，只抹掉请求 / 响应正文。清理后详情弹窗的'
        + '「预览对话」将无原文可看，统计数字分毫不动。',
    },
  };

  /**
   * 打开中的弹窗状态；null = 没开。
   *   { mode: 'all' | 'raw', counts: {all, raw, dbBytes} | null }
   */
  let state = null;
  /** 压缩进度轮询的定时器（3 秒一拍）：关窗即停 */
  let vacuumTimer = null;
  /** 清理请求在途标志：在途时不许关窗（关掉会让「到底删没删」变成未知状态） */
  let clearing = false;

  // ─── 工具 ──────────────────────────────────

  /** 字节数人类可读：B → KB → MB（1 位小数）→ GB。预览行里不塞裸数字 */
  function formatBytes(bytes) {
    const value = Math.max(0, Number(bytes) || 0);
    if (value < 1024) return `${Math.round(value)} B`;
    const kb = value / 1024;
    if (kb < 1024) return `${kb.toFixed(1)} KB`;
    const mb = kb / 1024;
    if (mb < 1024) return `${mb.toFixed(1)} MB`;
    return `${(mb / 1024).toFixed(2)} GB`;
  }

  /**
   * 当前列表的筛选参数（查询串形态，与列表 GET 同源）。
   * 面板未就绪时按「无筛选」处理 —— 那种状态下清理走的是全量语义，
   * 而全量语义后面还有 all=1 护栏兜着，不会静默全删。
   */
  function currentQuery() {
    const query = window.wbRequestsPanel?.clearParams?.();
    return typeof query === 'string' ? query : '';
  }

  /** 脚注（弹窗内的错误出口）：toast 几秒后就没了，脚注留一份 */
  function setHint(text) {
    const hint = $('rcm-hint');
    if (hint) hint.textContent = text || '';
  }

  // ─── 打开 / 关闭 ────────────────────────────

  /** 打开弹窗：按需建壳、拉预览统计。默认「全部删除」（破坏面最大但不带筛选
   *  时最常用；raw 是精细选择，主动选比默认选中更安全） */
  function open() {
    state = { mode: 'all', counts: null };
    $(MODAL_ID)?.remove();
    buildShell();
    void refreshPreview();
  }

  /** 关闭弹窗：清理在途时不响应（见 clearing 的说明） */
  function close() {
    if (clearing) return;
    stopVacuumPolling();
    $(MODAL_ID)?.remove();
    state = null;
  }

  // ─── 弹窗骨架 ──────────────────────────────

  function modeCardHtml(mode) {
    const active = state.mode === mode;
    const info = MODES[mode];
    return `<label class="clear-mode${active ? ' active' : ''}" data-mode="${mode}">
        <input type="radio" name="rcm-mode" value="${mode}"${active ? ' checked' : ''}>
        <span class="t">${esc(info.title)}</span>
        <span class="d">${esc(info.desc)}</span>
      </label>`;
  }

  function buildShell() {
    const mask = document.createElement('div');
    mask.id = MODAL_ID;
    mask.className = 'modal-mask open';
    mask.setAttribute('role', 'dialog');
    mask.setAttribute('aria-modal', 'true');
    mask.setAttribute('aria-labelledby', 'rcm-title');
    mask.innerHTML = `<div class="modal">
        <div class="modal-head">
          <h2 id="rcm-title">清理请求日志</h2>
          <button type="button" id="rcm-close" title="关闭">✕</button>
        </div>
        <div class="modal-body">
          <div class="clear-modes" id="rcm-modes">${modeCardHtml('all')}${modeCardHtml('raw')}</div>
          <div class="rcm-preview"><span class="detail" id="rcm-preview">正在统计…</span></div>
        </div>
        <div class="modal-foot">
          <span class="detail" id="rcm-hint"></span>
          <div class="spacer"></div>
          <button type="button" id="rcm-vacuum" class="sm"
            title="回收已删除数据占用的磁盘空间（checkpoint + VACUUM，后台执行）">压缩数据库</button>
          <button type="button" id="rcm-cancel">取消</button>
          <button type="button" id="rcm-execute" class="danger">执行清理</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    $('rcm-close')?.addEventListener('click', close);
    $('rcm-cancel')?.addEventListener('click', close);
    // 点遮罩空白处 = 关闭（与其它弹窗同一交互），点弹窗本体不关
    mask.addEventListener('click', event => {
      if (event.target === mask) close();
    });
    // 单选卡片：改 state.mode 并就地切换 .active（不重绘整壳 —— 预览行不受影响）
    $('rcm-modes')?.addEventListener('change', event => {
      const input = event.target.closest('input[name="rcm-mode"]');
      if (!input || !state) return;
      state.mode = input.value === 'raw' ? 'raw' : 'all';
      $('rcm-modes')?.querySelectorAll('.clear-mode').forEach(card => {
        card.classList.toggle('active', card.dataset.mode === state.mode);
      });
    });
    $('rcm-vacuum')?.addEventListener('click', () => { void compact(); });
    $('rcm-execute')?.addEventListener('click', () => { void execute(); });
  }

  // ─── 预览统计 ──────────────────────────────

  /** 把 clear-preview 的响应写进预览行与压缩按钮状态 */
  function renderCounts(preview) {
    if (!state) return;
    const counts = {
      all: Number(preview?.all) || 0,
      raw: Number(preview?.raw) || 0,
      dbBytes: Number(preview?.dbBytes) || 0,
    };
    state.counts = counts;
    const box = $('rcm-preview');
    if (box) {
      box.textContent = `将删除 ${counts.all} 条日志 · 其中 ${counts.raw} 条仍带报文原文 · 数据库占用 ${formatBytes(counts.dbBytes)}`;
    }
    setVacuumUi(preview?.vacuumRunning === true);
  }

  /** 压缩按钮的两种形态：空闲可点 / 进行中（禁用 + 文案） */
  function setVacuumUi(running) {
    const button = $('rcm-vacuum');
    if (!button) return;
    button.disabled = running;
    button.textContent = running ? '压缩中…' : '压缩数据库';
    button.title = running
      ? '压缩正在后台执行，完成后自动恢复'
      : '回收已删除数据占用的磁盘空间（checkpoint + VACUUM，后台执行）';
  }

  /** 拉一次预览统计（打开时 / 压缩收尾后调用） */
  async function refreshPreview() {
    if (!state) return;
    const box = $('rcm-preview');
    if (box) box.textContent = '正在统计…';
    try {
      const preview = await api.getStatsClearPreview(currentQuery());
      if (!state || !$(MODAL_ID)) return;   // 等待期间已关窗：丢弃这次结果
      renderCounts(preview);
      // 打开时就有压缩在跑（别人触发的 / 上次没看完的）：直接接上它的进度
      if (preview?.vacuumRunning) startVacuumPolling();
    } catch (error) {
      if (!state || !$(MODAL_ID)) return;
      if (box) box.textContent = '预览读取失败';
      setHint(`预览读取失败：${error?.message || error}`);
    }
  }

  // ─── 压缩数据库（VACUUM）────────────────────

  /**
   * 触发压缩：POST compact 只表示「是否受理」，进度靠 clear-preview 的
   * vacuumRunning 轮询。409（已有一个在跑）不算失败 —— 接上它的进度，
   * 用户看到的结果与「自己触发成功」一致，只是多一句提示。
   */
  async function compact() {
    if (!state) return;
    const button = $('rcm-vacuum');
    if (button) { button.disabled = true; button.textContent = '压缩中…'; }
    try {
      await api.compactStatsDb();
      setHint('压缩已在后台开始，完成后会自动提示');
      startVacuumPolling();
    } catch (error) {
      const message = error?.message || String(error);
      // 409 = 另一个压缩正在跑：后端文案「数据库压缩正在进行中…」，
      // 桥接层只透出这句话（不带状态码），两种特征都认一下
      if (/409|正在进行/.test(message)) {
        toast('压缩已在进行中');
        setHint('压缩已在进行中，接上它的进度等待完成');
        startVacuumPolling();
        return;
      }
      setVacuumUi(false);
      setHint(`压缩失败：${message}`);
      toast(`压缩失败：${message}`, 'err');
    }
  }

  /** 每 3 秒问一次预览：vacuumRunning 翻回 false 即收尾（照 OmniProxy 的节奏） */
  function startVacuumPolling() {
    if (vacuumTimer) return;   // 已在轮询：接上即可，不重复起表
    setVacuumUi(true);
    vacuumTimer = setInterval(async () => {
      if (!state || !$(MODAL_ID)) { stopVacuumPolling(); return; }
      let preview;
      try {
        preview = await api.getStatsClearPreview(currentQuery());
      } catch {
        return;   // 单次轮询失败不影响后续轮询（与 OmniProxy 同一取舍）
      }
      if (!state || !$(MODAL_ID)) return;
      renderCounts(preview);
      if (preview?.vacuumRunning) return;
      stopVacuumPolling();
      setHint('');
      toast('✅ 压缩完成');
      // 压缩不改数据，但库占用变小了：预览刚随上面那次响应更新过，
      // 列表顺带刷一次（vacuum 期间可能又进了新请求）
      void window.wbRequestsPanel?.load?.({ silent: true });
    }, 3_000);
  }

  function stopVacuumPolling() {
    if (vacuumTimer) clearInterval(vacuumTimer);
    vacuumTimer = null;
  }

  // ─── 执行清理 ──────────────────────────────

  /**
   * 二次确认后执行清理。DELETE 的参数拼装（**安全关键**）：
   *   · `mode` 必带（后端缺省也是 all，但显式写出来，回看请求也一目了然）；
   *   · 带筛选 = 只删命中（`mode=all&<筛选参数>`）；
   *   · 不带筛选 = 全量清空，两种 mode 都必须显式 `all=1`（后端护栏，见
   *     stats_api::clear_stats_requests）—— 参数漏传的代价是不可逆的全删，
   *     这里拼死，不指望后端那句 400 提示来兜。
   */
  async function execute() {
    if (!state || clearing) return;
    const ask = window.wbConfirm?.ask;
    if (!ask) return;
    const mode = state.mode;
    const label = MODES[mode].title;
    const query = currentQuery();
    const hasFilters = query.length > 0;
    const deleting = mode === 'raw' ? Number(state.counts?.raw) || 0 : Number(state.counts?.all) || 0;
    const confirmed = await ask({
      title: '清理请求日志',
      html: hasFilters
        ? `确定按当前筛选执行「<strong>${esc(label)}</strong>」？将处理 <strong>${deleting}</strong> 条，此操作无法恢复。`
        : `确定对<strong>全部</strong>请求日志执行「<strong>${esc(label)}</strong>」？将处理 <strong>${deleting}</strong> 条，此操作无法恢复。`,
      okText: mode === 'raw' ? '抹掉原文' : '删除',
      okClass: 'danger',
    });
    if (!confirmed) return;

    clearing = true;
    const button = $('rcm-execute');
    const cancel = $('rcm-cancel');
    if (button) { button.disabled = true; button.textContent = '清理中…'; }
    if (cancel) cancel.disabled = true;
    try {
      const requestQuery = hasFilters ? `mode=${mode}&${query}` : `mode=${mode}&all=1`;
      const result = await api.clearStatsRequests(requestQuery);
      const deleted = Number(result?.deleted) || 0;
      // raw 模式删的是正文而不是行：「已删除 N 条」会让人以为行没了，
      // 文案按实际删掉的东西写
      toast(mode === 'raw' ? `已抹掉 ${deleted} 条报文原文` : `已删除 ${deleted} 条`);
      clearing = false;
      close();
      // 列表收口刷新：筛选清单可能整批消失（清了明细后下拉不该再列着旧值），
      // 由面板的 notifyCleared 一并处理（清单节流清零 + 回第 1 页）
      void window.wbRequestsPanel?.notifyCleared?.();
    } catch (error) {
      const message = error?.message || String(error);
      setHint(`清理失败：${message}`);
      toast(`清理失败：${message}`, 'err');
      if (button) { button.disabled = false; button.textContent = '执行清理'; }
      if (cancel) cancel.disabled = false;
    } finally {
      clearing = false;
    }
  }

  // Esc = 关闭本弹窗（只在本弹窗开着时动作，与其它弹窗的 Esc 互不干扰）
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && $(MODAL_ID)) close();
  });

  window.wbRequestClearModal = { open };
})();
