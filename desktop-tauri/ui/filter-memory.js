/* Agent2API · 页面筛选条件的跨次启动记忆（账号 / 模型 / 请求日志 / 日志四页共用） */

/**
 * 「筛选条件记住上次的选择」的读 / 写小工具。
 *
 * 每个页面一个自己的 localStorage 键，值是「维度 → 取值」的扁平 JSON 对象
 * （如 `{provider:'workbuddy', enabled:'all', limit:'limited'}`）。空串是合法值
 * —— 下拉类筛选用空串表示「全部」。各页自己决定哪些维度值得记（描述「此刻
 * 在盯什么」的临时开关不记，见 requests-panel 的「仅看进行中」）。
 *
 * 为什么收成一份工具：四个页面的行为完全一致（启动恢复、变更落盘、存坏回落
 * 默认），逐页各写一份迟早漂成四种行为。这里只管存取，UI 的回填（分段按钮
 * 的 active、下拉的 value）仍归各页 —— 控件形态不同，收上来只会多出一堆配置项。
 */
(() => {
  /**
   * 读一份筛选条件。按 defaults 的键收窄：非字符串的值（存坏 / 手改坏）一律
   * 落回该键的默认值 —— 记忆是锦上添花，不能让首屏拿到半份数据或抛错。
   */
  function load(key, defaults) {
    const out = { ...defaults };
    try {
      const raw = JSON.parse(localStorage.getItem(key) || '{}');
      if (raw && typeof raw === 'object') {
        for (const name of Object.keys(defaults)) {
          const value = raw[name];
          if (typeof value === 'string') out[name] = value;
        }
      }
    } catch { /* 解析失败按全默认算 */ }
    return out;
  }

  /** 合并写回：patch 里的键覆盖存量，其余维度保留（各页通常一次只改一个维度） */
  function save(key, patch) {
    try {
      let current = {};
      try { current = JSON.parse(localStorage.getItem(key)) || {}; } catch { }
      localStorage.setItem(key, JSON.stringify({ ...current, ...patch }));
    } catch { /* 存储不可用只影响下次打开，本次会话照常 */ }
  }

  window.wbFilterMemory = { load, save };
})();
