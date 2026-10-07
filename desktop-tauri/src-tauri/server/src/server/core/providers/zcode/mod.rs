//! ZCode（智谱 / Z.AI 的编码代理客户端）适配：**国内版 / 国际版**两个 provider。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//! ZCode 的「编码套餐」（Coding Plan / Start Plan）可以在客户端里用**订阅
//! 登录态**调用，不必去开放平台申请 API Key。本家复刻的就是这条链路：
//!
//! ```text
//!   zcode 平面（登录 / 领取 / 客户端配置）        推理平面
//!   https://zcode.z.ai                     国内 → https://open.bigmodel.cn
//!   （两地相同）                            国际 → https://api.z.ai
//! ```
//!
//! 推理是 **OpenAI 兼容**协议（`{openai_base}/chat/completions`），
//! 与 raccoon / autoclaw 同构（无状态、Bearer 鉴权、SSE 回写模型名），
//! 因此本家不需要 `is_stateful` 那条「适配器自己发一次请求」的路子。
//!
//! ── 两条通道：编码套餐 / 活动套餐（**按账号选**）──────────────
//! 同一份订阅登录态可以走两条上游通道，它们**不通用**：
//!
//! | 账号里的 `plan` | 上游 | 鉴权 | 协议 |
//! |---|---|---|---|
//! | `coding-plan`（默认） | `{openai_base}/chat/completions` | `accessToken` | OpenAI Chat |
//! | `start-plan` | `{zcode}/api/v1/zcode-plan/anthropic/v1/messages` | 套餐 `jwt` | Anthropic Messages |
//!
//! 为什么必须让用户选：**编码套餐与活动套餐是两份独立的额度**，而
//! 「套餐已到期」这类拒绝只由其中一条通道给出（用户实测：编码套餐到期后，
//! 走 OpenAI 通道恒回 429「您的GLM Coding Plan套餐已到期」，而当天领到的
//! 活动额度只能从 `zcode.z.ai` 的 anthropic 端点花掉）。两条通道的
//! URL / 协议 / 系统提示词要求全在 [`plan`] 里，本文件只放「选哪条」的判据。
//!
//! ── 与前端预设 `glm` / `glm-cn` 的分工（别当成重复建设）──────
//! `ui/preset-providers.js` 里已有两项预设指向同一批端点，但那条路要用户
//! 自己提供 **API Key**（走开放平台计费）。本家补的是**订阅登录态**那条通道：
//! OAuth 登录 → JWT → 转发。两者并存，各取所需。
//!
//! ── 本家没有签到，替代它的是「限时套餐领取」────────────────
//! 其余各家都是每日签到（`core::auto_checkin`），ZCode 的运营玩法是限时发放的
//! 体验套餐：官方定期换一期（周末套餐 / Global Build / **ZCode Trust Build**），
//! 活动窗口里账号可以领一份额度。因此本家给这一行的是「领套餐」而不是「签到」。
//!
//! **2026-09-28 那期是「每天领一次」**：套餐 id 带日期段
//! （`zcode-v3-start-plan-trust-0928`），所以活动期内每天都会出现一个新套餐、
//! 每天都能领一次（同一期的第二次领取回 `1003 already claimed`）。界面据此
//! 按自然日显示「今日已领」，见 `core::account_store::StoredAccount::claim_at`。
//!
//! ── 子模块与当前进度 ────────────────────────────────────────
//!   region.rs       地区（域名 / 身份 / 环境变量 / 账号 id 前缀）—— 已完成
//!   models.rs       模型清单（静态表，两地共用）—— 已完成
//!   claim.rs        限时套餐领取（探测 / 领取 / 失败分类 / 调度语义）—— 已完成
//!   captcha.rs      活动套餐通道的人机验证令牌池（界面铸造 → 网关消费）—— 已完成
//!   balance.rs      套餐余额（`billing/balance`：余额桶 + 套餐 + 到期）—— 已完成
//!   plan.rs         活动套餐通道（系统提示词块 + Anthropic 协议 + JWT 鉴权）—— 已完成
//!   reasoning.rs    GLM-5.3 家族的思考等级契约（等级 ↔ 预算、预算与 max_tokens 配对）—— 已完成
//!   adapter.rs      `ProviderAdapter` 实现（按账号的两条通道 + 余额接线）—— 已完成
//!   credentials.rs  凭证（访问令牌 + 套餐 JWT + 设备标识）—— 已完成
//!   oauth.rs        CLI 轮询登录（init / poll / 授权地址中转页）—— 已完成
//!   coding_key.rs   编码套餐凭证换取（OAuth 令牌 → 能用于推理的 API Key）—— 已完成
//!
//! 登录链已接进 `core/login/zcode.rs`（`start_zcode_login` / `run_zcode_login`，
//! 与 Qoder 那支同构：启动任务 → 轮询 → 落账号），界面上「添加账号」的两个
//! 入口（网页登录 / 填写凭证）都可用。
//!
//! ── 三件**已知未做**的事（别误以为已经覆盖）───────────────────
//!   1. **凭证续期**：上游没有续期协议 —— OAuth 令牌用坏了只能重新登录
//!      （参考实现实测：JWT 只带 `iat` 不带 `exp`，8 天前的仍能用，过期只以
//!      401 暴露）。适配器的 `refresh_access_token` 因此如实报错，见 `adapter.rs`。
//!   2. **客户端请求签名**（Client Request Signing V4）：上游的
//!      `GET {zcode}/api/v1/agent/configs` 此刻回 `codingPlanSignature.enable: true`，
//!      官方客户端会给编码套餐的推理请求加 Ed25519 签名 + 工作量证明头。
//!      参考实现对它**全程 fail-open**（握手失败、连续两次 401 `VERIFY_*`
//!      都退回未签名），且实测未签名请求的拒绝理由是鉴权（`Authentication
//!      Failed`）而不是签名 —— 因此本家先不做，靠 `coding_key` 把凭证换对。
//!      哪天真被 `VERIFY_SIGNATURE_INVALID` 拒了，再照参考实现的
//!      `src/proxy/client-signing.ts` 补。
//!   3. ~~活动套餐通道上的人机验证~~ —— **2026-10-01 起改为「听上游的」**：
//!      2026-09-28 实测推理端点缺 `X-Aliyun-Captcha-Verify-Param` 一律回
//!      `400 {"code":3007}`，于是当时做成了「桌面端 WebView 静默铸令牌 →
//!      `POST /api/zcode/captcha` 进池 → 转发层每条请求取一个」（见 [`captcha`]），
//!      headless / Docker 上这条通道因此不可用。后来上游在
//!      `GET /api/v1/client/configs` 里下发了
//!      `configs.captcha.skip_model_request: true`（2026-10-01 实测，与客户端版本
//!      无关），也就是说**推理请求**这一侧它此刻不要令牌了。现在这一格由
//!      [`config_sync`] 每 5 分钟同步、`plan.rs` 照它决定要不要带那两个头，
//!      上游回 3007 则当场翻回「要码」（代价被钉在最多一次往返）。
//!      **仍然没解决的一半**：上游把这一格改回 `false` 之后，headless 部署就又
//!      用不了这条通道了 —— 铸令牌仍然只有 WebView 会。

use serde_json::Value;

use crate::server::core::prompt::GatewayBlocks;
pub mod adapter;
pub mod balance;
pub mod captcha;
pub mod claim;
pub mod coding_key;
pub mod config_sync;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod plan;
pub mod reasoning;
pub mod region;

/// 账号记录上的「用哪条通道」字段名（`accounts.json` 的键；界面读同一个名字）。
///
/// 为什么带 `zcode` 前缀而不是光叫 `plan`：账号记录是**各家共用**的一张形状
/// （`StoredAccount` 的裸字段表），`plan` 这个词在别家（Cline 的套餐、
/// Trae 的 planType）都有各自含义，裸名迟早撞车；带前缀之后前端、存储、
/// 适配器三处读的是同一个字符串，不存在映射表。
pub const PLAN_FIELD: &str = "zcodePlan";

/// 编码套餐通道（默认值）：开放平台的 OpenAI 兼容端点 + `accessToken`
pub const PLAN_CODING: &str = "coding-plan";

/// 活动套餐通道：`zcode.z.ai` 的 Anthropic 端点 + 套餐 `jwt`（见 [`plan`]）
pub const PLAN_START: &str = "start-plan";

/// 「这家有一段**网关自带**的提示词」的说明（设置页逐家提示词那张表用它）。
///
/// 事实依据是 2026-09-28 对上游的实测（五条对照只差 system 的形态）：
///
/// | system 形态 | 上游 |
/// |---|---|
/// | 官方三段各自成块 | 200 |
/// | 官方三段各自成块、去掉 `cache_control` | 200 |
/// | 官方三段的文本**并成一块** | 405 `3012 request has been blocked` |
/// | 客户端自带的 ZCode 提示词（单块） | 405 `3012` |
/// | 官方三段 + 客户端自带（接在后面） | 200 |
///
/// 也就是说：上游按**结构**（那三段必须各自成块、排在最前）认身份，不认文本、
/// 也不认 `cache_control`；且客户端自己的提示词替代不了它。**默认**因此是
/// 「装上」——那是存量行为、也是当前能跑通的那条路。
///
/// 但它是**可关的**（`KEY_PROMPT_GATEWAY`）：上游的口径是它自己的实现细节，
/// 哪天真放开了、或者用户有别的路子，这个开关就是出口。所以这段文字写的是
/// 「关掉会发生什么 + 依据来自哪次实测」，而不是「不许关」。
pub const OFFICIAL_PROMPT_NOTE: &str = "\
这段是**网关自己装上去**的，不来自客户端、也不受「模式 / 提示词文件」影响：\
活动套餐通道（start-plan）的上游按**结构**校验身份 —— 实测（2026-09-28）官方三段各自成块 \
200、三段并成一段 405/3012、用客户端自带的 ZCode 提示词顶替 405/3012。\
所以默认装上；关掉之后上游认不认，取决于它当前的口径，需要重新实测才知道。\
另外：客户端自己的 system 怎么处理仍由「模式 / 提示词文件」决定（透传 = 客户端那份跟在官方三段之后）。";

/// [`OFFICIAL_PROMPT_NOTE`] 那段装配的字符数（设置页只读子行的「约 N 字符」）。
///
/// 只算**静态文本**（身份句 + 稳定段 + 动态段的两端），不含逐请求生成的
/// Environment 段（工作目录、平台、模型名）—— 那一段的长度随机器与模型名变，
/// 报一个固定的数才是错的。界面上的措辞因此是「约」。
pub fn official_prompt_approx_chars() -> usize {
    plan::official_prompt_approx_chars()
}

/// [`OFFICIAL_PROMPT_NOTE`] 那段装配的**正文模板**（三段，Environment 段带占位符）。
///
/// 设置页的编辑器拿它当「官方原文」：用户改过的那一段，发请求时用他的文本
/// （占位符照旧换成真实运行值：工作目录 / 平台 / shell / 模型名），没改的段继续
/// 用这份 —— 逐段合并的口径见 `core::prompt::GatewayBlocks::or`。资源坏了给
/// `None`（界面此时不提供编辑入口，免得用户拿一份空文本把官方原文覆盖掉）。
pub fn official_prompt_blocks() -> Option<GatewayBlocks> {
    plan::official_blocks_template()
}

/// 归一化界面/接口给的通道取值：只认上面两个常量，其余（含空值）返回 None。
///
/// 取值口径与 `claim.rs` 的 `planId` 一样**不猜**：认不出的值一律拒绝，
/// 而不是静默回落到默认通道 —— 那会让用户以为切过去了，实际还在用旧通道。
pub fn normalize_plan(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        PLAN_CODING => Some(PLAN_CODING),
        PLAN_START => Some(PLAN_START),
        _ => None,
    }
}

/// 账号（**公开形态**：`ZcodeAdapter::build_chat_request` 拿到的会话、
/// `provider_loop` 分派点的账号对象都算）用哪条通道。
///
/// 缺失/认不出 = [`PLAN_CODING`]：这是存量账号的默认，也是「添加账号后直接能用」
/// 的那条路（默认行为因此与本次改造之前逐字节相同）。
pub fn plan_of(account: &Value) -> &'static str {
    account
        .get(PLAN_FIELD)
        .and_then(Value::as_str)
        .and_then(normalize_plan)
        .unwrap_or(PLAN_CODING)
}

/// 通道的展示名：账号页的下拉、保存后的变更提示、错误文案共用这一处措辞
/// （三处各写一遍的话，用户会在弹窗里看到「编码套餐」、在日志里看到
/// 「Coding Plan」，对不上是同一条设置）。
pub fn plan_label(plan: &str) -> &'static str {
    match plan {
        PLAN_START => "活动套餐（Start Plan）",
        _ => "编码套餐（Coding Plan）",
    }
}

