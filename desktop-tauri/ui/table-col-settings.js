/* Agent2API · 表格列设置（列的显示 / 隐藏 · 顺序 · 对齐） */
/* global wbIcons */

/**
 * 四张表共用的「列设置」能力：点页面上的按钮弹出一个面板，逐列切换显示、
 * 拖动调整顺序、选左 / 中 / 右对齐；改动即时生效并存在 localStorage。
 *
 * ── 为什么要一个共享模块 ──────────────────────────────────────
 * 四张表的渲染方式并不一样（账号表整表重建、模型管理与网关 Key 是静态表头 +
 * 动态行、请求日志是 CSS grid），但**列设置这一层是同一件事**：一份「列 key →
 * { 显示, 对齐 } 的有序配置」。把它抽出来，四张表各自只做「按配置渲染」，
 * 而不是各写一份面板 HTML + 各写一份持久化 + 各写一份拖动排序。
 *
 * ── 表怎么消费这份配置 ────────────────────────────────────────
 * 表在 `register` 时声明自己的列（key / 表头文案 / 默认对齐），然后有两种用法：
 *   · `apply(id, columns)` —— 传入按**渲染顺序**排好的列数组，拿回一份
 *     按用户配置重排、过滤（隐藏的去掉）并带上 `align` 的新数组。
 *     适合自己拼 HTML 的渲染方（账号表、请求日志、模型 / Key 的单元格同步）。
 *   · `configOf(id)` —— 直接拿有序配置，自己判断显隐与对齐。
 *
 * ── 「渲染顺序」是一条硬约定 ──────────────────────────────────
 * 列的 key 与渲染顺序的对应关系只能有一处定义：`spec.columns` 的顺序。渲染方
 * 必须按这个顺序产出单元格（模型管理 / 网关 Key 的 DOM 同步按位置给 <td> 认列，
 * 靠的就是它）。要加一列时改 spec.columns 并同步渲染方，不要在渲染方另排一遍。
 *
 * ── 面板为什么挂在 body 上（fixed）而不是跟着按钮 ──────────────
 * 与 tooltip.js / select.js 踩过的是同一个坑：`.panel` 是 overflow:hidden，
 * 面板留在 DOM 原处会被卡片直接裁掉。所以浮层常驻 body、按按钮位置定位。
 * 也因此需要自己处理「点外面收起 / Esc / 滚动跟随 / 窗口缩放」这几件事
 * （它是常驻节点，不会随宿主消失）。
 */

(() => {
  const STORE_PREFIX = 'agent2api-col-config:';
  const PANEL_CLASS = 'colset-panel';

  /** 对齐三档；值直接写进 class 后缀（ta-left / ta-center / ta-right） */
  const ALIGNS = [
    { value: 'left', label: '左' },
    { value: 'center', label: '中' },
    { value: 'right', label: '右' },
  ];

  /** 拖动手柄的六点图标（内联 SVG，与 icons.js 同款手法：不依赖字体字形） */
  const GRIP_ICON = '<svg viewBox="0 0 24 24" width="13" height="13" fill="currentColor" aria-hidden="true">'
    + '<circle cx="9" cy="6" r="1.6"/><circle cx="15" cy="6" r="1.6"/>'
    + '<circle cx="9" cy="12" r="1.6"/><circle cx="15" cy="12" r="1.6"/>'
    + '<circle cx="9" cy="18" r="1.6"/><circle cx="15" cy="18" r="1.6"/></svg>';

  /** 注册过的表：id → { spec, config } */
  const registry = new Map();

  /** 当前浮层：{ id, el, anchor } */
  let panel = null;
  /** 正在拖动的列 key（null = 没在拖） */
  let dragging = null;

  // ─── 配置的读取与归一 ────────────────────────

  function defaultsOf(spec) {
    return spec.columns.map(column => ({
      key: column.key,
      visible: column.visible !== false,
      align: column.align || 'left',
    }));
  }

  /**
   * 存盘的配置 → 当前可用的配置。
   *
   * 归一的三条（都是为了让「改过列定义之后旧的本地配置还能用」）：
   *   1. 存盘里已经不存在的 key 丢掉（那一列被删了）；
   *   2. spec 里新增的 key 按默认值插到它在 spec 里的**相对位置**
   *      （紧跟在 spec 中排在它前面、且用户配置里也存在的那一列之后）；
   *   3. align 只认三档，脏值退回该列默认。
   * 顺序以存盘为准 —— 顺序正是用户拖出来的东西。
   *
   * ── 新增列为什么插到声明位置而不是追加到末尾（本次修正）────────
   * 早先一律追加到末尾，理由是「用户第一次看到它就在最后，下一次自己拖到想要
   * 的位置」。那个理由只对**位置无所谓**的新列成立；列的位置有时是语义的：
   * 账号表的代理列声明在「账号」与「连接数」之间（它读起来是账号的属性），
   * 追加到末尾会让已经存过配置的老用户看到的默认位置与预期不符 ——
   * 他还得自己拖一次才能回到「默认」。插到声明位置之后，新装与已存过配置的
   * 两种用户看到的是同一个默认顺序，而用户拖过的列顺序仍然完全不受影响
   * （插入只发生在「存盘里根本没有这个 key」时）。
   *
   * ── 对齐：要能分辨「存的是用户挑的」与「存的是当时的默认值」────────
   * 显示与顺序一律以存盘为准，对齐不能 —— **默认值本身会随版本调整**
   * （账号表本次就从「除操作列外全部左对齐」改成「操作列居右、其余居中」），
   * 一律以存盘为准的话，老用户永远看不到新默认值，改了默认值等于没改。
   *
   * 判据是「存盘值 ≠ 当时那份默认值」即视为用户挑过，于是需要知道**当时**的默认值，
   * 两种存盘形态各有一个来源：
   *   · v2 `{ v, defaults, items }`：defaults 是上次写盘时各列的默认对齐快照
   *     （见 persist）。精确 —— 用户改过的列一定与快照不同。
   *   · v1 裸数组（本次改造前的形态，没有快照）：用列上声明的 `legacyAlign`
   *     （= 上一版的默认对齐）。它只对那些**默认值改过的列**有必要，
   *     其余列的旧默认就是当前默认，不声明也精确。
   *
   * 两种形态都只在「值恰好等于旧默认」时改判成新默认，所以除这一种歧义
   * （用户主动选了与旧默认相同的值，无从分辨）外不会覆盖用户的选择；
   * 顺序与显隐完全不受这套判定影响。
   */
  function normalize(spec, saved) {
    // v1 是裸数组，v2 是 { v, defaults, items }。两种都当「列表 + 可选快照」读
    const legacy = Array.isArray(saved);
    const items = legacy ? saved : (Array.isArray(saved?.items) ? saved.items : []);
    const snapshot = !legacy && saved?.defaults && typeof saved.defaults === 'object' ? saved.defaults : null;

    const byKey = new Map(spec.columns.map(column => [column.key, column]));
    const defaults = new Map(defaultsOf(spec).map(item => [item.key, item]));
    const out = [];
    const seen = new Set();
    for (const item of items) {
      const key = String(item?.key || '');
      if (!byKey.has(key) || seen.has(key)) continue;
      seen.add(key);
      const column = byKey.get(key);
      const fallback = column.align || 'left';
      // 「当时那份默认值」：v2 读快照，v1 读列上声明的旧默认（未声明即当前默认）
      const before = snapshot ? (snapshot[key] ?? fallback) : (column.legacyAlign ?? fallback);
      const stored = ALIGNS.some(a => a.value === item.align) ? item.align : null;
      out.push({
        key,
        visible: item.visible !== false,
        // 存盘值等于旧默认 → 用户没动过这一列 → 让新默认生效
        align: stored && stored !== before ? stored : fallback,
      });
    }
    for (const column of spec.columns) {
      if (seen.has(column.key)) continue;
      insertBySpecOrder(out, spec, { ...defaults.get(column.key) });
    }
    return out;
  }

  /**
   * 把一个新增列插到 `out` 里、紧跟着它在 `spec.columns` 中的前驱
   * （前驱不在 `out` 里就继续往前找，都找不到则插到最前）。
   *
   * 多个新增列按 `spec.columns` 的顺序依次调用本函数，最终相对次序与 spec 一致 ——
   * 因为每个新列都插在「它前面那个已存在的列」之后，而它前面的新列已经先插好了。
   */
  function insertBySpecOrder(out, spec, item) {
    const at = spec.columns.findIndex(column => column.key === item.key);
    for (let index = at - 1; index >= 0; index -= 1) {
      const previous = out.findIndex(entry => entry.key === spec.columns[index].key);
      if (previous >= 0) {
        out.splice(previous + 1, 0, item);
        return;
      }
    }
    out.unshift(item);
  }

  function load(spec) {
    try {
      const raw = localStorage.getItem(STORE_PREFIX + spec.id);
      return normalize(spec, raw ? JSON.parse(raw) : []);
    } catch {
      // 存坏了就退回默认：列设置读不出来不该让整张表渲染不了
      return defaultsOf(spec);
    }
  }

  /**
   * 写盘。存 `{ v, defaults, items }`：
   *   · `items` 是配置本体（key / visible / align），与 v1 的裸数组同形；
   *   · `defaults` 是**本次写盘时各列的默认对齐**，只为下次启动时能分辨
   *     「存的这一档是用户挑的」还是「当时的默认值」—— 见 normalize。
   *     不写这份快照，将来再改默认值时就没法只对「没动过的列」生效。
   * 多出来的 v / defaults 两个键不影响老版本读它（老代码只读数组本身会失败，
   * 于是退回默认 —— 那正是升级前的行为，不会更糟）。
   */
  function persist(spec) {
    try {
      const defaults = {};
      for (const column of spec.columns) defaults[column.key] = column.align || 'left';
      localStorage.setItem(STORE_PREFIX + spec.id, JSON.stringify({
        v: 2,
        defaults,
        items: spec.config,
      }));
    } catch { /* 隐私模式等存不了就算了：本次会话内仍然生效 */ }
  }

  // ─── 对外接口 ────────────────────────────────

  const configOf = id => registry.get(id)?.config || null;

  const labelOf = (spec, key) => spec.columns.find(column => column.key === key)?.label || key;

  /**
   * 把一份「按渲染顺序排好的列数组」投影成用户配置的顺序与显隐，并带上对齐。
   * 数组里的每一项至少要带 key（其余字段原样保留，渲染方可以顺手把单元格
   * 渲染函数一起塞进来）。
   */
  function apply(id, columns) {
    const entry = registry.get(id);
    if (!entry) return columns;
    const byKey = new Map(columns.map(column => [column.key, column]));
    return entry.config
      .filter(item => item.visible && byKey.has(item.key))
      .map(item => ({ ...byKey.get(item.key), align: item.align }));
  }

  /** 改动落地：写盘 → 通知该表重绘 → 面板还开着就重画一遍 */
  function commit(spec, { silent = false } = {}) {
    persist(spec);
    if (!silent) spec.onChange?.(spec.config);
    if (panel && panel.id === spec.id) renderPanel();
  }

  // ─── 面板 ────────────────────────────────────

  const $ = id => document.getElementById(id);

  function rowHtml(spec, item) {
    const key = item.key;
    const alignButtons = ALIGNS.map(align =>
      `<button type="button" class="colset-align${item.align === align.value ? ' active' : ''}"`
      + ` data-align="${align.value}" data-col-key="${key}"`
      + ` aria-pressed="${item.align === align.value}" title="这一列内容${align.label}对齐">${align.label}</button>`).join('');
    return `<div class="colset-row${dragging === key ? ' dragging' : ''}" data-col-key="${key}">`
      + `<span class="colset-grip" data-grip="${key}" title="按住拖动调整列顺序">${GRIP_ICON}</span>`
      + `<span class="colset-name" title="${labelOf(spec, key)}">${labelOf(spec, key)}</span>`
      + `<label class="switch colset-switch" title="${item.visible ? '这一列正在显示' : '这一列已隐藏'}">`
      + `<input type="checkbox" data-visible="${key}"${item.visible ? ' checked' : ''} aria-label="显示「${labelOf(spec, key)}」列">`
      + '<span class="track"></span></label>'
      + `<span class="colset-aligns">${alignButtons}</span>`
      + '</div>';
  }

  function panelHtml(spec) {
    return `<div class="colset-head"><span></span><span>列</span><span>显示</span><span>对齐</span></div>`
      + `<div class="colset-body">${spec.config.map(item => rowHtml(spec, item)).join('')}</div>`
      + `<div class="colset-foot">`
      + '<span class="colset-hint">拖动 ⋮⋮ 调整顺序</span>'
      + `<button type="button" class="sm colset-reset" data-reset>恢复默认</button>`
      + '</div>';
  }

  function renderPanel() {
    if (!panel) return;
    panel.el.innerHTML = panelHtml(panel.spec);
  }

  /** 按按钮位置摆浮层：默认贴按钮右下，右边 / 下边放不下就翻向 */
  function place() {
    if (!panel) return;
    const rect = panel.anchor.getBoundingClientRect();
    const box = panel.el.getBoundingClientRect();
    const EDGE = 8;
    const left = Math.max(EDGE, Math.min(rect.right - box.width, window.innerWidth - EDGE - box.width));
    const below = window.innerHeight - rect.bottom - 6 - EDGE;
    const top = below >= box.height || rect.top < box.height + 6
      ? rect.bottom + 6
      : rect.top - 6 - box.height;
    panel.el.style.left = `${Math.round(left)}px`;
    panel.el.style.top = `${Math.round(Math.max(EDGE, Math.min(top, window.innerHeight - EDGE - box.height)))}px`;
  }

  function closePanel() {
    if (!panel) return;
    panel.anchor.classList.remove('open');
    panel.el.remove();
    panel = null;
    dragging = null;
  }

  function openPanelFor(spec, anchor) {
    closePanel();
    const el = document.createElement('div');
    el.className = PANEL_CLASS;
    el.setAttribute('role', 'dialog');
    el.setAttribute('aria-label', `${spec.label}的列设置`);
    // 委托挂在面板本身上（不是每一行）：commit 会重画整块 innerHTML，
    // 行节点随之换掉，挂在行上的监听会跟着丢 —— 委托到常驻的面板元素就没有这个问题。
    el.addEventListener('click', event => onPanelClick(spec, event));
    el.addEventListener('change', event => onPanelChange(spec, event));
    el.addEventListener('pointerdown', event => {
      const grip = event.target.closest('.colset-grip');
      if (!grip) return;
      startDrag(spec, grip.dataset.grip, event);
    });
    document.body.appendChild(el);
    panel = { id: spec.id, spec, el, anchor };
    anchor.classList.add('open');
    renderPanel();
    // 先摆位再进 DOM 是量不到高度的：渲染完立刻量一次（同一个任务内，不闪）
    place();
  }

  const togglePanel = (spec, anchor) => {
    if (panel && panel.id === spec.id) closePanel();
    else openPanelFor(spec, anchor);
  };

  // ─── 面板内的交互 ────────────────────────────

  /** 把第 from 个配置项移到 to 的位置（拖动排序的最小单元） */
  function moveItem(config, from, to) {
    const next = [...config];
    const [item] = next.splice(from, 1);
    next.splice(to, 0, item);
    return next;
  }

  /** 指针落点下面那一行的列 key（拖动时用它判断「跨过了哪一列」） */
  function rowKeyAt(x, y, spec) {
    const node = document.elementFromPoint(x, y)?.closest?.('.colset-row');
    if (!node || node.closest(`.${PANEL_CLASS}`) !== panel?.el) return '';
    const key = node.dataset.colKey || '';
    return spec.config.some(item => item.key === key) ? key : '';
  }

  /**
   * 拖动排序用指针事件而不是 HTML5 拖放（dragstart/dragover）：
   * 面板每次改动都要重画（开关与对齐按钮的状态跟着配置走），重画会换掉行节点，
   * 而 HTML5 拖放的会话是绑在**那个节点**上的 —— 一重画拖动就断了。
   * 指针事件的状态只在 JS 变量里（dragging），节点换掉不影响会话。
   */
  function startDrag(spec, key, event) {
    dragging = key;
    const row = event.target.closest('.colset-row');
    row?.classList.add('dragging');
    document.body.classList.add('colset-dragging');
    event.preventDefault();

    const move = moveEvent => {
      const overKey = rowKeyAt(moveEvent.clientX, moveEvent.clientY, spec);
      if (!overKey || overKey === dragging) return;
      const from = spec.config.findIndex(item => item.key === dragging);
      const to = spec.config.findIndex(item => item.key === overKey);
      if (from < 0 || to < 0) return;
      spec.config = moveItem(spec.config, from, to);
      renderPanel();
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('colset-dragging');
      dragging = null;
      // 顺序只在松手时写盘与通知：拖动过程中每跨一行都重绘表格会很吵
      commit(spec);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }

  function onPanelClick(spec, event) {
    const reset = event.target.closest('[data-reset]');
    if (reset) {
      spec.config = defaultsOf(spec);
      commit(spec);
      return;
    }
    const align = event.target.closest('.colset-align');
    if (align) {
      const item = spec.config.find(entry => entry.key === align.dataset.colKey);
      if (!item || item.align === align.dataset.align) return;
      item.align = align.dataset.align;
      commit(spec);
    }
  }

  function onPanelChange(spec, event) {
    const box = event.target.closest('[data-visible]');
    if (!box) return;
    const item = spec.config.find(entry => entry.key === box.dataset.visible);
    if (!item) return;
    // 最后一列不许关：全隐藏之后表格只剩空壳，用户得先想起「是我自己关的」
    // 才能从面板里找回来。关不掉比关得掉再懊恼一次好。
    if (!box.checked && spec.config.filter(entry => entry.visible).length <= 1) {
      box.checked = true;
      return;
    }
    item.visible = box.checked;
    commit(spec);
  }

  // ─── 全局收起与跟随 ──────────────────────────

  function bindGlobal() {
    document.addEventListener('pointerdown', event => {
      if (!panel) return;
      if (panel.el.contains(event.target) || panel.anchor.contains(event.target)) return;
      closePanel();
    }, true);
    document.addEventListener('keydown', event => {
      if (event.key === 'Escape' && panel) closePanel();
    });
    // 浮层是 fixed，不随内容滚动：跟随重新定位，锚点滚出视口才收起
    // （与 tooltip.js 同一取舍：读面板时滚动不该把面板弄没）
    const follow = () => {
      if (!panel) return;
      if (!panel.anchor.isConnected) { closePanel(); return; }
      const rect = panel.anchor.getBoundingClientRect();
      if (rect.width && rect.bottom > 0 && rect.top < window.innerHeight) place();
      else closePanel();
    };
    window.addEventListener('scroll', follow, true);
    window.addEventListener('resize', follow);
  }

  // ─── 按钮与注册 ──────────────────────────────

  function makeButton(spec) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = `sm colset-btn${spec.buttonClass ? ` ${spec.buttonClass}` : ''}`;
    button.id = `btn-colset-${spec.id}`;
    button.title = `调整「${spec.label}」的列：显示 / 隐藏、顺序、对齐`;
    button.innerHTML = `${wbIcons?.icon?.('settings', 14) || ''}<span>列设置</span>`;
    button.addEventListener('click', () => togglePanel(spec, button));
    return button;
  }

  function mountOf(spec) {
    const target = typeof spec.mount === 'function' ? spec.mount() : document.querySelector(spec.mount);
    return target || null;
  }

  /**
   * 登记一张表：读回本地配置、把「列设置」按钮插进指定容器。
   *
   * `mount` 取该页的操作区（元素、选择器或返回元素的函数）：工具条右侧、卡片头
   * 的操作组、或批量栏那类只放按钮的容器都行。
   * 按钮插在**最前面**：它是「怎么看这张表」的开关，与旁边那些「对数据做什么」
   * 的操作按钮不是一类，排在前面不会被误当成主操作按钮。
   */
  function register(spec) {
    const entry = { ...spec, config: load(spec) };
    registry.set(spec.id, entry);
    const host = mountOf(spec);
    if (host) host.insertBefore(makeButton(entry), host.firstChild);
    return {
      apply: columns => apply(spec.id, columns),
      config: () => entry.config,
    };
  }

  // ─── 静态表头的同步（模型管理 / 网关 Key）─────
  //
  // 这两张表的 <colgroup> 与 <thead> 写在 index.html 里（不是每次渲染重画），
  // 所以列顺序与显隐要**就地重排既有元素**，不能按字符串重建：
  //   · <col> 上带着 table-columns.js 拖出来的 inline 宽度，重建会丢；
  //   · <th> 里插着列宽把手（.col-grip），重建后要等它下次补偿。
  // appendChild 对已存在的元素是「移动」，两者都自动跟着走。
  // 提前 return 的那个 continue 很关键：隐藏的列**从 DOM 里摘掉**而不是
  // display:none —— <col> 的 display 在表布局里各浏览器行为不一致，
  // 摘掉才是可靠的「这一列不存在」。

  const ALIGN_CLASSES = ALIGNS.map(align => `ta-${align.value}`);

  /** 把一列的对齐写到表头 / 单元格上（三档互斥，先摘后加） */
  function applyAlign(el, align) {
    if (!el) return;
    el.classList.remove(...ALIGN_CLASSES);
    el.classList.add(`ta-${align}`);
  }

  /**
   * 静态表头的元素表：id → { table, row, group, ths, cols }。
   *
   * 为什么必须缓存：隐藏一列是把它的 <th> / <col> 从 DOM 里**摘掉**（见
   * syncStaticHead），摘掉之后 `querySelectorAll` 就再也查不到它了 ——
   * 每次都现查的话，用户重新勾上那一列会**放不回去**（元素只在内存里飘着）。
   * 所以首次同步时把这批元素记住，之后一直从缓存取。
   *
   * 整张表被重建时（引用变了，或 thead 不在文档里了）缓存作废重取 ——
   * 重建后 DOM 里本就只剩当时可见的列，这是可接受的退化：表格重建自己
   * 也会丢掉列宽之类的东西。
   */
  const staticHeads = new Map();
  function staticHeadOf(id, table) {
    const cached = staticHeads.get(id);
    if (cached && cached.table === table && cached.row.isConnected) return cached;
    const row = table?.querySelector('thead tr');
    if (!row) return null;
    const group = table.querySelector('colgroup');
    const entry = {
      table,
      row,
      group,
      ths: new Map([...row.querySelectorAll('th[data-col]')].map(el => [el.dataset.col, el])),
      cols: new Map([...(group?.querySelectorAll('col[data-col]') || [])].map(el => [el.dataset.col, el])),
    };
    staticHeads.set(id, entry);
    return entry;
  }

  /**
   * 让一张静态表（<colgroup> + <thead> 写死在 HTML 里）的表头跟上配置：
   * 按配置顺序重排、隐藏的摘掉、对齐类重新贴一遍。
   *
   * 数据行不在这里管 —— 它们由各自的渲染函数按同一份配置产出单元格，
   * 每次重绘自然就是对的（见 models-panel / keys-panel 的行渲染）。
   */
  function syncStaticHead(id, table) {
    const spec = registry.get(id);
    if (!spec || !table) return;
    const head = staticHeadOf(id, table);
    if (!head) return;
    const { row, group, ths, cols } = head;

    for (const item of spec.config) {
      const th = ths.get(item.key);
      if (!th) continue;
      const col = cols.get(item.key);
      if (!item.visible) {
        th.remove();
        col?.remove();
        continue;
      }
      applyAlign(th, item.align);
      // 顺序即配置顺序：appendChild 把它们逐个移到末尾，最终顺序就是遍历顺序
      row.appendChild(th);
      if (col && group) group.appendChild(col);
    }
    // 表头换了一副样子，列宽那一层要跟着重新对一遍：把手是按「哪一列在最后」放的
    // （最后一列不给 —— 它钉在表格右缘会顶出横向滚动条），而最后是哪一列由列设置
    // 说了算。见 table-columns.js 的 repaint。
    window.wbTableColumns?.repaint?.(id);
  }

  window.wbColSettings = {
    register,
    apply,
    configOf,
    syncStaticHead,
    /** 面板是整页唯一的浮层，两处各弹一个是不可能的（同时只留一个） */
    close: closePanel,
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bindGlobal, { once: true });
  } else {
    bindGlobal();
  }
})();
