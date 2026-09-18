/* WorkBuddy 本地代理 · 桌面端渲染层 */
/* global workbuddyDesktop */

const api = window.workbuddyDesktop;
const $ = id => document.getElementById(id);
let state = null;
let currentConfig = null;
// busy 是「全局一次只干一件事」的互斥锁：刷新、切换账号、登录、保存配置等都要先占它。
// 注意它与 account-panel.js 里的 panelBusy 是两把互不相干的锁：弹窗内的保存不占这把锁。
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
  // 同步操作系统标题栏的深浅色：界面切深色但标题栏仍是系统主题时，顶部会留一条白带。
  // 浏览器直开时没有桥，用可选链 + try 吞掉，避免影响主题本身生效。
  // 跟随系统时必须传 null 把窗口主题交回系统，不要在这里把它整形回 dark/light 两态：
  // 窗口一旦被手动主题钉住，WebView2 的 prefers-color-scheme 会跟着窗口走而非系统，
  // 下面读到的就是被污染的值，界面会永远卡死在手动主题上。
  // 这行还要排在读 matchMedia 之前：先交回系统才读得到真实值；
  // 交回系统是异步生效的，读到旧值时靠「change 监听 + 落定后复核」双保险兜住。
  let pendingWindowTheme;
  try {
    pendingWindowTheme = window.workbuddyDesktop?.setWindowTheme?.(mode === 'system' ? null : mode);
  } catch { /* 非桌面环境忽略 */ }
  // system 模式下 color-scheme 也要跟随系统，否则原生控件会停留在浅色
  const systemDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
  const effective = mode === 'dark' || (mode === 'system' && systemDark) ? 'dark' : 'light';
  document.documentElement.style.colorScheme = effective;
  // 双保险①：文件末尾的 change 监听，兜「运行中系统主题变化」（以及多数切回 system 的场景）。
  // 双保险②：这里的落定后复核，兜「切回 system 时 WebView2 未必派发 change」——
  // 窗口主题真正恢复后若与本次判定不符，说明本次读到的是污染值，就按真实值再算一遍；
  // 再算时 matchMedia 已是真实值、条件自然不成立，所以最多多跑一轮，不会递归下去。
  // 非桌面环境下桥返回 undefined，可选链保证整条链静默跳过。
  pendingWindowTheme?.then?.(() => {
    // 手动 dark/light 是同步钉死窗口主题的语义，不存在污染，无需复核
    if (mode !== 'system') return;
    // 复核期间用户若已改成手动主题，以用户选择为准，不能拿本次判定覆盖回去
    if ((localStorage.getItem('workbuddy-desktop-theme') || 'system') !== 'system') return;
    const settledDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
    if ((settledDark ? 'dark' : 'light') !== effective) applyTheme('system');
  })?.catch?.(() => { /* 命令失败时不做复核，界面保持本次结果 */ });
  localStorage.setItem('workbuddy-desktop-theme', mode);
  document.querySelectorAll('#theme-switch button').forEach(item => {
    item.classList.toggle('active', item.dataset.mode === mode);
  });
}

// ─── 页面导航 ────────────────────────────────

const PAGE_KEY = 'workbuddy-desktop-page';
const PAGES = ['overview', 'accounts', 'gateway', 'desensitize', 'logs', 'settings'];
/** 页签中文名：顶栏面包屑用。overview 的用户可见名是「报表」（内部标识保持不变：
 *  localStorage 记忆、showPage 与 CSS 的 [data-page] 选择器都依赖它） */
const PAGE_LABELS = {
  overview: '报表',
  accounts: '账号',
  gateway: '网关',
  desensitize: '脱敏',
  logs: '日志',
  settings: '设置',
};

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
  const crumb = $('crumb-page');
  if (crumb) crumb.textContent = PAGE_LABELS[page] || page;
  if (persist) localStorage.setItem(PAGE_KEY, page);
  // 切到报表页时拉一次统计（面板内部自持时间范围与数据，这里只做转发）
  if (page === 'overview') {
    window.wbReport?.load?.();
  }
  // 切到日志页时清掉未读标记，并立即拉一次最新日志
  if (page === 'logs') {
    clearLogsBadge();
    window.wbLogsPanel?.load?.();
  }
  // 切到设置页时拉一次启动设置与网关地址（面板内部自持状态，这里只做转发）
  if (page === 'settings') {
    window.wbSettingsPanel?.load?.();
  }
  renderTopbarStatus();
  // 导航项上的内容随页面变：日志未读徽标交给日志面板，更新提示在这里重画
  // （校验更新提示的可见性与所在页面有关，见 syncUpdateBadge）
  syncUpdateBadge();
}

// ─── 顶栏状态 ─────────────────────────────────

/** 顶栏右侧状态区：随当前页给出该页最关心的两三个状态 */
function renderTopbarStatus() {
  const box = $('topbar-status');
  if (!box) return;
  const health = state?.health || {};
  const session = state?.session || {};
  const accounts = state?.accounts?.accounts || [];
  const enabled = accounts.filter(a => a.enabled !== false).length;
  // accounts-model.js 在 app.js 之后加载，首屏这次调用可能早于它就绪，故用可选链
  const isRateLimited = window.wbAccountsModel?.isRateLimited;
  const limited = isRateLimited ? accounts.filter(a => a.enabled !== false && isRateLimited(a)).length : 0;
  const up = health.upstreamConfigured;
  const chip = (text, kind = '', optional = false) =>
    `<span class="badge ${kind}${optional ? ' optional' : ''}"><span class="dot"></span>${esc(text)}</span>`;
  const port = gatewayBase.replace(/^https?:\/\//, '');
  /** 直接复用某面板已渲染的徽标文案（词表 / 日志面板自持状态，这里不重复计算） */
  const mirror = id => {
    const badge = $(id);
    if (!badge || !badge.textContent || badge.textContent === '—') return '';
    return `<span class="badge ${badge.className.replace('badge', '').trim()}">${esc(badge.textContent)}</span>`;
  };

  const views = {
    accounts: () => chip(`${enabled} 个启用`, enabled ? 'ok' : '')
      + (limited ? chip(`${limited} 个已限流`, 'warn') : ''),
    gateway: () => (up ? chip('监听 127.0.0.1', 'ok') : chip('未就绪', 'bad')) + chip(port, '', true),
    desensitize: () => mirror('desensitize-badge'),
    logs: () => mirror('logs-badge'),
    settings: () => (up ? chip('网关运行中', 'ok', true) : chip('未就绪', 'bad', true))
      + (enabled ? chip(`${enabled} 个账号启用`) : ''),
    overview: () => (up ? chip('网关运行中', 'ok') : chip('未就绪', 'bad'))
      + (session.loggedIn ? chip('已登录', 'ok') : chip('未登录', 'warn')),
  };

  box.innerHTML = views[currentPage]?.() ?? views.overview();
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

// ─── 新版本可用提示 ───────────────────────────

/** 最近一次检查结果。存下来是为了换页时能重画提示（检查本身只在启动与手动触发时跑） */
let lastUpdateInfo = null;
/**
 * 已经看过提示的版本号。看过一次就没必要每次换页再闪一遍 ——
 * 与日志未读徽标的处理同一个取向（不打扰）。存版本号而不是布尔值：
 * 之后又发了更新的版本，提示该重新出现。
 */
let seenUpdateVersion = '';

/**
 * 按当前状态重画「设置」导航项上的更新提示。
 * 更新是设置页里的功能，所以提示挂在设置项上，不新增顶级页面。
 * 无更新、检查失败、版本号无法比较一律不显示（启动阶段的网络失败不该打扰用户）；
 * 人已经在设置页时也不显示 —— 面板里的「有新版本」徽标已经把话说完了。
 */
function syncUpdateBadge() {
  const badge = $('nav-count-update');
  if (!badge) return;
  const info = lastUpdateInfo;
  const latest = String(info?.latestVersion || '').trim();
  // 进设置页即视为「已看到」：结果与更新日志就在那个面板里
  if (currentPage === 'settings') seenUpdateVersion = latest;
  if (info?.hasUpdate !== true || !latest || latest === seenUpdateVersion) {
    badge.style.display = 'none';
    return;
  }
  // 只放一个「新」字而不是数字：这里没有「未读条数」的含义，
  // 写成数字容易被误会成还有多少个版本可以更新
  badge.textContent = '新';
  badge.title = `发现新版本 ${latest}（当前 ${info.currentVersion || '未知'}），`
    + '点开「设置 - 软件更新」可查看更新日志并下载';
  badge.style.display = '';
}

/** 检查更新结束后由软件更新面板调用（result 为 null 表示检查失败） */
function updateUpdateBadge(info) {
  lastUpdateInfo = info || null;
  syncUpdateBadge();
}

// ─── 渲染：会话状态 ────────────────────────────

function renderSession() {
  const session = state?.session || {};
  const health = state?.health || {};
  const badge = $('session-badge');
  const currentAccount = state?.accounts?.accounts?.find(a => a.id === state.accounts.currentAccountId);

  // title 还被「加载失败」用来挂后端不可达的原因（见 refresh 的 catch），
  // 每次重渲染都先清空：否则恢复后鼠标悬停仍会读到上一次的失败提示
  badge.title = '';

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
  renderSidebarStatus();
}

/**
 * 侧边栏底部的网关状态：常驻显示，不必切到「网关」页也能确认服务是否在监听。
 *
 * 端口来自 gatewayBase（真实端口由 getBackendStatus 异步补上），
 * 就绪与否看 state.health.upstreamConfigured —— 与顶栏徽标同一判定口径，
 * 避免两处对「网关是否可用」给出不同答案。
 * 首次渲染时 state 还没到，显示「正在检查…」而不是谎报未就绪。
 */
function renderSidebarStatus() {
  const dot = $('sidebar-status-dot');
  const text = $('sidebar-status-text');
  if (!dot || !text) return;
  const port = gatewayBase.replace(/^https?:\/\//, '');
  const health = state?.health;
  if (!health) {
    dot.className = 'live off';
    text.textContent = `正在检查… · ${port}`;
    return;
  }
  const up = health.upstreamConfigured;
  dot.className = up ? 'live pulse' : 'live bad';
  text.textContent = up ? `网关运行中 · ${port}` : `网关未就绪 · ${port}`;
  const box = $('sidebar-status');
  if (box) {
    box.title = up
      ? `本地网关正在监听 ${gatewayBase}，可直接调用 OpenAI 兼容接口`
      : `上游未就绪（${health.upstreamBaseUrl || '未配置'}），网关暂时无法转发请求`;
  }
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
    setModelsCount(0, 0);
    return;
  }
  // 倍率统一展示为官方风格 "0.03x"：兼容远程 "x0.03" 与内置 "x0.03 credits" 两种写法
  const formatCredits = c => {
    const m = /x\s*([\d.]+)/i.exec(c || '');
    return m ? `${m[1]}x` : (c || '');
  };
  // 芯片整块可点：点击即复制模型 ID（复制逻辑在 initCopyButtons 里统一处理）
  box.innerHTML = models.map(m => {
    const cls = m.isDefault ? 'model-chip default' : 'model-chip';
    const credits = m.credits ? `<span class="credits">${esc(formatCredits(m.credits))}</span>` : '';
    const name = m.name && m.name !== m.id ? `<span class="name">${esc(m.name)}</span>` : '';
    return `<button type="button" class="${cls}" data-copy="${esc(m.id)}" title="点击复制模型 ID：${esc(m.id)}">`
      + `<span class="id">${esc(m.id)}</span>${m.isDefault ? '<span class="name">（默认）</span>' : ''}${name}${credits}</button>`;
  }).join('');

  applyModelFilter();
  setModelsCount(models.length, models.length);

  const def = models.find(m => m.isDefault) || models[0];
  if (def) {
    const badge = $('default-model-badge');
    badge.textContent = `默认模型 ${def.id}`;
    badge.style.display = '';
    $('default-model').textContent = def.id;
  }
}

/** 模型清单计数：搜索时显示「命中 / 全部」 */
function setModelsCount(shown, total) {
  const box = $('models-count');
  if (!box) return;
  box.textContent = shown === total ? `共 ${total} 个可用模型 · 点击芯片复制 ID` : `匹配 ${shown} / ${total} 个模型`;
}

/** 按搜索框内容过滤模型芯片（本地过滤，不再请求后端） */
function applyModelFilter() {
  const box = $('models');
  const input = $('model-search');
  if (!box || !input) return;
  const keyword = input.value.trim().toLowerCase();
  const chips = [...box.querySelectorAll('.model-chip')];
  if (!chips.length) return;
  let shown = 0;
  chips.forEach(chip => {
    const hit = !keyword || chip.textContent.toLowerCase().includes(keyword);
    chip.classList.toggle('filtered', !hit);
    if (hit) shown++;
  });
  setModelsCount(shown, chips.length);
  const empty = box.querySelector('.models-empty');
  if (shown === 0) {
    if (!empty) {
      box.insertAdjacentHTML('beforeend', `<div class="empty models-empty" style="padding:14px 0;width:100%">没有匹配「${esc(input.value.trim())}」的模型</div>`);
    }
  } else {
    empty?.remove();
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

/**
 * 「本地代理状态」卡片已随报表页改造删除（原有信息与侧栏底部的网关状态、
 * 顶栏徽标重复）。这里保留判空只是为了让历史书签 / 旧 DOM 不报错：
 * 元素在就写，不在就静默跳过 —— 不该为了一个已删除的展示位把 render() 拖崩。
 */
function renderProxyStatus() {
  const box = $('proxy-status');
  if (!box) return;
  const health = state?.health;
  if (!health?.upstreamConfigured) {
    box.innerHTML = `<span style="color:var(--danger)">不可用：${esc(health?.unavailableReason || '无法连接代理')}</span>`;
    return;
  }
  // 首选项取自列表的派生值（转发顺序第一位）；具体某次请求走谁还受按模型的限额影响
  const snapshot = state?.accounts;
  const current = snapshot?.accounts?.find(a => a.id === snapshot.currentAccountId);
  const who = current ? (current.nickname || current.name || current.uid || current.id) : '—';
  box.textContent = `运行正常 · ${health.upstreamBaseUrl || ''} · 首选账号 ${who}（P${current?.priority ?? '—'}）`;
}

function render() {
  renderSession();
  renderAccountsView();
  renderGateway();
  renderModels();
  renderConfig();
  renderProxyStatus();
  renderNavCounts();
  renderTopbarStatus();
  renderSidebarStatus();
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

/**
 * 忙碌期间被压下的刷新请求（合并成一次待办）。
 * 为什么需要它：refresh() 过去在 busy 时直接 return，请求被静默丢弃 ——
 * 保存账号设置后主动调刷新，恰好撞上 20 秒轮询或别的操作，这次刷新就白丢了，
 * 界面只能等下一次轮询才更新（用户看到的就是「保存完十几秒才变」）。
 */
let refreshQueued = false;

/**
 * 释放 busy 锁，并把排队的刷新补跑掉。
 *
 * 为什么所有持锁函数都要经由它释放（而不是各自写 busy = false）：
 * 锁是共用的，排队标记却可能是在别人持锁期间被置上的（比如轮询撞上「等待网页登录」）。
 * 只在 refresh() 自己的 finally 里补跑，遇到「锁被非刷新操作持有」时就会漏掉这次请求，
 * 又要等下一个 20 秒。统一从这里释放，才能保证「锁一放开就补跑」。
 *
 * 补跑用 void 触发、不 await：调用方（如 runAccountAction）可能正持着锁，
 * 若在这里 await 补跑，就等于把锁借给别人、还会形成递归等待链。
 * 末尾挂 catch 只做兜底 —— refresh() 正常会把错误渲染成「加载失败」，
 * 但 render() 自身抛错时 Promise 会拒绝，void 出去的拒绝没人接就变成控制台噪音。
 * 此处 busy 已提前置 false，故补跑的那次 refresh() 一定拿得到锁。
 */
function releaseBusy() {
  busy = false;
  if (!refreshQueued) return;
  refreshQueued = false;
  refresh().catch(() => { /* 兜底：避免 void 出去的 Promise 拒绝无人处理 */ });
}

async function refresh() {
  // 忙碌时不丢弃请求，先排队；多次请求合并成一次补跑，避免连续触发时打出一串重复请求。
  // 这里立即返回（不等补跑完成）：调用方可能正持着锁等这个 Promise（如 runAccountAction
  // 内的 await refresh()），若返回的 Promise 依赖锁释放，就会自己把自己锁死。
  if (busy) { refreshQueued = true; return; }
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
    // 「本地代理状态」卡片已随报表页改造删除，加载失败改挂到会话徽标的 title 上：
    // 徽标此时本来就显示「⚠️ 未登录」，悬停即可看到真实原因（后端不可达 / 接口报错）。
    // 不用 toast：refresh() 每 20 秒轮询一次，后端长时间不可用会变成刷屏。
    const badge = $('session-badge');
    if (badge) badge.title = `加载失败：${error.message}`;
    console.warn('加载状态失败:', error.message);
  } finally {
    releaseBusy();
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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
    button.disabled = !$('auth-json-input').value.trim();
  }
}

// ─── 复制 ─────────────────────────────────────

/**
 * 复制按钮统一入口。两种用法：
 *   data-copy="文本"          直接复制给定文本（模型芯片）
 *   data-copy-from="元素 id"  复制该元素的当前文本（地址 / UID 等会变的值）
 * 地址类文本带 "POST " / "GET " 前缀，复制时去掉，保证粘出去能直接用。
 */
function copyTextOf(trigger) {
  const fromId = trigger.dataset.copyFrom;
  if (fromId) {
    const source = $(fromId);
    if (!source) return '';
    return source.textContent.replace(/^(POST|GET)\s+/i, '').trim();
  }
  return trigger.dataset.copy || '';
}

async function copyToClipboard(text) {
  if (!text) return false;
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // 剪贴板被拒（无权限 / 非安全上下文）时用兜底方案，避免功能静默失效
    try {
      const area = document.createElement('textarea');
      area.value = text;
      area.style.position = 'fixed';
      area.style.opacity = '0';
      document.body.appendChild(area);
      area.select();
      const ok = document.execCommand('copy');
      area.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

document.addEventListener('click', async event => {
  const trigger = event.target.closest('[data-copy], [data-copy-from]');
  if (!trigger) return;
  const text = copyTextOf(trigger);
  if (!(await copyToClipboard(text))) { toast('复制失败，请手动选择复制', 'err'); return; }
  toast(`已复制：${text.length > 46 ? `${text.slice(0, 46)}…` : text}`);
  // 复制按钮给个即时反馈（模型芯片本身是内容，不改它的外观）
  if (trigger.classList.contains('copy-btn')) {
    trigger.classList.add('done');
    trigger.textContent = '✓';
    setTimeout(() => {
      trigger.classList.remove('done');
      trigger.textContent = '⧉';
    }, 1200);
  }
});

$('model-search')?.addEventListener('input', applyModelFilter);

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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
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
    releaseBusy(); // 释放锁并补跑排队中的刷新（见 releaseBusy 注释）
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
  showPage,
  get currentPage() { return currentPage; },
  getState: () => state,
  runAccountAction,
  refresh,
  updateLogsBadge,
  // 软件更新面板在每次检查结束后回调它，把「有新版本」翻译成导航上的提示
  updateUpdateBadge,
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

/**
 * 图标注入：index.html 里的 .ico 与 .brand-logo 只留空占位，图形从这里填。
 * 放在 JS 而不是写死在 HTML 里，是为了让图标的定义集中在一处
 * （icons.js），后续换图标只改一个文件，不必在 HTML 里翻找。
 *
 * 品牌标与应用图标（icons/icon.png）同一造型，所以它也跟着这里注入 ——
 * 哪天图标换了，改 icons.js 的 brand 项即可，不会出现「窗口图标换了、
 * 侧栏还停在旧图形」这种不一致。
 */
function paintIcons() {
  const icon = window.wbIcons?.icon;
  if (!icon) return;
  document.querySelectorAll('.nav-item[data-icon] .ico').forEach(slot => {
    const name = slot.closest('.nav-item').dataset.icon;
    slot.innerHTML = icon(name, 17);
  });
  document.querySelectorAll('.brand-logo').forEach(slot => {
    slot.innerHTML = icon('brand', 32);
  });
}
paintIcons();

showPage(localStorage.getItem(PAGE_KEY) || 'overview', { persist: false });

refresh();

/**
 * 启动即自动检查一次更新：有新版本时在「设置」导航项上给提示。
 *
 * 放在 DOMContentLoaded 里而不是直接调用：app.js 在 index.html 里排得比
 * update-panel.js 靠前，脚本执行到这里时 window.wbUpdatePanel 还没挂上，
 * 直接调会静默什么都不做；DOMContentLoaded 在所有同步脚本执行完之后触发，
 * 那时面板已经就位。不 await（void 触发）—— 这是网络请求，首屏不该等它；
 * 失败静默（面板里留一条失败记录，导航提示不显示）。
 *
 * 面板的 load() 只读下载进度、不查版本，所以这里这一下不会和它重复请求。
 */
document.addEventListener('DOMContentLoaded', () => {
  void window.wbUpdatePanel?.check?.();
}, { once: true });

// 定时轮询：限额标记（429 + 恢复时间）与账号状态变化自动刷新；窗口隐藏时暂停
setInterval(() => { if (!document.hidden) refresh(); }, 20_000);