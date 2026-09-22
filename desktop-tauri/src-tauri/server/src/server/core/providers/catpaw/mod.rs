//! CatPaw（美团 AI 客户端）**协议层**：OpenAI 入站消息 → 上游 conversation
//! 协议的双向转换与有状态会话管理（Agent2API 二期，架构文档 §9）。
//!
//! ── 与其它 provider 的本质区别（为什么单独一个目录）──────────
//! WorkBuddy 与小浣熊都是「一次 HTTP 请求 = 一次对话」的无状态上游，
//! 适配器把请求头与 body 拼好交给转发层就够了（`build_chat_request`）。
//! CatPaw 不是：它的上游是一套 **conversation 会话协议**
//! （UPSTREAM_PROTOCOL.md 是唯一权威）：
//!
//! ```text
//! round(提交消息) → event(running) → turn(SSE 执行) → 工具循环
//!   → event(completed)      ← 每个轮次结束必须回报，否则下一轮 round 被拒
//! ```
//!
//! 一次客户端请求可能对应这里的好几个上游请求，中间还要维护
//! 「x-session-id → conversationId」的映射与已同步消息的指纹链（增量提交）。
//! 这些状态与多步时序**不能**塞进 `ProviderAdapter::build_chat_request`
//! 那种单请求契约里，所以架构文档 §4.2.1 给 trait 加了带默认实现的两个钩子：
//!   - `is_stateful()` 返回 true（默认 false）—— 编排层据此走会话式转发入口；
//!   - `forward_conversation(...)`（仅本 provider 覆写）—— 会话式转发的入口，
//!     产出与无状态路径**同一种** `ForwardOutcome`，于是 `chat.rs` 的其余链路
//!     （脱敏、记账、错误写出）零改动。
//!
//! ── 本目录的文件分工（对应架构文档 §9 的模块落位）──────────
//!   mod.rs           CatPawAdapter 的模块声明与上游常量（实现见 adapter.rs）
//!   adapter.rs       **ProviderAdapter 实现**（W5-T-d4）：is_stateful=true、
//!                    forward_conversation 入口、静态清单、环境变量旁路
//!   credentials.rs   凭证来源（账号记录 / auth.json 实时登录态 / CATPAW_COOKIE）
//!                    —— W5-T-d4 补上（架构文档 §9.1 的 Cookie 形态与本机登录态）
//!   messages.rs      OpenAI 消息 → 上游消息块归一化（**纯函数**）✅ T-d1
//!   blocks.rs        内容块与工具字段的归一化（messages.rs 的下半层）✅ T-d1
//!   fingerprint.rs   归一化消息的 SHA-256 指纹链（**纯函数**）✅ T-d1
//!   registry/        会话注册表：x-session-id → conversation 映射（内存表）✅ T-d1
//!     mod.rs         常量与记录结构（`SessionRecord` 等）+ `SessionRegistry` 句柄与
//!                    公开 API（占用 / 解析 / 登记 / 查询 / 排障）
//!     identity.rs    **账号身份归属**：`AccountIdentity` + 身份旁表判据 + 账号级对账
//!     cleanup.rs     移除与作废：TTL 清理、LRU 淘汰、索引与序号维护、四个作废入口、
//!                    移除日志
//!   image_compress.rs 图片压缩（>60KB：最长边 1568 / JPEG 质量 80→55）✅ T-d2
//!   models.rs        协议层错误类型 + 上游 modelType/effort/context 映射表 ✅ T-d3
//!   tools.rs         tools / tool_choice → 上游 toolConfigs 的归一化 ✅ T-d3
//!   prepare.rs       入站准备：参数解析 + 工具归一化 + 消息归一化 + 图片压缩 ✅ T-d3
//!   openai.rs        上游 SSE → OpenAI chunk 翻译层 + usage 口径修正 ✅ T-d3
//!   decision.rs      轮次判定三规则（§9.2）+ 工具续接消息的切取 ✅ T-d3
//!   conversation.rs  轮次状态机：round/event 编排 + turn 建流 + 收尾交接 ✅ T-d3
//!   upstream_http.rs 上游 HTTP 传输：请求头 / 短请求 / 状态回报 / 打断 / turn 建流 ✅ T-d3
//!   turn_executor.rs SSE 消费到底、轮次收尾与注册表写回（硬约束 1/2 的落点）✅ T-d3
//!
//! ── 接线（W5-T-d4 已落地）──────────────────────────────────
//! 本目录整体接进了 provider 注册表：`adapter_for(CatPaw)` 返回
//! [`adapter::CATPAW_ADAPTER`]，编排层按 `is_stateful()` 分派到
//! `forward_conversation`。账号 API 与旧数据导入在 `core::account_store` 的
//! `catpaw_accounts.rs` / `catpaw_import.rs`（模式与小浣熊一致）。
//! 因此模块级的 `#![allow(dead_code)]`（T-d3 留的）已经摘除：编译器重新
//! 把关，只会报出「真的没人用」的那几个。**

pub mod adapter;
pub mod balance;
pub mod blocks;
pub mod catalog;
pub mod conversation;
pub mod credentials;
pub mod decision;
pub mod fingerprint;
pub mod image_compress;
pub mod messages;
pub mod models;
pub mod openai;
pub mod prepare;
pub mod registry;
pub mod tools;
pub mod turn_executor;
pub mod upstream_http;

/// 上游 base URL（架构文档 §9；`catpaw-upstream-client.mjs` 的
/// `DEFAULT_BASE_URL`）。
///
/// 放这里而不是在 conversation.rs 里写死：它是「这是哪一家」的身份信息，
/// 与 provider 元数据同源。实际的取值点在
/// `credentials::upstream_base_url()`（`CATPAW_UPSTREAM_BASE_URL` 可覆盖）。
pub const DEFAULT_BASE_URL: &str = "https://ai.catpaw.meituan.com";

/// 上游 `source` 字段的固定取值（`catpaw-upstream-client.mjs`
/// 的 `DEFAULT_SOURCE`）。round / turn 请求体都要带上。
pub const DEFAULT_SOURCE: &str = "CatX";

/// 上游 `mode` 字段的固定取值（`DEFAULT_MODE`）
pub const DEFAULT_MODE: &str = "CATX_APP";

/// 上游 `toolVersion` 字段（`DEFAULT_TOOL_VERSION`）。
/// 它对应桌面端客户端版本，改动会让上游按不同的工具语义解释请求。
pub const DEFAULT_TOOL_VERSION: &str = "2.0.2";

/// 桌面端客户端版本（`X-Agent-Version` 头；架构文档 §9.1 写的是 1.0.1，
/// 与原实现的 `DEFAULT_CLIENT_VERSION` 一致）
pub const DEFAULT_CLIENT_VERSION: &str = "1.0.1";

/// `M-APPKEY` 头的固定值（架构文档 §9.1）：标识「外部版 CatPaw 客户端」。
pub const APP_KEY: &str = "fe_com.sankuai.catpaw.external.front";
