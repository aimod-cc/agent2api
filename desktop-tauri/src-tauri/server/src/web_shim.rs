//! 网页端桥接（headless 托管面板时注入的 `window.workbuddyDesktop` 实现）。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 界面代码（desktop-tauri/ui/）只依赖 `window.workbuddyDesktop` 这个接口，
//! 桌面壳由 `bridge.rs` 在页面脚本执行前注入同名对象（Tauri IPC → 壳 → 网关）。
//! headless 形态没有壳：本模块生成一份**纯 HTTP** 的同接口实现，由
//! `static_files` 注入进 index.html —— 界面代码零改动地跑在浏览器里，
//! 「界面不感知壳」的既有设计在这里兑现。
//!
//! ── 与桌面 bridge 的三条差异（其余方法一一对应）─────────────
//!   · 传输：`call` 直接 `fetch` 同源 `/api/*`，自动带 `x-api-key`；
//!     信封解包（`{success, data}` / 401）与桌面 `gateway::unwrap_envelope`
//!     逐条对齐，界面的 try/catch 语义不变。
//!   · 凭证：桌面壳自动带第一把 Key（进程内读）；网页端由**用户输入**
//!     （401 时弹出输入层，存 localStorage 后重试一次）。
//!   · 事件：Tauri 的 push 事件不存在，改为**模拟** ——
//!     `accounts:state-changed` 在写类请求成功后拉一次 /api/session 重放
//!     （300ms 防抖，合并连续写）；`login:state` 由本文件的登录状态机
//!     本地驱动；`backend:error` / `accounts:auto-maintained` 没有对应物，
//!     订阅返回空操作（桌面端它们也只是「去查一次」的提醒）。
//!
//! ── 登录链路在网页端的形态 ──────────────────────────────────
//! 桌面的 startLogin 是「壳开窗口 + 壳捕获回调」；网页端改为
//! `POST /api/session/login/start` 拿 authUrl → 打开新标签 → 轮询
//! `GET /api/session/login/wait?state=` 直到 done（与桌面壳的等待语义一致，
//! 返回最终 session；取消/错误原样上抛）。适用面（对齐各上游回调机制）：
//!   · WorkBuddy / Qoder / Cline：设备授权轮询，网页端完全可用；
//!   · AutoClaw（OAuth）/ CatPaw：回调打**本机网关 loopback 端口** ——
//!     浏览器与网关同机（compose 本地映射）时可用，远程面板不可用；
//!   · 小浣熊：自定义协议回调（office-raccoon://），浏览器无法转交，
//!     网页端用「填写凭证」。
//!
//! ── 壳特有命令的降级 ────────────────────────────────────────
//! 窗口主题、托盘、改端口、软件更新安装、桌面设置、文件对话框导入导出
//! 这些只有壳能做的事按「能映射则映射，不能则明确拒绝」处理：
//! 拒绝给出可读文案（界面会 toast 出来），绝不静默假成功。

/// 注入进 index.html 的桥接脚本全文（在所有界面脚本之前执行）。
///
/// 用 `r#"…"#` 原始字符串：脚本里不出现 `"#` 序列（字符串一律单引号），
/// 不需要任何转义；占位符也没有 —— 平台标识直接写死 `'web'`。
pub fn shim_js() -> &'static str {
    r#"(function () {
  'use strict';

  // ── API Key 的存取 ────────────────────────────────────────
  var KEY_STORAGE = 'agent2api.webKey';
  function readStoredKey() {
    try { return localStorage.getItem(KEY_STORAGE) || ''; } catch (e) { return ''; }
  }
  var apiKey = readStoredKey();
  function storeKey(value) {
    apiKey = value;
    try {
      if (value) localStorage.setItem(KEY_STORAGE, value);
      else localStorage.removeItem(KEY_STORAGE);
    } catch (e) { /* 隐私模式等：留在内存即可 */ }
  }

  // ── 覆盖层（Key 输入 / 链接兜底）：原生 DOM，界面样式不依赖 ──
  function ensureOverlay(titleText, bodyHtml, confirmText) {
    return new Promise(function (resolve) {
      var old = document.getElementById('a2a-web-overlay');
      if (old) old.remove();
      var overlay = document.createElement('div');
      overlay.id = 'a2a-web-overlay';
      overlay.style.cssText = 'position:fixed;inset:0;z-index:2147483647;display:flex;'
        + 'align-items:center;justify-content:center;background:rgba(0,0,0,.55);';
      var card = document.createElement('div');
      card.style.cssText = 'background:#1e1f22;color:#e8e8e8;border-radius:10px;'
        + 'padding:22px 24px;width:min(420px,86vw);box-shadow:0 12px 40px rgba(0,0,0,.4);'
        + 'font:14px/1.6 system-ui,-apple-system,"Segoe UI",sans-serif;';
      var title = document.createElement('div');
      title.textContent = titleText;
      title.style.cssText = 'font-size:15px;font-weight:600;margin-bottom:10px;';
      var body = document.createElement('div');
      body.innerHTML = bodyHtml;
      card.appendChild(title);
      card.appendChild(body);
      var input = body.querySelector('input');
      var button = document.createElement('button');
      button.textContent = confirmText;
      button.style.cssText = 'margin-top:14px;width:100%;padding:8px 0;border:0;border-radius:6px;'
        + 'background:#4c7dff;color:#fff;font-size:14px;cursor:pointer;';
      card.appendChild(button);
      overlay.appendChild(card);
      document.body.appendChild(overlay);
      var finish = function (value) {
        overlay.remove();
        resolve(value);
      };
      button.addEventListener('click', function () { finish(input ? input.value : true); });
      if (input) {
        input.addEventListener('keydown', function (event) {
          if (event.key === 'Enter') finish(input.value);
        });
        setTimeout(function () { input.focus(); }, 60);
      }
    });
  }

  function askForKey() {
    if (!askForKey.promise) {
      askForKey.promise = ensureOverlay(
        '需要网关 API Key',
        '<div style="margin-bottom:10px;">此面板受网关鉴权保护，请输入一把已启用的 API Key'
        + '（在「API Keys」页创建）。</div>'
        + '<input type="password" placeholder="sk-…" style="width:100%;box-sizing:border-box;'
        + 'padding:8px 10px;border:1px solid #3a3b3f;border-radius:6px;background:#26272b;'
        + 'color:#e8e8e8;">',
        '保存并继续'
      ).then(function (value) {
        askForKey.promise = null;
        if (value) storeKey(String(value).trim());
        return value;
      });
    }
    return askForKey.promise;
  }

  function showLinkFallback(url) {
    ensureOverlay(
      '浏览器拦截了弹出窗口',
      '<div>请点击下面的链接打开授权页：</div>'
      + '<a href="' + url.replace(/"/g, '&quot;') + '" target="_blank" rel="noopener" '
      + 'style="color:#7fa7ff;word-break:break-all;">' + url + '</a>',
      '已完成，关闭'
    );
  }

  // ── 错误归一（与桌面 bridge 的 asError 同语义）─────────────
  function asError(failure) {
    if (failure instanceof Error) return failure;
    if (typeof failure === 'string') return new Error(failure);
    if (failure && typeof failure === 'object') {
      var message = failure.message != null ? failure.message
        : failure.error != null ? failure.error
        : failure.msg;
      if (typeof message === 'string' && message) return new Error(message);
      try { return new Error(JSON.stringify(failure)); } catch (e) { /* 落到下面 */ }
    }
    return new Error(String(failure == null ? '操作失败' : failure));
  }

  // ── HTTP 调用：信封解包与桌面 gateway::unwrap_envelope 对齐 ──
  async function httpCall(method, path, body, retried) {
    var headers = { 'Accept': 'application/json' };
    if (apiKey) headers['x-api-key'] = apiKey;
    var init = { method: method, headers: headers };
    var wantsBody = method === 'POST' || method === 'PUT' || method === 'PATCH'
      || !(body === null || body === undefined);
    if (wantsBody) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body === null || body === undefined ? {} : body);
    }
    var response;
    try {
      response = await fetch(path, init);
    } catch (error) {
      throw new Error('无法连接网关：' + (error && error.message ? error.message : error));
    }
    var text = await response.text();
    if (response.status === 401 && !retried) {
      // 按错误类型分流。panel_login_required：管理员已注册、要登录 ——
      // 先用长效 refresh 静默换新（access 过期时用户无感），不行再整页
      // 跳独立登录页；其余 401 = 未配管理员的部署，弹 Key 框兜底。
      var probe = null;
      try { probe = JSON.parse(text); } catch (e) { /* 非 JSON */ }
      var errorType = probe && probe.error && probe.error.type;
      if (errorType === 'panel_login_required') {
        var refreshed = await tryRefresh();
        if (refreshed) return httpCall(method, path, body, true);
        window.location.href = '/login';
        // 页面即将整页跳转，返回一个挂起的承诺占位
        return new Promise(function () {});
      }
      var key = await askForKey();
      if (key) {
        storeKey(key);
        return httpCall(method, path, body, true);
      }
    }
    var payload;
    try { payload = JSON.parse(text); } catch (e) {
      throw new Error(text.trim() || '本地代理返回了空响应（HTTP ' + response.status + '）');
    }
    var ok = response.status >= 200 && response.status < 300;
    var success = typeof payload.success === 'boolean' ? payload.success : undefined;
    if (!ok || success === false) {
      var detail = payload.error != null ? payload.error
        : payload.message != null ? payload.message
        : payload.msg != null ? payload.msg
        : 'HTTP ' + response.status;
      throw asError(typeof detail === 'string' ? detail : (detail && detail.message) || JSON.stringify(detail));
    }
    return payload && Object.prototype.hasOwnProperty.call(payload, 'data') ? payload.data : payload;
  }

  function call(method, path, body) {
    return httpCall(method, path, body === undefined ? null : body);
  }

  // access 短效令牌过期后的静默续期：refresh cookie（path 限 /api/panel）
  // 会由浏览器自动带上；成功 = 新双令牌已落 cookie，原请求可重试。
  async function tryRefresh() {
    try {
      var resp = await fetch('/api/panel/refresh', { method: 'POST' });
      return resp.ok;
    } catch (e) {
      return false;
    }
  }

  // ── 事件模拟 ──────────────────────────────────────────────
  var stateListeners = new Set();
  var stateEmitTimer = null;
  function emitStateSoon() {
    if (stateEmitTimer) return;
    stateEmitTimer = setTimeout(async function () {
      stateEmitTimer = null;
      try {
        var next = await call('GET', '/api/session');
        stateListeners.forEach(function (cb) { try { cb(next); } catch (e) { console.warn(e); } });
      } catch (e) { /* 面板关闭中的正常失败 */ }
    }, 300);
  }

  // ── 登录状态机（对齐桌面壳 login:state 的 {active, provider}）──
  var loginActive = false;
  var loginProvider = '';
  var loginListeners = new Set();
  function emitLogin() {
    var payload = { active: loginActive, provider: loginProvider };
    loginListeners.forEach(function (cb) { try { cb(payload); } catch (e) { console.warn(e); } });
  }

  var sleep = function (ms) { return new Promise(function (r) { setTimeout(r, ms); }); };

  async function pollWait(state) {
    var deadline = Date.now() + 5 * 60 * 1000;
    while (Date.now() < deadline) {
      await sleep(3000);
      var wait = await call('GET', '/api/session/login/wait?state=' + encodeURIComponent(state));
      if (wait && wait.done) {
        if (wait.error) throw new Error(wait.error);
        return wait.session == null ? {} : wait.session;
      }
    }
    throw new Error('登录等待超时（5 分钟）');
  }

  /** 发起一次登录流程：先开窗口（用户手势还在时占位成功率高），再拿地址导航。 */
  async function runLoginFlow(provider, startRequest) {
    loginActive = true;
    loginProvider = provider;
    emitLogin();
    var popup = null;
    try { popup = window.open('about:blank', 'a2a-login'); } catch (e) { /* 拦截时走兜底 */ }
    try {
      var started = await startRequest;
      var authUrl = started && started.authUrl;
      if (!authUrl) throw new Error('网关未返回授权地址');
      if (popup && !popup.closed) {
        popup.location.href = authUrl;
      } else {
        var second = null;
        try { second = window.open(authUrl, '_blank'); } catch (e) { /* 落到链接兜底 */ }
        if (!second) showLinkFallback(authUrl);
      }
      return await pollWait(started.state);
    } finally {
      if (popup && !popup.closed) { try { popup.close(); } catch (e) { /* 无害 */ } }
      loginActive = false;
      loginProvider = '';
      emitLogin();
    }
  }

  // ── 壳特有命令的网页端降级表 ──────────────────────────────
  var SHELL_UNAVAILABLE = '该操作在网页端不可用（仅桌面端支持）';

  var shellCommands = {
    api_request: function (args) {
      var request = (args && args.request) || {};
      return httpCall(request.method || 'GET', request.path || '/', request.body);
    },
    check_update: function () {
      // 网页端走镜像更新（拉新镜像重启容器），不提供安装包下载
      return Promise.resolve({
        currentVersion: 'web', latestVersion: 'web', hasUpdate: false,
        notes: '网页端通过 Docker 镜像更新：拉取新镜像后重启容器即可。',
        publishedAt: '', pageUrl: '', installerKind: 'none',
      });
    },
    get_update_status: function () { return call('GET', '/api/update/status'); },
    backend_status: function () {
      return Promise.resolve({ ready: true, port: null, portFromEnv: false, failure: null });
    },
    port_occupant: function () { return Promise.resolve(null); },
    get_app_settings: function () {
      // 桌面设置（关窗到托盘 / 开机自启）在网页端没有宿主，固定默认值
      return Promise.resolve({ closeToTray: false, autostart: false, proxyPort: 0 });
    },
    open_release_page: function (args) {
      if (args && args.url) { try { window.open(args.url, '_blank'); } catch (e) { /* 无害 */ } }
      return Promise.resolve({ url: (args && args.url) || '' });
    },
    export_accounts: function () {
      return downloadFile('GET', '/api/accounts/export', 'agent2api-accounts.json');
    },
    export_logs: function () {
      return downloadFile('GET', '/api/logs/download', 'agent2api-logs.txt');
    },
  };

  /** 网页端的「文件对话框」：拉成 Blob 触发浏览器下载 */
  async function downloadFile(method, path, filename) {
    var headers = { 'Accept': '*/*' };
    if (apiKey) headers['x-api-key'] = apiKey;
    var response = await fetch(path, { method: method, headers: headers });
    if (!response.ok) throw new Error('导出失败（HTTP ' + response.status + '）');
    var blob = await response.blob();
    // 管理 API 的响应带 { success, data } 信封：落盘前剥掉，存 data 本体 ——
    // 与桌面端的导出文件同一格式；信封壳留着，这份文件导回去时顶层没有
    // accounts，只会得到「缺少 accounts 字段」。顺带把账号数带回给调用方
    //（设置页用它提示「已导出 N 个账号」，拿不到会误报「没有可导出的账号」）
    var summary = {};
    try {
      var body = JSON.parse(await blob.text());
      if (body && body.success === true && body.data && typeof body.data === 'object') {
        var accounts = Array.isArray(body.data.accounts) ? body.data.accounts : null;
        var providers = Array.isArray(body.data.customProviders) ? body.data.customProviders : null;
        if (accounts) summary.count = accounts.length;
        if (providers) summary.customProviders = providers.length;
        blob = new Blob([JSON.stringify(body.data, null, 2)], { type: 'application/json' });
      }
    } catch (e) { /* 非 JSON 响应（日志下载）按原文保存 */ }
    var url = URL.createObjectURL(blob);
    var link = document.createElement('a');
    link.href = url;
    link.download = filename;
    document.body.appendChild(link);
    link.click();
    link.remove();
    setTimeout(function () { URL.revokeObjectURL(url); }, 5000);
    summary.saved = true;
    return summary;
  }

  /** 壳命令的总入口（同时接住 UI 里两处直接的 internals.invoke('api_request')） */
  function invokeShell(command, args) {
    var handler = shellCommands[command];
    if (handler) return handler(args);
    // 文件对话框类：导入要走文件选择，单独给一条带输入框的链路
    if (command === 'import_accounts') return importAccountsViaFile();
    return Promise.reject(new Error(
      command === 'download_update' || command === 'update_progress' || command === 'cancel_update'
      || command === 'run_installer'
        ? '软件更新在网页端不可用：请通过 Docker 镜像更新'
        : SHELL_UNAVAILABLE
    ));
  }

  async function importAccountsViaFile() {
    return new Promise(function (resolve, reject) {
      var input = document.createElement('input');
      input.type = 'file';
      input.accept = '.json,application/json';
      input.style.display = 'none';
      input.addEventListener('change', async function () {
        var file = input.files && input.files[0];
        input.remove();
        if (!file) { reject(new Error('未选择文件')); return; }
        try {
          var text = await file.text();
          var payload = JSON.parse(text);
          resolve(await call('POST', '/api/accounts/import', payload));
        } catch (error) {
          reject(asError(error));
        }
      });
      document.body.appendChild(input);
      input.click();
    });
  }

  // ── 装配对外接口 ─────────────────────────────────────────
  // __TAURI_INTERNALS__ 先装：界面里两处直接 internals.invoke('api_request')
  // （sms-login 的兜底与添加账号的 postAccount）依赖它存在。
  window.__TAURI_INTERNALS__ = {
    invoke: function (command, args) {
      return invokeShell(command, args).catch(asError).then(function (value) {
        if (value instanceof Error) throw value;
        return value;
      });
    },
    transformCallback: function (callback) {
      // 事件订阅占位：网页端没有 push 事件，返回一个无害 id 即可
      return 0;
    },
  };

  window.workbuddyDesktop = {
    // 平台标识：界面据此裁剪本机功能（导入桌面端登录态等在 web 下隐藏）
    platform: 'web',

    // ── 会话 ──
    getState: function () { return call('GET', '/api/session'); },
    startLogin: function (edition, mode, provider) {
      var target = provider || 'workbuddy';
      return runLoginFlow(target, call('POST', '/api/session/login/start', {
        edition: edition || 'cn',
        provider: target,
      }));
    },
    getLoginState: function () {
      return Promise.resolve({ active: loginActive, provider: loginProvider });
    },
    cancelLogin: async function () {
      // 桌面壳只记一个活动登录；网页端由本状态机驱动，直接清状态。
      // 上游任务本身会因无人轮询而在超时后落定，不需要额外请求。
      var wasActive = loginActive;
      loginActive = false;
      loginProvider = '';
      emitLogin();
      return wasActive;
    },
    startAutoclawOauthLogin: function (state, authUrl, mode) {
      if (!state || !authUrl) return Promise.reject(new Error('缺少授权参数（state / authUrl）'));
      return runLoginFlow('autoclaw-intl', Promise.resolve({ state: state, authUrl: authUrl }));
    },
    getAutoclawOauthCaptchaConfig: function (provider) {
      return call('POST', '/api/session/login/oauth/captcha-config',
        provider ? { provider: String(provider) } : {});
    },
    startAutoclawOauth: function (provider, vendor, captchaVerifyParam) {
      return call('POST', '/api/session/login/oauth/start', {
        provider: provider ? String(provider) : undefined,
        vendor: String(vendor || ''),
        captchaVerifyParam: String(captchaVerifyParam || ''),
      });
    },
    onLoginState: function (callback) {
      loginListeners.add(callback);
      return function () { loginListeners.delete(callback); };
    },
    refreshSession: async function () {
      await call('POST', '/api/session/refresh', {});
      return call('GET', '/api/session');
    },
    logout: async function () {
      await call('POST', '/api/session/logout', {});
      return call('GET', '/api/session');
    },

    // ── 配置 ──
    getConfig: function () { return call('GET', '/api/config'); },
    saveConfig: function (payload) { return call('POST', '/api/config', payload); },

    // ── 模型清单 ──
    refreshModels: function () { return call('POST', '/api/models/refresh', {}); },
    getModelManage: function () { return call('GET', '/api/models/manage'); },
    setModelState: function (payload) { return call('POST', '/api/models/state', payload); },
    // 第 4 / 第 5 个参数（思考等级 / 映射开关）都按「有没有传」决定是否进请求体：
    // 后端按「请求体里有没有这个键」区分三态，undefined 的键不会进 JSON
    addModelMapping: function (alias, target, provider, reasoning, enabled) {
      var payload = { alias: alias, target: target, provider: provider };
      if (reasoning !== undefined) payload.reasoning = reasoning;
      if (enabled !== undefined) payload.enabled = enabled;
      return call('POST', '/api/models/mappings', payload);
    },
    removeModelMapping: function (alias, target, provider) {
      return call('POST', '/api/models/mappings/remove', { alias: alias, target: target, provider: provider });
    },
    addCustomModel: function (provider, id) { return call('POST', '/api/models/custom', { provider: provider, id: id }); },
    removeCustomModel: function (provider, id) { return call('POST', '/api/models/custom/remove', { provider: provider, id: id }); },

    // ── 网关 API Key（多把）──
    getKeys: function () { return call('GET', '/api/keys'); },
    createKey: function (payload) { return call('POST', '/api/keys', payload || {}); },
    updateKey: function (id, patch) { return call('PATCH', '/api/keys/' + encodeURIComponent(id), patch); },
    deleteKey: function (id) { return call('DELETE', '/api/keys/' + encodeURIComponent(id)); },

    // ── 多账号 ──
    switchAccount: function (id) { return call('POST', '/api/accounts/current', { id: id }); },
    refreshAccountToken: function (id) { return call('POST', '/api/accounts/refresh', id ? { id: id } : {}); },
    removeAccount: function (id) { return call('DELETE', '/api/accounts/' + encodeURIComponent(id)); },
    updateAccount: function (id, patch) { return call('PATCH', '/api/accounts/' + encodeURIComponent(id), patch); },
    moveAccount: function (id, direction) {
      return call('POST', '/api/accounts/' + encodeURIComponent(id) + '/move', {
        direction: direction === 'down' ? 'down' : 'up',
      });
    },
    clearRateLimits: function (id, model) {
      return call('POST', '/api/accounts/' + encodeURIComponent(id) + '/rate-limits/clear',
        model ? { model: model } : {});
    },
    batchAccounts: function (payload) {
      payload = payload || {};
      var body = {
        action: String(payload.action || ''),
        ids: Array.isArray(payload.ids) ? payload.ids : [],
      };
      if (Object.prototype.hasOwnProperty.call(payload, 'proxy')) body.proxy = payload.proxy;
      return call('POST', '/api/accounts/batch', body);
    },

    // ── 出网代理 ──
    getProxies: function () { return call('GET', '/api/proxies'); },
    testProxy: function (payload) {
      var body = {};
      if (payload && typeof payload === 'object' && !Array.isArray(payload)) {
        if (typeof payload.id === 'string' && payload.id) body.id = payload.id;
        else if (Object.prototype.hasOwnProperty.call(payload, 'proxy')) body.proxy = payload.proxy;
      }
      return call('POST', '/api/proxies/test', body);
    },

    // ── 积分 / 签到 ──
    getUsage: function () { return call('GET', '/api/usage'); },
    getCheckinStatus: function () { return call('GET', '/api/checkin/status'); },
    claimCheckin: function () { return call('POST', '/api/checkin', {}); },
    getAllBalances: function (id) {
      return call('GET', '/api/accounts/usage' + (id ? '?id=' + encodeURIComponent(id) : ''));
    },
    getBalancesSnapshot: function () { return call('GET', '/api/accounts/usage/snapshot'); },
    getAccountConnections: function () { return call('GET', '/api/accounts/connections'); },
    checkinAllAccounts: function (id) { return call('POST', '/api/accounts/checkin', id ? { id: id } : {}); },

    // ── 手机验证码登录（AutoClaw 国内版）──
    sendSmsCode: function (input) {
      var isObject = input && typeof input === 'object';
      var phone = String((isObject ? input.phone : input) || '');
      var provider = isObject && input.provider ? String(input.provider) : '';
      return call('POST', '/api/session/login/sms/send', {
        phone: phone,
        provider: provider || undefined,
      });
    },
    verifySmsLogin: function (payload) {
      payload = payload || {};
      var body = {
        phone: String(payload.phone || ''),
        code: String(payload.code || ''),
      };
      if (payload.deviceId) body.deviceId = String(payload.deviceId);
      if (payload.name) body.name = String(payload.name);
      if (payload.provider) body.provider = String(payload.provider);
      return call('POST', '/api/session/login/sms/verify', body);
    },

    // ── 定时签到 ──
    getAutoCheckin: function () { return call('GET', '/api/auto-checkin'); },
    saveAutoCheckin: function (patch) { return call('POST', '/api/auto-checkin', patch); },
    runAutoCheckinNow: function () { return call('POST', '/api/auto-checkin/run', {}); },

    // ── 间隔型定时任务 ──
    getScheduledTasks: function () { return call('GET', '/api/scheduled-tasks'); },
    saveScheduledTask: function (id, patch) {
      return call('PATCH', '/api/scheduled-tasks/' + encodeURIComponent(String(id || '')), patch);
    },
    runScheduledTask: function (id) {
      return call('POST', '/api/scheduled-tasks/' + encodeURIComponent(String(id || '')) + '/run', {});
    },

    // ── 软件更新（检查可用，下载 / 安装明确拒绝）──
    checkUpdate: function () { return invokeShell('check_update'); },
    getUpdateStatus: function () { return call('GET', '/api/update/status'); },
    downloadUpdate: function (payload) { return invokeShell('download_update', payload); },
    updateProgress: function () { return invokeShell('update_progress'); },
    cancelUpdate: function () { return invokeShell('cancel_update'); },
    runInstaller: function (path, restart) {
      return invokeShell('run_installer', { path: String(path || ''), restart: restart !== false });
    },
    openReleasePage: function (url) { return invokeShell('open_release_page', { url: String(url || '') }); },

    // ── 出站指纹脱敏 ──
    getSanitize: function () { return call('GET', '/api/sanitize'); },
    saveSanitize: function (enabled) {
      return call('PUT', '/api/sanitize', { sanitizeBlacklistFingerprints: enabled === true });
    },

    // ── 系统提示词与内容拦截降级 ──
    getPrompt: function () { return call('GET', '/api/prompt'); },
    savePrompt: function (payload) {
      payload = payload || {};
      return call('PUT', '/api/prompt', {
        promptMode: payload.promptMode != null ? String(payload.promptMode) : null,
        promptFile: payload.promptFile != null ? String(payload.promptFile) : null,
        clearDegrade: !!payload.clearDegrade,
      });
    },

    // ── 运行日志 ──
    getLogs: function (query) { return call('GET', '/api/logs' + toQuery(query)); },
    getLogStats: function () { return call('GET', '/api/logs/stats'); },
    clearLogs: function (query) { return call('DELETE', '/api/logs' + toQuery(query)); },
    exportLogs: function () { return invokeShell('export_logs'); },

    // ── 请求统计报表 / 数据保留 ──
    getStatsSummary: function (range) {
      return call('GET', '/api/stats/summary?range=' + encodeURIComponent(range));
    },
    getStatsRequests: function (query) { return call('GET', '/api/stats/requests' + toQuery(query)); },
    getStatsRequestFilters: function () { return call('GET', '/api/stats/requests/filters'); },
    clearStatsRequests: function (query) { return call('DELETE', '/api/stats/requests' + toQuery(query)); },
    // 按 id 取单条请求的原始正文（详情弹窗「预览对话」的数据源；找不到给 404）
    getStatsRequestRaw: function (id) {
      return call('GET', '/api/stats/requests/raw' + toQuery({ id: id }));
    },
    // 清理弹窗的预览统计（与 DELETE 共用同一份筛选解析，预览与执行必须同源）
    getStatsClearPreview: function (query) {
      return call('GET', '/api/stats/requests/clear-preview' + toQuery(query));
    },
    // 后台压缩数据库：重复触发 409，进度看 clear-preview 的 vacuumRunning
    compactStatsDb: function () { return call('POST', '/api/stats/requests/compact'); },
    getRetention: function () { return call('GET', '/api/retention'); },
    saveRetention: function (patch) { return call('PUT', '/api/retention', patch); },

    // ── 面板登录（headless 托管面板才有「登录面板」的概念）──
    panelLogout: function () { return call('POST', '/api/panel/logout', {}); },

    // ── 机器人校验开关（登录 / 注册的 ALTCHA proof-of-work）──
    getCaptchaSetting: function () { return call('GET', '/api/captcha'); },
    saveCaptchaSetting: function (on) {
      return call('PUT', '/api/captcha', { captchaEnabled: on === true });
    },

    // ── 数据存储概况 ──
    getStorage: function () { return call('GET', '/api/storage'); },

    // ── 数据结构升级 ──
    getUpgrade: function () { return call('GET', '/api/upgrade'); },
    runUpgrade: function () { return call('POST', '/api/upgrade/run', {}); },

    // ── 请求重试 ──
    getRetry: function () { return call('GET', '/api/retry'); },
    saveRetry: function (patch) { return call('PUT', '/api/retry', patch); },

    // ── 调试模式 ──
    getDebug: function () { return call('GET', '/api/debug'); },
    saveDebug: function (enabled) { return call('PUT', '/api/debug', { debugMode: enabled }); },
    getDebugTraffic: function (id) {
      return call('GET', '/api/debug/traffic?id=' + encodeURIComponent(id));
    },

    // ── 事件 ──
    onStateChanged: function (callback) {
      stateListeners.add(callback);
      return function () { stateListeners.delete(callback); };
    },
    // 后端主动推送的维护提醒在网页端没有对应物：订阅合法但不触发
    onAutoMaintained: function () { return function () {}; },
    onBackendError: function () { return function () {}; },

    // ── 壳特有：后端就绪与端口处置 ──
    getBackendStatus: function () { return invokeShell('backend_status'); },
    getPortOccupant: function () { return invokeShell('port_occupant'); },
    endPortOccupant: function () { return Promise.reject(new Error(SHELL_UNAVAILABLE)); },
    checkPort: function () {
      return Promise.reject(new Error('网页端不探测端口：端口由 AGENT2API_PORT 环境变量决定'));
    },
    changePort: function () {
      return Promise.reject(new Error('网页端不支持改端口：请设置环境变量 AGENT2API_PORT 后重启容器'));
    },
    restartApp: function () {
      return Promise.reject(new Error('网页端不支持重启：请重启容器（docker compose restart）'));
    },

    // ── 窗口主题：没有窗口主题可钉，交给系统/浏览器偏好 ──
    setWindowTheme: function () { return Promise.resolve(); },

    // ── 自定义标题栏的窗口三键：网页端没有应用窗口 ──
    // 按「能映射则映射，不能则明确拒绝」的惯例处理：三个动作明确拒绝，
    // isMaximized 是查询而非动作，照 backend_status 的口径返回常态 false；
    // 事件订阅照 onAutoMaintained 的口径返回空操作。
    // 实际上标题栏在网页端根本不渲染（titlebar.js 的 platform 守卫：
    // 本 shim 注入 platform='web'），这组方法只是兜底防误调。
    windowMinimize: function () {
      return Promise.reject(new Error(SHELL_UNAVAILABLE + '：浏览器里没有应用窗口'));
    },
    windowToggleMaximize: function () {
      return Promise.reject(new Error(SHELL_UNAVAILABLE + '：浏览器里没有应用窗口'));
    },
    windowClose: function () {
      return Promise.reject(new Error(SHELL_UNAVAILABLE + '：浏览器里没有应用窗口'));
    },
    windowIsMaximized: function () { return Promise.resolve(false); },
    onWindowResize: function () { return function () {}; },

    // ── 应用设置与账号导入导出 ──
    getAppSettings: function () { return invokeShell('get_app_settings'); },
    saveAppSettings: function (patch) {
      // 无处持久化也不该报错：界面保存成功即可（本次会话内忽略）
      return Promise.resolve(Object.assign({ closeToTray: false, autostart: false, proxyPort: 0 }, patch));
    },
    exportAccounts: function () { return invokeShell('export_accounts'); },
    importAccounts: function () { return invokeShell('import_accounts'); },
  };

  // 把筛选条件转成查询串（与桌面 bridge 的 toQuery 同语义：空值跳过）
  function toQuery(query) {
    if (!query) return '';
    if (typeof query === 'string') {
      return query.indexOf('?') === 0 || query === '' ? query : '?' + query;
    }
    if (query instanceof URLSearchParams) {
      var text = query.toString();
      return text ? '?' + text : '';
    }
    var params = new URLSearchParams();
    for (var key in query) {
      if (!Object.prototype.hasOwnProperty.call(query, key)) continue;
      var value = query[key];
      if (value === undefined || value === null || value === '') continue;
      params.append(key, String(value));
    }
    var result = params.toString();
    return result ? '?' + result : '';
  }

  // 写类请求成功后重放一次状态（模拟桌面的 accounts:state-changed 推送）：
  // 拦一层 call，只对管理 API 的非 GET 生效，/v1/* 与登录轮询不受影响。
  var rawCall = call;
  call = function (method, path, body) {
    var promise = rawCall(method, path, body);
    if (method !== 'GET' && path.indexOf('/api/') === 0) {
      promise.then(function () { emitStateSoon(); }, function () {});
    }
    return promise;
  };
})();
"#
}
