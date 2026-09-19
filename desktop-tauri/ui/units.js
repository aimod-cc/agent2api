/* Agent2API · Token 计量单位的展示口径（报表读数 / 图表刻度共用） */
/* global */

/**
 * Token 读数只在这里格式化一次，于是「亿 / 万」与「M / k」两套口径不会在
 * 报表的十来处读数里各写一遍、也必然一起切换。
 *
 * ── 为什么要有两套口径 ───────────────────────────────────────
 * 「1140.05M」与「11.4亿」是同一个数，但前者要读三遍才反应得过来量级，
 * 而界面文案全是中文。所以默认按中文量级显示（万 / 亿），需要英文缩写
 * （k / M，与上游文档、日志里的写法一致）时可以在设置页关掉。
 *
 * ── 为什么这个开关存 localStorage 而不是 desktop-settings.json ──
 * 它只改变本机界面怎么显示一串数字：不影响进程行为（那是 closeToTray /
 * autostart 的范畴，所以要放主进程），也不该跟着账号数据迁移到别的机器
 * （那边可能是英文习惯）。这与「主题」「设置页当前分类」属于同一类的纯
 * 前端偏好，所以沿用同一套存储方式与 workbuddy-desktop-* 键名前缀。
 *
 * ── 单位怎么选 ───────────────────────────────────────────────
 * 中文：一万以内给精确千分位（这个量级下缩写反而看不出差别），再往上按
 *       「万」「亿」两档，各保留一位小数、整数不带小数点（1.2亿 / 8400万）。
 * 英文：同样的分档逻辑，但用 k / M 两个后缀，且与改造前的写法逐字一致
 *       （1.20M / 8.4k）—— 关掉开关就是回到旧观感。
 *
 * ── 轴刻度与读数的差别 ───────────────────────────────────────
 * 中文档下每个刻度各自按自己的量级定单位（0 / 6000万 / 1.2亿 / 1.8亿 / 2.4亿），
 * 因为「万」与「亿」的字面量级一目了然；英文档下按**整条轴**的最大值定单位，
 * 免得同一条轴上混排「3,437.5」与「13.8k」这两种看着像两套坐标的写法。
 * 两种取舍的差别由 `formatAxis` 的 `max` 参数表达：中文档不用它。
 */
(() => {
  /** 持久化键：沿用项目既有的 workbuddy-desktop-* 前缀 */
  const STORAGE_KEY = 'workbuddy-desktop-chinese-units';

  /** 只有明确存过 'off' 才算关闭；读取抛错（存储被禁用）时按默认值处理 */
  function readChinese() {
    try {
      return localStorage.getItem(STORAGE_KEY) !== 'off';
    } catch {
      return true;
    }
  }

  /** 默认开启：界面文案是中文，读数跟着中文量级更好读 */
  let chinese = readChinese();

  // ─── 单位换算 ──────────────────────────────

  const formatInt = value => (Number(value) || 0).toLocaleString('zh-CN');

  /** 四舍五入到 `digits` 位，并顺手去掉尾随的零（2.40 → 2.4、1.00 → 1）。
   *  走 Number 而不是字符串 trim：`(1.00).toFixed(1)` 是 "1.0"，
   *  而 `Number("1.0")` 再拼进模板就是 "1"，不必自己写一个去零正则。 */
  const fix = (value, digits) => Number(value.toFixed(digits));

  /** 中文量级：一万以内给精确千分位，再往上按万 / 亿缩写 */
  function chineseTokens(value) {
    const num = Number(value) || 0;
    if (num < 10_000) return formatInt(num);
    if (num < 100_000_000) {
      const wan = fix(num / 10_000, 1);
      // 9999.95 万往上会被四舍五入成「10000万」，进位成「1亿」才符合读数直觉
      return wan >= 10_000 ? `${fix(num / 100_000_000, 1)}亿` : `${wan}万`;
    }
    return `${fix(num / 100_000_000, 1)}亿`;
  }

  /** 英文量级：k / M 两档，与改造前 report.js 的写法保持一致 */
  function englishTokens(value) {
    const num = Number(value) || 0;
    if (num < 10_000) return formatInt(num);
    if (num < 1_000_000) return `${fix(num / 1000, 1)}k`;
    return `${fix(num / 1_000_000, 2)}M`;
  }

  /** 读数：概览小格、图表标注、tooltip 都走这一个函数 */
  const formatTokens = value => (chinese ? chineseTokens(value) : englishTokens(value));

  /**
   * 纵轴刻度文案。`max` 只在英文档下用到（整条轴统一单位，见文件头）。
   * 小量级下刻度多是整数；上限只有个位数时可能出现 .5，保留一位即可。
   */
  function formatAxis(value, max) {
    if (chinese) return chineseTokens(value);
    if (max >= 1_000_000) return `${fix(value / 1_000_000, 2)}M`;
    if (max >= 10_000) return `${fix(value / 1000, 1)}k`;
    return Number.isInteger(value) ? formatInt(value) : value.toFixed(1);
  }

  /** 切换口径。值没变就什么都不做：设置页回填开关时也会调到这里 */
  function setChinese(on) {
    const next = on === true;
    if (next === chinese) return;
    chinese = next;
    try {
      localStorage.setItem(STORAGE_KEY, next ? 'on' : 'off');
    } catch { /* 存储不可用只影响下次启动，本次会话照常 */ }
    // 用事件而不是直接回调：订阅方（报表页）与设置页互不认识，谁关心谁注册，
    // 以后再加订阅方也不必回来改这里
    window.dispatchEvent(new CustomEvent('wb-units-changed'));
  }

  window.wbUnits = {
    formatInt,
    formatTokens,
    formatAxis,
    isChinese: () => chinese,
    setChinese,
  };
})();
