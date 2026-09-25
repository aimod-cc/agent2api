/* 「登录 / 添加账号」弹窗：WorkBuddy 的账号版本、网页登录打开方式与弹窗事件。

   网页登录流程由 web-login.js 统一管理，这里只提供 WorkBuddy 的配置。
   依赖 app.js 的顶层全局 api / $ / toast / refresh，以及 web-login.js 的控制器。
   本文件必须在 add-provider-forms.js 之前加载：同一添加按钮先打开弹窗，再同步提供商。 */

// ─── 添加账号弹窗 ─────────────────────────────

function openModal() {
  $('add-modal').classList.add('open');
  syncLoginModeHint();
  syncSocialRestoreState();
  // 以主进程真实状态复位按钮：上次若在等待中被关窗，这里会重新可用
  void window.wbWebLogin?.refresh();
}

function closeModal() {
  $('add-modal').classList.remove('open');
  // 关窗即放弃等待：通知主进程中止后端轮询，否则按钮会一直卡在禁用态。
  // 不传 provider = 「谁在等待就取消谁」：两家共用同一个弹窗，这里无需区分。
  void window.wbWebLogin?.cancelIfActive('');
  // AutoClaw 国际版的 OAuth 另有一套等待（先跑验证码、再等登录窗口），
  // 与上面那条链的取消语义不同（它还要作废本地那次验证码等待），因此
  // 单独通知一次。没有发起过时它什么都不做。
  window.wbAutoclawOauth?.cancel?.();
}

/**
 * 弹窗里的选择项都是分段控件，但分两条路：
 *   · index.html 里静态的这两处（账号版本 / 打开方式）由 React 岛渲染
 *     （ui/islands/ui.js），见下面的 mountAddSegs；
 *   · 动态生成的那些（提供商 / 添加方式 / 地区 / 登录方式）仍归
 *     add-provider-forms.js 的 bindSeg（切换后派发 wb-seg-change）。
 *
 * 岛是受控的，所以这两处的取值以 segState 为准 —— 不再从 DOM 反查：岛渲染出来的
 * 选项上没有 .active，选中态表达为 data-checked。这反而更贴回改造前的本意
 * （那时注释就写着「只保留一处状态，不留第二份」）。
 */
const segState = { edition: 'cn', loginMode: 'embedded' };

/** 岛句柄：setLoginMode 要靠它把值回灌进去 */
const segIslands = { edition: null, loginMode: null };

/** 弹窗里的分段条比页面上大一号，这个类名带来尺寸覆盖（见 page-accounts-providers.css） */
const ADD_SEG_CLASS = 'add-seg';

/**
 * 挂两处静态分段控件的岛。脚本顺序上 add-account.js 排在 islands/ui.js 之后、
 * DOM 也已解析完，所以这里可以直接挂。
 */
function mountAddSegs() {
  if (!window.wbSegmented) return;
  const editionHost = $('add-edition-seg');
  if (editionHost) {
    segIslands.edition = window.wbSegmented.mount(editionHost, {
      options: [
        { value: 'cn', label: '国内版（WorkBuddy）' },
        { value: 'intl', label: '国际版（WorkBuddy AI）' },
      ],
      value: segState.edition,
      ariaLabel: '账号版本',
      className: ADD_SEG_CLASS,
      // 国际版默认走系统浏览器、国内版默认内嵌窗口 —— 沿用改造前的联动
      onChange: value => {
        segState.edition = value;
        setLoginMode(value === 'intl' ? 'external' : 'embedded');
      },
    });
  }
  const loginHost = $('add-login-mode');
  if (loginHost) {
    segIslands.loginMode = window.wbSegmented.mount(loginHost, {
      options: [
        { value: 'embedded', label: '内嵌窗口' },
        { value: 'external', label: '系统默认浏览器' },
      ],
      value: segState.loginMode,
      ariaLabel: '网页登录的打开方式',
      className: ADD_SEG_CLASS,
      // 打开方式影响第三方入口开关的可用态（要「国际版 + 内嵌窗口」同时成立），
      // syncSocialRestoreState 内部会顺带刷新提示文案
      onChange: value => {
        segState.loginMode = value;
        syncSocialRestoreState();
      },
    });
  }
}

/** 弹窗里选的账号版本（cn=国内版 / intl=国际版） */
function selectedEdition() {
  return segState.edition === 'intl' ? 'intl' : 'cn';
}

/** 网页登录的打开方式（embedded=内嵌窗口 / external=系统默认浏览器） */
function selectedLoginMode() {
  return segState.loginMode === 'external' ? 'external' : 'embedded';
}

/**
 * 是否恢复 Google / GitHub 第三方登录入口（默认不勾选）。
 *
 * 国际版登录页自己会按上游入口策略（`/v2/plugin/login/entry-policy` 返回的
 * `enable_oneid_only_login`）把 Google / GitHub / X 三个按钮用 CSS 隐藏掉，
 * 只留邮箱与 SSO。勾选此项时壳侧会在内嵌窗口里摘掉那段样式，按钮就回来了。
 *
 * 只对 WorkBuddy（国际版）有意义：国内版登录页没有这套隐藏逻辑，其它提供商
 * 也不走这个弹窗的这条链，所以值照传、壳侧按 provider 忽略。
 *
 * 取不到这个复选框时按 false（不恢复）：与界面默认态一致，也让「DOM 被人改坏」
 * 收敛到保守的那一侧，而不是悄悄放宽一次登录的域名白名单。
 */
function socialRestoreEnabled() {
  return $('add-social-restore')?.checked === true;
}

function setLoginMode(mode) {
  segState.loginMode = mode === 'external' ? 'external' : 'embedded';
  segIslands.loginMode?.setValue(segState.loginMode);
  syncLoginModeHint();
  syncSocialRestoreState();
}

/** 登录按钮与提示随「打开方式」变化（等待中不改文案，交给引擎的 applyState） */
function syncLoginModeHint() {
  workbuddyLogin?.syncTexts();
}

/**
 * 「恢复 Google / GitHub 入口」的可用条件：**国际版 + 内嵌窗口**。
 *
 *   - 系统浏览器模式下页面跑在你日常浏览器里，我们没有注入能力（也不该有 ——
 *     那要往用户自己的浏览器塞脚本），上游隐藏了入口就是隐藏了；
 *   - 国内版登录页根本没有这两个入口（它的登录方式是微信 / 手机号 / 邮箱 / SSO），
 *     壳侧也会忽略这个值。
 *
 * 不满足时把开关置灰，避免给出一个「选了不生效」的假选项。
 */
function syncSocialRestoreState() {
  const box = $('add-social-restore');
  if (!box) return;
  const intl = selectedEdition() === 'intl';
  const embedded = selectedLoginMode() === 'embedded';
  const usable = intl && embedded;
  box.disabled = !usable;
  const label = box.closest('label');
  if (label) {
    label.title = usable
      ? '国际版登录页默认只显示邮箱登录，勾选后恢复 Google / GitHub / X 入口'
      : !intl
        ? '国内版登录页没有 Google / GitHub 入口（它用微信 / 手机号 / 邮箱登录）'
        : '只有「内嵌窗口」能恢复第三方入口：系统浏览器里我们无法改动登录页';
  }
  syncLoginModeHint();
}

// ─── WorkBuddy 的网页登录（引擎配置 + 两个入口按钮）──────────
//
// 打开方式（打开方式分段值）只影响这一家的文案：international 版默认走系统浏览器
// （可复用已有登录态，见 index.html 的分段默认值）。小浣熊没有这一级
// （它的回调是自定义协议深链，只有内嵌窗口才收得到，见 src-tauri/src/login.rs）。

const workbuddyLogin = window.wbWebLogin.create({
  provider: 'workbuddy',
  buttonId: 'web-login-button',
  cancelId: 'web-login-cancel',
  hintId: 'web-login-hint',
  busyText: '等待网页登录…',
  texts: () => {
    const external = selectedLoginMode() === 'external';
    return {
      button: external ? '在浏览器中打开登录页' : '打开网页登录',
      hint: external
        ? '将用系统默认浏览器打开，登录完成后自动加入账号列表；关掉此窗口即取消等待'
        : '将打开内嵌窗口，登录完成后自动加入账号列表；关掉此窗口即取消等待',
    };
  },
  start: () =>
    api.startLogin(selectedEdition(), selectedLoginMode(), 'workbuddy', socialRestoreEnabled()),
  onSuccess: async () => {
    const editionLabel = selectedEdition() === 'intl' ? '国际版' : '国内版';
    closeModal();
    await refresh();
    toast(`✅ 登录成功，${editionLabel}账号已加入列表`);
  },
});

// 关窗就是放弃等待：closeModal 里统一处理（两家同一出口）

// 报表页那个「添加账号」按钮已随会话状态卡片一起删除，现在只剩账号页这一个：
// 加可选链是必需的 —— 上面几行与它无关，但这里一抛错，后面所有监听都注册不上。
$('btn-add-account')?.addEventListener('click', openModal);
$('btn-add-account-2').addEventListener('click', openModal);

$('close-modal').addEventListener('click', closeModal);
$('add-modal').addEventListener('click', event => { if (event.target === $('add-modal')) closeModal(); });
$('web-login-button').addEventListener('click', () => workbuddyLogin.start());
$('web-login-cancel').addEventListener('click', () => workbuddyLogin.cancel());
// 版本与打开方式这两处分段的交互已随岛走（见 mountAddSegs 里的两个 onChange）：
// 切版本要联动打开方式（国际版默认系统浏览器、国内版默认内嵌窗口），
// 打开方式又影响第三方入口开关的可用态，两处都会顺带刷新提示文案。
$('add-social-restore')?.addEventListener('change', () => {
  // 勾选本身只影响发起登录时传给壳侧的值，不需要重建界面，但要让提示保持最新
  syncLoginModeHint();
});
document.addEventListener('keydown', event => { if (event.key === 'Escape') closeModal(); });

// 挂弹窗里两处静态分段控件的岛。放在文件末尾：那时 segState / segIslands 都已初始化。
mountAddSegs();

// 跨模块入口：别的页面要打开这张弹窗时走它（弹窗是全局的，调用方不必先跳账号页，
// 见 models-panel.js 的「＋ 新建自定义提供商」）。不把 openModal / closeModal 直接
// 留在全局：keys-panel.js 里另有一个同名的 openModal（作用域在自己的 IIFE 内），
// 同名函数散在全局迟早有人接错线。
window.wbAddAccountModal = { open: openModal, close: closeModal };

// 登录进行状态由主进程推送，按钮复位在 web-login.js 的引擎里统一处理
// （它是唯一知道「等待中的是哪一家」的地方，见 applyShellState）。
