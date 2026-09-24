/* Agent2API · 账号设置 / 批量操作 */
/* global workbuddyDesktop, wbApp, wbProxyForm */

/**
 * 账号编辑相关的弹窗都集中在这里，主脚本只负责挂按钮：
 *   - 单账号设置（优先级 / 启用 / 备注名 / 出网代理；所属提供商是**自定义家**
 *     时还会多一段「提供商」：名称 / 协议 / Base URL / 删除提供商）
 *   - 批量操作（启用 / 禁用 / 改代理 / 删除）
 *
 * 代理表单由 wbProxyForm 提供（与批量弹窗共用同一份实现）。
 * 优先级唯一约束：输入框旁实时提示已占用数值，冲突时标红并在保存前拦下；
 * 该约束的作用域是**同 provider 内**（后端 provider_peers 的口径），
 * 因此占用提示也只列同一家的账号 —— 跨家比较会给出错误的「已被占用」告警。
 *
 * 「登录 / 添加账号」弹窗的提供商分叉（提供商分段控件、未知提供商的占位块、
 * 三家的表单块、弹窗内 .seg 分段控件的交互）在 add-provider-forms.js。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  const DEFAULT_PRIORITY = 100;
  const MIN_PRIORITY = 0;
  const MAX_PRIORITY = 9999;

  let panelBusy = false;
  let editingId = null;      // 设置弹窗正在编辑的账号
  let settingsProxyForm = null;
  let batchProxyForm = null;
  let batchIds = [];         // 批量弹窗当前选中的账号

  function allAccounts() {
    const accounts = wbApp.getState?.()?.accounts?.accounts;
    return Array.isArray(accounts) ? accounts : [];
  }

  function findAccount(id) {
    return allAccounts().find(account => account.id === id) || null;
  }

  /** 账号展示名（昵称优先，退化到备注名 / uid） */
  function labelOf(account) {
    return account?.nickname || account?.name || account?.id || '';
  }

  /** 账号所属 provider（缺失按 workbuddy 兜底，与后端 store 同口径） */
  const providerOf = account =>
    (typeof account?.provider === 'string' && account.provider) || 'workbuddy';

  /**
   * 同 provider 的其它账号：优先级唯一性的作用域就是这一组。
   *
   * 后端 `store_crud` 的 `provider_peers` 只在**同一家**内校验唯一与号段，
   * 于是「workbuddy 有 P100」与「小浣熊有 P100」是合法并存。这里必须按同一口径
   * 过滤：跨家比较会把一个完全可以保存的数值报成「已被占用」，用户只能改用大号段。
   */
  function peersOf(account) {
    const provider = providerOf(account);
    return allAccounts().filter(item => item.id !== account.id && providerOf(item) === provider);
  }

  // ─── 单账号设置 ─────────────────────────────

  /**
   * 优先级占用提示：列出同 provider 内已被占用的数值，并在输入冲突时即时标红。
   * 优先级在**同一家内**唯一（写入侧强制），让用户一眼看到哪些号段被占了，
   * 比事后被 409 拒绝友好得多。
   */
  function refreshPriorityUsage() {
    const input = $('account-priority-input');
    const hint = $('priority-usage');
    if (!input || !hint) return;
    const account = findAccount(editingId);
    if (!account) return;
    const value = Number(input.value);
    const peers = peersOf(account);
    const holder = peers.find(item => Number(item.priority) === value);
    const used = [...new Set(peers.map(item => Number(item.priority)))].sort((a, b) => a - b);

    if (holder) {
      input.style.borderColor = 'var(--danger)';
      hint.innerHTML = `<span style="color:var(--danger)">已被「${esc(labelOf(holder))}」占用</span>`;
      return;
    }
    input.style.borderColor = '';
    hint.textContent = used.length
      ? `同提供商已占用：${used.join('、')}`
      : '同提供商内暂无其他账号占用优先级';
  }

  async function openSettings(id) {
    editingId = id;
    const account = findAccount(id);
    if (!account) { toast('账号不存在，请刷新后重试', 'err'); return; }

    $('account-modal-title').textContent = `账号设置 · ${labelOf(account)}`;
    $('account-name-input').value = account.name || '';
    $('account-priority-input').value = String(account.priority ?? DEFAULT_PRIORITY);
    $('account-enabled-input').checked = account.enabled !== false;
    $('account-modal-status').textContent = '';
    mountBalanceTokenField(account);
    await mountProviderSection(account);

    if (!settingsProxyForm) settingsProxyForm = wbProxyForm.create($('account-proxy-form'));
    const mode = settingsProxyForm.fill(account.proxy);
    $('account-modal').classList.add('open');
    refreshPriorityUsage();
    await settingsProxyForm.loadClashOptions({
      selectedUid: mode === 'clash' ? (account.proxy?.config?.listenerUid || null) : null,
    });

    // 账号代理解析失败时给出明确提示（仍可用，只是转发回退直连）
    if (account.proxy?.error) {
      $('account-modal-status').innerHTML =
        `<span style="color:var(--danger)">当前代理不可用：${esc(account.proxy.error)}（转发时会回退直连）</span>`;
    }
  }

  // ─── CatPaw 的余额查询凭证（balanceToken）─────

  /**
   * 余额凭证输入框：**只有 CatPaw 账号有这一项**，所以动态注入而不是写死在
   * index.html 里（写死的话每家账号打开设置都会看到一个与自己无关的输入框，
   * 而 index.html 同时被其它任务维护，这里不重排/新增它的既有节点）。
   *
   * ── 现在它是**可选的回退项**，不再是查询的前提 ──────────────────
   * 积分查询已改走客户端自己的网关 API（`catx.nocode.cn/api/gateway/credit/balance`），
   * 凭证就是转发用的那个 token —— 不需要用户额外配置（见
   * `providers/catpaw/balance.rs` 模块头）。这一栏留给两种旧数据：
   * 已经填过的用户（值仍在），以及从原项目导入的 `balanceCookie.token2`。
   * 空着完全不影响查询。
   *
   * `hasBalanceToken` 由后端公开形态给出（**只给真假，不给值** —— 它是完整凭证，
   * 不进账号列表这种会被截图/贴出来排障的界面）：已配置时占位提示「留空则不修改」，
   * 并给一个显式的「清除」按钮。
   */
  const BALANCE_TOKEN_ROW_ID = 'account-balance-token-row';

  function mountBalanceTokenField(account) {
    $(BALANCE_TOKEN_ROW_ID)?.remove();
    if (providerOf(account) !== 'catpaw') return;
    const section = $('account-name-input')?.closest('.modal-section');
    if (!section) return;
    const configured = account.hasBalanceToken === true;
    const row = document.createElement('div');
    row.id = BALANCE_TOKEN_ROW_ID;
    row.className = 'field-row';
    row.style.marginTop = '9px';
    row.innerHTML = `<label for="account-balance-token-input">余额查询凭证</label>`
      + `<input id="account-balance-token-input" type="text" `
      + `placeholder="${configured ? '已配置，留空则不修改' : '一般不用填'}" `
      + `title="积分查询已复用转发用的登录凭证，这里通常留空即可。只有旧版本填过、或从旧代理导入过凭证时才有值">`
      // 清除是**显式动作**（保存时的空值一律理解为「不修改」，见 readBalanceTokenPatch）
      + (configured
        ? `<button id="account-balance-token-clear" title="清除已配置的余额查询凭证">清除</button>`
        : '');
    section.appendChild(row);
    $('account-balance-token-clear')?.addEventListener('click', clearBalanceToken);
  }

  /** 清除余额凭证（立即落库，不走保存按钮 —— 它是一个独立的撤销动作） */
  async function clearBalanceToken() {
    if (panelBusy || !editingId) return;
    panelBusy = true;
    try {
      await api.updateAccount(editingId, { balanceToken: null });
      mountBalanceTokenField({ ...findAccount(editingId), hasBalanceToken: false });
      toast('✅ 已清除余额查询凭证');
      await wbApp.refresh?.();
    } catch (error) {
      toast(`清除失败：${error.message}`, 'err');
    } finally {
      panelBusy = false;
    }
  }

  /**
   * 读余额凭证输入框的值：空串 = **不修改**（不是清除）。
   *
   * 为什么不做「清空即清除」：后端公开形态只给 `hasBalanceToken` 真假、不回显原值，
   * 于是这个输入框永远是空的 —— 若把「空」解释成清除，用户每次保存设置
   * （比如只改备注名）都会把已经配好的凭证删掉。清除走上面那个显式按钮。
   */
  function readBalanceTokenPatch() {
    const input = $('account-balance-token-input');
    if (!input) return null;
    const value = input.value.trim();
    return value ? { balanceToken: value } : null;
  }

  // ─── 自定义提供商的「提供商」一段 ─────────────

  /**
   * 账号所属的**自定义提供商**可以在同一个弹窗里就地改（名称 / 协议 / Base URL）
   * 或删除；内置家不注入这一段（它们的协议与地址写在代码里，没有可改的字段）。
   *
   * 为什么落在这里：账号页那颗「自定义提供商」按钮与它打开的管理弹窗已整体移除
   * （模型清单统一到「模型管理」页，提供商本身的编辑与删除只剩这一处入口，见
   * custom-provider-ui.js 的文件头）。而改 Base URL 的动机通常正是「这个账号
   * 连不上了」—— 在账号设置里就地改，比先退出去找一个管理入口近一步。
   *
   * 字段随同一个「保存」提交（见 saveSettings），不另起一颗保存按钮：
   * 一个弹窗里两颗保存按钮是事故来源。
   */
  const PROVIDER_SECTION_ID = 'account-provider-section';
  /** 名称长度上限，与后端 custom_providers::MAX_NAME_CHARS 一致（前端先挡一次） */
  const MAX_PROVIDER_NAME_CHARS = 64;

  async function mountProviderSection(account) {
    $(PROVIDER_SECTION_ID)?.remove();
    // 目录里查得到才算自定义家：id 前缀只说明「长得像」，而记录本身才带着
    // 协议 / Base URL 的现值（三个字段要拿它预填）
    const provider = await window.wbCustomProvidersUi?.find?.(providerOf(account));
    const anchor = $('account-name-input')?.closest('.modal-section');
    if (!provider || !anchor) return;

    const options = (window.wbProviders?.PROTOCOL_OPTIONS || []).map(option =>
      `<option value="${esc(option.value)}"${option.value === provider.protocol ? ' selected' : ''}>${esc(option.label)}</option>`).join('');
    // 名下账号数含自己：peersOf 给的是「同 provider 的其它账号」
    const count = peersOf(account).length + 1;
    const section = document.createElement('div');
    section.id = PROVIDER_SECTION_ID;
    section.className = 'modal-section';
    // id 挂在段上而不是某个输入框上：保存时按它找回这家（字段本身没有 id 语义）
    section.dataset.providerId = provider.id;
    section.innerHTML = `<h3>提供商</h3>`
      + `<p>这一栏改的是<strong>「${esc(provider.name || provider.id)}」本身</strong>（名下 ${count} 个账号共用）。`
      + `改协议 / Base URL 会改变它们的转发方式，正在进行的请求可能失败；模型清单与映射在「模型管理」页。</p>`
      + `<div class="field-row"><label for="account-provider-name">名称</label>`
      + `<input id="account-provider-name" type="text" maxlength="${MAX_PROVIDER_NAME_CHARS}" `
      + `value="${esc(provider.name || '')}" placeholder="提供商显示名，1~${MAX_PROVIDER_NAME_CHARS} 个字符"></div>`
      + `<div class="field-row" style="margin-top:9px"><label for="account-provider-protocol">协议</label>`
      + `<select id="account-provider-protocol" class="custom-provider-select">${options}</select></div>`
      + `<div class="field-row" style="margin-top:9px"><label for="account-provider-baseurl">Base URL</label>`
      + `<input id="account-provider-baseurl" type="text" value="${esc(provider.baseUrl || '')}" `
      + `placeholder="OpenAI 兼容填到 /v1；Anthropic 填根地址"></div>`
      + `<div class="field-row" style="margin-top:12px">`
      + `<button type="button" class="danger" id="account-provider-remove" `
      + `title="删除该提供商及其名下全部账号">删除提供商</button>`
      + `<span class="detail">级联删除名下全部账号，不可恢复</span></div>`;
    anchor.insertAdjacentElement('afterend', section);
    $('account-provider-remove')?.addEventListener('click', () => { void removeProvider(provider.id); });
  }

  /**
   * 读「提供商」那一段的改动：没这一段（内置家）或三个字段都没动 → null（不提交）。
   *
   * 返回 `{ error }` 而不是直接提交的原因：空名称 / 空 Base URL 后端会 400，而那时
   * **账号已经存下了** —— 用户看到「保存失败」却发现账号的改动生效了。所以先把
   * 表单验一遍，验不过就拦在账号落库之前。
   */
  function readProviderPatch() {
    const id = $(PROVIDER_SECTION_ID)?.dataset.providerId || '';
    if (!id) return null;
    const name = ($('account-provider-name')?.value || '').trim();
    const protocol = $('account-provider-protocol')?.value || '';
    const baseUrl = ($('account-provider-baseurl')?.value || '').trim();
    if (!name) return { error: '请填写提供商名称' };
    if (!baseUrl) return { error: '请填写提供商的 Base URL' };
    const current = (window.wbProviders?.customList?.() || []).find(item => item.id === id);
    if (current && current.name === name && current.protocol === protocol && current.baseUrl === baseUrl) return null;
    return { id, name, protocol, baseUrl };
  }

  /** 删除该账号所属的自定义提供商：删掉了就把本弹窗一起关掉（账号也没了） */
  async function removeProvider(providerId) {
    if (panelBusy) return;
    if (await window.wbCustomProvidersUi?.remove?.(providerId)) closeSettings();
  }

  function closeSettings() {
    $('account-modal').classList.remove('open');
    // 余额凭证行与「提供商」那一段都是注入的：顺手清掉，避免下次打开别家账号
    // 时它们还在（两个 mount 也会先删，这里只是让关闭态也干净）
    $(BALANCE_TOKEN_ROW_ID)?.remove();
    $(PROVIDER_SECTION_ID)?.remove();
    editingId = null;
  }

  async function saveSettings() {
    if (panelBusy || !editingId) return;
    const account = findAccount(editingId);
    if (!account) { toast('账号不存在，请刷新后重试', 'err'); return; }

    let proxy;
    try {
      proxy = settingsProxyForm.read();
    } catch (error) {
      toast(error.message, 'err');
      return;
    }
    const priority = Number($('account-priority-input').value);
    if (!Number.isFinite(priority)) { toast('优先级必须是数字', 'err'); return; }
    // 本地先挡一次冲突（后端也会校验），省得白跑一趟请求。
    // 冲突范围是**同 provider**：跨家同名数值合法（见 peersOf 的说明）
    const clamped = Math.min(MAX_PRIORITY, Math.max(MIN_PRIORITY, Math.round(priority)));
    const holder = peersOf(account).find(item => Number(item.priority) === clamped);
    if (holder) {
      $('account-modal-status').innerHTML =
        `<span style="color:var(--danger)">优先级 ${esc(String(clamped))} 已被同一提供商的「${esc(labelOf(holder))}」占用，请换一个数值</span>`;
      refreshPriorityUsage();
      return;
    }

    // 提供商那一段（只有自定义家才有）：先在账号落库**之前**验一遍字段 ——
    // 空名称 / 空 Base URL 会被后端 400 挡下，而那时账号已经存下了，
    // 用户看到「保存失败」却发现账号的改动其实生效了
    const providerPatch = readProviderPatch();
    if (providerPatch?.error) {
      $('account-modal-status').innerHTML =
        `<span style="color:var(--danger)">${esc(providerPatch.error)}</span>`;
      return;
    }

    panelBusy = true;
    const button = $('account-modal-save');
    button.disabled = true;
    button.textContent = '保存中…';
    try {
      // CatPaw 的余额凭证是**按需附加**的字段（别的家没有这一项，
      // 输入框也不存在）：没填就不进 patch，后端据此保持原值不变
      const balancePatch = readBalanceTokenPatch();
      await api.updateAccount(editingId, {
        name: $('account-name-input').value.trim() || account.name,
        priority: clamped,
        enabled: $('account-enabled-input').checked,
        proxy,
        ...(balancePatch || {}),
      });
      // 提供商那一段排在账号之后（账号是本弹窗的主角，先落库）。它失败时账号
      // 已经存下了，所以留在弹窗里把那句话说清楚 —— 笼统报成「保存失败」会把
      // 两件事混成一件，用户会以为账号的改动也丢了
      if (providerPatch) {
        try {
          await window.wbCustomProvidersUi.update(providerPatch);
        } catch (error) {
          $('account-modal-status').innerHTML = `<span style="color:var(--danger)">`
            + `账号已保存，但提供商未更新：${esc(error.message)}</span>`;
          try { await wbApp.refresh?.(); } catch { /* 交给下一次轮询 */ }
          return;
        }
      }
      // 先关窗并反馈成功：改动已经落库，刷新只是让列表卡片跟上，
      // 不该让用户对着「保存中…」再多等一次网络往返（刷新若被排队更是等不到头）。
      closeSettings();
      toast('✅ 账号设置已保存');
      // 保存后必须主动刷新列表：以前只关窗不刷新，卡片上的代理/优先级等仍是旧数据，
      // 要等 app.js 每 20 秒一次的轮询才更新 —— 用户看到的就是「保存完十几秒才变」。
      // 单独兜一层错：刷新失败只影响本次界面同步（后续轮询会自愈），
      // 不能掉进下面的 catch 被报成「保存失败」（保存其实已经成功了）。
      try {
        await wbApp.refresh?.();
      } catch { /* 刷新失败不影响保存结果，交给下一次轮询 */ }
    } catch (error) {
      // 后端校验失败（如优先级冲突）：留在弹窗里显示原因，方便直接改
      $('account-modal-status').innerHTML = `<span style="color:var(--danger)">${esc(error.message)}</span>`;
      refreshPriorityUsage();
      toast(`保存失败：${error.message}`, 'err');
    } finally {
      panelBusy = false;
      button.disabled = false;
      button.textContent = '保存';
    }
  }

  // ─── 批量操作 ───────────────────────────────

  /** 打开批量弹窗：ids 为账号 id 列表，action 决定显示哪一组控件 */
  async function openBatch(ids, action = 'enable') {
    const selected = (Array.isArray(ids) ? ids : []).filter(id => findAccount(id));
    if (!selected.length) { toast('请先勾选要操作的账号', 'err'); return; }
    batchIds = selected;
    $('batch-modal-title').textContent = `批量操作 · 已选 ${selected.length} 个账号`;
    $('batch-summary').textContent = selected.map(id => labelOf(findAccount(id))).join('、');
    $('batch-result').textContent = '';

    // 先备好代理表单再同步动作：syncBatchAction 会触发 Clash 出口拉取，
    // 若表单尚未创建，首次打开「修改代理」时下拉会是空的
    if (!batchProxyForm) batchProxyForm = wbProxyForm.create($('batch-proxy-form'));
    batchProxyForm.fill(null);

    // 打开哪个动作就默认选中哪个
    const radio = document.querySelector(`input[name="batch-action"][value="${action}"]`);
    if (radio) radio.checked = true;
    syncBatchAction();

    $('batch-modal').classList.add('open');
  }

  function closeBatch() {
    $('batch-modal').classList.remove('open');
    batchIds = [];
  }

  function selectedBatchAction() {
    return document.querySelector('input[name="batch-action"]:checked')?.value || 'enable';
  }

  /** 只有「修改代理」需要额外的代理表单 */
  function syncBatchAction() {
    const isProxy = selectedBatchAction() === 'proxy';
    $('batch-proxy-block').style.display = isProxy ? '' : 'none';
    if (isProxy) void batchProxyForm?.loadClashOptions();
  }

  /** 批量删除是不可逆操作，按数量做二次确认（自绘弹窗：原生 confirm 在 Tauri WebView 里不弹窗直接放行） */
  function confirmBatchRemove(count) {
    return window.wbConfirm?.ask?.({
      title: '删除选中的账号',
      html: `确定删除选中的 <strong>${count}</strong> 个账号？此操作不可恢复，账号的登录态会一并移除。`,
      okText: '删除',
      okClass: 'danger',
    });
  }

  async function runBatch() {
    if (panelBusy) return;
    if (!batchIds.length) { toast('没有选中的账号', 'err'); return; }
    const action = selectedBatchAction();

    let proxy;
    if (action === 'proxy') {
      try {
        proxy = batchProxyForm.read();
      } catch (error) {
        toast(error.message, 'err');
        return;
      }
    }
    if (action === 'remove' && !(await confirmBatchRemove(batchIds.length))) return;

    // 批量禁用/删除可能把所有启用中的账号一起停掉，转发将无账号可用，提醒一下更稳妥。
    // 判据是「选中的账号是否已覆盖全部启用中的账号」，而不是数量对比 ——
    // 否则勾了已禁用账号凑够数量也会误报。
    if (action === 'disable' || action === 'remove') {
      const picked = new Set(batchIds);
      const survivors = allAccounts().filter(a => a.enabled !== false && !picked.has(a.id));
      if (!survivors.length) {
        const what = action === 'remove' ? '删除' : '禁用';
        if (!(await window.wbConfirm?.ask?.({
          title: `全部启用中的账号将被${what}`,
          html: `这会<strong>${what}</strong>所有启用中的账号，转发将不可用。确定继续？`,
          okText: what,
          okClass: action === 'remove' ? 'danger' : 'primary',
        }))) return;
      }
    }

    panelBusy = true;
    const button = $('batch-run');
    button.disabled = true;
    button.textContent = '执行中…';
    $('batch-result').textContent = '';
    try {
      const payload = { action, ids: batchIds };
      if (action === 'proxy') payload.proxy = proxy;
      const data = await api.batchAccounts(payload);
      const okCount = (data?.ok || []).filter(item => item.changes?.length).length;
      const removedCount = (data?.removed || []).length;
      const failed = data?.failed || [];
      const succeeded = action === 'remove' ? removedCount : okCount;
      const verb = { enable: '启用', disable: '禁用', proxy: '修改代理', remove: '删除' }[action] || action;
      if (failed.length) {
        const detail = failed.slice(0, 3).map(item => labelOf(findAccount(item.id)) || item.id).join('、');
        toast(`${verb}完成：成功 ${succeeded} 个，失败 ${failed.length} 个（${detail}${failed.length > 3 ? ' 等' : ''}）`, 'err');
        $('batch-result').innerHTML = `<span style="color:var(--danger)">失败 ${failed.length} 个：`
          + esc(failed.map(item => `${labelOf(findAccount(item.id)) || item.id}（${item.error}）`).join('；')) + '</span>';
      } else {
        toast(`✅ ${verb}完成：共 ${succeeded} 个账号`);
        closeBatch();
      }
      await wbApp.refresh?.();
    } catch (error) {
      $('batch-result').innerHTML = `<span style="color:var(--danger)">${esc(error.message)}</span>`;
      toast(`批量操作失败：${error.message}`, 'err');
    } finally {
      panelBusy = false;
      button.disabled = false;
      button.textContent = '执行';
    }
  }

  // ─── 优先级占用提示：作用域是「同 provider」 ───
  //
  // 后端把优先级唯一性收窄到 provider 内（account_store::provider_peers），
  // 于是「workbuddy 有 P100」与「小浣熊有 P100」是合法并存。这里按同一口径过滤，
  // 否则跨家比较会把一个完全可以保存的数值报成「已被占用」，用户只能改用大号段。

  // ─── 事件绑定 ───────────────────────────────

  // 单账号设置
  $('account-priority-input').addEventListener('input', refreshPriorityUsage);
  $('account-modal-save').addEventListener('click', saveSettings);
  $('account-modal-cancel').addEventListener('click', closeSettings);
  $('account-modal-close').addEventListener('click', closeSettings);
  $('account-modal').addEventListener('click', event => {
    if (event.target === $('account-modal')) closeSettings();
  });

  // 批量操作
  document.querySelectorAll('input[name="batch-action"]').forEach(input => {
    input.addEventListener('change', syncBatchAction);
  });
  $('batch-run').addEventListener('click', runBatch);
  $('batch-cancel').addEventListener('click', closeBatch);
  $('batch-close').addEventListener('click', closeBatch);
  $('batch-modal').addEventListener('click', event => {
    if (event.target === $('batch-modal')) closeBatch();
  });
  document.addEventListener('keydown', event => {
    if (event.key !== 'Escape') return;
    if ($('account-modal')?.classList.contains('open')) closeSettings();
    else if ($('batch-modal')?.classList.contains('open')) closeBatch();
  });

  window.wbAccountPanel = {
    open: openSettings,
    close: closeSettings,
    openBatch,
    closeBatch,
    /** 「添加账号」弹窗打开时可用：按 providers 摘要重建选项并复位到 WorkBuddy */
    syncAddProvider: () => window.wbAccountAddForms?.syncAddProvider(),
    /** 账号列表刷新后调用：Clash 端口可能已在 Clash 侧改过 */
    invalidate: () => {
      wbProxyForm.invalidateClashCache();
    },
  };
})();
