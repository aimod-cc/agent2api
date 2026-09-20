/* Agent2API · 报表页（时间范围 / 统计概览 / Top 提供商 / 热力图 / 缓存命中率 / 两个趋势图） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的报表面板模块：自持时间范围、自持最近一次的后端数据。
 *
 * 与 logs-panel.js / settings-panel.js 同构：依赖 window.wbApp 的 esc / currentPage，
 * 通过 window.wbReport 暴露 load 给 app.js（切到本页时立即刷新）。
 *
 * 三张图都是手写 SVG（项目没有图表库，也不为一个页面引依赖）：
 *   · 折线 / 柱状图按**像素坐标**画：先量容器宽度再算坐标，文字与刻度线因此
 *     不会被拉变形。改窗口时由 ResizeObserver 重算（见「尺寸自适应」一节）。
 *   · 热力图格子边长固定、只让列数随宽度自适应，观感与 GitHub 贡献图一致。
 *
 * 五块内容共用一次 /api/stats/summary 请求：它们本来就是同一次聚合的产物，
 * 拆成五个请求只会让「切范围」变成五次往返，还会出现各板块版本不一致的瞬间。
 */

(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc } = wbApp;

  // ─── 常量 ──────────────────────────────────

  /** 合法的时间范围：与后端 /api/stats/summary 的白名单逐字一致（非法值后端返回 400） */
  const RANGES = ['today', '7', '30', 'month', 'all'];
  const DEFAULT_RANGE = '7';
  /** 持久化键：沿用项目既有的 workbuddy-desktop-* 前缀（主题 / 页码 / 日志开关是同一套） */
  const RANGE_KEY = 'workbuddy-desktop-report-range';
  const RANGE_LABEL = { today: '今天', 7: '近 7 天', 30: '近 30 天', month: '本月', all: '全部' };

  /** 热力图与折线图共用的星期行标签：只标周一 / 三 / 五，七行全写会糊成一片 */
  const WEEKDAY_ROWS = [[0, '一'], [2, '三'], [4, '五']];

  /**
   * 超过这个条数就不再给热区挂 data-tip，改用 SVG 自带的 <title>。
   *
   * 这道闸是必要的：data-tip 由 tooltip.js 自动增强，每个元素都会拿到
   * 六个事件监听 + 一个专属 ResizeObserver。固定开销已经不小 ——
   * 热力图 365 格 + 折线 24 格 —— 柱状图再叠几百上千个就会明显拖慢切页。
   * 阈值 400：加起来仍在一千以内；而 backend 的按天序列理论上能到 4000 天
   * （手改保留期才会出现），那种极端区间必须挡在外面。
   */
  const TIP_LIMIT = 400;

  // ─── 状态 ──────────────────────────────────

  /**
   * 自动刷新间隔（毫秒），由「定时任务」页配置
   * （`scheduledTasks.reportAutoRefresh`）。
   *
   * 与日志页 / 请求日志页同构：定时器长在本页，只在**本页可见时**才请求
   * （`document.hidden` 与 `wbApp.currentPage` 都判），离开页面完全静默。
   * 「定时任务」页改完会调 `applyAutoRefresh` 把新值推过来，本页启动时也自己
   * 拉一次（见 `syncAutoRefresh`），于是无论用户先开哪一页都对得上。
   *
   * 兜底值 1 秒 = 后端的默认间隔（`DEFAULT_REPORT_AUTO_REFRESH_SECONDS`）。
   */
  const DEFAULT_AUTO_REFRESH_MS = 1_000;
  let autoRefreshMs = DEFAULT_AUTO_REFRESH_MS;
  /** 任务关闭时置 false：定时器不跑（区别于「间隔很大」） */
  let autoEnabled = true;
  /** 是否已经从后端读到过间隔配置（自读只做一次；推送来的值也算同步过） */
  let autoSynced = false;
  /** 页面同步定时器 */
  let timer = null;
  /**
   * 是否有一次轮询触发的拉取还在途中。
   *
   * 定时器是 `setInterval`（不等上一次完成），而间隔可以是 1 秒 ——
   * 一次慢响应就会与后来的几拍叠在一起。`load` 本身按序号只认最新结果
   * （不会显示乱序数据），但**每次都会打一次接口**，白白压着后端；
   * 所以轮询这一拍撞上在途请求时直接跳过，由下一拍补上
   * （与 logs-panel / requests-panel 的 `polling` 同一处理）。
   */
  let polling = false;

  /** 最近一次成功拿到的 summary：容器尺寸变化时用它原地重绘，不再打请求 */
  let summary = null;
  /** 各板块上次渲染出的 HTML 指纹：内容没变就整块不重绘。
   *  直接比 HTML 串而不是自己算 hash —— 重建 365 个格子既费 DOM 又会把
   *  用户正悬停的 tooltip、已增强的节点全部作废，能跳过就跳过。 */
  const printed = {};
  /** 只留一组观察器引用，避免被垃圾回收（观察器被回收 = 尺寸自适应失效） */
  const watchers = [];
  /** 请求序号：并发时只认最新一次的结果（见 load 的说明） */
  let seq = 0;

  /** 当前范围以内存为准，localStorage 只负责跨次启动恢复 */
  let range = readRange();

  // ─── 工具 ──────────────────────────────────

  /** 只有明确存过合法值才采纳；无值 / 读取抛错 / 值被改坏都回落默认的 7 天 */
  function readRange() {
    try {
      const saved = localStorage.getItem(RANGE_KEY);
      return RANGES.includes(saved) ? saved : DEFAULT_RANGE;
    } catch {
      return DEFAULT_RANGE;
    }
  }

  /** `YYYY-MM-DD` → 本地零点。不用 new Date('2026-09-17')：那种写法按 UTC 解析，
   *  在东八区会得到前一天 08:00，星期几就跟着错一天。 */
  function parseDay(key) {
    const [y, m, d] = String(key || '').split('-').map(Number);
    return new Date(y || 1970, (m || 1) - 1, d || 1);
  }

  /** 日期写成人话：9月17日 周三（tooltip 用） */
  function dayLabel(key) {
    const date = parseDay(key);
    return `${date.getMonth() + 1}月${date.getDate()}日 周${'日一二三四五六'[date.getDay()]}`;
  }

  /** 本地整点键 `YYYY-MM-DDTHH` → `15:00`（只取时钟位，时区口径由后端定死） */
  function hourText(key) {
    const time = String(key || '').slice(11, 13);
    return time ? `${time}:00` : '—';
  }

  /** 计数：千分位。请求数天然是整数，缩写反而看不出量级差 */
  function formatInt(value) {
    return (Number(value) || 0).toLocaleString('zh-CN');
  }

  /**
   * Token 读数与纵轴刻度都交给 `units.js`：
   * 中文量级（亿 / 万）与英文缩写（M / k）的差别、以及设置页那个开关的读写
   * 全在那边一处，这里只管「用哪个函数画在哪」。
   * 本模块另外十来处读数（概览、tooltip、柱顶标注）也都走它 —— 口径一致，
   * 用户拨一下开关就是整页一起变。
   */
  const units = window.wbUnits || {};
  const formatTokens = value => (units.formatTokens ? units.formatTokens(value) : String(Number(value) || 0));
  const axisText = (value, max) => (units.formatAxis ? units.formatAxis(value, max) : String(value));

  /** 命中率保留一位小数：整数百分比看不出 87.5% 与 87.9% 的差别 */
  function formatPercent(rate) {
    return `${((Number(rate) || 0) * 100).toFixed(1)}%`;
  }

  /** 坐标保留一位小数即可，串更短、指纹比对也更稳 */
  const round1 = value => Math.round(value * 10) / 10;

  /**
   * 每格步长取「好读」的档位（1 / 1.5 / 2 / 2.5 / 3 / 4 / 5 / 6 / 8 × 10^n）。
   *
   * ── 为什么不是先定上限再四等分 ───────────────────────────────
   * 对峰值直接取 niceCeil（1/2/5×10^n）再四等分，刻度常落在「1.25亿」这类
   * 值上（2.16亿的峰值 → 上限 5亿 → 每格 1.25亿）。刻度是拿来读的，遇到
   * 1.25 亿还得在心里换算一遍才敢用。改成先定每格再乘 4 段：峰值 2.16亿
   * 时每格 6000万、上限 2.4亿，五个刻度就是 0 / 6000万 / 1.2亿 / 1.8亿 / 2.4亿，
   * 每一个都是能一眼读出来的数。
   *
   * 档位里带上 1.5 / 2.5 / 3 / 6 / 8 而不是只留 1/2/5：只留三档时，
   * 峰值稍高于 2×10^n 就会跳到 5×10^n，柱子高度从 90% 掉到 40%，
   * 白白浪费半张图。档位密一点，柱子始终撑得住画布。
   */
  const STEP_LADDER = [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10];

  function niceStep(value) {
    if (!(value > 0)) return 1;
    const pow = 10 ** Math.floor(Math.log10(value));
    const norm = value / pow;
    const step = STEP_LADDER.find(item => item >= norm - 1e-9) ?? 10;
    return step * pow;
  }

  /** 纵轴上限 = 好读的每格步长 × 4 段（刻度线固定 5 条，见各图表的网格循环） */
  function axisMax(peak) {
    return niceStep(peak / 4) * 4;
  }

  /** 量容器宽度；页面还没显示时是 0，用一个兜底宽度先画出来，
   *  等 ResizeObserver 拿到真实宽度会强制重绘（见「尺寸自适应」） */
  function widthOf(box) {
    return box?.clientWidth || 680;
  }

  // ─── 渲染骨架 ──────────────────────────────

  /**
   * 写入某个板块。内容与上次一模一样时整块跳过 —— 这是拖窗口时最重要的优化：
   * 位置没真的变就一个节点都不重建，用户正悬停的气泡也不会被作废。
   */
  function paint(id, html) {
    const box = $(id);
    if (!box) return;
    if (printed[id] === html) return;
    printed[id] = html;
    box.innerHTML = html;
  }

  /** 统一的空态 / 错误态文案：与组件层 .empty 同档留白 */
  const placeholder = (text, className = 'empty') =>
    `<div class="${className}" style="padding:14px 0">${esc(text)}</div>`;

  // ─── 板块一：统计概览 ──────────────────────

  function overviewHtml(overview, range, days) {
    const data = overview || {};
    const top = data.topModel;
    const requests = Number(data.requests) || 0;
    const successful = Number(data.successful) || 0;
    // 请求数为 0 时成功率没有意义（0/0），给破折号而不是 0%
    const successRate = requests ? `${((successful / requests) * 100).toFixed(1)}%` : '—';

    const cells = [
      ['总请求数', formatInt(requests), RANGE_LABEL[range] || ''],
      ['成功请求数', formatInt(successful), `成功率 ${successRate}`],
      ['总 Token', formatTokens(data.tokens), '输入 + 输出'],
      ['活跃天数', formatInt(data.activeDays), `区间共 ${days} 天`],
      ['当前连续天数', formatInt(data.streak), '含今天在内往前数'],
      top
        ? ['Top 模型', esc(top.model), `${formatTokens(top.tokens)} tokens · 占 ${formatPercent(top.percentage)}`, 'mono']
        : ['Top 模型', '—', '所选范围内还没有模型用量'],
    ];

    return cells.map(([label, value, sub, extra]) => `<div class="field">
        <div class="label">${esc(label)}</div>
        <div class="value ${extra || ''}">${value}${sub ? `<div class="sub">${esc(sub)}</div>` : ''}</div>
      </div>`).join('');
  }

  // ─── 板块二：Top 提供商 / Top 账号排行 ──────
  //
  // 两张卡片（并排）共用同一套渲染：都是「一行一个主体 + 占比条 + 请求数 + 百分比」，
  // 差别只在数据来源、名字怎么取、以及「其它」那一行怎么称呼。抽成一个函数传配置，
  // 而不是写两份 —— 两份的列宽、tooltip 格式、排序口径迟早会漂，而这类漂移
  // 不会报错，只会让两张并排的卡片看起来像两套设计。

  /**
   * 数据来自 summary.providers / summary.accounts
   * （`[{id,label,requests,success,totalTokens}]`，按请求数降序）。
   *
   * 整块的可见性由 paintRank 决定：**契约里没有这个字段就整块隐藏**
   * （旧后端 / 该维度还没接上），与「字段在但为空数组」不同 —— 后者是「这段时间
   * 一条明细都没记下这一维」（例如全部请求都没走到转发），此时给一句空态即可。
   * 两者混在一起会让「版本落后」看起来像「今天没用量」。
   *
   * 展示上只取前 5 项（超过就并进「其它」）：这块是概览不是明细表，项数一多
   * 每行的条就细到看不出差别；完整数据本来也能在模型请求里按列核对。
   */
  const RANK_TOP = 5;

  /** 两张卡片的配置（渲染逻辑共用，差异全在这里） */
  const RANK_CARDS = {
    providers: {
      panelId: 'report-providers-panel',
      listId: 'report-providers',
      labelId: 'report-providers-label',
      // 「其它 N 家」：provider 的量词是「家」
      restLabel: count => `其它 ${count} 家`,
      emptyText: '所选范围内还没有请求记录',
      // provider 的 id 是注册表里的短标识（workbuddy / qoder），tooltip 里值得带上
      tipName: item => (item.id ? `${item.label}（${item.id}）` : item.label),
    },
    accounts: {
      panelId: 'report-accounts-panel',
      listId: 'report-accounts',
      labelId: 'report-accounts-label',
      // 「其它 N 个」：账号的量词是「个」，与账号页的称呼一致
      restLabel: count => `其它 ${count} 个`,
      emptyText: '所选范围内还没有账号用量',
      // 账号名可能重复（两家都能叫「默认」），id 才是身份 —— tooltip 里带上它
      tipName: item => (item.id ? `${item.label}（${item.id}）` : item.label),
    },
  };

  /**
   * 一组排行行 → HTML。
   *
   * 两条 `RANK_CARDS` 的 label 语义不同（provider 的由后端注册表现算，
   * 账号的是聚合时留下的名字快照），但都已经是可直接显示的字符串，
   * 所以这里按同一形态消费，不必分叉。
   */
  function rankHtml(list, config) {
    const rows = (Array.isArray(list) ? list : [])
      .map(item => ({
        id: String(item?.id ?? '').trim(),
        // label 缺省用 id 兜底；两者都空的那一组是后端的「未知」
        label: String(item?.label ?? '').trim(),
        requests: Number(item?.requests) || 0,
        success: Number(item?.success) || 0,
        tokens: Number(item?.totalTokens ?? item?.tokens) || 0,
      }))
      .filter(item => item.requests > 0 || item.tokens > 0);

    if (!rows.length) return placeholder(config.emptyText);

    const total = rows.reduce((sum, item) => sum + item.requests, 0);
    // 总量为 0（只有 token 没有请求，理论上不该出现）时不给百分比：0/0 没有意义
    const share = count => (total ? (count / total) * 100 : 0);

    // 按请求数排序后截断，溢出的合并成「其它」一行 —— 合并后的条仍然按真实
    // 总量画，所以所有条加起来始终是 100%，不会因为截断而看起来缺一截
    const sorted = [...rows].sort((left, right) => right.requests - left.requests);
    const head = sorted.slice(0, RANK_TOP);
    const rest = sorted.slice(RANK_TOP);
    if (rest.length) {
      head.push({
        id: '',
        label: config.restLabel(rest.length),
        requests: rest.reduce((sum, item) => sum + item.requests, 0),
        success: rest.reduce((sum, item) => sum + item.success, 0),
        tokens: rest.reduce((sum, item) => sum + item.tokens, 0),
      });
    }

    return head.map(item => {
      const percent = share(item.requests);
      // 成功率只在有请求时给（0/0 没有意义），与概览里的口径一致
      const rate = item.requests ? `${((item.success / item.requests) * 100).toFixed(1)}%` : '—';
      const name = item.label || item.id || '未知';
      // 「名字（id）」这一形态有两处用途：整行的气泡说明，以及名字列的原生 title
      // （名字列可能被省略号截断，原生 title 是最直接的补救）。两者同源，
      // 名字列的显示与身份标识不会各说各话。
      const full = config.tipName({ ...item, label: name });
      // 条宽用百分比：容器宽度变化时条跟着伸缩，不必像 SVG 那样量宽度重绘
      const tip = `${full}：${formatInt(item.requests)} 次请求 · 占 ${percent.toFixed(1)}%`
        + ` · 成功率 ${rate} · ${formatTokens(item.tokens)} tokens`;
      return `<div class="rank-row" data-tip="${esc(tip)}">
          <span class="name" title="${esc(full)}">${esc(name)}</span>
          <span class="track"><span class="bar" style="width:${percent.toFixed(1)}%"></span></span>
          <span class="num">${formatInt(item.requests)}</span>
          <span class="pct">${percent.toFixed(1)}%</span>
        </div>`;
    }).join('');
  }

  /**
   * 写入一张排行卡片，并决定整块的显隐。
   *
   * 字段缺失（`undefined`）→ 整块隐藏：这是「这份后端还没有这一维」，
   * 不是「这一维没数据」。面板与页头的小标题一起藏，避免页面上留一个空卡片。
   */
  function paintRank(key, list) {
    const config = RANK_CARDS[key];
    if (!config) return;
    const panel = $(config.panelId);
    if (!panel) return;
    const has = Array.isArray(list);
    panel.hidden = !has;
    if (!has) return;
    paint(config.listId, rankHtml(list, config));
    paint(config.labelId, esc(`${formatInt(
      list.reduce((sum, item) => sum + (Number(item?.requests) || 0), 0))} 次请求`));
  }

  // ─── 板块三：热力图 ────────────────────────

  /**
   * 热力图分档：0 / 1-2 / 3-5 / 6-10 / 10+ 五档。
   *
   * 用**固定阈值**而不是分位数：本工具的日请求量普遍是个位数，分位数会把
   * 「那天只有 1 次请求」也涂成最深一档，颜色就不再表示「多少」，
   * 只剩「相对排名」，反而看不出使用强度。
   *
   * 阈值只在这里定义一次：格子着色（levelOf）与图例上的说明文字都读它，
   * 改一个数两处一起变 —— 否则会出现「图例写着 ≤5，实际 ≤7 才不变深」这种
   * 谁也发现不了的漂移。
   */
  const HEAT_LEVELS = [0, 2, 5, 10, Infinity];

  /** 每一档的说明文字（图例用） */
  function heatLevelText(level) {
    if (level === 0) return '0';
    const lower = HEAT_LEVELS[level - 1];
    const upper = HEAT_LEVELS[level];
    if (!Number.isFinite(upper)) return `>${lower}`;
    // 上一档的上限 +1 才是本档起点（档与档之间是闭区间，不能重叠）
    return lower === 0 ? `≤${upper}` : `${lower + 1}–${upper}`;
  }

  /**
   * GitHub 贡献图布局：53 列（周）× 7 行（星期）。
   * 首列用空格补齐到周一、末列不满也留空，与 GitHub 观感一致。
   *
   * ── 为什么要横向铺满容器 ─────────────────────────────────────
   * 一年固定 53 列，格子边长若按「下限 7px、上限 13px」夹取，在常见窗口宽度
   * （内容区 1000px 上下）里算出来会停在下限附近，整块图只占容器三分之二宽、
   * 右边空一大截，看着像一个没对齐的残缺块。这里改成**按容器宽度反推格子边长**
   * （夹取区间放宽到 7–26px）：算出来的边长让 53 列恰好填满可用宽度，
   * 于是图形左右两边与面板边框对齐。
   *
   * 夹取区间仍然必要：窗口极窄时不能让格子小到看不清（此时交给 .heat-wrap
   * 横向滚动），窗口极宽时也不能让格子大到一格占满屏幕（此时整块居中留白）。
   */
  function heatmapHtml(days) {
    const list = Array.isArray(days) ? days : [];
    if (!list.length) return placeholder('暂无热力图数据');

    const pad = (parseDay(list[0].date).getDay() + 6) % 7;   // 周一为 0：一周从周一起算
    const cells = new Array(pad).fill(null).concat(list);
    const cols = Math.ceil(cells.length / 7);

    const gap = 3;
    const labelW = 20;    // 左侧星期标签
    // 顶部要给月份标签留位置；格子边长变大时标签也跟着长，所以这里按算出来的
    // 边长放大留白（否则宽窗口下月份标签会贴到第一行格子上）
    const boxWidth = widthOf($('report-heatmap'));
    // 反推：53 列连同列间隙要刚好填满可用宽度（见函数头「为什么要横向铺满」）。
    // 上限取 26px 而不是原来的 13px —— 13px 时 53 列只占住 700 多像素，
    // 在常见窗口里永远填不满，那正是「偏左留白」的来源。26px 是「再大就不像
    // 贡献图了」的观感上限，只有超宽窗口才会碰到它。
    // 可用宽度可能为 0（页面还没显示），此时 widthOf 的兜底值会给出一个正常尺寸
    const available = boxWidth - labelW;
    const fit = Math.floor((available - gap * (cols - 1)) / cols);
    const size = Math.max(7, Math.min(26, fit));
    const topH = size + 3;
    // 夹取后（窗口极宽 / 极窄）图形不再等于容器宽度，改为居中：两边留白对称，
    // 不会像现在这样只往左边挤
    const step = size + gap;
    const gridW = labelW + cols * step - gap;
    const offsetX = Math.max(0, Math.round((boxWidth - gridW) / 2));
    const height = topH + 7 * step - gap + 2;
    const svgW = Math.max(gridW, Math.round(boxWidth));

    const levelOf = requests => HEAT_LEVELS.findIndex(limit => Number(requests) <= limit);

    let rects = '';
    for (let col = 0; col < cols; col += 1) {
      for (let row = 0; row < 7; row += 1) {
        const day = cells[col * 7 + row];
        if (!day) continue;
        const requests = Number(day.requests) || 0;
        const tip = requests
          ? `${dayLabel(day.date)} · ${formatInt(requests)} 次请求 · ${formatTokens(day.tokens)} tokens`
          : `${dayLabel(day.date)} · 无请求`;
        // 预置 tabindex="-1"：tooltip.js 只在元素**不**匹配 [tabindex] 时才补 tabIndex=0，
        // 这样 365 个格子不进 Tab 序列（否则键盘用户要按几百次 Tab 才能走到下一个控件），
        // 同时仍然享受 data-tip 的气泡。
        rects += `<rect class="hm-cell l${levelOf(requests)}" x="${round1(offsetX + labelW + col * step)}"`
          + ` y="${round1(topH + row * step)}" width="${size}" height="${size}" rx="2"`
          + ` tabindex="-1" data-tip="${esc(tip)}"></rect>`;
      }
    }

    const weekday = WEEKDAY_ROWS.map(([row, text]) =>
      `<text class="hm-axis" x="${offsetX + labelW - 6}" y="${round1(topH + row * step + size / 2)}"`
      + ` text-anchor="end" dominant-baseline="middle">${text}</text>`).join('');

    // 月份标签落在「该列最早一天所属月份」与上一列不同的那一列，
    // 与 GitHub 同一手法；两列挨太近（<3 列）时这次不写，但仍记下月份，避免标签叠字
    let months = '';
    let lastMonth = -1;
    let lastLabelCol = -9;
    for (let col = 0; col < cols; col += 1) {
      const head = cells.slice(col * 7, col * 7 + 7).find(Boolean);
      if (!head) continue;
      const month = parseDay(head.date).getMonth();
      if (month === lastMonth) continue;
      lastMonth = month;
      if (col - lastLabelCol < 3) continue;
      lastLabelCol = col;
      months += `<text class="hm-axis" x="${round1(offsetX + labelW + col * step)}" y="10">${month + 1}月</text>`;
    }

    return `<svg class="report-svg" width="${svgW}" height="${height}"`
      + ` viewBox="0 0 ${svgW} ${height}" role="img" aria-label="近 365 天活跃热力图">`
      + `${months}${weekday}${rects}</svg>`;
  }

  /**
   * 图例：五档色块 + 每档的请求数范围，与格子共用 --hm-* 变量与 HEAT_LEVELS
   * 阈值，色值与档位都只定义一处。
   *
   * 档位说明是必要的：本工具多数日子的请求数落在 0–5 之间，只给一个渐变色阶
   * 看不出「这几格的颜色差一档到底差多少请求」，而它是这张图的全部信息量。
   */
  function heatLegendHtml() {
    return [0, 1, 2, 3, 4].map(level =>
      `<span class="heat-legend-item"><span class="l${level}"></span>${esc(heatLevelText(level))}</span>`).join('');
  }

  // ─── 板块三：缓存命中率四窗口 ──────────────

  function cacheRatesHtml(rates) {
    const data = rates || {};
    const windows = [
      ['last10m', '近 10 分钟'],
      ['last1h', '近 1 小时'],
      ['last24h', '近 24 小时'],
      ['last7d', '近 7 天'],
    ];
    return windows.map(([key, label]) => {
      const item = data[key] || {};
      const input = Number(item.inputTokens) || 0;
      return `<div class="field rate">
          <div class="label">${label}</div>
          <div class="value big">${input ? formatPercent(item.rate) : '—'}
            <div class="sub">命中 ${formatTokens(item.hitTokens)} / 输入 ${formatTokens(input)}</div>
          </div>
        </div>`;
    }).join('');
  }

  // ─── 板块四：近 24 小时命中率折线 ──────────

  /**
   * 双轴折线：命中率（左轴，蓝）与总 Token（右轴，琥珀）。
   *
   * ── 为什么两条线画在一张图里 ─────────────────────────────────
   * 命中率与用量是本工具最需要对照着看的一对：用量突然拔高时命中率有没有塌，
   * 是判断「钱花得冤不冤」最直接的一眼。分两张图就得来回扫两遍横轴。
   * 两者的量纲差着好几个数量级（0–100% vs 上亿 Token），所以各自一条纵轴 ——
   * 共用一条轴时要么 Token 线压平在底部、要么命中率线贴着顶边。
   *
   * ── 读数为什么直接写在点上 ───────────────────────────────────
   * 悬停气泡只能读一个点，而这张图的常态用法是「扫一眼看出哪几个小时爆了量」。
   * 命中率与 Token 各用自己那条线的颜色标在点的上下两侧；无请求的整点
   * （补零出来的 0%）不标注，否则会连成一排毫无信息量的「0%」。
   *
   * 标注会互相避让：放不下就整条跳过（宁可少标一个，也不让两串数字叠成一团）。
   * 跳过的点仍能用悬停气泡读到完整数值，所以信息没有损失。
   */
  function cacheTrendHtml(series) {
    const list = Array.isArray(series) ? series : [];
    if (!list.length) return placeholder('暂无缓存趋势数据');

    const width = widthOf($('report-cache-trend'));
    // 比单轴版高一档：点的上方要放 Token 标注、下方要放命中率标注
    const height = 230;
    const pad = { left: 44, right: 56, top: 26, bottom: 24 };
    const plotW = Math.max(40, width - pad.left - pad.right);
    const plotH = height - pad.top - pad.bottom;

    const rates = list.map(item => Number(item.rate) || 0);
    const tokens = list.map(item => Number(item.totalTokens) || 0);
    // 命中率的纵轴上限按真实峰值抬：上游把缓存读取与输入分开上报时命中率会
    // 合法地超过 100%，夹到 100% 等于谎报数据，抬上限只是让曲线留在画布里。
    // 下限仍是 1（=100%）：峰值不高时也给它一条完整的百分比轴，
    // 否则「今天命中率普遍 60%」会被画成顶满，看着像 100%
    const rateMax = Math.max(1, axisMax(Math.max(...rates)));
    // 用量轴同样按峰值定上限；整段区间一条用量都没记（老后端没给 totalTokens、
    // 或这些请求全失败被清零）时不画这条线也不画右侧刻度 —— 画一条贴地的直线
    // 会让人以为「用量就是 0」，而事实是「这份数据里没有」
    const tokenPeak = Math.max(...tokens);
    const tokenMax = axisMax(tokenPeak);
    const hasTokens = tokenPeak > 0;

    const xAt = index => pad.left + (list.length > 1 ? (plotW * index) / (list.length - 1) : plotW / 2);
    const yRate = rate => pad.top + plotH - (plotH * rate) / rateMax;
    const yTokens = value => pad.top + plotH - (plotH * value) / tokenMax;

    // ── 网格与两侧刻度 ──
    // 网格线跟着命中率的 5 等分走（左轴是主读数），右侧用量刻度贴在同一批
    // 横线上：两条轴都被等分 4 段，所以同一根线在两边各有各的读数
    let grid = '';
    for (let i = 0; i <= 4; i += 1) {
      const value = (rateMax * i) / 4;
      const y = round1(yRate(value));
      const pct = value * 100;
      grid += `<line class="chart-grid" x1="${pad.left}" y1="${y}" x2="${round1(pad.left + plotW)}" y2="${y}"></line>`
        + `<text class="chart-axis" x="${pad.left - 8}" y="${y}" text-anchor="end" dominant-baseline="middle">`
        + `${Number.isInteger(pct) ? pct : pct.toFixed(1)}%</text>`;
      if (hasTokens) {
        grid += `<text class="chart-axis" x="${round1(pad.left + plotW + 8)}" y="${y}"`
          + ` text-anchor="start" dominant-baseline="middle">`
          + `${esc(axisText((tokenMax * i) / 4, tokenMax))}</text>`;
      }
    }

    // ── 折线 ──
    // 点也要画，而且单点时要画得更醒目：polyline 只有一个点时连不出线段
    // （SVG 折线至少要两个点才有笔画），此时「整张图」就剩这一个圆点，
    // 半径还按常态的 2.5 会小到像渲染失败。
    // 线色由 cls 对应的 CSS 规则决定（.chart-line.rate / .tokens 与
    // .chart-dot.rate / .tokens），两条线的颜色因此只在 CSS 里各定义一次。
    const dotR = list.length === 1 ? 4.5 : 2.5;
    const lineOf = (values, yOf, cls) =>
      `<polyline class="chart-line ${cls}" points="${values.map((value, index) => `${round1(xAt(index))},${round1(yOf(value))}`).join(' ')}"></polyline>`
      + values.map((value, index) =>
        `<circle class="chart-dot ${cls}" cx="${round1(xAt(index))}" cy="${round1(yOf(value))}" r="${dotR}"></circle>`).join('');

    // ── 点上的读数标注 ──
    // 宽度按字宽粗估（中日韩字符占满格、数字与 % 只有半格）：这是给避让用的
    // 近似值，不必精确 —— 差几像素只会让一两处标注多留一点余量。
    const textW = text => [...text].reduce((sum, ch) => sum + (/[\u2e80-\u9fff]/.test(ch) ? 9.7 : 5.4), 0);
    const boxes = [];
    const label = (x, y, text, cls, anchor = 'middle') => {
      const w = textW(text);
      const left = anchor === 'middle' ? x - w / 2 : anchor === 'end' ? x - w : x;
      const box = { left, right: left + w, top: y - 8.5, bottom: y + 2.5 };
      // 留 2px 横向、1px 纵向的呼吸：贴着不重叠也算「挤在一起」，一样难认
      if (boxes.some(item => box.left < item.right + 2 && box.right > item.left - 2
        && box.top < item.bottom + 1 && box.bottom > item.top - 1)) return '';
      boxes.push(box);
      return `<text class="chart-label ${cls}" x="${round1(x)}" y="${round1(y)}" text-anchor="${anchor}">${esc(text)}</text>`;
    };

    /** 首尾两点的标注贴到画布边缘就会被裁掉一半，这里按「会不会越界」改对齐方式：
     *  居中的串若往左探出绘图区，就改成左对齐；往右探出就改成右对齐。
     *  中间的点永远居中（居中最好读，右边那条轴线也不必跟着它移动）。 */
    const anchorFor = (x, text) => {
      const half = textW(text) / 2;
      if (x - half < pad.left - 6) return 'start';
      if (x + half > pad.left + plotW + 6) return 'end';
      return 'middle';
    };

    // 先标用量（字宽、更易被挤掉），再标命中率（短、多半放得下）——
    // 顺序反过来的话，长串数字会因为先被短标签占位而大面积消失
    let tokenLabels = '';
    if (hasTokens) {
      list.forEach((item, index) => {
        if (!(Number(item.totalTokens) > 0)) return;
        // 点的上方；顶到画布边时翻到点下方（最高那个点的标注只能这么安放）
        const y = yTokens(tokens[index]);
        const above = y - 12 >= pad.top - 10;
        const text = formatTokens(tokens[index]);
        tokenLabels += label(xAt(index), above ? y - 12 : y + 13, text, 'tokens', anchorFor(xAt(index), text));
      });
    }

    let rateLabels = '';
    list.forEach((item, index) => {
      // 无请求的整点不标：它是补零出来的 0%，标出来只会连成一排 0%
      const used = (Number(item.inputTokens) || 0) + (Number(item.hitTokens) || 0) > 0;
      if (!used) return;
      const y = yRate(rates[index]);
      // 点的下方；贴到横轴时翻到点上方，避免和刻度文字叠在一起
      const below = y + 15 <= pad.top + plotH + 8;
      const text = formatPercent(rates[index]);
      rateLabels += label(xAt(index), below ? y + 13 : y - 7, text, 'rate', anchorFor(xAt(index), text));
    });

    // 热区按「整点带宽」铺满，鼠标落在两个点之间也能读到最近的那个点的数值。
    // 左右两端各会超出半个带宽，这里夹回绘图区：越界的那半截会盖住 Y 轴标签，
    // 悬停刻度文字却弹出「某小时的命中率」很突兀。
    // tabindex="-1" 与热力图格子同理：不进 Tab 序列，但仍享受 data-tip 气泡
    // （tooltip.js 只在元素没有任何 tabindex 时才补 tabIndex=0）。
    const band = list.length > 1 ? plotW / (list.length - 1) : plotW;
    const plotLeft = pad.left;
    const plotRight = pad.left + plotW;
    const tips = list.length <= TIP_LIMIT;
    const hits = list.map((item, index) => {
      const tip = `${dayLabel(String(item.hour).slice(0, 10))} ${hourText(item.hour)}`
        + ` · 命中率 ${formatPercent(item.rate)}`
        + ` · 总 ${formatTokens(item.totalTokens)} tokens`
        + `（命中 ${formatTokens(item.hitTokens)} / 输入 ${formatTokens(item.inputTokens)}）`;
      const left = Math.max(plotLeft, xAt(index) - band / 2);
      const right = Math.min(plotRight, xAt(index) + band / 2);
      return tips
        ? `<rect class="chart-hit" x="${round1(left)}" y="${pad.top}" width="${round1(right - left)}"`
          + ` height="${round1(plotH)}" tabindex="-1" data-tip="${esc(tip)}"></rect>`
        : '';
    }).join('');

    // 每 3 小时一个刻度（24 点 → 8 个）；首尾两个用 start/end 对齐，免得溢出画布
    const tickStep = Math.max(1, Math.ceil(list.length / 8));
    let ticks = '';
    for (let i = 0; i < list.length; i += tickStep) {
      const last = i + tickStep >= list.length;
      ticks += `<text class="chart-axis" x="${round1(xAt(i))}" y="${height - 8}"`
        + ` text-anchor="${i === 0 ? 'start' : last ? 'end' : 'middle'}">${hourText(list[i].hour)}</text>`;
    }

    return `<svg class="report-svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"`
      + ` role="img" aria-label="近 24 小时缓存命中率与总 Token 趋势">${grid}`
      + (hasTokens ? lineOf(tokens, yTokens, 'tokens') : '')
      + lineOf(rates, yRate, 'rate')
      + `${tokenLabels}${rateLabels}${ticks}</svg>`
      + (hits ? `<svg class="report-svg chart-hit-layer" width="${width}" height="${height}"`
        + ` viewBox="0 0 ${width} ${height}" aria-hidden="true">${hits}</svg>` : '');
  }

  // ─── 板块五：按天 Token 柱状图 ─────────────

  /**
   * 按天 Token 柱状图。柱顶直接标出当天的用量。
   *
   * 标注的取舍与折线图一致（见 cacheTrendHtml 的说明）：给的是「扫一眼看量级」
   * 的读数，所以不必每根柱子都标 —— 柱子密到标注必然重叠时，只标当区间里
   * 最大的那几根，其余靠悬停气泡读。这样图始终是干净的，而峰值一眼可见。
   */
  function dailyTrendHtml(series) {
    const list = Array.isArray(series) ? series : [];
    if (!list.length) return placeholder('暂无趋势数据');

    const width = widthOf($('report-daily-trend'));
    // 比不带标注的版本高一档：柱顶要留出写读数的位置（见 pad.top）
    const height = 240;
    const pad = { left: 50, right: 14, top: 26, bottom: 26 };
    const plotW = Math.max(40, width - pad.left - pad.right);
    const plotH = height - pad.top - pad.bottom;

    const values = list.map(item => Number(item.tokens) || 0);
    const peak = Math.max(...values);
    const yMax = axisMax(peak);
    const step = plotW / list.length;
    // 极长区间（手改保留期才会出现）下柱子只剩一两像素：此时不再压窄，保证看得见
    const barW = Math.max(1, Math.min(28, step * 0.68));
    const baseY = pad.top + plotH;
    const yAt = value => pad.top + plotH - (plotH * value) / yMax;
    const tips = list.length <= TIP_LIMIT;

    // 全为 0 时柱高为 0，整块图会空得像渲染失败：不画网格（此时五个刻度
    // 会缩成 0/0/0/0/0 这类重复标签），改为给一句明确的说明。
    const blank = peak <= 0;

    let grid = '';
    if (!blank) {
      for (let i = 0; i <= 4; i += 1) {
        const value = (yMax * i) / 4;
        const y = round1(yAt(value));
        // 刻度文案交给 units.js（中文档按各刻度自己的量级，英文档按整条轴统一）
        grid += `<line class="chart-grid" x1="${pad.left}" y1="${y}" x2="${round1(pad.left + plotW)}" y2="${y}"></line>`
          + `<text class="chart-axis" x="${pad.left - 8}" y="${y}" text-anchor="end" dominant-baseline="middle">`
          + `${esc(axisText(value, yMax))}</text>`;
      }
    }

    const bars = list.map((item, index) => {
      const value = Number(item.tokens) || 0;
      if (value <= 0) return '';   // 无数据的日子不画柱子，留空比画一个 0 高的假柱子诚实
      const y = yAt(value);
      const tip = `${dayLabel(item.date)} · ${formatTokens(value)} tokens · ${formatInt(item.requests)} 次请求`;
      const attrs = `x="${round1(pad.left + step * index + (step - barW) / 2)}" y="${round1(y)}"`
        + ` width="${round1(barW)}" height="${round1(Math.max(1, baseY - y))}" rx="${barW > 4 ? 2 : 1}"`;
      // 超长区间不挂 data-tip 时改用 SVG 原生 <title>：提示成本降到零，hover 仍有读数
      return tips
        ? `<rect class="bar" ${attrs}></rect>`
        : `<rect class="bar" ${attrs}><title>${esc(tip)}</title></rect>`;
    }).join('');

    // 柱顶读数：每根柱子都要放得下才逐一标注（区间一长、柱子一密就必然重叠）。
    // 放不下时退成「只标最大的那几根」—— 峰值是最该一眼看到的那个数
    // （哪天的用量最高、比次高的高出多少），而每一根的具体值仍能从气泡读到。
    const labelWidth = text => [...text].reduce(
      (sum, ch) => sum + (/[\u2e80-\u9fff]/.test(ch) ? 9.7 : 5.4), 0);
    const texts = list.map(item => (Number(item.tokens) > 0 ? formatTokens(item.tokens) : ''));
    const widest = Math.max(0, ...texts.map(labelWidth));
    const allFit = texts.every(text => !text) || widest + 4 <= step;   // +4：相邻标注之间留一点缝

    // 退让模式下取用量最高的前几名；并列多少就取多少（上限只是防极端区间
    // 标出一大串，正常区间里这个数远不到）
    const marked = new Set();
    if (!allFit) {
      const ranked = list
        .map((item, index) => ({ index, value: Number(item.tokens) || 0 }))
        .filter(item => item.value > 0)
        .sort((left, right) => right.value - left.value)
        .slice(0, 8);
      ranked.forEach(item => marked.add(item.index));
    }

    let barLabels = '';
    list.forEach((item, index) => {
      if (!texts[index]) return;
      if (!allFit && !marked.has(index)) return;
      const y = yAt(Number(item.tokens) || 0) - 6;
      // 顶到画布边（柱子接近 100%）时翻到柱子内侧，免得文字被裁掉
      const inside = y - 9 < pad.top - 10;
      barLabels += `<text class="chart-label tokens" x="${round1(pad.left + step * index + step / 2)}"`
        + ` y="${round1(inside ? y + 13 : y)}" text-anchor="middle">${esc(texts[index])}</text>`;
    });

    // 热区整列铺满（含零值日），鼠标扫过任何一列都能读到当天读数
    const hits = tips ? list.map((item, index) => {
      const tip = `${dayLabel(item.date)} · ${formatTokens(item.tokens)} tokens · ${formatInt(item.requests)} 次请求`;
      return `<rect class="chart-hit" x="${round1(pad.left + step * index)}" y="${pad.top}"`
        + ` width="${round1(step)}" height="${round1(plotH)}" tabindex="-1" data-tip="${esc(tip)}"></rect>`;
    }).join('') : '';

    // 刻度稀疏：最多 6 个，且末位日期一定标出来 —— 等距取样常常落下「今天」，
    // 而今天恰恰是用户最先看的那一格
    const labelStep = Math.max(1, Math.ceil(list.length / 6));
    const marks = [];
    for (let i = 0; i < list.length; i += labelStep) marks.push(i);
    if (marks[marks.length - 1] !== list.length - 1) marks.push(list.length - 1);
    const ticks = marks.map(index => {
      const only = list.length === 1;
      const last = !only && index === list.length - 1;
      const date = parseDay(list[index].date);
      const x = only ? pad.left + plotW / 2 : last ? pad.left + plotW : round1(pad.left + step * index + step / 2);
      const anchor = only ? 'middle' : last ? 'end' : index === 0 ? 'start' : 'middle';
      return `<text class="chart-axis" x="${x}" y="${height - 8}" text-anchor="${anchor}">`
        + `${date.getMonth() + 1}/${date.getDate()}</text>`;
    }).join('');

    // 全为 0 时柱高为 0，整块图会空得像渲染失败：这里补一句明确说明
    const empty = peak <= 0
      ? `<text class="chart-empty" x="${round1(pad.left + plotW / 2)}" y="${round1(pad.top + plotH / 2)}"`
        + ` text-anchor="middle">所选范围内暂无请求</text>`
      : '';

    return `<svg class="report-svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"`
      + ` role="img" aria-label="按天 Token 趋势">${grid}${bars}${barLabels}${ticks}${empty}</svg>`
      + (hits ? `<svg class="report-svg chart-hit-layer" width="${width}" height="${height}"`
        + ` viewBox="0 0 ${width} ${height}" aria-hidden="true">${hits}</svg>` : '');
  }

  // ─── 统一渲染 ──────────────────────────────

  /** 各板块一起画。宽度变化不必额外传参：三张图的 HTML 里已经写了按宽度算出的
   *  坐标，宽度真变了 HTML 自然不同，paint 的指纹比对就会重绘；
   *  宽度只是被拖动改了一点点、算出来的整数尺寸没变时，它自然什么也不做。 */
  function renderAll() {
    if (!summary) return;
    const trend = Array.isArray(summary.dailyTrend) ? summary.dailyTrend : [];
    const rangeKey = RANGES.includes(summary.range) ? summary.range : range;

    paint('report-overview', overviewHtml(summary.overview, rangeKey, trend.length));
    // 两张排行卡：整块的显隐由 paintRank 自己判断（各自字段缺失就藏起来）
    paintRank('providers', summary.providers);
    paintRank('accounts', summary.accounts);
    paint('report-heatmap', heatmapHtml(summary.heatmap));
    paint('report-cache-rates', cacheRatesHtml(summary.cacheRates));
    paint('report-cache-trend', cacheTrendHtml(summary.cacheTrend24h));
    paint('report-daily-trend', dailyTrendHtml(summary.dailyTrend));
    paint('report-range-label', esc(RANGE_LABEL[rangeKey] || ''));
    paint('report-trend-label', esc(`${summary.startDate || '—'} ～ ${summary.endDate || '—'}`));
  }

  /**
   * 加载失败：各板块各自显示错误态，而不是整页崩掉。
   * 每块都写、而不是只弹一个 toast —— 用户需要知道的是「这些数字现在不可信」。
   * 两张排行卡不在此列：它们本来就要按「有没有这一维数据」决定显隐，
   * 整页读取失败时留着上一轮的条反而会让人以为它还是可信的，所以一并藏起来。
   */
  function renderFailure(message) {
    const html = placeholder(`读取报表失败：${message}`, 'empty report-error');
    ['report-overview', 'report-heatmap', 'report-cache-rates', 'report-cache-trend', 'report-daily-trend']
      .forEach(id => paint(id, html));
    // 两张排行卡一起藏（各自的 panelId 从同一份配置取，不在这里另写一遍 id）
    Object.keys(RANK_CARDS).forEach(key => {
      const panel = $(RANK_CARDS[key].panelId);
      if (panel) panel.hidden = true;
    });
  }

  function render(data) {
    if (data !== undefined) summary = data || null;
    renderAll();
  }

  /**
   * 单位口径变了就原地重绘：手里那份 summary 不用重取（数值一个都没变，
   * 变的只是「怎么写成字」），所以拨一下开关是瞬时的 —— 不必重新拉一次报表，
   * 也不会因为重取期间的延迟让人以为开关没生效。
   */
  window.addEventListener('wb-units-changed', renderAll);

  /** silent：只压掉控制台噪音（首屏自持加载用），错误态照常显示 */
  async function load({ silent = false } = {}) {
    // 兜一次间隔配置：冷启动时首次读取可能撞上「后端还没起来」而失败，
    // 那时会把兜底值一直用下去（同步过一次就立刻返回，无额外开销）。
    // 与 requests-panel 的 load 同一处理。
    void syncAutoRefresh();
    // 不做互斥锁，而是「最后一次请求胜出」：用户连点几档范围时，
    // 用锁会把后面几次点击直接吞掉（界面停在旧数据上，看着像没反应）；
    // 这里让它们照常发出，只认最新那次的结果，先回来的旧响应一律作废。
    const token = ++seq;
    const requested = range;
    try {
      const data = await api.getStatsSummary(requested);
      if (token !== seq) return summary;
      if (!data || typeof data !== 'object') throw new Error('后端未返回报表数据');
      summary = data;
      renderAll();
      return summary;
    } catch (error) {
      if (token !== seq) return summary;
      summary = null;
      if (!silent) console.warn('读取报表数据失败:', error.message);
      renderFailure(error.message || '未知错误');
      return null;
    }
  }

  // ─── 自动刷新（间隔由「定时任务」页配置，见文件头状态区的说明）────

  /**
   * 应用「定时任务」页推来的新配置（也用于启动时自读，见 `syncAutoRefresh`）。
   *
   * `task` 的形状 = `/api/scheduled-tasks` 里的一条（`{enabled, interval, unit}`）。
   * 传 null / 形状不符时退回默认值 —— 界面不该因为一个读不到的配置就
   * 完全停止刷新（那看起来像功能坏了）。
   */
  function applyAutoRefresh(task) {
    // 配置已经由推送方给过，标上已同步：待重试的自读就不必再跑（更糟的是，
    // 那次读若失败会把用户刚在定时任务页改好的间隔覆盖回兜底值）
    autoSynced = true;
    const interval = Number(task?.interval);
    const valid = task && typeof task === 'object'
      && Number.isFinite(interval) && interval > 0
      && (task.unit === 'seconds' || task.unit === 'minutes');
    if (!valid) {
      autoEnabled = true;
      autoRefreshMs = DEFAULT_AUTO_REFRESH_MS;
    } else {
      autoEnabled = task.enabled !== false;
      autoRefreshMs = task.unit === 'minutes' ? interval * 60_000 : interval * 1000;
    }
    startAuto();
  }

  /**
   * 启动时自己拉一次配置。
   *
   * 为什么本面板要自己拉而不是等「定时任务」页推：用户完全可能直接打开报表页
   * （上次停留的页），而从未进过定时任务页 —— 那样推的动作永远不会发生，
   * 间隔就一直是兜底值。与 logs-panel / requests-panel 同一处理。
   *
   * 读取失败**不**标记为已同步，于是切回本页时（`showPage` 里的重试）会再来
   * 一次 —— 首次读取失败最常见的原因就是「后端还没起来」（冷启动），
   * 一次失败就永久用兜底值，用户会以为「我在定时任务页改的间隔没生效」。
   */
  async function syncAutoRefresh() {
    if (autoSynced) return;
    try {
      const list = await api.getScheduledTasks();
      const task = (list?.tasks || []).find(item => item.id === 'reportAutoRefresh');
      applyAutoRefresh(task || null);
      // 请求成功就标记同步过（哪怕这一条不在清单里 —— 那是后端版本旧，
      // 再重试也不会有，标记下来免得每次切页面都白跑一次请求）
      autoSynced = Array.isArray(list?.tasks);
    } catch (error) {
      // 读不到就用兜底值继续跑（见 applyAutoRefresh 的说明），下次切进本页再试
      console.warn('读取报表自动刷新间隔失败，按默认 1 秒:', error.message);
      applyAutoRefresh(null);
    }
  }

  function startAuto() {
    stopAuto();
    if (!autoEnabled || autoRefreshMs <= 0) return;
    timer = setInterval(() => {
      // 只在报表页可见时轮询，避免后台无谓请求
      if (document.hidden || wbApp.currentPage !== 'overview') return;
      // 上一轮还没回来就跳过这一拍（见 `polling` 的说明）
      if (polling) return;
      polling = true;
      // silent：自动刷新是背景动作，偶发失败不该往控制台刷噪音
      //（错误态照常显示在页面上，用户看得到）
      void load({ silent: true }).finally(() => { polling = false; });
    }, autoRefreshMs);
  }

  function stopAuto() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  // ─── 时间范围 ──────────────────────────────

  /** 把选中态刷到分段控件上（HTML 里预置的是默认值 7，存过的值在这里纠正） */
  function syncRangeButtons() {
    document.querySelectorAll('#report-range .seg-item[data-range]').forEach(item => {
      item.classList.toggle('active', item.dataset.range === range);
    });
  }

  function setRange(next) {
    const value = RANGES.includes(next) ? next : DEFAULT_RANGE;
    if (value === range) return;
    range = value;
    try {
      localStorage.setItem(RANGE_KEY, value);
    } catch { /* 存储不可用时只影响下次启动，本次会话照常 */ }
    syncRangeButtons();
    void load();
  }

  // ─── 尺寸自适应 ────────────────────────────
  // 三张图的坐标都是按容器宽度算出来的像素值，窗口一变就得重算。
  // 只认「宽度」变化：重绘会改容器高度，若连高度也响应就会自激循环。
  // 注意「容器宽度」本身就是坐标的输入，所以这里没有别的落脚点，
  // 只能靠 renderAll 里的指纹比对保证「宽度没实质变化就不重建」。

  function watch(id) {
    const box = $(id);
    if (!box || typeof ResizeObserver !== 'function') return;
    let last = box.clientWidth;
    let queued = false;
    const observer = new ResizeObserver(() => {
      const width = box.clientWidth;
      // 页面被藏起来时宽度是 0，跳过；重新显示时 0 → 实际宽度会再触发一次
      if (!width || width === last) return;
      last = width;
      // 合并到一帧：拖窗口会连续吐尺寸事件，每帧重绘一次即可
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        renderAll();
      });
    });
    observer.observe(box);
    watchers.push(observer);
  }

  watch('report-heatmap');
  watch('report-cache-trend');
  watch('report-daily-trend');

  // ─── 事件绑定 ──────────────────────────────

  $('report-range')?.addEventListener('click', event => {
    const item = event.target.closest('.seg-item[data-range]');
    if (item && !item.disabled) setRange(item.dataset.range);
  });

  // 页头原先那颗「刷新」按钮已移除：本页现在按「定时任务」页配置的间隔自动
  // 刷新（默认 1 秒，页面可见才跑），手动再点一次已经没有意义 —— 留着它反而
  // 会让人以为「不点就不会更新」。要改节奏去「定时任务」页，要立刻看最新数据
  // 切走再切回来即可（`showPage` 会调一次 load）。

  paint('report-heat-legend', heatLegendHtml());
  syncRangeButtons();

  window.wbReport = {
    load,
    render,
    lastSummary: () => summary,
    // 「定时任务」页改完间隔后推过来（见 tasks-panel.js 的 pushAutoRefresh）
    applyAutoRefresh,
  };

  // 首屏自持加载：app.js 的 showPage 在本脚本加载前就执行过了（脚本排在 app.js 之后），
  // 若上次停留在报表页，那次调用还拿不到 window.wbReport，这里必须补一次
  if (wbApp.currentPage === 'overview') void load({ silent: true });

  // 自动刷新：启动时自读一次配置（失败就按兜底值跑），随后由定时器接管。
  // 不依赖「定时任务」页推 —— 用户完全可能一次都没进过那一页。
  void syncAutoRefresh();
})();
