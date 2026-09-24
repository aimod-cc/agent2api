/* Agent2API · 自定义提供商的模型清单数据源（模型管理页选中自定义家时的取数与写入） */
/* global wbApp */

/**
 * 模型管理页（models-panel.js）在选中一个自定义提供商时，本模块负责三件事：
 *
 *   1. **取数**：把该家记录（`models` / `mappings` 两个数组）适配成与
 *      `GET /api/models/manage` 同形的 `{models, mappings}` —— 于是表格渲染、
 *      搜索、映射 chip、思考等级、列设置全部照用，一行都不用为自定义家另写；
 *   2. **写入**：该家没有逐条接口，只有「整表替换」（`POST /api/custom-providers/models`）。
 *      每次操作都「读当前记录 → 应用这一处改动 → 提交全量 → 刷新目录缓存」，
 *      于是界面上的每一步（开关 / 别名 / 思考等级 / 移除）都**立即生效** ——
 *      与内置家的逐条即时生效在体验上完全一致，没有保存按钮、没有草稿态，
 *      也就没有「改完忘了保存」这一类问题；
 *   3. **拉取**：`fetch-models` 由服务端代拉上游清单，新模型并入后同样走整表提交。
 *
 * ── 为什么在模型管理页里做适配，而不是把自定义家并进后端 manage_view ──
 * 后端 `core/custom_providers.rs` 的注释写明了「modelRules 的 disabled/hidden
 * **不适用于**自定义家」：内置家的启停规则挂在全局 modelRules 上（映射是全局表、
 * 同一个别名可在多家各建一条做主备），自定义家的清单本身就是用户逐条登记、
 * 存在提供商记录里的数组。两套存储语义不同，合并要重构存储，而收益只是
 * 「少一层前端适配」。所以这里做的是**适配**：读的时候转成同形，写的时候转回去。
 *
 * ── 同名映射（alias == target）是早期数据形态 ──────────────
 * 早期版本会把「原始 ID 的默认绑定」也写进 mappings 数组。表格里默认绑定由模型行
 * 自己生成（开关 = `models[].enabled`、等级 = `models[].reasoning`），所以：
 *   · 读取时把它们**合并进默认绑定**（enabled 取与、reasoning 取先有值者），
 *     不在映射列另占一格；
 *   · 提交时**不再写回**（见 `draftOf` 的剔除）—— 否则用户改了默认绑定的开关，
 *     下一次读取会被那条旧条目覆盖回去，是最难查的一类「改了没生效」。
 *
 * 目录缓存（`wbProviders.customList`）是唯一的取数来源：它由 providers.js 统一
 * 拉取与刷新，本模块不自己发 GET，只在写入成功后 `refreshCustom()` 一次。
 */
(() => {
  /**
   * 目录模块（providers.js）在 index.html 里排在本文件**之后**加载（它要给账号页
   * 的几个模块共用，位置靠后），所以只能**运行期**取 —— 加载期解构会拿到
   * undefined，表现是自定义家整块静默失效（左栏永远「还没有自定义提供商」）。
   */
  const providers = () => window.wbProviders;

  /** 归一化判重口径：去空白 + 大小写不敏感（与后端一致） */
  const norm = value => String(value ?? '').trim().toLowerCase();
  const same = (left, right) => norm(left) === norm(right);

  /** `custom-` 前缀判据（与后端 `is_custom_provider_id` 一致） */
  function isCustom(id) {
    return typeof id === 'string' && id.startsWith('custom-');
  }

  /** 全部自定义提供商（目录缓存的原始记录，按 createdAt 升序） */
  function list() {
    return (providers()?.customList?.() || []).filter(provider => isCustom(provider?.id));
  }

  function record(id) {
    return list().find(provider => provider.id === id) || null;
  }

  /**
   * 记录 → 可提交的草稿（`{models, mappings}`）。
   *
   * 字段归一化与 `custom-models-modal` 当年那份深拷贝同口径：缺省 enabled 视作开、
   * reasoning 视作空串；同名映射并入默认绑定后从 mappings 里剔除（见文件头）。
   * 每次提交都从**目录缓存的当前值**重建，所以本函数是幂等的：连点两次开关，
   * 第二次读到的就是第一次提交后的值。
   */
  function draftOf(provider) {
    const models = (Array.isArray(provider?.models) ? provider.models : [])
      .map(model => ({
        id: String(model?.id ?? '').trim(),
        enabled: model?.enabled !== false,
        reasoning: typeof model?.reasoning === 'string' ? model.reasoning : '',
      }))
      .filter(model => model.id);
    const mappings = (Array.isArray(provider?.mappings) ? provider.mappings : [])
      .map(mapping => ({
        alias: String(mapping?.alias ?? '').trim(),
        target: String(mapping?.target ?? '').trim(),
        enabled: mapping?.enabled !== false,
        reasoning: typeof mapping?.reasoning === 'string' ? mapping.reasoning : '',
      }))
      .filter(mapping => mapping.alias && mapping.target);

    for (const model of models) {
      const legacy = mappings.find(mapping => same(mapping.alias, model.id) && same(mapping.target, model.id));
      if (!legacy) continue;
      model.enabled = model.enabled && legacy.enabled;
      model.reasoning = legacy.reasoning || model.reasoning;
    }
    const cleaned = mappings.filter(mapping =>
      !(same(mapping.alias, mapping.target) && models.some(model => same(model.id, mapping.target))));
    return { models, mappings: cleaned };
  }

  /**
   * 目录记录 → 表格同形数据。返回 `null` = 这家已不存在（被别处删掉了）。
   *
   * 形状与后端 `catalog::manage_view` 逐字对齐，models-panel 的渲染只认这些字段：
   *   · models: `{id, name, provider, providerLabel, source, enabled, aliases}`
   *     —— 自定义家没有倍率与来源概念，`source` 留空串（选中自定义家时那两列
   *     本来就按视图隐藏，见 models-panel 的 `visibleColumns`）；
   *   · mappings: `{alias, target, provider, enabled, reasoning, isDefault, dangling, carried}`
   *     —— 默认绑定（alias == target）由模型行现造，与内置家一致。
   */
  function buildView(id) {
    const provider = record(id);
    if (!provider) return null;
    const draft = draftOf(provider);
    const name = provider.name || id;
    const models = [];
    const mappings = [];
    for (const model of draft.models) {
      models.push({
        id: model.id,
        name: '',
        provider: id,
        providerLabel: name,
        source: '',
        enabled: model.enabled,
        aliases: [],
      });
      mappings.push({
        alias: model.id,
        target: model.id,
        provider: id,
        enabled: model.enabled,
        reasoning: model.reasoning,
        isDefault: true,
        dangling: false,
        carried: true,
      });
    }
    for (const mapping of draft.mappings) {
      const row = models.find(model => same(model.id, mapping.target));
      mappings.push({
        alias: mapping.alias,
        target: mapping.target,
        provider: id,
        enabled: mapping.enabled,
        reasoning: mapping.reasoning,
        isDefault: false,
        // 目标不在清单里 = 这条映射挂不到任何一行（表格底部的「未挂载」分组）。
        // 自定义家不该出现这种条目（移除模型会连带删映射），但手改过的数据文件
        // 或早期版本可能留下它 —— 照实列出来，比让用户找不到它强。
        dangling: !row,
        carried: Boolean(row),
      });
      if (row) row.aliases.push(mapping.alias);
    }
    return { models, mappings };
  }

  /**
   * 提交入口：读当前记录 → 交给 `mutate` 改草稿 → 整表提交 → 刷新目录缓存。
   * `mutate` 的返回值原样透传给调用方（用于拼 toast 文案）。
   */
  async function submit(id, mutate) {
    if (!providers()?.customRequest) throw new Error('目录模块未就绪');
    const provider = record(id);
    if (!provider) throw new Error('该自定义提供商已不存在（可能已被删除），请刷新后重试');
    const draft = draftOf(provider);
    const result = mutate(draft);
    await providers()?.customRequest('POST', '/api/custom-providers/models', {
      providerId: id,
      models: draft.models,
      mappings: draft.mappings,
    });
    // 目录缓存先刷（左栏计数、账号页弹窗的「N 个模型」都读它），再返回
    await providers()?.refreshCustom();
    return result;
  }

  /**
   * 一条绑定的开关 / 思考等级 —— 内置家那两个调用点（chip 开关、等级弹窗）的
   * 自定义家实现，语义逐条对齐：
   *   · `alias == target` 且清单里有这个模型 = **默认绑定**：改模型自己的字段；
   *   · 否则按 (alias, target) 找已有映射，找到就改、找不到就**新增**（添加映射）；
   *   · `patch.enabled === false` 且映射不存在 = 调用方在关一条不存在的映射，报错。
   *
   * `patch` 里没给的字段保持现值（与后端「三态」协议同一取向：不带 = 不改）。
   */
  async function setBinding(id, alias, target, patch = {}) {
    const enabled = patch.enabled;
    const reasoning = patch.reasoning;
    return submit(id, draft => {
      if (same(alias, target)) {
        const model = draft.models.find(item => same(item.id, target));
        if (!model) throw new Error(`该提供商的清单里没有模型「${target}」`);
        if (enabled !== undefined) model.enabled = Boolean(enabled);
        if (reasoning !== undefined) model.reasoning = String(reasoning ?? '');
        return null;
      }
      const existing = draft.mappings.find(item => same(item.alias, alias) && same(item.target, target));
      if (existing) {
        if (enabled !== undefined) existing.enabled = Boolean(enabled);
        if (reasoning !== undefined) existing.reasoning = String(reasoning ?? '');
        return null;
      }
      if (enabled === false) throw new Error(`映射「${alias} → ${target}」不存在`);
      draft.mappings.push({
        alias,
        target,
        enabled: enabled !== false,
        reasoning: String(reasoning ?? ''),
      });
      return null;
    });
  }

  /** 删除一条映射（默认绑定不可删 —— 界面上它没有删除按钮） */
  async function removeMapping(id, alias, target) {
    return submit(id, draft => {
      const before = draft.mappings.length;
      draft.mappings = draft.mappings.filter(item => !(same(item.alias, alias) && same(item.target, target)));
      if (draft.mappings.length === before) throw new Error(`映射「${alias} → ${target}」不存在`);
      return null;
    });
  }

  /** 登记一个模型（手动添加走它，判重口径与内置家一致） */
  async function addModel(id, modelId) {
    const value = String(modelId ?? '').trim();
    if (!value) throw new Error('请填写模型 ID');
    return submit(id, draft => {
      if (draft.models.some(model => same(model.id, value))) {
        throw new Error(`模型「${value}」已存在（忽略大小写判重）`);
      }
      draft.models.push({ id: value, enabled: true, reasoning: '' });
      return null;
    });
  }

  /**
   * 批量登记（「获取模型」弹窗的导入按钮）：已存在的跳过，返回实际新增条数。
   * 与 `fetchModels` 的差别只在**来源** —— 那个是服务端代拉上游后全量并入，
   * 这个是用户在弹窗里逐个勾出来的子集。
   */
  async function addModels(id, modelIds) {
    const values = (Array.isArray(modelIds) ? modelIds : [])
      .map(value => String(value ?? '').trim())
      .filter(Boolean);
    if (!values.length) throw new Error('没有选中任何模型');
    return submit(id, draft => {
      let added = 0;
      for (const value of values) {
        if (draft.models.some(model => same(model.id, value))) continue;
        draft.models.push({ id: value, enabled: true, reasoning: '' });
        added++;
      }
      return added;
    });
  }

  /** 移除一个模型：连带删掉 target 指向它的映射，返回删了几条（用于 toast） */
  async function removeModel(id, modelId) {
    return submit(id, draft => {
      const before = draft.mappings.length;
      draft.mappings = draft.mappings.filter(mapping => !same(mapping.target, modelId));
      const removedMappings = before - draft.mappings.length;
      draft.models = draft.models.filter(model => !same(model.id, modelId));
      return { removedMappings };
    });
  }

  /**
   * 从上游拉清单并并入（对应内置家的「刷新模型清单」）：服务端代拉、不落盘，
   * 已存在的 id 跳过（忽略大小写），只提交新增的那些。
   */
  async function fetchModels(id) {
    if (!providers()?.customRequest) throw new Error('目录模块未就绪');
    const data = await providers()?.customRequest('POST', '/api/custom-providers/fetch-models', { providerId: id });
    const ids = (Array.isArray(data?.models) ? data.models : [])
      .map(value => String(value ?? '').trim())
      .filter(Boolean);
    const added = await submit(id, draft => {
      let count = 0;
      for (const value of ids) {
        if (draft.models.some(model => same(model.id, value))) continue;
        draft.models.push({ id: value, enabled: true, reasoning: '' });
        count++;
      }
      return count;
    });
    return { total: ids.length, added: Number(added) || 0 };
  }

  window.wbModelsCustom = {
    isCustom,
    list,
    record,
    buildView,
    setBinding,
    removeMapping,
    addModel,
    addModels,
    removeModel,
    fetchModels,
  };
})();
