/* WorkBuddy 本地代理 · 桌面端渲染层 */
/* global workbuddyDesktop */

const api = window.workbuddyDesktop;
const $ = id => document.getElementById(id);
let state = null;
let currentConfig = null;
let busy = false;

// 3065 是官方默认端口，但主进程可能被 WORKBUDDY_PROXY_PORT 覆盖，
// 所以它只当兜底值：真实端口由 getBackendStatus() 异步补上（见 syncGatewayPort）。
const PROXY_PORT = 3065;
const PROXY_BASE = `http://127.0.0.1:${PROXY_PORT}`;

// ─── 工具 ────────────────────────────────────

function esc(value) {
  return String(value ?? '').replace(/[&<>"']/g, ch => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[ch]));
}

function toast(message, type = 'ok') {
  const element = $('toast');
  element.textContent = message;
  element.className = type;
  element.style.display = 'block';
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { element.style.display = 'none'; }, 3500);
}

function formatTime(value) {
  if (!value) return '';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '';
  return date.toLocaleString('zh-CN', { hour12: false });
}

/** 短时间：同天显示 HH:mm，跨天显示 MM-DD HH:mm（用于限额恢复时间） */
function formatShortTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '';
  const sameDay = date.toDateString() === new Date().toDateString();
  return date.toLocaleString('zh-CN', sameDay
    ? { hour: '2-digit', minute: '2-digit', hour12: false }
    : { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false });
}

function formatRemaining(ms) {
  if (!Number.isFinite(ms) || !ms) return '';
  const diff = ms - Date.now();
  if (diff <= 0) return '（已过期）';
  const minutes = Math.round(diff / 60000);
  if (minutes < 60) return `（${minutes} 分钟后过期）`;
  return `（${Math.round(minutes / 60)} 小时后过期）`;
}

// ─── 主题 ────────────────────────────────────

function applyTheme(mode) {
  document.documentElement.dataset.theme = mode;
  // system 模式下 color-scheme 也要跟随系统，否则原生控件会停留在浅色
  const systemDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
  const effective = mode === 'dark' || (mode === 'system' && systemDark) ? 'dark' : 'light';
  document.documentElement.style.colorScheme = effective;
  localStorage.setItem('workbuddy-desktop-theme', mode);
  document.querySelectorAll('#theme-switch button').forEach(item => {
    item.classList.toggle('active', item.dataset.mode === mode);
  });
}

// ─── 页面导航 ────────────────────────────────

const PAGE_KEY = 'workbuddy-desktop-page';
const PAGES = ['overview', 'accounts', 'gateway', 'desensitize', 'logs', 'settings'];

/** 当前页（子模块据此判断是否需要重新加载） */
let currentPage = 'overview';

function showPage(name, { persist = true } = {}) {
  const page = PAGES.includes(name) ? name : 'overview';
  currentPage = page;
  document.querySelectorAll('.page').forEach(section => {
    section.classList.toggle('active', section.dataset.page === page);
  });
  document.querySelectorAll('.nav-item').forEach(item => {
    item.classList.toggle('active', item.dataset.page === page);
  });
  if (persist) localStorage.setItem(PAGE_KEY, page);
  // 切到日志页时清掉未读标记，并立即拉一次最新日志
  if (page === 'logs') {
    clearLogsBadge();
    window.wbLogsPanel?.load?.();
  }
  // 切到设置页时拉一次启动设置与网关地址（面板内部自持状态，这里只做转发）
  if (page === 'settings') {
    window.wbSettingsPanel?.load?.();
  }
}

// ─── 日志未读徽标 ─────────────────────────────

let lastSeenLogId = Number(localStorage.getItem('workbuddy-desktop-log-seen') || 0);

function clearLogsBadge() {
  const badge = $('nav-count-logs');
  if (badge) badge.style.display = 'none';
  const stats = window.wbLogsPanel?.lastStats?.();
  if (stats?.lastId) {
    lastSeenLogId = stats.lastId;
    localStorage.setItem('workbuddy-desktop-log-seen', String(lastSeenLogId));
  }
}

/** 有新日志时在导航上显示未读数（只在非日志页显示，避免打扰当前阅读） */
function updateLogsBadge(stats) {
  const badge = $('nav-count-logs');
  if (!badge || !stats) return;
  const lastId = Number(stats.lastId) || 0;
  const warnCount = (stats.byLevel?.warn || 0) + (stats.byLevel?.error || 0);
  // 未读 = 上次已读之后新增的条数；首次运行把当前水位记为已读
  if (!lastSeenLogId) {
    lastSeenLogId = lastId;
    localStorage.setItem('workbuddy-desktop-log-seen', String(lastId));
    badge.style.display = 'none';
    return;
  }
  const unread = Math.max(0, lastId - lastSeenLogId);
  if (currentPage === 'logs' || !unread) {
    badge.style.display = 'none';
    if (currentPage === 'logs') {
      lastSeenLogId = lastId;
      localStorage.setItem('workbuddy-desktop-log-seen', String(lastId));
    }
    return;
  }
  badge.textContent = unread > 99 ? '99+' : String(unread);
  badge.title = `${unread} 条新日志（其中警告/错误 ${warnCount} 条）`;
  badge.style.display = '';
}

// ─── 渲染：会话状态 ────────────────────────────

function renderSession() {
  const session = state?.session || {};
  const health = state?.health || {};
  const badge = $('session-badge');
  const currentAccount = state?.accounts?.accounts?.find(a => a.id === state.accounts.currentAccountId);

  if (!health.upstreamConfigured) {
    badge.className = 'badge bad';
    badge.textContent = '⚠️ 未登录';
    badge.title = health.unavailableReason || '';
  } else if (!session.loggedIn) {
    badge.className = 'badge bad';
    badge.textContent = '未登录';
  } else {
    const expiresAt = Number(session.tokenExpiresAt) || 0;
    const expiringSoon = expiresAt && expiresAt - Date.now() < 5 * 60 * 1000;
    badge.className = `badge ${expiringSoon ? 'warn' : 'ok'}`;
    badge.textContent = expiringSoon ? '临期（将自动刷新）' : '已登录';
  }

  $('session-nickname').textContent = session.nickname || currentAccount?.nickname || currentAccount?.name || '—';
  $('session-uid').textContent = session.accountUid || currentAccount?.uid || '—';
  $('session-endpoint').textContent = session.endpoint || '—';
  const expiresAt = Number(session.tokenExpiresAt) || 0;
  $('session-expires').textContent = session.loggedIn
    ? `${formatTime(expiresAt)}${formatRemaining(expiresAt)}`
    : '—';

  // 退出登录 / 刷新 Token 是否可用，取决于后端是否有可用登录态
  $('btn-logout').disabled = !session.loggedIn;
  $('btn-refresh-token').disabled = !session.loggedIn;
}

// ─── 渲染：网关 / 模型 / 配置 ──────────────────

/** 当前展示的网关地址：默认端口起步，拿到真实端口后替换 */
let gatewayBase = PROXY_BASE;
let gatewayPortResolved = false;
let gatewayPortLoading = false;

/** 只负责写这 3 个元素，首次渲染与端口补更新共用，避免两处文案走偏 */
function paintGatewayAddress(base) {
  $('api-chat').textContent = `POST ${base}/v1/chat/completions`;
  $('api-models').textContent = `GET ${base}/v1/models`;
  $('api-base').textContent = `${base}/v1`;
}

/**
 * 显示给用户复制的地址必须是真实监听端口（否则复制出去连不上）。
 * render() 是同步的、调用方众多，所以这里先用兜底值渲染，再异步取真实端口补更新；
 * 取不到（抛错或字段缺失）就保持兜底值，页面照常显示，不留空也不报错。
 */
async function syncGatewayPort() {
  // 端口在进程启动时定死，成功拿到一次即长期有效；失败则留待下次渲染重试
  if (gatewayPortResolved || gatewayPortLoading) return;
  gatewayPortLoading = true;
  try {
    const status = await api.getBackendStatus();
    const port = Number(status?.port) || 0;
    if (!port) return;
    gatewayPortResolved = true;
    const next = `http://127.0.0.1:${port}`;
    if (next === gatewayBase) return;
    gatewayBase = next;
    paintGatewayAddress(next);
  } catch (error) {
    console.warn('读取后端端口失败，网关地址沿用默认端口:', error.message);
  } finally {
    gatewayPortLoading = false;
  }
}

function renderGateway() {
  paintGatewayAddress(gatewayBase);
  void syncGatewayPort();
}

function renderModels() {
  const box = $('models');
  const models = Array.isArray(state?.models) ? state.models : [];
  if (!models.length) {
    box.innerHTML = '<div class="empty" style="padding:14px 0">暂无模型</div>';
    return;
  }
  // 倍率统一展示为官方风格 "0.03x"：兼容远程 "x0.03" 与内置 "x0.03 credits" 两种写法
  const formatCredits = c => {
    const m = /x\s*([\d.]+)/i.exec(c || '');
    return m ? `${m[1]}x` : (c || '');
  };
  box.innerHTML = models.map(m => {
    const cls = m.isDefault ? 'model-chip default' : 'model-chip';
    const credits = m.credits ? `<span class="credits">${esc(formatCredits(m.credits))}</span>` : '';
    const name = m.name && m.name !== m.id ? `<span class="credits">${esc(m.name)}</span>` : '';
    return `<span class="${cls}">${esc(m.id)}${m.isDefault ? '（默认）' : ''}${name}${credits}</span>`;
  }).join('');

  const def = models.find(m => m.isDefault) || models[0];
  if (def) {
    const badge = $('default-model-badge');
    badge.textContent = `默认 ${def.id}`;
    badge.style.display = '';
    $('default-model').textContent = def.id;
  }
}

function renderConfig() {
  currentConfig = state?.config || { apiKeySet: false, apiKey: null };
  const { apiKeySet, apiKey } = currentConfig;
  $('api-key-input').placeholder = apiKeySet
    ? `当前已设置：${apiKey}（输入新值覆盖）`
    : '留空表示不启用鉴权；至少 8 个字符';
  $('api-key-status').textContent = apiKeySet
    ? `✅ 已启用，客户端请求需带 Authorization: Bearer <key>（当前 ${apiKey}）`
    : '未启用（仅监听 127.0.0.1，建议本机使用）';
}

function renderProxyStatus() {
  const health = state?.health;
  if (!health?.upstreamConfigured) {
    $('proxy-status').innerHTML = `<span style="color:var(--danger)">不可用：${esc(health?.unavailableReason || '无法连接代理')}</span>`;
    return;
  }
  // 首选项取自列表的派生值（转发顺序第一位）；具体某次请求走谁还受按模型的限额影响
  const snapshot = state?.accounts;
  const current = snapshot?.accounts?.find(a => a.id === snapshot.currentAccountId);
  const who = current ? (current.nickname || current.name || current.uid || current.id) : '—';
  $('proxy-status').textContent = `运行正常 · ${health.upstreamBaseUrl || ''} · 首选账号 ${who}（P${current?.priority ?? '—'}）`;
}

function render() {
  renderSession();
  renderAccountsView();
  renderGateway();
  renderModels();
  renderConfig();
  renderProxyStatus();
  renderNavCounts();
}

/** 账号列表由 accounts-view 模块负责（含优先级/禁用/代理徽章与行内面板） */
function renderAccountsView() {
  window.wbAccountsView?.render();
}

/** 导航上的数量徽标：账号数 / 日志未读 */
function renderNavCounts() {
  window.wbAccountsView?.renderNavCount();
}

// ─── 加载 ─────────────────────────────────────

async function refresh() {
  if (busy) return;
  busy = true;
  try {
    const next = await api.getState();
    state = next;
    // 配置面板（apiKey / defaultModel）与词表通过独立接口加载，避免阻塞状态
    await Promise.all([loadConfig(), loadDesensitize()]);
    // 清掉已删除账号的本地缓存；代理可选项也可能在 Clash 侧改过，下次打开弹窗重读
    const validIds = new Set((state.accounts?.accounts || []).map(a => a.id));
    window.wbAccountsView?.refreshCaches(validIds);
    window.wbAccountPanel?.invalidate();
    render();
  } catch (error) {
    state = null;
    render();
    $('proxy-status').innerHTML = `<span style="color:var(--danger)">加载失败：${esc(error.message)}</span>`;
  } finally {
    busy = false;
  }
}

async function loadConfig() {
  try {
    const cfg = await api.getConfig();
    // config 里的 desensitize 是精简摘要（无 terms），不要拿它覆盖完整词表状态
    state = { ...(state || {}), config: cfg };
    renderConfig();
  } catch (error) {
    console.warn('读取配置失败:', error.message);
  }
}

/** 词表面板自带状态，这里只做转发，避免再往主 state 里塞一份 */
function loadDesensitize() {
  return window.wbDesensitizePanel?.load();
}

// ─── 账号操作（列表按钮统一入口；积分/签到由 accounts-view 自行消化） ───

async function runAccountAction(action, id) {
  if (busy) return;
  // 设置是弹窗内的编辑流程，不占用列表的 busy 锁（弹窗自己管自己的保存态）
  if (action === 'settings') {
    window.wbAccountPanel?.open(id);
    return;
  }
  busy = true;
  try {
    if (action === 'switch') {
      await api.switchAccount(id);
      await refresh();
      toast('✅ 已设为首选账号（置顶转发顺序）');
    } else if (action === 'refresh') {
      await api.refreshAccountToken(id);
      await refresh();
      toast('✅ Token 已刷新');
    } else if (action === 'remove') {
      const account = state?.accounts?.accounts?.find(a => a.id === id);
      if (!confirm(`确定删除账号「${account?.nickname || account?.name || id}」？`)) return;
      await api.removeAccount(id);
      await refresh();
      toast('账号已删除');
    }
  } catch (error) {
    toast(`操作失败：${error.message}`, 'err');
  } finally {
    busy = false;
  }
}

// ─── 添加账号弹窗 ─────────────────────────────

function openModal() {
  $('add-modal').classList.add('open');
  syncLoginModeHint();
  // 以主进程真实状态复位按钮：上次若在等待中被关窗，这里会重新可用
  api.getLoginState()
    .then(state => applyLoginState(state?.active))
    .catch(() => applyLoginState(false));
}

function closeModal() {
  $('add-modal').classList.remove('open');
  $('auth-json-input').value = '';
  $('upload-button').disabled = true;
  $('account-name').value = '';
  // 关窗即放弃等待：通知主进程中止后端轮询，否则按钮会一直卡在禁用态
  if (loginActive) {
    api.cancelLogin()
      .then(() => toast('已取消登录等待'))
      .catch(() => {})
      .finally(() => applyLoginState(false));
  }
}

/** 弹窗里选的账号版本（cn=国内版 / intl=国际版） */
function selectedEdition() {
  const checked = document.querySelector('input[name="account-edition"]:checked');
  return checked?.value === 'intl' ? 'intl' : 'cn';
}

/** 弹窗里选的登录方式（embedded=内嵌窗口 / external=系统默认浏览器） */
function selectedLoginMode() {
  const checked = document.querySelector('input[name="login-mode"]:checked');
  return checked?.value === 'external' ? 'external' : 'embedded';
}

function setLoginMode(mode) {
  const input = document.querySelector(`input[name="login-mode"][value="${mode}"]`);
  if (input) input.checked = true;
  syncLoginModeHint();
}

/**
 * 登录状态（由主进程推送）：等待中时按钮显示「取消等待」。
 * 关键点：状态来自主进程而非本地变量，弹窗反复开关也不会把按钮卡在禁用态。
 */
let loginActive = false;

function applyLoginState(active) {
  loginActive = Boolean(active);
  const button = $('web-login-button');
  const cancel = $('web-login-cancel');
  if (!button) return;
  if (loginActive) {
    button.disabled = true;
    button.innerHTML = '<span class="spinner"></span>等待登录完成…';
    if (cancel) cancel.style.display = '';
  } else {
    button.disabled = false;
    if (cancel) cancel.style.display = 'none';
    syncLoginModeHint();
  }
}

/** 登录按钮与提示随登录方式变化（等待中不改文案，交给 applyLoginState） */
function syncLoginModeHint() {
  if (loginActive) return;
  const external = selectedLoginMode() === 'external';
  $('web-login-button').textContent = external ? '在浏览器中打开登录页' : '打开网页登录';
  $('web-login-hint').textContent = external
    ? '将用系统默认浏览器打开，登录完成后自动加入账号列表；关掉此窗口即取消等待'
    : '将打开内嵌窗口，登录完成后自动加入账号列表；关掉此窗口即取消等待';
}

async function startWebLogin() {
  if (busy || loginActive) return;
  busy = true;
  const edition = selectedEdition();
  const mode = selectedLoginMode();
  const editionLabel = edition === 'intl' ? '国际版' : '国内版';
  const button = $('web-login-button');
  button.disabled = true;
  button.innerHTML = '<span class="spinner"></span>等待网页登录…';
  try {
    const result = await api.startLogin(edition, mode);
    if (result?.canceled) {
      toast('已取消登录等待');
      return;
    }
    closeModal();
    await refresh();
    toast(`✅ 登录成功，${editionLabel}账号已加入列表`);
  } catch (error) {
    toast(`登录失败：${error.message}`, 'err');
  } finally {
    busy = false;
    // 以主进程状态为准复位按钮，避免这里与真实状态不一致
    const state = await api.getLoginState().catch(() => null);
    applyLoginState(state?.active);
  }
}

/** 取消等待中的登录（主进程会通知后端中止轮询） */
async function cancelWebLogin() {
  try {
    await api.cancelLogin();
    toast('已取消登录等待');
  } catch (error) {
    toast(`取消失败：${error.message}`, 'err');
  }
  applyLoginState(false);
}

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
    busy = false;
    button.disabled = !$('auth-json-input').value.trim();
  }
}

// ─── 事件绑定 ─────────────────────────────────

document.querySelectorAll('#theme-switch button').forEach(button => {
  button.addEventListener('click', () => applyTheme(button.dataset.mode));
});
window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  if ((localStorage.getItem('workbuddy-desktop-theme') || 'system') === 'system') applyTheme('system');
});
applyTheme(localStorage.getItem('workbuddy-desktop-theme') || 'system');

$('btn-refresh-state').addEventListener('click', () => refresh().then(() => toast('状态已刷新')));
$('btn-refresh-token').addEventListener('click', async () => {
  if (busy) return;
  busy = true;
  try {
    await api.refreshSession();
    await refresh();
    toast('✅ Token 已刷新');
  } catch (error) {
    toast(`刷新失败：${error.message}`, 'err');
  } finally {
    busy = false;
  }
});
$('btn-logout').addEventListener('click', async () => {
  if (!confirm('确定退出登录？这会把首选账号从列表里删除（可重新登录加回来）。')) return;
  if (busy) return;
  busy = true;
  try {
    await api.logout();
    await refresh();
    toast('已退出登录');
  } catch (error) {
    toast(`退出失败：${error.message}`, 'err');
  } finally {
    busy = false;
  }
});
$('btn-add-account').addEventListener('click', openModal);
$('btn-add-account-2').addEventListener('click', openModal);

$('btn-save-key').addEventListener('click', async () => {
  const value = $('api-key-input').value.trim();
  if (busy) return;
  busy = true;
  try {
    await api.saveConfig({ apiKey: value || null });
    $('api-key-input').value = '';
    await loadConfig();
    toast(value ? '✅ API Key 已保存' : 'API Key 已清除');
  } catch (error) {
    toast(`保存失败：${error.message}`, 'err');
  } finally {
    busy = false;
  }
});
$('btn-clear-key').addEventListener('click', async () => {
  if (busy) return;
  busy = true;
  try {
    await api.saveConfig({ apiKey: null });
    $('api-key-input').value = '';
    await loadConfig();
    toast('API Key 已清除');
  } catch (error) {
    toast(`清除失败：${error.message}`, 'err');
  } finally {
    busy = false;
  }
});

$('close-modal').addEventListener('click', closeModal);
$('add-modal').addEventListener('click', event => { if (event.target === $('add-modal')) closeModal(); });
$('web-login-button').addEventListener('click', startWebLogin);
$('web-login-cancel').addEventListener('click', cancelWebLogin);
$('upload-button').addEventListener('click', uploadAccount);
// 登录方式切换：只影响按钮文案与提示
document.querySelectorAll('input[name="login-mode"]').forEach(input => {
  input.addEventListener('change', syncLoginModeHint);
});
// 版本切换：国际版默认用系统浏览器（可复用已有登录态），国内版默认回到内嵌窗口
document.querySelectorAll('input[name="account-edition"]').forEach(input => {
  input.addEventListener('change', () => setLoginMode(selectedEdition() === 'intl' ? 'external' : 'embedded'));
});
$('auth-json-input').addEventListener('input', () => {
  $('upload-button').disabled = !$('auth-json-input').value.trim();
});
document.addEventListener('keydown', event => { if (event.key === 'Escape') closeModal(); });

api.onStateChanged(next => {
  if (!next?.accounts && !next?.session && !next?.health) return;
  // 合并：旧 state + 新字段。脱敏状态由词表面板自持，这里直接丢掉，避免
  // /api/session 的精简摘要（无 terms）覆盖掉完整词表。
  const { desensitize: _ignored, ...rest } = next;
  state = { ...(state || {}), ...rest };
  render();
  // 状态变更后顺带刷新一次词表，命中统计跟着更新
  void loadDesensitize();
});

// 登录进行状态由主进程推送：等待结束（成功/失败/取消）后按钮自动复位
api.onLoginState?.(payload => applyLoginState(payload?.active));

// ─── 共享给子模块（账号视图 / 词表面板 / 日志面板 / 账号设置面板）───
window.wbApp = {
  esc,
  toast,
  formatTime,
  formatShortTime,
  showPage,
  get currentPage() { return currentPage; },
  getState: () => state,
  runAccountAction,
  refresh,
  updateLogsBadge,
};

// ─── 启动自动维护：主进程会拉一次临期 token 刷新 + 余额查询 ───
api.onAutoMaintained?.(({ refreshed, balances }) => {
  const count = Array.isArray(refreshed) ? refreshed.length : 0;
  const applied = window.wbAccountsView?.applyBalances(balances) || 0;
  if (count || applied) window.wbAccountsView?.render();
  if (count) toast(`已自动刷新 ${count} 个临期账号的 Token，并更新积分余额`);
});

// ─── 初始化 ───────────────────────────────────

// 导航：点击切换页面，记住上次所在页
$('nav').addEventListener('click', event => {
  const item = event.target.closest('.nav-item[data-page]');
  if (item) showPage(item.dataset.page);
});
showPage(localStorage.getItem(PAGE_KEY) || 'overview', { persist: false });

refresh();

// 定时轮询：限额标记（429 + 恢复时间）与账号状态变化自动刷新；窗口隐藏时暂停
setInterval(() => { if (!document.hidden) refresh(); }, 20_000);