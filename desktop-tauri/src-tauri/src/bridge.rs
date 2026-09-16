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

pub const BRIDGE_JS: &str = r#"
(() => {
  const invoke = (command, args) => {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      return Promise.reject(new Error('桌面运行时不可用（Tauri 未初始化）'));
    }
    return internals.invoke(command, args);
  };

  /** 统一调用管理 API：返回后端 data，失败时抛错（界面按原有 try/catch 处理） */
  const call = (method, path, body) =>
    invoke('api_request', { request: { method, path, body: body === undefined ? null : body } });

  /** 把渲染层的筛选条件转成查询串 */
  const toQuery = query => {
    if (!query) return '';
    if (typeof query === 'string') return query.startsWith('?') || query === '' ? query : '?' + query;
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
   */
  const on = (event, callback) => {
    let unlisten = null;
    let disposed = false;
    invoke('plugin:event|listen', {
      event,
      target: { kind: 'Any' },
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
    // ── 会话 ──
    getState: () => call('GET', '/api/session'),
    startLogin: (edition, mode) =>
      invoke('start_login', { edition: edition || 'cn', mode: mode || 'embedded' }),
    getLoginState: () => invoke('login_state'),
    cancelLogin: () => invoke('cancel_login'),
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

    // ── 多账号 ──
    // switchAccount 即「设为当前」：把账号置顶为转发顺序第一位
    switchAccount: id => call('POST', '/api/accounts/current', { id }),
    refreshAccountToken: id => call('POST', '/api/accounts/refresh', id ? { id } : {}),
    removeAccount: id => call('DELETE', '/api/accounts/' + encodeURIComponent(id)),
    uploadAccount: (name, fileText, edition) => {
      let parsed;
      try {
        parsed = JSON.parse(fileText);
      } catch (error) {
        throw new Error('粘贴内容不是有效 JSON');
      }
      return call('POST', '/api/accounts', {
        name: typeof name === 'string' ? name.trim().slice(0, 100) : '',
        ...parsed,
        // 弹窗里选的版本优先于粘贴内容里的同名字段
        edition: edition === 'intl' ? 'intl' : 'cn',
      });
    },
    updateAccount: (id, patch) => call('PATCH', '/api/accounts/' + encodeURIComponent(id), patch),
    moveAccount: (id, direction) =>
      call('POST', '/api/accounts/' + encodeURIComponent(id) + '/move', {
        direction: direction === 'down' ? 'down' : 'up',
      }),
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
    getAllBalances: () => call('GET', '/api/accounts/usage'),
    checkinAllAccounts: id => call('POST', '/api/accounts/checkin', id ? { id } : {}),

    // ── 定时签到 ──
    getAutoCheckin: () => call('GET', '/api/auto-checkin'),
    saveAutoCheckin: patch => call('POST', '/api/auto-checkin', patch),
    runAutoCheckinNow: () => call('POST', '/api/auto-checkin/run', {}),

    // ── 软件更新 ──
    // checkUpdate 走壳命令：当前版本号只有壳知道（后端是独立进程），
    // 由壳把版本带上去交给后端比较
    checkUpdate: () => invoke('check_update'),
    downloadUpdate: payload => invoke('download_update', {
      url: String((payload && payload.url) || ''),
      name: (payload && payload.name) ? String(payload.name) : null,
    }),
    updateProgress: () => invoke('update_progress'),
    cancelUpdate: () => invoke('cancel_update'),
    runInstaller: (path, restart) =>
      invoke('run_installer', { path: String(path || ''), restart: restart !== false }),
    openReleasePage: url => invoke('open_release_page', { url: String(url || '') }),

    // ── 敏感词脱敏 ──
    getDesensitize: () => call('GET', '/api/desensitize'),
    setDesensitizeEnabled: enabled =>
      call('POST', '/api/desensitize/enabled', { enabled: enabled === true }),
    setDesensitizeRoles: roles =>
      call('POST', '/api/desensitize/roles', {
        roles: Array.isArray(roles) ? roles.filter(role => typeof role === 'string') : [],
      }),
    saveDesensitizeTerms: terms => call('PUT', '/api/desensitize/terms', { terms }),
    addDesensitizeTerms: terms => call('POST', '/api/desensitize/terms', { terms }),
    removeDesensitizeTerm: term => call('DELETE', '/api/desensitize/terms', { terms: [term] }),
    resetDesensitizeTerms: () => call('POST', '/api/desensitize/reset', {}),
    resetDesensitizeStats: () => call('POST', '/api/desensitize/stats/reset', {}),

    // ── 运行日志 ──
    getLogs: query => call('GET', '/api/logs' + toQuery(query)),
    getLogStats: () => call('GET', '/api/logs/stats'),
    clearLogs: () => call('DELETE', '/api/logs'),
    exportLogs: () => invoke('export_logs'),

    // ── 事件 ──
    onStateChanged: callback => on('accounts:state-changed', callback),
    onAutoMaintained: callback => on('accounts:auto-maintained', callback),

    // ── 本壳特有：后端就绪状态（界面可选使用） ──
    getBackendStatus: () => invoke('backend_status'),

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
