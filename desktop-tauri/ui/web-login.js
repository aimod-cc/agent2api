/* Agent2API · 「网页登录」交互引擎（WorkBuddy 与小浣熊共用）

   两家 provider 的网页登录在界面上是同一种交互：一个「打开网页登录」按钮、
   一个「取消等待」按钮、一行 hint，等待期间按钮禁用并转圈，成功后关弹窗刷新列表。
   差别只有三处（发起时传什么参数、按钮/提示的文案、成功后提示什么），
   因此这里把**流程**实现一次，各家只交一份配置（见 create 的 config 说明）。
   为什么不是复制两份：等待态与按钮复位必须与主进程的真实状态对齐
   （applyLoginState 的注释解释了这点），两份实现迟早会在某一次改动后分叉，
   而分叉的表现是「关掉弹窗后按钮永远卡在禁用态」这种很难复现的故障。

   依赖 app.js 的顶层全局（经典 script 的顶层声明在全局可见）：$ / toast / busy /
   releaseBusy / refresh。脚本顺序见 index.html：必须排在 add-account.js 之前 ——
   后者在加载期就要 create() 出 WorkBuddy 那一份（它的 DOM 在 index.html 里）。

   与主进程的契约：同一时刻只允许一个登录流程（壳侧 start_login 会拒绝第二个），
   因此这里维护一份**全局**的 loginActive / loginProvider，而不是每个控制器各存一份；
   某一家在等待时，另一家的发起按钮也要禁用（点下去必然报错）。 */

(() => {
  const $ = id => document.getElementById(id);
  const { toast } = window.wbApp;

  /** 所有已登记的控制器（按 create 顺序） */
  const controllers = [];
  /** 是否有登录流程在等待（主进程的真实状态，见 applyShellState） */
  let loginActive = false;
  /** 等待中的那家 provider；主进程状态里没带 provider 时按 workbuddy 兜底 */
  let loginProvider = '';
  /** 一次 start() 正在收尾：此时主进程状态可能还没回到「空闲」，
   *  期间任何「顺手取消」（关弹窗时）都必须让路 —— 否则会把一次
   *  刚成功的登录取消掉，界面上表现为「登录成功却提示已取消」。 */
  let flowSettling = false;
  /** 递增的登录代次。异步响应（URL / 成功 / 失败）回来时先核对代次，
   *  过期的响应不得改动界面或账号状态（见 pagehide 处理与 start()）。
   *  一个过期响应必须同时让位于「更新的登录」与「pagehide 之后的复位」。 */
  let generation = 0;
  /** 页面是否已被 pagehide 作废（进 bfcache 前的同步复位标志）。
   *  置位后所有在途响应都被判为过期；新一次 start() 会清掉它。 */
  let pageInvalidated = false;

  const providerLabel = id => window.wbProviders?.labelOf?.(id) || id;

  /**
   * 建一个登录控制器。
   *
   * config：
   *   provider   本控制器对应的 provider id（与壳侧 start_login 的参数一致）
   *   buttonId / cancelId / hintId   三个 DOM id（各家的块用前缀区分）
   *   texts()    返回 { button, hint }：当前该显示的按钮文案与提示。
   *              做成函数是因为 WorkBuddy 的文案随「打开方式」分段控件变化，
   *              而那个值只有 add-account.js 知道（这里不去读别家的 DOM）。
   *   busyText   等待中的按钮文案（前缀会自动加转圈）
   *   start()    真正发起登录，返回壳命令的结果（`{canceled:true}` 表示用户取消）
   *   onSuccess(result)  登录成功后的收尾（关弹窗、刷新列表、提示）
   */
  function create(config) {
    const button = () => $(config.buttonId);
    const cancelButton = () => $(config.cancelId);
    const hint = () => $(config.hintId);

    const syncTexts = () => {
      const button_ = button();
      if (!button_) return;
      const texts = config.texts ? config.texts() : { button: '', hint: '' };
      if (texts.button) button_.textContent = texts.button;
      const hint_ = hint();
      if (hint_ && texts.hint) hint_.textContent = texts.hint;
    };

    /** 把主进程状态落到本控制器的 DOM（等待中/别家在等待/空闲三种形态） */
    const applyState = () => {
      const button_ = button();
      if (!button_) return;
      const cancel_ = cancelButton();
      if (loginActive && loginProvider === config.provider) {
        button_.disabled = true;
        button_.innerHTML = `<span class="spinner"></span>${config.busyText || '等待登录完成…'}`;
        // 取消按钮切 hidden 属性而不是行内 display：它是组件库的 Button（自带带
        // !important 的 inline-flex 工具类），行内样式压不过它
        if (cancel_) cancel_.hidden = false;
        return;
      }
      if (cancel_) cancel_.hidden = true;
      if (loginActive) {
        // 别家在等待：主进程同一时刻只允许一个登录流程，本家的发起按钮必须禁用
        button_.disabled = true;
        const hint_ = hint();
        if (hint_) {
          hint_.textContent = `正在等待${providerLabel(loginProvider)}登录完成；完成或取消后`
            + `才能发起新的登录`;
        }
        return;
      }
      button_.disabled = false;
      syncTexts();
    };

    const controller = {
      provider: config.provider,
      /** 提示与按钮文案随本家的其它控件变化时调用（等待中不覆盖，见 applyState） */
      syncTexts: () => {
        if (!loginActive) syncTexts();
      },
      applyState,
      /** 发起登录；与 takeBusy 同一把锁（弹窗里的按钮互斥） */
      async start() {
        if (busy || loginActive) return;
        busy = true;
        const button_ = button();
        if (button_) {
          button_.disabled = true;
          button_.innerHTML = `<span class="spinner"></span>${config.busyText || '等待网页登录…'}`;
        }
        // 本轮登录的代次：pagehide 或下一次 start() 都会把它作废
        pageInvalidated = false;
        generation += 1;
        const myGeneration = generation;
        try {
          const result = await config.start();
          if (myGeneration !== generation || pageInvalidated) return;
          if (result?.canceled) {
            toast('已取消登录等待');
            return;
          }
          flowSettling = true;
          await config.onSuccess(result);
        } catch (error) {
          if (myGeneration !== generation || pageInvalidated) return;
          toast(`登录失败：${error.message}`, 'err');
        } finally {
          flowSettling = false;
          releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
          // 过期响应不得复位界面：pagehide / 新登录已经或即将接管它
          if (myGeneration === generation) {
            // 以主进程状态为准复位按钮，避免这里与真实状态不一致
            await syncShellState();
          }
        }
      },
      /** 取消等待（本家在等待时才发请求） */
      async cancel() {
        if (!loginActive || loginProvider !== config.provider) {
          applyState();
          return;
        }
        const canceled = await cancelLogin();
        toast(canceled ? '已取消登录等待' : '登录已结束');
        await syncShellState();
      },
    };
    controllers.push(controller);
    applyState();
    return controller;
  }

  /** 通知主进程取消等待中的登录（不区分是哪一家：主进程只记一个活动登录） */
  async function cancelLogin() {
    try {
      await window.workbuddyDesktop.cancelLogin();
      return true;
    } catch (error) {
      toast(`取消失败：${error.message}`, 'err');
      return false;
    }
  }

  /** 把主进程的登录状态落到所有控制器 */
  function applyShellState(active, provider) {
    loginActive = Boolean(active);
    // 老版本壳不带 provider 字段：那时只有 workbuddy 一家有网页登录
    loginProvider = loginActive ? (provider || 'workbuddy') : '';
    for (const controller of controllers) controller.applyState();
  }

  /** 问一次主进程的真实状态并落定（弹窗打开、流程结束时调用）。

      名字不叫 refresh：那个名字在 app.js 里是「刷新账号列表」，两者都会在
      同一个函数里被调用，重名会让读者以为是同一件事。 */
  async function syncShellState() {
    try {
      const state = await window.workbuddyDesktop.getLoginState();
      applyShellState(state?.active, state?.provider);
    } catch {
      applyShellState(false, '');
    }
  }

  /**
   * 页面进 bfcache / 关闭前的**同步**复位。
   *
   * pagehide 可能发生在等待登录的过程中：浏览器把整页放进 bfcache，恢复时
   * **不会重新挂载**组件，因此在途响应 finally 里的 `myGeneration === generation`
   * 守卫会正确地拒绝改动状态 —— 结果就是按钮永久卡在忙碌态。这不是我们想要的
   * 「正确」。这里必须同步：作废代次 → 立刻复位 busy/hint → 用 keepalive 尽力
   * 通知服务端取消（拿不到 keepalive 支持也不影响界面已经复位）。
   */
  function handlePageHide() {
    if (typeof window === 'undefined') return;
    pageInvalidated = true;
    generation += 1;
    releaseBusy(); // 同步清掉弹窗级 busy 锁（并补跑排队中的刷新）
    applyShellState(false, ''); // 同步复位所有控制器的按钮禁用态与 hint 文案
    try {
      // 不 await：页面正在卸载。keepalive 让取消请求在页面销毁后仍有机会送达。
      window.workbuddyDesktop?.cancelLogin?.({ keepalive: true })?.catch?.(() => {});
    } catch {
      /* 卸载中取消失败无所谓：服务端 pending 会自行超时作废 */
    }
  }

  window.wbWebLogin = {
    create,
    refresh: syncShellState,
    isActive: () => loginActive,
    activeProvider: () => loginProvider,
    /** 暴露给自动化测试与宿主页面显式调用（见 handlePageHide） */
    handlePageHide,
    /** 放弃等待中的登录。给定 provider 时只取消该家的流程，避免干扰其它提供商。
     *  返回是否真的发了取消请求。 */
    async cancelIfActive(provider) {
      if (!loginActive || flowSettling) return false;
      if (provider && loginProvider !== provider) return false;
      const canceled = await cancelLogin();
      await syncShellState();
      return canceled;
    },
  };

  // 页面进 bfcache / 关闭：同步复位登录锁，不能只靠被代次守卫拦住的 finally
  window.addEventListener?.('pagehide', handlePageHide);

  // 登录进行状态由主进程推送：等待结束（成功/失败/取消）后按钮自动复位
  window.workbuddyDesktop.onLoginState?.(payload =>
    applyShellState(payload?.active, payload?.provider));
})();
