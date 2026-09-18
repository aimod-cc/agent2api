/* WorkBuddy 本地代理 · 报表页（时间范围 / 统计概览 / 热力图 / 缓存命中率 / 两个趋势图） */
/* global workbuddyDesktop, wbApp */

/**
 * 独立于 app.js 的报表面板模块：自持时间范围、自持最近一次的后端数据。
 *
 * 与 logs-panel.js / settings-panel.js 同构：依赖 window.wbApp 的 esc / toast / currentPage，
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
  const { esc, toast } = wbApp;

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
   * Token 数：一万以内给精确千分位，超过才缩写成 k / M。
   * 理由是分布 —— 本工具多数日子只有几千 Token，精确值比「8.4k」好读；
   * 而上万以后数字会开始撑破概览小格子，缩写换来的是版式稳定。
   */
  function formatTokens(value) {
    const num = Number(value) || 0;
    if (num < 10_000) return num.toLocaleString('zh-CN');
    if (num < 1_000_000) return `${Number((num / 1000).toFixed(1))}k`;
    return `${Number((num / 1_000_000).toFixed(2))}M`;
  }

  /** 命中率保留一位小数：整数百分比看不出 87.5% 与 87.9% 的差别 */
  function formatPercent(rate) {
    return `${((Number(rate) || 0) * 100).toFixed(1)}%`;
  }

  /** 坐标保留一位小数即可，串更短、指纹比对也更稳 */
  const round1 = value => Math.round(value * 10) / 10;

  /** 纵轴上限抬到「好读」的档位（1/2/5 × 10^n）：刻度才不会出现 37213 这种数 */
  function niceCeil(value) {
    if (!(value > 0)) return 1;
    const pow = 10 ** Math.floor(Math.log10(value));
    const norm = value / pow;
    const step = norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 5 ? 5 : 10;
    return step * pow;
  }

  /**
   * 纵轴刻度文案：单位由**上限**一次性定下，整条轴才统一。
   * 若按每个刻度的数值各自决定（3,437.5 / 13.8k 混排），
   * 同一条轴上会同时出现千分位与 k，看着像两套坐标。
   */
  function axisText(value, max) {
    if (max >= 1_000_000) return `${Number((value / 1_000_000).toFixed(2))}M`;
    if (max >= 10_000) return `${Number((value / 1000).toFixed(1))}k`;
    // 小量级下刻度多是整数；上限只有个位数时可能出现 .5，保留一位即可
    return Number.isInteger(value) ? formatInt(value) : value.toFixed(1);
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

  // ─── 板块二：热力图 ────────────────────────

  /**
   * GitHub 贡献图布局：53 列（周）× 7 行（星期）。
   * 首列用空格补齐到周一、末列不满也留空，与 GitHub 观感一致。
   *
   * 分档（0 / 1-2 / 3-5 / 6-10 / 10+）用**固定阈值**而不是分位数：
   * 本工具的日请求量普遍是个位数，分位数会把「那天只有 1 次请求」也涂成最深一档，
   * 颜色就不再表示「多少」，只剩「相对排名」，反而看不出使用强度。
   */
  function heatmapHtml(days) {
    const list = Array.isArray(days) ? days : [];
    if (!list.length) return placeholder('暂无热力图数据');

    const pad = (parseDay(list[0].date).getDay() + 6) % 7;   // 周一为 0：一周从周一起算
    const cells = new Array(pad).fill(null).concat(list);
    const cols = Math.ceil(cells.length / 7);

    const gap = 3;
    const labelW = 20;    // 左侧星期标签
    const topH = 14;      // 顶部月份标签
    // 格子边长随宽度自适应，但夹在 7–13px：再小看不清、再大就不像贡献图了
    const size = Math.max(7, Math.min(13, Math.floor((widthOf($('report-heatmap')) - labelW - gap * (cols - 1)) / cols)));
    const step = size + gap;
    const height = topH + 7 * step - gap + 2;
    const svgW = labelW + cols * step - gap;

    const levelOf = requests => {
      const num = Number(requests) || 0;
      if (num <= 0) return 0;
      if (num <= 2) return 1;
      if (num <= 5) return 2;
      if (num <= 10) return 3;
      return 4;
    };

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
        rects += `<rect class="hm-cell l${levelOf(requests)}" x="${round1(labelW + col * step)}"`
          + ` y="${round1(topH + row * step)}" width="${size}" height="${size}" rx="2"`
          + ` tabindex="-1" data-tip="${esc(tip)}"></rect>`;
      }
    }

    const weekday = WEEKDAY_ROWS.map(([row, text]) =>
      `<text class="hm-axis" x="${labelW - 6}" y="${round1(topH + row * step + size / 2)}"`
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
      months += `<text class="hm-axis" x="${round1(labelW + col * step)}" y="10">${month + 1}月</text>`;
    }

    return `<svg class="report-svg" width="${round1(svgW)}" height="${height}"`
      + ` viewBox="0 0 ${round1(svgW)} ${height}" role="img" aria-label="近 365 天活跃热力图">`
      + `${months}${weekday}${rects}</svg>`;
  }

  /** 图例：五档色块，与格子共用 --hm-* 变量，色值只定义一处 */
  function heatLegendHtml() {
    return [0, 1, 2, 3, 4].map(level => `<span class="l${level}"></span>`).join('');
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

  function cacheTrendHtml(series) {
    const list = Array.isArray(series) ? series : [];
    if (!list.length) return placeholder('暂无缓存趋势数据');

    const width = widthOf($('report-cache-trend'));
    const height = 190;
    const pad = { left: 44, right: 14, top: 12, bottom: 24 };
    const plotW = Math.max(40, width - pad.left - pad.right);
    const plotH = height - pad.top - pad.bottom;

    const rates = list.map(item => Number(item.rate) || 0);
    // 纵轴上限按真实峰值抬：上游把缓存读取与输入分开上报时命中率会合法地超过 100%，
    // 夹到 100% 等于谎报数据，抬上限只是让曲线留在画布里
    const yMax = Math.max(1, niceCeil(Math.max(...rates)));
    const xAt = index => pad.left + (list.length > 1 ? (plotW * index) / (list.length - 1) : plotW / 2);
    const yAt = rate => pad.top + plotH - (plotH * rate) / yMax;
    const pointAt = index => `${round1(xAt(index))},${round1(yAt(rates[index]))}`;

    // 网格线同时当刻度用：等分 4 段，标签按是否整百分比决定小数位
    let grid = '';
    for (let i = 0; i <= 4; i += 1) {
      const value = (yMax * i) / 4;
      const y = round1(yAt(value));
      const pct = value * 100;
      grid += `<line class="chart-grid" x1="${pad.left}" y1="${y}" x2="${round1(pad.left + plotW)}" y2="${y}"></line>`
        + `<text class="chart-axis" x="${pad.left - 8}" y="${y}" text-anchor="end" dominant-baseline="middle">`
        + `${Number.isInteger(pct) ? pct : pct.toFixed(1)}%</text>`;
    }

    // 点也要画，而且单点时要画得更醒目：polyline 只有一个点时连不出线段
    // （SVG 折线至少要两个点才有笔画），此时「整张图」就剩这一个圆点，
    // 半径还按常态的 2.5 会小到像渲染失败。
    const dotR = list.length === 1 ? 4.5 : 2.5;
    const dots = list.map((_, index) =>
      `<circle class="chart-dot" cx="${round1(xAt(index))}" cy="${round1(yAt(rates[index]))}" r="${dotR}"></circle>`).join('');

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
        + ` · 命中 ${formatTokens(item.hitTokens)} / 输入 ${formatTokens(item.inputTokens)}`;
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
      + ` role="img" aria-label="近 24 小时缓存命中率趋势">${grid}`
      + `<polyline class="chart-line" points="${list.map((_, index) => pointAt(index)).join(' ')}"></polyline>`
      + `${dots}${ticks}</svg>${hits ? `<svg class="report-svg chart-hit-layer" width="${width}" height="${height}"`
      + ` viewBox="0 0 ${width} ${height}" aria-hidden="true">${hits}</svg>` : ''}`;
  }

  // ─── 板块五：按天 Token 柱状图 ─────────────

  function dailyTrendHtml(series) {
    const list = Array.isArray(series) ? series : [];
    if (!list.length) return placeholder('暂无趋势数据');

    const width = widthOf($('report-daily-trend'));
    const height = 210;
    const pad = { left: 50, right: 14, top: 12, bottom: 26 };
    const plotW = Math.max(40, width - pad.left - pad.right);
    const plotH = height - pad.top - pad.bottom;

    const values = list.map(item => Number(item.tokens) || 0);
    const peak = Math.max(...values);
    const yMax = niceCeil(peak);
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
        // 刻度文案按上限统一单位（见 axisText 的取舍说明）
        grid += `<line class="chart-grid" x1="${pad.left}" y1="${y}" x2="${round1(pad.left + plotW)}" y2="${y}"></line>`
          + `<text class="chart-axis" x="${pad.left - 8}" y="${y}" text-anchor="end" dominant-baseline="middle">`
          + `${axisText(value, yMax)}</text>`;
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
      + ` role="img" aria-label="按天 Token 趋势">${grid}${bars}${ticks}${empty}</svg>`
      + (hits ? `<svg class="report-svg chart-hit-layer" width="${width}" height="${height}"`
        + ` viewBox="0 0 ${width} ${height}" aria-hidden="true">${hits}</svg>` : '');
  }

  // ─── 统一渲染 ──────────────────────────────

  /** 五块一起画。宽度变化不必额外传参：三张图的 HTML 里已经写了按宽度算出的
   *  坐标，宽度真变了 HTML 自然不同，paint 的指纹比对就会重绘；
   *  宽度只是被拖动改了一点点、算出来的整数尺寸没变时，它自然什么也不做。 */
  function renderAll() {
    if (!summary) return;
    const trend = Array.isArray(summary.dailyTrend) ? summary.dailyTrend : [];
    const rangeKey = RANGES.includes(summary.range) ? summary.range : range;

    paint('report-overview', overviewHtml(summary.overview, rangeKey, trend.length));
    paint('report-heatmap', heatmapHtml(summary.heatmap));
    paint('report-cache-rates', cacheRatesHtml(summary.cacheRates));
    paint('report-cache-trend', cacheTrendHtml(summary.cacheTrend24h));
    paint('report-daily-trend', dailyTrendHtml(summary.dailyTrend));
    paint('report-range-label', esc(RANGE_LABEL[rangeKey] || ''));
    paint('report-trend-label', esc(`${summary.startDate || '—'} ～ ${summary.endDate || '—'}`));
  }

  /**
   * 加载失败：五个板块各自显示错误态，而不是整页崩掉。
   * 每块都写、而不是只弹一个 toast —— 用户需要知道的是「这些数字现在不可信」。
   */
  function renderFailure(message) {
    const html = placeholder(`读取报表失败：${message}`, 'empty report-error');
    ['report-overview', 'report-heatmap', 'report-cache-rates', 'report-cache-trend', 'report-daily-trend']
      .forEach(id => paint(id, html));
  }

  function render(data) {
    if (data !== undefined) summary = data || null;
    renderAll();
  }

  /** silent：只压掉控制台噪音（首屏自持加载用），错误态照常显示 */
  async function load({ silent = false } = {}) {
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

  $('btn-report-refresh')?.addEventListener('click', async event => {
    const button = event.currentTarget;
    button.disabled = true;
    const data = await load();
    button.disabled = false;
    toast(data ? '✅ 报表已刷新' : '刷新失败，详见控制台', data ? 'ok' : 'err');
  });

  paint('report-heat-legend', heatLegendHtml());
  syncRangeButtons();

  window.wbReport = { load, render, lastSummary: () => summary };

  // 首屏自持加载：app.js 的 showPage 在本脚本加载前就执行过了（脚本排在 app.js 之后），
  // 若上次停留在报表页，那次调用还拿不到 window.wbReport，这里必须补一次
  if (wbApp.currentPage === 'overview') void load({ silent: true });
})();
