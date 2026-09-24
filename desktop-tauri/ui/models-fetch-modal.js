/* Agent2API · 「获取模型」弹窗（模型管理页那颗按钮的本体） */
/* global workbuddyDesktop, wbApp */

/**
 * 点「获取模型」打开的弹窗，形态对照 OmniProxy 的 ModelDiscoveryModal
 *（`D:\Code\OmniProxy\src\pages\upstreams\ModelDiscoveryModal.tsx`）：
 * 拉一份上游模型清单 → 勾选要哪些 → 导入。
 *
 * ── 两种提供商，两种形态 ──────────────────────────────
 * · **自定义提供商**：清单是用户逐条登记的，所以「拉取 → 勾选 → 导入」完整成立 ——
 *   拉回来的 id 与本地清单比对，已在清单里的标「已添加」且复选框**禁用**（不重复导入），
 *   默认勾选未添加的那些；导入走整表提交（models-custom-source.js 的 addModels）。
 * · **内置提供商**：清单来自平台统一目录，上游有什么就自动有什么，没有「导入」这一步 ——
 *   弹窗退化成「刷新各家的远程目录 + 把逐家结果列清楚」。原先只有一句 toast
 *   （3.5 秒就没了）加表格下方一行小字，失败原因来不及看；现在收进一张表里。
 *
 * 弹窗按需创建、关闭即移除（与其它弹窗同一手法），不往 index.html 里常驻空弹窗。
 */
(() => {
  const { esc, toast } = wbApp;
  const $ = id => document.getElementById(id);

  const MODAL_ID = 'fetch-models-modal';
  /** 判重口径：去空白 + 大小写不敏感（与后端及其它入口一致） */
  const norm = value => String(value ?? '').trim().toLowerCase();

  /**
   * 当前会话，`null` = 弹窗没开：
   * `{ providerId, custom, name }` —— `custom` 决定走哪一种形态。
   */
  let session = null;
  /** 自定义家：上游拉回来的模型 id（去重、保序） */
  let upstream = [];
  /** 自定义家：本地清单里已有的 id（小写集合，判「已添加」） */
  let managed = new Set();
  /** 自定义家：勾选中的 id（原样大小写，导入时按它提交） */
  let picked = new Set();
  /** 在途标志（拉取 / 导入）：置真期间不许关窗，关掉会让「到底成没成」变成未知 */
  let busy = false;

  /**
   * 表格的两种骨架（两个形态各一张表、各存一份列宽，见 table-columns.js 的登记）：
   * id 供列宽层定位；COLS 是列 key 的**顺序表**，<colgroup> 按它建。
   * 表头 th 上的 data-col 与 col 的 data-col 同名 —— 列宽恢复（写 col）、
   * 把手定位（找 th）、拖动反查（认列）三处都靠这对属性对齐。
   *
   * TABLE_ID 是 DOM 元素 id，TABLE_COL_ID 是列宽登记 id（table-columns.js 的
   * TABLES，与元素 id 是两个命名空间 —— 登记按「表」而不是「元素」记账，
   * 同一登记的元素会在弹窗重建中换好几茬）。
   */
  const TABLE_ID = { intl: 'fm-table-intl', custom: 'fm-table-custom' };
  const TABLE_COL_ID = { intl: 'fetch-models', custom: 'fetch-models-custom' };
  const COLS_INTL = ['provider', 'source', 'state', 'count', 'note'];
  const COLS_CUSTOM = ['pick', 'model', 'state'];

  /**
   * 表格骨架 / 表头重建后，请列宽层重画一次：恢复拖过的列宽、补拖动把手、
   * 绑拖动事件（表和表头都是随弹窗重建的，把手会随表头一起消失 —— 每轮
   * renderHead 后都必须补，见 table-columns.js 的 repaint 说明）。
   */
  function repaintTable() {
    if (!session) return;
    window.wbTableColumns?.repaint?.(session.custom ? TABLE_COL_ID.custom : TABLE_COL_ID.intl);
  }

  /** 该家当前清单里的模型 id（读目录缓存；自定义家才有意义） */
  function managedIds(providerId) {
    const provider = (window.wbProviders?.customList?.() || []).find(item => item.id === providerId);
    return new Set((Array.isArray(provider?.models) ? provider.models : [])
      .map(model => norm(model?.id)).filter(Boolean));
  }

  /** 「模型来源」的落盘键：`{providerId: accountId}` —— 用户换过账号就记住，
      下次打开弹窗、「重新获取」都以它为准（见 sourceMap 的优先级说明） */
  const SOURCE_KEY = 'workbuddy-desktop-fetch-model-source';
  const savedSource = window.wbFilterMemory
    ? window.wbFilterMemory.load(SOURCE_KEY, {})
    : {};

  /**
   * 目录刷新**不走账号维度**的家（与后端 `ProviderAdapter::refresh_uses_account`
   * 的 false 集合同源）：Cline 的清单接口无鉴权、内容是全局的，「用哪个账号去拉」
   * 对它没有意义 —— 那一列显示为空占位，而不是给一个选了也一样的下拉。
   */
  const ACCOUNTLESS_REFRESH = new Set(['cline-free', 'cline-pass']);

  /** 账号的展示名：与账号页的主名口径同源 —— 以邮箱报名字的家（Qoder /
      AutoClaw 国际版，见 accounts-groups 的 emailAsName）直接用邮箱，
      其余取昵称 / 名称 / 邮箱 / id；缺邮箱时 emailAsName 的家回落名字。 */
  function accountLabel(account) {
    const email = String(account?.email || '').trim();
    const features = window.wbAccountsModel?.providerFeatures?.(account?.provider);
    if (features?.emailAsName && email) return email;
    const name = String(account?.nickname || account?.name || email || account?.id || '').trim();
    return name || '未命名账号';
  }

  /**
   * 该家可用于拉取目录的账号（「模型来源」下拉的选项）。
   *
   * 过滤 = `available`（后端口径：启用 + 有凭证）；排序用账号页同一条
   * `byPriorityOrder`（前端渲染层与后端 `order_key` 是同构实现，两边算出的
   * 「第一个账号」必然是同一条）。数据直接读主状态的账号列表，不另拉接口。
   */
  function sourceAccounts(providerId) {
    if (ACCOUNTLESS_REFRESH.has(providerId)) return [];
    const accounts = wbApp.getState?.()?.accounts?.accounts || [];
    const order = window.wbAccountsModel?.byPriorityOrder;
    const list = accounts.filter(account => (account?.provider || 'workbuddy') === providerId
      && account?.available !== false && account?.enabled !== false);
    return order ? list.sort(order) : list;
  }

  /**
   * 本次刷新「每家点名用哪个账号」的映射（请求体里的 `accounts`）。
   *
   * 优先级：**表体下拉的当前值**（用户刚改的）→ **落盘的记忆**（上次改的）
   * → 不带该家（后端按默认选取 = 队首可用账号）。下拉是每次刷新后重画的，
   * 首次打开弹窗时表体还没有下拉，此时靠落盘值把「上次的选择」带给后端。
   */
  function sourceMap() {
    const map = {};
    for (const [providerId, accountId] of Object.entries(savedSource)) {
      if (accountId) map[providerId] = accountId;
    }
    document.querySelectorAll('#fm-tbody select[data-provider]').forEach(select => {
      if (select.value) map[select.dataset.provider] = select.value;
    });
    return map;
  }

  /**
   * 本次刷新的范围：**模型管理页左栏实有清单的家 ∪ 有启用账号的家**。
   *
   * 与「模型来源」下拉同一哲学（见 sourceAccounts）：用户在界面上都看不到的
   * 家（没有启用账号、清单也为空）不该参加刷新 —— 刷它只会得到一行「缺少
   * 登录态」，而用户既没有账号可选、列表里也根本没有这家，无从理解。名单随
   * 请求带给后端（见 api/models.rs 的 providers）：名单外的家不打网络、也不
   * 进结果。
   *
   * 「有启用账号的家」并进来，是为「刚加完账号、清单还是空的」这一档：它暂时
   * 不在左栏，但用户马上就该看到它 —— 刷一次清单就有了。左栏数据缺席（页面
   * 数据未加载）时不降级成全量：宁缺毋滥，自动刷新任务会兜底。
   */
  function scopeProviders() {
    const ids = new Set(window.wbModelsPanel?.builtinProviders?.() || []);
    for (const account of wbApp.getState?.()?.accounts?.accounts || []) {
      if (account?.available === false || account?.enabled === false) continue;
      ids.add(account?.provider || 'workbuddy');
    }
    return [...ids];
  }

  // ─── 骨架 ──────────────────────────────────

  function close() {
    if (busy) return;
    $(MODAL_ID)?.remove();
    session = null;
    upstream = [];
    picked = new Set();
    managed = new Set();
  }

  /** 按当前 session 拼出弹窗壳（两种形态共用；表体各自渲染） */
  function buildShell() {
    const custom = session.custom;
    const mask = document.createElement('div');
    mask.id = MODAL_ID;
    mask.className = 'modal-mask open';
    mask.setAttribute('role', 'dialog');
    mask.setAttribute('aria-modal', 'true');
    mask.setAttribute('aria-labelledby', 'fm-title');
    mask.innerHTML = `<div class="modal ${custom ? 'modal-narrow' : 'modal-wide'}">
        <div class="modal-head">
          <h2 id="fm-title">获取模型 — ${esc(session.name)}</h2>
          <button type="button" id="fm-close" title="关闭">✕</button>
        </div>
        <div class="modal-body">
          <div class="field-row fm-tools">
            <button type="button" class="sm" id="fm-refetch" title="重新从上游拉一次">重新获取</button>
            ${custom ? '<span class="input-affix fm-search"><span class="affix">⌕</span>'
              + '<input type="search" id="fm-search" placeholder="搜索模型 ID…" autocomplete="off"></span>' : ''}
            <div class="spacer"></div>
            <span class="detail" id="fm-summary"></span>
            ${custom ? '<button type="button" class="sm ghost" id="fm-select-all">全选未添加</button>'
              + '<button type="button" class="sm ghost" id="fm-clear">清空</button>' : ''}
          </div>
          <div class="models-table-wrap fm-wrap">
            <table class="models-table fm-table" id="${custom ? TABLE_ID.custom : TABLE_ID.intl}">
              <colgroup>${(custom ? COLS_CUSTOM : COLS_INTL)
                .map(key => `<col class="f-${key}" data-col="${key}">`).join('')}</colgroup>
              <thead><tr id="fm-head"></tr></thead>
              <tbody id="fm-tbody"></tbody>
            </table>
          </div>
        </div>
        <div class="modal-foot">
          <span class="detail" id="fm-hint"></span>
          <div class="spacer"></div>
          <button type="button" id="fm-done">${custom ? '取消' : '完成'}</button>
          ${custom ? '<button type="button" class="primary" id="fm-import" disabled>导入</button>' : ''}
        </div>
      </div>`;
    document.body.appendChild(mask);

    $('fm-close')?.addEventListener('click', close);
    $('fm-done')?.addEventListener('click', close);
    mask.addEventListener('click', event => { if (event.target === mask) close(); });
    $('fm-refetch')?.addEventListener('click', () => { void load(); });
    if (custom) {
      $('fm-search')?.addEventListener('input', renderRows);
      $('fm-select-all')?.addEventListener('click', () => {
        for (const item of upstream) if (!managed.has(norm(item))) picked.add(item);
        renderRows();
      });
      $('fm-clear')?.addEventListener('click', () => { picked.clear(); renderRows(); });
      $('fm-import')?.addEventListener('click', () => { void importPicked(); });
    }
    // 表体 change：自定义家是模型勾选，内置家是「模型来源」下拉
    // （选中即落盘 —— 下次打开与「重新获取」都以它为准）
    $('fm-tbody')?.addEventListener('change', event => {
      const box = event.target.closest('input[data-model]');
      if (box) {
        if (box.checked) picked.add(box.dataset.model);
        else picked.delete(box.dataset.model);
        paintSummary();
        return;
      }
      const picker = event.target.closest('select[data-provider]');
      if (picker?.value) {
        savedSource[picker.dataset.provider] = picker.value;
        window.wbFilterMemory?.save(SOURCE_KEY, savedSource);
      }
    });
  }

  // ─── 渲染 ──────────────────────────────────

  /** 表头（两种形态不同；th 带 data-col 供列宽层定位，见 TABLE_ID 的说明） */
  function renderHead() {
    const head = $('fm-head');
    if (!head) return;
    head.innerHTML = session.custom
      ? '<th class="fm-pick" data-col="pick"></th><th data-col="model">上游模型</th>'
        + '<th class="fm-state" data-col="state">状态</th>'
      : '<th data-col="provider">提供商</th><th class="fm-source" data-col="source">模型来源</th>'
        + '<th class="fm-state" data-col="state">状态</th><th class="fm-count" data-col="count">模型数</th>'
        + '<th data-col="note">说明</th>';
    repaintTable();
  }

  /** 汇总行：自定义家是「已选 N / 共 M」，内置家是「N 家 · 成功 X · 失败 Y」 */
  function paintSummary(extra) {
    const summary = $('fm-summary');
    if (summary) {
      if (session.custom) {
        summary.textContent = `已选 ${picked.size} 个 · 上游共 ${upstream.length} 个 · 清单已有 ${managed.size} 个`;
      } else if (extra) {
        summary.textContent = extra;
      }
    }
    const button = $('fm-import');
    if (button) {
      button.disabled = !picked.size || busy;
      button.textContent = picked.size ? `导入选中的 ${picked.size} 个模型` : '导入';
    }
  }

  function emptyRow(text) {
    const span = session.custom ? 3 : 5;
    return `<tr><td colspan="${span}" class="empty">${esc(text)}</td></tr>`;
  }

  /** 自定义家的表体：一行一个上游模型 + 勾选框 + 是否已在清单里 */
  function renderRows() {
    const body = $('fm-tbody');
    if (!body || !session?.custom) return;
    const keyword = ($('fm-search')?.value || '').trim().toLowerCase();
    const shown = upstream.filter(id => !keyword || id.toLowerCase().includes(keyword));
    if (!shown.length) {
      body.innerHTML = emptyRow(upstream.length ? `没有匹配「${keyword}」的模型` : '上游没有返回任何模型');
    } else {
      body.innerHTML = shown.map(id => {
        const has = managed.has(norm(id));
        return `<tr${has ? ' class="off"' : ''} data-id="${esc(id)}">
            <td class="fm-pick"><input type="checkbox" data-model="${esc(id)}"${has ? ' disabled' : ''}${picked.has(id) ? ' checked' : ''}></td>
            <td><div class="mid"><span class="t">${esc(id)}</span></div></td>
            <td>${has
              ? '<span class="badge brand" title="这个模型已经在这家的清单里，不重复导入">已添加</span>'
              : '<span class="badge" title="勾上它，导入后即进入这家的清单">未添加</span>'}</td>
          </tr>`;
      }).join('');
    }
    paintSummary();
  }

  /**
   * 「模型来源」格：该家账号的下拉（用哪个账号去打这家的目录接口）。
   *
   * 选中值三档：**落盘的选择**（上次改的那条还在列表里就用它）→
   * **后端回读的实际账号**（`item.accountId`；用户没选过时它就是「这次真正
   * 用的是谁」，队首）→ **列表第一条**（首次打开、两者都没有时的默认 =
   * 队首可用账号，与后端默认选取同一条）。
   * 无可选账号（还没加过号）与不走账号维度的家（Cline）都显示空占位。
   */
  function sourcePicker(item) {
    const providerId = String(item.provider || '');
    const accounts = sourceAccounts(providerId);
    if (!accounts.length) {
      return ACCOUNTLESS_REFRESH.has(providerId)
        ? '<span class="detail" title="这家的模型清单是全局的，不跟账号走">—</span>'
        : '<span class="detail" title="这家还没有可用账号，刷新会使用默认登录态">—</span>';
    }
    const saved = String(savedSource[providerId] || '');
    const reported = String(item.accountId || '');
    const selected = accounts.some(account => String(account.id) === saved)
      ? saved
      : (accounts.some(account => String(account.id) === reported)
        ? reported
        : String(accounts[0].id));
    return `<select data-provider="${esc(providerId)}" title="用哪个账号去拉这家的模型清单">`
      + accounts.map(account => `<option value="${esc(String(account.id))}"`
        + `${String(account.id) === selected ? ' selected' : ''}>${esc(accountLabel(account))}</option>`).join('')
      + '</select>';
  }

  /** 内置家的表体：逐家刷新结果（提供商 / 模型来源 / 状态 / 数量 / 说明） */
  function renderResults(result) {
    const body = $('fm-tbody');
    if (!body || session?.custom) return;
    const results = Array.isArray(result?.results) ? result.results : [];
    if (!results.length) {
      body.innerHTML = emptyRow('刷新完成，但没有得到任何结果');
      paintSummary('0 家');
      return;
    }
    const KIND = {
      refreshed: ['brand', '已刷新'],
      failed: ['bad', '失败'],
      skipped: ['', '跳过'],
    };
    body.innerHTML = results.map(item => {
      const [kind, label] = KIND[item.status] || ['', item.status || '未知'];
      const note = item.status === 'refreshed'
        ? '远程目录已更新，「来源」列会显示为「远程」'
        : (item.message || (item.fixed ? '这家用固定模型清单，无可刷新' : '本次没有取到新内容'));
      return `<tr>
          <td><div class="mid"><span class="t">${esc(item.providerLabel || item.provider || '')}</span></div></td>
          <td class="fm-source">${sourcePicker(item)}</td>
          <td><span class="badge ${kind}">${esc(label)}</span></td>
          <td><span class="rate">${Number(item.count) ? `${Number(item.count)} 个` : '—'}</span></td>
          <td><span class="detail">${esc(note)}</span></td>
        </tr>`;
    }).join('');
    const done = results.filter(item => item.status === 'refreshed').length;
    const failed = results.filter(item => item.status === 'failed').length;
    paintSummary(`${results.length} 家 · 成功 ${done}${failed ? ` · 失败 ${failed}` : ''}`);
  }

  // ─── 取数与导入 ────────────────────────────

  /** 按当前 session 走对应的取数：自定义家拉该家上游，内置家刷新各家目录 */
  async function load() {
    if (!session || busy) return;
    const button = $('fm-refetch');
    const hint = $('fm-hint');
    busy = true;
    if (button) { button.disabled = true; button.textContent = '获取中…'; }
    if (hint) hint.textContent = '';
    $('fm-tbody').innerHTML = emptyRow('正在获取…');
    paintSummary(session.custom ? undefined : '正在刷新…');
    try {
      if (session.custom) {
        const data = await window.wbProviders?.customRequest?.('POST', '/api/custom-providers/fetch-models', {
          providerId: session.providerId,
        });
        const seen = new Set();
        upstream = (Array.isArray(data?.models) ? data.models : [])
          .map(value => String(value ?? '').trim())
          .filter(id => id && !seen.has(norm(id)) && seen.add(norm(id)));
        managed = managedIds(session.providerId);
        // 默认勾选「未添加」的那些 —— 与 OmniProxy 同一默认值：用户点进来就是想补新的
        picked = new Set(upstream.filter(id => !managed.has(norm(id))));
        renderRows();
      } else {
        // 「模型来源」下拉的选择与刷新范围随请求带上（范围见 scopeProviders：
        // 界面看不到的家不刷也不列）
        const result = await workbuddyDesktop.refreshModels({
          accounts: sourceMap(),
          providers: scopeProviders(),
        });
        renderResults(result);
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      $('fm-tbody').innerHTML = emptyRow(`获取失败：${message}`);
      if (hint) hint.textContent = message;
      paintSummary('');
    } finally {
      busy = false;
      if (button) { button.disabled = false; button.textContent = '重新获取'; }
      paintSummary();
    }
  }

  /** 导入勾选的模型：整表提交（已存在的后端也会跳过，前端先按本地清单挡一道） */
  async function importPicked() {
    if (!session?.custom || busy || !picked.size) return;
    const button = $('fm-import');
    const hint = $('fm-hint');
    busy = true;
    if (button) { button.disabled = true; button.textContent = '导入中…'; }
    try {
      const added = await window.wbModelsCustom.addModels(session.providerId, [...picked]);
      toast(`✅ 已导入 ${Number(added) || 0} 个模型（${picked.size - (Number(added) || 0)} 个已存在，跳过）`);
      busy = false;
      const done = session.onDone;
      close();
      done?.();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      toast(`导入失败：${message}`, 'err');
      if (hint) hint.textContent = message;
      busy = false;
      paintSummary();
    }
  }

  // ─── 入口 ──────────────────────────────────

  /**
   * 打开弹窗。`providerId` 是当前左栏选中的那家；`custom` 由调用方判好
   *（它已经知道自己是哪一边），`onDone` 在导入成功后回调（调用方重绘表格）。
   */
  function open({ providerId, custom, name, onDone }) {
    if (!providerId) return;
    close();
    session = { providerId, custom: Boolean(custom), name: name || providerId, onDone };
    buildShell();
    renderHead();
    void load();
  }

  // Esc = 关闭本弹窗（只在本弹窗开着时动作，与其它弹窗的 Esc 互不干扰）
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && $(MODAL_ID)) close();
  });

  window.wbModelsFetchModal = { open };
})();
