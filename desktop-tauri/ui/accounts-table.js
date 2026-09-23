/* Agent2API · 账号表格的行渲染（列定义 / 单元格 HTML / 行状态） */
/* global wbApp, wbAccountsModel, wbUsagePanel */

/**
 * 账号列表的**表格渲染层**：把一行账号画成 `<tr>`，把展开的明细画成紧随其后的一行。
 *
 * ── 为什么从 accounts-view.js 拆出 ──────────────────────────────
 * 账号列表从「按 provider 分卡的网格」改成「四家混排的一张表」后，单元格 HTML
 * 比卡片 HTML 更长，而 accounts-view.js 有自己的职责（筛选状态、展开态、事件
 * 委托、网络调用）。按职责切：
 *   · 本文件：**纯展示** —— 给定账号与视图侧上下文，产出 HTML 字符串；不请求、不绑事件。
 *   · accounts-view.js：有状态的那一半 —— 筛选状态、展开态、事件委托、网络调用。
 * 这样「一行长什么样」只有一处实现，重绘与局部更新不会各画一个样。
 *
 * ── 全局一条队列（优先级不再按 provider 分段）────────────────────
 * 优先级在后端是**全局唯一**的一条队列：四家账号混排，转发时按优先级从小到大
 * 逐个尝试，跳过禁用 / 不支持该模型 / 该模型限流中的账号（见 priority.rs 与
 * rotate.rs 的模块头）。所以本表按优先级升序排行，第一列给出全局序号 #N；
 * ↑/↓ 与全局相邻账号交换，「设为首选」只把账号移到全局队列第一位，
 * 不改变启用状态，也不表示请求正在使用该账号（这一项住在 ⋯ 菜单里，
 * 不在行上 —— 见 actionsCell 与 accounts-model.js 的 moreMenuHtml）。
 *
 * 同理，优先级输入框**不做冲突判定**：冲突只有后端一处判（全局唯一，409 带占位者
 * 姓名），前端拦下来只会出现「界面放过、后端拒绝」或反过来的分歧。
 * 前端负责把 409 翻译成可读提示并把显示值还原。
 */

(() => {
  const { esc, formatTime } = wbApp;
  const {
    providerOf,
    providerFeatures,
    identifierOf,
    tokenExpiryOf,
    isEnabled,
    isDesktopAccount,
    supportsUsage,
    supportsCheckin,
    checkedInToday,
    accountTags,
    editionSuffix,
    formatResetText,
    activeLimits,
    limitPanelHtml,
  } = wbAccountsModel;

  /** 优先级号段（与后端 priority.rs 的 MIN/MAX/DEFAULT 逐字一致）。
   *  放在本文件是因为输入框的 min/max 属性与「归一后没变就不发请求」的判定同源。 */
  const PRIORITY_MIN = 0;
  const PRIORITY_MAX = 9999;
  const PRIORITY_DEFAULT = 100;

  /**
   * 账号名隐藏开关（表头「账号」旁边那颗眼睛）：打开后账号列的名字整体显示成
   * 星号，截图 / 演示时不必逐个打码。这是「怎么看这张表」的偏好，与列宽同一档，
   * 存 localStorage 跨次启动保留。
   */
  const NAMES_HIDDEN_KEY = 'workbuddy-desktop-accounts-names-hidden';
  let namesHidden = localStorage.getItem(NAMES_HIDDEN_KEY) === '1';

  /** 切换隐藏状态并落盘，返回切换后的值（点击处理用它触发整表重绘） */
  function toggleNamesHidden() {
    namesHidden = !namesHidden;
    localStorage.setItem(NAMES_HIDDEN_KEY, namesHidden ? '1' : '0');
    return namesHidden;
  }

  /**
   * 星号掩码：按原名长度生成、4~12 个封顶 —— 完全不保长会把账号列压成短短一截，
   * 与关闭时版式差得太多；封顶 12 则不让超长名字把星号串拉到撑破列宽。
   */
  const maskName = name => '*'.repeat(Math.min(Math.max(name.length, 4), 12));

  const clampPriority = value => Math.min(PRIORITY_MAX, Math.max(PRIORITY_MIN, Math.round(value)));
  const priorityOf = account => {
    const value = Number(account?.priority);
    return Number.isFinite(value) ? value : PRIORITY_DEFAULT;
  };

  // ─── 列定义（表头、colgroup 与 colspan 的唯一来源）─────

  /**
   * 列：勾选 / 优先级 / 提供商 / 账号 / 连接数 / 状态 / 限流 / 有效期 / 余额 · 积分 / 操作。
   *
   * 优先级是整张表的主线（全局队列），所以放在提供商之前、紧跟勾选列；
   * `key` 同时是 CSS 类名后缀（`cell-<key>`），默认列宽在 page-accounts-table.css
   * 里按这些类名声明 —— 用户拖宽后由 accounts-columns.js 按同一批 key 存取，
   * 键名只有这一处定义。
   *
   * 连接数紧跟账号列：它回答的是「这个账号此刻有几个请求在跑」，属于**账号的身份**
   * 而非健康状态 —— 放在状态列之前，与状态列（可用性）分工清楚。
   *
   * `hint` 是表头里那小字副标题（如「全局队列」），`title` 是悬停说明。
   * 两者都写在这里而不是散在 headRowHtml 的三元表达式里：列一多，
   * 那种链式判定就要为每列各加一层，改一处得先读懂整串。
   *
   * `render` 是该列**内容**的渲染函数（不含 `<td>` 外壳），签名统一为
   * `(account, ctx)`：本表从「按固定顺序拼单元格」改成「按列定义逐列渲染」，
   * 是为了让「列设置」里的显示 / 隐藏与顺序调整对数据行同样生效 ——
   * 否则藏起来的列仍在行里占位，表格与表头对不上。
   * 只有勾选列没有 render：它的内容要用行上下文里的 `picked`，单独处理。
   * `align` 是该列在「列设置」里的**默认**对齐（用户可以逐列改，见
   * table-col-settings.js）。
   *
   * ── 默认对齐：操作列居右，其余全部居中（本次改造）──────────────
   * 这两档各有一个理由，不是随手配的：
   *   · **操作列居右**：它是行尾的一组按钮，贴住表格右缘时整列有一条整齐的
   *     竖线（`.acct-actions` 的 `justify-content: flex-end` 与它同源）——
   *     扫视时不必逐行去找「按钮从哪儿开始」，每行的 ⋯ 都在同一条线上。
   *   · **其余列居中**：这些格子里装的是徽章、开关、序号、读数这类**等宽或很短**
   *     的内容，居中之后同一列的各行对齐到一条中轴，比左对齐更好扫读；
   *     账号列（唯一伸缩的一列）也跟着居中，整张表的中轴才是齐的。
   * 用户改过的对齐存在 localStorage（按列 key）。默认值这次**变了**，所以
   * 旧存盘（v1 裸数组，没有默认值快照）靠下面的 `legacyAlign` 辨认：
   * 值等于上一版默认的列改判成新默认，其余（= 用户真挑过的）原样保留。
   * 新存盘带默认值快照（v2），将来再改默认值时不必再写 legacyAlign。
   * 完整判定见 table-col-settings.js 的 normalize。
   */
  const COLUMNS = [
    // 勾选列的 label 给「选择」而不是空串：它在表格里确实没有表头文案（那一格是
    // 「全选」复选框），但**列设置面板里必须有个名字** —— 面板按 label 显示，
    // 空串会退化成原始 key「pick」，用户看不懂这是哪一列。
    // legacyAlign 只写在「上一版默认与新版不同」的列上（上一版只有操作列声明过
    // align: 'right'，其余都是隐式的左对齐）—— 写全一份没有信息量，
    // 反而会让「哪几列的对齐这次变了」看不出来。
    { key: 'pick', label: '选择', align: 'center', legacyAlign: 'left' },
    {
      key: 'priority', label: '优先级', hint: '全局队列',
      title: '全局一条队列：数值越小越先用，不分提供商',
      align: 'center', legacyAlign: 'left',
      render: priorityCell,
    },
    // providerCell 原本的入参是 (provider, account)：这里就地适配成统一的
    // (account, ctx)，免得为它一个人破例 —— 破例一次，后面每加一列都要先
    // 去确认「这一列的入参是哪个顺序」。
    {
      key: 'provider', label: '提供商', align: 'center', legacyAlign: 'left',
      render: (account, ctx) => providerCell(providerOf(account), account, ctx),
    },
    { key: 'account', label: '账号', align: 'center', legacyAlign: 'left', render: accountCell },
    {
      // 代理（本次新增，列形态参考 OmniProxy 的「代理」列）：这个账号出网走
      // 哪条线路。它原先挤在账号列第二行，抽出来之后扫这一列就能回答
      // 「哪几个账号在走代理、是不是同一个出口」。
      // 默认位置在账号与连接数之间（列设置里可拖走）—— 紧挨着账号列，
      // 因为它读起来是账号的**属性**，与后面的运行时读数（连接数 / 状态 /
      // 限流）不是一类。
      // 这一列上一版不存在，因此没有「旧默认对齐」这回事，不给 legacyAlign。
      key: 'proxy', label: '代理',
      title: '该账号出网走的代理（Clash 出口 / 自定义 / 直连）；点击可修改',
      align: 'center',
      render: proxyCell,
    },
    {
      // 不加 hint 小字：这一列只有 56px，「连接数」三个字加副标题会撑破表头。
      // 口径说明放在 title 里（悬停可见）。
      // 这一列上一版就是居中（CSS 里写死了 th/td 一起居中），旧默认 = 新默认，
      // 所以不给 legacyAlign。
      key: 'connections', label: '连接数',
      title: '此刻正在使用这个账号的请求数（含还在下发内容的流式请求）；为 0 时不显示',
      align: 'center',
      render: connectionsCell,
    },
    { key: 'status', label: '状态', align: 'center', legacyAlign: 'left', render: statusCell },
    {
      key: 'limits', label: '限流', hint: '按模型',
      title: '该账号当前限流中的模型；点徽章看明细',
      align: 'center', legacyAlign: 'left',
      render: limitsCell,
    },
    { key: 'expiry', label: '有效期', align: 'center', legacyAlign: 'left', render: expiryCell },
    // 「余额 / 积分」改成「余额」（本次改造）：这一列现在只放**读数**，
    // 那颗查询按钮已移到操作列（见 usageCell 与 actionsCell）——
    // 一个只显示余额数字的列叫「余额 / 积分」会让人以为这里还能点。
    // 而「余额」这个词也容得下各家的不同叫法（WorkBuddy 是积分、
    // 小浣熊是积分、AutoClaw 是余额），不必在表头枚举。
    { key: 'usage', label: '余额', align: 'center', legacyAlign: 'left', render: usageCell },
    // 操作列上一版就是右对齐，旧默认 = 新默认，不给 legacyAlign
    { key: 'actions', label: '操作', align: 'right', render: actionsCell },
  ];

  /**
   * 「列设置」登记的列定义（顺序、表头文案、默认对齐）。
   *
   * `apply` 是唯一入口：传入 COLUMNS 就拿到「用户配置的顺序 + 只保留可见列，
   * 每项带 align」的新数组。表头、colgroup、数据行三处都走它，所以三者的
   * 列集合与顺序天然一致 —— 不必各自再判一遍显隐（那正是最容易漂移的地方）。
   *
   * 投给列设置的项带上 `legacyAlign`（有的话）：它是**上一版的默认对齐**，
   * 只被 table-col-settings.js 用来分辨旧存盘里那一档是「用户挑的」还是
   * 「旧默认值」—— 少了它，这次改的默认对齐对老用户就不生效。见那边的 normalize。
   */
  const colSettings = window.wbColSettings?.register({
    id: 'accounts',
    label: '账号表',
    columns: COLUMNS.map(column => ({
      key: column.key,
      label: column.label,
      align: column.align,
      legacyAlign: column.legacyAlign,
    })),
    // 挂载点是批量栏右侧的操作组（「批量操作 / 取消选择」那两颗）：齿轮插在最前，
    // 正好落在「批量操作」左边。放在这里而不是工具条右侧的操作组，是因为工具条
    // 那组已经被「查询余额 / 全部签到 / 添加账号」占满，齿轮挤在它们前面时
    // 会与三段筛选抢同一行的右端；批量栏这组按钮本就偏「对当前这张表做什么」，
    // 列设置（怎么看这张表）排头更顺。
    mount: () => document.querySelector('#batch-bar .batch-actions'),
    onChange: () => window.wbAccountsView?.render?.(),
  });

  const visibleColumns = () => (colSettings ? colSettings.apply(COLUMNS) : COLUMNS);

  /** 表头行：优先级 / 限流 / 连接数列带口径说明；勾选列放「全选当前筛选结果」的第二个入口。
   *  每个可拖列的右缘放一枚把手（accounts-columns.js 委托 mousedown / dblclick）。
   *  列集合与顺序由 `visibleColumns()` 给（见 colSettings 的说明）。 */
  function headRowHtml() {
    const columns = visibleColumns();
    const cells = columns.map((column, index) => {
      const hint = column.hint ? `<span class="th-hint">${esc(column.hint)}</span>` : '';
      // 账号列的「隐藏账号名」眼睛：点一下名字整列变星号（处理在 accounts-view.js
      // 的委托里，与本表「纯展示、不绑事件」的分工一致）。图标随状态换睁眼 / 闭眼，
      // 隐藏生效时常亮主色 —— 「现在处于打码状态」不用悬停就能看出来。
      const eye = column.key === 'account'
        ? `<button class="name-eye${namesHidden ? ' on' : ''}" data-action="toggle-names"`
          + ` title="${namesHidden ? '显示账号名' : '隐藏账号名（名字显示为星号）'}"`
          + ` aria-label="${namesHidden ? '显示账号名' : '隐藏账号名'}"`
          + ` aria-pressed="${namesHidden}">${window.wbIcons?.icon?.(namesHidden ? 'eyeOff' : 'eye', 13) || ''}</button>`
        : '';
      // 把手不放勾选列（47px 宽，把手会压住复选框），也不放最后一列 ——
      // 它绝对定位在右缘（right: -4px），钉在表格右缘会顶出一条横向滚动条
      // （见模块头）。「哪一列在最后」是用户配置出来的，所以按渲染后的位置判。
      const grip = column.key === 'pick' || index === columns.length - 1
        ? ''
        : '<span class="col-grip" title="拖动调整列宽（双击还原）"></span>';
      if (column.key === 'pick') {
        return `<th class="cell-${column.key} ta-${column.align}" data-col="${esc(column.key)}">`
          + '<input type="checkbox" id="acct-select-all"'
          + ` title="全选 / 取消全选当前筛选结果（与批量栏同一个选择）"></th>`;
      }
      // data-col 是列的身份：列设置能换顺序，列宽（accounts-columns.js 的
      // columnAt）与它自己的 <col> 都靠这个属性认列，不再靠「第几个」。
      return `<th class="cell-${column.key} ta-${column.align}" data-col="${esc(column.key)}">`
        + `<span class="th-label"${column.title ? ` title="${esc(column.title)}"` : ''}>${esc(column.label)}${hint}${eye}</span>${grip}</th>`;
    }).join('');
    return `<thead><tr>${cells}</tr></thead>`;
  }

  /** 列宽骨架：默认宽度来自 CSS（.cell-* 类），用户拖过的列由 style 覆盖 */
  function colGroupHtml(widths) {
    const cols = visibleColumns().map(column =>
      `<col data-col="${esc(column.key)}"${widths?.[column.key] ? ` style="width:${Number(widths[column.key])}px"` : ''}>`).join('');
    return `<colgroup>${cols}</colgroup>`;
  }

  /** 整张表：列宽骨架 + 表头 + 行。rowsHtml 由调用方按顺序拼好（全局优先级升序） */
  function tableHtml(rowsHtml, widths) {
    return `<table class="acct-table">${colGroupHtml(widths)}${headRowHtml()}<tbody>${rowsHtml}</tbody></table>`;
  }

  // ─── 单元格 ────────────────────────────────

  const pickCell = (account, picked) => `<td class="cell-pick"><input type="checkbox"`
    + ` data-pick="${esc(account.id)}"${picked ? ' checked' : ''} title="勾选后可批量操作"></td>`;

  /**
   * 优先级：全局序号 + 「↓ 数字 ↑」合并控件。
   *
   * 序号（#N）回答「第几位」，控件里的数字回答「队列值」—— 两者是同一个事实的
   * 两种读法：序号便于扫读（「我的账号在第 3 位」），数值便于精确定位与手工对齐。
   * 为什么数字一直可编辑而不是「双击才变输入框」：这一列的主用途就是改顺序，
   * 双击先要用户发现「这里能双击」；而两枚箭头已经覆盖了最常用的「挪一位」，
   * 剩下的「改成一个具体数字」交给一个本身就长得像输入框的控件最直接。
   * 保存时机是**失焦 / 回车**（回车即触发一次失焦），不是在 input 事件里 ——
   * 每敲一位就发一次请求会让「改成 250」变成三次 PATCH，中间还会撞上冲突。
   *
   * ── 为什么把「数字框 + 两枚箭头」合成一个控件（本次改造）──────
   * 改造前是三块并排的独立控件：一个 44px 的数字框 + 一组「↑ ↓」（间距 3px）。
   * 三块拼在一起时，箭头组与输入框之间没有任何视觉关联，看着像
   * 「一个输入框 + 两个无关按钮」；而它们实际上是**同一个东西**——
   * 都是「改这个值」的入口，只是一个给增量、一个给绝对值。
   * 合成一个控件（共享外框与圆角，内部用分隔线断开）之后，那层关系在
   * 视觉上直接成立。
   *
   * ── 方向与语义的对应（**极易搞反，改动时先读这里**）──────────
   * 优先级是**数值越小越先用**（全局队列一条，见 accounts-view.js 的模块头），
   * 所以：
   *   · 上箭头（↑）= 与队列里的**上一个**账号交换 = 排得更靠前 = **优先级数值变小**
   *   · 下箭头（↓）= 与队列里的**下一个**账号交换 = 排得更靠后 = **优先级数值变大**
   * 这两个方向来自后端 `move_account(id, "up" | "down")` 的语义：
   * `down` 找 `index + 1`（后一个），`up` 找 `index - 1`（前一个），见
   * `core::account_store::store_crud::move_account`。
   * **视觉顺序也是这个含义**：控件里上箭头在**上方/右侧靠↑**、下箭头在下方，
   * 与「值往哪个方向走」一致 —— 写成 `↓ 数字 ↑` 而不是 `↑ 数字 ↓`，
   * 是因为竖排方向上「上」在心理模型里对应「往前排」。
   *
   * 按钮的 `data-action` 保持 `move-up` / `move-down` 不变：事件处理在
   * accounts-view.js，改名会让那边的分支静默失效（它按字符串匹配）。
   */
  function priorityCell(account, ctx) {
    const value = ctx.draft ?? priorityOf(account);
    const title = '全局唯一：所有提供商的账号都不能重号，数值越小越先用';
    const seat = ctx.seat || { position: 1, total: 1 };
    // 拼串时不留多余缩进空白：td 是块级上下文，模板里的换行与缩进会原样进入
    // 文本节点，把控件挤开。所有片段都紧凑地贴在标签上。
    //
    // 控件内部的顺序：↓ / 数字框 / ↑。
    //   · 下箭头在左、上箭头在右，中间夹着数字框 —— 与「左降右升」的
    //     横排直觉一致（左低右高），也让两枚按钮贴着输入框的两侧，
    //     视觉上是一组而不是三块。
    //
    // ── 箭头为什么是 SVG 而不是 ↓ / ↑ 字符（本次修复）──────────────
    // 那两个字是字体字形，墨迹在行盒里天生偏下（Segoe UI 下 ascent=7 /
    // descent=0，整个字形贴在基线之上），而同一行的数字是 ascent=8 ——
    // 两者都靠 flex 把行盒居中，于是箭头视觉重心比几何中心低约 0.5px，
    // 用户实测能看到「有点靠下」。SVG 的箭头在 24 画布内上下对称
    // （顶点 5 / 底点 19），盒居中即墨迹居中，与字体无关。
    // 图标本体与理由写在 icons.js 的 arrowDown / arrowUp。
    const arrow = (name, size) => window.wbIcons?.icon?.(name, size) || '';
    return `<td class="cell-priority"><div class="prio">`
      + `<span class="seat" title="全局队列第 ${seat.position} 位，共 ${seat.total} 位">#${seat.position}</span>`
      + `<span class="prio-stepper">`
      + `<button class="prio-arrow" data-action="move-down" data-id="${esc(account.id)}"`
      + ` title="与队列里的下一个账号交换优先级（可能是另一家的账号）"${seat.position >= seat.total ? ' disabled' : ''}>${arrow('arrowDown', 14)}</button>`
      + `<input class="prio-input" type="number" data-prio="${esc(account.id)}"`
      + ` min="${PRIORITY_MIN}" max="${PRIORITY_MAX}" step="1" value="${esc(String(value))}"`
      + ` aria-label="优先级" title="${esc(title)}">`
      + `<button class="prio-arrow" data-action="move-up" data-id="${esc(account.id)}"`
      + ` title="与队列里的上一个账号交换优先级（可能是另一家的账号）"${seat.position <= 1 ? ' disabled' : ''}>${arrow('arrowUp', 14)}</button>`
      + `</span></div></td>`;
  }

  /**
   * 提供商：一枚徽章，带版本后缀（「WorkBuddy 国际版」）——与 AutoClaw 那种
   * 「名字自带版本」的家同一种形态，不再提供商、版本两枚并排。列宽不够时
   * 整块折行（flex-wrap，见 .cell-provider .pv 的样式说明）。
   * 徽章的配色按 provider id 生成（`p-<id>` 类），未登记的家在 CSS 里落到中性兜底
   * —— 加一家时不必改样式表，也不会显示成空白。
   */
  function providerCell(provider, account) {
    const label = window.wbProviders?.labelOf?.(provider) || provider;
    const edition = providerFeatures(provider).edition ? editionSuffix(account) : '';
    const text = edition ? `${label} ${edition}` : label;
    return `<td class="cell-provider"><div class="pv">`
      + `<span class="pbadge p-${esc(provider)}" title="提供商：${esc(text)}">${esc(text)}</span>`
      + `</div></td>`;
  }

  /**
   * 账号：第一行名称，第二行「桌面端」标记，第三行只在异常时出现
   * （代理不可用原因 / 账号不可用）。
   *
   * 标识（UID / userId）与 Token 尾号**不再上屏**：它们对「这条账号能不能用」没有
   * 信息量，却把副标题占掉大半 —— 同一屏里账号名和状态才是要一眼扫到的东西。
   * 标识仍留在账号名的悬停提示里（要核对串号时鼠标一停就能看到），
   * 原始字段也照旧由接口返回，需要时随时能查。
   *
   * 把健康说明放这一列而不是「状态」列：状态列只有几十像素，放不下必须读全的文案；
   * 账号列是唯一随窗口与列宽变化伸缩的一列，长文案在这里才读得到。
   * 限流的恢复时间不再出现在这里 —— 限额按模型记，它有自己的列（见 limitsCell）。
   *
   * ── 代理那一段本次搬走了 ────────────────────────────────────
   * 它原先在这里的第二行（「桌面端 · 代理 Clash 混合端口 7890」），现在有独立的
   * 代理列（见 proxyCell）。两处显示同一个事实只会让人怀疑它们会不会不一致，
   * 而代理是**线路**（决定请求从哪出去、出问题先看哪），与「这条账号是不是
   * 桌面端登录态」不是同一类信息。第二行因此只剩桌面端标记，
   * 普通账号（非桌面端、无代理）的副标识行整个不渲染（见 sub 的判空）。
   */
  function accountCell(account) {
    const ident = identifierOf(account);
    const features = providerFeatures(providerOf(account));
    const name = account.nickname || account.name || ident || '未命名账号';
    const title = [
      ident ? `${features.identifier} ${ident}` : '',
      account.tokenTail ? `Token 尾号 ${account.tokenTail}` : '',
      account.updatedAt ? `更新于 ${formatTime(account.updatedAt)}` : '',
      account.source ? `来源 ${account.source === 'imported' ? '旧数据导入' : '手动添加'}` : '',
    ].filter(Boolean).join('；');

    const desktop = isDesktopAccount(account)
      ? '<span class="badge desktop-tag" title="桌面端实时登录态：凭证每次从客户端登录态文件读取">桌面端</span>'
      : '';
    // 明细行为空时整行不渲染：一个空的 .acct-sub 仍占一行行高（margin + line-height），
    // 在没有任何副标识的账号上会白留一道空隙，而它恰恰是「这行没什么可说的」那种账号
    const note = healthNote(account);
    // 隐藏开关打开时名字整体变星号（掩码按原名取长，见 maskName 的说明）；
    // 悬停提示里本来就没有名字（只有标识 / Token 尾号），照常保留
    const shown = namesHidden ? maskName(name) : name;
    return `<td class="cell-account"><div class="acct-name"${title ? ` title="${esc(title)}"` : ''}>`
      + `<span class="name">${esc(shown)}</span></div>`
      + (desktop ? `<div class="acct-sub">${desktop}</div>` : '')
      + note
      + '</td>';
  }

  /**
   * 代理：这个账号出网走哪条线路（列形态参考 OmniProxy 的「代理」列 ——
   * 一格一件事，扫一眼就知道走的是哪个出口）。
   *
   * ── 三种形态 ────────────────────────────────────────────────
   *   · 未配置 → 「直连」（中性色）。空着会被当成渲染缺失，而写「无」不像状态；
   *     「直连」是准确的说法 —— 它就是不走代理。
   *   · 已配置 → 后端给的展示名 `proxy.label`（Clash 是「节点名（:7890）」或
   *     「Clash 混合端口 7890」，自定义是「http://host:port」；换算在
   *     `core::proxies::describe_account_proxy`，前端不自己拼 —— 两处拼法迟早漂）。
   *   · 解析失败 → 「解析失败」红字，完整原因进 title（账号列的异常说明里
   *     还有一份更显眼的，两处都指向「去设置里改」）。
   *
   * ── 为什么整格是一个按钮，而不是像 OmniProxy 那样的行内下拉 ──────
   * OmniProxy 的代理是**独立实体**（有 id / name / protocol / host / port），
   * 所以一个下拉就能换。本项目的代理是**账号内嵌的配置**，三种形态里
   * 「自定义」（协议 + host + port + 用户名密码）根本表达不进一个下拉，
   * 而 Clash 出口列表还要异步读 Clash Verge 的配置（见 proxy-form.js 的
   * `loadClashOptions`）。行内下拉只能覆盖「直连 / Clash 出口」两态，
   * 第三种仍要开弹窗 —— 与其做一半、让用户猜「为什么这里改不了自定义」，
   * 不如让整格都是「去改它」的入口：点开的就是那个完整的代理表单
   * （`data-action="settings"`，与操作列那颗「设置」走同一条链，
   * 处理在 app.js 的 runAccountAction）。
   */
  function proxyCell(account) {
    const button = (label, kind, title) =>
      `<button class="proxy-cell ${kind}" data-action="settings" data-id="${esc(account.id)}"`
      + ` title="${esc(title)}">${esc(label)}</button>`;
    const proxy = account.proxy;
    if (!proxy) {
      return `<td class="cell-proxy">${button('直连', 'none', '该账号直连上游，未配置出网代理；点击可设置')}</td>`;
    }
    if (proxy.error) {
      return `<td class="cell-proxy">${button(
        '解析失败',
        'bad',
        `代理不可用：${proxy.error}（转发时会回退直连）；点击可修改`,
      )}</td>`;
    }
    const label = proxy.label || '已设置';
    const from = proxy.source === 'clash' ? 'Clash Verge 出口' : '自定义代理';
    return `<td class="cell-proxy">${button(label, 'on', `${from}：${label}；点击可修改`)}</td>`;
  }

  /**
   * 异常说明（第三行）。只覆盖「不随请求变化」的故障（代理 / 不可用）；
   * 限流是按模型的、会自动解除，它的展示与操作都在「限流」列里。
   * 没有异常时返回空串 —— 不能给正常账号留一行占位，那一行的高度会白送给整张表。
   */
  function healthNote(account) {
    const notes = [];
    if (account.proxy?.error) notes.push(`代理不可用：${account.proxy.error}`);
    if (account.available === false) notes.push(account.reason || '账号当前不可用');
    if (!notes.length) return '';
    return `<div class="acct-note bad" title="${esc(notes.join('；'))}">${esc(notes.join('；'))}</div>`;
  }

  /**
   * 连接数格子的**内容**（不含 td 外壳）：0 / 缺失都渲染成空串。
   *
   * 拆出来是为了就地更新：2 秒一次的轮询只改这一格的 innerHTML
   * （见 accounts-view.js 的 syncConnections），不重绘整张表 ——
   * 整表重绘会打断正在编辑的优先级输入框、也会把用户展开的明细行重排。
   */
  function connectionsHtml(count) {
    const value = Number(count) || 0;
    if (value <= 0) return '';
    const title = `${value} 个请求正在使用该账号（含还在下发内容的流式请求）`;
    return `<span class="conn-count" title="${esc(title)}">${value}</span>`;
  }

  /**
   * 连接数：此刻正在使用这个账号的请求数（`ctx.connections`，由 accounts-view
   * 从 `/api/accounts/connections` 拉的实时计数）。
   *
   * 口径与 OmniProxy 上游管理页的「连接」列一致 —— 有连接时显示数字、为 0 时
   * **什么都不显示**（留空）。为什么空着而不是显示 0：这一列绝大多数时间都是空的，
   * 满屏的 0 会把少数几个真正在跑的账号淹没；要看「谁是 0」时空白本身就是答案。
   *
   * 计数缺失（还没拉到、后端不可达）与 0 同样处理 —— 都渲染成空。
   * 这条取舍是刻意的：把一个尚未知的值渲染成 0 会读成「这个账号没在用」，
   * 而事实可能是「数据还没到」。
   *
   * 数字带 title 说明，因为「连接数」这个词在本项目里没有别的用法，
   * 不看说明容易误解成 TCP 连接数。
   */
  function connectionsCell(account, ctx) {
    return `<td class="cell-connections">${connectionsHtml(ctx.connections)}</td>`;
  }

  /**
   * 状态：启用 / 禁用开关（+ 需要留意时的健康徽章）。
   *
   * 开关直接落 `PATCH { enabled }`（与「⋯」菜单里的启用/禁用是同一条链，语义一致），
   * 不做二次确认 —— 这个动作可逆，且关掉后账号记录仍在列表里（不是删除）。
   * 徽章沿用 accounts-model 的 accountTags：代理异常 / 不可用 / 仅账号管理；
   * 一切正常时它返回空串，这里就**不渲染徽章那一行** —— 启用状态由开关的
   * 轨道位置与滑块表达，再补一枚「启用」是同一格里的第二次说明。
   */
  function statusCell(account) {
    const enabled = isEnabled(account);
    const tags = accountTags(account);
    // 开关没有可见文字（这一列很窄），所以必须有 aria-label：
    // 冒号后面补的是账号名，读屏时能听出「启用 <账号名>」而不是孤零零一个「复选框」
    const who = account.nickname || account.name || identifierOf(account) || account.id;
    // 压成一行输出（td 内是块级上下文，模板里的缩进会原样变成文本节点，
    // 在开关与轨道之间多出一道空隙）：label 是 inline-flex
    return `<td class="cell-status">`
      + `<label class="switch" title="${enabled ? '已启用，点击禁用（不参与转发）' : '已禁用，点击启用'}">`
      + `<input type="checkbox" data-toggle="${esc(account.id)}"${enabled ? ' checked' : ''}`
      + ` aria-label="${enabled ? '禁用' : '启用'}${esc(who)}">`
      + `<span class="track"></span></label>`
      + (tags ? `<div class="status-tags">${tags}</div>` : '')
      + '</td>';
  }

  /**
   * 限流：这个账号**当前限流中的模型**。
   *
   * 限额在后端是按「账号 × 模型」记的（`rateLimits[model]`，见 store_admin），四家
   * 通用 —— 所以这一列对四家都成立，不再需要「选个模型看队列」的筛选器。
   *   - 有限流：可点的黄色徽章「N 个模型 ▾」，点开/收起行下的明细面板
   *     （accounts-model 的 limitPanelHtml：模型 / 恢复时间 / 上游原因 / 清除标记）；
   *   - 正常：绿点「正常」（不可点，没有可展开的东西）。
   * 徽章下的小字给出「最早恢复」，扫一眼就知道还要等多久；完整信息在面板里。
   */
  function limitsCell(account, ctx) {
    const entries = activeLimits(account);
    if (!entries.length) {
      return `<td class="cell-limits"><span class="lim-none" title="当前没有任何模型处于限流中"><i></i>正常</span></td>`;
    }
    const soonest = formatResetText(entries[0].resetAt);
    const open = ctx.limitsOpen === true;
    return `<td class="cell-limits">`
      + `<button class="lim${open ? ' open' : ''}" data-action="limits" data-id="${esc(account.id)}"`
      + ` title="点击${open ? '收起' : '查看'}各模型的限流明细">`
      + `${entries.length} 个模型<span class="caret">▾</span></button>`
      + `<span class="lim-sub" title="最早恢复">最早 ${esc(soonest === '已限流' ? '待定' : soonest)} 恢复</span></td>`;
  }

  /**
   * 有效期：按「这家有没有版本概念」选字段（workbuddy 是 expiresAt，
   * 三家是 tokenExpiresAt），与 accounts-model 的 tokenExpiryOf 同口径。
   * 文案收短成「30 天后」，完整句留在 title —— 列宽有限。
   */
  function expiryCell(account) {
    const features = providerFeatures(providerOf(account));
    const expiresAt = Number(features.edition ? account.expiresAt : tokenExpiryOf(account)) || 0;
    if (!expiresAt) return '<td class="cell-expiry"><span class="muted" title="记录里没有过期时间">—</span></td>';
    const left = expiresAt - Date.now();
    if (left <= 0) return '<td class="cell-expiry"><span class="badge bad" title="凭证已过期，转发时会先刷新">已过期</span></td>';
    const text = left < 3600e3 ? `${Math.max(1, Math.round(left / 60e3))} 分钟后`
      : left < 48 * 3600e3 ? `${(left / 3600e3).toFixed(1)} 小时后`
        : `${Math.floor(left / 24 / 3600e3)} 天后`;
    // 完整时间点只在解析得出时补进 title：formatTime 对非法时间戳返回空串，
    // 直接拼会留下一个空的「（）」
    const full = formatTime(expiresAt);
    const title = full ? `${text}过期（${full}）` : `${text}过期`;
    return `<td class="cell-expiry"><span title="${esc(title)}">${esc(text)}</span></td>`;
  }

  /** 数值 → 展示串（与 usage-panel.js 的同名私有函数同口径，取不到给「—」） */
  function numberText(value) {
    if (value === null || value === undefined || value === '') return '—';
    const number = Number(value);
    return Number.isFinite(number) ? String(number) : String(value);
  }

  /**
   * 余额结果 → 一行摘要（`{text, kind, title}`）。
   *
   * 形状探测沿用 usage-panel.js 的判据（`totalLeft` 键 = workbuddy 既有形状，
   * 否则看 `available` / `wallets`），**不按 provider 猜** —— provider 只决定
   * 「谁去查」，不决定「查回来长什么样」，按 provider 分会把同一条知识写两遍。
   * 失败与「未配置」的分流同样走 usageFailureOf，保证摘要与展开的明细面板不会
   * 一个说红一个说灰。摘要文案刻意压到「数字 + 单位」，完整句放进 title。
   */
  function usageSummary(entry) {
    if (entry === undefined) return { text: '未查询', kind: 'muted', title: '尚未查询该账号的余额' };
    if (entry === null) return { text: '查询中…', kind: 'muted', title: '正在查询' };
    const failure = wbUsagePanel.usageFailureOf(entry);
    if (failure) {
      return failure.notConfigured
        ? { text: '未配置', kind: 'muted', title: `${failure.message}（去该账号的「设置」里填上查询凭证即可）` }
        : { text: '查询失败', kind: 'bad', title: failure.message };
    }
    if (typeof entry !== 'object' || entry === null) return { text: '无数据', kind: 'muted', title: String(entry) };
    if (Object.prototype.hasOwnProperty.call(entry, 'totalLeft')) {
      const total = entry.unlimited ? '∞' : numberText(entry.totalLeft);
      return {
        text: `可用 ${total}`,
        kind: 'ok',
        title: `总剩余 ${total} · 套餐 ${numberText(entry.planLeft)} · 奖励 ${numberText(entry.bonusLeft)}`,
      };
    }
    if (Object.prototype.hasOwnProperty.call(entry, 'available') || Array.isArray(entry.wallets)) {
      const unit = String(entry.unit || '积分');
      const wallets = Array.isArray(entry.wallets) ? entry.wallets : [];
      const detail = wallets.map(wallet => `${wallet?.displayName || wallet?.type || '明细'} ${numberText(wallet?.balance)}`).join(' · ');
      return {
        text: `可用 ${numberText(entry.available)} ${unit}`,
        kind: 'ok',
        title: detail ? `可用 ${numberText(entry.available)} ${unit}（${detail}）` : `可用 ${numberText(entry.available)} ${unit}`,
      };
    }
    return { text: '无数据', kind: 'muted', title: '未返回可识别的余额数据' };
  }

  /**
   * 余额：只放**摘要读数**（不可点），查询按钮已移到操作列。
   *
   * ── 为什么按钮挪走（本次改造）────────────────────────────────
   * 原先这一列是「一颗「积分」按钮 + 一行摘要」。按钮在余额列里的问题是
   * **它的位置与它的作用不符**：它发起的是一个网络动作（查上游余额），
   * 而这一列是读数区（有效期、状态、限流都是读数）。用户扫这一列是想看
   * 「还剩多少」，结果每行第一个东西是一颗要点的按钮。
   * 挪到操作列之后，这一列纯粹是读数，与相邻几列的语义一致。
   *
   * 摘要仍然只做展示（不可点）：展开/收起由操作列那颗按钮负责，
   * 同一格放两个能点的东西会让人分不清哪个是查询、哪个是展开。
   */
  function usageCell(account, ctx) {
    if (!supportsUsage(account)) {
      return '<td class="cell-usage"><span class="muted" title="该提供商没有余额查询">—</span></td>';
    }
    const summary = usageSummary(ctx.usageEntry);
    return `<td class="cell-usage">`
      + `<span class="usage-sum ${summary.kind}" title="${esc(summary.title)}">${esc(summary.text)}</span></td>`;
  }

  /**
   * 操作：签到 / 查余额（或收起）/ 设置 / ⋯。四颗按钮，顺序固定。
   *
   * ── 本次改造：按钮从五颗收到四颗，顺序改成「签到 → 余额 → 设置 → ⋯」──
   * 「设为首选」从行上**移进了 ⋯ 菜单**（见 accounts-model.js 的 moreMenuHtml）。
   * 理由是这颗按钮在行上占的位置与它的使用频率不符：它是四个字里最长的一颗
   * （62px，比「已签到」还宽），而它回答的「这个账号排第几」在左起第二列
   * （优先级列）已经写着 —— 行上留一颗按钮去重复隔壁列的信息，代价是操作列
   * 要为它多留 50px，那 50px 全是从账号列挤出来的。
   * 移走之后操作列从 250px 收到 200px（新值同样是量出来的，算式见
   * page-accounts-table.css 的那条声明），省下的宽度归账号列。
   * 可达性不受影响：它在 ⋯ 菜单的第二项（紧跟在启用/禁用之后）。
   *
   * 顺序按「点的频次」排，签到排头：它是这张表里唯一**每天都会做一次**的动作
   * （其余几颗都是「需要时才点」），排在第一位让手指有固定的落点 ——
   * 按钮的显隐会随账号状态变（见下），但**顺序不跟着变**，
   * 所以「第一颗是签到」这条肌肉记忆在任何一行都成立。
   *
   * ── 查询余额按钮（上次改造从余额列挪来）──────────────────────
   * 文案**恒定**是「余额」，展开态表达在 `title` 与 `.open` 类上 ——
   * 这不是随手取的，而是**列宽预算的要求**（见下）。
   * 它原先在余额列里就是同一套做法（标签恒为「积分」，只有 title 变化）。
   *
   * 为什么不改成「余额 / 收起余额」两态文案（看起来更直白）：操作列是这张表里
   * 最挤的一格（四颗按钮并排），多两个字（约 23px）只能从账号列挤。
   * 而这一列里已经有一颗**真的**用两态文案的按钮（签到 / 已签到），那是有理由的：
   * 「已签到」是**不可点**的状态（带 disabled），用户必须一眼看出「今天没得签了」，
   * 藏进 title 就失去意义。余额按钮则两态都可点、且展开态本身有更强的信号
   * （下面那行明细面板整条展开了，控件高亮着）—— 不缺这一句文案。
   *
   * 行为与它原先在余额列里**逐字一致**（`data-action="usage"` 的处理在
   * accounts-view.js，那里对已展开的行走「只收起、不发请求」的分支），
   * 所以下游一行没改 —— 改变的只有它渲染在哪一列。
   *
   * `supportsUsage` 不适用的家不渲染这颗按钮（与余额列显示破折号同一判据，
   * 两处必须同源：列里写着「该提供商没有余额查询」而操作列却给一颗能点的
   * 按钮，用户会以为按钮坏了）。
   *
   * ── 签到按钮的两种形态 ─────────────────────────────────────
   * 今天已经签过（`checkinAt` 落在本地今天，含自动签到与手动签到两条路径）时
   * 显示为**「已签到」并置灰**：这天再点也只能拿到上游「今天已签到」，
   * 留着可点会让人以为还能再领一次。判定与文案的依据见 accounts-groups 的
   * `checkedInToday`。
   *
   * `disabled` 是真的禁用属性（而不是只加个灰样式）：这才同时挡住点击与键盘
   * 操作，也让读屏软件念出「不可用」—— 与「设为首选」在队首时的处理一致。
   *
   * ── 禁用账号也渲染签到按钮（本次改动）─────────────────────
   * `!enabled` 不再影响签到按钮：签到与转发是两件事，一个被禁用的账号依然可以
   * 每天签到攒积分，用户对它点「签到」本身就是明确意图。后端
   * `core::billing::checkin` 的单账号路径同样不再看 `enabled`（只拒国际版），
   * 两条路径口径一致 —— 不会出现「界面给了按钮、后端却 400」。
   */
  function actionsCell(account, ctx) {
    const checkedIn = checkedInToday(account);
    const checkin = !supportsCheckin(account)
      ? ''
      : checkedIn
        ? `<button data-action="checkin" data-id="${esc(account.id)}" disabled`
          + ` title="${esc(checkinDoneTitle(account))}">已签到</button>`
        : `<button data-action="checkin" data-id="${esc(account.id)}" title="为该账号签到">签到</button>`;
    const open = ctx.usageOpen === true;
    const usage = supportsUsage(account)
      ? `<button class="usage-btn${open ? ' open' : ''}" data-action="usage" data-id="${esc(account.id)}"`
        + ` title="${esc(open ? '收起余额明细' : '查询该账号剩余余额')}">余额</button>`
      : '';
    const settings = `<button data-action="settings" data-id="${esc(account.id)}" title="备注名 / 启用 / 代理">设置</button>`;
    return `<td class="cell-actions"><div class="acct-actions">${checkin}${usage}${settings}`
      + `<button data-action="more" data-id="${esc(account.id)}" title="更多操作">⋯</button></div></td>`;
  }

  /** 「已签到」按钮的悬停说明：给出签到时刻与重置时机，回答「为什么点不动、什么时候能再签」 */
  function checkinDoneTitle(account) {
    const at = Number(account?.checkinAt) || 0;
    const clock = at > 0 ? `今天 ${new Date(at).toTimeString().slice(0, 5)}` : '今天';
    return `${clock} 已签到；签到按自然日重置，明天 0 点后可再签`;
  }

  // ─── 整行 ──────────────────────────────────

  /**
   * 一行账号。
   *
   * ctx（由 accounts-view.js 给）：
   *   seat             `{position, total}`：**全局队列**里的位置（序号与 ↑/↓ 边界）
   *   picked           是否被勾选
   *   usageEntry       余额缓存条目；usageOpen 是否已展开明细
   *   limitsOpen       是否已展开限流明细
   *   connections      该账号此刻的活跃请求数（实时轮询的结果，缺失 = 0）
   *   draft            正在编辑中的优先级草稿（重绘时保住用户没提交完的输入）
   */
  /**
   * 把该列的对齐贴到单元格上（列设置换对齐后重绘即可生效，不必改样式表）。
   *
   * 各个 `*Cell` 函数返回的都是 `<td class="cell-xxx">…` 这一形态，所以在
   * class 里补一个后缀就行。补不上（以后有人改了外壳写法）时**回退到包裹**：
   * 让用户看到「设了没完全生效」好过静默丢掉对齐 —— 前者能查出来，后者只能猜。
   */
  function withAlign(html, align) {
    const replaced = html.replace(/^<td class="([^"]*)"/, `<td class="$1 ta-${align}"`);
    return replaced === html ? `<td class="ta-${align}">${html}</td>` : replaced;
  }

  function rowHtml(account, ctx) {
    const classes = ['acct-row'];
    if (!isEnabled(account)) classes.push('disabled');
    if (ctx.picked) classes.push('selected');
    // 逐列渲染（顺序与显隐都来自列设置）。勾选列的内容依赖 ctx.picked，
    // 所以它不走 COLUMNS 里的 render —— 其余列都是「账号 + 行上下文」的纯函数。
    const cells = visibleColumns().map(column => withAlign(
      column.key === 'pick' ? pickCell(account, ctx.picked) : (column.render?.(account, ctx) || ''),
      column.align,
    )).join('');
    return `<tr class="${classes.join(' ')}" data-id="${esc(account.id)}"`
      + `${isDesktopAccount(account) ? ' data-desktop="1"' : ''}>`
      + cells
      + '</tr>';
  }

  /**
   * 展开的明细行（限流 / 积分 / 签到）。挂在账号行**之后**的独立 `<tr>` 上而不是
   * 塞进某个单元格：明细是整宽内容，放进单元格会被那一列的宽度锁死。
   * 未展开时调用方根本不渲染这一行 —— 多一个空 `<tr>` 会白白多出一条分隔线。
   *
   * colspan 取**当前可见列数**：列设置里藏起几列之后，仍按 COLUMNS.length 铺开
   * 会让这一行比表体宽出一截（多出来的格子把整张表顶出横向滚动）。
   */
  function panelsRowHtml(account, panelsHtml) {
    return `<tr class="acct-panels" data-panels-for="${esc(account.id)}">`
      + `<td colspan="${visibleColumns().length}">${panelsHtml}</td></tr>`;
  }

  // ─── 编辑中的输入框：跨重绘保住 ───────────────
  //
  // app.js 每 20 秒拉一次状态并重绘整张表。如果用户正在优先级框里打字，重绘会把
  // 输入框连值一起换掉（光标与没提交的数字都没了）。这里在重绘前把「谁在编辑、
  // 编辑到哪个值」记下来，重绘后放回原位 —— 输入框是整张表里唯一的可编辑控件，
  // 需要保住的也只有它。

  /** 重绘前记下正在编辑的输入框（不在编辑时返回 null） */
  function captureEditing() {
    const active = document.activeElement;
    if (!active || !active.classList?.contains('prio-input')) return null;
    const id = active.dataset.prio;
    if (!id) return null;
    return { id, value: active.value, start: active.selectionStart, end: active.selectionEnd };
  }

  /** 重绘后把编辑中的输入框恢复回去（值、光标位置、焦点） */
  function restoreEditing(snapshot) {
    if (!snapshot) return;
    const input = document.querySelector(`#account-list .prio-input[data-prio="${CSS.escape(snapshot.id)}"]`);
    if (!input) return;
    input.value = snapshot.value;
    input.focus();
    // 光标位置只在数值没被浏览器改写时可用；setSelectionRange 对 number 输入会抛错
    try { input.setSelectionRange(snapshot.start, snapshot.end); } catch { /* number 输入不支持，忽略 */ }
  }

  window.wbAccountsTable = {
    tableHtml,
    rowHtml,
    panelsRowHtml,
    // 连接数格子由视图侧**就地更新**（2 秒轮询只改这一格，不重绘整表；
    // 见 accounts-view.js 的 syncConnections），所以这个渲染函数要导出
    connectionsHtml,
    // 优先级的号段常量与归一：视图侧的输入框提交要按同一份口径判「改了没有」，
    // 所以一起导出（列宽之类的纯内部细节则不导出）
    PRIORITY_DEFAULT,
    priorityOf,
    clampPriority,
    // 账号名隐藏开关：状态在本模块（accountCell / headRowHtml 渲染时都要读），
    // 切换入口给视图侧的点击委托调 —— 本模块不绑事件，见模块头的分工说明
    toggleNamesHidden,
    captureEditing,
    restoreEditing,
  };
})();
