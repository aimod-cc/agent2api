/* Agent2API · 端口状态面板
 *
 * ── 这个面板负责什么 ────────────────────────────────────────
 * 侧栏底部那两条状态（网关进程 / 可用账号），以及端口冲突时的两个出口
 * （结束占用进程 / 更换端口）。
 *
 * ── 为什么从 app.js 拆出来 ──────────────────────────────────
 * 它有一整套自己的状态与交互：后端状态轮询、两个按钮、一个弹窗、以及
 * 「先探测再写盘再重启」的保存流程。留在 app.js 里会让那个文件继续膨胀
 * （超过项目约定的 800 行），而它与账号、模型这些业务没有耦合 ——
 * 与其它面板（accounts-view / tasks-panel …）同一个模式，自持状态、只读全局。
 *
 * ── 与 app.js 的分工 ────────────────────────────────────────
 *   · app.js：页面的 render() 编排、state（账号/会话）的获取与分发
 *   · 本文件：后端进程状态与端口。**不碰 state**，只读它算「可用账号」那一行。
 * 暴露 window.wbPortPanel 给 app.js 的顶栏徽标用（它也要显示网关是否就绪）。
 *
 * 依赖 window.wbApp 的 getState / toast / renderTopbarStatus（与其它面板一致）。
 */
(() => {
  const api = window.workbuddyDesktop;
  const $ = id => document.getElementById(id);

  /** 3065 是官方默认端口；真实端口由 getBackendStatus 异步补上 */
  const FALLBACK_PORT = 3065;

  /** 当前展示的网关地址：默认端口起步，拿到真实端口后替换 */
  let gatewayBase = `http://127.0.0.1:${FALLBACK_PORT}`;
  /** 防止并发重复请求（轮询与切页可能同时触发） */
  let syncing = false;

  /**
   * 壳侧上报的后端状态（`getBackendStatus` 的最近一次结果）。
   *
   * 与 `state.health` 是两件事，别混：
   *   · 这里说的是**网关进程**（在不在监听、为什么没起来）
   *   · state.health 说的是**上游凭证**（有没有可用账号）
   * 为 null 表示还没查过（首屏那一刻），不是「不正常」。
   */
  let backendStatus = null;

  /** 端口号（顶栏徽标与状态行共用；不带协议） */
  const portLabel = () => gatewayBase.replace(/^https?:\/\//, '');

  /** 网关进程是否在监听（顶栏徽标用；null = 还没查过） */
  const isReady = () => backendStatus?.ready === true;

  /**
   * 只负责写接口条里的地址元素，首次渲染与端口补更新共用，避免两处文案走偏。
   *
   * 对话协议三种都写出来（Chat Completions / Responses / Anthropic Messages）：
   * 客户端支持哪种就填哪一行，三家共用同一套模型与账号池。
   * 元素缺失（不在网关页 / 旧 DOM）时静默跳过，别让 render 崩。
   */
  function paintGatewayAddress(base) {
    const paint = (id, text) => {
      const el = $(id);
      if (el) el.textContent = text;
    };
    paint('api-base', `${base}/v1`);
    paint('api-chat', `POST ${base}/v1/chat/completions`);
    paint('api-responses', `POST ${base}/v1/responses`);
    paint('api-messages', `POST ${base}/v1/messages`);
    paint('api-models', `GET ${base}/v1/models`);
  }

  /**
   * 侧栏底部的两条状态：常驻显示，不必切到「网关」页也能确认服务是否在监听。
   *
   * ── 为什么是两条（别再合并回一条）──────────────────────────
   * 这里原本只有一行「网关运行中 / 网关未就绪」，判据却取自 /api/session 的
   * upstreamConfigured（= 有没有可用账号）。于是「没有账号」被说成了
   * 「网关没就绪」—— 而能渲染出这句话本身就证明网关正在监听（否则这个请求
   * 根本发不出去）。同一个字段在顶栏又叫「未登录」，用户无从判断哪个坏了。
   *
   * 现在拆成两个互相独立的事实，判据各自独立：
   *   · 网关进程：`backendStatus.ready`，来自壳侧 is_ready 探测，
   *     **不依赖本页面能否调通管理 API**（端口冲突时管理 API 全打不通，
   *     那时 state 是 null，只有这份状态能说清发生了什么）
   *   · 可用账号：`state.accounts.currentAccountId`，全局队列队首，
   *     **与转发口径一致**（health.upstreamConfigured 收窄到 workbuddy 一家，
   *     只登录小浣熊的机器上会误报红）
   */
  function renderSidebarStatus() {
    const dot = $('sidebar-status-dot');
    const text = $('sidebar-status-text');
    if (!dot || !text) return;
    const port = portLabel();

    // ── 第一条：网关进程 ──
    const ready = backendStatus?.ready;
    const failure = backendStatus?.failure;
    if (ready === true) {
      dot.className = 'live pulse';
      text.textContent = `网关运行中 · ${port}`;
    } else if (failure) {
      // 短标签由后端给（failure.label），界面不自己拼 —— 否则侧栏、弹窗、
      // 日志里会出现三套说法，用户无从判断哪个是真的
      dot.className = 'live bad';
      text.textContent = `${failure.label || '网关启动失败'} · ${port}`;
    } else if (ready === false) {
      dot.className = 'live off';
      text.textContent = `网关启动中… · ${port}`;
    } else {
      dot.className = 'live off';
      text.textContent = `正在检查… · ${port}`;
    }
    const box = $('sidebar-status');
    if (box) {
      // 完整说明（可能上百字）只进 title 与弹窗：状态条只有一行
      box.title = failure
        ? failure.message
        : ready === true
          ? `本地网关正在监听 ${gatewayBase}，可直接调用 OpenAI 兼容接口`
          : '正在确认网关是否已就绪';
    }

    // ── 第二条：可用账号 ──
    const accountDot = $('sidebar-account-dot');
    const accountText = $('sidebar-account-text');
    if (accountDot && accountText) {
      const current = window.wbApp?.getState?.();
      const accounts = current?.accounts?.accounts || [];
      const enabled = accounts.filter(item => item.enabled !== false).length;
      if (!current) {
        // 状态还没到（首屏）或管理 API 打不通 —— 打不通的原因由上面那条说
        accountDot.className = 'live off';
        accountText.textContent = '正在检查…';
        accountText.title = '';
      } else if (current.accounts?.currentAccountId) {
        accountDot.className = 'live';
        accountText.textContent = `${enabled} 个账号可用`;
        accountText.title = '转发请求时会在这些账号之间按优先级轮换';
      } else {
        accountDot.className = 'live bad';
        accountText.textContent = '无可用账号';
        accountText.title = accounts.length
          ? '账号都被禁用或缺少凭证，转发会失败 —— 去「账号」页启用一个'
          : '还没有登录任何账号 —— 去「账号」页添加一个';
      }
    }

    // ── 出口：有端口冲突时才露出 ──
    //
    // 两个按钮的显隐条件**不同**，不能一起判断：
    //   · 端口被别的进程占着 → 两个都给（结束进程能拿回端口，换端口是备选）
    //   · 端口被系统保留     → 只给「更换端口」（那里没有进程可杀，
    //                          给个点了必然失败的按钮比不给更糟）
    //   · 非端口原因起不来   → 都不给（换端口解决不了配置迁移失败这类问题）
    const actions = $('sidebar-status-actions');
    if (actions) {
      actions.hidden = !failure?.conflict;
      const endButton = $('btn-end-occupant');
      if (endButton) endButton.hidden = !backendStatus?.canEndOccupant;
    }
  }

  /**
   * 显示给用户复制的地址必须是真实监听端口（否则复制出去连不上）。
   * render() 是同步的、调用方众多，所以这里先用兜底值渲染，再异步取真实端口补更新；
   * 取不到（抛错或字段缺失）就保持兜底值，页面照常显示，不留空也不报错。
   *
   * 端口可能被用户改（「更换端口」会重启应用），所以这里**不做一次性缓存**：
   * 每次渲染都重新问一次壳侧，改完端口重启回来就能立刻显示新地址。
   */
  async function sync() {
    if (syncing) return;
    syncing = true;
    try {
      const status = await api.getBackendStatus();
      backendStatus = status;
      const port = Number(status?.port) || 0;
      if (port) {
        const next = `http://127.0.0.1:${port}`;
        if (next !== gatewayBase) {
          gatewayBase = next;
          paintGatewayAddress(next);
        }
      }
      // 就绪与否、失败原因都可能在这一刻变化，重画状态区与顶栏徽标
      renderSidebarStatus();
      window.wbApp?.renderTopbarStatus?.();
    } catch (error) {
      // 壳侧命令不可用（浏览器直开调试）或调用失败：保持兜底值，
      // 状态区维持上一次的结果，不清空 —— 清空会让状态灯无故熄灭
      console.warn('读取后端状态失败:', error.message);
    } finally {
      syncing = false;
    }
  }

  /** 由 app.js 的 render() 调用：重画地址与状态，并异步补上真实端口 */
  function render() {
    paintGatewayAddress(gatewayBase);
    renderSidebarStatus();
    void sync();
  }

  // ─── 出口一：结束占用端口的进程 ────────────────

  /**
   * 结束之前**必须让用户看清要动的是谁**：这是唯一一处会结束本机其它进程的操作，
   * 而「占用端口的进程」既可能是上次没退干净的旧网关，也可能是用户自己起的服务。
   * 把进程名与路径摆出来，用户才有机会说「不」。
   */
  async function endPortOccupant() {
    const button = $('btn-end-occupant');
    if (!button || button.disabled) return;
    button.disabled = true;
    const original = button.textContent;
    button.textContent = '查询中…';
    try {
      const info = await api.getPortOccupant();
      const occupant = info?.occupant;
      if (!occupant) {
        // 没查到进程：多半是系统保留段（那里根本没有进程可杀）。
        // 这里不引导用户去杀进程，直接把出路指向换端口。
        window.wbApp?.toast?.(`端口 ${info?.port ?? ''} 上没有查到监听进程，请改用「更换端口」`, 'err');
        return;
      }
      const lines = [
        '即将结束以下进程：',
        '',
        `进程名：${occupant.name}`,
        `PID：${occupant.pid}`,
        `路径：${occupant.path || '（未知）'}`,
        '',
        '结束它会强制退出该程序未保存的数据。确认继续？',
      ];
      // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行（等于没有确认）——
      // 走自绘确认弹窗（wbConfirm）；text 形态内部会转义并保留换行
      if (!(await window.wbConfirm?.ask?.({
        title: '结束占用端口的进程',
        text: lines.join('\n'),
        okText: '结束进程',
        okClass: 'danger',
      }))) return;

      button.textContent = '结束中…';
      const result = await api.endPortOccupant();
      if (result?.released) {
        window.wbApp?.toast?.(`已结束进程 ${occupant.name}（PID ${occupant.pid}），端口已释放`);
        // 端口释放了，但网关还没起来（它启动时端口被占，已经放弃）。
        // 问一句是否现在重启，而不是替用户决定重启。
        if (await window.wbConfirm?.ask?.({
          title: '重启程序',
          text: '端口已释放。现在重启程序让网关用这个端口启动？',
          okText: '重启',
        })) {
          await restartApp('重启中…');
        } else {
          await sync();
        }
      } else {
        window.wbApp?.toast?.('进程已结束，但端口仍被占用，建议改用「更换端口」', 'err');
        await sync();
      }
    } catch (error) {
      window.wbApp?.toast?.(`结束失败：${error.message}`, 'err');
    } finally {
      button.disabled = false;
      button.textContent = original;
    }
  }

  // ─── 出口二：更换端口 ─────────────────────────

  /** 重启应用（壳侧会拉起新进程）；提示语由调用方给，因为触发场景不同 */
  async function restartApp(message) {
    window.wbApp?.toast?.(message || '正在重启…');
    try {
      await api.restartApp();
      // 重启会带走本进程，新窗口由壳侧拉起；这里不做后续处理
    } catch (error) {
      window.wbApp?.toast?.(`重启失败：${error.message}`, 'err');
    }
  }

  /** 打开「更换端口」弹窗 */
  async function openPortModal() {
    const modal = $('port-modal');
    if (!modal) return;
    const status = backendStatus || (await api.getBackendStatus().catch(() => null));
    const port = Number(status?.port) || FALLBACK_PORT;
    $('port-modal-status').textContent = status?.failure?.message
      || `当前端口 ${port}，网关运行正常。`;
    const input = $('port-input');
    // 预填一个大概率可用的候选：当前端口 +1（避开当前那个占用者）
    input.value = port < 65535 ? String(port + 1) : String(port - 1);
    $('port-hint').textContent = '';
    $('port-modal-message').textContent = '';
    updatePortPreview();
    modal.classList.add('open');
    input.focus();
    input.select();
  }

  function closePortModal() {
    $('port-modal')?.classList.remove('open');
  }

  /** 弹窗里「新地址」预览随输入实时更新 */
  function updatePortPreview() {
    const value = Number($('port-input')?.value) || 0;
    const target = $('port-new-base');
    if (target) target.textContent = value ? `http://127.0.0.1:${value}` : 'http://127.0.0.1:—';
  }

  /**
   * 保存新端口并重启。
   *
   * 保存前先让后端做一次真实 bind 探测（`checkPort`）—— 用户填的端口可能
   * 同样被占或落在系统保留段里，那时候写盘 + 重启只会让程序起不来。
   * 探测用的判据与启动时完全一致，所以这里说可用就是真可用。
   */
  async function savePort() {
    const input = $('port-input');
    const button = $('port-modal-save');
    const message = $('port-modal-message');
    const hint = $('port-hint');
    const port = Number(input?.value) || 0;
    if (!button || button.disabled) return;

    if (!port || port < 1024 || port > 65535) {
      if (message) message.textContent = '端口需在 1024-65535 之间';
      return;
    }

    button.disabled = true;
    const original = button.textContent;
    button.textContent = '校验中…';
    if (message) message.textContent = '';
    if (hint) hint.textContent = '';
    try {
      const check = await api.checkPort(port);
      if (!check?.ok) {
        // 不可用的原因由后端给（含「被系统保留」的完整说明），直接显示
        if (hint) hint.textContent = check?.message || '该端口不可用';
        return;
      }
      if (check.same) {
        if (hint) hint.textContent = check.message;
        return;
      }
      button.textContent = '保存中…';
      const result = await api.changePort(port);
      if (result?.changed === false) {
        if (hint) hint.textContent = '与当前端口相同，无需重启';
        return;
      }
      closePortModal();
      await restartApp(`端口已改为 ${port}，正在重启…`);
    } catch (error) {
      if (message) message.textContent = error.message;
    } finally {
      button.disabled = false;
      button.textContent = original;
    }
  }

  // ─── 事件绑定 ─────────────────────────────────

  $('btn-end-occupant')?.addEventListener('click', () => { void endPortOccupant(); });
  $('btn-change-port')?.addEventListener('click', () => { void openPortModal(); });
  $('port-modal-close')?.addEventListener('click', closePortModal);
  $('port-modal-cancel')?.addEventListener('click', closePortModal);
  $('port-modal')?.addEventListener('click', event => {
    if (event.target === $('port-modal')) closePortModal();
  });
  $('port-modal-save')?.addEventListener('click', () => { void savePort(); });
  $('port-input')?.addEventListener('input', updatePortPreview);
  $('port-input')?.addEventListener('keydown', event => {
    if (event.key === 'Enter') void savePort();
  });
  // ESC 关闭：与其它弹窗一致（项目里 ESC 是逐弹窗实现的，没有全局约定）
  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && $('port-modal')?.classList.contains('open')) closePortModal();
  });

  /**
   * 订阅启动失败事件：`backend:error` 在端口冲突等场景下由壳侧发出。
   *
   * 事件只是「现在就去查一次」的提醒 —— 真正的状态以 `getBackendStatus`
   * 的返回为准（事件可能在界面订阅之前就发过了，见 Rust 侧 state.rs 的说明）。
   */
  api.onBackendError?.(() => { void sync(); });

  window.wbPortPanel = { render, sync, portLabel, isReady };
})();
