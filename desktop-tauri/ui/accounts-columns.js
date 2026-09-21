/* Agent2API · 账号表的列宽拖动与持久化 */
/* global wbApp */

/**
 * 账号表的**列宽**交互：拖表头右缘的把手改列宽，双击还原，宽度存 localStorage。
 *
 * ── 为什么单独成文件 ────────────────────────────────────────────
 * accounts-view.js 管渲染与事件委托，行数吃紧；列宽是一块自洽的小交互
 * （一组默认值 + 一份持久化 + 两个委托监听），放进来不与任何渲染逻辑耦合。
 *
 * ── 实现要点 ──────────────────────────────────────────────────
 * 表格是 table-layout: fixed，列宽由 <colgroup> 的 <col> 决定 —— 拖动只改被拖的
 * 那一列的 style.width，其余列不动（fixed 布局下各列互不推挤），账号列的伸缩
 * 由它自己的宽度值决定。默认宽度在 DEFAULTS 里与 page-accounts-table.css 的
 * .cell-* 类保持一致：没有拖过的列不带 inline style，走 CSS；拖过（或还原过）
 * 之后就以这里的值为准 —— 所以改 CSS 默认列宽时两处要同步。
 *
 * 持久化按列 key（COLUMNS 的 key，即 CSS 类后缀）存，与列的顺序无关：
 * 以后调整列顺序不会让旧数据错位。
 */
(() => {
  const STORE_KEY = 'agent2api-accounts-col-widths';

  /**
   * 默认列宽（px）：与 page-accounts-table.css 的 .cell-* 一一对应。
   *
   * 改这里**必须**同时改 CSS 与那份文件头的列宽预算说明 —— 三处是同一组数字。
   * 漂移的症状：用户双击把手「还原」后列宽跳到另一个值（还原读的是这里的
   * DEFAULTS，而首屏渲染走的是 CSS）。
   *
   * 本次改造调了两列：usage 100 → 84、actions 190 → 250。
   * 原因是查询余额按钮从余额列挪进了操作列（见 accounts-table.js 的
   * usageCell / actionsCell）：按钮数多了一颗、最坏组合变成
   * 「设为首选 + 余额 + 设置 + 已签到 + ⋯」，两处必须一起动 ——
   * 只改一处会让「已签到」被省略号切掉（那是不可点的状态，
   * 切掉之后用户看不到「今天没得签了」）。
   * 250 这个值是量出来的（算式见 page-accounts-table.css 里那条声明）。
   */
  const DEFAULTS = {
    pick: 26,
    priority: 132,
    provider: 132,
    account: 300,
    connections: 56,
    status: 80,
    limits: 148,
    expiry: 80,
    usage: 84,
    actions: 250,
  };

  /** 拖动的下限：再窄就该点不准里面的控件了 */
  const MIN_WIDTH = 56;

  /** 用户改过的列宽（只有与默认不同的列才会有值），启动时从 localStorage 恢复 */
  const overrides = (() => {
    try {
      const raw = JSON.parse(localStorage.getItem(STORE_KEY) || '{}');
      const clean = {};
      for (const [key, value] of Object.entries(raw || {})) {
        const width = Number(value);
        if (DEFAULTS[key] && Number.isFinite(width) && width >= MIN_WIDTH) clean[key] = Math.round(width);
      }
      return clean;
    } catch {
      return {};
    }
  })();

  const persist = () => {
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify(overrides));
    } catch { /* 隐私模式等存不了就算了：本次会话内仍然生效 */ }
  };

  /** 渲染时的列宽表：默认值 + 用户覆盖（每个 key 都有值，colgroup 一次写全） */
  function widths() {
    const map = {};
    for (const [key, width] of Object.entries(DEFAULTS)) {
      map[key] = overrides[key] ?? width;
    }
    return map;
  }

  /**
   * 第 index 个表头格对应的「列 key + 它的 <col>」。
   *
   * 列设置（table-col-settings.js）能藏列、能换顺序，所以**不能**只按位置认列：
   * 位置只是拿表头格用的，真正的身份是 `data-col`（表头 th 与 colgroup 的 <col>
   * 上同名），拿到 key 之后再按 key 找那个 <col> —— 两处口径一致，用户拖过顺序
   * 之后也不会把宽度写到别的列上。
   * `data-col` 缺失（老标记）时退回按类名 `cell-xxx` 解析，两种写法同一个 key。
   */
  function columnAt(table, index) {
    const header = table?.querySelector(`thead th:nth-child(${index + 1})`);
    const key = header?.dataset.col || header?.className.match(/cell-([a-z]+)/)?.[1];
    if (!key) return null;
    const col = table?.querySelector(`colgroup col[data-col="${CSS.escape(key)}"]`)
      // 没有 data-col 的骨架（老标记）退回按位置取
      || table?.querySelectorAll('colgroup col')[index];
    return col ? { key, col } : null;
  }

  /** 把一次宽度落进 <col>（拖动中实时调用的就是它） */
  function applyWidth(col, key, px) {
    const width = Math.max(MIN_WIDTH, Math.round(px));
    col.style.width = width + 'px';
    if (DEFAULTS[key] && width !== DEFAULTS[key]) overrides[key] = width;
    else delete overrides[key];
  }

  /**
   * 委托绑定：mousedown 开拖、dblclick 还原。挂在滚动容器上一次即可，
   * 表格被整表重绘后监听仍然有效（委托到容器，不依赖具体节点）。
   */
  function bind(host) {
    if (!host) return;
    let dragging = null;

    host.addEventListener('mousedown', event => {
      const grip = event.target.closest?.('.col-grip');
      if (!grip) return;
      event.preventDefault();
      const th = grip.closest('th');
      const table = th?.closest('table');
      const index = th ? [...th.parentElement.children].indexOf(th) : -1;
      const column = columnAt(table, index);
      if (!column) return;
      const startX = event.clientX;
      const startWidth = column.col.getBoundingClientRect().width;
      grip.classList.add('active');
      document.body.classList.add('col-resizing');
      const move = moveEvent => {
        applyWidth(column.col, column.key, startWidth + moveEvent.clientX - startX);
      };
      const up = () => {
        grip.classList.remove('active');
        document.body.classList.remove('col-resizing');
        window.removeEventListener('mousemove', move);
        window.removeEventListener('mouseup', up);
        persist();
        // 拖完重绘一次：colgroup 由 widths() 统一生成，重绘让 DOM 与持久化状态对齐
        window.wbAccountsView?.render?.();
      };
      window.addEventListener('mousemove', move);
      window.addEventListener('mouseup', up);
    });

    host.addEventListener('dblclick', event => {
      const grip = event.target.closest?.('.col-grip');
      if (!grip) return;
      const th = grip.closest('th');
      const table = th?.closest('table');
      const index = th ? [...th.parentElement.children].indexOf(th) : -1;
      const column = columnAt(table, index);
      if (!column) return;
      delete overrides[column.key];
      persist();
      window.wbAccountsView?.render?.();
    });
  }

  window.wbAccountsColumns = { widths, bind };
})();
