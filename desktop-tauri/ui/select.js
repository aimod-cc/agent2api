/* Agent2API · 下拉控件增强（原生 select → 触发器 + 浮层） */

/**
 * 把页面上的原生 <select> 就地增强成「触发器 + 浮层选项列表」的现代下拉。
 *
 * 为什么用「增强」而不是「替换」：
 * 业务代码（accounts-view.js / proxy-form.js / logs-panel.js）直接读写 select.value、
 * 监听 select 的 change 事件。要保证这些逻辑一行不用改，原生 select 必须留在 DOM 里
 * 继续当唯一数据源 —— 组件只负责给它套一层可视外壳，值、事件全部原样透传。
 *
 * 自动化策略（页面没有任何一处需要手动调用 init）：
 *   · body 级 MutationObserver 发现新插入的 select（含弹窗里动态渲染的代理表单）
 *   · 每个已增强 select 上再挂一个 MutationObserver，盯住 option 增删与 selected 变化
 *     （模型筛选会整体重建 innerHTML、日志分类会 appendChild）
 *   · 业务代码直接写 select.value 时两个 observer 都收不到信号（改 value 不动属性节点），
 *     所以给实例装了一层 value / selectedIndex 访问器截获赋值，打开浮层前还会按内容
 *     指纹再比对一次兜底；body 级 observer 用微任务合并同一次渲染里的多次变更。
 *
 * 键盘 / 无障碍：触发器是 button + aria-haspopup="listbox"，浮层 role="listbox"，
 * 选项 role="option" 并维护 aria-selected；↑↓ 只移动视觉高亮（不立即改值），
 * Enter / Space 才确认，与原生 select 的手感一致。
 */
(() => {
  /** 已增强的 select → 实例上下文（避免重复增强，也方便 observer 反查） */
  const registry = new WeakMap();
  /** 当前打开着浮层的实例：同屏只允许一个，切换时先关旧的 */
  let openInstance = null;
  /** 实例自增序号：给浮层生成唯一 id，供 aria-controls 引用 */
  let seq = 0;

  /** 触发器右侧的下箭头；与 icons.js 同款 24 画布 + currentColor 填充 */
  const CARET = '<svg viewBox="0 0 24 24" width="15" height="15" fill="currentColor" aria-hidden="true" focusable="false">'
    + '<path d="M12 15.4 5.6 9l1.4-1.4 5 5 5-5L18.4 9 12 15.4Z"/></svg>';

  /** 选中项右侧的对勾；尺寸比箭头小一号，避免在选项行里抢视线 */
  const CHECK = '<svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor" aria-hidden="true" focusable="false">'
    + '<path d="M9.6 17.2 4.4 12l1.4-1.4 3.8 3.8 8.6-8.6L19.6 7l-10 10.2Z"/></svg>';

  /** 浮层 id 生成器：给 role="listbox" 一个唯一 id，供触发器的 aria-controls 指向 */
  const uid = () => `wbsel-${++seq}`;

  /**
   * 取 select 的当前显示文本：优先 selectedOptions，保证和原生观感一字不差。
   *
   * 多选（`<select multiple>`）没有「当前项」这个概念，把已勾选项的文案用顿号连起来；
   * 一个都没勾时退回 `data-placeholder`（设置页的「作用提供商」用它说明空选的含义）。
   * 不直接写「未选择」这种硬编码文案：占位语是要与业务语义对齐的，交给页面声明。
   */
  function selectedText(select) {
    if (select.multiple) {
      const labels = [...select.selectedOptions]
        .map(option => option.textContent.trim())
        .filter(Boolean);
      return labels.length ? labels.join('、') : (select.dataset.placeholder || '');
    }
    const option = select.selectedOptions?.[0] || select.options[select.selectedIndex];
    if (!option) return '';
    return option.textContent.trim();
  }

  /** 取 select 的当前选中值（无选中项时按原生语义回落到空串） */
  function selectedValue(select) {
    return select.value ?? '';
  }

  /** 当前已勾选的 value 集合：单选回落到「只有一个成员的集合」，两处判定共用一份逻辑 */
  function selectedSet(select) {
    if (!select.multiple) return new Set([selectedValue(select)]);
    return new Set([...select.selectedOptions].map(option => option.value ?? ''));
  }

  /**
   * 打开浮层时按触发器位置摆好浮层。
   *
   * 定位用 fixed，并且打开期间把浮层挂到 body 上（关闭时再放回 shell 内）。
   * 为什么不按最省事的「absolute 挂在 shell 里」：
   *   · shell 所在的容器几乎都带裁剪 —— .panel 是 overflow:hidden（日志筛选、
   *     账号工具条），.modal 是 overflow:hidden + .modal-body 是 overflow-y:auto
   *     （账号设置 / 批量操作弹窗里的出网代理表单）。absolute 会让浮层被这些容器
   *     直接裁掉一截，日志页和弹窗里的下拉等于用不了。
   *   · 改 fixed 也还不够：.modal-mask 带 backdrop-filter，它会给 fixed 子孙
   *     重新指定包含块，导致中间那些 overflow 祖先照样裁剪。挂到 body 上就彻底
   *     与裁剪无关了。
   * 代价只是「不跟随滚动」，所以滚动 / 缩放时直接关闭（见 bindGlobal）—— 这与
   * 「挂在 shell 内、不跟随滚动」的取舍一致，只是多绕开了裁剪。
   */
  function place(ctx) {
    const { popover, trigger } = ctx;
    const rect = trigger.getBoundingClientRect();

    const GAP = 6;                  // 触发器与浮层之间的缝
    const EDGE = 8;                 // 距视口边缘的安全距离
    const below = window.innerHeight - rect.bottom - GAP - EDGE;
    const above = rect.top - GAP - EDGE;
    // 下方放不下整块浮层、而上方更宽裕时向上翻
    const flip = below < 180 && above > below;

    // 宽度：先按 max-content 量出「内容原本需要多宽」，再夹到
    // [触发器宽度, 320px] 之间，并且不超出视口。
    // 用 max-content 而不是 scrollWidth 量：此时元素上还残留着上一次打开的
    // left/top，宽度会被那组约束裁剪，量出来的值不可信。
    popover.style.width = 'max-content';
    const natural = popover.offsetWidth;
    const width = Math.min(Math.max(rect.width, Math.min(natural, 320)), window.innerWidth - EDGE * 2);
    popover.style.width = `${width}px`;
    popover.style.maxHeight = `${Math.max(140, Math.min(260, flip ? above : below))}px`;
    // 向上翻时用浮层实际（已被 max-height 收过的）高度回推 top，
    // 所以这一行必须排在设完 max-height 之后
    popover.style.top = flip ? `${rect.top - GAP - popover.offsetHeight}px` : `${rect.bottom + GAP}px`;

    // 横向夹回视口内：靠右边缘的下拉（日志筛选那一排）浮层不能溢出窗口
    const maxLeft = Math.max(EDGE, window.innerWidth - EDGE - width);
    popover.style.left = `${Math.max(EDGE, Math.min(rect.left, maxLeft))}px`;
  }

  /**
   * 同步「当前高亮项」的可见位置：打开浮层时选中项可能落在滚动区外，
   * 键盘上下移动时高亮项也会跑出视野，这里把浮层滚到能露出它。
   *
   * 手算而不是用 scrollIntoView：后者会把祖链上的滚动容器（弹窗、页面）一起带动，
   * 而浮层是 fixed 定位、只在打开那一刻量过一次坐标，祖先一滚就和触发器错位了。
   *
   * 用 getBoundingClientRect 而不是 offsetTop 做基准：浮层打开期间挂在 body 上，
   * offsetParent 会随之变化（fixed 元素在无定位祖先下 offsetTop 是相对视口的），
   * 两套坐标混用会算出错误的滚动量；视口坐标两边都成立。
   */
  function scrollHighlightIntoView(popover, option) {
    if (!option || !popover.clientHeight) return;
    const box = popover.getBoundingClientRect();
    // 浮层自身可能有边框，可见区从边框内侧算起
    const viewTop = box.top + popover.clientTop;
    const viewBottom = viewTop + popover.clientHeight;
    const item = option.getBoundingClientRect();
    if (item.top < viewTop) popover.scrollTop -= viewTop - item.top;
    else if (item.bottom > viewBottom) popover.scrollTop += item.bottom - viewBottom;
  }

  function enhance(select) {
    if (registry.has(select)) return;
    // `size>1` 的列表选择框（列表框）不适用这套「触发器 + 浮层」外壳，直接跳过；
    // 多选（`multiple`）走另一条路 —— 浮层形态一样，但点选项是切换勾选（见 toggle）。
    // 判定顺序有讲究：`<select multiple>` 不写 size 时，size **属性**的默认值是 4，
    // 而 DOM 的 `size` 访问器出于兼容返回 0（MDN 明确记了这条差异），
    // 所以必须先判 multiple，否则一个显式写了 `size="4"` 的多选会被 size 这条误伤。
    if (!select.multiple && Number(select.size) > 1) return;
    // 已经被别处包过壳的跳过，避免嵌套
    if (select.parentElement?.classList.contains('select-shell')) return;

    const shell = document.createElement('div');
    shell.className = 'select-shell';
    // 先把外壳插到 select 原来的位置，再把 select 搬进外壳：外壳原地顶替 select
    // 在父容器里的那一个位置。父容器多是 flex 行（工具条、日志筛选行、代理表单行），
    // 顺序错了或包成两层都会让整行重排。
    // 原生 select 留在外壳里当「宽度与高度的锚」（见 select.css），
    // 触发器绝对定位盖在它上面，于是外壳尺寸完全等于原生控件。
    select.parentNode.insertBefore(shell, select);
    shell.appendChild(select);

    const trigger = document.createElement('button');
    trigger.type = 'button';
    trigger.className = 'select-trigger';
    trigger.setAttribute('aria-haspopup', 'listbox');
    trigger.setAttribute('aria-expanded', 'false');

    const valueEl = document.createElement('span');
    valueEl.className = 'select-value';
    trigger.appendChild(valueEl);

    const caret = document.createElement('span');
    caret.className = 'select-caret';
    caret.innerHTML = CARET;
    trigger.appendChild(caret);

    const popover = document.createElement('div');
    popover.className = 'select-popover';
    popover.setAttribute('role', 'listbox');
    popover.id = uid();
    trigger.setAttribute('aria-controls', popover.id);

    shell.appendChild(trigger);
    shell.appendChild(popover);

    // 只搬语义属性，不搬 class：
    //   · 宽度 / 布局不用搬 —— 原生 select 留在外壳里当锚，它自己的
    //     .model-select{max-width} / .log-filters select{min-width} / .proxy-select
    //     等规则照旧生效，外壳跟着它一起被撑到同样尺寸。
    //   · 不能搬 —— proxy-form.js 用 container.querySelector('.pf-clash-select')
    //     取控件读写 value，类名一旦出现在外壳上，这些查询会命中外壳而不是 select，
    //     业务逻辑当场失效。外观上的差异（如 .model-select 的强调边框）改由
    //     css/select.css 里对应的 :has() 规则镜像到触发器上。
    if (select.title) trigger.title = select.title;
    if (select.getAttribute('aria-label')) trigger.setAttribute('aria-label', select.getAttribute('aria-label'));
    if (select.disabled) trigger.disabled = true;

    const ctx = {
      select, shell, trigger, valueEl, popover,
      // multiple 的判定在增强时就固定下来（DOM 属性不会中途改），后面各分支都读它，
      // 不必反复查 select.multiple
      multi: select.multiple === true,
      highlight: -1, optionEls: [], sig: '',
      pendingSync: false, writing: false,
    };
    registry.set(select, ctx);
    if (ctx.multi) {
      shell.classList.add('select-shell-multi');
      popover.setAttribute('aria-multiselectable', 'true');
    }

    bindTrigger(ctx);
    bindKeyboard(ctx);
    bindPopover(ctx);
    renderOptions(ctx);
    syncState(ctx);
    // 装在渲染之后：patchValue 会截获此后所有 value / selectedIndex 赋值
    patchValue(ctx);

    // 盯住这个 select 自身：option 增删、selected 变化、disabled 变化。
    // 属性过滤器只列这两个（外加子树的文本变化由 childList 覆盖）：
    // 业务侧的重建走 innerHTML / appendChild，改值走 patchValue 的访问器，
    // 都不依赖属性观察，列多了只是徒增回调。
    const watcher = new MutationObserver(() => {
      renderOptions(ctx);
      syncState(ctx);
    });
    watcher.observe(select, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ['selected', 'disabled'],
    });
    ctx.watcher = watcher;

    // 触发器被量成 0 尺寸时说明它（连同外层容器）被藏起来了 —— 弹窗关闭会把
    // .modal-mask 设成 display:none、切页会把非当前 .page 设成 display:none。
    // 这些关闭动作多由点击触发（那样已经被「点外部关闭」接住），但业务也可能
    // 直接改样式而没有任何点击事件，此时浮层还挂在 body 上就成了悬浮的孤儿。
    // 用 ResizeObserver 兜住这种情况：开销极小，且只在真的隐藏时才动作。
    // 加存在性判断是为了在缺少该 API 的旧内核上安静降级，不影响增强本身。
    if (typeof ResizeObserver === 'function') {
      const sizeWatcher = new ResizeObserver(entries => {
        if (openInstance !== ctx) return;
        const box = entries[0]?.contentRect;
        if (box && !box.width && !box.height) close(ctx);
      });
      sizeWatcher.observe(trigger);
      ctx.sizeWatcher = sizeWatcher;
    }
  }

  /**
   * 渲染浮层选项：按 select.options 重建列表。
   * 只负责结构与禁用态；「哪一项是选中」交给紧随其后的 syncState —— 那里是
   * 唯一的选中态出口，省得两处各写一份、日后改一处忘一处。
   */
  function renderOptions(ctx) {
    const { select, popover } = ctx;
    popover.innerHTML = '';
    ctx.optionEls = [];

    for (const option of select.options) {
      const el = document.createElement('div');
      el.className = 'select-option';
      el.setAttribute('role', 'option');
      if (option.disabled) {
        el.classList.add('is-disabled');
        el.setAttribute('aria-disabled', 'true');
      }
      // 值挂在 dataset 上：选项文本可能重复，值才是唯一标识
      el.dataset.value = option.value;

      const label = document.createElement('span');
      label.className = 'select-option-label';
      label.textContent = option.textContent.trim();
      el.appendChild(label);

      popover.appendChild(el);
      ctx.optionEls.push(el);
    }
  }

  /** 同步触发器文本、禁用态与浮层选中态（值变更 / option 变更后调用） */
  function syncState(ctx) {
    const { select, trigger, valueEl } = ctx;
    const text = selectedText(select) || '';
    // 空选项（无内容）时由 CSS 补一个零宽占位，避免触发器塌成一条线
    valueEl.textContent = text;
    valueEl.classList.toggle('is-empty', !text);
    // 禁用态直接落在 button 上：:disabled 天然带「不进 Tab 序列、不派发 click」，
    // 比在外壳上挂类名再逐条模拟要稳
    trigger.disabled = !!select.disabled;
    // 开着的时候被业务禁用（proxy-form.js 拉不到 Clash 出口就会这么干），
    // 浮层要跟着收起来，否则会留一个点不动的浮层挂在屏幕上
    if (select.disabled && openInstance === ctx) close(ctx);

    const selected = selectedSet(select);
    let selectedIndex = -1;
    ctx.optionEls.forEach((el, index) => {
      const match = selected.has(el.dataset.value ?? '');
      el.classList.toggle('is-selected', match);
      el.setAttribute('aria-selected', match ? 'true' : 'false');
      const check = el.querySelector('.select-option-check');
      if (match && !check) {
        const mark = document.createElement('span');
        mark.className = 'select-option-check';
        mark.innerHTML = CHECK;
        el.appendChild(mark);
      } else if (!match && check) {
        check.remove();
      }
      if (match) selectedIndex = index;
    });

    // 高亮项失效（浮层没开时）就复位到选中项，保证下次打开落在正确位置
    if (openInstance !== ctx || ctx.highlight < 0 || !ctx.optionEls[ctx.highlight]) {
      ctx.highlight = selectedIndex;
    }
    paintHighlight(ctx);
    ctx.sig = signature(select);
  }

  /**
   * 当前内容指纹：值 + 禁用态 + 每个 option 的 value/文本/禁用态。
   * 用来判断界面上显示的东西是否还是最新的 —— 见 refreshIfStale。
   *
   * 多选下 `select.value` 只是**第一个**选中项的 value，改别的项时它可能一动不动，
   * 所以这里改把每个 option 的 selected 也编进指纹：业务代码写
   * `option.selected = true`（多选回填的常见写法，不动 value 属性节点）时，
   * 打开浮层前的那一次指纹比对才能发现界面是旧的。
   */
  function signature(select) {
    let sig = `${select.value}\u0001${select.disabled ? 1 : 0}\u0001${select.options.length}`;
    for (const option of select.options) {
      sig += `\u0001${option.value}\u0002${option.textContent}\u0002${option.disabled ? 1 : 0}`
        + `\u0002${option.selected ? 1 : 0}`;
    }
    return sig;
  }

  /**
   * 兜底同步：业务侧「只改值、不重建 option」时（accounts-view.js 里模型跟随
   * 最近一次请求、proxy-form.js 的 fill 回填协议）既没有 childList 变更、
   * 也不会产生 selected 属性变更，两个 MutationObserver 都收不到信号。
   * 这类写入无法靠观察 DOM 捕获，所以：① 用下面的 patchValue 直接截获赋值；
   * ② 每次打开浮层前再按指纹比对一次，双保险。
   */
  function refreshIfStale(ctx) {
    const next = signature(ctx.select);
    if (ctx.sig === next) return;
    renderOptions(ctx);
    syncState(ctx);
  }

  /**
   * 给这个 select 实例装一个自己的 value / selectedIndex 访问器，
   * 转发到原型上的原生实现，赋值后额外触发一次同步。
   *
   * 为什么必须这么做：`select.value = 'x'` 不会改动任何属性节点，
   * MutationObserver 完全看不见，触发器文案会停在上一个模型名上。
   * 装在实例上（而不是改原型）只影响本组件接管的下拉，不污染全局。
   */
  function patchValue(ctx) {
    const proto = Object.getPrototypeOf(ctx.select);
    for (const key of ['value', 'selectedIndex']) {
      const desc = Object.getOwnPropertyDescriptor(proto, key);
      if (!desc?.get || !desc?.set) continue;
      Object.defineProperty(ctx.select, key, {
        configurable: true,
        enumerable: false,
        get() { return desc.get.call(this); },
        set(next) {
          desc.set.call(this, next);
          // 组件自己写值（见 pick）时不需要再刷新一遍界面，否则每次选择都会
          // 白跑一次浮层重建
          if (ctx.writing) return;
          // 赋值方（业务代码）可能正在同一次渲染里连续改多处，用微任务合并，
          // 保证它这一轮写完之后再刷新界面
          if (!ctx.pendingSync) {
            ctx.pendingSync = true;
            queueMicrotask(() => {
              ctx.pendingSync = false;
              // select 可能已被业务移除（切页 / 重渲染），别对着游离节点干活
              if (registry.has(ctx.select) && ctx.select.isConnected) {
                renderOptions(ctx);
                syncState(ctx);
              }
            });
          }
        },
      });
    }
  }

  function paintHighlight(ctx) {
    ctx.optionEls.forEach((el, index) => {
      el.classList.toggle('is-highlight', index === ctx.highlight);
    });
  }

  function bindTrigger(ctx) {
    // 只处理鼠标（与读屏软件派发的合成 click）。键盘那条路在 bindKeyboard 里
    // 自行处理并用 preventDefault 掐掉了按钮的默认激活，不会走到这里来。
    ctx.trigger.addEventListener('click', () => {
      if (ctx.select.disabled) return;
      if (openInstance === ctx) close(ctx);
      else open(ctx);
    });
  }

  /**
   * 键盘挂在 shell 上（触发器在里面），不拆到 document：
   * 两处各处理一次会互相打架（方向键走两步、回车「确认完又被点开」）。
   */
  function bindKeyboard(ctx) {
    ctx.shell.addEventListener('keydown', event => {
      const isOpen = openInstance === ctx;
      switch (event.key) {
        case 'ArrowDown':
        case 'ArrowUp': {
          // 关着的时候方向键只负责打开（高亮落在当前选中项），开着才移动；
          // 若打开的同时也走一步，用户按一次↓会跳过一项，和原生手感对不上
          if (!isOpen) open(ctx);
          else move(ctx, event.key === 'ArrowDown' ? 1 : -1);
          break;
        }
        case 'Home':
        case 'End':
          if (!isOpen) open(ctx);
          else jump(ctx, event.key === 'Home' ? 0 : ctx.optionEls.length - 1);
          break;
        case 'Enter':
        case ' ':                      // 空格：原生 select 也用空格确认
          // 开着时这两个键表示「确认高亮项」，没开则用来打开。
          // 下面的 preventDefault 顺带取消 button 自己的激活（Enter 的合成 click
          // 在 keydown、空格的在 keyup），于是开启 / 确认只会发生一次。
          if (isOpen) confirm(ctx);
          else open(ctx);
          break;
        case 'Escape':
          if (!isOpen) return;         // 没开就别拦，让上层（弹窗）自己处理 Esc
          close(ctx);
          event.preventDefault();
          // 必须掐住冒泡：弹窗（账号设置 / 批量操作）在 document 上也有 Escape
          // 监听，不拦的话一次 Esc 会先关浮层、再顺手把整个弹窗也关掉。
          // 这里只吞掉「浮层开着」的那一次，之后按 Esc 就归弹窗处理。
          event.stopPropagation();
          return;
        case 'Tab':
          if (isOpen) close(ctx);
          return;                      // Tab 不拦截，让焦点正常走出
        default:
          return;
      }
      event.preventDefault();
    });
  }

  function bindPopover(ctx) {
    const { popover } = ctx;
    // 只在按到选项上时拦下 mousedown：否则点选项会把焦点从触发器挪走，
    // 焦点一离开 shell，focusin 的「移出即关闭」就会在 click 之前先把浮层关掉。
    // 不加判断地整块拦截会把浮层自己的滚动条也一并废掉（按住滑块拖不动）。
    popover.addEventListener('mousedown', event => {
      if (event.target.closest('.select-option')) event.preventDefault();
    });
    popover.addEventListener('click', event => {
      const el = event.target.closest('.select-option');
      if (!el || !popover.contains(el)) return;
      if (el.classList.contains('is-disabled')) return;
      pick(ctx, el);
    });
    // 鼠标划过时同步键盘高亮，避免出现「两处高亮」的错觉
    popover.addEventListener('mousemove', event => {
      const el = event.target.closest('.select-option');
      if (!el || el.classList.contains('is-disabled')) return;
      const index = ctx.optionEls.indexOf(el);
      if (index >= 0 && index !== ctx.highlight) {
        ctx.highlight = index;
        paintHighlight(ctx);
      }
    });
  }

  /**
   * 判断事件目标是否属于「当前这个打开着的下拉」。
   * 浮层打开期间挂在 body 上（见 place），不在 shell 里，所以两处都要查。
   */
  function owns(ctx, target) {
    if (!target || ctx.shell.contains(target)) return true;
    return ctx.popover.contains(target);
  }

  function bindGlobal() {
    // 点浮层外部关闭：用捕获阶段，保证业务自己的 click 处理器先照常跑到。
    // 目标判断走 owns：浮层此刻可能挂在 body 上，只看 shell 会把它当「外部」
    document.addEventListener('click', event => {
      if (!openInstance) return;
      if (owns(openInstance, event.target)) return;
      close(openInstance);
    }, true);

    // 浮层是 fixed 定位、不跟随滚动：滚动或改窗口尺寸时直接关闭，
    // 免得浮层停在一个已经和触发器错位的位置上。
    // 但浮层自身的滚动不算 —— 选项多的时候要用滚轮翻，关掉就没法用了。
    window.addEventListener('scroll', event => {
      if (!openInstance) return;
      if (openInstance.popover.contains(event.target)) return;
      close(openInstance);
    }, true);
    window.addEventListener('resize', () => { if (openInstance) close(openInstance); });

    // 焦点离开整个下拉就关闭（Tab 走开、点到别的控件）
    document.addEventListener('focusin', event => {
      if (!openInstance) return;
      if (owns(openInstance, event.target)) return;
      close(openInstance);
    });
  }

  /** 步进到下一个可选中的选项：跳过 disabled，到头停住（原生 select 也不循环） */
  function step(ctx, from, delta) {
    let index = from;
    for (let i = 0; i < ctx.optionEls.length; i++) {
      index += delta;
      if (index < 0 || index >= ctx.optionEls.length) return from;
      if (!ctx.optionEls[index].classList.contains('is-disabled')) return index;
    }
    return from;
  }

  function move(ctx, delta) {
    // 起点：已经有高亮就从它出发；没有的话按方向从列表两端之外找起
    // （↑ 从末尾往前、↓ 从开头往后），于是「打开后直接按↓」落在第一项
    const start = ctx.highlight < 0 ? (delta > 0 ? -1 : ctx.optionEls.length) : ctx.highlight;
    const next = step(ctx, start, delta);
    if (next === ctx.highlight) return;     // step 到头会原样返回起点
    ctx.highlight = next;
    paintHighlight(ctx);
    scrollHighlightIntoView(ctx.popover, ctx.optionEls[next]);
  }

  function jump(ctx, index) {
    if (index < 0 || index >= ctx.optionEls.length) return;
    if (ctx.optionEls[index].classList.contains('is-disabled')) return;
    ctx.highlight = index;
    paintHighlight(ctx);
    scrollHighlightIntoView(ctx.popover, ctx.optionEls[index]);
  }

  /**
   * 确认高亮项：单选写值 + 关浮层 + 派发 change；多选只切换勾选、不关浮层
   * （见 pick 的说明），剩下的交给业务监听。
   */
  function confirm(ctx) {
    const el = ctx.optionEls[ctx.highlight];
    if (!el || el.classList.contains('is-disabled')) {
      close(ctx);
      return;
    }
    pick(ctx, el);
  }

  /**
   * 多选：切换某一项的勾选态（不关浮层、不改别的项）。
   *
   * 与单选「点一下就定」不同，多选的浮层必须留着 —— 用户要连着勾好几家，
   * 点一下就关等于每选一项都要重新打开一次。也因此**不**在这里 close(ctx)。
   *
   * 写值走原生 option.selected：`select.value = x` 在多选下的语义是「只留这一项」，
   * 会静默清掉其它勾选，正是这里要避免的。
   *
   * 这里显式调一次 syncState（而不是等那个 MutationObserver）：改 option.selected
   * 确实会让 observer 收到通知，但它的回调是**微任务**，而紧随其后的
   * dispatchEvent 是同步的 —— 业务监听读到的会是没同步过的界面。
   */
  function toggle(ctx, el) {
    const { select } = ctx;
    const option = [...select.options].find(item => (item.value ?? '') === (el.dataset.value ?? ''));
    if (!option || option.disabled) return;

    ctx.writing = true;
    try {
      option.selected = !option.selected;
    } finally {
      ctx.writing = false;
    }
    syncState(ctx);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function pick(ctx, el) {
    if (ctx.multi) {
      toggle(ctx, el);
      return;
    }
    const value = el.dataset.value ?? '';
    const { select } = ctx;
    // 先关浮层再改值：业务监听里通常会整块重绘（列表 / 表格），
    // 重绘时浮层如果还开着，它的节点会被一起搬走
    close(ctx);
    if (selectedValue(select) === value) {
      // 值没变就不派发 change（与原生行为一致），但要把界面同步回来
      syncState(ctx);
      return;
    }
    // writing 期间屏蔽 patchValue 的回调：这次改动由组件自己负责同步，
    // 而且必须在派发 change 之前同步完，让业务监听读到的是已经更新好的界面
    ctx.writing = true;
    try {
      select.value = value;
    } finally {
      ctx.writing = false;
    }
    syncState(ctx);
    select.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function open(ctx) {
    // 没有可选项时不打开：空浮层（比如日志分类还没拉到字典）在视觉上像个 bug
    if (!ctx.optionEls.length) return;
    if (openInstance && openInstance !== ctx) close(openInstance);
    if (ctx.select.disabled) return;
    // 打开前先按指纹兜一次底：值被业务代码悄悄改过而信号没传到时，
    // 这里能保证浮层打开时显示的选中项一定是最新的
    refreshIfStale(ctx);
    openInstance = ctx;
    // 打开时高亮落在当前选中项上；没有选中项则落到第一个可选项
    const selectedIndex = ctx.optionEls.findIndex(el => el.classList.contains('is-selected'));
    ctx.highlight = selectedIndex >= 0 ? selectedIndex : step(ctx, -1, 1);
    paintHighlight(ctx);
    ctx.trigger.classList.add('is-open');
    ctx.trigger.setAttribute('aria-expanded', 'true');
    // 挂到 body 再显示：见 place 的注释（绕开祖先容器的 overflow 裁剪）
    document.body.appendChild(ctx.popover);
    ctx.popover.classList.add('open');
    // 先加 .open（display 从 none 变 block）再量尺寸：display:none 时
    // offsetHeight / scrollHeight 全是 0，量不到就只能得到错的高度预算
    place(ctx);
    // 滚到可见要等浏览器把上面的 max-height 应用完，否则 clientHeight 还是旧值
    requestAnimationFrame(() => scrollHighlightIntoView(ctx.popover, ctx.optionEls[ctx.highlight]));
  }

  function close(ctx) {
    if (openInstance === ctx) openInstance = null;
    ctx.popover.classList.remove('open');
    ctx.trigger.classList.remove('is-open');
    ctx.trigger.setAttribute('aria-expanded', 'false');
    // 浮层放回 shell：结构上与触发器维持父子关系，shell 被移除时浮层跟着走
    // （开关期间挂在 body 上，若此时宿主整块被移除，浮层会变成孤儿节点）
    if (ctx.popover.parentNode !== ctx.shell) {
      ctx.popover.removeAttribute('style');
      ctx.shell.appendChild(ctx.popover);
    }
  }

  /** 扫描一棵子树（含自身）里的 select 并增强；批量插入时只调用一次 */
  function scan(root) {
    if (root.nodeType !== 1) return;
    if (root.matches?.('select')) enhance(root);
    root.querySelectorAll?.('select').forEach(enhance);
  }

  function boot() {
    scan(document.body);
    // 全局监听只装一次（点外部关闭 / Esc / 滚动 / 尺寸变化 / 焦点移出），
    // 它们都通过 openInstance 找到当前打开的那个实例
    bindGlobal();
    // 页面后续插入的 select（弹窗内容、动态表单）由这个 observer 接住。
    // 用「微任务合并」而不是立即处理：一次 innerHTML 可能连续插入多个节点，
    // 每个节点都触发一次回调，攒到微任务里统一扫一遍，避免重复遍历。
    let queued = false;
    const pending = [];
    const observer = new MutationObserver(records => {
      // 顺带回收：宿主整体被移除（容器 innerHTML 重建、切页销毁内容）时，
      // 打开着的浮层因为挂在 body 上不会跟着消失，得在这里补一刀。
      // 放在同一个回调里判断，避免为这一件小事再挂一个 observer。
      if (openInstance && !openInstance.shell.isConnected) close(openInstance);
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

    // 对外只暴露排查 / 兜底入口，业务不需要（也不应该）手动调用
    window.wbSelect = {
      enhance: select => { scan(select); return select; },
      enhanced: select => registry.has(select),
      close: () => { if (openInstance) close(openInstance); },
      /**
       * 立即把界面同步到 select 的当前状态。
       *
       * 给「直接改 option.selected / 重建 option 而不经过 value 赋值器」的业务用 ——
       * 多选的回填就是这种写法（`select.value = x` 在多选下只会留下 x 一项）。
       * 两个 MutationObserver 其实都会收到通知，但它们的回调跑在微任务里，
       * 同一轮同步代码里接着读界面还是旧的；这里同步跑一次补上这个空档。
       */
      sync: select => {
        const ctx = registry.get(select);
        if (!ctx) return false;
        renderOptions(ctx);
        syncState(ctx);
        return true;
      },
    };
  }

  // app.js 在 script 尾部同步执行 DOM 查询，select.js 必须排在它之前；
  // 排在之前时 DOM 可能还没解析完，所以按下 readyState 决定何时启动。
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot, { once: true });
  else boot();
})();
