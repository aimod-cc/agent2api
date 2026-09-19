/* Agent2API · 「登录 / 添加账号」弹窗：按提供商分叉 */
/* global wbApp */

/**
 * 「登录 / 添加账号」弹窗里按提供商分叉的部分：提供商分段控件、未知提供商的
 * 「即将上线」占位块、小浣熊 / CatPaw / AutoClaw 三家的三种添加方式（数据驱动见
 * ADD_FORMS），以及弹窗内**所有** .seg 分段控件的交互（点击 / 方向键 /
 * roving tabindex，见 bindSeg）。
 *
 * 来源：原 account-panel.js 的「添加账号弹窗：按提供商分叉」段与「事件绑定」段里
 * 属于它的那部分，按下标拆出，逐字搬移、行为不变。
 *
 * 弹窗骨架（#add-modal）与 WorkBuddy 区块（账号版本 / 登录方式 / 打开方式 / 粘贴
 * 登录态 JSON）仍写在 index.html 里；WorkBuddy 那块的登录与分段控件联动在
 * add-account.js。加载顺序见 index.html：本文件排在 add-account.js 之后、
 * account-panel.js 之前 —— 它自带加载期绑定（mountAddProviderUi 等，需要 DOM 已
 * 解析），且两个「登录 / 添加账号」按钮的监听要按「add-account.js → 本文件 →
 * account-panel.js」的顺序注册（同元素同事件按注册顺序触发）。
 *
 * 依赖：wbApp 的 esc / toast / refresh、window.wbProviders（提供商摘要）、
 * window.__TAURI_INTERNALS__（POST /api/accounts）与 DOM id；不依赖 account-panel.js。
 */
(() => {
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  // ─── 添加账号弹窗：按提供商分叉 ─────────────

  const ADD_PROVIDER_SEG_ID = 'add-provider-seg';
  /** WorkBuddy 原有区块的容器（注入后原有四个 .modal-section 都搬进它） */
  const ADD_WB_BLOCK_ID = 'add-block-workbuddy';
  const ADD_PLACEHOLDER_ID = 'add-block-placeholder';

  /**
   * 分段控件（.seg）的交互在这里实现一次，弹窗内所有分段控件共用：点击、左右 / 上下
   * 方向键、Home / End 切换选中项。选中态写在 .active + aria-checked 上，并用
   * roving tabindex（只有选中项能被 Tab 到）表达「一组里只能选一个」，与 index.html
   * 里的初始标记同一套约定。切换后派发 SEG_EVENT，由关心它的模块在容器上监听：
   * add-account.js 接 WorkBuddy 那三个（版本 / 登录方式 / 打开方式），本模块接提供商与添加方式。
   */
  const SEG_EVENT = 'wb-seg-change';

  const segItemsOf = group => [...group.querySelectorAll('.seg-item')];
  /** 分段控件当前选中项的取值（容器还没渲染出选项时给空串，调用方自行兜底） */
  const segValueOf = group => group?.querySelector('.seg-item.active')?.dataset.value || '';

  function selectSegItem(group, item) {
    for (const node of segItemsOf(group)) {
      const on = node === item;
      node.classList.toggle('active', on);
      node.setAttribute('aria-checked', String(on));
      node.tabIndex = on ? 0 : -1;
    }
  }

  /** 选中指定取值（该取值不存在时不动）：给「复位到 WorkBuddy」这类程序化切换用 */
  function setSegValue(group, value) {
    const item = segItemsOf(group).find(node => node.dataset.value === value);
    if (item) selectSegItem(group, item);
  }

  /**
   * 绑定一个分段控件。用事件委托而不是逐个按钮挂监听：提供商选项会按摘要重建
   * （innerHTML 整段换掉），委托在容器上就不会因为重排而失效。
   * 初始对齐 tabindex 时**不**派发事件 —— 那时业务方多半还没挂上监听。
   */
  function bindSeg(group) {
    if (!group || group.dataset.segBound) return;
    group.dataset.segBound = '1';
    selectSegItem(group, group.querySelector('.seg-item.active') || segItemsOf(group)[0]);
    const pick = item => {
      selectSegItem(group, item);
      group.dispatchEvent(new CustomEvent(SEG_EVENT, { bubbles: true }));
    };
    group.addEventListener('click', event => {
      const item = event.target.closest('.seg-item');
      if (item && group.contains(item)) pick(item);
    });
    group.addEventListener('keydown', event => {
      const item = event.target.closest('.seg-item');
      if (!item) return;
      const items = segItemsOf(group);
      const index = items.indexOf(item);
      const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
      let next = null;
      if (step) next = items[(index + step + items.length) % items.length];
      else if (event.key === 'Home') next = items[0];
      else if (event.key === 'End') next = items[items.length - 1];
      if (!next) return;
      event.preventDefault(); // 方向键 / Home / End 不再顺带滚动弹窗
      next.focus();           // 选中随焦点走，就是单选组方向键的标准交互
      pick(next);
    });
  }

  /** 备注名长度上限（与后端 truncate_chars(name, 100) 一致，这里先挡一次） */
  const MAX_NAME_LENGTH = 100;
  /** uid / deviceId 长度上限（与后端 MAX_USER_UID_LENGTH / MAX_IDENTITY_LENGTH 一致） */
  const MAX_IDENTITY_LENGTH = 256;
  /** 单行控件的长度上限：备注名按 100（后端会截断），其余标识字段按 256 */
  const maxLengthOf = field => (field.key === 'name' ? MAX_NAME_LENGTH : MAX_IDENTITY_LENGTH);

  /** 粘贴内容里是否有非空字符串键（本地预检用；键名与后端取值链对齐） */
  const hasText = (object, keys) =>
    keys.some(key => typeof object[key] === 'string' && object[key].trim());

  /**
   * 三家的「填表单添加」配置：同时驱动块构造（buildProviderBlock）与提交
   * （addProviderManual / Json / Desktop）。字段 key 就是请求体里的键名，
   * 与后端取值链逐一对齐（见各条注释）；方式二统一是「整份 JSON 展开 + 覆盖
   * provider」，方式三统一是 `{ provider, importDesktop: true }`（后端各读自己的
   * 桌面端登录态文件）。`manualNoteHtml` 用于带行内链接的说明（只有小浣熊有）。
   */
  const ADD_FORMS = [
    {
      // raccoon_accounts::add_raccoon_account：token / access_token、refreshToken / refresh_token、name
      provider: 'raccoon',
      label: '小浣熊',
      addButton: '添加小浣熊账号',
      // 网页登录（后端 providers::raccoon::oauth）：官方登录页 + 一次性授权码回调。
      // 只给内嵌窗口，没有「打开方式」这一级 —— 回调是自定义协议深链
      // （office-raccoon://auth/callback），系统浏览器模式下要靠系统注册该协议
      // 才回得来（那是小浣熊官方客户端装的，装了才有），给了这个选项只会让
      // 用户选完永远等不到回调。理由详见 src-tauri/src/login.rs 的模块头。
      webLogin: {
        noteHtml: '打开小浣熊官方登录页，在<strong>内嵌窗口</strong>里完成登录：登录成功后官方页面会回调本机，网关自动用一次性授权码换取凭证并加入账号列表（授权码只在本机传给网关，界面不显示明文 token）。',
        button: '打开网页登录',
        hint: '将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待',
        busyText: '等待小浣熊登录完成…',
      },
      manualTitle: '粘贴 token / refreshToken',
      manualNoteHtml: 'token 是小浣熊的登录凭证（JWT）。refreshToken 可选，填了之后到期可自动续期；两者都可从<a href="#" class="raccoon-hint-link" data-raccoon-hint>小浣熊客户端登录态文件</a>里取到。',
      fields: [
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用凭证里的账号名' },
        { key: 'token', label: 'token', rows: 3, placeholder: '粘贴 access_token（一长串 JWT）' },
        { key: 'refreshToken', inputKey: 'refresh', label: 'refreshToken', rows: 2, optional: true, placeholder: '可选' },
      ],
      jsonTitle: '粘贴小浣熊 auth.json 内容',
      jsonNoteHtml: '把客户端登录态文件（含 <code>access_token</code> / <code>refresh_token</code>）整个粘进来即可，字段名会自动识别，无需手工挑字段。',
      jsonPlaceholder: '{"access_token":"...","refresh_token":"..."}',
      jsonSource: '小浣熊 auth.json 内容',
      jsonCheck: parsed => hasText(parsed, ['access_token', 'token', 'auth_token', 'accessToken']),
      jsonMissing: '内容里没有 access_token（请确认这是小浣熊的登录态文件）',
      desktopNote: '读本机小浣熊客户端当前的登录态建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后，点「刷新 Token」即可同步。',
    },
    {
      // catpaw_accounts::add_catpaw_account：token / accessToken / access_token / auth_token
      // （即 X-Passport-Token）、uid / userId / loginName、name；没有刷新机制
      provider: 'catpaw',
      label: 'CatPaw',
      manualNote: 'token 是 CatPaw 的 X-Passport-Token（登录态 Cookie）。后端要求 uid 或 loginName 来标识账号，本手填表单需填写 uid；含 loginName 的账号记录可用下方 JSON 方式添加。CatPaw 没有刷新机制，token 过期后需在客户端重新登录。',
      fields: [
        { key: 'token', label: 'token', rows: 3, placeholder: 'CatPaw 的 X-Passport-Token（登录态 Cookie）' },
        { key: 'uid', label: 'uid', placeholder: '必填，CatPaw 账号标识' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用登录名或 uid' },
      ],
      jsonTitle: '粘贴旧项目账号 JSON',
      jsonNote: '把旧项目 catpaw-proxy-accounts.json 里的一条账号记录整个粘进来即可：字段名自动识别，provider 一律归到 CatPaw。',
      jsonPlaceholder: '{"id":"...","uid":"...","accessToken":"..."}',
      jsonSource: 'CatPaw 旧项目账号 JSON',
      jsonCheck: parsed => hasText(parsed, ['accessToken', 'token', 'access_token', 'auth_token'])
        || hasText(parsed.auth || {}, ['accessToken', 'token', 'access_token']),
      jsonMissing: '内容里没有 accessToken / token（请确认这是 CatPaw 的账号记录）',
      desktopNote: '读本机 CatPaw 客户端当前的登录态建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后重新导入即可同步。',
      desktopHint: '读取 ~/.meituan-catpaw/auth.json，需已在 CatPaw 客户端登录',
    },
    {
      // AutoClaw 最终凭证解析只读取 token / accessToken、refreshToken、deviceId；name 由 API 读取。
      // 入口虽列出 access_token / refresh_token，解析链未消费；不宣称支持这两个别名或 enc_value。
      provider: 'autoclaw',
      label: 'AutoClaw',
      manualNote: 'token 可填明文 JWT；token / refreshToken 字符串支持 enc: 前缀，后端在 Windows 上使用本机密钥解密。未填写 refreshToken 无法自动续期；deviceId 可选。',
      fields: [
        { key: 'token', label: 'token', rows: 3, placeholder: '明文 JWT 或 auth.json 里的 enc: 加密值（自动解密）' },
        { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '没有则无法自动续期' },
        { key: 'deviceId', label: 'deviceId', optional: true, placeholder: '可选，续期时带上' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用 userId' },
      ],
      jsonTitle: '粘贴 AutoClaw auth.json 内容',
      jsonNote: '粘贴 %APPDATA%/AutoClaw/auth.json 的完整内容：token（或 accessToken）、refreshToken、deviceId。token / refreshToken 字符串的 enc: 前缀由后端解密，不识别 enc_value 键。',
      jsonPlaceholder: '{"token":"enc:...","refreshToken":"enc:...","deviceId":"..."}',
      jsonSource: 'AutoClaw auth.json 内容',
      jsonCheck: parsed => hasText(parsed, ['token', 'accessToken']),
      jsonMissing: '内容里没有 token / accessToken（请确认这是 AutoClaw 的 auth.json）',
      desktopNote: '读本机 AutoClaw 客户端当前的登录态（%APPDATA%/AutoClaw/auth.json，DPAPI + AES-GCM 解密）建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。',
      desktopHint: '读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows',
    },
  ];

  /** 块 id / input id 的前缀：三家都与 provider id 同名，直接复用（少一处要维护的字段） */
  const prefixOf = config => config.provider;
  // inputKey 保留小浣熊既有的 refresh-input id，请求体键仍为 refreshToken。
  const fieldIdOf = (config, field) => `${config.provider}-${field.inputKey || field.key}-input`;
  const addButtonText = config => config.addButton || `添加 ${config.label} 账号`;
  /** 方式一标题：默认「填写凭证添加」，小浣熊沿用原文案 */
  const manualTitleOf = config => config.manualTitle || '填写凭证添加';
  /** 方式一 / 方式二说明：`*Html`（带行内标记）优先，否则转义纯文本 */
  const noteOf = (html, text) => (html || esc(text));
  const manualNoteOf = config => noteOf(config.manualNoteHtml, config.manualNote);
  const jsonNoteOf = config => noteOf(config.jsonNoteHtml, config.jsonNote);
  /** 方式三按钮：没有 desktopHint 的（小浣熊）不挂 title */
  const desktopButtonOf = config => `<button id="${prefixOf(config)}-desktop-button"`
    + (config.desktopHint ? ` title="${esc(config.desktopHint)}"` : '')
    + `>从本机导入桌面端登录态</button>`;

  /** 支持「填表单添加」的提供商：其余家只显示「即将上线」占位 */
  const ADD_FORM_PROVIDERS = { workbuddy: ADD_WB_BLOCK_ID };
  for (const config of ADD_FORMS) ADD_FORM_PROVIDERS[config.provider] = `add-block-${config.provider}`;

  /**
   * 三家的三种添加方式：分段项文案要短（并排一行，太长会把弹窗挤到换行），
   * 详细说明留在各段自己的标题与正文里。id 同时用于拼段落的元素 id：
   * `${provider}-${id}-block`。
   *
   * `webOnly` 的那一项（网页登录）只对**支持网页登录**的家露出（见 methodsOf），
   * 且排在最前面 —— 它是用户最容易走通的一条路（不必知道 token 长什么样、
   * 也不用找客户端的登录态文件）。
   */
  const ADD_METHODS = [
    { id: 'web', label: '网页登录', webOnly: true },
    { id: 'manual', label: '填写凭证' },
    { id: 'json', label: '粘贴 JSON' },
    { id: 'desktop', label: '导入桌面端登录态' },
  ];

  /** 这一家的添加方式列表（不支持网页登录的家把那一项去掉） */
  const methodsOf = config => ADD_METHODS.filter(method => !method.webOnly || config.webLogin);

  /**
   * 注入添加账号弹窗的提供商选择区，并把既有 WorkBuddy 区块收进一个容器。
   *
   * 为什么要把 WorkBuddy 区块挪进一个新容器：它们必须作为一个整体随「提供商」显隐，
   * 而 modal-body 的直接子节点里混着它们与即将注入的新块。挪动只改父子关系
   * （appendChild），节点引用与它们身上的事件监听全部保留。
   */
  function mountAddProviderUi() {
    const body = $('add-modal')?.querySelector('.modal-body');
    if (!body || $(ADD_PROVIDER_SEG_ID)) return;

    // ① 提供商选择区（弹窗第一块）：分段控件，选项由 syncAddProviderOptions 按摘要重建
    const picker = document.createElement('div');
    picker.className = 'modal-section';
    picker.innerHTML = `<h3>提供商</h3>`
      + `<p>选择要添加哪一家的账号。各家的凭证格式与转发路由互相独立，可同时保存多家。</p>`
      + `<div class="seg add-seg" id="${ADD_PROVIDER_SEG_ID}" role="radiogroup"`
      + ` aria-label="选择要添加账号的提供商"></div>`;
    body.insertBefore(picker, body.firstChild);
    bindSeg($(ADD_PROVIDER_SEG_ID));

    // ② 把既有区块（除刚插入的选择区）整体收进 WorkBuddy 容器
    const workbuddy = document.createElement('div');
    workbuddy.id = ADD_WB_BLOCK_ID;
    workbuddy.className = 'add-provider-block';
    [...body.children].forEach(node => {
      if (node !== picker) workbuddy.appendChild(node);
    });
    body.appendChild(workbuddy);

    // ③ 三家的表单块（raccoon / catpaw / autoclaw，同一套构造，见 ADD_FORMS）
    for (const config of ADD_FORMS) {
      const block = document.createElement('div');
      block.id = ADD_FORM_PROVIDERS[config.provider];
      block.className = 'add-provider-block';
      block.hidden = true;
      block.innerHTML = buildProviderBlock(config);
      body.appendChild(block);
    }

    // ④ 其它 provider 的占位块（摘要里出现但后端还没有添加入口）
    const placeholder = document.createElement('div');
    placeholder.id = ADD_PLACEHOLDER_ID;
    placeholder.className = 'add-provider-block';
    placeholder.hidden = true;
    placeholder.innerHTML = `<div class="modal-section">`
      + `<h3>该提供商账号添加功能即将上线</h3>`
      + `<p id="add-placeholder-text">该提供商的账号添加功能还在开发中，敬请期待。</p>`
      + `</div>`;
    body.appendChild(placeholder);
  }

  /**
   * 拼一个表单块（数据驱动，见 ADD_FORMS）：一个分段控件在若干段之间切换 ——
   * 网页登录（只有支持网页登录的家有）/ 填写凭证 / 粘贴 JSON / 导入桌面端登录态，
   * 选中哪种只显示哪一段。控件的 id 用 `${prefix}-${key}-input`，
   * 由提交函数反查，因此这里不必留 DOM 引用。
   */
  function buildProviderBlock(config) {
    const prefix = prefixOf(config);
    const fieldRows = config.fields.map(field => {
      const id = fieldIdOf(config, field);
      // 必填只在标签上标出（弹窗不是 <form>，原生 required 不生效），真正的拦截在 addProviderManual
      const marker = field.optional ? '' : '（必填）';
      const control = field.rows
        ? `<textarea id="${id}" rows="${field.rows}" placeholder="${esc(field.placeholder)}"></textarea>`
        : `<input id="${id}" type="text" maxlength="${maxLengthOf(field)}" placeholder="${esc(field.placeholder)}">`;
      return `<div class="field-row${field.rows ? ' stack' : ''}">`
        + `<label for="${id}">${esc(field.label)}${marker}</label>${control}</div>`;
    }).join('');
    const methodItems = methodsOf(config).map((method, index) =>
      `<button type="button" class="seg-item${index ? '' : ' active'}" data-value="${method.id}"`
      + ` role="radio" aria-checked="${index ? 'false' : 'true'}"`
      + ` tabindex="${index ? '-1' : '0'}">${esc(method.label)}</button>`).join('');
    return `<div class="modal-section">
        <h3>添加方式</h3>
        <div class="seg add-seg" id="${prefix}-method-seg" role="radiogroup"
          aria-label="${esc(config.label)} 账号的添加方式">${methodItems}</div>
      </div>

      ${webLoginBlockOf(config)}

      <div class="modal-section" id="${prefix}-manual-block" hidden>
        <h3>${esc(manualTitleOf(config))}</h3>
        <p>${manualNoteOf(config)}</p>
        ${fieldRows}
        <div class="field-row">
          <button id="${prefix}-add-button" class="primary">${esc(addButtonText(config))}</button>
          <span class="detail" id="${prefix}-add-hint"></span>
        </div>
      </div>

      <div class="modal-section" id="${prefix}-json-block" hidden>
        <h3>${esc(config.jsonTitle)}</h3>
        <p>${jsonNoteOf(config)}</p>
        <div class="field-row stack">
          <textarea id="${prefix}-json-input" rows="4" placeholder='${esc(config.jsonPlaceholder)}'></textarea>
        </div>
        <div class="field-row">
          <button id="${prefix}-json-button" class="primary">识别并添加</button>
        </div>
      </div>

      <div class="modal-section" id="${prefix}-desktop-block" hidden>
        <h3>从本机导入桌面端登录态</h3>
        <p>${esc(config.desktopNote)}</p>
        <div class="field-row">
          ${desktopButtonOf(config)}
        </div>
      </div>`;
  }

  /**
   * 网页登录那一段（只有配了 webLogin 的家有）。
   *
   * ── 为什么不用 .add-sub 包一层 ──────────────────────────────
   * `.add-sub` 是 index.html 里 WorkBuddy「第二级」的样式（左侧竖线 + 缩进），
   * 那里它是**挂在「登录方式」分段控件之下**的子项，缩进表达从属关系。
   * 这里「网页登录」本身就是「添加方式」的一个段落（与填写凭证 / 粘贴 JSON 平级），
   * 再用同一套缩进会让它看起来是上一段的下级。两处结构不同，视觉就该不同。
   *
   * 这里**没有**「打开方式」这一级：小浣熊只支持内嵌窗口（回调是自定义协议深链，
   * 系统浏览器模式下要靠系统注册该协议才收得到，见 src-tauri/src/login.rs 的模块头）。
   * 交互不在这个文件里 —— 引擎是 ui/web-login.js（两家共用），这里只出 DOM。
   */
  function webLoginBlockOf(config) {
    if (!config.webLogin) return '';
    const prefix = prefixOf(config);
    const web = config.webLogin;
    return `<div class="modal-section" id="${prefix}-web-block">
        <h3>网页登录</h3>
        <p>${web.noteHtml}</p>
        <div class="field-row">
          <button id="${prefix}-web-button" class="primary">${esc(web.button)}</button>
          <button id="${prefix}-web-cancel" style="display:none">取消等待</button>
          <span class="detail" id="${prefix}-web-hint">${esc(web.hint)}</span>
        </div>
      </div>`;
  }

  /** 方式分段：选中哪种只显示哪一段（各段自己的表单、按钮与监听都不动，只切显隐） */
  function syncProviderMethod(config) {
    const prefix = prefixOf(config);
    const value = segValueOf($(`${prefix}-method-seg`)) || methodsOf(config)[0].id;
    for (const method of ADD_METHODS) {
      const block = $(`${prefix}-${method.id}-block`);
      if (block) block.hidden = method.id !== value;
    }
  }

  /** 当前选中的提供商 id（弹窗打开时复位到 WorkBuddy，见文件末尾的两个入口按钮） */
  let addProvider = 'workbuddy';

  /**
   * 刷新弹窗标题与块显隐：标题里带上 provider label，用户不必回想刚才选了什么。
   * 未知 provider 显示占位块并说明原因 —— 不报错、不留空白。
   *
   * 显隐按「块 id → provider」反查（ADD_FORM_PROVIDERS），新增一家只改那张表。
   * label 优先问 wbProviders（摘要的权威来源），其次读分段控件里选中项的文案，
   * 最后才退回 id：不能只依赖 DOM —— 选项重建与选中态落定的时序在极端情况下会错开，
   * 读不到时至少别把标题写成「登录 / 添加raccoon账号」。
   */
  function syncAddProvider() {
    const seg = $(ADD_PROVIDER_SEG_ID);
    if (!seg) return;
    const id = addProvider || 'workbuddy';
    const label = window.wbProviders?.labelOf?.(id)
      || seg.querySelector('.seg-item.active')?.textContent?.trim()
      || id;
    const block = ADD_FORM_PROVIDERS[id];
    const title = $('add-title');
    if (title) title.textContent = `登录 / 添加 ${label} 账号`;
    for (const blockId of Object.values(ADD_FORM_PROVIDERS)) {
      if ($(blockId)) $(blockId).hidden = block !== blockId;
    }
    const placeholder = $(ADD_PLACEHOLDER_ID);
    if (placeholder) {
      placeholder.hidden = Boolean(block);
      const text = $('add-placeholder-text');
      if (text && !block) text.textContent = `「${label}」的账号添加功能还在开发中，敬请期待。`;
    }
  }

  /**
   * 选项按 providers 摘要重建（动态：后端注册表加一家就多一项）。
   * 用「id:label」指纹决定要不要重排 DOM：摘要每次刷新都是新数组，无脑重排会让
   * 正在用键盘操作的那个按钮丢掉焦点（连同 :focus-visible 一起消失）。
   * 重建后一定重新落一次选中态 —— 新按钮默认都能被 Tab 到，不落就没有 roving tabindex。
   */
  function syncAddProviderOptions() {
    const seg = $(ADD_PROVIDER_SEG_ID);
    if (!seg) return;
    const list = window.wbProviders?.all?.() || [];
    // 摘要还没到时先放 WorkBuddy 一项：弹窗不能因为一次状态未就绪就空着
    const options = list.length
      ? list.map(item => ({ id: item.id, label: item.label }))
      : [{ id: 'workbuddy', label: 'WorkBuddy' }];
    const signature = options.map(item => `${item.id}:${item.label}`).join('|');
    if (seg.dataset.signature !== signature) {
      seg.dataset.signature = signature;
      seg.innerHTML = options.map(item =>
        `<button type="button" class="seg-item" data-value="${esc(item.id)}"`
        + ` role="radio" aria-checked="false">${esc(item.label)}</button>`).join('');
    }
    // 选中的那家已不在摘要里（账号删光、后端摘了注册表）时退到 WorkBuddy，
    // 它也没有就退到摘要在列的第一家 —— 否则整块都不显示，弹窗会是空的
    if (!options.some(item => item.id === addProvider)) {
      addProvider = options.some(item => item.id === 'workbuddy') ? 'workbuddy' : options[0].id;
    }
    setSegValue(seg, addProvider);
    syncAddProvider();
  }

  // ─── 三家的三种添加方式（数据驱动，配置见 ADD_FORMS）─────────

  /**
   * 统一提交入口：`POST /api/accounts`（body 直接带 provider 字段）。
   *
   * 为什么不走 bridge.rs 的 uploadAccount：那个方法会把入参整形（强制补 edition、
   * 解析 JSON 文本），是 workbuddy 专属的路径，契约里也明确「workbuddy 添加方式不变」。
   * 其余三家这条需要透传 provider / token / refreshToken / importDesktop，
   * 走 api_request 的通用命令最直接（body 原样交给后端，契约 §5 的分支在服务端）。
   */
  async function postAccount(payload) {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      throw new Error('桌面运行时不可用（Tauri 未初始化）');
    }
    const data = await internals.invoke('api_request', {
      request: { method: 'POST', path: '/api/accounts', body: payload },
    });
    return data;
  }

  /** 只有按钮在忙：与列表的 busy 锁无关（添加成功后会 refresh，那次刷新自己会排队） */
  let addBusy = false;

  async function runAdd(button, task) {
    if (addBusy) return;
    addBusy = true;
    const original = button?.textContent;
    if (button) { button.disabled = true; button.textContent = '添加中…'; }
    try {
      await task();
    } finally {
      addBusy = false;
      if (button) { button.disabled = false; button.textContent = original; }
    }
  }

  /** 添加成功后统一收尾：关窗、刷新列表、提示（三家逐字同一套） */
  async function afterAdd(name, label) {
    $('add-modal')?.classList.remove('open');
    await wbApp.refresh?.();
    toast(`✅ ${label}账号已添加${name ? `：${name}` : ''}`);
  }

  /** 添加成功后展示的账号名：公开形态里 name 一定在，标识字段按各家兜底 */
  const addedLabelOf = account => account?.name || account?.uid || account?.userId || '';

  /** 清空该块的表单（添加成功后调用；失败时保留内容方便改动重试） */
  function clearProviderForms(config) {
    const prefix = prefixOf(config);
    const ids = [
      ...config.fields.map(field => fieldIdOf(config, field)),
      `${prefix}-json-input`,
    ];
    for (const id of ids) {
      const node = $(id);
      if (node) node.value = '';
    }
    const hint = $(`${prefix}-add-hint`);
    if (hint) hint.textContent = '';
  }

  /** 方式一：手填字段（只有必填项校验；可选字段留空就不进请求体） */
  async function addProviderManual(config) {
    const prefix = prefixOf(config);
    const payload = { provider: config.provider };
    for (const field of config.fields) {
      const value = $(fieldIdOf(config, field)).value.trim();
      if (!field.optional && !value) { toast(`请填写 ${field.label}`, 'err'); return; }
      if (value) payload[field.key] = value;
    }
    const button = $(`${prefix}-add-button`);
    await runAdd(button, async () => {
      try {
        const data = await postAccount(payload);
        clearProviderForms(config);
        await afterAdd(addedLabelOf(data?.account), config.label);
      } catch (error) {
        toast(`添加失败：${error.message}`, 'err');
      }
    });
  }

  /**
   * 方式二：粘贴整份登录态 JSON（字段名由后端自动识别）。
   *
   * 本地只做「是 JSON 对象 + 认得出凭证」两道预检：整段粘进来时用户最容易粘错层级
   * （例如把整个配置文件的外层对象贴进来），在这里说清楚比等后端 400 更直观。
   * `provider` 放在展开之后 —— 粘贴内容里即使带着别家的 provider 也一律归到当前家
   * （后端按 provider 分派，串了就会把凭证存进别家的组）。
   */
  async function addProviderJson(config) {
    const prefix = prefixOf(config);
    const text = $(`${prefix}-json-input`).value.trim();
    if (!text) { toast(`请粘贴${config.jsonSource}`, 'err'); return; }
    let parsed;
    try {
      parsed = JSON.parse(text);
    } catch {
      toast('粘贴内容不是有效 JSON', 'err');
      return;
    }
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      toast('粘贴内容必须是一个 JSON 对象', 'err');
      return;
    }
    if (!config.jsonCheck(parsed)) {
      toast(config.jsonMissing, 'err');
      return;
    }
    const button = $(`${prefix}-json-button`);
    await runAdd(button, async () => {
      try {
        // name 单独挑出来，别把整个对象当备注名传（后端只认字符串）
        const name = typeof parsed.name === 'string' ? parsed.name.trim() : '';
        const data = await postAccount({
          ...parsed,
          provider: config.provider,
          ...(name ? { name } : {}),
        });
        clearProviderForms(config);
        await afterAdd(addedLabelOf(data?.account), config.label);
      } catch (error) {
        toast(`添加失败：${error.message}`, 'err');
      }
    });
  }

  /** 方式三：从本机导入桌面端登录态（后端各读自己的登录态文件） */
  async function addProviderDesktop(config) {
    const button = $(`${prefixOf(config)}-desktop-button`);
    await runAdd(button, async () => {
      try {
        const data = await postAccount({ provider: config.provider, importDesktop: true });
        await afterAdd(data?.account?.name || '', config.label);
      } catch (error) {
        // 读不到客户端登录态时后端给 400 + 明确原因，原样透出即可
        toast(`导入失败：${error.message}`, 'err');
      }
    });
  }

  /**
   * 方式一（网页登录）：建一份 ui/web-login.js 的控制器并挂上两个按钮。
   *
   * 为什么交互在 web-login.js 而不是这里：同一套等待态 / 取消 / 按钮复位
   * WorkBuddy 也要用，两份实现迟早分叉（见那个文件的模块头）。本文件只负责
   * 「这一家有网页登录时把 DOM 与配置交出去」。
   *
   * `start` 直接走壳命令 `start_login`（与 WorkBuddy 同一条 IPC）：后端
   * `/api/session/login/start` 会按 provider 分派到适配器的授权地址，
   * 不必再经一层 api_request。
   *
   * 控制器在**加载期**就建（这时本家的块已经注入，见 mountAddProviderUi 的调用
   * 顺序）：隐藏状态不影响 DOM 上的禁用态与文案，等到用户切到这家时直接就是对的；
   * 反过来「切到位再建」需要额外记一份「建过没有」，且中途的主进程状态推送会
   * 落在没有控制器的空窗期里。
   */
  function mountWebLogin(config) {
    if (!config.webLogin) return;
    const prefix = prefixOf(config);
    const controller = window.wbWebLogin?.create({
      provider: config.provider,
      buttonId: `${prefix}-web-button`,
      cancelId: `${prefix}-web-cancel`,
      hintId: `${prefix}-web-hint`,
      busyText: config.webLogin.busyText,
      texts: () => ({ button: config.webLogin.button, hint: config.webLogin.hint }),
      // mode 固定 embedded：小浣熊只有内嵌窗口一种（理由见 webLoginBlockOf 的注释）。
      // 后端对非 workbuddy 的 provider 也忽略 mode（那条链不读它）。
      start: () => window.workbuddyDesktop.startLogin('cn', 'embedded', config.provider),
      onSuccess: async () => {
        // 只收起弹窗：这里不调 add-account.js 的 closeModal —— 那个函数还会清
        // WorkBuddy 的粘贴表单，而本次流程与它无关（别家的输入不该被顺手清掉）
        $('add-modal')?.classList.remove('open');
        await wbApp.refresh?.();
        toast(`✅ ${config.label}账号已添加`);
      },
    });
    if (!controller) return; // web-login.js 未加载（脚本顺序被人改坏）时不静默吞掉按钮
    $(`${prefix}-web-button`)?.addEventListener('click', () => controller.start());
    $(`${prefix}-web-cancel`)?.addEventListener('click', () => controller.cancel());
  }

  /** 「登录态文件在哪」的提示：点一下把路径显示在旁边（不打开文件管理器，只给地址） */
  function showRaccoonHint(event) {
    event.preventDefault();
    const hint = $('raccoon-add-hint');
    if (hint) {
      hint.textContent = '登录态文件路径：~/.box-agent/config/auth.json（Windows：C:\\Users\\<你的用户名>\\.box-agent\\config\\auth.json）';
    }
  }

  // 添加账号弹窗：分段控件的交互 + 提供商切换 + 三家的三种添加方式（逐块按前缀挂监听，见 ADD_FORMS）
  mountAddProviderUi();
  // WorkBuddy 区块的三个分段控件（版本 / 登录方式 / 打开方式）结构在 index.html 里，
  // 交互同样归这里绑定；切换后由 add-account.js 的监听接住（见 add-account.js 事件绑定处的注释）
  for (const id of ['add-edition-seg', 'add-login-method', 'add-login-mode']) bindSeg($(id));
  syncAddProviderOptions();
  $(ADD_PROVIDER_SEG_ID)?.addEventListener(SEG_EVENT, () => {
    addProvider = segValueOf($(ADD_PROVIDER_SEG_ID)) || 'workbuddy';
    syncAddProvider();
  });
  for (const config of ADD_FORMS) {
    const prefix = prefixOf(config);
    const seg = $(`${prefix}-method-seg`);
    bindSeg(seg);
    seg?.addEventListener(SEG_EVENT, () => syncProviderMethod(config));
    syncProviderMethod(config);
    $(`${prefix}-add-button`)?.addEventListener('click', () => addProviderManual(config));
    $(`${prefix}-json-button`)?.addEventListener('click', () => addProviderJson(config));
    $(`${prefix}-desktop-button`)?.addEventListener('click', () => addProviderDesktop(config));
    mountWebLogin(config);
  }
  document.addEventListener('click', event => {
    if (event.target.closest('[data-raccoon-hint]')) showRaccoonHint(event);
  });

  /**
   * 两个「登录 / 添加账号」按钮：打开弹窗后同步提供商选项并复位到 WorkBuddy。
   *
   * add-account.js 把这两个按钮绑到它自己的 openModal（加 .open 类、复位 WorkBuddy 表单）。
   * 这里再挂一个监听，在它之后执行（add-account.js 先加载、监听先注册，同元素同事件按注册
   * 顺序触发），把「按摘要重建选项 + 复位提供商」补上。
   */
  for (const id of ['btn-add-account', 'btn-add-account-2']) {
    $(id)?.addEventListener('click', () => {
      addProvider = 'workbuddy';
      syncAddProviderOptions();
    });
  }

  window.wbAccountAddForms = {
    /** 「登录 / 添加账号」弹窗打开时可用：按 providers 摘要重建选项并复位到 WorkBuddy */
    syncAddProvider: () => {
      addProvider = 'workbuddy';
      syncAddProviderOptions();
    },
  };
})();
