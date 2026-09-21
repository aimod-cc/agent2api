/* Agent2API · 请求日志的富文本悬停面板（重试链 / 敏感词命中） */
/* global wbApp */

/**
 * 「请求日志」页里那两枚标签的**悬停面板**：重试链与敏感词命中。
 *
 * ── 为什么单独成文件 ──────────────────────────────────────────
 * 与 tooltip.js / select.js 同一类东西：一块自洽的浮层交互（定位、翻转、
 * 事件、生命周期）。但**不复用 tooltip.js**，因为它的契约是纯文本 ——
 * `bubble.textContent = text`，内容来自 `data-tip` 属性。这里要弹的是一份
 * 结构化的富文本（切换路径一串箭头、每次尝试一行、故障行红字、成功行绿字、
 * 敏感词是「词 × 次数」的列表），塞进属性会被 HTML 转义成一堆源码，
 * 用户看到的是标签而不是面板。
 *
 * 另一条路是给 tooltip.js 加一个「HTML 内容」开关，但那会让一个被全站几十处
 * `data-tip` 依赖的公共组件多出一条**安全上更敏感**的路径（`innerHTML`）——
 * 现有的纯文本契约正是它简单可靠的原因。所以这里另起一个只服务请求日志表的
 * 浮层，与 OmniProxy 的 `FailoverTip` / `SensitiveMaskedTag` 一一对应。
 *
 * ── 与 requests-panel.js 的分工 ────────────────────────────────
 *   · 本文件      内容 HTML 的构造（`chainHtml` / `sensitiveHtml`）+ 浮层机制
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

  function cancelTimers() {
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    showTimer = 0;
    hideTimer = 0;
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

  /**
   * 切换路径行：`A → B → C`。
   *
   * **只在真实发生过 ≥2 次尝试时显示**（与 OmniProxy 同判据）：只有一次尝试时
   * 那串箭头就是「A → 成功」，没有信息量；而这一列的标签本来就只在
   * `attempts > 1` 时出现，所以这条判据主要是防「明细比 attempts 短」的边界
   * （截断、或部分尝试没采到）。
   */
  function chainHtml(details) {
    if (details.length < 2) return '';
    const names = details.map(item => providerLabel(item.provider) || '未知');
    return `<div class="rh-chain"><span class="rh-chain-k">切换路径：</span>`
      + `<span class="rh-chain-v">${esc(names.join(' → '))}</span></div>`;
  }

  /**
   * 单次尝试的一行：`尝试 N · 提供商 → 成功(200) / 失败(500)：错误摘要`。
   *
   * 三种结局的判据与颜色：
   *   · `error` 非空         → 失败（红），带状态码（可能没有：传输层失败）
   *   · `status` 有值、无错误 → 成功（绿），带状态码
   *   · 两者都无             → 未定论（淡灰）。只在「这一轮还在飞」时出现，
   *     而列表里看到的行都是已收尾的，所以它几乎只会来自被手工改过的库；
   *     给一个中性文案而不是猜成功/失败。
   */
  function attemptRowHtml(item, index) {
    const name = providerLabel(item?.provider);
    const status = Number(item?.status);
    const hasStatus = item?.status !== null && item?.status !== undefined && Number.isFinite(status);
    const error = item?.error ? String(item.error) : '';
    // 序号从 1 起（后端明细数组本身就是发生顺序，不需要另存 attempt_no）
    const head = `<span class="rh-no">尝试 ${index + 1} ·</span>`
      + `<span class="rh-who">${esc(name || '未知')}</span>`
      + '<span class="rh-arrow">→</span>';
    if (error) {
      return `<div class="rh-row">${head}<span class="rh-bad">失败${
        hasStatus ? `（${esc(String(status))}）` : ''}：${esc(error)}</span></div>`;
    }
    if (hasStatus) {
      return `<div class="rh-row">${head}<span class="rh-ok">成功（${esc(String(status))}）</span></div>`;
    }
    // 有状态码之外的最后一种：状态码缺失且没有错误摘要（见上面「未定论」）
    return `<div class="rh-row">${head}<span class="rh-dim">无结果记录</span></div>`;
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
    return chainHtml(details)
      + details.map(attemptRowHtml).join('')
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
    // 错误摘要更长。用 max-content 量出理想宽度，上限夹到视口可用宽度，
    // 放不下的部分交给面板内部的换行与滚动。
    panel.style.maxWidth = 'none';
    panel.style.width = 'max-content';
    const natural = panel.offsetWidth;
    const cap = Math.min(Math.max(220, natural), avail);
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
    cancelTimers();
    if (anchor && anchor !== el) close();
    anchor = el;
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

    host.addEventListener('pointerover', event => {
      const tag = event.target.closest?.('[data-req-hover]');
      if (!tag || !host.contains(tag)) return;
      cancelTimers();
      if (anchor === tag && panel.classList.contains('open')) return;
      // 已经开着别的面板时立即切换（用户在连着看），否则等满延迟防误触
      const delay = anchor ? 0 : SHOW_DELAY;
      if (anchor) close();
      showTimer = setTimeout(() => {
        showTimer = 0;
        open(tag, htmlFor(tag, entryOf));
      }, delay);
    });

    host.addEventListener('pointerout', event => {
      const tag = event.target.closest?.('[data-req-hover]');
      if (!tag || anchor !== tag) return;
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
    // 只导出宿主需要的那两个入口；内容构造函数保持私有（宿主要显示什么，
    // 由标签上的 data-req-hover 决定，不必也不该由它拼 HTML）
    bind,
    close,
  };
})();
