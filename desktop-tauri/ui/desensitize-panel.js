/* Agent2API · 敏感词脱敏面板（词表维护 / 开关 / 作用角色 / 作用提供商 / 命中统计） */
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

  // ─── 作用提供商：选项与状态行 ────────────────

  /** 最近一次渲染进下拉的提供商 id 列表：用来判断选项是否需要重建 */
  let providerIds = [];
  /** 作用提供商：是否有待提交的勾选（保存进行中又勾了新的） */
  let providerDirty = false;
  /** 作用提供商：是否正在提交（多选是「勾一家存一次」，用它与 dirty 配合合并请求） */
  let providersSaving = false;

  /**
   * 把下拉选项与提供商摘要对齐（新增 / 下线 provider 后自动跟上）。
   *
   * 只在集合真的变了时重建 —— 重建会清掉当前选中态，而 render() 每轮都要调它
   * （1 秒级的轮询、每次保存后的重读），无脑重建会让用户刚勾的那一项当场丢掉。
   * 选项顺序取摘要给的注册表顺序（workbuddy → raccoon → …），不重排成字母序：
   * 注册表顺序在账号页分组、设置页路由优先级里是同一套，三处对得上才好找。
   */
  function renderProviderOptions() {
    const select = $('desensitize-providers');
    if (!select) return;
    const list = window.wbProviders?.all?.() || [];
    const ids = list.map(item => item.id);
    // 摘要还没到位（主状态首屏 / 加载失败）时保留已有选项：清空会让当前勾选
    // 无处可放，界面看着像「配置丢了」
    if (!ids.length) return;
    if (ids.length === providerIds.length && ids.every((id, index) => id === providerIds[index])) return;

    const keep = new Set([...select.options].filter(option => option.selected).map(option => option.value));
    select.innerHTML = list
      .map(item => `<option value="${esc(item.id)}">${esc(item.label)}</option>`)
      .join('');
    // 重建后按 id 把原来的勾选还回去（摘要变了但这家还在时不该丢勾选）
    [...select.options].forEach(option => { option.selected = keep.has(option.value); });
    providerIds = ids;
  }

  /**
   * 选中数量说明：一行讲清「现在会处理谁」。一项都没勾时点明后果 ——
   * 后端会拒绝空列表（400「提供商列表为空」），提前在这里说明比让用户撞一次报错好。
   */
  function renderProviderState(scope) {
    const box = $('desensitize-providers-state');
    if (!box) return;
    const select = $('desensitize-providers');
    const selected = scope || new Set([...(select?.selectedOptions || [])].map(option => option.value));
    if (!selected.size) {
      box.textContent = '至少要选一家；想整体停用请用上面的开关';
      return;
    }
    const labels = [...selected].map(id => window.wbProviders?.labelOf?.(id) || id);
    box.textContent = `对 ${labels.join('、')} 的请求做脱敏`;
  }

  /** 读取下拉里当前勾选的提供商 id（保持选项顺序，去重由 select 本身保证） */
  function selectedProviders() {
    const select = $('desensitize-providers');
    return select ? [...select.selectedOptions].map(option => option.value) : [];
  }

  /**
   * 延后重跑保存：把「面板忙 / 正在提交」期间的一次勾选排进队列。
   *
   * 为什么不做成「立刻重试」的忙等：panelBusy 是被别的设置项占着的短时状态
   * （改开关、加词），隔一拍再看最省事。重试次数封顶，避免那种占用一直不放的
   * 情况下无限自旋；真的超了就放弃这一次 —— 界面上的勾选与后端会不一致，
   * 但用户再点一下就会重存（比无限自旋烧 CPU 强）。
   */
  let providerRetryTimer = null;
  function scheduleProviderSave(attempt = 0) {
    if (providerRetryTimer) return;
    if (attempt > 8) { providerDirty = false; return; }
    providerRetryTimer = setTimeout(() => {
      providerRetryTimer = null;
      if (providersSaving || panelBusy) { scheduleProviderSave(attempt + 1); return; }
      providerDirty = false;
      void saveProviders();
    }, 200);
  }

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
      // 作用提供商跟着标不可用：读不到配置时不能留着上一轮的勾选让人以为它还生效，
      // 也要挡住保存（否则会把一份「旧界面上的勾选」写给后端）
      const control = $('desensitize-providers');
      if (control) {
        control.disabled = true;
        [...control.options].forEach(option => { option.selected = false; });
        window.wbSelect?.sync?.(control);
      }
      const note = $('desensitize-providers-state');
      if (note) note.textContent = '不可用';
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

    // 作用提供商（多选下拉）：先把选项与提供商摘要对齐，再按后端给的作用集合勾选 ——
    // 顺序不能反：回勾一个还不存在的 option 会被浏览器静默丢掉（值留不住）
    renderProviderOptions();
    const scope = new Set(Array.isArray(data_.providers) ? data_.providers : []);
    const select = $('desensitize-providers');
    if (select) {
      // 读到了配置就把控件解禁（上一轮可能是「不可用」的禁用态）
      select.disabled = false;
      // 有勾选正在提交（或排队）时不动勾选态：app.js 每 20 秒刷新一次主状态并转发到
      // 本面板，若那次 GET 恰好落在 PUT 之前，回填的是旧集合 —— 用户会看到自己刚勾上
      // 的那一项弹回去。提交完成后由 saveProviders 的响应负责渲染（那里才是权威值）。
      if (!providersSaving && !providerDirty) {
        [...select.options].forEach(option => { option.selected = scope.has(option.value); });
        renderProviderState(scope);
      }
      // 选中态是直接写 DOM 的（多选不能用 select.value 回填：那只会留下最后一项），
      // 而增强组件的两个 observer 回调都跑在微任务里 —— 这里同步刷一次，
      // 勾选标记与触发器文案当场就是对的，不必等下一个微任务
      window.wbSelect?.sync?.(select);
    }

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
      // 提供商摘要可能还没进主状态（首屏、主状态加载失败），先补齐再渲染：
      // 否则多选项列表是空的，用户会以为「没有可选的提供商」
      await window.wbProviders?.load?.();
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

  /**
   * 保存作用提供商（全量替换语义）。
   *
   * ── 为什么不复用 guard() ──
   * 那个助手会把错误吞成一句 toast，本函数拿不到「成没成」这个信号，也就无法决定
   * 要不要把勾选回滚；而且它会把面板锁住 —— 多选下每勾一家就锁一次、浮层跟着
   * 被禁用关闭，连勾三家就要开三次浮层，很难用。
   *
   * ── 为什么要有 providerDirty / 定时重试 ──
   * 多选是「勾一家存一次」，用户连着勾时上一次可能还在飞；面板也可能正被别的
   * 设置项占着（panelBusy）。直接丢弃这些勾选最糟 —— 界面显示已勾上、后端却没存。
   * 所以统一记成「还有待提交的勾选」，由 scheduleProviderSave 排一个定时器在上一次
   * 落定后重跑，提交的永远是最新的那份全量集合（最后一次勾选胜出）。
   */
  async function saveProviders() {
    if (providersSaving || panelBusy) { providerDirty = true; scheduleProviderSave(); return; }
    const providers = selectedProviders();
    // 空列表先在本地挡下：后端对空数组会 400（文案「提供商列表为空」），
    // 提前拦一次省一趟往返，而且这里的提示能直接告诉用户该改哪个开关
    if (!providers.length) {
      toast('至少要选一家提供商；想停用脱敏请用上面的开关', 'err');
      // 把刚取消的勾还回去（不重读：后端状态本来就没错）
      if (current) apply(current);
      return;
    }

    providersSaving = true;
    // 待提交的勾选由 scheduleProviderSave 的定时器接手（置 dirty 的那条分支一定会
    // 排一个定时器）；这里不立刻重跑 —— 两条路都重试会多发一次同样的请求
    try {
      const next = await window.wbProviders.saveScope(providers);
      // 先把「正在提交」落定：成功分支里紧接着要 apply → render，而 render 在提交
      // 期间会刻意跳过勾选回填（见 render 里那段注释），不落定就白渲染一次
      providersSaving = false;
      // 期间用户又勾了别的项（dirty）：这份响应描述的是**旧集合**，不能拿它覆盖
      // 界面，也不能宣称「已更新成 X」—— 留给定时器补一次最新集合的提交
      if (providerDirty) return;
      // 接口按全量状态返回（与 roles / terms 同款）；换壳实现只回一份 providers
      // 数组时也接得住（补成 { providers } 让 render 认得）
      if (next) apply(Array.isArray(next) ? { ...current, providers: next } : next);
      const labels = providers.map(id => window.wbProviders?.labelOf?.(id) || id);
      toast(`✅ 作用提供商已更新：${labels.join('、')}`);
    } catch (error) {
      providersSaving = false;
      toast(`保存失败：${error.message}`, 'err');
      if (!providerDirty) await load();   // 回滚到后端的真实状态
    }
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
  // 作用提供商：select.js 增强后每次勾选都会派发一次 change（多选下浮层不关闭），
  // 所以这里是「勾一家存一次」。面板忙时 select.js 已把触发器禁用，不会重入。
  $('desensitize-providers')?.addEventListener('change', saveProviders);
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
