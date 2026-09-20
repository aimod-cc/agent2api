/* Agent2API · 通用确认弹窗（替代原生 confirm） */
/* global wbApp */

/**
 * 为什么要有这个模块：Tauri 的 WebView 里原生 `window.confirm()` 不弹窗、
 * 直接放行（返回 true），所有依赖它的危险操作（清空 / 删除）等于没有确认。
 * 这里用项目既有的 .modal-mask / .modal 外壳（与设置页确认弹窗同一套观感）
 * 提供一个 Promise 化的确认框，调用方写法与 confirm() 一样线性：
 *
 *   if (!await wbConfirm.ask({ title: '清空运行日志', html: '…', okText: '清空', okClass: 'danger' })) return;
 *
 * 同时只开一个弹窗：重入时把前一个按「取消」收尾（与设置页确认弹窗同一取向），
 * 避免两个调用方的 Promise 永远挂着。
 *
 * 依赖 window.wbApp.esc（app.js 之后加载的场景也能用：esc 有本地兜底实现）。
 */
(() => {
  const $ = id => document.getElementById(id);

  const esc = value => String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');

  /** 当前弹窗的 resolver；非空即表示弹窗开着 */
  let resolver = null;

  /** 收口：关窗并把结果交给等待者（重复调用无副作用） */
  function settle(result) {
    const resolve = resolver;
    resolver = null;
    $('confirm-modal')?.classList.remove('open');
    if (resolve) resolve(result);
  }

  /**
   * 弹确认框，返回 Promise<boolean>：确认 true；取消 / 关闭 / 遮罩 / Esc false。
   *
   *   title      标题（纯文本）
   *   html       正文，允许 <strong> 等少量标记；内容由调用方负责转义
   *   text       纯文本正文的便捷入参（与 html 二选一，内部转义，\n 转成 <br>）
   *   okText     确认键文案（如「清空」「删除」）
   *   okClass    确认键样式：primary（默认）或 danger（不可恢复的危险操作）
   *   bodyClass  正文容器样式：danger-zone（默认红底警示框）；普通提示传空串
   *   cancelText 取消键文案
   */
  function ask({
    title = '确认操作',
    html = '',
    text = '',
    okText = '确定',
    okClass = 'primary',
    bodyClass = 'danger-zone',
    cancelText = '取消',
  } = {}) {
    return new Promise(resolve => {
      if (resolver) settle(false);
      resolver = resolve;

      const titleEl = $('confirm-modal-title');
      const bodyEl = $('confirm-modal-text');
      const ok = $('confirm-modal-ok');
      const cancel = $('confirm-modal-cancel');
      if (titleEl) titleEl.textContent = title;
      if (bodyEl) {
        bodyEl.className = bodyClass;
        bodyEl.innerHTML = html || esc(text).replace(/\n/g, '<br>');
      }
      if (ok) {
        ok.textContent = okText;
        ok.className = okClass;
      }
      if (cancel) cancel.textContent = cancelText;

      $('confirm-modal')?.classList.add('open');
      // 焦点落在「取消」：不可恢复的操作，敲回车不该等于同意
      cancel?.focus();
    });
  }

  $('confirm-modal-ok')?.addEventListener('click', () => settle(true));
  $('confirm-modal-cancel')?.addEventListener('click', () => settle(false));
  $('confirm-modal-close')?.addEventListener('click', () => settle(false));
  $('confirm-modal')?.addEventListener('click', event => {
    if (event.target === $('confirm-modal')) settle(false);
  });
  // Esc = 取消。app.js / settings-panel 各有自己的全局 Esc（互不干扰：
  // 那边只关自己的弹窗，这边只在确认弹窗开着时动作）
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && resolver) settle(false);
  });

  window.wbConfirm = { ask };
})();
