/* Agent2API · 设置页「软件更新」卡片 */
/* global workbuddyDesktop, wbApp, wbMarkdown */

/**
 * 独立面板模块（与 settings-panel / desensitize-panel 同构）。
 *
 * 壳命令契约（本文件只按此调用）：
 *   checkUpdate()                       → { currentVersion, latestVersion, hasUpdate,
 *                                          notes, publishedAt, pageUrl, ... }
 *   downloadUpdate({ url, name })       → 下载任务快照
 *   updateProgress()                    → 下载任务快照
 *   cancelUpdate()                      → { canceled }
 *   runInstaller(path, restart)         → { launched, path, restart }
 *   openReleasePage(url)                → { url }（打开外链的唯一出口）
 *
 * 下载进度用轮询而不是事件：后端下载是单任务的长轮询场景，
 * 轮询实现更简单，也便于页面重新进入时立刻拿到当前状态。
 *
 * 面板里两块数据同源，都来自 checkUpdate：版本号用于判断有没有新版本，
 * 正文（notes）用于渲染更新日志。所以「检查更新」一次就把两块都刷新了，
 * 不必再单独拉一次历史版本列表。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  /** 进度轮询间隔：下载 25MB 左右，1 秒足够顺滑又不会太密 */
  const POLL_MS = 1000;

  let info = null;       // 最近一次检查结果（更新日志也由它渲染）
  let polling = null;    // 进度轮询定时器
  let busy = false;
  let downloading = false;  // 本会话是否正在下载（决定按钮是「下载并安装」还是「取消下载」）
  let autoInstall = false;  // 本次下载完成后是否自动安装（避免页面重入时误装遗留任务）

  let repository = '';      // owner/repo，来自接口：作者主页与仓库地址由它拼出来，不写死
  let checkedAt = 0;        // 上次检查更新的时刻

  // ─── 平台文案 ─────────────────────────────────
  //
  // 安装包形态由后端按**编译目标平台**给出（checkUpdate 的 installerKind）：
  //   'nsis' → Windows：启动安装程序需要管理员权限，会弹 UAC 确认框；
  //   'dmg'  → macOS：挂载磁盘映像，用户自己把 app 拖进「应用程序」，没有提权这一步。
  //
  // 为什么把这几句话集中在一处：它们散在下载中 / 下载完成 / 安装启动三个位置，
  // 各自硬编码会让「macOS 上提示 UAC」这种错位很难发现 —— 而 macOS 用户看到
  // 「会弹出 UAC 确认框」只会一头雾水（那台机器上根本不存在 UAC）。
  //
  // ── 为什么还要一条 UA 兜底 ──────────────────────────────────
  // `installerKind` 来自 checkUpdate 的返回，而「面板切回来时发现上次遗留的
  // 下载任务」这条路（load 里的 renderTask）**可能早于任何一次 checkUpdate**：
  // 那时 info 还是 null，只看 installerKind 会退回 Windows 文案。
  // 此时读一次 UA 就能判准，比让 macOS 用户看到 UAC 提示强。
  // 有后端给的值时一律以后端为准（它才是编译目标的权威口径）。
  const uaLooksMac = () => /Mac|iPhone|iPad/.test(navigator.userAgent || '');

  const isMacInstaller = () => (info?.installerKind
    ? info.installerKind === 'dmg'
    : uaLooksMac());

  /** 「开始下载」时的提示：说明下载完会发生什么 */
  const downloadHint = () => (isMacInstaller()
    ? '正在下载安装包…（下载完成后会挂载磁盘映像，把应用拖进「应用程序」即可完成安装）'
    : '正在下载安装包…（下载完成后启动安装程序需要管理员权限，会弹出 UAC 确认框）');

  /** 「安装包已就绪」的提示：说明下一步该做什么 */
  const readyHint = name => (isMacInstaller()
    ? `安装包已就绪：${name}。点击「打开安装包」后会挂载磁盘映像，把应用拖进「应用程序」即可完成安装。`
    : `安装包已就绪：${name}。点击「安装并重启」后需要管理员权限，会弹出 UAC 确认框。`);

  /** 安装按钮的文案：macOS 不重启（dmg 与运行中的进程没有文件冲突） */
  const installButtonText = () => (isMacInstaller() ? '打开安装包' : '安装并重启');

  // ─── 工具 ─────────────────────────────────────

  /** 两位补零。与 logs-panel 的时间格式同一套写法（手工 pad + 本地时区），
   *  不用 toLocaleString：它的输出随系统区域设置变，面板里的其它时间都是定宽格式 */
  const pad2 = value => String(value).padStart(2, '0');

  /** 发布 / 拉取时间 → `YYYY-MM-DD HH:mm`（publishedAt 是 UTC 的 ISO 串，这里转本地时区） */
  function formatDateTime(value) {
    const date = new Date(value);
    if (!value || Number.isNaN(date.getTime())) return '';
    return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`
      + ` ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  }

  /** 「上次检查」只要时刻：同一天内的检查看几点几分几秒就够了 */
  function formatClock(value) {
    const date = new Date(value);
    if (!value || Number.isNaN(date.getTime())) return '';
    return `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
  }

  /** 外链白名单：只放行 http(s)（与后端 open_release_page 的口径一致，这里先挡一道） */
  function safeExternal(value) {
    const url = String(value || '').trim();
    return /^https?:\/\//i.test(url) ? url : '';
  }

  // ─── 面板状态 ─────────────────────────────────

  function setBadge(text, kind) {
    const badge = $('update-badge');
    if (!badge) return;
    badge.className = `badge${kind ? ` ${kind}` : ''}`;
    badge.textContent = text;
  }

  function setState(text, isError) {
    const box = $('update-state');
    if (!box) return;
    box.textContent = text || '';
    box.style.color = isError ? 'var(--danger)' : '';
  }

  function renderVersions() {
    const current = $('update-current');
    const latest = $('update-latest');
    if (!current || !latest) return;
    current.textContent = info?.currentVersion || '—';
    latest.textContent = info?.latestVersion
      ? `${info.latestVersion}${info.prerelease ? '（预发布）' : ''}`
      : '—';
  }

  /** 把最近一次检查结果铺到面板上：检查成功后与「重新进入设置页」复用同一段 */
  function renderCheckResult() {
    renderVersions();
    toggleActions();
    if (!info) return;
    const at = checkedAt ? `（检查于 ${formatClock(checkedAt)}）` : '';
    if (info.hasUpdate === true) {
      setBadge('有新版本', 'warn');
      setState(`发现新版本 ${info.latestVersion}（当前 ${info.currentVersion}）。${at}`);
    } else if (info.hasUpdate === false) {
      setBadge('已是最新', 'ok');
      setState(`当前已是最新版本（${info.currentVersion}）。${at}`);
    } else {
      // hasUpdate 为 null：版本号无法比较（本地是开发版或 tag 非语义化）
      setBadge('无法比较', 'warn');
      setState(info.latestVersion
        ? `最新发布版本为 ${info.latestVersion}，但当前版本号「${info.currentVersion || '未知'}」无法解析，未做新旧判断。${at}`
        : `仓库暂无发布版本。${at}`);
    }
  }

  function toggleActions() {
    const download = $('btn-update-download');
    if (!download) return;
    // 有更新且真的有可下载资产时才给出「下载并安装」
    const actionable = info?.hasUpdate === true && !!info?.asset?.url;
    download.style.display = actionable ? '' : 'none';
  }

  // ─── 下载 ─────────────────────────────────────

  function stopPolling() {
    if (polling) { clearInterval(polling); polling = null; }
  }

  /** 下载进度条：只在下载中显示，百分比直接驱动宽度 */
  function setProgress(percent) {
    const box = $('update-progress');
    if (!box) return;
    const value = Math.max(0, Math.min(100, Number(percent) || 0));
    box.querySelector('.bar').style.width = `${value}%`;
    box.hidden = value <= 0 || value >= 100;
  }

  /** 下载任务状态 → 界面文案 */
  function renderTask(task) {
    if (!task) return false;

    if (task.active) {
      const percent = Number(task.percent) || 0;
      const mb = (Number(task.received) || 0) / 1024 / 1024;
      const totalMb = (Number(task.total) || 0) / 1024 / 1024;
      setBadge('下载中', 'warn');
      setState(`正在下载 ${task.filename || '安装包'}：${percent}%（${mb.toFixed(1)} / ${totalMb.toFixed(1)} MB）`);
      setProgress(percent);
      const button = $('btn-update-download');
      if (button) { button.textContent = '取消下载'; button.disabled = false; }
      return true;
    }

    stopPolling();
    downloading = false;
    setProgress(0);
    const button = $('btn-update-download');
    if (task.error) {
      setBadge('下载失败', 'bad');
      setState(`下载失败：${task.error}`, true);
      if (button) button.textContent = '重试下载';
      autoInstall = false;
      return true;
    }
    if (task.canceled) {
      setBadge('已取消', 'warn');
      setState('下载已取消，可重新点击「下载并安装」。');
      if (button) button.textContent = '下载并安装';
      autoInstall = false;
      return true;
    }
    if (task.done && task.path) {
      setBadge('可安装', 'ok');
      // 只有本次会话发起的下载才自动装：页面切回来时碰上遗留的完成任务
      // 就直接弹安装程序，属于用户没有预期的副作用
      if (autoInstall) {
        autoInstall = false;
        setState(isMacInstaller()
          ? '安装包已下载完成，正在挂载磁盘映像…'
          : '安装包已下载完成，正在启动安装程序（需要管理员权限，会弹出 UAC 确认框）…');
        void install(task.path);
      } else {
        setState(readyHint(task.filename || task.path));
        if (button) {
          button.textContent = installButtonText();
          button.dataset.installPath = task.path;
        }
      }
      return true;
    }
    return false;
  }

  async function pollProgress() {
    try {
      const task = await api.updateProgress();
      renderTask(task);
    } catch (error) {
      stopPolling();
      setState(`读取下载进度失败：${error.message}`, true);
    }
  }

  function startPolling() {
    stopPolling();
    polling = setInterval(() => { void pollProgress(); }, POLL_MS);
  }

  /** 安装：启动安装包；Windows 上由壳退出本程序，让出文件占用 */
  async function install(path) {
    try {
      // 返回值里的 restart 是**壳按平台定的**：Windows 覆盖安装前必须先退出，
      // macOS 挂载 dmg 则不需要（也不该）退出 —— 所以这里不假设重启，
      // 按壳回传的取值决定提示语（否则 macOS 用户会等一个不会发生的退出）
      const result = await api.runInstaller(path, true);
      const willRestart = result?.restart !== false;
      setState(willRestart
        ? '安装程序已启动，本程序将退出以便完成覆盖安装。安装程序需要管理员权限，会弹出 UAC 确认框，请选择「是」。'
        : '安装包已挂载，请在弹出的窗口里把应用拖进「应用程序」完成安装。安装完成后重新打开本程序即可。');
    } catch (error) {
      setBadge('启动失败', 'bad');
      // UAC 被拒时壳侧返回的提示已经说明「可重新点击安装并重启」，这里原样透出，
      // 不额外包装 —— 用户照着做就能重试成功
      setState(`启动安装程序失败：${error.message}`, true);
    }
  }

  // ─── 更新日志 ─────────────────────────────────

  /** 仓库全名（owner/repo）来自接口；形态不对就不用，免得拼出个乱七八糟的链接 */
  function applyRepository(value) {
    const repo = String(value || '').trim();
    if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) return;
    repository = repo;
    renderAuthor();
  }

  /** 仓库主页：收藏项目按钮的落点 */
  const repoUrl = () => (repository ? `https://github.com/${repository}` : '');

  /** 作者主页：取 repository 的 owner 段，这样 fork 之后自动指向 fork 者，不必改代码 */
  const ownerUrl = () => (repository ? `https://github.com/${repository.split('/')[0]}` : '');

  /**
   * 「关于作者」那一行。文案是静态的，链接地址来自接口 ——
   * 所以拿到数据之前整行隐藏，避免出现一个点了没反应的按钮。
   */
  function renderAuthor() {
    const box = $('update-author');
    const link = $('update-author-link');
    const star = $('btn-update-favorite');
    if (!box) return;
    const owner = ownerUrl();
    const repo = repoUrl();
    if (!owner || !repo) { box.hidden = true; return; }
    box.hidden = false;
    if (link) {
      link.href = owner;
      link.dataset.external = owner;
      link.textContent = repository.split('/')[0];
    }
    if (star) star.dataset.external = repo;
  }

  /**
   * 更新日志只展示**最新一次**发布的说明，数据直接取自 checkUpdate 的返回。
   *
   * 为什么不再单独拉历史列表：历史版本对「本机现在要不要升级」没有帮助 ——
   * 用户只需要知道最新这版改了什么，再早的改动去 GitHub Releases 页面看更合适。
   * 顺带少了每次进设置页的一次 GitHub 请求（匿名限额 60 次/时）。
   *
   * 与上方「最新版本」天然同源：两者都来自同一次 checkUpdate，不会出现
   * 版本号是新的、日志还是旧的这种情况。
   */
  function renderChangelog() {
    const body = $('update-log-body');
    if (!body) return;

    const meta = $('update-log-meta');
    if (meta) {
      const tag = String(info?.latestVersion || '').trim();
      meta.textContent = tag ? `${tag}${checkedAt ? ` · 检查于 ${formatClock(checkedAt)}` : ''}` : '';
    }

    const notes = String(info?.notes || '').trim();
    if (!notes) {
      body.innerHTML = '<div class="update-log-hint">'
        + (info ? '这个版本没有填写发布说明。' : '点击「检查更新」后，这里会显示最新版本的更新说明。')
        + '</div>';
      return;
    }

    // 正文与「在 GitHub 查看」都在一张静态卡片里，不再折叠：
    // 只有一份说明，展开/收起反而多一次点击才能看到内容
    const pageUrl = safeExternal(info?.pageUrl);
    const html = window.wbMarkdown?.render?.(notes) || '';
    body.innerHTML = '<div class="rel-note">'
      + '<div class="rel-note-head">'
      + `<span class="rel-tag">${esc(info?.latestVersion || '最新版本')}</span>`
      + (info?.prerelease === true ? '<span class="badge tag warn">预发布</span>' : '')
      + (formatDateTime(info?.publishedAt)
        ? `<span class="rel-date">${esc(formatDateTime(info.publishedAt))}</span>`
        : '')
      + '</div>'
      + `<div class="rel-note-body"><div class="md-body">${html || '<p class="md-body-empty">这个版本没有填写发布说明。</p>'}</div>`
      + (pageUrl
        ? `<div class="rel-foot"><a href="${esc(pageUrl)}" data-external="${esc(pageUrl)}"`
          + ' target="_blank" rel="noopener">在 GitHub 查看完整说明</a></div>'
        : '')
      + '</div></div>';
  }

  // ─── 事件 ─────────────────────────────────────

  /** 打开外链：一律交给系统浏览器。webview 里直接导航会白屏，
   *  而且本程序持有桥接权限，外部链接不该在应用内部打开 */
  async function openExternal(url) {
    const target = safeExternal(url);
    if (!target) { toast('链接地址不受支持', 'err'); return; }
    try {
      await api.openReleasePage(target);
    } catch (error) {
      toast(`打开链接失败：${error.message}`, 'err');
    }
  }

  /**
   * 事件委托挂在整块面板上：时间轴条目是动态渲染出来的，逐个绑定既费事又容易漏；
   * 而且 Markdown 正文里任意数量、任意位置的外链也要被同一套逻辑接住。
   */
  function onPanelClick(event) {
    const trigger = event.target.closest('[data-external]');
    if (!trigger) return;
    event.preventDefault();   // 掐掉 webview 自己的导航（跳过去只会白屏）
    void openExternal(trigger.dataset.external);
  }

  async function check() {
    if (busy) return null;
    busy = true;
    const button = $('btn-update-check');
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '检查中…'; }
    setBadge('检查中', 'warn');
    setState('正在查询 GitHub 上的最新发布版本…');
    try {
      info = await api.checkUpdate();
      checkedAt = Date.now();
      if (info?.repository) applyRepository(info.repository);
      renderCheckResult();
      // 更新日志与上面那段同源（都是这次 checkUpdate 的结果），一起重绘即可
      renderChangelog();
    } catch (error) {
      info = null;
      renderVersions();
      toggleActions();
      renderChangelog();
      setBadge('检查失败', 'bad');
      setState(`检查更新失败：${error.message}`, true);
    } finally {
      busy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
    // 左侧导航的提示：只有确实有新版本才亮，失败与「已是最新」都静默
    wbApp.updateUpdateBadge?.(info);
    return info;
  }

  async function downloadOrCancel() {
    if (busy) return;
    const button = $('btn-update-download');

    // 已有就绪的安装包：按钮在这一步是「安装并重启」
    const readyPath = button?.dataset.installPath;
    if (!downloading && readyPath) {
      busy = true;
      button.disabled = true;
      try {
        await install(readyPath);
      } finally {
        busy = false;
        if (button) button.disabled = false;
      }
      return;
    }

    // 正在下载时按钮变成「取消下载」
    if (downloading) {
      try {
        await api.cancelUpdate();
        setState('正在取消下载…');
      } catch (error) {
        toast(`取消失败：${error.message}`, 'err');
      }
      return;
    }

    const asset = info?.asset;
    if (!asset?.url) { toast('没有可下载的安装包', 'err'); return; }

    busy = true;
    if (button) button.disabled = true;
    try {
      await api.downloadUpdate({ url: asset.url, name: asset.name });
      downloading = true;
      autoInstall = true;
      delete button?.dataset.installPath;
      setBadge('下载中', 'warn');
      setState(downloadHint());
      if (button) button.textContent = '取消下载';
      startPolling();
    } catch (error) {
      setBadge('下载失败', 'bad');
      setState(`下载失败：${error.message}`, true);
    } finally {
      busy = false;
      if (button) button.disabled = false;
    }
  }

  /** 面板数据入口（切入设置页时调用） */
  async function load() {
    // 日志与版本号同源，所以先按当前结果铺一次（含启动时那次自动检查）：
    // 切回来时不会白着一块等接口
    renderChangelog();

    // 先看有没有上次遗留的下载任务（页面切走再回来时进度不丢）
    try {
      const task = await api.updateProgress();
      if (task?.active) {
        downloading = true;
        autoInstall = false;   // 遗留任务：下完只提示，不自动装
        renderTask(task);
        setBadge('下载中', 'warn');
        startPolling();
        return;
      }
      if (task?.done && task.path) {
        autoInstall = false;
        renderTask(task);
        return;
      }
    } catch { /* 后端未就绪：按未检查处理 */ }

    // 启动时已经自动检查过一次的话，把那次结果原样铺回来（含检查时刻）。
    // 这里曾经无条件重置成「未检查」，那样等于把启动检查的结果白白丢掉
    if (info) { renderCheckResult(); return; }

    setBadge('未检查');
    setState('点击「检查更新」查询 GitHub 上的最新发布版本。');
  }

  // ─── 绑定 ─────────────────────────────────────

  $('btn-update-check')?.addEventListener('click', check);
  $('btn-update-download')?.addEventListener('click', downloadOrCancel);

  // 面板内的外链统一走委托（含「关于作者」与日志正文里的链接）
  $('update-notes')?.closest('.panel')?.addEventListener('click', onPanelClick);

  window.wbUpdatePanel = { load, check };
})();
