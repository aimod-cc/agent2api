/* 「登录 / 添加账号」弹窗：WorkBuddy 区块的交互（分段控件取值、登录方式联动、
   粘贴登录态 JSON）以及弹窗自己的事件绑定。

   网页登录的**流程**不在这个文件里：WorkBuddy 与小浣熊共用 ui/web-login.js 的
   同一个引擎（等待态、取消、按钮复位、成功收尾），这里只交一份配置并接住
   弹窗自己的那两个分段控件（登录方式 / 打开方式）。

   依赖 app.js 的顶层全局（经典 script 的顶层声明在全局可见）：api / $ / toast / refresh /
   busy / releaseBusy。脚本顺序见 index.html：必须排在 app.js 之后（上面这些要先就位），
   且在 web-login.js 之后（加载期就要用它建 WorkBuddy 的控制器）、
   在 account-panel.js 之前 —— 后者也在两个「添加账号」按钮上挂监听，
   同元素同事件按注册顺序触发。弹窗 DOM 写在 index.html 里，脚本执行到这里时已解析就位。 */

// ─── 添加账号弹窗 ─────────────────────────────

function openModal() {
  $('add-modal').classList.add('open');
  syncLoginMethod();
  syncLoginModeHint();
  // 以主进程真实状态复位按钮：上次若在等待中被关窗，这里会重新可用
  void window.wbWebLogin?.refresh();
}

function closeModal() {
  $('add-modal').classList.remove('open');
  $('auth-json-input').value = '';
  $('upload-button').disabled = true;
  $('account-name').value = '';
  // 关窗即放弃等待：通知主进程中止后端轮询，否则按钮会一直卡在禁用态。
  // 不传 provider = 「谁在等待就取消谁」：两家共用同一个弹窗，这里无需区分。
  void window.wbWebLogin?.cancelIfActive('');
}

/**
 * 弹窗里的选择项都是分段控件（.seg，结构见 index.html 的 #add-modal、动态部分见
 * account-panel.js）：选中态就落在 DOM 上 —— 选中项带 .active 与 aria-checked="true"，
 * 取值放在 data-value。因此这里只读 / 写这一处状态，不再保留隐藏 radio 那第二份状态
 * （两份状态要靠 change 事件互相同步，edit 逻辑稍不留神就会不一致）。
 * 点击与键盘交互由 account-panel.js 的 bindSeg 统一绑定（切换后派发 wb-seg-change）。
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

/** 第二级：网页登录的打开方式（embedded=内嵌窗口 / external=系统默认浏览器） */
function selectedLoginMode() {
  return segValue('add-login-mode') === 'external' ? 'external' : 'embedded';
}

function setLoginMode(mode) {
  setSegValue('add-login-mode', mode);
  syncLoginModeHint();
}

/** 第一级：登录方式（web=网页登录 / json=粘贴登录态 JSON） */
function selectedLoginMethod() {
  return segValue('add-login-method') === 'json' ? 'json' : 'web';
}

/**
 * 第一级切换：网页登录 → 露出第二级（打开方式 + 登录按钮），粘贴 → 只留 JSON 表单。
 *
 * 切到粘贴时若正在等待**本家**的登录，就顺手取消：此时候入口（第二级）已整体藏起来，
 * 用户看不见「取消等待」按钮，但主进程的轮询还在跑 —— 一旦登录成功，
 * 收尾会关窗并刷新列表，而用户此刻多半正在粘 JSON，界面会莫名其妙自己跳一下。
 * 取消走共用的 wbWebLogin.cancelIfActive（并传 provider，别顺带取消别家的登录：
 * 这个分段只负责收起 WorkBuddy 那一段 UI）。
 */
function syncLoginMethod() {
  const pasting = selectedLoginMethod() === 'json';
  const web = $('add-web-login-block');
  const json = $('add-json-block');
  if (web) web.hidden = pasting;
  if (json) json.hidden = !pasting;
  if (pasting) { void window.wbWebLogin?.cancelIfActive('workbuddy'); return; }
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

async function uploadAccount() {
  const text = $('auth-json-input').value.trim();
  if (!text || busy) return;
  busy = true;
  const edition = selectedEdition();
  const button = $('upload-button');
  button.disabled = true;
  try {
    JSON.parse(text); // 先做本地格式校验，再交给后端
    await api.uploadAccount($('account-name').value.trim(), text, edition);
    closeModal();
    await refresh();
    toast(`账号已导入（${edition === 'intl' ? '国际版' : '国内版'}）`);
  } catch (error) {
    toast(`导入失败：${error.message}`, 'err');
  } finally {
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
    button.disabled = !$('auth-json-input').value.trim();
  }
}

$('btn-add-account').addEventListener('click', openModal);
$('btn-add-account-2').addEventListener('click', openModal);

$('close-modal').addEventListener('click', closeModal);
$('add-modal').addEventListener('click', event => { if (event.target === $('add-modal')) closeModal(); });
$('web-login-button').addEventListener('click', () => workbuddyLogin.start());
$('web-login-cancel').addEventListener('click', () => workbuddyLogin.cancel());
$('upload-button').addEventListener('click', uploadAccount);
// 弹窗里的三个分段控件（版本 / 登录方式 / 打开方式）：点击与键盘交互由 account-panel.js
// 的 bindSeg 统一绑定，切换后派发 wb-seg-change —— 这里只接结果，各自读回选中值即可。
//   版本 → 国际版默认用系统浏览器（可复用已有登录态），国内版默认回到内嵌窗口
//   打开方式 → 只影响登录按钮文案与提示
//   登录方式 → 决定第二级与 JSON 表单的显隐（顺带收掉等待中的登录）
$('add-edition-seg').addEventListener('wb-seg-change', () => {
  setLoginMode(selectedEdition() === 'intl' ? 'external' : 'embedded');
});
$('add-login-mode').addEventListener('wb-seg-change', syncLoginModeHint);
$('add-login-method').addEventListener('wb-seg-change', syncLoginMethod);
$('auth-json-input').addEventListener('input', () => {
  $('upload-button').disabled = !$('auth-json-input').value.trim();
});
document.addEventListener('keydown', event => { if (event.key === 'Escape') closeModal(); });

// 登录进行状态由主进程推送，按钮复位在 web-login.js 的引擎里统一处理
// （它是唯一知道「等待中的是哪一家」的地方，见 applyShellState）。
