/* Agent2API · 桌面端渲染层 */
/* global workbuddyDesktop */

const api = window.workbuddyDesktop;
const $ = id => document.getElementById(id);
let state = null;
// 最近一次 `refresh()` 的失败原因（成功时清空）。
// 它是给顶栏状态区挂 title 用的：后端不可达时页面上其余数字都只是「上一次的值」，
// 而顶栏是常驻可见的那一处，把原因挂在那里用户悬停就能看到（见 renderTopbarStatus）。
let stateError = '';
// busy 是「全局一次只干一件事」的互斥锁：刷新、切换账号、登录、保存配置等都要先占它。
// 注意它与 account-panel.js 里的 panelBusy 是两把互不相干的锁：弹窗内的保存不占这把锁。
let busy = false;

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
const PAGES = ['overview', 'accounts', 'gateway', 'keys', 'docs', 'logs', 'tasks', 'requests', 'settings'];
/** 页签中文名：顶栏面包屑用。overview 的用户可见名是「报表」、gateway 的是「模型管理」
 *  （内部标识保持不变：localStorage 记忆、showPage 与 CSS 的 [data-page] 选择器都依赖它） */
const PAGE_LABELS = {
  overview: '报表',
  accounts: '账号',
  gateway: '模型管理',
  keys: '网关 Key',
  docs: '文档',
  logs: '日志',
  tasks: '定时任务',
  requests: '请求日志',
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
  // 列的设置面板常驻 body（.panel 是 overflow:hidden，留在原处会被裁掉，
  // 见 table-col-settings.js），不随宿主页面的 display:none 一起消失 ——
  // 不收起的话，切页之后它会孤零零地浮在新页面上。它自己不认识「页面」，
  // 由切页这一处统一告诉它。
  window.wbColSettings?.close?.();
  // 切到报表页时拉一次统计（面板内部自持时间范围与数据，这里只做转发）
  if (page === 'overview') {
    window.wbReport?.load?.();
  }
  // 切到日志页时清掉未读标记，并立即拉一次最新日志
  if (page === 'logs') {
    clearLogsBadge();
    window.wbLogsPanel?.load?.();
  }
  // 请求日志页自持数据与筛选，切进去时拉一次最新；顺带清掉未读失败角标
  if (page === 'requests') {
    clearRequestsBadge();
    window.wbRequestsPanel?.load?.();
  }
  // 定时任务页自持清单与编辑态，切进去时拉一次最新
  if (page === 'tasks') {
    window.wbTasksPanel?.load?.();
  }
  // 切到设置页时拉一次启动设置与网关地址（面板内部自持状态，这里只做转发）
  if (page === 'settings') {
    window.wbSettingsPanel?.load?.();
  }
  // 切到模型管理页时重拉一次：内置清单可能被上游刷新过，自定义家的清单可能刚在
  // 账号页被改过（编辑 / 删除提供商），而这一页的数据是自持的、不随主状态轮询更新
  if (page === 'gateway') {
    void window.wbModelsPanel?.load?.();
  }
  // 切到账号页时立刻补一次连接数：那条 2 秒轮询只在「当时就在账号页」时才发请求，
  // 切走的这段时间里缓存已经过期，不补一下会先看到几秒前的旧数字
  if (page === 'accounts') {
    void window.wbAccountsView?.syncConnections?.();
  }
  // 模型管理 / 网关 Key 页各自持有数据，切进去时拉一次最新
  if (page === 'gateway') {
    window.wbModelsPanel?.load?.();
  }
  if (page === 'keys') {
    window.wbKeysPanel?.load?.();
  }
  // 文档页显示的是网关地址：切进去时补一次端口同步 —— 端口只在
  // getBackendStatus 的返回里，render() 那一轮之外的变动（用户换了端口重启）
  // 到这页才被发现的话，页面上会先显示一段过时的地址，而它就等着被复制
  if (page === 'docs') {
    void window.wbPortPanel?.sync?.();
  }
  renderTopbarStatus();
  // 导航项上的内容随页面变：日志未读徽标交给日志面板，更新提示在这里重画
  // （校验更新提示的可见性与所在页面有关，见 syncUpdateBadge）
  syncUpdateBadge();
  // 请求日志徽标跟着换页立即重画：离开该页时进行中的请求要在 5 秒轮询之前
  // 就位（进页时则由 paintRequestsBadge 收起 —— 页内有自己的读数）
  paintRequestsBadge();
}

// ─── 顶栏状态 ─────────────────────────────────

/** 顶栏右侧状态区：随当前页给出该页最关心的两三个状态 */
function renderTopbarStatus() {
  const box = $('topbar-status');
  if (!box) return;
  const session = state?.session || {};
  const accounts = state?.accounts?.accounts || [];
  const enabled = accounts.filter(a => a.enabled !== false).length;
  // accounts-model.js 在 app.js 之后加载，首屏这次调用可能早于它就绪，故用可选链
  const isRateLimited = window.wbAccountsModel?.isRateLimited;
  const limited = isRateLimited ? accounts.filter(a => a.enabled !== false && isRateLimited(a)).length : 0;
  // 「网关运行中」用的是**网关进程**的判据（壳侧 is_ready 探测，见 port-panel.js），
  // 不是 upstreamConfigured（那个说的是有没有账号）。两者会独立变化：没账号不影响
  // 网关监听，端口被占也不影响账号存在 —— 混用会让用户对着「未就绪」去查账号。
  const gatewayUp = window.wbPortPanel?.isReady?.() === true;
  const chip = (text, kind = '', optional = false) =>
    `<span class="badge ${kind}${optional ? ' optional' : ''}"><span class="dot"></span>${esc(text)}</span>`;
  const port = window.wbPortPanel?.portLabel?.() || '';
  /** 直接复用某面板已渲染的徽标文案（词表 / 日志面板自持状态，这里不重复计算） */
  const mirror = id => {
    const badge = $(id);
    if (!badge || !badge.textContent || badge.textContent === '—') return '';
    return `<span class="badge ${badge.className.replace('badge', '').trim()}">${esc(badge.textContent)}</span>`;
  };

  const views = {
    accounts: () => chip(`${enabled} 个启用`, enabled ? 'ok' : '')
      + (limited ? chip(`${limited} 个已限流`, 'warn') : ''),
    gateway: () => (gatewayUp ? chip('监听 127.0.0.1', 'ok') : chip('未就绪', 'bad')) + chip(port, '', true),
    keys: () => mirror('keys-status'),
    // 文档页没有自己的徽标（它只有一组复制的地址），跟着网关的运行状态走 ——
    // 地址在页面上的意义就是「现在能不能连」，网关没起来时那个状态最要紧
    docs: () => (gatewayUp ? chip('网关运行中', 'ok') : chip('未就绪', 'bad')) + chip(port, '', true),
    logs: () => mirror('logs-badge'),
    requests: () => mirror('req-badge'),
    // 定时任务页的徽标由 tasks-panel 自己渲染（「N / M 个已开启」），直接镜像
    tasks: () => mirror('tasks-badge'),
    settings: () => (gatewayUp ? chip('网关运行中', 'ok', true) : chip('未就绪', 'bad', true))
      + (enabled ? chip(`${enabled} 个账号启用`) : ''),
    overview: () => (gatewayUp ? chip('网关运行中', 'ok') : chip('未就绪', 'bad'))
      + (session.loggedIn ? chip('已登录', 'ok') : chip('未登录', 'warn')),
  };

  box.innerHTML = views[currentPage]?.() ?? views.overview();
  // 加载失败的原因挂在这里（会话状态卡片删除后它没有别的落点）：此时下面各页面
  // 显示的都是上一次的值，顶栏是唯一常驻可见的位置。成功一次即清空。
  box.title = stateError ? `加载失败：${stateError}` : '';
}

// ─── 日志未读徽标（只提示 error） ─────────────

/**
 * 已读水位（日志 id）。null 表示「还没有水位」（首次运行），
 * 必须与 0 区分开：清空日志后水位会合法地落到 0（id 重新从 1 数起），
 * 那时若按首次运行处理，紧接着发生的错误会被当成已读吞掉。
 */
let lastSeenLogId = readSeenLogId();
/** 未读错误查询是否在飞：日志面板轮询与全局轮询都会触发它，同一时刻只该有一次 */
let unreadErrorsInFlight = false;

/** 读回持久化的水位；无值 / 值被改坏一律当作「还没有水位」 */
function readSeenLogId() {
  try {
    const raw = localStorage.getItem('workbuddy-desktop-log-seen');
    if (raw === null || raw.trim() === '') return null;
    const value = Number(raw);
    return Number.isInteger(value) && value >= 0 ? value : null;
  } catch {
    return null;
  }
}

/** 推进已读水位；localStorage 只负责跨次启动恢复，本会话的判断都看内存值 */
function markLogsSeen(id) {
  lastSeenLogId = id;
  try {
    localStorage.setItem('workbuddy-desktop-log-seen', String(id));
  } catch {
    // 存储不可用时只影响下次启动的起点，不影响本次会话
  }
}

function clearLogsBadge() {
  const badge = $('nav-count-logs');
  if (badge) badge.style.display = 'none';
  const stats = window.wbLogsPanel?.lastStats?.();
  if (stats?.lastId) markLogsSeen(stats.lastId);
}

/**
 * 导航徽标只提示 **error** 级别，数字含义是「已读水位之后新增的错误条数」。
 *
 * 为什么不再按日志总数统计：info/warn 是常态（签到、切号、刷新模型目录都写一条），
 * 按总数统计等于常挂一个红数字，亮久了就没人再看它；只有错误值得主动打断。
 * 数字口径也要跟着换 —— 显示的是错误条数，不再是「有新日志」的条数。
 *
 * 未读条数交给后端算：`GET /api/logs?level=error&sinceId=<水位>` 的 matched
 * 就是水位之后新增的 error 条数（level 过滤是「该级别及以上」，error 已是最高
 * 级别，等价于「仅 error」）。日志上限 500 条且全在内存里，这个查询代价可忽略。
 */
function updateLogsBadge(stats) {
  const badge = $('nav-count-logs');
  // stats 缺失（接口失败）时什么都不做：不能拿默认的 lastId=0 去判断水位，
  // 否则会被下面「日志被清空」那条分支误判成 id 归零，把水位一起抹掉
  if (!badge || !stats) return;
  const lastId = Number(stats.lastId) || 0;
  // 首次运行：把当前水位记为已读，否则一装上就挂着历史错误
  if (lastSeenLogId === null) {
    markLogsSeen(lastId);
    badge.style.display = 'none';
    return;
  }
  // 日志被清空（id 重新从 1 数起）：水位必须跟着回落，否则新日志的 id
  // 永远小于水位，徽标从此不再出现
  if (lastId < lastSeenLogId) {
    markLogsSeen(lastId);
    badge.style.display = 'none';
    return;
  }
  // 人就在日志页：等于已经看到，水位推进到最新
  // （顺带免掉每 10 秒轮询的这一次查询）
  if (currentPage === 'logs') {
    markLogsSeen(lastId);
    badge.style.display = 'none';
    return;
  }
  void refreshUnreadErrors(badge);
}

/** 查未读错误数并重画徽标。失败静默：查询偶尔失败不该反过来抹掉已有提示 */
async function refreshUnreadErrors(badge) {
  if (unreadErrorsInFlight || lastSeenLogId === null) return;
  unreadErrorsInFlight = true;
  const from = lastSeenLogId;
  try {
    const result = await api.getLogs({ limit: 1, level: 'error', sinceId: from });
    // 等待期间水位被推进（用户进了日志页），这次结果已过期，别拿旧数覆盖新状态
    if (from !== lastSeenLogId || currentPage === 'logs') return;
    const unread = Number(result?.matched) || 0;
    if (!unread) {
      badge.style.display = 'none';
      return;
    }
    badge.textContent = unread > 99 ? '99+' : String(unread);
    badge.title = `${unread} 条错误日志未读，点开「日志」查看`;
    badge.style.display = '';
  } catch {
    // 保持上一次的显示
  } finally {
    unreadErrorsInFlight = false;
  }
}

/** 拉一次日志统计再重画徽标：挂在 20 秒全局轮询上，非日志页也能及时看到新错误 */
async function syncLogsBadge() {
  try {
    updateLogsBadge(await api.getLogStats());
  } catch {
    // 静默：统计拿不到时保持现状
  }
}

// ─── 请求日志导航徽标（进行中优先，其次未读失败） ─────────

/**
 * 这颗徽标先后承载过两种数字，现在两种共存、按优先级画：
 *   1. **进行中的请求数**（5 秒轻量轮询）：转发是否卡住在哪个页面都该第一眼看到，
 *      不必等用户进请求日志页 —— 进行中的事是「现在」的，优先于历史的失败；
 *   2. **未读失败数**（20 秒全局轮询 + 水位）：running 归零后，历史失败还有提示在。
 *
 * 未读失败沿用水位机制：请求日志没有日志那样的自增 id（按 ts 升序、用时间分页），
 * 水位只能取毫秒时间戳：进过一次请求日志页就把水位推到当下，之后的失败才算未读。
 * 「请求发起」与「设置水位」落在同一毫秒这种碰撞按未读算（start 是含边界）——
 * 宁可多提示一条，不冒漏提示的险；多提示的代价是进一次页面就清掉。
 * 未读条数复用请求日志接口：`status=error&start=<水位>` 的 matched 正是水位之后的
 * 失败数（与面板/报表同一个成功口径：非 2xx 或带错误摘要），limit 压到 1 只为拿计数。
 */
let lastSeenReqTs = readSeenReqTs();
/** 最近一次查到的进行中请求数（5 秒轻量轮询维护；0 = 没有） */
let runningRequests = 0;
/** 最近一次查到的未读失败数（20 秒全局轮询维护；0 = 没有） */
let unreadFailures = 0;

/** 读回持久化的水位；无值 / 值被改坏一律当作「还没有水位」（首次运行） */
function readSeenReqTs() {
  try {
    const raw = localStorage.getItem('workbuddy-desktop-req-seen');
    if (raw === null || raw.trim() === '') return null;
    const value = Number(raw);
    return Number.isFinite(value) && value >= 0 ? value : null;
  } catch {
    return null;
  }
}

/** 推进已读水位；localStorage 只负责跨次启动恢复，本会话的判断都看内存值 */
function markRequestsSeen(ts) {
  lastSeenReqTs = ts;
  try {
    localStorage.setItem('workbuddy-desktop-req-seen', String(ts));
  } catch {
    // 存储不可用时只影响下次启动的起点，不影响本次会话
  }
}

function clearRequestsBadge() {
  const badge = $('nav-count-requests');
  if (badge) badge.style.display = 'none';
  markRequestsSeen(Date.now());
  // 水位推到当下 = 在此之前的失败都算已读：把内存里的计数一并归零，
  // 否则离开页面后 paintRequestsBadge 会拿旧数把角标重新点亮
  unreadFailures = 0;
}

/**
 * 重画请求日志导航徽标。两种数字共用一颗徽标（都不亮就收起），
 * 写法上收口到这一个函数：两条轮询各自只更新自己的计数，谁也不直接碰 DOM，
 * 避免「20 秒一拍的失败查询把刚亮出的进行中数字又改回去」这种互相覆盖。
 */
function paintRequestsBadge() {
  const badge = $('nav-count-requests');
  if (!badge) return;
  // 人就在请求日志页：页内已有「· M 进行中」读数，导航徽标不再重复
  if (currentPage === 'requests') {
    badge.style.display = 'none';
    return;
  }
  if (runningRequests > 0) {
    badge.textContent = runningRequests > 99 ? '99+' : String(runningRequests);
    badge.title = `${runningRequests} 个请求正在转发中，点开「请求日志」查看`;
    badge.style.display = '';
    // 类名用 is-running 而不是 live：layout.css 里 .live 是**侧栏状态灯**
    // （7px 圆点 + 绿底 + 光晕），加上它会把这颗数字角标压成小圆点 ——
    // 两个组件恰好都叫「live 状态」，但一个是灯、一个是数字，样式不可共用。
    badge.classList.add('is-running');
    return;
  }
  badge.classList.remove('is-running');
  if (unreadFailures > 0) {
    badge.textContent = unreadFailures > 99 ? '99+' : String(unreadFailures);
    badge.title = `${unreadFailures} 条失败请求未读，点开「请求日志」查看`;
    badge.style.display = '';
    return;
  }
  badge.style.display = 'none';
}

/** 查水位之后的失败请求数并重画徽标：挂在 20 秒全局轮询上 */
async function syncRequestsBadge() {
  // 首次运行：把当下记为已读，否则一装上就挂着历史失败
  if (lastSeenReqTs === null) {
    markRequestsSeen(Date.now());
    paintRequestsBadge();
    return;
  }
  // 人就在请求日志页：等于已经看到，水位推进到当下
  if (currentPage === 'requests') {
    markRequestsSeen(Date.now());
    paintRequestsBadge();
    return;
  }
  try {
    const result = await api.getStatsRequests({
      status: 'error',
      start: lastSeenReqTs,
      limit: 1,
    });
    // 等待期间进了请求日志页：这次结果已过期，别拿旧数把刚清掉的角标又点亮
    if (currentPage === 'requests') return;
    unreadFailures = Number(result?.matched) || 0;
  } catch {
    return; // 保持上一次的显示：查询偶尔失败不该反过来抹掉已有提示
  }
  paintRequestsBadge();
}

// ── 进行中请求的轻量轮询（5 秒）────────────────
//
// 单独起一条 5 秒定时器而不是搭 20 秒全局轮询：进行中的请求通常几秒就收尾，
// 20 秒一拍会整段错过，徽标就永远等不到亮出的机会。查询压到 limit=1，
// 只要 matched（计数），不拉明细。列表自身的 1 秒轮询只在请求日志页跑
// （见 requests-panel.js 的 startAuto），人不在那页时，推进徽标的只有这里。
let runningQueryBusy = false;

async function syncRunningRequests() {
  if (runningQueryBusy) return;
  // 人已在请求日志页：不发查询（页内读数更准），徽标也由页面接管而不亮
  if (currentPage === 'requests') return;
  runningQueryBusy = true;
  try {
    const result = await api.getStatsRequests({ status: 'running', limit: 1 });
    runningRequests = Number(result?.matched) || 0;
  } catch {
    return; // 保持上一次的显示：查询偶尔失败不该反过来抹掉已有提示
  } finally {
    runningQueryBusy = false;
  }
  paintRequestsBadge();
}

// 窗口隐藏时暂停（与 20 秒全局轮询同一取向）：后台页没有「第一眼」可言
setInterval(() => {
  if (document.hidden) return;
  void syncRunningRequests();
}, 5_000);

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
  maybeShowUpdateModal(info);
}

// ─── 「检测到更新」弹窗 ────────────────────────

/** 「跳过此次更新」记在 localStorage 的键（值 = 跳过的版本号） */
const UPDATE_SKIP_KEY = 'workbuddy-desktop-update-skip';
/** 本会话内已弹过提示的版本号：用户选「取消」后，同一版本不再连着弹
 *  （后端的定时检查每 5 分钟就会再次发现它，弹一次/轮是预期节奏） */
let promptedUpdateVersion = '';

function closeUpdateModal() {
  $('update-modal')?.classList.remove('open');
}

/**
 * 检测到新版本时弹出提示弹窗（标题「检测到更新」+ Markdown 更新日志）。
 *
 * 弹与不弹的判定：
 *   - 「跳过此次更新」记的是**版本号**：该版本不再弹，将来更新的版本照常弹；
 *   - 「取消」什么都不记：下一次检测到（定时任务的下一轮）还会再弹；
 *   - 人已经在设置页时不弹 —— 软件更新面板就在眼前，再盖一层弹窗纯属打扰
 *     （与 syncUpdateBadge 的取向一致）。
 */
function maybeShowUpdateModal(info) {
  const mask = $('update-modal');
  if (!mask || !info || info.hasUpdate !== true) return;
  const latest = String(info.latestVersion || '').trim();
  if (!latest || wbApp.currentPage === 'settings') return;
  let skipped = '';
  try { skipped = localStorage.getItem(UPDATE_SKIP_KEY) || ''; } catch { /* 隐私模式等：当作没跳过 */ }
  if (latest === skipped || latest === promptedUpdateVersion) return;
  promptedUpdateVersion = latest;

  $('update-modal-version').textContent = latest;
  $('update-modal-current').textContent = info.currentVersion || '未知';
  const notes = String(info.notes || '').trim();
  $('update-modal-notes').innerHTML = notes
    ? (window.wbMarkdown?.render?.(notes) || `<p>${esc(notes)}</p>`)
    : '<p>这个版本没有填写发布说明。</p>';
  mask.classList.add('open');
}

$('update-modal-go')?.addEventListener('click', () => {
  closeUpdateModal();
  showPage('settings');
  // 更新面板在设置页的「关于」分类下；设置页自己会按 localStorage 恢复上次
  // 手点过的分类（比如「数据」），所以跳过去之后要显式切到「关于」。
  // 只切视图、不写偏好 —— 这是弹窗带来的深链，不该改用户手点的默认分类。
  window.wbSettingsPanel?.showCategory?.('about');
  // 跳到设置页后直接把下载跑起来，别让人再点一次「下载并安装」——
  // 他点「去更新」的意图就是要更新，停在面板上等下一步是多余的。
  // 用 lastUpdateInfo（弹窗自己那次 checkUpdate 的结果）而不是让面板重查：
  // 省一次往返，也避免「弹窗说有新版、面板查到没有」的不一致。
  void window.wbUpdatePanel?.openAndDownload?.(lastUpdateInfo);
});
$('update-modal-skip')?.addEventListener('click', () => {
  try { localStorage.setItem(UPDATE_SKIP_KEY, promptedUpdateVersion); } catch { /* 忽略：下次照常弹 */ }
  closeUpdateModal();
});
$('update-modal-cancel')?.addEventListener('click', closeUpdateModal);
$('update-modal-close')?.addEventListener('click', closeUpdateModal);
$('update-modal')?.addEventListener('click', event => {
  if (event.target === $('update-modal')) closeUpdateModal();
});

// ─── 渲染：网关 / 模型 / 配置 ──────────────────

/**
 * 网关页的地址展示与侧栏那两条状态都由 port-panel.js 负责
 * （它自持后端状态与端口）。这里只做转发。
 */
function renderGateway() {
  window.wbPortPanel?.render?.();
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
  // 这里说的是「上游凭证」（有没有可用账号），与网关进程是否在监听是两件事，
  // 所以措辞明确指向账号 —— 别再说成「代理不可用」（那会让人去查端口）
  if (!health?.upstreamConfigured) {
    box.innerHTML = `<span style="color:var(--danger)">无可用账号：${esc(health?.unavailableReason || '尚未登录')}</span>`;
    return;
  }
  box.textContent = `运行正常 · ${health.upstreamBaseUrl || ''}`;
}

function render() {
  renderAccountsView();
  renderGateway();
  renderModels();
  renderProxyStatus();
  renderTopbarStatus();
}

/**
 * 模型管理区块由 models-panel.js 负责（表格渲染 / 筛选 / 启停 / 映射 / 刷新清单）。
 * 它自持从 /api/models/manage 拉来的数据；这里的调用只在轮询到新状态时提醒它
 * 「清单可能变了」，由它决定是否重拉。用可选链委托 —— 万一它还没执行完也只是
 * 本轮不画，不抛错把 render() 拖崩。
 */
function renderModels() {
  window.wbModelsPanel?.render();
}

/** 账号列表由 accounts-view 模块负责（含优先级/禁用/代理徽章与行内面板） */
function renderAccountsView() {
  window.wbAccountsView?.render();
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
    // 拿到状态即清掉上一次的失败说明（见下面 catch 的注释）
    stateError = '';
    // 清掉已删除账号的本地缓存；代理可选项也可能在 Clash 侧改过，下次打开弹窗重读
    const validIds = new Set((state.accounts?.accounts || []).map(a => a.id));
    window.wbAccountsView?.refreshCaches(validIds);
    window.wbAccountPanel?.invalidate();
    render();
  } catch (error) {
    state = null;
    stateError = error.message;
    render();
    // 「本地代理状态」卡片与会话状态卡片都已删除，加载失败的说明改挂到顶栏状态区的
    // title 上（见 renderTopbarStatus）：那里此时本来就显示「未登录 / 未就绪」，
    // 悬停即可看到真实原因（后端不可达 / 接口报错）。
    // 不用 toast：refresh() 每 20 秒轮询一次，后端长时间不可用会变成刷屏。
    console.warn('加载状态失败:', error.message);
  } finally {
    releaseBusy();
  }
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
      const result = await api.switchAccount(id);
      await refresh();
      toast(result?.changed === false ? '账号已在全局队列第一位' : '✅ 已将账号优先级调整到全局第一位');
    } else if (action === 'refresh') {
      await api.refreshAccountToken(id);
      await refresh();
      toast('✅ Token 已刷新');
    } else if (action === 'remove') {
      const account = state?.accounts?.accounts?.find(a => a.id === id);
      const name = esc(account?.nickname || account?.name || id);
      // 原生 confirm 在 Tauri 的 WebView 里不弹窗、直接放行（等于没有确认），
      // 危险确认一律走自绘弹窗（wbConfirm，见 confirm-dialog.js）—— 下同
      const note = window.wbAccountsModel?.isDesktopAccount?.(account) ? '（不会影响客户端登录态）' : '';
      if (!(await window.wbConfirm?.ask?.({
        title: '删除账号',
        html: `确定删除账号「<strong>${name}</strong>」？${esc(note)}`,
        okText: '删除',
        okClass: 'danger',
      }))) return;
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

// 模型搜索框的 `input` 监听不在这里：过滤逻辑（applyModelFilter）已随模型区块
// 一起搬进 models-panel.js，监听留在原地会让两边各持一半 —— 见该文件的说明。

// ─── 事件绑定 ─────────────────────────────────

document.querySelectorAll('#theme-switch button').forEach(button => {
  button.addEventListener('click', () => applyTheme(button.dataset.mode));
});
window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  if ((localStorage.getItem('workbuddy-desktop-theme') || 'system') === 'system') applyTheme('system');
});
applyTheme(localStorage.getItem('workbuddy-desktop-theme') || 'system');

api.onStateChanged(next => {
  if (!next?.accounts && !next?.session && !next?.health) return;
  // 合并：旧 state + 新字段
  state = { ...(state || {}), ...next };
  render();
});

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
  renderTopbarStatus,
  updateLogsBadge,
  syncLogsBadge,
  // 软件更新面板在每次检查结束后回调它，把「有新版本」翻译成导航上的提示
  updateUpdateBadge,
};

// ─── 启动自动维护：主进程会拉一次临期 token 刷新 ───
// 余额不在这里：它归「定时查询积分」那条定时任务（首轮在网关就绪后立刻跑一次，
// 见 commands.rs 的 startup_maintenance），界面由下面的轮询读快照应用。
api.onAutoMaintained?.(({ refreshed }) => {
  const count = Array.isArray(refreshed) ? refreshed.length : 0;
  if (count) window.wbAccountsView?.render();
  if (count) toast(`已自动刷新 ${count} 个临期账号的 Token`);
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

// ─── 端口状态与冲突处置 ───────────────────────
//
// 侧栏那两条状态（网关进程 / 可用账号）与端口冲突时的两个出口
// （结束占用进程 / 更换端口）都在 port-panel.js 里 —— 它自持后端状态与
// 一整套弹窗交互，留在本文件会让这里继续膨胀。本文件只负责在 render()
// 里委托它重画，并在首屏主动问一次后端状态。

showPage(localStorage.getItem(PAGE_KEY) || 'overview', { persist: false });

refresh();
// 首屏就问一次后端状态：此时 state 还没回来，侧栏两条状态各自显示
// 「正在检查…」，拿到结果后立刻变成真值（端口冲突会直接给出失败原因）
void window.wbPortPanel?.sync?.();

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
  // 数据结构升级：这次更新把数据存储换成了单个 SQLite 库，启动时后端只探测
  // 「还有没有旧文件没搬进库」，有待迁移就直接导入（**不弹窗** —— 升级没有
  // 选项也不能取消，弹窗只是多余的一道坎）。同样放 DOMContentLoaded：
  // 面板脚本排在 app.js 之后。
  // 它**不是**更新检查那种「有新版本就提示」的可选动作 —— 没升级时账号是空的，
  // 所以过程与结果都要 toast 报出来，不能让用户面对一个「账号怎么空了」的疑问。
  void window.wbUpgradePanel?.check?.();
}, { once: true });

// 定时轮询：限额标记（429 + 恢复时间）与账号状态变化自动刷新；窗口隐藏时暂停
setInterval(() => {
  if (document.hidden) return;
  refresh();
  // 后端状态一起轮询：网关可能在这期间起停（端口冲突、用户结束占用进程后重启），
  // 而管理 API 打不通时 refresh() 只会静默失败，看不出发生了什么
  void window.wbPortPanel?.sync?.();
  // 日志未读错误也一起轮询：否则人不在日志页时，只有日志面板那次 10 秒轮询
  // 才会更新徽标 —— 而那个轮询恰恰只在日志页可见时才发请求（见 logs-panel.js）
  void syncLogsBadge();
  // 请求日志的未读失败同理：人不在请求日志页时也要有人推进角标
  void syncRequestsBadge();
  // 「定时查询积分」的结果快照也跟着这一轮读一次：后端的定时任务在跑，
  // 界面得跟上它（否则用户不点按钮就永远停在启动那次的旧余额上）。
  // 快照时间戳没变时它自己会早退，不会造成无谓的重绘。
  void window.wbAccountsView?.syncBalancesSnapshot?.();
}, 20_000);

// 定时「软件版本检查」的结果轮询（1 分钟）。
//
// 真正的检查在**后端定时任务**里跑（定时任务页的「软件版本检查」，默认 5 分钟
// 一次，结果缓存于 UpdateManager）；这里只是低频读一次缓存来亮/灭侧栏徽标，
// 不自己打 GitHub —— 匿名限额 60 次/小时，双端各查一遍就贴顶了。
// hasUpdate 为 null（无法比较）或 false 时 syncUpdateBadge 自会不亮标；
// 读到 checked:false（本进程还没查过）不覆盖 lastUpdateInfo ——
// 启动那次壳命令检查的结果仍是最准的一份。
setInterval(() => {
  if (document.hidden) return;
  void api.getUpdateStatus?.().then(info => {
    if (!info || info.checked === false) return;
    wbApp.updateUpdateBadge(info);
  }).catch(() => { /* 静默：下一次轮询自然重试 */ });
}, 60_000);