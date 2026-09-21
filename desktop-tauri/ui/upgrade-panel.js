/* Agent2API · 数据结构升级（旧 JSON/JSONL → 单个 SQLite 库，启动即自动执行） */
/* global workbuddyDesktop, wbApp */

/**
 * 这个模块解决什么
 * ────────────────
 * 本次更新把数据存储从「八个 JSON/JSONL 文件」换成了单个 SQLite 库
 * `{config_dir}/agent2api.db`。后端**不在启动时自动迁移**，而是启动时只探测
 * 「还有没有没搬进库的旧文件」，把结果放在 `GET /api/upgrade`；本模块拿到
 * `pending: true` 就直接调 `POST /api/upgrade/run` 把它导进来。
 *
 * ── 为什么从「弹窗问一次」改成「打开就升级」（本次改造）────────
 * 改造前这里会弹一个确认窗，用户点「升级」才导入。实测下来那个弹窗是**多余的
 * 一道坎**：升级本身没有选项、没有分支、也不能取消（不升级 = 账号与历史记录
 * 不可用，用户唯一会选的就是升级），而弹窗却要求用户先读三段说明再点一次。
 * 更糟的是它有一个「稍后」出口 —— 用户点一次「稍后」就得等下次启动才能再看到，
 * 中间那段时间账号一直是空的，看起来像坏了。
 * 现在改成打开即升级：用户什么都不用做，账号与历史记录直接就在。
 *
 * ── 旧数据为什么仍然安全 ────────────────────────────────────
 * 自动执行**不改变**「旧数据永不删除」这条硬不变量：迁移成功后旧文件只被
 * **改名**成 `{原名}.migrated` 留在原处（见 `db::migrate::backup` 的论证 ——
 * 改名是同一文件系统的元数据操作，原子、不产生第二份数据）。整个迁移框架里
 * **没有任何删除用户数据的路径**。这一点仍然要说给用户听，所以迁移开始前
 * 先 toast 一条提示、迁移结束再 toast 结果（见 `announce` / `finish`）。
 *
 * ── 为什么不用原生 confirm ────────────────────────────────────
 * Tauri 的 WebView 里原生 `confirm()` / `alert()` 不弹窗、直接放行（返回 true）
 * —— 项目注释里写明过这一点，全站危险操作才都改成了自绘弹窗
 * （见 confirm-dialog.js 的模块头）。本流程现在**不需要任何确认**，
 * 所以这里连自绘弹窗也不用了：只需要「进行中」与「失败」两种可见反馈。
 *
 * ── 失败了怎么办（自动执行必须回答的问题）─────────────────────
 * 静默失败是自动流程唯一的真风险：用户什么都没点，也就不会去看结果。
 * 所以失败走**两条**可见路径：① toast 明确报错（`type='err'`，带可操作的话）；
 * ② 后端那一侧的失败项已经写进「日志」页（`run_upgrade` 逐项记日志）。
 * 界面**不弹窗**：弹窗会打断用户当下的操作，而这次失败并不要求他立刻处理
 * —— 下次启动会自动再试一次（迁移项各自幂等）。
 *
 * ── 为什么不需要 localStorage 记「已升级」──────────────────────
 * `pending` 的真值来源是后端探测（旧文件还在不在），不是界面记忆。升级成功后
 * 旧文件全部改名，下次启动探测结果自然是 false、本模块什么都不做。界面再记一份
 * 反而会造出「界面以为升过了、后端其实还有文件」这种分叉。
 *
 * 桥契约（本文件只按此调用，不另造方法名）：
 *   getUpgrade()  → { pending, items: [可读名…] }
 *   runUpgrade()  → { outcomes: [{label, imported}], pending }
 */
(() => {
  const api = workbuddyDesktop;
  const { toast } = wbApp;

  /** 导入中：挡重入（`check` 可能被多次调用，见它的说明） */
  let running = false;
  /** 本会话是否已经跑过（同一次运行里 refresh 会反复问，不必反复试） */
  let attempted = false;

  /**
   * 迁移完成后让界面数据跟上。
   *
   * 复用**各面板已有的加载入口**，不另造一套刷新：
   *   · `wbApp.refresh()` 是主界面的整体刷新（重拉 /api/session 并重绘账号列表、
   *     导航计数、顶栏状态）—— 账号是这次迁移最要紧的一块（没升级时它一直空着）；
   *   · 各页自持数据（日志 / 请求记录 / 报表 / 设置里的存储概况 / 脱敏词表）
   *     由各自的 `load()` 重拉，它们是迁移直接改动的数据源。
   * 全部用可选链：某个面板还没执行到（脚本顺序）或不在当前页时跳过，
   * 绝不让一次刷新把升级结果的展示拖崩。
   */
  async function refreshPanels() {
    await wbApp.refresh?.();
    void window.wbLogsPanel?.load?.();
    void window.wbRequestsPanel?.load?.();
    void window.wbReport?.load?.();
    void window.wbSettingsPanel?.load?.();
  }

  /**
   * 启动时探测一次：有待迁移就直接导入（**不弹窗、不等用户**）。
   *
   * 由 app.js 在 DOMContentLoaded 里调（那时各面板脚本已执行完，
   * `wbApp` 与所有 `wb*Panel` 都挂好了）。
   *
   * 探测失败静默：后端不可用时连状态页都打不开，再报一个「读不到升级状态」
   * 只会添乱；下一次启动照常会问。
   *
   * ── 为什么先 toast 再 POST ──────────────────────────────────
   * 迁移要读几十 MB 旧文件、写一整批数据库行，实测有可感的耗时。这一条 toast
   * 是**开始**的信号（用户看到「正在升级」才知道程序在忙而不是卡了），
   * 完成时由 `finish` 再报一条结果。
   */
  async function check() {
    if (attempted || running) return;
    let info = null;
    try {
      info = await api.getUpgrade();
    } catch (error) {
      console.warn('读取数据结构升级状态失败:', error.message);
      return;
    }
    if (info?.pending !== true) return;
    attempted = true;
    running = true;
    // 「旧数据不会被删除」这句是这条 toast 最要紧的信息：用户没有点过任何东西，
    // 却看到程序在搬他的数据，第一反应会是「我的文件呢」。
    toast('正在把旧数据导入 SQLite 数据库（旧文件会保留，不会删除）…');
    let result = null;
    try {
      result = await api.runUpgrade();
    } catch (error) {
      running = false;
      // 自动流程的失败必须说清「数据还在」与「会自动重试」——否则用户无从判断
      // 要不要做什么。后端也已把失败项写进日志页（见模块头）。
      toast(`数据升级失败：${error.message}（旧数据仍在原处，下次启动会自动重试）`, 'err');
      return;
    }
    running = false;
    finish(result);
  }

  /**
   * 收尾：报结果 + 刷新界面。
   *
   * 三种结果分别措辞（都从 `result` 的两个字段判出，不猜）：
   *   · `pending === false` 且导入了东西 → 成功；
   *   · `pending === false` 但一项都没导 → 没什么可导的（旧文件早被别处迁过）；
   *   · `pending === true` → 还有项没成（某一项失败且旧文件留在原处），
   *     报成错误并说明会自动重试。
   */
  function finish(result) {
    const outcomes = Array.isArray(result?.outcomes) ? result.outcomes : [];
    const imported = outcomes.length;
    if (result?.pending) {
      toast(`数据升级未全部完成（已导入 ${imported} 项，仍有数据待导入，下次启动会自动重试）`, 'err');
    } else if (imported) {
      toast(`✅ 数据升级完成（${imported} 项）`);
    } else {
      toast('数据升级：没有需要导入的旧数据');
    }
    void refreshPanels();
  }

  window.wbUpgradePanel = { check };
})();
