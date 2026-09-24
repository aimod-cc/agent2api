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

  const ADD_PROVIDER_GRID_ID = 'add-provider-grid';
  /** 第 1 步（选提供商）与第 2 步（选方式 + 填凭证）的容器 */
  const ADD_STEP_PICK_ID = 'add-step-pick';
  const ADD_STEP_FORM_ID = 'add-step-form';
  const ADD_STEP_BACK_ID = 'add-step-back';
  /** 底部操作条：主按钮与失败提示的落点，由需要它的块在 mount 时把自己的按钮搬进来 */
  const ADD_FOOT_ID = 'add-foot';
  const ADD_FOOT_ACTIONS_ID = 'add-foot-actions';
  const ADD_FOOT_HINT_ID = 'add-foot-hint';
  const ADD_SEARCH_ID = 'add-provider-search';
  /** 账号类型分段（反代 / 预置 API / 自定义）：容器与三个取值 */
  const ADD_TYPE_SEG_ID = 'add-type-seg';
  /** 反代：客户端登录态 / 官方授权页（内置八家） */
  const TYPE_PROXY = 'proxy';
  /** 预置 API：地址 / 协议 / 图标都预置好的官方与托管端点（preset-providers.js） */
  const TYPE_PRESET = 'preset';
  /** 自定义：自建上游（手动新建）与用户已建的自定义家（给这家加账号） */
  const TYPE_CUSTOM = 'custom';
  /** 分段值归一：三个取值之外的一律按「反代」处理（DOM 被人改坏时的保守落点，
      与 resetAddStep 的复位取向一致） */
  const typeValueOf = value =>
    (value === TYPE_PRESET || value === TYPE_CUSTOM ? value : TYPE_PROXY);
  /** 预置家卡片的取值前缀（不是 provider id，只是卡片自己的标记） */
  const PRESET_CARD_PREFIX = 'preset:';
  /** 「新建自定义提供商」那张卡片的取值（不是 provider id，只是卡片自己的标记） */
  const NEW_PROVIDER_CARD_ID = '__new__';
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
      //
      // ── 两个地区各占一项（与 Cline 两池同一手法）──────────────────
      // `autoclaw`（国内版，id 不变 —— 存量账号的落盘契约）与
      // `autoclaw-intl`（国际版，本次新增）。两项**相邻**排列：它们是同一条
      // 产品线的两个版本，中间隔着别家会让「找国际版」变成一次扫描。
      //
      // 两地的差别有三处，其余配置逐字相同：
      //   1. 域名（后端 `autoclaw::region` 里，前端不体现）；
      //   2. **桌面端导入两个地区都给** —— auth.json 两地共用、没有地区标记，
      //      地区由用户在哪一项下点导入决定（见 src-tauri/.../autoclaw/region.rs
      //      与 credentials.rs 的 `local_credentials`）；
      //   3. **登录方式完全不同**：国内版只有手机验证码；国际版只有
      //      Zai / Google OAuth 网页登录（本次把它的手机验证码入口移除，
      //      理由见下面国际版那一项）。
      provider: 'autoclaw',
      label: 'AutoClaw 国内版',
      manualNote: 'token 可填明文 JWT；token / refreshToken 字符串支持 enc: 前缀，后端在 Windows 上使用本机密钥解密。未填写 refreshToken 无法自动续期；deviceId 可选。',
      // 手机验证码登录：国内版**唯一**的官方登录方式（它的登录页不渲染
      // OAuth 按钮 —— 已核对构建产物）。理由详见 login.rs 的模块头。
      smsLogin: {
        noteHtml: '用 AutoClaw 绑定的手机号登录：点「获取验证码」，收到短信后填入并登录。'
          + '这是 AutoClaw 国内版官方唯一的登录方式（它没有网页授权登录），'
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
      // 这一家的登录态是 Electron safeStorage 密文，解密要走 DPAPI（仅 Windows），
      // 因此 macOS 上整段收起（理由见 desktopImportAvailable）
      desktopWindowsOnly: true,
    },
    {
      // AutoClaw 国际版（`autoglm-api.autoglm.ai`）：与国内版同一套协议、
      // 同一套签名指纹（appId/appKey 两地逐字相同，已实测），只有站点不同。
      provider: 'autoclaw-intl',
      label: 'AutoClaw 国际版',
      manualNote: 'token 可填明文 JWT。未填写 refreshToken 无法自动续期；deviceId 可选。国际版与国内版是两套独立的账号体系，请填国际版账号的凭证。若你是用 Zai / Google 账号登录国际版客户端的，可直接用上方「网页登录」或下方「导入桌面端登录态」。',
      // ── OAuth 网页登录：国际版**唯一**的登录方式 ──────────────
      // 这一家的登录页只渲染 Zai / Google 两个按钮（手机验证码表单被死代码
      // 消除，已核对构建产物），因此这里把它放在**第一位** —— 用户最该先看到
      // 的就是官方推荐的那条路。
      //
      // 与另外五家的网页登录差别：授权地址前有一次**强制风控验证码**
      // （阿里云滑块），必须在浏览器里跑完才能拿地址，因此点按钮后会先在
      // 本弹窗里弹滑块（见 ui/autoclaw-oauth.js 的文件头）。
      //
      // 「打开方式」这一级与另外四家同款（见 `modes`）。
      oauthLogin: {
        title: '网页登录（Zai / Google）',
        noteHtml: '用你的 Zai 或 Google 账号登录 AutoClaw <strong>国际版</strong>：'
          + '点下方按钮后先完成一次滑块验证（官方要求的风控步骤），'
          + '随后会打开官方登录页，登录完成即自动添加账号。'
          + '<br>这是国际版官方唯一的登录方式；若你已在客户端登录过，'
          + '用「导入桌面端登录态」更快。',
        // ── 两种打开方式（与 CatPaw / Qoder / Cline 同一级）──────
        // 回调落在本机网关的 loopback 端口（见后端 oauth.rs 的模块头），
        // 与浏览器在哪无关，因此两条路都走得通：
        //   · 内嵌窗口每次用**全新的临时环境**，连着加多个账号互不影响；
        //   · 系统浏览器复用你已登录的 Zai / Google 账号 —— Google 在部分
        //     环境下会拒绝内嵌窗口登录，那条路走不通时用它兜底。
        modes: [
          {
            value: 'embedded',
            label: '内嵌窗口（推荐）',
            hint: '将打开内嵌窗口完成官方登录，登录完成后自动加入账号列表。'
              + '关掉窗口即取消等待（Google 账号若在此被拒，改用「系统浏览器」）',
          },
          {
            value: 'external',
            label: '系统浏览器',
            hint: '将用系统默认浏览器打开官方登录页（会复用浏览器里已登录的 '
              + 'Zai / Google 账号）；完成登录后自动加入账号列表，关掉弹窗即取消等待',
          },
        ],
      },
      fields: [
        { key: 'token', label: 'token', rows: 3, placeholder: '明文 JWT（国际版账号的 access token）' },
        { key: 'refreshToken', label: 'refreshToken', rows: 2, optional: true, placeholder: '没有则无法自动续期' },
        { key: 'deviceId', label: 'deviceId', optional: true, placeholder: '可选，续期时带上' },
        { key: 'name', label: '备注名', optional: true, placeholder: '可选，留空则用 userId' },
      ],
      // 桌面端登录态导入：**两个地区都给**。
      //
      // ── 为什么不再对国际版收起（本次修正）──────────────────────
      // `%APPDATA%/AutoClaw/auth.json` 里没有地区标记，两个构建共用同一个
      // 目录 —— 这一点没变，但**结论要反过来**：正因为本机判断不了，才更该
      // 让用户自己选。他在哪一项下点导入，就得到哪一家的账号；猜错的后果是
      // 上游 401（可见的失败），换一项重导即可。
      //
      // 更要紧的是：国际版客户端的**主登录方式是 Zai / Google OAuth**，
      // 而当时那条链路网关走不通（强制风控验证码，见后端 login.rs 的模块头），
      // 于是「从客户端导入」几乎是 OAuth 用户唯一实用的入口。收起它等于
      // 把最需要这条路的人挡在外面。
      //
      // OAuth 现已接上（上方那一项），导入不再是唯一入口 —— 但它照旧两个地区
      // 都给：它不需要过一次验证码，是一条独立可用的路径。
      desktopNote: '读本机 AutoClaw 客户端当前的登录态（%APPDATA%/AutoClaw/auth.json，DPAPI + AES-GCM 解密）建一个「桌面端实时登录态」账号：凭证不落账号文件、每次实时读取（删掉这条记录不影响客户端登录态）。用 Zai / Google 账号登录国际版客户端的用户走这一条。',
      desktopHint: '读取 %APPDATA%/AutoClaw/auth.json 并解密，仅 Windows',
      desktopWindowsOnly: true,
    },
    window.wbQoderAddForm,
    // Cline 是两家（免费池 / 订阅池各占一个 provider），所以这里**展开**而不是
    // 一项：池已经是身份，界面上不再有「额度池」那一级选择（见 add-cline.js
    // 的模块头）。两份配置的 provider id 各带池名，块 id 随之天然不撞。
    ...(window.wbClineAddForms || []),
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

  /**
   * 壳的编译目标平台（`'macos'` / `'windows'` / `'linux'`），由桥接脚本注入。
   *
   * 浏览器直开（没有壳）时拿不到它，退回空串 —— 此时按「不裁剪」处理：
   * 浏览器直连网关本来就用不了这些壳侧功能，多显示一个选项不会误导谁，
   * 而误裁掉一个**本来可用**的功能会让 Windows 用户莫名其妙少一项。
   */
  const platform = () => window.workbuddyDesktop?.platform || '';

  /**
   * 桌面端导入在这一家、这个平台是否可用。
   *
   * ── 为什么 AutoClaw 要按平台裁掉 ─────────────────────────────
   * 它读的是 `%APPDATA%/AutoClaw/auth.json`，而那个文件里的 token 是
   * Electron safeStorage 的密文，要先过 DPAPI（`CryptUnprotectData`）才能解出
   * 密钥 —— DPAPI 只有 Windows 有。在 macOS 上这条链从「找文件」这一步就断了
   * （后端 `default_user_data_dir()` 非 Windows 直接返回 None），点下去只会
   * 得到一句「仅支持 Windows 平台」的错误。
   *
   * 另外三家（小浣熊 `~/.box-agent`、CatPaw `~/.meituan-catpaw`、
   * Cline `~/.cline`）读的都是 `HOME` 下的明文 JSON，macOS 上照样能导入 ——
   * 所以**只裁 AutoClaw**，不是「macOS 上没有导入功能」。
   */
  const desktopImportAvailable = config => {
    if (config.desktop === false) return false;
    // 网页端（headless 托管面板注入 platform='web'）：没有本机桌面客户端可读，
    // 所有「导入桌面端登录态」入口整段收起
    if (platform() === 'web') return false;
    if (config.desktopWindowsOnly && platform() === 'macos') return false;
    return true;
  };

  /** 支持「填表单添加」的提供商：其余家只显示「即将上线」占位 */
  const ADD_FORM_PROVIDERS = { workbuddy: ADD_WB_BLOCK_ID };
  for (const config of ADD_FORMS) ADD_FORM_PROVIDERS[config.provider] = `add-block-${config.provider}`;

  /**
   * 添加方式的候选分段项：文案要短（并排一行，太长会把弹窗挤到换行），
   * 详细说明留在各段自己的标题与正文里。id 同时用于拼段落的元素 id：
   * `${provider}-${id}-block`。
   *
   * 露出哪些项由 methodsOf 按各家配置裁剪：手机验证码登录只给配了 `smsLogin`
   * 的家（当前只有 AutoClaw **国内版** —— 国际版那条入口已移除，它只有 OAuth
   * 网页登录）；网页登录只给支持它的家；桌面端导入只给 desktop !== false 的家。
   *
   * ── `oauth` 为什么单独一项（不并进「网页登录」）──────────────
   * 对用户来说两者都是「跳去官方页面登录」，但**交互不同**：网页登录点一下就
   * 开窗口（等待态由 web-login.js 统一管），而 AutoClaw 国际版要先在**本弹窗
   * 里弹一个阿里云滑块**让用户拖完，才能拿到授权地址（见 ui/autoclaw-oauth.js
   * 的文件头）。并进网页登录那一段会让那个引擎多出「有些家要先跑一段验证码」
   * 的分支，而两者的发起时序（验证码在前 / 窗口在前）与按钮布局都不一样
   * （这一项是两个变体各一个按钮）。因此各占一项。
   */
  const ADD_METHODS = [
    { id: 'oauth', label: '网页登录（Zai / Google）', oauthOnly: true },
    { id: 'sms', label: '手机验证码登录', smsOnly: true },
    { id: 'web', label: '网页登录', webOnly: true },
    { id: 'manual', label: '填写凭证' },
    { id: 'desktop', label: '导入桌面端登录态' },
  ];

  const regionOf = config => segValueOf($(`${prefixOf(config)}-region-seg`))
    || config.regionOptions?.[0]?.value;
  const methodsOf = config => ADD_METHODS.filter(method => {
    if (method.id === 'oauth') return Boolean(config.oauthLogin);
    if (method.id === 'sms') return Boolean(config.smsLogin);
    if (method.id === 'web') return config.webLogin
      && (!config.webLogin.region || regionOf(config) === config.webLogin.region);
    if (method.id === 'desktop') return desktopImportAvailable(config);
    return true;
  });

  // ─── 后注册的提供商表单（自定义提供商，见 add-custom-provider.js）──────
  //
  // ADD_FORMS 在本文件加载时定型，而 add-custom-provider.js 按 index.html 的
  // 约定排在本文件**之后**加载 —— 自定义提供商的表单（新建 / 加入已有两套
  // 字段、两个不同的提交端点）与 ADD_FORMS 的「字段 + 统一提交 POST
  // /api/accounts」模型对不上，硬塞进去要给每个环节开特例。这里开一个
  // 后注册口：对方在加载期调 registerAddForm，把整个块挂进弹窗，结构、
  // 交互与提交全部自治。
  //
  // 必须声明在 renderProviderCards **之前**：本文件加载末尾就会调它一次，
  // const 声明放在后面会踩暂时性死区（TDZ）直接抛 ReferenceError。
  const EXTRA_ADD_FORMS = [];

  /**
   * 挂一个后注册的表单块。配置形状（provider / label 与 ADD_FORMS 条目同名
   * 字段含义相同，其余由对方定义）：
   *   provider       块 id 与选项值的 key（如 'custom'）
   *   label          卡片与弹窗标题里的展示名（选中具体某一家时以那家的名字为准）
   *   buildBlock()   返回块的 HTML（结构与交互完全由对方定义）
   *   mount(block)   块进 DOM 后绑定自己的事件
   *   onShow({providerId})
   *                  第 2 步切到这一项时回调：对方借此刷新动态内容，并按
   *                  providerId（空串 = 没有指定某一家）决定落到哪种模式
   *
   * 块插在第 2 步容器里、「即将上线」占位之前；block id 登记进 ADD_FORM_PROVIDERS 后，
   * 显隐切换（syncAddProvider）与卡片重画（renderProviderCards）无需再改。
   */
  function registerAddForm(config) {
    if (!config?.provider || EXTRA_ADD_FORMS.some(item => item.provider === config.provider)) return;
    EXTRA_ADD_FORMS.push(config);
    const blockId = `add-block-${config.provider}`;
    ADD_FORM_PROVIDERS[config.provider] = blockId;
    // 两步改造后 modal-body 的直接子节点只剩两个步骤容器，块一律挂进第 2 步
    const host = $(ADD_STEP_FORM_ID) || $('add-modal')?.querySelector('.modal-body');
    if (host && !$(blockId)) {
      const block = document.createElement('div');
      block.id = blockId;
      block.className = 'add-provider-block';
      block.hidden = true;
      block.innerHTML = config.buildBlock?.() || '';
      const placeholder = $(ADD_PLACEHOLDER_ID);
      if (placeholder) host.insertBefore(block, placeholder);
      else host.appendChild(block);
      config.mount?.(block);
    }
    // 选项里补上刚注册的这家（当前选中项不受影响：注册发生在加载期，
    // 那时弹窗还没开，addProvider 仍是缺省的 workbuddy）
    renderProviderCards();
  }

  /**
   * 注入添加账号弹窗的两步结构：
   *   第 1 步（#add-step-pick）—— 提供商卡片列表 + 搜索；
   *   第 2 步（#add-step-form）—— 各家的表单块。
   *
   * 为什么拆两步：改造前是一屏里两层横向分段（上面 9 项选家、下面最多 5 项选方式），
   * 小窗口下换行、找不到入口，且「刚才选的是哪家」在长弹窗里滚两屏就看不见了。
   * 拆开后每一步只回答一个问题。
   *
   * 返回键与主操作分别落在头部与底部，都不占步骤容器：
   *   · 返回键做成 .modal-head 里的一枚图标钮（与右侧关闭键对称），标题自己带着
   *     「给哪家加账号」，原来那条「‹ 上一步　已选：xxx」独占一行、信息还和标题重复；
   *   · 底部操作条（.modal-foot）默认收起，谁把自己的主按钮搬进来谁点亮它 ——
   *     内置家的按钮仍在各自段落里，不受影响。
   *
   * 为什么要挪动既有节点：WorkBuddy 的账号版本 / 网页登录两个 section 写在
   * index.html 里（交互在 add-account.js），它们必须作为一个整体随「哪一家」显隐。
   * 挪动只改父子关系（appendChild），节点引用与它们身上的事件监听全部保留。
   */
  function mountAddProviderUi() {
    const body = $('add-modal')?.querySelector('.modal-body');
    if (!body || $(ADD_STEP_PICK_ID)) return;

    // ① 第 1 步：账号类型分段 + 提供商卡片列表（选项由 renderProviderCards 按摘要重建）。
    // 类型分段与列表**同一块**（一个 .modal-section）：两者是同一个问题的两面
    // （「给什么形态的上游加账号」→「给哪一家加」），分两块带边框会让人以为
    // 是两个独立步骤；列表高度固定（CSS），切分段只换内容、弹窗高度不变。
    const pick = document.createElement('div');
    pick.className = 'add-step';
    pick.id = ADD_STEP_PICK_ID;
    pick.innerHTML = `<div class="modal-section">`
      + `<h3>选择提供商</h3>`
      + `<div class="seg add-seg" id="${ADD_TYPE_SEG_ID}" role="radiogroup"`
      + ` aria-label="账号类型">`
      + `<button type="button" class="seg-item active" data-value="${TYPE_PROXY}"`
      + ` role="radio" aria-checked="true" tabindex="0">反代</button>`
      + `<button type="button" class="seg-item" data-value="${TYPE_PRESET}"`
      + ` role="radio" aria-checked="false" tabindex="-1">预置 API</button>`
      + `<button type="button" class="seg-item" data-value="${TYPE_CUSTOM}"`
      + ` role="radio" aria-checked="false" tabindex="-1">自定义</button>`
      + `</div>`
      + `<p class="add-type-hint" id="add-type-hint">把本机客户端的登录态包装成账号，或用官方授权页登录。</p>`
      + `<span class="input-affix add-provider-search">`
      + `<span class="affix">⌕</span>`
      + `<input type="search" id="${ADD_SEARCH_ID}" placeholder="搜索提供商…" autocomplete="off">`
      + `</span>`
      + `<div class="add-provider-grid" id="${ADD_PROVIDER_GRID_ID}" role="listbox"`
      + ` aria-label="选择要添加账号的提供商"></div>`
      + `</div>`;
    body.insertBefore(pick, body.firstChild);

    // ② 第 2 步：各家的块（返回键在头部、主操作在底部，这里只放块本身）
    const form = document.createElement('div');
    form.className = 'add-step';
    form.id = ADD_STEP_FORM_ID;
    form.hidden = true;
    body.appendChild(form);

    // ③ 返回键：插在标题前面，只在第 2 步显示（第 1 步没有上一级）
    const head = $('add-modal')?.querySelector('.modal-head');
    const headTitle = $('add-title');
    if (head && headTitle && !$(ADD_STEP_BACK_ID)) {
      const back = document.createElement('button');
      back.type = 'button';
      back.id = ADD_STEP_BACK_ID;
      back.className = 'add-back';
      back.title = '返回选择提供商';
      back.setAttribute('aria-label', '返回选择提供商');
      back.textContent = '‹';
      back.hidden = true;
      head.insertBefore(back, headTitle);
      back.addEventListener('click', () => showAddStep('pick'));
    }

    // ④ 底部操作条：主操作从字段流里拿出来，失败提示也有了固定位置
    const modal = $('add-modal')?.querySelector('.modal');
    if (modal && !$(ADD_FOOT_ID)) {
      const foot = document.createElement('div');
      foot.className = 'modal-foot';
      foot.id = ADD_FOOT_ID;
      foot.hidden = true;
      foot.innerHTML = `<span class="add-foot-hint" id="${ADD_FOOT_HINT_ID}"></span>`
        + `<span class="add-foot-actions" id="${ADD_FOOT_ACTIONS_ID}"></span>`;
      modal.appendChild(foot);
    }

    // ⑤ 把既有区块（除刚插入的两个步骤容器）整体收进 WorkBuddy 容器
    const workbuddy = document.createElement('div');
    workbuddy.id = ADD_WB_BLOCK_ID;
    workbuddy.className = 'add-provider-block';
    [...body.children].forEach(node => {
      if (node !== pick && node !== form) workbuddy.appendChild(node);
    });
    form.appendChild(workbuddy);

    // ⑥ 各家的表单块（同一套构造，见 ADD_FORMS）
    for (const config of ADD_FORMS) {
      const block = document.createElement('div');
      block.id = ADD_FORM_PROVIDERS[config.provider];
      block.className = 'add-provider-block';
      block.hidden = true;
      block.innerHTML = buildProviderBlock(config);
      form.appendChild(block);
    }

    // ⑦ 其它 provider 的占位块（摘要里出现但后端还没有添加入口）
    const placeholder = document.createElement('div');
    placeholder.id = ADD_PLACEHOLDER_ID;
    placeholder.className = 'add-provider-block';
    placeholder.hidden = true;
    placeholder.innerHTML = `<div class="modal-section">`
      + `<h3>该提供商账号添加功能即将上线</h3>`
      + `<p id="add-placeholder-text">该提供商的账号添加功能还在开发中，敬请期待。</p>`
      + `</div>`;
    form.appendChild(placeholder);

    // ⑧ 账号类型分段、搜索与卡片点选：只作用在步骤显隐与卡片列表上，不碰各家的块
    bindSeg($(ADD_TYPE_SEG_ID));
    $(ADD_TYPE_SEG_ID)?.addEventListener(SEG_EVENT, () => {
      // 先把选中值落进 addAccountType（renderProviderCards / syncTypeHint 都读它）
      addAccountType = typeValueOf(segValueOf($(ADD_TYPE_SEG_ID)));
      syncTypeHint();
      renderProviderCards();
    });
    $(ADD_SEARCH_ID)?.addEventListener('input', () => renderProviderCards());
    $(ADD_PROVIDER_GRID_ID)?.addEventListener('click', event => {
      const card = event.target.closest('.add-provider-card');
      if (card) pickProvider(card.dataset.provider || '');
    });
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

      ${oauthLoginBlockOf(config)}

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
    if (!desktopImportAvailable(config) || !config.desktopNote) return '';
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
   * OAuth 网页登录那一段（只有配了 `oauthLogin` 的家生成，当前只有 AutoClaw 国际版）。
   *
   * ── 与 `webLoginBlockOf` 的两处结构差别 ──────────────────────
   *   1. **两个按钮**（Zai / Google）：上游是两个独立端点、两套账号体系，
   *      不能用一个按钮加一个下拉 —— 用户点的那个账号在哪个体系里，只有他知道；
   *   2. **hint 由引擎按阶段改写**：验证码那一段在弹窗里（「请在弹出的滑块中
   *      完成验证」），拿到地址之后才进入等窗口/浏览器 —— 而 `webLoginBlockOf`
   *      的 hint 只在打开方式变化时改一次。这里给的初值仍是「打开方式对应的
   *      空闲文案」，流程结束后引擎会恢复成它（见 autoclaw-oauth.js 的 hint）。
   *
   * 「打开方式」这一级与网页登录那一段同款（`.add-sub`）：两条路的回调都落在
   * 本机网关，与浏览器在哪无关，因此都走得通；内嵌窗口用全新临时环境，
   * 系统浏览器复用已有的 Zai / Google 登录态。
   *
   * 交互本身在 ui/autoclaw-oauth.js（含阿里云验证码的移植），本文件只拼 DOM
   * 与交出回调 —— 与 smsLogin / webLogin 同一分工。
   */
  function oauthLoginBlockOf(config) {
    if (!config.oauthLogin) return '';
    const prefix = prefixOf(config);
    const oauth = config.oauthLogin;
    const modes = oauth.modes?.length ? oauth.modes : [];
    const modeSeg = modes.length
      ? `<div class="add-sub">
          <span class="detail">打开方式</span>
          <div class="seg" id="${prefix}-oauth-mode" role="radiogroup"
            aria-label="${esc(config.label)} 网页登录的打开方式">${modes.map((mode, index) =>
    `<button type="button" class="seg-item${index ? '' : ' active'}" data-value="${esc(mode.value)}"`
    + ` role="radio" aria-checked="${index ? 'false' : 'true'}"`
    + ` tabindex="${index ? '-1' : '0'}">${esc(mode.label)}</button>`).join('')}</div>
        </div>`
      : '';
    return `<div class="modal-section" id="${prefix}-oauth-block" hidden>
        <h3>${esc(oauth.title || '网页登录')}</h3>
        <p>${oauth.noteHtml || ''}</p>
        ${modeSeg}
        <div class="field-row">
          <button id="${prefix}-oauth-zai" class="primary">使用 Zai 账号登录</button>
          <button id="${prefix}-oauth-google">使用 Google 账号登录</button>
          <button id="${prefix}-oauth-cancel" style="display:none">取消</button>
        </div>
        <div class="field-row">
          <span class="detail" id="${prefix}-oauth-hint">${esc(modes[0]?.hint || oauth.hint || '')}</span>
        </div>
      </div>`;
  }

  /**
   * 手机验证码登录那一段（只有配了 `smsLogin` 的家生成，当前只有 AutoClaw
   * **国内版** —— 国际版那条入口已移除，它只有 OAuth 网页登录）。
   *
   * ── 为什么它不是「网页登录」的一种形态 ───────────────────────
   * 网页登录的交互是「开窗口 → 用户在官方页面上操作 → 网关等回调」，因此复用了
   * web-login.js 那套等待/取消/轮询。这条链路完全不同：上游没有授权页、没有回调，
   * 就是「发码 → 用码换 token」两次同步请求，全程在本弹窗里完成。硬塞进网页登录
   * 只会让那个引擎多出一堆「这条路没有窗口也没有 state」的分支。
   *
   * 「获取验证码」与「登录」是两个按钮：发码是个独立的用户动作（要等短信到达），
   * 合并成一个按钮就得替用户猜「这次点的是发码还是登录」。
   *
   * 手机号形态因此只有国内那一种（`1[2-9]` 开头 11 位）—— 国际版曾在同一段里
   * 走 6-15 位的宽松规则，那条分支随入口一起删掉了（见 sms-login.js 的说明）。
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
   * 第 1 步的账号类型（'proxy' = 反代 / 'custom' = 自定义）。
   * 反代段是现有八家（登录态 / OAuth，账号由网关包装）；自定义段是自定义提供商
   * （API Key 直连上游）：已建的家 + 预置家 + 「手动新建」卡。打开弹窗复位到反代。
   */
  let addAccountType = TYPE_PROXY;

  /**
   * 当前处于哪一步：'pick' = 选提供商，'form' = 选方式 + 填凭证。
   * 打开弹窗（以及点「上一步」）回到 'pick'；选中一家后进 'form'。
   */
  let addStep = 'pick';
  /**
   * 选中某一家自定义提供商时记下它的 id（`custom-…`）。
   * 自定义家的表单是**一个**后注册块（内部有「新建 / 选择已有」两种模式），
   * 这个值就是给那个块的上下文：非空时它切到「选择已有」并预选这一家。
   * 空串 = 没有上下文（例如从「手动新建」那张卡进来，或选的是内置家）。
   */
  let addProviderHint = '';
  /**
   * 第 1 步点的是**预置家**卡片时记下它的 key（preset-providers.js 的目录项）。
   * 交给自定义块的 onShow，让它把名称 / 协议 / Base URL 预填进新建表单。
   * 空串 = 不是从预置卡进来的（手动新建 / 加入已有 / 内置家）。
   */
  let addPresetKey = '';

  /** 该提供商此刻名下的账号数（读主状态；自定义家不在摘要里，只能现算） */
  function accountCountOf(providerId) {
    const accounts = wbApp.getState?.()?.accounts?.accounts || [];
    return accounts.filter(account => (account?.provider || 'workbuddy') === providerId).length;
  }

  /**
   * 第 1 步的卡片列表数据，按账号类型分三段：
   *   · 反代 —— 内置家来自 providers 摘要（现有八家）；
   *   · 预置 API —— 预置目录（preset-providers.js）的官方与托管端点，
   *     点一张卡 = 创建这一家并预填；**已建过同名家的预置卡不再出现** ——
   *     那张已建卡就在「自定义」段里，再给一份只会让人犹豫点哪张；
   *   · 自定义 —— 已建的自定义家（customList），每张卡是「给这家加账号」
   *     的对象（点击进「加入已有」并预选）。「手动新建」那张卡**不在这个
   *     列表里**：它不是一家提供商，由 renderProviderCards 单独插在队首。
   */
  function providerCards() {
    if (addAccountType === TYPE_PRESET) {
      const customNames = new Set((window.wbProviders?.customList?.() || [])
        .map(item => item.name || ''));
      return (window.wbPresetProviders?.list || [])
        .filter(preset => !customNames.has(preset.name))
        .map(preset => ({
          id: PRESET_CARD_PREFIX + preset.key,
          label: preset.name,
          count: 0,
          preset: true,
        }));
    }
    if (addAccountType === TYPE_CUSTOM) {
      return (window.wbProviders?.customList?.() || []).map(provider => ({
        id: provider.id,
        label: provider.name || provider.id,
        count: accountCountOf(provider.id),
        custom: true,
      }));
    }
    const list = window.wbProviders?.all?.() || [];
    // 摘要还没到时先放 WorkBuddy 一张：弹窗不能因为一次状态未就绪就空着
    const cards = list.length
      ? list.map(item => ({
        id: item.id,
        label: item.label,
        count: Number(item.count) || 0,
        custom: false,
      }))
      : [{ id: 'workbuddy', label: 'WorkBuddy', count: accountCountOf('workbuddy'), custom: false }];
    // 展示顺序微调：两个 AutoClaw 版本要挨着（两列网格里同处一行）且**国内版
    // 在前** —— 摘要给的是注册表顺序（…CatPaw, AutoClaw, AutoClawIntl, Qoder…），
    // 把 Qoder 挪到国内版前面即可：<CatPaw | Qoder>、<国内版 | 国际版>。
    const from = cards.findIndex(item => item.id === 'qoder');
    const to = cards.findIndex(item => item.id === 'autoclaw');
    if (to >= 0 && from > to) cards.splice(to, 0, cards.splice(from, 1)[0]);
    return cards;
  }

  /**
   * 内置家的真实图标：`id → assets/providers/<file>.png`，图取自各客户端
   * 安装目录内嵌的图标（与系统里显示的为同一张；AutoClaw 国内 / 国际版、
   * Cline 两种账号各自共用一张 —— 它们本来就是同一个客户端）。
   * 自定义家与没收录图标的家回落到首字母徽章（见 logoHtml）。
   */
  const PROVIDER_ICONS = {
    workbuddy: 'assets/providers/workbuddy.png',
    raccoon: 'assets/providers/raccoon.png',
    catpaw: 'assets/providers/catpaw.png',
    autoclaw: 'assets/providers/autoclaw.png',
    'autoclaw-intl': 'assets/providers/autoclaw.png',
    qoder: 'assets/providers/qoder.png',
    'cline-free': 'assets/providers/cline.png',
    'cline-pass': 'assets/providers/cline.png',
  };

  /** 卡片图标：收录过的家出真实图标，其余仍用首字母徽章。
   *  预置家（preset: 前缀）问预置目录要图标（preset-providers.js 已收录 20 家）。 */
  function logoHtml(item) {
    const icon = item.id.startsWith(PRESET_CARD_PREFIX)
      ? window.wbPresetProviders?.iconOf?.(item.id.slice(PRESET_CARD_PREFIX.length)) || ''
      : PROVIDER_ICONS[item.id];
    if (icon) {
      return `<span class="add-provider-logo has-icon"><img src="${icon}" alt="" loading="lazy"></span>`;
    }
    const initial = String(item.label || '?').trim().slice(0, 1).toUpperCase() || '?';
    return `<span class="add-provider-logo">${esc(initial)}</span>`;
  }

  /**
   * 卡片正文第二行（meta）。三段各说各的动作，徽章不再出现 —— 分段已经把
   * 类型分开了，整段都是同一类，再给每张卡挂一枚「预置 / 自定义」徽章只是噪音：
   *   · 预置卡：点开即预填，动作是「创建」；
   *   · 已建自定义家：动作是「给这家加账号」；
   *   · 内置家：只报账号数（添加方式进第 2 步才出现）。
   */
  function cardMeta(item) {
    if (item.preset) return '点开即预填，填 Key 接入';
    if (item.custom) return item.count ? `${item.count} 个账号，点击添加` : '点击添加账号';
    return item.count ? `${item.count} 个账号` : '还没有账号';
  }

  function cardHtml(item) {
    return `<button type="button" class="add-provider-card" data-provider="${esc(item.id)}" role="option">`
      + logoHtml(item)
      + `<span class="add-provider-info">`
      + `<span class="add-provider-name">${esc(item.label)}</span>`
      + `<span class="add-provider-meta">${esc(cardMeta(item))}</span>`
      + `</span>`
      + `<span class="add-provider-go">›</span>`
      + `</button>`;
  }

  /** 「新建自定义提供商」卡：落在后注册的自定义块上，由那个块切到「新建」模式 */
  function newCardHtml() {
    return `<button type="button" class="add-provider-card is-new" data-provider="${NEW_PROVIDER_CARD_ID}" role="option">`
      + `<span class="add-provider-logo is-new">＋</span>`
      + `<span class="add-provider-info">`
      + `<span class="add-provider-name">新建自定义提供商</span>`
      + `<span class="add-provider-meta">接入一个 OpenAI / Anthropic 兼容的上游</span>`
      + `</span>`
      + `<span class="add-provider-go">›</span>`
      + `</button>`;
  }

  /**
   * 重画第 1 步的卡片列表。每次重画都整段替换（家数是个位数，不值得做局部更新），
   * 但**不做重排指纹**：卡片上没有正在输入的内容，焦点由浏览器在点击后自己落到
   * 新卡片上，重画的开销可以忽略。
   *
   * 「手动新建自定义提供商」只在自定义段出现、排在最前（紧贴搜索框、横跨整行）：
   * 这一段里只有它是「动作」，其余都是「选择」；原先排在队尾时它跟着家数一起
   * 往下沉，建了几家之后就得先滚到底才看得见。
   * 搜索只过滤当前段的已有家；有关键词时「新建」那张卡收起来 —— 用户在找的是一家。
   */
  function renderProviderCards() {
    const grid = $(ADD_PROVIDER_GRID_ID);
    if (!grid) return;
    const keyword = ($(ADD_SEARCH_ID)?.value || '').trim().toLowerCase();
    const cards = providerCards();
    const hit = cards.filter(item => !keyword || item.label.toLowerCase().includes(keyword));
    const newCard = addAccountType === TYPE_CUSTOM && !keyword ? newCardHtml() : '';
    grid.innerHTML = newCard + hit.map(cardHtml).join('');
    if (!hit.length && keyword) {
      grid.innerHTML = `<div class="add-provider-empty">没有匹配「${esc(keyword)}」的提供商</div>`;
    }
  }

  /** 切换步骤：只切两个容器的显隐，块的选择与标题由 syncAddProvider 统一收口 */
  function showAddStep(step) {
    addStep = step;
    const pick = $(ADD_STEP_PICK_ID);
    const form = $(ADD_STEP_FORM_ID);
    const back = $(ADD_STEP_BACK_ID);
    if (pick) pick.hidden = step !== 'pick';
    if (form) form.hidden = step !== 'form';
    // 返回键只在第 2 步有意义：第 1 步已经是这个弹窗的最外层
    if (back) back.hidden = step === 'pick';
    syncAddProvider();
  }

  /**
   * 选中一家（或「新建」/ 预置卡）并进入第 2 步。
   *
   * 四种取值分别落到哪一块：
   *   · `__new__`（手动新建卡）→ 后注册的自定义块，不带 hint（它自己默认「新建」模式）；
   *   · `preset:<key>`（预置卡）→ 同一个自定义块的「新建」模式，context 带上 preset，
   *     让它预填名称 / 协议 / Base URL；
   *   · 某个自定义家 id         → 同一个自定义块，hint 带上这家，让它切「选择已有」并预选；
   *   · 其它（内置家 id）       → 该家自己的块。
   */
  function pickProvider(id) {
    if (!id) return;
    if (id === NEW_PROVIDER_CARD_ID) {
      addProvider = 'custom';
      addProviderHint = '';
      addPresetKey = '';
    } else if (id.startsWith(PRESET_CARD_PREFIX)) {
      addProvider = 'custom';
      addProviderHint = '';
      addPresetKey = id.slice(PRESET_CARD_PREFIX.length);
    } else if (window.wbProviders?.customList?.().some(item => item.id === id)) {
      addProvider = 'custom';
      addProviderHint = id;
      addPresetKey = '';
    } else {
      addProvider = id;
      addProviderHint = '';
      addPresetKey = '';
    }
    showAddStep('form');
  }

  /**
   * 刷新弹窗标题、返回条与块显隐：标题里带上 provider label，用户不必回想刚才选了什么。
   * 未知 provider 显示占位块并说明原因 —— 不报错、不留空白。
   *
   * 显隐按「块 id → provider」反查（ADD_FORM_PROVIDERS），新增一家只改那张表。
   * label 优先问 wbProviders（摘要的权威来源），自定义家问自定义目录（用户起的名字），
   * 最后才退回 id：不能只依赖 DOM —— 卡片重画与选中态落定的时序在极端情况下会错开，
   * 读不到时至少别把标题写成「登录 / 添加raccoon账号」。
   */
  /**
   * 刷新弹窗标题与块显隐：标题里带上 provider label，用户不必回想刚才选了什么。
   * 未知 provider 显示占位块并说明原因 —— 不报错、不留空白。
   *
   * 显隐按「块 id → provider」反查（ADD_FORM_PROVIDERS），新增一家只改那张表。
   * label 优先问 wbProviders（摘要的权威来源），自定义家问自定义目录（用户起的名字），
   * 最后才退回 id：不能只依赖 DOM —— 卡片重画与选中态落定的时序在极端情况下会错开，
   * 读不到时至少别把标题写成「登录 / 添加raccoon账号」。
   */
  function syncAddProvider() {
    const id = addProvider || 'workbuddy';
    // 后注册的表单（自定义提供商）不进 wbProviders 目录，label 直接用配置里的
    const extra = EXTRA_ADD_FORMS.find(item => item.provider === id);
    // 从某一家自定义提供商的卡片进来：标题用那家的名字，而不是「自定义提供商」
    const pickedCustom = addProviderHint
      ? window.wbProviders?.customList?.().find(item => item.id === addProviderHint)?.name
      : '';
    const label = pickedCustom
      || (addPresetKey ? window.wbPresetProviders?.presetOf?.(addPresetKey)?.name : '')
      || extra?.label || window.wbProviders?.labelOf?.(id) || id;
    // 后注册的自定义块有三种进入方式：某一家（hint 带 id）、预置家（preset 带 key）
    // 或「手动新建」卡（两者都为空）。前两者标题用那家的名字，最后一种才说「新建」。
    const heading = extra && !addProviderHint && !addPresetKey
      ? `新建${extra.label}`
      : `登录 / 添加 ${label} 账号`;
    const block = ADD_FORM_PROVIDERS[id];
    const title = $('add-title');
    if (title) title.textContent = addStep === 'pick' ? '添加账号' : heading;
    // 底部操作条默认收起：只有把自己的主按钮搬进来的块（自定义提供商）才重新点亮它
    const foot = $(ADD_FOOT_ID);
    if (foot) foot.hidden = true;
    for (const blockId of Object.values(ADD_FORM_PROVIDERS)) {
      if ($(blockId)) $(blockId).hidden = block !== blockId;
    }
    const placeholder = $(ADD_PLACEHOLDER_ID);
    if (placeholder) {
      placeholder.hidden = Boolean(block);
      const text = $('add-placeholder-text');
      if (text && !block) text.textContent = `「${label}」的账号添加功能还在开发中，敬请期待。`;
    }
    // 弹窗当前显示的是后注册的块：给它一个信号，让它刷新自己的动态内容
    //（自定义提供商要借此重读列表、切「新建 / 选择已有」并预选某一家；
    //  从预置卡进来的还要预填名称 / 协议 / Base URL）。
    // **只在第 2 步发这个信号**：那个块的 onShow 会点亮底部操作条（它把自己的
    // 主按钮搬在底部条上），而点「上一步」返回第 1 步时本函数也会被调到 ——
    // 无条件发信号会让「创建并添加账号」残留成第 1 步底部的孤儿按钮。
    if (extra && block && addStep === 'form') {
      extra.onShow?.({ providerId: addProviderHint, preset: addPresetKey });
    }
  }

  /** 账号类型分段下面那行说明：随选中段变化（放在 mountAddProviderUi 之前声明，加载期就要用） */
  function syncTypeHint() {
    const hint = $('add-type-hint');
    if (!hint) return;
    hint.textContent = addAccountType === TYPE_PRESET
      ? '用 API Key 直连上游，常用提供商的地址与协议已预置。'
      : addAccountType === TYPE_CUSTOM
        ? '自建 OpenAI / Anthropic 兼容上游，地址与协议自己填。'
        : '把本机客户端的登录态包装成账号，或用官方授权页登录。';
  }

  /**
   * 锁定弹窗高度：把 .modal 的高度钉在第 1 步（选提供商）的自然高度上。
   *
   * ── 为什么要在这一步量 ──────────────────────────────────────
   * 第 1 步的高度是常量：标题 / 分段 / 说明 / 搜索固定，卡片列表固定 312px
   * （CSS），所以「停在第 1 步时弹窗的自然高度」就是最稳的基准。第 2 步各家
   * 表单长短差很多（WorkBuddy 的块比自定义的长一倍），不锁的话点进去弹窗就
   * 跟着表单跳；锁掉之后由 CSS 让 body 吃掉 head / foot 之外的全部高度
   * （见 `#add-modal .modal-body` 的 flex:1），表单比它高就内部滚动。
   *
   * ── 为什么只量一次 ──────────────────────────────────────────
   * 内容结构是固定的（列表高度、标题区都不随数据变），窗口宽度变化对弹窗
   * 宽度的影响只发生在视口极窄时（.modal 是 min(620px, 100%)），那种情况下
   * 固定高度也只是让第 1 步轻微滚动 —— 不值得为它引入 resize 重算的复杂度。
   * 弹窗隐藏时（display:none）offsetHeight 是 0，量到 0 就放弃，等下一次
   * 打开弹窗再量（加载期那次调用走的就是这条）。
   */
  function lockModalHeight() {
    const modal = $('add-modal')?.querySelector('.modal');
    if (!modal || modal.dataset.heightLocked) return;
    const height = modal.offsetHeight;
    if (height <= 0) return;
    modal.style.height = `${height}px`;
    modal.dataset.heightLocked = '1';
  }

  /** 回到第 1 步并复位选中项（弹窗打开时与点「上一步」时都走这里） */
  function resetAddStep() {
    addProvider = 'workbuddy';
    addProviderHint = '';
    addPresetKey = '';
    addAccountType = TYPE_PROXY;
    setSegValue($(ADD_TYPE_SEG_ID), TYPE_PROXY);
    syncTypeHint();
    const search = $(ADD_SEARCH_ID);
    if (search) search.value = '';
    renderProviderCards();
    showAddStep('pick');
    // 此刻弹窗正停在第 1 步：量一次它的自然高度并锁住（只锁一次，见函数说明）
    lockModalHeight();
    // 摘要还没到（首次打开弹窗早于首屏那次 refresh）时补拉一次再重画：
    // 否则卡片上会清一色写「还没有账号」，而账号其实早就有了。
    // 只在这一步补 —— 已经拿到摘要时不重复发请求。
    if (!(window.wbProviders?.all?.() || []).length) {
      void window.wbProviders?.load?.().then(() => {
        if (addStep === 'pick') renderProviderCards();
      });
    }
    // 自定义目录同理：切到「自定义」段时已建的家要显示出来
    void window.wbProviders?.refreshCustom?.().then(() => {
      if (addStep === 'pick') renderProviderCards();
    });
  }

  // ─── 账号添加（数据驱动，配置见 ADD_FORMS）─────────

  /**
   * 取可读的错误文案。
   *
   * ── 为什么不能直接写 `error.message`（真实踩过）────────────────
   * 壳侧命令签名是 `Result<Value, String>`，Tauri 把 `Err` 里的 String
   * **原样序列化**给 JS —— rejection 携带的是一个**字符串**而不是 Error 对象，
   * 于是 `error.message` 是 `undefined`，界面显示成「导入失败：undefined」。
   * 真实发生过：AutoClaw 国际版导入被后端拒绝时，那句说明原因的文案被整条
   * 吃掉，用户只看到一个 undefined（后端日志里才有真正的原因）。
   *
   * 桥接层（`window.workbuddyDesktop`）的错误已由 asError 归一化，但本文件的
   * `postAccount` 是直连 `internals.invoke` 的（POST /api/accounts 在桥里没有
   * 对应具名方法），因此这一层兜底必须有 —— 与 sms-login.js /
   * autoclaw-oauth.js 的同名函数是一回事。
   */
  const describeError = error => {
    if (error instanceof Error && error.message) return error.message;
    const text = String(error ?? '').trim();
    return text || '未知错误';
  };

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
        toast(`添加失败：${describeError(error)}`, 'err');
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
        toast(`导入失败：${describeError(error)}`, 'err');
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
      // 这个形参是**各家的变体选择器**，壳侧与后端按 provider 解释它：
      // 优先级：配置里写死的 `edition`（某一家若将来只支持单一变体就填它）
      //  → 地区分段的当前值（Qoder：国际版 global / 中国版 cn，两站都要登录）
      //  → 'cn'（小浣熊既没有地区级，且后端那条链不读这个字段）。
      // Qoder 的 `global` 由壳侧归一成 `intl`（见 src-tauri/src/login.rs）。
      //
      // Cline 曾经也走这个位置传「额度池」，拆分后没有了：两个池是两个
      // provider，池已在 `config.provider` 里，没有第二个旋钮。
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

  /**
   * AutoClaw 国际版的 OAuth 网页登录：把配置与收尾交给 ui/autoclaw-oauth.js
   * （DOM 已由 `oauthLoginBlockOf` 拼好）。与另外两套同一分工 —— 本文件只回答
   * 「这一家有这条链路时把 DOM 与回调交出去」，交互（验证码 SDK、登录窗口、
   * 取消）都在那个文件里。
   *
   * ── 交给引擎的三样东西 ─────────────────────────────────────
   *   · `mode`：「打开方式」分段控件**现读**（惰性函数而不是快照值）——
   *     用户可能在发起之前来回切换，快照会让最后一次切换不生效；
   *   · `hint`：当前打开方式对应的空闲提示，引擎在流程结束与切换打开方式时
   *     用它恢复那一行文案（流程中的阶段提示由引擎自己写，见那个文件的说明）；
   *   · `cancelId`：「取消」按钮。全程挂着：验证码阶段点它 = 作废滑块等待，
   *     拿到地址后的等待登录阶段点它 = 撤掉壳侧那一轮（系统浏览器模式下
   *     没有可关的窗口，它是唯一的取消出口）。
   *
   * `onSuccess` 复用 afterAdd：那条链最终也是往账号库里加一条记录，收尾逻辑
   * 与「填写凭证」「手机验证码」没有理由分三套（响应形状由后端统一）。
   */
  function mountOauthLogin(config) {
    if (!config.oauthLogin) return;
    const prefix = prefixOf(config);
    const oauth = config.oauthLogin;
    const modeSeg = $(`${prefix}-oauth-mode`);
    const modeOf = () => segValueOf(modeSeg) || oauth.modes?.[0]?.value || 'embedded';
    const hintOf = () => oauth.modes?.find(mode => mode.value === modeOf())?.hint
      || oauth.hint || '';
    const controller = window.wbAutoclawOauth?.create({
      provider: config.provider,
      mode: modeOf,
      hint: hintOf,
      cancelId: `${prefix}-oauth-cancel`,
      // 不传账号名：这条链的账号由**网关侧**在回调里落库（壳只回 `{ok:true}`，
      // 拿不到账号记录），传 provider 名当 name 会得到
      // 「AutoClaw 国际版账号已添加：AutoClaw 国际版」这种同义重复。
      // 账号名由 `afterAdd` 里的列表刷新显示。
      onSuccess: () => afterAdd('', config.label),
    });
    // 切换打开方式只影响文案（两个变体按钮、方式列表与所选方式都不变）
    bindSeg(modeSeg);
    modeSeg?.addEventListener(SEG_EVENT, () => controller?.syncTexts());
  }

  /** 「登录态文件在哪」的提示：点一下把路径显示在旁边（不打开文件管理器，只给地址） */
  function showRaccoonHint(event) {
    event.preventDefault();
    const hint = $('raccoon-add-hint');
    if (hint) {
      hint.textContent = '登录态文件路径：~/.box-agent/config/auth.json（Windows：C:\\Users\\<你的用户名>\\.box-agent\\config\\auth.json）';
    }
  }

  // 添加账号弹窗：两步结构（选提供商 → 选方式填凭证）、分段控件交互与各家的添加方式。
  mountAddProviderUi();
  // WorkBuddy 的版本与打开方式由 add-account.js 处理选中项变化。
  for (const id of ['add-edition-seg', 'add-login-mode']) bindSeg($(id));
  resetAddStep();
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
    mountOauthLogin(config);
  }
  document.addEventListener('click', event => {
    if (event.target.closest('[data-raccoon-hint]')) showRaccoonHint(event);
  });

  /**
   * 「添加账号」按钮：打开弹窗后重画卡片列表并复位到第 1 步。
   *
   * add-account.js 把按钮绑到它自己的 openModal（加 .open 类、复位 WorkBuddy 表单）。
   * 这里再挂一个监听，在它之后执行（add-account.js 先加载、监听先注册，同元素同事件按注册
   * 顺序触发），把「按摘要重画卡片 + 回到选家那一步」补上。
   *
   * id 列表里仍留着 `btn-add-account`：报表页那个按钮已随会话状态卡片一起删除，
   * 现在只有 `btn-add-account-2`（账号页）存在。不改成写死单个 id 是因为这里用
   * 可选链遍历、多一个不存在的 id 只是空转一次 —— 而万一以后又在别处加了按钮，
   * 沿用同名约定就能自动接上，不必回来改这一处。
   */
  for (const id of ['btn-add-account', 'btn-add-account-2']) {
    $(id)?.addEventListener('click', resetAddStep);
  }

  window.wbAccountAddForms = {
    /** 「添加账号」弹窗打开时可用：重画卡片列表并复位到第 1 步 */
    syncAddProvider: resetAddStep,
    /**
     * 从模型管理页左栏的「＋ 新建自定义提供商」直达新建表单：打开弹窗、复位
     * 第 1 步，再走「新建」那张卡的同一路径进第 2 步 —— 不跳页，也不模拟点
     * 按钮（那要求按钮必须在场）。弹窗的开关归 add-account.js
     * （`wbAddAccountModal.open`），步骤与表单归本文件。
     */
    openNewCustomForm() {
      window.wbAddAccountModal?.open?.();
      resetAddStep();
      pickProvider(NEW_PROVIDER_CARD_ID);
    },
    // 后注册口：add-custom-provider.js 的两种添加方式共用本文件的两步结构
    // （返回键在头部、主按钮在底部操作条，都由它自己按 context 落到哪一种）
    registerAddForm,
    bindSeg,
    segValueOf,
    setSegValue,
    SEG_EVENT,
  };
})();
