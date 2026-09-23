/* Agent2API · 「登录 / 添加账号」弹窗：自定义提供商（新建 / 加入已有） */
/* global wbApp */

/**
 * 自定义提供商（custom- 前缀，运行期数据）的添加表单。
 *
 * ── 为什么独立成文件、走「后注册口」──────────────────────────
 * add-provider-forms.js 已 1000+ 行，且它的 ADD_FORMS 模型是「一份字段配置 +
 * 统一提交 POST /api/accounts」；自定义提供商却有两种添加方式（新建提供商 +
 * 首个账号 → POST /api/custom-providers；往已有提供商再加账号 → POST
 * /api/accounts），字段与端点都成对出现，硬塞进那份模型要给每个环节开特例。
 * 因此表单整体自治在本文件，通过 add-provider-forms.js 暴露的
 * registerAddForm 把块挂进弹窗（本文件按 index.html 约定排在其后加载）。
 *
 * 表单结构（块 id = add-block-custom）：
 *   · 添加方式分段：新建提供商 / 选择已有提供商 —— **只有已存在自定义提供商
 *     时才显示**（一家都没有时「选择已有」无处可选，显示它只会让人点进去看空表单）；
 *   · 新建模式：名称（必填，1~64 字符，与后端 MAX_NAME_CHARS 一致）、协议
 *     （三选一下拉）、Base URL（必填）、API Key（可选密码框，留空 = 无鉴权上游）；
 *   · 已有模式：提供商下拉（customList 的 name）、API Key（可选）、备注名（可选）。
 *
 * 依赖 wbApp、wbAccountAddForms（后注册口与 .seg 工具）、wbProviders
 * （自定义目录：customList / refreshCustom / customRequest）与已解析的弹窗 DOM。
 */
(() => {
  const { esc, toast } = wbApp;
  const forms = window.wbAccountAddForms;
  const providers = window.wbProviders;
  const $ = id => document.getElementById(id);

  // 脚本顺序被改坏时尽早暴露（registerAddForm 挂不上，入口永远出不来），
  // 比静默缺一个提供商选项好定位
  if (!forms?.registerAddForm || !providers?.customRequest) {
    console.warn('[CustomProvider] add-provider-forms.js / providers.js 未就绪，自定义提供商添加入口未注册');
    return;
  }

  /** 协议下拉的三个选项：值与后端 PROTOCOLS 逐字一致，选项定义收在 providers.js
   *  （添加表单与编辑弹窗共用一份），这里只留兜底 —— 目录模块没加载出来时
   *  下拉仍然可用（此时提交会在运行时层面失败，但表单不至于整块空白） */
  const PROTOCOL_OPTIONS = providers.PROTOCOL_OPTIONS || [
    { value: 'chat_completions', label: 'OpenAI - Chat Completions' },
    { value: 'responses', label: 'OpenAI - Responses' },
    { value: 'anthropic', label: 'Anthropic - Messages' },
  ];
  /** 展示名长度上限（与后端 custom_providers::MAX_NAME_CHARS 一致，前端先挡一次） */
  const MAX_NAME_CHARS = 64;
  /** 账号备注名长度上限（与 add-provider-forms 的 MAX_NAME_LENGTH 同一口径） */
  const MAX_ACCOUNT_NAME_CHARS = 100;

  /** 当前添加方式：'create' = 新建提供商；'existing' = 加入已有提供商 */
  let mode = 'create';
  /** 提交互斥锁：弹窗内的提交不占用账号列表的 busy 锁（与 addBusy 同一取向） */
  let submitBusy = false;

  // ─── 块结构 ────────────────────────────────

  /** 协议下拉的选项串（值 / 文案都是常量，无需转义，仍走 esc 保持同一习惯） */
  const protocolOptionsHtml = PROTOCOL_OPTIONS.map((option, index) =>
    `<option value="${esc(option.value)}"${index ? '' : ' selected'}>${esc(option.label)}</option>`).join('');

  function buildBlock() {
    // 「选择已有」模式默认隐藏：有没有可选项由 syncModeUi 按 customList 决定
    return `<div class="modal-section" id="custom-mode-section">
        <h3>添加方式</h3>
        <div class="seg add-seg" id="custom-mode-seg" role="radiogroup" aria-label="自定义提供商的添加方式">
          <button type="button" class="seg-item active" data-value="create" role="radio"
            aria-checked="true" tabindex="0">新建提供商</button>
          <button type="button" class="seg-item" data-value="existing" role="radio"
            aria-checked="false" tabindex="-1">选择已有提供商</button>
        </div>
      </div>

      <div class="modal-section" id="custom-create-block">
        <h3>新建自定义提供商</h3>
        <p>把一个 OpenAI / Anthropic 兼容的上游接进网关：名称用于在账号列表里分组显示，
          协议决定请求按哪种格式转发，Base URL 是上游的服务地址。创建时会同时建立该家的第一个账号。</p>
        <div class="field-row">
          <label for="custom-name-input">名称（必填）</label>
          <input id="custom-name-input" type="text" maxlength="${MAX_NAME_CHARS}"
            placeholder="提供商显示名，1~64 个字符">
        </div>
        <div class="field-row">
          <label for="custom-protocol-select">协议</label>
          <select id="custom-protocol-select" class="custom-provider-select">${protocolOptionsHtml}</select>
        </div>
        <div class="field-row">
          <label for="custom-baseurl-input">Base URL（必填）</label>
          <input id="custom-baseurl-input" type="text"
            placeholder="OpenAI 兼容填到 /v1；Anthropic 填根地址">
        </div>
        <div class="field-row">
          <label for="custom-apikey-input">API Key</label>
          <input id="custom-apikey-input" type="password" autocomplete="new-password"
            placeholder="可选，留空表示无鉴权上游">
        </div>
        <div class="field-row">
          <button id="custom-create-button" class="primary">创建并添加账号</button>
          <span class="detail" id="custom-create-hint"></span>
        </div>
      </div>

      <div class="modal-section" id="custom-existing-block" hidden>
        <h3>添加到已有自定义提供商</h3>
        <p>为已创建的自定义提供商再加一个账号：同一上游可以放多把 key，按优先级轮换。</p>
        <div class="field-row">
          <label for="custom-existing-select">提供商</label>
          <select id="custom-existing-select" class="custom-provider-select"></select>
        </div>
        <div class="field-row">
          <label for="custom-existing-apikey-input">API Key</label>
          <input id="custom-existing-apikey-input" type="password" autocomplete="new-password"
            placeholder="可选，留空表示无鉴权上游">
        </div>
        <div class="field-row">
          <label for="custom-existing-name-input">备注名</label>
          <input id="custom-existing-name-input" type="text" maxlength="${MAX_ACCOUNT_NAME_CHARS}"
            placeholder="可选，留空则用提供商名称">
        </div>
        <div class="field-row">
          <button id="custom-existing-button" class="primary">添加账号</button>
          <span class="detail" id="custom-existing-hint"></span>
        </div>
      </div>`;
  }

  // ─── 模式显隐与已有提供商下拉 ────────────────

  /** 选中哪种方式只显示哪一段（与 syncProviderMethod 同一手法，只切显隐） */
  function syncModeVisibility() {
    const create = $('custom-create-block');
    const existing = $('custom-existing-block');
    if (create) create.hidden = mode !== 'create';
    if (existing) existing.hidden = mode !== 'existing';
  }

  /** 按当前列表重画「选择已有」的下拉（保留原选中项；空了退回第一项） */
  function syncExistingSelect() {
    const select = $('custom-existing-select');
    if (!select) return;
    const list = providers.customList() || [];
    const previous = select.value;
    select.innerHTML = '';
    for (const item of list) {
      const option = document.createElement('option');
      option.value = item.id;
      // textContent 赋值即转义：提供商名是用户输入，不能拼进 HTML
      option.textContent = item.name || item.id;
      select.appendChild(option);
    }
    // 之前选中的家还在（列表刷新前后通常一致）就保持；不在了退回第一项
    select.value = list.some(item => item.id === previous) ? previous : (list[0]?.id || '');
  }

  /**
   * 按「有没有已存在的自定义提供商」同步整个块的可用形态：
   *   · 没有任何提供商：整段「添加方式」收起（无处可选），方式钉死为新建；
   *   · 有：分段控件显示，两种方式都可用。
   * 列表是异步的：onShow 先按缓存画一次，refreshCustom 回来后再同步一次，
   * 前后两次调用的开销都可忽略（纯 DOM 显隐 + 下拉重建）。
   */
  function syncModeUi() {
    const hasExisting = (providers.customList() || []).length > 0;
    const section = $('custom-mode-section');
    if (section) section.hidden = !hasExisting;
    if (!hasExisting) mode = 'create';
    // 分段选中态与 mode 对齐（mode 被钉死成 create 时也要落回那个按钮）
    forms.setSegValue?.($('custom-mode-seg'), mode);
    syncModeVisibility();
    syncExistingSelect();
  }

  // ─── 提交 ─────────────────────────────────

  /** 提交按钮的忙态包装（与 add-provider-forms 的 runAdd 同一形制）。
   *  上一次的错误提示在**开始时**清掉（失败写进的文案要留到用户下次尝试，
   *  不能在 finally 里清 —— 那会把它刚写进去的错误立刻抹掉） */
  async function runSubmit(button, hintId, task) {
    if (submitBusy) return;
    submitBusy = true;
    const hint = $(hintId);
    if (hint) hint.textContent = '';
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '提交中…'; }
    try {
      await task();
    } finally {
      submitBusy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
  }

  /** 失败提示：toast（与其它表单同一模式）+ 写进 hint（toast 3.5 秒后就没了） */
  function showSubmitError(hintId, error) {
    const message = error instanceof Error ? error.message : String(error);
    toast(`添加失败：${message}`, 'err');
    const hint = $(hintId);
    if (hint) hint.textContent = message;
  }

  /** 添加成功后的统一收尾：关弹窗、刷新自定义目录与账号列表、提示（同 afterAdd） */
  async function afterCustomAdd(message) {
    $('add-modal')?.classList.remove('open');
    // 目录先刷：账号行 / 筛选器显示的提供商名都来自 wbProviders 的缓存
    void providers.refreshCustom();
    await wbApp.refresh?.();
    toast(message);
  }

  /** 清空一种方式的表单（成功后调用；失败保留内容方便改动重试） */
  function clearForm(ids) {
    for (const id of ids) {
      const node = $(id);
      if (node) node.value = '';
    }
  }

  /** 新建模式：POST /api/custom-providers（提供商 + 首个账号一次建成） */
  function submitCreate() {
    const name = ($('custom-name-input')?.value || '').trim();
    const protocol = $('custom-protocol-select')?.value || '';
    const baseUrl = ($('custom-baseurl-input')?.value || '').trim();
    const apiKey = ($('custom-apikey-input')?.value || '').trim();
    // 必填拦截在本地先做一次（弹窗不是 <form>，原生 required 不生效）
    if (!name) { toast('请填写名称', 'err'); return; }
    if (!baseUrl) { toast('请填写 Base URL', 'err'); return; }
    const button = $('custom-create-button');
    void runSubmit(button, 'custom-create-hint', async () => {
      const payload = { name, protocol, baseUrl };
      if (apiKey) payload.apiKey = apiKey; // 留空 = 无鉴权上游，不进请求体
      try {
        const data = await providers.customRequest('POST', '/api/custom-providers', payload);
        clearForm(['custom-name-input', 'custom-baseurl-input', 'custom-apikey-input']);
        const created = data?.provider?.name || name;
        await afterCustomAdd(`✅ 已创建自定义提供商「${created}」并添加账号`);
      } catch (error) {
        showSubmitError('custom-create-hint', error);
      }
    });
  }

  /** 已有模式：POST /api/accounts（custom 账号走 provider = custom-xxx 分支） */
  function submitExisting() {
    const providerId = $('custom-existing-select')?.value || '';
    if (!providerId) { toast('请先选择一个自定义提供商', 'err'); return; }
    const apiKey = ($('custom-existing-apikey-input')?.value || '').trim();
    const name = ($('custom-existing-name-input')?.value || '').trim();
    const button = $('custom-existing-button');
    void runSubmit(button, 'custom-existing-hint', async () => {
      const payload = { provider: providerId };
      if (apiKey) payload.apiKey = apiKey;
      if (name) payload.name = name;
      try {
        const data = await providers.customRequest('POST', '/api/accounts', payload);
        clearForm(['custom-existing-apikey-input', 'custom-existing-name-input']);
        const label = data?.account?.name || providers.customList()?.find(item => item.id === providerId)?.name || '';
        await afterCustomAdd(`✅ 账号已添加${label ? `：${label}` : ''}`);
      } catch (error) {
        showSubmitError('custom-existing-hint', error);
      }
    });
  }

  // ─── 挂载 ─────────────────────────────────

  providers.refreshCustom(); // 提前拉一次目录：第一次打开弹窗时下拉就有数据

  forms.registerAddForm({
    provider: 'custom',
    label: '自定义提供商',
    buildBlock,
    mount() {
      // 「添加方式」分段复用 add-provider-forms 的同一套 .seg 交互
      const seg = $('custom-mode-seg');
      forms.bindSeg?.(seg);
      seg?.addEventListener(forms.SEG_EVENT, () => {
        mode = forms.segValueOf?.(seg) || 'create';
        syncModeVisibility();
      });
      $('custom-create-button')?.addEventListener('click', submitCreate);
      $('custom-existing-button')?.addEventListener('click', submitExisting);
    },
    /** 弹窗切到自定义提供商：按缓存先画，再拉一次目录补齐（新建的提供商要出现在下拉里） */
    onShow() {
      syncModeUi();
      void providers.refreshCustom().then(syncModeUi);
    },
  });
})();
