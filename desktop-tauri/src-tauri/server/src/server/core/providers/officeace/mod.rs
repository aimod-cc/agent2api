//! OfficeAce（华为云果办 / OfficeClaw / jiuwenclaw）接入。
//!
//! ── 这条通道长什么样 ─────────────────────────────────────────
//! 上游**本身就是 OpenAI 兼容**：chat 打 `{model_api_url_base}/v2/chat/completions`，
//! 鉴权是一个 Basic 头（`model_app_key`:`model_app_secret`）。桌面端只是把这张表
//! 落在本机 `~/.office-claw/.jiuwenclaw/config/routing_state/users/<账号ID>/models.json`
//! —— 读文件，不是中间人抓包。
//!
//! 凭据**分两层**（别混）：
//!   · **网关凭据**：`model_app_key` / `model_app_secret`，给 chat 用，**不过期**；
//!   · **临时凭据**：`HSTA…` 的 AK/SK + `security_token` + `project_id`，**实测 2 小时**，
//!     只用来签控制面（问云端要模型表、续期）。
//!
//! 拿网关凭据的路有两条：自助 OAuth（PKCE + P-256 DPoP → 轮询取授权码 → STS 换临时
//! 凭据 → `client-permission-validate` 换网关凭据），或用户直接从别的机器导入
//! `api_base` + `Authorization`。
//!
//! ── 与 CodeArts 的关系 ───────────────────────────────────────
//! 同属华为云身份栈（OAuth PKCE + DPoP + SDK-HMAC-SHA256 + IAM v3），但**不是同一套**：
//! 签名规范化的三处差异见 [`signer`] 的模块头；上游 chat 面无状态（不需要 CodeArts
//! 那种并发会话闸门）。

pub mod adapter;
pub mod chat;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod probe;
pub mod signer;
