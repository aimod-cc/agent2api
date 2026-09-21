/* Agent2API · 网关 Key 页：多把 API Key 的列表 / 新建 / 启停 / 删除 / 可用范围限制 */
/* global workbuddyDesktop, wbApp */

/**
 * 数据来自 `GET /api/keys`（`{keys, authRequired, providers}`，keys 带明文 key
 * 与掩码，另带 `allowedProviders` / `allowedModels` 两个白名单数组）。写接口都
 * 返回最新列表，就地替换后重绘。列表默认显示掩码，每行可单独「显示」明文并复制
 * （复制走 clipboard.js 的 data-copy 委托）。
 *
 * ── 可用提供商 / 可用模型（R9，参考 OmniProxy 的 Key 限制）──────
 * 每把 Key 可以限制「允许路由到哪几家」与「允许请求哪些对外模型名」，
 * **空数组 = 不限制**（语义与字段说明见后端 `core::api_keys` 的模块头）。
 * 两个多选都复用自研下拉（`select.js` 的多选形态 —— 原生 `<select multiple>`
 * 被增强成触发器 + 浮层，选项运行时灌入），所以本文件不新造控件；
 * 选项来源也都不硬编码：
 *   · 提供商 = 后端 `list_json` 下发的注册表摘要（`providers`），
 *     项目禁止维护第二份 provider 清单（见 `account_store` 模块头）；
 *   · 模型 = 后端下发的 `modelsByProvider`（每家 → 对外名清单）按**当前勾选的
 *     提供商**取并集 —— 见下面「可用模型随提供商联动」一节。
 *
 * ── 可用模型随提供商联动（本次改造）──────────────────────────
 * 改造前模型候选是一份**与提供商无关的全量清单**（`/api/session` 的 models）：
 * 一家都没勾时列出全部模型，勾了某几家也还是全部 —— 用户无法从候选里看出
 * 「这个模型是哪家提供的」，勾了一个根本没有对应提供商的模型，请求必然被拒。
 * 现在两者联动：
 *   · 一家都没勾（= 不限制提供商）→ 模型候选**为空**。这不是「没得选」而是
 *     「还没有约束范围」：没有可选项时浮层里给一句说明，见 `fillMultiSelect`
 *     的 emptyHint；
 *   · 勾了 N 家 → 候选 = 这 N 家的对外名并集（后端 `modelsByProvider` 里各家的
 *     数组取并集，忽略大小写去重、按名称排序）。
 * 取并集而不是交集：多选语义是「允许这几家」，而模型白名单是「允许这些名字」，
 * 一个名字只要**有任意一家**能提供它就值得被勾选（`allows_model` 与
 * `allows_provider` 是 AND 关系，最终仍由转发层按两家各自的白名单收窄）。
 *
 * 候选表一次取全、勾选时**本地**重算：每次勾选都发请求既慢又会有「响应回来时
 * 用户已经改了勾选」的竞态。
 *
 * ── 联动时已经勾上的模型怎么办（不能悄悄清掉）────────────────
 * 取消勾选某家后，那家独有的模型不该继续留在白名单里（它们已不可能被路由到），
 * 但它们**仍在用户眼前**：重建候选时按「并集 ∪ 已勾选的」铺选项，已勾的照旧
 * 显示为已勾、用户能看见并自己取消。直接把已勾项丢掉会让「保存范围」变成一次
 * 静默的数据修改 —— 用户没动过模型那栏，白名单却少了几项。
 *
 * ── 编辑既有 Key 的限制 ─────────────────────────────────────
 * 列表每行的「限制」按钮打开同一个弹窗的编辑形态（与模型管理页「点 chip 改
 * 思考等级」同一取舍：字段完全重合，独立窗口只会让两件事看起来不相干）。
 * 提交走 `PATCH /api/keys/{id}`，只带 `allowedProviders` / `allowedModels`
 * 两个键 —— **名称与启停不动**（部分更新语义，后端按「键在不在」区分「不改」
 * 与「清空」，见 `api_keys::update` 的说明）。
 */

(() => {
  const { esc, toast, formatTime } = wbApp;
  const $ = id => document.getElementById(id);

  let data = null;
  let loading = false;
  /** 已切到明文显示的 key id */
  const revealed = new Set();
  const pending = new Set();

  function keys() { return Array.isArray(data?.keys) ? data.keys : []; }

  /** 后端下发的提供商摘要（注册表顺序：workbuddy → raccoon → …） */
  function providers() {
    return Array.isArray(data?.providers) ? data.providers : [];
  }

  /**
   * 后端下发的「每家 → 对外名清单」（`modelsByProvider`）。
   *
   * 这张表是模型候选的**唯一**数据源（见模块头「可用模型随提供商联动」）：
   * 它按家给出各自能收的对外名（含映射别名），不像 `/api/session` 的 models
   * 那样跨家去重 —— 同名模型被别家认领时，另一家在这里仍看得到自己的那一条。
   */
  function modelsByProvider() {
    const map = data?.modelsByProvider;
    return map && typeof map === 'object' ? map : {};
  }

  /**
   * 按**当前勾选的提供商**取对外名并集，铺成多选选项。
   *
   * `selected` 里的名字即使不在并集里也照样保留（铺成已勾选状态）——
   * 用户取消勾选某家之后，那家独有的模型仍要看得见、能自己取消，否则「保存范围」
   * 会变成一次静默的数据修改（理由见模块头那一节）。
   *
   * 一家都没勾时返回**空数组**（需求：没有选择可用提供商时，可用模型就该是空的）。
   */
  function modelOptions(selectedProviders, selected) {
    const table = modelsByProvider();
    const names = new Map();   // 小写 → 原始名（先到先得，保住后端给的大小写）
    const put = value => {
      const text = String(value ?? '').trim();
      if (!text) return;
      const key = text.toLowerCase();
      if (!names.has(key)) names.set(key, text);
    };
    (Array.isArray(selectedProviders) ? selectedProviders : []).forEach(id => {
      const list = table[id] ?? table[String(id).toLowerCase()];
      if (Array.isArray(list)) list.forEach(put);
    });
    // 已勾选的模型无论是否还在并集里都要铺出来（见函数说明）
    (Array.isArray(selected) ? selected : []).forEach(put);
    const out = [...names.values()].map(id => ({ id, label: id }));
    out.sort((a, b) => (a.id.toLowerCase() < b.id.toLowerCase() ? -1 : 1));
    return out;
  }

  async function load() {
    if (loading) return;
    loading = true;
    try {
      data = await workbuddyDesktop.getKeys();
      render();
    } catch (error) {
      toast(`读取 Key 列表失败：${error.message}`, 'err');
    } finally {
      loading = false;
    }
  }

  function accept(next) {
    if (next && Array.isArray(next.keys)) data = next;
    render();
  }

  function renderStatus() {
    const badge = $('keys-status');
    if (!badge) return;
    const enabled = keys().filter(k => k.enabled).length;
    if (data?.authRequired) {
      badge.className = 'badge ok';
      badge.textContent = `已启用鉴权 · ${enabled} 把 Key 生效`;
    } else {
      badge.className = 'badge warn';
      badge.textContent = '未启用鉴权';
    }
  }

  /**
   * 一行 Key 名下的**限制摘要**（照抄 OmniProxy 表单下方那句说明的措辞，
   * 把「留空 = 不限制」摊开成具体事实）。
   *
   * 为什么要显示而不是留空：限制是**看不见的**——列表上不写，用户就只记得
   * 「我配过点什么」，客户端 404 时会去查模型、查账号，最后才想到是 Key 的限制。
   * 无限制时显示「不限制」而不是省略：省略与「没读到」在界面上长得一样。
   */
  function restrictionText(k) {
    const providers = Array.isArray(k.allowedProviders) ? k.allowedProviders : [];
    const models = Array.isArray(k.allowedModels) ? k.allowedModels : [];
    if (!providers.length && !models.length) return '不限制';
    const parts = [];
    if (providers.length) {
      const labels = providers.map(id => window.wbProviders?.labelOf?.(id) || id);
      parts.push(labels.join('、'));
    }
    if (models.length) {
      // 模型那半边只报个数：一屏 Row 里塞不下十几个模型名，悬停由 title 给全量
      parts.push(`${models.length} 个模型`);
    }
    return `限制：${parts.join(' / ')}`;
  }

  /** 限制摘要的完整说明（悬停 title 用；列不宽，详情只能挂这里） */
  function restrictionTitle(k) {
    const providers = Array.isArray(k.allowedProviders) ? k.allowedProviders : [];
    const models = Array.isArray(k.allowedModels) ? k.allowedModels : [];
    if (!providers.length && !models.length) {
      return '这把 Key 不限制提供商与模型（可用全部上游与全部对外模型）';
    }
    const lines = [];
    if (providers.length) {
      lines.push(`可用提供商：${providers.map(id => window.wbProviders?.labelOf?.(id) || id).join('、')}`);
    }
    if (models.length) lines.push(`可用模型：${models.join('、')}`);
    return lines.join('\n');
  }

  function row(k) {
    const busyRow = pending.has(k.id);
    const shown = revealed.has(k.id);
    const restriction = `<div class="mname" title="${esc(restrictionTitle(k))}">${esc(restrictionText(k))}</div>`;
    return `<tr class="${k.enabled ? '' : 'off'}" data-id="${esc(k.id)}">`
      + `<td><div class="mid"><span class="t">${esc(k.name || '未命名')}</span></div>${restriction}</td>`
      + `<td><div class="keycell"><code class="kv">${esc(shown ? k.key : k.masked)}</code>`
      + `<button type="button" class="sm ghost" data-act="reveal" data-id="${esc(k.id)}">${shown ? '隐藏' : '显示'}</button>`
      + `<button type="button" class="sm ghost" data-copy="${esc(k.key)}" title="复制 Key">复制</button></div></td>`
      + `<td><span class="muted">${k.createdAt ? esc(formatTime(k.createdAt)) : '—'}</span></td>`
      + `<td class="state"><label class="switch"><input type="checkbox" data-act="toggle" data-id="${esc(k.id)}"${k.enabled ? ' checked' : ''}${busyRow ? ' disabled' : ''}><span class="track"></span></label></td>`
      + `<td class="r"><div class="row-actions">`
      + `<button type="button" class="sm ghost" data-act="restrict" data-id="${esc(k.id)}"${busyRow ? ' disabled' : ''}>可用范围</button>`
      + `<button type="button" class="sm ghost danger-text" data-act="remove" data-id="${esc(k.id)}"${busyRow ? ' disabled' : ''}>删除</button>`
      + `</div></td></tr>`;
  }

  function render() {
    const body = $('keys-list');
    if (!body) return;
    renderStatus();
    const list = keys();
    body.innerHTML = list.length
      ? list.map(row).join('')
      : `<tr><td colspan="5" class="empty">${data ? '还没有 Key，当前不鉴权' : '加载中…'}</td></tr>`;
    // 顶栏徽标镜像的是 #keys-status，重绘后同步一次
    wbApp.renderTopbarStatus?.();
  }

  async function runRowAction(id, run, doneText) {
    if (pending.has(id)) return;
    pending.add(id);
    render();
    try {
      accept(await run());
      if (doneText) toast(doneText);
    } catch (error) {
      toast(`操作失败：${error.message}`, 'err');
    } finally {
      pending.delete(id);
      render();
    }
  }

  // ─── 弹窗（新建 / 改可用范围共用）────────────

  let saving = false;
  /** 编辑形态的 Key id（null = 新建）。非空时名称与 Key 输入禁用、只提交两个白名单 */
  let editingId = null;

  /**
   * 把两个多选的选项灌好（每次打开都重建）。
   *
   * 重建而不是增量补：数据源（注册表摘要 / 模型清单）可能整体换过，
   * 增量比对要写一整套 diff，收益只有「保留勾选」—— 而勾选由调用方在重建后
   * 用 `fillMultiSelect` 的 `selected` 参数按 id 还回去，那才是必须保住的东西。
   * 重建后必须显式 `wbSelect.sync()`：select.js 的 observer 回调跑在微任务里，
   * 紧接着同步读界面还是旧的（它的注释里写明了这一点，多选回填正是那个场景）。
   */
  function fillMultiSelect(select, options, selected) {
    if (!select) return;
    const chosen = new Set((selected || []).map(item => String(item).toLowerCase()));
    select.innerHTML = options.map(item =>
      `<option value="${esc(item.id)}"${chosen.has(item.id.toLowerCase()) ? ' selected' : ''}>${esc(item.label)}</option>`,
    ).join('');
    // 空选项时 select.js 的浮层打不开（它不会为一个空浮层开口子），
    // 这里补一条禁用的说明项，让用户至少看到「为什么没有东西可选」
    if (!options.length) {
      select.innerHTML = `<option value="" disabled>${esc(select.dataset.emptyHint || '暂无可选项')}</option>`;
    }
    window.wbSelect?.sync?.(select);
  }

  /** 读一个多选里当前勾选的 value（保持选项顺序） */
  function selectedValues(select) {
    return select ? [...select.selectedOptions].map(option => option.value).filter(Boolean) : [];
  }

  function paintRestrictionState() {
    const box = $('key-restrict-summary');
    if (!box) return;
    const providers = selectedValues($('key-allowed-providers'));
    const models = selectedValues($('key-allowed-models'));
    if (!providers.length && !models.length) {
      box.textContent = '当前不限制：这把 Key 可以用全部提供商与全部对外模型';
      return;
    }
    const parts = [];
    if (providers.length) {
      parts.push(`提供商：${providers.map(id => window.wbProviders?.labelOf?.(id) || id).join('、')}`);
    }
    if (models.length) parts.push(`模型：${models.join('、')}`);
    // 只限制了模型、没限制提供商（旧数据里可能存在这种组合）：模型候选此刻只剩
    // 已勾的那几个（没有提供商就没有并集可铺），要说清怎么把候选拿回来 ——
    // 否则用户会以为「模型清单坏了，加不了新的」
    if (!providers.length) {
      parts.push('（模型候选需先选提供商；不选则沿用当前这几项，保存后仍按模型白名单生效）');
    }
    box.textContent = parts.join('　');
  }

  /**
   * 重建「可用模型」的候选（提供商勾选变化时调）。
   *
   * 勾选前先把**当前已勾的模型**读出来当保留项传进去 —— 否则取消勾选一家会
   * 让那家独有的、已被勾上的模型从选项里消失，用户再想取消它都点不到
   * （理由见模块头「联动时已经勾上的模型怎么办」）。
   */
  function rebuildModelOptions() {
    const select = $('key-allowed-models');
    if (!select) return;
    const chosenProviders = selectedValues($('key-allowed-providers'));
    const chosenModels = selectedValues(select);
    // 空候选的说明文案分两种：没勾提供商时是「先选提供商」，勾了却是空
    // 才是「这几家现在没有可用模型」（后端没给该家的清单 = 没登录态）
    select.dataset.emptyHint = chosenProviders.length
      ? '这几家当前没有可用模型（账号未登录或清单为空）'
      : '请先在上面选择可用提供商';
    fillMultiSelect(select, modelOptions(chosenProviders, chosenModels), chosenModels);
    paintRestrictionState();
  }

  /** 打开弹窗。`key` 为 null = 新建 */
  function openModal(key) {
    editingId = key?.id || null;
    const editing = Boolean(editingId);
    $('key-modal-title').textContent = editing ? `可用范围 · ${key.name || '未命名'}` : '新建 API Key';
    $('key-modal-save').textContent = editing ? '保存范围' : '创建';
    $('key-name').value = key?.name || '';
    $('key-value').value = '';
    $('key-name').disabled = editing;
    $('key-value').disabled = editing;
    // 编辑形态隐藏 Key 输入行：那一行在改范围时没有意义（Key 值不可改）
    const keyRow = $('key-value')?.closest('.field-row');
    if (keyRow) keyRow.hidden = editing;

    fillMultiSelect($('key-allowed-providers'), providers(), key?.allowedProviders || []);
    // 模型候选按刚铺好的提供商勾选取并集（`rebuildModelOptions` 自己读勾选，
    // 所以这里不必把 providers 再传一遍）；它会同时写空候选的说明文案
    rebuildModelOptions();

    $('key-modal-status').textContent = '';
    $('key-modal').classList.add('open');
    setTimeout(() => {
      if (editing) $('key-allowed-providers')?.focus();
      else $('key-name').focus();
    }, 0);
  }

  function closeModal() {
    if (saving) return;
    $('key-modal').classList.remove('open');
  }

  async function save() {
    if (saving) return;
    const allowedProviders = selectedValues($('key-allowed-providers'));
    const allowedModels = selectedValues($('key-allowed-models'));
    const status = $('key-modal-status');
    saving = true;
    $('key-modal-save').disabled = true;
    status.textContent = '保存中…';
    try {
      if (editingId) {
        // 只提交两个白名单：别名与启停都不动（部分更新语义，见文件头）
        const next = await workbuddyDesktop.updateKey(editingId, { allowedProviders, allowedModels });
        saving = false;
        accept(next);
        closeModal();
        toast('✅ 可用范围已保存');
        return;
      }
      const name = $('key-name').value.trim();
      const key = $('key-value').value.trim();
      if (key && key.length < 8) {
        status.textContent = 'Key 至少需要 8 个字符';
        return;
      }
      const next = await workbuddyDesktop.createKey({
        name,
        key: key || undefined,
        allowedProviders,
        allowedModels,
      });
      if (next?.created?.id) revealed.add(next.created.id);
      accept(next);
      saving = false;
      closeModal();
      toast('✅ Key 已创建，记得复制给客户端');
    } catch (error) {
      status.textContent = `保存失败：${error.message}`;
    } finally {
      saving = false;
      $('key-modal-save').disabled = false;
    }
  }

  // ─── 事件 ─────────────────────────────

  async function onClick(event) {
    const button = event.target.closest('[data-act]');
    if (!button) return;
    const { act, id } = button.dataset;
    if (act === 'reveal') {
      if (revealed.has(id)) revealed.delete(id); else revealed.add(id);
      render();
      return;
    }
    if (act === 'restrict') {
      const key = keys().find(k => k.id === id);
      if (key) openModal(key);
      return;
    }
    if (act === 'remove') {
      const key = keys().find(k => k.id === id);
      const name = esc(key?.name || key?.masked || id);
      // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行（等于没有确认）
      if (!(await window.wbConfirm?.ask?.({
        title: '删除网关 Key',
        html: `确定删除 Key「<strong>${name}</strong>」？使用它的客户端会立刻无法访问。`,
        okText: '删除',
        okClass: 'danger',
      }))) return;
      void runRowAction(id, () => workbuddyDesktop.deleteKey(id), 'Key 已删除');
    }
  }

  function onChange(event) {
    const input = event.target.closest('input[data-act="toggle"]');
    if (!input) return;
    const { id } = input.dataset;
    const enabled = input.checked;
    void runRowAction(id, () => workbuddyDesktop.updateKey(id, { enabled }), enabled ? 'Key 已启用' : 'Key 已停用');
  }

  $('keys-list')?.addEventListener('click', onClick);
  $('keys-list')?.addEventListener('change', onChange);
  $('btn-add-key')?.addEventListener('click', () => openModal(null));
  $('key-modal-close')?.addEventListener('click', closeModal);
  $('key-modal-cancel')?.addEventListener('click', closeModal);
  $('key-modal-save')?.addEventListener('click', () => { void save(); });
  $('key-value')?.addEventListener('keydown', event => { if (event.key === 'Enter') void save(); });
  // 提供商的勾选变化要**重建模型候选**（联动，见模块头）—— 它内部会顺带
  // 刷新摘要；模型自己的勾选变化只影响摘要
  $('key-allowed-providers')?.addEventListener('change', rebuildModelOptions);
  $('key-allowed-models')?.addEventListener('change', paintRestrictionState);
  $('key-modal')?.addEventListener('click', event => { if (event.target === $('key-modal')) closeModal(); });

  window.wbKeysPanel = { load, render };
  if (wbApp.currentPage === 'keys') void load();
})();
