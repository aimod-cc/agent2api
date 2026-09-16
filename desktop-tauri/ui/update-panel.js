/* WorkBuddy 本地代理 · 设置页「软件更新」卡片 */
/* global workbuddyDesktop, wbApp */

/**
 * 独立面板模块（与 settings-panel / desensitize-panel 同构）。
 *
 * 壳命令契约（本文件只按此调用）：
 *   checkUpdate()                       → { currentVersion, latestVersion, hasUpdate, ... }
 *   downloadUpdate({ url, name })       → 下载任务快照
 *   updateProgress()                    → 下载任务快照
 *   cancelUpdate()                      → { canceled }
 *   runInstaller(path, restart)         → { launched, path, restart }
 *   openReleasePage(url)                → { url }
 *
 * 下载进度用轮询而不是事件：后端下载是单任务的长轮询场景，
 * 轮询实现更简单，也便于页面重新进入时立刻拿到当前状态。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  /** 进度轮询间隔：下载 25MB 左右，1 秒足够顺滑又不会太密 */
  const POLL_MS = 1000;

  let info = null;       // 最近一次检查结果
  let polling = null;    // 进度轮询定时器
  let busy = false;
  let downloading = false;  // 本会话是否正在下载（决定按钮是「下载并安装」还是「取消下载」）
  let autoInstall = false;  // 本次下载完成后是否自动安装（避免页面重入时误装遗留任务）

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

  function renderNotes() {
    const box = $('update-notes');
    if (!box) return;
    const notes = String(info?.notes || '').trim();
    if (!notes) { box.style.display = 'none'; box.textContent = ''; return; }
    box.style.display = '';
    box.textContent = notes;
  }

  function toggleActions() {
    const download = $('btn-update-download');
    if (!download) return;
    // 有更新且真的有可下载资产时才给出「下载并安装」
    const actionable = info?.hasUpdate === true && !!info?.asset?.url;
    download.style.display = actionable ? '' : 'none';
  }

  function stopPolling() {
    if (polling) { clearInterval(polling); polling = null; }
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
      const button = $('btn-update-download');
      if (button) { button.textContent = '取消下载'; button.disabled = false; }
      return true;
    }

    stopPolling();
    downloading = false;
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
        setState('安装包已下载完成，正在启动安装程序…');
        void install(task.path);
      } else {
        setState(`安装包已就绪：${task.filename || task.path}`);
        if (button) {
          button.textContent = '安装并重启';
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

  /** 安装：启动安装包并由壳退出本程序，让出文件占用 */
  async function install(path) {
    try {
      await api.runInstaller(path, true);
      setState('安装程序已启动，本程序将退出以便完成覆盖安装。');
    } catch (error) {
      setBadge('启动失败', 'bad');
      setState(`启动安装程序失败：${error.message}`, true);
    }
  }

  async function check() {
    if (busy) return;
    busy = true;
    const button = $('btn-update-check');
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '检查中…'; }
    setBadge('检查中', 'warn');
    setState('正在查询 GitHub 上的最新发布版本…');
    try {
      info = await api.checkUpdate();
      renderVersions();
      renderNotes();
      toggleActions();
      if (info?.hasUpdate === true) {
        setBadge('有新版本', 'warn');
        setState(`发现新版本 ${info.latestVersion}（当前 ${info.currentVersion}）。`);
      } else if (info?.hasUpdate === false) {
        setBadge('已是最新', 'ok');
        setState(`当前已是最新版本（${info.currentVersion}）。`);
      } else {
        // hasUpdate 为 null：版本号无法比较（本地是开发版或 tag 非语义化）
        setBadge('无法比较', 'warn');
        setState(info?.latestVersion
          ? `最新发布版本为 ${info.latestVersion}，但当前版本号「${info.currentVersion || '未知'}」无法解析，未做新旧判断。`
          : '仓库暂无发布版本。');
      }
    } catch (error) {
      info = null;
      renderVersions();
      toggleActions();
      setBadge('检查失败', 'bad');
      setState(`检查更新失败：${error.message}`, true);
    } finally {
      busy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
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
      setState('正在下载安装包…');
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
    setBadge('未检查');
    setState('点击「检查更新」查询 GitHub 上的最新发布版本。');
  }

  $('btn-update-check')?.addEventListener('click', check);
  $('btn-update-download')?.addEventListener('click', downloadOrCancel);

  window.wbUpdatePanel = { load, check };
})();
