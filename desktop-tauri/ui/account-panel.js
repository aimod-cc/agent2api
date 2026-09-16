/* WorkBuddy 本地代理 · 账号设置与批量操作面板 */
/* global workbuddyDesktop, wbApp, wbProxyForm */

/**
 * 账号编辑相关的弹窗都集中在这里，主脚本只负责挂按钮：
 *   - 单账号设置（优先级 / 启用 / 备注名 / 出网代理）
 *   - 批量操作（启用 / 禁用 / 改代理 / 删除）
 *
 * 代理表单由 wbProxyForm 提供（与批量弹窗共用同一份实现）。
 * 优先级唯一约束：输入框旁实时提示已占用数值，冲突时标红并在保存前拦下。
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

  // ─── 单账号设置 ─────────────────────────────

  /**
   * 优先级占用提示：列出已被占用的数值，并在输入冲突时即时标红。
   * 优先级全局唯一（写入侧强制），让用户一眼看到哪些号段被占了，
   * 比事后被 409 拒绝友好得多。
   */
  function refreshPriorityUsage() {
    const input = $('account-priority-input');
    const hint = $('priority-usage');
    if (!input || !hint) return;
    const value = Number(input.value);
    const holder = allAccounts().find(a => a.id !== editingId && Number(a.priority) === value);
    const used = allAccounts()
      .filter(a => a.id !== editingId)
      .map(a => Number(a.priority))
      .sort((a, b) => a - b);

    if (holder) {
      input.style.borderColor = 'var(--danger)';
      hint.innerHTML = `<span style="color:var(--danger)">已被「${esc(labelOf(holder))}」占用</span>`;
      return;
    }
    input.style.borderColor = '';
    hint.textContent = used.length ? `已占用：${used.join('、')}` : '暂无其他账号占用优先级';
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

  function closeSettings() {
    $('account-modal').classList.remove('open');
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
    // 本地先挡一次冲突（后端也会校验），省得白跑一趟请求
    const clamped = Math.min(MAX_PRIORITY, Math.max(MIN_PRIORITY, Math.round(priority)));
    const holder = allAccounts().find(a => a.id !== editingId && Number(a.priority) === clamped);
    if (holder) {
      $('account-modal-status').innerHTML =
        `<span style="color:var(--danger)">优先级 ${esc(String(clamped))} 已被「${esc(labelOf(holder))}」占用，请换一个数值</span>`;
      refreshPriorityUsage();
      return;
    }

    panelBusy = true;
    const button = $('account-modal-save');
    button.disabled = true;
    button.textContent = '保存中…';
    try {
      await api.updateAccount(editingId, {
        name: $('account-name-input').value.trim() || account.name,
        priority: clamped,
        enabled: $('account-enabled-input').checked,
        proxy,
      });
      closeSettings();
      toast('✅ 账号设置已保存');
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

  /** 批量删除是不可逆操作，按数量做二次确认 */
  function confirmBatchRemove(count) {
    return confirm(`确定删除选中的 ${count} 个账号？此操作不可恢复，账号的登录态会一并移除。`);
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
    if (action === 'remove' && !confirmBatchRemove(batchIds.length)) return;

    // 批量禁用/删除可能把首选账号一起停掉（首选是优先级的派生值，
    // 被禁用后会自动落到下一个账号），提醒一下更稳妥。
    // 判据是「选中的账号是否已覆盖全部启用中的账号」，而不是数量对比 ——
    // 否则勾了已禁用账号凑够数量也会误报。
    if (action === 'disable' || action === 'remove') {
      const picked = new Set(batchIds);
      const survivors = allAccounts().filter(a => a.enabled !== false && !picked.has(a.id));
      if (!survivors.length) {
        const what = action === 'remove' ? '删除' : '禁用';
        if (!confirm(`这会${what}所有启用中的账号，转发将不可用。确定继续？`)) return;
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
    /** 账号列表刷新后调用：Clash 端口可能已在 Clash 侧改过 */
    invalidate: () => {
      wbProxyForm.invalidateClashCache();
    },
  };
})();
