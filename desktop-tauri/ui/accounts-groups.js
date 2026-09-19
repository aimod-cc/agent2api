/* Agent2API · 账号的 provider 维度、基础判定与分组（纯逻辑） */
/* global wbApp */

/**
 * 从 accounts-model.js 按职责拆出的「provider 维度 + 领域判定 + 筛选与队列」层：
 * provider 能力表与摘要归一化、账号可用性 / 限流判定、筛选口径与分段计数、
 * 全局队列的位置表与队首派生。
 *
 * 这些都是**纯逻辑**：不读写模块状态、不碰事件、不生成卡片级 HTML
 * （不生成任何卡片或行级 HTML，那些在 accounts-model / accounts-table）。
 * 之所以单独成文件：它是账号页（accounts-view.js）与顶栏状态（app.js）共用的口径，
 * 而「标签 / 卡片渲染」是另一件事（留在 accounts-model.js），两者混在一起会
 * 让那个文件持续膨胀（约定单文件 ≤800 行）。
 *
 * 通过 window.wbAccountsGroups 暴露，并在 accounts-model.js 里按名解构 + 原样再导出：
 * window.wbAccountsModel 的公开面与拆分前完全一致，消费方的引用路径不必改。
 * **必须在 accounts-model.js 之前加载**（见 index.html 的 script 顺序）。
 */
(() => {
  const { esc } = wbApp;
  // ─── provider 维度 ──────────────────────────
  //
  // Agent2API 之后账号分属不同提供商（workbuddy / raccoon / 后续 catpaw、autoclaw）。
  // 界面**一律按 GET /api/accounts 的 providers 摘要动态渲染**，不写死两家：
  // 筛选器取 id 列表与 label、表格走能力表，
  // 未登记的 provider 走 GENERIC 兜底。后端注册表加一家时前端零改动
  // （添加表单另说，见 account-panel.js 的 provider 分叉与「即将上线」占位）。

  /** 缺省 provider id（后端注册表的默认项；旧账号记录没有该字段时的兜底） */
  const DEFAULT_PROVIDER_ID = 'workbuddy';
  /** 小浣熊 provider id（只有它需要「桌面端实时登录态」这类专属标记） */
  const RACCOON_PROVIDER_ID = 'raccoon';

  /**
   * provider 能力表：决定卡片上出现哪些按钮、哪行明细显示什么。
   *
   * 为什么是「按 provider 查表」而不是在渲染处写 if：账号页的每个分支（积分按钮、
   * 签到按钮、版本徽章、明细行字段名）都要问同一个问题 ——「这家有没有这个概念」。
   * 散在各处写 if 的话，加一家就要翻一遍全文件，漏掉一处不报错、只静默少一个按钮。
   *
   * usage **四家都是 true**（余额 / 积分查询已扩到全部提供商）：各家的接口、鉴权、
   * 凭证来源全不相同，但都由各自的适配器实现（`ProviderAdapter::query_usage`），
   * 前端只回答「这一家有没有这个概念」。CatPaw 的余额接口要单独配置一个网页会话
   * 凭证（token2），没配置时后端返回可识别的「未配置」、面板显示成中性提示
   * （见 usage-panel.js）—— 所以它的按钮照样渲染，用户才有「去配置」的入口。
   * checkin **只有 workbuddy**（签到的活动确实只有那一家有）；
   * edition 决定卡片是否显示国内版 / 国际版徽章与组内二级分组；
   * identifier / expiry 是「账号标识」与「有效期」在记录里的键名
   * （workbuddy 用 uid / expiresAt，小浣熊用 userId / tokenExpiresAt）。
   */
  const PROVIDER_FEATURES = {
    workbuddy: { usage: true, checkin: true, edition: true, identifier: 'uid', expiry: 'expiresAt' },
    raccoon: { usage: true, checkin: false, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt' },
    catpaw: { usage: true, checkin: false, edition: false, identifier: 'uid', expiry: 'tokenExpiresAt' },
    autoclaw: { usage: true, checkin: false, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt' },
  };

  /**
   * 未登记 provider 的兜底能力：不显示积分 / 签到 / 版本 —— 这三个都是 provider
   * 私有概念，未知的家不该被假定拥有（宁可少一个按钮，也不要一个点了必然报错的入口）。
   * 标识字段假定成 userId（catpaw / autoclaw 的账号标识都是这类用户 id），
   * 取不到时明细行自动少一项，不会显示空值。
   */
  const GENERIC_FEATURES = {
    usage: false, checkin: false, edition: false, identifier: 'userId', expiry: 'tokenExpiresAt',
  };

  /** 账号所属 provider（字段缺失 / 非字符串按默认 provider 兜底，与后端 store 口径一致） */
  function providerOf(account) {
    const id = account?.provider;
    return typeof id === 'string' && id ? id : DEFAULT_PROVIDER_ID;
  }

  /** provider id → 能力表（未登记的家走 GENERIC_FEATURES） */
  function providerFeatures(providerId) {
    return PROVIDER_FEATURES[providerId] || GENERIC_FEATURES;
  }

  /**
   * providers 摘要归一化：`{ providers: [{id,label,count}] }`。
   *
   * 两处兜底是刻意的（摘要缺一项就让整页空白，代价远大于一个小偏差）：
   *   · 后端没给摘要（旧版 / 首屏 state 尚未到达）→ 按现有账号派生出分组所需的 id 列表；
   *   · 摘要里没有、但账号里出现的 provider → 补在末尾，**计数按现有账号算**
   *     —— 摘要更新滞后于数据时，筛选器不能写「catpaw（0）」而列表里明明有一组它。
   * label 优先问共享的 wbProviders 目录（providers.js 按 id 回显摘要里的中文名），
   * 都取不到时退化成 id 本身：宁可用一个陌生英文 id，也好过留一块空白。
   */
  function providerSummaries(snapshot) {
    const list = Array.isArray(snapshot?.providers) ? snapshot.providers : [];
    const accounts = Array.isArray(snapshot?.accounts) ? snapshot.accounts : [];
    // 账号按 provider 计数：既用来补缺项的 count，也保证账号里的家一定会出现
    const counts = new Map();
    accounts.forEach(account => {
      const id = providerOf(account);
      counts.set(id, (counts.get(id) || 0) + 1);
    });
    const known = new Map();
    list.forEach(item => {
      if (!item?.id) return;
      known.set(String(item.id), {
        id: String(item.id),
        label: String(item.label || item.id),
        count: Number(item.count) || 0,
      });
    });
    counts.forEach((count, id) => {
      if (known.has(id)) return;
      known.set(id, {
        id,
        label: window.wbProviders?.labelOf?.(id) || id,
        count,
      });
    });
    if (!known.size) known.set(DEFAULT_PROVIDER_ID, { id: DEFAULT_PROVIDER_ID, label: 'WorkBuddy', count: 0 });
    return [...known.values()];
  }

  /** 账号标识（workbuddy 是 uid，小浣熊是 userId）；取不到返回空串 */
  function identifierOf(account) {
    return String(account?.[providerFeatures(providerOf(account)).identifier] || '');
  }

  /** token 过期时间戳（毫秒，0 表示记录里没有这个字段） */
  function tokenExpiryOf(account) {
    const value = Number(account?.[providerFeatures(providerOf(account)).expiry]);
    return Number.isFinite(value) ? value : 0;
  }

  /** 该账号所属 provider 是否有积分概念（没有就不渲染「积分」按钮，也不参与批量查询） */
  function supportsUsage(account) {
    return providerFeatures(providerOf(account)).usage;
  }

  /** 是否为「桌面端实时登录态」账号（凭证实时读客户端文件；可禁用、也可删除） */
  function isDesktopAccount(account) {
    return account?.desktop === true;
  }

  // ─── 基础判定 ──────────────────────────────

  function typeLabel(type) {
    if (type === 'enterprise') return '企业';
    if (type === 'ultimate') return '旗舰';
    return '个人';
  }

  /**
   * 转发顺序排序键：优先级升序，并列时按加入时间。
   * 与后端 src/workbuddy-account-store.mjs 的 byPriorityOrder 保持一致
   * （渲染层无法 import 后端 ESM，只能同构实现；改一处必须同步另一处）。
   * 优先级在写入侧强制唯一，并列只会出现在手工编辑的账号文件里。
   */
  function byPriorityOrder(a, b) {
    const diff = Number(a?.priority ?? 100) - Number(b?.priority ?? 100);
    if (diff !== 0) return diff;
    return (Number(a?.addedAt) || 0) - (Number(b?.addedAt) || 0);
  }

  /** 账号是否启用（禁用账号不参与转发） */
  function isEnabled(account) {
    return account?.enabled !== false;
  }

  /**
   * 账号是否处于限流状态（存在未到恢复时间的限额记录）。
   * 传入 model 时只判定该模型 —— 限额是按模型记的，这是「模型筛选」的核心：
   * 一个账号可能对 A 模型限额、对 B 模型完全正常。
   */
  function isRateLimited(account, model = '') {
    const limits = account?.rateLimits || {};
    const now = Date.now();
    if (model) return Number(limits[model]?.resetAt) > now;
    return Object.values(limits).some(info => Number(info?.resetAt) > now);
  }

  /** 该账号此刻能否承接指定模型的请求（与后端 accountUsability 同构） */
  function usableForModel(account, model) {
    if (!isEnabled(account)) return false;
    return !isRateLimited(account, model);
  }

  /**
   * 账号是否有可用凭证（后端公开形态的 `hasCredentials`，四家同字段名）。
   *
   * 与后端 `StoredAccount::has_credentials` 同义：桌面端实时账号（记录里按设计
   * 不落 token，凭证在客户端登录态文件里）也算「有凭证」。转发选路挑出候选后还要
   * 取到会话才用，取不到就继续往后找 —— 凭证是转发口径的一部分，前端推算
   * 「这一家会走谁」时必须一起判，否则会给一个转发时必然被跳过的账号打 ★。
   * 字段缺失（旧版后端）按「有凭证」处理，不让所有 ★ 凭空消失。
   */
  function hasCredentials(account) {
    return account?.hasCredentials !== false;
  }

  /** 账号所属版本：cn=国内 / intl=国际（缺省视为国内，兼容旧账号记录） */
  function accountEdition(account) {
    return account?.edition === 'intl' ? 'intl' : 'cn';
  }

  /** 只有有签到活动的那一家（workbuddy）的国内版账号才参与签到 */
  function supportsCheckin(account) {
    if (!providerFeatures(providerOf(account)).checkin) return false;
    return accountEdition(account) !== 'intl';
  }

  /** 可参与签到的账号（一键签到只用这批：workbuddy + 启用 + 国内版） */
  function checkinableAccounts(list) {
    return (list || []).filter(account => isEnabled(account) && supportsCheckin(account));
  }

  // ─── 筛选维度 ───────────────────────────────
  //
  // 三个维度各自独立、可任意组合：provider / edition / enabled / limit。
  // 判定函数集中在这里，让「可见列表」与「分段计数」共用同一份口径 ——
  // 否则徽标数字与点进去看到的结果会各算各的。
  // 曾经的「模型」维度已随「账号页不再选模型」下线：限流是按模型记的，
  // 「限流」现在指的是该账号**任一模型**限流中，具体哪个模型去行上的
  // 「限流」明细里看（见 accounts-model 的 limitPanelHtml）。

  function matchProvider(account, filter) {
    return filter.provider === 'all' || providerOf(account) === filter.provider;
  }

  function matchEdition(account, filter) {
    return filter.edition === 'all' || accountEdition(account) === filter.edition;
  }

  function matchEnabled(account, filter) {
    if (filter.enabled === 'all') return true;
    return filter.enabled === 'enabled' ? isEnabled(account) : !isEnabled(account);
  }

  /** 已禁用账号既不算「正常」也不算「已限流」：限流状态只对参与转发的账号有意义 */
  function matchLimit(account, filter) {
    if (filter.limit === 'all') return true;
    if (!isEnabled(account)) return false;
    return filter.limit === 'limited' ? isRateLimited(account) : !isRateLimited(account);
  }

  /** 当前筛选条件下的可见账号（四个维度同时生效） */
  function visibleAccounts(all, filter) {
    return (all || []).filter(account => matchProvider(account, filter)
      && matchEdition(account, filter)
      && matchEnabled(account, filter)
      && matchLimit(account, filter));
  }

  /**
   * 分段计数：某分段显示的数字 = 「其余维度保持当前选择、本维度取该值」的账号数。
   * 返回以 HTML 上 data-count 的键为索引的对象（provider 维度是 `p-<id>`）。
   */
  function filterCounts(all, filter, summaries) {
    const list = all || [];
    // 排除掉某个维度后的口径：except='edition' 表示「不管版本筛选」的账号集合
    const scope = except => list.filter(account => (except === 'provider' || matchProvider(account, filter))
      && (except === 'edition' || matchEdition(account, filter))
      && (except === 'enabled' || matchEnabled(account, filter))
      && (except === 'limit' || matchLimit(account, filter)));

    const forEdition = scope('edition');
    const forEnabled = scope('enabled');
    const forLimit = scope('limit');
    const forProvider = scope('provider');
    const counts = {
      editionAll: forEdition.length,
      cn: forEdition.filter(a => accountEdition(a) === 'cn').length,
      intl: forEdition.filter(a => accountEdition(a) === 'intl').length,
      enabledAll: forEnabled.length,
      enabled: forEnabled.filter(isEnabled).length,
      disabled: forEnabled.filter(a => !isEnabled(a)).length,
      limitAll: forLimit.length,
      normal: forLimit.filter(a => isEnabled(a) && !isRateLimited(a)).length,
      limited: forLimit.filter(a => isEnabled(a) && isRateLimited(a)).length,
      providerAll: forProvider.length,
    };
    // 摘要里每一家都要有键（没有账号的家显示 0 并置灰），否则它的徽标会停在旧数字上
    (summaries || []).forEach(item => { counts[`p-${item.id}`] = 0; });
    forProvider.forEach(account => {
      const key = `p-${providerOf(account)}`;
      counts[key] = (counts[key] || 0) + 1;
    });
    return counts;
  }

  /**
   * 账号**当前生效**的限流记录：`[{model, status, code, resetAt, message}]`，
   * 按恢复时间升序（最早恢复的排最前）。过期记录视为不存在（冷却自然结束）。
   * 「限流」列与展开的明细面板共用这一份口径 —— 列上的数字与点开看到的条数不会对不上。
   */
  function activeLimits(account) {
    const limits = account?.rateLimits;
    if (!limits || typeof limits !== 'object') return [];
    const now = Date.now();
    return Object.entries(limits)
      .map(([model, info]) => ({ model, ...(info && typeof info === 'object' ? info : {}) }))
      .filter(item => Number(item.resetAt) > now)
      .sort((a, b) => Number(a.resetAt) - Number(b.resetAt));
  }

  /**
   * 转发顺序位置表：accountId → `{ position, total }`（1 起）。
   *
   * **全局一条队列**：四家账号按优先级混排，序号就是整张表的行序，
   * 与后端选路（全局优先级）、↑/↓ 的边界同源 —— 否则会出现
   * 「界面上不是第一位、但下移按钮已经点不动」这种对不上的情况。
   */
  function positionMap(all) {
    const ordered = (all || []).slice().sort(byPriorityOrder);
    const map = new Map();
    ordered.forEach((account, index) => map.set(account.id, { position: index + 1, total: ordered.length }));
    return map;
  }

  /**
   * 全局队列的选路结果：优先级升序里第一个「启用 + 对该模型未限流 + 有可用凭证」的账号。
   *
   * 与后端 pick_account_by_priority 一致（不含 429 重试的排除列表；转发调用方还有
   * 「挑出候选后取不到会话就继续往后找」的循环，这里同样把凭证算进判据，少了它界面
   * 会把一个转发时必然被跳过的账号标成队首）。
   *
   * 注意它**不含**「这个模型由哪几家提供」这道收窄（那要读模型目录、且聚合视图是
   * 跨家去重过的，前端还原不出来），所以只当兜底用：正式的首选来自后端
   * `routedAccountId`（见 routedId）。`model` 留空 = 不限模型。
   */
  function pickForModel(list, model = '') {
    const candidates = (list || [])
      .filter(account => usableForModel(account, model) && hasCredentials(account))
      .sort(byPriorityOrder);
    return candidates[0] || null;
  }

  /**
   * 队列第一位（**不看限额**）：优先级升序里第一个「启用 + 有可用凭证」的账号。
   *
   * 与后端 `pick_current`（快照的 `currentAccountId`）逐条同口径 —— 不判 rateLimits
   * 是刻意的：它回答的是「队列排最前的是谁」，而不是「这次请求会走谁」。
   * 两者不同（队首正被限流）才会用到这个函数，见 routedId 与操作列的「队首」块。
   *
   * 不能拿 `pickForModel(list, '')` 顶替：那个函数在 model 为空时判的是「任一模型
   * 限流中」（见 isRateLimited 的无模型分支），会把队首跳过，于是「队首」块永远
   * 不显示、还会给真正的队首错误地渲染出「设为首选」按钮。
   */
  function pickQueueHead(list) {
    const candidates = (list || [])
      .filter(account => isEnabled(account) && hasCredentials(account))
      .sort(byPriorityOrder);
    return candidates[0] || null;
  }

  /**
   * 队首账号 id（★ / 「首选」标的就是它）。
   *
   * 两个候选按优先级取：
   *   1. `routedAccountId` —— 后端按**最近一次请求的模型**派生的「下一个请求会先用
   *      谁」，判据与转发层同一套（候选 = 提供该模型的那些家的账号，再取「启用 +
   *      有凭证 + 对该模型未限流」里优先级最小的）。这是**正确**的那个答案。
   *   2. `currentAccountId` —— 不限模型的队首，旧版后端给不出前者时的回落。
   *
   * 为什么不能只看 `currentAccountId`：它不看按模型记的限额，队首正在限流时会把 ★
   * 标在一个本次请求必然被跳过的账号上 —— 用户看到的就是「界面标着首选，请求却走
   * 了别人」。
   *
   * 候选指向的账号可能已被删（两次快照之间），所以还得校验它在不在列表里；
   * 都不在时按同一判据在前端退化派生，避免整列 ★ 凭空消失。
   */
  function routedId(all, routedAccountId = null, currentAccountId = null) {
    const exists = id => id && (all || []).some(account => account.id === id);
    if (exists(routedAccountId)) return routedAccountId;
    if (exists(currentAccountId)) return currentAccountId;
    return pickQueueHead(all)?.id || null;
  }

  window.wbAccountsGroups = {
    // provider 维度
    DEFAULT_PROVIDER_ID,
    RACCOON_PROVIDER_ID,
    providerOf,
    providerFeatures,
    providerSummaries,
    identifierOf,
    tokenExpiryOf,
    supportsUsage,
    isDesktopAccount,
    // 基础判定
    byPriorityOrder,
    typeLabel,
    isEnabled,
    isRateLimited,
    activeLimits,
    usableForModel,
    hasCredentials,
    accountEdition,
    supportsCheckin,
    checkinableAccounts,
    // 筛选与队列
    matchProvider,
    matchEdition,
    matchEnabled,
    matchLimit,
    visibleAccounts,
    filterCounts,
    positionMap,
    pickForModel,
    pickQueueHead,
    routedId,
  };
})();
