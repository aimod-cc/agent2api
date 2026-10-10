//! 配置的**对外契约**：默认值、键名常量、各设置的形状与取值范围。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 这些名字同时是**三处的契约**：`config.json`（现在是 `kv` 表的键）、
//! HTTP API 的响应体、以及前端 `config.X` 的读法。它们必须只有一处定义 ——
//! 读侧与写侧各写一遍字符串，任一处拼错都不报错，只会静默地读到默认值。
//! 抽成独立文件后「配置一共有哪些键、各自什么范围」一眼可见，
//! 而 `mod.rs` 只留「怎么读写」。
//!
//! ── 键名为什么一字不改（camelCase 保留）─────────────────────
//! 改造前它们是 `config.json` 的顶层键；进 `kv` 表后**仍然用原名**，
//! 不改成 snake_case：这些名字同时是 HTTP API 契约（`GET /api/config` 的
//! 响应体、前端 `config.X` 的读法），改名会让前端、账号迁移、模型规则三处
//! 同时受影响。数据库里的键名与 JSON 键一一对应，排障时能直接把库里的值
//! 贴进配置文件比对（约定见 `db::schema` 模块头）。
//! 另一面：这些键**绝不能与 `db::schema::RESERVED_KV_KEYS` 相撞** ——
//! 撞名的后果是配置写入把别人的状态删掉（论证见那里）。
//!
//! ── 两类常量 ────────────────────────────────────────────────
//!   - **键名**（`KEY_*`）：读写两侧共用的字符串；
//!   - **边界**（`DEFAULT_*` / `*_MIN_*` / `*_MAX_*`）：读侧回落与写侧校验
//!     共用同一份数字，避免「接口拒绝 60 而手改库接受它」这种两套口径。

use std::collections::BTreeMap;
use std::sync::Arc;

/// 默认模型：客户端未指定模型时使用（对应 Node 版 `--default-model` 默认值）
pub const DEFAULT_MODEL: &str = "auto";
/// 计费接口默认语言（对应 Node 版 `--locale` 默认值）
pub const DEFAULT_LOCALE: &str = "zh-CN";

// ─── 保留期设置的键名与边界（config.json 里的字段名**就是契约**）─────
// 命名风格与既有字段（apiKey / locale / lastRequestModel / autoCheckin）一致：
// camelCase。这里把键名提成常量，是因为**读侧与写侧必须用同一个字符串** ——
// 任一处手写拼错都不会报错，只会静默地读到默认值。

/// 事件日志（统一库的 `logs` 表）保留天数
pub const KEY_LOG_RETENTION_DAYS: &str = "logRetentionDays";
/// 请求日志（统一库的 `requests` 表）保留天数
pub const KEY_REQUEST_RETENTION_DAYS: &str = "requestRetentionDays";
/// 按天聚合（统一库的 `request_daily` 表）保留天数
pub const KEY_DAILY_RETENTION_DAYS: &str = "dailyRetentionDays";

/// 事件日志的保存目录（config.json 键）。
///
/// 值是**绝对路径**；缺省 / 空串 / 相对路径（读侧视为写坏）都回落配置目录 ——
/// 两类数据各一个键，互不约束（可以搬到同一个目录，文件名不冲突）。
pub const KEY_LOG_DIR: &str = "logDir";
/// 请求日志（明细 + 按天聚合）的保存目录（config.json 键），语义同 `KEY_LOG_DIR`
pub const KEY_REQUEST_STATS_DIR: &str = "requestStatsDir";
/// 调试模式原始报文的保存目录（config.json 键），语义同 `KEY_LOG_DIR`
pub const KEY_DEBUG_DIR: &str = "debugDir";

/// 「软件更新」的出网线路（config.json 键，**缺省 = 直连**）。
///
/// 值是归一后的代理配置对象（与账号代理同一形状：`{source:'clash',
/// listenerUid}` / `{source:'custom',…}`），null / 缺省都表示直连。
/// 检查更新与下载安装包共用它（`core::update::client` 的出口候选），
/// 定时检查任务同一条路 —— 所以改完不用重启，下一次检查就生效。
/// 归一与解析都复用账号代理那两份实现（`normalize_account_proxy` /
/// `resolve_account_proxy`），这里只存取。
pub const KEY_UPDATE_PROXY: &str = "updateProxy";

/// GitHub 令牌的密文信封（config.json 键，**存的不是明文**）。
///
/// 值是 `core::update::token` 用 AES-256-GCM 加密出来的 `enc1:<base64>` 信封，
/// 明文只进内存、只在拼请求头时用；密钥在库外的密钥文件里（细节见那个
/// 模块的头注释）。API 只报「有没有、来自哪」，不回显本体。
/// 环境变量 `GITHUB_TOKEN` 仍是兜底来源（优先级：这个键 > 环境变量）。
pub const KEY_GITHUB_TOKEN: &str = "githubToken";

/// 调试模式开关（config.json 键）。
///
/// 开启后转发层会把**发给上游的请求头（脱敏）与请求体、上游返回的响应头与
/// 响应体**完整落到统一库的 `debug_traffic` 表（见 `core::debug_traffic`），请求日志
/// 页的「详情」列据此展示。默认关闭 —— 报文体积可达数百 KB，常开会让日志目录
/// 迅速膨胀；关闭时采集路径完全不执行（零开销，见各采集点的 `if enabled`）。
pub const KEY_DEBUG_MODE: &str = "debugMode";

/// 出站请求体黑名单指纹脱敏开关（config.json 键，对应 workbuddy2api 的
/// `features.sanitize_blacklist_fingerprints`）。
///
/// 开启后转发层在每次出站前剥离上游内容审核的黑名单指纹（见
/// `core::sanitize`）：表头键值整段删除、承载语义的模板句最小改写。
/// **默认开启**（与参考项目同默认）：关掉它等于把客户端 system 模板原样发给
/// 上游，那正是模板句被误拦（400 code=11128）的原因。
/// 转发层逐请求读快照，改完下一个请求立即生效，不重启进程。
pub const KEY_SANITIZE_FINGERPRINTS: &str = "sanitizeBlacklistFingerprints";

/// **Cline 转发头的逐键覆盖**（config.json 键）。
///
/// 形状：`{"User-Agent": "Cline/3.0.62", ...}`（string → string）。语义是
/// **覆盖表**而非全量配置：Cline 上游请求的伪装头默认值硬编码在
/// `core::providers::cline::headers`（与官方客户端形态对齐的那一套，含
/// `X-CLIENT-TYPE: cline-sdk` 这类产品面校验头），这张表里**非空**的项按键
/// 覆盖默认值（也可新增自定义头），**空串**的项表示「这个头不要发」——
/// 于是「回到默认」与「删掉某个头」都是一次 PUT 就能表达的事。
///
/// 为什么默认值不进配置：与 `promptGatewayText` 同一取向 —— 默认值是代码里
/// 的一等公民（上游行为收紧时跟着版本走），配置里只存用户**改过的**那部分，
/// 存量安装不迁移、不回种。
pub const KEY_CLINE_UPSTREAM_HEADERS: &str = "clineUpstreamHeaders";

/// 网关面（`/v1/*`）跨域访问开关的键（config.json 键，设置页「安全 → 网关跨域访问」）。
///
/// **默认关闭**：不开时浏览器里第三方来源的页面调不到网关面 —— 预检请求会落到
/// API Key 中间件上被 401 拒掉（预检按规范不携带 `Authorization` 头），页面侧
/// 只能看到一句「无法连接 API」。要用浏览器里的本地页面（自建 Web UI、单文件
/// 前端应用等）直接连网关时把它打开。开启后网关面按面板路由的老口径应答：
/// 预检直接 204，三个 `Access-Control-Allow-*` 头逐响应下发（来源为 `*`）。
///
/// ── 为什么默认关，而面板路由一直是 `*` ────────────────────────
/// 面板路由的 `*` 是既有行为（面板与 `/api/*` 同源，浏览器本来不需要它），
/// 改它属于另一件事；而网关面是**真正转发上游、消耗额度**的那一面 ——
/// 开着 `*` 又没配 API Key 时，任何网页都能借本机网关打上游。默认关 + 显式
/// 开启，至少让用户知道自己打开了什么。
///
/// 面板路由（`/api/*`）不受本开关影响，保持既有的无条件 CORS
/// （见 `http::panel_router` 与 `http::cors`）。
pub const KEY_CORS_ENABLED: &str = "corsEnabled";

/// 「丢弃保活换行分片」开关的键（详见 `core::upstream::sse::is_newline_keepalive`）。
///
/// 打开后，**声明了该能力位的**提供商（目前只有 WorkBuddy 与 AutoClaw）在下发
/// SSE 帧时丢掉「整片只由换行组成」的 content 分片 —— 那是那两条上游的保活节拍
/// （同一个流里 `: heartbeat` 注释行也在发），客户端按 markdown 渲染就被排成
/// 一行一个词。两天 58 发 WorkBuddy 200 请求里 20 发带这类分片（34.5%），
/// 脏请求里它们占正文分片中位 13.4%（区间 1.2%–30.8%）。
///
/// **默认关闭**，与适配器那位 `sse_strip_newline_chunks()` 是**与**的关系：
/// 能力位回答「这家的上游会不会发这种东西」，这个开关回答「本机要不要为它改写
/// 下发帧」。默认关的理由不是怀疑判据，而是取舍的方向性 —— 开了之后，模型真
/// 有一个「单独成片」的换行也会被一起吃掉；那是一处**看得见的外观损失**，
/// 而它换掉的是一整屏断句。留一个不改代码就能开关的口子，是为了让这件事能在
/// 真流量上验证与随时撤回（见 `api::keepalive` 的 `/api/keepalive-strip`）。
///
/// 部署级兜底：配置里没有这个键时读环境变量 `AGENT2API_STRIP_NEWLINE_KEEPALIVE`
/// （`1` / `true` / `on` 才算开启，其余含空值都算关，见 `parse::env_strip_newline_keepalive`）
/// —— Docker 部署想从第一次启动就打开的，不必先进面板点一次。
pub const KEY_STRIP_NEWLINE_KEEPALIVE: &str = "stripNewlineKeepalive";

/// 机器人校验开关的键（config.json 键，ALTCHA proof-of-work，见 `server::altcha`）。
///
/// **默认开启**：登录 / 注册是公开的认证边界，脚本可以无限打（暴破密码、
/// 抢注管理员）；ALTCHA 让每个请求先花一次算力，配合失败锁定把批量攻击
/// 打得没性价比。对真人无感 —— 登录页在后台把题算完才允许提交。
/// 只影响面板的 login / setup 两个端点，与 `/v1/*` 的 API Key 鉴权无关。
///
/// 部署级兜底：配置里没有这个键时读环境变量 `AGENT2API_CAPTCHA_ENABLED`
/// （登录页人机验证组件环境变量，默认为1开启，0为关闭，见
/// `parse::env_captcha_enabled`）—— Docker 想从第一次启动就关掉校验的，
/// 在 compose / `.env` 里设它即可；设置页改过一次之后以库里的值为准
/// （优先级「配置里的值 > 环境变量」）。
pub const KEY_CAPTCHA_ENABLED: &str = "captchaEnabled";

/// 系统提示词模式的键（config.json 键，对应 workbuddy2api 的 `prompt.mode`）。
///
/// 取值 `passthrough` / `custom` / `append`（见 `core::prompt::PromptMode`）；
/// **默认 `passthrough`**（与参考项目同默认）：不动客户端 system，行为与改造前
/// 逐字相同。`custom` / `append` 改用网关自有提示词替换/追加 system 消息 ——
/// 那是脱敏之外的第二层防护：从**源头**消灭 system 来源的指纹，而不是等它
/// 出站前再改（两层叠加、互不替代，见 `core::prompt` 的模块头）。
pub const KEY_PROMPT_MODE: &str = "promptMode";

/// 系统提示词文件的键（config.json 键，对应 workbuddy2api 的 `prompt.file`）。
///
/// 空串 / 缺失 = 用内置默认提示词（`core::prompt::BUILT_IN_PROMPT`）；
/// 否则读该路径（UTF-8 文本）。只在 `custom` / `append` 模式下有意义 ——
/// `passthrough` 不读文件。
pub const KEY_PROMPT_FILE: &str = "promptFile";

/// **界面里直接编辑**的系统提示词正文（config.json 键）。
///
/// 非空白 = **以它为准**，优先级高于 [`KEY_PROMPT_FILE`] 与内置默认；空串 /
/// 缺失 = 回到「文件 > 内置默认」的老路径。逐家的项里也是这个名字（与
/// `promptMode` / `promptFile` 一样，全局与逐家共用一套键名）。
///
/// ── 为什么要有它（与提示词文件的关系）────────────────────────
/// 文件路径适合「我有一份自己维护的提示词」；改一个字要去编辑器里开文件、
/// 存盘、再切回设置页，对「只想补一句『回答用中文』」这种需求太重。界面正文
/// 是**就地编辑的那一份**，保存即生效（`set_prompt` 会重跑解析，下一个请求
/// 就用新文本）。两者不互相替代：界面正文清空后，文件与内置默认照旧生效 ——
/// 用户不必为了试一句话就把文件路径先删掉。
pub const KEY_PROMPT_TEXT: &str = "promptText";

/// **按提供商**覆盖系统提示词设置的键（config.json 键）。
///
/// 形状：`{"<providerId>": {"promptMode": "...", "promptFile": "..."}}` ——
/// 项里的键名与全局那两个**逐字相同**（三处只有一套名字：配置、接口载荷、
/// 接口响应），值是**稀疏**的 ——
/// 只写想单独配置的那几家，其余一律沿用上面两个全局键。删除某家 = 把这个键从
/// 对象里去掉（接口侧传 `null`）。
///
/// ── 为什么需要它（与全局键的关系）────────────────────────────
/// 「用哪份提示词」本来就是**按上游**不同的问题：一家的内容审核按逐字指纹拦截、
/// 另一家（ZCode 活动套餐通道）只认它自己的官方身份块，拿同一份提示词套所有家
/// 不是过严就是过松。全局键保留为**默认值**（不写这个键时行为与改造前逐字相同），
/// 逐家的差异写在这里。
pub const KEY_PROMPT_PROVIDERS: &str = "promptProviders";

/// **网关自带提示词**的逐家开关（config.json 键）：`{"<providerId>": false}`。
///
/// 上面那个键管的是**客户端 system 怎么处理**；这一项管的是**网关自己要不要装
/// 它那段内置文本**（今天只有 ZCode 活动套餐通道的官方三段身份块，见
/// `zcode::OFFICIAL_PROMPT_NOTE` 的实测记录）。
///
/// ── 为什么单独一张表，而不并进 `promptProviders` ──────────────
/// 两者的生效条件与默认值都不一样：`promptProviders` 的项是「这家**有**自己的
/// 模式 / 文件」（缺省 = 跟随全局），而这一项是「这家要不要装网关的内置段」
/// （缺省 = **装**，因为那是上游当下的硬性要求）。并进同一项里，用户只想拨一下
/// 开关就得连带把模式 / 文件一起落成显式值 —— 于是「今天关掉官方段」会顺手把
/// 这家的模式钉死在当时的全局值上，之后改全局它不再跟随：一个动作产生两个后果，
/// 而且第二个后果用户看不见。分表之后两者互不干扰，各自的缺省语义也说得清。
///
/// 值存 `true` / `false`（用户明确拨过的那一侧）；**键缺失 = 默认（装）**。
/// 接口传 `null` = 删键、回到默认（见 `/api/prompt` 的载荷说明）—— 留着用户
/// 拨过一次的 `true` 是有意的：配置里一眼看得出「这家被明确确认过要装」，
/// 与「从来没动过」区分开，排障时少一次猜测。
pub const KEY_PROMPT_GATEWAY: &str = "promptGateway";

/// **网关自带提示词的正文覆盖**（config.json 键）：
/// `{"<providerId>": {"identity": "...", "stable": "...", "dynamic": "..."}}`。
///
/// 上面那个键管的是「装不装」，这一项管的是「装的那段长什么样」—— 用户改过的
/// 那一段以配置里的文本为准（逐段合并：只写改过的那几段即可，缺的段用官方原文，
/// 见 `core::prompt::GatewayBlocks::or`）。段名就是三段的名字：身份句 / 稳定段 /
/// 动态段 —— 三段各自成块是上游的结构要求，所以不提供「合成一整段」的编辑方式。
///
/// 动态段里的 `{cwd}` / `{platform}` / `{shell}` / `{os_version}` / `{git}` /
/// `{provider}` / `{model}` 是**占位符**：发请求时才换成真实运行值（工作目录、
/// 平台、模型名……）。官方原文里本来就带 `{provider}` / `{model}` 两个，这里只是
/// 把 Environment 段那几行也变成同样的写法，用户想改哪一行都行、想删也可以。
///
/// 删掉某家 = 把这个键从对象里去掉（接口侧传 `null`）= 回到官方原文。
pub const KEY_PROMPT_GATEWAY_TEXT: &str = "promptGatewayText";

/// 写入**某一家**的提示词覆盖时的载荷（`set_prompt_provider` 的入参）。
///
/// 字段与 [`ProviderPrompt`] 前三项一一对应；用独立类型而不是元组，是为了
/// 将来加字段时不必再挨个改调用点的解构。
#[derive(Clone, Debug)]
pub struct ProviderPromptPatch {
    /// 模式（透传 / 替换 / 追加）
    pub mode: crate::server::core::prompt::PromptMode,
    /// 提示词文件（`None` / 空白 = 用内置默认提示词）
    pub file: Option<String>,
    /// 界面里编辑的正文（`None` / 空白 = 没有这一份，回落文件 / 内置默认）
    pub inline: Option<String>,
}

/// 系统提示词设置（设置页「通用 → 系统提示词」）。
///
/// 与 `RetrySettings` 同一取舍：几个值总是一起用（转发层逐请求取一次、
/// 接口一起返回），打包成一个值让调用方一次拿到，不必多次读锁。
/// 与另外几个设置不同，这个**不是 `Copy`**：`text` 是提示词全文（可能几百行），
/// 逐请求克隆它纯属浪费 —— 转发层拿的是借用视图（`core::prompt::PromptPlan`）。
#[derive(Clone, Debug)]
pub struct PromptSettings {
    /// 模式（透传 / 替换 / 追加）
    pub mode: crate::server::core::prompt::PromptMode,
    /// 用户指定的提示词文件（`None` = 未指定）
    pub file: Option<String>,
    /// 界面里编辑的正文（`None` = 没编辑过这一份；有值时它是**生效文本的来源**）
    pub inline: Option<String>,
    /// **实际生效**的提示词文本：`custom` / `append` 下是界面正文 / 文件内容 /
    /// 内置默认；`passthrough` 下**没有生效的正文**，但会把用户存下的界面正文
    /// 带出来（设置页的编辑框要看得见它，见 `resolve_choice` 的说明）
    pub text: String,
    /// 文本来源（界面与日志要能回答「这次用的到底是哪一份」）
    pub source: crate::server::core::prompt::PromptSource,
    /// 指定了文件但读不到时的原因（`None` = 没这回事）；读失败时 `text`
    /// 回落成内置默认 —— 与「写坏回落」的既有取向一致：桌面应用不能因为
    /// 一个提示词文件的问题启动不了或转发不了
    pub file_error: Option<String>,
    /// **按提供商**的覆盖（`KEY_PROMPT_PROVIDERS`）；不在这张表里的家走上面那五项。
    /// `BTreeMap` 而不是 `HashMap`：界面与接口响应都要有稳定顺序（同一份配置
    /// 每次渲染的行序不同，用户会以为设置被改动过）
    pub providers: BTreeMap<String, ProviderPrompt>,
    /// **网关自带提示词的逐家开关**（`KEY_PROMPT_GATEWAY`）：`false` = 这一家不装，
    /// 不在表里 = 装（默认）。与 `providers` 分开的两点理由见那个键的说明。
    pub gateway: BTreeMap<String, bool>,
    /// **网关自带提示词的逐家正文覆盖**（`KEY_PROMPT_GATEWAY_TEXT`）：只含被改过的
    /// 家；不在表里 = 用官方原文。与开关分两张表：改文本与拨开关是两件事。
    pub gateway_text: BTreeMap<String, crate::server::core::prompt::GatewayBlocks>,
}

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            mode: crate::server::core::prompt::PromptMode::default(),
            file: None,
            inline: None,
            text: String::new(),
            source: crate::server::core::prompt::PromptSource::None,
            file_error: None,
            providers: BTreeMap::new(),
            gateway: BTreeMap::new(),
            gateway_text: BTreeMap::new(),
        }
    }
}

/// 单一提供商上的提示词覆盖（`KEY_PROMPT_PROVIDERS` 的每一项）。
///
/// 字段与 [`PromptSettings`] 的前六项**逐一同构**：解析口径也只有一处
/// （`prompt_from` 里那个 `resolve_choice`），于是「全局默认怎么解析、逐家就怎么
/// 解析」不会分叉成两套。不存在「这家没配 file 于是继承全局 file」这种半继承
/// 语义：某家一旦出现在 map 里，它的 mode / file / inline **都**以自己这份为准
/// （`file: None` = 用内置默认提示词）—— 半继承是最容易让用户看不懂的模式，
/// 界面上「这家配了什么」与「实际用了什么」必须一眼对得上。
#[derive(Clone, Debug)]
pub struct ProviderPrompt {
    /// 模式（透传 / 替换 / 追加）
    pub mode: crate::server::core::prompt::PromptMode,
    /// 用户指定的提示词文件（`None` = 未指定，用内置默认）
    pub file: Option<String>,
    /// 界面里编辑的正文（`None` = 没编辑过这一份）
    pub inline: Option<String>,
    /// **实际生效**的提示词文本（`passthrough` 下同全局那份：没有生效正文，
    /// 但有界面正文就带出来）
    pub text: String,
    /// 文本来源
    pub source: crate::server::core::prompt::PromptSource,
    /// 指定了文件但读不到时的原因（`None` = 没这回事）
    pub file_error: Option<String>,
}

/// 三档保留天数的默认值（缺失时用它们）
pub const DEFAULT_LOG_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_REQUEST_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_DAILY_RETENTION_DAYS: i64 = 365;

/// **历史**路由优先级键：`{"workbuddy": 10, "raccoon": 20}`。
///
/// provider 路由优先级已随「账号全局一条队列」下线（先用哪一家由账号优先级
/// 决定）。这个键只在账号存储的启动迁移里读一次（`legacy_provider_route`），
/// 用来把旧版「按家分队」的号码按旧的实际顺序合并成全局队列；不再有写侧，
/// 文件里残留的值也不会被抹掉（未知字段全量保留）。
pub const KEY_PROVIDER_ROUTE: &str = "providerRoute";

/// 天数的合法范围：下限 1 天（保留 0 天等于什么都不存，不是有效配置），
/// 上限 10 年（防手改 config.json 写个天文数字让裁剪逻辑空转）。
///
/// **写侧（`stats_api::parse_days`）与读侧（`days_field`）共用这两个常量**：
/// 若两边各写一套数字，手改文件与走接口设值就会出现两套口径
/// （比如接口拒绝 5000 而读侧接受它）。两侧的处理方式不同是有意的：
/// 走接口的非法值给 400（用户当场能改），手改文件的非法值回落到默认（不打扰）。
pub const RETENTION_MIN_DAYS: i64 = 1;
pub const RETENTION_MAX_DAYS: i64 = 3650;

/// 三档保留天数（设置页「数据保留」区域）。
///
/// 用独立结构而不是三个散落的取值函数：三个值总是一起用（GET 一起返回、
/// 裁剪时各自取用），打包成一个 `Copy` 值让调用方一次拿到、不必多次读锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionSettings {
    /// 事件日志保留天数
    pub log_days: i64,
    /// 请求日志保留天数
    pub request_days: i64,
    /// 按天聚合保留天数
    pub daily_days: i64,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            log_days: DEFAULT_LOG_RETENTION_DAYS,
            request_days: DEFAULT_REQUEST_RETENTION_DAYS,
            daily_days: DEFAULT_DAILY_RETENTION_DAYS,
        }
    }
}

/// 保留期的**部分**更新入参（PUT /api/retention 允许只传其中几项）。
/// `None` = 这一项不动（对应「允许部分字段」的契约）。
#[derive(Clone, Copy, Debug, Default)]
pub struct RetentionPatch {
    pub log_days: Option<i64>,
    pub request_days: Option<i64>,
    pub daily_days: Option<i64>,
}

// ─── 定时任务设置的键名与边界（config.json 里的字段名**就是契约**）─────
//
// 间隔型任务（凭证维护 / 模型刷新 / 软件版本检查 / 两个前端自动刷新）打包在
// `scheduledTasks` 对象下；自动签到不在其中 —— 它是**每天定点**型，时刻与上次
// 执行结果由 `core::auto_checkin` 自己管（`autoCheckin` 字段），本模块不重复持有。
// 余额查询已退役成每账号各自配置的自动查询（`core::usage_query`），不在其中。
// 页面上的这些间隔型任务是「同一个形状」，所以配置也写成同一形状，
// 免得读侧要按任务名各写一套解析。

/// 间隔型任务的配置对象键
pub const KEY_SCHEDULED_TASKS: &str = "scheduledTasks";
/// 凭证自动维护在 `scheduledTasks` 下的子键
pub const KEY_CREDENTIAL_MAINTENANCE: &str = "credentialMaintenance";
/// 模型目录定时刷新在 `scheduledTasks` 下的子键
pub const KEY_MODEL_REFRESH: &str = "modelRefresh";
/// 日志页自动刷新在 `scheduledTasks` 下的子键（**前端**定时器，后端只存配置）
pub const KEY_LOGS_AUTO_REFRESH: &str = "logsAutoRefresh";
/// 请求日志页自动刷新在 `scheduledTasks` 下的子键（同上）
pub const KEY_REQUESTS_AUTO_REFRESH: &str = "requestsAutoRefresh";
/// 报表页自动刷新在 `scheduledTasks` 下的子键（同上）
pub const KEY_REPORT_AUTO_REFRESH: &str = "reportAutoRefresh";
/// 软件版本检查在 `scheduledTasks` 下的子键（后端定时向 GitHub 查最新发布版本）
pub const KEY_UPDATE_CHECK: &str = "updateCheck";

/// 凭证维护默认间隔（分钟）：与改造前的硬编码 600 秒一致
pub const DEFAULT_CREDENTIAL_MAINTENANCE_MINUTES: i64 = 10;
/// 模型目录定时刷新默认间隔（分钟）。
///
/// 保守取值：WorkBuddy 的 `/v3/config` 拉取**没有 TTL 早退**，每一轮都是真打
/// 上游（见 `providers::workbuddy` 的 `refresh_models`），间隔太密等于给上游
/// 添无谓的负载。一小时的粒度对「模型清单变了没」这个问题足够。
pub const DEFAULT_MODEL_REFRESH_MINUTES: i64 = 60;
/// 两个前端自动刷新的默认间隔（秒）：每秒一次。
///
/// 比改造前的硬编码 10 秒密得多，这是有意的：两条任务都只在**对应页面可见时**
/// 才请求（`document.hidden` 与当前页都判过），离开页面就完全静默，所以
/// 「密」的代价只落在用户正盯着那一页的时候 —— 而那正是他想要实时的时刻。
/// 两条接口都是本地读写（一条读日志库、一条查统计库），不出网。
pub const DEFAULT_LOGS_AUTO_REFRESH_SECONDS: i64 = 1;
pub const DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS: i64 = 1;
pub const DEFAULT_REPORT_AUTO_REFRESH_SECONDS: i64 = 1;
/// 软件版本检查默认间隔（分钟）：每 20 分钟查一次 GitHub 最新发布。
///
/// 20 分钟 = 3 次/小时，相对匿名限额（60 次/小时，且**按出口 IP 计** —— 同一
/// 出口下的其它程序共用这份额度）留足余量，又能让新版本的提示在一刻钟量级内
/// 出现。**这个默认值只影响「没配过间隔」的情形**：已经保存过
/// `scheduledTasks.updateCheck.interval` 的配置按原值跑（`interval_field`
/// 只在键缺失或越界时才回落到默认），所以调整它不会改写老用户的设置。
pub const DEFAULT_UPDATE_CHECK_MINUTES: i64 = 20;

/// 间隔型任务的取值范围。上下限分两套（分钟 / 秒），因为两类任务的合理区间
/// 差着量级：后端维护任务按分钟（1 分钟～1 天），前端刷新按秒（1 秒～10 分钟）。
///
/// 秒级下限放到 1 秒：这两条任务是**页面可见才跑**的本地轮询（不出网、不打上游），
/// 密一点最坏是「多读几次本地库里的一页数据」，不会给任何外部服务添负担。
/// 原来的 5 秒下限没有技术理由，只是照着改造前的 10 秒兜底值随手划的。
///
/// 与保留期同样：**写侧（`scheduled_tasks::parse_interval`）与读侧
/// （`interval_field`）共用这些常量**，否则会出现「接口拒绝 60 而手改文件接受它」。
pub const INTERVAL_MIN_MINUTES: i64 = 1;
pub const INTERVAL_MAX_MINUTES: i64 = 1440;
pub const INTERVAL_MIN_SECONDS: i64 = 1;
pub const INTERVAL_MAX_SECONDS: i64 = 600;

/// 一个间隔型任务的配置：开关 + 间隔（单位由任务定义决定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalTask {
    pub enabled: bool,
    /// 间隔值，单位见任务定义（分钟或秒）
    pub interval: i64,
}

/// 六条间隔型任务的配置（设置页「定时任务」区域）。
///
/// 与 `RetentionSettings` 同一取舍：几个值总是一起用（GET 一次返回、各自循环
/// 各取所需），打包成一个 `Copy` 值让调用方一次拿到、不必多次读锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledSettings {
    pub credential_maintenance: IntervalTask,
    pub model_refresh: IntervalTask,
    pub logs_auto_refresh: IntervalTask,
    pub requests_auto_refresh: IntervalTask,
    pub report_auto_refresh: IntervalTask,
    pub update_check: IntervalTask,
}

impl Default for ScheduledSettings {
    fn default() -> Self {
        Self {
            credential_maintenance: IntervalTask {
                enabled: true,
                interval: DEFAULT_CREDENTIAL_MAINTENANCE_MINUTES,
            },
            model_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_MODEL_REFRESH_MINUTES,
            },
            logs_auto_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_LOGS_AUTO_REFRESH_SECONDS,
            },
            requests_auto_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS,
            },
            report_auto_refresh: IntervalTask {
                enabled: true,
                interval: DEFAULT_REPORT_AUTO_REFRESH_SECONDS,
            },
            update_check: IntervalTask {
                enabled: true,
                interval: DEFAULT_UPDATE_CHECK_MINUTES,
            },
        }
    }
}

/// 一条间隔型任务的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Copy, Debug, Default)]
pub struct IntervalTaskPatch {
    pub enabled: Option<bool>,
    pub interval: Option<i64>,
}

// ─── 请求重试设置的键名与边界（config.json 里的字段名**就是契约**）─────
//
// 转发层的退避重试读这三个值（见 `upstream::provider_loop::send_with_retry`
// 与 `attempt_queue`）。
//
// ── 为什么分两档次数（同账号原地重发 / 换账号）──────────────
// 一次转发失败后的处置有两条路，代价与收益完全不同：
//   - **在同一个账号上原地重发**：便宜，可能只是瞬时抖动或敏感词误拦，
//     等一个间隔再发一次往往就好了 —— 这是 `retryCount`；
//   - **换一个账号再试**：换号要走另一份额度与限流，还可能是另一家，
//     用户往往希望「先在当前账号多试几次，实在不行再换号」—— 这是
//     `retryCrossProviderCount`。
//
// 判定口径（`provider_loop::attempt_queue` 记账）：
//   - 请求**首次选中的那个账号**用 `retryCount`，原地重发几次；
//   - **每换一个账号**（不分是同家的下一个还是另一家的）扣一次
//     `retryCrossProviderCount`，扣满就带着最后一次的错误收尾，
//     不再往下顺延。
//
// ── 第二档为什么按「账号」而不是按「提供商」（2026-09 修正）────
// 旧实现按提供商分段：同一家名下的所有账号算一段、共用一份原地重发预算，
// 只有跨家才重新给一份。这有两个后果，都与用户对这个数字的预期不符：
//   - 「能换几个账号」根本没有任何设置管 —— 只受队列里账号总数限制，
//     某家囤了 9 个账号时，一次请求会一路试到第 10 个（用户实测截图）；
//   - 用户填的 5 只在「跨家之后」生效，而跨家本身已经是队列走完的副产品，
//     等到那时往往早就没有可用账号了。
// 现在改成「换账号次数」：不管换到哪一家，换一次扣一次。键名保留
// `retryCrossProviderCount` 不变（改名会让老配置读不到、回落默认值，
// 得额外做迁移，不划算）—— 键名是历史包袱，语义以本节为准。

/// **同一账号内**的原地重发次数（0 = 失败立即换号，不重发）
pub const KEY_RETRY_COUNT: &str = "retryCount";
/// **换账号**的次数（0 = 不换号，直接收尾）。
///
/// ── 常量名与 JSON 键名为什么对不上 ─────────────────────────
/// 键名（`retryCrossProviderCount`）是**配置契约**：改名会让改了名的老配置
/// 读不到、静默回落成默认值，还得额外做一次迁移，不划算 —— 所以字符串
/// 原样冻结。常量名（Rust 侧标识符）说的才是真语义：**按账号计，不分家**，
/// 换到同家的下一个账号与换到另一家都算一次。语义详见本节开头的说明。
pub const KEY_RETRY_ACCOUNT_SWITCH_COUNT: &str = "retryCrossProviderCount";
/// 两次重试之间的等待秒数
pub const KEY_RETRY_INTERVAL_SECONDS: &str = "retryIntervalSeconds";

/// 同一账号的原地重发次数默认值：失败后再试 3 次（连同首发共 4 次发送）
pub const DEFAULT_RETRY_COUNT: i64 = 3;
/// 换账号的次数默认值：失败后最多换 5 个账号。
///
/// 比原地重发那档更宽是刻意的：换号的成本主要在「等到下一个可用账号」，
/// 而一个账号失败往往说明它这份额度确实不通，多换几个比快速失败更符合预期。
pub const DEFAULT_RETRY_SWITCH_COUNT: i64 = 5;
/// 重试间隔默认值：5 秒
pub const DEFAULT_RETRY_INTERVAL_SECONDS: i64 = 5;

/// 「指定错误码直接换号」的配置键（值是 HTTP 状态码数组，如 `[402, 429]`）。
///
/// 键名里的 `NoRetry` 是历史措辞（最早的语义是「命中即报错」），行为后来
/// 改成了「不在同一账号重发、直接换下一个账号」—— 键名是配置契约，改名
/// 会让老配置读不到，保留至今。命中名单的上游失败跳过本账号：原地重发与
/// 同账号补救（内容拦截换提示词 / 401 刷新）都不做，按队列换下一个账号
/// 继续试，换满仍失败才把错误给客户端。
pub const KEY_RETRY_NO_RETRY_CODES: &str = "noRetryStatusCodes";
/// 默认名单：402（WorkBuddy 积分不足）。余额问题重发结论不变，
/// 客户端拿到 402 才能如实体感「这个账号没钱了」。
pub const DEFAULT_NO_RETRY_CODES: &[u16] = &[402];
/// 名单里状态码的合法范围：HTTP 状态码本身就定义在 100–599
pub const RETRY_CODE_MIN: u16 = 100;
pub const RETRY_CODE_MAX: u16 = 599;
/// 名单长度上限：状态码总共就 500 个，50 项足够表达任何配置，
/// 也防止一次粘贴把界面和 config.json 撑爆
pub const RETRY_MAX_NO_RETRY_CODES: usize = 50;

/// 次数与间隔的合法范围。
///
/// 上限 10 次 / 300 秒：次数过多或间隔过长都会让客户端干等（重试是「再发一次」，
/// 换号也是「换个人再发一次」，都不是把请求拆成多次）。下限 0：次数 0 = 关闭
/// 该档（不重发 / 不换号），间隔 0 = 立即重发。
pub const RETRY_MIN_COUNT: i64 = 0;
pub const RETRY_MAX_COUNT: i64 = 10;
pub const RETRY_MIN_INTERVAL_SECONDS: i64 = 0;
pub const RETRY_MAX_INTERVAL_SECONDS: i64 = 300;

/// 请求重试设置（设置页「通用 → 请求重试」区域）。
///
/// 与 `RetentionSettings` 同一取舍：几个值总是一起用（转发层每次重试判定
/// 都取），打包成一个快照值让调用方一次拿到、不必多次读锁。
/// 曾经是 `Copy` 的；`no_retry_codes` 加进来后共享列表只能 `Clone`
/// （`Arc` 本身不是 Copy）—— 快照克隆只多一次指针自增，热路径无感。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetrySettings {
    /// **同一账号内**的原地重发次数（0 = 失败立即换号）
    pub count: i64,
    /// **换账号**的次数上限（0 = 不换号，直接收尾）。
    ///
    /// 字段名保留了旧措辞、JSON 键名也没变（见
    /// [`KEY_RETRY_ACCOUNT_SWITCH_COUNT`]）—— 键名是配置契约，改名会让老配置
    /// 读不到。此处标识符说的是真语义：**按账号计，不分家**，换到同家的下一个
    /// 账号与换到另一家都算一次。
    pub account_switch_count: i64,
    /// 两次重试之间的间隔（秒）
    pub interval_seconds: i64,
    /// **指定错误码直接换号**名单（[`KEY_RETRY_NO_RETRY_CODES`]）。
    ///
    /// 为什么是 `Arc<[u16]>` 而不是 `Vec<u16>`：快照被逐失败请求取用，
    /// `Arc` 让克隆只付一次指针自增；判定（`no_retry`）读的是共享切片，
    /// 不需要任何锁。
    pub no_retry_codes: Arc<[u16]>,
}

impl RetrySettings {
    /// 间隔的毫秒形态（转发层的 sleep 直接用）
    pub fn delay_ms(&self) -> u64 {
        self.interval_seconds.max(0) as u64 * 1000
    }

    /// 同一账号的原地重发预算（与 [`Self::switch_budget`] 是两个独立的口径）。
    ///
    /// 负值按 0 处理：`bounded_int_field` 已保证范围，这里是防御性的
    /// （负数转 `usize` 会回绕成天文数字，那会让重试变成死循环）。
    pub fn resend_budget(&self) -> usize {
        self.count.max(0) as usize
    }

    /// 换账号的次数上限（换一次扣一次，扣满即收尾）。
    pub fn switch_budget(&self) -> usize {
        self.account_switch_count.max(0) as usize
    }

    /// 这个上游状态码是否命中「直接换号」名单。
    pub fn no_retry(&self, status: u16) -> bool {
        self.no_retry_codes.contains(&status)
    }
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            count: DEFAULT_RETRY_COUNT,
            account_switch_count: DEFAULT_RETRY_SWITCH_COUNT,
            interval_seconds: DEFAULT_RETRY_INTERVAL_SECONDS,
            no_retry_codes: Arc::from(DEFAULT_NO_RETRY_CODES),
        }
    }
}

/// 请求重试的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Debug, Default)]
pub struct RetryPatch {
    pub count: Option<i64>,
    pub account_switch_count: Option<i64>,
    pub interval_seconds: Option<i64>,
    pub no_retry_codes: Option<Vec<u16>>,
}

// ─── 上游请求超时（四个阶段，对应 OmniProxy 的同名设置）──────────────

/// 连接超时：建立上游 TCP/TLS 连接或代理隧道的最大等待时间（秒）
pub const KEY_TIMEOUT_CONNECT_SECONDS: &str = "connectTimeoutSeconds";
/// 等待响应超时：请求发出后等待上游响应头的最大时间（秒）
pub const KEY_TIMEOUT_HEADERS_SECONDS: &str = "headersTimeoutSeconds";
/// 流式响应空闲超时：流式响应相邻数据之间允许的最大空闲时间（秒），收到新数据后重新计时
pub const KEY_TIMEOUT_STREAM_IDLE_SECONDS: &str = "streamIdleTimeoutSeconds";
/// 非流式响应超时：读取完整非流式响应体允许的最大时间（秒）
pub const KEY_TIMEOUT_BODY_SECONDS: &str = "bodyTimeoutSeconds";

/// 连接超时默认值：30 秒（与 egress 里原先的硬编码值一致）
pub const DEFAULT_TIMEOUT_CONNECT_SECONDS: i64 = 30;
/// 等待响应超时默认值：300 秒（与 request.rs 原先的 HEADERS_TIMEOUT_MS 一致）
pub const DEFAULT_TIMEOUT_HEADERS_SECONDS: i64 = 300;
/// 流式空闲超时默认值：300 秒（与 OmniProxy 的 stream_idle_timeout 一致）
pub const DEFAULT_TIMEOUT_STREAM_IDLE_SECONDS: i64 = 300;
/// 非流式响应超时默认值：300 秒（与 OmniProxy 的 body_timeout 一致）
pub const DEFAULT_TIMEOUT_BODY_SECONDS: i64 = 300;

/// 四项超时的合法范围（秒）：与 OmniProxy 的 1~3600 逐字一致。
///
/// 下限 1 而不是 0：0 在这里没有合理语义（「立即超时」等于禁用转发，
/// 想禁用某一阶段保护的人其实要的是把它调大到上限）。
pub const TIMEOUT_MIN_SECONDS: i64 = 1;
pub const TIMEOUT_MAX_SECONDS: i64 = 3600;

/// 上游请求超时（设置页「通用 → 请求超时」区域）。
///
/// 四个阶段各一个值，与 OmniProxy 的 connect / headers / stream_idle / body
/// 一一对应；转发层逐请求取一次快照（`Copy`，四个 i64）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeoutSettings {
    /// 建连（含到代理的那一段）
    pub connect_seconds: i64,
    /// 请求发出 → 响应头到达
    pub headers_seconds: i64,
    /// 流式响应相邻数据之间的最大空闲
    pub stream_idle_seconds: i64,
    /// 非流式响应体读完的总预算
    pub body_seconds: i64,
}

impl TimeoutSettings {
    /// 负数 / 0 按最小值兜底（防御性：读侧已由 bounded_int_field 保证范围，
    /// 转 `u64` 前必须挡住，否则回绕成天文数字）
    fn ms(seconds: i64) -> u64 {
        seconds.max(TIMEOUT_MIN_SECONDS) as u64 * 1000
    }

    pub fn connect_ms(&self) -> u64 {
        Self::ms(self.connect_seconds)
    }

    pub fn headers_ms(&self) -> u64 {
        Self::ms(self.headers_seconds)
    }

    pub fn stream_idle_ms(&self) -> u64 {
        Self::ms(self.stream_idle_seconds)
    }

    pub fn body_ms(&self) -> u64 {
        Self::ms(self.body_seconds)
    }

    /// 传输层 read_timeout 的后备上限：取「等响应头」与「流空闲」两者的大者。
    ///
    /// reqwest 的 `read_timeout` 作用于每一次读（既覆盖首包前、也覆盖数据块
    /// 之间），而这两个阶段是分开的旋钮 —— 后备值取大者，保证它**永不**成为
    /// 哪个旋钮的隐藏天花板（真正的判定在各阶段自己的计时器，见
    /// `upstream::request` 与 `upstream::ForwardStream`）。
    pub fn read_timeout_backstop_ms(&self) -> u64 {
        self.headers_ms().max(self.stream_idle_ms())
    }
}

impl Default for TimeoutSettings {
    fn default() -> Self {
        Self {
            connect_seconds: DEFAULT_TIMEOUT_CONNECT_SECONDS,
            headers_seconds: DEFAULT_TIMEOUT_HEADERS_SECONDS,
            stream_idle_seconds: DEFAULT_TIMEOUT_STREAM_IDLE_SECONDS,
            body_seconds: DEFAULT_TIMEOUT_BODY_SECONDS,
        }
    }
}

/// 四项超时的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeoutPatch {
    pub connect_seconds: Option<i64>,
    pub headers_seconds: Option<i64>,
    pub stream_idle_seconds: Option<i64>,
    pub body_seconds: Option<i64>,
}

// ─── 排队等待（走排队制的上游：目前只有 Qoder 的免费模型）─────────────

/// 排队时最多等待几次（0 = 不等待，直接把排队态作为错误返回）
pub const KEY_QUEUE_MAX_WAITS: &str = "queueMaxWaits";
/// 单次排队等待的秒数（0 = 跟随上游建议值）
pub const KEY_QUEUE_WAIT_SECONDS: &str = "queueWaitSeconds";

/// 默认等待次数：2 次。
///
/// 与参考实现（CLIProxyAPI 的 qoder2api 插件 `queue_max_waits` 默认值）取同一
/// 档：上游给的建议间隔实测 9～30 秒，两次最多等一分钟 —— 落在「用户还能接受
/// 的首字等待」与「默认 300 秒的等待响应超时」之间。
pub const DEFAULT_QUEUE_MAX_WAITS: i64 = 2;
/// 默认单次等待秒数：0 = 跟随上游建议（上游没给建议时适配器退到 15 秒）
pub const DEFAULT_QUEUE_WAIT_SECONDS: i64 = 0;

/// 等待次数的合法范围 0~10：0 = 关闭等待（排队即报错）。上限 10 是因为
/// 「等一会儿再发」等得太多次不如让客户端自己决定重试 —— 它会收到一条写清
/// 「排队中、不是登录态或额度问题」的错误，而不是一个看不出所以然的超时。
pub const QUEUE_MIN_MAX_WAITS: i64 = 0;
pub const QUEUE_MAX_MAX_WAITS: i64 = 10;
/// 单次等待秒数的合法范围 0~120（0 = 跟随上游建议）
pub const QUEUE_MIN_WAIT_SECONDS: i64 = 0;
pub const QUEUE_MAX_WAIT_SECONDS: i64 = 120;

/// 排队等待设置（设置页「通用 → 排队等待」区域）。
///
/// 与 `TimeoutSettings` 同一取舍：两个值总是一起用（每次排队判定取一份快照），
/// 打包成一个 `Copy` 值让调用方一次拿到。
///
/// ── 它影响谁 ────────────────────────────────────────────────
/// 只有**走排队制的适配器**读它（目前是 Qoder：免费模型繁忙时上游用 403 +
/// 业务码 10605 回一句「暂不可服务，建议 N 秒后再来」）。其它家没有排队语义，
/// 这份设置对它们无影响。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueSettings {
    /// 最多等待次数（0 = 不等待）
    pub max_waits: i64,
    /// 单次等待秒数（0 = 跟随上游建议值）
    pub wait_seconds: i64,
}

impl QueueSettings {
    /// 等待次数预算（负值按 0：读侧已保证范围，这里是防御性的）
    pub fn wait_budget(&self) -> usize {
        self.max_waits.clamp(QUEUE_MIN_MAX_WAITS, QUEUE_MAX_MAX_WAITS) as usize
    }

    /// 强制单次等待时长（毫秒）；`None` = 跟随上游建议
    pub fn forced_wait_ms(&self) -> Option<u64> {
        let seconds = self.wait_seconds.clamp(QUEUE_MIN_WAIT_SECONDS, QUEUE_MAX_WAIT_SECONDS);
        if seconds > 0 {
            Some(seconds as u64 * 1000)
        } else {
            None
        }
    }
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            max_waits: DEFAULT_QUEUE_MAX_WAITS,
            wait_seconds: DEFAULT_QUEUE_WAIT_SECONDS,
        }
    }
}

/// 排队等待的**部分**更新入参（`None` = 该项不动）。
#[derive(Clone, Copy, Debug, Default)]
pub struct QueuePatch {
    pub max_waits: Option<i64>,
    pub wait_seconds: Option<i64>,
}
