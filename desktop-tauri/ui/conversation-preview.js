/* Agent2API · 「预览对话」的纯渲染（请求 / 响应原文 → 对话气泡 HTML） */
/* global wbApp, wbMarkdown */

/**
 * 请求详情弹窗「预览对话」那一块的全部渲染逻辑：把后端 `GET
 * /api/stats/requests/raw` 给的两段**原文**（下游请求体、最终下发的响应）
 * 解析并还原成一轮对话。
 *
 * ── 为什么单独成文件 ──────────────────────────────────────────
 * 与 request-detail.js 按职责拆开（那个文件管弹窗与数据获取，本项目约定
 * 单文件不过 800 行）。本文件是**纯函数**：输入两个字符串、输出 HTML 字符串，
 * 不碰 DOM、不发请求、没有状态 —— 解析容错是这里最大的一块内容（SSE 逐帧
 * 拼装、多协议 content 数组、一段原文解析不了时的降级链），混在弹窗的状态
 * 机里两边都会难改。对外只暴露一个 `window.wbConversationPreview.render`。
 *
 * ── 数据形态（后端契约，见 request_stats 的 raw_body）──────────
 *   · requestBody  = 下游请求体原文（通常是 JSON 文本：messages 数组）
 *   · responseBody = 最终下发给客户端的响应：流式为 chat SSE 文本、
 *                    非流式为 chat JSON。两者都可能为空串（无原文）。
 * 预**览只吃这两段原文**：调试模式的上游报文（request-detail 的第三个标签）
 * 与这里无关，不要混用 —— 那边是上游视角，这边是客户端实际收发的内容。
 *
 * ── 解析容错（每一层失败都只降一级，不整块报错）───────────────
 *   请求：JSON 解析失败 / 没有 messages → 「无法解析请求体」+ 原文折叠；
 *   响应：以 data: 行开头 → 按 SSE 逐帧拼装；否则按 JSON 取 choices[0]；
 *         两路都解析不出内容 → 「响应原文」折叠展示；无原文 → 空态文案。
 * 参考 OmniProxy 的 ConversationPreview（流文本按 delta 累积、思考过程单独
 * 归拢、工具调用拼成代码块），但协议支持面按本项目实际收到什么写：
 * chat SSE / chat JSON / anthropic messages（工具块与 thinking 就近折叠）。
 */
(() => {
  const { esc } = wbApp;

  // ─── 小工具 ──────────────────────────────────

  /** 普通对象判断（JSON.parse 出来的可能是数组 / null / 标量） */
  function isPlainObject(value) {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
  }

  /**
   * 富文本 → HTML。优先交给 markdown.js（气泡里的助手输出常带标题 / 列表 /
   * 代码块），它从结构上不做原文透传（见那边模块头），返回值可直接进 innerHTML。
   * 渲染器缺失或抛错时退回转义纯文本 —— 少一层排版，不少内容。
   */
  function richText(text) {
    const source = String(text ?? '');
    if (!source.trim()) return '';
    try {
      const html = window.wbMarkdown?.render?.(source);
      if (typeof html === 'string' && html) return html;
    } catch {
      // 渲染器抛错不该带走整块预览，下面退回纯文本
    }
    return `<p>${esc(source)}</p>`;
  }

  /**
   * 图片地址白名单：只放行 `data:`（且必须声明为图片）与 http(s)。
   * **不做 decodeURIComponent**：属性值里的百分号编码浏览器自己不会解码，
   * 原样使用最安全；解码只会把「本来不可解释的编码」变成可被解释的字符，
   * 白白扩大攻击面（这是任务硬约束里那条「解码前先判协议」的更保守做法）。
   */
  function safeImgSrc(url) {
    const value = String(url ?? '').trim();
    return /^(data:image\/|https?:\/\/)/i.test(value) ? value : '';
  }

  /** 单张图片；地址不过白名单时返回空串（整段忽略，不渲染破图） */
  function imgHtml(url, alt) {
    const src = safeImgSrc(url);
    if (!src) return '';
    return `<img class="cv-img" src="${esc(src)}" alt="${esc(alt || '图片')}" loading="lazy">`;
  }

  /** `arguments` 常是 JSON 文本：能解析就缩进展示，解析不了原样（不猜格式） */
  function prettyJson(text) {
    const source = String(text ?? '');
    if (!source.trim()) return '';
    try {
      return JSON.stringify(JSON.parse(source), null, 2);
    } catch {
      return source;
    }
  }

  // ─── 内容段（content 的两种形态）──────────────

  /**
   * 一段内容 → { text, images, calls, results, thinking }。
   *
   * content 为字符串时就是一段文本；为数组时逐段分拣（chat 的
   * `{type:"text"|"image_url"}` 与 anthropic 的 `text` / `image` /
   * `tool_use` / `tool_result` / `thinking` 都收敛到同一组输出），
   * 未知段型忽略 —— 宁少画一块，不猜一个语义。
   */
  function collectContent(content, out) {
    if (typeof content === 'string') {
      if (content) out.text += (out.text ? '\n' : '') + content;
      return out;
    }
    if (!Array.isArray(content)) return out;
    for (const part of content) {
      if (typeof part === 'string') {
        if (part) out.text += (out.text ? '\n' : '') + part;
        continue;
      }
      if (!isPlainObject(part)) continue;
      const type = String(part.type || '');
      if (type === 'text' && typeof part.text === 'string') {
        if (part.text) out.text += (out.text ? '\n' : '') + part.text;
      } else if (type === 'image_url' || type === 'input_image') {
        // chat 形态：{image_url:{url}}；少数实现把 url 直接放 part 上
        const url = part.image_url?.url ?? part.image_url ?? part.url;
        if (url) out.images.push(String(url));
      } else if (type === 'image') {
        // anthropic 形态：{source:{type:"base64", media_type, data}}
        const source = part.source;
        if (source?.type === 'base64' && source.data && /^image\//.test(String(source.media_type || ''))) {
          out.images.push(`data:${source.media_type};base64,${source.data}`);
        } else if (source?.type === 'url' && source.url) {
          out.images.push(String(source.url));
        }
      } else if (type === 'tool_use') {
        out.calls.push({ name: String(part.name || 'tool'), args: prettyJson(JSON.stringify(part.input ?? {})) });
      } else if (type === 'tool_result') {
        const result = collectContent(part.content, { text: '', images: [], calls: [], results: [], thinking: '' });
        out.results.push({ label: part.tool_use_id ? `工具结果 · ${part.tool_use_id}` : '工具结果', text: result.text });
      } else if (type === 'thinking' && typeof part.thinking === 'string') {
        out.thinking.push(part.thinking);
      }
    }
    return out;
  }

  const newParts = () => ({ text: '', images: [], calls: [], results: [], thinking: '' });

  // ─── 请求侧：messages 数组 → 气泡 ─────────────

  /**
   * 一条请求消息 → HTML。返回空串表示这条不渲染（空内容且无工具调用）。
   * 角色映射与 OmniProxy 同口径：system / developer → 灰条；user → 右侧；
   * assistant → 左侧；tool（或带 tool_call_id 的结果）→ 折叠块；
   * 其余未知角色有内容时按 user 处理。
   */
  function messageHtml(message) {
    if (!isPlainObject(message)) return '';
    const parts = collectContent(message.content, newParts());
    // assistant 的 tool_calls：完整形态（请求侧回放的历史助手消息）
    if (Array.isArray(message.tool_calls)) {
      for (const call of message.tool_calls) {
        const fn = isPlainObject(call?.function) ? call.function : {};
        parts.calls.push({
          name: String(fn.name || call?.name || call?.type || 'tool'),
          args: prettyJson(fn.arguments ?? call?.arguments ?? ''),
        });
      }
    }
    const role = String(message.role || 'user');
    const reasoning = typeof message.reasoning_content === 'string' ? message.reasoning_content : '';

    if (role === 'system' || role === 'developer') {
      const body = richText(parts.text);
      if (!body) return '';
      return `<div class="cv-row sys"><div class="cv-msg sys md-body">${body}</div></div>`;
    }
    if (role === 'assistant') {
      const inner = assistantInner(parts, reasoning);
      if (!inner) return '';
      return `<div class="cv-row"><div class="cv-msg assistant md-body">${inner}</div></div>`;
    }
    if (role === 'tool') {
      if (!parts.text && !parts.images.length) return '';
      const label = message.tool_call_id ? `工具结果 · ${esc(String(message.tool_call_id))}` : '工具结果';
      return toolDetailsHtml(label, parts);
    }
    // user（与未知角色）：纯文本 + 图片。**不做 markdown** —— 主色底上的
    // 标题 / 代码配色会糊成一片，而用户输入本来就是纯文本居多。原文多一行
    // 空白都保留（white-space: pre-wrap 见 CSS）。
    if (!parts.text && !parts.images.length) return '';
    return `<div class="cv-row user"><div class="cv-msg user">${userInner(parts)}</div></div>`;
  }

  /** 用户气泡：文本原样（转义）+ 图片行 */
  function userInner(parts) {
    const text = parts.text ? `<div class="cv-plain">${esc(parts.text)}</div>` : '';
    const images = parts.images.map(url => imgHtml(url, '请求图片')).join('');
    return text + images;
  }

  /** 助手气泡：思考折叠 + 正文（markdown）+ 工具调用代码块 + 工具结果折叠 */
  function assistantInner(parts, reasoning) {
    return reasoningsHtml(reasoning)
      + richText(parts.text)
      + parts.calls.map(callHtml).join('')
      + parts.results.map(result => toolDetailsHtml(esc(result.label), { text: result.text, images: [], calls: [] })).join('');
  }

  /** 「思考过程」折叠块（reasoning_content / thinking 段都用它） */
  function reasoningsHtml(reasoning) {
    const text = String(reasoning ?? '').trim();
    if (!text) return '';
    return `<details class="cv-details"><summary>思考过程</summary>`
      + `<div class="cv-reasoning md-body">${richText(text)}</div></details>`;
  }

  /** 一次工具调用：标题 + 参数代码块 */
  function callHtml(call) {
    const name = String(call?.name || 'tool');
    const args = String(call?.args ?? '');
    return `<div class="cv-toolcall"><div class="cv-toolcall-name">调用工具 <code class="md-code">${esc(name)}</code></div>`
      + (args ? `<pre class="req-detail-pre cv-pre">${esc(args)}</pre>` : '') + '</div>';
  }

  /** 工具结果折叠块（label 需要是已转义文本） */
  function toolDetailsHtml(label, parts) {
    const images = (parts.images || []).map(url => imgHtml(url, '工具结果图片')).join('');
    const body = (parts.text ? `<div class="cv-plain">${esc(parts.text)}</div>` : '') + images;
    if (!body) return '';
    return `<details class="cv-tool"><summary>${label}</summary>${body}</details>`;
  }

  /**
   * 请求体原文 → 气泡 HTML 或降级块。
   * 解析成功但一条消息都没渲染出来时也给降级块：空对话没有可读性，
   * 原文折叠里至少还能看到请求里到底装了什么。
   */
  function requestHtml(raw) {
    const source = String(raw ?? '');
    if (!source.trim()) return '';
    let body = null;
    try {
      body = JSON.parse(source);
    } catch {
      body = null;
    }
    if (isPlainObject(body) && Array.isArray(body.messages)) {
      // anthropic 的顶层 system 是独立字段（不在 messages 里），补成最前面的灰条
      const blocks = [];
      if (typeof body.system === 'string' && body.system.trim()) {
        blocks.push(`<div class="cv-row sys"><div class="cv-msg sys md-body">${richText(body.system)}</div></div>`);
      } else if (Array.isArray(body.system)) {
        const system = collectContent(body.system, newParts());
        if (system.text) blocks.push(`<div class="cv-row sys"><div class="cv-msg sys md-body">${richText(system.text)}</div></div>`);
      }
      for (const message of body.messages) blocks.push(messageHtml(message));
      const filled = blocks.filter(Boolean);
      if (filled.length) return filled.join('');
    }
    // 降级：不是 JSON / 没有 messages / 消息全空 —— 说清为什么，并保留原文
    return `<div class="cv-parse-empty">无法解析请求体（不是含 messages 数组的 JSON）</div>`
      + `<details class="cv-details"><summary>请求原文</summary><pre class="req-detail-pre">${esc(source)}</pre></details>`;
  }

  // ─── 响应侧：SSE / JSON → 助手气泡 ────────────

  /** 以 data: 行开头即按 SSE 解析（任务口径；event: 行的帧也由 data 承载内容） */
  const looksSse = raw => /^\s*data:/.test(raw);

  /** SSE 分帧：帧间空行分隔，data 行可多行（按 SSE 规范用 \n 连接） */
  function sseFrames(raw) {
    const frames = [];
    for (const block of raw.split(/\r?\n\r?\n+/)) {
      const data = [];
      for (const line of block.split(/\r?\n/)) {
        if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
      }
      if (data.length) frames.push(data.join('\n'));
    }
    return frames;
  }

  /**
   * SSE 响应 → 助手消息，累积正文 / 思考 / 工具调用增量。
   * 参考 OmniProxy 对流文本的处理：逐帧 JSON.parse，取 chat 的
   * `choices[0].delta`；`[DONE]` 与解析失败的帧跳过；工具调用按 index
   * 累积（name 拼一次、arguments 字符串逐段接上）。
   * 返回 null = 一帧有效内容都没有（调用方降级为原文折叠）。
   */
  function parseSseResponse(raw) {
    let content = '';
    let reasoning = '';
    const calls = new Map();
    let errorText = '';
    for (const frame of sseFrames(raw)) {
      if (frame === '[DONE]') continue;
      let parsed = null;
      try {
        parsed = JSON.parse(frame);
      } catch {
        continue;   // 非 JSON 帧（keep-alive 等）跳过
      }
      if (!isPlainObject(parsed)) continue;
      const choice = Array.isArray(parsed.choices) ? parsed.choices[0] : null;
      const delta = isPlainObject(choice) ? choice.delta : null;
      if (isPlainObject(delta)) {
        if (typeof delta.content === 'string') content += delta.content;
        if (typeof delta.reasoning_content === 'string') reasoning += delta.reasoning_content;
        if (typeof delta.reasoning === 'string') reasoning += delta.reasoning;
        if (Array.isArray(delta.tool_calls)) {
          for (const piece of delta.tool_calls) {
            if (!isPlainObject(piece)) continue;
            // index 缺失时按「当前已见条数」归类：没有更好的键可用，
            // 而真实上游的同一次调用增量必然共用同一个 index
            const index = Number.isFinite(piece.index) ? piece.index : calls.size;
            const entry = calls.get(index) || { name: '', args: '' };
            const fn = isPlainObject(piece.function) ? piece.function : {};
            if (typeof fn.name === 'string' && fn.name) entry.name += fn.name;
            if (typeof fn.arguments === 'string') entry.args += fn.arguments;
            calls.set(index, entry);
          }
        }
      }
      // 上游错误帧（HTTP 200 之后才发生的失败只能靠帧表达）
      if (parsed.error && !errorText) {
        errorText = String(isPlainObject(parsed.error) ? parsed.error.message : parsed.error);
      }
    }
    const finalCalls = [...calls.values()].map(call => ({ name: call.name || 'tool', args: prettyJson(call.args) }));
    if (!content && !reasoning && !finalCalls.length && !errorText) return null;
    return { text: content, reasoning, calls: finalCalls, errorText };
  }

  /** 非流式 JSON 响应 → 助手消息；error 字段按「系统条」展示原因 */
  function parseJsonResponse(raw) {
    let body = null;
    try {
      body = JSON.parse(raw);
    } catch {
      return null;
    }
    if (!isPlainObject(body)) return null;
    if (body.error) {
      const message = isPlainObject(body.error) ? body.error.message : body.error;
      return { errorText: String(message ?? '上游返回错误') };
    }
    const choice = Array.isArray(body.choices) ? body.choices[0] : null;
    const message = isPlainObject(choice) ? choice.message : null;
    if (!isPlainObject(message)) return null;
    const parts = collectContent(message.content, newParts());
    if (Array.isArray(message.tool_calls)) {
      for (const call of message.tool_calls) {
        const fn = isPlainObject(call?.function) ? call.function : {};
        parts.calls.push({ name: String(fn.name || 'tool'), args: prettyJson(fn.arguments ?? '') });
      }
    }
    const reasoning = typeof message.reasoning_content === 'string' ? message.reasoning_content : '';
    if (!parts.text && !parts.images.length && !parts.calls.length && !reasoning) return null;
    return { text: parts.text, images: parts.images, reasoning, calls: parts.calls, errorText: '' };
  }

  /** 解析出的响应对象 → 气泡 HTML；内容全空时返回空串 */
  function responseResultHtml(result) {
    if (result.errorText) {
      // 错误条：把错误原因说清（错误帧是排障的高频入口，markdown 渲染器兜底转义）
      return `<div class="cv-row sys"><div class="cv-msg sys cv-error md-body">${richText(result.errorText)}</div></div>`;
    }
    const parts = {
      text: result.text || '',
      images: result.images || [],
      calls: result.calls || [],
      results: [],
    };
    if (!parts.text && !parts.images.length && !parts.calls.length) return '';
    return `<div class="cv-row"><div class="cv-msg assistant md-body">${assistantInner(parts, result.reasoning)}</div></div>`;
  }

  /** 响应原文 → 气泡 HTML（含两级降级）；无原文返回空串 */
  function responseHtml(raw) {
    const source = String(raw ?? '');
    if (!source.trim()) return '';
    const parsed = looksSse(source) ? parseSseResponse(source) : parseJsonResponse(source);
    if (parsed) {
      const html = responseResultHtml(parsed);
      if (html) return html;
    }
    // 降级：解析不出内容（或解析成功但内容全空）→ 原文折叠。
    // 这一档不是错误，是「格式没认出来，但原文还在」。
    return `<div class="cv-parse-empty">无法解析响应内容</div>`
      + `<details class="cv-details"><summary>响应原文</summary><pre class="req-detail-pre">${esc(source)}</pre></details>`;
  }

  // ─── 对外入口 ────────────────────────────────

  /**
   * 渲染整个「预览对话」。两段原文都为空 → 返回空串（调用方显示 404 空态）；
   * 只有一侧为空时给那一侧一句说明，另一侧照常渲染。
   *
   * 返回的 HTML 里所有外部文本都经过 esc / markdown 渲染器处理，
   * 可直接赋给 innerHTML。
   */
  function render(requestBody, responseBody) {
    const request = String(requestBody ?? '');
    const response = String(responseBody ?? '');
    if (!request.trim() && !response.trim()) return '';
    const blocks = [
      requestHtml(request) || (response.trim() ? '<div class="cv-parse-empty">无请求原文</div>' : ''),
      responseHtml(response) || (request.trim() ? '<div class="cv-parse-empty">无响应原文</div>' : ''),
    ];
    return `<div class="cv">${blocks.join('')}</div>`;
  }

  window.wbConversationPreview = { render };
})();
