/* Agent2API · 问号提示增强（[data-tip] → 小问号气泡） */

/**
 * 把页面上带 data-tip 的元素就地增强成「小问号 + 气泡说明」。
 *
 * 自动增强（与 select.js 同款思路）：页面上只写
 *   <span class="tip-q" data-tip="说明全文"></span>
 * 触发方式、定位、动画与无障碍全部由本模块负责，业务 JS 零感知 ——
 * 后续新增设置项照抄这个写法即可，不需要任何 init 调用。
 *
 * 交互取舍：
 *   · 悬停 150ms 后显示、移开 80ms 后收起：问号多排在标题右侧，
 *     鼠标横穿标题行时不该闪出一排气泡；收起留一点宽限，手抖不至于把气泡弄没。
 *   · 点击切换：触摸设备没有悬停，桌面端也有用户习惯点开读长说明。
 *     点开的（pinned）不因鼠标移开而消失，直到再次点击 / 点页面其它位置 / Esc。
 *   · 键盘可达：增强时补 tabindex，focus 显示、blur 收起。
 *
 * 定位用 fixed 且常驻挂在 body 上 —— 与 select.js 踩的是同一个坑：
 * .panel 是 overflow:hidden，absolute 气泡会被卡片直接裁掉。
 * 气泡带 pointer-events:none，纯展示、不抢指针事件。
 */
(() => {
  /** 悬停进入 / 离开的延迟（ms）：进入慢一点防误触，离开快一点跟着走 */
  const SHOW_DELAY = 150;
  const HIDE_DELAY = 80;
  /** 距视口边缘的安全距离，以及气泡与锚点的间距（箭头就落在这段间隙里） */
  const EDGE = 8;
  const GAP = 8;
  /** 气泡宽度：默认收在 18rem（288px）内换行；内容里有长串（URL 之类）撑不开时放宽到 20rem */
  const MAX_W = 288;
  const MAX_W_HARD = 320;
  const BUBBLE_ID = 'wb-tip-bubble';

  /** 触摸设备没有悬停：只保留点击切换。判断一次即可，不必每次事件都查 */
  const CAN_HOVER = window.matchMedia('(hover: hover)').matches;

  /**
   * 内核是否支持 :focus-visible（用来区分键盘聚焦与鼠标点出的聚焦）。
   * 桌面端是 WebView2，支持是常态；这里仍做一次探测，
   * 万一不支持就退回「一律按键盘聚焦处理」，不至于因为选择器报错而整个组件失效。
   */
  const HAS_FOCUS_VISIBLE = (() => {
    try {
      document.createDocumentFragment().querySelector(':focus-visible');
      return true;
    } catch {
      return false;
    }
  })();

  /** 已增强的触发元素（避免重复绑定） */
  const registry = new WeakSet();

  // 气泡全局只有一个：同屏同时只允许显示一条说明，复用节点也让进出动画连得上
  const bubble = document.createElement('div');
  bubble.className = 'tip-bubble';
  bubble.id = BUBBLE_ID;
  bubble.setAttribute('role', 'tooltip');

  let anchor = null;      // 当前挂着的触发元素
  let pinned = false;     // 由点击（而不是悬停 / 焦点）打开，鼠标移开也不收
  let showTimer = 0;
  let hideTimer = 0;

  function cancelTimers() {
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    showTimer = 0;
    hideTimer = 0;
  }

  /**
   * 按锚点位置摆气泡：默认在下方，下方放不下且上方更宽裕时翻到上方。
   * 锚点多在标题行（页面上半部），所以「优先向下」是稳妥的默认。
   */
  function place() {
    if (!anchor) return;
    const rect = anchor.getBoundingClientRect();
    const avail = window.innerWidth - EDGE * 2;

    // 量宽度用两层内在尺寸，而不是 scrollWidth：
    //  · max-content = 整段文字排成一行需要多宽（自然宽度）
    //  · min-content = 最长的那个「不可断开的片段」有多宽（长 URL 就是它）
    // 两者一夹就得到理想宽度：正常句子自然宽度远小于 288px，直接一行放下；
    // 长说明自然宽度超 288px，按 288px 换行；只有不可断片段本身超过 288px
    // （等宽 URL 之类）才放宽到 320px，再超就允许硬断词。
    // 元素此刻已 display:block，量到的都是本次文案的真实尺寸（同任务内两次量值，
    // 只触发一次重排，不会闪）。
    bubble.style.overflowWrap = '';
    bubble.style.maxWidth = 'none';
    bubble.style.width = 'max-content';
    const natural = bubble.offsetWidth;
    bubble.style.width = 'min-content';
    const longest = bubble.offsetWidth;

    // 上限夹进「视口可用宽度」：窗口很窄时优先不溢出，宽不宽得下让给换行去解决
    const cap = Math.min(Math.max(MAX_W, Math.min(longest, MAX_W_HARD)), avail);
    bubble.style.maxWidth = `${cap}px`;
    bubble.style.width = 'auto';
    // 放宽到 320px 仍装不下最长片段：允许在任意位置断词，保证不横向溢出。
    // 放在设完 max-width 之后判断，避免影响上面两次内在尺寸的测量。
    if (natural > cap && longest > cap) bubble.style.overflowWrap = 'anywhere';

    const width = bubble.offsetWidth;
    const height = bubble.offsetHeight;

    const below = window.innerHeight - rect.bottom - GAP - EDGE;
    const above = rect.top - GAP - EDGE;
    const flip = below < height && above > below;

    const centerX = rect.left + rect.width / 2;
    const left = Math.max(EDGE, Math.min(centerX - width / 2, window.innerWidth - EDGE - width));
    const top = flip ? rect.top - GAP - height : rect.bottom + GAP;

    bubble.style.left = `${Math.round(left)}px`;
    bubble.style.top = `${Math.round(Math.max(EDGE, Math.min(top, window.innerHeight - EDGE - height)))}px`;
    // 箭头跟着锚点偏移而不是永远居中：气泡被视口夹紧时，箭头仍指着问号。
    // 两侧各留 10px，箭头不会跑到圆角外面去。
    const arrowX = Math.max(10, Math.min(centerX - left, width - 10));
    bubble.style.setProperty('--tip-arrow-x', `${Math.round(arrowX)}px`);
    // 进场缩放的原点指向锚点，观感上是「从问号那里长出来」；
    // 位移方向同理：从贴近锚点的那一侧滑出来
    bubble.style.setProperty('--tip-origin', `${Math.round(arrowX)}px ${flip ? '100%' : '0%'}`);
    bubble.style.setProperty('--tip-shift', flip ? '4px' : '-4px');
    bubble.classList.toggle('is-top', flip);
  }

  /** 展开锚点的气泡；同一个锚点重复调用只重新定位 */
  function open(el) {
    const text = (el.dataset.tip || '').trim();
    if (!text) return;
    cancelTimers();
    if (anchor && anchor !== el) close();
    anchor = el;
    bubble.textContent = text;
    el.classList.add('active');
    // aria-describedby 只在展开期间挂：气泡是全局复用的一个节点，
    // 常驻引用会把六条说明同时读给读屏软件
    el.setAttribute('aria-describedby', BUBBLE_ID);
    // 必须先让气泡参与布局（display:none 的元素量出来全是 0），再摆位 ——
    // 两步都在同一个任务里完成，浏览器直接按最终位置绘制，
    // 不会出现「先闪在上一处、再滑过来」的一帧。
    bubble.classList.add('open');
    place();
  }

  function close() {
    cancelTimers();
    if (!anchor) return;
    anchor.classList.remove('active');
    anchor.removeAttribute('aria-describedby');
    anchor = null;
    pinned = false;
    bubble.classList.remove('open');
  }

  /** 悬停进入：已经开着别的气泡时立即切换（用户显然在连着看说明），否则等满延迟 */
  function scheduleShow(el) {
    cancelTimers();
    if (anchor === el && bubble.classList.contains('open')) return;
    const delay = anchor ? 0 : SHOW_DELAY;
    if (anchor) close();
    showTimer = setTimeout(() => { showTimer = 0; open(el); }, delay);
  }

  function scheduleHide(el) {
    clearTimeout(showTimer);
    showTimer = 0;
    // 点开的气泡是「读长说明」用的，鼠标移开不该把它弄没
    if (pinned) return;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      hideTimer = 0;
      if (anchor === el && !pinned) close();
    }, HIDE_DELAY);
  }

  function bind(el) {
    el.addEventListener('pointerenter', () => { if (CAN_HOVER) scheduleShow(el); });
    el.addEventListener('pointerleave', () => { if (CAN_HOVER) scheduleHide(el); });

    el.addEventListener('focus', () => {
      // 鼠标 / 触摸点出来的聚焦不在这里展开：那一下紧接着会走 click 分支，
      // 两处都开会让「点一次」变成开-关-开。:focus-visible 正是
      // 「这次聚焦来自键盘」的语义，用它把指针路径让给 click。
      if (HAS_FOCUS_VISIBLE && !el.matches(':focus-visible')) return;
      cancelTimers();
      open(el);
    });

    // 焦点离开就收起：点开的常驻气泡也一样 —— 用户已经转移到别的控件上了
    el.addEventListener('blur', () => { if (anchor === el) close(); });

    el.addEventListener('click', () => {
      // 已经是「点开」状态就取消；否则切换过来（open 内部会先收掉上一个气泡，
      // 而收气泡会复位 pinned，所以 pinned 必须等 open 之后再立）
      if (anchor === el && pinned) { close(); return; }
      open(el);
      pinned = true;
    });

    el.addEventListener('keydown', event => {
      if (event.key !== 'Escape' || anchor !== el) return;
      close();
      // 只掐掉默认行为（部分内核 Esc 会取消激活），不拦冒泡：
      // 弹窗自己也要用 Esc 关闭，这里吞掉会让用户以为卡住了
      event.preventDefault();
    });
  }

  function enhance(el) {
    if (registry.has(el)) return;
    if (!(el.dataset.tip || '').trim()) return;
    registry.add(el);
    // 小问号的「?」由组件补上，页面里只写空的 .tip-q 即可（与「业务 JS 零感知」一致）。
    // 用真实文本而不是 CSS content：读屏软件能把「?」连同 tooltip 语义一起念出来，
    // 字号与居中则由 css/tooltip.css 控制。
    if (el.classList.contains('tip-q') && !el.textContent.trim()) el.textContent = '?';
    // 键盘可达：本身能聚焦的元素（按钮 / 输入框 / 已带 tabindex 的）不重复补
    if (!el.matches('a[href], button, input, select, textarea, [tabindex]')) el.tabIndex = 0;
    bind(el);

    // 触发元素被量成 0 尺寸时说明它（连同外层容器）被藏起来了 —— 最典型的是
    // 设置页切分类：非当前分类的 pane 是 display:none。普通点击路径上那次
    // 点击已经顺手把气泡关了，但程序化切换（不派发点击）没有这个信号，
    // 气泡会变成浮在新分类上方的孤儿。用 ResizeObserver 兜住：
    // 开销极小，且只在真的隐藏时才动作（select.js 对同样的坑用同一招）。
    if (typeof ResizeObserver === 'function') {
      const sizeWatcher = new ResizeObserver(entries => {
        if (anchor !== el) return;
        const box = entries[0]?.contentRect;
        if (box && !box.width && !box.height) close();
      });
      sizeWatcher.observe(el);
    }
  }

  function bindGlobal() {
    // 点气泡与问号之外的地方收起。用捕获阶段，先于业务自己的 click 处理，
    // 保证「关提示」不会影响页面本来的点击行为。
    document.addEventListener('click', event => {
      if (!anchor) return;
      if (event.target === anchor || anchor.contains(event.target)) return;
      close();
    }, true);

    // 气泡是 fixed 定位、不跟随滚动。与下拉浮层不同，这里不关闭而是重新定位：
    // 读说明时滚动内容不该把气泡弄没；锚点被滚出视口才收起。
    const follow = () => {
      if (!anchor) return;
      if (!anchor.isConnected) { close(); return; }
      const rect = anchor.getBoundingClientRect();
      if (rect.width && rect.bottom > 0 && rect.top < window.innerHeight) place();
      else close();
    };
    window.addEventListener('scroll', follow, true);
    window.addEventListener('resize', follow);
  }

  /** 扫描一棵子树（含自身）里的 [data-tip] 并增强；批量插入时只调用一次 */
  function scan(root) {
    if (root.nodeType !== 1) return;
    if (root.matches?.('[data-tip]')) enhance(root);
    root.querySelectorAll?.('[data-tip]').forEach(enhance);
  }

  function boot() {
    document.body.appendChild(bubble);
    scan(document.body);
    bindGlobal();
    // 动态插入的 [data-tip]（弹窗、重新渲染的面板）由这个 observer 接住。
    // 与 select.js 一样按微任务合并：一次 innerHTML 里的多个节点只扫一遍。
    let queued = false;
    const pending = [];
    const observer = new MutationObserver(records => {
      // 锚点被整体移除时气泡得跟着收：它挂在 body 上，不会随宿主消失
      if (anchor && !anchor.isConnected) close();
      for (const record of records) {
        for (const node of record.addedNodes) {
          if (node.nodeType === 1) pending.push(node);
        }
      }
      if (!pending.length || queued) return;
      queued = true;
      queueMicrotask(() => {
        queued = false;
        const batch = pending.splice(0, pending.length);
        for (const node of batch) scan(node);
      });
    });
    observer.observe(document.body, { childList: true, subtree: true });

    // 对外只暴露排查用的开关，业务不需要（也不应该）手动调用
    window.wbTooltip = { open, close, enhanced: el => registry.has(el) };
  }

  // 与 select.js 同理：排在各业务模块之前时 DOM 可能还没解析完，按下 readyState 决定何时启动
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot, { once: true });
  else boot();
})();
