/* Agent2API · 请求日志面板（网关转发明细 · 筛选 / 分页 / 自动刷新） */
/* global workbuddyDesktop, wbApp, wbRequestHover, wbRequestDetail, wbRequestPhase */

/**
 * 「请求日志」页的自持面板：网关每次转发到上游的请求日志。
 * 与 logs-panel.js（系统事件）同构但互不依赖：两边数据源、筛选维度、
 * 分页口径都不同，拆成两个模块后各自的轮询与状态不再互相牵连。
 *
 * ── 数据口径 ────────────────────────────────────────────────
 * 明细按保留期（默认 30 天）存盘，条数没有上限，一次拉全不现实，
 * 所以走后端 offset/limit 真分页；每页 50 条（后端默认值，这里显式传，
 * 页数才可算），上限 500 条由后端夹紧。后端支持的筛选是时间区间、状态、
 * 模型与提供商四个维度（`RequestQuery`）—— 四个条件在**存储层的同一份
 * `FilterPlan`** 里编译成 SQL，所以「页面上筛出来的 N 条」与「清空删掉的
 * 那批」必然是同一个集合（见 `request_stats/sql.rs`）。
 *
 * ── 模型 / 提供商两个下拉的选项从哪来 ───────────────────────
 * 不来自前端配置，而是 `GET /api/stats/requests/filters` —— 明细里**实际
 * 出现过**的模型名与 provider id（按出现次数降序）。按配置清单列会让
 * 「列表里有这一行、下拉里没有这个选项」发生（历史请求用过的模型名、
 * 已删除账号所属的提供商都会漏），那是最难向用户解释的一类不一致。
 * 清单进页面时拉一次：它与时间档位无关，翻页 / 换筛选都不重拉。
 * 拉不到时两个下拉只留「全部」选项（筛选器退化成不可用，列表照常显示）。
 *
 * ── 「重试」列为什么是一个入口而不是一个标记（本次改造）─────
 * 后端明细里有 `attemptDetails`（每一次上游尝试的 provider / 状态码 /
 * 错误摘要）与 `sensitiveHits`（本次命中的敏感词 × 次数）两个字段，
 * 这一列因此可以回答「换了谁、哪一次失败在哪」与「命中了什么词」。
 * 两枚标签各自带悬停面板，面板的机制与内容构造在 `request-hover.js`
 * （本文件只渲染标签并提供「按 id 反查数据」的回调）—— 拆出去的理由
 * 与 tooltip.js / select.js 相同：浮层的定位与生命周期是一整块自洽逻辑，
 * 混在列表渲染里会让两边都难改。
 *
 * ── 自动刷新的间隔从哪来 ────────────────────────────────────
 * 与 logs-panel.js 同一套：由「定时任务」页配置（config.json 的
 * `scheduledTasks.requestsAutoRefresh`），本文件启动时自读一次、
 * 之后接受那边推送（`applyAutoRefresh`）。页头的开关已随之移除。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  // toast 不再被本文件使用（清理动作的提示都移去了 request-clear-modal.js）
  const { esc } = wbApp;

  // ─── 列设置（显示 / 隐藏、顺序、对齐）──────────
  //
  // 本表是 CSS grid 形态（没有 <table>），所以列的显隐与顺序靠**轨道列表**表达：
  // 渲染方按可见列产出格子，table-columns.js 按同一个列集合拼 `--req-cols`
  // （见那边 spec 里的 columnsOf）。两处必须读同一份配置，否则格子与轨道
  // 条数对不上，整行会错位。
  //
  // key 取 tab-group.js（table-columns.js）里登记的同一套：time / target / retry /
  // status / model / dur / usage / error / detail，与 CSS 里的 `.req-xxx` 类同名。
  // `sel` 是表头格的选择器、`track` 是默认轨道，两者都从 table-columns.js 的登记
  // 同源复制过来 —— 那份登记管「列宽」，这里管「列的显隐与顺序」，
  // 列的集合只有一套（改列时两处要一起改，见各自的注释）。
  const COLUMNS = [
    { key: 'time', label: '时间', sel: '.req-time', track: '92px' },
    { key: 'target', label: '提供商 / 账号', sel: '.req-target', track: 'minmax(0, 1.1fr)' },
    { key: 'retry', label: '重试', sel: '.req-retry', track: '52px' },
    { key: 'status', label: '状态', sel: '.req-status', track: '96px' },
    { key: 'model', label: '模型', sel: '.req-model', track: 'minmax(0, 1.3fr)' },
    { key: 'dur', label: '用时', sel: '.req-dur', track: '96px', align: 'right' },
    { key: 'usage', label: '用量', sel: '.req-usage', track: 'minmax(0, 1.6fr)' },
    { key: 'error', label: '错误', sel: '.req-error-cell', track: 'minmax(0, 1.2fr)' },
    { key: 'detail', label: '详情', sel: '.req-detail', track: '60px', align: 'right' },
  ];

  const colSettings = window.wbColSettings?.register({
    id: 'requests',
    label: '请求日志表',
    columns: COLUMNS.map(({ key, label, align }) => ({ key, label, align })),
    mount: () => document.querySelector('.page[data-page="requests"] .panel-head .head-actions'),
    // 顺序很关键：先重画列表（格子按新列集合产出），再让 table-columns.js
    // 重算轨道变量 —— 它读的 visibleColumns() 已经是新配置了，
    // 于是「格子数 = 轨道数」在同一次任务里对齐，不会闪出一次错位的中间态。
    onChange: () => { render(); window.wbTableColumns?.repaint?.('requests'); },
  });

  /**
   * 当前可见的列（顺序即配置顺序）。
   *
   * 返回的项带 `sel` / `track`：`sel` 给渲染方定位表头格与数据格，
   * `track` 交给 table-columns.js 拼轨道 —— 两处都从这一个函数拿，
   * 所以「藏了哪几列」在两边是同一个答案。
   */
  const visibleColumns = () => (colSettings ? colSettings.apply(COLUMNS) : COLUMNS);

  /**
   * 自动刷新间隔兜底值（毫秒）= 后端的默认间隔
   * （`DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS`）：读不到配置时与用户没改过时一致。
   * 实际值由「定时任务」页决定（见模块头）。默认提到 1s 是为了进行中请求的
   * 实时性 —— 转发中的行（status=0）靠这一拍拍轮询把「已用时」与终态刷出来；
   * 这条查询按当前筛选走索引、行数以十计，对后端是轻请求。
   */
  const DEFAULT_AUTO_REFRESH_MS = 1_000;
  let autoRefreshMs = DEFAULT_AUTO_REFRESH_MS;
  /**
   * 是否已经从后端读到过间隔配置。
   *
   * 两个作用：① 自读只做一次，切页面不重复请求；② 「定时任务」页推过来的值
   * 也算同步过（见 `applyAutoRefresh`），避免一次迟到的失败自读把用户刚改好的
   * 间隔覆盖回兜底值。
   */
  let autoSynced = false;
  /** 任务关闭时置 false：定时器不跑（区别于「间隔很大」） */
  let autoEnabled = true;
  /**
   * 是否有一次轮询触发的拉取还在途中。
   *
   * 定时器是 `setInterval`（不等上一次完成），而间隔可以调到 1 秒 ——
   * 一次慢响应就会与后来的几拍叠在一起。本面板用 `seq` 只认最后一次响应，
   * 所以叠了也不会显示错数据，但每次响应都会重绘一次列表；间隔这么密时
   * 无谓的重绘会让正在看的人眼花。所以轮询撞上在途请求就跳过这一拍。
   *
   * 只挡轮询：用户翻页 / 换筛选是有意操作，不该被上一次自动刷新挡掉。
   */
  let polling = false;

  /** 每页条数：与后端 DEFAULT_LIMIT 一致 */
  const PAGE_SIZE = 50;
  /** 时间档位的持久化键：沿用拆分前「模型请求」视图的键，用户已选的档位不因拆页丢失。
   *  前缀沿用项目既有的 workbuddy-desktop-*（主题、事件日志档位用的是同一套） */
  const RANGE_KEY = 'workbuddy-desktop-logs-requests-range';

  /** 合法的时间档位与后端 /api/stats/summary 的白名单同字面量（报表页也是这一组） */
  const RANGES = ['today', '7', '30', 'month', 'all'];
  const DEFAULT_RANGE = 'all';
  const RANGE_LABEL = { today: '今天', 7: '近 7 天', 30: '近 30 天', month: '本月', all: '全部' };

  /** 只有明确存过合法档位才采纳；无值 / 读取抛错 / 值被改坏一律回落「全部」 */
  function readRange() {
    try {
      const saved = localStorage.getItem(RANGE_KEY);
      return RANGES.includes(saved) ? saved : DEFAULT_RANGE;
    } catch {
      return DEFAULT_RANGE;
    }
  }

  function persistRange(value) {
    try {
      localStorage.setItem(RANGE_KEY, value);
    } catch {
      // 存储不可用只影响下次打开，不影响本次会话
    }
  }

  let range = readRange();
  /**
   * 三个下拉筛选（状态 / 提供商 / 模型）的跨次启动记忆。空串 = 「全部」。
   * 状态下拉的选项是静态 HTML，启动即可回填；提供商 / 模型的候选清单是异步
   * 拉的（refreshFilterOptions），回填走 fillFilterSelect 的 desired 入参 ——
   * 提前给 select.value 赋值在选项还不存在时会被浏览器丢弃。
   */
  const FILTERS_KEY = 'workbuddy-desktop-requests-filters';
  const savedFilters = window.wbFilterMemory
    ? window.wbFilterMemory.load(FILTERS_KEY, { status: '', provider: '', model: '' })
    : { status: '', provider: '', model: '' };
  let entries = [];
  let total = 0;      // 明细总量（未过滤）
  let matched = 0;    // 命中筛选条件的条数
  /** 同一筛选下进行中（status=0）的条数：响应的 running 字段，页头读数与导航徽标共用 */
  let runningCount = 0;
  /**
   * 「仅看进行中」开关（进行中的行 = status=0，转发还没收尾）。
   * 只在会话内有效：不持久化 —— 它描述的是「此刻在盯转发」，不是偏好设置，
   * 下次打开默认回到全量视图比停在「可能早已为空」的进行中视图更合理。
   */
  let runningOnly = false;
  let offset = 0;
  let seq = 0;        // 请求序号：连点翻页时只认最新一次响应
  let timer = null;

  // ─── 时间档位 → start 参数 ────────────────────

  /** 取某个时刻的本地零点毫秒值 */
  function midnight(date) {
    return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  }

  /**
   * 档位对应的毫秒下界（闭区间起点），口径与报表页 `range_bounds` 逐日一致：
   * 「N 天」= 含今天在内的 N 个自然日，所以往前推 N-1 天。
   * `new Date(y, m, d)` 走本地时区构造，跨月 / 跨年 / 夏令时都交给 Date 自己算。
   * 「全部」返回 null（不传 start）。
   */
  function rangeStart(value) {
    const now = new Date();
    switch (value) {
      case 'today': return midnight(now);
      case '7': return midnight(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 6));
      case '30': return midnight(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 29));
      case 'month': return midnight(new Date(now.getFullYear(), now.getMonth(), 1));
      default: return null;
    }
  }

  // ─── 单元格渲染 ──────────────────────────────

  /** 明细跨天（最多 30 天），日期与时刻分两行，月日必不可少 */
  function timeCell(ts) {
    if (!ts) return '<span class="req-time">—</span>';
    const d = new Date(ts);
    const pad = n => String(n).padStart(2, '0');
    const date = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
    const clock = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    return `<span class="req-time"><span>${esc(date)}</span><span>${esc(clock)}</span></span>`;
  }

  /** 用时：秒以内给毫秒，分钟以上给分秒（与 OmniProxy 同一格式） */
  function formatDuration(ms) {
    const rounded = Math.round(Number(ms) || 0);
    if (rounded < 1000) return `${rounded}ms`;
    const seconds = Math.floor(rounded / 1000);
    if (seconds >= 60) return `${Math.floor(seconds / 60)}分${seconds % 60}秒`;
    const millis = rounded % 1000;
    return millis > 0 ? `${seconds}秒${millis}ms` : `${seconds}秒`;
  }

  /**
   * 首响：上游首帧到达相对请求开始的耗时（OmniProxy 的 ttfb 同义）。
   * null（旧数据没有该字段 / 请求在首帧之前就失败）显示「-」——
   * 走 formatDuration 会把 null 算成 0ms，假数字比空占位更糟。
   */
  function formatFirstResponse(ms) {
    const value = Number(ms);
    if (!Number.isFinite(value) || value <= 0) return '-';
    return formatDuration(value);
  }

  /** 用量读数：明细里存的是精确值，展示也用精确值（千分位），缩写会丢比对基准 */
  function formatTokens(value) {
    return (Number(value) || 0).toLocaleString('zh-CN');
  }

  /**
   * 进行中行的「已用时」：now - ts 取整秒（进行中不足 1 秒也显示 1 秒 ——
   * 「0秒」读起来像没动）。1 秒轮询每拍重绘，这个数自然一秒一跳。
   */
  function formatElapsed(ts) {
    const elapsed = Math.max(0, Date.now() - (Number(ts) || 0));
    return `${Math.max(1, Math.floor(elapsed / 1000))}秒`;
  }

  /** 缓存命中率：命中读取 / 输入，分母为 0 时无意义，给「-」 */
  function cacheRate(entry) {
    const prompt = Number(entry.promptTokens) || 0;
    if (prompt <= 0) return '-';
    return `${((Number(entry.cacheReadTokens) || 0) / prompt * 100).toFixed(2)}%`;
  }

  /** 成功口径与后端 `RequestEntry::is_success` 逐字对齐：2xx **且**没有错误摘要。
   *  流式请求的 HTTP 200 在响应头阶段就发出去了，之后的上游断流 / 错误帧只能靠
   *  `error` 表达 —— 只按状态码判会把这类失败画成绿色，与后端筛选（status=ok/error）、
   *  报表成功率对不上账。后端落盘时已把空串摘要滤成 null，所以「有非空 error 即失败」。 */
  function isOk(entry) {
    const status = Number(entry.status) || 0;
    return status >= 200 && status < 300 && !entry.error;
  }

  /**
   * 是否「仍在转发中」。后端契约：转发一开始就插一条 status=0 的行，
   * 收尾后由终态记账覆盖（见 stats_api::stats_requests 的模块注释）。
   * 分界线画在 error 上：status=0 **且**没有错误摘要才是进行中 ——
   * status=0 却带摘要的行是旧口径里「还没发出请求就失败」的历史数据，
   * 仍按失败渲染（那次失败已经落定，不该被画成还在跑）。
   */
  function isRunning(entry) {
    return (Number(entry?.status) || 0) === 0 && !entry?.error;
  }

  /**
   * 状态列。
   *
   * ── 进行中为什么不是一枚徽章而是「徽章 + 阶段计时」两行（本次改造）──
   * 改造前这里只有一枚「进行中」：它只回答「跑没跑完」，用户盯着一条卡住的
   * 请求时真正要问的是**卡在哪一步**（连上游 / 等模型出首字 / 正在刷内容 /
   * 正在重试换号），四个阶段的排查方向完全不同。现在第一行是阶段徽章
   * （文案与配色见 `request-phase.js`，对标 OmniProxy 的四个阶段标签），
   * 第二行是**当前阶段**已经持续了多久（`phaseElapsedMs`，服务端现算）——
   * 与用时列的「总用时 + 首响」两行结构同一手法，两列各答一个问题。
   *
   * 没有阶段可读（旧行、更早版本写入的在途行）时徽章回落成「进行中」、
   * 第二行整行省掉 —— 那时确实没有更细的事实可说。
   */
  function statusCell(entry) {
    // 进行中：不走「失败」的红徽章 —— 它不是失败，只是还没收尾。
    // 1 秒轮询每拍重绘，阶段计时这个数自然一秒一跳（与用时列的「已用时」同一手法）。
    if (isRunning(entry)) {
      // 阶段模块没就绪时回落成改造前的静态徽章（与重试列对 request-hover 的
      // 可选链同一手法）：加载顺序本就是硬要求，但真出问题时整列不该空着
      const phase = window.wbRequestPhase;
      if (!phase) {
        return '<span class="req-status"><span class="badge tag running"'
          + ' title="请求正在转发中，用时列显示的是已用时">'
          + '<span class="req-live-dot" aria-hidden="true"></span>进行中</span></span>';
      }
      const badge = phase.badgeHtml(entry);
      const elapsed = phase.elapsedLineHtml(entry);
      return '<span class="req-status">'
        + `<span class="req-status-badge">${badge}</span>`
        + (elapsed ? `<span class="req-status-line">${elapsed}</span>` : '')
        + '</span>';
    }
    const status = Number(entry.status) || 0;
    const ok = isOk(entry);
    // 走到这里 status=0 的只剩「还没发出请求就失败」的历史行（进行中的已在
    // 上面分流，新口径里那种失败落定后仍记 status=0 + 错误摘要）：直接写 0
    // 会被读成 HTTP 状态码，所以退成「失败」两个字。
    // 2xx 却带错误摘要时（流式请求在响应体阶段失败）单看数字会以为成功，
    // 给 title 说明「状态码是 2xx，失败在响应体阶段」。
    const title = !ok && status >= 200 && status < 300
      ? `HTTP ${status}，但响应体阶段出错：见错误列`
      : '';
    return `<span class="req-status"><span class="badge tag ${ok ? 'ok' : 'bad'}"${title ? ` title="${esc(title)}"` : ''}>${status || '失败'}</span></span>`;
  }

  /**
   * 重试列：上游尝试链 + 过程事实（`attemptDetails`）+ 敏感词命中（`sensitiveHits`）。
   *
   * ── 这一列现在承载两件事（本次改造）──────────────────────────
   * 改造前这里只是一个「重试」标记（`attempts > 1` 时出现），敏感词命中在
   * 「日志」页由 `[Desensitize]` 那类应用日志间接呈现 —— 两处都只说「发生过」，
   * 不说「发生了什么」。现在两者都收敛到这一列，并各自带一个悬停面板：
   *   · 橙标签 → 切换路径 + 每次尝试的（提供商 · 账号 → 成功/失败 · 状态码 ·
   *     错误）+ 每轮内部的退避重试 + 提示（代理回退等）
   *   · 紫标签 → 命中的词 × 次数
   * 形态照 OmniProxy 的 `FailoverTip` / `SensitiveMaskedTag`（同一列里两枚
   * 标签，各自挂自己的弹层 —— 那边特意**不**把整格包一层 Tooltip，
   * 否则两枚标签的弹层会互相嵌套、悬停时同时弹出）。
   *
   * ── 橙标签的判据是 `hasProcessFacts`，不是 `attempts > 1`（本次改造）──
   * `attempts` 只数账号轮换，同账号内的退避重试不计入它 —— 一次被 11128
   * 敏感词拦截、重试 3 次后成功的请求 `attempts` 仍是 1，改造前这类请求
   * **连标签都不出现**（而运行日志那边刷了 3 行，这正是要收敛掉的东西）。
   * 判据与文案的完整说明见 request-hover.js 的 `hasProcessFacts`。
   *
   * ── 敏感词标签为什么只写一个「敏」字（本次改造）──────────────
   * 表头已经收窄成「重试」（见 headHtml 的说明），这一列只有 52px 宽：
   * 「敏感词」三个字会把标签撑得比「重试 N」还宽，两枚标签竖排时右边留一大块
   * 空白、列也容易被挤到换行。命中是**有没有**的问题，不是**几个字**的问题，
   * 所以缩成一个「敏」字，完整含义交给点击/聚焦弹出的富文本面板
   * （那里面仍然逐词列出命中明细，一个字都没少）。
   * `aria-label` 补上完整说法，读屏软件不会只念出一个孤零零的「敏」。
   * 两枚标签都不挂 `title`：面板已经给出完整明细，原生提示会与它叠在一起
   * 重复一层（同一处同时冒出两个说明框）。
   *
   * ── 为什么标签是 button ──────────────────────────────────────
   * `cursor: help` 只对鼠标有意义；做成 button 之后键盘能 Tab 到、聚焦即弹出
   * （request-hover.js 的 focusin/focusout），读屏软件也会把它当成可交互元素。
   * 数据不塞进 `data-*`：一次尝试明细可达 24 条、错误摘要 200 字符，
   * 50 行就是几百 KB 的属性文本 —— 会拖慢整表重绘。改为按 `data-req-id`
   * 反查 `entries` 里的那一行（见本文件绑定的 `entryOf`）。
   *
   * 该列为空时显示 `-`（两枚标签都没有内容时）。
   */
  function retryCell(entry) {
    const attempts = Number(entry.attempts) || 1;
    const key = rowKey(entry);
    const tags = [];
    // 判据交给 request-hover（它要读明细内部的字段）。兜底成 `attempts > 1`：
    // 万一那个模块没就绪，至少换过号的请求仍能显示标签（旧行为），
    // 而不是整列静默变空。
    const showChain = window.wbRequestHover?.hasProcessFacts?.(entry) ?? attempts > 1;
    if (showChain) {
      // 标签文案带次数：换过号时 `attempts` 就是轮数；没换号只有重试时，
      // 那个数字来自重试链（两者都读不到时退回不带次数）。
      const count = attempts > 1 ? attempts : countRetries(entry);
      tags.push(`<button type="button" class="badge tag warn req-hover-tag"`
        + ` data-req-hover="chain" data-req-id="${esc(key)}">`
        + `重试${count > 1 ? ` ${count}` : ''}</button>`);
    }
    // 敏感词命中：判据是「命中表非空」（后端 sensitiveHits 字段）。
    // 不用 attempts 那类计数 —— 命中是这条请求的属性，与重试次数无关
    // （一次成功的请求同样可能命中敏感词，那时这一列只有这一枚标签）。
    if (Array.isArray(entry.sensitiveHits) && entry.sensitiveHits.length) {
      tags.push(`<button type="button" class="badge tag req-hover-tag sensitive"`
        + ` data-req-hover="sensitive" data-req-id="${esc(key)}"`
        + ` aria-label="命中了敏感词">敏</button>`);
    }
    if (!tags.length) return '<span class="req-none">-</span>';
    return `<span class="req-retry">${tags.join('')}</span>`;
  }

  /**
   * 这条请求里所有尝试明细的内部重试总次数（没有明细时为 0）。
   *
   * 只用于标签上的那个数字：`attempts > 1` 时优先用 `attempts`（那是换号轮数，
   * 与「切换路径」那串箭头长度一致），否则用本函数 —— 一次 11128 拦截重试
   * 3 次而没换号的请求，标签显示「重试 3」而不是光秃秃一个「重试」。
   */
  function countRetries(entry) {
    const details = Array.isArray(entry?.attemptDetails) ? entry.attemptDetails : [];
    return details.reduce((sum, item) => (
      sum + (Array.isArray(item?.retries) ? item.retries.length : 0)
    ), 0);
  }

  /**
   * 一行请求在 DOM 里的身份键（悬停面板反查数据用）。
   *
   * 优先用 `id`（调试模式下与报文同键、天然唯一）；没有 id 的旧行退回 `ts`。
   * 两者都缺时给空串 —— 那时标签仍会渲染（按钮可聚焦、面板会显示「未采集」
   * 的兜底文案），只是反查不到数据，比不渲染更不容易让人以为界面坏了。
   */
  const rowKey = entry => String(entry?.id || entry?.ts || '');

  /**
   * 提供商与账号同格两行（OmniProxy 的「提供商/key」一列对应到这里是
   * 「提供商 + 承载账号」）。提供商三级兜底，顺序不能变：
   *   1. `providerLabel`（后端在注册表里换出来的名字，前端不必维护第二份映射）；
   *   2. 前端 providers.js 的目录（后端比前端旧、没给 label 时用本地摘要补上）；
   *   3. 原样回显 id（陌生的 id 也好过空白，至少能看出是谁）。
   * 三者都为空（旧数据没有该字段 / 请求在选定上游之前就失败）时给「—」。
   * 账号为空 = 请求在选定账号之前就失败了（后端契约：无账号时给空串），
   * 显式给破折号而不是留空：空着会被当成渲染缺失。
   */
  function targetCell(entry) {
    const id = String(entry.provider ?? '').trim();
    const fromBackend = String(entry.providerLabel ?? '').trim();
    const provider = fromBackend || (id ? window.wbProviders?.labelOf?.(id) || id : '');
    const providerHtml = provider
      ? `<span class="req-provider" title="${esc(id && id !== provider ? `${provider}（${id}）` : provider)}">${esc(provider)}</span>`
      : '<span class="req-provider is-empty" title="这条明细没有记录提供商（旧数据或请求未走到转发）">—</span>';
    const account = entry.accountName
      ? `<span class="req-acct" title="${esc(entry.accountId || entry.accountName)}">${esc(entry.accountName)}</span>`
      : '<span class="req-acct is-empty" title="请求在任何账号接手之前就失败了">—</span>';
    return `<span class="req-target">${providerHtml}${account}</span>`;
  }

  /**
   * 用量两行：in / out / all 与 缓存读取 / 命中率。
   * 失败请求的 token 由后端一律清零（见 NewRequestEntry::normalize），
   * 写「0」会让人以为真的消耗了这些量，照 OmniProxy 的口径退成「-」。
   */
  function usageCell(entry) {
    // 进行中：用量要等收尾才记账，此刻没有任何读数可给 —— 留空比「-」更准确
    //（「-」在这里会被读成「没有用量」，而进行中的真实含义是「还没有」）
    if (isRunning(entry)) return '<span class="req-usage"></span>';
    if (!isOk(entry)) {
      return `<span class="req-usage" title="失败请求不记录用量">`
        + `<span class="req-usage-line">in: - / out: - / all: -</span>`
        + `<span class="req-usage-line sub">缓存读取: - / 命中率: -</span></span>`;
    }
    const line1 = `in: ${formatTokens(entry.promptTokens)} / out: ${formatTokens(entry.completionTokens)} / all: ${formatTokens(entry.totalTokens)}`;
    const line2 = `缓存读取: ${formatTokens(entry.cacheReadTokens)} / 命中率: ${cacheRate(entry)}`;
    return `<span class="req-usage" title="${esc(`${line1}\n${line2}`)}">`
      + `<span class="req-usage-line">${esc(line1)}</span>`
      + `<span class="req-usage-line sub">${esc(line2)}</span></span>`;
  }

  /**
   * 模型列。转发名与请求名一致（绝大多数请求）→ 单行，与既有显示完全一致；
   * 不一致（映射 / 备援按家改写发生过）→ 两行：
   *   ⬆️ 上游实际收到的模型名（主读数，在上）
   *   ⬇️ 下游请求的模型名（次读数，淡一档，在下）
   *
   * 推理等级：模型名后带 `(等级)` 后缀，如 `glm-5.3-flash(max)` ——
   *   - 上游行 = upstreamReasoning（实际随上游请求发出的等级：映射绑定的
   *     或客户端显式指定的，经承载家归一后的值）
   *   - 下游行 = clientReasoning（客户端请求体里显式指定的等级）
   *   - 单行（无映射）时两者指同一个模型，优先显示上游等级（它是真正发出去
   *     的档位），没有（客户端没指定且映射没绑 / 承载家不接）回退下游等级
   *   - 两键都为空串 = 无等级随行，只显示模型名（与既有显示一致）；
   *     旧数据没有这两个键，同样落到这里
   * 双名缺失时退回单行：`model` 缺失或旧数据没有 clientModel / upstreamModel
   * （这两个键是后加的），请求没走到上游的那次失败也没有上游名。
   */
  function modelCell(entry) {
    const client = String(entry.clientModel ?? '').trim();
    const upstream = String(entry.upstreamModel ?? '').trim();
    const shown = String(entry.model ?? '').trim();
    const clientLevel = String(entry.clientReasoning ?? '').trim();
    const upstreamLevel = String(entry.upstreamReasoning ?? '').trim();
    const tag = (name, level) => (level ? `${name}(${level})` : name);
    if (!client || !upstream || upstream.toLowerCase() === client.toLowerCase()) {
      const text = tag(shown, upstreamLevel || clientLevel);
      return `<span class="req-model" title="${esc(text)}">${esc(text || '—')}</span>`;
    }
    const upText = tag(upstream, upstreamLevel);
    const downText = tag(client, clientLevel);
    return `<span class="req-model req-model-split">`
      + `<span class="req-model-line" title="${esc(`转发到上游的模型：${upText}`)}">⬆️ ${esc(upText)}</span>`
      + `<span class="req-model-line sub" title="${esc(`下游请求的模型：${downText}`)}">⬇️ ${esc(downText)}</span></span>`;
  }

  /**
   * 详情入口：该条请求在调试模式下保存的**上游原始报文**（见后端
   * `core::debug_traffic`）。
   *
   * 只有带 `id` 的行才有入口 —— 旧数据（本字段引入前落盘的行）与
   * 「转发前就失败」的请求都没有 id，也就没有报文可看。按钮**不做可用性
   * 预判**：调试模式是否开启、报文是否还在保留条数内，都由点击时的接口
   * 回答（404 时弹窗里给出原因），这样界面不必跟着设置页的开关重绘。
   */
  function detailCell(entry) {
    const id = entry.id ? String(entry.id) : '';
    if (!id) return '<span class="req-none req-detail">-</span>';
    return `<span class="req-detail"><button type="button" class="sm req-detail-btn"`
      + ` data-detail="${esc(id)}" title="查看该请求的上游原始报文">详情</button></span>`;
  }

  /**
   * 数据单元格：按列 key 建表，每个函数返回**该格本身**（网格项，带 `.req-xxx`
   * 列类名 —— 原设计如此：这一列的一切排版都由那个类名上的声明决定）。
   *
   * 拆成表而不是行内联的 9 个 `${}`：`rowHtml` 只回答「按哪些列、什么顺序」，
   * 而「某一格长什么样」只有一处实现 —— 列设置重排时才不会各画一个样。
   */
  const CELLS = {
    // 注意 timeCell 收的是**时间戳**（它同时被别处按 ts 调用），不是 entry
    time: entry => timeCell(entry.ts),
    target: entry => targetCell(entry),
    retry: entry => retryCell(entry),
    status: entry => statusCell(entry),
    model: entry => modelCell(entry),
    // 进行中行没有 durationMs（收尾才记账）：主行显示已用时（每拍轮询在走）。
    // 次行（首响）**有值就显示**：首响在上游第一帧到达时就有了，而且转发期间
    // 就回写进了这一行（见后端 `live_row_sink`）—— 一条跑几分钟的流式请求，
    // 首响其实一秒内就定了，藏到收尾才显示等于白采。没有值时才整行省掉：
    // 写「-」会被读成「首响失败」，而真实含义是「还没到首帧」。
    dur: entry => (isRunning(entry)
      ? `<span class="req-num req-dur">`
        + `<span class="req-dur-line" title="已用时（请求仍在转发中）">${esc(formatElapsed(entry.ts))}</span>`
        + runningFirstLine(entry)
        + '</span>'
      : '<span class="req-num req-dur">'
        + `<span class="req-dur-line">${esc(formatDuration(entry.durationMs))}</span>`
        + `<span class="req-dur-line sub" title="首响：上游首帧到达的耗时">首响 ${esc(formatFirstResponse(entry.firstResponseMs))}</span></span>`),
    usage: entry => usageCell(entry),
    // 没有错误时用「-」占位（空着会被当成渲染缺失，与重试列同一手法）；
    // 进行中行例外：错误还没有发生，留空 —— 「-」会说成「没出错」，
    // 留空才是「还没到有错误的时刻」
    error: entry => (isRunning(entry)
      ? '<span class="req-error-cell"></span>'
      : entry.error
        ? `<span class="req-error" title="${esc(String(entry.error))}">${esc(String(entry.error))}</span>`
        : '<span class="req-none req-error-cell">-</span>'),
    detail: entry => detailCell(entry),
  };

  /** 把该列的对齐贴到格子上（与 accounts-table.js 的 withAlign 同一手法） */
  function withAlign(html, align) {
    const replaced = html.replace(/^<span class="([^"]*)"/, `<span class="$1 ta-${align}"`);
    return replaced === html ? `<span class="ta-${align}">${html}</span>` : replaced;
  }

  /**
   * 进行中行的「首响」次行：**有值才渲染**（空串 = 首帧还没到，整行省掉）。
   *
   * 与收尾行的写法只差这一层判断：那边 `formatFirstResponse` 用「-」兜住
   * 缺失值（那里缺失确实等于「全程没有帧到达」= 失败），而进行中的缺失只是
   * 「还没到」—— 同一列上两种含义不能共用同一个占位符。
   */
  function runningFirstLine(entry) {
    const value = Number(entry.firstResponseMs);
    if (!Number.isFinite(value) || value <= 0) return '';
    return `<span class="req-dur-line sub" title="首响：上游首帧到达的耗时（请求仍在转发中）">`
      + `首响 ${esc(formatDuration(value))}</span>`;
  }

  /**
   * 一行：按**可见列**逐格产出，顺序与表头一致（两处都走 visibleColumns）。
   * 每个格子仍是带 `.req-xxx` 的网格项，只是多一个用户选的对齐类。
   */
  function rowHtml(entry) {
    const ok = isOk(entry);
    // 进行中行不吃失败行的红底（.failed）：它还没落定，红底是终态的颜色
    const running = isRunning(entry);
    const cells = visibleColumns()
      .map(column => withAlign(CELLS[column.key]?.(entry) || '', column.align))
      .join('');
    return `<div class="req-row${running ? ' running' : ok ? '' : ' failed'}">${cells}</div>`;
  }

  // ─── 整块渲染 ────────────────────────────────

  /**
   * 表头是渲染出来的一部分（不是常驻节点）：空态时列表里只有一条 .log-empty，
   * 才能命中「唯一子元素居中」那条规则。
   * 每个格子都带与数据行同名的类（req-model / req-provider / …）：
   * 窄窗口下网格要按「哪一列」重排（见 page-requests.css 的媒体查询），
   * 有了稳定的类名就不必依赖「它是第几个子元素」这种会随字段增删失效的判据。
   *
   * 第三列的表头是「重试」而不是「重试 / 敏感词」：敏感词那枚标签已经缩成
   * 一个「敏」字（见 retryCell 的说明），表头跟着收窄才配得上这一列 52px 的
   * 宽度 —— 原来那五个字在这点宽度里只能靠省略号收住，等于没写。
   * 这一列的两枚标签各自带悬停面板，含义不靠表头解释。
   *
   * ── 格子为什么由 JS 拼（本次改造）───────────────────────────
   * 列的显隐与顺序可调之后，表头不能再是一段写死的 HTML：要按可见列逐格产出，
   * 顺序与数据行逐格一致（两处都走 visibleColumns）。类名沿用 `.req-xxx`
   * —— 它是列的身份（渲染方按它选表头格、CSS 按它配色、窄窗口按它重排）。
   */
  function headHtml() {
    const cells = visibleColumns().map(column => {
      const cls = column.sel.slice(1) + (column.key === 'dur' ? ' req-num' : '');
      return `<span class="${cls} ta-${column.align}" data-col="${column.key}">${esc(column.label)}</span>`;
    }).join('');
    return `<div class="req-head">${cells}</div>`;
  }

  function emptyText() {
    if (!total) return '暂无请求日志，网关还没有转发过请求';
    return '没有符合筛选条件的请求';
  }

  /**
   * 页脚读数：范围、状态、提供商与模型都写出来，「为什么只有这几条」一眼可查。
   *
   * 后两项显示**当前选中的原值**（提供商按注册表换展示名，取不到就回显 id）——
   * 与列表里那一列同一个口径（见 targetCell），所以读数与行内容对得上。
   */
  function renderSummary() {
    const box = $('req-summary');
    if (!box) return;
    const parts = [RANGE_LABEL[range] || '全部'];
    // 仅看进行中开着时，状态维度的实际取值是 running（状态下拉已停用），
    // 读数按**生效中的条件**写，不写下拉的残留值
    if (runningOnly) parts.push('只看进行中');
    else if ($('req-status')?.value === 'ok') parts.push('只看成功');
    else if ($('req-status')?.value === 'error') parts.push('只看失败');
    const provider = $('req-provider')?.value;
    if (provider) parts.push(window.wbProviders?.labelOf?.(provider) || provider);
    const model = $('req-model')?.value;
    if (model) parts.push(model);
    box.textContent = parts.join(' · ');
  }

  function renderBadge() {
    const badge = $('req-badge');
    if (!badge) return;
    const base = matched === total ? `${total} 条` : `${matched} / ${total} 条`;
    // 进行中读数来自同一次响应的 running 字段（同筛选下 status=0 的条数）。
    // 「仅看进行中」开着时整页都是进行中，再缀一遍就成了复读，省掉
    const live = !runningOnly && runningCount > 0 ? ` · ${runningCount} 进行中` : '';
    badge.className = 'badge';
    badge.textContent = base + live;
  }

  function renderPager() {
    const pageCount = Math.max(1, Math.ceil(matched / PAGE_SIZE));
    const currentPage = Math.min(pageCount, Math.floor(offset / PAGE_SIZE) + 1);
    const info = $('req-page-info');
    if (info) info.textContent = `第 ${currentPage} / ${pageCount} 页`;
    const prev = $('btn-req-prev');
    const next = $('btn-req-next');
    if (prev) prev.disabled = offset <= 0;
    if (next) next.disabled = offset + PAGE_SIZE >= matched;
  }

  /**
   * 渲染请求日志。errorText 有值时列表位置显示错误文案，**不动**计数与页码 ——
   * 那组读数是上一次成功加载的结果，写 0 会让人以为明细被删了；
   * 徽标退成「—」表示「现在这个读数不可信」，比给一个假数字诚实。
   */
  function render(errorText) {
    const list = $('req-list');
    if (!list) return;
    if (errorText) {
      const badge = $('req-badge');
      if (badge) { badge.className = 'badge'; badge.textContent = '—'; }
      list.innerHTML = `<div class="log-empty">${esc(errorText)}</div>`;
      return;
    }
    renderBadge();
    renderSummary();
    renderPager();
    if (!entries.length) {
      list.innerHTML = `<div class="log-empty">${esc(emptyText())}</div>`;
      return;
    }
    list.innerHTML = headHtml() + entries.map(rowHtml).join('');
  }

  // ─── 筛选下拉（提供商 / 模型）─────────────

  /**
   * 用候选清单重建一个筛选下拉，**保留「全部」项与当前选中的值**。
   *
   * ── 为什么必须保住当前选中值（一个真实的坑）─────────────────
   * 清单来自后端（按出现次数降序、上限 200 条）。若当前选中的值不在这次清单里
   * （筛着某个模型时把日志清空了、或它排在第 201 位），重建之后 `select.value`
   * 会被浏览器重置成空 —— 用户看到的筛选条件**静默消失**，而列表还是上一次的
   * 结果（要等下一次 load 才按新条件拉）。所以：选中值不在清单里时把它自己补进去。
   *
   * 第一项（「全部提供商」/「全部模型」）由 HTML 声明，原样保留 ——
   * 它的文案是页面的一部分，不在数据里。
   */
  function fillFilterSelect(id, items, desired) {
    const select = $(id);
    if (!select) return;
    // desired（记忆恢复的值）优先于 DOM 当前值：候选清单是异步填的，
    // 启动时先给 select 赋过一次值，在选项不存在时并不会被记住
    const current = desired === undefined ? select.value : desired;
    const head = select.options[0];
    const headHtml = head ? head.outerHTML : '<option value="">全部</option>';
    const labels = new Map(items.map(item => [String(item.value), String(item.label)]));
    const values = [...labels.keys()].filter(Boolean);
    if (current && !labels.has(current)) labels.set(current, current);
    if (current && !values.includes(current)) values.unshift(current);
    select.innerHTML = headHtml + values.map(value =>
      `<option value="${esc(value)}">${esc(labels.get(value) || value)}</option>`).join('');
    select.value = current;
  }

  /** 上次拉取筛选清单的时刻（节流用；见 refreshFilterOptions） */
  let filterOptionsAt = 0;
  /** 清单的复用窗口：翻页 / 换筛选都会走非静默 load，不值得每次都重算一遍 */
  const FILTER_OPTIONS_TTL_MS = 30_000;

  /**
   * 刷新两个筛选下拉的候选清单（内部节流 30 秒）。
   *
   * ── 清单不随筛选条件收窄（有意的）──────────────────────────
   * 筛了「今天」之后，下拉里仍会列出 30 天前用过的模型。跟着结果集收窄看着更
   * 「聪明」，代价是选中项会在下一次刷新后凭空消失（用户连取消筛选都要重新找）——
   * 而它本来就是「这份日志里出现过什么」的清单，与时间档位无关。
   * 它只随**明细里出现过什么**变，所以新用过的模型名会在下一次进页面
   * （或 30 秒后的下一次非静默 load）出现在下拉里。
   *
   * 失败静默：读不到清单时两个下拉只留「全部」项 —— 筛选器退化成不可用，
   * 列表照常显示（与 `load` 的静默失败同一取向：一个辅助功能不该让整页报错）。
   */
  async function refreshFilterOptions() {
    if (Date.now() - filterOptionsAt < FILTER_OPTIONS_TTL_MS) return;
    filterOptionsAt = Date.now();
    try {
      const filters = await api.getStatsRequestFilters();
      const providers = Array.isArray(filters?.providers) ? filters.providers : [];
      fillFilterSelect('req-provider', providers.map(item => ({
        value: String(item?.id || ''),
        label: String(item?.label || item?.id || ''),
      })), savedFilters.provider);
      const models = Array.isArray(filters?.models) ? filters.models : [];
      fillFilterSelect('req-model', models.map(name => ({
        value: String(name || ''),
        label: String(name || ''),
      })), savedFilters.model);
    } catch (error) {
      console.warn('读取请求日志筛选清单失败，筛选项退化为「全部」:', error.message);
    }
  }

  // ─── 加载 ──────────────────────────────────

  /**
   * 当前筛选条件（**不含分页**）→ URLSearchParams。
   *
   * GET 与 DELETE（清空）共用它：「清空当前筛选结果」必须与列表用的是同一套条件，
   * 两处各写一遍迟早会漂 —— 少传一个参数，用户看到的就是「清空删掉的条数
   * 与筛选出的条数不一致」，而那种 bug 只在同时用两个筛选维度时才出现。
   */
  function filterParams() {
    const params = new URLSearchParams();
    const start = rangeStart(range);
    if (start !== null) params.set('start', String(start));
    // 「仅看进行中」开启时状态固定发 running（后端 status 过滤认的伪状态值），
    // 覆盖状态下拉 —— 「成功 / 失败」与「进行中」是同一维度的互斥取值，
    // 并存只会打架；下拉此刻已被停用（见按钮绑定处），这里只是兜住取值
    const status = runningOnly ? 'running' : ($('req-status')?.value || '');
    if (status) params.set('status', status);
    const provider = $('req-provider')?.value;
    if (provider) params.set('provider', provider);
    const model = $('req-model')?.value;
    if (model) params.set('model', model);
    return params;
  }

  function queryParams() {
    const params = filterParams();
    params.set('offset', String(offset));
    params.set('limit', String(PAGE_SIZE));
    return params.toString();
  }

  async function load({ silent = false, resetPage = false } = {}) {
    // 筛选条件换了就该从第 1 页看起；普通刷新（含轮询）保持当前页
    if (resetPage) offset = 0;
    // 兜一次间隔配置：冷启动时首次读取可能撞上「后端还没起来」而失败，
    // 那时会把兜底值一直用下去（同步过一次就立刻返回，无额外开销）
    void syncAutoRefresh();
    // 非静默调用（进页面 / 翻页 / 换筛选）顺带刷新筛选清单（内部有节流，
    // 见 refreshFilterOptions）—— 轮询不碰它，否则每秒一次全表 GROUP BY
    if (!silent) void refreshFilterOptions();
    // 连点翻页时不做互斥锁，只认最后一次响应：用锁会把后面的点击直接吞掉
    const token = ++seq;
    try {
      const result = await api.getStatsRequests(queryParams());
      if (token !== seq) return;
      entries = Array.isArray(result?.entries) ? result.entries : [];
      total = Number(result?.total) || 0;
      matched = Number(result?.matched) || 0;
      runningCount = Number(result?.running) || 0;
      // 明细被清空或被保留期裁掉后，停在第 5 页会看到一片空白：
      // 先把 offset 夹回最后一页再取一次。夹完必然落在合法页（lastOffset 是
      // 本次响应算出来的），所以不会来回递归；用 return 把这次重取并进同一个 Promise，
      // 调用方（刷新按钮）的 then 才会等到真正拿到数据之后才弹提示。
      const lastOffset = Math.max(0, (Math.ceil(matched / PAGE_SIZE) - 1) * PAGE_SIZE);
      if (offset > lastOffset) {
        offset = lastOffset;
        return load({ silent });
      }
      render();
      // 换筛选（页码回 1）才回顶；普通刷新与翻页保留当前滚动位置，
      // 否则每隔一个刷新周期就把正在看明细的人踢回页首
      if (resetPage) setListScroll('req-list');
    } catch (error) {
      // 静默（轮询）时保留上一屏数据，只当没刷过：把读数清成 0 会让人以为明细被删了
      if (!silent) {
        console.warn('读取请求日志失败:', error.message);
        render('读取请求日志失败，详见控制台');
      }
    }
  }

  /** 列表滚动定位：不带参数即回顶部；元素缺失时静默跳过 */
  function setListScroll(id, top = 0) {
    const list = $(id);
    if (list) list.scrollTop = top;
  }

  // ─── 轮询 ──────────────────────────────────

  /**
   * 起 / 重起轮询定时器。
   *
   * 两个前置条件缺一不可：任务已开启（`autoEnabled`）、间隔为正。
   * 关闭时不排定时器（而不是排一个永不触发的），否则「关掉了但定时器还在跑」
   * 会让「间隔改了却像没生效」变得难排查。
   */
  function startAuto() {
    stopAuto();
    if (!autoEnabled || autoRefreshMs <= 0) return;
    timer = setInterval(() => {
      // 只在本页可见时轮询，避免后台无谓请求
      if (document.hidden || wbApp.currentPage !== 'requests') return;
      // 上一轮还没回来就跳过这一拍（见 `polling` 的说明）
      if (polling) return;
      polling = true;
      void load({ silent: true }).finally(() => { polling = false; });
    }, autoRefreshMs);
  }

  function stopAuto() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  /**
   * 应用「定时任务」页推来的新配置（也用于启动时自读，见 `syncAutoRefresh`）。
   * 形状 = `/api/scheduled-tasks` 里那条 `requestsAutoRefresh`。
   * 传 null / 形状不符时退回默认值（理由与 logs-panel 的同名函数一致）。
   *
   * 这里顺带把 `autoSynced` 置上：配置已经由推送方给过了，
   * 再让待重试的自读去跑一次毫无意义（更糟的是：如果那次读还失败，
   * 会把用户刚在定时任务页改好的间隔**覆盖回兜底值**）。
   */
  function applyAutoRefresh(task) {
    autoSynced = true;
    const interval = Number(task?.interval);
    const valid = task && typeof task === 'object'
      && Number.isFinite(interval) && interval > 0
      && (task.unit === 'seconds' || task.unit === 'minutes');
    if (!valid) {
      autoEnabled = true;
      autoRefreshMs = DEFAULT_AUTO_REFRESH_MS;
    } else {
      autoEnabled = task.enabled !== false;
      autoRefreshMs = task.unit === 'minutes' ? interval * 60_000 : interval * 1000;
    }
    startAuto();
  }

  /**
   * 启动时自己拉一次配置（用户在没进过「定时任务」页时也能拿到正确的间隔）。
   *
   * 读取失败**不**标记为已同步，于是下一次进本页（`load` 里的重试）会再来一次
   * —— 首次失败最常见的原因是「后端还没起来」（冷启动），
   * 一次失败就永久用兜底值，用户会以为「在定时任务页改的间隔没生效」。
   */
  async function syncAutoRefresh() {
    if (autoSynced) return;
    try {
      const list = await api.getScheduledTasks();
      const task = (list?.tasks || []).find(item => item.id === 'requestsAutoRefresh');
      applyAutoRefresh(task || null);
      // 请求成功就标记同步过（哪怕这一条不在清单里 —— 那是后端版本旧）
      autoSynced = Array.isArray(list?.tasks);
    } catch (error) {
      console.warn('读取请求日志自动刷新间隔失败，按默认 1 秒:', error.message);
      applyAutoRefresh(null);
    }
  }

  // ─── 操作 ──────────────────────────────────

  /** 请求日志翻页要真打接口（offset 是后端口径），到边界直接不发请求 */
  function gotoPage(target) {
    const pageCount = Math.max(1, Math.ceil(matched / PAGE_SIZE));
    const next = Math.min(Math.max(1, target), pageCount);
    const nextOffset = (next - 1) * PAGE_SIZE;
    if (nextOffset === offset) return;
    offset = nextOffset;
    setListScroll('req-list');   // 新一页从顶部开始读
    void load();
  }

  /**
   * 清理弹窗用的筛选参数：与 `queryParams` 同一套条件（不含 offset/limit，两者都从
   * `filterParams` 来）。带条件 = 只删命中的明细；不带条件 = 全部清空，此时调用方
   * 要显式带 `all=1`（后端护栏，mode=raw / mode=all 一视同仁，拼串在 request-clear-modal.js）。
   *
   * 返回**查询串**（与 queryParams 一致）而不是 URLSearchParams 对象：
   * 桥接层的 toQuery 只认字符串 / 普通对象，传对象会静默变成「没有参数」，
   * 那样筛选清空就变成了全清 —— 这个 bug 已经踩过一次，别再踩。
   *
   * 供 request-clear-modal.js 取用（预览与 DELETE 都用它）—— 本文件不再直接
   * 打清理接口，删除方式的选择（全部删除 / 仅清空报文原文）与压缩入口都收进弹窗。
   */
  function clearParams() {
    return filterParams().toString();
  }

  // ─── 事件绑定 ──────────────────────────────

  // 时间档位的默认值（「全部」）写在 HTML 里，存过的值在这里纠正
  $('req-range')?.querySelectorAll('.seg-item[data-range]').forEach(item => {
    item.classList.toggle('active', item.dataset.range === range);
  });

  // 状态下拉的选项是静态 HTML，存过的值直接回填（提供商 / 模型两个下拉
  // 的候选是异步的，走 fillFilterSelect 的 desired 入参，见 savedFilters）
  if ($('req-status')) $('req-status').value = savedFilters.status;

  $('req-range')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-range]');
    if (!item) return;
    const next = RANGES.includes(item.dataset.range) ? item.dataset.range : DEFAULT_RANGE;
    if (next === range) return;
    range = next;
    persistRange(next);
    $('req-range')?.querySelectorAll('.seg-item[data-range]').forEach(node => {
      node.classList.toggle('active', node.dataset.range === next);
    });
    void load({ resetPage: true });
  });

  // 「清理」打开清理弹窗（request-clear-modal.js：两种删除方式 + 预览统计 +
  // 压缩数据库都在那边；本文件只负责把当前筛选参数给它，见 clearParams）
  $('btn-req-clear').addEventListener('click', () => {
    void window.wbRequestClearModal?.open?.();
  });
  $('btn-req-prev').addEventListener('click', () => gotoPage(Math.floor(offset / PAGE_SIZE)));
  $('btn-req-next').addEventListener('click', () => gotoPage(Math.floor(offset / PAGE_SIZE) + 2));

  // ─── 仅看进行中 ────────────────────────────
  //
  // 进行中的请求（status=0）默认混在全量列表里，转发一卡住不容易第一时间看到。
  // 开关与其它筛选并存（提供商 / 模型 / 时间照常生效），唯独与状态下拉互斥 ——
  // 「成功 / 失败」和「进行中」是 status 维度上的并列取值，同时发两个只会打架；
  // 开启时把下拉停用（视觉上说明条件已被接管），关闭时还原。
  const runningButton = $('btn-req-running');
  runningButton?.addEventListener('click', () => {
    runningOnly = !runningOnly;
    runningButton.classList.toggle('active', runningOnly);
    runningButton.setAttribute('aria-pressed', String(runningOnly));
    const statusSelect = $('req-status');
    if (statusSelect) {
      statusSelect.disabled = runningOnly;
      statusSelect.title = runningOnly
        ? '「仅看进行中」开启时，状态固定为进行中'
        : '按请求结果筛选';
    }
    // 筛选换结果集，回到第 1 页（与三个下拉同一取向）
    void load({ resetPage: true });
  });

  // 四个筛选维度都会换掉结果集，页码必须回到第 1 页，否则停的位置没有意义。
  // 三个下拉共用一条绑定（它们的语义完全一致，逐个写三遍只会多三处要同步的地方）；
  // 变更同时落盘 —— 下次启动按同一批条件恢复（见 savedFilters 的说明）。
  for (const id of ['req-status', 'req-provider', 'req-model']) {
    $(id)?.addEventListener('change', () => {
      savedFilters[id.slice(4)] = $(id)?.value || '';
      window.wbFilterMemory?.save(FILTERS_KEY, savedFilters);
      void load({ resetPage: true });
    });
  }

  // ─── 详情弹窗（上游原始报文）─────────────────
  //
  // 实现整体拆到 **request-detail.js**（本次改造）：那一块与列表渲染零耦合
  // （自己按 id 拉报文、自己管弹窗的开合与分段状态），而本文件在加上
  // 「重试列弹层」与「详情分段」之后已到 900 行，超过项目「单文件不过 800 行」
  // 的约定。拆分口径与项目既有先例一致（usage-panel.js 从 accounts-model.js
  // 拆出、request-hover.js 与本文件的分工）。
  //
  // 本文件只留两件事：
  //   ① 列表里的「详情」按钮 → wbRequestDetail.open(id, row)（下面那个委托，
  //      row 是从当前一屏数据里反查出的行对象，弹窗的「请求详情」标签要吃它）；
  //   ② 弹窗自己的关闭/标签事件全在那边绑（它独占 #req-detail-modal 那组 DOM），
  //      本文件不再碰那些节点。
  $('req-list')?.addEventListener('click', event => {
    const button = event.target.closest('[data-detail]');
    if (!button) return;
    const id = button.dataset.detail;
    // 当前行对象一并传过去（详情弹窗的「请求详情」标签要吃行上的完整字段）：
    // 与 wbRequestHover.entryOf 同一套反查口径 —— id 优先、无 id 的旧行退回 ts，
    // 都查不到（列表在点开前恰好刷新过）传 null，弹窗里显示「请重试」的空态
    const row = entries.find(item => String(item?.id || '') === id)
      || entries.find(item => String(item?.ts || '') === id)
      || null;
    void window.wbRequestDetail?.open?.(id, row);
  });

  // ─── 重试列 / 敏感词的悬停面板 ───────────────
  //
  // 反查而不是把数据写进属性：标签上只留一个 `data-req-id`（行的身份键），
  // 面板内容由本函数从**当前这一屏的数据**（`entries`）里找出来现算。
  // 这样既省掉每行几百 KB 的属性文本（见 retryCell 的说明），也自动跟着
  // 数据的更新走 —— 列表重绘时 `entries` 已经换成新的一屏，
  // 面板永远弹的是「屏幕上那一条」，不会弹出上一屏的残留。
  //
  // 找不到时的兜底：返回 null（面板则不显示）。理论上不该发生 ——
  // 标签与数据在同一次 `render` 里生成，`id` 为空的行退回 `ts`，
  // 而 `ts` 在同一屏里可能重复（同一毫秒的并发请求），所以按 id 优先、
  // 找不到再用 ts 匹配第一条（重复时弹第一条的明细，比什么都不弹好排障）。
  window.wbRequestHover?.bind?.({
    host: $('req-list'),
    entryOf: tag => {
      const key = tag?.dataset?.reqId || '';
      if (!key) return null;
      return entries.find(item => String(item?.id || '') === key)
        || entries.find(item => String(item?.ts || '') === key)
        || null;
    },
  });

  window.wbRequestsPanel = {
    load,
    // 「定时任务」页改完间隔后推给本面板（见 applyAutoRefresh 的说明）
    applyAutoRefresh,
    // 当前可见列（顺序即配置顺序）：table-columns.js 拼 `--req-cols` 轨道时
    // 读它 —— 轨道条数与顺序必须与渲染出的格子一一对应，两处同源才不会错位
    visibleColumns,
    // 当前筛选参数（查询串形态）：清理弹窗的预览与 DELETE 用同一份条件，
    // 「预览说删 N 条」与「确认删掉的那批」才能对上（后端三条路由共用同一份
    // FilterPlan，前端这里也不能第二套口径）
    clearParams,
    /**
     * 清理完成后的收口刷新：筛选清单的节流计时清零（明细被清后可能整批
     * 模型名 / 提供商都消失了，下拉里不该再列着），并回到第 1 页重拉。
     */
    notifyCleared() {
      filterOptionsAt = 0;
      return load({ resetPage: true });
    },
  };

  // 首屏自持加载：即便 app.js 的 refresh 失败，本页也能独立显示真实状态。
  // 自动刷新先按兜底值起一次（页面立刻有轮询），同时异步读配置校准 ——
  // 不 await：一次本地接口调用不该拖住首屏，读到后 applyAutoRefresh 会重启定时器。
  void load({ silent: true });
  // 筛选清单与首屏数据并行拉（都是本地接口，互不依赖）：清单晚到一点不影响
  // 列表显示，而它决定两个下拉什么时候可用（首屏那次 load 是静默的，不会带它）
  void refreshFilterOptions();
  startAuto();
  void syncAutoRefresh();
})();
