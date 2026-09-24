/* Agent2API · 自定义提供商的提供商级操作（读记录 / 改配置 / 删除） */
/* global wbApp */

/**
 * 自定义提供商的**提供商级**操作只有三件事：读一条记录、改它（名称 / 协议 /
 * Base URL）、删它（级联删名下账号）。本文件只提供这三件事，界面不在这里 ——
 * 字段与「保存」都在**账号设置弹窗**里就地渲染（account-panel.js 的
 * mountProviderSection）。
 *
 * ── 入口的演变（为什么从组头搬到账号设置）──────────────────────
 * 这个入口最初挂在账号表 custom 组的组头行上（「模型管理 / 编辑 / 删除」）。
 * 用户随后要求账号列表**不要分组**（组头行整个下线，见 accounts-view.js 的
 * render 说明），入口于是搬到账号页工具栏，做成一颗「自定义提供商」按钮 →
 * 管理弹窗（列出全部自定义提供商，每行三项操作）。模型清单统一到「模型管理」页
 * 之后，那个弹窗里只剩「编辑 / 删除」两件事：它列出的名称 / 协议 / 地址 / 账号数
 * 在别处都已能读到（名称与模型数在模型管理页左栏、账号数在账号页筛选器），
 * 「模型清单 →」退化成一次跳转 —— 整个弹窗与那颗按钮因此一并移除。
 *
 * 编辑落在**账号自己的设置**里：改 Base URL 的动机通常正是「这个账号连不上了」，
 * 就地改比先退出去找一个管理入口近一步；删除也放在同一段里（级联删账号是个
 * 危险动作，与它要删的东西摆在一起比单开一个入口更容易读懂）。
 *
 * ── 已知的可达性缺口 ─────────────────────────────────────────
 * 账号设置是唯一的编辑入口，所以**名下账号被删光**的提供商没有编辑 / 删除的
 * 入口（创建流程是「创建并添加账号」，正常都有一条账号）。它仍出现在模型管理页
 * 左栏与账号页的提供商筛选器里，只是点不进去改；去那家加一条账号即可恢复入口。
 *
 * 目录数据（customList / refreshCustom / customRequest）全部来自 providers.js
 * —— 自定义提供商的前端取数只有那一处实现。
 */
(() => {
  const { esc, toast } = wbApp;
  const providers = window.wbProviders;

  /** custom- 前缀判据（与后端 ID_PREFIX 一致；内置家永远不走本文件） */
  function isCustomProviderId(id) {
    return typeof id === 'string' && id.startsWith('custom-');
  }

  /** 该提供商此刻名下的账号数（读主状态的全量列表，不跟筛选走） */
  function accountCountOf(providerId) {
    const accounts = wbApp.getState?.()?.accounts?.accounts || [];
    return accounts.filter(account => (account?.provider || 'workbuddy') === providerId).length;
  }

  /**
   * 取一条提供商记录：先读目录缓存，没有（缓存还没拉到 / 刚被别人改过）就
   * 现拉一次 —— 返回 null 时调用方按「可能已被删除」处理。
   * 账号设置弹窗用它判断「这个账号属不属于自定义家」并预填三个字段，
   * 所以这里不认 id 前缀就够：**记录本身**才是字段的事实来源。
   */
  async function findProvider(providerId) {
    if (!isCustomProviderId(providerId)) return null;
    const cached = (providers.customList() || []).find(item => item.id === providerId);
    if (cached) return cached;
    const list = await providers.refreshCustom();
    return (list || []).find(item => item.id === providerId) || null;
  }

  /**
   * 改动落库后的收尾：目录先刷（账号页的提供商徽章、筛选器与模型管理页左栏都读它），
   * 再全量刷一次账号列表补重绘。
   *
   * 两处刷新各自兜错：刷新失败只影响本次界面同步（后续轮询会自愈），
   * 不能让它把「已经存成功」报成失败。
   */
  async function refreshAfterChange() {
    try { await providers.refreshCustom(); } catch { /* 下一次轮询会自愈 */ }
    try { await wbApp.refresh?.(); } catch { /* 同上 */ }
  }

  /**
   * 保存提供商配置：POST /api/custom-providers/update（三个字段一起提交，
   * 都是表单上的必填值，后端也会再校验一次）。
   * 失败原样抛出（不在这里 toast）：调用方是账号设置弹窗，它要把「账号已保存、
   * 提供商未更新」这句话写在弹窗里，笼统报一句「保存失败」会把两件事混成一件。
   */
  async function update({ id, name, protocol, baseUrl }) {
    if (!isCustomProviderId(id)) throw new Error('不是自定义提供商');
    await providers.customRequest('POST', '/api/custom-providers/update', { id, name, protocol, baseUrl });
    await refreshAfterChange();
  }

  /**
   * 删除提供商：二次确认 → POST /api/custom-providers/remove。
   * 返回**是否真的删了**，调用方据此决定要不要关掉自己的界面（账号都没了）。
   *
   * N 用**该家名下账号数**现算（与账号页的计数同源：同一份账号列表按 provider
   * 过滤）—— 确认文案必须先把「会连带删掉多少条」说清楚，这是级联删除与单条、
   * 可逆操作的分界。响应里的 accountsRemoved 是权威值，成功提示用它（万一与
   * 本地计数不一致，以删掉的真实条数为准）。
   */
  async function remove(providerId) {
    const provider = await findProvider(providerId);
    if (!provider) {
      toast('该自定义提供商已不存在（可能已被删除），请刷新后重试', 'err');
      return false;
    }
    const name = provider.name || providerId;
    const count = accountCountOf(providerId);
    // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行，危险确认一律走 wbConfirm
    const ok = await window.wbConfirm?.ask?.({
      title: '删除自定义提供商',
      html: `确定删除自定义提供商「<strong>${esc(name)}</strong>」？`
        + `将同时删除该提供商下 <strong>${count}</strong> 个账号，删除后无法恢复。`,
      okText: '删除',
      okClass: 'danger',
    });
    if (!ok) return false;
    try {
      const data = await providers.customRequest('POST', '/api/custom-providers/remove', { id: providerId });
      const removed = Number(data?.accountsRemoved);
      toast(`✅ 已删除自定义提供商「${name}」${Number.isFinite(removed) ? `及 ${removed} 个账号` : ''}`);
      await refreshAfterChange();
      return true;
    } catch (error) {
      toast(`删除失败：${error instanceof Error ? error.message : String(error)}`, 'err');
      return false;
    }
  }

  window.wbCustomProvidersUi = { find: findProvider, update, remove };
})();
