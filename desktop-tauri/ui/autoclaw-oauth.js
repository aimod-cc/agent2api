/* Agent2API · AutoClaw 国际版 OAuth 网页登录（Zai / Google）

   ── 为什么这一家要单独一个文件 ──────────────────────────────
   另外五家的网页登录都是「点按钮 → 壳开窗口 → 等回调」，界面只显示等待态
   （见 web-login.js）。AutoClaw 国际版多出一个**必须在浏览器里完成的强制
   风控验证码**（阿里云滑块），而且顺序是倒的：

     点按钮 → 跑验证码（本文件）→ 带验证码参数换授权地址 → 交壳开窗口

   因此它既不能塞进 web-login.js（那条链没有「前端先跑一段 SDK」这一步），
   也不能塞进 sms-login.js（那条链没有窗口与回调）。

   ── 「打开方式」与另外四家同款 ──────────────────────────────
   这一家的回调落在本机网关的 loopback 端口（见后端 oauth.rs 的模块头），
   与「哪个浏览器」无关，因此内嵌窗口与系统浏览器都走得通 —— 选择项由调用方
   在配置里给（`modes`，见 add-provider-forms.js 的 oauthLogin），本文件只
   通过 `config.mode()` 现读它、按它调整文案与取消入口：

     · 内嵌窗口：独立临时环境，连着加多个账号互不影响；取消靠关窗；
     · 系统浏览器：复用你已登录的 Zai / Google 账号（Google 在部分环境下会
       拒绝内嵌窗口），但**没有窗口可关** —— 因此这一条多一个「取消等待」
       按钮（见 create 里 `paintCancel` 的说明）。

   ── 验证码这一段是从官方客户端逐条移植的 ────────────────────
   来源：AutoClaw 桌面端 `app.asar` 的渲染层（`chatStore-*.js` 里
   `requestAliyunPopupCaptcha` / `initializeAliyunCaptcha` /
   `captchaVerifyCallback` 三个函数），本文件保持它的结构与常量，
   **只做三处删减**（都是客户端专属的埋点与 i18n，与登录无关）：
     1. 去掉 `traceCaptchaEvent` 埋点（我们不上报火山）；
     2. 去掉 i18n 查表（文案直接写中文）；
     3. 去掉数美（shumei）那条备选 —— 上游实测只发 aliyun
        （`captcha_supplier: "aliyun"`），留着一条永远走不到的分支只会
        让「验证码出问题时该看哪段代码」变模糊。

   ── SDK 是浏览器端 JS，为什么能原样跑在主窗口里 ─────────────
   SDK 从 `o.alicdn.com` 加载（`AliyunCaptcha.js`）。Tauri 主窗口的 CSP 是
   `null`（见 tauri.conf.json），没有 `script-src` 限制，因此这个外域脚本
   能正常加载与执行 —— 这正是「直接搬过来」可行的前提。若哪天给主窗口加了
   CSP，必须在 `script-src` 里放行 `https://o.alicdn.com` 与
   `https://*.alicdn.com`（SDK 自己还会再拉资源），否则验证码会静默加载失败。

   依赖 app.js 的顶层全局（经典 script 的顶层声明在全局可见）：$ / toast /
   window.workbuddyDesktop。脚本顺序见 index.html：必须在 add-provider-forms.js
   之前 —— 后者加载期就要 create()。 */

(() => {
  const $ = id => document.getElementById(id);

  // ── 常量：逐字照抄客户端（改任何一个都要重新对照一遍上游）────────

  const ALIYUN_CAPTCHA_SCRIPT_URL =
    'https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js';
  const SCRIPT_ID = 'aliyun-captcha-sdk';
  /** SDK 挂载点（滑块面板的容器） */
  const ELEMENT_ID = 'aliyun-captcha-element';
  /** 触发按钮：SDK 要求传一个 button 选择器，点击它才弹出滑块。
   *  客户端把它做成 1×1 透明不可见，由代码 `button.click()` 触发 —— 这里照做。 */
  const BUTTON_ID = 'aliyun-captcha-trigger';

  const SCRIPT_LOAD_TIMEOUT_MS = 40000;
  const INIT_TIMEOUT_MS = 40000;
  const VERIFY_TIMEOUT_MS = 120000;
  /** 初始化后至少等 2.1 秒再点按钮（客户端实测：SDK 预热没完成时点击无效） */
  const MINIMUM_WARMUP_MS = 2100;
  /** 初始化结果最多复用 19 分钟（超过则重建，避免实例内部状态过期） */
  const INITIALIZATION_MAX_AGE_MS = 19 * 60000;

  // ── 模块级状态（客户端也是模块级的：一次只允许一个验证码流程）────

  let scriptLoadPromise = null;
  let initializationPromise = null;
  /** 初始化键（region:prefix:sceneId:language）—— 配置变了就重建实例 */
  let initializationKey = '';
  /** 代际号：异步流程回来时用它判断「这一轮是否已被作废」 */
  let initializationGeneration = 0;
  let initializedAt = 0;
  let captchaInstance = null;
  /** 正在等验证码结果的那一次请求 */
  let pendingVerification = null;

  const sleep = ms => new Promise(resolve => window.setTimeout(resolve, ms));

  /**
   * 语言映射（客户端的 `resolveAliyunCaptchaLanguage`）。
   *
   * 阿里云只认这几个短码，传别的会被它当成不认识而回落到英文 ——
   * 因此必须在这里归一，不能直接把 `zh-CN` 递给 SDK。
   */
  function resolveAliyunCaptchaLanguage(language) {
    const normalized = String(language || '').trim().replace(/_/g, '-').toLowerCase();
    if (normalized === 'zh-tw' || normalized.startsWith('zh-hant')) return 'tw';
    if (normalized.startsWith('zh')) return 'cn';
    if (normalized.startsWith('ar')) return 'ar';
    if (normalized.startsWith('de')) return 'de';
    if (normalized.startsWith('es')) return 'es';
    if (normalized.startsWith('fr')) return 'fr';
    if (normalized.startsWith('id') || normalized.startsWith('in')) return 'in';
    if (normalized.startsWith('it')) return 'it';
    if (normalized.startsWith('ja')) return 'ja';
    if (normalized.startsWith('ko')) return 'ko';
    if (normalized.startsWith('pt')) return 'pt';
    if (normalized.startsWith('ru')) return 'ru';
    if (normalized.startsWith('th')) return 'th';
    if (normalized.startsWith('tr')) return 'tr';
    if (normalized.startsWith('vi')) return 'vi';
    return 'en';
  }

  /** 界面当前语言（与网关设置页同一来源；取不到按英文，阿里云能兜住） */
  const currentLanguage = () => document.documentElement.lang || 'zh-CN';

  function getInitAliyunCaptcha() {
    const value = window.initAliyunCaptcha;
    return typeof value === 'function' ? value : null;
  }

  /** 验证码流程失败时抛的错误（带一句人话，界面直接展示） */
  class CaptchaError extends Error {
    constructor(message) {
      super(message);
      this.name = 'CaptchaError';
    }
  }

  /** 用户主动取消（点「取消」或关弹窗）—— 与「失败」分开，界面不报红 */
  class CaptchaCancelledError extends CaptchaError {
    constructor() {
      super('已取消验证码');
      this.name = 'CaptchaCancelledError';
    }
  }

  function removeCaptchaElements() {
    document.getElementById(ELEMENT_ID)?.remove();
    document.getElementById(BUTTON_ID)?.remove();
  }

  /** 作废当前实例（配置变了 / 验证码用完了要重建时调） */
  function invalidateInitialization() {
    const instance = captchaInstance;
    initializationGeneration += 1;
    captchaInstance = null;
    initializationPromise = null;
    initializedAt = 0;
    removeCaptchaElements();
    // destroy 可能不存在（SDK 老版本），调用失败也不影响我们自己的状态
    try { instance?.destroy?.(); } catch { /* 忽略：实例已不可用 */ }
  }

  /**
   * 备好滑块容器与触发按钮（客户端的 `ensureCaptchaElements`）。
   *
   * 两个元素都由 SDK 按 id 找：`element` 是滑块面板的落点，`button` 是
   * 「点它才弹」的触发器。客户端把按钮做成 1×1 透明且 `pointer-events: none`
   * —— 这样用户看不到也点不到它，触发完全由代码控制（我们只在准备好之后
   * 主动 `button.click()` 一次），不会出现「用户自己点出两个滑块」。
   */
  function ensureCaptchaElements() {
    let element = document.getElementById(ELEMENT_ID);
    if (!element) {
      element = document.createElement('div');
      element.id = ELEMENT_ID;
      element.style.position = 'relative';
      element.style.zIndex = '2147483000';
      document.body.appendChild(element);
    }
    let button = document.getElementById(BUTTON_ID);
    if (!button) {
      button = document.createElement('button');
      button.id = BUTTON_ID;
      button.type = 'button';
      button.tabIndex = -1;
      button.setAttribute('aria-hidden', 'true');
      button.style.position = 'fixed';
      button.style.width = '1px';
      button.style.height = '1px';
      button.style.opacity = '0';
      button.style.pointerEvents = 'none';
      button.style.overflow = 'hidden';
      document.body.appendChild(button);
    }
    return button;
  }

  /**
   * 加载 SDK 脚本（客户端的 `loadAliyunCaptchaScript`）。
   *
   * `window.AliyunCaptchaConfig` 必须在脚本加载**之前**设好 —— SDK 读它决定
   * 打哪个阿里云站点（`region` / `prefix`）。设晚了 SDK 会用一个默认站点，
   * 表现是「验证码弹出来但一直转圈」。
   */
  function loadAliyunCaptchaScript(config) {
    window.AliyunCaptchaConfig = { region: config.region, prefix: config.prefix };
    if (getInitAliyunCaptcha()) return Promise.resolve();
    if (scriptLoadPromise) return scriptLoadPromise;
    scriptLoadPromise = new Promise((resolve, reject) => {
      const existing = document.getElementById(SCRIPT_ID);
      const script = existing || document.createElement('script');
      let timer = 0;
      const cleanup = () => {
        window.clearTimeout(timer);
        script.removeEventListener('load', onLoad);
        script.removeEventListener('error', onError);
      };
      const fail = error => {
        cleanup();
        scriptLoadPromise = null;
        script.remove();
        reject(error);
      };
      const onLoad = () => {
        cleanup();
        if (getInitAliyunCaptcha()) resolve();
        else fail(new CaptchaError('验证码组件加载异常，请重试'));
      };
      const onError = () => fail(new CaptchaError('验证码组件加载失败，请检查网络后重试'));
      timer = window.setTimeout(
        () => fail(new CaptchaError('验证码组件加载超时，请检查网络后重试')),
        SCRIPT_LOAD_TIMEOUT_MS,
      );
      script.addEventListener('load', onLoad);
      script.addEventListener('error', onError);
      if (!existing) {
        script.id = SCRIPT_ID;
        script.async = true;
        script.src = ALIYUN_CAPTCHA_SCRIPT_URL;
        document.head.appendChild(script);
      }
    });
    return scriptLoadPromise;
  }

  /** 把一次等待落定（客户端 `settlePendingVerification` 的简化版） */
  function settlePending(pending, outcome) {
    if (pendingVerification !== pending || pending.settled) return false;
    pending.settled = true;
    window.clearTimeout(pending.timer);
    if (outcome.error) pending.reject(outcome.error);
    else pending.resolve(outcome.value);
    return true;
  }

  /**
   * SDK 回调：拿到不透明验证串 → 交给上层去换授权地址（客户端的 `captchaVerifyCallback`）。
   *
   * 返回值必须是 `{captchaResult, bizResult}`：SDK 据此决定「这一关过了没有」。
   * `captchaResult` 是**验证码本身**是否通过（阿里云那侧），`bizResult` 是
   * **我们的业务**是否接受它（这里 = 上游给不给授权地址）。两个都为 true
   * SDK 才收起滑块；否则它会让用户重试。
   */
  async function captchaVerifyCallback(generation, captchaVerifyParam) {
    const pending = pendingVerification;
    if (!pending || pending.generation !== generation) {
      return { captchaResult: false, bizResult: false };
    }
    if (typeof captchaVerifyParam !== 'string' || captchaVerifyParam.length === 0) {
      settlePending(pending, {
        error: new CaptchaError('验证码校验失败，请重试'),
      });
      return { captchaResult: false, bizResult: false };
    }
    try {
      // `pending.request` 是上层注入的「用这个串去换授权地址」
      const result = await pending.request(captchaVerifyParam);
      if (pendingVerification !== pending || pending.settled) {
        return {
          captchaResult: Boolean(result?.captchaResult),
          bizResult: Boolean(result?.bizResult),
        };
      }
      settlePending(pending, { value: result });
      return {
        captchaResult: Boolean(result?.captchaResult),
        bizResult: Boolean(result?.bizResult),
      };
    } catch (error) {
      settlePending(pending, { error });
      return { captchaResult: false, bizResult: false };
    }
  }

  /**
   * 初始化 SDK 实例（客户端的 `initializeAliyunCaptcha`）。
   *
   * 同一个配置只初始化一次，19 分钟内复用；配置变了或过期就重建
   * （见 `invalidateInitialization`）。`getInstance` 回调是「SDK 准备好了」
   * 的信号 —— 它不给这个回调我们就不知道实例什么时候可用。
   */
  function initializeAliyunCaptcha(config) {
    const language = resolveAliyunCaptchaLanguage(currentLanguage());
    const key = `${config.region}:${config.prefix}:${config.sceneId}:${language}`;
    if (initializationPromise && initializationKey === key) {
      const inFlight = initializedAt === 0;
      const fresh = Date.now() - initializedAt < INITIALIZATION_MAX_AGE_MS;
      if (inFlight || fresh) return initializationPromise;
      invalidateInitialization();
    }
    if (initializationKey && initializationKey !== key) invalidateInitialization();
    initializationKey = key;
    const generation = ++initializationGeneration;
    initializationPromise = (async () => {
      await loadAliyunCaptchaScript(config);
      ensureCaptchaElements();
      const initAliyunCaptcha = getInitAliyunCaptcha();
      if (!initAliyunCaptcha) throw new CaptchaError('验证码组件不可用，请重试');
      await new Promise((resolve, reject) => {
        let settled = false;
        let boundInstance = null;
        const timer = window.setTimeout(() => {
          if (settled) return;
          settled = true;
          reject(new CaptchaError('验证码组件初始化超时，请重试'));
        }, INIT_TIMEOUT_MS);
        const settle = callback => {
          if (settled) return false;
          settled = true;
          window.clearTimeout(timer);
          callback();
          return true;
        };
        try {
          initAliyunCaptcha({
            SceneId: config.sceneId,
            mode: 'popup',
            element: `#${ELEMENT_ID}`,
            button: `#${BUTTON_ID}`,
            captchaVerifyCallback: param => captchaVerifyCallback(generation, param),
            // 业务结果回调：上游对「验证码过了但业务没通过」的反馈走这里。
            // 客户端也是空实现（它只关心 captchaVerifyCallback 的返回值）。
            onBizResultCallback: () => {},
            getInstance: instance => {
              boundInstance = instance;
              if (generation !== initializationGeneration) {
                try { instance.destroy?.(); } catch { /* 忽略 */ }
                return;
              }
              if (!settle(() => {
                captchaInstance = instance;
                initializedAt = Date.now();
                resolve();
              })) {
                try { instance.destroy?.(); } catch { /* 忽略 */ }
              }
            },
            slideStyle: { width: 360, height: 40 },
            language,
            onError: error => {
              const pending = pendingVerification;
              if (pending && pending.generation === generation && pending.instance === boundInstance) {
                settlePending(pending, {
                  error: new CaptchaError(
                    (error && error.message) || '验证码校验失败，请重试',
                  ),
                });
                return;
              }
              if (settled) {
                if (generation === initializationGeneration
                  && (!boundInstance || captchaInstance === boundInstance)) {
                  invalidateInitialization();
                }
                return;
              }
              settle(() => reject(new CaptchaError(
                (error && error.message) || '验证码组件不可用，请重试',
              )));
            },
          });
        } catch (error) {
          settle(() => reject(new CaptchaError(
            (error && error.message) || '验证码组件不可用，请重试',
          )));
        }
      });
      if (!captchaInstance) throw new CaptchaError('验证码组件不可用，请重试');
    })();
    return initializationPromise;
  }

  /** 等 SDK 预热完成（客户端实测：太快点击弹不出滑块） */
  async function waitForWarmup() {
    const remaining = MINIMUM_WARMUP_MS - (Date.now() - initializedAt);
    if (remaining > 0) await sleep(remaining);
  }

  /**
   * 走一次完整验证码：初始化 → 预热 → 弹滑块 → 用户拖 → 拿串换授权地址。
   *
   * `request` 由调用方注入：`(captchaVerifyParam) => Promise<{captchaResult, bizResult}>`
   * —— 它是「用这个串去换授权地址」的那一步（调网关
   * `/api/session/login/oauth/start`）。把它做成参数而不是写死在这里，
   * 是为了让本文件只管验证码、不认识登录协议的字段名。
   */
  async function requestAliyunPopupCaptcha(config, request) {
    await initializeAliyunCaptcha(config);
    await waitForWarmup();
    const button = ensureCaptchaElements();
    const instance = captchaInstance;
    if (!instance) throw new CaptchaError('验证码组件不可用，请重试');
    const generation = initializationGeneration;
    return new Promise((resolve, reject) => {
      const pending = {
        generation,
        instance,
        request,
        resolve,
        reject,
        timer: 0,
        settled: false,
      };
      pending.timer = window.setTimeout(() => {
        settlePending(pending, {
          error: new CaptchaError('验证码校验超时，请重试'),
        });
      }, VERIFY_TIMEOUT_MS);
      pendingVerification = pending;
      button.click();
    });
  }

  /** 用户取消：把等待中的那次落定成「已取消」并作废实例 */
  function cancelAliyunPopupCaptcha() {
    const pending = pendingVerification;
    if (!pending) return false;
    const cancelled = settlePending(pending, { error: new CaptchaCancelledError() });
    if (cancelled) invalidateInitialization();
    return cancelled;
  }

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
        const started = await requestAliyunPopupCaptcha(
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
        const cancelledCaptcha = cancelAliyunPopupCaptcha();
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
