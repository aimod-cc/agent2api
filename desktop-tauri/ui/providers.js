/* Agent2API · 提供商目录（provider 摘要与 provider 维度读写的唯一前端入口） */

/**
 * 提供商目录：把「当前有哪些提供商、各自叫什么、有多少账号」收在一处，供三处面板共用：
 *   · 设置页「转发路由」的优先级编辑（route-panel.js）
 *   · 脱敏页「作用提供商」多选（desensitize-panel.js）
 *   · 请求明细的「提供商」列（logs-panel.js）
 * 为什么不让三处各拉一份：三处都在渲染里读这份数据，而日志明细会随 10 秒轮询反复
 * 重渲；各发一次请求不但浪费，还会出现「同一家 provider 在两处名字不一样」的瞬间
 * （两个响应先后到达）。集中一份也对得上「新 provider 注册后自动出现」这条要求 ——
 * 摘要本身就含注册表里的**全部** provider，界面不必知道任何一家具体名字。
 *
 * 数据源：`GET /api/session` 的 `accounts.providers`（后端 store 的 provider 摘要，
 * 形状 `[{id,label,count}]`，没有账号的那家 count 为 0 也照样在）。
 * app.js 每 20 秒刷新一次主状态、每次保存后也会刷新，所以绝大多数调用连请求都不发，
 * 直接读主状态；只有在主状态还没就绪（首屏 / 加载失败）时才补一次请求。
 *
 * 与 accounts-model.js 的分工：那个文件管「账号」维度的判定与标签，本文件只管
 * 「提供商」这份目录本身，不碰账号数据。
 */
(() => {
  const api = workbuddyDesktop;

  /** 主状态读不到时的兜底副本（本模块自己拉的那一份） */
  let fallback = null;
  /** 进行中的请求：并发调用合并成一次 */
  let inflight = null;

  /**
   * 摘要项归一化：只认 id 为非空字符串的项，label 缺省用 id 兜底。
   * 宁可显示一个陌生的英文 id，也好过留一块空白 —— 与后端 provider_label 的
   * 取舍一致（未注册的 id 原样回显）。
   */
  function normalize(list) {
    return (Array.isArray(list) ? list : [])
      .filter(item => item && typeof item.id === 'string' && item.id)
      .map(item => ({
        id: item.id,
        label: typeof item.label === 'string' && item.label ? item.label : item.id,
        count: Number(item.count) || 0,
      }));
  }

  /** 主状态里的那份摘要（app.js 的 refresh 一直在维护它，读它零成本） */
  function fromAppState() {
    const list = window.wbApp?.getState?.()?.accounts?.providers;
    return Array.isArray(list) ? normalize(list) : [];
  }

  /**
   * 提供商列表（同步）。优先用主状态里的那一份（始终最新），没有才退回兜底副本 ——
   * 兜底副本只在主状态暂时不可用时才有值，不会拿旧数据盖掉更新的主状态。
   */
  function all() {
    const live = fromAppState();
    if (live.length) return live;
    return fallback || [];
  }

  /** 按 id 取展示名：空值给空串、查不到就原样回显 id（调用方自行决定占位符） */
  function labelOf(id) {
    const key = String(id ?? '').trim();
    if (!key) return '';
    const found = all().find(item => item.id === key);
    return found ? found.label : key;
  }

  /** 拉一次 /api/session，只取 providers 摘要存进兜底副本（并发合并成一次） */
  function fetchProviders() {
    if (!inflight) {
      inflight = api.getState()
        .then(data => {
          const list = normalize(data?.accounts?.providers);
          if (list.length) fallback = list;
          return list;
        })
        .finally(() => { inflight = null; });
    }
    return inflight;
  }

  /**
   * 对外加载入口：主状态里已有就直接返回（不发请求），否则补一次请求。
   *
   * 失败不抛错：调用方拿到的是「目前已知的那一份」，界面照常把已有的行渲染出来，
   * 只是可能少几家 —— 整块报错反而更糟（首屏那一瞬间主状态本来就可能还没到）。
   * 真的不可用由各自面板的徽标表达（它们各自有更准确的失败语义）。
   */
  async function load({ force = false } = {}) {
    const live = fromAppState();
    if (live.length && !force) return live;
    try {
      const fetched = await fetchProviders();
      return fetched.length ? fetched : all();
    } catch (error) {
      console.warn('读取提供商列表失败:', error.message);
      return all();
    }
  }

  /**
   * 保存脱敏作用提供商（`PUT /api/desensitize/providers`，body `{providers:[id…]}`）。
   *
   * 为什么要分两条路：桥接层（src-tauri/src/bridge.rs 的 BRIDGE_JS）已经暴露了
   * `setDesensitizeProviders`，桌面端走的都是它；下面那条直连 `api_request` 只是
   * 「壳里还没有这个方法」时的兜底（旧版壳、或将来桥接层漏加方法时界面不炸）。
   * 两条路的入参与返回值口径一致（都是管理 API 的 request 对象、返回值同样是被
   * 解包过的 data），错误照旧是 rejected Promise 里的可读消息。
   *
   * 注意 `__TAURI_INTERNALS__` 是 Tauri 注入的底层 IPC，`withGlobalTauri: false`
   * 不影响它 —— bridge.rs 自己也是这么取的。非桌面环境（浏览器直开 ui/）下它会缺失，
   * 此时给一句与桥接层同款的错误，界面按普通失败处理。
   */
  function saveScope(providers) {
    const list = (Array.isArray(providers) ? providers : [])
      .filter(id => typeof id === 'string' && id);
    const bridged = api.setDesensitizeProviders;
    if (typeof bridged === 'function') return bridged(list);
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      return Promise.reject(new Error('桌面运行时不可用（Tauri 未初始化）'));
    }
    return internals.invoke('api_request', {
      request: { method: 'PUT', path: '/api/desensitize/providers', body: { providers: list } },
    });
  }

  window.wbProviders = { all, labelOf, load, saveScope };
})();
