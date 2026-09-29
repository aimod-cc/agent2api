//! Trae（字节跳动 AI IDE）提供商。
//!
//! ── 这一家在网关眼里长什么样 ──────────────────────────────
//! 上游不是 OpenAI 兼容端点，而是 `trae-api-cn.mchost.guru` 上的
//! `POST /api/agent/v3/llm_utils_chat`：请求体是"白名单重建"的自定义信封
//! （见 `payload.rs`），响应恒是 SSE 且**业务错误塞在 200 的流里**
//! （见 `stream.rs` 与 `errors.rs`）。所以本家与 accio / codearts 一样走
//! stateful 路径，由自己掌控出站与流解析。
//!
//! 与 codearts 最大的差别是**这里没有签名层**：鉴权就是一个
//! `Cloud-IDE-JWT <token>` 加一批身份头（`headers.rs`），唯一的密码学是
//! 登录时的 PKCE(S256)。省掉的正是 codearts 那边最贵的一块。
//!
//! ── 只支持 SOLO 这一条通道，是有意的 ──────────────────────
//! `llm_utils_chat` 只认 `function=solo_work_lite`。Trae 的另外两条通道
//! （IDE 的 `/api/agent/v3/create_agent_task`、`llm_raw_chat` 的 `solo_agent`）
//! 一条要 AES-256-GCM 加密请求体、一条被上游限成"只能由 agent 任务内部调用"，
//! 都不适合做透明代理。目录里属于那两条通道的配置会在解析阶段直接过滤
//! （`errors::config_is_solo_agent_only`），因为它们在本通道必定流内 4001。
//!
//! Intl（`core-normal.trae.ai` 的 `chat_sessions` → `events` 两步握手、
//! 累积 thought、`Origin` 绑定、无 tools）是**另一套协议**，单列里程碑，
//! 不与本模块共用信封。
//!
//! ── 事实来源 ─────────────────────────────────────────────
//! 全部判定逐条对齐参考实现 `cpa-multi-plugins/plugins/trae`（v0.12.95），
//! 并由 `vectors/trae-vectors.json` 钉住 —— 那份向量是参考实现自己算出来的
//! 答案（生成器在它的 `upstream` 包内），不是这里的人凭记忆写的。
//! 施工方案与实测记录见 `cpa-deploy/notes/agent2api-trae-port-plan.md`。

pub mod adapter;
pub mod callback_server;
pub mod checkin;
pub mod credentials;
pub mod device;
pub mod errors;
pub mod forward;
pub mod headers;
pub mod http;
pub mod login;
pub mod models;
pub mod oauth;
pub mod payload;
pub mod profile;
pub mod refresh;
pub mod stream;
pub mod usage;

// 适配器与账号存储在后续里程碑接入（M1 凭据 / M3 目录 / M4 转发 / M5 注册）。
#[allow(unused_imports)]
use errors::ErrorKind;

/// 上游主机。CN 与 SOLO 共用；`api.trae.cn` / `api.trae.com.cn` 是
/// OAuth 与积分那条链的 host（见 `credentials.rs`，M1）。
pub const AGENT_BASE_URL: &str = "https://trae-api-cn.mchost.guru";
pub const CHAT_PATH: &str = "/api/agent/v3/llm_utils_chat";
pub const MODELS_PATH: &str = "/api/ide/v1/get_detail_param";

/// 提供商 id（注册表与 `ProviderKind` 用同一条）。
pub const PROVIDER_ID: &str = "trae";
