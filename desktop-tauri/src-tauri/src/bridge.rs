//! 前端桥接脚本。
//!
//! 界面代码（desktop-tauri/ui/）依赖 `window.workbuddyDesktop` 这个接口。
//! 这里在页面脚本执行前注入同名对象，把每个方法映射到 Tauri 的 invoke。
//!
//! 保持接口名与签名稳定是有意为之：界面代码里不提 Tauri，将来换壳也不用动界面。
//! 路径拼接与入参整形集中在本文件，便于与后端路由对照排查。
//!
//! 注：invoke 在调用时才去取（而不是定义时捕获），这样不依赖注入时序 ——
//! 初始化脚本与 Tauri 内核脚本的先后顺序变化都不会让桥接失效。
//!
//! ── 平台标识为什么由壳注入而不是前端自己嗅探 ──────────────────
//! 界面里有若干「这个功能在 macOS 上不可用」的裁剪（例如导入桌面端登录态：
//! 它读的是各家 Windows 客户端的登录态文件与 DPAPI 密文）。用 UA 嗅探在
//! WebView 里并不可靠（各平台 UA 形态会变，且 UA 表达的是「像什么浏览器」，
//! 不是「哪个编译目标」），而壳知道确切答案 —— `cfg!(target_os)` 是编译期的。
//! 因此这里注入 `platform`，界面按它裁剪，判断口径只有一处。

/// 本进程的编译目标平台（`"macos"` / `"windows"` / `"linux"`）。
///
/// 用 `cfg!` 而不是运行期探测：界面要裁剪的那些差异是**编译期**就定死的
/// （哪些平台有 DPAPI、哪些平台能读 `%APPDATA%`），不是运行环境决定的。
fn target_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// 桥接脚本全文：模板里的 `__AGENT2API_PLATFORM__` 换成 [`target_platform`]。
///
/// 为什么用占位替换而不是 `format!`：脚本里有大量 `{}`（对象字面量、模板串），
/// 走 `format!` 得把它们全转义成 `{{}}`，改一次脚本就要小心翼翼地对一遍括号。
/// 一个不会被误伤的长占位名更稳。
pub fn bridge_js() -> String {
    BRIDGE_JS.replace("__AGENT2API_PLATFORM__", target_platform())
}

const BRIDGE_JS: &str = r#"
(() => {
  const invokeRaw = (command, args) => {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      return Promise.reject(new Error('桌面运行时不可用（Tauri 未初始化）'));
    }
    return internals.invoke(command, args);
  };

  /**
   * 把命令的失败值统一成 Error。
   *
   * Tauri 在命令返回 `Err` 时，rejection 携带的是**序列化后的错误值本身**，
   * 不是 Error 对象 —— 本项目的命令错误类型是 String，于是界面拿到的是一个
   * 裸字符串，`error.message` 为 undefined，所有 `操作失败：${error.message}`
   * 都显示成「操作失败：undefined」（真实发生过：小浣熊刷新 token 被上游 401
   * 拒绝时，界面只显示 undefined，用户看不到「authorization_verify_error」）。
   *
   * 界面有 50 多处 `error.message`，逐个改成兼容写法既啰嗦又容易漏；统一在
   * 桥接层包一次，界面代码与 Electron 时代完全一致（那时 preload 抛的也是 Error）。
   * 非字符串的失败值（将来若改成结构化错误对象）原样透出，交给界面自己取字段。
   */
  const asError = failure => {
    if (failure instanceof Error) return failure;
    if (typeof failure === 'string') return new Error(failure);
    // 结构化错误：尽量取一个可读消息，取不到就整体 JSON 化（总比 undefined 强）
    if (failure && typeof failure === 'object') {
      const message = failure.message ?? failure.error ?? failure.msg;
      if (typeof message === 'string' && message) return new Error(message);
      try {
        return new Error(JSON.stringify(failure));
      } catch {
        return new Error('操作失败（错误信息无法序列化）');
      }
    }
    return new Error(String(failure ?? '操作失败'));
  };

  /**
   * 调用命令并把失败值归一成 Error —— **所有命令都走这里**（定义成 invoke
   * 而不是让各调用点自己包，避免将来新增方法时漏掉）。
   */
  const invoke = (command, args) =>
    invokeRaw(command, args).catch(failure => {
      throw asError(failure);
    });

  /** 统一调用管理 API：返回后端 data，失败时抛错（界面按原有 try/catch 处理） */
  const call = (method, path, body) =>
    invoke('api_request', { request: { method, path, body: body === undefined ? null : body } });

  /** 把渲染层的筛选条件转成查询串 */
  const toQuery = query => {
    if (!query) return '';
    if (typeof query === 'string') return query.startsWith('?') || query === '' ? query : '?' + query;
    // URLSearchParams 本身就是可用的查询串，直接取出来 —— 落到下面的
    // Object.entries 会得到空数组（它的数据不是自有可枚举属性），静默变成
    // 「没有参数」：调用方以为带了筛选条件、实际发的是全量请求
    //（曾经的清空事故：筛选清空变成了清空全部）。这里兜住，别让它再发生。
    if (query instanceof URLSearchParams) {
      const text = query.toString();
      return text ? '?' + text : '';
    }
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query)) {
      if (value === undefined || value === null || value === '') continue;
      params.append(key, String(value));
    }
    const text = params.toString();
    return text ? '?' + text : '';
  };

  /**
   * 订阅后端事件。Tauri 的 listen 是异步的，这里同步返回取消函数，
   * 与原 preload 的用法（返回值即 unsubscribe）保持一致。
   *
   * target 缺省为 Any —— 后端 `emit` 的应用事件（login:state 等）对任何
   * 监听 target 都投递。但窗口**内置**事件（tauri://resize 等）不一样：
   * 壳侧按窗口 label 定向投递（Tauri 的 emit_to_window 只放行 Window /
   * WebviewWindow 两种监听 target，对 Any 一律不投），订阅它们必须传
   * 当前窗口的 target —— 见 onWindowResize。
   */
  const on = (event, callback, target) => {
    let unlisten = null;
    let disposed = false;
    invoke('plugin:event|listen', {
      event,
      target: target || { kind: 'Any' },
      handler: window.__TAURI_INTERNALS__.transformCallback(evt => callback(evt && evt.payload)),
    })
      .then(fn => {
        if (disposed && typeof fn === 'function') fn();
        else unlisten = fn;
      })
      .catch(error => console.warn('订阅事件失败:', event, error));
    return () => {
      disposed = true;
      if (typeof unlisten === 'function') unlisten();
    };
  };

  window.workbuddyDesktop = {
    // ── 平台 ──
    // 壳的编译目标平台（'macos' / 'windows' / 'linux'）。界面用它裁剪
    // 各平台不可用的功能（见文件头「平台标识」一节）。
    platform: '__AGENT2API_PLATFORM__',

    // ── 会话 ──
    getState: () => call('GET', '/api/session'),
    startLogin: (edition, mode, provider, socialRestore) =>
      invoke('start_login', {
        edition: edition || 'cn',
        mode: mode || 'embedded',
        // provider 缺省留给 Rust 侧兜底成 workbuddy：老界面（或将来别处的调用点）
        // 不传它时必须走原来那条链，这里不做猜测式整形
        provider: provider || null,
        // 第三方登录入口恢复（Google / GitHub / X）。缺省 false 与界面默认不勾选一致：
        // 不传就照官方登录页的形态走，不额外放宽域名白名单
        socialRestore: socialRestore === true,
      }),
    getLoginState: () => invoke('login_state'),
    cancelLogin: () => invoke('cancel_login'),
    // ── AutoClaw 国际版 OAuth（**授权地址由界面先拿好**）──────────
    // 与 startLogin 的区别：那条是「壳去问后端要地址」，这条是「界面已经
    // 拿到地址了，壳只负责开窗口并等待」。原因是这条链前面有一次**必须在
    // 浏览器里跑完**的强制风控验证码（阿里云 SDK），而主窗口就是那个环境
    // —— 顺序因此被倒过来（见 commands.rs 的 start_autoclaw_oauth_login）。
    //
    // state / authUrl 来自 `POST /api/session/login/oauth/start`，
    // 界面原样把它们交给这里；两个都不能为空（壳侧会拒绝）。
    startAutoclawOauthLogin: (state, authUrl, mode) =>
      invoke('start_autoclaw_oauth_login', {
        state: String(state || ''),
        authUrl: String(authUrl || ''),
        // 缺省内嵌窗口：与另外几家的默认一致（系统浏览器是可选方式）
        mode: mode === 'external' ? 'external' : 'embedded',
      }),
    // AutoClaw OAuth 的两条前置查询（都直接走网关，不经壳）：
    //   captchaConfig 取风控配置（前端据此初始化阿里云 SDK）；
    //   oauthStart    带验证码参数换授权地址。
    // 放在这里而不是让界面自己拼路径：桥接层是「界面 → 网关」的唯一入口，
    // 绕开它（直接 invoke('api_request')）会丢掉 asError 的错误归一化
    // —— 那时 rejection 携带的是裸字符串，`error.message` 是 undefined，
    // 界面会显示成「登录失败：undefined」（见 sendSmsCode 的同款说明）。
    getAutoclawOauthCaptchaConfig: provider =>
      call('POST', '/api/session/login/oauth/captcha-config', {
        ...(provider ? { provider: String(provider) } : {}),
      }),
    startAutoclawOauth: (provider, vendor, captchaVerifyParam) =>
      call('POST', '/api/session/login/oauth/start', {
        ...(provider ? { provider: String(provider) } : {}),
        vendor: String(vendor || ''),
        captchaVerifyParam: String(captchaVerifyParam || ''),
      }),
    // ── ZCode「周末套餐」领取（三个薄封装，直接打账号子路径接口）──────
    // 与上面 AutoClaw 那三个方法同一形态，**两处必须成对存在**：本文件是
    // 桌面壳的桥接，`server/src/web_shim.rs` 是 headless 面板的桥接 ——
    // 只加一边时，另一形态下的界面会报「当前环境不支持领取（桥接方法缺失）」
    // （zcode-claim.js 的 `api?.zcodeClaimPreview` 判定）。
    //
    // 契约（详见 `api/zcode_claim.rs` 的模块头）：
    //   · captchaConfig 拿阿里云风控配置（前端用它初始化滑块 SDK）；
    //     返回 `{enabled:false}` 表示上游此刻不要验证码 —— 前端**不该**弹滑块；
    //   · preview 只读探测，返回 `{plans:[...], deployed}`；
    //     `deployed:false` = 活动接口尚未部署（开抢前的正常状态，不是错误）；
    //   · claim 真正领取；**业务失败也走 200**，由 `ok:false` + `failure`
    //     表达（前端据此选提示文案）。
    zcodeClaimCaptchaConfig: accountId =>
      call('POST', `/api/accounts/${encodeURIComponent(String(accountId || ''))}/zcode-claim/captcha-config`),
    zcodeClaimPreview: accountId =>
      call('POST', `/api/accounts/${encodeURIComponent(String(accountId || ''))}/zcode-claim/preview`),
    zcodeClaim: (accountId, planId, captchaVerifyParam, captchaRegion) =>
      call('POST', `/api/accounts/${encodeURIComponent(String(accountId || ''))}/zcode-claim`, {
        planId: planId ? String(planId) : '',
        captchaVerifyParam: String(captchaVerifyParam || ''),
        captchaRegion: captchaRegion ? String(captchaRegion) : '',
      }),
    onLoginState: callback => on('login:state', callback),
    refreshSession: async () => {
      await call('POST', '/api/session/refresh', {});
      return call('GET', '/api/session');
    },
    logout: async () => {
      await call('POST', '/api/session/logout', {});
      return call('GET', '/api/session');
    },

    // ── 配置 ──
    getConfig: () => call('GET', '/api/config'),
    saveConfig: payload => call('POST', '/api/config', payload),

    // ── 模型清单 ──
    // 手动刷新（「获取模型」弹窗）：只刷支持远程目录的家、强制绕过缓存，
    // 返回 `{results, refreshed, skipped, failed, models}` —— `models` 是
    // **session 形状**的聚合清单（同 /api/session 的那份），给「就地重绘
    // /api/session 快照」的调用方用；模型管理页那份 manage 视图（左栏计数、
    // 行的「来源」列）不在其中 —— 它由调用方在刷新落地后自己重拉
    // （见 models-fetch-modal.js 的 onRefreshed）。
    //
    // 可选入参两个键，都是新增的可选维度（老调用方不传 = 各家按默认选取 + 全部家）：
    //   · `{accounts: {providerId: accountId}}`：「获取模型」弹窗每行的
    //     「模型来源」下拉点名的账号（用谁去打该家的目录接口）；逐条结果里带
    //     `accountId` 供界面回读。
    //   · `{providers: [providerId, ...]}`：本次刷新的**范围白名单**，名单外的家
    //     不打网络也不进结果（弹窗按「模型管理页实有清单的家 ∪ 有启用账号的家」
    //     组装，见 models-fetch-modal.js 的 scopeProviders）。
    refreshModels: payload => call('POST', '/api/models/refresh', payload || {}),
    // 模型管理（启停 / 映射）：写接口都返回最新 {models, mappings, reasoningLevels}
    // 映射照抄 OmniProxy 语义：对外名自由命名（允许与上游 id 同名），同一对外名
    // 可在不同提供商各建一条（主备）；provider 为空 = 旧版全局语义
    //
    // 第 4 个参数是**思考等级绑定**（照抄 OmniProxy 的手动绑定列表，见
    // 模型管理页的下拉）：省略 = 不动已有等级（旧调用点的行为），
    // '' / null = 清成「不覆盖」，其它字符串 = 设成该等级。
    // 第 5 个参数是**映射开关**（chip 上的小滑块）：省略 = 不动，bool = 显式开 / 关。
    // 后端按「请求体里有没有这个键」区分三态，所以两个可选参数都展开成条件键 ——
    // 直接塞 `reasoning: reasoning || ''` 会把「不改」也变成「清空」，
    // 那是一次静默的数据丢失。
    getModelManage: () => call('GET', '/api/models/manage'),
    setModelState: payload => call('POST', '/api/models/state', payload),
    addModelMapping: (alias, target, provider, reasoning, enabled) => call('POST', '/api/models/mappings', {
      alias, target, provider,
      ...(reasoning === undefined ? {} : { reasoning }),
      ...(enabled === undefined ? {} : { enabled }),
    }),
    removeModelMapping: (alias, target, provider) => call('POST', '/api/models/mappings/remove', { alias, target, provider }),
    // 自定义模型（手动登记上游目录里没有的模型）。「移除」而不是「隐藏」——
    // 见后端 `api::model_manage::remove_custom` 的说明。
    addCustomModel: (provider, id) => call('POST', '/api/models/custom', { provider, id }),
    removeCustomModel: (provider, id) => call('POST', '/api/models/custom/remove', { provider, id }),

    // ── 网关 API Key（多把）──
    getKeys: () => call('GET', '/api/keys'),
    createKey: payload => call('POST', '/api/keys', payload || {}),
    updateKey: (id, patch) => call('PATCH', '/api/keys/' + encodeURIComponent(id), patch),
    deleteKey: id => call('DELETE', '/api/keys/' + encodeURIComponent(id)),

    // ── 多账号 ──
    // switchAccount 仅把账号移到全局队列第一位，不改变启用状态
    switchAccount: id => call('POST', '/api/accounts/current', { id }),
    refreshAccountToken: id => call('POST', '/api/accounts/refresh', id ? { id } : {}),
    removeAccount: id => call('DELETE', '/api/accounts/' + encodeURIComponent(id)),
    updateAccount: (id, patch) => call('PATCH', '/api/accounts/' + encodeURIComponent(id), patch),
    moveAccount: (id, direction) =>
      call('POST', '/api/accounts/' + encodeURIComponent(id) + '/move', {
        direction: direction === 'down' ? 'down' : 'up',
      }),
    // 清除限流标记（账号页「限流明细」面板）：model 缺省 = 清掉该账号全部模型的标记
    clearRateLimits: (id, model) =>
      call('POST', '/api/accounts/' + encodeURIComponent(id) + '/rate-limits/clear',
        model ? { model } : {}),
    batchAccounts: payload =>
      call('POST', '/api/accounts/batch', {
        action: String((payload && payload.action) || ''),
        ids: Array.isArray(payload && payload.ids) ? payload.ids : [],
        ...(payload && Object.prototype.hasOwnProperty.call(payload, 'proxy')
          ? { proxy: payload.proxy }
          : {}),
      }),

    // ── 出网代理 ──
    getProxies: () => call('GET', '/api/proxies'),
    testProxy: payload => {
      const body = {};
      if (payload && typeof payload === 'object' && !Array.isArray(payload)) {
        if (typeof payload.id === 'string' && payload.id) body.id = payload.id;
        // proxy 允许显式 null（测直连），因此按字段存在性判断
        else if (Object.prototype.hasOwnProperty.call(payload, 'proxy')) body.proxy = payload.proxy;
      }
      return call('POST', '/api/proxies/test', body);
    },

    // ── 积分 / 签到 ──
    getUsage: () => call('GET', '/api/usage'),
    getCheckinStatus: () => call('GET', '/api/checkin/status'),
    claimCheckin: () => call('POST', '/api/checkin', {}),
    // `id` 给定 = 只查那一个账号（账号页每行的「积分」按钮走这条）。
    // 它**不看启用状态** —— 禁用只是「不参与转发」，与余额能否查无关；
    // 按启用挡掉会让单查退化成一句「未返回余额数据」。见 core::usage_query。
    getAllBalances: id =>
      call('GET', '/api/accounts/usage' + (id ? '?id=' + encodeURIComponent(id) : '')),
    // 最近一次「定时查询积分」的结果快照（形状同上，多一个 at 时间戳）。
    // 账号页轮询它，于是用户不点按钮也能看到最新余额。
    getBalancesSnapshot: () => call('GET', '/api/accounts/usage/snapshot'),
    // 逐账号活跃连接数 `{counts: {id: n}}`（只含非零项，缺失即 0）。
    // 账号页「连接数」列 2 秒轮询它 —— 后端是进程内计数，这条请求很轻。
    getAccountConnections: () => call('GET', '/api/accounts/connections'),
    checkinAllAccounts: id => call('POST', '/api/accounts/checkin', id ? { id } : {}),

    // ── 手机验证码登录（AutoClaw **国内版**专用）──
    // 与网页登录那条链（开窗口、等回调）不同：上游没有授权页，就是「发码 →
    // 用码换 token」两次同步调用，所以走管理 API 而不是 start_login。
    //
    // 必须走 call 而不是让界面自己 invoke('api_request')：call 会经 invoke/asError
    // 把壳侧 `Err(String)` 归一成 Error —— 绕过它时 rejection 携带的是**裸字符串**，
    // 界面的 `error.message` 会拿到 undefined，显示成「登录失败：undefined」。
    // `sendSmsCode` 返回的 deviceId 要原样回传给 verify（上游把验证码绑在发码时
    // 那台设备上，见 api::session::login_sms_send 的说明）。
    // ── `sendSmsCode` 的入参形态（两种都收）──────────────────────
    // 老界面传**裸手机号字符串**（历史契约），新界面传 `{phone, provider}`。
    // 两种都要收：界面与壳是两个独立产物，升级不同步时旧界面不能直接坏掉。
    //
    // `provider` 是**哪一个地区**（`autoclaw` 国内版 / `autoclaw-intl` 国际版）：
    // 两个地区的接口是同一套路径、两个站点，因此原样带上去 —— 国际版会在后端
    // 被明确拒绝（它的手机验证码入口已移除，见 api::session::login_sms_send），
    // 不带则按历史口径落国内版。省略即国内版（与老界面的行为一致）。
    sendSmsCode: input => {
      const isObject = input && typeof input === 'object';
      const phone = String((isObject ? input.phone : input) || '');
      const provider = isObject && input.provider ? String(input.provider) : '';
      return call('POST', '/api/session/login/sms/send', {
        phone,
        ...(provider ? { provider } : {}),
      });
    },
    verifySmsLogin: payload =>
      call('POST', '/api/session/login/sms/verify', {
        phone: String((payload && payload.phone) || ''),
        code: String((payload && payload.code) || ''),
        // deviceId / name 可选：空串会被后端当成一个真值带上去，
        // 因此按「有值才带」整形（与其它命令的省略语义一致）
        ...((payload && payload.deviceId) ? { deviceId: String(payload.deviceId) } : {}),
        ...((payload && payload.name) ? { name: String(payload.name) } : {}),
        ...((payload && payload.provider) ? { provider: String(payload.provider) } : {}),
      }),

    // ── 定时签到 ──
    getAutoCheckin: () => call('GET', '/api/auto-checkin'),
    saveAutoCheckin: patch => call('POST', '/api/auto-checkin', patch),
    runAutoCheckinNow: () => call('POST', '/api/auto-checkin/run', {}),

    // ── 间隔型定时任务（凭证自动维护 / 定时查询积分 / 模型刷新 / 两个前端自动刷新）──
    // 改一条任务用 PATCH（后端同时受理 POST 作别名：CORS 允许方法里没有 PATCH，
    // 浏览器直连时预检会拦下它；走本桥的调用两种都能用，这里按规范用 PATCH）。
    getScheduledTasks: () => call('GET', '/api/scheduled-tasks'),
    saveScheduledTask: (id, patch) =>
      call('PATCH', '/api/scheduled-tasks/' + encodeURIComponent(String(id || '')), patch),
    runScheduledTask: id =>
      call('POST', '/api/scheduled-tasks/' + encodeURIComponent(String(id || '')) + '/run', {}),

    // ── 软件更新 ──
    // checkUpdate 走壳命令：当前版本号只有壳知道（后端是独立进程），
    // 由壳把版本带上去交给后端比较
    checkUpdate: () => invoke('check_update'),
    // 最近一次「检查更新」的结果（定时任务写入；app.js 轮询它亮侧栏徽标）
    getUpdateStatus: () => call('GET', '/api/update/status'),
    downloadUpdate: payload => invoke('download_update', {
      url: String((payload && payload.url) || ''),
      name: (payload && payload.name) ? String(payload.name) : null,
    }),
    updateProgress: () => invoke('update_progress'),
    cancelUpdate: () => invoke('cancel_update'),
    runInstaller: (path, restart) =>
      invoke('run_installer', { path: String(path || ''), restart: restart !== false }),
    openReleasePage: url => invoke('open_release_page', { url: String(url || '') }),

    // ── 出站指纹脱敏开关 ──
    // 与 getDebug / saveDebug 同形：GET 读、PUT 写，响应体就是新状态。
    getSanitize: () => call('GET', '/api/sanitize'),
    saveSanitize: enabled =>
      call('PUT', '/api/sanitize', { sanitizeBlacklistFingerprints: enabled === true }),

    // ── 系统提示词与内容拦截降级 ──
    // 与 getRetry / saveRetry 同形：GET 读、PUT 写（允许部分字段），响应体是
    // 生效后的全量状态（含降级是否生效）。`clearDegrade` 是同一端点上的一个
    // 动作位（参考项目只能等到零点，本项目允许用户当场解除）。
    getPrompt: () => call('GET', '/api/prompt'),
    savePrompt: payload =>
      call('PUT', '/api/prompt', {
        promptMode: payload && payload.promptMode != null ? String(payload.promptMode) : null,
        promptFile: payload && payload.promptFile != null ? String(payload.promptFile) : null,
        clearDegrade: !!(payload && payload.clearDegrade),
      }),

    // ── 运行日志 ──
    getLogs: query => call('GET', '/api/logs' + toQuery(query)),
    getLogStats: () => call('GET', '/api/logs/stats'),
    // 清空支持筛选条件（可选对象，与 getLogs 同一套键）：带条件 = 只删命中的，
    // 不传 = 全部清空（后端两种语义不同，见 logs_api::clear_logs）
    clearLogs: query => call('DELETE', '/api/logs' + toQuery(query)),
    exportLogs: () => invoke('export_logs'),

    // ── 请求统计报表 / 数据保留 ──
    // range 只认 today / 7 / 30 / month / all，非法值由后端返回 400
    // （不在这里静默改口径：界面选了什么就该拿到什么，报错比默默换区间好排查）
    getStatsSummary: range => call('GET', '/api/stats/summary?range=' + encodeURIComponent(range)),
    // 筛选条件是可选对象，复用上面 toQuery 的「空值跳过」语义：
    // 清空的输入框不该变成 `?model=` 这种永不命中的条件
    getStatsRequests: query => call('GET', '/api/stats/requests' + toQuery(query)),
    // 筛选下拉的候选清单（出现过的模型 / 提供商）。不带参数、不随筛选变化 ——
    // 它只在进页面时拉一次（理由见 api::stats_api::stats_request_filters）。
    // 读不到时前端只保留「全部」选项，不弹错（筛选器退化成不可用，列表照常）
    getStatsRequestFilters: () => call('GET', '/api/stats/requests/filters'),
    // 清空同样支持筛选条件（带条件 = 只删命中的明细，不传 = 全部清空）
    clearStatsRequests: query => call('DELETE', '/api/stats/requests' + toQuery(query)),
    // 按 id 取单条请求的原始正文 `{id, requestBody, responseBody, truncated}`
    // （详情弹窗「预览对话」的数据源；列表接口不回正文，行保持轻）。
    // 找不到给 404，前端据此提示「没有保存原始报文」。
    getStatsRequestRaw: id => call('GET', '/api/stats/requests/raw' + toQuery({ id })),
    // 手动终止一条**在途**请求（详情弹窗的「终止请求」按钮）：
    // 受理 `{success:true,terminated:true}`；不在进行中给 404（已结束 /
    // 上次启动遗留的进行中行），前端据此提示刷新列表
    terminateStatsRequest: id => call('POST', '/api/stats/requests/terminate' + toQuery({ id })),
    // 清理弹窗的预览统计 `{all, raw, dbBytes, vacuumRunning, lastVacuum}`：
    // 与 DELETE 共用同一份筛选解析（后端 filter_from_params），预览说删 N 条、
    // 确认删掉的就是 N 条 —— 预览与执行必须同源，否则就是新的「清空事故」
    getStatsClearPreview: query => call('GET', '/api/stats/requests/clear-preview' + toQuery(query)),
    // 后台压缩数据库（checkpoint + VACUUM）：已受理 {started:true}；重复触发 409，
    // 进度靠 getStatsClearPreview 的 vacuumRunning / lastVacuum 轮询
    compactStatsDb: () => call('POST', '/api/stats/requests/compact'),
    getRetention: () => call('GET', '/api/retention'),
    // PUT 是后端已定契约（允许部分字段 + 立即清理）。
    // gateway.rs 的 request_builder 支持 GET/POST/PUT/PATCH/DELETE，
    // 且对 PUT 无 body 时会补一个空对象，所以这里直接透传即可。
    saveRetention: patch => call('PUT', '/api/retention', patch),

    // ── 数据存储概况（设置页「保存位置」）──
    // 只读：数据统一在配置目录的 agent2api.db 里，不再支持换目录
    // （改造前的 relocateStorage / storageProgress 两条写命令已随单库语义删除，
    //  见 server/api/storage_api.rs 的模块头）。
    getStorage: () => call('GET', '/api/storage'),

    // ── 数据结构升级（旧 JSON/JSONL → 统一 SQLite 库）──
    // 启动时只探测、不自动迁移；界面拿到 pending 后弹窗，用户点「升级」才导入。
    // 旧数据不会被删除（只改名为 *.migrated），详见 server/api/upgrade_api.rs。
    getUpgrade: () => call('GET', '/api/upgrade'),
    runUpgrade: () => call('POST', '/api/upgrade/run', {}),

    // ── 请求重试（设置页「通用 → 请求重试」）──
    // 转发层退避的次数 / 间隔，存后端 config.json（/api/retry）。
    // 契约同 saveRetention：PUT 允许部分字段，返回生效后的全量值。
    getRetry: () => call('GET', '/api/retry'),
    saveRetry: patch => call('PUT', '/api/retry', patch),

    // ── 上游请求超时（设置页「通用 → 请求超时」）──
    // 四项秒数（连接 / 等待响应 / 流式空闲 / 非流式响应体），存配置（/api/timeouts）。
    // 契约同 saveRetry：PUT 允许部分字段，返回生效后的全量值。
    getTimeouts: () => call('GET', '/api/timeouts'),
    saveTimeouts: patch => call('PUT', '/api/timeouts', patch),

    // ── 调试模式（设置页「通用 → 调试模式」）──
    // 开关存配置（debugMode）：开启后转发层把上游原始报文（凭据类头已脱敏）
    // 落到统一库的 debug_traffic 表，请求日志页的「详情」列据此展示。
    getDebug: () => call('GET', '/api/debug'),
    saveDebug: enabled => call('PUT', '/api/debug', { debugMode: enabled }),
    // 按 id 取一条请求的原始报文（列表接口不返回报文，见后端 debug_api 的模块头）。
    // 找不到时后端给 404，前端据此显示「该请求没有保存原始报文」。
    getDebugTraffic: id => call('GET', '/api/debug/traffic?id=' + encodeURIComponent(id)),

    // ── 事件 ──
    onStateChanged: callback => on('accounts:state-changed', callback),
    onAutoMaintained: callback => on('accounts:auto-maintained', callback),
    // 启动失败（含端口冲突）。payload 是 StartupFailure：
    // { message, conflict, canEndOccupant }。事件只是「去查一次」的提醒，
    // 状态以 getBackendStatus 为准 —— 它可能在界面订阅之前就发出去了。
    onBackendError: callback => on('backend:error', callback),

    // ── 本壳特有：后端就绪状态与端口冲突处置（界面可选使用） ──
    // 后端返回 { ready, port, portFromEnv, failure }：failure 为 null 表示
    // 启动正常，非 null 时带 { message, conflict, canEndOccupant }，
    // 界面据此决定给不给「结束占用进程 / 更换端口」两个出口。
    getBackendStatus: () => invoke('backend_status'),
    // 查 / 结束占用网关端口的进程（只动查到的那个 PID，不做无差别清理）
    getPortOccupant: () => invoke('port_occupant'),
    endPortOccupant: () => invoke('end_port_occupant'),
    // 换端口：checkPort 只探测不写盘（供输入框即时校验），
    // changePort 会写设置文件并重启后端，changePort 内部已包含一次探测
    checkPort: port => invoke('check_port', { port: Number(port) }),
    changePort: port => invoke('change_port', { port: Number(port) }),
    restartApp: () => invoke('restart_app'),

    // ── 本壳特有：窗口主题 ──
    // 三态语义：'dark' / 'light' 把窗口主题钉死，null 交回系统跟随（对应 Rust 侧的 None）。
    // 跟随系统时必须真的传 null，不能整形回只有两态：窗口被手动主题钉住时，
    // WebView2 的 prefers-color-scheme 会跟着窗口主题走而不是系统主题，
    // 渲染层再读 matchMedia 就拿到被污染的值，切「跟随系统」会卡在手动主题上。
    // 其它非法值（undefined 等）没有明确语义，按跟随系统兜底，同样走 null。
    setWindowTheme: theme => invoke('set_window_theme', {
      theme: theme === 'dark' || theme === 'light' ? theme : null,
    }),

    // ── 本壳特有：自定义标题栏的窗口三键 ──
    // 主窗口去掉了系统装饰（lib.rs 建窗处 decorations(false)），最小化 /
    // 最大化 / 关闭改由界面标题栏承担（titlebar.js）。这四个方法是对壳命令
    // 的薄映射，与其它命令一样统一走 invoke（错误归一成 Error）。
    // 网页版 shim 里对应给出拒绝 / 空实现（浏览器没有应用窗口）——
    // 标题栏在网页端根本不渲染（titlebar.js 的 platform 守卫），这里只是兜底。
    windowMinimize: () => invoke('window_minimize'),
    // 切换最大化 / 还原。「当前是否最大化」的事实来源是 windowIsMaximized
    // 的查询结果：双击标题栏的切换走 data-tauri-drag-region 的原生行为，
    // 不经这里，界面靠 onWindowResize 重查来同步图标
    windowToggleMaximize: () => invoke('window_toggle_maximize'),
    // 关闭 = 发出 CloseRequested，与点系统关闭按钮同语义：「关闭到托盘」
    // 开启时被壳拦成隐藏（托盘继续转发），否则正常退出
    windowClose: () => invoke('window_close'),
    windowIsMaximized: () => invoke('window_is_maximized'),
    // 窗口尺寸变化（含最大化 / 还原 / 拖拽缩放）。Tauri 的内置窗口事件
    // `tauri://resize` 每次变化都会发出，界面订阅后重查 windowIsMaximized
    // 即可让标题栏图标保持同步（事件名收在桥里，界面不出现 Tauri 字样）。
    // 注意 target 不能用缺省的 Any：窗口内置事件按窗口 label 定向投递
    // （见 on 的注释），这里用 Tauri 注入的 metadata 拿当前窗口 label ——
    // 与 invoke 一样是调用时才取值，不依赖桥接脚本的注入时序
    onWindowResize: callback =>
      on('tauri://resize', callback, {
        kind: 'WebviewWindow',
        label: window.__TAURI_INTERNALS__.metadata.currentWebview.label,
      }),

    // ── 本壳特有：应用设置与账号导入导出 ──
    // 这四项不走 api_request：设置存在桌面端本地（与后端无关），
    // 导入导出需要调用系统文件对话框，只有壳进程能做。
    getAppSettings: () => invoke('get_app_settings'),
    // 参数名 patch 必须与 Rust 侧 save_app_settings 的形参名一致
    saveAppSettings: patch => invoke('save_app_settings', { patch }),
    exportAccounts: () => invoke('export_accounts'),
    importAccounts: () => invoke('import_accounts'),
  };
})();
"#;
