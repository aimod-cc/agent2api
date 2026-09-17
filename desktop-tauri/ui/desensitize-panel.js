/* WorkBuddy 本地代理 · 敏感词脱敏面板（词表维护 / 开关 / 作用角色 / 命中统计） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的面板模块：自持一份脱敏状态，避免与主 state 的合并逻辑互相干扰
 * （/api/session 里只带脱敏摘要，若混进主 state 会覆盖掉完整的词表）。
 *
 * 依赖 app.js 暴露的 window.wbApp（esc / toast）。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  let panelBusy = false;
  let current = null; // 最近一次从后端读到的完整脱敏状态

  // ─── 渲染 ──────────────────────────────────

  function render(data) {
    if (data !== undefined) current = data;
    const badge = $('desensitize-badge');
    const toggle = $('desensitize-enabled');
    if (!badge || !toggle) return;

    if (!current) {
      badge.className = 'badge warn';
      badge.textContent = '不可用';
      $('desensitize-state').textContent = '代理未返回脱敏配置';
      return;
    }

    const data_ = current;
    toggle.checked = data_.enabled === true;
    toggle.disabled = false;
    badge.className = data_.enabled ? 'badge ok' : 'badge';
    badge.textContent = data_.enabled ? `已启用 · ${data_.termCount} 个词` : '已关闭';
    // 作用角色由下方勾选框直接呈现，这里不重复罗列，只说明当前是否生效
    $('desensitize-state').textContent = data_.enabled
      ? '按下方勾选的角色改写请求'
      : '当前不处理任何消息；关闭状态已保存，重启后仍然生效';

    // 作用角色勾选
    const active = new Set(Array.isArray(data_.roles) ? data_.roles : []);
    document.querySelectorAll('#desensitize-roles input[type="checkbox"]').forEach(box => {
      box.checked = active.has(box.value);
    });

    // 词表 chips
    const list = $('term-list');
    const terms = Array.isArray(data_.terms) ? data_.terms : [];
    if (!terms.length) {
      list.innerHTML = '<div class="empty" style="padding:14px 0">词表为空，脱敏不会命中任何内容</div>';
    } else {
      list.innerHTML = terms.map(term => `
        <span class="term-chip"><span>${esc(term)}</span>
          <button type="button" data-term="${esc(term)}" title="删除该词" aria-label="删除 ${esc(term)}">×</button>
        </span>`).join('');
    }

    // 命中统计
    const stats = data_.stats || {};
    $('stat-requests').textContent = String(stats.requests ?? 0);
    $('stat-changed').textContent = String(stats.changedRequests ?? 0);
    $('stat-hits').textContent = String(stats.totalHits ?? 0);
    const top = Array.isArray(stats.topTerms) ? stats.topTerms : [];
    $('stat-terms').innerHTML = top.length
      ? '<span class="detail" style="font-size:12px;align-self:center">命中最多：</span>'
        + top.map(item => `<span class="stat-term">${esc(item.term)} ×${esc(item.count)}</span>`).join('')
      : '';

    if (data_.file) $('desensitize-file').textContent = data_.file;
  }

  /** 从后端拉完整状态并渲染 */
  async function load() {
    try {
      render(await api.getDesensitize());
    } catch (error) {
      console.warn('读取脱敏配置失败:', error.message);
      render(null);
    }
  }

  // ─── 操作 ──────────────────────────────────

  function apply(data) {
    if (data && typeof data === 'object') render(data);
  }

  /** 解析输入框里的词：逗号、分号、换行与中文标点都当分隔符 */
  function parseTermInput(value) {
    return String(value || '')
      .split(/[,，;；\n\r\t]+/)
      .map(item => item.trim())
      .filter(Boolean);
  }

  async function guard(button, label, action) {
    if (panelBusy) return;
    panelBusy = true;
    const original = button?.textContent;
    if (button) { button.disabled = true; if (label) button.textContent = label; }
    try {
      await action();
    } catch (error) {
      toast(`操作失败：${error.message}`, 'err');
    } finally {
      panelBusy = false;
      if (button) { button.disabled = false; if (label) button.textContent = original; }
    }
  }

  async function toggleEnabled(enabled) {
    await guard(null, null, async () => {
      apply(await api.setDesensitizeEnabled(enabled));
      toast(enabled ? '✅ 已开启内容脱敏' : '已关闭内容脱敏');
    });
    // 失败时把开关拨回真实状态
    const toggle = $('desensitize-enabled');
    if (toggle && current && toggle.checked !== (current.enabled === true)) {
      toggle.checked = current.enabled === true;
    }
  }

  async function saveRoles() {
    const roles = [...document.querySelectorAll('#desensitize-roles input:checked')].map(box => box.value);
    await guard(null, null, async () => {
      apply(await api.setDesensitizeRoles(roles));
      toast(`✅ 作用角色已更新：${roles.join('、') || '（空，等同关闭）'}`);
    });
  }

  async function addTerms() {
    const input = $('term-input');
    const terms = parseTermInput(input.value);
    if (!terms.length) { toast('请先输入要添加的词', 'err'); return; }
    await guard($('btn-add-term'), '添加中…', async () => {
      apply(await api.addDesensitizeTerms(terms));
      input.value = '';
      toast(`✅ 已添加 ${terms.length} 个词`);
    });
  }

  async function removeTerm(term) {
    await guard(null, null, async () => {
      apply(await api.removeDesensitizeTerm(term));
      toast(`已删除「${term}」`);
    });
  }

  // ─── 事件绑定 ──────────────────────────────

  $('desensitize-enabled').addEventListener('change', event => toggleEnabled(event.target.checked));
  document.querySelectorAll('#desensitize-roles input[type="checkbox"]').forEach(box => {
    box.addEventListener('change', saveRoles);
  });
  $('btn-add-term').addEventListener('click', addTerms);
  $('term-input').addEventListener('keydown', event => {
    if (event.key === 'Enter') { event.preventDefault(); addTerms(); }
  });
  $('term-list').addEventListener('click', event => {
    const button = event.target.closest('button[data-term]');
    if (button) removeTerm(button.dataset.term);
  });
  $('btn-reset-terms').addEventListener('click', async () => {
    if (!confirm('恢复为内置默认词表？当前自定义的词会被替换掉。')) return;
    await guard($('btn-reset-terms'), '恢复中…', async () => {
      apply(await api.resetDesensitizeTerms());
      toast('✅ 已恢复默认词表');
    });
  });
  $('btn-reset-stats').addEventListener('click', async () => {
    await guard($('btn-reset-stats'), '清空中…', async () => {
      apply(await api.resetDesensitizeStats());
      toast('命中统计已清空');
    });
  });

  window.wbDesensitizePanel = { load, render };

  // 自持首屏加载：即便 app.js 的 refresh 失败，面板也能独立显示真实状态
  void load();
})();
