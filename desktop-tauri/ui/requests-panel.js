/* Agent2API · 请求日志面板（网关转发明细 · 筛选 / 分页 / 自动刷新） */
/* global workbuddyDesktop, wbApp, wbRequestHover, wbRequestDetail */

/**
 * 「请求日志」页的自持面板：网关每次转发到上游的请求日志。
 * 与 logs-panel.js（系统事件）同构但互不依赖：两边数据源、筛选维度、
 * 分页口径都不同，拆成两个模块后各自的轮询与状态不再互相牵连。
 *
 * ── 数据口径 ────────────────────────────────────────────────
 * 明细按保留期（默认 30 天）存盘，条数没有上限，一次拉全不现实，
 * 所以走后端 offset/limit 真分页；每页 50 条（后端默认值，这里显式传，
 * 页数才可算），上限 500 条由后端夹紧。后端支持的筛选只有时间区间与
 * 状态（ok/error）—— 模型 / 提供商筛选、关键词搜索不在接口契约里。
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
  const { esc, toast } = wbApp;

  /**
   * 自动刷新间隔兜底值（毫秒）= 后端的默认间隔
   * （`DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS`）：读不到配置时与用户没改过时一致。
   * 实际值由「定时任务」页决定（见模块头）。
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
  let panelBusy = false;

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
  let entries = [];
  let total = 0;      // 明细总量（未过滤）
  let matched = 0;    // 命中筛选条件的条数
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

  function statusCell(entry) {
    const status = Number(entry.status) || 0;
    const ok = isOk(entry);
    // status 为 0 是后端契约里的「还没发出请求就失败」，直接写 0 会被读成 HTTP 状态码，
    // 所以退成「失败」两个字。
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
   * 表头已经收窄成「重试」（见 REQ_HEAD 的说明），这一列只有 52px 宽：
   * 「敏感词」三个字会把标签撑得比「重试 N」还宽，两枚标签竖排时右边留一大块
   * 空白、列也容易被挤到换行。命中是**有没有**的问题，不是**几个字**的问题，
   * 所以缩成一个「敏」字，完整含义交给两个既有出口：`title` 悬停提示与
   * 点击/聚焦弹出的富文本面板（那里面仍然逐词列出命中明细，一个字都没少）。
   * `aria-label` 补上完整说法，读屏软件不会只念出一个孤零零的「敏」。
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
        + ` data-req-hover="chain" data-req-id="${esc(key)}"`
        + ` title="查看每次尝试的提供商、账号与重试原因">`
        + `重试${count > 1 ? ` ${count}` : ''}</button>`);
    }
    // 敏感词命中：判据是「命中表非空」（后端 sensitiveHits 字段）。
    // 不用 attempts 那类计数 —— 命中是这条请求的属性，与重试次数无关
    // （一次成功的请求同样可能命中敏感词，那时这一列只有这一枚标签）。
    if (Array.isArray(entry.sensitiveHits) && entry.sensitiveHits.length) {
      tags.push(`<button type="button" class="badge tag req-hover-tag sensitive"`
        + ` data-req-hover="sensitive" data-req-id="${esc(key)}"`
        + ` title="命中了敏感词，点击查看明细" aria-label="命中了敏感词">敏</button>`);
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
   * 双名缺失时退回单行：`model` 缺失或旧数据没有 clientModel / upstreamModel
   * （这两个键是后加的），请求没走到上游的那次失败也没有上游名。
   */
  function modelCell(entry) {
    const client = String(entry.clientModel ?? '').trim();
    const upstream = String(entry.upstreamModel ?? '').trim();
    const shown = String(entry.model ?? '').trim();
    if (!client || !upstream || upstream.toLowerCase() === client.toLowerCase()) {
      return `<span class="req-model" title="${esc(shown)}">${esc(shown || '—')}</span>`;
    }
    return `<span class="req-model req-model-split">`
      + `<span class="req-model-line" title="转发到上游的模型名">⬆️ ${esc(upstream)}</span>`
      + `<span class="req-model-line sub" title="下游请求的模型名">⬇️ ${esc(client)}</span></span>`;
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

  function rowHtml(entry) {
    const ok = isOk(entry);
    const error = entry.error ? String(entry.error) : '';
    return `<div class="req-row${ok ? '' : ' failed'}">
        ${timeCell(entry.ts)}
        ${targetCell(entry)}
        ${retryCell(entry)}
        ${statusCell(entry)}
        ${modelCell(entry)}
        <span class="req-num req-dur"><span class="req-dur-line">${esc(formatDuration(entry.durationMs))}</span><span class="req-dur-line sub" title="首响：上游首帧到达的耗时">首响 ${esc(formatFirstResponse(entry.firstResponseMs))}</span></span>
        ${usageCell(entry)}
        ${error
          ? `<span class="req-error" title="${esc(error)}">${esc(error)}</span>`
          : '<span class="req-none req-error-cell">-</span>'}
        ${detailCell(entry)}
      </div>`;
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
   * 这一列的两枚标签各自带悬停面板，含义不靠表头解释。 */
  const REQ_HEAD = `<div class="req-head">
      <span class="req-time">时间</span>
      <span class="req-target">提供商 / 账号</span>
      <span class="req-retry">重试</span>
      <span class="req-status">状态</span>
      <span class="req-model">模型</span>
      <span class="req-num req-dur">用时</span>
      <span class="req-usage">用量</span>
      <span class="req-error-cell">错误</span>
      <span class="req-detail">详情</span>
    </div>`;

  function emptyText() {
    if (!total) return '暂无请求日志，网关还没有转发过请求';
    return '没有符合筛选条件的请求';
  }

  /** 页脚读数：范围与状态都写出来，「为什么只有这几条」一眼可查 */
  function renderSummary() {
    const box = $('req-summary');
    if (!box) return;
    const status = $('req-status')?.value;
    const parts = [RANGE_LABEL[range] || '全部'];
    if (status === 'ok') parts.push('只看成功');
    else if (status === 'error') parts.push('只看失败');
    box.textContent = parts.join(' · ');
  }

  function renderBadge() {
    const badge = $('req-badge');
    if (!badge) return;
    badge.className = 'badge';
    badge.textContent = matched === total ? `${total} 条` : `${matched} / ${total} 条`;
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
    list.innerHTML = REQ_HEAD + entries.map(rowHtml).join('');
  }

  // ─── 加载 ──────────────────────────────────

  function queryParams() {
    const params = new URLSearchParams();
    const start = rangeStart(range);
    const status = $('req-status')?.value;
    if (start !== null) params.set('start', String(start));
    if (status) params.set('status', status);
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
    // 连点翻页时不做互斥锁，只认最后一次响应：用锁会把后面的点击直接吞掉
    const token = ++seq;
    try {
      const result = await api.getStatsRequests(queryParams());
      if (token !== seq) return;
      entries = Array.isArray(result?.entries) ? result.entries : [];
      total = Number(result?.total) || 0;
      matched = Number(result?.matched) || 0;
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
      console.warn('读取请求日志自动刷新间隔失败，按默认 10 秒:', error.message);
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

  /** 统一忙碌守卫：按钮禁用 + 文案切换，避免重复点击（与 logs-panel 的 guard 同一写法） */
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

  /**
   * 清空用的筛选参数：与 queryParams 同一套条件（不含 offset/limit）。
   * 带条件 = 只删命中的明细（受影响日期的按天聚合由后端重算）；
   * 不带条件 = 全部清空，此时调用方要显式带 `all=1`（后端护栏，见 clearRequests）。
   *
   * 返回**查询串**（与 queryParams 一致）而不是 URLSearchParams 对象：
   * 桥接层的 toQuery 只认字符串 / 普通对象，传对象会静默变成「没有参数」，
   * 那样筛选清空就变成了全清 —— 这个 bug 已经踩过一次，别再踩。
   */
  function clearParams() {
    const params = new URLSearchParams();
    const start = rangeStart(range);
    const status = $('req-status')?.value;
    if (start !== null) params.set('start', String(start));
    if (status) params.set('status', status);
    return params.toString();
  }

  async function clearRequests() {
    const query = clearParams();
    const hasFilters = query.length > 0;
    const message = hasFilters
      ? `确定清空当前筛选出的 <strong>${matched}</strong> 条请求？按天聚合会随删除重算，此操作无法恢复。`
      : '确定清空<strong>全部</strong>请求日志？按天聚合会一并清空，此操作无法恢复。';
    // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行（等于没有确认），
    // 危险确认一律走自绘弹窗（wbConfirm，见 confirm-dialog.js）
    if (!(await window.wbConfirm?.ask?.({
      title: '清空请求日志',
      html: message,
      okText: '清空',
      okClass: 'danger',
    }))) return;
    await guard($('btn-req-clear'), '清空中…', async () => {
      // 无筛选时显式带 all=1：后端要求「清空全部」必须显式声明，
      // 免得哪天参数漏传又被当成全清（前端写错一次就是全部数据没了）
      await api.clearStatsRequests(hasFilters ? query : 'all=1');
      // 清空后没有「当前页」可言：回到第 1 页并把滚动位置一起归零
      await load({ resetPage: true });
      toast('请求日志已清空');
    });
  }

  // ─── 事件绑定 ──────────────────────────────

  // 时间档位的默认值（「全部」）写在 HTML 里，存过的值在这里纠正
  $('req-range')?.querySelectorAll('.seg-item[data-range]').forEach(item => {
    item.classList.toggle('active', item.dataset.range === range);
  });

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

  $('btn-req-clear').addEventListener('click', clearRequests);
  $('btn-req-prev').addEventListener('click', () => gotoPage(Math.floor(offset / PAGE_SIZE)));
  $('btn-req-next').addEventListener('click', () => gotoPage(Math.floor(offset / PAGE_SIZE) + 2));
  // 状态筛选会换掉结果集，页码必须回到第 1 页，否则停的位置没有意义
  $('req-status').addEventListener('change', () => load({ resetPage: true }));

  // ─── 详情弹窗（上游原始报文）─────────────────
  //
  // 实现整体拆到 **request-detail.js**（本次改造）：那一块与列表渲染零耦合
  // （自己按 id 拉报文、自己管弹窗的开合与分段状态），而本文件在加上
  // 「重试列弹层」与「详情分段」之后已到 900 行，超过项目「单文件不过 800 行」
  // 的约定。拆分口径与项目既有先例一致（usage-panel.js 从 accounts-model.js
  // 拆出、request-hover.js 与本文件的分工）。
  //
  // 本文件只留两件事：
  //   ① 列表里的「详情」按钮 → wbRequestDetail.open(id)（下面那个委托）；
  //   ② 弹窗自己的关闭/分段事件全在那边绑（它独占 #req-detail-modal 那组 DOM），
  //      本文件不再碰那些节点。
  $('req-list')?.addEventListener('click', event => {
    const button = event.target.closest('[data-detail]');
    if (!button) return;
    void window.wbRequestDetail?.open?.(button.dataset.detail);
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
  };

  // 首屏自持加载：即便 app.js 的 refresh 失败，本页也能独立显示真实状态。
  // 自动刷新先按兜底值起一次（页面立刻有轮询），同时异步读配置校准 ——
  // 不 await：一次本地接口调用不该拖住首屏，读到后 applyAutoRefresh 会重启定时器。
  void load({ silent: true });
  startAuto();
  void syncAutoRefresh();
})();
