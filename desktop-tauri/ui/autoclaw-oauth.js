/* Agent2API · AutoClaw 国际版 OAuth 网页登录（Zai / Google）

   ── 为什么这一家要单独一个文件 ──────────────────────────────
   另外五家的网页登录都是「点按钮 → 壳开窗口 → 等回调」，界面只显示等待态
   （见 web-login.js）。AutoClaw 国际版多出一个**必须在浏览器里完成的强制
   风控验证码**（阿里云滑块），而且顺序是倒的：

     点按钮 → 跑验证码（本文件）→ 带验证码参数换授权地址 → 交壳开窗口

   因此它既不能塞进 web-login.js（那条链没有「前端先跑一段 SDK」这一步），
   也不能塞进 sms-login.js（那条链没有窗口与回调）。

   ── 「打开方式」与另外四家同款 ──────────────────────────────
   这一家的回调落在 z.ai 给官方客户端登记的那四个 loopback 端口上（网关登录时
   临时占一个，再转回自己的回调路由，见后端 providers/autoclaw/callback_server.rs
   的模块头），与「哪个浏览器」无关，因此内嵌窗口与系统浏览器都走得通 ——
   选择项由调用方在配置里给（`modes`，见 add-provider-forms.js 的 oauthLogin），
   本文件只通过 `config.mode()` 现读它、按它调整文案与取消入口：

     · 内嵌窗口：独立临时环境，连着加多个账号互不影响；取消靠关窗；
     · 系统浏览器：复用你已登录的 Zai / Google 账号（Google 在部分环境下会
       拒绝内嵌窗口），但**没有窗口可关** —— 因此这一条多一个「取消等待」
       按钮（见 create 里 `paintCancel` 的说明）。

   ── 验证码求解已抽到共用模块 ────────────────────────────────
   那一段（从官方客户端逐条移植的阿里云滑块求解器）现在在
   `ui/aliyun-captcha.js`，与 ZCode 的「周末套餐领取」共用 —— 那家同样要过
   这道风控，只是拿到 `verifyParam` 之后直接去领取，不像这里要接着换授权地址。
   本文件只调它的 `solve` / `cancel`，并在 `request` 回调里做「换授权地址」
   这半步（那是本家独有的协议）。求解器为什么能跑在主窗口里、以及
   「改主窗口 CSP 时必须放行 alicdn」那条约束，都记在那个文件的头部。

   依赖 app.js 的顶层全局（经典 script 的顶层声明在全局可见）：$ / toast /
   window.workbuddyDesktop；以及 `window.wbAliyunCaptcha`（上面那个共用模块，
   脚本顺序见 index.html：它必须排在本文件之前）。本文件必须在
   add-provider-forms.js 之前 —— 后者加载期就要 create()。 */

(() => {
  const $ = id => document.getElementById(id);

  /**
   * ── 验证码求解在共用模块里（本文件不再自带一份）─────────────
   * 这段代码原先是本文件的私有实现，现已抽到 `ui/aliyun-captcha.js`，
   * 与 ZCode 的「周末套餐领取」共用 —— 那家同样要过这道阿里云风控，
   * 但它拿到 `verifyParam` 之后是直接去领取，不像这里要接着换授权地址。
   * 两处只有「拿到串之后干什么」不同，求解过程完全一致（指纹要稳定、
   * 容器要清理、代际号要作废、取消要收尾、SDK 配置必须在加载前设好），
   * 因此那部分只留一份。
   *
   * 这里只取两件事：解出验证串（`solve`）与取消等待（`cancel`）。
   * 下面 `start()` 里的 `request` 回调就是两家分岔的那一半 ——
   * 它拿串去调 `/api/session/login/oauth/start` 换授权地址，
   * 并把地址借返回值带回上层。
   *
   * `CaptchaError` / `CaptchaCancelledError` 也从那边取：本文件多处 `catch`
   * 要按类型区分「用户取消」（不报红）与「真失败」（报错），
   * 所以这里解构出同名常量 —— 两个类只有一个实现，`instanceof` 才成立。
   */
  const {
    solve: solveCaptcha,
    cancel: cancelCaptcha,
    CaptchaError,
    CaptchaCancelledError,
  } = window.wbAliyunCaptcha;

  // ── 上层：把验证码与登录流程接起来 ──────────────────────────

  /**
   * 建一个 OAuth 登录控制器（当前只有 AutoClaw 国际版用）。
   *
   * config：
   *   provider   本控制器对应的 provider id（`autoclaw-intl`）
   *   buttonId / hintId   按钮与提示的 DOM id
   *   mode()     当前选中的打开方式（`embedded` / `external`）—— 做成函数而不是
   *              快照值：用户在点按钮之前可以来回切换，而发起时才需要它。
   *              缺省（不传或返回空）按内嵌窗口处理。
   *   hint()     当前打开方式对应的**空闲提示**：流程没在跑时那一行该显示什么。
   *              流程中它被阶段文案（「请完成验证」「已打开登录页…」）覆盖，
   *              结束后恢复成它 —— 与 web-login.js 的 texts() 同一分工。
   *   cancelId   「取消等待」按钮的 DOM id（可选）。见下面 `paintCancel` 的说明。
   *   onSuccess(result)   登录成功后的收尾（关弹窗、刷新列表、提示），
   *                       由调用方传 —— 与 sms-login.js 同一分工，
   *                       避免这里依赖 add-provider-forms 的内部函数
   */
  function create(config) {
    const prefix = config.provider;
    const hint = () => $(config.hintId || `${prefix}-oauth-hint`);
    /** 两个 OAuth 变体各一个按钮（Zai / Google） */
    const vendorButtons = () => ({
      zai: $(`${prefix}-oauth-zai`),
      google: $(`${prefix}-oauth-google`),
    });
    const cancelButton = () => (config.cancelId ? $(config.cancelId) : null);

    /** 当前的打开方式（取不到一律按内嵌窗口 —— 与壳侧缺省一致） */
    const modeOf = () => (config.mode?.() === 'external' ? 'external' : 'embedded');
    /** 空闲时该显示的那行提示（打开方式对应的文案，由调用方给） */
    const idleHint = () => config.hint?.() || '';

    /**
     * 两个按钮的初始文案（忙碌时被换成转圈文案，复位时要还原）。
     *
     * 在 `create()` 时抓一次而不是复位时现算：`create` 由 add-provider-forms
     * 在加载期调用，那时 DOM 刚拼好、文案还是模板里的原文 —— 抓下来的就是
     * 「标准形态」。复位时现算的话，第二次忙碌会把第一次的转圈文案当成标准
     * 形态存下来，按钮文字会永久退化成「等待验证码…」。
     */
    const initialLabels = Object.fromEntries(
      Object.entries(vendorButtons()).map(([key, node]) => [key, node ? node.textContent : '']),
    );

    let busy = false;
    /** 最近一次发起的 state（仅用于日志与「有没有发起过」的判断） */
    let activeState = '';
    /**
     * 流程代际号：cancel() 把它 +1 作废当前一轮，旧 start() 里各处异步落定
     * （迟到的验证码结果、壳侧登录返回）对照它发现过期就**不再碰 UI**。
     *
     * 没有它的话，cancel() 里「立即复位界面」与旧 start() 的 finally 复位会
     * 互相踩：用户关弹窗后马上重开再点按钮，旧一轮此时才从壳侧返回 canceled，
     * 它的 finally 会把新一轮刚画上去的「请完成验证…」复位掉，busy 也被清零，
     * 第三次点击就放进来了。
     */
    let flowGeneration = 0;

    const setHint = text => {
      const node = hint();
      if (node) node.textContent = text;
    };

    /** 取可读的错误文案（与 sms-login.js 同一手法：桥接层已归一成 Error） */
    const describeError = error => {
      if (error instanceof Error && error.message) return error.message;
      const text = String(error ?? '').trim();
      return text || '未知错误';
    };

    /**
     * 按钮的忙碌态：两个变体一起禁用（同一时刻只能有一次登录 —— 壳侧也
     * 只允许一个登录流程，并点第二个必然报错）。
     *
     * 不在这里改 `busy`：那个标志只该由 `start()` 自己管（它在最外层
     * 判断重入），把它塞进一个「改样式」的函数里会让两处状态可能不一致。
     */
    function paintBusy(value, label) {
      const nodes = vendorButtons();
      for (const [key, node] of Object.entries(nodes)) {
        if (!node) continue;
        node.disabled = value;
        node.textContent = value ? label : (initialLabels[key] || node.textContent);
      }
    }

    /**
     * 「取消」按钮的显隐。
     *
     * ── 为什么全程显示（曾经只在「拿到授权地址之后」）──────────
     * 这条链在拿到授权地址之前还有一段**滑块验证**，而那段恰恰是用户最想
     * 反悔的时候：滑块面板右上角的关闭 SDK 不回调（我们无从感知），此前
     * 又没有显式的取消入口，界面就停在「请完成验证…」干等 120 秒超时。
     * 因此按钮从发起那一刻就挂出来 —— 验证码阶段点它 = 作废滑块等待；
     * 拿到地址后的等待登录阶段点它 = 撤掉壳侧那一轮（两种语义 cancel()
     * 里按进度自动分流，按钮本身不需要变）。
     *
     * 系统浏览器模式下没有窗口可关，这个按钮更是唯一的取消出口。
     */
    function paintCancel(visible) {
      const node = cancelButton();
      if (node) node.style.display = visible ? '' : 'none';
    }

    /**
     * 发起一次 OAuth 登录（某个变体）。
     *
     * 顺序（与另外五家相反，理由见文件头）：
     *   ① 取风控配置（缓存一次即可，SDK 自己也会复用实例）
     *   ② 跑验证码 → 拿到不透明验证串
     *   ③ 用验证串换授权地址（网关带上它去打上游）
     *   ④ 把 {state, authUrl} 交给壳去开窗口并等待
     */
    async function start(vendor) {
      if (busy) return;
      const bridge = window.workbuddyDesktop;
      if (!bridge?.startAutoclawOauth || !bridge?.getAutoclawOauthCaptchaConfig
        || !bridge?.startAutoclawOauthLogin) {
        window.wbApp.toast('当前壳版本不支持 AutoClaw 网页登录，请更新应用', 'err');
        return;
      }
      busy = true;
      const flow = ++flowGeneration;
      paintBusy(true, '准备验证…');
      setHint('');
      // 取消按钮从发起那一刻就挂着（理由见 paintCancel 的说明）
      paintCancel(true);
      try {
        // ① 风控配置。`enabled: false` = 这一家没有这条登录方式（国内版就是这个值）
        const captchaConfig = await bridge.getAutoclawOauthCaptchaConfig(prefix);
        if (!captchaConfig?.enabled) {
          throw new CaptchaError('这一家当前不支持网页登录，请改用填写凭证');
        }
        if (!captchaConfig.prefix || !captchaConfig.sceneId) {
          throw new CaptchaError('风控验证配置不完整，请稍后重试');
        }
        // ②③ 跑验证码 → 拿验证串 → 换授权地址。**到此为止，不再多走一步**：
        // `request` 一返回 SDK 就收起滑块（bizResult=true），120 秒的验证码超时
        // 也只包着「拖滑块 + 换地址」。等登录动辄几分钟，塞在这里面会被验证码
        // 超时误杀 —— 前端报「验证码校验超时」复位，壳与网关却还在等回调，
        // 用户随后真完成登录时账号加了、界面却毫无反应（三方状态错乱）。
        paintBusy(true, '请完成验证…');
        setHint('请在弹出的滑块中完成验证（官方要求的风控步骤）');
        const started = await solveCaptcha(
          {
            region: captchaConfig.region || 'ga',
            prefix: captchaConfig.prefix,
            sceneId: captchaConfig.sceneId,
          },
          async captchaVerifyParam => {
            const answer = await bridge.startAutoclawOauth(prefix, vendor, captchaVerifyParam);
            const authUrl = String(answer?.authUrl || '').trim();
            if (!authUrl) {
              throw new CaptchaError('未能获取授权地址，请重试');
            }
            // 这一轮有降级时后端会给一句话（如「回调端口被官方客户端占着」）：
            // 在这里提示，别等到用户按「系统浏览器」走完一遍才发现收不到回调
            const warning = String(answer?.warning || '').trim();
            if (warning) window.wbApp.toast(warning, 'warn');
            // bizResult 的语义 = 上游给没给授权地址（见文件头）——给了就算过，
            // SDK 收起滑块；地址与 state 由返回值带给 start()（started.answer）
            return { captchaResult: true, bizResult: true, answer };
          },
        );
        // 等待期间可能已被取消（点取消 / 关弹窗）：迟到的结果不得再碰 UI
        if (flow !== flowGeneration) return;
        const authUrl = String(started?.answer?.authUrl || '').trim();
        activeState = String(started?.answer?.state || '');
        // 打开方式在这里**现读**（用户在跑验证码期间也可能切了那一级），
        // 并且文案随它分叉：系统浏览器下没有「窗口」可关，说「窗口中」
        // 会让用户去找一个不存在的窗口
        const mode = modeOf();
        paintBusy(true, '等待登录完成…');
        setHint(mode === 'external'
          ? '已用系统默认浏览器打开登录页，请在浏览器中完成登录…'
          : '已打开官方登录页，请在窗口中完成登录…');
        // ④ 交给壳开窗口 / 打开浏览器（阻塞到登录完成/取消/超时）。壳侧自带
        // 5 分钟兜底，不受上面 120 秒验证码超时的约束。
        const outcome = await bridge.startAutoclawOauthLogin(activeState, authUrl, mode);
        if (flow !== flowGeneration) return;
        activeState = '';
        if (!outcome?.ok) {
          // 用户取消（关窗 / 点取消 / 关弹窗）：不报错，只提示
          window.wbApp.toast('已取消登录等待');
          return;
        }
        await config.onSuccess?.(outcome);
      } catch (error) {
        // 已被作废的轮次：UI 由 cancel() 复位过，这里什么都不做（含不报错）
        if (flow !== flowGeneration) return;
        if (error instanceof CaptchaCancelledError) {
          window.wbApp.toast('已取消验证码');
          return;
        }
        // 换地址失败（风控没过 / 上游拒绝）：滑块面板还停在「验证中」，作废实例
        // 让它收起 —— 否则面板与报错同时在场，用户不知道该信哪一个
        invalidateInitialization();
        const reason = describeError(error);
        setHint(`登录失败：${reason}`);
        window.wbApp.toast(`登录失败：${reason}`, 'err');
      } finally {
        // 只复位「仍是当前这一轮」的流程；被 cancel 作废的轮次由 cancel 自己
        // 复位，迟到的落定不得覆盖新一轮刚画上去的状态
        if (flow === flowGeneration) {
          activeState = '';
          busy = false;
          paintBusy(false, '');
          paintCancel(false);
          // 恢复空闲提示（不是清空）：那一行同时承担「打开方式是什么、会怎么打开」
          // 的说明职责，清掉之后用户切回来看到的是一片空白
          setHint(idleHint());
        }
      }
    }

    const nodes = vendorButtons();
    if (nodes.zai) nodes.zai.addEventListener('click', () => start('zai'));
    if (nodes.google) nodes.google.addEventListener('click', () => start('google'));

    const controller = {
      provider: prefix,
      start,
      /**
       * 提示文案随「打开方式」变化时调用（调用方在分段控件切换后调）。
       *
       * 忙碌中不覆盖：那时 hint 显示的是阶段文案（「请完成验证」「已打开…」），
       * 覆盖成打开方式的说明会把当前进度抹掉。与 web-login.js 的 syncTexts
       * 同一口径（那边也是「等待中不覆盖」）。
       */
      syncTexts() {
        if (!busy) setHint(idleHint());
      },
      /**
       * 取消（「取消」按钮与弹窗关闭时都由它）。
       *
       * 三件事：
       *   1. **先作废流程代际并立即复位 UI** —— 旧 start() 此刻多半还挂在
       *      `startAutoclawOauthLogin` 上（壳侧要等 IPC 往返才返回 canceled），
       *      不能指望它的 finally；不复位的话，关掉弹窗重开看到的还是转圈按钮。
       *   2. 作废本地验证码等待（用户可能正拖滑块）。
       *   3. 通知壳撤掉那一轮登录 —— 否则壳侧的等待循环要空转到 5 分钟超时。
       *
       * `cancelLogin` 是按「当前活动登录」取消的（壳侧只记一个），因此不需要
       * 把 state 传过去 —— 这也正是 `activeState` 只用于「有没有发起过」的
       * 判断、不参与取消的原因。没发起过登录任务就不打这次 IPC（壳侧会早退，
       * 但白打一次没有意义）。
       */
      cancel() {
        flowGeneration += 1;
        const cancelledCaptcha = cancelCaptcha();
        const wasWaiting = Boolean(activeState);
        activeState = '';
        busy = false;
        paintBusy(false, '');
        paintCancel(false);
        setHint(idleHint());
        if (wasWaiting || cancelledCaptcha) {
          window.workbuddyDesktop?.cancelLogin?.().catch(() => {});
        }
      },
    };
    cancelButton()?.addEventListener('click', () => controller.cancel());
    controllers.push(controller);
    return controller;
  }

  /** 所有已登记的控制器（按 create 顺序；当前只有国际版一份） */
  const controllers = [];

  /**
   * 放弃等待中的登录（弹窗关闭时由 add-account.js 调）。
   *
   * ── 为什么在模块级再包一层（控制器上已经有 cancel 了）────────
   * 调用方（`closeModal`）不知道「哪一家在等待」—— 与 web-login 的
   * `cancelIfActive('')` 同一处境，那边也是模块级函数遍历控制器。
   * 让调用方去记「现在该取消哪个控制器」等于把一份状态复制到弹窗代码里，
   * 而那份状态只在启动时写、关闭时读，中间任何一次 provider 切换都可能让它
   * 过期。这里遍历一遍最省事，也没有第二个消费方。
   *
   * 没有发起过时它什么都不做（每个控制器的 cancel 自己判）。
   */
  function cancelAll() {
    for (const controller of controllers) controller.cancel();
  }

  window.wbAutoclawOauth = { create, cancel: cancelAll };
})();
