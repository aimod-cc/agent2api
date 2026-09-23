/* Agent2API · 自定义提供商的管理入口（管理弹窗 / 编辑弹窗 / 删除确认 / 模型管理转发） */
/* global wbApp */

/**
 * 账号页工具栏「自定义提供商」按钮打开的**管理弹窗**，以及它转发出去的三项操作。
 *
 * ── 入口的演变（为什么从组头搬到这里）────────────────────────────
 * 这个入口最初挂在账号表 custom 组的组头行上（「模型管理 / 编辑 / 删除」）。
 * 用户随后要求账号列表**不要分组**（组头行整个下线，见 accounts-view.js 的
 * render 说明），组头没了，入口必须另找落点。最终形态是工具栏一颗按钮 →
 * 一个管理弹窗（列出全部自定义提供商，每行三项操作）：
 *   · 入口位置与「添加账号」同处一排，是账号页唯一的提供商级操作区；
 *   · 弹窗里的列表天然覆盖「零账号的提供商」（建了提供商但账号被删光的
 *     场景）—— 组头方案当年正是为了这个可达性才补 orphanCustomGroupHeads，
 *     管理弹窗把这个问题一次性解决：列表读目录缓存，与账号存在与否无关。
 *
 * ── 三项操作各自的本体 ─────────────────────────────────────────
 *   · 模型管理 —— 只转发：弹窗本体在 custom-models-modal.js（全局
 *                 wbCustomModelsModal），本文件不掺和弹窗内部的事；
 *   · 编辑     —— 动态弹一个小表单（名称 / 协议 / Base URL，复用添加表单的字段
 *                 与协议选项，**不含 API Key** —— key 是账号的属性，不在提供商
 *                 记录里，改它走账号自己的「设置」），提交 /api/custom-providers/update；
 *   · 删除     —— wbConfirm 二次确认（文案说明将级联删除该家 N 个账号），确认后
 *                 调 /api/custom-providers/remove，成功后刷新账号列表与目录。
 *
 * ── handleAction 为什么保留这个名字与形状 ──────────────────────
 * 三项操作的实现一行未改（当年为组头按钮写的），管理弹窗只换了一个**调用点**；
 * 函数名与 (action, providerId) 的签名保持原样，于是将来任何新的入口
 * （右键菜单、快捷键、命令行）都能以同一形状接进来，不必再动本文件。
 *
 * 目录数据（customList / refreshCustom / customRequest / PROTOCOL_OPTIONS）全部
 * 来自 providers.js —— 自定义提供商的前端取数只有那一处实现。
 * 编辑弹窗与管理弹窗都按需创建、关闭即移除（与 ⋯ 菜单的「点击才插入 DOM」
 * 同一手法，不必往 index.html 里常驻空弹窗）。
 */
(() => {
  const { esc, toast } = wbApp;
  const providers = window.wbProviders;
  const $ = id => document.getElementById(id);

  const MANAGE_MODAL_ID = 'custom-providers-manage-modal';
  const EDIT_MODAL_ID = 'custom-provider-edit-modal';
  /** 展示名长度上限（与后端 custom_providers::MAX_NAME_CHARS 一致，前端先挡一次） */
  const MAX_NAME_CHARS = 64;

  /** custom- 前缀判据（与后端 ID_PREFIX 一致；管理弹窗的列表口径同源） */
  function isCustomProviderId(id) {
    return typeof id === 'string' && id.startsWith('custom-');
  }

  /** 该提供商此刻名下的账号数（读主状态的全量列表，不跟筛选走） */
  function accountCountOf(providerId) {
    const accounts = wbApp.getState?.()?.accounts?.accounts || [];
    return accounts.filter(account => account?.provider === providerId).length;
  }

  // ─── 管理弹窗 ──────────────────────────────

  function closeManageModal() {
    $(MANAGE_MODAL_ID)?.remove();
  }

  /** 弹窗里的列表体：每行 = 名称 + 协议 + Base URL + 账号数 + 三项操作 */
  function manageListHtml(list) {
    if (!list.length) {
      return '<div class="empty">还没有自定义提供商。点「添加账号」→ 选「自定义提供商」即可创建。</div>';
    }
    return `<div class="cp-list">${list.map(provider => {
      const id = provider.id || '';
      const name = provider.name || id;
      const protocol = (providers.PROTOCOL_OPTIONS || [])
        .find(option => option.value === provider.protocol)?.label || provider.protocol || '';
      const count = accountCountOf(id);
      return `<div class="cp-item" data-provider-id="${esc(id)}">
          <div class="cp-main">
            <div class="cp-name">${esc(name)}</div>
            <div class="cp-sub">
              <span class="cp-badge">${esc(protocol)}</span>
              <span class="cp-url" title="${esc(provider.baseUrl || '')}">${esc(provider.baseUrl || '')}</span>
              <span class="cp-count">${count} 个账号</span>
            </div>
          </div>
          <div class="cp-actions">
            <button type="button" data-cp-action="models" title="管理该提供商的模型清单与映射">模型管理</button>
            <button type="button" data-cp-action="edit" title="修改名称 / 协议 / Base URL">编辑</button>
            <button type="button" class="danger" data-cp-action="remove"
              title="删除该提供商及其名下全部账号">删除</button>
          </div>
        </div>`;
    }).join('')}</div>`;
  }

  /** 重画列表（数据变了一次就整体重画：条目数是个位数，不值得做局部更新） */
  function paintManageList() {
    const host = $(MANAGE_MODAL_ID)?.querySelector('.cp-list-host');
    if (!host) return;
    host.innerHTML = manageListHtml(providers.customList() || []);
  }

  /**
   * 打开管理弹窗：先用缓存画一屏，再拉一次目录补齐（刚建 / 刚被别处改过的
   * 条目要出现或消失）。`wbApp.refresh()` 不在这里跑 —— 列表只读本地目录与
   * 主状态，账号数由主状态现算，改完某一项后**由那项操作自己**刷新（见
   * confirmRemove / saveEdit 的收尾）。
   */
  function openManageModal() {
    closeManageModal();
    const mask = document.createElement('div');
    mask.id = MANAGE_MODAL_ID;
    mask.className = 'modal-mask open';
    mask.setAttribute('role', 'dialog');
    mask.setAttribute('aria-modal', 'true');
    mask.setAttribute('aria-labelledby', 'cp-manage-title');
    mask.innerHTML = `<div class="modal">
        <div class="modal-head">
          <h2 id="cp-manage-title">自定义提供商</h2>
          <button type="button" id="cp-manage-close" title="关闭">✕</button>
        </div>
        <div class="modal-body">
          <div class="modal-section">
            <p>自定义提供商是运行期接入的上游（OpenAI / Anthropic 兼容协议）。
              每家的「模型管理」控制它向下游暴露哪些模型与映射；删除会级联清掉名下全部账号。</p>
            <div class="cp-list-host"></div>
          </div>
        </div>
        <div class="modal-foot">
          <span class="detail">添加新提供商：「添加账号」弹窗里选「自定义提供商」</span>
          <div class="spacer"></div>
          <button type="button" id="cp-manage-done" class="primary">完成</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    // 点遮罩空白处 = 关闭（与其它弹窗同一交互），点弹窗本体不关
    mask.addEventListener('click', event => {
      if (event.target === mask) closeManageModal();
    });
    $('cp-manage-close')?.addEventListener('click', closeManageModal);
    $('cp-manage-done')?.addEventListener('click', closeManageModal);

    // 列表内的事件委托：三项操作全部转发到 handleAction（见文件头）
    mask.addEventListener('click', event => {
      const button = event.target.closest('button[data-cp-action]');
      if (!button) return;
      const providerId = button.closest('.cp-item')?.dataset.providerId || '';
      void handleAction(button.dataset.cpAction, providerId);
    });

    paintManageList();
    // 目录可能还没拉到 / 已被别处改过：补拉一次后重画。刚建的空目录拉到数据
    // 也是走同一条路（paintManageList 读的是刷新后的 customList）
    void providers.refreshCustom?.().then(paintManageList);
  }

  /**
   * 一项操作完成后的收尾：重画管理弹窗的列表（账号数 / 名称可能已变），
   * 并刷新账号列表（自定义家的账号行、筛选器计数都依赖主状态）。
   * 弹窗没开（操作是从别处发起的）时只做账号刷新 —— 不必凭空造一个弹窗。
   */
  async function afterAction() {
    void providers.refreshCustom();
    await wbApp.refresh?.();
    if ($(MANAGE_MODAL_ID)) paintManageList();
  }

  /**
   * 操作入口。providerId 从管理弹窗的行数据来，正常情况下必然存在；
   * 数组索引错的极端情况由各分支自己的「找不到」提示兜住。
   */
  async function handleAction(action, providerId) {
    if (!isCustomProviderId(providerId)) return;
    if (action === 'models') {
      // 打开该提供商的「模型管理」弹窗：本体在 custom-models-modal.js（全局
      // wbCustomModelsModal，弹窗按需创建、关闭即移除），这里只转发 ——
      // 与「弹窗各自成文件」的分工一致，本文件不掺和它的内部实现。
      void window.wbCustomModelsModal?.open?.(providerId);
      return;
    }
    if (action === 'edit') {
      await openEditDialog(providerId);
      return;
    }
    if (action === 'remove') {
      await confirmRemove(providerId);
    }
  }

  /**
   * 取一条提供商记录：先读目录缓存，没有（缓存还没拉到 / 刚被别人改过）就
   * 现拉一次 —— 返回 null 时调用方给一句「可能已被删除」的提示。
   */
  async function findProvider(providerId) {
    const cached = (providers.customList() || []).find(item => item.id === providerId);
    if (cached) return cached;
    const list = await providers.refreshCustom();
    return (list || []).find(item => item.id === providerId) || null;
  }

  // ─── 编辑弹窗 ──────────────────────────────

  function closeEditDialog() {
    $(EDIT_MODAL_ID)?.remove();
  }

  /** 按当前记录拼出编辑弹窗（协议下拉预选当前值，其余字段预填现值） */
  async function openEditDialog(providerId) {
    const provider = await findProvider(providerId);
    if (!provider) {
      toast('该自定义提供商已不存在（可能已被删除），请刷新列表后重试', 'err');
      return;
    }
    closeEditDialog();
    const protocolOptions = (providers.PROTOCOL_OPTIONS || []).map(option =>
      `<option value="${esc(option.value)}"${option.value === provider.protocol ? ' selected' : ''}>${esc(option.label)}</option>`).join('');
    const mask = document.createElement('div');
    mask.id = EDIT_MODAL_ID;
    mask.className = 'modal-mask open';
    mask.setAttribute('role', 'dialog');
    mask.setAttribute('aria-modal', 'true');
    mask.setAttribute('aria-labelledby', 'custom-edit-title');
    mask.innerHTML = `<div class="modal">
        <div class="modal-head">
          <h2 id="custom-edit-title">编辑自定义提供商</h2>
          <button type="button" id="custom-edit-close" title="关闭">✕</button>
        </div>
        <div class="modal-body">
          <div class="modal-section">
            <h3>提供商配置</h3>
            <p>改协议 / Base URL 会改变<strong>该提供商名下全部账号</strong>的转发方式，正在进行的请求可能失败。</p>
            <div class="field-row">
              <label for="custom-edit-name">名称（必填）</label>
              <input id="custom-edit-name" type="text" maxlength="${MAX_NAME_CHARS}" value="${esc(provider.name || '')}"
                placeholder="提供商显示名，1~64 个字符">
            </div>
            <div class="field-row">
              <label for="custom-edit-protocol">协议</label>
              <select id="custom-edit-protocol" class="custom-provider-select">${protocolOptions}</select>
            </div>
            <div class="field-row">
              <label for="custom-edit-baseurl">Base URL（必填）</label>
              <input id="custom-edit-baseurl" type="text" value="${esc(provider.baseUrl || '')}"
                placeholder="OpenAI 兼容填到 /v1；Anthropic 填根地址">
            </div>
          </div>
        </div>
        <div class="modal-foot">
          <span class="detail" id="custom-edit-hint"></span>
          <div class="spacer"></div>
          <button type="button" id="custom-edit-cancel">取消</button>
          <button type="button" id="custom-edit-save" class="primary">保存</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    $('custom-edit-close')?.addEventListener('click', closeEditDialog);
    $('custom-edit-cancel')?.addEventListener('click', closeEditDialog);
    // 点遮罩空白处 = 关闭（与其它弹窗同一交互），点弹窗本体不关
    mask.addEventListener('click', event => {
      if (event.target === mask) closeEditDialog();
    });
    $('custom-edit-save')?.addEventListener('click', () => { void saveEdit(provider); });
    $('custom-edit-name')?.focus();
  }

  /** 保存编辑：POST /api/custom-providers/update（三个字段一起提交，都是表单上的必填值） */
  async function saveEdit(provider) {
    const name = ($('custom-edit-name')?.value || '').trim();
    const protocol = $('custom-edit-protocol')?.value || '';
    const baseUrl = ($('custom-edit-baseurl')?.value || '').trim();
    if (!name) { toast('请填写名称', 'err'); return; }
    if (!baseUrl) { toast('请填写 Base URL', 'err'); return; }
    const save = $('custom-edit-save');
    if (save) { save.disabled = true; save.textContent = '保存中…'; }
    try {
      await providers.customRequest('POST', '/api/custom-providers/update', {
        id: provider.id, name, protocol, baseUrl,
      });
      closeEditDialog();
      toast(`✅ 自定义提供商「${name}」已更新`);
      // 目录先刷（管理弹窗列表 / 筛选器的展示名都读它），再全量刷新账号列表补一次重绘
      await afterAction();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast(`保存失败：${message}`, 'err');
      // 后端 400 的文案写进脚注留一份（toast 几秒后就没了）
      const hint = $('custom-edit-hint');
      if (hint) hint.textContent = message;
      if (save) { save.disabled = false; save.textContent = '保存'; }
    }
  }

  // ─── 删除提供商 ────────────────────────────

  /**
   * 删除自定义提供商：二次确认 → POST /api/custom-providers/remove。
   *
   * N 用**该家名下账号数**现算（与列表行上的「N 个账号」同源：同一份账号列表
   * 按 provider 过滤）—— 确认文案必须先把「会连带删掉多少条」说清楚，这是
   * 级联删除与单条、可逆操作的分界。响应里的 accountsRemoved 是权威值，
   * 成功提示用它（万一与本地计数不一致，以删掉的真实条数为准）。
   */
  async function confirmRemove(providerId) {
    const provider = await findProvider(providerId);
    if (!provider) {
      toast('该自定义提供商已不存在（可能已被删除），请刷新列表后重试', 'err');
      return;
    }
    const name = provider.name || providerId;
    const count = accountCountOf(providerId);
    // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行，危险确认一律走 wbConfirm
    const ok = await window.wbConfirm?.ask?.({
      title: '删除自定义提供商',
      html: `确定删除自定义提供商「<strong>${esc(name)}</strong>」？`
        + `将同时删除该提供商下 <strong>${count}</strong> 个账号，删除后无法恢复。`,
      okText: '删除',
      okClass: 'danger',
    });
    if (!ok) return;
    try {
      const data = await providers.customRequest('POST', '/api/custom-providers/remove', { id: providerId });
      const removed = Number(data?.accountsRemoved);
      toast(`✅ 已删除自定义提供商「${name}」${Number.isFinite(removed) ? `及 ${removed} 个账号` : ''}`);
      await afterAction();
    } catch (error) {
      toast(`删除失败：${error instanceof Error ? error.message : String(error)}`, 'err');
    }
  }

  // ─── 挂载 ─────────────────────────────────

  // 工具栏按钮（账号页）—— 入口的唯一落点，见文件头「入口的演变」。
  // 按钮在 index.html 里常驻，这里加载期就绑上（accounts-view.js 的工具栏
  // 绑定只管批量与查询那几颗，本文件的按钮不归它管）。
  $('btn-manage-custom-providers')?.addEventListener('click', openManageModal);

  // Esc = 关闭最上层的那一个（编辑弹窗优先：它在管理弹窗之上）；两个都没开时
  // 什么都不做，与其它弹窗的 Esc 互不干扰
  document.addEventListener('keydown', event => {
    if (event.key !== 'Escape') return;
    if ($(EDIT_MODAL_ID)) { closeEditDialog(); return; }
    if ($(MANAGE_MODAL_ID)) closeManageModal();
  });

  window.wbCustomProvidersUi = { handleAction, isCustomProviderId, openManageModal };
})();
