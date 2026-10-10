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
//!   stats_api.rs  GET /api/stats/summary、/api/stats/requests、DELETE /api/stats/requests、
//!                 GET/PUT /api/retention
//!   retry_api.rs  GET/PUT /api/retry（请求重试设置，转发层退避的次数 / 间隔）
//!   queue_api.rs  GET/PUT /api/queue（排队等待：次数与单次时长）
//!   accounts.rs   /api/accounts*（对照 workbuddy-account-routes.mjs）
//!   proxies.rs    /api/proxies*（Clash 读取 + 出口测试）
//!   billing.rs    积分 / 签到 / 运营活动（对照 workbuddy-billing.mjs + server.mjs 871-911 行）
//!   chat.rs       POST /v1/chat/completions、GET /v1/models（对话主链路）
//!   models.rs     POST /api/models/refresh（手动刷新模型清单；管理 API，
//!                 与上一条的对外只读探针是两个分组，见该文件模块头）
//!   sanitize.rs   GET/PUT /api/sanitize（出站指纹脱敏开关）
//!   prompt.rs     GET/PUT /api/prompt（系统提示词模式 + 内容拦截降级状态）
//!   auto_checkin.rs /api/auto-checkin*（定时签到设置 / 手动执行）
//!   scheduled_tasks.rs /api/scheduled-tasks*（间隔型定时任务的开关 / 间隔 / 立即执行）
//!   update.rs     /api/update/*（软件更新检查 / 下载 / 进度 / 取消）
//!   endpoints.rs  GET /api/endpoints（接口清单）
//!   storage_api.rs GET /api/storage（统一库的位置、大小与各表条数，只读）
//!   zcode_captcha.rs GET/POST /api/zcode/captcha（活动套餐通道的人机验证令牌池：
//!                 界面静默铸造后推入，转发层按请求取用）
//!   upgrade_api.rs GET /api/upgrade、POST /api/upgrade/run（旧数据 → SQLite 库）
//!
//! 管理 API 已全部就位（切片 1-6）。stats_api 是统计报表任务新增的唯一模块
//! （切片 7 之后的路由扩展），照同样的分工：新文件 + `http::router` 里登记，
//! 并保持与 Node 版一致的分组（需鉴权的一律挂 `protected`）。

pub mod accounts;
// `/api/accounts/usage` 的查询与结果组装（从 accounts.rs 拆出：余额能力从
// 「只服务 workbuddy」扩到四家混查时新增，见该文件模块头）
pub mod accounts_usage;
pub mod auto_checkin;
pub mod billing;
pub mod captcha;
pub mod chat;
// 签到中心的聚合快照（GET /api/checkin-center）：分组口径在后端唯一化，
// 资格类状态由前端按需惰性查询，见该文件模块头
pub mod checkin_center;
pub mod cline_headers;
pub mod codearts_welfare;
pub mod config_api;
// 网关面（`/v1/*`）跨域访问开关：`GET/PUT /api/cors`（与 `/api/sanitize`
// 同形的单开关端点，见该文件模块头）
pub mod cors;
// 自定义提供商的管理接口（新建时顺带创建首个账号；存储与账号接入见
// `core::custom_providers` 与 `core::account_store::custom_accounts`）
pub mod custom_providers;
pub mod debug_api;
// 进行中行的断线兜底守卫（从 pipeline.rs 拆出：那边已近 1000 行，而它是
// 「handler 被 axum 取消时补 408 终态」这一件事的完整封装，见该文件模块头）
pub mod disconnect_guard;
pub mod endpoints;
pub mod health;
// 「从其他工具导入」的扫描接口（当前来源：cc-switch；存储读取见
// `core::import_ccswitch`）
pub mod import_sources;
pub mod keys_api;
/// 保活换行丢弃开关（`/api/keepalive-strip`）：与 `/api/sanitize`、`/api/debug`
/// 同形的单开关端点，独立成文件而不是塞进 config_api —— 理由见该模块头。
pub mod keepalive;
pub mod logs_api;
pub mod model_manage;
// 模型测试（模型管理页操作列的「测试」）：挂管理面、走真实转发链路，
// 只把候选收窄到「这一家 × 这一个账号」并把流量标成测试，见该文件模块头
pub mod model_test;
pub mod models;
// Loomy 新手任务的查询 / 一键领取（签到后在界面弹窗），见该文件模块头
pub mod onboarding;
pub mod panel;
pub mod pipeline;
pub mod prompt;
pub mod protocol;
pub mod proxies;
pub mod qoder_umid;
pub mod queue_api;
pub mod retry_api;
pub mod sanitize;
pub mod scheduled_tasks;
pub mod session;
pub mod stats_api;
pub mod storage_api;
pub mod timeouts_api;
pub mod update;
pub mod upgrade_api;
// 活动套餐通道的人机验证令牌池（界面铸造 → 网关消费），见该文件模块头
pub mod zcode_captcha;
pub mod zcode_claim;
