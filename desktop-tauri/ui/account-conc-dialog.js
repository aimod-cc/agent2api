/* Agent2API · 账号的「并发上限」小对话框（⋯ 菜单的入口） */
/* global workbuddyDesktop, wbApp */

/**
 * 单账号并发上限的编辑弹窗：一个数字输入 + 一段口径说明 + 保存/取消。
 * 从 ⋯ 菜单的「并发上限」项打开（菜单项在 accounts-model.js 的 moreMenuHtml，
 * 点击分支在 accounts-view.js，那里调 window.wbAccountConcDialog.open）。
 *
 * ── 为什么单独成文件 ────────────────────────────────────────
 * accounts-view.js 已经超出单文件行数约定，而本弹窗是**单字段**的小表单，
 * 与它的职责（重绘编排、批量选择、事件委托）不相干 —— 按 confirm-dialog.js /
 * request-clear-modal.js 那类「一个弹窗一个文件」的既有模式单独成件，
 * 只暴露一个 open(account)。
 *
 * 弹窗形态照 custom-provider-ui.js 的 openEditDialog 手法：点击时动态建、
 * 关闭即移除，不在 index.html 里常驻 —— 常驻节点会让 index.html 为每一张
 * 表背一份用不到的 DOM。说明文案写清「达上限会跳过、全达上限按余量挤占」，
 * 那是选路兜底的承诺（后端 rotate.rs 的第二级），界面上说了就要与后端行为一致。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  const MODAL_ID = 'acct-conc-modal';
  /** 与后端 apply_patch 的封顶值一致（store_crud 的 MAX_CONCURRENT_LIMIT） */
  const MAX_LIMIT = 999;

  function closeDialog() {
    $(MODAL_ID)?.remove();
  }

  /**
   * 打开对话框。当前值从账号公开形态的 `maxConcurrent` 读（后端恒输出数字，
   * 缺省 0 = 不限），输入框预填它 —— 用户看得见「现在是多少」，而不是一个空框。
   */
  function open(account) {
    if (!account?.id) return;
    closeDialog();
    const current = Number(account.maxConcurrent) || 0;
    const name = account.nickname || account.name || account.id;
    const mask = document.createElement('div');
    mask.id = MODAL_ID;
    mask.className = 'modal-mask open';
    mask.setAttribute('role', 'dialog');
    mask.setAttribute('aria-modal', 'true');
    mask.setAttribute('aria-labelledby', 'acct-conc-title');
    mask.innerHTML = `<div class="modal">
        <div class="modal-head">
          <h2 id="acct-conc-title">并发上限 · ${esc(name)}</h2>
          <button type="button" id="acct-conc-close" title="关闭">✕</button>
        </div>
        <div class="modal-body">
          <div class="modal-section">
            <div class="field-row">
              <label for="acct-conc-input">同时处理的请求数</label>
              <input id="acct-conc-input" type="number" min="0" max="${MAX_LIMIT}" step="1" value="${current}">
            </div>
            <p>该账号同时最多处理的请求数，0 表示不限制。达到上限的账号会跳过，请求转给其他账号；全部账号都达上限时按余量挤占。</p>
          </div>
        </div>
        <div class="modal-foot">
          <span class="detail" id="acct-conc-hint"></span>
          <div class="spacer"></div>
          <button type="button" id="acct-conc-cancel">取消</button>
          <button type="button" id="acct-conc-save" class="primary">保存</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    $('acct-conc-close')?.addEventListener('click', closeDialog);
    $('acct-conc-cancel')?.addEventListener('click', closeDialog);
    // 点遮罩空白处 = 关闭（与其它弹窗同一交互），点弹窗本体不关
    mask.addEventListener('click', event => {
      if (event.target === mask) closeDialog();
    });
    $('acct-conc-save')?.addEventListener('click', () => { void save(account.id); });
    $('acct-conc-input')?.focus();
  }

  /**
   * 保存：PATCH `/api/accounts/{id}` 只带 `maxConcurrent` 一个字段（后端
   * apply_patch 是 patch 语义，没传的字段一概不动）。成功 toast「已保存」
   * 并刷新账号列表（与行内启用/禁用同一条收尾）；失败 toast 错误并把脚注
   * 留一份（toast 几秒后就没了）。
   *
   * 归一在本地先做（与优先级输入框同一手法）：number 输入挡不住手工键入的
   * 脏值，负数/小数/超界都 clamp 到 0~999 的整数 —— 后端 400 只该是最后防线，
   * 而不是日常路径。
   */
  async function save(accountId) {
    const input = $('acct-conc-input');
    if (!input) return;
    const raw = Number(input.value);
    if (!Number.isFinite(raw)) {
      toast(`并发上限必须是 0~${MAX_LIMIT} 的整数`, 'err');
      return;
    }
    const next = Math.min(MAX_LIMIT, Math.max(0, Math.round(raw)));
    const save = $('acct-conc-save');
    if (save) { save.disabled = true; save.textContent = '保存中…'; }
    try {
      await api.updateAccount(accountId, { maxConcurrent: next });
      // 先关窗并反馈成功（与设置弹窗同一收尾顺序）：改动已落库，
      // 刷新只是让列表跟上；刷新失败只影响本次界面同步，不折进「保存失败」
      closeDialog();
      toast('✅ 已保存');
      await wbApp.refresh?.();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast(`保存失败：${message}`, 'err');
      const hint = $('acct-conc-hint');
      if (hint) hint.textContent = message;
      if (save) { save.disabled = false; save.textContent = '保存'; }
    }
  }

  window.wbAccountConcDialog = { open, close: closeDialog };
})();
