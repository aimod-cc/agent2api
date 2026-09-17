//! HTTP 路由处理器（每个文件一组端点，与 Node 版的 route 模块一一对应）。
//!
//! 命名约定：`*_api.rs` 对应 Node 版 src/workbuddy-*-routes.mjs 形态的模块；
//! 单端点模块直接用端点名（health.rs / session.rs / endpoints.rs）。
//!
//! 现有模块：
//!   health.rs     GET /health
//!   session.rs    GET /api/session、/api/session/login/*、/api/session/refresh|logout、
//!                 POST /auth/login、POST /auth/logout
//!   config_api.rs GET/POST /api/config
//!   logs_api.rs   GET /api/logs、/api/logs/stats、/api/logs/download、DELETE /api/logs
//!   accounts.rs   /api/accounts*（对照 workbuddy-account-routes.mjs）
//!   proxies.rs    /api/proxies*（Clash 读取 + 出口测试）
//!   billing.rs    积分 / 签到 / 运营活动（对照 workbuddy-billing.mjs + server.mjs 871-911 行）
//!   chat.rs       POST /v1/chat/completions、GET /v1/models（对话主链路）
//!   desensitize.rs /api/desensitize*（词表维护 / 开关 / 角色 / 命中统计）
//!   auto_checkin.rs /api/auto-checkin*（定时签到设置 / 手动执行）
//!   update.rs     /api/update/*（软件更新检查 / 下载 / 进度 / 取消）
//!   endpoints.rs  GET /api/endpoints（接口清单）
//!
//! 管理 API 已全部就位（切片 1-6）。切片 7 是打包收尾，不再新增路由模块；
//! 若真需要新增，按同样的分工加文件并在 `http::router` 里登记（保持 Node 版分组）。

pub mod accounts;
pub mod auto_checkin;
pub mod billing;
pub mod chat;
pub mod config_api;
pub mod desensitize;
pub mod endpoints;
pub mod health;
pub mod logs_api;
pub mod proxies;
pub mod session;
pub mod update;
