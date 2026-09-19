/* Agent2API · 「登录 / 添加账号」弹窗：按提供商分叉 */
/* global wbApp */

/**
 * 按提供商构造添加账号表单，并统一绑定弹窗内的分段控件。
 * WorkBuddy 的账号版本与网页登录区块在 index.html，交互在 add-account.js。
 *
 * 本文件须在 add-account.js 之后、account-panel.js 之前加载：添加按钮上的
 * 监听按此顺序打开弹窗、同步提供商，再同步账号面板。
 *
 * 依赖 wbApp、wbProviders、Tauri 的 api_request 命令与已解析的弹窗 DOM。
 */
(() => {
  const $ = id => document.getElementById(id);
  const { esc, toast } = wbApp;

  // ─── 添加账号弹窗：按提供商分叉 ─────────────

  const ADD_PROVIDER_SEG_ID = 'add-provider-seg';
  /** WorkBuddy 账号版本与网页登录区块的容器 */
  const ADD_WB_BLOCK_ID = 'add-block-workbuddy';
  const ADD_PLACEHOLDER_ID = 'add-block-placeholder';

  /**
   * 分段控件（.seg）的交互在这里实现一次，弹窗内所有分段控件共用：点击、左右 / 上下
   * 方向键、Home / End 切换选中项。选中态写在 .active + aria-checked 上，并用
   * roving tabindex（只有选中项能被 Tab 到）表达「一组里只能选一个」，与 index.html
   * 里的初始标记同一套约定。切换后派发 SEG_EVENT，由关心它的模块在容器上监听：
   * add-account.js 接 WorkBuddy 的版本与打开方式，本模块接提供商与添加方式。
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

  /**
   * 各家表单字段直接对应请求体键名；桌面端导入统一提交
   * `{ provider, importDesktop: true }`，由后端读取对应客户端的登录态文件。
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
      desktopNote: '读本机小浣熊客户端当前的登录态建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后，点「刷新 Token」即可同步。',
    },
    {
      // catpaw_accounts::add_catpaw_account：token / accessToken / access_token / auth_token
      // （即 X-Passport-Token）、uid / userId / loginName、name；没有刷新机制
      provider: 'catpaw',
      label: 'CatPaw',
      // 网页登录：美团 passport 授权页 + **上游把 token 推回本机网关的 loopback
      // 回调**（见 src-tauri/src/server/core/login/catpaw.rs 的模块头）。
      //
      // ── 为什么这一家有两种「打开方式」──────────────────────────
      // 回调是上游往**本机网关的 http://127.0.0.1:<port> 发的一次表单 POST**，
      // 不是自定义协议深链（那是小浣熊的形态，必须靠系统注册才收得到，所以
      // 那家只给内嵌窗口）。既然回调落点与「哪个浏览器」无关，系统浏览器
      // 就完全走得通 —— 而且它是内嵌窗口走不通时的兜底：美团 passport 的
      // 扫码登录 / 第三方账号登录在部分环境下会拒绝内嵌窗口。
      webLogin: {
        noteHtml: '打开 CatPaw 官方登录页（美团 passport），用你的 CatPaw 账号完成登录：'
          + '登录成功后官方页面会把登录凭证回调到本机网关，自动加入账号列表（界面不显示明文 token）。',
        button: '打开 CatPaw 网页登录',
        busyText: '等待 CatPaw 登录完成…',
        modes: [
          {
            value: 'embedded',
            label: '内嵌窗口（推荐）',
            hint: '将打开内嵌窗口；登录完成后自动加入账号列表。关掉窗口即取消等待',
          },
          {
            value: 'external',
            label: '系统浏览器',
            hint: '将用系统默认浏览器打开登录页（会复用浏览器里已登录的美团账号）；完成登录后自动加入账号列表，关掉弹窗即取消等待',
          },
        ],
      },
      manualNote: 'token 是 CatPaw 的 X-Passport-Token（登录态 Cookie），uid 为必填的账号标识。CatPaw 没有刷新机制，token 过期后需在客户端重新登录。',
      fields: [
        { key: 'token', label: 'token', rows: 3, placeholder: 'CatPaw 的 X-Passport-Token（登录态 Cookie）' },
        { key: 'uid', label: 'uid', placeholder: '必填，CatPaw 账号标识' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用登录名或 uid' },
      ],
      desktopNote: '读本机 CatPaw 客户端当前的登录态建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。客户端重新登录后重新导入即可同步。',
      desktopHint: '读取 ~/.meituan-catpaw/auth.json，需已在 CatPaw 客户端登录',
    },
    {
      // AutoClaw 最终凭证解析只读取 token / accessToken、refreshToken、deviceId；name 由 API 读取。
      // 入口虽列出 access_token / refresh_token，解析链未消费；不宣称支持这两个别名或 enc_value。
      provider: 'autoclaw',
      label: 'AutoClaw',
      manualNote: 'token 可填明文 JWT；token / refreshToken 字符串支持 enc: 前缀，后端在 Windows 上使用本机密钥解密。未填写 refreshToken 无法自动续期；deviceId 可选。',
      // 手机验证码登录（AutoClaw 国内版**唯一**的官方登录方式）。
      // 这一家没有网页登录：上游不开授权页、没有授权码回调，也没有公网 Web 版，
      // 官方客户端自己就是手机号 + 短信验证码。理由详见
      // src-tauri/src/server/core/providers/autoclaw/login.rs 的模块头。
      smsLogin: {
        noteHtml: '用 AutoClaw 绑定的手机号登录：点「获取验证码」，收到短信后填入并登录。'
          + '这是 AutoClaw 官方唯一的登录方式（它没有网页授权登录），'
          + '验证码由本机直接提交给官方接口，界面不显示 token。',
      },
      fields: [
        { key: 'token', label: 'token', rows: 3, placeholder: '明文 JWT 或 auth.json 里的 enc: 加密值（自动解密）' },
        { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '没有则无法自动续期' },
        { key: 'deviceId', label: 'deviceId', optional: true, placeholder: '可选，续期时带上' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用 userId' },
      ],
      desktopNote: '读本机 AutoClaw 客户端当前的登录态（%APPDATA%/AutoClaw/auth.json，DPAPI + AES-GCM 解密）建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。',
      desktopHint: '读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows',
    },
    window.wbQoderAddForm,
  ].filter(Boolean);

  /** 块 id / input id 的前缀与 provider id 同名，直接复用（少一处要维护的字段） */
  const prefixOf = config => config.provider;
  // inputKey 保留小浣熊既有的 refresh-input id，请求体键仍为 refreshToken。
  const fieldIdOf = (config, field) => `${config.provider}-${field.inputKey || field.key}-input`;
  const addButtonText = config => config.addButton || `添加 ${config.label} 账号`;
  /** 手填表单标题：默认「填写凭证添加」，小浣熊沿用原文案 */
  const manualTitleOf = config => config.manualTitle || '填写凭证添加';
  /** 说明中的行内标记优先，否则转义纯文本 */
  const noteOf = (html, text) => (html || esc(text));
  const manualNoteOf = config => noteOf(config.manualNoteHtml, config.manualNote);
  /** 桌面端导入按钮：没有 desktopHint 的（小浣熊）不挂 title */
  const desktopButtonOf = config => `<button id="${prefixOf(config)}-desktop-button"`
    + (config.desktopHint ? ` title="${esc(config.desktopHint)}"` : '')
    + `>从本机导入桌面端登录态</button>`;

  /** 支持「填表单添加」的提供商：其余家只显示「即将上线」占位 */
  const ADD_FORM_PROVIDERS = { workbuddy: ADD_WB_BLOCK_ID };
  for (const config of ADD_FORMS) ADD_FORM_PROVIDERS[config.provider] = `add-block-${config.provider}`;

  /**
   * 添加方式的候选分段项：文案要短（并排一行，太长会把弹窗挤到换行），
   * 详细说明留在各段自己的标题与正文里。id 同时用于拼段落的元素 id：
   * `${provider}-${id}-block`。
   *
   * 露出哪些项由 methodsOf 按各家配置裁剪：手机验证码登录只给配了 `smsLogin`
   * 的家（AutoClaw）且排在最前 —— 它是那一家唯一走得通的官方登录方式，用户
   * 最该先看到它；网页登录只给支持它的家；桌面端导入只给 desktop !== false 的家。
   */
  const ADD_METHODS = [
    { id: 'sms', label: '手机验证码登录', smsOnly: true },
    { id: 'web', label: '网页登录', webOnly: true },
    { id: 'manual', label: '填写凭证' },
    { id: 'desktop', label: '导入桌面端登录态' },
  ];

  const regionOf = config => segValueOf($(`${prefixOf(config)}-region-seg`))
    || config.regionOptions?.[0]?.value;
  const methodsOf = config => ADD_METHODS.filter(method => {
    if (method.id === 'sms') return Boolean(config.smsLogin);
    if (method.id === 'web') return config.webLogin
      && (!config.webLogin.region || regionOf(config) === config.webLogin.region);
    if (method.id === 'desktop') return config.desktop !== false;
    return true;
  });

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

    // ③ 各家的表单块（同一套构造，见 ADD_FORMS）
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
   * 网页登录 / 填写凭证 / 导入桌面端登录态，选中哪种只显示哪一段
   * （某一项不适用于这家时不生成对应段落）。控件的 id 用 `${prefix}-${key}-input`，
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
    return `${regionBlockOf(config)}<div class="modal-section">
        <h3>添加方式</h3>
        <div class="seg add-seg" id="${prefix}-method-seg" role="radiogroup"
          aria-label="${esc(config.label)} 账号的添加方式">${methodItems}</div>
      </div>

      ${smsLoginBlockOf(config)}

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

      ${desktopBlockOf(config)}`;
  }

  /** 桌面端登录态导入段：只有能导入的家生成（Qoder 没有这个来源） */
  function desktopBlockOf(config) {
    if (config.desktop === false || !config.desktopNote) return '';
    const prefix = prefixOf(config);
    return `<div class="modal-section" id="${prefix}-desktop-block" hidden>
        <h3>从本机导入桌面端登录态</h3>
        <p>${esc(config.desktopNote)}</p>
        <div class="field-row">
          ${desktopButtonOf(config)}
        </div>
      </div>`;
  }

  /** 地区分段：只有带 regionOptions 的提供商（Qoder）生成 */
  function regionBlockOf(config) {
    if (!config.regionOptions?.length) return '';
    const prefix = prefixOf(config);
    const items = config.regionOptions.map((option, index) =>
      `<button type="button" class="seg-item${index ? '' : ' active'}" data-value="${esc(option.value)}"`
      + ` role="radio" aria-checked="${index ? 'false' : 'true'}"`
      + ` tabindex="${index ? '-1' : '0'}">${esc(option.label)}</button>`).join('');
    return `<div class="modal-section">
        <h3>地区</h3>
        <div class="seg add-seg" id="${prefix}-region-seg" role="radiogroup"
          aria-label="${esc(config.label)} 账号地区">${items}</div>
      </div>`;
  }

  /**
   * 手机验证码登录那一段（只有配了 `smsLogin` 的家生成，当前只有 AutoClaw）。
   *
   * ── 为什么它不是「网页登录」的一种形态 ───────────────────────
   * 网页登录的交互是「开窗口 → 用户在官方页面上操作 → 网关等回调」，因此复用了
   * web-login.js 那套等待/取消/轮询。这条链路完全不同：上游没有授权页、没有回调，
   * 就是「发码 → 用码换 token」两次同步请求，全程在本弹窗里完成。硬塞进网页登录
   * 只会让那个引擎多出一堆「这条路没有窗口也没有 state」的分支。
   *
   * 「获取验证码」与「登录」是两个按钮：发码是个独立的用户动作（要等短信到达），
   * 合并成一个按钮就得替用户猜「这次点的是发码还是登录」。
   */
  function smsLoginBlockOf(config) {
    if (!config.smsLogin) return '';
    const prefix = prefixOf(config);
    return `<div class="modal-section" id="${prefix}-sms-block" hidden>
        <h3>手机验证码登录</h3>
        <p>${config.smsLogin.noteHtml
          || '用 AutoClaw 账号绑定的手机号登录：点击「获取验证码」，收到短信后填入下方并登录。验证码由本机直接提交给官方接口，界面不显示 token。'}</p>
        <div class="field-row">
          <label for="${prefix}-sms-phone">手机号（必填）</label>
          <input id="${prefix}-sms-phone" type="text" maxlength="11" placeholder="11 位大陆手机号">
          <button id="${prefix}-sms-send">获取验证码</button>
        </div>
        <div class="field-row">
          <label for="${prefix}-sms-code">验证码（必填）</label>
          <input id="${prefix}-sms-code" type="text" maxlength="6" placeholder="6 位数字验证码">
        </div>
        <div class="field-row">
          <label for="${prefix}-sms-name">备注名</label>
          <input id="${prefix}-sms-name" type="text" placeholder="可选，留空则用脱敏手机号">
        </div>
        <div class="field-row">
          <button id="${prefix}-sms-submit" class="primary">登录并添加</button>
          <span class="detail" id="${prefix}-sms-hint"></span>
        </div>
      </div>`;
  }

  /**
   * 网页登录那一段（只有配置了网页登录的提供商才生成）。交互由 web-login.js 统一管理。
   *
   * ── 两种形态（由配置决定）────────────────────────────────────
   *   · 没有 `modes` 的（小浣熊）：一个按钮 + 取消 + 一行提示。它的回调是自定义
   *     协议深链，系统浏览器模式下要靠系统注册该协议才收得到，所以只给内嵌窗口。
   *   · 有 `modes` 的（Qoder）：多一级「打开方式」分段控件（`.add-sub`，与
   *     WorkBuddy 那一级同款样式）—— 内嵌窗口每次用全新临时环境，系统浏览器
   *     复用浏览器已有登录态，两条路各有走不通的场景（见各家配置里的说明）。
   *
   * 「打开方式」只在网页登录那一段里出现，因此它是这一段的二级结构；而**切换
   * 打开方式不改变方法列表**（与地区不同），也就是说这里不需要 rebuildMethods。
   */
  function webLoginBlockOf(config) {
    if (!config.webLogin) return '';
    const prefix = prefixOf(config);
    const web = config.webLogin;
    const modeSeg = web.modes?.length
      ? `<div class="add-sub">
          <span class="detail">打开方式</span>
          <div class="seg" id="${prefix}-web-mode" role="radiogroup"
            aria-label="${esc(config.label)} 网页登录的打开方式">${web.modes.map((mode, index) =>
    `<button type="button" class="seg-item${index ? '' : ' active'}" data-value="${esc(mode.value)}"`
    + ` role="radio" aria-checked="${index ? 'false' : 'true'}"`
    + ` tabindex="${index ? '-1' : '0'}">${esc(mode.label)}</button>`).join('')}</div>
        </div>`
      : '';
    return `<div class="modal-section" id="${prefix}-web-block">
        <h3>网页登录</h3>
        <p>${web.noteHtml}</p>
        ${modeSeg}
        <div class="field-row">
          <button id="${prefix}-web-button" class="primary">${esc(web.button)}</button>
          <button id="${prefix}-web-cancel" style="display:none">取消等待</button>
          <span class="detail" id="${prefix}-web-hint">${
    esc(web.hint || web.modes?.[0]?.hint || '')}</span>
        </div>
      </div>`;
  }

  /** 网页登录选中的打开方式（没有这一级时给空串，由壳侧按各家默认处理） */
  const webModeOf = config => (config.webLogin?.modes?.length
    ? segValueOf($(`${prefixOf(config)}-web-mode`)) || config.webLogin.modes[0].value
    : '');

  /** 方式分段：选中哪种只显示哪一段（各段自己的表单、按钮与监听都不动，只切显隐） */
  function syncProviderMethod(config) {
    const prefix = prefixOf(config);
    const value = segValueOf($(`${prefix}-method-seg`)) || methodsOf(config)[0].id;
    for (const method of ADD_METHODS) {
      const block = $(`${prefix}-${method.id}-block`);
      if (block) block.hidden = method.id !== value;
    }
  }

  /**
   * 重建方式分段（地区切换后调用）。
   *
   * 目前没有哪一家会因地区改变方法列表（Qoder 两站都支持网页登录），因此这
   * 通常是一次无变化的原地重建；保留它是为了**结构上**正确 —— `methodsOf`
   * 仍按 `webLogin.region` 裁剪，将来若某家只支持单一站点，切地区时这里就会
   * 真的收起那一项。
   *
   * 为什么必须重算选中项：收起的那一项可能正是当前选中项 —— 只重建按钮不改选中，
   * 屏幕上会出现「所有段都藏着、一个可见的选中项也没有」。因此选中项不在新列表里时
   * 退到第一项，这是用户此刻唯一能走通的路。
   */
  function rebuildMethods(config) {
    const prefix = prefixOf(config);
    const seg = $(`${prefix}-method-seg`);
    if (!seg) return;
    const methods = methodsOf(config);
    seg.innerHTML = methods.map((method, index) =>
      `<button type="button" class="seg-item${index ? '' : ' active'}" data-value="${method.id}"`
      + ` role="radio" aria-checked="${index ? 'false' : 'true'}"`
      + ` tabindex="${index ? '-1' : '0'}">${esc(method.label)}</button>`).join('');
    bindSeg(seg);
    if (!methods.some(method => method.id === segValueOf(seg))) {
      setSegValue(seg, methods[0].id);
    }
    syncProviderMethod(config);
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

  // ─── 账号添加（数据驱动，配置见 ADD_FORMS）─────────

  /** 统一提交入口：POST /api/accounts，保留各提供商自己的凭证字段。 */
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

  /** 添加成功后统一收尾：关窗、刷新列表、提示 */
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
    const ids = config.fields.map(field => fieldIdOf(config, field));
    for (const id of ids) {
      const node = $(id);
      if (node) node.value = '';
    }
    const hint = $(`${prefix}-add-hint`);
    if (hint) hint.textContent = '';
  }

  /**
   * 手工填写凭证提交（只有必填项校验；可选字段留空就不进请求体）。
   * 各家的字段名由 ADD_FORMS 的 fields 声明，后端按 provider 分派解析。
   */
  async function addProviderManual(config) {
    const prefix = prefixOf(config);
    const payload = { provider: config.provider };
    // 地区（Qoder）：整块共用一个分段控件
    if (config.regionOptions?.length) payload.mode = regionOf(config);
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

  /** 从本机导入桌面端登录态（后端各读自己的登录态文件） */
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
   * 网页登录：建一份 ui/web-login.js 的控制器并挂上两个按钮。
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
    const modes = config.webLogin.modes;
    // 文案随打开方式与服务端状态变化（等待中不被覆盖，由引擎负责）
    const texts = () => {
      const mode = modes?.find(item => item.value === webModeOf(config));
      return { button: config.webLogin.button, hint: mode?.hint || config.webLogin.hint || '' };
    };
    const controller = window.wbWebLogin?.create({
      provider: config.provider,
      buttonId: `${prefix}-web-button`,
      cancelId: `${prefix}-web-cancel`,
      hintId: `${prefix}-web-hint`,
      busyText: config.webLogin.busyText,
      texts,
      // ── 传哪个 edition 给壳侧 ──────────────────────────────────
      // 优先级：配置里写死的 `edition`（某一家若将来只支持单一站点就填它）
      //  → 地区分段的当前值（Qoder：国际版 global / 中国版 cn，两站都要登录）
      //  → 'cn'（小浣熊没有地区级，且后端那条链不读这个字段）。
      // Qoder 的 `global` 由壳侧归一成 `intl`（`login.rs` 的 edition_id），
      // 所以这里原样把分段值传出去即可，不必在这里翻译。
      start: () => window.workbuddyDesktop.startLogin(
        config.webLogin.edition || regionOf(config) || 'cn',
        webModeOf(config) || 'embedded',
        config.provider),
      onSuccess: async () => {
        $('add-modal')?.classList.remove('open');
        await wbApp.refresh?.();
        toast(`✅ ${config.label}账号已添加`);
      },
    });
    if (!controller) return; // web-login.js 未加载（脚本顺序被人改坏）时不静默吞掉按钮
    // 切换打开方式只影响文案（方法列表与所选方法都不变，见 webLoginBlockOf 的注释）
    const modeSeg = $(`${prefix}-web-mode`);
    bindSeg(modeSeg);
    modeSeg?.addEventListener(SEG_EVENT, () => controller.syncTexts());
    $(`${prefix}-web-button`)?.addEventListener('click', () => controller.start());
    $(`${prefix}-web-cancel`)?.addEventListener('click', () => controller.cancel());
  }


  /**
   * 手机验证码登录：把配置与收尾交给 ui/sms-login.js 的引擎
   * （DOM 已由 `smsLoginBlockOf` 拼好）。与小浣熊那套同一分工 —— 本文件只回答
   * 「这一家有这条链路时把 DOM 与回调交出去」，交互本身（发码 / 登录 / 按钮
   * 忙态 / deviceId 记忆）在那个文件里，避免本文件继续膨胀。
   *
   * `onSuccess` 复用 afterAdd：验证码登录接口返回的形状与 `POST /api/accounts`
   * 一致（`{account, list}`），收尾逻辑没有理由分两套。
   */
  function mountSmsLogin(config) {
    if (!config.smsLogin) return;
    window.wbSmsLogin?.create({
      provider: config.provider,
      onSuccess: data => afterAdd(addedLabelOf(data?.account), config.label),
    });
  }

  /** 「登录态文件在哪」的提示：点一下把路径显示在旁边（不打开文件管理器，只给地址） */
  function showRaccoonHint(event) {
    event.preventDefault();
    const hint = $('raccoon-add-hint');
    if (hint) {
      hint.textContent = '登录态文件路径：~/.box-agent/config/auth.json（Windows：C:\\Users\\<你的用户名>\\.box-agent\\config\\auth.json）';
    }
  }

  // 添加账号弹窗：分段控件交互、提供商切换与各家的添加方式。
  mountAddProviderUi();
  // WorkBuddy 的版本与打开方式由 add-account.js 处理选中项变化。
  for (const id of ['add-edition-seg', 'add-login-mode']) bindSeg($(id));
  syncAddProviderOptions();
  $(ADD_PROVIDER_SEG_ID)?.addEventListener(SEG_EVENT, () => {
    addProvider = segValueOf($(ADD_PROVIDER_SEG_ID)) || 'workbuddy';
    syncAddProvider();
  });
  for (const config of ADD_FORMS) {
    const prefix = prefixOf(config);
    const seg = $(`${prefix}-method-seg`);
    // 地区分段（Qoder）：切换后重算方式列表（当前没有哪家会因此收起某一项，
    // 但选中项仍以新列表为准 —— 见 rebuildMethods 的说明）。
    // 网页登录那一段不必跟着重建：它的按钮与提示只随「打开方式」变化，
    // 而「登录哪一站」是发起时现读地区分段值的（见 mountWebLogin 的 start）。
    const regionSeg = $(`${prefix}-region-seg`);
    bindSeg(regionSeg);
    regionSeg?.addEventListener(SEG_EVENT, () => rebuildMethods(config));
    bindSeg(seg);
    seg?.addEventListener(SEG_EVENT, () => syncProviderMethod(config));
    syncProviderMethod(config);
    $(`${prefix}-add-button`)?.addEventListener('click', () => addProviderManual(config));
    $(`${prefix}-desktop-button`)?.addEventListener('click', () => addProviderDesktop(config));
    mountWebLogin(config);
    mountSmsLogin(config);
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
