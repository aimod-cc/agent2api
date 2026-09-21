/* Agent2API · 定时任务面板（开关 / 间隔 / 立即执行） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的定时任务面板：自持清单与逐条的编辑态，
 * 通过 window.wbTasksPanel 暴露 load 给 app.js（切到该页时加载）。
 * 与 logs-panel.js / settings-panel.js 同构，依赖 window.wbApp 的 esc / toast / refresh。
 *
 * ── 这一页管两类任务（接口也是两组）──────────────────────────
 *   · **间隔型**（凭证自动维护 / 定时查询积分 / 模型目录刷新 / 软件版本检查 /
 *     日志页自动刷新 / 请求日志自动刷新 / 报表自动刷新）
 *     —— 形状统一：`{enabled, interval, unit}`，走 /api/scheduled-tasks。
 *   · **自动签到** —— 每天定点型：`{enabled, time}` 外加当天去重与启动补签，
 *     走 /api/auto-checkin。它与间隔型不是同一个形状，所以后端也是两组接口
 *     （理由见 `core::scheduled_tasks` 与 `api::scheduled_tasks` 的模块头）。
 *     界面上两者收在同一页，用同一套卡片观感，只是签到多一个时刻输入框。
 *
 * ── 前端任务与后端任务的区别（界面上要看得出来）──────────────
 * `runner: "frontend"` 的三条（日志页 / 请求日志页 / 报表页自动刷新）执行者是
 * **页面自己**，所以它们没有「上次执行 / 下次执行」，也不给「立即执行」按钮 ——
 * 按钮点了也没有任何东西可跑。改成配置后由 `logs-panel` / `requests-panel` /
 * `report` 自己的定时器读取（它们原先写死在本地的轮询已换成配置值，
 * 见那三个文件的 startAuto）。
 *
 * ── 任务说明改成问号（与设置页同款）──────────────────────────
 * 七条任务原先各自常驻一整段说明（`.task-desc`），叠起来是一屏灰字，而它其实
 * 只在「第一次配置」时需要读一遍。现在收进标题旁的 `.tip-q` + `data-tip`
 * （由 tooltip.js 增强成气泡），卡片本身只剩标题、状态与控件。
 * 间隔型那七条的文案由后端随任务下发；自动签到的不在注册表里，文案写在
 * 本文件（见 `CHECKIN_DESC`）。
 *
 * ── 保存时机：开关立即存、间隔失焦才存 ────────────────────────
 * 开关是「拨一下就该生效」的动作（与设置页其它开关一致）；间隔输入框若每敲一位
 * 就提交，改「10」→「60」会先存成 1（越界 400）再存成 6，用户看到的是一串报错。
 * 所以间隔走 change（失焦或回车），与设置页保留天数的处理完全相同。
 */

(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc, toast, refresh } = wbApp;

  /** 自动签到在界面上的 id：它不是间隔型任务，但要在同一个列表里排序与定位 */
  const CHECKIN_ID = 'autoCheckin';
  /**
   * 自动签到的说明文案（问号 tooltip 的内容）。
   *
   * 它不来自后端：签到不走 `/api/scheduled-tasks`（形状不同，见文件头），
   * 后端那份注册表里没有这一条，所以文案只能写在这里。其余六条的说明都由
   * 后端随任务下发（`TaskDef::description`），前端不抄一份。
   */
  const CHECKIN_DESC = '到点后自动签到勾选提供商的已启用账号（WorkBuddy 走每日签到接口，仅限国内版；'
    + '小浣熊走桌面端每日积分链路；AutoClaw 走官方客户端的每日签到任务；国际版账号没有签到活动，会被跳过）。'
    + '多个账号串行执行，避免同时请求触发上游风控。若启动时当天还没签过，会立即补签一次，'
    + '不会因为当时没开机而漏掉。各家签到接口都是幂等的，重复执行不会重复领取。';
  /** 页面可见时的自动同步间隔：与 app.js 的主状态轮询同频，页面不可见时不跑 */
  const SYNC_MS = 20_000;

  let panelBusy = false;
  /** 最近一次拉到的任务清单（间隔型各条） */
  let tasks = [];
  /** 最近一次拉到的自动签到状态 */
  let checkin = null;
  /**
   * 是否已经成功拉到过数据。
   *
   * 用来区分「还在加载」与「加载失败」——两者都是 tasks 空 + checkin 空，
   * 只看数据会把「后端不可用」显示成永远转不完的「正在加载…」，
   * 而那正是用户最需要知道「不是慢、是坏了」的时候。
   */
  let loaded = false;
  /** 页面同步定时器 */
  let timer = null;

  // ─── 工具 ────────────────────────────────────

  /** 间隔单位的中文与秒数换算；单位由后端给定，前端不自己猜 */
  function unitOf(unit) {
    return unit === 'seconds'
      ? { label: '秒', ms: 1000 }
      : { label: '分钟', ms: 60_000 };
  }

  /** 把毫秒时间戳化成「时:分:秒」，供上次 / 下次执行展示 */
  function clockOf(value) {
    const ms = Number(value) || 0;
    if (!ms) return '';
    return new Date(ms).toLocaleTimeString('zh-CN', { hour12: false });
  }

  /** 「下次执行」的相对说法：比只给一个时刻更有用（用户关心的是还有多久） */
  function describeNext(value) {
    const ms = Number(value) || 0;
    if (!ms) return '';
    const diff = ms - Date.now();
    if (diff <= 0) return '即将执行';
    const seconds = Math.round(diff / 1000);
    if (seconds < 60) return `${seconds} 秒后`;
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return `${minutes} 分钟后`;
    const hours = Math.round(minutes / 60);
    if (hours < 24) return `${hours} 小时后`;
    return `${Math.round(hours / 24)} 天后`;
  }

  // ─── 渲染：间隔型任务 ────────────────────────

  /**
   * 一条任务的卡片。
   *
   * 输入框带 `data-task-input` 标记：`render` 重绘时不整块重建 DOM，
   * 而是就地更新（见 `renderTask` 的说明）—— 所以这里只负责生成初始结构。
   *
   * ── 介绍文案为什么是问号（与设置页同款）──────────────────────
   * 原先每条任务的说明是一整段常驻的 `.task-desc`，七条任务叠起来是一屏
   * 密不透风的灰字，而它其实是「第一次配置时才需要读一遍」的内容。
   * 改成 `tip-q` + `data-tip`（tooltip.js 增强成气泡）后，说明挪到鼠标
   * 悬停处，卡片本身只剩下标题与运行状态 —— 与设置页各面板标题的做法一致。
   *
   * `data-tip` 的内容经 `esc` 转义：说明里含中文引号与括号，但**不含 HTML**，
   * 转义只是防御（后端文案将来若带上尖括号，不至于把 tooltip 撑坏）。
   */
  function taskCard(task) {
    const unit = unitOf(task.unit);
    // 有没有「立即执行」按钮由后端的 canRun 决定（而不是前端按 runner 猜）：
    // 将来若出现「后端执行但不可手动触发」这类任务，判据改在注册表一处即可
    const backend = task.runner === 'backend';
    const state = [];
    if (task.lastRunAt) {
      state.push(`上次执行 ${esc(clockOf(task.lastRunAt))}`
        + (task.lastResult ? `（${esc(task.lastResult)}）` : ''));
    } else {
      state.push(backend ? '本次启动后还未执行' : '由页面按间隔自动刷新');
    }
    if (backend && task.enabled && task.nextRunAt) {
      state.push(`下次执行 ${esc(clockOf(task.nextRunAt))}（${esc(describeNext(task.nextRunAt))}）`);
    }

    return `
      <div class="task-item" data-task="${esc(task.id)}">
        <div class="task-main">
          <div class="task-title">
            <label class="switch">
              <input type="checkbox" data-task-toggle ${task.enabled ? 'checked' : ''}>
              <span class="track"></span>
              <span class="task-name">${esc(task.label)}</span>
            </label>
            <span class="tip-q" data-tip="${esc(task.description)}"></span>
            <span class="badge ${task.enabled ? 'ok' : ''} task-badge" data-task-badge>${task.enabled ? '已开启' : '已关闭'}</span>
            ${task.running ? '<span class="badge warn task-badge">执行中…</span>' : ''}
          </div>
          <div class="task-state" data-task-state>${state.join('　·　')}</div>
        </div>
        <div class="task-actions">
          <span class="task-interval">
            <span class="task-interval-label">每</span>
            <input type="number" data-task-interval inputmode="numeric"
                   min="${task.min}" max="${task.max}" step="1" value="${task.interval}"
                   title="可填 ${task.min}–${task.max} ${unit.label}（默认 ${task.defaultInterval} ${unit.label}）">
            <span class="task-interval-unit">${unit.label}</span>
          </span>
          ${task.canRun === true ? `
          <button class="sm" data-task-run ${task.running ? 'disabled' : ''}>立即执行</button>
          ` : ''}
        </div>
      </div>`;
  }

  /** 自动签到卡片：形状与间隔型不同（时刻而非间隔），所以单独渲染 */
  function checkinCard() {
    const data = checkin;
    const enabled = data?.enabled === true;
    const state = [];
    if (!data) {
      state.push('后端未返回自动签到设置');
    } else {
      state.push(enabled ? `每天 ${esc(data.time)} 自动签到` : '未开启，账号需要手动签到');
      if (enabled && data.nextRunAt) {
        state.push(`下次执行 ${esc(clockOf(data.nextRunAt))}（${esc(describeNext(data.nextRunAt))}）`);
      }
      if (data.lastResult) {
        const result = data.lastResult;
        const when = result.at ? new Date(Number(result.at)).toLocaleString('zh-CN', { hour12: false }) : '';
        const head = `${when ? `${when} ` : ''}上次执行（${esc(result.reason || '定时')}）：`
          + `${Number(result.succeeded) || 0}/${Number(result.total) || 0} 个成功`;
        const extras = [];
        if (Number(result.skipped)) extras.push(`跳过 ${Number(result.skipped)} 个`);
        if (Number(result.failedCount)) extras.push(`失败 ${Number(result.failedCount)} 个`);
        // 失败明细只列前两条，与账号页的展示密度一致
        const failed = Array.isArray(result.failed) && result.failed.length
          ? `（${esc(result.failed.slice(0, 2).join('；'))}${result.failed.length > 2 ? ' 等' : ''}）`
          : '';
        state.push(`${head}${extras.length ? `，${extras.join('、')}` : ''}${failed}`);
      }
    }
    // 签到提供商复选框：选项与默认勾选都由后端下发（providerOptions / providers），
    // 前端不抄一份清单 —— 以后加第三家时只改后端
    const providers = Array.isArray(data?.providers) ? data.providers : [];
    const options = Array.isArray(data?.providerOptions) ? data.providerOptions : [];
    return `
      <div class="task-item" data-task="${CHECKIN_ID}">
        <div class="task-main">
          <div class="task-title">
            <label class="switch">
              <input type="checkbox" id="task-checkin-toggle" ${enabled ? 'checked' : ''} ${data ? '' : 'disabled'}>
              <span class="track"></span>
              <span class="task-name">自动签到</span>
            </label>
            <span class="tip-q" data-tip="${esc(CHECKIN_DESC)}"></span>
            <span class="badge ${enabled ? 'ok' : ''} task-badge" id="task-checkin-badge">${
              !data ? '不可用' : enabled ? (data.lastFiredToday ? '今日已执行' : '已开启') : '已关闭'}</span>
            ${data?.running ? '<span class="badge warn task-badge">执行中…</span>' : ''}
          </div>
          <div class="task-providers" id="task-checkin-providers">
            <span class="lead">签到提供商：</span>
            ${options.map(option => `
            <label class="check">
              <input type="checkbox" data-checkin-provider="${esc(option.id)}"
                     ${providers.includes(option.id) ? 'checked' : ''} ${data ? '' : 'disabled'}>
              <span>${esc(option.label)}</span>
            </label>`).join('')}
          </div>
          <div class="task-state" id="task-checkin-state">${state.join('　·　')}</div>
        </div>
        <div class="task-actions">
          <span class="task-interval">
            <span class="task-interval-label">每天</span>
            <input type="time" id="task-checkin-time" value="${esc(data?.time || '00:01')}" ${data ? '' : 'disabled'}>
          </span>
          <button class="sm" id="btn-task-checkin-logs">查看签到日志</button>
          <button class="sm" id="btn-task-checkin-run" ${data ? '' : 'disabled'}>立即签到</button>
        </div>
      </div>`;
  }

  /**
   * 整块重建时的卡片序列。
   *
   * ── 为什么后六条要裹一层两栏网格 ─────────────────────────────
   * 一条任务一整行时，八条要滚两屏，而每行右侧动作区之外大片留白（任务名与
   * 状态都短）。自动签到与凭证自动维护两条留在整行：前者自带提供商勾选与
   * 两个按钮，本来就比别的卡片高，压进半栏会挤成一团；后者留一行，也让下面的
   * 分栏看着是「其中一段收成两栏」而不是整页换了排版。
   *
   * 后六条按两栏三行排，**列优先**（顺序即阅读顺序）：左栏三条是后端任务
   * （定时查询积分 / 模型目录刷新 / 软件版本检查，都带「立即执行」），
   * 右栏三条是页面任务（日志页 / 请求日志页 / 报表页自动刷新，执行者是页面自己的
   * 定时器，所以没有按钮）—— 分栏恰好把这两类分开，扫一眼就知道哪一栏是
   * 「网关在跑」、哪一栏是「页面在跑」。
   *
   * 分栏由 CSS 的 flex / grid 布局实现（`.task-grid`，见 page-tasks.css）；
   * 这里只负责把后六条裹进那个容器。条数将来变了也只是分栏比例变，不会错位。
   */
  const LEAD_CARDS = 2;   // 整行铺开的条数：自动签到 + 凭证自动维护

  function cardsHtml() {
    const cards = [checkinCard(), ...tasks.map(taskCard)];
    const rest = cards.slice(LEAD_CARDS);
    return cards.slice(0, LEAD_CARDS).join('')
      + (rest.length ? `<div class="task-grid">${rest.join('')}</div>` : '');
  }

  /**
   * 整页重绘。
   *
   * 只在**结构变化**时重建 DOM（任务增减）；平常只就地更新每张卡片的文字与控件值
   * ——重建会让正在编辑的间隔输入框丢焦点、正在输入的数字被冲掉
   * （与设置页保留天数、账号表列的同类处理一致）。
   */
  function render() {
    const list = $('task-list');
    const badge = $('tasks-badge');
    if (!list) return;

    // 一条任务都没有：区分「还在加载」与「加载失败 / 后端没返回任务」
    if (!tasks.length && !checkin) {
      list.innerHTML = loaded
        ? '<div class="log-empty">没有可显示的定时任务。后端未返回任务清单，请确认网关正在运行。</div>'
        : '<div class="log-empty">正在加载定时任务…</div>';
      if (badge) { badge.className = 'badge bad'; badge.textContent = loaded ? '不可用' : '—'; }
      return;
    }

    // 期望的卡片顺序：自动签到在最前（用户最关心的那条），其余按后端给的顺序
    const wanted = [CHECKIN_ID, ...tasks.map(task => task.id)];
    const existing = [...list.querySelectorAll('.task-item')].map(item => item.dataset.task);
    const sameShape = existing.length === wanted.length
      && existing.every((id, index) => id === wanted[index]);

    if (!sameShape) {
      // 结构变了：整块重建（事件走容器委托，不必重新绑定）
      list.innerHTML = cardsHtml();
    } else {
      // 结构没变 → 只更新每张卡的值（保留焦点与编辑中的输入）
      tasks.forEach(task => updateTask(task));
      updateCheckin();
    }

    const enabledCount = tasks.filter(task => task.enabled).length + (checkin?.enabled ? 1 : 0);
    if (badge) {
      badge.className = `badge ${enabledCount ? 'ok' : ''}`.trim();
      badge.textContent = `${enabledCount} / ${tasks.length + 1} 个已开启`;
    }
    // 顶栏那块是本页徽标的镜像（见 app.js 的 renderTopbarStatus），这里重绘完
    // 顺手让它跟上 —— 否则要等下一次主状态轮询（20 秒）才会同步。
    // 与 keys-panel.js 的做法一致；走可选链，非桌面环境下静默跳过。
    wbApp.renderTopbarStatus?.();
  }

  /** 就地更新一条间隔型任务（结构不变时走这里，保住输入框焦点） */
  function updateTask(task) {
    const item = document.querySelector(`.task-item[data-task="${task.id}"]`);
    if (!item) return;
    const toggle = item.querySelector('[data-task-toggle]');
    if (toggle) toggle.checked = task.enabled;
    const badge = item.querySelector('[data-task-badge]');
    if (badge) {
      badge.className = `badge ${task.enabled ? 'ok' : ''} task-badge`.trim();
      badge.textContent = task.enabled ? '已开启' : '已关闭';
    }
    // 间隔只在用户没在编辑时回填：否则一次同步会把敲到一半的数字冲掉
    const interval = item.querySelector('[data-task-interval]');
    if (interval && document.activeElement !== interval) interval.value = String(task.interval);
    const run = item.querySelector('[data-task-run]');
    if (run) run.disabled = task.running === true;

    const state = [];
    if (task.lastRunAt) {
      state.push(`上次执行 ${clockOf(task.lastRunAt)}${task.lastResult ? `（${task.lastResult}）` : ''}`);
    } else {
      state.push(task.runner === 'backend' ? '本次启动后还未执行' : '由页面按间隔自动刷新');
    }
    if (task.runner === 'backend' && task.enabled && task.nextRunAt) {
      state.push(`下次执行 ${clockOf(task.nextRunAt)}（${describeNext(task.nextRunAt)}）`);
    }
    const stateBox = item.querySelector('[data-task-state]');
    if (stateBox) stateBox.textContent = state.join('　·　');
  }
  /** 就地更新自动签到卡片 */
  function updateCheckin() {
    const toggle = $('task-checkin-toggle');
    const time = $('task-checkin-time');
    const badge = $('task-checkin-badge');
    const stateBox = $('task-checkin-state');
    if (!toggle || !time || !badge || !stateBox) return;

    if (!checkin) {
      toggle.disabled = true;
      time.disabled = true;
      badge.className = 'badge bad task-badge';
      badge.textContent = '不可用';
      stateBox.textContent = '后端未返回自动签到设置';
      return;
    }

    const enabled = checkin.enabled === true;
    toggle.disabled = false;
    time.disabled = false;
    toggle.checked = enabled;
    // 只在用户没在编辑时回填时刻（与间隔输入框同理）
    if (document.activeElement !== time && typeof checkin.time === 'string') time.value = checkin.time;
    badge.className = `badge ${enabled ? 'ok' : ''} task-badge`.trim();
    badge.textContent = enabled ? (checkin.lastFiredToday ? '今日已执行' : '已开启') : '已关闭';
    // 提供商勾选：就地回填（勾选是瞬时动作，一般没有编辑中的焦点冲突）
    const picked = Array.isArray(checkin.providers) ? checkin.providers : [];
    document.querySelectorAll('#task-checkin-providers input[data-checkin-provider]')
      .forEach(input => { input.checked = picked.includes(input.dataset.checkinProvider); });

    const lines = [];
    if (!enabled) {
      lines.push('未开启，账号需要手动签到');
    } else {
      lines.push(`每天 ${checkin.time} 自动签到`);
      if (checkin.nextRunAt) {
        lines.push(`下次执行 ${clockOf(checkin.nextRunAt)}（${describeNext(checkin.nextRunAt)}）`);
      }
    }
    const result = checkin.lastResult;
    if (result) {
      const when = result.at ? new Date(Number(result.at)).toLocaleString('zh-CN', { hour12: false }) : '';
      const head = `${when ? `${when} ` : ''}上次执行（${result.reason || '定时'}）：`
        + `${Number(result.succeeded) || 0}/${Number(result.total) || 0} 个成功`;
      const extras = [];
      if (Number(result.skipped)) extras.push(`跳过 ${Number(result.skipped)} 个`);
      if (Number(result.failedCount)) extras.push(`失败 ${Number(result.failedCount)} 个`);
      const failed = Array.isArray(result.failed) && result.failed.length
        ? `（${result.failed.slice(0, 2).join('；')}${result.failed.length > 2 ? ' 等' : ''}）`
        : '';
      lines.push(`${head}${extras.length ? `，${extras.join('、')}` : ''}${failed}`);
    }
    stateBox.textContent = lines.join('　·　');
  }

  // ─── 加载 ────────────────────────────────────

  /** 拉一次清单（结构与数据都更新） */
  async function load() {
    try {
      const [list, checkinState] = await Promise.all([
        api.getScheduledTasks(),
        // 签到失败不该让整页不可用：单独 catch，降级成「不可用」的那张卡片
        api.getAutoCheckin().catch(error => {
          console.warn('读取自动签到设置失败:', error.message);
          return null;
        }),
      ]);
      tasks = Array.isArray(list?.tasks) ? list.tasks : [];
      checkin = checkinState;
      loaded = true;
      render();
    } catch (error) {
      console.warn('读取定时任务失败:', error.message);
      tasks = [];
      checkin = null;
      loaded = true;   // 标记「尝试过了」，于是上面的空态显示成失败说明而不是加载中
      render();
    }
  }

  /**
   * 静默同步：只更新运行状态（上次 / 下次执行），不动结构。
   * 每 20 秒一次，跟随 app.js 的主状态轮询节奏；页面不可见时不请求。
   */
  async function sync() {
    if (panelBusy || document.hidden || wbApp.currentPage !== 'tasks') return;
    try {
      const [list, checkinState] = await Promise.all([
        api.getScheduledTasks(),
        api.getAutoCheckin().catch(() => null),
      ]);
      const next = Array.isArray(list?.tasks) ? list.tasks : [];
      // 任务集合变了（理论上只有升级后才会）→ 走完整重绘
      if (next.length !== tasks.length) {
        tasks = next;
        checkin = checkinState;
        render();
        return;
      }
      tasks = next;
      checkin = checkinState;
      tasks.forEach(task => updateTask(task));
      updateCheckin();
    } catch (error) {
      // 静默：下一次同步自然会重试，不必打扰正在看页面的人
      console.warn('同步定时任务状态失败:', error.message);
    }
  }

  function startSync() {
    stopSync();
    timer = setInterval(() => { void sync(); }, SYNC_MS);
  }

  function stopSync() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  // ─── 操作：间隔型任务 ────────────────────────

  /** 按 id 找最近一次拉到的任务（事件回调里拿到的只有 id） */
  function taskById(id) {
    return tasks.find(task => task.id === id) || null;
  }

  /**
   * 保存一条任务并就地重绘。
   *
   * 以接口返回值为准（后端会把非法值夹到范围内并在必要时回落默认值），
   * 所以不能「按前端算的值渲染」—— 那会在后端实际拒绝时显示成已生效。
   */
  async function saveTask(id, patch, label) {
    if (panelBusy) return;
    panelBusy = true;
    try {
      const saved = await api.saveScheduledTask(id, patch);
      tasks = tasks.map(task => (task.id === id ? saved : task));
      updateTask(saved);
      pushAutoRefresh(saved);
      const enabledCount = tasks.filter(task => task.enabled).length + (checkin?.enabled ? 1 : 0);
      const badge = $('tasks-badge');
      if (badge) badge.textContent = `${enabledCount} / ${tasks.length + 1} 个已开启`;
      toast(`✅ 已更新「${label}」`);
    } catch (error) {
      toast(`保存失败：${error.message}`, 'err');
      // 回滚到后端的真实状态：失败时界面显示的可能是用户刚拨过去的假值
      await load();
    } finally {
      panelBusy = false;
    }
  }

  /**
   * 把三条**前端**任务的配置推给执行者（日志页 / 请求日志页 / 报表页的定时器）。
   *
   * 只有这三条需要推：后端任务由后端的循环自己读配置（下一轮生效），
   * 而前端页面的定时器长在各自的模块里，改完得有人告诉它们。
   *
   * 走可选链：那几个面板可能还没加载（加载顺序上本文件排在它们之后，
   * 所以正常都就绪；但万一脚本加载失败，这里不该抛错把保存流程带崩）。
   * 它们在启动时也会自读一次配置，所以推失败不会留下不一致。
   */
  function pushAutoRefresh(task) {
    if (task.id === 'logsAutoRefresh') {
      window.wbLogsPanel?.applyAutoRefresh?.(task);
    } else if (task.id === 'requestsAutoRefresh') {
      window.wbRequestsPanel?.applyAutoRefresh?.(task);
    } else if (task.id === 'reportAutoRefresh') {
      window.wbReport?.applyAutoRefresh?.(task);
    }
  }

  /** 间隔提交前的本地校验：与后端同一范围（范围由后端随任务下发，两边不会漂） */
  function parseInterval(task, raw) {
    const text = String(raw ?? '').trim();
    const unit = unitOf(task.unit).label;
    if (!text) return { ok: false, message: `间隔不能为空（可填 ${task.min}–${task.max} ${unit}）` };
    if (!/^\d+$/.test(text)) return { ok: false, message: '间隔必须是整数' };
    const value = Number(text);
    if (value < task.min || value > task.max) {
      return { ok: false, message: `间隔必须在 ${task.min}–${task.max} ${unit} 之间（当前填的是 ${text}）` };
    }
    return { ok: true, value };
  }

  async function submitInterval(task, input) {
    const parsed = parseInterval(task, input.value);
    if (!parsed.ok) {
      toast(parsed.message, 'err');
      input.value = String(task.interval);
      return;
    }
    // 与后端一致就不发请求：数字框里换个写法（如 010）也会触发 change
    if (parsed.value === task.interval) { input.value = String(task.interval); return; }
    await saveTask(task.id, { interval: parsed.value }, `${task.label}间隔`);
  }

  async function runTask(id, button) {
    const task = taskById(id);
    if (!task || panelBusy) return;
    panelBusy = true;
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '执行中…'; }
    try {
      const result = await api.runScheduledTask(id);
      if (result?.task) {
        tasks = tasks.map(item => (item.id === id ? result.task : item));
        updateTask(result.task);
      }
      toast(`✅ ${task.label}：${result?.summary || '已执行'}`);
      // 凭证刷新会改账号页的有效期 / 凭证状态，顺手刷新主界面
      if (id === 'credentialMaintenance') await refresh?.();
      // 立即查询积分刚写下一份新快照，让账号页马上应用它 ——
      // 否则用户点完「立即执行」切到账号页，看到的还是上一次的旧余额
      // （要等 20 秒那一轮轮询才跟上，那正是「点了像没反应」）
      if (id === 'usageQuery') await window.wbAccountsView?.syncBalancesSnapshot?.();
    } catch (error) {
      toast(`执行失败：${error.message}`, 'err');
      await load();
    } finally {
      panelBusy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
  }

  // ─── 操作：自动签到 ──────────────────────────

  async function saveCheckin(patch, label) {
    if (panelBusy) return;
    panelBusy = true;
    const toggle = $('task-checkin-toggle');
    const time = $('task-checkin-time');
    if (toggle) toggle.disabled = true;
    if (time) time.disabled = true;
    $('task-checkin-providers')?.querySelectorAll('input[data-checkin-provider]')
      .forEach(input => { input.disabled = true; });
    try {
      checkin = await api.saveAutoCheckin(patch);
      updateCheckin();
      toast(`✅ 已更新「${label}」`);
    } catch (error) {
      toast(`保存失败：${error.message}`, 'err');
      // 回滚到后端的真实状态
      checkin = await api.getAutoCheckin().catch(() => null);
      updateCheckin();
    } finally {
      panelBusy = false;
      if (toggle) toggle.disabled = false;
      if (time) time.disabled = false;
      $('task-checkin-providers')?.querySelectorAll('input[data-checkin-provider]')
        .forEach(input => { input.disabled = false; });
    }
  }

  /** 收集签到提供商复选框的当前勾选（change 事件里拼 patch 用） */
  function pickedProviders() {
    return [...document.querySelectorAll('#task-checkin-providers input[data-checkin-provider]')]
      .filter(input => input.checked)
      .map(input => input.dataset.checkinProvider);
  }

  async function runCheckin(button) {
    if (panelBusy) return;
    panelBusy = true;
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '签到中…'; }
    try {
      const result = await api.runAutoCheckinNow();
      const succeeded = Number(result?.succeeded) || 0;
      const total = Number(result?.total) || 0;
      const failed = Number(result?.failedCount) || 0;
      if (failed) {
        toast(`签到完成：${succeeded}/${total} 成功，${failed} 个失败`, 'err');
      } else {
        toast(`✅ 签到完成：${succeeded}/${total} 个账号成功领取`);
      }
      // run 的响应把 state 合并进来了（见 api::auto_checkin::run_now），
      // 于是这里不必再跑一趟 GET
      if (result) { checkin = result; updateCheckin(); }
      // 积分可能已变化，顺带刷新账号页的余额展示
      await refresh?.();
    } catch (error) {
      toast(`签到失败：${error.message}`, 'err');
      checkin = await api.getAutoCheckin().catch(() => null);
      updateCheckin();
    } finally {
      panelBusy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
  }

  // ─── 事件绑定（全部走容器委托，只绑一次）────────

  /**
   * 绑定 `#task-list` 上的交互。
   *
   * 用**委托**挂在容器上而不是逐个绑控件：卡片会被整块重建（任务增减、
   * 加载失败后重试），逐个绑就得在每次重建后重跑一遍，漏一次那个控件就哑了。
   * 容器 `#task-list` 本身是 index.html 里的静态元素，所以只需绑一次
   * —— 重建的是它的内容，不是它自己。
   *
   * 判定一律用 `closest('.task-item')` + 元素标记，而不是「先看是不是某个 id」：
   * 签到的开关与时刻输入是模板生成的，靠 id 判会把「签到「间隔型」两条路径
   * 混在同一个监听里，加一个控件就要多写一处判断。
   */
  function bindList() {
    const list = $('task-list');
    if (!list) return;

    // change 覆盖四种提交：任务开关、任务间隔、签到开关、签到时刻
    list.addEventListener('change', event => {
      const target = event.target;

      if (target.id === 'task-checkin-toggle') {
        void saveCheckin(
          { enabled: target.checked, time: $('task-checkin-time').value },
          '自动签到开关',
        );
        return;
      }
      // 时刻用 change 而不是 input：拖动时间选择器时不该每动一下就发请求
      if (target.id === 'task-checkin-time') {
        void saveCheckin(
          { enabled: $('task-checkin-toggle').checked, time: target.value },
          '签到触发时刻',
        );
        return;
      }
      // 签到提供商勾选：按收集到的完整清单保存（后端校验至少一家）
      if (target.matches('[data-checkin-provider]')) {
        void saveCheckin({ providers: pickedProviders() }, '签到提供商');
        return;
      }

      const item = target.closest('.task-item');
      if (!item) return;
      const task = taskById(item.dataset.task);
      if (!task) return;
      if (target.matches('[data-task-toggle]')) {
        void saveTask(task.id, { enabled: target.checked }, `${task.label}开关`);
      } else if (target.matches('[data-task-interval]')) {
        void submitInterval(task, target);
      }
    });

    // 回车等价于「失焦提交」：不同内核里 Enter 是否派发 change 并不一致，
    // 这里主动 blur 一次把它统一成「值已提交」这一条路径
    list.addEventListener('keydown', event => {
      if (event.key !== 'Enter') return;
      const target = event.target;
      if (target.matches('[data-task-interval]') || target.id === 'task-checkin-time') {
        target.blur();
      }
    });

    list.addEventListener('click', event => {
      if (event.target.id === 'btn-task-checkin-run') {
        void runCheckin(event.target);
        return;
      }
      // 「查看签到日志」：跳到日志页并把分类筛选预设成「自动签到」
      if (event.target.id === 'btn-task-checkin-logs') {
        void window.wbLogsPanel?.showCategory?.('checkin');
        return;
      }
      const run = event.target.closest('[data-task-run]');
      if (run) void runTask(run.closest('.task-item')?.dataset.task, run);
    });
  }

  // ─── 初始化 ──────────────────────────────────

  $('btn-tasks-refresh')?.addEventListener('click', () => load().then(() => toast('定时任务已刷新')));

  bindList();
  startSync();

  window.wbTasksPanel = { load };

  // 首屏自持加载：app.js 的 showPage 在脚本加载前已执行过，
  // 若上次停留在本页，这里补一次加载，避免一直停在「正在加载…」
  if (wbApp.currentPage === 'tasks') void load();
})();
