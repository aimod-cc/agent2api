/* Agent2API · 模型管理页：表格渲染 + 筛选 + 启停 / 删除 / 恢复 + 模型映射 + 刷新模型清单 */
/* global workbuddyDesktop, wbApp */

/**
 * 数据来自 `GET /api/models/manage`（`{models, mappings}`，含禁用 / 隐藏的条目，
 * 每条带 enabled / hidden / aliases）。本模块自持这份数据：写接口都会返回最新的
 * 同形数据，就地替换后重绘，不经过 app.js 的 state（那是 /api/session 的快照，
 * 轮询会整份覆盖）。
 *
 * 表格按提供商分组（顺序 = 后端数组顺序 = 路由优先级），每组默认只展开前
 * `GROUP_LIMIT` 行，其余折叠成一行「展开其余 N 个」；有搜索词或非「全部」筛选时
 * 不折叠 —— 用户在找东西，藏起来只会让他以为没有。
 *
 * 跨文件引用一律走 `wbApp`（esc / toast 是 app.js 里的全局单份实现）。
 */

(() => {
  const { esc, toast } = wbApp;
  const $ = id => document.getElementById(id);

  const REFRESH_TITLE = '只刷新支持远程目录的提供商（WorkBuddy、小浣熊）；'
    + 'CatPaw 与 AutoClaw 使用固定模型清单，刷新不会改变它们';
  const GROUP_LIMIT = 8;

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

  function aliasChips(m) {
    const chips = (m.aliases || []).map(alias =>
      `<span class="alias${m.enabled ? '' : ' off'}"><span class="t">${esc(alias)}</span>`
      + `<button type="button" class="x" data-act="unmap" data-alias="${esc(alias)}" title="删除映射 ${esc(alias)}">×</button></span>`).join('');
    const add = m.hidden ? '' : `<button type="button" class="alias-add" data-act="map" data-id="${esc(m.id)}">＋ 映射</button>`;
    return `<div class="aliases">${chips}${add}</div>`;
  }

  function row(m) {
    const busyRow = pending.has(m.id);
    const name = m.name && m.name !== m.id ? `<div class="mname">${esc(m.name)}</div>` : '';
    const credits = m.credits ? `<span class="rate">${esc(formatCredits(m.credits))}</span>` : '<span class="rate">—</span>';
    const actions = m.hidden
      ? `<button type="button" class="sm ghost" data-act="restore" data-id="${esc(m.id)}"${busyRow ? ' disabled' : ''}>恢复</button>`
      : `<button type="button" class="sm ghost danger-text" data-act="hide" data-id="${esc(m.id)}"${busyRow ? ' disabled' : ''}>删除</button>`;
    return `<tr class="${m.enabled && !m.hidden ? '' : 'off'}" data-id="${esc(m.id)}">`
      + `<td><div class="mid"><span class="t">${esc(m.id)}</span>`
      + `<button type="button" class="cp" data-copy="${esc(m.id)}" title="复制模型 ID">⧉</button></div>${name}</td>`
      + `<td>${credits}</td>`
      + `<td>${aliasChips(m)}</td>`
      + `<td class="state"><label class="switch"><input type="checkbox" data-act="toggle" data-id="${esc(m.id)}"${m.enabled ? ' checked' : ''}${m.hidden || busyRow ? ' disabled' : ''}><span class="track"></span></label></td>`
      + `<td class="r"><div class="row-actions">${actions}</div></td></tr>`;
  }

  function render() {
    const body = $('models');
    if (!body) return;
    renderProviderSeg();
    const all = models();
    const keyword = searchTerm();
    const shown = all.filter(m => matches(m, keyword));
    // 计数：总数 / 启用数 / 映射数（不受筛选影响，是「这台网关现在的状态」）
    const visible = all.filter(m => !m.hidden);
    const count = $('models-count');
    if (count) {
      count.textContent = all.length
        ? `${visible.length} 个上游模型 · ${visible.filter(m => m.enabled).length} 已启用 · ${mappings().length} 条映射`
          + (all.length !== visible.length ? ` · ${all.length - visible.length} 已删除` : '')
        : '';
    }
    if (!all.length) {
      body.innerHTML = `<tr><td colspan="5" class="empty">${data ? '暂无模型（请先添加账号）' : '加载中…'}</td></tr>`;
      return;
    }
    if (!shown.length) {
      body.innerHTML = `<tr><td colspan="5" class="empty">没有匹配${keyword ? `「${esc(keyword)}」` : '当前筛选'}的模型</td></tr>`;
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
      const head = `<tr class="tr-group"><td colspan="5"><span class="prov-tag">${esc(group.label)}</span>${group.items.length} 个模型</td></tr>`;
      const more = rest > 0
        ? `<tr class="tr-more"><td colspan="5"><button type="button" class="sm ghost" data-act="expand" data-provider="${esc(key)}">展开其余 ${rest} 个模型 ▾</button></td></tr>`
        : (open && collapsible && group.items.length > GROUP_LIMIT
          ? `<tr class="tr-more"><td colspan="5"><button type="button" class="sm ghost" data-act="collapse" data-provider="${esc(key)}">收起 ▴</button></td></tr>`
          : '');
      return head + items.map(row).join('') + more;
    }).join('');
  }

  // ─── 行内操作 ────────────────────────────

  async function runRowAction(id, run, doneText) {
    if (pending.has(id)) return;
    pending.add(id);
    render();
    try {
      accept(await run());
      if (doneText) toast(doneText);
    } catch (error) {
      toast(`操作失败：${error.message}`, 'err');
      render();
    } finally {
      pending.delete(id);
      render();
    }
  }

  function onTableClick(event) {
    const button = event.target.closest('[data-act]');
    if (!button) return;
    const { act, id, alias, provider } = button.dataset;
    if (act === 'expand') { expanded.add(provider); render(); return; }
    if (act === 'collapse') { expanded.delete(provider); render(); return; }
    if (act === 'hide') {
      if (!confirm(`确定删除模型「${id}」？只是从清单隐藏，可在「已删除」筛选里恢复。`)) return;
      void runRowAction(id, () => workbuddyDesktop.setModelState({ id, hidden: true }), '模型已删除');
      return;
    }
    if (act === 'restore') {
      void runRowAction(id, () => workbuddyDesktop.setModelState({ id, hidden: false }), '模型已恢复');
      return;
    }
    if (act === 'unmap') {
      if (!confirm(`确定删除映射「${alias}」？`)) return;
      void runRowAction(alias, () => workbuddyDesktop.removeModelMapping(alias), '映射已删除');
      return;
    }
    if (act === 'map') openMapping(id);
  }

  function onTableChange(event) {
    const input = event.target.closest('input[data-act="toggle"]');
    if (!input) return;
    const { id } = input.dataset;
    const enabled = input.checked;
    void runRowAction(id, () => workbuddyDesktop.setModelState({ id, enabled }), enabled ? '模型已启用' : '模型已禁用');
  }

  // ─── 映射弹窗 ────────────────────────────

  let mappingSaving = false;

  function mappingPreview() {
    const alias = $('mapping-alias').value.trim() || '<映射名>';
    const select = $('mapping-target');
    const option = select.options[select.selectedIndex];
    const target = option?.value || '—';
    const label = option?.dataset.label || '';
    $('mapping-preview').innerHTML = `下游请求 <b>${esc(alias)}</b> → 实际转发 <b>${esc(target)}</b>${label ? `（${esc(label)}）` : ''}`;
  }

  function openMapping(presetTarget) {
    const select = $('mapping-target');
    const candidates = models().filter(m => !m.hidden);
    select.innerHTML = candidates.map(m =>
      `<option value="${esc(m.id)}" data-label="${esc(m.providerLabel || m.provider || '')}">${esc(m.id)} · ${esc(m.providerLabel || m.provider || '')}</option>`).join('');
    if (presetTarget) select.value = presetTarget;
    // 下拉外壳（select.js）盯的是 MutationObserver，同步代码里接着读界面还是旧的，显式同步一次
    window.wbSelect?.sync?.(select);
    $('mapping-alias').value = '';
    $('mapping-modal-status').textContent = '';
    mappingPreview();
    $('mapping-modal').classList.add('open');
    setTimeout(() => $('mapping-alias').focus(), 0);
  }

  function closeMapping() {
    if (mappingSaving) return;
    $('mapping-modal').classList.remove('open');
  }

  async function saveMapping() {
    if (mappingSaving) return;
    const alias = $('mapping-alias').value.trim();
    const target = $('mapping-target').value;
    const status = $('mapping-modal-status');
    if (!alias) { status.textContent = '请填写映射名'; return; }
    if (!target) { status.textContent = '请选择目标模型'; return; }
    mappingSaving = true;
    $('mapping-modal-save').disabled = true;
    status.textContent = '保存中…';
    try {
      accept(await workbuddyDesktop.addModelMapping(alias, target));
      mappingSaving = false;
      closeMapping();
      toast(`✅ 已添加映射 ${alias} → ${target}`);
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
  $('mapping-target')?.addEventListener('change', mappingPreview);
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
