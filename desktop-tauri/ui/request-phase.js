/* Agent2API · 请求日志的「转发阶段」渲染（连接中 / 等待响应 / 响应中 / 重试中） */
/* global wbApp */

/**
 * 进行中请求的**阶段**读数：徽章文案、配色类名、阶段计时。
 *
 * ── 为什么单独一个模块 ──────────────────────────────────────
 * 同一套阶段要在**三处**渲染，且三处必须逐字一致（口径漂了就会看到
 * 「列表说响应中、详情说等待响应」这种自相矛盾）：
 *   · requests-panel.js —— 列表的状态列（徽章 + 阶段计时第二行）
 *   · request-detail.js —— 详情弹窗「请求详情」的状态一格
 *   · 将来任何读同一行数据的地方
 * 文案与配色各写一遍太容易漏改，所以集中在这里；调用方只提供行对象。
 *
 * ── 数据从哪来（后端契约）──────────────────────────────────
 * 后端在**在途**期间把阶段写进那一行（`requests.phase` /
 * `phase_started_at`，见 `core::upstream::usage::LogPhase`），列表接口透出：
 *   · `phase`          四个字面量之一：connecting / waiting / streaming /
 *                      retrying；**空串 = 不在途**（终态行、旧行、更早版本
 *                      写入的行）——那时回落到通用的「进行中」；
 *   · `phaseElapsedMs` 在当前阶段里已持续的毫秒数（服务端现算，只对在途行有值）。
 * 字面量与 OmniProxy 的 `LogPhase` 逐字相同，文案也照那边取（连接中 /
 * 等待响应 / 响应中 / 重试中），于是两边的读法可以直接对照。
 *
 * ── 为什么文案是「响应中」而不是笼统的「进行中」──────────────
 * 「进行中」只回答「跑没跑完」，而用户盯着一条卡住的请求时真正要问的是
 * **卡在哪一步**：在连上游、在等模型出第一个字、在往下刷内容、还是在重试换号。
 * 这四个阶段各有各的排查方向（对照 OmniProxy 的请求日志，那四个标签就是它的
 * 全部状态读数），笼统的「进行中」把这条信息抹掉了。旧行没有阶段可读时才回落
 * 到「进行中」——那时确实没有更细的事实可说。
 *
 * ── 为什么判据不在这里 ──────────────────────────────────────
 * 「这一行是不是进行中」的判据是 `status === 0 && !error`（见 requests-panel.js
 * 的 isRunning），本模块只在**调用方已经判定为进行中**之后负责「怎么显示」。
 * 不把判据搬进来：那个判据同时在读两列，与 `isOk` 是一对，分开放会让
 * 「成功 / 失败 / 进行中」三态散在两个文件里。
 */
(() => {
  const { esc } = wbApp;

  /**
   * 四个阶段 → 文案 + 配色类名。
   *
   * 配色对标 OmniProxy 的 Tag color（那边是 antd 的 cyan / green / orange /
   * processing），落到本项目的令牌上：
   *   · connecting 蓝（--info，与旧「进行中」徽章同一组色）
   *   · waiting    青（本文件就地定色，理由见 CSS）
   *   · streaming  绿（--ok）
   *   · retrying   琥珀（--warn）
   * 类名带 `phase-` 前缀：`.waiting` / `.streaming` 这类裸词太容易与别处的
   * 语义类撞名（`--req-live-dot` 的动画名就踩过同类坑，见 page-requests.css）。
   */
  const PHASES = {
    connecting: { label: '连接中', cls: 'phase-connecting' },
    waiting: { label: '等待响应', cls: 'phase-waiting' },
    streaming: { label: '响应中', cls: 'phase-streaming' },
    retrying: { label: '重试中', cls: 'phase-retrying' },
  };

  /** 阶段缺失（旧行 / 更早版本写入的在途行）时的回落文案 */
  const FALLBACK_LABEL = '进行中';
  const FALLBACK_CLS = 'running';

  /** 每个阶段的悬停说明：徽章只有四个字，落点要说清「这一步在干什么」 */
  const TITLES = {
    connecting: '请求已受理，正在选路、取凭证、建立上游连接',
    waiting: '上游请求已发出，正在等第一个字节到达（模型的思考时间也在这段）',
    streaming: '首帧已到，上游内容正在下发',
    retrying: '本轮尝试失败，正在退避等待或切换到下一个账号',
    '': '请求正在转发中，用时列显示的是已用时',
  };

  /** 行对象里那个阶段字面量，非法值一律按「没有阶段」处理（不猜） */
  function phaseOf(entry) {
    const raw = String(entry?.phase ?? '').trim().toLowerCase();
    return Object.prototype.hasOwnProperty.call(PHASES, raw) ? raw : '';
  }

  /** 阶段文案（空串阶段 → 「进行中」） */
  function labelOf(entry) {
    const phase = phaseOf(entry);
    return phase ? PHASES[phase].label : FALLBACK_LABEL;
  }

  /**
   * 阶段耗时 → 展示文案。
   *
   * 格式与「用时」列的 `formatDuration` 同族（N秒 / N分M秒，中文单位），
   * 两列竖着排在一起时单位口径一致；不足 1 秒按 1 秒算 —— 「0秒」读起来像
   * 没动，而这一行恰恰是「它在动」的信号（与列表「已用时」的取整同一口径）。
   *
   * 拿不到读数（null / undefined / 空串，即不在途或服务端没给）返回空串，
   * 调用方据此整行省掉。
   *
   * ── 为什么必须先排除空值再转数字 ────────────────────────────
   * `Number(null)` **是 0**（不是 NaN），`Number(undefined)` 才是 NaN ——
   * 只判 `Number.isFinite` 会把「没有这个读数」的 null 当成「0 毫秒」，
   * 于是 `max(1, …)` 把它渲染成「1秒」：一个凭空编出来的秒数出现在
   * 根本没有阶段数据的行上。空串同理（`Number('')` 也是 0）。
   * OmniProxy 那边没有这个坑，因为它的格式化函数只接受 `number | null`
   * 且调用点已经先判过了；这里把判断收进函数本身，调用方不必重复。
   */
  function elapsedText(ms) {
    if (ms === null || ms === undefined || ms === '') return '';
    const value = Number(ms);
    if (!Number.isFinite(value) || value < 0) return '';
    const seconds = Math.max(1, Math.floor(value / 1000));
    if (seconds < 60) return `${seconds}秒`;
    return `${Math.floor(seconds / 60)}分${seconds % 60}秒`;
  }

  /**
   * 当前的阶段计时读数（毫秒），取不到给 null。
   *
   * 优先用服务端算好的 `phaseElapsedMs`（口径见后端 `report::entry_json`：
   * 与库里的阶段起点同一个时钟，浏览器时钟偏了也不会算出离谱的读数）；
   * 老响应没带这个字段时，用 `phaseStartedAt` 在本地减一次兜底 ——
   * 缺了它就整行不显示，比显示一个错误的秒数好，但本地减是同一份事实的
   * 另一种算法，能算就算。
   *
   * ── 为什么先判 null 再转数字 ────────────────────────────────
   * 不在途的行这两列都是「没有值」：`phaseElapsedMs` 是 JSON null、
   * `phaseStartedAt` 是 null。而 `Number(null)` **是 0**（不是 NaN），
   * 直接转换会把「没有这个读数」读成「0 毫秒」——那一行（旧版本写入的
   * 在途行、阶段列为空）就会显示成「进行中 1秒」，一个凭空编出来的秒数。
   * 所以先排除空值，再交给 Number 处理。
   */
  function elapsedMsOf(entry) {
    const raw = entry?.phaseElapsedMs;
    if (raw !== null && raw !== undefined && raw !== '') {
      const server = Number(raw);
      if (Number.isFinite(server) && server >= 0) return server;
    }
    const started = Number(entry?.phaseStartedAt);
    if (!Number.isFinite(started) || started <= 0) return null;
    const local = Date.now() - started;
    return Number.isFinite(local) ? Math.max(0, local) : null;
  }

  /**
   * 阶段徽章（呼吸点 + 文案）。**只返回徽章本身**，不含外层列容器 ——
   * 列表那格是 `.req-status`（网格项），详情弹窗那格是表格单元，
   * 两边的容器不同，共用的是这一枚标签。
   */
  function badgeHtml(entry) {
    const phase = phaseOf(entry);
    const label = phase ? PHASES[phase].label : FALLBACK_LABEL;
    const cls = phase ? PHASES[phase].cls : FALLBACK_CLS;
    const title = TITLES[phase] || TITLES[''];
    return `<span class="badge tag ${cls}" title="${esc(title)}">`
      + `<span class="req-live-dot" aria-hidden="true"></span>${esc(label)}</span>`;
  }

  /**
   * 阶段计时的第二行（列表状态列与详情弹窗共用）。**有值才渲染** ——
   * 整行省掉而不是写占位：没有读数的成因是「还没测到」（刚插入的行、
   * 老响应），写「-」会被读成「这一段没花时间」。
   */
  function elapsedLineHtml(entry) {
    const text = elapsedText(elapsedMsOf(entry));
    if (!text) return '';
    const label = labelOf(entry);
    return `<span class="req-phase-elapsed" title="${esc(`进入「${label}」阶段已持续 ${text}`)}">`
      + `${esc(text)}</span>`;
  }

  window.wbRequestPhase = {
    labelOf,
    badgeHtml,
    elapsedLineHtml,
    elapsedText,
    /** 阶段字面量（调用方一般不用，个别地方要按阶段分支出文案时用得上） */
    phaseOf,
  };
})();
