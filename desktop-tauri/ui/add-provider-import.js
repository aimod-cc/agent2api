/* Agent2API · 「登录 / 添加账号」弹窗 → 「导入」分段：从其他工具批量导入供应商 */
/* global wbApp */

/**
 * 「从其他工具导入」面板的实现。当前来源只有 **cc-switch**（后端
 * `GET /api/import/cc-switch` 扫描本机 SQLite，见 core::import_ccswitch），
 * new-api / sub2api 待续 —— 届时在 scan 结果上并成多来源列表即可，本文件的
 * 「扫描 → 勾选 → 批量创建」三段结构不用动。
 *
 * ── 与 add-provider-forms.js 的分工 ──────────────────────────
 * forms 拥有第 1 步的骨架（分段控件、搜索、卡片网格、步骤切换）与底部操作条；
 * 本文件拥有「导入」这一屏的全部内容：面板 DOM 与「导入所选」按钮由 `mount`
 * 注入（与 add-custom-provider.js 把自己的主按钮搬进底部条同一手法），
 * 扫描 / 勾选 / 批量创建都在这里。对外只暴露三个入口：
 *
 *   mount({host, before, footActions})  装面板与底部按钮（forms 的 mountAddProviderUi 调）
 *   setSegment(on)                       分段是否停在「导入」（面板显隐 + 惰性扫描）
 *   setActive(on)                        底部按钮是否该亮（在第 1 步且停在导入段）
 *
 * 这样分段取值（TYPE_IMPORT）只留在 forms 侧：本文件不关心它叫什么。
 *
 * ── 为什么扫描与导入都在第 1 步完成 ──────────────────────────
 * 导入进来的每一家都对应一个「新建自定义提供商 + 首个账号」，没有需要用户
 * 逐条填的字段（名称 / 协议 / Base URL / Key 全部来自被导入的配置），所以
 * 不点卡片进第 2 步表单 —— 勾选完直接提交。
 *
 * ── 依赖 ────────────────────────────────────────────────────
 * wbApp（esc / toast / refresh）、wbProviders.customRequest、Tauri 的 api_request。
 */
(() => {
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  /** 面板与底部按钮的 DOM id（由本文件创建，外部不再拼这些字符串） */
  const PANEL_ID = 'add-import-panel';
  const LIST_ID = 'add-import-list';
  const STATUS_ID = 'add-import-status';
  const FOOT_BUTTON_ID = 'import-foot-button';
  /** 重扫按钮（面板状态区里的行内按钮） */
  const RESCAN_ID = 'add-import-rescan';

  /** 扫描状态机：'' = 还没扫过（首次切入时触发）；'loading'；'done' */
  let scanState = '';
  /** 最近一次扫描的响应体（{available, path, reason, providers}） */
  let scan = null;
  /** 勾选集合（扫描条目 id；只有 importable 的条目会出现在集合里） */
  const checked = new Set();
  /** 导入进行中：与底部条的其他提交动作同一把锁的语义（各自独立，不共享） */
  let busy = false;
  /** 分段是否停在「导入」（面板显隐 + 决定要不要惰性扫描） */
  let segmentOn = false;
  /** 底部按钮是否该亮（第 1 步 + 导入段；第 2 步由各家的按钮占位） */
  let footOn = false;
  /** 底部操作条是不是被本模块点亮的（只有自己点亮的才由自己收回去，见 syncFoot） */
  let footLit = false;

  const request = (method, path, body) =>
    window.wbProviders?.customRequest?.(method, path, body);

  // ─── 装载 ────────────────────────────────────────────────

  /**
   * 把面板插进第 1 步、把「导入所选」按钮搬进底部操作条。由
   * add-provider-forms.js 的 mountAddProviderUi 在骨架建好后调（DOM 必已就绪）。
   *
   * `host` / `before`：面板插在卡片网格的位置上（与网格互斥显隐），
   * 放在网格之前 —— 两屏内容同一块区域，滚动位置也就跟着换，不会串。
   */
  function mount({ host, before, footActions } = {}) {
    if (host && !$(PANEL_ID)) {
      const panel = document.createElement('div');
      panel.id = PANEL_ID;
      panel.hidden = true;
      panel.innerHTML = `<div id="${STATUS_ID}"></div><div id="${LIST_ID}"></div>`;
      host.insertBefore(panel, before || null);
    }
    if (footActions && !$(FOOT_BUTTON_ID)) {
      const button = document.createElement('button');
      button.type = 'button';
      button.id = FOOT_BUTTON_ID;
      button.className = 'primary';
      button.textContent = '导入所选';
      button.hidden = true;
      button.addEventListener('click', () => { void run(); });
      footActions.appendChild(button);
    }
  }

  /** 分段切入 / 切出：面板显隐 + 首次切入触发扫描（之后再切回不重扫，勾选原样保留） */
  function setSegment(on) {
    segmentOn = Boolean(on);
    const panel = $(PANEL_ID);
    if (panel) panel.hidden = !segmentOn;
    if (segmentOn) void runScan();
  }

  /** 底部按钮该不该亮（forms 在步骤 / 分段变化时调） */
  function setActive(on) {
    footOn = Boolean(on);
    syncFoot();
  }

  // ─── 扫描 ────────────────────────────────────────────────

  /** 扫描本机 cc-switch（失败在状态区给原因，不 toast —— 空态本身就是结果） */
  async function runScan({ force = false } = {}) {
    if (busy) return;
    const status = $(STATUS_ID);
    const list = $(LIST_ID);
    if (!status || !list) return;
    if (scanState === 'loading' || (scanState === 'done' && !force)) return;
    scanState = 'loading';
    status.textContent = '正在扫描本机 cc-switch…';
    list.innerHTML = '';
    try {
      const data = await request('GET', '/api/import/cc-switch');
      scan = data || { available: false, reason: '响应为空' };
    } catch (error) {
      scan = {
        available: false,
        reason: error instanceof Error ? error.message : String(error),
      };
    }
    scanState = 'done';
    renderPanel();
  }

  /** 协议值的展示名（扫描条目上的小徽标） */
  const protocolLabel = protocol =>
    (protocol === 'anthropic' ? 'Anthropic' : protocol === 'responses' ? 'Responses' : protocol);

  /** 行副标题里那句「模型 …」：清单会长（档位覆盖可能好几条），超过两个就折叠成「等 N 个」 */
  function modelNote(item) {
    const models = (Array.isArray(item?.models) ? item.models : [])
      .map(entry => String(entry || '').trim())
      .filter(Boolean);
    if (!models.length) return '';
    const head = models.slice(0, 2).join('、');
    return ` · 模型 ${head}${models.length > 2 ? ` 等 ${models.length} 个` : ''}`;
  }

  /** 渲染扫描结果：状态行（来源说明 / 空态原因）+ 勾选列表 */
  function renderPanel() {
    const status = $(STATUS_ID);
    const list = $(LIST_ID);
    if (!status || !list) return;
    if (!scan || !scan.available) {
      status.innerHTML = `<div class="add-import-empty">`
        + `<p>${esc(scan?.reason || '未能读取 cc-switch 数据')}</p>`
        + `<button type="button" id="${RESCAN_ID}">重新扫描</button>`
        + `</div>`;
      $(RESCAN_ID)?.addEventListener('click', () => { void runScan({ force: true }); });
      list.innerHTML = '';
      syncFoot();
      return;
    }
    const providers = Array.isArray(scan.providers) ? scan.providers : [];
    const importable = providers.filter(item => item.importable === true);
    // 默认全选可导入项（首次扫描时）；之后的重扫保留用户已勾的
    if (!checked.size) {
      for (const item of importable) checked.add(String(item.id));
    }
    status.innerHTML = `<div class="add-import-head">`
      + `<span>在 cc-switch 里找到 <b>${providers.length}</b> 条配置，`
      + `其中 <b>${importable.length}</b> 条可导入</span>`
      + `<button type="button" id="${RESCAN_ID}" class="linkish">重新扫描</button>`
      + `</div>`;
    $(RESCAN_ID)?.addEventListener('click', () => { void runScan({ force: true }); });

    list.innerHTML = providers.map(item => {
      const id = String(item.id);
      const canImport = item.importable === true;
      const isChecked = checked.has(id);
      const meta = canImport
        // 模型名一并展示：它会被登记成新家的初始清单，用户看得见「带过来了什么」
        ? esc(String(item.baseUrl || '') + modelNote(item))
        : esc(String(item.reason || '不支持导入'));
      const badge = canImport
        ? `<span class="add-import-badge">${esc(protocolLabel(item.protocol))}</span>`
        : '';
      return `<label class="add-import-row${canImport ? '' : ' disabled'}" title="${esc(canImport ? String(item.notes || '') : String(item.reason || ''))}">`
        + (canImport
          ? `<input type="checkbox" data-import-id="${esc(id)}"${isChecked ? ' checked' : ''}>`
          : `<span class="add-import-dash"></span>`)
        + `<span class="add-import-info">`
        + `<span class="add-import-name">${esc(String(item.name || id))}${badge}</span>`
        + `<span class="add-import-meta">${meta}</span>`
        + `</span>`
        + `</label>`;
    }).join('');

    list.querySelectorAll('input[type="checkbox"][data-import-id]').forEach(box => {
      box.addEventListener('change', () => {
        const id = box.dataset.importId || '';
        if (box.checked) checked.add(id);
        else checked.delete(id);
        syncFoot();
      });
    });
    syncFoot();
  }

  /**
   * 底部条上的「导入所选」按钮：可见性、文案（带数量）与禁用态。
   *
   * 底部操作条本身默认由 forms 的 syncAddProvider 收起（它管所有步骤 / 块的
   * 切换），谁把自己的主按钮搬进来谁负责点亮 —— 自定义块在 onShow 里
   * `foot.hidden = false`，这里是同一手法。**只收自己点亮的**（footLit）：
   * 第 2 步的 foot 归自定义块管，本模块的 setActive(false) 在那里被调到时
   * 不能把别人点亮的条收掉。
   */
  function syncFoot() {
    const foot = $('add-foot');
    const button = $(FOOT_BUTTON_ID);
    if (!foot || !button) return;
    button.hidden = !footOn;
    if (footOn) {
      foot.hidden = false;
      footLit = true;
    } else if (footLit) {
      foot.hidden = true;
      footLit = false;
    }
    if (!footOn) return;
    if (checked.size) {
      button.disabled = busy;
      button.textContent = busy ? '导入中…' : `导入所选（${checked.size}）`;
    } else {
      button.disabled = true;
      button.textContent = '导入所选';
    }
  }

  // ─── 导入 ────────────────────────────────────────────────

  /**
   * 执行导入：把勾选的每条 cc-switch 配置创建成自定义提供商 + 首个账号
   * （复用 POST /api/custom-providers，一条请求建齐）。逐条提交、逐条记账：
   * 单条失败不拖累其余（失败的行标出原因留在列表里可重试），全部成功才
   * 关弹窗 —— 有失败时留着界面，用户能看见差在哪。
   */
  async function run() {
    const selected = (Array.isArray(scan?.providers) ? scan.providers : [])
      .filter(item => item.importable === true && checked.has(String(item.id)));
    if (!selected.length || busy) return;
    busy = true;
    clearFootHint();
    syncFoot();
    let ok = 0;
    const failed = [];
    for (const item of selected) {
      const payload = {
        name: String(item.name || ''),
        protocol: String(item.protocol || 'chat_completions'),
        baseUrl: String(item.baseUrl || ''),
        apiKey: String(item.apiKey || ''),
      };
      try {
        const created = await request('POST', '/api/custom-providers', payload);
        await saveModels(created, item);
        checked.delete(String(item.id));
        ok += 1;
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        failed.push({ item, message });
        // 失败的行原地标出原因（灰显 + 说明），不再参与本轮
        const row = $(LIST_ID)?.querySelector(`input[data-import-id="${CSS.escape(String(item.id))}"]`)
          ?.closest('.add-import-row');
        if (row) {
          row.classList.add('disabled');
          const meta = row.querySelector('.add-import-meta');
          if (meta) meta.textContent = `导入失败：${message}`;
        }
      }
    }
    busy = false;
    // 有成功的就作废扫描缓存：下次进入本段重扫一次，刚导入的家会被后端判成
    // 「同名跳过」而不是又一份「可导入」—— 否则用户再点一次就是重复创建。
    if (ok > 0) scanState = '';
    if (ok > 0) {
      // 目录与账号列表都要刷：账号行 / 筛选器 / 模型管理页左栏都读它们
      void window.wbProviders?.refreshCustom?.();
      await wbApp.refresh?.();
    }
    if (ok > 0 && !failed.length) {
      $('add-modal')?.classList.remove('open');
      toast(`✅ 已从 cc-switch 导入 ${ok} 个供应商`);
    } else if (ok > 0) {
      toast(`已导入 ${ok} 个，失败 ${failed.length} 个（失败项已标注在列表里）`, 'err');
      syncFoot();
    } else {
      showFootHint(failed[0]?.message || '导入失败');
      syncFoot();
    }
  }

  /**
   * 把 cc-switch 配置里配的模型登记成新家的**初始模型清单**（含 `[1M]` 别名映射）。
   *
   * 为什么必须做：自定义家的模型清单是路由的前提 —— 没登记过的模型名会被
   * 直接拒掉（`custom_providers::bindings::resolve` 只在 models / mappings
   * 里找），前端「模型管理」页要手动登记、「获取模型」也要用户主动去点。
   * cc-switch 里 claude 类有 `ANTHROPIC_MODEL` 与档位覆盖、codex 类有
   * `model`，都是用户当时在用的模型名，顺手带过来新家开箱能用。
   *
   * `modelMappings` 那条：Claude Code 发的模型名可能带 `[1M]` 本地能力标记
   * （上游不认，后端已剥成清单里的上游名），把带标记的原名登记成
   * alias → 上游名，客户端发带标记的名字也能路由到。
   *
   * 清单写失败**不推翻已建的家**：提供商与账号已经落盘，模型管理页可以补
   * 登记；这里失败只留一条控制台记录，导入计数照常 +1。
   */
  async function saveModels(created, item) {
    const providerId = created?.provider?.id;
    const models = (Array.isArray(item?.models) ? item.models : [])
      .map(entry => String(entry || '').trim())
      .filter(Boolean)
      .map(id => ({ id, enabled: true, reasoning: '' }));
    if (!providerId || !models.length) return;
    const mappings = (Array.isArray(item?.modelMappings) ? item.modelMappings : [])
      .map(pair => ({
        alias: String(pair?.alias || '').trim(),
        target: String(pair?.target || '').trim(),
      }))
      .filter(pair => pair.alias && pair.target
        && pair.alias.toLowerCase() !== pair.target.toLowerCase())
      .map(pair => ({ ...pair, enabled: true, reasoning: '' }));
    try {
      await request('POST', '/api/custom-providers/models', { providerId, models, mappings });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      console.warn('导入时写入初始模型清单失败:', message);
    }
  }

  // ─── 底部提示位（与 add-custom-provider.js 的同一落点、同一手法）───

  function showFootHint(message) {
    const hint = $('add-foot-hint');
    if (hint) { hint.textContent = message; hint.classList.add('err'); }
  }

  function clearFootHint() {
    const hint = $('add-foot-hint');
    if (hint) { hint.textContent = ''; hint.classList.remove('err'); }
  }

  window.wbAddImport = { mount, setSegment, setActive };
})();
