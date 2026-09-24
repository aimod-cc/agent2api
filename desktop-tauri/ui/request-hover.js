/* Agent2API · 请求日志的富文本悬停面板（重试链 / 敏感词命中） */
/* global wbApp */

/**
 * 「请求日志」页里那两枚标签的**悬停面板**：重试链与敏感词命中。
 *
 * ── 这一块为什么是「逐请求日志的唯一去处」（本次改造）────────────
 * 改造前，一次转发的过程事实散在两个页面：换号顺延、退避重试、401 刷新、
 * 限额降级、上游报错、代理回退各自往**运行日志**写一行（有的按请求刷屏），
 * 而请求日志这边只能看到「换了几次号」与最后一次的结果。现在这些事实全部
 * 收进尝试明细（`attemptDetails` 的 `account` / `retries` / `notice`），
 * 本模块负责把它们渲染出来 —— 于是排障时**只看这一处**就够：
 *   · 谁承载了每一轮（提供商 + 账号）
 *   · 每一轮成没成、失败原因
 *   · 每一轮内部退避重试了几次、每次为什么、等了多久
 *   · 出口有没有降级（代理不可用 → 直连）
 *   · 配合「详情」列的调试报文，敏感词场景下能看到完整的请求与响应原文
 *     （这正是「有敏感词时在运行日志里看不全」的解法：报文归请求日志）
 *
 * ── 为什么单独成文件 ──────────────────────────────────────────
 * 与 tooltip.js / select.js 同一类东西：一块自洽的浮层交互（定位、翻转、
 * 事件、生命周期）。但**不复用 tooltip.js**，因为它的契约是纯文本 ——
 * `bubble.textContent = text`，内容来自 `data-tip` 属性。这里要弹的是一份
 * 结构化的富文本（切换路径一串箭头、每次尝试一行、重试子行、故障行红字、
 * 成功行绿字、敏感词是「词 × 次数」的列表），塞进属性会被 HTML 转义成
 * 一堆源码，用户看到的是标签而不是面板。
 *
 * 另一条路是给 tooltip.js 加一个「HTML 内容」开关，但那会让一个被全站几十处
 * `data-tip` 依赖的公共组件多出一条**安全上更敏感**的路径（`innerHTML`）——
 * 现有的纯文本契约正是它简单可靠的原因。所以这里另起一个只服务请求日志表的
 * 浮层，与 OmniProxy 的 `FailoverTip` / `SensitiveMaskedTag` 一一对应。
 *
 * ── 与 requests-panel.js 的分工 ────────────────────────────────
 *   · 本文件      内容 HTML 的构造（`chainHtml` / `retryRowsHtml` /
 *                 `sensitiveHtml`）+ 浮层机制 + 「标签该不该出现」的判据
 *                 （`hasProcessFacts`，因为那要读明细内部的字段）
 *   · 请求日志页  只负责渲染那两枚标签，并把「怎么从标签反查数据」告诉本模块
 *                 （`bind({ host, entryOf })`）
 * 反查而不是把数据塞进 `data-*`：一次尝试明细可达 24 条、错误摘要 200 字符，
 * 每页 50 行就是几百 KB 的属性文本 —— 那会拖慢整表重绘，而数据本来就在
 * `entries` 数组里（按行号反查是常数时间，且列表重绘时标签与数据同生共死，
 * 不存在「标签还在、数据换了」的错位）。
 *
 * ── 定位与生命周期 ────────────────────────────────────────────
 * `position: fixed` 挂在 body 上：`.panel` 是 `overflow: hidden`，absolute 的
 * 浮层会被卡片裁掉（tooltip.js / select.js 踩的是同一个坑）。定位策略照抄
 * tooltip.js：默认在锚点下方，放不下且上方更宽裕时翻到上方；贴边时夹进视口。
 *
 * 与 tooltip.js 的另一处差别：请求日志页默认 1 秒一拍自动刷新、每次整表重绘
 * （`list.innerHTML = ...`），锚点节点会被换成新的。所以宿主在重绘前后各通知
 * 一次（`beforeListRedraw` / `afterListRedraw`），本模块把锚点迁到新节点上 ——
 * 不迁移的话，重绘落在 150ms 打开延迟窗口里时会拿**游离节点**定位，
 * `getBoundingClientRect()` 全 0，面板落在视口左上角；而且游离节点收不到
 * pointerout，面板开了就不会自己关（一次实测的 bug）。
 *
 * 隐藏用 `display: none`（而不是 `visibility` / 透明度）：浮层里有长文本，
 * 让它继续保持布局会参与每帧的重排计算，而它绝大多数时间是不可见的。
 */
(() => {
  const { esc } = wbApp;

  /** 悬停进入 / 离开的延迟（ms）：与 tooltip.js 同一组值，手感一致 */
  const SHOW_DELAY = 150;
  const HIDE_DELAY = 80;
  /** 距视口边缘的安全距离，以及浮层与锚点的间距（箭头落在这段间隙里） */
  const EDGE = 8;
  const GAP = 8;
  /**
   * 面板宽度的下限（px）：内容再短也至少这么宽。
   *
   * 重试链面板里最长的一行是「尝试 N · 提供商 账号 → 失败（状态码）：错误摘要」，
   * 头部加常见长度的错误摘要就有 400–600px —— 下限给足，常见场景整行显示。
   * 敏感词面板是「词 × 次数」的列表，内容本身不长，用较小的基准宽度即可
   * （铺太宽会让词与次数隔得老远，反而难读）。
   */
  const MIN_PANEL_WIDTH = 520;
  const MIN_PANEL_WIDTH_SENSITIVE = 220;
  /**
   * 面板宽度的上限（px，还要再夹进视口可用宽度）。
   *
   * 内容自适应负责「够宽」，这里收住「过宽」：一条 200 字符的上游报错
   * 能把 max-content 顶到视口满宽，而铺满整屏的一行 12px 字读起来很累 ——
   * 超出的部分交给面板内部的换行（见 .rh-bad 的 overflow-wrap）。
   */
  const MAX_PANEL_WIDTH = 900;

  /** 命中词列表最多显示几行：这一块是「命中了什么」的快照，不是词表编辑器。
   *  按次数降序取前 N 条 —— 一份几十个词的词表被整篇命中时，
   *  全列出来会把面板撑得比屏幕还高，而尾部那些「命中 1 次」的词信息量最小。 */
  const MAX_TERM_ROWS = 12;

  /** 面板节点（全局一个：同屏同时只允许一条说明，复用节点也让进出动画连得上） */
  const panel = document.createElement('div');
  panel.className = 'req-hover';
  panel.setAttribute('role', 'tooltip');

  let anchor = null;      // 当前挂着的标签
  let showTimer = 0;
  let hideTimer = 0;
  /**
   * 延迟窗口里排着打开的那枚标签（`showTimer` 的排队目标）。
   *
   * 需要单独记：定时器排上时 `anchor` 还是 null（面板还没开），而 pointerout
   * 的取消判据此前只看 `anchor` —— 于是「悬一下就走」照样会弹出，列表重绘
   * 把标签删掉时浏览器补发的 pointerout 也拦不住这个定时器（见 pointerout
   * 处理器与 open 的游离节点判据）。
   */
  let pendingTag = null;

  /** 宿主列表与「标签 → 数据」的反查函数（bind 时记下；整表重绘后找回锚点要用） */
  let hostEl = null;
  let entryOfFn = null;
  /**
   * 重绘期间的锚点处置意图（见 beforeListRedraw / afterListRedraw）：
   * `null` = 没有重绘在进行；`{ action: 'move', kind, key }` = 迁移；`{ action: 'close' }` = 收起。
   */
  let remap = null;

  function cancelTimers() {
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    showTimer = 0;
    hideTimer = 0;
    pendingTag = null;
  }

  // ─── 内容构造（纯函数，输入是请求日志条目）───────────────────

  /**
   * provider id → 展示名。三级兜底与请求日志的「提供商」列**逐字同源**
   * （见 requests-panel.js 的 targetCell）：后端 label → 前端 providers 目录 →
   * 原样回显 id。同源是必要的：同一行的提供商列显示「小浣熊」而悬停面板显示
   * `raccoon`，读起来像两个不同的家。
   */
  function providerLabel(id) {
    const key = String(id ?? '').trim();
    if (!key) return '';
    return window.wbProviders?.labelOf?.(key) || key;
  }

  /** 一次尝试的明细数组（后端字段是 attemptDetails；旧行没有该键 → 空表） */
  const detailsOf = entry => (Array.isArray(entry?.attemptDetails) ? entry.attemptDetails : []);

  /** 一次尝试内部的退避重试数组（旧明细没有该键 → 空表） */
  const retriesOf = item => (Array.isArray(item?.retries) ? item.retries : []);

  /**
   * 这条请求是否有**值得展示的过程事实**（决定「重试」那枚标签显不显示）。
   *
   * ── 判据为什么不是 `attempts > 1`（本次改造）─────────────────
   * `attempts` 只数**账号轮换**（口径见后端 `TelemetrySnapshot::attempts`），
   * 而同账号内的退避重试（11128 敏感词拦截、瞬时 5xx、传输层失败、401 刷新）
   * 不计入它 —— 一次被 11128 拦下、重试 3 次后成功的请求，`attempts` 仍是 1。
   * 改造前这类请求在列表里**连标签都不出现**（而运行日志那边刷了 3 行），
   * 用户只能去「日志」页看。现在重试链进了明细，判据要跟着扩成「换过号
   * **或**重试过**或**有过提示」，否则新采集到的重试信息永远显示不出来。
   *
   * 敏感词命中不在这里判：它是同一列里另一枚标签（`sensitiveHits`）的事，
   * 两者各自独立出现（见 requests-panel.js 的 retryCell）。
   */
  function hasProcessFacts(entry) {
    if ((Number(entry?.attempts) || 1) > 1) return true;
    return detailsOf(entry).some(item => retriesOf(item).length > 0 || item?.notice);
  }

  /**
   * 切换路径行：`A → B → C`。
   *
   * **只在真实发生过 ≥2 次尝试时显示**（与 OmniProxy 同判据）：只有一次尝试时
   * 那串箭头就是「A → 成功」，没有信息量；而这一列的标签本来就只在
   * 有过程事实时出现，所以这条判据主要是防「明细比 attempts 短」的边界
   * （截断、或部分尝试没采到）。
   */
  function chainHtml(details) {
    if (details.length < 2) return '';
    const names = details.map(item => providerLabel(item.provider) || '未知');
    return `<div class="rh-chain"><span class="rh-chain-k">切换路径：</span>`
      + `<span class="rh-chain-v">${esc(names.join(' → '))}</span></div>`;
  }

  /**
   * 一次尝试内部的退避重试子行：`↻ 重试 N 次` + 逐条原因。
   *
   * 挂在它所属的那一行尝试下面（缩进），而不是作为并列的尝试行 —— 它们是
   * **同一轮账号内**的重发（换的是时间不是账号），并列会让「切换路径」那串
   * 箭头里混进一串同名项，把真正的换号链埋掉（口径见后端 `AttemptDetail::retries`）。
   *
   * 逐条文案：`原因（HTTP 状态码，无则省略），X秒后重试`。
   */
  function retryRowsHtml(item) {
    const retries = retriesOf(item);
    if (!retries.length) return '';
    const head = `<div class="rh-retry-head">↻ 重试 ${retries.length} 次</div>`;
    const rows = retries.map((retry) => {
      const reason = String(retry?.reason ?? '').trim();
      const status = Number(retry?.status);
      const hasStatus = retry?.status !== null && retry?.status !== undefined
        && Number.isFinite(status);
      const delayMs = Number(retry?.delayMs);
      const delay = Number.isFinite(delayMs) && delayMs > 0
        ? `，${esc(formatDelay(delayMs))}后重试`
        : '';
      return `<div class="rh-retry-row"><span class="rh-retry-why">${esc(reason || '未知原因')}</span>`
        + (hasStatus ? `<span class="rh-retry-status">HTTP ${esc(String(status))}</span>` : '')
        + (delay ? `<span class="rh-dim">${delay}</span>` : '')
        + '</div>';
    }).join('');
    return `<div class="rh-retry">${head}${rows}</div>`;
  }

  /** 退避时长的可读形态（X秒；不足 1 秒给 X毫秒） */
  function formatDelay(ms) {
    return ms >= 1000 ? `${Math.round(ms / 1000)}秒` : `${Math.round(ms)}毫秒`;
  }

  /**
   * 单次尝试的一行：`尝试 N · 提供商（账号）→ 成功(200) / 失败(500)：错误摘要`。
   *
   * 三种结局的判据与颜色：
   *   · `error` 非空         → 失败（红），带状态码（可能没有：传输层失败）
   *   · `status` 有值、无错误 → 成功（绿），带状态码
   *   · 两者都无             → 未定论（淡灰）。**两种来源**：这一轮还在飞
   *     （`inFlight`，进行中行的最后一条明细 —— 转发一开始就在途回写，
   *     所以这是常态），以及被手工改过的库。前者写「进行中…」，后者才写
   *     「无结果记录」：对一条正在跑的请求说「无结果」会被读成它已经失败。
   *
   * 账号名（`item.account`）挂在提供商名后面：一家可以有多个账号，
   * 「WorkBuddy / aibjchat001@gmail.com」比只有家名更能定位到那一轮
   * （改造前这个信息只在运行日志的「按队列顺延」那行里，请求日志看不到）。
   *
   * 下面还跟着这一轮的重试子行与提示行（有才显示）。
   */
  function attemptRowHtml(item, index, inFlight) {
    const name = providerLabel(item?.provider);
    const account = String(item?.account ?? '').trim();
    const status = Number(item?.status);
    const hasStatus = item?.status !== null && item?.status !== undefined && Number.isFinite(status);
    const error = item?.error ? String(item.error) : '';
    // 序号从 1 起（后端明细数组本身就是发生顺序，不需要另存 attempt_no）
    const head = `<span class="rh-no">尝试 ${index + 1} ·</span>`
      + `<span class="rh-who">${esc(name || '未知')}</span>`
      + (account ? `<span class="rh-account">${esc(account)}</span>` : '')
      + '<span class="rh-arrow">→</span>';
    const notice = String(item?.notice ?? '').trim();
    const extras = (notice ? `<div class="rh-notice">⚠️ ${esc(notice)}</div>` : '')
      + retryRowsHtml(item);
    const body = error
      ? `<span class="rh-bad">失败${
        hasStatus ? `（${esc(String(status))}）` : ''}：${esc(error)}</span>`
      : hasStatus
        ? `<span class="rh-ok">成功（${esc(String(status))}）</span>`
        // 有状态码之外的最后一种：状态码缺失且没有错误摘要（见上面「未定论」）
        : inFlight
          ? '<span class="rh-dim">进行中…</span>'
          : '<span class="rh-dim">无结果记录</span>';
    return `<div class="rh-row">${head}${body}</div>${extras}`;
  }

  /**
   * 重试面板的内容（对应 OmniProxy 的 `FailoverTip`）。
   *
   * ── 两种形态 ────────────────────────────────────────────────
   *   ① 有明细：切换路径 + 每次尝试。
   *   ② 没有明细（旧数据 / 转发前就失败）：给一句「共 N 次尝试，最终由 X 承载」
   *      —— 这条记录来自「尝试明细」这个字段上线之前，明细确实拿不到，
   *      但已有的两个读数（次数、最终承载者）仍然值得显示。
   *      **不编造中间过程**：那会是猜，而不是事实。
   *
   * ── 进行中行也走①（本次改造）────────────────────────────────
   * 转发一开始就在途回写，所以还在跑的行同样有明细，最后一条的
   * `status` / `error` 都为空 —— 那是「这一轮还在飞」，由 `attemptRowHtml`
   * 的第三个参数渲染成「进行中…」（判据与列表的进行中徽章同一个：
   * status=0 且没有错误摘要）。
   */
  function chainPanelHtml(entry) {
    const attempts = Number(entry?.attempts) || 1;
    const details = detailsOf(entry);
    if (!details.length) {
      const finalProvider = providerLabel(entry?.provider);
      const sentence = finalProvider
        ? `共 ${attempts} 次尝试，最终由 ${finalProvider} 承载`
        : `共 ${attempts} 次尝试`;
      return `<div class="rh-row rh-dim">${esc(sentence)}</div>`
        + '<div class="rh-row rh-dim">这条记录的尝试明细未采集（该字段上线前的旧数据，或请求在转发前就失败）</div>';
    }
    // 明细被体积闸截断时如实说明（保头：留下的是最早那几轮）。判据是
    // 「明细比 attempts 少」，与后端的 MAX_ATTEMPT_DETAILS 无关 —— 上限值
    // 改了这条提示仍然成立。
    const truncated = details.length < attempts
      ? `<div class="rh-row rh-dim">另有 ${attempts - details.length} 次尝试未记录明细（只保留最早的 ${details.length} 条）</div>`
      : '';
    // 「还在飞」只可能是最后一条：尝试是严格串行的，前面的轮次一旦定局就不再
    // 变化（后端的口径见 `usage::AttemptDetail::status`）
    const running = (Number(entry?.status) || 0) === 0 && !entry?.error;
    return chainHtml(details)
      + details.map((item, index) => (
        attemptRowHtml(item, index, running && index === details.length - 1)
      )).join('')
      + truncated;
  }

  /** 敏感词命中明细（后端字段是 sensitiveHits；旧行没有该键 → 空表） */
  const hitsOf = entry => (Array.isArray(entry?.sensitiveHits) ? entry.sensitiveHits : []);

  /**
   * 敏感词面板的内容（对应 OmniProxy 的 `SensitiveMaskedTag`）：
   * 标题 + 「词 × 次数」列表。
   *
   * 没有命中明细（只有布尔事实的旧数据）时给一句如实说明 —— 标签本身是由
   * 「命中表非空」驱动的，所以这条分支在正常数据下走不到；它兜住的是
   * 「有人只改了 attempts 之外的字段」那种手工改动。
   */
  function sensitivePanelHtml(entry) {
    const hits = hitsOf(entry);
    if (!hits.length) {
      return '<div class="rh-row rh-dim">这条记录命中了敏感词，但没有留下命中明细</div>';
    }
    // 后端已按次数降序给出，这里不重排（顺序定义只有一处，见 usage.rs 的说明）
    const rows = hits.slice(0, MAX_TERM_ROWS)
      .map(hit => `<div class="rh-row rh-hit"><span class="rh-hit-w">${esc(String(hit?.word ?? ''))}</span>`
        + `<span class="rh-hit-c">× ${esc(String(Number(hit?.count) || 0))}</span></div>`)
      .join('');
    const more = hits.length > MAX_TERM_ROWS
      ? `<div class="rh-row rh-dim">另有 ${hits.length - MAX_TERM_ROWS} 个词命中</div>`
      : '';
    return `<div class="rh-title">命中的敏感词</div>${rows}${more}`;
  }

  // ─── 浮层机制 ──────────────────────────────────────────────

  /** 按锚点位置摆面板：默认下方，下方放不下且上方更宽裕时翻到上方 */
  function place() {
    if (!anchor) return;
    const rect = anchor.getBoundingClientRect();
    const avail = window.innerWidth - EDGE * 2;
    // 宽度按内容自适应再夹进视口：切换路径那行可能很长（三家的中文名 + 箭头），
    // 错误摘要更长。用 max-content 量出理想宽度，再夹到下限（MIN_PANEL_WIDTH）
    // 与两个上限（视口可用宽度、MAX_PANEL_WIDTH），放不下的部分交给面板内部
    // 的换行与滚动。
    panel.style.maxWidth = 'none';
    panel.style.width = 'max-content';
    const natural = panel.offsetWidth;
    const minWidth = panel.dataset.kind === 'sensitive' ? MIN_PANEL_WIDTH_SENSITIVE : MIN_PANEL_WIDTH;
    const cap = Math.min(Math.max(minWidth, natural), avail, MAX_PANEL_WIDTH);
    panel.style.maxWidth = `${cap}px`;
    panel.style.width = 'auto';

    const width = panel.offsetWidth;
    const height = panel.offsetHeight;
    const below = window.innerHeight - rect.bottom - GAP - EDGE;
    const above = rect.top - GAP - EDGE;
    const flip = below < height && above > below;

    const centerX = rect.left + rect.width / 2;
    const left = Math.max(EDGE, Math.min(centerX - width / 2, window.innerWidth - EDGE - width));
    // 夹进视口后仍放不下（面板比可用高度还高）时贴顶显示，超出部分由
    // CSS 的 max-height + overflow 接管（见 page-requests.css）
    const top = flip ? rect.top - GAP - height : rect.bottom + GAP;

    panel.style.left = `${Math.round(left)}px`;
    panel.style.top = `${Math.round(Math.max(EDGE, Math.min(top, window.innerHeight - EDGE - height)))}px`;
    panel.classList.toggle('is-top', flip);
  }

  /** 展开锚点的面板。`html` 由调用方（本模块自己的两个内容构造函数）给出 */
  function open(el, html) {
    if (!html) return;
    // 锚点必须还在文档里：整表重绘（本页默认 1 秒一拍自动刷新）会把标签换成
    // 新节点，而 150ms 延迟窗口里排下的这次打开拿到的可能正是被删掉的旧节点
    // —— 游离节点的 rect 全 0，place() 会把面板摆到视口左上角，且它收不到
    // pointerout、开了就不会自己关（实测的 bug 现象）。作废这次打开即可：
    // 指针还停在标签上的话，浏览器补发的 pointerover 会重新排一次。
    if (!el.isConnected) return;
    cancelTimers();
    if (anchor && anchor !== el) close();
    anchor = el;
    // 面板类型（chain / sensitive）交给 place()：两类内容的宽度下限不同，
    // 见 MIN_PANEL_WIDTH 的说明。取自标签自己的 data-req-hover 属性
    // （requests-panel.js 渲染时写上的，与内容构造函数的选择同一个来源）。
    panel.dataset.kind = el.dataset.reqHover || '';
    panel.innerHTML = html;
    el.classList.add('active');
    anchor.setAttribute('aria-describedby', panel.id);
    // 必须先让它参与布局（display:none 时量出来全是 0）再摆位 ——
    // 两步在同一任务内完成，不会闪出「先落在上一处」的一帧（tooltip.js 同法）
    panel.classList.add('open');
    place();
  }

  function close() {
    cancelTimers();
    if (!anchor) return;
    anchor.classList.remove('active');
    anchor.removeAttribute('aria-describedby');
    anchor = null;
    panel.classList.remove('open');
  }

  // ─── 整表重绘时的锚点迁移 ──────────────────────────────────

  /**
   * 列表整表重绘**前**调用（宿主在替换 `innerHTML` 之前）：定下锚点的处置意图。
   *
   * 只有「指针正悬着」的锚点才迁移（鼠标用户在看面板，面板该跟着新一屏走），
   * 身份用「标签种类 + 行身份键」（`data-req-hover` 与 `data-req-id`）记下 ——
   * 用属性而不是节点引用：整表重绘必然换节点，引用一定会失效。其余情况一律
   * 记为收起，理由各是一条独立的边界：
   *   · `hideTimer` 排着 = 指针已经移开、正在等 HIDE_DELAY 收起 —— 迁移过去
   *     之后那个定时器的判据（`anchor === tag`）已不成立，会变成「该关没关」；
   *   · 指针不在而焦点在标签上 = 键盘打开 —— 重绘会把焦点元素删掉、焦点回落
   *     到 body，面板跟过去就成了「焦点不在标签上、面板却开着」的错位；
   *   · 身份键缺失 = 这枚标签不是按常规渲染出来的，无从找回。
   */
  function beforeListRedraw() {
    if (!anchor) return;
    const kind = String(anchor.dataset?.reqHover || '');
    const key = String(anchor.dataset?.reqId || '');
    const movable = Boolean(kind && key) && anchor.matches(':hover') && !hideTimer;
    remap = movable ? { action: 'move', kind, key } : { action: 'close' };
  }

  /**
   * 列表整表重绘**后**调用（宿主在替换 `innerHTML` 之后）：按处置意图收尾。
   *
   * 迁移：锚点换人、内容重算、位置重摆 —— 面板跟着新一屏走，用户看不到任何
   * 闪动。找不回新节点（行被挤出当前页 / 列表变空 / 出错态 / 反查不到数据）
   * 或意图本就是收起：close()。
   *
   * ── 为什么不是「重绘即收起」─────────────────────────────────
   * 本页默认 1 秒重绘一次，正盯着面板看的人会被每秒关一次、还要动一下鼠标
   * 才重开；迁移把这件事对用户完全隐去。顺带这也是「游离锚点」这个状态的
   * 彻底解法 —— 迁不走的（找不到新节点的）一律收起，不会留下死锚点。
   */
  function afterListRedraw() {
    const pending = remap;
    remap = null;
    if (!pending || !anchor) return;
    if (pending.action === 'close') {
      close();
      return;
    }
    const next = findTag(hostEl, pending.kind, pending.key);
    // 内容一并重算而不是沿用旧 HTML：进行中的行每秒都在变（阶段、已用时），
    // 旧 HTML 会显示上一拍的事实；entryOf 反查的本来就是当前这一屏
    const html = next ? htmlFor(next, entryOfFn) : '';
    if (!next || !html) {
      close();
      return;
    }
    if (next === anchor) return;   // 整表重绘必然换节点；这条兜住宿主改成局部更新后误伤
    anchor.classList.remove('active');
    anchor.removeAttribute('aria-describedby');
    anchor = next;
    next.classList.add('active');
    next.setAttribute('aria-describedby', panel.id);
    panel.innerHTML = html;
    place();
  }

  /** 在新 DOM 里按「标签种类 + 行身份键」找回锚点；没有就是这一行不在本屏了 */
  function findTag(host, kind, key) {
    // 用遍历而不是拼属性选择器：身份键来自后端（id 或时间戳），不必让它参与
    // 选择器解析 —— 转义漏一个字符就是一个静默查不到。一行最多两枚标签，
    // 50 行的遍历对每 1 秒一次的重绘完全无感。
    for (const el of host.querySelectorAll('[data-req-hover]')) {
      if (el.dataset.reqHover === kind && el.dataset.reqId === key) return el;
    }
    return null;
  }

  /**
   * 标签 → 面板内容。数据由宿主通过 `entryOf` 反查（见模块头「与
   * requests-panel.js 的分工」）。
   */
  function htmlFor(el, entryOf) {
    const entry = entryOf?.(el);
    if (!entry) return '';
    return el.dataset.reqHover === 'sensitive'
      ? sensitivePanelHtml(entry)
      : chainPanelHtml(entry);
  }

  /**
   * 绑定宿主列表：一次委托覆盖所有行，整表重绘后监听仍然有效
   * （与 accounts-view.js 的事件委托同一手法）。
   *
   * `entryOf(el)` 由宿主提供：收一个标签元素，返回它所属的那条请求日志。
   */
  function bind({ host, entryOf } = {}) {
    if (!host) return;
    // 记下宿主与反查函数：整表重绘后按身份找回锚点要用（见 afterListRedraw）
    hostEl = host;
    entryOfFn = entryOf;

    host.addEventListener('pointerover', event => {
      const tag = event.target.closest?.('[data-req-hover]');
      if (!tag || !host.contains(tag)) return;
      cancelTimers();
      if (anchor === tag && panel.classList.contains('open')) return;
      // 已经开着别的面板时立即切换（用户在连着看），否则等满延迟防误触
      const delay = anchor ? 0 : SHOW_DELAY;
      if (anchor) close();
      pendingTag = tag;
      showTimer = setTimeout(() => {
        showTimer = 0;
        pendingTag = null;
        open(tag, htmlFor(tag, entryOf));
      }, delay);
    });

    host.addEventListener('pointerout', event => {
      const tag = event.target.closest?.('[data-req-hover]');
      if (!tag) return;
      // ① 还在延迟窗口里（面板没开、anchor 为 null）：取消排着的那次打开 ——
      //    「悬一下就走」不该弹出。列表重绘把标签删掉时浏览器补发的 pointerout
      //    也走这里：不取消的话，定时器到点会拿游离节点去 open（见 open 的判据）
      if (pendingTag === tag) {
        clearTimeout(showTimer);
        showTimer = 0;
        pendingTag = null;
        return;
      }
      // ② 面板开着且锚的就是它：按 HIDE_DELAY 收起
      if (anchor !== tag) return;
      clearTimeout(showTimer);
      showTimer = 0;
      hideTimer = setTimeout(() => {
        hideTimer = 0;
        if (anchor === tag) close();
      }, HIDE_DELAY);
    });

    // 键盘可达：标签是 button（见 requests-panel.js 的 retryCell），
    // 聚焦即展开、失焦即收起
    host.addEventListener('focusin', event => {
      const tag = event.target.closest?.('[data-req-hover]');
      if (tag) open(tag, htmlFor(tag, entryOf));
    });
    host.addEventListener('focusout', event => {
      const tag = event.target.closest?.('[data-req-hover]');
      if (tag && anchor === tag) close();
    });

    // Esc 收起（不拦冒泡：弹窗自己也要用 Esc）
    host.addEventListener('keydown', event => {
      if (event.key !== 'Escape' || !anchor) return;
      close();
    });
  }

  function bindGlobal() {
    document.body.appendChild(panel);
    panel.id = 'req-hover-panel';

    // 点标签之外的任何地方收起（捕获阶段：先于业务自己的 click，
    // 保证「关面板」不影响页面本来的点击行为）
    document.addEventListener('click', event => {
      if (!anchor) return;
      if (event.target === anchor || anchor.contains(event.target)) return;
      close();
    }, true);

    /**
     * 滚动 / 缩放时的处理与 tooltip.js 相反：这里**收起**而不是重新定位。
     *
     * 理由是这个面板挂在**列表内的行**上，而列表自身可滚（`.log-list`）。
     * 滚动时行在动、面板是 fixed 的（不动），重新定位要跟随每一帧滚动事件，
     * 而名单里 50 行滚动时能滑很快 —— 追随会让面板在屏幕上拖着走，反而干扰
     * 正在滚动的手。收起之后指针还停在标签上，轻微一动就会重新弹出。
     */
    window.addEventListener('scroll', () => close(), true);
    window.addEventListener('resize', () => close());
  }

  bindGlobal();

  window.wbRequestHover = {
    // 只导出宿主需要的入口；内容构造函数保持私有（宿主要显示什么，
    // 由标签上的 data-req-hover 决定，不必也不该由它拼 HTML）。
    //
    // `hasProcessFacts` 是唯一一个**给宿主用的判断**：那枚「重试」标签显不显示
    // 由它回答 —— 判据（换过号 **或** 重试过 **或** 有提示）需要读明细内部
    // 的字段，让宿主自己拼一遍就会有两份判据（见它的说明）。
    //
    // `beforeListRedraw` / `afterListRedraw` 是给宿主的**重绘通知**：整表重绘
    // 会换掉锚点节点，必须通知本模块迁移，否则面板会拿游离节点定位
    // （见那两个函数的说明）。
    bind,
    close,
    hasProcessFacts,
    beforeListRedraw,
    afterListRedraw,
  };
})();
