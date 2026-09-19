/* Agent2API · 极简 Markdown 渲染器（设置页「更新日志」专用） */
/* global window */

/**
 * 把 Release 说明（Markdown 原文）渲染成可安全插入文档的 HTML 字符串。
 * 对外只暴露一个纯函数：`window.wbMarkdown.render(text) → html`。
 *
 * ── 为什么自己写而不引库 ──────────────────────────────────────
 *   1. 界面是零依赖、无构建的 IIFE 脚本（见 index.html 的脚本清单），
 *      引一个 Markdown 库要么把几十 KB 的第三方代码 vendor 进仓库，
 *      要么引入打包步骤，两者都与本项目的约定冲突；
 *   2. 更要紧的是安全边界：现成的库默认允许原始 HTML 透传，需要额外挂
 *      sanitizer 才能收口。而本页面的脚本持有 Tauri 桥接（能下载并启动安装程
 *      序、能读写本机文件），Release 说明又是外部输入 —— 一旦被注入脚本，
 *      危害远大于普通网页。这里宁可自己实现一套「只认白名单语法」的渲染器：
 *      它从结构上就不存在「原文直接变成标签」这条路径。
 *
 * ── 渲染策略：按行扫描 + 行内单遍扫描 ────────────────────────
 *   ① 整段先做一次 HTML 转义，之后所有处理都在这份**已转义文本**上进行：
 *      行内扫描器只会原样搬运已转义字符，或在外面套上自己生成的标签。
 *      因此不存在「先拼标签再整体转义」（会把生成的标签一起转义掉）或
 *      「先拼标签不转义」（原文透传）这两类错误。
 *   ② 块级按行扫描：``` 进代码块（内部整块保留，不再做行内解析），
 *      否则依次判定标题 / 分隔线 / 引用 / 列表 / 段落。
 *      不用「一条正则吃完整篇」是因为代码块里可能出现任何字符，
 *      整段正则会被它带偏，嵌套列表也会跟着出错。
 *   ③ 行内用一次从左到右的单遍扫描（一条组合正则 + 分支处理），
 *      代码 > 链接 > 强调的优先级由分支顺序保证；先把匹配结果收集成数组、
 *      再逐个渲染，避免递归调用共享正则的 lastIndex 互相踩。
 *   ④ 不认识的语法一律原样输出（此时已是转义后的安全文本）。
 *
 * 支持的语法：标题(降级到 h3~h6)、粗体、斜体、行内代码、围栏代码块、
 * 无序/有序列表(含 2~4 空格缩进嵌套)、任务列表、链接、尖括号与裸链接、
 * 引用、分隔线、段落与换行。
 * 不支持的语法（按纯文本显示，不影响安全）：表格、图片、脚注、setext 标题、
 * 原始 HTML（会被转义成可见文本）。
 */
(() => {
  'use strict';

  // ─── 转义 ─────────────────────────────────────

  const ESCAPE_MAP = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };

  /**
   * HTML 转义：本文件里唯一一处把外部字符转成实体的地方。
   * 刻意不复用 app.js 的 esc()：渲染器要求「任何调用顺序下都安全」，
   * 一次漏转义就是一个注入点，安全相关的代码不该依赖别的模块先加载。
   */
  const escapeHtml = value => String(value ?? '').replace(/[&<>"']/g, ch => ESCAPE_MAP[ch]);

  // ─── 常量 ─────────────────────────────────────

  /**
   * 行内语法组合正则：**分支顺序即优先级**（代码 > 链接 > 强调），不能随手调换 ——
   * 否则 `**x**` 会被单星号规则先咬掉一半，链接里的反引号也会被当成代码起点。
   */
  const INLINE_RE = new RegExp(
    [
      // 行内代码：起止反引号数量一致（`x` / ``x`` 都支持），内容不参与后续解析
      '(?<tick>`+)(?<code>[^\\n]*?)\\k<tick>',
      // 链接：[文字](地址)
      '\\[(?<label>[^\\]]*)\\]\\((?<href>[^)\\n]*)\\)',
      // 尖括号自动链接 <https://…>：原文的尖括号此时已被转义成 &lt; &gt;
      '&lt;(?<auto>https?://[^\\s<]+)&gt;',
      // 裸链接 https://…
      // 字符类里同时放行 `&xxx;` 实体：进块级扫描前 URL 里的 `&`、`'` 已经被转义，
      // 不放行的话带查询串的地址（`?tab=readme&page=2`）会在 `&` 处被截断。
      // 实体在属性里会还原成原字符，不会破坏属性引号 —— 结尾标点另由 trimTail 削一次
      '(?<bare>https?://(?:&[a-zA-Z#][a-zA-Z0-9]*;|[^\\s&<>"`|\\[\\]*])+)',
      '\\*\\*(?<bold>[^*\\n]+)\\*\\*',
      '__(?<boldAlt>[^_\\n]+)__',
      '~~(?<strike>[^~\\n]+)~~',
      '\\*(?<em>[^*\\n]+)\\*',
      // 下划线斜体：两侧不能紧邻字母数字，否则 snake_case 会被误判成斜体
      '(?<![A-Za-z0-9])_(?<emAlt>[^_\\n]+)_(?![A-Za-z0-9])',
    ].join('|'),
    'g'
  );

  const RE_FENCE = /^ {0,3}(`{3,}|~{3,})\s*([A-Za-z0-9_+#.-]*)\s*$/;
  const RE_HEADING = /^ {0,3}(#{1,6})\s+(.*)$/;
  const RE_HORIZONTAL = /^ {0,3}(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,})$/;
  // 引用标记：正文在进块级扫描前已经整体转义，原文的 `>` 这时是 `&gt;`。
  // 两种写法都认（转义只会把 `&` 变 `&amp;`，所以正文里出现 `&gt;` 只可能来自 `>`）
  const RE_QUOTE = /^ {0,3}(?:&gt;|>)\s?(.*)$/;
  const RE_ITEM = /^(\s*)([-*+]|\d{1,9}[.)])(\s+)(.*)$/;

  /** 强调/链接的套娃深度上限：畸形输入（如成串的 `*`）不该把调用栈吃满 */
  const MAX_INLINE_DEPTH = 4;
  /** 引用嵌套深度上限：同理，`>>>>…` 这种输入要有个终点 */
  const MAX_BLOCK_DEPTH = 4;

  // ─── 行内 ─────────────────────────────────────

  /**
   * 链接地址白名单：只放行 http(s)。
   * 入参是「已转义文本」—— 转义只会产生实体、不会凭空造出字母，
   * 所以拿它做前缀判断是可靠的，同时它本身就是合法的 HTML 属性写法。
   * `javascript:` / `data:` / `vbscript:` 等一律返回空串，由调用方降级成纯文本。
   */
  function safeUrl(raw) {
    // 地址后面可能跟 Markdown 的标题写法（`[x](url "标题")`），只取第一段
    const url = String(raw || '').trim().split(/\s+/)[0];
    return /^https?:\/\//i.test(url) ? url : '';
  }

  /** 裸链接结尾的标点通常是句子的一部分，不是地址的一部分 */
  function trimTail(url) {
    let out = url.replace(/[.,;:!?。，、；：！？]+$/, '');
    // 右括号只在「没有配对的左括号」时才削掉，避免砍坏 .../Foo_(bar) 这类地址
    while (out.endsWith(')') && out.split(')').length > out.split('(').length) {
      out = out.slice(0, -1);
    }
    return out;
  }

  /** 生成的 <a> 一律带 data-external：面板用事件委托接住，交给系统浏览器打开 */
  const anchor = (url, text) =>
    `<a href="${url}" data-external="${url}" target="_blank" rel="noopener">${text}</a>`;

  /** 单个行内匹配 → HTML */
  function inlineToken(match, depth) {
    const g = match.groups;

    if (g.tick !== undefined) return `<code class="md-code">${g.code}</code>`;

    if (g.href !== undefined) {
      const url = safeUrl(g.href);
      // 白名单不过：连方括号一起原样显示（用户看得出这里本该是个链接，但点不了）
      if (!url) return match[0];
      const label = g.label === '' ? url : inline(g.label, depth + 1);
      return anchor(url, label);
    }

    if (g.auto || g.bare) {
      const raw = g.auto || g.bare;
      const url = safeUrl(trimTail(raw));
      if (!url) return match[0];
      // 被削掉的结尾标点按纯文本补回去，否则句号会被吞进链接文字里
      return anchor(url, url) + raw.slice(trimTail(raw).length);
    }

    if (g.bold || g.boldAlt) return `<strong>${inline(g.bold ?? g.boldAlt, depth + 1)}</strong>`;
    if (g.strike) return `<del>${inline(g.strike, depth + 1)}</del>`;
    if (g.em || g.emAlt) return `<em>${inline(g.em ?? g.emAlt, depth + 1)}</em>`;

    return match[0];
  }

  /**
   * 行内渲染。入参**必须已转义**，且约定为单行（调用方逐行传入，段落内的换行
   * 由调用方拼 <br>）—— 单行内做匹配，跨行的 `*` 就不会把两行文本连成一段强调。
   */
  function inline(text, depth = 0) {
    if (depth > MAX_INLINE_DEPTH) return text;

    INLINE_RE.lastIndex = 0;
    const matches = [];
    let match = INLINE_RE.exec(text);
    // 先收完所有匹配再渲染：渲染过程会递归调用本函数，而递归会改写共享正则的
    // lastIndex —— 边扫边替换会让外层游标被内层重置，进而重复匹配甚至死循环
    while (match !== null) {
      matches.push(match);
      match = INLINE_RE.exec(text);
    }

    let out = '';
    let cursor = 0;
    for (const item of matches) {
      out += text.slice(cursor, item.index);
      out += inlineToken(item, depth);
      cursor = item.index + item[0].length;
    }
    return out + text.slice(cursor);
  }

  // ─── 列表 ─────────────────────────────────────

  /** 缩进宽度：制表符按 4 列算（GitHub 上两种缩进混用很常见） */
  const indentWidth = text => String(text || '').replace(/\t/g, '    ').length;

  /**
   * 收集列表块。返回扁平项表（缩进 + 有序/无序 + 文本行）与下一个待处理行号。
   * 缩进交给 renderListTree 建树 —— 在这里边扫边开合标签会让「子列表要落在
   * 父 <li> 里面」这件事变得很难写对。
   */
  function parseList(lines, start) {
    const items = [];
    let last = null;
    let index = start;

    while (index < lines.length) {
      const line = lines[index];
      // 空行结束列表：松列表（项之间空行）会因此拆成两个列表，视觉上只多一个间隔
      if (!line.trim()) break;

      const match = RE_ITEM.exec(line);
      if (match) {
        last = {
          indent: indentWidth(match[1]),
          ordered: /^\d/.test(match[2]),
          text: [match[4]],
        };
        items.push(last);
        index += 1;
        continue;
      }

      // 非列表行：块级起点（标题/围栏/引用…）直接结束列表，其余按「懒续行」并入当前项
      if (!last || startsBlock(line)) break;
      last.text.push(line.trim());
      index += 1;
    }

    return { html: renderListTree(items), next: index };
  }

  /**
   * 扁平项表 → 嵌套列表 HTML。
   * 缩进变深即开一层（挂在上一层最后一个 <li> 里），变浅就逐层收掉；
   * 同缩进但有序/无序标记变了，在 GitHub 上是两个并列列表，这里同样分层处理。
   */
  function renderListTree(items) {
    const roots = [];
    const stack = [];

    const closeLevel = () => {
      const level = stack.pop();
      // target 是「父层的最后一个 <li> 的子列表容器」，创建时就记下来：
      // 收尾时父层可能已经不在栈顶了，靠它才能把子列表挂回正确的位置
      if (level.target) level.target.push(level);
      else roots.push(level);
    };

    for (const item of items) {
      while (stack.length) {
        const top = stack[stack.length - 1];
        const shallower = item.indent < top.indent;
        const switched = item.indent === top.indent && item.ordered !== top.ordered;
        if (!shallower && !switched) break;
        closeLevel();
      }

      const parent = stack[stack.length - 1];
      if (!parent || item.indent > parent.indent) {
        // indent 必须存在：层级的开合全靠它比对，缺了会让所有子项都被拍平到一层
        const level = { indent: item.indent, ordered: item.ordered, items: [], target: null };
        if (parent && parent.items.length) {
          const holder = parent.items[parent.items.length - 1];
          level.target = holder.children;
        }
        stack.push(level);
      }

      stack[stack.length - 1].items.push({ text: item.text, children: [] });
    }

    while (stack.length) closeLevel();

    return roots.map(renderLevel).join('');
  }

  /** 一个层级 → <ul>/<ol>；有序列表没有 start 属性时浏览器默认从 1 开始 */
  function renderLevel(level) {
    const tag = level.ordered ? 'ol' : 'ul';
    return `<${tag} class="md-list">${level.items.map(renderItem).join('')}</${tag}>`;
  }

  /** 列表项：首行可能是任务列表标记，其余行按 <br> 接着排 */
  function renderItem(item) {
    const task = /^\[([ xX])\]\s+(.*)$/.exec(item.text[0] || '');
    let body = '';
    let className = '';

    if (task) {
      const done = task[1].toLowerCase() === 'x';
      className = ` class="md-task${done ? ' md-task-done' : ''}"`;
      // 勾选框用 span + CSS 画，不放原生 input：本项目的复选控件都藏在
      // .switch 的自绘轨道后面，裸露的原生控件在深浅两套主题下都不协调
      body = '<span class="md-task-box" aria-hidden="true"></span>' + inline(task[2]);
      body += item.text.slice(1).map(line => `<br>${inline(line)}`).join('');
    } else {
      body = item.text.map(line => inline(line)).join('<br>');
    }

    return `<li${className}>${body}${item.children.map(renderLevel).join('')}</li>`;
  }

  // ─── 块级 ─────────────────────────────────────

  /** 该行是否是另一个块级结构的起点（用于判断段落到此为止、列表是否继续） */
  function startsBlock(line) {
    if (!line.trim()) return true;
    if (RE_FENCE.test(line)) return true;
    if (RE_HEADING.test(line)) return true;
    if (RE_HORIZONTAL.test(line)) return true;
    if (RE_QUOTE.test(line)) return true;
    return RE_ITEM.test(line);
  }

  /** 围栏代码块：整块原样保留（只做转义，不解析 Markdown），未闭合时吃到文末 */
  function renderFence(lines, start) {
    const marker = lines[start].match(RE_FENCE);
    const body = [];
    let index = start + 1;

    while (index < lines.length) {
      const close = RE_FENCE.exec(lines[index]);
      // 收尾围栏：同种符号且不短于起始处（CommonMark 口径）
      if (close && close[1][0] === marker[1][0] && close[1].length >= marker[1].length) break;
      body.push(lines[index]);
      index += 1;
    }

    const lang = marker[2] ? ` class="lang-${marker[2]}"` : '';
    const html = `<pre class="md-pre"><code${lang}>${body.join('\n')}</code></pre>`;
    // index 停在收尾围栏上（或文末）：+1 跳过它，没有收尾时越界即结束
    return { html, next: index + 1 };
  }

  /** 引用块：连续 `>` 行去掉标记后按块级再解析一层（引用里可以再套列表/标题） */
  function renderQuote(lines, start, depth) {
    const inner = [];
    let index = start;
    while (index < lines.length) {
      const match = RE_QUOTE.exec(lines[index]);
      if (!match) break;
      inner.push(match[1]);
      index += 1;
    }
    const body = depth < MAX_BLOCK_DEPTH
      ? renderBlocks(inner, depth + 1)
      : `<p>${inner.map(line => inline(line)).join('<br>')}</p>`;
    return { html: `<blockquote class="md-quote">${body}</blockquote>`, next: index };
  }

  /** 逐行扫描：块级结构在文本里是「一行的开头」，所以不需要回溯解析 */
  function renderBlocks(lines, depth) {
    const out = [];
    let index = 0;

    while (index < lines.length) {
      const line = lines[index];

      if (RE_FENCE.test(line)) {
        const block = renderFence(lines, index);
        out.push(block.html);
        index = block.next;
        continue;
      }

      if (!line.trim()) {
        index += 1;
        continue;
      }

      const heading = RE_HEADING.exec(line);
      if (heading) {
        // 源文档的 h1/h2 会跟面板标题（h2）抢层级，整体下移两档；
        // 超过 h6 的一律压到 h6（HTML 里没有 h7）
        const level = Math.min(heading[1].length + 2, 6);
        const text = heading[2].replace(/\s+#+\s*$/, '');
        out.push(`<h${level} class="md-h">${inline(text)}</h${level}>`);
        index += 1;
        continue;
      }

      if (RE_HORIZONTAL.test(line)) {
        out.push('<hr class="md-hr">');
        index += 1;
        continue;
      }

      if (RE_QUOTE.test(line)) {
        const block = renderQuote(lines, index, depth);
        out.push(block.html);
        index = block.next;
        continue;
      }

      if (RE_ITEM.test(line)) {
        const block = parseList(lines, index);
        out.push(block.html);
        index = block.next;
        continue;
      }

      // 段落：连续吃到空行或下一个块级起点。段内单换行转 <br>（GitHub 在
      // Release / Issue 正文里就是硬换行口径），说明里手写的折行因此不会挤成一坨
      const paragraph = [];
      while (index < lines.length && !startsBlock(lines[index])) {
        paragraph.push(lines[index]);
        index += 1;
      }
      out.push(`<p>${paragraph.map(line => inline(line)).join('<br>')}</p>`);
    }

    return out.join('');
  }

  // ─── 对外入口 ─────────────────────────────────

  /**
   * Markdown 原文 → HTML 字符串。
   * 空输入返回空串（调用方据此显示「没有填写说明」）。返回值可以直接赋给
   * innerHTML：其中除本文件生成的标签外，不存在任何未转义的外部字符。
   */
  function render(text) {
    const source = String(text ?? '').replace(/\r\n?/g, '\n');
    if (!source.trim()) return '';
    return renderBlocks(escapeHtml(source).split('\n'), 0);
  }

  window.wbMarkdown = { render };
})();
