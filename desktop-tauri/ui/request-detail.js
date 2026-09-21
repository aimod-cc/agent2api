/* Agent2API · 请求日志的详情弹窗（上游原始报文 · 分段切换） */
/* global workbuddyDesktop, wbApp */

/**
 * 「请求日志」页的**详情弹窗**：调试模式下保存的上游原始报文。
 *
 * ── 为什么单独成文件 ──────────────────────────────────────────
 * 从 requests-panel.js 按职责拆出（那个文件已到 900 行，见项目「单文件不过
 * 800 行」的约定）。这一块与列表渲染**没有任何耦合**：它只要一个请求 id、
 * 自己去拉报文、自己管弹窗的开合与分段状态；列表那边只在点「详情」时调一次
 * `open(id)`，并转发一次分段点击。项目里已有同类先例（usage-panel.js 从
 * accounts-model.js 拆出、request-hover.js 与 requests-panel.js 分工）。
 *
 * ── 数据来源 ────────────────────────────────────────────────
 * 报文由 `/api/debug/traffic?id=` 按需拉取：列表接口不带报文（一个完整 SSE
 * 响应可达数百 KB，塞进列表会让每页体积爆掉）。取不到时后端给 404，弹窗里
 * 如实说明原因（当时没开调试模式 / 已超出保留条数），而不是留一片空白。
 *
 * 内容按「请求 → 响应」分块展示，与排障时的阅读顺序一致；头是 JSON 对象
 * （后端已把凭据类字段换成 [redacted]），体是原始文本（请求体是 JSON 文本，
 * 响应体是上游 SSE 原文）。
 *
 * ── 为什么是「分段选择器」而不是平铺四块（本次改造）───────────
 * 平铺时四块依次摞在弹窗里，每一块都是等宽原始报文（响应体常是几百行 SSE）。
 * 想看「上游发来的响应头」要先把请求体那一大段滚过去 —— 而排障时真正需要的
 * 往往只有其中一块。改成标签切换后，弹窗高度始终是「一块报文」的高度，
 * 四块之间一键直达。
 *
 * 参考 OmniProxy 的 `RequestLogDetailModal`（那边用 antd 的 `Tabs`），
 * 但我们没有组件库，所以用原生 HTML + 本项目自己的 `.seg` 分段样式
 * （与请求日志页的时间档位、账号页的筛选分段是同一套视觉）。
 *
 * **切换不重新拉数据**：四块内容在一次 `getDebugTraffic` 里全部拿到
 * （报文是一个整体，后端没有分块接口），所以切换只是改 class ——
 * 这样做还有个副作用是好的：不重建 DOM，当前分段的滚动位置不会丢。
 * 标签上另有「有内容没内容」的标记（空段置灰但仍可点），于是
 * 「这一侧没有报文」在点击**之前**就能看出来，不必点进去才发现是空的。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc } = wbApp;

  /** 弹窗开着时按 Esc 关闭（与确认弹窗同一交互） */
  function onKeydown(event) {
    if (event.key === 'Escape') close();
  }

  function close() {
    $('req-detail-modal')?.classList.remove('open');
    document.removeEventListener('keydown', onKeydown);
  }

  /** 对象 → 缩进 JSON 文本；失败时给空串（不让展示层抛错） */
  function jsonText(value) {
    if (value === null || value === undefined) return '';
    if (typeof value === 'string') return value;
    try {
      return JSON.stringify(value, null, 2);
    } catch {
      return '';
    }
  }

  /**
   * 详情弹窗的四个分段（顺序即阅读顺序：请求 → 响应，头 → 体）。
   *
   * `key` 同时是 DOM 上的 `data-detail-seg` 取值与分段状态的键；
   * `label` 是标签文案；`pick(data)` 从报文对象里取出这一段的正文文本。
   *
   * **取文本的逻辑集中在 `pick` 里**：改造前是四个 `detailBlock(...)` 表达式
   * 平铺在模板里，现在由这里的数据驱动 —— 加一段只改这张表，
   * 标签、面板、空态判定的口径不可能各写一套。
   */
  const SEGS = [
    {
      key: 'req-headers',
      label: '请求头',
      // 后端的键名与调试报文的契约逐字一致（见 core::debug_traffic 的 entry_json）
      pick: data => jsonText(data.requestHeaders),
    },
    { key: 'req-body', label: '请求体', pick: data => jsonText(data.requestBody) },
    {
      key: 'res-headers',
      label: '响应头',
      pick: data => jsonText(data.responseHeaders),
    },
    {
      key: 'res-body',
      label: '响应体',
      // 响应体是原始文本（SSE），不是 JSON —— 不经过 jsonText 的序列化
      pick: data => (data.responseBody ? String(data.responseBody) : ''),
    },
  ];

  /**
   * 当前选中的分段。**不跨次保留**（每次打开都回到第一个有内容的分段）。
   *
   * ── 为什么不做「记住上次」────────────────────────────────────
   * 记忆的语义在这里站不住：每次点开的都是**另一条请求**，而各条请求的
   * 内容分布差别很大（成功的流式请求有响应体、转发前失败的可能只有请求头）。
   * 记住上一次的「响应体」而这一次那条请求没有响应体，用户点开详情看到的
   * 就是一片空白 —— 那比「每次都从有内容的那一段开始」糟得多。
   * 也不写 localStorage：那是跨会话的偏好，而这个选择连本次会话内都
   * 不该跟着走（同一个理由）。
   *
   * 默认落到**第一个有内容的分段**（通常是请求头；完全没有报文时落到第一段
   * 并显示空态），于是「打开就能看到点东西」这条不依赖运气。
   */
  let active = SEGS[0].key;

  /** 分段选择器：标签 + 每段的「有没有内容」标记（空段置灰，但**仍可点**） */
  function segsHtml(blocks) {
    const items = SEGS.map(seg => {
      const empty = !blocks[seg.key];
      const on = seg.key === active;
      // 空段用小圆点提示；同时进 title —— 一个纯视觉标记对读屏软件没有意义
      const mark = empty ? '<span class="seg-dot" aria-hidden="true"></span>' : '';
      const title = empty ? `${seg.label}（这条请求没有保存这一段）` : seg.label;
      return `<button type="button" class="seg-item${on ? ' active' : ''}${empty ? ' zero' : ''}"`
        + ` data-detail-seg="${seg.key}" role="tab"`
        + ` aria-selected="${on}" title="${esc(title)}">${esc(seg.label)}${mark}</button>`;
    }).join('');
    return `<div class="seg req-detail-segs" role="tablist" aria-label="上游报文分段">${items}</div>`;
  }

  /** 四个分段面板（一次全部渲染，由 CSS 按 `is-active` 决定显示哪一个） */
  function panesHtml(blocks) {
    return SEGS.map(seg => {
      const text = blocks[seg.key];
      const on = seg.key === active;
      // 空段给一句空态而不是留白：留白会被读成「渲染没跑完」
      const inner = text
        ? `<pre class="req-detail-pre">${esc(text)}</pre>`
        : '<div class="req-detail-empty">这一条请求没有保存这一段报文</div>';
      return `<section class="req-detail-pane${on ? ' is-active' : ''}"`
        + ` data-detail-pane="${seg.key}" role="tabpanel">`
        + `<h3>${esc(seg.label)}</h3>${inner}</section>`;
    }).join('');
  }

  function render(data) {
    const body = $('req-detail-body');
    const hint = $('req-detail-hint');
    if (!body) return;
    const status = data.status === null || data.status === undefined ? '-' : String(data.status);
    const lines = [
      data.url ? `URL: ${data.url}` : '',
      data.provider ? `提供商: ${data.provider}` : '',
      `上游状态码: ${status}`,
    ].filter(Boolean);
    // 先把四段文本一次算出来：标签的「空/非空」标记、默认分段的选择、
    // 以及面板内容都读同一份，不会出现「标签说非空、点开却是空态」的错位
    const blocks = {};
    for (const seg of SEGS) blocks[seg.key] = seg.pick(data) || '';
    // 默认分段：第一个有内容的（全都为空时落到第一段，那里会显示空态文案）。
    // 每次 render 都重算 —— 与「不跨次保留」是同一件事（见 active 的说明）
    const firstFilled = SEGS.find(seg => blocks[seg.key]);
    active = (firstFilled || SEGS[0]).key;
    body.innerHTML =
      `<div class="req-detail-meta">${lines.map(esc).join('<br>')}</div>`
      + segsHtml(blocks)
      + panesHtml(blocks);
    if (hint) {
      hint.textContent = data.truncated
        ? '报文超过单条上限，请求体 / 响应体已按上限截断'
        : '内容按原样保存，请求头中的凭据字段已替换为 [redacted]';
    }
  }

  /**
   * 切换分段（纯 DOM 操作，**不发请求**，见模块头「切换不重新拉数据」）。
   *
   * **只改 class，不动 innerHTML**：重新渲染会丢掉用户在当前分段里的滚动位置
   * （排障时常见动作就是「滚到响应体中间对一段日志」，一切换就回到顶部很难用）。
   */
  function switchSeg(key) {
    if (!SEGS.some(seg => seg.key === key)) return;
    active = key;
    const body = $('req-detail-body');
    body?.querySelectorAll('[data-detail-seg]').forEach(node => {
      const on = node.dataset.detailSeg === key;
      node.classList.toggle('active', on);
      node.setAttribute('aria-selected', String(on));
    });
    body?.querySelectorAll('[data-detail-pane]').forEach(node => {
      node.classList.toggle('is-active', node.dataset.detailPane === key);
    });
  }

  /** 打开某条请求的详情（列表那边点「详情」时调它） */
  async function open(id) {
    const modal = $('req-detail-modal');
    const body = $('req-detail-body');
    const hint = $('req-detail-hint');
    if (!modal || !body) return;
    body.textContent = '正在读取…';
    if (hint) hint.textContent = '—';
    modal.classList.add('open');
    document.addEventListener('keydown', onKeydown);
    try {
      render(await api.getDebugTraffic(id));
    } catch (error) {
      // 404（没开调试模式 / 超出保留条数）与其它失败在这里收敛成同一块提示：
      // 后端给的文案已经说清了原因，直接展示它
      body.innerHTML = `<div class="req-detail-empty">${esc(error.message || '读取失败')}</div>`;
      if (hint) hint.textContent = '在设置 → 通用里开启「调试模式」后，新发生的请求才会保存报文';
    }
  }

  // ─── 事件绑定 ──────────────────────────────
  //
  // 委托在弹窗主体上：分段标签是每次 render 重建的，绑在按钮上会随重绘失效
  // （与列表的委托同一手法）。

  $('req-detail-close')?.addEventListener('click', close);
  $('req-detail-cancel')?.addEventListener('click', close);
  $('req-detail-modal')?.addEventListener('click', event => {
    // 点遮罩关闭（与确认弹窗同一交互）
    if (event.target === $('req-detail-modal')) close();
  });
  $('req-detail-body')?.addEventListener('click', event => {
    const item = event.target.closest('[data-detail-seg]');
    if (item) switchSeg(item.dataset.detailSeg);
  });

  window.wbRequestDetail = { open, close };
})();
