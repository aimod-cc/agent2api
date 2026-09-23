/* Agent2API · 请求日志的详情弹窗（请求详情 / 预览对话 / 原始报文 · 三标签） */
/* global workbuddyDesktop, wbApp, wbConversationPreview */

/**
 * 「请求日志」页的**详情弹窗**，三个标签（参考 OmniProxy 的
 * `RequestLogDetailModal`，那边是 antd Tabs，这里用本项目自己的 `.seg`）：
 *
 *   ① 请求详情（默认）—— 列表行数据的完整展开：时间 / 状态 / 耗时 / 模型 /
 *      提供商 / 账号 / 令牌 / 错误 / 尝试明细 / 敏感词命中。数据来自**调用方
 *      传进来的行对象** `open(id, row)`（列表当前一屏的数据，不发任何请求）；
 *      row 缺失（列表在弹窗打开前恰好刷新过）时显示「数据已刷新，请重试」。
 *   ② 预览对话 —— 调 `getStatsRequestRaw(id)` 拿下游原文（请求体 + 最终响应），
 *      解析与气泡渲染全在 conversation-preview.js（纯函数）。404 / 双空给空态：
 *      报文有自己的容量闸（按条数与时间丢最旧），「明细还在、原文已被挤掉」
 *      是正常状态，不是错误。
 *   ③ 原始报文 —— 调试模式保存的**上游**原始报文（`getDebugTraffic` 的四段：
 *      请求头 / 请求体 / 响应头 / 响应头，凭据已脱敏），原样保留为四块竖排。
 *      调试模式没开（或超出保留）时**整个标签隐藏**——它本来就是可选的排障
 *      数据，缺了不该让用户在两个空标签里找内容。
 *
 * ── 拉取与切换 ────────────────────────────────────────────────
 * 两个接口在 open 时**并行**发起，谁到了渲染谁（各自动自己那块 pane 与标签
 * 条，互不阻塞）；**切换标签不发任何请求** —— 只改 class，当前 pane 的滚动
 * 位置因此得以保留（与改造前分段切换同一手法）。快速连点两条详情时用 seq
 * 只认最后一次 open，迟到的旧响应一律丢弃。
 *
 * ── 与列表的分工 ─────────────────────────────────────────────
 * requests-panel.js 只在点「详情」时调 `open(id, row)` 并转发弹窗内的事件
 * 委托之外的**所有**弹窗逻辑都在本文件（它独占 #req-detail-modal 那组 DOM）。
 * 列表的格式化函数（时间 / 耗时 / 状态徽章等）是那个 IIFE 的私有成员，这里
 * 按同一口径各有一份小实现 —— 两边的判据注释里互相指认，改口径时一起改。
 */
(() => {
  const api = workbuddyDesktop;
  const $ = id => document.getElementById(id);
  const { esc } = wbApp;

  // ─── 与列表同口径的格式化（requests-panel.js 同名函数是私有成员，这里是
  //     同一份判据的镜像；口径改动时两处要一起改）──────────────────

  /** 时间：完整本地格式（列表的 timeCell 分两行，详情里一行放得下） */
  function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    const pad = n => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
      + ` ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }

  /** 用时：秒以内给毫秒，分钟以上给分秒（与列表 formatDuration 逐字同源） */
  function fmtDuration(ms) {
    const rounded = Math.round(Number(ms) || 0);
    if (rounded < 1000) return `${rounded}ms`;
    const seconds = Math.floor(rounded / 1000);
    if (seconds >= 60) return `${Math.floor(seconds / 60)}分${seconds % 60}秒`;
    const millis = rounded % 1000;
    return millis > 0 ? `${seconds}秒${millis}ms` : `${seconds}秒`;
  }

  /** 首响：没有值（null / 0 / 旧数据）显示「-」，不走 fmtDuration 避免假 0ms */
  function fmtFirstResponse(ms) {
    const value = Number(ms);
    if (!Number.isFinite(value) || value <= 0) return '-';
    return fmtDuration(value);
  }

  /** 令牌读数：精确值 + 千分位（与列表 formatTokens 同源；0 就显示 0） */
  function fmtTokens(value) {
    return (Number(value) || 0).toLocaleString('zh-CN');
  }

  /** 进行中行的「已用时」：取整秒，不足 1 秒也显示 1 秒（列表同款） */
  function fmtElapsed(ts) {
    const elapsed = Math.max(0, Date.now() - (Number(ts) || 0));
    return `${Math.max(1, Math.floor(elapsed / 1000))}秒`;
  }

  /** 成功口径与后端 RequestEntry::is_success 对齐：2xx 且没有错误摘要 */
  function isOk(entry) {
    const status = Number(entry.status) || 0;
    return status >= 200 && status < 300 && !entry.error;
  }

  /** 进行中：status=0 且没有错误摘要（status=0 带摘要的是旧口径的失败行） */
  function isRunning(entry) {
    return (Number(entry?.status) || 0) === 0 && !entry?.error;
  }

  /** 提供商展示名：后端 label → 前端 providers 目录 → 原样回显 id（列表同源） */
  function providerName(entry) {
    const id = String(entry.provider ?? '').trim();
    return String(entry.providerLabel ?? '').trim()
      || (id ? window.wbProviders?.labelOf?.(id) || id : '');
  }

  /** 普通对象判断（row / debug / raw 响应的形状守卫；数组不算） */
  function isPlainObject(value) {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
  }

  // ─── 标签定义与状态 ──────────────────────────

  /**
   * 三个标签。`raw` 的显隐由调试报文的拉取结果决定（见 renderTabs），
   * 所以它不参与「默认选中」—— 默认恒为「请求详情」。
   */
  const TABS = [
    { key: 'detail', label: '请求详情' },
    { key: 'preview', label: '预览对话' },
    { key: 'raw', label: '原始报文' },
  ];
  const DEFAULT_TAB = 'detail';

  let active = DEFAULT_TAB;
  /**
   * 本次弹窗的全部状态。**不跨次保留**：每次 open 都整体重建（点开的总是
   * 另一条请求，内容分布差别很大，记住上次的标签只会增加「打开是空的」概率）。
   *   · row   列表行对象（可能为 null —— 见 detailPaneHtml 的空态）
   *   · raw   预览对话的数据：null = 还在路上；{ truncated, body } = 已到
   *   · debug 调试报文：null = 还在路上；'gone' = 确认没有（标签随之隐藏）；
     *         { truncated, data } = 已到
   */
  let view = null;
  /** 只认最后一次 open：连点两条详情时迟到的旧响应直接丢弃 */
  let seq = 0;

  function onKeydown(event) {
    if (event.key === 'Escape') close();
  }

  function close() {
    $('req-detail-modal')?.classList.remove('open');
    document.removeEventListener('keydown', onKeydown);
  }

  // ─── 标签条与面板骨架 ────────────────────────

  /**
   * 标签条。「原始报文」只在调试报文确认存在时出现（`view.debug` 是对象）；
   * 拉取失败与还没回来时都不渲染它 —— 前者是没数据，后者是「先别许诺」，
   * 标签出现的那一刻点进去必有内容。
   */
  function tabsHtml() {
    const visible = TABS.filter(tab => tab.key !== 'raw' || isPlainObject(view?.debug));
    const items = visible.map(tab => {
      const on = tab.key === active;
      return `<button type="button" class="seg-item${on ? ' active' : ''}"`
        + ` data-detail-tab="${tab.key}" role="tab" aria-selected="${on}">${esc(tab.label)}</button>`;
    }).join('');
    return `<div class="seg req-detail-segs" role="tablist" aria-label="详情内容">${items}</div>`;
  }

  const paneWrap = (key, inner) =>
    `<section class="req-detail-pane${key === active ? ' is-active' : ''}"`
    + ` data-detail-pane="${key}" role="tabpanel">${inner}</section>`;

  /** 空态块（请求详情 / 预览对话的兜底文案共用同一形制） */
  const emptyHtml = text => `<div class="req-detail-empty">${esc(text)}</div>`;

  // ─── 标签 ①：请求详情（数据全部来自 row）──────

  /**
   * 状态徽章：与列表 statusCell 同款（进行中带呼吸点、2xx 带摘要给 title）。
   * 判据（isRunning / isOk）是上面那两个镜像函数，文案与结构逐字同源。
   */
  function statusBadgeHtml(row) {
    if (isRunning(row)) {
      return `<span class="badge tag running"><span class="req-live-dot" aria-hidden="true"></span>进行中</span>`;
    }
    const status = Number(row.status) || 0;
    const ok = isOk(row);
    const title = !ok && status >= 200 && status < 300
      ? `HTTP ${status}，但响应体阶段出错`
      : '';
    return `<span class="badge tag ${ok ? 'ok' : 'bad'}"${title ? ` title="${esc(title)}"` : ''}>${status || '失败'}</span>`;
  }

  /**
   * 模型：与列表 modelCell 同口径 —— 下游 / 上游双名齐全且不同时两行
   * （⬆️ 上游实际收到的 / ⬇️ 下游请求的），否则回落 `model` 单行。
   * 推理等级同列表：模型名带 `(等级)` 后缀（上游 = 实际发出的等级，
   * 下游 = 客户端指定的等级；空串 = 无等级随行，与旧行为一致）。
   */
  function modelHtml(row) {
    const client = String(row.clientModel ?? '').trim();
    const upstream = String(row.upstreamModel ?? '').trim();
    const shown = String(row.model ?? '').trim();
    const clientLevel = String(row.clientReasoning ?? '').trim();
    const upstreamLevel = String(row.upstreamReasoning ?? '').trim();
    const tag = (name, level) => (level ? `${name}(${level})` : name);
    if (!client || !upstream || upstream.toLowerCase() === client.toLowerCase()) {
      return esc(tag(shown, upstreamLevel || clientLevel) || '—');
    }
    const upText = tag(upstream, upstreamLevel);
    const downText = tag(client, clientLevel);
    return `<span class="req-detail-model-split">`
      + `<span title="转发到上游的模型：${esc(upText)}">⬆️ ${esc(upText)}</span>`
      + `<span class="sub" title="下游请求的模型：${esc(downText)}">⬇️ ${esc(downText)}</span></span>`;
  }

  /** 令牌一行：输入 / 输出 / 总计 / 缓存读（0 显示 0 —— 与列表的数字格式一致） */
  function tokensHtml(row) {
    return `输入 ${esc(fmtTokens(row.promptTokens))} · 输出 ${esc(fmtTokens(row.completionTokens))}`
      + ` · 总计 ${esc(fmtTokens(row.totalTokens))} · 缓存读 ${esc(fmtTokens(row.cacheReadTokens))}`;
  }

  /** 耗时 / 首帧：进行中显示已用时（首响还没发生，不写假数字） */
  function durationHtml(row) {
    if (isRunning(row)) return `${esc(fmtElapsed(row.ts))}<span class="req-detail-sub">（请求仍在转发中）</span>`;
    return `${esc(fmtDuration(row.durationMs))} / 首帧 ${esc(fmtFirstResponse(row.firstResponseMs))}`;
  }

  /**
   * 尝试明细：每次上游尝试一行（提供商 / 账号 / 结果 / 内部重试 / 提示）。
   * 数据形状见后端 AttemptDetail；结果的三态判据与 request-hover 的
   * attemptRowHtml 同源（有 error 即失败、有 status 即成功、两者皆无是
   * 未定论）。明细比 attempts 短时如实说明（保头截断，列表弹层同一口径）。
   */
  function attemptsHtml(row) {
    const details = Array.isArray(row.attemptDetails) ? row.attemptDetails : [];
    if (!details.length) return '-';
    const attempts = Number(row.attempts) || 1;
    const rows = details.map((item, index) => {
      const status = Number(item?.status);
      const hasStatus = item?.status !== null && item?.status !== undefined && Number.isFinite(status);
      const error = item?.error ? String(item.error) : '';
      const result = error
        ? `<span class="req-attempt-bad">失败${hasStatus ? `（${esc(String(status))}）` : ''}：${esc(error)}</span>`
        : hasStatus
          ? `<span class="req-attempt-ok">成功（${esc(String(status))}）</span>`
          : '<span class="req-detail-sub">无结果记录</span>';
      const retries = Array.isArray(item?.retries) ? item.retries : [];
      const retryText = retries.length
        ? `↻ ${retries.length} 次`
        : '-';
      const retryTitle = retries.length
        ? retries.map((retry, i) => `第 ${i + 1} 次：${String(retry?.reason || '未知原因')}`).join('\n')
        : '';
      const notice = String(item?.notice ?? '').trim();
      return `<tr>`
        + `<td>${index + 1}</td>`
        + `<td>${esc(providerName(item) || (item?.provider ? String(item.provider) : '未知'))}</td>`
        + `<td>${item?.account ? esc(String(item.account)) : '—'}</td>`
        + `<td>${result}</td>`
        + `<td${retryTitle ? ` title="${esc(retryTitle)}"` : ''}>${esc(retryText)}</td>`
        + `<td>${notice ? esc(notice) : '—'}</td>`
        + `</tr>`;
    }).join('');
    const truncated = details.length < attempts
      ? `<tr class="req-attempt-more"><td colspan="6">另有 ${attempts - details.length} 次尝试未记录明细（只保留最早的 ${details.length} 条）</td></tr>`
      : '';
    return `<table class="req-attempt-table"><thead><tr>`
      + '<th>轮次</th><th>提供商</th><th>账号</th><th>结果</th><th>内部重试</th><th>提示</th>'
      + `</tr></thead><tbody>${rows}${truncated}</tbody></table>`;
  }

  /** 敏感词命中：紫色小标签（与列表「敏」标签同一套配色，这里展开写词与次数） */
  function sensitiveHtml(row) {
    const hits = Array.isArray(row.sensitiveHits) ? row.sensitiveHits : [];
    if (!hits.length) return '-';
    return hits.map(hit => `<span class="badge tag sensitive">`
      + `${esc(String(hit?.word ?? ''))} × ${esc(String(Number(hit?.count) || 0))}</span>`).join(' ');
  }

  /** 请求详情的两列网格（label 列 + value 列；value 里可能是表格 / 徽章组） */
  function detailPaneHtml(row) {
    if (!row) {
      // 列表在点开前恰好刷新过：行对象按 id 反查不到。详情标签只吃 row，
      // 没有可显示的数据 —— 说清原因让用户关掉重开，比一片空字段诚实
      return emptyHtml('列表数据已刷新，请关闭后重新打开这条详情');
    }
    // <table> 的单元格内容一律 esc / 白名单函数产出（attemptsHtml 与
    // sensitiveHtml 内部各自完成转义），这里不做二次拼接
    const cell = (label, value) => `<tr><th>${esc(label)}</th><td>${value}</td></tr>`;
    const account = String(row.accountName ?? '').trim();
    return `<table class="req-detail-grid"><tbody>`
      + cell('时间', esc(fmtTime(row.ts)))
      + cell('状态', statusBadgeHtml(row))
      + cell('耗时', durationHtml(row))
      + cell('重试次数', esc(String(Number(row.attempts) || 1)))
      + cell('模型', modelHtml(row))
      + cell('提供商', esc(providerName(row) || '—')
        + (account ? `<span class="req-detail-sub">（账号：${esc(account)}）</span>` : ''))
      + cell('令牌', tokensHtml(row))
      + cell('错误', row.error ? `<span class="req-detail-error">${esc(String(row.error))}</span>` : '-')
      + cell('尝试明细', attemptsHtml(row))
      + cell('敏感词', sensitiveHtml(row))
      + `</tbody></table>`;
  }

  // ─── 标签 ②：预览对话 ────────────────────────

  /**
   * 预览对话面板。数据三种状态：
   *   null          → 「正在读取…」（并行拉取还在路上）
   *   'gone'        → 确认没有（404 / 桥接返回形状不对 / 两侧都空）→ 空态
   *   body 到了     → conversation-preview 的渲染结果（内部自带各层降级）
   * truncated 是**读取侧**的启发式标记（任一侧达到采集上限就置位）——
   * 恰好等于上限而没截断的报文会被误标，代价只是多一行提示，可接受。
   */
  function previewPaneHtml() {
    if (!view.raw) return emptyHtml('正在读取…');
    if (view.raw === 'gone' || !view.raw.body) {
      return emptyHtml('无报文原文（可能已被清理或超出保留范围）');
    }
    const html = window.wbConversationPreview?.render?.(view.raw.body.requestBody, view.raw.body.responseBody);
    if (!html) return emptyHtml('无报文原文（可能已被清理或超出保留范围）');
    return (view.raw.body.truncated ? '<div class="cv-truncated">正文超过单条上限，可能已被截断</div>' : '') + html;
  }

  // ─── 标签 ③：原始报文（调试模式的四段）───────

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

  /** 调试报文的四段（顺序即阅读顺序：请求 → 响应，头 → 体；键名是后端契约） */
  const DEBUG_SEGS = [
    { label: '请求头', pick: data => jsonText(data.requestHeaders) },
    { label: '请求体', pick: data => jsonText(data.requestBody) },
    { label: '响应头', pick: data => jsonText(data.responseHeaders) },
    // 响应体是原始文本（SSE），不是 JSON —— 不经过 jsonText 的序列化
    { label: '响应体', pick: data => (data.responseBody ? String(data.responseBody) : '') },
  ];

  /**
   * 原始报文面板：四段竖排（每段一个 pre，等宽字体 + 内部滚动 —— 原分段
   * 切换的「一块的高度」诉求由 pre 的 max-height 承接，四段之间用标题分隔）。
   * 顶部保留原来的 meta 行（URL / 提供商 / 上游状态码，来自调试报文自身）。
   */
  function rawPaneHtml(data) {
    const status = data.status === null || data.status === undefined ? '-' : String(data.status);
    const meta = [
      data.url ? `URL: ${data.url}` : '',
      data.provider ? `提供商: ${data.provider}` : '',
      `上游状态码: ${status}`,
    ].filter(Boolean);
    const blocks = DEBUG_SEGS.map(seg => {
      const text = seg.pick(data) || '';
      return `<h3>${esc(seg.label)}</h3>`
        + (text ? `<pre class="req-detail-pre">${esc(text)}</pre>` : emptyHtml('这条请求没有保存这一段报文'));
    }).join('');
    return (meta.length ? `<div class="req-detail-meta">${meta.map(esc).join('<br>')}</div>` : '') + blocks;
  }

  // ─── 渲染收口 ────────────────────────────────

  /** 重画标签条 + 各面板（open 时一次性；此后各数据到达只动自己的 pane） */
  function renderAll() {
    const body = $('req-detail-body');
    if (!body) return;
    body.innerHTML = tabsHtml()
      + paneWrap('detail', detailPaneHtml(view.row))
      + paneWrap('preview', previewPaneHtml())
      + (isPlainObject(view.debug) ? paneWrap('raw', rawPaneHtml(view.debug.data)) : '');
  }

  /** 数据到达后的局部更新：标签条重画 + 各 pane 重画（只动 innerHTML，
   *  active 类随后同步 —— 未被更新的 pane 内容不变，滚动位置得以保留） */
  function patchPane() {
    const body = $('req-detail-body');
    if (!body) return;
    body.innerHTML = tabsHtml()
      + paneWrap('detail', detailPaneHtml(view.row))
      + paneWrap('preview', previewPaneHtml())
      + (isPlainObject(view.debug) ? paneWrap('raw', rawPaneHtml(view.debug.data)) : '');
    // active 面板可能刚好是被更新的那块：确保 is-active 与 active 同步
    body.querySelectorAll('[data-detail-pane]').forEach(node => {
      node.classList.toggle('is-active', node.dataset.detailPane === active);
    });
  }

  /** 切换标签：只改 class，不发请求、不重建 DOM（滚动位置得以保留） */
  function switchTab(key) {
    if (!TABS.some(tab => tab.key === key)) return;
    // 「原始报文」标签只在调试报文已到达时存在于 DOM，点不到就不用防
    if (key === 'raw' && !isPlainObject(view?.debug)) return;
    active = key;
    const body = $('req-detail-body');
    body?.querySelectorAll('[data-detail-tab]').forEach(node => {
      const on = node.dataset.detailTab === key;
      node.classList.toggle('active', on);
      node.setAttribute('aria-selected', String(on));
    });
    body?.querySelectorAll('[data-detail-pane]').forEach(node => {
      node.classList.toggle('is-active', node.dataset.detailPane === key);
    });
  }

  // ─── open / 数据拉取 ─────────────────────────

  /** 调试报文（标签 ③）：确认有内容才亮出标签；没有（404 等）标签整体隐藏 */
  async function loadDebug(id, token) {
    try {
      const data = await api.getDebugTraffic(id);
      if (token !== seq) return;
      const hasAny = DEBUG_SEGS.some(seg => (seg.pick(data) || '').trim());
      if (!hasAny) {
        view.debug = 'gone';
      } else {
        view.debug = { data };
        if ($('req-detail-hint')) {
          $('req-detail-hint').textContent = data.truncated
            ? '报文超过单条上限，请求体 / 响应体已按上限截断'
            : '内容按原样保存，请求头中的凭据字段已替换为 [redacted]';
        }
      }
    } catch (error) {
      if (token !== seq) return;
      view.debug = 'gone';
      // 调试模式没开是最常见的原因（后端 404 的文案已说明），foot hint 提一句
      if ($('req-detail-hint')) {
        $('req-detail-hint').textContent = '在设置 → 通用里开启「调试模式」后，新发生的请求才会保存报文';
      }
      console.warn('读取调试报文失败（原始报文标签隐藏）:', error?.message || error);
    }
    patchPane();
  }

  /** 下游原文（标签 ②）：404 与失败收敛为同一个空态（原因对用户是同一件事） */
  async function loadRaw(id, token) {
    try {
      const body = await api.getStatsRequestRaw(id);
      if (token !== seq) return;
      // 形状守卫：契约是 {id, requestBody, responseBody, truncated}；
      // 不是对象或两侧全空（后端对「有行无正文」也返回空串）都算「没有原文」
      const hasText = String(body?.requestBody ?? '').trim() !== ''
        || String(body?.responseBody ?? '').trim() !== '';
      view.raw = isPlainObject(body) && hasText
        ? { body, truncated: !!body.truncated }
        : 'gone';
    } catch (error) {
      if (token !== seq) return;
      view.raw = 'gone';
      console.warn('读取原始正文失败（预览对话显示空态）:', error?.message || error);
    }
    patchPane();
  }

  /**
   * 打开某条请求的详情（列表点「详情」时调它）。
   *
   * `row` 是调用方从当前一屏数据里按 id 反查出的行对象；请求详情标签只吃它，
   * 预览对话 / 原始报文按 id 各自拉取 —— row 缺失只影响第一个标签。
   */
  async function open(id, row) {
    const modal = $('req-detail-modal');
    const body = $('req-detail-body');
    const hint = $('req-detail-hint');
    if (!modal || !body) return;
    seq += 1;
    const token = seq;
    active = DEFAULT_TAB;
    view = { id, row: isPlainObject(row) ? row : null, raw: null, debug: null };
    body.textContent = '';
    if (hint) hint.textContent = '—';
    modal.classList.add('open');
    document.addEventListener('keydown', onKeydown);
    renderAll();
    // 两个数据源并行拉，谁到了渲染谁（各自只动自己的 pane）
    void loadRaw(id, token);
    void loadDebug(id, token);
  }

  // ─── 事件绑定 ────────────────────────────────
  //
  // 委托在弹窗主体上：标签条每次 renderAll / patchPane 都会重建，绑在按钮上
  // 会随重绘失效（与列表的委托同一手法）。

  $('req-detail-close')?.addEventListener('click', close);
  $('req-detail-cancel')?.addEventListener('click', close);
  $('req-detail-modal')?.addEventListener('click', event => {
    // 点遮罩关闭（与确认弹窗同一交互）
    if (event.target === $('req-detail-modal')) close();
  });
  $('req-detail-body')?.addEventListener('click', event => {
    const item = event.target.closest('[data-detail-tab]');
    if (item) switchTab(item.dataset.detailTab);
  });

  window.wbRequestDetail = { open, close };
})();
