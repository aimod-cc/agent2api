/* Agent2API · 「手机验证码登录」交互引擎（当前只有 AutoClaw 用）

   与小浣熊 / Qoder 的网页登录（web-login.js）是**两套东西**，不要合并：

     网页登录：开窗口 → 用户在官方页面上操作 → 网关等回调/轮询 → 拿凭证。
     手机验证码：在本弹窗里填手机号 → 发码 → 填码 → 直接换 token。

   上游形态决定了这个差别 —— AutoClaw 国内版没有授权页、没有授权码回调
   （理由见 src-tauri/src/server/core/providers/autoclaw/login.rs 的模块头），
   硬塞进网页登录引擎只会让那边多出一堆「这条路没有窗口也没有 state」的分支。

   依赖 app.js 的顶层全局（经典 script 的顶层声明在全局可见）：$ / toast /
   __TAURI_INTERNALS__ 的 api_request。脚本顺序见 index.html：与 web-login.js
   同批加载，必须在 add-provider-forms.js 之前 —— 后者加载期就要 create()。 */

(() => {
  const $ = id => document.getElementById(id);

  /**
   * 建一个手机验证码登录控制器。
   *
   * config：
   *   provider   本控制器对应的 provider id（拼 DOM id 用）
   *   onSuccess  登录成功后的收尾（关弹窗、刷新列表、提示），
   *              由调用方传（与 beforeSubmit 一样，避免这里依赖 add-provider-forms
   *              的内部函数 afterAdd —— 那样两个文件的加载顺序会被写死成硬约束）
   *
   * 返回 { get deviceId }：deviceId 要不要暴露出去由调用方决定，当前不用
   * （它只在「发码」与「登录」两次点击之间有意义，全在这个闭包里）。
   */
  function create(config) {
    const prefix = config.provider;
    const hint = () => $(`${prefix}-sms-hint`);
    const phoneInput = () => $(`${prefix}-sms-phone`);
    const codeInput = () => $(`${prefix}-sms-code`);
    const nameInput = () => $(`${prefix}-sms-name`);

    /** 最近一次发码的 deviceId（发码成功后才写入，见下） */
    let deviceId = '';
    /** 发码与登录共用一把锁：两个按钮都打上游，不能并点 */
    let busy = false;

    const setHint = text => {
      const node = hint();
      if (node) node.textContent = text;
    };

    /**
     * 取可读的错误文案。
     *
     * ── 为什么不能直接写 `error.message`（真实踩过）────────────────
     * 壳侧命令签名是 `Result<Value, String>`，Tauri 把 `Err` 里的 String
     * **原样序列化**给 JS —— rejection 携带的是一个**字符串**而不是 Error 对象，
     * 于是 `error.message` 是 `undefined`，界面显示成「登录失败：undefined」。
     *
     * 正规路径是走桥接层（`window.workbuddyDesktop` 的 sendSmsCode / verifySmsLogin）
     * —— 那里统一用 asError 归一化过（见 src-tauri/src/bridge.rs 的说明）。
     * 这里再兜一层是因为**壳版本可能比界面旧**：新界面配上还没有这两个方法的旧壳时
     * 会退回直连 invoke，那条路的失败值就是裸字符串。
     */
    const describeError = error => {
      if (error instanceof Error && error.message) return error.message;
      const text = String(error ?? '').trim();
      return text || '未知错误';
    };

    /**
     * 调一次短信登录接口。
     *
     * 优先走桥接层（错误已归一成 Error）；桥接层没这两个方法时（旧版壳）退回
     * 直连 `api_request` —— 那时错误是裸字符串，由上面的 describeError 兜住。
     * 这个分层不是多余：界面与壳是两个独立产物，升级不同步时不该直接白屏报错。
     */
    const request = (path, payload) => {
      const bridge = window.workbuddyDesktop;
      if (path.endsWith('/send') && typeof bridge?.sendSmsCode === 'function') {
        return bridge.sendSmsCode(payload.phone);
      }
      if (path.endsWith('/verify') && typeof bridge?.verifySmsLogin === 'function') {
        return bridge.verifySmsLogin(payload);
      }
      const internals = window.__TAURI_INTERNALS__;
      if (!internals || typeof internals.invoke !== 'function') {
        return Promise.reject(new Error('桌面运行时不可用（Tauri 未初始化）'));
      }
      return internals.invoke('api_request', {
        request: { method: 'POST', path, body: payload },
      });
    };

    const phoneOf = () => phoneInput()?.value.trim() || '';
    const codeOf = () => codeInput()?.value.trim() || '';

    /** 手机号 / 验证码的本地校验（与后端同一条正则，先在界面上挡一次） */
    const PHONE_RE = /^1[2-9]\d{9}$/;
    const CODE_RE = /^\d{6}$/;

    /**
     * 发码冷却秒数。
     *
     * ── 60 这个数从哪来（不是猜的）────────────────────────────────
     * 上游**不返回**冷却时长：实测（2026-09-19）限频时返回
     * `{"code":630101,"msg":"抱歉,获取验证码过于频繁，请稍后再试。"}`，
     * 响应头里也没有 `Retry-After`（只有 date）—— 所以没有任何服务端秒数可用。
     *
     * 但官方桌面端的登录页把它写在客户端里（app.asar 的 `LoginView`）：
     * 发码成功后 `setRemaining(60)`，每秒递减，按钮显示 `${remaining}s`
     * 并在倒计时期间禁用。因此 60 是**官方口径**，不是我们编的。
     *
     * ── 为什么成功与限频都进入冷却 ───────────────────────────────
     * 官方只在成功时倒计时，但那样用户连点几下把额度打满后就只能一遍遍
     * 吃「过于频繁」—— 而那条路径本来就能靠倒计时避开。两条路都进冷却，
     * 界面在任何一次发码请求之后都不会立刻再打上游。
     */
    const RESEND_COOLDOWN_SECONDS = 60;

    /** 冷却到期的定时器；置 0 表示当前不在冷却中 */
    let cooldownTimer = 0;
    let cooldownLeft = 0;

    /** 把按钮恢复成「可点」的初始形态（冷却结束或被清理时调） */
    function resetSendButton() {
      const button = $(`${prefix}-sms-send`);
      if (!button) return;
      button.disabled = false;
      button.textContent = '获取验证码';
    }

    /**
     * 开始倒计时：按钮禁用并显示剩余秒数。
     *
     * 用 `setInterval` + 自减而不是记一个截止时间戳：这里只需要「大概每秒跳一下」
     * 的视觉反馈，秒数与服务端并无契约（它不给），因此不值得为漂移做补偿。
     * 每次都从 DOM 现取按钮、且先清掉旧定时器 —— 重入（连点、切换提供商后再发）
     * 不会留下两个叠加的 interval 把秒数减半。
     */
    function startCooldown(seconds = RESEND_COOLDOWN_SECONDS) {
      const button = $(`${prefix}-sms-send`);
      if (!button) return;
      window.clearInterval(cooldownTimer);
      cooldownLeft = seconds;
      const paint = () => {
        if (cooldownLeft <= 0) {
          window.clearInterval(cooldownTimer);
          cooldownTimer = 0;
          resetSendButton();
          return;
        }
        button.disabled = true;
        button.textContent = `${cooldownLeft}s 后重发`;
        cooldownLeft -= 1;
      };
      paint();
      cooldownTimer = window.setInterval(paint, 1000);
    }

    async function send() {
      if (busy || cooldownLeft > 0) return;
      const phone = phoneOf();
      if (!PHONE_RE.test(phone)) { window.wbApp.toast('请填写 11 位大陆手机号', 'err'); return; }
      const button = $(`${prefix}-sms-send`);
      busy = true;
      if (button) { button.disabled = true; button.textContent = '发送中…'; }
      setHint('');
      try {
        const data = await request('/api/session/login/sms/send', { phone });
        // ── deviceId 为什么要留住 ─────────────────────────────────
        // 上游把「刚发的这个码」绑在发码时的 device_id 上，登录必须带同一个。
        // 存在这个闭包里而不是每次现取，也不放进模块级状态 —— 它只在这两次
        // 点击之间有意义，放进模块级会在用户切换提供商后串味。
        deviceId = data?.deviceId || '';
        window.wbApp.toast('验证码已发送，请查看短信');
        setHint('验证码已发送。收到后填入下方并点「登录并添加」');
        // 成功也进冷却：官方口径（见 RESEND_COOLDOWN_SECONDS 的说明）
        startCooldown();
      } catch (error) {
        const reason = describeError(error);
        setHint(`发送失败：${reason}`);
        window.wbApp.toast(`发送失败：${reason}`, 'err');
        // ── 为什么失败也要倒计时 ───────────────────────────────────
        // 「过于频繁」（上游码 630101）正是最该冷却的一种失败：不打冷却的话
        // 用户会继续点，每次都稳定失败且可能延长服务端的限制窗口。
        // 其余失败（网络、手机号格式等）同样进冷却是**有意的保守选择** ——
        // 区分「该冷却」与「可立即重试」需要解析业务码，而前端的 code 判据
        // 目前只拿到文案；一律等 60 秒的代价远小于让用户打满限流。
        startCooldown();
      } finally {
        busy = false;
      }
    }

    async function submit() {
      if (busy) return;
      const phone = phoneOf();
      const code = codeOf();
      if (!PHONE_RE.test(phone)) { window.wbApp.toast('请填写 11 位大陆手机号', 'err'); return; }
      if (!CODE_RE.test(code)) { window.wbApp.toast('请填写 6 位数字验证码', 'err'); return; }
      const button = $(`${prefix}-sms-submit`);
      busy = true;
      if (button) { button.disabled = true; button.textContent = '登录中…'; }
      setHint('');
      try {
        const payload = { phone, code };
        // deviceId 缺省时不传：后端会现生成一个（上游接受「新设备直接登录」），
        // 传空串反而会覆盖掉那个兜底
        if (deviceId) payload.deviceId = deviceId;
        const name = nameInput()?.value.trim() || '';
        if (name) payload.name = name;
        const data = await request('/api/session/login/sms/verify', payload);
        // 成功后清掉验证码（手机号留着：连加第二个账号时省一次输入）
        const codeNode = codeInput();
        if (codeNode) codeNode.value = '';
        deviceId = '';
        await config.onSuccess?.(data);
      } catch (error) {
        const reason = describeError(error);
        setHint(`登录失败：${reason}`);
        window.wbApp.toast(`登录失败：${reason}`, 'err');
      } finally {
        busy = false;
        if (button) { button.disabled = false; button.textContent = '登录并添加'; }
      }
    }

    $(`${prefix}-sms-send`)?.addEventListener('click', send);
    $(`${prefix}-sms-submit`)?.addEventListener('click', submit);
  }

  window.wbSmsLogin = { create };
})();
