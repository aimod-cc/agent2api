//! axum Router 组装：路由表、CORS、API Key 检查、404/405 兜底、body 限制。
//!
//! 与 Node 版 server.mjs 的 `createRequestHandler`（672-991 行）逐条对齐：
//!   - 每个响应都带 CORS 头（含 404 与错误响应）—— Node 版在最外层无条件下发
//!   - OPTIONS 直接回 204（预检不需要业务逻辑）
//!   - 未配置 API Key 时全部放行；配置后由中间件统一比对
//!   - 404 文案 `Not found: <METHOD> <path>`
//!   - 未捕获错误统一走 errors::GatewayError → OpenAI 风格 payload + 500
//!
//! ── 路由分组（后续切片照这个模式扩展）────────────────────────
//!   `public`    免鉴权：/health、/api/endpoints（/api/session 已挪 protected）
//!               （Node 版这三条确实都没调 checkApiKey）
//!   `protected` 需鉴权：/api/config、/api/logs*、/api/stats*、/api/retention、
//!               /api/accounts*、/api/proxies*、/api/custom-providers*、
//!               /api/usage、/api/checkin*、/api/activity/*、/api/sanitize、
//!               /api/auto-checkin*、/api/update/*、
//!               /api/session/login/*、/api/session/refresh|logout、/auth/*
//!
//! 中间件只挂在 `protected` 上，而不是「全局中间件 + 白名单」：
//! Node 版是每个分支各自 `if (!checkApiKey(req)) return unauthorized(res)`，
//! 分组写法更贴近这个语义，新增路由时也不会忘记加检查（放错组一眼能看出来）。
//!
//! ── 路由匹配的一个坑 ──────────────────────────────────────
//! `/api/accounts/{id}` 与 `/api/accounts/export` 这类静态子路径在同一个
//! Router 里必须共存：matchit 的优先级是「静态 > 参数」，所以不用像 Node 版
//! 那样靠代码顺序保证 export/import 不被当成账号 id —— 但也**不能**注册
//! `/api/accounts/{*rest}` 这种通配（它会与方法不匹配检查互相干扰）。
//! 因此这里把每条子路径显式列出来。

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::server::api;
use crate::server::errors;
use crate::server::logging;
use crate::server::ServerState;

/// 管理面请求体上限：32MB（对应 Node 版 MAX_BODY_SIZE）。
/// 只约束面板路由（/api/* 管理接口）与手动 `to_bytes` 的读取；
/// 网关面 /v1/* 不设上限 —— 长上下文 + base64 图片的大请求体不能被
/// 网关先拒（上游会自己表达能收多大），见 `gateway_router` 里的 disable。
pub const MAX_BODY_SIZE: usize = 32 * 1024 * 1024;

/// CORS 允许的方法，逐字照抄 Node 版 sendCORS。
///
/// 注意：这里**不含 PATCH** —— Node 版就是这样，虽然账号批量修改等接口
/// 在服务端支持 PATCH，但预检响应里没声明。不做「顺手修复」，
/// 浏览器端若有 PATCH 需求应由后续切片对照 Node 版行为一起评估。
const CORS_METHODS: &str = "GET, POST, OPTIONS, DELETE";
/// CORS 允许的请求头，逐字照抄 Node 版
const CORS_HEADERS: &str = "Content-Type, Authorization, x-api-key";

/// 组装完整路由（面板 + 网关**同端口**：默认形态与桌面壳）。
///
/// 分端口部署（headless 设了 `AGENT2API_PANEL_PORT`）时不要用本函数：
/// 用 [`panel_router`] + [`gateway_router`] 各挂一个监听（见 `mod::start`）。
pub fn router(state: ServerState) -> Router {
    panel_router(state.clone()).merge(gateway_router(state))
}

/// 管理面路由：管理界面（静态文件 fallback）+ `/api/*`。
///
/// 分端口形态下它单独监听 `AGENT2API_PANEL_PORT` —— 面板端口可以不暴露
/// 公网（防火墙 / compose 不映射），管理面就整体留在内网。
pub fn panel_router(state: ServerState) -> Router {
    // 免鉴权：/api/endpoints（接口清单，排查用）
    //
    // GET /api/session 原本也在免鉴权组（「前端首屏在配置 Key 之前也要能读」，
    // 见 session.rs），后来挪进 protected：桌面壳的管理请求由壳自动带第一把
    // Key，不受影响；而 headless/公网形态下它暴露账号昵称、各家健康状态等
    // 部署信息，不该在无 Key 的世界里裸奔（网页端 bridge 首次 401 会引导输入）。
    let public = Router::new()
        .route("/api/endpoints", get(api::endpoints::handle))
        // 面板登录：调用方此时没有任何凭证，必须挂 public —— 安全由
        // 失败锁定（同 IP 连续 5 次锁 5 分钟）与 bcrypt 校验成本承担
        // （见 api::panel 与 access 的模块头）。未配置管理员的部署
        // （桌面形态）这条返回 404，等于功能不存在。
        .route("/api/panel/login", post(api::panel::panel_login))
        // 注册 / 会话刷新 / 登出：同为「认证边界」端点 —— 调用方要么还没有
        // 凭证（setup / login），要么凭证本身就是它们的主张（refresh 带
        // refresh cookie、logout 撤自己的会话），挂 public 由端点自理。
        .route("/api/panel/status", get(api::panel::panel_status))
        .route("/api/panel/setup", post(api::panel::panel_setup))
        .route("/api/panel/refresh", post(api::panel::panel_refresh))
        .route("/api/panel/logout", post(api::panel::panel_logout))
        // ALTCHA 领题：与 status / setup 同为「认证边界」端点 —— 领题时
        // 用户还没有任何凭证（开关关闭时回 400，前端跳过校验）
        .route("/api/panel/captcha", get(api::panel::captcha_challenge))
        // CatPaw 网页登录的 loopback 回调：**上游浏览器直接 POST 到这里**
        // （redirect 指向本网关自己的 loopback 端口，见 core::login::catpaw），
        // 所以它必须免鉴权 —— 调用方是美团 passport 页面，它没有我们的 API Key。
        // 安全性由一次性 `state` 承担（逐字比对，见该处理函数的说明）。
        // 注意方向：与小浣熊那条 `/api/session/login/callback` 相反，那条是
        // **我们自己的登录窗口**转交上来的，因此留在 protected 组。
        .route(
            "/api/session/login/catpaw-callback",
            post(api::session::login_catpaw_callback),
        )
        // AutoClaw OAuth（国际版）的 loopback 回调：**浏览器 302 到这里**
        // （授权页完成后顶层导航到我们交给上游的 navigate_uri，见
        // `providers::autoclaw::oauth`），所以同样必须免鉴权 —— 调用方是用户的
        // 浏览器，它没有我们的 API Key。
        //
        // 路径与官方客户端**逐字同款**（`/auth/callback-zai|google`）：Zai 的
        // OAuth 服务在 authorize 阶段按 redirect_uri 白名单校验，host 用
        // `127.0.0.1` 或路径不照抄都会被拒（`Redirect URI not registered for
        // this client`，2026-09-22 实测）。回调 URL 里没有我们的任务 state
        // （放查询串会与上游回带的 state 撞参数名），任务关联按变体匹配
        // 进行中的登录完成（见 `core::login::autoclaw`）。
        .route(
            "/auth/callback-{vendor}",
            get(api::session::login_autoclaw_oauth_callback),
        );

    // 需鉴权：Node 版对这些路径都调用了 checkApiKey
    let protected = Router::new()
        // GET /api/session（首屏状态）：从 public 组挪进来 —— 它会透出账号
        // 昵称、各家健康状态、路由中的账号等部署信息（字段清单见 session.rs），
        // 桌面壳的请求由壳带第一把 Key（gateway::request_builder）不受影响。
        .route("/api/session", get(api::session::get_session))
        .route(
            "/api/config",
            get(api::config_api::get_config)
                .post(api::config_api::post_config)
                // PUT 是 Agent2API 改造新增的**别名**（架构文档 §5 的接口表写作
                // GET/PUT）：走同一个处理函数，行为逐字一致。既有前端只发 POST，
                // 多注册一个方法不影响它；新前端按 §5 发 PUT 也能用。
                .put(api::config_api::post_config),
        )
        .route(
            "/api/logs",
            get(api::logs_api::query_logs).delete(api::logs_api::clear_logs),
        )
        .route("/api/logs/stats", get(api::logs_api::stats_logs))
        .route("/api/logs/download", get(api::logs_api::download_logs))
        // Node 版对 /api/logs* 的未知子路径（含 `/api/logs/` 上的非 GET/DELETE）
        // 返回管理 API 形状的 404（`{success:false,error:"Not found: <METHOD> <path>"}`），
        // 而不是全局兜底的 OpenAI 形状
        .route("/api/logs/", any(api::logs_api::not_found))
        .route("/api/logs/{*rest}", any(api::logs_api::not_found))
        // ── 统计报表与数据保留（切片 7 之后的扩展，非 Node 版对齐项）──
        // /api/stats/* 与 /api/retention 都挂 protected：
        // 它们能读到全部请求日志（含模型、账号、token 用量）并能删数据 / 改保留期，
        // 与 /api/logs 同级敏感，必须和日志接口一样走 API Key 检查。
        // 未知 /api/stats/* 子路径返回管理信封 404（照 logs_api::not_found 的做法，
        // 而不是全局兜底的 OpenAI 形状）—— 同一前缀下的 404 形状保持一致。
        .route("/api/stats/summary", get(api::stats_api::stats_summary))
        .route(
            "/api/stats/requests",
            get(api::stats_api::stats_requests).delete(api::stats_api::clear_stats_requests),
        )
        // 筛选下拉的候选清单（出现过的模型 / 提供商）。登记在 `/api/stats/requests`
        // 之后、`/api/stats/{*rest}` **之前**：通配兜底是 404，顺序反了会让这个
        // 端点永远拿到 404（形状还是管理信封，看起来就像路径拼错了）
        .route(
            "/api/stats/requests/filters",
            get(api::stats_api::stats_request_filters),
        )
        // 单条请求的原始正文（预览对话详情弹窗的数据源）。同样要排在通配之前
        .route(
            "/api/stats/requests/raw",
            get(api::stats_api::stats_request_raw),
        )
        // 手动终止一条在途请求（详情弹窗的「终止请求」按钮）。POST 且同样
        // 排在通配之前 —— 顺序理由与上面两条相同
        .route(
            "/api/stats/requests/terminate",
            post(api::stats_api::stats_request_terminate),
        )
        // 清理弹窗的预览统计（将删明细数 / 带报文数 / 库占用 / 压缩状态）
        .route(
            "/api/stats/requests/clear-preview",
            get(api::stats_api::stats_clear_preview),
        )
        // 压缩数据库（checkpoint + VACUUM，后台线程执行；重复触发 409）
        .route(
            "/api/stats/requests/compact",
            post(api::stats_api::compact_stats_db),
        )
        // 无尾段的 `/api/stats` 也登记成管理信封 404：这个前缀下没有「列表」端点
        // （报表有三条子路径），但同一前缀下的 404 形状必须一致 ——
        // 前端拼错路径时拿到的若是 OpenAI 形状，会误以为是转发链路的问题
        .route("/api/stats", any(api::stats_api::not_found))
        .route("/api/stats/", any(api::stats_api::not_found))
        .route("/api/stats/{*rest}", any(api::stats_api::not_found))
        // 保留期：GET 读三档天数，PUT（允许部分字段）更新并**立即**触发清理。
        // 与 /api/config 的区别：config 管鉴权与语言，这条只管数据保留策略 ——
        // 放在独立端点是因为它的写操作带副作用（删数据），不该混进 config 的
        // 「无副作用设置」里，误调一次 config 不该把历史数据裁掉。
        .route(
            "/api/retention",
            get(api::stats_api::get_retention).put(api::stats_api::put_retention),
        )
        // 请求重试：GET 读「次数 / 间隔」，PUT（允许部分字段）更新。
        // 与 /api/retention 同一模式独立成端点而不混进 /api/config：
        // config 是 Node 对齐项（apiKey / locale），重试是本壳新增的转发行为
        // 设置；这条没有「立即清理」类副作用，保存后对下一个失败请求生效。
        .route(
            "/api/retry",
            get(api::retry_api::get_retry).put(api::retry_api::put_retry),
        )
        // ── 调试模式（设置页「通用 → 调试模式」）──
        // GET/PUT 开关；traffic 是按 id 取原始报文的详情端点（列表接口不返回
        // 报文，见 debug_api 的模块头）。挂 protected：报文含上游 URL 与请求体。
        .route(
            "/api/debug",
            get(api::debug_api::get_debug).put(api::debug_api::put_debug),
        )
        .route("/api/debug/traffic", get(api::debug_api::get_traffic))
        // ── 数据保存位置（设置页「保存位置」）──
        // 只读概况（库在哪、多大、各表多少条）。挂 protected 与 /api/retention
        // 同级：它能读到「这台机器上有多少账号与请求记录」这类存储规模信息。
        // 改造前的两条写路由（relocate / progress）随「三类数据各自一个目录」
        // 的语义一起删除，理由见 `api::storage_api` 的模块头。
        .route("/api/storage", get(api::storage_api::get_storage))
        // ── 数据结构升级（旧 JSON/JSONL → 统一 SQLite 库）──
        // 启动时**不自动迁移**，由用户在弹窗里点「升级」触发（需求：必须让用户
        // 知道数据换了存储形态、旧数据保留）。挂 protected：run 会写库里的
        // 全部业务表（账号、日志、统计、配置），与 /api/config 同级敏感。
        .route("/api/upgrade", get(api::upgrade_api::get_upgrade))
        .route("/api/upgrade/run", post(api::upgrade_api::run_upgrade))
        // ── 账号管理（对照 workbuddy-account-routes.mjs）──
        // 用 any(...) 注册两条入口（无尾段 + 通配尾段），方法/路径的判定交给
        // api::accounts::dispatch —— 这是为了复刻 Node 版 tryHandle 的判定顺序
        // （见那边的注释：`DELETE /api/accounts/export` 会被当成账号 id）。
        // axum 的静态路由 + {id} 写法会把这类组合拆成 405，与 Node 分叉。
        // 单独登记尾斜杠形态：`/api/accounts/` 在 axum 的 `{*rest}` 里匹配不上
        // （通配要求至少一个非空段），不登记就会落到全局 404（OpenAI 形状），
        // 而 Node 版对它的响应是管理 API 形状的 404
        .route("/api/accounts", any(api::accounts::accounts_entry))
        .route("/api/accounts/", any(api::accounts::accounts_entry))
        .route("/api/accounts/{*rest}", any(api::accounts::accounts_entry))
        // ── 出网代理（Clash Verge 实时读取 + 出口连通性测试）──
        // /api/proxies 之外（如 /api/proxies/zzz）不注册 → 落到全局 404 兜底，
        // 与 Node 版「前缀判定不通过 → 全局兜底」一致
        .route("/api/proxies", any(api::accounts::proxies_entry))
        .route("/api/proxies/test", any(api::accounts::proxies_entry))
        // ── 积分 / 签到 / 运营活动（对照 server.mjs 871-911 行）──
        // 六条都挂在 protected（Node 版每条都调了 checkApiKey），
        // 失败时的 body 是 OpenAI 风格（那几条在 server.mjs 的大 try 里）
        .route("/api/usage", get(api::billing::get_usage))
        .route("/api/checkin/status", get(api::billing::checkin_status))
        .route("/api/checkin", post(api::billing::claim_checkin))
        .route(
            "/api/checkin/claim-and-report",
            post(api::billing::claim_and_report),
        )
        .route("/api/activity/banner", get(api::billing::activity_banner))
        .route(
            "/api/activity/ambassador",
            get(api::billing::activity_ambassador),
        )
        // ── 会话与登录 ──
        .route("/api/session/login/start", post(api::session::login_start))
        .route("/api/session/login/wait", get(api::session::login_wait))
        .route("/api/session/login/cancel", post(api::session::login_cancel))
        // 网页登录的回调入口：壳侧登录窗口把 `office-raccoon://auth/callback?…`
        // 原样 POST 到这里（Tauri 不能像 Electron 那样在会话里注册协议处理器，
        // 见 api::session::login_callback 的说明）。与其他 login/* 一样在
        // protected 组 —— 它写账号库，必须过 API Key。
        .route("/api/session/login/callback", post(api::session::login_callback))
        // AutoClaw 的手机号验证码登录（**不是**网页登录，见 api::session 模块头）：
        // 上游没有授权码 / 回调这条路，登录就是「发码 → 用码换 token」两次请求，
        // 因此不需要登录窗口与轮询。两条都挂 protected —— 它们都写账号库，
        // 与上面 callback 同一判据。
        .route(
            "/api/session/login/sms/send",
            post(api::session::login_sms_send),
        )
        .route(
            "/api/session/login/sms/verify",
            post(api::session::login_sms_verify),
        )
        // AutoClaw OAuth 网页登录（**国际版**的官方主方式）。三段里只有前两段
        // 在这里：第三段（loopback 回调）在 public 组（调用方是用户的浏览器，
        // 见那边的注释）。这两段都挂 protected —— 它们要带验证码参数去打上游、
        // 并往任务表里登记一次登录，与 sms/* 同级敏感。
        .route(
            "/api/session/login/oauth/captcha-config",
            get(api::session::login_oauth_captcha_config)
                .post(api::session::login_oauth_captcha_config),
        )
        .route(
            "/api/session/login/oauth/start",
            post(api::session::login_oauth_start),
        )
        .route("/api/session/refresh", post(api::session::session_refresh))
        .route("/api/session/logout", post(api::session::session_logout))
        .route("/auth/login", post(api::session::auth_login))
        .route("/auth/logout", post(api::session::auth_logout))
        // ── 对话链路的四条入口（chat/completions / responses / messages /
        // messages/count_tokens）原本挂在这里，拆端口时挪进了
        // [`gateway_router`]（都查 API Key，转发语义归网关面）；
        // /v1/models 那条免鉴权探针同去。同端口形态经 `router()` 合并，
        // 行为与拆分前逐字一致。
        // 手动刷新模型清单（网关页按钮）：它会**真打上游**（各家的模型目录接口），
        // 所以和 /v1/chat/completions 一样必须过 API Key；GET /v1/models 那条
        // 免鉴权的只读探针不受影响（两者是不同的东西，见 api::models 模块头）。
        // 永远返回 2xx：逐家结果自己表达成败，理由见该模块头
        .route("/api/models/refresh", post(api::models::refresh_models))
        // 模型管理（启停 / 隐藏 / 映射）与网关 Key 列表：都是写配置的管理接口，挂 protected
        .route("/api/models/manage", get(api::model_manage::get_manage))
        .route("/api/models/state", post(api::model_manage::set_state))
        .route("/api/models/mappings", post(api::model_manage::add_mapping))
        .route("/api/models/mappings/remove", post(api::model_manage::remove_mapping))
        // 自定义模型（手动登记上游目录里没有的模型）
        .route("/api/models/custom", post(api::model_manage::add_custom))
        .route("/api/models/custom/remove", post(api::model_manage::remove_custom))
        // ── 自定义提供商（用户自建上游端点：存储 + 管理）──
        // 与 /api/models/manage 同级敏感：写配置（customProviders 键）且「新建」
        // 会顺带写账号库，挂 protected。账号侧不经这里 —— 客户端走
        // /api/accounts（其 add 分派认 custom- 前缀的 provider id），
        // 两条入口最终都落在 `account_store::custom_accounts` 上。
        .route(
            "/api/custom-providers",
            get(api::custom_providers::get_custom_providers)
                .post(api::custom_providers::create_custom_provider),
        )
        .route(
            "/api/custom-providers/update",
            post(api::custom_providers::update_custom_provider),
        )
        .route(
            "/api/custom-providers/remove",
            post(api::custom_providers::remove_custom_provider),
        )
        // 模型清单的两条（第二阶段）：整表保存 / 服务端代理拉取上游清单。
        // 挂 protected 的理由与上面的管理四条相同；fetch-models 还会真打上游
        // （一次 GET {baseUrl}/models），与 /api/models/refresh 同级敏感。
        .route(
            "/api/custom-providers/models",
            post(api::custom_providers::set_custom_models),
        )
        .route(
            "/api/custom-providers/fetch-models",
            post(api::custom_providers::fetch_custom_models),
        )
        .route("/api/keys", get(api::keys_api::list_keys).post(api::keys_api::create_key))
        .route("/api/keys/{id}", patch(api::keys_api::update_key).delete(api::keys_api::delete_key))
        // ── 出站指纹脱敏开关 ──
        // 与 /api/debug 同形的单开关端点（GET 读 / PUT 写），挂 protected：
        // 它决定出站请求体要不要剥离审核指纹，敏感度与调试模式同级。
        // 改造前的 /api/desensitize* 八条端点（词表增删改 / 作用角色 / 作用提供商 /
        // 命中统计 / 远程同步）随词表方案整体删除，只剩这一个开关。
        .route(
            "/api/sanitize",
            get(api::sanitize::get_sanitize).put(api::sanitize::put_sanitize),
        )
        // ── 机器人校验开关 ──
        // 与 /api/sanitize 同形的单开关端点（GET 读 / PUT 写），挂 protected：
        // 它决定登录 / 注册是否要求 ALTCHA proof-of-work，敏感度同级。
        .route(
            "/api/captcha",
            get(api::captcha::get_captcha).put(api::captcha::put_captcha),
        )
        // ── 系统提示词与内容拦截降级 ──
        // 与 /api/sanitize 同为「出站内容处理」的开关，但形状不同（枚举 + 文件
        // 路径 + 一个运行期状态要一起返回），所以单独一条端点，见 api::prompt
        // 的模块头。挂 protected：它能改出站的 system 提示词，敏感度同级。
        .route(
            "/api/prompt",
            get(api::prompt::get_prompt).put(api::prompt::put_prompt),
        )
        // ── 定时签到（对照 server.mjs 726-746 行）──
        // 三条都在 Node 的最外层大 try 里，失败走 OpenAI 风格 body（含 run 的
        // 「签到正在执行中，请稍候」）—— 形状由 api::auto_checkin 自己保证。
        // Node 的判定是「path === '/api/auto-checkin' || path === '/api/auto-checkin/run'」，
        // 因此 `/api/auto-checkin/` 与 `/api/auto-checkin/xxx` 都落到全局 404，
        // 这里同样只注册这两条精确路径。
        .route(
            "/api/auto-checkin",
            get(api::auto_checkin::get_state).post(api::auto_checkin::configure),
        )
        .route("/api/auto-checkin/run", post(api::auto_checkin::run_now))
        // ── 间隔型定时任务（凭证自动维护 / 模型目录刷新 / 两个前端自动刷新）──
        // 挂 protected：它能改后端后台任务的执行节奏（间隔 1 分钟会让网关持续
        // 打上游），并触发真打上游的刷新，敏感度与 /api/retention 同级。
        //
        // 三条入口都是 any(...)、方法判定交给 `api::scheduled_tasks::entry` ——
        // 与 /api/accounts 同一取舍：拆成独立 axum 路由会让
        // 「已注册路径 + 未注册方法」变成 405 兜底，而这个前缀下希望统一给 404。
        // 单独登记尾斜杠形态：`/api/scheduled-tasks/` 在 `{*rest}` 里匹配不上
        // （通配要求至少一个非空段），不登记就会落到全局 404。
        .route("/api/scheduled-tasks", any(api::scheduled_tasks::entry))
        .route("/api/scheduled-tasks/", any(api::scheduled_tasks::entry))
        .route(
            "/api/scheduled-tasks/{*rest}",
            any(api::scheduled_tasks::entry),
        )
        // ── 软件更新（对照 server.mjs 749-773 行）──
        // Node 是 `path.startsWith('/api/update/')` 的前缀判定：命中后逐条比对，
        // 都不匹配则落到全局 404（不带 success 信封）。用 {*rest} 通配入口 +
        // dispatch 复刻这个结构（拆成独立路由会把「已注册路径 + 未注册方法」
        // 变成 405，与 Node 分叉）。
        // 注意：`/api/update`（没有尾斜杠）在 Node 里不命中前缀判定 → 全局 404，
        // 这里同样只注册 `{*rest}`（它要求至少一个非空段），行为一致。
        .route("/api/update/{*rest}", any(api::update::entry))
        // ── 切片 7 收尾时要知道的事 ─────────────────────────────
        // 管理 API 到此**全部就位**（切片 1-6 覆盖了 health/session/config/logs/
        // accounts/proxies/billing/chat/sanitize/auto-checkin/update），
        // 路由表不再有缺口。切片 7 只需处理打包收尾：
        //   ① 从 package.json / resources 里摘掉旧的 node 后端产物
        //      （server.cjs + node.exe 的随包分发），确认 tauri.conf.json 的
        //      resources 与 bundle 配置同步；
        //   ② 清掉本文件与 server/mod.rs 里的 `#![allow(dead_code)]` 抑制，
        //      按 warning 逐条处理遗留的未使用公共设施；
        //   ③ 版本号与 `.zcode/release-notes` 更新（安装包名与 asset 命名约定
        //      要与 update 的 pickInstaller 规则相符：带 setup 的 .exe 优先）。
        .layer(middleware::from_fn(require_api_key));

    // 静态托管只在 headless 打开（set_ui_dir）：桌面形态的界面由 Tauri 壳出，
    // 网关的未知路径保持纯 404。ui_dir 是启动即定值，克隆进闭包。
    let ui_dir = state.ui_dir().cloned();
    Router::new()
        .merge(public)
        .merge(protected)
        .fallback(move |request: Request| {
            let ui_dir = ui_dir.clone();
            async move {
                match ui_dir {
                    Some(dir) => super::static_files::serve(request, dir).await,
                    None => not_found(request).await,
                }
            }
        })
        // 方法不匹配的兜底：Node 版没有 405 概念，一律落到 404 分支，
        // 所以这里把 405 也转成同样的 Not found 文案，保持行为一致
        .method_not_allowed_fallback(method_not_allowed)
        // CORS 与 body 限制必须在最外层：404 与错误响应也要带 CORS 头
        .layer(middleware::from_fn(cors))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

/// 网关面路由：`/v1/*`（模型探针 + 三条协议入口 + token 计数）与 `/health`。
///
/// 分端口形态下它单独监听主端口（`AGENT2API_PROXY_PORT`）—— 对外的
/// OpenAI 兼容端点，没有任何管理界面与管理 API；想收敛暴露面，防火墙 /
/// compose 只映射这个端口即可。同端口形态经 [`router`] 与管理面合并。
pub fn gateway_router(state: ServerState) -> Router {
    // /health（壳侧就绪探测与部署探活）与 /v1/models 免鉴权：
    // /v1/models 是只读探针，Node 版就不查 API Key —— OpenAI 客户端常在
    // 配置 key 之前先拉模型列表（README 也把 /v1/models 列在免鉴权探针里）。
    let open = Router::new()
        .route("/health", get(api::health::handle))
        .route("/v1/models", get(api::chat::list_models))
        .with_state(state.clone());
    // 三条协议入口 + Anthropic 的 token 计数端点：都查 API Key。
    // 与 chat/completions 完全同构：都走同一套转发链路，差异只在出入口的
    // 协议翻译（见 api::protocol 与 core::protocol 的模块头）。它们**就是**
    // 对话入口而不是管理 API —— 客户端的 base_url 换成 /v1 即可直接用。
    // count_tokens：Claude Code 在网关模式下会探它做上下文预算，上游没有
    // 对应能力，这里给带 `estimated: true` 的估算（见该函数说明）—— 给 404
    // 会让客户端按「网关不支持」处理，比一个粗略数字更糟。
    let guarded = Router::new()
        .route("/v1/chat/completions", post(api::chat::chat_completions))
        .route("/v1/responses", post(api::protocol::responses_endpoint))
        .route("/v1/messages", post(api::protocol::messages_endpoint))
        .route("/v1/messages/count_tokens", post(api::chat::count_tokens))
        .layer(middleware::from_fn(require_api_key))
        .with_state(state);
    // 网关面必须显式 disable：axum 对没挂 DefaultBodyLimit 层的路由兜底
    // 2MB（axum-core Request::with_limited_body），长上下文 + base64 图片
    // 的大请求体会被先拒成 413，客户端重试原样请求体只会白打转。这里的
    // disable 在合并形态下也压得过面板路由外层的 max —— 扩展是逐层 insert，
    // 内层后写覆盖外层先写。上限交给上游自己表达。
    open.merge(guarded)
        .layer(axum::extract::DefaultBodyLimit::disable())
}

/// 未匹配路由：文案照抄 Node 版 `Not found: <METHOD> <path>`。
/// 注意用 errors::not_found_response 而非 GatewayError —— Node 版 404 没有 type 字段。
async fn not_found(request: Request) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    logging::verbose("[HTTP]", &format!("← 404 {method} {path}"));
    errors::not_found_response(&method, &path)
}

/// 方法不匹配（例如 GET /api/config 之外的方法落到已注册路径上）：
/// Node 版会走 404 分支，这里保持同样文案。
async fn method_not_allowed(request: Request) -> Response {
    not_found(request).await
}

/// CORS 中间件。
///
/// 直接操作响应头而不是用 tower-http 的 CorsLayer：Node 版是在**每个**响应上
/// 无条件下发这三个头（包括 404、错误、SSE），用中间件逐字复刻最稳，
/// 也避免为了这一件事引入 tower-http。
async fn cors(request: Request, next: Next) -> Response {
    // 预检直接 204，不进业务逻辑（对应 Node 版 `if (req.method === 'OPTIONS')`）
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        attach_cors(response.headers_mut());
        return response;
    }

    let path = request.uri().path().to_string();
    let method = request.method().as_str().to_string();
    // 详细模式下记录每个入站请求（对应 Node 版 `if (opts.verbose && !path.startsWith('/v1/'))`）
    // ——只有开了 AGENT2API_VERBOSE=1（旧名 WORKBUDDY_VERBOSE 仍可读）才入库，
    // 普通启动只是控制台多一行
    if logging::is_verbose() && !path.starts_with("/v1/") {
        let query = request.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
        logging::verbose("[HTTP]", &format!("← {method} {path}{query}"));
    }

    let mut response = next.run(request).await;
    attach_cors(response.headers_mut());
    response
}

/// 写入三个 CORS 头（值固定，不会失败；失败时静默跳过而不是 panic）
fn attach_cors(headers: &mut axum::http::HeaderMap) {
    if let Ok(value) = HeaderValue::from_str("*") {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if let Ok(value) = HeaderValue::from_str(CORS_METHODS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, value);
    }
    if let Ok(value) = HeaderValue::from_str(CORS_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, value);
    }
}

/// API Key 检查中间件，逻辑照抄 Node 版 checkApiKey：
///   - 未配置 key → 全部放行（网关只监听 127.0.0.1）
///   - 配置了 key → 请求头 `Authorization: Bearer <key>` 或 `x-api-key: <key>`
///     任一匹配即通过
/// 不通过返回 401 + OpenAI 风格错误 body。
///
/// 只需要「当前生效的配置」这一份全局状态，所以用 `from_fn`（无 state）即可：
/// 后续切片的路由挂进来时不用重复传状态。
///
/// ── R9：命中的那把 Key 要带到 handler（可用提供商 / 可用模型）──────
/// 校验通过后把**那条记录**（`api_keys::entry_for_key`）包成
/// [`key_scope::KeyScope`] 放进请求扩展 —— 后面 handler 才知道该套用哪套白名单。
/// 选请求扩展而不是「改 body / 加请求头」的理由见 `key_scope` 的模块头
/// （body 要去重、头要透传给上游，都不能动）。
///
/// 三处「没有命中的 Key」的情形都不放这个扩展，于是消费点统一按**不限制**处理：
///   - 免鉴权模式（一把启用的 Key 都没有）：下面第一个分支直接放行。
///     **这是既有语义，不要因为「没命中 Key」改成拒绝** —— 改造前未配置
///     API Key 时网关对 /v1/* 是完全开放的（只监听 127.0.0.1）。
///   - 环境变量 Key（`WORKBUDDY_PROXY_API_KEY`）：`entry_for_key` 对它返回
///     None（没有列表记录可挂白名单）→ 不限制。
///   - Key 在两次快照之间被删/停用：同样取不到记录 → 不限制（宁可放行，
///     不可因为管理页上改了一下就把正在跑的客户端全挡掉）。
///
/// 注意 `/v1/models` 在 public 组、**不经过本中间件** —— 它有自己的一条限制
/// 生效点（见 `api::chat::list_models`），用的是同一份 `KeyScope` 语义
/// （但那里是「有记录就按记录过滤，没有就不过滤」，因为它免鉴权也能用）。
async fn require_api_key(mut request: Request, next: Next) -> Response {
    // 每次都读内存快照（不是读文件），所以「刚保存的新 key」下一个请求就生效。
    // 快照取一次、整个判定过程复用（`current()` 会克隆整份配置，鉴权是每个请求
    // 都要走的热路径 —— 多取几次就是为同一件事重复克隆）
    let snapshot = crate::server::config::current();
    let keys = snapshot.active_api_keys();
    let path = request.uri().path();

    // ── 面板认证（headless 配置了管理员时启用）──────────────────
    // `/api/*` 认「会话 cookie（人，账号密码换来）」或「API Key（程序，
    // 桌面壳/脚本自动带）」。两者都没有 → 401 并注明是面板登录 ——
    // web_shim 据此弹账号密码框而不是 Key 框。桌面形态不开这个开关
    // （未配置管理员环境变量），下面的原有语义原样生效。
    let panel_auth = crate::server::access::panel_auth_enabled();
    if panel_auth && path.starts_with("/api/") {
        if crate::server::access::session_valid(request.headers()) {
            return next.run(request).await;
        }
        if !keys.is_empty()
            && keys
                .iter()
                .any(|expected| request_matches_key(&request, expected))
        {
            // Key 也能操作管理接口（桌面壳 / 脚本自动化的通道）
            return next.run(request).await;
        }
        return errors::panel_login_required_response();
    }
    // ── 未注册闸门（headless 专属，桌面壳不开启）────────────────
    // 管理员还没注册（也没用环境变量预置）：/api/* 只放行 /api/panel/
    // 前缀的认证边界端点（status / setup / login / refresh / logout ——
    // 登录页依赖它们），其余一律 401 panel_login_required，web_shim 收到
    // 后会整页跳到 /login 的注册表单。没有这道闸，面板前端拿到放行的
    // 200 会直接进主界面，「部署完先注册」就成了可绕过的一步。
    //
    // 两个例外必须放行，否则闸门会把既有部署形态打断：
    //   · **兼容模式**（AGENT2API_PROXY_API_KEY 预置了 Key、无管理员）：
    //     Key 本来就是这类部署的管理凭证（桌面壳/脚本自动带），闸门必须
    //     先试 Key 再拒绝；
    //   · `/api/panel/` 前缀：注册 / 状态 / 登录本身就在这个前缀里。
    if !panel_auth && crate::server::access::panel_gate() && path.starts_with("/api/") {
        if !path.starts_with("/api/panel/")
            && !(keys.iter().any(|expected| request_matches_key(&request, expected)))
        {
            return errors::panel_login_required_response();
        }
        return next.run(request).await;
    }

    if keys.is_empty() {
        // 免鉴权模式。公网形态下 /v1/* fail-closed：没有 Key 可校验时，
        // 把模型额度对全网开放是不可接受的 —— 拒绝服务并引导去面板建 Key
        // （面板本身有账号密码登录，见 access 模块头）。桌面形态不进
        // 这个分支（开关只在 headless 启动时按需打开）。
        if crate::server::access::v1_fail_closed() && path.starts_with("/v1/") {
            return errors::v1_fail_closed_response();
        }
        return next.run(request).await;
    }

    let matched = keys
        .iter()
        .find(|expected| request_matches_key(&request, expected))
        .cloned();
    if let Some(matched) = matched {
        // 命中的那把 Key 的限制随请求带到 handler（见函数头）
        if let Some(scope) = scope_for_key(&snapshot.raw(), &matched) {
            crate::server::core::key_scope::attach(&mut request, scope);
        }
        return next.run(request).await;
    }

    let method = request.method().as_str().to_string();
    logging::log("[Security]", &format!("❌ 拒绝未授权的请求: {method} {path}"));
    errors::unauthorized_response()
}

/// 在**给定快照**里找命中的 Key 并构造它的限制（R9）。
///
/// 抽成普通函数是为了另一条调用路径：**`GET /v1/models` 挂在免鉴权组**
/// （Node 版这条就是不查 API Key 的只读探针，客户端常在配 Key 之前先拉列表），
/// 所以它不经过 `require_api_key`。但它的「只返回被授权的模型」这条限制又必须
/// 认 Key —— 于是那个 handler 自己取一份快照、调本函数，与中间件共用**同一套
/// 匹配口径**（`headers_match_key` + `entry_for_key_from`），不会出现
/// 「中间件认这把 Key、列表接口不认」这种自相矛盾。
///
/// 返回值 `None` = 没有命中的记录（免鉴权模式 / 环境变量 Key / 未知 Key）——
/// 调用方一律按**不限制**处理，理由见 `core::key_scope` 的模块头。
pub fn key_scope_from_headers(
    headers: &axum::http::HeaderMap,
) -> Option<crate::server::core::key_scope::KeyScope> {
    let snapshot = crate::server::config::current();
    let keys = snapshot.active_api_keys();
    if keys.is_empty() {
        return None;
    }
    let matched = keys.iter().find(|expected| headers_match_key(headers, expected))?;
    scope_for_key(&snapshot.raw(), matched)
}

/// 命中的明文 Key → 它的 `KeyScope`（在给定快照里查记录；查不到 = 不限制）
fn scope_for_key(
    raw: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<crate::server::core::key_scope::KeyScope> {
    crate::server::core::api_keys::entry_for_key_from(raw, key)
        .map(|entry| crate::server::core::key_scope::KeyScope::from_entry(&entry))
}

/// `request_matches_key` 的头比较部分，抽出来给只有 `HeaderMap` 的调用方用
/// （`GET /v1/models` 的 handler）。实现与 `request_matches_key` 逐字一致 ——
/// 后者委托给它，保证两处不可能漂移。
fn headers_match_key(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if strip_bearer_prefix(value) == expected {
            return true;
        }
    }

    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if value == expected {
            return true;
        }
    }

    false
}

/// 比对请求头里的凭证，逐字复刻 Node 版 checkApiKey：
///
/// ```js
/// const bearer = auth.replace(/^Bearer\s+/i, '');
/// return bearer === opts.apiKey || apiKeyHeader === opts.apiKey;
/// ```
///
/// 注意 Node 的行为细节：`replace` 在**不匹配时原样返回整个字符串**，
/// 所以 `Authorization: <key>`（不带 Bearer 前缀）也会被判为通过。
/// 这是「顺手修复」的诱惑点，但契约以 Node 版为准，保持一致。
/// `x-api-key` 则要求严格相等（不做 trim）。大小写方面：HTTP 头名不区分大小写
/// （由 HeaderMap 处理），值区分大小写。
///
/// 实现委托给 [`headers_match_key`]：`/v1/models` 的 handler 只有 `HeaderMap`
/// 而没有完整请求（它不在中间件链上），两处必须是**同一套**匹配口径 ——
/// 各写一份就会漂移成「中间件认这把 Key、列表接口不认」，而那种分叉在界面上
/// 完全看不出来（都是 200，只是列表少几个模型）。
fn request_matches_key(request: &Request, expected: &str) -> bool {
    headers_match_key(request.headers(), expected)
}

/// 去掉 `Bearer ` 前缀（大小写不敏感，`\s+` 至少一个空白）。
/// 不匹配时**原样返回**整个字符串 —— 与 Node 的 `String.replace` 语义一致。
fn strip_bearer_prefix(value: &str) -> &str {
    const PREFIX: &str = "bearer";
    if value.len() < PREFIX.len() {
        return value;
    }
    let (head, rest) = value.split_at(PREFIX.len());
    if !head.eq_ignore_ascii_case(PREFIX) {
        return value;
    }
    // `\s+` 要求至少一个空白字符，"BearerX" 不匹配、应原样返回
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        return value;
    }
    trimmed
}

/// 裸 JSON 响应（不带信封）：/health 与 OpenAI 风格错误 body 用它。
/// Node 版对这两种响应就是直接发对象，不是 `{success,data}`。
pub fn raw_json(data: Value) -> Response {
    Json(data).into_response()
}

/// 管理 API 的成功信封：`{ success: true, data: ... }`。
/// 所有 /api/* 路由都用它，保证与 Node 版 sendJson 的形状一致。
pub fn ok_json(data: Value) -> Response {
    Json(json!({ "success": true, "data": data })).into_response()
}

/// 管理 API 的不带 data 的成功响应：`{ success: true }`
pub fn ok_empty() -> Response {
    Json(json!({ "success": true })).into_response()
}

/// 解析 JSON 请求体；空 body 视为 `{}`（对应 Node 版
/// `JSON.parse((await readRawBody(req)).toString('utf8') || '{}')`）。
pub fn parse_body(bytes: &[u8]) -> Result<Value, errors::GatewayError> {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(&text)
        .map_err(|error| errors::GatewayError::bad_request(format!("请求体不是合法 JSON: {error}")))
}

/// 从原始查询串里取一个参数（先按 `&` 切、再按 `=` 切，最后百分号解码）。
///
/// 缺失返回 `None`；**不区分「参数不存在」与「参数为空」**（`?id=` 与无此参数
/// 都得到 `Some("")`）—— 调用方若要按空值走另一条分支，自己 `filter` 一下，
/// 这也正是 `accounts_usage` 取 `id` 时的写法。
///
/// 为什么提到 HTTP 层：`/api/update` 的 `current` 与本条各有一份私有实现，
/// 两者口径必须一致（都对应 JS 的 `URLSearchParams`），各写一份迟早会漂。
/// 既有的那份私有实现保持原样未动（避免顺手改动已交付路径），新代码请用这个。
pub fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if name == key {
            return Some(percent_decode(value));
        }
    }
    None
}

/// 百分号解码（对应 URLSearchParams 的解码；`+` 也算空格，与表单语义一致）。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|text| u8::from_str_radix(text, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 把查询串里的时间戳解析成毫秒整数（供 `/api/logs` 与 `/api/stats/*` 共用）。
///
/// **为什么不放在某个 api 模块里**：两个路由模块都要按同一口径解析 `start` / `end`，
/// 各写一份的话两份实现迟早会漂（今天一个容忍小数、明天一个不容忍），
/// 而这两个参数在两端表达的是同一件事。与 `parse_body` 一样归到 HTTP 层公共设施。
///
/// 解析规则（**非法一律返回 None，由调用方忽略该边界，不报错**）：
///   - 空串 / 全空白 → None（前端把筛选框清空时会发 `?start=`，这是合法形态）
///   - 整数优先按 `i64` 直解，避免大数值经浮点往返丢精度
///   - 其余尝试 `f64`（容忍 JS 侧 `Date.now()/1000` 之类带小数的形态），
///     但**只接受整值**：时间戳带小数没有意义，截断会悄悄挪动区间边界
///   - 非有限值（NaN / inf）与超出 i64 范围的取值 → None
///
/// 为什么不报错：`start` / `end` 是筛选条件，为一次填错让整页（日志页 / 报表页）
/// 报错，不如把该维度当作没筛 —— 页面照常出数据，只是范围宽一点。
pub fn parse_query_ms(value: Option<&String>) -> Option<i64> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(number) = text.parse::<i64>() {
        return Some(number);
    }
    let number = text.parse::<f64>().ok().filter(|item| item.is_finite())?;
    if number.fract() != 0.0 || number.abs() > i64::MAX as f64 {
        return None;
    }
    Some(number as i64)
}
