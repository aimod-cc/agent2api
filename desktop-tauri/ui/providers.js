/* Agent2API · 提供商目录（provider 摘要与 provider 维度读写的唯一前端入口） */

/**
 * 提供商目录：把「当前有哪些提供商、各自叫什么、有多少账号」收在一处，供两处面板共用：
 *   · 设置页「转发路由」的优先级编辑（route-panel.js）
 *   · 请求日志的「提供商」列（requests-panel.js）
 * 为什么不让两处各拉一份：三处都在渲染里读这份数据，而请求日志会随轮询反复
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
 *
 * 自定义提供商（custom- 前缀）也归本文件管：注册表摘要里没有它们，这里额外
 * 拉一份 `GET /api/custom-providers` 把 {id: name} 并进 labelOf 的查找链，并
 * 暴露列表缓存（customList）、刷新（refreshCustom）与通用管理 API 调用
 * （customRequest）给添加表单与账号页的管理弹窗 —— 自定义提供商的前端取数
 * 只有这一处实现，见下面「自定义提供商」一节的说明。
 *
 * 曾经的第三个消费者是脱敏页的「作用提供商」多选，随该页一起删除。
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

  // ─── 自定义提供商（custom- 前缀）─────────────
  //
  // 自定义提供商是**运行期数据**：后端注册表（providers::summary_json）只枚举
  // 内置八家，因此 /api/session 的 providers 摘要里没有它们 —— 但账号与请求
  // 日志里会出现 `custom-xxx` 这样的 provider id，展示名（用户起的名字）只能
  // 自己拉一份 `GET /api/custom-providers` 来对上。
  //
  // ── 为什么只并进 labelOf、不并进 all() ──────────────────────
  // all() 的消费方之一是「添加账号」弹窗的提供商选择（add-provider-forms.js）：
  // 每个自定义提供商各占一项会让那个分段控件随建随涨，而自定义提供商的添加
  // 入口是单独一项「自定义提供商」（见 add-custom-provider.js）。所以列表本身
  // 按需暴露（customList），只有 {id: name} 并进 labelOf 的查找链 —— 账号表的
  // 提供商徽章、筛选下拉、请求日志的提供商列都走它，一处维护处处生效。

  /** 进行中的列表请求：并发调用合并成一次（与上面 fetchProviders 同一口径） */
  let customInflight = null;
  /** 最近一次 GET /api/custom-providers 的列表（后端按 createdAt 升序） */
  let customItems = [];
  /** id → 展示名；添加 / 编辑 / 删除后由 refreshCustom() 更新 */
  const customNames = new Map();

  /** 吸收一份列表：只认带 custom- 前缀 id 的条目（陌生形状不进目录），name 缺省回退 id */
  function absorbCustomProviders(list) {
    customItems = (Array.isArray(list) ? list : []).filter(item =>
      item && typeof item.id === 'string' && item.id.startsWith('custom-'));
    customNames.clear();
    for (const item of customItems) {
      customNames.set(item.id, typeof item.name === 'string' && item.name ? item.name : item.id);
    }
  }

  /**
   * 调一次管理 API。既有数据源（providers 摘要）走桥接的具名方法，但自定义
   * 提供商是后端新加的接口、桥（bridge.rs 已定稿）没给它留具名方法，因此与
   * add-provider-forms.js 的 postAccount 走同一条通用链：壳的 api_request 命令。
   * 失败值由壳归一成 Error（后端 400 的 error 文案在里面），调用方照常 catch。
   */
  function customRequest(method, path, body) {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      return Promise.reject(new Error('桌面运行时不可用（Tauri 未初始化）'));
    }
    return internals.invoke('api_request', {
      request: { method, path, body: body === undefined ? null : body },
    });
  }

  /**
   * 拉一次自定义提供商列表（并发合并成一次；失败静默保留旧值 —— 目录偶发
   * 打不通不该把已显示的名字抹掉，与 load() 的失败取向一致）。
   *
   * 拉到且名字真的变了时补一次账号表重绘：首屏那次渲染常发生在本请求回来
   * 之前，不补的话账号表 / 筛选器里的 custom id 要裸奔到下一轮 20 秒轮询。
   * 只在名字变化时重绘，添加 / 删除后的那次 refreshCustom 不会引发多余重绘
   * （那些路径本来就跟着 wbApp.refresh() 全量刷一遍）。
   */
  function fetchCustomProviders() {
    if (!customInflight) {
      customInflight = customRequest('GET', '/api/custom-providers')
        .then(data => {
          const before = [...customNames.entries()];
          absorbCustomProviders(data?.providers);
          const changed = before.length !== customNames.size
            || before.some(([id, name]) => customNames.get(id) !== name);
          // 账号数据还没就绪（首次 refresh 未返回）时不补重绘：那会把列表容器里
          // 的「正在加载…」覆盖成「暂无账号」，等 refresh 自己的那轮渲染即可
          if (changed && window.wbApp?.getState?.()?.accounts?.accounts?.length) {
            window.wbAccountsView?.render?.();
          }
          return customItems;
        })
        .catch(error => {
          console.warn('读取自定义提供商列表失败:', error.message);
          return customItems;
        })
        .finally(() => { customInflight = null; });
    }
    return customInflight;
  }

  /** 自定义提供商列表（同步读缓存；没拉到 / 一家都没建过时是空数组） */
  function customList() {
    return customItems;
  }

  /** 刷新自定义提供商目录：添加 / 编辑 / 删除后由各界面回调（见 add-custom-provider.js 等） */
  function refreshCustom() {
    return fetchCustomProviders();
  }

  /**
   * 协议下拉的三个选项：值与后端 custom_providers::PROTOCOLS 逐字一致，
   * 文案即添加表单与编辑弹窗共用的展示名。放在本文件（目录层）而不是让两个
   * 表单各写一份：新增一种协议时值与文案只有一处要改。
   */
  const PROTOCOL_OPTIONS = [
    { value: 'chat_completions', label: 'OpenAI - Chat Completions' },
    { value: 'responses', label: 'OpenAI - Responses' },
    { value: 'anthropic', label: 'Anthropic - Messages' },
  ];

  /** 按 id 取展示名：空值给空串、查不到就原样回显 id（调用方自行决定占位符） */
  function labelOf(id) {
    const key = String(id ?? '').trim();
    if (!key) return '';
    const found = all().find(item => item.id === key);
    if (found) return found.label;
    // 自定义提供商不在注册表摘要里：问自定义目录，再取不到才回显 id
    return customNames.get(key) || key;
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

  // 加载期拉一次自定义提供商目录：账号表 / 筛选器首屏就要显示 custom id 的
  // 展示名，等用户打开某个面板再拉就晚了。失败静默（console.warn），下一次
  // refreshCustom（添加 / 删除后）或重启会再试。
  void fetchCustomProviders();

  window.wbProviders = {
    all,
    labelOf,
    load,
    // 自定义提供商目录（custom- 前缀）：列表缓存、刷新与通用管理 API 调用。
    // 添加 / 编辑表单与账号页的管理弹窗都从这里取数，保证同一份目录只有一处实现
    customList,
    refreshCustom,
    customRequest,
    // 协议下拉的选项（添加表单与编辑弹窗共用，见上面 PROTOCOL_OPTIONS 的说明）
    PROTOCOL_OPTIONS,
  };
})();
