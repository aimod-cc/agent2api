//! 领域逻辑（端点常量、账号存储、鉴权会话、无头登录、出网代理、模型目录、转发、脱敏）。
//!
//! 对照 Node 版 src/*.mjs 的落位：
//!   endpoints.rs         端点/版本/UA/上下文          （workbuddy-endpoints.mjs）
//!   account_store/       账号列表、凭证、优先级、迁移  （workbuddy-account-store.mjs）
//!   account_transfer.rs  账号导入/导出                （workbuddy-account-transfer.mjs）
//!   auth_http.rs         上游请求发送与解包、鉴权错误   （workbuddy-auth.mjs 的 api/unwrap）
//!   auth.rs              会话、getStatus、鉴权头、刷新（workbuddy-auth.mjs 会话部分）
//!   login.rs             无头登录与登录任务表          （auth.mjs 登录部分 + server.mjs 任务表）
//!   clash.rs             Clash Verge 配置读取与快照缓存（workbuddy-proxy.mjs 的 Clash 部分）
//!   proxies.rs           账号级代理归一/解析/描述       （workbuddy-proxy.mjs 的代理部分）
//!   egress.rs            出网点（按出口缓存 Client）+ 连通性（workbuddy-proxy.mjs 的 dispatch 部分）
//!   billing/             积分 / 签到 / 运营活动         （workbuddy-billing.mjs）
//!   models.rs            模型目录（内置 + /v3/config 刷新）  （workbuddy-models.mjs）
//!   desensitize/         内容脱敏（词表/编译/改写/统计）（workbuddy-desensitize.mjs）
//!   routing.rs           账号选路（优先级 + 限额冷却）  （workbuddy-routing.mjs）
//!   upstream/            对话转发（选路/轮换/SSE/聚合） （workbuddy-upstream-client.mjs）
//!   auto_checkin.rs      定时签到调度（轮询 + 补签）    （workbuddy-auto-checkin.mjs）
//!   update/              软件更新（版本/出网/下载状态机）（workbuddy-update.mjs）
//!
//! 约定：core 里的模块只做纯逻辑 + 文件读写 + 上游 HTTP，不认识 axum；
//! api/ 里的 handler 负责把 HTTP 输入转成 core 调用、再把结果转成响应。

pub mod account_store;
pub mod account_transfer;
pub mod auth;
pub mod auth_http;
pub mod auto_checkin;
pub mod billing;
pub mod clash;
pub mod desensitize;
pub mod egress;
pub mod endpoints;
pub mod login;
pub mod models;
pub mod proxies;
pub mod routing;
pub mod update;
pub mod upstream;
