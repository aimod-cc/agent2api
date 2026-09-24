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
 * ── 两种添加方式怎么选 ────────────────────────────────────
 * **不由本块自己问**。第 1 步点的是「新建自定义提供商」那张卡还是一家已有提供商的
 * 卡片，本身就已经回答了「给谁加账号」：前者 hint 为空 → 新建，后者 hint 带着那家
 * 的 id → 加入已有并预选它。原来块内还有一条「添加方式」分段让用户再选一遍，是
 * 重复劳动；要改主意点头部的 ‹ 回第 1 步重选。于是这里只剩「按 context 落到哪种
 * 模式」，没有模式切换控件，也就没有模式状态需要在两处同步。
 *
 * 表单结构：
 *   · 新建模式（#custom-create-block）：名称（必填，1~64 字符，与后端
 *     MAX_NAME_CHARS 一致）、协议（三选一下拉）、Base URL（必填）、API Key
 *     （可选密码框，留空 = 无鉴权上游）；
 *   · 已有模式（#custom-existing-block）：提供商下拉（customList 的 name）、
 *     API Key（可选）、备注名（可选）。
 *   两张卡只显一张 —— 一次只回答一个问题，也避免「卡片套卡片」的层叠观感。
 *
 * 字段行走 .add-field 网格（标签列定宽右对齐，见 page-accounts-providers.css）：
 * 标签自然宽度会让四个控件的左边缘四段锯齿，定宽后全部落在同一条竖线上。
 * 帮助文字写成控件下方的常驻 .hint，不用 placeholder —— 后者一输入就没了。
 *
 * 主按钮不在字段流里，而是搬进弹窗底部的操作条（#add-foot，见
 * add-provider-forms.js 的 mountAddProviderUi）：留在字段流里它会和输入框同一个
 * 节奏、主次不分，底部条同时给失败提示一个固定位置。
 *
 * 依赖 wbApp、wbAccountAddForms（后注册口）与 wbProviders（自定义目录：
 * customList / refreshCustom / customRequest）与已解析的弹窗 DOM。
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

  /** Base URL 的两种填法：Anthropic 走根地址，OpenAI 兼容要带 /v1（后端按协议拼路径） */
  const BASE_HINT_OPENAI = 'OpenAI 兼容填到 /v1；Anthropic 填根地址';
  const BASE_HINT_ANTHROPIC = 'Anthropic 协议填根地址，不要带 /v1';
  const BASE_PLACEHOLDER_OPENAI = 'https://open.bigmodel.cn/api/paas/v4';
  const BASE_PLACEHOLDER_ANTHROPIC = 'https://api.anthropic.com';

  /** 当前添加方式：'create' = 新建提供商；'existing' = 加入已有提供商。
   *  由 onShow 按第 1 步点的那张卡定死，块内没有切换控件。 */
  let mode = 'create';
  /**
   * 本次进入新建模式所依据的预置项（wbPresetProviders 的目录条目，从预置卡
   * 进来才有）。名称 / 协议 / Base URL 预填进表单（用户可改），而 quirks
   * （上游特判）不进表单 —— 提交时原样随记录写入，见 submitCreate。
   * 手动新建 / 加入已有模式下为 null。
   */
  let activePreset = null;
  /**
   * 待选中的已有提供商 id：第 1 步点的是**某一家**自定义提供商的卡片，上下文里就带着
   * 那家的 id（见 add-provider-forms.js 的 pickProvider）。落到这里直接预选它 ——
   * 用户点的就是「给这家加账号」，不该再让他自己找一遍。
   * 下拉里还没有这一项（目录尚未拉到 / 已被删除）时保留标记，等下一次重建再试。
   */
  let pendingPick = '';
  /** 提交互斥锁：弹窗内的提交不占用账号列表的 busy 锁（与 addBusy 同一取向） */
  let submitBusy = false;

  /**
   * 底部操作条上的主按钮：一颗模式一颗，都挂在同一个容器里，按当前模式显隐。
   * 文案与提交函数成对写在一处，加一种模式只加一条。
   */
  const FOOT_ACTIONS = {
    create: { id: 'custom-create-button', text: '创建并添加账号', submit: () => submitCreate() },
    existing: { id: 'custom-existing-button', text: '添加账号', submit: () => submitExisting() },
  };
  /**
   * 「删除此提供商」按钮的 id：排在「添加账号」左边，只在「加入已有」模式出现。
   * 不在 FOOT_ACTIONS 里 —— 那张表的每一项都是「提交」，而这是一条危险操作：
   * 显隐条件、文案与点击处理都不一样，混进同一张表会让「提交」这个词失真。
   */
  const REMOVE_BUTTON_ID = 'custom-existing-remove';

  // ─── 块结构 ────────────────────────────────

  /** 协议下拉的选项串（值 / 文案都是常量，无需转义，仍走 esc 保持同一习惯） */
  const protocolOptionsHtml = PROTOCOL_OPTIONS.map((option, index) =>
    `<option value="${esc(option.value)}"${index ? '' : ' selected'}>${esc(option.label)}</option>`).join('');

  /** 必填标记：只标星号，不把「（必填）」塞进标签 —— 后者会让四行标签的字重与
   *  长度都不齐，星号配 aria-required 才是表单的通行写法（弹窗不是 <form>，
   *  原生 required 不生效，真正的拦截在 submitCreate） */
  const requiredMark = '<i class="req" aria-hidden="true">*</i>';

  function buildBlock() {
    return `<div class="modal-section" id="custom-create-block">
        <div class="add-panel-head">
          <h3>上游信息</h3>
          <span>创建这个提供商，并同时建立它的第一个账号。</span>
        </div>
        <div class="add-form">
          <div class="add-field">
            <label for="custom-name-input">名称${requiredMark}</label>
            <input id="custom-name-input" type="text" maxlength="${MAX_NAME_CHARS}"
              aria-required="true" placeholder="如：智谱 GLM">
            <span class="hint">1~64 个字符，账号列表里按它分组显示</span>
          </div>
          <div class="add-field">
            <label for="custom-protocol-select">协议</label>
            <select id="custom-protocol-select" class="custom-provider-select">${protocolOptionsHtml}</select>
          </div>
          <div class="add-field">
            <label for="custom-baseurl-input">Base URL${requiredMark}</label>
            <input id="custom-baseurl-input" type="text" aria-required="true"
              placeholder="${BASE_PLACEHOLDER_OPENAI}">
            <span class="hint" id="custom-baseurl-hint">${BASE_HINT_OPENAI}</span>
          </div>
          <div class="add-field">
            <label for="custom-apikey-input">API Key</label>
            <input id="custom-apikey-input" type="password" autocomplete="new-password"
              placeholder="sk-…">
            <span class="hint">留空表示无鉴权上游</span>
          </div>
        </div>
      </div>

      <div class="modal-section" id="custom-existing-block" hidden>
        <div class="add-panel-head">
          <h3>账号信息</h3>
          <span>添加到 <b id="custom-existing-picked"></b>　同一家可以放多把 key，按优先级轮换。</span>
        </div>
        <div class="add-form">
          <div class="add-field">
            <label for="custom-existing-select">提供商</label>
            <select id="custom-existing-select" class="custom-provider-select"></select>
          </div>
          <div class="add-field">
            <label for="custom-existing-apikey-input">API Key</label>
            <input id="custom-existing-apikey-input" type="password" autocomplete="new-password"
              placeholder="sk-…">
            <span class="hint">留空表示无鉴权上游</span>
          </div>
          <div class="add-field">
            <label for="custom-existing-name-input">备注名</label>
            <input id="custom-existing-name-input" type="text" maxlength="${MAX_ACCOUNT_NAME_CHARS}"
              placeholder="可选">
            <span class="hint">留空则用提供商名称</span>
          </div>
        </div>
      </div>`;
  }

  // ─── 模式显隐与已有提供商下拉 ────────────────

  /** 选中哪种方式只显示哪一段（连同底部条上对应的那颗按钮） */
  function syncModeVisibility() {
    const create = $('custom-create-block');
    const existing = $('custom-existing-block');
    if (create) create.hidden = mode !== 'create';
    if (existing) existing.hidden = mode !== 'existing';
    for (const [key, spec] of Object.entries(FOOT_ACTIONS)) {
      const button = $(spec.id);
      if (button) button.hidden = key !== mode;
    }
    // 「删除此提供商」与「加入已有」模式同显隐：新建 / 预置预填模式没有可删的对象
    const remove = $(REMOVE_BUTTON_ID);
    if (remove) remove.hidden = mode !== 'existing';
  }

  /** 按当前列表重画「选择已有」的下拉（保留原选中项；空了退回第一项），
   *  并把选中的家名同步到面板副标题 —— 那是「正在给谁加账号」的唯一读数 */
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
    // 之前选中的家还在（列表刷新前后通常一致）就保持；不在了退回第一项。
    // 待选中的那家（带着上下文进来）优先，选中后清掉标记。
    const wanted = pendingPick && list.some(item => item.id === pendingPick) ? pendingPick : '';
    if (wanted) pendingPick = '';
    const keep = wanted || previous;
    select.value = list.some(item => item.id === keep) ? keep : (list[0]?.id || '');
    const picked = $('custom-existing-picked');
    if (picked) picked.textContent = list.find(item => item.id === select.value)?.name || '该提供商';
  }

  /** 协议换了，Base URL 的填法跟着换：Anthropic 走根地址，OpenAI 兼容要带 /v1。
   *  提示与 placeholder 一起改 —— 两者都在说「这一栏该填成什么样」。 */
  function syncBaseHint() {
    const anthropic = ($('custom-protocol-select')?.value || '') === 'anthropic';
    const hint = $('custom-baseurl-hint');
    const input = $('custom-baseurl-input');
    if (hint) hint.textContent = anthropic ? BASE_HINT_ANTHROPIC : BASE_HINT_OPENAI;
    if (input) input.placeholder = anthropic ? BASE_PLACEHOLDER_ANTHROPIC : BASE_PLACEHOLDER_OPENAI;
  }

  /**
   * 按当前模式同步整个块的可用形态。列表是异步的：onShow 先按缓存画一次，
   * refreshCustom 回来后再同步一次，前后两次调用的开销都可忽略
   * （纯 DOM 显隐 + 下拉重建）。
   */
  function syncModeUi() {
    syncModeVisibility();
    syncExistingSelect();
    syncBaseHint();
    // 底部操作条默认由 add-provider-forms 收起，本块是搬了按钮进来的那一个，点亮它
    const foot = $('add-foot');
    if (foot) foot.hidden = false;
  }

  /** 清空底部操作条的提示位。打开弹窗时清一次；提交失败写进去的那句要留到用户
   *  下次尝试，所以不能在 runSubmit 的 finally 里清 —— 那会把它刚写进去的立刻抹掉 */
  function clearFootHint() {
    const hint = $('add-foot-hint');
    if (hint) { hint.textContent = ''; hint.classList.remove('err'); }
  }

  // ─── 提交 ─────────────────────────────────

  /** 提交按钮的忙态包装（与 add-provider-forms 的 runAdd 同一形制） */
  async function runSubmit(button, task) {
    if (submitBusy) return;
    submitBusy = true;
    clearFootHint();
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '提交中…'; }
    try {
      await task();
    } finally {
      submitBusy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
  }

  /** 失败提示：toast（与其它表单同一模式）+ 写进底部条的提示位（toast 3.5 秒后就没了） */
  function showSubmitError(error) {
    const message = error instanceof Error ? error.message : String(error);
    toast(`添加失败：${message}`, 'err');
    const hint = $('add-foot-hint');
    if (hint) { hint.textContent = message; hint.classList.add('err'); }
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
    const button = $(FOOT_ACTIONS.create.id);
    void runSubmit(button, async () => {
      const payload = { name, protocol, baseUrl };
      // 预置家的上游特判随记录写入（转发层按这些字段修正请求，见
      // preset-providers.js 的 quirks 说明与后端 custom_providers 的字段注释）
      const quirks = activePreset?.quirks || {};
      if (quirks.urlSuffix) payload.urlSuffix = quirks.urlSuffix;
      if (quirks.headers && Object.keys(quirks.headers).length) payload.headers = { ...quirks.headers };
      if (quirks.anthropicToolType) payload.anthropicToolType = quirks.anthropicToolType;
      if (apiKey) payload.apiKey = apiKey; // 留空 = 无鉴权上游，不进请求体
      try {
        const data = await providers.customRequest('POST', '/api/custom-providers', payload);
        clearForm(['custom-name-input', 'custom-baseurl-input', 'custom-apikey-input']);
        const created = data?.provider?.name || name;
        await afterCustomAdd(`✅ 已创建自定义提供商「${created}」并添加账号`);
      } catch (error) {
        showSubmitError(error);
      }
    });
  }

  /** 已有模式：POST /api/accounts（custom 账号走 provider = custom-xxx 分支） */
  function submitExisting() {
    const providerId = $('custom-existing-select')?.value || '';
    if (!providerId) { toast('请先选择一个自定义提供商', 'err'); return; }
    const apiKey = ($('custom-existing-apikey-input')?.value || '').trim();
    const name = ($('custom-existing-name-input')?.value || '').trim();
    const button = $(FOOT_ACTIONS.existing.id);
    void runSubmit(button, async () => {
      const payload = { provider: providerId };
      if (apiKey) payload.apiKey = apiKey;
      if (name) payload.name = name;
      try {
        const data = await providers.customRequest('POST', '/api/accounts', payload);
        clearForm(['custom-existing-apikey-input', 'custom-existing-name-input']);
        const label = data?.account?.name || providers.customList()?.find(item => item.id === providerId)?.name || '';
        await afterCustomAdd(`✅ 账号已添加${label ? `：${label}` : ''}`);
      } catch (error) {
        showSubmitError(error);
      }
    });
  }

  // ─── 删除（「加入已有」模式下的那一家的提供商级删除）───

  /**
   * 删除「加入已有」模式下选中的那一家（级联删账号）。
   *
   * 动作本身全在 `wbCustomProvidersUi.remove` 里 —— 二次确认（说明将级联删掉
   * 多少账号）、POST /api/custom-providers/remove、刷新目录与账号列表都在那边，
   * 与账号设置弹窗里的「删除提供商」共用同一条链；这里只回答两件事：删的是
   * 哪一家（下拉当前值）、删完收什么尾（关弹窗 —— 名下账号连同删光，弹窗里
   * 没有可停留的上下文了）。
   *
   * 与提交动作共用 `submitBusy` 互斥：确认框弹着的时候表单按钮不该还能提交。
   */
  async function removeExistingProvider() {
    const providerId = $('custom-existing-select')?.value || '';
    if (!providerId) { toast('请先选择一个自定义提供商', 'err'); return; }
    const remover = window.wbCustomProvidersUi?.remove;
    if (typeof remover !== 'function') { toast('删除功能不可用（脚本未就绪）', 'err'); return; }
    if (submitBusy) return;
    submitBusy = true;
    try {
      const removed = await remover(providerId);
      if (removed) $('add-modal')?.classList.remove('open');
    } finally {
      submitBusy = false;
    }
  }

  // ─── 挂载 ─────────────────────────────────

  providers.refreshCustom(); // 提前拉一次目录：第一次打开弹窗时下拉就有数据

  forms.registerAddForm({
    provider: 'custom',
    label: '自定义提供商',
    buildBlock,
    mount() {
      // 主按钮搬进弹窗底部的操作条（DOM 已由 mountAddProviderUi 建好：
      // 本文件按 index.html 约定排在 add-provider-forms.js 之后加载）
      const actions = $('add-foot-actions');
      for (const [key, spec] of Object.entries(FOOT_ACTIONS)) {
        const button = document.createElement('button');
        button.type = 'button';
        button.id = spec.id;
        button.className = 'primary';
        button.textContent = spec.text;
        button.hidden = key !== mode;
        button.addEventListener('click', () => spec.submit());
        actions?.appendChild(button);
      }
      // 「删除此提供商」插在「添加账号」左边：危险操作不占主位（右侧那颗是
      // 用户的主路径），但要在同一个上下文里够得着 —— 点的是某一家已建家的
      // 卡片进来，那一屏说的就是「这一家」。样式与账号设置弹窗里的同名动作
      // 一致（.danger + 级联删除说明在确认框里，见 removeExistingProvider）。
      const removeButton = document.createElement('button');
      removeButton.type = 'button';
      removeButton.id = REMOVE_BUTTON_ID;
      removeButton.className = 'danger';
      removeButton.textContent = '删除此提供商';
      removeButton.title = '级联删除名下全部账号，不可恢复';
      removeButton.hidden = mode !== 'existing';
      removeButton.addEventListener('click', () => { void removeExistingProvider(); });
      const existingButton = $(FOOT_ACTIONS.existing.id);
      if (existingButton?.parentElement) {
        existingButton.parentElement.insertBefore(removeButton, existingButton);
      } else {
        actions?.appendChild(removeButton);
      }
      $('custom-protocol-select')?.addEventListener('change', syncBaseHint);
      $('custom-existing-select')?.addEventListener('change', syncExistingSelect);
    },
    /**
     * 弹窗切到自定义块时回调。第 1 步点的是**哪一张卡**就落到哪种模式，不复用上一次：
     *   · 某一家自定义提供商的卡片 → 上下文带那家的 id → 「加入已有」并预选它；
     *   · 预置家卡片 → 上下文带 preset key → 「新建」并预填名称 / 协议 / Base URL；
     *   · 「手动新建」那张卡 → 上下文里没有 id 也没有 preset → 「新建」（空白表单）。
     * 用户点的是哪张卡，就该看到哪种表单。
     */
    onShow(context) {
      const wanted = context?.providerId || '';
      const presetKey = context?.preset || '';
      mode = wanted ? 'existing' : 'create';
      pendingPick = wanted;
      // 预置项整体记下：名称 / 协议 / Base URL 预填进表单，quirks 留给提交时
      // 随记录写入（它们不进表单 —— 是转发层的修正项，不是用户要逐条确认的配置）
      activePreset = !wanted && presetKey
        ? window.wbPresetProviders?.presetOf?.(presetKey) || null
        : null;
      clearFootHint();
      syncModeUi();
      // 预置家：把目录项预填进新建表单（名称可改、协议可改）。预填放在 syncModeUi
      // 之后 —— syncBaseHint 依赖协议下拉的当前值，先填协议再刷提示才是对的。
      if (activePreset) {
        const name = $('custom-name-input');
        const protocol = $('custom-protocol-select');
        const baseUrl = $('custom-baseurl-input');
        if (name) name.value = activePreset.name || '';
        if (protocol && activePreset.protocol) protocol.value = activePreset.protocol;
        if (baseUrl) baseUrl.value = activePreset.baseUrl || '';
        const hint = $('custom-baseurl-hint');
        if (hint) hint.textContent = activePreset.hint || (activePreset.protocol === 'anthropic'
          ? BASE_HINT_ANTHROPIC : BASE_HINT_OPENAI);
      }
      syncModeUi();
      void providers.refreshCustom().then(syncModeUi);
    },
  });
})();
