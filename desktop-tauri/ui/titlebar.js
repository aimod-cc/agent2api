/* Agent2API · 自定义标题栏（桌面端） */
/* global workbuddyDesktop, wbIcons */

/**
 * 主窗口去掉系统装饰后的自绘标题条（src-tauri 建窗处 decorations(false)）。
 * 32px 高：左侧品牌图标 + 标题，右侧最小化 / 最大化(还原) / 关闭三键。
 * 形态参考 OmniProxy 的 TitleBar.tsx（Electron + antd 版）。
 *
 * ── 为什么由脚本动态创建，而不是写死在 index.html 里 ─────────
 * 网页版（headless 托管面板）没有应用窗口，标题栏不该出现。动态创建
 * 让网页版零痕迹（连 DOM 都不建），body 布局也不用为它预留占位 ——
 * 桌面端创建时才给 body 挂 .has-titlebar，.shell 的高度随之少一行标题栏
 * （见 layout.css）；网页端没有这个 class，布局保持 100vh 不变。
 *
 * ── 渲染守卫（两层，缺一不渲染）───────────────────────────
 * 1. 平台：桥接层注入的 platform 是壳的编译目标（'windows' / 'macos' /
 *    'linux'），网页 shim 注入 'web' —— 与 settings-panel.js 裁剪
 *    「面板登录」的判断同一口径；缺字段（桥未就绪的异常形态）同样不动。
 * 2. 方法：三键方法在桥里缺失（旧壳）时不渲染，避免摆出一排点了没反应
 *    的按钮。
 *
 * ── 三键的调用路径 ────────────────────────────────────────
 * 全部走 window.workbuddyDesktop（bridge.rs → commands.rs 的四个窗口命令），
 * 与其余功能同一封装。其中「关闭」发出 CloseRequested，与点系统关闭按钮
 * 同语义：「关闭到托盘」开启时被壳拦成隐藏（托盘继续转发），否则正常退出。
 *
 * ── 最大化状态如何保持同步 ────────────────────────────────
 * 初始化查一次 windowIsMaximized；此后订阅 onWindowResize（Tauri 的内置
 * 事件 tauri://resize，最大化 / 还原 / 拖拽缩放都会发出）重查 —— 双击标题栏
 * 切最大化走的是 Tauri 的原生行为（不经按钮），靠这条也能追上。
 *
 * ── 拖动 / 双击最大化 ─────────────────────────────────────
 * 标题条上声明 data-tauri-drag-region 即可（Tauri 2 内建，无需 JS）：core
 * 脚本监听 mousedown，目标元素带该属性才拖动（子元素不带不拖），双击切
 * 最大化。品牌区整体 pointer-events:none（layout.css），让按下时的 target
 * 始终落回标题条本身。
 */
(() => {
  const bridge = window.workbuddyDesktop;
  if (!bridge) return;
  // 守卫一：平台标识（见文件头「渲染守卫」）
  if (!bridge.platform || bridge.platform === 'web') return;
  // 守卫二：三键方法齐备才渲染
  if (typeof bridge.windowMinimize !== 'function'
    || typeof bridge.windowToggleMaximize !== 'function'
    || typeof bridge.windowClose !== 'function') return;

  // 三键图标：24×24 画布 + stroke 1.8 的内联 SVG，与 icons.js 的
  // arrowUp/arrowDown 同一风格（不用字体字形：不同机器基线与粗细不可控）。
  // 还原图标是两块错开的方框，「前块」填标题栏底色遮住后块的相交线
  // （.ico-restore .fg 的 fill 由 CSS 变量下发，深浅主题各自正确）。
  const stroke = 'fill="none" stroke="currentColor" stroke-width="1.8"'
    + ' stroke-linecap="round" stroke-linejoin="round"';
  const svg = inner =>
    `<svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true" focusable="false">${inner}</svg>`;
  const ICONS = {
    minimize: svg(`<g ${stroke}><path d="M5 12h14"/></g>`),
    maximize: svg(`<g ${stroke}><rect x="5.5" y="5.5" width="13" height="13" rx="1.5"/></g>`),
    restore: svg(`<g ${stroke}><rect x="8.5" y="4.5" width="11" height="11" rx="1.5"/>`
      + `<rect class="fg" x="4.5" y="8.5" width="11" height="11" rx="1.5"/></g>`),
    close: svg(`<g ${stroke}><path d="M6 6l12 12"/><path d="M18 6 6 18"/></g>`),
  };

  // ── 组装 DOM：header 挂在 body 最顶部，sidebar / main 之外整宽一条 ──
  const bar = document.createElement('header');
  bar.className = 'titlebar';
  // 拖动与双击最大化：Tauri 2 内建支持，声明属性即可（见文件头说明）
  bar.setAttribute('data-tauri-drag-region', '');

  // 左侧品牌区（图标复用 icons.js 的 brand，与侧栏品牌区 / 应用图标同一造型；
  // 文案与窗口 title 保持一致）
  const brand = document.createElement('div');
  brand.className = 'titlebar-brand';
  brand.innerHTML =
    (window.wbIcons ? wbIcons.icon('brand', 16) : '')
    + '<span class="txt">Agent2API · 多提供商本地网关</span>';
  bar.appendChild(brand);

  // 右侧三键（普通 button，不带拖动属性：Tauri 只认带属性的元素）
  const actions = document.createElement('div');
  actions.className = 'titlebar-actions';
  const makeBtn = (cls, label) => {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = `titlebar-btn ${cls}`;
    btn.title = label;
    btn.setAttribute('aria-label', label);
    return btn;
  };
  const minBtn = makeBtn('min', '最小化');
  minBtn.innerHTML = ICONS.minimize;
  const maxBtn = makeBtn('max', '最大化');
  const closeBtn = makeBtn('close', '关闭');
  closeBtn.innerHTML = ICONS.close;
  actions.append(minBtn, maxBtn, closeBtn);
  bar.appendChild(actions);

  document.body.insertBefore(bar, document.body.firstChild);
  // 布局随标题栏让位（.shell 高度少一行，见 layout.css）
  document.body.classList.add('has-titlebar');

  // ── 最大化状态：图标 / 悬停文案随状态切换 ──
  let maximized = false;
  const paintMax = () => {
    maxBtn.innerHTML = maximized ? ICONS.restore : ICONS.maximize;
    maxBtn.classList.toggle('is-maximized', maximized);
    maxBtn.title = maximized ? '还原' : '最大化';
    maxBtn.setAttribute('aria-label', maxBtn.title);
  };

  // 重查一次最大化状态；查询失败（罕见）保持现状即可
  const syncMaximized = async () => {
    if (typeof bridge.windowIsMaximized !== 'function') return;
    try {
      maximized = !!(await bridge.windowIsMaximized());
      paintMax();
    } catch { /* 窗口正在销毁等瞬时错误：下一条 resize 事件会再来 */ }
  };

  paintMax();
  syncMaximized();
  // 尺寸变化（最大化 / 还原 / 拖拽缩放）后重查，让图标始终与实际状态一致
  if (typeof bridge.onWindowResize === 'function') {
    bridge.onWindowResize(() => { syncMaximized(); });
  }

  // ── 三键行为（失败静默：窗口动作没有可提示的去处，界面层不弹 toast）──
  minBtn.addEventListener('click', () => {
    bridge.windowMinimize().catch(() => {});
  });
  maxBtn.addEventListener('click', async () => {
    try {
      await bridge.windowToggleMaximize();
      await syncMaximized();
    } catch (error) {
      console.warn('切换窗口大小失败:', error);
    }
  });
  // 关闭 = CloseRequested，与系统关闭按钮同语义：close_to_tray 开启时被壳
  // 拦成隐藏（托盘继续转发），否则正常退出（见 commands.rs 的 window_close）
  closeBtn.addEventListener('click', () => {
    bridge.windowClose().catch(() => {});
  });
})();
