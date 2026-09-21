/* Agent2API · 模型管理页：表格渲染 + 筛选 + 启停 / 删除 / 恢复 + 模型映射 + 刷新模型清单 */
/* global workbuddyDesktop, wbApp */

/**
 * 数据来自 `GET /api/models/manage`（`{models, mappings, reasoningLevels}`，含禁用 /
 * 隐藏的条目，每条带 enabled / hidden / aliases）。本模块自持这份数据：写接口都会
 * 返回最新的同形数据，就地替换后重绘，不经过 app.js 的 state（那是 /api/session
 * 的快照，轮询会整份覆盖）。
 *
 * 表格按提供商分组（顺序 = 后端数组顺序 = 路由优先级），每组默认只展开前
 * `GROUP_LIMIT` 行，其余折叠成一行「展开其余 N 个」；有搜索词或非「全部」筛选时
 * 不折叠 —— 用户在找东西，藏起来只会让他以为没有。
 *
 * 映射（照抄 OmniProxy 的模型映射语义）：对外名自由命名（**允许**与上游模型
 * ID 同名 —— 同名时该上游的原生路由优先，映射是追加的兜底路，不产生遮蔽）；
 * 同一对外名可以在多个提供商各建一条（每行的映射 chips 只属于自己那行），
 * 下游用同一个名字请求时，网关在「原生承载家 + 各映射提供商」之间按账号
 * 全局优先级主备切换，发送时按承载家自动换成它认识的真名。
 *
 * ── 思考等级（照抄 OmniProxy 的手动绑定，R7）─────────────────
 * 每条映射可以带一个「思考等级」，值取自**后端下发的** `reasoningLevels`
 * （= `model_rules::REASONING_LEVELS`，与 OmniProxy 的
 * `GENERIC_REASONING_LEVELS` 同一张表；前端不自己抄一份，免得两处漂移），
 * 或表外的自定义值。等级显示在映射 chip 上（`alias → target · high`）。
 * **绑定会真的注入转发**：等级跟着它所在的这条映射走，由承载的那家适配器
 * 翻译成本家上游认识的档位字段（CatPaw 把通用 6 档归并成 low/high/max，Qoder
 * 按模型自己声明的档位归一）。**故意不注入**的几种情形（关闭思考 off/none、
 * 表外自定义值、客户端已显式指定、这家上游不认识档位字段）与理由写在
 * `core::model_rules::reasoning` 的模块头里，界面上有问号如实标注。
 *
 * 跨文件引用一律走 `wbApp`（esc / toast 是 app.js 里的全局单份实现）。
 */

(() => {
  const { esc, toast } = wbApp;
  const $ = id => document.getElementById(id);

  // 提示里不点名哪几家：支持远程目录的家会变（CatPaw / AutoClaw 接入后也支持
  // 刷新了），硬编码名单每加一家就要改一次，而漏改只会给用户一句过时说明。
  // 「谁被跳过」由后端逐家结果里的 `fixed` 标记如实给出（见下方 describeResults）。
  // 五家现在都有远程目录，所以「上游没有目录接口」只作为兜底情形保留措辞
  // （将来新接入的 provider 若走固定清单，`fixed: true` 会命中它）。
  const REFRESH_TITLE = '刷新各提供商的远程模型目录；'
    + '使用固定模型清单的提供商（上游没有目录接口）刷新不会改变它们。'
    + '拉到远程清单后，「来源」列会从「内置」变为「远程」';
  const GROUP_LIMIT = 8;
  /** 思考等级组件（chip 上的等级标 / 弹窗里的下拉）。住在 models-reasoning.js：
      那一块内部自洽（候选表 / 索引 / 那个下拉的读写），拆出去让本文件回到
      表格与映射本身。缺了它（脚本没加载）时下面几个调用点都退化成「不显示
      等级标」—— 看得到的是「少了个功能」，而不是整套面板报错。 */
  const reasoning = window.wbModelsReasoning;

  /** 当前数据（null = 还没拉到） */
  let data = null;
  let loading = false;
  let refreshing = false;
  /** 筛选状态 */
  let providerFilter = 'all';
  let stateFilter = 'all';
  /** 已展开全部行的提供商集合 */
  const expanded = new Set();
  /** 行内操作在途标记：防同一行连点 */
  const pending = new Set();

  // ─── 数据 ─────────────────────────────────

  function models() { return Array.isArray(data?.models) ? data.models : []; }
  function mappings() { return Array.isArray(data?.mappings) ? data.mappings : []; }

  /**
   * 「三元组 → 思考等级」查询闭包（由 `models-reasoning.js` 建）。
   *
   * 必须**每次渲染前重建一次**（见 `rebuildReasoningIndex`）：数据换了索引就得
   * 跟着换，否则用户改完等级、列表重绘，chip 上还是旧的那个字。
   * 初值给一个恒返回空串的闭包 —— 在第一次 render 之前调用它（理论上不会，
   * 但弹窗是独立入口）也不会炸，只是显示成「未绑定」。
   */
  let reasoningOf = () => '';

  function rebuildReasoningIndex() {
    reasoningOf = reasoning?.buildIndex(mappings()) || (() => '');
  }

  /** chip 上那枚等级标（转调 `models-reasoning.js`）。
      缺失该脚本时给空串 —— 少一枚可以点击的标，chip 名字与删除按钮照常，
      不整块报错（与 `reasoningOf` 的兜底同一取舍）。 */
  function badgeHtml(alias, target, provider, busy) {
    if (!reasoning) return '';
    return reasoning.badge({
      alias,
      target,
      provider,
      level: reasoningOf(alias, target, provider),
      busy,
    });
  }

  /** 提供商下拉选项（id + 展示名；按数据里出现的顺序去重） */
  function providerOptions() {
    const seen = new Map();
    models().forEach(m => {
      const key = m.provider || '';
      if (key && !seen.has(key)) seen.set(key, m.providerLabel || key);
    });
    return [...seen].map(([id, label]) => ({ id, label }));
  }

  /**
   * 某一家当前清单里的模型（映射弹窗的「上游模型」下拉数据源）。
   *
   * 含已禁用 / 已删除的行：映射是「名字 → 名字」的静态规则，与启停正交 ——
   * 用户完全可能先建好映射、之后才把那个模型打开。把它们藏起来会让
   * 「为什么我的模型不在下拉里」变成一个查不出的问题。
   * 排序：启用的在前（与表格分组内同一取舍），组内保持后端顺序。
   */
  function upstreamOptions(providerId) {
    if (!providerId) return [];
    return models()
      .filter(m => (m.provider || '') === providerId)
      .sort((a, b) => Number(a.enabled === false || a.hidden === true)
        - Number(b.enabled === false || b.hidden === true))
      .map(m => ({
        id: m.id,
        // 展示名与 id 不同才补在括号里，避免出现「GLM-5.3（GLM-5.3）」这种重复
        label: m.name && m.name !== m.id ? `${m.id}（${m.name}）` : m.id,
        off: m.enabled === false || m.hidden === true,
      }));
  }

  async function load() {
    if (loading) return;
    loading = true;
    try {
      data = await workbuddyDesktop.getModelManage();
      render();
    } catch (error) {
      toast(`读取模型清单失败：${error.message}`, 'err');
    } finally {
      loading = false;
    }
  }

  /** 写接口返回的最新数据直接替换 */
  function accept(next) {
    if (next && Array.isArray(next.models)) data = next;
    render();
  }

  // ─── 渲染 ─────────────────────────────────

  const formatCredits = c => {
    const m = /x\s*([\d.]+)/i.exec(c || '');
    return m ? `${m[1]}x` : (c || '');
  };

  function searchTerm() {
    return ($('model-search')?.value || '').trim().toLowerCase();
  }

  function matches(m, keyword) {
    if (stateFilter === 'hidden' ? !m.hidden : m.hidden) return false;
    if (stateFilter === 'enabled' && !m.enabled) return false;
    if (stateFilter === 'disabled' && m.enabled) return false;
    if (stateFilter === 'mapped' && !(m.aliases || []).length) return false;
    if (providerFilter !== 'all' && (m.provider || '') !== providerFilter) return false;
    if (!keyword) return true;
    const hay = [m.id, m.name, ...(m.aliases || [])].join(' ').toLowerCase();
    return hay.includes(keyword);
  }

  function renderProviderSeg() {
    const seg = $('models-provider-seg');
    if (!seg) return;
    const counts = new Map();
    models().forEach(m => {
      if (m.hidden) return;
      const key = m.provider || '';
      const entry = counts.get(key) || { label: m.providerLabel || key || '未知', n: 0 };
      entry.n++;
      counts.set(key, entry);
    });
    if (providerFilter !== 'all' && !counts.has(providerFilter)) providerFilter = 'all';
    const total = [...counts.values()].reduce((sum, item) => sum + item.n, 0);
    const item = (key, label, n) =>
      `<button class="seg-item${providerFilter === key ? ' active' : ''}" data-provider="${esc(key)}">${esc(label)} <span class="n">${n}</span></button>`;
    seg.innerHTML = item('all', '全部', total)
      + [...counts].map(([key, entry]) => item(key, entry.label, entry.n)).join('');
  }

  /** 映射 chips（照抄 OmniProxy）：每条 chip 属于自己所在的那一行（提供商 ×
      上游模型），删除时带三元组精确定位 —— 同一对外名在多行出现是主备关系。
      绑了思考等级的 chip 在名字后面挂一枚可点的小标（`· high`），点它打开映射
      弹窗改等级：那是这条映射上唯一的**可编辑属性**（alias / target / provider
      是它的身份，改了就变成另一条映射），所以入口就挂在它旁边。
      未绑定时那枚标显示「＋等级」（见 `badgeHtml`）—— 入口要一直看得见。 */
  function aliasChips(m) {
    const provider = m.provider || '';
    const chips = (m.aliases || []).map(alias => `<span class="alias${m.enabled ? '' : ' off'}">`
      + `<span class="t">${esc(alias)}</span>`
      + badgeHtml(alias, m.id, provider, false)
      + `<button type="button" class="x" data-act="unmap" data-alias="${esc(alias)}" data-target="${esc(m.id)}" data-provider="${esc(provider)}" title="删除映射 ${esc(alias)}">×</button></span>`).join('');
    const add = m.hidden ? '' : `<button type="button" class="alias-add" data-act="map" data-id="${esc(m.id)}" data-provider="${esc(provider)}">＋ 映射</button>`;
    return `<div class="aliases">${chips}${add}</div>`;
  }

  function row(m) {
    const busyRow = pending.has(rowKey(m));
    const name = m.name && m.name !== m.id ? `<div class="mname">${esc(m.name)}</div>` : '';
    const credits = m.credits ? `<span class="rate">${esc(formatCredits(m.credits))}</span>` : '<span class="rate">—</span>';
    const actions = m.hidden
      ? `<button type="button" class="sm ghost" data-act="restore" data-id="${esc(m.id)}" data-provider="${esc(m.provider || '')}"${busyRow ? ' disabled' : ''}>恢复</button>`
      : `<button type="button" class="sm ghost danger-text" data-act="hide" data-id="${esc(m.id)}" data-provider="${esc(m.provider || '')}"${busyRow ? ' disabled' : ''}>删除</button>`;
    return `<tr class="${m.enabled && !m.hidden ? '' : 'off'}" data-id="${esc(m.id)}" data-provider="${esc(m.provider || '')}">`
      + `<td><div class="mid"><span class="t">${esc(m.id)}</span>`
      + `<button type="button" class="cp" data-copy="${esc(m.id)}" title="复制模型 ID">⧉</button></div>${name}</td>`
      + `<td>${credits}</td>`
      + `<td>${sourceCell(m)}</td>`
      + `<td>${aliasChips(m)}</td>`
      + `<td class="state"><label class="switch"><input type="checkbox" data-act="toggle" data-id="${esc(m.id)}" data-provider="${esc(m.provider || '')}"${m.enabled ? ' checked' : ''}${m.hidden || busyRow ? ' disabled' : ''}><span class="track"></span></label></td>`
      + `<td class="r"><div class="row-actions">${actions}</div></td></tr>`;
  }

  /** 「来源」列：这一家的清单当前是远程拉的还是内置静态表（后端给的 `source`）。
      它是**家**级属性（同一家所有行同值），前端只做文案映射与样式，不自己推断。
      认不出的值显示破折号：后端没给 `source`（旧版网关）时不该硬说「内置」 */
  function sourceCell(m) {
    if (m.source !== 'remote' && m.source !== 'builtin') return '<span class="rate">—</span>';
    const remote = m.source === 'remote';
    const hint = remote
      ? '来自上游目录接口（刷新失败时保留上一份成功结果）'
      : '上游目录尚未拉到，用的是内置静态清单；点「刷新模型清单」可重试';
    return `<span class="badge tag${remote ? ' brand' : ''}" title="${hint}">${remote ? '远程' : '内置'}</span>`;
  }

  /** 行内操作的防重入键：同名模型在多家同时存在时，`id` 不足以定位一行 */
  function rowKey(m) {
    return `${m.provider || ''}:${m.id}`;
  }

  /**
   * 挂不到任何一行的映射（后端在 `manage_view` 里算好，字段 `dangling`）。
   *
   * 管理页按「提供商 × 上游模型」分行，映射 chip 挂在 (provider, target) 命中的
   * 那一行上。目标模型不在该行清单里时这条映射**没有任何行可以显示**，
   * 于是「保存成功，列表里却找不到它」—— 必须让用户看得见、能删掉。
   *
   * 判据由后端算：后端直接对着它刚构建的那批行问「有没有一行接得住」，
   * 与渲染 chip 的口径逐字同源。前端只有收窄后的广告清单，自己算会与表格
   * 对不上（多标或漏标）。
   *
   * 落进这一组的两种情况，界面上都不该说成「无效」：
   *   - 目标名字真不存在（手输打错、上游下架）→ 确实该删或该改；
   *   - 目标模型存在、路由也认，只是**这家现在不提供它**（清单里没有）→
   *     配置没错，只是这家此刻不广告它；删掉反而会让那个短名路由不到。
   * 所以分组标题用「未挂载」、说明用「不在该家当前清单里」，把判断留给用户。
   */
  function orphanMappings() {
    return mappings().filter(mapping => mapping.dangling === true);
  }

  function render() {
    const body = $('models');
    if (!body) return;
    // 重建思考等级索引（每次渲染一次，见 rebuildReasoningIndex 的说明）
    rebuildReasoningIndex();
    renderProviderSeg();
    const all = models();
    const keyword = searchTerm();
    const shown = all.filter(m => matches(m, keyword));
    // 计数：总数 / 启用数 / 映射数（不受筛选影响，是「这台网关现在的状态」）
    const visible = all.filter(m => !m.hidden);
    const count = $('models-count');
    if (count) {
      // 条数只算「挂上了行的」映射；孤儿映射单独点名 —— 混在一起数会让
      // 「25 条映射」在表格里怎么数都对不上
      const orphans = orphanMappings().length;
      count.textContent = all.length
        ? `${visible.length} 个上游模型 · ${visible.filter(m => m.enabled).length} 已启用 · ${mappings().length - orphans} 条映射`
          + (orphans ? ` · ${orphans} 条未挂载` : '')
          + (all.length !== visible.length ? ` · ${all.length - visible.length} 已删除` : '')
        : '';
    }
    if (!all.length) {
      // 一条模型都没有（没加账号）：此时把映射全列成「未挂载」只是噪音，
      // 「请先添加账号」才是用户该看到的话
      body.innerHTML = `<tr><td colspan="6" class="empty">${data ? '暂无模型（请先添加账号）' : '加载中…'}</td></tr>`;
      return;
    }
    // 孤儿映射不随模型的启停 / 删除筛选走：那些维度是「模型的状态」，
    // 而它们连行都没有；「全部」与「有映射」两个筛选下才列出来。
    // 但**提供商筛选要跟随** —— 见 orphanSection 的说明。
    const orphans = (stateFilter === 'all' || stateFilter === 'mapped') ? orphanSection(keyword) : '';
    if (!shown.length) {
      body.innerHTML = orphans
        || `<tr><td colspan="6" class="empty">没有匹配${keyword ? `「${esc(keyword)}」` : '当前筛选'}的模型</td></tr>`;
      return;
    }
    // 折叠只在「无搜索、全部状态」下生效（见文件头）
    const collapsible = !keyword && stateFilter === 'all';
    const groups = new Map();
    shown.forEach(m => {
      const key = m.provider || '';
      if (!groups.has(key)) groups.set(key, { label: m.providerLabel || key || '未知', items: [] });
      groups.get(key).items.push(m);
    });
    body.innerHTML = [...groups].map(([key, group]) => {
      const open = expanded.has(key) || !collapsible;
      const items = open ? group.items : group.items.slice(0, GROUP_LIMIT);
      const rest = group.items.length - items.length;
      const head = `<tr class="tr-group"><td colspan="6"><span class="prov-tag">${esc(group.label)}</span>${group.items.length} 个模型</td></tr>`;
      const more = rest > 0
        ? `<tr class="tr-more"><td colspan="6"><button type="button" class="sm ghost" data-act="expand" data-provider="${esc(key)}">展开其余 ${rest} 个模型 ▾</button></td></tr>`
        : (open && collapsible && group.items.length > GROUP_LIMIT
          ? `<tr class="tr-more"><td colspan="6"><button type="button" class="sm ghost" data-act="collapse" data-provider="${esc(key)}">收起 ▴</button></td></tr>`
          : '');
      return head + items.map(row).join('') + more;
    }).join('') + orphans;
  }

  /**
   * 「挂不到行的映射」分组（见 [`orphanMappings`]）：只在有这类映射时出现，
   * 排在各家分组之后，表头标签走警示色（`.tr-orphan`）区别于提供商分组。
   *
   * ── 为什么**跟随提供商筛选**（而启停 / 删除筛选不跟随）──────────
   * 顶部那个提供商分段是「我在看哪一家」的视角，用户点「Cline Free」时
   * 期待看到的是**这一家的全部信息**。孤儿映射带 provider（旧版全局条目除外），
   * 所以完全筛得动：不过滤的话，看 Cline Free 时会看到一屏 `cline-pass/*`
   * 的条目，很容易被当成「Cline Free 收 pass 的模型」—— 而它们恰恰是
   * **另一家**的。启停 / 删除那两档不跟随，是因为它们描述的是「模型的状态」，
   * 而孤儿映射连行都没有，套用那些维度没有意义（见调用点）。
   *
   * 旧版全局条目（`provider` 为 null）在**任何一家**的筛选下都列出：它不属于
   * 任何一家，把它藏起来才是骗人（用户会以为那条映射不见了）。
   *
   * ── 分成两档（后端 `carried` 字段）──────────────────────────
   * 落进这一组的映射都挂不到行上，但原因不同、该给用户的建议也相反：
   *   - `carried === false`：这个名字**哪儿都没有**（手输打错、上游下架）。
   *     映射是死的，该改掉或删掉。行头标「无法路由」。
   *   - `carried === true`：名字有效、路由认得，只是这家**现在清单里没有它**
   *     （上游下架了那个模型、或这一家的账号还没加进来）。删掉它反而会让
   *     那个短名路由不到。标「未广告」。
   * 两种都列出来（都看不见行），措辞必须分开 —— 一律说「无效」会误导用户
   * 删掉一条本来正确的配置。
   */
  function orphanSection(keyword) {
    const orphans = orphanMappings().filter(mapping => {
      // 全局条目在任何视角下都在（见函数头）；带 provider 的跟着筛选走
      if (mapping.provider && providerFilter !== 'all' && mapping.provider !== providerFilter) {
        return false;
      }
      if (!keyword) return true;
      return `${mapping.alias} ${mapping.target}`.toLowerCase().includes(keyword);
    });
    if (!orphans.length) return '';
    const head = `<tr class="tr-group tr-orphan"><td colspan="6">`
      + `<span class="prov-tag">未挂载的映射</span>${orphans.length} 条</td></tr>`;
    const rows = orphans.map(mapping => {
      // 展示名从**注册表**查（`wbProviders.labelOf`），不是从表格行里收集：
      // 一条映射挂不上行时，常常正是因为那一家整个没进表格（没加账号），
      // 而 `providerOptions()` 只认表格里出现过的 provider —— 用它就会在
      // 最需要说清「这是哪一家」的时候回落成 provider id（`cline-pass`
      // 这种内部标识，用户认不出）。
      const label = mapping.provider
        ? (window.wbProviders?.labelOf?.(mapping.provider) || mapping.provider)
        : '任意提供商';
      const key = `${mapping.alias}:${mapping.target}:${mapping.provider || ''}`;
      const busy = pending.has(key);
      const triple = `data-alias="${esc(mapping.alias)}"`
        + ` data-target="${esc(mapping.target)}" data-provider="${esc(mapping.provider || '')}"`;
      const del = `data-act="unmap" ${triple}`;
      // 孤儿映射同样能改思考等级：那条绑定跟着映射走，映射在哪儿可编辑、
      // 它的等级就在哪儿可编辑（否则挂不到行的映射反而成了改不了死角的配置）。
      // `data-act` 必须是各自的（不能复用 del 那串）：事件委托按 data-act 分派，
      // 一个按钮挂两个动作会让「点等级」变成「删映射」。
      const chips = `<span class="alias orphan"><span class="t">${esc(mapping.alias)}</span>`
        + badgeHtml(mapping.alias, mapping.target, mapping.provider, busy)
        + `<button type="button" class="x" ${del} title="删除映射 ${esc(mapping.alias)}"${busy ? ' disabled' : ''}>×</button></span>`;
      // 两档的差异全在右半边那句小字上（表格里没有「状态」列可用，也不该为它加一列）
      //
      // 第二档区分「这家没加账号」与「清单里没这个模型」：前者整家不进表格
      // （`providerOptions` 是从行里收集的，没有这家 = 它没进广告），
      // 后者是这家有行、却没有这一行。两种的处理办法不同（去加账号 / 上游确实
      // 下架了），所以值得分开说。
      const hasProviderRows = providerOptions().some(item => item.id === mapping.provider);
      // 三句文案都在这里拼好并**整体转义**：`label` 来自注册表（后端可控），
      // 逐段拼再插进 HTML 会把转义责任散到三处
      const why = esc(mapping.carried === false
        ? '上游模型名不存在于任何提供商'
        : hasProviderRows
          ? `${label} 的清单里没有这个模型`
          : `${label} 还没有账号，它的模型都没有列出`);
      return `<tr class="off" data-id="${esc(mapping.target)}" data-provider="${esc(mapping.provider || '')}">`
        + `<td><div class="mid"><span class="t">${esc(mapping.target)}</span></div>`
        + `<div class="mname">${why}</div></td>`
        + '<td><span class="rate">—</span></td>'
        + '<td><span class="rate">—</span></td>'
        + `<td>${chips}</td>`
        + '<td class="state"><span class="rate">—</span></td>'
        + '<td class="r"><div class="row-actions">'
        + `<button type="button" class="sm ghost danger-text" ${del}${busy ? ' disabled' : ''}>删除映射</button>`
        + '</div></td></tr>';
    }).join('');
    return head + rows;
  }

  // ─── 行内操作 ────────────────────────────

  /**
   * 行内操作执行器。`key` 是防重入标记（提供商:模型 id）——同名模型在多家
   * 同时存在，只按 id 记会把两家的行一起标成「执行中」。
   */
  async function runRowAction(key, run, doneText) {
    if (pending.has(key)) return;
    pending.add(key);
    render();
    try {
      accept(await run());
      if (doneText) toast(doneText);
    } catch (error) {
      toast(`操作失败：${error.message}`, 'err');
      render();
    } finally {
      pending.delete(key);
      render();
    }
  }

  async function onTableClick(event) {
    const button = event.target.closest('[data-act]');
    if (!button) return;
    const { act, id, alias, target, provider } = button.dataset;
    // 同一模型 id 在多家同时存在时（如 kimi-k3 同时由 CatPaw 与小浣熊提供），
    // 启停 / 删除都要带上提供商才能精确到一行
    const key = `${provider || ''}:${id}`;
    if (act === 'expand') { expanded.add(provider); render(); return; }
    if (act === 'collapse') { expanded.delete(provider); render(); return; }
    if (act === 'hide') {
      // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行（等于没有确认）—— 下同
      if (!(await window.wbConfirm?.ask?.({
        title: '删除模型',
        html: `确定删除模型「<strong>${esc(id)}</strong>」？只是从该提供商的清单隐藏，可在「已删除」筛选里恢复。`,
        okText: '删除',
        okClass: 'danger',
      }))) return;
      void runRowAction(key, () => workbuddyDesktop.setModelState({ id, provider, hidden: true }), '模型已删除');
      return;
    }
    if (act === 'restore') {
      void runRowAction(key, () => workbuddyDesktop.setModelState({ id, provider, hidden: false }), '模型已恢复');
      return;
    }
    if (act === 'unmap') {
      // 同名映射允许多条，删除按（对外名 + 上游模型 + 提供商）三元组定位
      if (!(await window.wbConfirm?.ask?.({
        title: '删除映射',
        html: `确定删除映射「<strong>${esc(alias)} → ${esc(target)}</strong>」？`,
        okText: '删除',
        okClass: 'danger',
      }))) return;
      void runRowAction(
        `${alias}:${target}:${provider || ''}`,
        () => workbuddyDesktop.removeModelMapping(alias, target, provider),
        '映射已删除',
      );
      return;
    }
    if (act === 'map') openMapping({ target: id, provider });
    // 点 chip 上的等级标：只改这条映射的思考等级（alias 也在上下文里，弹窗据此
    // 进入锁定形态）。不用先弹确认框 —— 它只写一个字段，保存前还能取消。
    if (act === 'reasoning' && alias && target) {
      openMapping({ alias, target, provider });
    }
  }

  function onTableChange(event) {
    const input = event.target.closest('input[data-act="toggle"]');
    if (!input) return;
    const { id, provider } = input.dataset;
    const enabled = input.checked;
    const key = `${provider || ''}:${id}`;
    void runRowAction(
      key,
      () => workbuddyDesktop.setModelState({ id, provider, enabled }),
      enabled ? '模型已启用' : '模型已禁用',
    );
  }

  // ─── 映射弹窗（照抄 OmniProxy 的模型映射）────────────────

  let mappingSaving = false;
  /** 行内打开时锁定的上下文；顶部按钮打开时为 null。三种入口：
   *  `{target, provider}`（行内「＋映射」）、`{alias, target, provider}`（点 chip
   *  上的等级标，只改等级）、以及不带上下文（顶部「添加映射」）。 */
  let mappingContext = null;

  /** 当前选中的上游模型（下拉值） */
  function upstreamValue() {
    return ($('mapping-upstream')?.value || '').trim();
  }

  /**
   * 把「该家的模型清单」灌进上游下拉（打开时、切换提供商时都走它）。
   *
   * `keep` 不在候选里时**仍把它补进去**（而不是像早先那样退回首项），
   * 但**只在锁定态**（行内入口 / 改等级形态）这么做：
   * 孤儿映射的 target 恰恰常常不在该家清单里（那正是它挂不上行的原因），
   * 而它照样有自己的思考等级要改。丢掉 keep 会让弹窗里显示「这一家的第一个
   * 模型」，用户在「设置思考等级」形态下点保存，三元组就从 (alias, target)
   * 变成 (alias, 另一个模型) —— 命中的是另一条规则（或新建一条），
   * 等级存到了错的地方，而界面上看不出任何异常。
   *
   * 解锁态（顶部「添加映射」自选提供商）不补：那里用户刚换了一家，
   * 上一家的 target 对新家毫无意义，退回首项才是他要的。
   */
  function fillUpstreamSelect(providerId, keep, locked = false) {
    const select = $('mapping-upstream');
    if (!select) return;
    const options = upstreamOptions(providerId);
    const wanted = (keep || '').trim();
    const has = id => options.some(item => item.id.toLowerCase() === id.toLowerCase());
    if (locked && wanted && !has(wanted)) {
      options.unshift({
        id: wanted,
        label: `${wanted}（不在该家当前清单里）`,
        off: true,
      });
    }
    select.innerHTML = options.map(item =>
      `<option value="${esc(item.id)}">${esc(item.label)}${item.off ? '（已禁用）' : ''}</option>`).join('');
    // 记住用户已经选过的那个：切换提供商再切回来时不该被重置
    if (wanted && has(wanted)) {
      // 用候选里的原始拼写（大小写可能与 keep 不同）：value 必须与 option 的
      // value 逐字相同才会被选中
      select.value = options.find(item => item.id.toLowerCase() === wanted.toLowerCase())?.id || wanted;
    } else {
      select.value = options[0]?.id || '';
    }
    window.wbSelect?.sync?.(select);
  }

  /** 弹窗里那个思考等级下拉的两个元素（readonly，不缓存 DOM 引用之外的任何状态） */
  const reasoningSelect = () => $('mapping-reasoning');
  const reasoningCustom = () => $('mapping-reasoning-custom');

  function fillReasoningSelect(keep) {
    reasoning?.fillSelect(reasoningSelect(), reasoningCustom(), reasoning.levels(data), keep);
  }

  function syncReasoningCustom() {
    reasoning?.syncCustom(reasoningSelect(), reasoningCustom());
  }

  /** 当前选择 → 交给后端的值（`''` = 显式清空；永不为 undefined，见文件头） */
  function reasoningValue() {
    return reasoning?.valueOf(reasoningSelect(), reasoningCustom()) || '';
  }

  function mappingPreview() {
    const alias = $('mapping-alias')?.value.trim() || '<对外名>';
    const upstream = upstreamValue() || '<上游模型>';
    const provider = $('mapping-provider')?.value;
    const label = providerOptions().find(item => item.id === provider)?.label || provider || '(全局)';
    const level = reasoningValue();
    const suffix = level ? ` · 思考等级 <b>${esc(level)}</b>` : '';
    $('mapping-preview').innerHTML = `下游请求 <b>${esc(alias)}</b> → 转发 <b>${esc(upstream)}</b>（${esc(label)}）${suffix}`;
  }

  /**
   * 打开映射弹窗。
   * `context` 为行内入口带的上下文（提供商 + 上游模型锁定，只填对外名）；
   * 顶部「添加映射」按钮不传 —— 提供商与上游模型都要自己选。
   *
   * 上游模型**只能是下拉**（数据来自该家当前清单）。这里曾经放开过「手动输入
   * 上游模型 ID」，已删除：对外名只有在**目标模型已被广告**时才会跟着进广告视图
   * （`catalog::models_response` 是「遍历已广告的模型 → 补它的别名」这个方向），
   * 而入口校验以广告视图为准 —— 手输一个清单里没有的名字，映射建了也永远调不通
   * （实测 400 `model_not_found`），只会让用户以为配好了。要用清单外的模型，
   * 得先让它进清单（刷新远程目录 / 修该家的静态表）。
   *
   * `context.alias` 有值时走「只改这条映射的思考等级」形态：alias 与 target
   * 都是那一条的身份，全部锁定，只留等级可动。共用一个弹窗而不是另开一个
   * 「设置等级」的小窗：两者要填的字段完全重合，独立窗口只会让「等级」和
   * 「映射」在界面语言里变成两件不相干的事，而它们本就是一条记录。
   */
  function openMapping(context) {
    mappingContext = context || null;
    // 索引在这里再建一次：本函数是**唯一**读 `reasoningOf` 的地方，而它可能被
    // 非渲染路径调到（行内点击、将来的快捷键）。重建是一遍 Map 填充（几十到
    // 上百项），比「依赖 render() 刚跑过」这条隐式前提划算得多 ——
    // 那个前提一旦不成立，表现是「弹窗里的等级是空的」，而保存时会把用户
    // 已有的绑定静默清掉。
    rebuildReasoningIndex();
    const locked = Boolean(mappingContext);
    /** 改等级形态：alias 也是锁定的（它来自 chip，就是那条映射的对外名） */
    const editing = Boolean(mappingContext?.alias);
    /** 编辑形态的提供商：它就是那条映射的属性，必须能在下拉里选中 */
    const contextProvider = mappingContext?.provider || '';
    const options = providerOptions();
    // ── 为什么要把上下文里那家补进候选（`providerOptions` 收不全）──────
    // `providerOptions()` 是从**表格行**里收集的（有行才有这家），而孤儿映射
    // 恰恰常常属于「整个没进表格」的家（那家没加账号，一个行都没有）。
    // 不补的话 `providerSelect.value = provider` 会静默落到空串
    //（把 value 设成不存在的选项 = 不选中任何项），用户在「设置思考等级」里
    // 点保存就会把 provider 一起发成空 —— 三元组一变，命中的是**另一条**规则
    //（或新建一条 provider 为 null 的旧版全局条目），等级也就存到了错的地方。
    if (contextProvider && !options.some(item => item.id === contextProvider)) {
      options.push({
        id: contextProvider,
        // 展示名走注册表（`wbProviders.labelOf`，查不到原样回显 id）——
        // 与 orphanSection 里那句小字同一口径，不在这里另写一份 id → 名字的映射
        label: window.wbProviders?.labelOf?.(contextProvider) || contextProvider,
      });
    }
    const providerSelect = $('mapping-provider');
    providerSelect.innerHTML = options.map(item =>
      `<option value="${esc(item.id)}">${esc(item.label)}</option>`).join('');
    const provider = contextProvider || options[0]?.id || '';
    providerSelect.value = provider;
    // 锁定 = 行内入口：提供商与上游模型就是这一行，不允许改（改了就变成另一条映射）
    providerSelect.disabled = locked;
    window.wbSelect?.sync?.(providerSelect);

    const select = $('mapping-upstream');
    // 行内入口的 target 就是这一行的模型，必在清单里（那行就来自清单），
    // 所以直接按 keep 灌即可；万一清单在这期间刷新过、目标已不在，仍由
    // fillUpstreamSelect 退回首项 —— 但那种情况上游下拉是禁用的，
    // 用户看到的是「这一行的模型」，不会被误导成别的选择
    // 锁定态的 keep 一定要保住（第三个参数 true，理由见 fillUpstreamSelect）
    fillUpstreamSelect(provider, locked ? mappingContext.target : '', locked);
    // 上游下拉在锁定态也不可改：它就是这一行
    select.disabled = locked;
    window.wbSelect?.sync?.(select);

    const aliasInput = $('mapping-alias');
    aliasInput.value = editing ? mappingContext.alias : '';
    aliasInput.disabled = editing;
    // 等级回填：改等级形态用 chip 上那条映射的现值；新建形态一律「不覆盖」
    //（照抄 OmniProxy 的「no override」默认值 —— 不替用户绑一个他没选的档位）
    const current = editing ? reasoningOf(mappingContext.alias, mappingContext.target, provider) : '';
    fillReasoningSelect(current);

    $('mapping-modal-status').textContent = '';
    $('mapping-modal-title').textContent = editing
      ? '设置思考等级'
      : locked ? '添加模型映射' : '添加模型映射（自选提供商与上游）';
    $('mapping-modal-save').textContent = editing ? '保存等级' : '保存映射';
    mappingPreview();
    $('mapping-modal').classList.add('open');
    setTimeout(() => {
      // 改等级形态没别的可填，把焦点直接放在等级下拉上
      if (editing) $('mapping-reasoning')?.focus();
      else aliasInput.focus();
    }, 0);
  }

  function closeMapping() {
    if (mappingSaving) return;
    $('mapping-modal').classList.remove('open');
  }

  async function saveMapping() {
    if (mappingSaving) return;
    const editing = Boolean(mappingContext?.alias);
    const alias = editing ? mappingContext.alias : $('mapping-alias').value.trim();
    const target = upstreamValue();
    const provider = $('mapping-provider').value;
    const reasoning = reasoningValue();
    const status = $('mapping-modal-status');
    if (!alias) { status.textContent = '请填写对外映射名'; return; }
    // 下拉为空 = 这一家清单里一个模型都没有（还没加账号 / 清单没拉到）
    if (!target) { status.textContent = '该提供商当前没有可选的上游模型'; return; }
    if (!provider) { status.textContent = '请选择提供商'; return; }
    mappingSaving = true;
    $('mapping-modal-save').disabled = true;
    status.textContent = '保存中…';
    try {
      // 第 4 个参数**总是显式给出**（空串 = 清空绑定）：
      // 「三元组相同」走的也是这条接口，而用户在这个弹窗里看到的就是他要的结果 ——
      // 传 undefined（= 不改）会让「从 high 改成不覆盖」这一步静默无效。
      accept(await workbuddyDesktop.addModelMapping(alias, target, provider, reasoning));
      mappingSaving = false;
      closeMapping();
      const suffix = reasoning ? ` · 思考等级 ${reasoning}` : '';
      toast(editing ? `✅ 已更新 ${alias} 的思考等级` : `✅ 已添加映射 ${alias} → ${target}（${provider}）${suffix}`);
    } catch (error) {
      status.textContent = `保存失败：${error.message}`;
    } finally {
      mappingSaving = false;
      $('mapping-modal-save').disabled = false;
    }
  }

  // ─── 刷新模型清单 ─────────────────────────────

  function describeResults(result) {
    const results = Array.isArray(result?.results) ? result.results : [];
    const done = results.filter(item => item.status === 'refreshed');
    const failed = results.filter(item => item.status === 'failed');
    const skipped = results.filter(item => item.status === 'skipped');
    const parts = [];
    if (done.length) {
      parts.push(`已刷新 ${done.map(item =>
        `${item.providerLabel || item.provider}（${Number(item.count) || 0} 个）`).join('、')}`);
    }
    if (failed.length) {
      const first = failed[0];
      const more = failed.length > 1 ? `（另有 ${failed.length - 1} 家失败）` : '';
      parts.push(`失败 ${failed.map(item => item.providerLabel || item.provider).join('、')}：`
        + `${first.message || '原因未知'}${more}`);
    }
    if (skipped.length) {
      // 「固定清单」与「本次没取到新内容」分开说；判据用后端的 `fixed` 标记而不是文案
      const fixed = skipped.filter(item => item.fixed === true);
      if (fixed.length) {
        parts.push(`跳过 ${fixed.length} 家（${fixed.map(item =>
          item.providerLabel || item.provider).join('、')} 使用固定模型清单，无可刷新）`);
      }
      const rest = skipped.length - fixed.length;
      if (rest > 0) parts.push(`另有 ${rest} 家本次没有取到新清单`);
    }
    if (!parts.length) return '刷新完成，但没有得到任何结果';
    return parts.join('；');
  }

  /** 逐家明细写进表格下方的小字（长期可见，toast 3.5 秒就没了） */
  function paintNote(result) {
    const note = $('models-refresh-note');
    if (!note) return;
    const results = Array.isArray(result?.results) ? result.results : [];
    if (!results.length) {
      note.textContent = '';
      note.className = 'models-refresh-note';
      return;
    }
    const failed = results.filter(item => item.status === 'failed').length;
    note.className = failed ? 'models-refresh-note err' : 'models-refresh-note';
    note.textContent = results.map(item => {
      const label = item.providerLabel || item.provider;
      if (item.status === 'refreshed') return `${label} ✓ ${Number(item.count) || 0} 个`;
      if (item.status === 'failed') return `${label} ✗ ${item.message || '刷新失败'}`;
      return `${label} — ${item.message || '未刷新'}`;
    }).join(' · ');
  }

  async function refreshModels() {
    if (refreshing) return;
    const button = $('btn-refresh-models');
    const label = button?.textContent;
    refreshing = true;
    if (button) {
      button.disabled = true;
      button.textContent = '刷新中…';
    }
    try {
      const result = await workbuddyDesktop.refreshModels();
      paintNote(result);
      const failed = Number(result?.failed) || 0;
      toast(describeResults(result), failed ? 'err' : 'ok');
      // 刷新响应里的清单是 /api/session 形状（不带启停 / 映射标记），管理页重拉一次
      await load();
    } catch (error) {
      const note = $('models-refresh-note');
      if (note) {
        note.className = 'models-refresh-note err';
        note.textContent = `⚠️ 刷新请求失败：${error.message}`;
      }
      toast(`刷新失败：${error.message}`, 'err');
    } finally {
      refreshing = false;
      if (button) {
        button.disabled = false;
        button.textContent = label || '刷新模型清单';
      }
    }
  }

  // ─── 绑定 ─────────────────────────────

  $('model-search')?.addEventListener('input', render);
  $('models')?.addEventListener('click', onTableClick);
  $('models')?.addEventListener('change', onTableChange);
  $('models-provider-seg')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-provider]');
    if (!item) return;
    providerFilter = item.dataset.provider;
    render();
  });
  $('models-state-seg')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-state]');
    if (!item) return;
    stateFilter = item.dataset.state;
    $('models-state-seg').querySelectorAll('.seg-item').forEach(el => el.classList.toggle('active', el === item));
    render();
  });
  $('btn-add-mapping')?.addEventListener('click', () => openMapping());
  $('mapping-modal-close')?.addEventListener('click', closeMapping);
  $('mapping-modal-cancel')?.addEventListener('click', closeMapping);
  $('mapping-modal-save')?.addEventListener('click', () => { void saveMapping(); });
  $('mapping-alias')?.addEventListener('input', mappingPreview);
  $('mapping-alias')?.addEventListener('keydown', event => { if (event.key === 'Enter') void saveMapping(); });
  $('mapping-provider')?.addEventListener('change', () => {
    // 换了一家，上游候选整体换掉（保留同名项，切回来时不用重选）
    fillUpstreamSelect($('mapping-provider').value, upstreamValue());
    mappingPreview();
  });
  $('mapping-upstream')?.addEventListener('change', mappingPreview);
  $('mapping-upstream')?.addEventListener('keydown', event => { if (event.key === 'Enter') void saveMapping(); });
  // 等级下拉：选中「自定义等级」时露出输入框；其余值直接进预览
  $('mapping-reasoning')?.addEventListener('change', () => {
    syncReasoningCustom();
    mappingPreview();
  });
  $('mapping-reasoning-custom')?.addEventListener('input', mappingPreview);
  $('mapping-reasoning-custom')?.addEventListener('keydown', event => { if (event.key === 'Enter') void saveMapping(); });
  $('mapping-modal')?.addEventListener('click', event => { if (event.target === $('mapping-modal')) closeMapping(); });

  const refreshButton = $('btn-refresh-models');
  if (refreshButton) {
    refreshButton.title = REFRESH_TITLE;
    refreshButton.addEventListener('click', () => { void refreshModels(); });
  }
  const refreshHint = $('models-refresh-hint');
  if (refreshHint) refreshHint.textContent = REFRESH_TITLE;

  // 首次进入模型管理页时 load()；app.js 的 render() 只触发重绘（数据自持）
  window.wbModelsPanel = { render, load, refreshModels };
  // app.js 末尾的 showPage() 跑在本文件之前：启动时若记住的就是本页，那次调用
  // 拿不到 wbModelsPanel，这里补拉一次
  if (wbApp.currentPage === 'gateway') void load();
})();
