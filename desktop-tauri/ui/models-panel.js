/* Agent2API · 模型管理页：提供商导航 + 表格渲染 + 筛选 + 启停 + 自定义模型 + 模型映射（含映射开关）+ 刷新模型清单 */
/* global workbuddyDesktop, wbApp */

/**
 * ── 一个页面，两种数据源 ──────────────────────────────
 * 左栏（`.prov-rail`）是提供商导航：**内置提供商**一组（「全部」= 各家的聚合视图，
 * 各家是筛选视角）、**自定义提供商**一组（一家一项，一家一份清单）。选中谁，
 * 右栏就是谁家的模型表 —— 同一个表格骨架、同一批映射 chip 控件、同一套搜索与列设置。
 *
 * 两个数据源的差别只在取数与写入，渲染完全共用：
 *   · 内置家：`GET /api/models/manage`（`{models, mappings, reasoningLevels}`，含
 *     禁用的条目与关闭的映射），写操作逐条即时生效（桥接的具名方法，都返回最新的
 *     同形数据，就地替换后重绘）；
 *   · 自定义家：本地目录缓存（`wbProviders.customList`）里该家记录的 models /
 *     mappings，由 `models-custom-source.js` 适配成上面那个同形数据；写入是整表
 *     替换（每次操作读当前值 → 改一处 → 提交全量），于是体验与内置家一致 ——
 *     点一下立即生效，没有保存按钮，也没有草稿态。
 * 本模块自持内置家那份数据，不经过 app.js 的 state（那是 /api/session 的快照，
 * 轮询会整份覆盖）。
 *
 * 选中「全部」时表格按提供商分组（顺序 = 后端数组顺序 = 路由优先级），每组默认
 * 只展开前 `GROUP_LIMIT` 行，其余折叠成一行「展开其余 N 个」；选中单家时不分组
 * —— 标题已经写了是哪一家，再叠一条分组带是重复的。有搜索词或非「全部」的状态
 * 筛选时也不折叠：用户在找东西，藏起来只会让他以为没有。
 *
 * 映射（照抄 OmniProxy 的模型映射语义）：对外名自由命名（**允许**与上游模型
 * ID 同名 —— 同名时该上游的原生路由优先，映射是追加的兜底路，不产生遮蔽）；
 * 同一对外名可以在多个提供商各建一条（每行的映射 chips 只属于自己那行），
 * 下游用同一个名字请求时，网关在「原生承载家 + 各映射提供商」之间按账号
 * 全局优先级主备切换，发送时按承载家自动换成它认识的真名。
 *
 * 每条映射自带一个**开关**（chip 上的小滑块，参考 OmniProxy 的模型管理）：
 * 关掉 = 这条别名暂时不存在（不广告、不路由），可再打开；删除才是不可逆的。
 * 切换走 `/api/models/mappings`（只传 alias / target / provider / enabled，
 * 不带 reasoning —— 不动等级），与「模型行的启停开关」同一套乐观更新模式。
 *
 * ── 自定义模型（顶部「＋ 添加自定义模型」）─────────────────────
 * 手动登记一个「上游目录里没有、但实际能路由」的模型（灰度中的新模型、按账号
 * 下发却没进目录的模型）。登记后它**真的进入该家清单**（后端在
 * `catalog::manifest_for` 里拼接），于是表格里出现这一行、`/v1/models` 会广告
 * 它、路由与转发也都认它 —— 与内置模型相比只少几个能力位元数据（用户无从
 * 知道那些值，编一个等于对下游撒谎）。
 *
 * 它的「移除」是**直接移除这条登记**：内置模型的存在性由上游清单决定
 * （启停开关管的是「接不接请求」），自定义模型的存在性完全由这次登记决定，
 * 不要了就移除。操作列因此只剩这一种按钮（source=manual 的行才有）。
 * 「来源」列多一个 `manual` 值（后端逐条标出，前端只做文案映射）。
 *
 * ── 思考等级（照抄 OmniProxy 的手动绑定，R7）─────────────────
 * 每条映射可以带一个「思考等级」，值取自**后端下发的** `reasoningLevels`
 * （= `model_rules::REASONING_LEVELS`，与 OmniProxy 的
 * `GENERIC_REASONING_LEVELS` 同一张表；前端不自己抄一份，免得两处漂移），
 * 或表外的自定义值。等级显示在映射 chip 上（`alias → target · high`）。
 * **绑定会真的注入转发**：等级跟着它所在的这条映射走，由承载的那家适配器
 * 翻译成本家上游认识的档位字段（CatPaw 把通用 6 档归并成 low/high/max，Qoder
 * 按模型自己声明的档位归一）。**故意不注入**的几种情形（关闭思考 off/none、
 * 表外自定义值、客户端已显式指定、这家上游不认识档位字段）与理由写在
 * `core::model_rules::reasoning` 的模块头里，界面上有问号如实标注。
 *
 * 跨文件引用一律走 `wbApp`（esc / toast 是 app.js 里的全局单份实现）。
 */

(() => {
  const { esc, toast } = wbApp;
  const $ = id => document.getElementById(id);

  const GROUP_LIMIT = 8;
  /** 思考等级组件（chip 上的等级标 / 弹窗里的下拉）。住在 models-reasoning.js：
      那一块内部自洽（候选表 / 索引 / 那个下拉的读写），拆出去让本文件回到
      表格与映射本身。缺了它（脚本没加载）时下面几个调用点都退化成「不显示
      等级标」—— 看得到的是「少了个功能」，而不是整套面板报错。 */
  const reasoning = window.wbModelsReasoning;

  /** 当前数据（内置家；null = 还没拉到） */
  let data = null;
  let loading = false;
  /**
   * 左栏选中的提供商：`'all'`（内置全部家）/ 某个内置家 id / 某个 `custom-` id。
   * 初值来自上次会话的存盘（wbFilterMemory）—— 认不出的取值由 `renderRail` 的
   * 白名单校验兜底（那家可能已经被删了，回落「全部」）。
   */
  const FILTERS_KEY = 'workbuddy-desktop-models-filters';
  const savedFilters = window.wbFilterMemory
    ? window.wbFilterMemory.load(FILTERS_KEY, { provider: 'all', state: 'all', search: '' })
    : { provider: 'all', state: 'all', search: '' };
  const MODEL_STATES = ['all', 'enabled', 'disabled', 'mapped'];
  /** 状态分段控件上的标签（键与 MODEL_STATES 一一对应，顺序也照它） */
  const MODEL_STATE_LABEL = { all: '全部', enabled: '已启用', disabled: '已禁用', mapped: '有映射' };
  let currentProvider = savedFilters.provider;
  let stateFilter = MODEL_STATES.includes(savedFilters.state) ? savedFilters.state : 'all';
  /**
   * 搜索关键词。搜索框岛是**非受控**的（值由浏览器持有，打字零延迟），
   * 这里跟着 onInput 同步一份供筛选逻辑读 —— 不再从 DOM 反查。
   */
  let searchText = savedFilters.search || '';
  /** 已展开全部行的提供商集合 */
  const expanded = new Set();
  /** 行内操作在途标记：防同一行连点 */
  const pending = new Set();

  // ─── 列设置（显示 / 隐藏、顺序、对齐）──────────
  //
  // 本表的 <colgroup> 与 <thead> 写死在 index.html 里（不随数据重绘），所以列的
  // 顺序与显隐由 wbColSettings.syncStaticHead 就地**重排既有元素**，不按字符串重建
  // —— <col> 上带着拖出来的列宽、<th> 里插着列宽把手，重建会把两者一起丢掉。
  //
  // key 用表格里既有的 `data-col`（model / rate / source / alias / state / act）：
  // index.html 的 `<col class="c-xxx" data-col="xxx">`、表头 th、table-columns.js
  // 的列宽登记三处同名，键名只有一套。
  const COLUMNS = [
    { key: 'model', label: '上游模型' },
    { key: 'rate', label: '倍率' },
    { key: 'source', label: '来源' },
    { key: 'alias', label: '模型映射' },
    { key: 'act', label: '操作', align: 'right' },
  ];

  const colSettings = window.wbColSettings?.register({
    id: 'models',
    label: '模型管理表',
    columns: COLUMNS,
    mount: () => document.querySelector('.page[data-page="gateway"] .panel-head .head-actions'),
    // 排在那排操作按钮**之后**：这一排有明确主次 —— 添加 / 刷新是主操作，
    // 列设置是「怎么看这张表」的辅助开关，放末尾不会挤在主操作前面
    buttonPlacement: 'last',
    // 表头重排 + 数据行按新的列集合重画（两处读同一份配置，不会各画一个样）
    onChange: () => { syncHead(); render(); },
  });

  /**
   * 自定义家没有「倍率」「来源」这两个概念（它们是内置家清单的字段）：选中自定义家时
   * 这两列**按视图隐藏**，列设置里的配置本身不动 —— 切回内置家原样恢复。表头与数据行
   * 读同一份过滤结果（见 syncHead 的第三个参数与 visibleColumns），不会各画一个样。
   */
  const CUSTOM_HIDDEN_COLUMNS = new Set(['rate', 'source']);

  const syncHead = () => window.wbColSettings?.syncStaticHead(
    'models',
    document.querySelector('table.models-table:not(.keys-table)'),
    isCustomView() ? CUSTOM_HIDDEN_COLUMNS : null,
  );

  /** 该表当前可见的列（顺序即配置顺序；列设置未就绪时退回全部列） */
  const visibleColumns = () => {
    const columns = colSettings ? colSettings.apply(COLUMNS) : COLUMNS;
    if (!isCustomView()) return columns;
    return columns.filter(column => !CUSTOM_HIDDEN_COLUMNS.has(column.key));
  };

  /**
   * 跨整行的单元格（分组带 / 展开更多 / 空态 / 孤儿映射区）该跨几列。
   *
   * 必须跟着**可见列数**走：写死 6 之后，用户在列设置里藏起两列，这些整行
   * 单元格会比表体宽出两格 —— 多出来的格子把整张表顶出横向滚动，
   * 而滚动条一出现，吸顶表头与表体的对齐也跟着偏。
   */
  const span = () => visibleColumns().length;

  /** 把该列的对齐贴到单元格外壳上（与 accounts-table.js 的 withAlign 同一手法） */
  function withAlign(html, align) {
    const replaced = html.replace(/^<td class="([^"]*)"/, `<td class="$1 ta-${align}"`);
    return replaced === html ? `<td class="ta-${align}">${html}</td>` : replaced;
  }

  // ─── 数据 ─────────────────────────────────
  //
  // 两个数据源（见文件头）：内置家读 `data`（/api/models/manage），自定义家读
  // `customView`（由 models-custom-source.js 从目录缓存适配，render 开头重建一次）。
  // 下面所有取数都经过 viewData()，于是渲染 / 搜索 / 两个索引 / 弹窗候选自动跟着
  // 选中的家走，不必在每个消费点各判一次「这是哪一家」。

  /** 自定义家的数据源（models-custom-source.js）；脚本缺失时降级为「选不了自定义家」 */
  const customSource = window.wbModelsCustom;
  /** 当前自定义家的适配数据（`null` = 还没建 / 这家已不存在） */
  let customView = null;

  /** 当前选中项是不是自定义家 */
  function isCustomView() {
    return Boolean(customSource?.isCustom?.(currentProvider));
  }

  /** 按当前选中项重建自定义家视图（目录缓存是权威数据，重建很便宜） */
  function rebuildCustomView() {
    customView = isCustomView() ? customSource.buildView(currentProvider) : null;
  }

  const EMPTY_VIEW = { models: [], mappings: [] };
  function viewData() {
    if (isCustomView()) return customView || EMPTY_VIEW;
    return data || EMPTY_VIEW;
  }

  function models() { return Array.isArray(viewData().models) ? viewData().models : []; }
  function mappings() { return Array.isArray(viewData().mappings) ? viewData().mappings : []; }

  /**
   * 「三元组 → 思考等级」查询闭包（由 `models-reasoning.js` 建）。
   *
   * 必须**每次渲染前重建一次**（见 `rebuildReasoningIndex`）：数据换了索引就得
   * 跟着换，否则用户改完等级、列表重绘，chip 上还是旧的那个字。
   * 初值给一个恒返回空串的闭包 —— 在第一次 render 之前调用它（理论上不会，
   * 但弹窗是独立入口）也不会炸，只是显示成「未绑定」。
   */
  let reasoningOf = () => '';

  function rebuildReasoningIndex() {
    reasoningOf = reasoning?.buildIndex(mappings()) || (() => '');
  }

  /**
   * 「三元组 → 映射条目」查询闭包（chip 开关的 enabled 状态来源）。
   *
   * chip 的名字来自行上的 `m.aliases`（后端管理视图**全量**列出，含关闭的），
   * 而开关状态挂在顶层 `mappings` 的条目上 —— 与思考等级同一套查询方式：
   * 按 (alias, target, provider) 三元组查。口径与后端 `Mapping` 的命中规则
   * 对齐：**旧版全局条目（provider 缺失）对任何家都命中**（它们本来就显示在
   * 所有承载 target 的行上），行上 provider 总是非空，所以查询侧按
   * 「条目 provider 缺失或相等」判命中即可。
   *
   * 与 `rebuildReasoningIndex` 同一取舍：每次渲染前重建一次（数据换了索引就
   * 得跟着换），建成哈希查询而不是每个 chip 对全表 find —— render() 在搜索框
   * 每敲一个字就跑一次。
   */
  let mappingOf = () => undefined;

  function rebuildMappingIndex() {
    const exact = new Map();
    const global = new Map();
    const keyOf = (alias, target, provider) =>
      `${String(alias ?? '').trim().toLowerCase()}\u0001${String(target ?? '').trim().toLowerCase()}`
      + `\u0001${String(provider ?? '').trim().toLowerCase()}`;
    mappings().forEach(mapping => {
      const key = keyOf(mapping.alias, mapping.target, mapping.provider || '');
      (mapping.provider ? exact : global).set(key, mapping);
    });
    // 先查按家条目（更精确），未命中再看全局条目 —— 与后端展示口径一致
    mappingOf = (alias, target, provider) =>
      exact.get(keyOf(alias, target, provider)) || global.get(keyOf(alias, target, ''));
  }

  /** chip 上那枚映射开关（复用 state 列的 switch 结构，CSS 里有 chip 内的小号版）。
      `on` 是开关状态；关着的 chip 整体弱化（`map-off` class，见 page-gateway.css）。 */
  function chipSwitchHtml(alias, target, provider, on, busy) {
    return `<label class="switch" title="${on ? '映射已启用，点击关闭' : '映射已关闭，点击启用'}">`
      + `<input type="checkbox" data-act="map-toggle" data-alias="${esc(alias)}"`
      + ` data-target="${esc(target)}" data-provider="${esc(provider || '')}"`
      + `${on ? ' checked' : ''}${busy ? ' disabled' : ''}><span class="track"></span></label>`;
  }

  /** chip 上那枚等级标（转调 `models-reasoning.js`）。
      缺失该脚本时给空串 —— 少一枚可以点击的标，chip 名字与删除按钮照常，
      不整块报错（与 `reasoningOf` 的兜底同一取舍）。 */
  function badgeHtml(alias, target, provider, busy) {
    if (!reasoning) return '';
    return reasoning.badge({
      alias,
      target,
      provider,
      level: reasoningOf(alias, target, provider),
      busy,
    });
  }

  /** 提供商下拉选项（id + 展示名；按数据里出现的顺序去重） */
  function providerOptions() {
    const seen = new Map();
    models().forEach(m => {
      const key = m.provider || '';
      if (key && !seen.has(key)) seen.set(key, m.providerLabel || key);
    });
    return [...seen].map(([id, label]) => ({ id, label }));
  }

  /**
   * 某一家当前清单里的模型（映射弹窗的「上游模型」下拉数据源）。
   *
   * 含已禁用的行：映射是「名字 → 名字」的静态规则，与启停正交 ——
   * 用户完全可能先建好映射、之后才把那个模型打开。把它们藏起来会让
   * 「为什么我的模型不在下拉里」变成一个查不出的问题。
   * 排序：启用的在前（与表格分组内同一取舍），组内保持后端顺序。
   */
  function upstreamOptions(providerId) {
    if (!providerId) return [];
    return models()
      .filter(m => (m.provider || '') === providerId)
      .sort((a, b) => Number(a.enabled === false) - Number(b.enabled === false))
      .map(m => ({
        id: m.id,
        // 展示名与 id 不同才补在括号里，避免出现「GLM-5.3（GLM-5.3）」这种重复
        label: m.name && m.name !== m.id ? `${m.id}（${m.name}）` : m.id,
        off: m.enabled === false,
      }));
  }

  /** 在途加载的序号：force 重入时用它作废更早那次的响应（见 load 说明） */
  let loadSeq = 0;

  /**
   * 拉内置家的 manage 清单并重绘。
   *
   * `force` 供「切页刷新」这类**必须落地**的调用使用：默认情况下 `loading` 守卫
   * 会把重复调用合并掉，但切页时用户刚在账号页改过东西（加 / 删提供商），这次
   * 刷新被吞掉就等于整页没刷新 —— 所以 force 放行并发，并用序号保证**只有最后
   * 一次的结果生效**（先发的请求后回来时不覆盖新数据）。
   *
   * 失败也要重绘：左栏与自定义家的表格读的是本地目录缓存（providers.js），
   * 它们不该跟着这次网络失败一起停更（内置家的那份保持上一份成功结果）。
   */
  async function load({ force = false } = {}) {
    if (loading && !force) return;
    const seq = ++loadSeq;
    loading = true;
    try {
      const next = await workbuddyDesktop.getModelManage();
      // 期间若有更新的一次加载发起，本次结果作废
      if (seq !== loadSeq) return;
      data = next;
      render();
    } catch (error) {
      if (seq !== loadSeq) return;
      render();
      toast(`读取模型清单失败：${error.message}`, 'err');
    } finally {
      if (seq === loadSeq) loading = false;
    }
  }

  /**
   * 切到本页时的整页刷新（app.js 的 showPage 转发进来），两件事：
   *   1. **自定义提供商目录**（providers.js 的缓存）：左栏那组条目、每个家的
   *      模型清单与「这家还在不在」全读它。提供商是运行期数据，可能在账号页
   *      被添加 / 改名 / 删除，而这一页自持数据、不随主状态轮询更新；
   *   2. **内置家的 manage 清单**：上游目录可能被别处的「获取模型」刷新过。
   *
   * 顺序不能颠倒：目录缓存是自定义家的数据源，先刷它再重绘，左栏与表格才是
   * 同一份数据画出来的。目录刷新失败不阻断 —— 按现有缓存重绘，总比整页停更
   * 好（`load` 的 force 保证这次一定真的重拉并重绘）。
   */
  async function refreshAll() {
    try {
      await window.wbProviders?.refreshCustom?.();
    } catch { /* 目录偶发打不通：用现有缓存重绘，别把整页刷新拖没 */ }
    await load({ force: true });
  }

  /**
   * 写操作返回的数据就位。
   * 内置家的写接口都返回最新的 manage_view，直接替换；自定义家走整表提交、没有
   * 返回体（`next` 为 null）—— 它的新数据在 models-custom-source.js 里已经刷进
   * 目录缓存，这里重绘时 `rebuildCustomView()` 会读到新值。
   */
  function accept(next) {
    if (!isCustomView() && next && Array.isArray(next.models)) data = next;
    render();
  }

  // ─── 渲染 ─────────────────────────────────

  const formatCredits = c => {
    const m = /x\s*([\d.]+)/i.exec(c || '');
    return m ? `${m[1]}x` : (c || '');
  };

  function searchTerm() {
    return searchText.trim().toLowerCase();
  }

  /** 行的启停判定：还有任一条生效的绑定（默认或别名）就算启用，全部关闭才算
      禁用。「已启用 / 已禁用」筛选与组内排序共用这一份口径，避免两处各判各的。 */
  function rowEnabled(m) {
    return bindingsOf(m).some(binding => binding.enabled !== false);
  }

  function matches(m, keyword) {
    if (stateFilter === 'enabled' && !rowEnabled(m)) return false;
    if (stateFilter === 'disabled' && rowEnabled(m)) return false;
    if (stateFilter === 'mapped' && !(m.aliases || []).length) return false;
    if (currentProvider !== 'all' && (m.provider || '') !== currentProvider) return false;
    if (!keyword) return true;
    const hay = [m.id, m.name, ...(m.aliases || [])].join(' ').toLowerCase();
    return hay.includes(keyword);
  }

  /** 内置各家的 id → {label, n}。从**内置全量**（`data.models`）收集，不能用
      `models()` —— 后者跟着选中项走，选中某一家的那一刻其余各家的计数会全变 0。 */
  function builtinRailItems() {
    const counts = new Map();
    (Array.isArray(data?.models) ? data.models : []).forEach(m => {
      const key = m.provider || '';
      if (!key) return;
      const entry = counts.get(key) || { label: m.providerLabel || key, n: 0 };
      entry.n++;
      counts.set(key, entry);
    });
    return counts;
  }

  /**
   * 左栏当前展示的内置家 id（有清单的家）——「获取模型」弹窗据此组装刷新范围
   * （见 models-fetch-modal.js 的 scopeProviders）：模型管理页看不到的家不该
   * 出现在刷新结果里。
   */
  const builtinProviders = () => [...builtinRailItems().keys()];

  /**
   * 选中项白名单校验：存盘里记的那家可能已经被删了（或内置清单这次没拉到它），
   * 认不出就回落「全部」—— 否则右栏会是一张永远空的表，而用户找不到原因。
   */
  function normalizeSelection() {
    if (currentProvider === 'all') return;
    if (builtinRailItems().has(currentProvider)) return;
    if ((customSource?.list?.() || []).some(provider => provider.id === currentProvider)) return;
    currentProvider = 'all';
  }

  /**
   * 左栏：内置提供商（全部 + 各家）+ 自定义提供商（每家 + 新建）。
   *
   * 自定义家的计数读目录缓存里该家的 `models` 数组长度，与「这家有没有账号」无关
   * —— 建了提供商但账号被删光时，清单照样要能管（与账号页管理弹窗同一条口径）。
   *
   * 自定义家条目额外挂一颗**删除按钮**（hover 才显形，见 .pv-del 的样式）。
   * 为什么在这里补这个入口：提供商级的编辑 / 删除唯一入口是账号设置弹窗，而它
   * 挂在账号行上 —— 名下账号被删光的提供商因此没有任何界面能删掉它（见
   * custom-provider-ui.js 的「已知的可达性缺口」），只剩这一页左栏还看得见它。
   * 删除动作本身仍走 wbCustomProvidersUi.remove（二次确认 + 级联删账号 + 目录
   * 刷新，与账号设置弹窗里那颗按钮同一条路），这里只负责把本页重画一遍。
   */
  function renderRail() {
    const rail = $('prov-rail');
    if (!rail) return;
    const counts = builtinRailItems();
    const customs = customSource?.list?.() || [];
    const total = [...counts.values()].reduce((sum, item) => sum + item.n, 0);
    const item = (id, label, n, { removable = false } = {}) => {
      const pick = `<button type="button" class="pv${currentProvider === id ? ' on' : ''}"`
        + ` data-provider="${esc(id)}" title="${esc(label)}">`
        + `<span class="nm">${esc(label)}</span><span class="n">${n}</span></button>`;
      if (!removable) return pick;
      // 删除按钮不能嵌进 .pv（button 套 button 是无效 HTML，解析器会把内层
      // 提到外面、绝对定位跟着失去参照），所以外面套一层定位容器
      return `<div class="pv-row">${pick}`
        + `<button type="button" class="pv-del" data-del-provider="${esc(id)}"`
        + ` title="删除这个自定义提供商（连同名下账号）" aria-label="删除自定义提供商">×</button></div>`;
    };
    rail.innerHTML = '<div class="rail-label">内置提供商</div>'
      + item('all', `全部（${counts.size} 家）`, total)
      + [...counts].map(([key, entry]) => item(key, entry.label, entry.n)).join('')
      + '<div class="rail-label">自定义提供商</div>'
      + (customs.length
        ? customs.map(provider => item(
          provider.id,
          provider.name || provider.id,
          Array.isArray(provider.models) ? provider.models.length : 0,
          { removable: true },
        )).join('')
        : '<div class="rail-empty">还没有自定义提供商</div>')
      + '<button type="button" class="pv-add" id="rail-add-custom"'
      + ' title="新建一个自定义提供商（同时创建它的第一个账号）">＋ 新建自定义提供商</button>';
  }

  /** 映射 chips（照抄 OmniProxy）：每条 chip 属于自己所在的那一行（提供商 ×
      上游模型），删除时带三元组精确定位 —— 同一对外名在多行出现是主备关系。
      chip 上现在有两枚控件 + 一枚等级标：开关（`data-act="map-toggle"`，
      关掉的映射 = 这条别名暂时不存在，可再打开）与删除 ×；绑了思考等级的
      chip 在名字后面挂一枚可点的小标（`· high`），点它打开映射弹窗改等级
      （alias / target / provider 是它的身份，改了就变成另一条映射）。
      行禁用时 chips 随行压淡（`off`），映射自己的开关另用 `map-off` 弱化 ——
      两个维度独立：行开着、映射关着的状态必须一眼可辨。 */
  function bindingsOf(model) {
    const provider = model.provider || '';
    const same = (left, right) => String(left || '').toLowerCase() === String(right || '').toLowerCase();
    const bindings = mappings().filter(mapping => same(mapping.target, model.id)
      && (!mapping.provider || same(mapping.provider, provider)));
    const unique = new Map();
    for (const binding of bindings) {
      const key = String(binding.alias).toLowerCase();
      if (!unique.has(key) || binding.provider) unique.set(key, binding);
    }
    const idKey = String(model.id).toLowerCase();
    const defaults = unique.get(idKey);
    unique.delete(idKey);
    return [{ ...defaults, alias: model.id, target: model.id, provider,
      enabled: model.enabled !== false && defaults?.enabled !== false, isDefault: true },
    ...unique.values()];
  }

  function aliasChips(m) {
    const provider = m.provider || '';
    const chip = binding => {
      const alias = binding.alias;
      const on = binding.enabled !== false;
      const busy = pending.has(`${alias}:${m.id}:${provider}`);
      const label = binding.isDefault ? '<span class="binding-default">默认</span>' : '';
      const remove = binding.isDefault ? ''
        : `<button type="button" class="x" data-act="unmap" data-alias="${esc(alias)}" data-target="${esc(m.id)}" data-provider="${esc(provider)}" title="删除映射 ${esc(alias)}"${busy ? ' disabled' : ''}>×</button>`;
      return `<span class="alias${on ? '' : ' map-off'}">`
        + chipSwitchHtml(alias, m.id, provider, on, busy)
        + `<span class="t" title="${esc(alias)}">${esc(alias)}</span>${label}`
        + badgeHtml(alias, m.id, provider, busy) + remove + '</span>';
    };
    // 「＋ 映射」并排跟在**默认**那条右边，不单独占一行：它是这一列的入口，
    // 不是一条绑定（绑定的排布仍是一条一行，见 .aliases 的注释）。默认绑定
    // 永远存在（没有映射时是合成出来的那条），所以这一行永远有内容；
    // 窄窗口下它会自己折到第二行，不挤扁 chip。
    const [head, ...rest] = bindingsOf(m);
    const add = `<button type="button" class="alias-add" data-act="map" data-id="${esc(m.id)}" data-provider="${esc(provider)}">＋ 映射</button>`;
    return `<div class="aliases">`
      + `<div class="alias-row">${head ? chip(head) : ''}${add}</div>`
      + rest.map(chip).join('')
      + '</div>';
  }

  /**
   * 各列的单元格（不含 `<td>` 外壳；入参统一为 `(m, busyRow)`）。
   *
   * 放在一个表里而不是行内联的三元链，是为了让 `row()` 只回答「按哪些列、什么顺序」
   * —— 列一多，那种链式拼接读起来要先数逗号才知道哪个 td 属于哪一列；
   * 而「某一列长什么样」只有一处实现，列设置重排时才不会各画一个样。
   */
  const CELLS = {
    model: m => {
      const name = m.name && m.name !== m.id ? `<div class="mname">${esc(m.name)}</div>` : '';
      return `<td class="cell-model"><div class="mid"><span class="t">${esc(m.id)}</span>`
        + `<button type="button" class="cp" data-copy="${esc(m.id)}" title="复制模型 ID">⧉</button></div>${name}</td>`;
    },
    rate: m => `<td class="cell-rate">${m.credits
      ? `<span class="rate">${esc(formatCredits(m.credits))}</span>`
      : '<span class="rate">—</span>'}</td>`,
    source: m => `<td class="cell-source">${sourceCell(m)}</td>`,
    alias: m => `<td class="cell-alias">${aliasChips(m)}</td>`,
    // 操作列只移除「可移除的」：内置家里是手动登记的那些（source=manual，上游
    // 目录带来的行由清单决定存在性，开关才是它的手段），自定义家里每一行都是
    // 用户自己登记进来的、都可以移除。对外名称统一在模型映射列切换。
    act: (m, busyRow) => {
      const removable = m.source === 'manual' || isCustomView();
      return '<td class="cell-act r"><div class="row-actions">'
        + (removable
          ? `<button type="button" class="sm ghost danger-text" data-act="hide" data-id="${esc(m.id)}" data-provider="${esc(m.provider || '')}"${busyRow ? ' disabled' : ''}>移除</button>`
          : '')
        + '</div></td>';
    },
  };

  function row(m) {
    const busyRow = pending.has(rowKey(m));
    return `<tr data-id="${esc(m.id)}" data-provider="${esc(m.provider || '')}">`
      + visibleColumns().map(column => withAlign(CELLS[column.key](m, busyRow), column.align)).join('')
      + '</tr>';
  }

  /** 「来源」列：这一家的清单当前是远程拉的还是内置静态表（后端给的 `source`）。
      它是**家**级属性（同一家所有行同值），前端只做文案映射与样式，不自己推断。
      认不出的值显示破折号：后端没给 `source`（旧版网关）时不该硬说「内置」。

      唯一的**条**级例外是 `manual`：用户手动登记的自定义模型（见顶部
      「＋ 添加自定义模型」）。它不属于该家清单的任何一种来源，后端逐条标出来，
      前端照实显示。

      「远程」还带一个 `refreshedAt`（后端给的毫秒时间戳）：进程重启后清单可能
      是**从持久化缓存恢复**的，与刚拉到的都是「远程」，时效只能靠这个时间戳
      说明 —— 没有它，用户没法判断手上这份是不是几天前的。 */
  function sourceCell(m) {
    if (m.source === 'manual') {
      return '<span class="badge tag brand" title="手动登记的上游模型；移除它会直接删掉这条登记">手动</span>';
    }
    if (m.source !== 'remote' && m.source !== 'builtin') return '<span class="rate">—</span>';
    const remote = m.source === 'remote';
    // `formatTime` 来自 app.js（全局；0 或非法值返回空串，这里据此省略那半句）
    const at = formatTime(Number(m.refreshedAt) || 0);
    const hint = remote
      ? `来自上游目录接口${at ? `，清单拉取于 ${at}` : ''}；刷新失败时保留上一份成功结果`
      : '上游目录尚未拉到，用的是内置静态清单；点「刷新模型清单」可重试';
    return `<span class="badge tag${remote ? ' brand' : ''}" title="${hint}">${remote ? '远程' : '内置'}</span>`;
  }

  /** 行内操作的防重入键：同名模型在多家同时存在时，`id` 不足以定位一行 */
  function rowKey(m) {
    return `${m.provider || ''}:${m.id}`;
  }

  /**
   * 随选中项变化的那几处文案：三颗按钮的文案与提示、表尾。
   * 与数据无关，所以独立成一个函数，由 render 与切换提供商两处调用。
   */
  function paintViewChrome() {
    const custom = isCustomView();
    const addButton = $('btn-add-custom-model');
    if (addButton) addButton.textContent = '＋ 添加模型';
    const refreshButton = $('btn-refresh-models');
    if (refreshButton) {
      // 文案与 title 只有一处事实来源（这里），index.html 里不写死
      refreshButton.textContent = '获取模型';
      refreshButton.title = custom
        ? '从这一家的上游拉一份模型清单，勾选要哪些再导入（已添加的不会重复导入）'
        : '刷新模型管理页里各提供商的远程模型目录，逐家结果列在弹窗里';
    }
    const foot = $('models-panel-foot');
    if (foot) {
      foot.innerHTML = custom
        ? '自定义提供商的清单<b>只属于这一家</b>：这里的模型不会出现在其他家，别名也只在这一家内生效。改名称 / 协议 / Base URL 在账号页该家账号的「设置」→ 提供商一栏；整家不要了，鼠标移到左栏这家上点 × 删除（连同名下账号）。'
        : '「默认」绑定就是原始模型 ID，只可开关、不可删除。原始 ID 与每个别名独立生效：关闭的名称不出现在 <code>/v1/models</code>，下游请求返回 404；其他开启的名称不受影响。';
    }
  }

  function render() {
    const body = $('models');
    if (!body) return;
    // 选中项先校验（那家可能已经被删 / 内置清单这次没拉到它），再按它重建
    // 自定义家的视图，最后画左栏与随选中项变的文案
    normalizeSelection();
    rebuildCustomView();
    renderRail();
    paintViewChrome();
    // 重建两个索引（思考等级 + 映射开关，每次渲染一次，见各自的 rebuild 说明）
    rebuildReasoningIndex();
    rebuildMappingIndex();
    const all = models();
    const keyword = searchTerm();
    const shown = all.filter(m => matches(m, keyword));
    if (!all.length) {
      // 一条模型都没有：内置家是「还没加账号」，自定义家是「还没登记 / 还没拉取」。
      // 说清下一步该做什么才有用
      const empty = isCustomView()
        ? (customView ? '这家还没有模型：点「添加模型」登记，或「获取上游模型」从上游拉取' : '该提供商已不存在（可能已被删除），请刷新列表')
        : (data ? '暂无模型（请先添加账号）' : '加载中…');
      body.innerHTML = `<tr><td colspan="${span()}" class="empty">${empty}</td></tr>`;
      return;
    }
    if (!shown.length) {
      body.innerHTML = `<tr><td colspan="${span()}" class="empty">没有匹配${keyword ? `「${esc(keyword)}」` : '当前筛选'}的模型</td></tr>`;
      return;
    }
    // 分组带只在「全部」视图里出现：选中单家时标题已经写了是哪一家，再叠一条
    // 「WorkBuddy 3 个模型」是重复的。折叠同理 —— 它是「全部」视图里控制长度的
    // 手段（见文件头），且只在无搜索、全部状态下生效
    const showGroups = currentProvider === 'all';
    const collapsible = showGroups && !keyword && stateFilter === 'all';
    const groups = new Map();
    shown.forEach(m => {
      const key = m.provider || '';
      if (!groups.has(key)) groups.set(key, { label: m.providerLabel || key || '未知', items: [] });
      groups.get(key).items.push(m);
    });
    // 组内排序：禁用的行沉到该组末尾，启用的排前面；Array#sort 稳定，组内
    // 仍按后端原序（判定与「已启用 / 已禁用」筛选同一份，见 rowEnabled）
    const disabledRank = m => Number(!rowEnabled(m));
    groups.forEach(group => group.items.sort((a, b) => disabledRank(a) - disabledRank(b)));
    body.innerHTML = [...groups].map(([key, group]) => {
      const open = expanded.has(key) || !collapsible;
      const items = open ? group.items : group.items.slice(0, GROUP_LIMIT);
      const rest = group.items.length - items.length;
      const head = showGroups
        ? `<tr class="tr-group"><td colspan="${span()}"><span class="prov-tag">${esc(group.label)}</span>${group.items.length} 个模型</td></tr>`
        : '';
      const more = rest > 0
        ? `<tr class="tr-more"><td colspan="${span()}"><button type="button" class="sm ghost" data-act="expand" data-provider="${esc(key)}">展开其余 ${rest} 个模型 ▾</button></td></tr>`
        : (open && collapsible && group.items.length > GROUP_LIMIT
          ? `<tr class="tr-more"><td colspan="${span()}"><button type="button" class="sm ghost" data-act="collapse" data-provider="${esc(key)}">收起 ▴</button></td></tr>`
          : '');
      return head + items.map(row).join('') + more;
    }).join('');
  }

  // ─── 行内操作 ────────────────────────────

  // 写操作分派：内置家的四个写动作走桥接的具名方法（逐条即时生效，返回最新
  // manage_view），自定义家走整表提交（models-custom-source.js，返回 null —— 它的
  // 新数据已经刷进目录缓存，重绘时读得到）。调用点只认这四个函数，不必各自判断
  // 「当前是哪一家」；`patch` 里没给的字段保持现值（两边的语义逐字对齐）。

  /** 开关 / 新增 / 改一条绑定（alias == target 时即该模型的默认绑定） */
  function writeBinding(provider, alias, target, patch) {
    if (isCustomView()) return customSource.setBinding(provider, alias, target, patch);
    return workbuddyDesktop.addModelMapping(alias, target, provider, patch.reasoning, patch.enabled);
  }

  function writeRemoveMapping(provider, alias, target) {
    if (isCustomView()) return customSource.removeMapping(provider, alias, target);
    return workbuddyDesktop.removeModelMapping(alias, target, provider);
  }

  function writeAddModel(provider, id) {
    if (isCustomView()) return customSource.addModel(provider, id);
    return workbuddyDesktop.addCustomModel(provider, id);
  }

  function writeRemoveModel(provider, id) {
    if (isCustomView()) return customSource.removeModel(provider, id);
    return workbuddyDesktop.removeCustomModel(provider, id);
  }

  /**
   * 行内操作执行器。`key` 是防重入标记（提供商:模型 id）——同名模型在多家
   * 同时存在，只按 id 记会把两家的行一起标成「执行中」。
   */
  async function runRowAction(key, run, doneText) {
    if (pending.has(key)) return;
    pending.add(key);
    render();
    try {
      accept(await run());
      if (doneText) toast(doneText);
    } catch (error) {
      toast(`操作失败：${error.message}`, 'err');
      render();
    } finally {
      pending.delete(key);
      render();
    }
  }

  async function onTableClick(event) {
    const button = event.target.closest('[data-act]');
    if (!button) return;
    const { act, id, alias, target, provider } = button.dataset;
    // 同一模型 id 在多家同时存在时（如 kimi-k3 同时由 CatPaw 与小浣熊提供），
    // 操作都要带上提供商才能精确到一行
    const key = `${provider || ''}:${id}`;
    if (act === 'expand') { expanded.add(provider); render(); return; }
    if (act === 'collapse') { expanded.delete(provider); render(); return; }
    if (act === 'hide') {
      // 能走到这里的只剩手动登记的自定义模型（source=manual，见 CELLS.act）：
      // 「移除」是删掉那条**登记**，不是隐藏 —— 它的存在完全由这次登记决定，
      // 没有「上游刷新会把它带回来」这回事，移除后 /v1/models、路由同时消失。
      // 判据用后端给的 `source`，前端不自己推断（见 sourceCell）。
      if (!(await window.wbConfirm?.ask?.({
        title: '移除自定义模型',
        html: `确定移除自定义模型「<strong>${esc(id)}</strong>」？这条登记会被<b>直接移除</b>，之后 <code>/v1/models</code> 不再广告它、请求它也会被拒。`,
        okText: '移除',
        okClass: 'danger',
      }))) return;
      void runRowAction(
        key,
        () => writeRemoveModel(provider, id),
        isCustomView() ? '模型已移除' : '自定义模型已移除',
      );
      return;
    }
    if (act === 'unmap') {
      // 同名映射允许多条，删除按（对外名 + 上游模型 + 提供商）三元组定位
      if (!(await window.wbConfirm?.ask?.({
        title: '删除映射',
        html: `确定删除映射「<strong>${esc(alias)} → ${esc(target)}</strong>」？`,
        okText: '删除',
        okClass: 'danger',
      }))) return;
      void runRowAction(
        `${alias}:${target}:${provider || ''}`,
        () => writeRemoveMapping(provider, alias, target),
        '映射已删除',
      );
      return;
    }
    if (act === 'map') openMapping({ target: id, provider });
    // 点 chip 上的等级标：只改这条映射的思考等级（alias 也在上下文里，弹窗据此
    // 进入锁定形态）。不用先弹确认框 —— 它只写一个字段，保存前还能取消。
    if (act === 'reasoning' && alias && target) {
      openMapping({ alias, target, provider });
    }
  }

  function onTableChange(event) {
    const input = event.target.closest('input[data-act]');
    if (!input) return;
    const { act, provider, alias, target } = input.dataset;
    // 映射 chip 上的开关：按三元组定位那条映射，只传 enabled 不带 reasoning
    // （后端三态协议：不带 reasoning 不动等级）。走与行内操作同一套
    // runRowAction：提交中禁用、失败 toast 后重绘即恢复原状态（data 未变）。
    if (act === 'map-toggle') {
      const enabled = input.checked;
      void runRowAction(
        `${alias}:${target}:${provider || ''}`,
        () => writeBinding(provider, alias, target, { enabled }),
        enabled ? '映射已启用' : '映射已关闭',
      );
    }
  }

  // ─── 映射弹窗（照抄 OmniProxy 的模型映射）────────────────

  let mappingSaving = false;
  /** 行内打开时锁定的上下文；顶部按钮打开时为 null。三种入口：
   *  `{target, provider}`（行内「＋映射」）、`{alias, target, provider}`（点 chip
   *  上的等级标，只改等级）、以及不带上下文（顶部「添加映射」）。 */
  let mappingContext = null;

  /** 当前选中的上游模型（下拉值） */
  function upstreamValue() {
    return ($('mapping-upstream')?.value || '').trim();
  }

  /**
   * 把「该家的模型清单」灌进上游下拉（打开时、切换提供商时都走它）。
   *
   * `keep` 不在候选里时**仍把它补进去**（而不是像早先那样退回首项），
   * 但**只在锁定态**（行内入口 / 改等级形态）这么做：
   * 孤儿映射的 target 恰恰常常不在该家清单里（那正是它挂不上行的原因），
   * 而它照样有自己的思考等级要改。丢掉 keep 会让弹窗里显示「这一家的第一个
   * 模型」，用户在「设置思考等级」形态下点保存，三元组就从 (alias, target)
   * 变成 (alias, 另一个模型) —— 命中的是另一条规则（或新建一条），
   * 等级存到了错的地方，而界面上看不出任何异常。
   *
   * 解锁态（顶部「添加映射」自选提供商）不补：那里用户刚换了一家，
   * 上一家的 target 对新家毫无意义，退回首项才是他要的。
   */
  function fillUpstreamSelect(providerId, keep, locked = false) {
    const select = $('mapping-upstream');
    if (!select) return;
    const options = upstreamOptions(providerId);
    const wanted = (keep || '').trim();
    const has = id => options.some(item => item.id.toLowerCase() === id.toLowerCase());
    if (locked && wanted && !has(wanted)) {
      options.unshift({
        id: wanted,
        label: `${wanted}（不在该家当前清单里）`,
        off: true,
      });
    }
    select.innerHTML = options.map(item =>
      `<option value="${esc(item.id)}">${esc(item.label)}${item.off ? '（已禁用）' : ''}</option>`).join('');
    // 记住用户已经选过的那个：切换提供商再切回来时不该被重置
    if (wanted && has(wanted)) {
      // 用候选里的原始拼写（大小写可能与 keep 不同）：value 必须与 option 的
      // value 逐字相同才会被选中
      select.value = options.find(item => item.id.toLowerCase() === wanted.toLowerCase())?.id || wanted;
    } else {
      select.value = options[0]?.id || '';
    }
    window.wbSelect?.sync?.(select);
  }

  /** 弹窗里那个思考等级下拉的两个元素（readonly，不缓存 DOM 引用之外的任何状态） */
  const reasoningSelect = () => $('mapping-reasoning');
  const reasoningCustom = () => $('mapping-reasoning-custom');

  function fillReasoningSelect(keep) {
    reasoning?.fillSelect(reasoningSelect(), reasoningCustom(), reasoning.levels(data), keep);
  }

  function syncReasoningCustom() {
    reasoning?.syncCustom(reasoningSelect(), reasoningCustom());
  }

  /** 当前选择 → 交给后端的值（`''` = 显式清空；永不为 undefined，见文件头） */
  function reasoningValue() {
    return reasoning?.valueOf(reasoningSelect(), reasoningCustom()) || '';
  }

  function mappingPreview() {
    const alias = $('mapping-alias')?.value.trim() || '<对外名>';
    const upstream = upstreamValue() || '<上游模型>';
    const provider = $('mapping-provider')?.value;
    const label = providerOptions().find(item => item.id === provider)?.label || provider || '(全局)';
    const level = reasoningValue();
    const suffix = level ? ` · 思考等级 <b>${esc(level)}</b>` : '';
    $('mapping-preview').innerHTML = `下游请求 <b>${esc(alias)}</b> → 转发 <b>${esc(upstream)}</b>（${esc(label)}）${suffix}`;
  }

  /**
   * 打开映射弹窗。
   * `context` 是行内入口带的上下文（提供商 + 上游模型锁定，只填对外名）。
   * 入口只剩行内两处 —— 行尾「＋ 映射」新建、点别名 chip 改等级 —— 所以
   * context 总是带着这一行的身份；不传 context 的形态（提供商与上游模型都
   * 自己选）随顶部那颗「＋ 添加映射」一起移除了。
   *
   * 上游模型**只能是下拉**（数据来自该家当前清单）。这里曾经放开过「手动输入
   * 上游模型 ID」，已删除：对外名只有在**目标模型已被广告**时才会跟着进广告视图
   * （`catalog::models_response` 是「遍历已广告的模型 → 补它的别名」这个方向），
   * 而入口校验以广告视图为准 —— 手输一个清单里没有的名字，映射建了也永远调不通
   * （实测 400 `model_not_found`），只会让用户以为配好了。
   *
   * 要用清单外的模型，正确做法是让那家的清单收录它 —— 现在有正规入口了：
   * 顶部的「＋ 添加自定义模型」（见 `openCustomModel`）会把它真的登记进该家清单，
   * 之后它自然出现在这里的下拉里。**在映射里手输名字这条路仍然不开**：
   * 那是绕过清单，而登记是补充清单，两者只在后者才真正可路由。
   *
   * `context.alias` 有值时走「只改这条映射的思考等级」形态：alias 与 target
   * 都是那一条的身份，全部锁定，只留等级可动。共用一个弹窗而不是另开一个
   * 「设置等级」的小窗：两者要填的字段完全重合，独立窗口只会让「等级」和
   * 「映射」在界面语言里变成两件不相干的事，而它们本就是一条记录。
   */
  function openMapping(context) {
    mappingContext = context || null;
    // 索引在这里再建一次：本函数是**唯一**读 `reasoningOf` 的地方，而它可能被
    // 非渲染路径调到（行内点击、将来的快捷键）。重建是一遍 Map 填充（几十到
    // 上百项），比「依赖 render() 刚跑过」这条隐式前提划算得多 ——
    // 那个前提一旦不成立，表现是「弹窗里的等级是空的」，而保存时会把用户
    // 已有的绑定静默清掉。
    rebuildReasoningIndex();
    const locked = Boolean(mappingContext);
    /** 改等级形态：alias 也是锁定的（它来自 chip，就是那条映射的对外名） */
    const editing = Boolean(mappingContext?.alias);
    /** 编辑形态的提供商：它就是那条映射的属性，必须能在下拉里选中 */
    const contextProvider = mappingContext?.provider || '';
    const options = providerOptions();
    // ── 为什么要把上下文里那家补进候选（`providerOptions` 收不全）──────
    // `providerOptions()` 是从**表格行**里收集的（有行才有这家），而孤儿映射
    // 恰恰常常属于「整个没进表格」的家（那家没加账号，一个行都没有）。
    // 不补的话 `providerSelect.value = provider` 会静默落到空串
    //（把 value 设成不存在的选项 = 不选中任何项），用户在「设置思考等级」里
    // 点保存就会把 provider 一起发成空 —— 三元组一变，命中的是**另一条**规则
    //（或新建一条 provider 为 null 的旧版全局条目），等级也就存到了错的地方。
    if (contextProvider && !options.some(item => item.id === contextProvider)) {
      options.push({
        id: contextProvider,
        // 展示名走注册表（`wbProviders.labelOf`，查不到原样回显 id），
        // 不在这里另写一份 id → 名字的映射
        label: window.wbProviders?.labelOf?.(contextProvider) || contextProvider,
      });
    }
    const providerSelect = $('mapping-provider');
    providerSelect.innerHTML = options.map(item =>
      `<option value="${esc(item.id)}">${esc(item.label)}</option>`).join('');
    const provider = contextProvider || options[0]?.id || '';
    providerSelect.value = provider;
    // 锁定 = 行内入口：提供商与上游模型就是这一行，不允许改（改了就变成另一条映射）
    providerSelect.disabled = locked;
    window.wbSelect?.sync?.(providerSelect);

    const select = $('mapping-upstream');
    // 行内入口的 target 就是这一行的模型，必在清单里（那行就来自清单），
    // 所以直接按 keep 灌即可；万一清单在这期间刷新过、目标已不在，仍由
    // fillUpstreamSelect 退回首项 —— 但那种情况上游下拉是禁用的，
    // 用户看到的是「这一行的模型」，不会被误导成别的选择
    // 锁定态的 keep 一定要保住（第三个参数 true，理由见 fillUpstreamSelect）
    fillUpstreamSelect(provider, locked ? mappingContext.target : '', locked);
    // 上游下拉在锁定态也不可改：它就是这一行
    select.disabled = locked;
    window.wbSelect?.sync?.(select);

    const aliasInput = $('mapping-alias');
    aliasInput.value = editing ? mappingContext.alias : '';
    aliasInput.disabled = editing;
    // 等级回填：改等级形态用 chip 上那条映射的现值；新建形态一律「不覆盖」
    //（照抄 OmniProxy 的「no override」默认值 —— 不替用户绑一个他没选的档位）
    const current = editing ? reasoningOf(mappingContext.alias, mappingContext.target, provider) : '';
    fillReasoningSelect(current);

    $('mapping-modal-status').textContent = '';
    $('mapping-modal-title').textContent = editing
      ? '设置思考等级'
      : locked ? '添加模型映射' : '添加模型映射（自选提供商与上游）';
    $('mapping-modal-save').textContent = editing ? '保存等级' : '保存映射';
    mappingPreview();
    $('mapping-modal').classList.add('open');
    setTimeout(() => {
      // 改等级形态没别的可填，把焦点直接放在等级下拉上
      if (editing) $('mapping-reasoning')?.focus();
      else aliasInput.focus();
    }, 0);
  }

  function closeMapping() {
    if (mappingSaving) return;
    $('mapping-modal').classList.remove('open');
  }

  async function saveMapping() {
    if (mappingSaving) return;
    const editing = Boolean(mappingContext?.alias);
    const alias = editing ? mappingContext.alias : $('mapping-alias').value.trim();
    const target = upstreamValue();
    const provider = $('mapping-provider').value;
    const reasoning = reasoningValue();
    const status = $('mapping-modal-status');
    if (!alias) { status.textContent = '请填写对外映射名'; return; }
    // 下拉为空 = 这一家清单里一个模型都没有（还没加账号 / 清单没拉到）
    if (!target) { status.textContent = '该提供商当前没有可选的上游模型'; return; }
    if (!provider) { status.textContent = '请选择提供商'; return; }
    const same = (a, b) => String(a || '').toLowerCase() === String(b || '').toLowerCase();
    if (!editing && same(alias, target) && models().some(model => same(model.id, target) && same(model.provider, provider))) {
      status.textContent = '原始 ID 已作为默认绑定，请直接使用该绑定的开关或等级按钮';
      return;
    }
    mappingSaving = true;
    $('mapping-modal-save').disabled = true;
    status.textContent = '保存中…';
    try {
      // `reasoning` **总是显式给出**（空串 = 清空绑定）：
      // 「三元组相同」走的也是这条接口，而用户在这个弹窗里看到的就是他要的结果 ——
      // 传 undefined（= 不改）会让「从 high 改成不覆盖」这一步静默无效。
      accept(await writeBinding(provider, alias, target, { reasoning }));
      mappingSaving = false;
      closeMapping();
      const suffix = reasoning ? ` · 思考等级 ${reasoning}` : '';
      // 展示名走注册表 / 自定义目录（`wbProviders.labelOf`）：直接印 provider id
      // 时，自定义家会显示成一串 `custom-3f2a91b04c7e`，用户认不出是哪一家
      const providerLabel = window.wbProviders?.labelOf?.(provider) || provider;
      toast(editing ? `✅ 已更新 ${alias} 的思考等级` : `✅ 已添加映射 ${alias} → ${target}（${providerLabel}）${suffix}`);
    } catch (error) {
      status.textContent = `保存失败：${error.message}`;
    } finally {
      mappingSaving = false;
      $('mapping-modal-save').disabled = false;
    }
  }

  // ─── 自定义模型弹窗 ───────────────────────────

  let customSaving = false;

  /** 自定义模型弹窗的提供商候选。
   *
   *  与映射弹窗的 `providerOptions()` **刻意不同**：那个只列「表格里出现过的家」
   *  （因为映射必须挂到一行上），而这里要列**全部可登记的家** —— 用户完全可能
   *  先给还没登录的家配好模型清单，等加上账号就生效。`wbProviders.all()` 读的是
   *  `/api/session` 的 `accounts.providers`（注册表全量，含 count=0 的家），
   *  自定义家再补上（它们不在注册表摘要里，见 providers.js）。
   *
   *  退化路径：`wbProviders` 没加载时回落到表格里出现过的家（少几个选项，
   *  但不会让弹窗空着打不开）。 */
  function customProviderOptions() {
    const options = [];
    const builtin = window.wbProviders?.all?.();
    if (Array.isArray(builtin)) {
      options.push(...builtin.map(item => ({ id: item.id, label: item.label || item.id })));
    }
    for (const provider of (customSource?.list?.() || [])) {
      options.push({ id: provider.id, label: provider.name || provider.id });
    }
    return options.length ? options : providerOptions();
  }

  function fillCustomProvider(keep) {
    const select = $('custom-model-provider');
    if (!select) return;
    const options = customProviderOptions();
    select.innerHTML = options.map(item =>
      `<option value="${esc(item.id)}">${esc(item.label)}</option>`).join('');
    const wanted = (keep || '').trim();
    if (wanted && options.some(item => item.id === wanted)) {
      select.value = wanted;
    } else if (options.length) {
      select.value = options[0].id;
    }
    window.wbSelect?.sync?.(select);
  }

  function customModelPreview() {
    const provider = $('custom-model-provider')?.value || '';
    const id = $('custom-model-id')?.value.trim() || '<上游模型 ID>';
    const label = customProviderOptions().find(item => item.id === provider)?.label || provider || '(未选)';
    $('custom-model-preview').innerHTML =
      `在 <b>${esc(label)}</b> 上登记上游模型 <b>${esc(id)}</b>（登记后即可用这个名字请求）`;
  }

  /** 打开自定义模型弹窗。无上下文（只有顶部按钮一个入口），每次都是新增。 */
  function openCustomModel() {
    const status = $('custom-model-modal-status');
    if (status) status.textContent = '';
    const input = $('custom-model-id');
    if (input) input.value = '';
    // 选中自定义家时这家是**锁定**的：模型就登记到它名下，不必（也不该）再选一次 ——
    // 让它在下拉里可选，用户换一家就等于在给别的家登记，而表格里根本看不到结果。
    // 内置家则预选当前正在看的那一家：点了某家的分段再来加模型时，这就是他要的家
    const locked = isCustomView() ? currentProvider : '';
    fillCustomProvider(locked || (currentProvider !== 'all' ? currentProvider : ''));
    const select = $('custom-model-provider');
    if (select) select.disabled = Boolean(locked);
    customModelPreview();
    $('custom-model-modal').classList.add('open');
    setTimeout(() => input?.focus(), 0);
  }

  function closeCustomModel() {
    // 保存中不许关：关掉会让「到底存没存进去」变成未知状态
    if (customSaving) return;
    $('custom-model-modal').classList.remove('open');
  }

  async function saveCustomModel() {
    if (customSaving) return;
    const provider = $('custom-model-provider')?.value || '';
    const id = $('custom-model-id')?.value.trim() || '';
    const status = $('custom-model-modal-status');
    if (!provider) { status.textContent = '请选择提供商'; return; }
    if (!id) { status.textContent = '请填写上游模型 ID'; return; }
    customSaving = true;
    $('custom-model-modal-save').disabled = true;
    status.textContent = '保存中…';
    try {
      const next = await writeAddModel(provider, id);
      accept(next);
      customSaving = false;
      closeCustomModel();
      const label = customProviderOptions().find(item => item.id === provider)?.label || provider;
      if (isCustomView()) {
        // 自定义家的清单就是用户自己的登记表，登记了必然出现在表里（后端不做
        // 「这家有没有账号」的过滤），所以只有一句成功提示
        toast(`✅ 已登记模型 ${id}（${label}）`);
      } else {
        // 内置家：登记成功但表格里看不到这一行时，必须说清为什么 —— 表格只列
        // 「当前有可用登录态」的家（后端的 active_manifests 过滤），给一个还没加
        // 账号的家登记模型不会立刻出现。不说的话用户会以为没保存成功，然后再加一遍。
        const visible = Array.isArray(next?.models)
          && next.models.some(item => item.id === id && (item.provider || '') === provider);
        if (visible) {
          toast(`✅ 已登记自定义模型 ${id}（${label}）`);
        } else {
          toast(`✅ 已登记 ${id}（${label}），但该提供商还没有可用账号，这一行要加上账号后才会显示`, 'err');
        }
      }
    } catch (error) {
      status.textContent = `保存失败：${error.message}`;
    } finally {
      customSaving = false;
      $('custom-model-modal-save').disabled = false;
    }
  }

  // ─── 获取模型（弹窗）─────────────────────────

  /**
   * 「获取模型」：打开弹窗，本体在 models-fetch-modal.js（全局 wbModelsFetchModal）。
   * 两种提供商在弹窗里走不同形态 —— 自定义家是「拉取 → 勾选 → 导入」，内置家是
   * 「刷新各家远程目录 + 逐家结果」，见那个文件的说明。本函数只负责把上下文
   * （选中项、展示名、成功回调）递过去，不掺和弹窗内部的事。
   *
   * 内置家的刷新**不针对当前选中项、也不是固定八家**：范围 = 左栏实有清单的家
   * ∪ 有启用账号的家（弹窗里组装，见 models-fetch-modal.js 的 scopeProviders）
   * —— 模型管理页看不到的家不该出现在结果里。因此标题只写「内置提供商」，
   * 实际刷了哪几家由结果表逐行列出。
   *
   * ── 为什么内置家要回调重拉（`onRefreshed`）──────────────────
   * 本页的数据是**自持**的（`data` 只由 load / 写操作更新），而刷新走的是弹窗里
   * 那条 `/api/models/refresh`：目录在后端换了一份，本页手里的 manage 视图
   * （左栏计数、行的「来源」列、新模型的行）却还是旧快照 —— 用户刚在弹窗里看到
   * 「已刷新 9 个」，关掉弹窗回到列表还是 5 行，看起来像刷新没生效。因此在刷新
   * 真落地后**重拉一次**（`load`），把两条取数路径重新对齐。
   */
  function refreshModels() {
    const custom = isCustomView();
    const name = custom
      ? (customSource?.record?.(currentProvider)?.name || currentProvider)
      : '内置提供商';
    window.wbModelsFetchModal?.open({
      providerId: currentProvider,
      custom,
      name,
      // 自定义家：导入成功 → 目录缓存已刷新，重绘即可（数据源是本地缓存）
      onDone: () => render(),
      // 内置家：远程目录落地 → 后端清单换了，必须重拉（本地那份已过期）。
      // force：这次重拉是「刷新已落地」的收尾，不能被一个更早的在途加载吞掉
      onRefreshed: () => { void load({ force: true }); },
    });
  }

  // ─── 绑定 ─────────────────────────────

  /**
   * 切换选中的提供商。左栏点击与账号页「模型清单 →」跳转都走它。
   * 选中项落盘（跨次启动记忆）；表头按视图重排一次（自定义家隐藏倍率 / 来源两列，
   * 见 CUSTOM_HIDDEN_COLUMNS），其余交给 render()。
   */
  function selectProvider(id) {
    currentProvider = String(id ?? '').trim() || 'all';
    window.wbFilterMemory?.save(FILTERS_KEY, { provider: currentProvider });
    syncHead();
    render();
  }

  /**
   * 「＋ 新建自定义提供商」：**就地**打开「添加账号」弹窗并直达新建表单。
   *
   * 弹窗本身是全局的（不在账号页里），跳页只是绕路 —— 用户在这一页点「新建」，
   * 就该在这一页完成它。表单、创建接口与成功收尾都在 add-provider-forms /
   * add-custom-provider 那一套里（见 wbAccountAddForms.openNewCustomForm）；
   * 创建成功后那次 refresh 会让本页左栏重画出新家（app.js 的 render → 本页 render）。
   */
  function openAddCustomProvider() {
    window.wbAccountAddForms?.openNewCustomForm?.();
  }

  /**
   * 删除一个自定义提供商（左栏条目上那颗 ×）。
   *
   * 删除逻辑（二次确认 / 级联删账号 / 目录与账号列表刷新）全在
   * custom-provider-ui.js 里，与账号设置弹窗那颗「删除提供商」同一实现；
   * 这里只做两件事：转发，然后把本页重画一遍。
   *
   * 删掉的是**当前选中的那家**时不用特判选中态：`render` 开头的
   * `normalizeSelection` 认不出已删的家，会自动回落「全部」。
   */
  async function removeCustomProvider(providerId) {
    const api = window.wbCustomProvidersUi;
    // 正常加载顺序下它一定在（custom-provider-ui.js 在本文件之后、账号页之前加载）。
    // 真缺了就说一声 —— 点了 × 什么都不发生比报错更难查
    if (!api?.remove) { toast('删除入口未就绪，请重试或重启应用', 'err'); return; }
    if (!(await api.remove(providerId))) return;
    // 重拉而非只重绘：该家从后端目录里消失了（它的模型不再参与路由），
    // 内置家那份 manage 视图里的承载关系可能跟着变
    await load({ force: true });
  }

  // 恢复上次的筛选：左栏的选中态由 renderRail 每次渲染时画（选中项就是
  // currentProvider），状态分段与搜索框由 React 岛渲染（见下）。
  // 搜索框的初值直接由岛的 value 参数带进去，不再需要「先写 DOM 再纠正」那一步。

  // 搜索框交给岛（ui/islands/ui.js）：非受控，浏览器先把字上屏，这里跟着存盘与重绘。
  // 重绘整张模型表可能不便宜，非受控正是为了不让打字等它 —— 见 input-control.tsx 的说明。
  const searchHost = $('models-search');
  if (searchHost && window.wbInput) {
    window.wbInput.mount(searchHost, {
      value: searchText,
      placeholder: '搜索模型 ID / 名称 / 映射名…',
      ariaLabel: '搜索模型',
      icon: '⌕',
      onInput: value => {
        searchText = value;
        // 各敲一个字写一次 localStorage，量小无感
        window.wbFilterMemory?.save(FILTERS_KEY, { search: value });
        render();
      },
    });
  }

  // 状态筛选交给岛（ui/islands/ui.js）：语义、键盘、滑块都在岛上。
  // 取值仍以本文件的 stateFilter 为准 —— 岛完全受控，这里只负责灌值与回灌。
  let stateIsland = null;
  const stateHost = $('models-state-seg');
  if (stateHost && window.wbSegmented) {
    stateIsland = window.wbSegmented.mount(stateHost, {
      options: MODEL_STATES.map(value => ({ value, label: MODEL_STATE_LABEL[value] })),
      value: stateFilter,
      ariaLabel: '按状态筛选',
      onChange: setStateFilter,
    });
  }

  function setStateFilter(next) {
    const value = MODEL_STATES.includes(next) ? next : 'all';
    if (value === stateFilter) return;
    stateFilter = value;
    window.wbFilterMemory?.save(FILTERS_KEY, { state: stateFilter });
    stateIsland?.setValue(value);
    render();
  }

  $('models')?.addEventListener('click', onTableClick);
  $('models')?.addEventListener('change', onTableChange);
  // 左栏：点一家切一家；「＋ 新建自定义提供商」就地打开新建弹窗（不跳页）；
  // 自定义家条目上的 × 删除该家（二次确认在 wbCustomProvidersUi 里）
  $('prov-rail')?.addEventListener('click', event => {
    const remove = event.target.closest('[data-del-provider]');
    if (remove) { void removeCustomProvider(remove.dataset.delProvider); return; }
    if (event.target.closest('#rail-add-custom')) { openAddCustomProvider(); return; }
    const item = event.target.closest('.pv[data-provider]');
    if (!item) return;
    selectProvider(item.dataset.provider);
  });
  $('mapping-modal-close')?.addEventListener('click', closeMapping);
  $('mapping-modal-cancel')?.addEventListener('click', closeMapping);
  $('mapping-modal-save')?.addEventListener('click', () => { void saveMapping(); });
  $('mapping-alias')?.addEventListener('input', mappingPreview);
  $('mapping-alias')?.addEventListener('keydown', event => { if (event.key === 'Enter') void saveMapping(); });
  $('mapping-provider')?.addEventListener('change', () => {
    // 换了一家，上游候选整体换掉（保留同名项，切回来时不用重选）
    fillUpstreamSelect($('mapping-provider').value, upstreamValue());
    mappingPreview();
  });
  $('mapping-upstream')?.addEventListener('change', mappingPreview);
  $('mapping-upstream')?.addEventListener('keydown', event => { if (event.key === 'Enter') void saveMapping(); });
  // 等级下拉：选中「自定义等级」时露出输入框；其余值直接进预览
  $('mapping-reasoning')?.addEventListener('change', () => {
    syncReasoningCustom();
    mappingPreview();
  });
  $('mapping-reasoning-custom')?.addEventListener('input', mappingPreview);
  $('mapping-reasoning-custom')?.addEventListener('keydown', event => { if (event.key === 'Enter') void saveMapping(); });
  $('mapping-modal')?.addEventListener('click', event => { if (event.target === $('mapping-modal')) closeMapping(); });

  // ── 自定义模型弹窗的绑定（与映射弹窗同构：关闭 / 取消 / 保存 / 回车 / 点遮罩）──
  $('btn-add-custom-model')?.addEventListener('click', openCustomModel);
  $('custom-model-modal-close')?.addEventListener('click', closeCustomModel);
  $('custom-model-modal-cancel')?.addEventListener('click', closeCustomModel);
  $('custom-model-modal-save')?.addEventListener('click', () => { void saveCustomModel(); });
  $('custom-model-provider')?.addEventListener('change', customModelPreview);
  $('custom-model-id')?.addEventListener('input', customModelPreview);
  $('custom-model-id')?.addEventListener('keydown', event => {
    if (event.key === 'Enter') void saveCustomModel();
  });
  $('custom-model-modal')?.addEventListener('click', event => {
    if (event.target === $('custom-model-modal')) closeCustomModel();
  });

  // 「获取模型」按钮：文案与 title 都由 paintViewChrome 按当前选中项写，
  // 这里只绑事件（首屏 render 会立刻补上那两处文案）。
  const refreshButton = $('btn-refresh-models');
  if (refreshButton) {
    refreshButton.addEventListener('click', () => { void refreshModels(); });
  }

  /**
   * 某家清单的最近拉取时刻（毫秒；0 = 未知 / 从未成功过）。
   *
   * 给「获取模型」弹窗的「更新日期」列用：那个弹窗**打开时不自动拉取**，所以
   * 它的第一屏（待获取态）没有本次结果可显示，而用户恰恰最想先看到「各家现在
   * 这份清单是什么时候的」再决定要不要刷。数据取自 manage 视图的 `refreshedAt`
   * （后端逐行给的家级字段，见 `catalog::manage_view`）—— 与本页「来源」列
   * 悬停提示里的那个时刻同源，两处不会各说一个时间。
   */
  function providerRefreshedAt(providerId) {
    const rows = Array.isArray(data?.models) ? data.models : [];
    const hit = rows.find(model => model.provider === providerId);
    return Number(hit?.refreshedAt) || 0;
  }

  // visibleColumns 导出给 table-columns.js：列宽那一层要按当前可见列算
  // （覆盖值落到哪个 <col>、末列不给把手），两边读同一份配置才不会各算一个样。
  // 必须在下面的 syncHead() **之前**挂好 —— 那次同步会顺带重算列宽与把手，
  // 挂晚了它读到的是「全列」，「末列不给把手」就会判到错的那一列上。
  //
  // selectProvider 导出给账号页的自定义提供商弹窗（「模型清单 →」跳过来并选中该家）。
  // providerRefreshedAt 导出给「获取模型」弹窗（见上）。
  window.wbModelsPanel = {
    render, load, refreshAll, refreshModels, visibleColumns, selectProvider, builtinProviders,
    providerRefreshedAt,
  };

  // 首屏同步一次静态表头：load() 只重画数据行，表头是本文件加载后按本地配置
  // 重排过的（顺序 / 显隐 / 对齐）—— 不补这一下，用户改过列设置后刷新页面会看到
  // 表头回到 index.html 里的原始顺序，而数据行已经是新顺序（一眼就对不上）。
  // 先校验一次选中项：存盘里记的那家可能已经被删了，而自定义家与内置家的可见列
  // 不同（见 CUSTOM_HIDDEN_COLUMNS），这一步决定了下面按哪一套列集合同步表头。
  normalizeSelection();
  syncHead();

  // 首次进入模型管理页时 load()；app.js 的 render() 只触发重绘（数据自持）
  // app.js 末尾的 showPage() 跑在本文件之前：启动时若记住的就是本页，那次调用
  // 拿不到 wbModelsPanel，这里补拉一次
  if (wbApp.currentPage === 'gateway') void load();
})();
