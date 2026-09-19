/* 「登录 / 添加账号」弹窗：WorkBuddy 的账号版本、网页登录打开方式与弹窗事件。

   网页登录流程由 web-login.js 统一管理，这里只提供 WorkBuddy 的配置。
   依赖 app.js 的顶层全局 api / $ / toast / refresh，以及 web-login.js 的控制器。
   本文件必须在 add-provider-forms.js 之前加载：同一添加按钮先打开弹窗，再同步提供商。 */

// ─── 添加账号弹窗 ─────────────────────────────

function openModal() {
  $('add-modal').classList.add('open');
  syncLoginModeHint();
  // 以主进程真实状态复位按钮：上次若在等待中被关窗，这里会重新可用
  void window.wbWebLogin?.refresh();
}

function closeModal() {
  $('add-modal').classList.remove('open');
  // 关窗即放弃等待：通知主进程中止后端轮询，否则按钮会一直卡在禁用态。
  // 不传 provider = 「谁在等待就取消谁」：两家共用同一个弹窗，这里无需区分。
  void window.wbWebLogin?.cancelIfActive('');
}

/**
 * 弹窗里的选择项都是分段控件（.seg，结构见 index.html 的 #add-modal、动态部分见
 * add-provider-forms.js）：选中态就落在 DOM 上 —— 选中项带 .active 与 aria-checked="true"，
 * 取值放在 data-value。因此这里只读 / 写这一处状态，不再保留隐藏 radio 那第二份状态
 * （两份状态要靠 change 事件互相同步，edit 逻辑稍不留神就会不一致）。
 * 点击与键盘交互由 add-provider-forms.js 的 bindSeg 统一绑定（切换后派发 wb-seg-change）。
 */
function segValue(id) {
  return document.querySelector(`#${id} .seg-item.active`)?.dataset.value || '';
}

function setSegValue(id, value) {
  document.querySelectorAll(`#${id} .seg-item`).forEach(item => {
    const on = item.dataset.value === value;
    item.classList.toggle('active', on);
    item.setAttribute('aria-checked', String(on));
    item.tabIndex = on ? 0 : -1;
  });
}

/** 弹窗里选的账号版本（cn=国内版 / intl=国际版） */
function selectedEdition() {
  return segValue('add-edition-seg') === 'intl' ? 'intl' : 'cn';
}

/** 网页登录的打开方式（embedded=内嵌窗口 / external=系统默认浏览器） */
function selectedLoginMode() {
  return segValue('add-login-mode') === 'external' ? 'external' : 'embedded';
}

function setLoginMode(mode) {
  setSegValue('add-login-mode', mode);
  syncLoginModeHint();
}

/** 登录按钮与提示随「打开方式」变化（等待中不改文案，交给引擎的 applyState） */
function syncLoginModeHint() {
  workbuddyLogin?.syncTexts();
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
  start: () => api.startLogin(selectedEdition(), selectedLoginMode(), 'workbuddy'),
  onSuccess: async () => {
    const editionLabel = selectedEdition() === 'intl' ? '国际版' : '国内版';
    closeModal();
    await refresh();
    toast(`✅ 登录成功，${editionLabel}账号已加入列表`);
  },
});

// 关窗就是放弃等待：closeModal 里统一处理（两家同一出口）

$('btn-add-account').addEventListener('click', openModal);
$('btn-add-account-2').addEventListener('click', openModal);

$('close-modal').addEventListener('click', closeModal);
$('add-modal').addEventListener('click', event => { if (event.target === $('add-modal')) closeModal(); });
$('web-login-button').addEventListener('click', () => workbuddyLogin.start());
$('web-login-cancel').addEventListener('click', () => workbuddyLogin.cancel());
// 版本与打开方式的分段交互由 add-provider-forms.js 绑定，这里接收选中项变化。
// 国际版默认使用系统浏览器，国内版默认使用内嵌窗口。
$('add-edition-seg').addEventListener('wb-seg-change', () => {
  setLoginMode(selectedEdition() === 'intl' ? 'external' : 'embedded');
});
$('add-login-mode').addEventListener('wb-seg-change', syncLoginModeHint);
document.addEventListener('keydown', event => { if (event.key === 'Escape') closeModal(); });

// 登录进行状态由主进程推送，按钮复位在 web-login.js 的引擎里统一处理
// （它是唯一知道「等待中的是哪一家」的地方，见 applyShellState）。
