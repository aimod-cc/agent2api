/* Agent2API · 自定义提供商的「模型管理」弹窗（管理弹窗里那颗「模型管理」按钮的本体） */
/* global wbApp */

/**
 * 账号页「自定义提供商」管理弹窗里那颗「模型管理」按钮打开的就是本弹窗
 * （转发链：custom-provider-ui.js 的 handleAction）。交互形态
 * 参考 OmniProxy 的模型管理弹窗（D:\Code\OmniProxy 的 ModelsModal.tsx）：
 * 获取上游模型 + 每个模型的启用开关 + 思考等级绑定 + 模型映射（含开关），
 * 但数据链路完全走本项目自己的后端接口（三个，全部已就绪）：
 *
 *   · GET  /api/custom-providers        —— 打开时从目录缓存读该家记录；
 *   · POST /api/custom-providers/fetch-models —— 服务端代拉上游清单（不落盘），
 *                                         结果合并进本地 models（新 id 追加）；
 *   · POST /api/custom-providers/models —— **整表替换**保存：models / mappings
 *                                         传全量数组，成功返回 {provider}。
 *
 * ── 编辑模型：打开时深拷贝一份本地副本，保存才提交 ─────────────
 * 打开时把该家记录的 models / mappings 深拷贝成本地编辑副本，弹窗里的所有
 * 操作（拉取 / 添加 / 移除 / 改开关 / 改等级）只改副本；点「保存」才把这两份
 * 全量数组整表提交，点「取消」/关弹窗即丢弃。这是「整表替换」这个后端语义的
 * 界面形态 —— 没有逐条保存的中间态，因为后端每次都在替换整张表。
 *
 * ── 与 models-panel.js 的关系：复用类名，不复制逻辑 ─────────────
 * 内置家的模型管理页（models-panel.js）有自己的一整套数据生命周期（/api/models/manage
 * 形状、分组折叠、行内操作直写后端），与这里的「本地副本 + 整表保存」是两种
 * 交互模型，套不进同一份代码。但**视觉语言必须同源**：表格（.models-table）、
 * 开关（.switch / .track）、映射 chip（.alias / .map-off）、行内操作
 * （.row-actions / .sm.ghost.danger-text）都用同一批 CSS 类（定义在
 * page-gateway.css 与 components.css），不在这里另造样式。
 *
 * ── 思考等级表的来源 ──────────────────────────────────────────
 * 权威表是后端 GET /api/models/manage 的 reasoningLevels 字段（与
 * models-panel / models-reasoning.js 同一张表）。本弹窗打开时补拉一次并缓存；
 * 拿不到（旧版网关 / 请求失败）时退回 models-reasoning.js 的本地兜底表
 * （`wbModelsReasoning.levels({})`）—— 下拉先按兜底表渲染，权威表回来后重绘。
 *
 * 弹窗按需创建、关闭即移除（与 custom-provider-ui.js 的编辑弹窗同一手法），
 * 不往 index.html 里常驻空弹窗。
 */
(() => {
  const { esc, toast } = wbApp;
  const providers = window.wbProviders;
  const $ = id => document.getElementById(id);

  const MODAL_ID = 'custom-models-modal';
  /** 模型 id 与映射 alias 的长度上限（与后端校验一致，前端先挡一次） */
  const MAX_ID_CHARS = 128;
  /** 思考等级字符串长度上限（与后端校验一致） */
  const MAX_REASONING_CHARS = 32;

  /** 权威等级表缓存（GET /api/models/manage 的 reasoningLevels）；null = 还没拉到 */
  let reasoningLevelsCache = null;
  /** 进行中的等级表请求：并发打开合并成一次 */
  let levelsInflight = null;

  /**
   * 当前编辑态。`null` = 弹窗没开。开着期间是本弹窗**唯一**的数据来源：
   *   { providerId, providerName, models: [{id, enabled, reasoning}], mappings: [{alias, target, enabled, reasoning}] }
   * 打开时深拷贝，保存/取消后置回 null —— 不把副本留在模块外，谁都无法在
   * 弹窗关闭后接着改它。
   */
  let editing = null;
  /** 保存中标志：置真期间不许关窗（关掉会让「到底存没存」变成未知状态） */
  let saving = false;

  // ─── 工具 ──────────────────────────────────

  /** 判重用的归一化：去空白 + 大小写不敏感（与任务口径一致） */
  const norm = value => String(value ?? '').trim().toLowerCase();

  /** 当前可用的思考等级表：权威表 > models-reasoning.js 的本地兜底表 */
  function currentReasoningLevels() {
    if (Array.isArray(reasoningLevelsCache) && reasoningLevelsCache.length) {
      return reasoningLevelsCache;
    }
    return window.wbModelsReasoning?.levels?.({}) || [];
  }

  /** 补拉一次权威等级表（缓存 + 并发合并；失败静默，下拉留在兜底表上） */
  function loadReasoningLevels() {
    if (reasoningLevelsCache) return Promise.resolve(reasoningLevelsCache);
    if (!levelsInflight) {
      levelsInflight = providers.customRequest('GET', '/api/models/manage')
        .then(data => {
          const list = (Array.isArray(data?.reasoningLevels) ? data.reasoningLevels : [])
            .filter(level => typeof level === 'string' && level);
          if (list.length) reasoningLevelsCache = list;
          return reasoningLevelsCache;
        })
        .catch(() => null) // 静默：兜底表照常可用，不值得为它弹错误
        .finally(() => { levelsInflight = null; });
    }
    return levelsInflight;
  }

  /** 该模型是否已在本地副本里（忽略大小写判重，供拉取 / 手动添加共用） */
  function hasModel(id) {
    return editing.models.some(model => norm(model.id) === norm(id));
  }

  /** 该 alias → target 的映射是否已在本地副本里（忽略大小写判重） */
  function hasMapping(alias, target) {
    return editing.mappings.some(mapping =>
      norm(mapping.alias) === norm(alias) && norm(mapping.target) === norm(target));
  }

  /**
   * 取一条提供商记录：先读目录缓存，没有（缓存还没拉到 / 刚被别人改过）就
   * 现拉一次 —— 与 custom-provider-ui.js 的 findProvider 同口径（那边是私有
   * 函数，不跨文件复用一份极小的查找逻辑，两处各自三行，改一处漏一处也无害）。
   */
  async function findProvider(providerId) {
    const cached = (providers?.customList?.() || []).find(item => item.id === providerId);
    if (cached) return cached;
    const list = await providers?.refreshCustom?.();
    return (list || []).find(item => item.id === providerId) || null;
  }

  // ─── 打开 / 关闭 ────────────────────────────

  /**
   * 打开弹窗。providerId 从管理弹窗行上的 data-provider-id 来
   * （custom-provider-ui.js 的 handleAction 转发进来）。
   */
  async function open(providerId) {
    if (typeof providerId !== 'string' || !providerId.startsWith('custom-')) return;
    // 先清掉编辑态：取数期间旧副本不可再被表内操作改到（各 handler 见 null 即退）
    editing = null;
    const provider = await findProvider(providerId);
    if (!provider) {
      toast('该自定义提供商已不存在（可能已被删除），请刷新列表后重试', 'err');
      return;
    }
    // 深拷贝成本地编辑副本（归一化字段：缺省 enabled 视作开、reasoning 视作空）
    editing = {
      providerId: provider.id,
      providerName: provider.name || provider.id,
      models: (Array.isArray(provider.models) ? provider.models : []).map(model => ({
        id: String(model?.id ?? ''),
        enabled: model?.enabled !== false,
        reasoning: typeof model?.reasoning === 'string' ? model.reasoning : '',
      })),
      mappings: (Array.isArray(provider.mappings) ? provider.mappings : []).map(mapping => ({
        alias: String(mapping?.alias ?? ''),
        target: String(mapping?.target ?? ''),
        enabled: mapping?.enabled !== false,
        reasoning: typeof mapping?.reasoning === 'string' ? mapping.reasoning : '',
      })),
    };
    for (const model of editing.models) {
      const defaults = editing.mappings.find(mapping => norm(mapping.alias) === norm(model.id)
        && norm(mapping.target) === norm(model.id));
      if (defaults) {
        model.enabled = model.enabled && defaults.enabled;
        model.reasoning = defaults.reasoning || model.reasoning;
      }
    }
    editing.mappings = editing.mappings.filter(mapping => norm(mapping.alias) !== norm(mapping.target)
      || !editing.models.some(model => norm(model.id) === norm(mapping.target)));
    $(MODAL_ID)?.remove();
    buildShell();
    renderTable();
    // 等级表还没拉到过就补一次：回来时弹窗还开着才重绘（把兜底表换成权威表）
    if (!reasoningLevelsCache) {
      void loadReasoningLevels().then(() => {
        if (reasoningLevelsCache && editing && $(MODAL_ID)) renderTable();
      });
    }
  }

  /** 关闭弹窗：保存中不响应（关掉会让「到底存没存进去」变成未知状态） */
  function close() {
    if (saving) return;
    $(MODAL_ID)?.remove();
    editing = null;
  }

  // ─── 弹窗骨架 ──────────────────────────────

  /** 按当前 editing 拼出弹窗（动态创建 modal-mask，关闭即移除） */
  function buildShell() {
    const mask = document.createElement('div');
    mask.id = MODAL_ID;
    mask.className = 'modal-mask open';
    mask.setAttribute('role', 'dialog');
    mask.setAttribute('aria-modal', 'true');
    mask.setAttribute('aria-labelledby', 'custom-models-title');
    // 列宽：模型 id 是主体信息给最宽；映射列装 chips 与「＋ 映射」输入行，
    // 与 models-panel 的 c-model 30% / c-alias 30% 同一取舍（谁装字谁占宽）
    mask.innerHTML = `<div class="modal modal-wide">
        <div class="modal-head">
          <h2 id="custom-models-title">模型管理 — ${esc(editing.providerName)}</h2>
          <button type="button" id="cmm-close" title="关闭">✕</button>
        </div>
        <div class="modal-body">
          <div class="field-row">
            <button type="button" id="cmm-fetch" class="sm"
              title="由服务端代拉上游模型清单（不落盘）；新模型并入下表，已存在的跳过">获取模型</button>
            <input type="text" id="cmm-add-input" maxlength="${MAX_ID_CHARS}"
              placeholder="手动登记的模型 ID" style="width:220px" aria-label="手动登记的模型 ID">
            <button type="button" id="cmm-add" class="sm"
              title="把输入的模型 ID 登记进清单（忽略大小写判重）">添加模型</button>
            <span class="detail">改动先存本地副本，点「保存」才整表提交</span>
          </div>
          <div class="models-table-wrap">
            <table class="models-table">
              <colgroup>
                <col style="width:26%">
                <col style="width:62%">
                <col style="width:12%">
              </colgroup>
              <thead><tr>
                <th>上游模型</th>
                <th>模型映射（原始 ID 与别名）</th>
                <th class="r">操作</th>
              </tr></thead>
              <tbody id="cmm-tbody"></tbody>
            </table>
          </div>
        </div>
        <div class="modal-foot">
          <span class="detail" id="cmm-hint"></span>
          <div class="spacer"></div>
          <button type="button" id="cmm-cancel">取消</button>
          <button type="button" id="cmm-save" class="primary">保存</button>
        </div>
      </div>`;
    document.body.appendChild(mask);

    $('cmm-close')?.addEventListener('click', close);
    $('cmm-cancel')?.addEventListener('click', close);
    // 点遮罩空白处 = 关闭（与编辑弹窗同一交互），点弹窗本体不关
    mask.addEventListener('click', event => {
      if (event.target === mask) close();
    });
    $('cmm-fetch')?.addEventListener('click', () => { void fetchModels(); });
    $('cmm-add')?.addEventListener('click', () => { if (editing) addModel(); });
    $('cmm-add-input')?.addEventListener('keydown', event => {
      if (event.key !== 'Enter') return;
      event.preventDefault();
      if (editing) addModel();
    });
    $('cmm-save')?.addEventListener('click', () => { void save(); });
    // 表内交互统一走事件委托（表格会随每次操作整块重绘，逐行绑定每次都要重挂）
    const body = $('cmm-tbody');
    body?.addEventListener('click', onTableClick);
    body?.addEventListener('change', onTableChange);
    body?.addEventListener('keydown', onTableKeydown);
    $('cmm-add-input')?.focus();
  }

  // ─── 表格渲染 ──────────────────────────────

  /** 该模型的映射（target 指向它，忽略大小写），带上它们在 mappings 里的原始下标 */
  function mappingsOf(model) {
    return editing.mappings
      .map((mapping, index) => ({ mapping, index }))
      .filter(entry => norm(entry.mapping.target) === norm(model.id));
  }

  /**
   * 思考等级下拉的选项：`不覆盖`（空值）+ 全部候选等级。
   *
   * 现值不在候选表里时（旧数据 / 表外自定义值）补一项原样回显并选中 ——
   * 不补的话 select 会静默落回第一项，用户改别的字段保存时把这条绑定悄悄改掉，
   * 是最难查的一类数据丢失（与 models-reasoning.js 的 fillSelect 同一取舍）。
   */
  function reasoningOptionsHtml(current) {
    const value = String(current ?? '').trim();
    const levels = currentReasoningLevels();
    const options = ['<option value="">不覆盖</option>']
      .concat(levels.map(level =>
        `<option value="${esc(level)}"${level === value ? ' selected' : ''}>${esc(level)}</option>`));
    if (value && !levels.includes(value)) {
      options.push(`<option value="${esc(value)}" selected>${esc(value)}</option>`);
    }
    return options.join('');
  }

  /** 一条映射的 chip：名字 + 小开关（复用 .alias .switch / .map-off）+ 迷你等级下拉 + × 删除 */
  function mappingChipHtml(model, entry) {
    const { mapping, index } = entry;
    const on = mapping.enabled !== false;
    return `<span class="alias${on ? '' : ' map-off'}">`
      + `<span class="t" title="对外映射名：${esc(mapping.alias)}（目标模型 ${esc(model.id)}）">${esc(mapping.alias)}</span>`
      + `<label class="switch" title="${on ? '映射已启用，点击关闭' : '映射已关闭，点击启用'}">`
      + `<input type="checkbox" data-act="map-toggle" data-map-index="${index}"${on ? ' checked' : ''}>`
      + `<span class="track"></span></label>`
      + `<select data-act="map-reasoning" data-map-index="${index}" title="这条映射的思考等级（空 = 不覆盖）"`
      + ` style="height:16px;min-width:58px;max-width:96px;font-size:10px;padding:0 12px 0 4px;border-radius:8px">`
      + reasoningOptionsHtml(mapping.reasoning)
      + `</select>`
      + `<button type="button" class="x" data-act="map-remove" data-map-index="${index}"`
      + ` title="删除映射 ${esc(mapping.alias)}">×</button></span>`;
  }

  /** 默认绑定与别名共用映射列；默认绑定只可关闭，不可删除。 */
  function rowHtml(model, index) {
    const defaults = `<span class="alias${model.enabled ? '' : ' map-off'}">`
      + `<label class="switch" title="原始 ID 的默认绑定：关闭后下游不能使用这个名称">`
      + `<input type="checkbox" data-act="model-toggle" data-model-index="${index}"${model.enabled ? ' checked' : ''}>`
      + `<span class="track"></span></label><span class="t">${esc(model.id)}</span>`
      + '<span class="binding-default">默认</span>'
      + `<select class="binding-reasoning" data-act="model-reasoning" data-model-index="${index}" title="默认绑定的思考等级">`
      + reasoningOptionsHtml(model.reasoning) + '</select></span>';
    const chips = mappingsOf(model).map(entry => mappingChipHtml(model, entry)).join('');
    return `<tr data-model-index="${index}"><td>${esc(model.id)}</td>`
      + `<td class="cell-alias"><div class="aliases">${defaults}${chips}</div>`
      + `<div class="binding-add"><input type="text" data-map-input="${index}" maxlength="${MAX_ID_CHARS}" placeholder="对外映射名" aria-label="对外映射名">`
      + `<button type="button" class="alias-add" data-act="map-add" data-model-index="${index}">＋ 映射</button></div></td>`
      + `<td class="r"><div class="row-actions"><button type="button" class="sm ghost danger-text" data-act="model-remove" data-model-index="${index}" title="移除该模型及其绑定">移除</button></div></td></tr>`;
  }

  function renderTable() {
    const body = $('cmm-tbody');
    if (!body || !editing) return;
    body.innerHTML = editing.models.map((model, index) => rowHtml(model, index)).join('')
      || '<tr><td colspan="3" class="empty">尚无模型：点「获取模型」从上游拉取，或手动添加</td></tr>';
  }

  // ─── 工具栏动作 ─────────────────────────────

  /** 获取模型：服务端代拉上游清单，新 id 并入本地 models（已存在的跳过） */
  async function fetchModels() {
    if (!editing || saving) return;
    const button = $('cmm-fetch');
    if (button) { button.disabled = true; button.textContent = '拉取中…'; }
    try {
      const data = await providers.customRequest('POST', '/api/custom-providers/fetch-models', {
        providerId: editing.providerId,
      });
      const ids = (Array.isArray(data?.models) ? data.models : [])
        .map(id => String(id ?? '').trim())
        .filter(Boolean);
      let added = 0;
      for (const id of ids) {
        if (hasModel(id)) continue; // 忽略大小写判重：已存在的跳过
        editing.models.push({ id, enabled: true, reasoning: '' });
        added++;
      }
      renderTable();
      toast(`拉取到 ${ids.length} 个模型，新增 ${added} 个`);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast(`获取模型失败：${message}`, 'err');
    } finally {
      if (button) { button.disabled = false; button.textContent = '获取模型'; }
    }
  }

  /** 添加模型（工具栏）：把手动输入的 id 追加进本地 models */
  function addModel() {
    const input = $('cmm-add-input');
    const id = (input?.value || '').trim();
    if (!id) { toast('请先输入要登记的模型 ID', 'err'); return; }
    if (id.length > MAX_ID_CHARS) {
      toast(`模型 ID 过长（最多 ${MAX_ID_CHARS} 个字符）`, 'err');
      return;
    }
    if (hasModel(id)) { toast(`模型「${id}」已存在（忽略大小写判重）`, 'err'); return; }
    editing.models.push({ id, enabled: true, reasoning: '' });
    if (input) { input.value = ''; input.focus(); }
    renderTable();
  }

  /** 添加映射（行内）：alias 输入框 + 按钮，target 固定为本行模型 id */
  function addMapping(modelIndex) {
    const model = editing.models[modelIndex];
    if (!model) return;
    const input = $(MODAL_ID)?.querySelector(`input[data-map-input="${modelIndex}"]`);
    const alias = (input?.value || '').trim();
    if (!alias) { toast('请先输入对外映射名', 'err'); return; }
    if (alias.length > MAX_ID_CHARS) {
      toast(`对外映射名过长（最多 ${MAX_ID_CHARS} 个字符）`, 'err');
      return;
    }
    if (norm(alias) === norm(model.id)) {
      toast('原始 ID 已作为默认绑定，请直接使用它的开关和思考等级设置', 'err');
      return;
    }
    if (hasMapping(alias, model.id)) { toast(`映射「${alias} → ${model.id}」已存在`, 'err'); return; }
    editing.mappings.push({ alias, target: model.id, enabled: true, reasoning: '' });
    if (input) input.value = '';
    renderTable();
  }

  /** 移除模型（本地副本）：连带删掉 target 指向它的映射，并说明删了几条 */
  function removeModel(modelIndex) {
    const model = editing.models[modelIndex];
    if (!model) return;
    const before = editing.mappings.length;
    editing.mappings = editing.mappings.filter(mapping => norm(mapping.target) !== norm(model.id));
    const removedMappings = before - editing.mappings.length;
    editing.models.splice(modelIndex, 1);
    toast(`已移除模型「${model.id}」`
      + (removedMappings ? `，连带移除 ${removedMappings} 条映射` : ''));
    renderTable();
  }

  // ─── 表内事件（委托）────────────────────────

  function onTableClick(event) {
    if (!editing) return;
    const button = event.target.closest('button[data-act]');
    if (!button) return;
    const { act } = button.dataset;
    if (act === 'map-add') {
      addMapping(Number(button.dataset.modelIndex));
      return;
    }
    if (act === 'map-remove') {
      const mapping = editing.mappings[Number(button.dataset.mapIndex)];
      if (!mapping) return;
      editing.mappings.splice(Number(button.dataset.mapIndex), 1);
      toast(`已删除映射「${mapping.alias} → ${mapping.target}」`);
      renderTable();
      return;
    }
    if (act === 'model-remove') removeModel(Number(button.dataset.modelIndex));
  }

  function onTableChange(event) {
    if (!editing) return;
    const control = event.target.closest('input[data-act], select[data-act]');
    if (!control) return;
    const { act } = control.dataset;
    // 三个开关 / 下拉都只改副本，不重绘整表（控件自己就在新状态上）；
    // 映射开关顺带就地切换 chip 的弱化样式（.map-off），与 models-panel 同源
    if (act === 'model-toggle') {
      const model = editing.models[Number(control.dataset.modelIndex)];
      if (model) model.enabled = control.checked;
      return;
    }
    if (act === 'model-reasoning') {
      const model = editing.models[Number(control.dataset.modelIndex)];
      if (model) model.reasoning = control.value;
      return;
    }
    if (act === 'map-toggle') {
      const mapping = editing.mappings[Number(control.dataset.mapIndex)];
      if (!mapping) return;
      mapping.enabled = control.checked;
      control.closest('.alias')?.classList.toggle('map-off', !mapping.enabled);
      return;
    }
    if (act === 'map-reasoning') {
      const mapping = editing.mappings[Number(control.dataset.mapIndex)];
      if (mapping) mapping.reasoning = control.value;
    }
  }

  /** 映射输入框里回车 = 添加这条映射 */
  function onTableKeydown(event) {
    if (event.key !== 'Enter') return;
    const input = event.target.closest('input[data-map-input]');
    if (!input) return;
    event.preventDefault();
    addMapping(Number(input.dataset.mapInput));
  }

  // ─── 保存 ──────────────────────────────────

  /**
   * 提交前的基本校验（后端还会再校验一遍：id/alias ≤128 非空、reasoning ≤32、去重）。
   * 返回空串 = 通过，否则返回那句要 toast 的文案。与后端同一口径，只挡能提前
   * 判死的项，不做后端才知道的判定。
   */
  function validateDraft() {
    const seenModels = new Set();
    for (const model of editing.models) {
      const id = String(model.id ?? '').trim();
      if (!id) return '存在模型 ID 为空的行，请补齐或先移除';
      if (id.length > MAX_ID_CHARS) return `模型 ID「${id.slice(0, 32)}…」超过 ${MAX_ID_CHARS} 个字符`;
      if (seenModels.has(norm(id))) return `模型 ID「${id}」重复（忽略大小写），请先去重`;
      seenModels.add(norm(id));
      if (String(model.reasoning ?? '').trim().length > MAX_REASONING_CHARS) {
        return `模型「${id}」的思考等级超过 ${MAX_REASONING_CHARS} 个字符`;
      }
    }
    const seenPairs = new Set();
    for (const mapping of editing.mappings) {
      const alias = String(mapping.alias ?? '').trim();
      const target = String(mapping.target ?? '').trim();
      if (!alias) return `映射到「${target || '未知模型'}」的对外名为空，请补齐或删除`;
      if (alias.length > MAX_ID_CHARS) return `对外映射名「${alias.slice(0, 32)}…」超过 ${MAX_ID_CHARS} 个字符`;
      if (!target) return `映射「${alias}」的目标模型为空`;
      if (String(mapping.reasoning ?? '').trim().length > MAX_REASONING_CHARS) {
        return `映射「${alias}」的思考等级超过 ${MAX_REASONING_CHARS} 个字符`;
      }
      const pairKey = `${norm(alias)}\u0001${norm(target)}`;
      if (seenPairs.has(pairKey)) return `映射「${alias} → ${target}」重复，请先去重`;
      seenPairs.add(pairKey);
    }
    return '';
  }

  /** 保存：把本地副本的 models / mappings 全量提交（整表替换） */
  async function save() {
    if (!editing || saving) return;
    const problem = validateDraft();
    if (problem) { toast(problem, 'err'); return; }
    const button = $('cmm-save');
    saving = true;
    if (button) { button.disabled = true; button.textContent = '保存中…'; }
    try {
      await providers.customRequest('POST', '/api/custom-providers/models', {
        providerId: editing.providerId,
        models: editing.models.map(model => ({
          id: model.id.trim(),
          enabled: model.enabled,
          reasoning: String(model.reasoning ?? '').trim(),
        })),
        mappings: editing.mappings.map(mapping => ({
          alias: mapping.alias.trim(),
          target: mapping.target.trim(),
          enabled: mapping.enabled,
          reasoning: String(mapping.reasoning ?? '').trim(),
        })),
      });
      saving = false;
      $(MODAL_ID)?.remove();
      editing = null;
      toast('✅ 已保存');
      // 目录先刷（管理弹窗列表 / 筛选器的展示名都读它），再全量刷新账号列表补一次重绘
      void providers.refreshCustom();
      await wbApp.refresh?.();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast(`保存失败：${message}`, 'err');
      // 后端 400 的文案写进脚注留一份（toast 几秒后就没了）；表单态保留
      const hint = $('cmm-hint');
      if (hint) hint.textContent = message;
      if (button) { button.disabled = false; button.textContent = '保存'; }
    } finally {
      saving = false;
    }
  }

  // Esc = 关闭本弹窗（只在本弹窗开着时动作，与其它弹窗的 Esc 互不干扰）
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && $(MODAL_ID)) close();
  });

  window.wbCustomModelsModal = { open };
})();
