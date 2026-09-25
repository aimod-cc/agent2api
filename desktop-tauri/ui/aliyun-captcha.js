/* Agent2API · 阿里云无痕验证（滑块）求解器 —— **多调用方共用**

   ── 为什么单独成文件（原来在 autoclaw-oauth.js 里）───────────
   这段代码最早是 AutoClaw 国际版 OAuth 登录的私有实现：那家的网页登录前面
   强制多一道风控验证码，必须先拖完滑块拿到 `verifyParam` 才能换授权地址。

   ZCode 的「周末套餐领取」同样要过这道验证码（上游的领取接口强制要求
   `X-Aliyun-Captcha-Verify-Param`），而它**与 AutoClaw 的用法只有一半相同**：

     · AutoClaw：解验证码 → 用 verifyParam 换授权地址 → 开窗口等回调
     · ZCode：   解验证码 → 用 verifyParam 直接去领取（到此为止）

   留在原处的话，第二家只能照抄一份。这段代码的坑都在细节里（指纹要稳定、
   容器要清理、代际号要作废、取消要收尾、SDK 的配置必须在加载前设好），
   抄一份就是两份会各自漂移的坑。因此抽到这里，两家都调它。

   ── 调用契约：`request` 由调用方注入，本模块不认识任何业务字段 ──
   `solve(config, request)` 里的 `request(verifyParam)` 是**调用方的事**：
   它拿验证串去干自己那件事，返回 `{captchaResult, bizResult}` 告诉 SDK
   这一关过没过（两个都为 true SDK 才收起滑块）。
   本模块因此不知道「授权地址」「套餐」这些概念 —— 加第三家时不用改这里。

   ── 这段代码的出身：从 AutoClaw 客户端逐条移植 ──────────────
   来源：AutoClaw 桌面端 `app.asar` 的渲染层（`chatStore-*.js` 里
   `requestAliyunPopupCaptcha` / `initializeAliyunCaptcha` /
   `captchaVerifyCallback` 三个函数），保持它的结构与常量，
   **只做三处删减**（都是客户端专属的埋点与 i18n，与业务无关）：
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

   ── 一次只允许一个验证码流程 ────────────────────────────────
   模块级状态（SDK 客户端也是模块级的）：两个调用方同时发起会互相踩。
   界面上这两条链不会同时开（添加账号弹窗与领取按钮互斥），因此不做排队，
   而是靠 `solve` 里的代际号让**后发起的那一轮**作废前一轮（迟到的回调
   落下时发现代际不符就丢弃）。

   依赖 app.js 的顶层全局：无（只读 documentElement.lang）。
   脚本顺序见 index.html：必须在 autoclaw-oauth.js 与 zcode-claim.js 之前。 */

(() => {
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
   * SDK 回调：拿到不透明验证串 → 交给调用方去干它那件事
   * （客户端的 `captchaVerifyCallback`）。
   *
   * 返回值必须是 `{captchaResult, bizResult}`：SDK 据此决定「这一关过了没有」。
   * `captchaResult` 是**验证码本身**是否通过（阿里云那侧），`bizResult` 是
   * **调用方的业务**是否接受它。两个都为 true SDK 才收起滑块；
   * 否则它会让用户重试。
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
      // `pending.request` 是调用方注入的「用这个串去干那件事」
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
            // 业务结果回调：调用方对「验证码过了但业务没通过」的反馈走这里。
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
   * 走一次完整验证码：初始化 → 预热 → 弹滑块 → 用户拖 → 拿串交给调用方。
   *
   * `config`：`{ region, prefix, sceneId }`（三样都来自上游下发的风控配置，
   * 调用方各自去取 —— AutoClaw 走 `/api/session/login/oauth/captcha-config`，
   * ZCode 领取走 `/api/accounts/{id}/zcode-claim/captcha-config`）。
   *
   * `request(verifyParam)`：调用方注入的「用这个串去干那件事」，
   * 返回 `{captchaResult, bizResult}`。它 resolve 出来的值会**原样**成为
   * `solve()` 的返回值 —— 调用方可以借它把「业务结果」带回来
   * （AutoClaw 就是靠这个把授权地址传回上层）。
   *
   * ── 超时只包着「拖滑块 + 那一次请求」────────────────────────
   * 120 秒的 `VERIFY_TIMEOUT_MS` 覆盖到这里为止。调用方若在 `request` 里
   * 做长流程（等登录回调之类），会被这个超时误杀 —— AutoClaw 的注释里
   * 记着那个坑：前端报「验证码超时」复位，壳与网关却还在等，用户随后真完成
   * 时账号加了、界面毫无反应。长流程要放在 `solve()` **返回之后**。
   */
  async function solve(config, request) {
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
  function cancel() {
    const pending = pendingVerification;
    if (!pending) return false;
    const cancelled = settlePending(pending, { error: new CaptchaCancelledError() });
    if (cancelled) invalidateInitialization();
    return cancelled;
  }

  /** 有没有正在等的验证码流程（调用方用它决定关闭弹窗时要不要提示） */
  const isBusy = () => pendingVerification !== null;

  window.wbAliyunCaptcha = { solve, cancel, isBusy, CaptchaError, CaptchaCancelledError };
})();
