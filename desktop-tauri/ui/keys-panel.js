/* Agent2API · 网关 Key 页：多把 API Key 的列表 / 新建 / 启停 / 删除 */
/* global workbuddyDesktop, wbApp */

/**
 * 数据来自 `GET /api/keys`（`{keys, authRequired}`，keys 带明文 key 与掩码）；
 * 写接口都返回最新列表，就地替换后重绘。列表默认显示掩码，每行可单独「显示」
 * 明文并复制（复制走 clipboard.js 的 data-copy 委托）。
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

  function row(k) {
    const busyRow = pending.has(k.id);
    const shown = revealed.has(k.id);
    return `<tr class="${k.enabled ? '' : 'off'}" data-id="${esc(k.id)}">`
      + `<td><div class="mid"><span class="t">${esc(k.name || '未命名')}</span></div></td>`
      + `<td><div class="keycell"><code class="kv">${esc(shown ? k.key : k.masked)}</code>`
      + `<button type="button" class="sm ghost" data-act="reveal" data-id="${esc(k.id)}">${shown ? '隐藏' : '显示'}</button>`
      + `<button type="button" class="sm ghost" data-copy="${esc(k.key)}" title="复制 Key">复制</button></div></td>`
      + `<td><span class="muted">${k.createdAt ? esc(formatTime(k.createdAt)) : '—'}</span></td>`
      + `<td class="state"><label class="switch"><input type="checkbox" data-act="toggle" data-id="${esc(k.id)}"${k.enabled ? ' checked' : ''}${busyRow ? ' disabled' : ''}><span class="track"></span></label></td>`
      + `<td class="r"><div class="row-actions"><button type="button" class="sm ghost danger-text" data-act="remove" data-id="${esc(k.id)}"${busyRow ? ' disabled' : ''}>删除</button></div></td></tr>`;
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

  async function onClick(event) {
    const button = event.target.closest('[data-act]');
    if (!button) return;
    const { act, id } = button.dataset;
    if (act === 'reveal') {
      if (revealed.has(id)) revealed.delete(id); else revealed.add(id);
      render();
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

  // ─── 新建弹窗 ────────────────────────────

  let saving = false;

  function openModal() {
    $('key-name').value = '';
    $('key-value').value = '';
    $('key-modal-status').textContent = '';
    $('key-modal').classList.add('open');
    setTimeout(() => $('key-name').focus(), 0);
  }

  function closeModal() {
    if (saving) return;
    $('key-modal').classList.remove('open');
  }

  async function save() {
    if (saving) return;
    const name = $('key-name').value.trim();
    const key = $('key-value').value.trim();
    const status = $('key-modal-status');
    if (key && key.length < 8) { status.textContent = 'Key 至少需要 8 个字符'; return; }
    saving = true;
    $('key-modal-save').disabled = true;
    status.textContent = '创建中…';
    try {
      const next = await workbuddyDesktop.createKey({ name, key: key || undefined });
      if (next?.created?.id) revealed.add(next.created.id);
      accept(next);
      saving = false;
      closeModal();
      toast('✅ Key 已创建，记得复制给客户端');
    } catch (error) {
      status.textContent = `创建失败：${error.message}`;
    } finally {
      saving = false;
      $('key-modal-save').disabled = false;
    }
  }

  $('keys-list')?.addEventListener('click', onClick);
  $('keys-list')?.addEventListener('change', onChange);
  $('btn-add-key')?.addEventListener('click', openModal);
  $('key-modal-close')?.addEventListener('click', closeModal);
  $('key-modal-cancel')?.addEventListener('click', closeModal);
  $('key-modal-save')?.addEventListener('click', () => { void save(); });
  $('key-value')?.addEventListener('keydown', event => { if (event.key === 'Enter') void save(); });
  $('key-modal')?.addEventListener('click', event => { if (event.target === $('key-modal')) closeModal(); });

  window.wbKeysPanel = { load, render };
  if (wbApp.currentPage === 'keys') void load();
})();
