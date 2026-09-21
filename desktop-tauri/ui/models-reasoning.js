/* Agent2API · 模型管理页的「思考等级」组件件（R7） */
/* global wbApp */

/**
 * 思考等级绑定在界面上的那一小块：候选列表、chip 上的等级标、弹窗里的下拉。
 *
 * ── 为什么单独一个文件 ──────────────────────────────────────
 * `models-panel.js` 已经把那页的表格 / 筛选 / 行内操作都装在里面（再加上
 * 「映射弹窗」），再塞进这一整套就会顶破项目约定的单文件体量。拆出来的这一块
 * 内部**自洽**：它只回答「有哪些等级、某条映射绑了什么、怎么渲染与读写那个下拉」，
 * 不碰表格、不碰网络请求、不碰面板自己的数据生命周期 —— 与
 * `models-panel.js` 的关系是「纯组件件 ← 消费方」，本文件不认识那个面板。
 *
 * ── 数据从哪儿来（**都不硬编码**）────────────────────────────
 *   · 候选等级 = 后端 `GET /api/models/manage` 的 `reasoningLevels`
 *     （= `model_rules::REASONING_LEVELS`，照抄 OmniProxy 的
 *     `GENERIC_REASONING_LEVELS`）。拿不到（旧版网关 / 首次加载失败）时退回一份
 *     本地兜底表，内容与后端那份**同源**，只是为了让下拉不至于空着。
 *   · 某条映射的等级 = 同一份响应顶层 `mappings` 里那条的 `reasoning` 字段。
 *     按 (alias, target, provider) 三元组查 —— 行上的 chip 已经带着这个三元组，
 *     不需要另加一份平行数组（两个数组一旦因过滤口径不同而对不上，界面上会
 *     出现「chip 在、等级丢了」这种无从解释的空档）。
 *
 * ── 级联（每一步都离不开前一步）─────────────────────────────
 *   models(data)                 面板的整份数据
 *     └ levels(data)             → 候选等级
 *     └ buildIndex(data.mappings) → 三元组 → 等级 的索引（每次渲染重建一次）
 *     └ badge(...)               → chip 上那枚可点的小标
 *     └ fillSelect(...)          → 弹窗里的下拉（不覆盖 + 各等级 + 自定义）
 *
 * ── 一条必须守住的语义：**「不覆盖」与「清空」不是一回事** ──────
 * 下拉的空值（`''`）表示「不覆盖」= 显式清掉绑定；而「这一轮不改」是**另一件事**，
 * 由调用方（`models-panel.js` 的保存流程）用「不传这个键」表达。本文件只负责
 * 前者：`valueOf` 返回空串就是「清空」，它绝不返回 `undefined` 那种含混值 ——
 * 那会让「从 high 改成不覆盖」这一步静默无效（后端按「键在不在」分流，
 * 见 `api::model_manage::add_mapping` 的三态说明）。
 */
(() => {
  const { esc } = wbApp;

  /** 「自定义等级」在下拉里的哨兵值：它不是一个真的等级，选中它只是把输入框露出来。
      用 `__custom__` 而不是空串 —— 空串在原生 select 里是「不覆盖」那一项的值，
      两者必须分开。 */
  const CUSTOM_LEVEL = '__custom__';

  /**
   * 拿不到后端候选表时的兜底（与后端那份**同源**：OmniProxy 的
   * GENERIC_REASONING_LEVELS）。正常路径下永远用后端那一份，
   * 所以后端调整候选时前端零改动；这份只在「老网关 / 首次加载失败」时兜底。
   */
  const FALLBACK_LEVELS = ['off', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

  /** 候选等级：优先后端下发的（`data.reasoningLevels`） */
  function levels(data) {
    const list = data?.reasoningLevels;
    if (Array.isArray(list) && list.length) {
      return list.filter(level => typeof level === 'string');
    }
    return FALLBACK_LEVELS;
  }

  /** 三元组 → 索引键（小写；用不可能出现在任何名字里的 \u0001 作分隔符） */
  function keyOf(alias, target, provider) {
    return `${String(alias ?? '').trim().toLowerCase()}\u0001${String(target ?? '').trim().toLowerCase()}`
      + `\u0001${String(provider ?? '').trim().toLowerCase()}`;
  }

  /**
   * 建「三元组 → 等级」索引。每次渲染 / 每次打开弹窗重建一次。
   *
   * ── 为什么是索引而不是每次 find ──────────────────────────────
   * 查询发生在**每个 chip** 上（一屏可能有上百个），而映射表本身也上百条 ——
   * 朴素写法是 O(chips × mappings) 次字符串比对，而 render() 在搜索框里
   * 每敲一个字就跑一次。索引建成后每次查询是一次哈希查找。
   *
   * 返回一个「查一条」的闭包（而不是把 Map 交出去）：调用方不必知道键怎么拼，
   * 也不会有人在别处拿这份 Map 做别的事。
   */
  function buildIndex(mappings) {
    const index = new Map();
    (Array.isArray(mappings) ? mappings : []).forEach(mapping => {
      const level = typeof mapping?.reasoning === 'string' ? mapping.reasoning.trim() : '';
      if (!level) return;
      index.set(keyOf(mapping.alias, mapping.target, mapping.provider), level);
    });
    return (alias, target, provider) => index.get(keyOf(alias, target, provider)) || '';
  }

  /**
   * chip 上那枚等级小标的 HTML。
   *
   * **未绑定时也渲染**（虚线样式的「＋等级」）：只给已绑定的显示标，会让
   * 「怎么给这条映射绑等级」变成一个查不出的问题 —— 唯一的入口会藏在
   * 「先删掉再按同名重建」里，而那是用户想不到的操作。
   *
   * `data-act="reasoning"` + 三元组 dataset 是**唯一**的分派方式
   *（事件委托按 data-act 走，见 models-panel 的 onTableClick）：
   * 调用方不要把 `data-act="unmap"` 那串复用进来 —— 一个按钮挂两个动作
   * 会让「点等级」变成「删映射」。
   */
  function badge({ alias, target, provider, level, busy = false }) {
    const triple = `data-act="reasoning" data-alias="${esc(alias)}"`
      + ` data-target="${esc(target)}" data-provider="${esc(provider || '')}"`;
    const disabled = busy ? ' disabled' : '';
    return level
      ? `<button type="button" class="alias-level" ${triple}${disabled}`
        + ` title="思考等级 ${esc(level)}（点击修改）">${esc(level)}</button>`
      : `<button type="button" class="alias-level unset" ${triple}${disabled}`
        + ` title="设置思考等级（当前未绑定）">＋等级</button>`;
  }

  /**
   * 把候选灌进弹窗里的下拉：`不覆盖` + 各等级 + `自定义等级…`。
   *
   * 选项**每次打开都重建**（候选表可能刚刷新过，值的集合变了），所以 `keep`
   * 要在重建后按值还回去。`keep` 不在候选里（用户上次填的自定义值、或后端
   * 调整过候选表）时落到「自定义」那一项并把值填进输入框 —— 直接丢掉会让
   * 「改别的字段时顺手把等级抹掉」，那是最难查的一类数据丢失。
   *
   * `sync()` 是必需的：select.js 的 observer 回调跑在**微任务**里，紧接着的
   * 同步读界面还是旧的（它的注释里写明了这一点，多选回填正是那个场景）。
   */
  function fillSelect(select, customInput, candidates, keep) {
    if (!select) return;
    const known = new Set(candidates);
    select.innerHTML = ['<option value="">不覆盖</option>']
      .concat(candidates.map(level => `<option value="${esc(level)}">${esc(level)}</option>`))
      .concat([`<option value="${CUSTOM_LEVEL}">自定义等级…</option>`])
      .join('');
    if (keep && known.has(keep)) select.value = keep;
    else if (keep) select.value = CUSTOM_LEVEL;
    else select.value = '';
    if (customInput) customInput.value = select.value === CUSTOM_LEVEL ? (keep || '') : '';
    syncCustom(select, customInput);
    window.wbSelect?.sync?.(select);
  }

  /** 自定义输入框只在「自定义等级」被选中时露出（并同步禁用态） */
  function syncCustom(select, customInput) {
    if (!select || !customInput) return;
    const isCustom = select.value === CUSTOM_LEVEL;
    customInput.hidden = !isCustom;
    customInput.disabled = !isCustom || select.disabled;
    if (isCustom) customInput.focus();
  }

  /**
   * 读当前选择 → 交给后端的那个值。
   *   返回 `''`（空串）= 显式清空绑定；返回等级字符串 = 设成它。
   * 永远不返回 `undefined`（那是「这一轮不改」，由调用方决定，见文件头）。
   */
  function valueOf(select, customInput) {
    if (!select) return '';
    if (select.value === CUSTOM_LEVEL) return (customInput?.value || '').trim();
    return select.value || '';
  }

  window.wbModelsReasoning = { CUSTOM_LEVEL, levels, buildIndex, badge, fillSelect, syncCustom, valueOf };
})();
