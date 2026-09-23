/* Agent2API · 模型管理 / 网关 Key / 请求日志三张表的列宽拖动与持久化 */
/* global wbApp */

/**
 * 表头右缘的把手：拖它改列宽，双击还原这一列，宽度存 localStorage。
 *
 * ── 为什么另起一份，不并进 accounts-columns.js ──────────────────
 * 账号表的列宽是一组写死的像素预算（九个定宽列 + 账号列吃剩余），拖宽之后整张表
 * 横向滚动 —— 那是「表格不压缩列宽」那条取舍的代价，语义与这里不同，合起来只会
 * 互相牵制。这里三张表的列宽是**相对**的（CSS 给百分数 / fr）：
 * 拖动只把被拖的那一列钉成像素，其余列按原权重分掉剩下的宽度。
 *
 * ── 实测过的两条事实（Chrome，table-layout: fixed）──────────────
 * · 全是 px 且总和小于容器 → 浏览器按原比例放大填满，不会留空；
 * · px 与 % 混用 → % 列按权重吃掉剩余宽度，只有 px 列本身超宽时表格才横向滚动。
 * 所以「拖宽一列 = 其余列按比例让位」是可靠的：不滚动、不留空，且拖动过程中
 * 列边界严格跟着光标（其余列彼此的比例不动，只是整体缩放到剩余宽度）。
 *
 * ── 两种表格形态 ──────────────────────────────────────────────
 * · table 模式（模型管理 / 网关 Key）：<table> + <colgroup>，宽度写回 <col>。
 *   没拖过的列不带 inline style，继续走 CSS 的百分数 —— 与拖动前完全一致。
 * · grid 模式（请求日志）：整张表是 display: grid，行由渲染方整体重绘。
 *   轨道列表写成**容器上的 CSS 变量**（--req-cols），靠继承作用到每一行 ——
 *   重绘出来的新行自动拿到同一套轨道，渲染方一行都不用改。
 *
 * ── 把手为什么最后一列不放（table 模式）────────────────────────
 * 把手按 right: -4px 定位（让竖线正好压住列边界），钉在表格右缘会顶出一条横向
 * 滚动条（实测 scrollWidth 比 clientWidth 大 4px）。账号表也是这么处理的。
 * 请求日志的最后一列落在容器内边距里（左右各 14px），不存在这个问题，所以照放。
 *
 * 默认宽度在两处：CSS 里的 width / grid-template-columns（没拖过的列走它）与下面
 * TABLES 里各列的 track（拖动时整条轨道列表以它为底）。改默认列宽时两处要同步。
 */
(() => {
  const STORE_PREFIX = 'agent2api-col-widths:';

  /** 拖动的下限：再窄就该点不准里面的控件了（与账号表同一个值） */
  const MIN_WIDTH = 56;

  const GRIP_CLASS = 'col-grip';
  const GRIP_HTML = `<span class="${GRIP_CLASS}" title="拖动调整列宽（双击还原）"></span>`;

  /**
   * 四张表的登记：columns 的顺序就是列顺序。
   *
   * · table 模式：只给 key，靠表头与 <col> 上的 data-col 属性定位（不依赖列序，
   *   以后在中间插一列不会让旧数据错位）。
   * · grid 模式：给表头单元格的选择器 sel 与默认轨道 track（与 page-requests.css
   *   的 grid-template-columns 一一对应），varName 是写到容器上的变量名。
   *
   * ── columnsOf：列设置与列宽的同源 ─────────────────────────────
   * 四张表都接了列设置（能藏列、能换顺序），而列宽这一层有多处要按**当前可见列**
   * 算：覆盖值落到哪个 <col>、轨道列表有几条、以及「末列不给把手」。各自的
   * columnsOf 从那张表的渲染模块取可见列，再用本登记的列对象对齐 —— 本登记是
   * 列宽（track / 默认宽度）的权威，列设置那头只管显隐与顺序，两者同一个答案。
   * 取不到（脚本未加载、表格还没就绪）就退回全部列，行为与接入前一致。
   */
  const visibleColumnsOf = read => function columnsOf() {
    const visible = read();
    if (!Array.isArray(visible)) return this.columns;
    const byKey = new Map(this.columns.map(column => [column.key, column]));
    return visible.map(column => byKey.get(column.key)).filter(Boolean);
  };

  const TABLES = [
    {
      id: 'models',
      mode: 'table',
      root: '.page[data-page="gateway"] table.models-table',
      columns: ['model', 'rate', 'source', 'alias', 'act'].map(key => ({ key })),
      columnsOf: visibleColumnsOf(() => window.wbModelsPanel?.visibleColumns?.()),
    },
    {
      id: 'keys',
      mode: 'table',
      root: 'table.keys-table',
      columns: ['name', 'key', 'time', 'state', 'act'].map(key => ({ key })),
      columnsOf: visibleColumnsOf(() => window.wbKeysPanel?.visibleColumns?.()),
    },
    {
      id: 'requests',
      mode: 'grid',
      root: '#req-list',
      head: '.req-head',
      varName: '--req-cols',
      columns: [
        { key: 'time', sel: '.req-time', track: '92px' },
        { key: 'target', sel: '.req-target', track: 'minmax(0, 1.1fr)' },
        { key: 'retry', sel: '.req-retry', track: '52px' },
        { key: 'status', sel: '.req-status', track: '96px' },
        { key: 'model', sel: '.req-model', track: 'minmax(0, 1.3fr)' },
        { key: 'dur', sel: '.req-dur', track: '96px' },
        { key: 'usage', sel: '.req-usage', track: 'minmax(0, 1.6fr)' },
        { key: 'error', sel: '.req-error-cell', track: 'minmax(0, 1.2fr)' },
        { key: 'detail', sel: '.req-detail', track: '60px' },
      ],
      /**
       * 当前该渲染哪些列、按什么顺序（见上面 visibleColumnsOf 的说明）。
       *
       * 只返回**可见**列：`track` 从本登记取（这里才是列宽的权威），
       * 列设置那头只管显隐与顺序。
       */
      columnsOf: visibleColumnsOf(() => window.wbRequestsPanel?.visibleColumns?.()),
    },
  ];

  // ─── 覆盖值（内存 + localStorage）─────────────

  /** 每张表的覆盖值：列 key → 像素宽（没拖过的列没有键） */
  const states = new Map();

  function stateOf(table) {
    const cached = states.get(table.id);
    if (cached) return cached;
    const overrides = {};
    try {
      const raw = JSON.parse(localStorage.getItem(STORE_PREFIX + table.id) || '{}');
      for (const [key, value] of Object.entries(raw || {})) {
        const width = Number(value);
        if (table.columns.some(column => column.key === key) && Number.isFinite(width) && width >= MIN_WIDTH) {
          overrides[key] = Math.round(width);
        }
      }
    } catch { /* 存坏了就当没拖过：默认列宽照样能用 */ }
    const state = { overrides };
    states.set(table.id, state);
    return state;
  }

  function persist(table) {
    try {
      localStorage.setItem(STORE_PREFIX + table.id, JSON.stringify(stateOf(table).overrides));
    } catch { /* 隐私模式等存不了就算了：本次会话内仍然生效 */ }
  }

  // ─── 定位与落笔 ──────────────────────────────

  const rootOf = table => document.querySelector(table.root);

  /** 表头单元格：量宽度、插把手都用它（table 模式是 <th>，grid 模式是表头里的那格） */
  function headCellOf(table, root, column) {
    if (table.mode === 'table') return root.querySelector(`th[data-col="${column.key}"]`);
    return root.querySelector(`${table.head} > ${column.sel}`);
  }

  /** 可供各列排布的宽度：表格模式取表格自身，网格模式取轨道空间 */
  function trackSpace(table, root) {
    if (table.mode === 'table') return root.getBoundingClientRect().width;
    const head = root.querySelector(table.head);
    if (!head) return root.getBoundingClientRect().width;
    const style = getComputedStyle(head);
    // 列间距不参与分列，必须扣掉：算漏了就会拖出一条横向滚动条。
    // 缝隙数是**当前可见列**减一（藏列之后轨道也跟着少，见 activeColumns）。
    const gap = (parseFloat(style.columnGap) || 0) * Math.max(0, activeColumns(table).length - 1);
    return head.clientWidth
      - (parseFloat(style.paddingLeft) || 0)
      - (parseFloat(style.paddingRight) || 0)
      - gap;
  }

  /**
   * 一张表**当前该渲染哪些列**（顺序即渲染顺序）。
   *
   * 列设置（table-col-settings.js）可以藏列、换顺序，所以「列宽」这一层也必须
   * 跟着变 —— grid 模式的轨道列表是逐列拼出来的，条数与顺序一旦与渲染出的
   * 格子对不上，整行会错位（第一格吃掉第二格的宽度）。
   * 表自己在 spec 里给 `columnsOf`（返回可见列及其 track）；没给就是全部列，
   * 也就是「没有接入列设置」的那些表。
   */
  const activeColumns = table => (table.columnsOf ? table.columnsOf() : table.columns);

  /** 把当前覆盖值落到 DOM 上（恢复、拖动中、还原都走它） */
  function paint(table) {
    const root = rootOf(table);
    if (!root) return;
    const { overrides } = stateOf(table);

    if (table.mode === 'table') {
      for (const column of activeColumns(table)) {
        const col = root.querySelector(`col[data-col="${column.key}"]`);
        if (!col) continue;
        const width = overrides[column.key];
        // 没拖过的列不留 inline style：CSS 的百分数仍然管着它
        if (width) col.style.width = width + 'px';
        else col.style.removeProperty('width');
      }
      return;
    }

    // grid 模式：整条轨道列表写到容器上，行由继承拿到。
    //
    // 写出的条件有两个，**缺一不可**：
    //   · 有拖过的列 → 用户宽度得有人表达；
    //   · 当前可见列与登记的全集不同（藏了列 / 换了顺序）→ CSS 里那条写死的
    //     默认轨道（page-requests.css 的 9 条）是按**全集 + 原始顺序**排的，
    //     藏一列就少一格、换个顺序就对不上位置，不写变量的话格子数与轨道数
    //     对不上，整行错位（第一格吃掉第二格的宽度）。这一条以前漏了：
    //     藏列之后只要没拖过任何列宽，就退回 CSS 默认 —— 正是最容易踩到的路径
    //     （新用户第一次试列设置）。
    // 两者都不成立时摘掉变量，走 CSS 默认 —— 与接入列设置之前完全一致。
    const columns = activeColumns(table);
    const untouched = columns.length === table.columns.length
      && columns.every((column, index) => column === table.columns[index]);
    if (untouched && !columns.some(column => overrides[column.key])) {
      root.style.removeProperty(table.varName);
      return;
    }
    root.style.setProperty(table.varName, columns
      .map(column => (overrides[column.key] ? `${overrides[column.key]}px` : column.track))
      .join(' '));
  }

  /**
   * 表头里补把手，并把不该留的摘掉。
   *
   * 「哪一列在最后」跟着列设置走（末列不给把手，见模块头），所以每次列集合
   * 变化都要重新对一遍：藏列 / 换顺序之后，上一轮插下的把手会跟着它所在的
   * <th> 一起留着 —— 那一列可能已经不是末列（该有把手却没有），也可能原本
   * 有把手的那一列变成了末列（把手顶出横向滚动条）。已有的不重复插，
   * 所以重绘后反复调用是安全的。
   */
  function paintGrips(table) {
    const root = rootOf(table);
    if (!root) return;
    const columns = activeColumns(table);
    const last = columns[columns.length - 1];

    for (const grip of [...root.querySelectorAll(`.${GRIP_CLASS}`)]) {
      const column = columnOfCell(table, root, grip.parentElement);
      // 认不出所属列（列表头被重绘过）→ 留着，由渲染方那边的重绘逻辑接管
      if (!column) continue;
      // 表格模式下末列不给把手：它 right: -4px 定位，钉在表格右缘会顶出横向滚动条
      if (table.mode === 'table' && column === last) grip.remove();
    }

    columns.forEach((column, index) => {
      if (table.mode === 'table' && index === columns.length - 1) return;
      const cell = headCellOf(table, root, column);
      if (!cell || cell.querySelector(`:scope > .${GRIP_CLASS}`)) return;
      cell.insertAdjacentHTML('beforeend', GRIP_HTML);
    });
  }

  // ─── 拖动 ────────────────────────────────────

  /**
   * 让位列要占掉的宽度（拖动时被拖的那一列必须为它们留出来）：
   * · 拖过的列 → 它自己的像素值，已经钉死；
   * · 没拖过、但默认轨道本身就是固定像素的列（请求日志的时间 / 重试 / 状态 / 用时）→
   *   轨道不参与伸缩，也按它的像素值扣（这一条漏了会拖出横向滚动条）；
   * · 其余（表格模式的百分数列、网格模式的 fr 列）→ 只留 MIN_WIDTH 兜底：
   *   它们会被按比例压扁，但不能压到看不见。
   */
  function reservedOf(table, column) {
    const fixed = /^(\d+(?:\.\d+)?)px$/.exec(table.mode === 'grid' ? column.track : '');
    return fixed ? Math.max(MIN_WIDTH, Math.round(parseFloat(fixed[1]))) : MIN_WIDTH;
  }

  /** 被拖这一列的宽度上限（others 按**可见列**算，藏起来的列不占宽度） */
  function limitOf(table, total, key, startWidth) {
    const { overrides } = stateOf(table);
    const others = activeColumns(table).reduce((sum, column) => (
      column.key === key ? sum : sum + (overrides[column.key] || reservedOf(table, column))
    ), 0);
    // 不允许低于当前宽度：现状是上一轮拖动留下的，往回缩一下更难看
    return Math.max(MIN_WIDTH, Math.round(total - others), Math.round(startWidth));
  }

  const clamp = (value, min, max) => Math.min(Math.max(value, min), max);

  /** 落一次宽度：写内存 → 立即重画（拖动中每帧都调，所以只改被拖的那一列） */
  function applyWidth(table, key, width) {
    stateOf(table).overrides[table.columns.find(column => column.key === key)?.key || key] = Math.round(width);
    paint(table);
  }

  /** 命中的表头格属于哪一列（按**当前可见列**找，藏列后索引会变） */
  function columnOfCell(table, root, cell) {
    if (!cell) return null;
    return activeColumns(table).find(column => headCellOf(table, root, column) === cell) || null;
  }

  /**
   * 委托绑定：mousedown 开拖、dblclick 还原。挂在表格/列表容器上一次即可，
   * 内部节点被重绘后监听仍然有效（委托到容器，不依赖具体节点）。
   *
   * 全程按**列 key** 而不是索引定位：列设置能换顺序、能藏列，索引随时会变，
   * 而 key 是稳定的身份（宽度覆盖值也是按 key 存的，两处口径一致）。
   */
  function bind(table) {
    const root = rootOf(table);
    if (!root) return;

    root.addEventListener('mousedown', event => {
      const grip = event.target.closest?.(`.${GRIP_CLASS}`);
      // 委托挂在各自的表上，所以命中的把手一定在这张表里；columnOfCell 再确认它属于哪一列
      const column = columnOfCell(table, root, grip?.parentElement);
      if (!column) return;
      event.preventDefault();

      const startX = event.clientX;
      const startWidth = grip.parentElement.getBoundingClientRect().width;
      const limit = limitOf(table, trackSpace(table, root), column.key, startWidth);
      grip.classList.add('active');
      document.body.classList.add('col-resizing');

      const move = moveEvent => {
        applyWidth(table, column.key, clamp(startWidth + moveEvent.clientX - startX, MIN_WIDTH, limit));
      };
      const up = () => {
        grip.classList.remove('active');
        document.body.classList.remove('col-resizing');
        window.removeEventListener('mousemove', move);
        window.removeEventListener('mouseup', up);
        persist(table);
      };
      window.addEventListener('mousemove', move);
      window.addEventListener('mouseup', up);
    });

    root.addEventListener('dblclick', event => {
      const grip = event.target.closest?.(`.${GRIP_CLASS}`);
      if (!grip) return;
      const column = columnOfCell(table, root, grip.parentElement);
      if (!column) return;
      const { overrides } = stateOf(table);
      if (!overrides[column.key]) return;
      // 还原这一列：删掉覆盖值 → 它回到 CSS 的默认宽度，腾出的宽度由其余列按权重分掉
      delete overrides[column.key];
      persist(table);
      paint(table);
    });
  }

  /**
   * 请求日志是整块重绘出来的（每次刷新都换掉表头与全部行），重绘后补一次
   * 把手与轨道。
   *
   * 轨道（`--req-cols`）也得跟着重算，不能只在注册时算一次：本文件比
   * requests-panel.js **先加载**，注册那一刻读不到「用户可见哪些列」，
   * 只能按全集算一遍；而面板首屏/每次刷新渲染出的是配置后的列集合。
   * 第一次重绘后立刻校准，从此两者一致。
   *
   * 观察者只对子节点变动触发；而 paint 写的是容器自身的 style，不会引发
   * 子节点变动，所以不存在自激循环。对已有把手直接跳过，重复调用也安全。
   */
  function watch(table) {
    if (typeof MutationObserver !== 'function') return;
    const root = rootOf(table);
    if (!root) return;
    new MutationObserver(() => {
      paint(table);
      paintGrips(table);
    }).observe(root, { childList: true, subtree: true });
  }

  /** 接入一张表：恢复列宽 → 补把手 → 绑事件（grid 模式再加一个重绘观察者） */
  function register(table) {
    TABLES.push(table);
    paint(table);
    paintGrips(table);
    bind(table);
    if (table.mode === 'grid') watch(table);
  }

  for (const table of TABLES.slice()) register(table);

  /**
   * 按 id 重画某张表：列设置改了显隐 / 顺序之后调它。
   *
   * 列宽与把手两件事都要重对一遍 —— 轨道条数（grid 模式）、覆盖值落到哪个
   * <col>（table 模式）、以及「末列不给把手」这条规则，全都按当前可见列算。
   */
  function repaint(id) {
    const table = TABLES.find(item => item.id === id);
    if (!table) return;
    paint(table);
    paintGrips(table);
  }

  // 供别处重画用（表格自己重绘了列宽骨架时调一次）
  window.wbTableColumns = { paint, grips: paintGrips, register, repaint };
})();
