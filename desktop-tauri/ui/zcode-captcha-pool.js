/* Agent2API · ZCode 活动套餐通道的**验证码令牌池守卫**

   ── 它解决的是什么（一句话）───────────────────────────────────
   ZCode 的「活动套餐」额度只从 `POST /api/v1/zcode-plan/anthropic/v1/messages`
   花得出去，而那个端点**每条请求都要一个当次铸的阿里云验证码令牌**：少它一律
   `400 {"code":3007,"msg":"captcha verify failed"}`（实测不是偶发挑战，是常规门禁）。
   Rust 侧铸不出来（要跑阿里云 SDK），所以由这个跑在 WebView 里的守卫铸：
   静默无痕验证 → 推给网关 → 转发层按请求取一个。

   ── 节奏（不铸多、不空窗）─────────────────────────────────────
   每 4 秒问一次网关 `GET /api/zcode/captcha`：
     · `needsTokens: true`（有账号走活动套餐，且库存低于目标）→ 补到目标；
     · `needsTokens: false` → 什么都不做（**没有账号用这条路时一个都不铸** ——
       阿里云对铸造频率有风控，白铸纯属烧配额，见参考实现的熔断那一段）；
     · 上游刚回过 3007（`rejected` 计数变大）→ 立刻补一轮，不等下一个周期。
   令牌寿命两分钟（网关那侧定的），每次只铸到目标（默认 3 个）：
   「够下一条请求立刻有得用」即可。

   ── 为什么不做成用户可见的按钮 ───────────────────────────────
   转发是后台发生的（用户在别的工具里发请求），没有人会在每条请求前点一次验证。
   这与「领套餐」的滑块不是一回事：那个是**用户动作**，必须让用户看见、确认；
   这个是**机器对机器的门禁令牌**，无痕模式本来就是为这种场景设计的。

   ── 失败时怎么办（如实，不装死）───────────────────────────────
   铸造失败（SDK 加载不出来、网络不通、阿里云限流）只写控制台并退让到下一个
   周期：令牌池空了，转发层会如实回一句「活动套餐通道需要人机验证令牌…」，
   用户知道去哪儿看。**不要**在这里弹 toast —— 它是后台循环，弹一次就会有
   第二次、第三次。

   依赖：`ui/aliyun-captcha.js` 的 `window.wbAliyunCaptcha.mintTraceless`
   （脚本顺序：本文件必须排在它之后）与桥接层 `zcodeCaptchaStats` /
   `pushZcodeCaptchaTokens`（见 desktop-tauri/src-tauri/src/bridge.rs）。 */

(() => {
  /** 轮询间隔（毫秒）。2.5 秒是「库存见底到补上」的平滑窗口 */
  const POLL_MS = 2500;
  /** 一轮铸 1 个：单实例单次铸造，避免并发冲突与 DOM 重建竞态 */
  const MAX_PER_ROUND = 1;
  /** 失败后的退让（毫秒）：失败时轻度退让，并在连续失败时重置铸造实例 */
  const BACKOFF_MIN_MS = 3000;
  const BACKOFF_MAX_MS = 15000;

  let timer = 0;
  let running = false;
  let stopped = false;
  /** 上一轮看到的 3007 计数（变大 = 上游刚拒了令牌，立刻补货） */
  let lastRejected = 0;
  /** 连续失败次数（退让用） */
  let failures = 0;

  const api = () => window.workbuddyDesktop || null;

  function log(message, ...args) {
    console.log(`[ZCodeCaptcha] ${message}`, ...args);
    if (window.wbApp?.debug) window.wbApp.debug(`[ZCodeCaptcha] ${message}`);
  }

  function warn(message, ...args) {
    console.warn(`[ZCodeCaptcha] ${message}`, ...args);
    if (window.wbApp?.debug) window.wbApp.debug(`[ZCodeCaptcha] [WARN] ${message}`);
  }

  /** 拿一次池子概况；桥接不可用（浏览器直开界面）时返回 null，循环安静地退让 */
  async function fetchStats() {
    const bridge = api();
    if (!bridge?.zcodeCaptchaStats) return null;
    try {
      return await bridge.zcodeCaptchaStats();
    } catch (error) {
      log(`读取令牌池概况失败：${error?.message || error}`);
      return null;
    }
  }

  async function pushTokens(tokens) {
    const bridge = api();
    if (!bridge?.pushZcodeCaptchaTokens || tokens.length === 0) return false;
    try {
      await bridge.pushZcodeCaptchaTokens(tokens);
      return true;
    } catch (error) {
      log(`令牌入池失败：${error?.message || error}`);
      return false;
    }
  }

  /** 铸一个（走无痕验证；`wbAliyunCaptcha` 未就绪或正忙时返回 null） */
  async function mintOne(config) {
    const captcha = window.wbAliyunCaptcha;
    if (!captcha?.mintTraceless) return null;
    // 用户正在拖滑块（领套餐 / 添加账号）时让路：两条流程共用 SDK 的同一个实例
    if (captcha.isBusy?.()) return null;
    try {
      return await captcha.mintTraceless(config);
    } catch (error) {
      log(`铸造失败：${error?.message || error}`);
      return null;
    }
  }

  /** 一轮：读概况 → 需要就补 → 排下一次 */
  async function round() {
    if (running || stopped) return;
    running = true;
    try {
      const stats = await fetchStats();
      if (!stats) {
        schedule(BACKOFF_MIN_MS);
        return;
      }
      // 上游刚拒过令牌（3007）：库存即使「够」也不可信了，也补一轮
      const rejectedNow = Number(stats.rejected) || 0;
      const challenged = rejectedNow > lastRejected;
      lastRejected = rejectedNow;
      const want = stats.needsTokens === true || challenged;
      // 「需要铸造」的三样前提缺一不可：有账号走活动套餐、库存不足、风控配置拿得到
      if (!want) {
        schedule(POLL_MS);
        return;
      }
      // 风控配置只在真要铸的时候才去取（带 5 分钟缓存；上游那份配置很少变）
      const config = await fetchCaptchaConfig(stats);
      if (!config) {
        schedule(BACKOFF_MIN_MS);
        return;
      }
      const target = Math.max(1, Number(stats.target) || 1);
      const ready = Number(stats.ready) || 0;
      const need = Math.min(MAX_PER_ROUND, Math.max(0, target - ready));
      const tokens = [];
      for (let index = 0; index < need; index += 1) {
        const param = await mintOne(config);
        if (!param) break;
        tokens.push({ param, region: config.region || '' });
        // 连续铸造之间留一点间隔：SDK 的同一个实例连着跑两轮会互相干扰
        await new Promise(resolve => window.setTimeout(resolve, 500));
      }
      if (tokens.length === 0) {
        // 一个都没铸出来（SDK 不可用 / 正忙）—— 退让，别原地空转
        failures += 1;
        if (failures >= 2) {
          try { window.wbAliyunCaptcha?.resetMint?.(); } catch {}
        }
        const delay = Math.min(BACKOFF_MIN_MS * failures, BACKOFF_MAX_MS);
        warn(`本轮未能铸出令牌（连续失败 ${failures} 次），将在 ${delay}ms 后重试`);
        schedule(delay);
        return;
      }
      await pushTokens(tokens);
      failures = 0;
      schedule(POLL_MS);
    } catch (error) {
      failures += 1;
      if (failures >= 2) {
        try { window.wbAliyunCaptcha?.resetMint?.(); } catch {}
      }
      warn(`本轮异常：${error?.message || error}`);
      schedule(Math.min(BACKOFF_MIN_MS * failures, BACKOFF_MAX_MS));
    } finally {
      running = false;
    }
  }

  /** 风控配置的缓存（上游那份配置一天也不会变几次，每轮都问纯属浪费） */
  let configCache = { value: null, expiresAt: 0 };
  const CONFIG_TTL_MS = 5 * 60000;

  /**
   * 取阿里云风控配置（`{enabled, prefix, sceneId, region}`）。
   *
   * 与「领套餐」走的是**同一份**上游配置（`GET /api/v1/client/configs` 的
   * `configs.captcha`，见 `api::zcode_claim::captcha_config`）。账号 id 由网关
   * 给（`captchaAccountId`，它挑的是走活动套餐的那个）—— 界面不必自己读账号表。
   */
  async function fetchCaptchaConfig(stats) {
    if (configCache.value && configCache.expiresAt > Date.now()) return configCache.value;
    const bridge = api();
    const accountId = String(stats.captchaAccountId || '');
    if (!bridge?.zcodeClaimCaptchaConfig || !accountId) return null;
    try {
      const config = await bridge.zcodeClaimCaptchaConfig(accountId);
      if (!config?.enabled || !config.sceneId) {
        // 上游此刻不要验证码（或配置不完整）：按「不铸造」处理，下一轮再问
        log('上游风控配置未启用（enabled:false）——不铸造');
        return null;
      }
      configCache = {
        value: { sceneId: config.sceneId, prefix: config.prefix, region: config.region },
        expiresAt: Date.now() + CONFIG_TTL_MS,
      };
      return configCache.value;
    } catch (error) {
      log(`取风控配置失败：${error?.message || error}`);
      return null;
    }
  }

  function schedule(delay) {
    window.clearTimeout(timer);
    if (stopped) return;
    timer = window.setTimeout(() => void round(), delay);
  }

  /** 起来（应用启动时调一次；重复调只是重置计时器） */
  function start() {
    stopped = false;
    schedule(1500);
  }

  function stop() {
    stopped = true;
    window.clearTimeout(timer);
  }

  window.wbZcodeCaptchaPool = { start, stop };

  // 自启：界面加载完就开始守着。桥接不可用（浏览器直开）时循环会安静退让，
  // 不会有报错刷屏（见 fetchStats 的返回 null 分支）
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    start();
  }
})();
