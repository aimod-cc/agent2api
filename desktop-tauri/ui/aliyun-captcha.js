/* Agent2API · 阿里云无痕验证（滑块）求解器 —— **多调用方共用**

   ── 为什么单独成文件（原来在 autoclaw-oauth.js 里）───────────
   这段代码最早是 AutoClaw 国际版 OAuth 登录的私有实现：那家的网页登录前面
   强制多一道风控验证码，必须先拖完滑块拿到 `verifyParam` 才能换授权地址。

   ZCode 的「领套餐」同样要过这道验证码（上游的领取接口要
   `X-Aliyun-Captcha-Verify-Param`；要不要由上游那份风控配置说了算 ——
   `enabled: false` 的那一刻前端**不该**弹滑块），而它**与 AutoClaw 的用法只有一半相同**：

     · AutoClaw：解验证码 → 用 verifyParam 换授权地址 → 开窗口等回调
     · ZCode：   解验证码 → 用 verifyParam 直接去领取（到此为止）

   留在原处的话，第二家只能照抄一份。这段代码的坑都在细节里（指纹要稳定、
   容器要清理、代际号要作废、取消要收尾、SDK 的配置必须在加载前设好），
   抄一份就是两份会各自漂移的坑。因此抽到这里，两家都调它。

   ── 第三个调用方：静默铸造（`mintTraceless`）─────────────────
   ZCode 的**活动套餐转发**通道（`providers::zcode::plan`）要求每条请求带一个
   阿里云验证码令牌：上游对缺令牌的请求一律回 3007。令牌要**当次铸**（一次性、
   两分钟寿命），而转发是后台发生的（用户没点任何按钮）—— 所以这里补一个
   **无痕验证**入口：`startTracelessVerification()` 不弹滑块、不要用户操作，
   SDK 自己跑完风控流程把串给回调（与参考实现在 happy-dom 里走的是同一个 API）。
   调用方是 `ui/zcode-captcha-pool.js`（池子守卫），它把铸好的串推给网关。

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
  /** 无痕铸造的超时（见 mintTraceless：卡住就重铸，不留着等） */
  const MINT_TIMEOUT_MS = 20000;
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

  /* ─── 静默铸造（活动套餐转发通道的令牌来源）─────────────────── */

  /**
   * 铸造用的容器 id **每次铸造都换一套**（与滑块流程的也分开）。
   *
   * 为什么不复用同一对 id：实测「destroy 之后在同一个容器上重新
   * `initAliyunCaptcha`」会卡在初始化（20 秒超时），而换一套新的容器就正常 ——
   * SDK 在容器上留了内部状态。反正铸造是一次一实例（见 `deliverMintResult`），
   * 每次配一套新容器最省心。
   */
  let mintSeq = 0;
  const mintIds = () => {
    mintSeq += 1;
    return {
      elementId: `zcode-mint-captcha-element-${mintSeq}`,
      buttonId: `zcode-mint-captcha-trigger-${mintSeq}`,
    };
  };

  /** 当前铸造实例（与滑块那个实例并存；一次铸造后即作废） */
  let mintInstancePromise = null;
  let mintInstanceKey = '';
  let mintInstanceRef = null;
  /** 当前实例占用的容器 id（作废时连同容器一起删掉） */
  let mintElementId = '';
  let mintButtonId = '';
  /** 正在等结果的那一次铸造 */
  let mintWaiter = null;
  let mintGeneration = 0;
  let mintInitializedAt = 0;

  /**
   * 模拟真实用户行为事件（激活状态、鼠标移动），提升阿里云无痕风控通过率，
   * 并防止后台窗口被判断为完全无交互的 headless 环境。
   */
  function simulateHumanActivity() {
    try {
      if (document.hidden) {
        Object.defineProperty(document, 'hidden', { value: false, configurable: true });
      }
      if (document.visibilityState !== 'visible') {
        Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true });
      }
    } catch { /* 忽略只读拦截 */ }

    try {
      for (let i = 0; i < 3; i++) {
        const x = Math.floor(100 + Math.random() * 300);
        const y = Math.floor(100 + Math.random() * 200);
        const event = new MouseEvent('mousemove', {
          bubbles: true,
          cancelable: true,
          view: window,
          clientX: x,
          clientY: y,
          screenX: x + 50,
          screenY: y + 50,
        });
        window.dispatchEvent(event);
      }
    } catch { /* 忽略事件派发失败 */ }
  }

  /**
   * 备好铸造用的容器与触发按钮。
   *
   * 放在**屏幕外**而不是隐藏（`display:none`）：阿里云 SDK 会读容器尺寸做布局，
   * 隐藏容器在部分版本里会让实例初始化不出来（无痕模式虽然不显示 UI，SDK 仍按
   * 容器初始化）。这与参考实现把整个 DOM 藏在无头环境里是同一个道理。
   */
  function ensureMintElements(elementId, buttonId) {
    let element = document.getElementById(elementId);
    if (!element) {
      element = document.createElement('div');
      element.id = elementId;
      element.style.position = 'fixed';
      element.style.left = '-9999px';
      element.style.top = '0';
      element.style.width = '360px';
      element.style.height = '40px';
      document.body.appendChild(element);
    }
    let button = document.getElementById(buttonId);
    if (!button) {
      button = document.createElement('button');
      button.id = buttonId;
      button.type = 'button';
      button.tabIndex = -1;
      button.setAttribute('aria-hidden', 'true');
      button.style.position = 'fixed';
      button.style.left = '-9999px';
      button.style.width = '1px';
      button.style.height = '1px';
      button.style.opacity = '0';
      button.style.pointerEvents = 'none';
      document.body.appendChild(button);
    }
  }

  /** 铸造实例作废（失败/超时后调；下一次会用新实例重来） */
  function invalidateMintInstance() {
    const instance = mintInstanceRef;
    mintInstancePromise = null;
    mintInstanceKey = '';
    mintInstanceRef = null;
    mintInitializedAt = 0;
    try { instance?.destroy?.(); } catch { /* 忽略：实例已不可用 */ }
    // 容器一起清掉：同一个容器在 destroy 之后再 init 会卡住（见 mintIds 的说明）
    try { document.getElementById(mintElementId)?.remove(); } catch { /* 忽略 */ }
    try { document.getElementById(mintButtonId)?.remove(); } catch { /* 忽略 */ }
    mintElementId = '';
    mintButtonId = '';
  }

  /** 铸造失败：落定正在等的那次（并作废实例） */
  function failMint(error) {
    const waiter = mintWaiter;
    mintWaiter = null;
    invalidateMintInstance();
    if (waiter) waiter.reject(error);
  }

  /**
   * 校验并归一一个验证串。
   *
   * 上游只认「约 280 字符的 base64 JSON，且内含一个长 securityToken」这一种形态：
   * 参考实现在这里做了同样的严格校验，理由是「SDK 的降级路径会给出短串，
   * 拿它去请求必回 3007」。我们实测过另一种形态（回调模式给的裸 JSON）同样被拒，
   * 所以宁可在这里拒绝、让上层重铸，也不要把一个注定 3007 的串推进池子 ——
   * 那会烧掉一次请求往返，还把「库存有货」这个读数变成假的。
   */
  function validateVerifyParam(value) {
    if (typeof value !== 'string' || value.trim().length < 200) {
      throw new CaptchaError('验证码组件返回的验证串不完整（请重试）');
    }
    const text = value.trim();
    try {
      const json = JSON.parse(atob(text));
      const token = json && (json.securityToken || json.SecurityToken);
      if (!token || String(token).length < 50) throw new Error('no securityToken');
    } catch {
      throw new CaptchaError('验证码组件返回的验证串不是上游认的形态（请重试）');
    }
    return text;
  }

  /**
   * SDK 的 `success` 回调：把结果交给正在等的那次铸造。
   *
   * 官方 SDK 在不同模式下给的值不一样（字符串 / 带 `verifyParam`、
   * `captchaVerifyParam`、`data`、`param` 的对象），四种都取一遍再交给
   * [`validateVerifyParam`] 把关。
   */
  function deliverMintResult(result) {
    const waiter = mintWaiter;
    if (!waiter) return;
    let value = result;
    if (result && typeof result === 'object') {
      value = result.verifyParam || result.captchaVerifyParam || result.data || result.param;
    }
    let param = null;
    let failure = null;
    try {
      param = validateVerifyParam(value);
    } catch (error) {
      failure = error;
    }
    mintWaiter = null;
    if (failure) {
      failMint(failure);
      return;
    }
    // ── 铸完就作废实例（**一次一铸**，实测结论）────────────────────
    // SDK 的同一个实例第二次调 `startTracelessVerification()` 必失败
    // （2026-09-28 实测：第一次 958ms 拿到串，第二次直接走 fail 回调）。
    // 因此每次铸造都用新实例 —— 代价约 1 秒（脚本已加载，只是重建实例），
    // 而池子目标只有 3 个、寿命 120 秒，这点开销换「第二次必然成功」是划算的。
    invalidateMintInstance();
    waiter.resolve(param);
  }

  /**
   * 静默铸一个验证串（无痕验证；不弹滑块、不需要用户操作）。
   *
   * ── 为什么转发链路必须用它 ─────────────────────────────────
   * ZCode 的活动套餐推理端点**每条请求**都要一个当次铸的令牌（少它一律
   * `400 {"code":3007}`，2026-09-28 实测：不是偶发挑战，是常规门禁）。转发发生
   * 在后台，没有让用户拖滑块的机会 —— 无痕模式本来就是为这种场景设计的：
   * `startTracelessVerification()` 让 SDK 自己跑完风控流程，把串交给 `success`。
   *
   * ── 为什么要自己的实例（而不是复用滑块那个）──────────────────
   * 两种集成模式拿到的值不一样：滑块走的 `captchaVerifyCallback` 收到的是
   * **裸 JSON**（`{sceneId, certifyId, deviceToken, data}`），上游拒收（实测
   * 3007）；无痕走 `success` 才拿到上游认的那个 base64 串。两套回调没法在一个
   * 实例上共存，所以铸造另起一个实例（元素/按钮 id 都分开）—— 与滑块那个并存，
   * 互不干扰（滑块流程忙碌时上层会让路，见 `ui/zcode-captcha-pool.js`）。
   *
   * 返回值就是这个串。**一次一用**：铸好不用，两分钟后自己过期（上游拒收）。
   */
  async function mintTraceless(config) {
    const instance = await ensureMintInstance(config);
    // ── SDK 预热等待 ───────────────────────────────────────────
    // 阿里云 SDK 初始化后必须收集足够的设备环境与指纹数据（至少等 2.1 秒），
    // 否则直接调用 startTracelessVerification 会触发 fail/onError。
    const elapsed = Date.now() - mintInitializedAt;
    if (elapsed < MINIMUM_WARMUP_MS) {
      await sleep(MINIMUM_WARMUP_MS - elapsed);
    }
    simulateHumanActivity();

    const generation = ++mintGeneration;
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        if (!mintWaiter || mintWaiter.generation !== generation) return;
        mintWaiter = null;
        invalidateMintInstance();
        reject(new CaptchaError('静默铸造超时，请重试'));
      }, MINT_TIMEOUT_MS);
      mintWaiter = {
        generation,
        resolve: value => { window.clearTimeout(timer); resolve(value); },
        reject: error => { window.clearTimeout(timer); reject(error); },
      };
      const start = typeof instance.startTracelessVerification === 'function'
        ? instance.startTracelessVerification
        : instance.show;
      if (typeof start !== 'function') {
        failMint(new CaptchaError('验证码组件不支持静默铸造'));
        return;
      }
      try {
        start.call(instance);
      } catch (error) {
        failMint(new CaptchaError((error && error.message) || '验证码组件启动失败'));
      }
    });
  }

  /** 备好铸造实例（同配置复用；失败/超时后由 invalidateMintInstance 作废） */
  function ensureMintInstance(config) {
    const language = resolveAliyunCaptchaLanguage(currentLanguage());
    const key = config.region + ':' + config.prefix + ':' + config.sceneId + ':' + language;
    if (mintInstancePromise && mintInstanceKey === key) return mintInstancePromise;
    if (mintInstanceRef) invalidateMintInstance();
    mintInstanceKey = key;
    const pending = (async () => {
      await loadAliyunCaptchaScript(config);
      const ids = mintIds();
      mintElementId = ids.elementId;
      mintButtonId = ids.buttonId;
      ensureMintElements(ids.elementId, ids.buttonId);
      const initAliyunCaptcha = getInitAliyunCaptcha();
      if (!initAliyunCaptcha) throw new CaptchaError('验证码组件不可用，请重试');
      return new Promise((resolve, reject) => {
        let settled = false;
        const timer = window.setTimeout(() => {
          if (settled) return;
          settled = true;
          mintInstancePromise = null;
          reject(new CaptchaError('验证码组件初始化超时，请重试'));
        }, INIT_TIMEOUT_MS);
        try {
          initAliyunCaptcha({
            SceneId: config.sceneId,
            mode: 'popup',
            region: config.region,
            prefix: config.prefix,
            element: '#' + ids.elementId,
            button: '#' + ids.buttonId,
            captchaLogoImg: '',
            showErrorTip: false,
            language,
            getInstance: instance => {
              if (settled) return;
              settled = true;
              window.clearTimeout(timer);
              mintInstanceRef = instance;
              mintInitializedAt = Date.now();
              resolve(instance);
            },
            success: result => deliverMintResult(result),
            fail: error => failMint(new CaptchaError(
              (error && error.message) || '验证码校验失败，请重试',
            )),
            onError: error => failMint(new CaptchaError(
              (error && error.message) || '验证码组件出错，请重试',
            )),
          });
        } catch (error) {
          if (settled) return;
          settled = true;
          window.clearTimeout(timer);
          mintInstancePromise = null;
          reject(new CaptchaError((error && error.message) || '验证码组件初始化失败'));
        }
      });
    })();
    // 初始化失败不要把失败的 promise 缓存住（下一次要能重来）
    pending.catch(() => {
      mintInstancePromise = null;
      mintInstanceKey = '';
    });
    mintInstancePromise = pending;
    return pending;
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

  window.wbAliyunCaptcha = {
    solve,
    cancel,
    isBusy,
    // 静默铸串（活动套餐转发通道的令牌来源，见 mintTraceless）
    mintTraceless,
    resetMint: invalidateMintInstance,
    CaptchaError,
    CaptchaCancelledError,
  };
})();
