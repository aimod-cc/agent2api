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
//!   `public`    免鉴权：/health、/api/session、/api/endpoints
//!               （Node 版这三条确实都没调 checkApiKey）
//!   `protected` 需鉴权：/api/config、/api/logs*、/api/stats*、/api/retention、
//!               /api/accounts*、/api/proxies*、
//!               /api/usage、/api/checkin*、/api/activity/*、/api/desensitize*、
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

/// 全局请求体上限：32MB（对应 Node 版 MAX_BODY_SIZE）。
/// 后续 /v1/chat/completions 会带长上下文的大请求体，默认 2MB 不够用。
pub const MAX_BODY_SIZE: usize = 32 * 1024 * 1024;

/// CORS 允许的方法，逐字照抄 Node 版 sendCORS。
///
/// 注意：这里**不含 PATCH** —— Node 版就是这样，虽然账号批量修改等接口
/// 在服务端支持 PATCH，但预检响应里没声明。不做「顺手修复」，
/// 浏览器端若有 PATCH 需求应由后续切片对照 Node 版行为一起评估。
const CORS_METHODS: &str = "GET, POST, OPTIONS, DELETE";
/// CORS 允许的请求头，逐字照抄 Node 版
const CORS_HEADERS: &str = "Content-Type, Authorization, x-api-key";

/// 组装完整路由。
pub fn router(state: ServerState) -> Router {
    // 免鉴权：/health（壳侧就绪探测）、/api/session（前端首屏状态）、
    // /api/endpoints（接口清单，排查用）
    //
    // /v1/models 也在这一组：Node 版这条**不查** API Key ——
    // 它是只读探针，OpenAI 客户端常在配置 key 之前先拉模型列表
    // （README 的接口说明里也把 /v1/models 列在免鉴权探针里）。
    let public = Router::new()
        .route("/health", get(api::health::handle))
        .route("/api/session", get(api::session::get_session))
        .route("/api/endpoints", get(api::endpoints::handle))
        .route("/v1/models", get(api::chat::list_models))
        // CatPaw 网页登录的 loopback 回调：**上游浏览器直接 POST 到这里**
        // （redirect 指向本网关自己的 loopback 端口，见 core::login::catpaw），
        // 所以它必须免鉴权 —— 调用方是美团 passport 页面，它没有我们的 API Key。
        // 安全性由一次性 `state` 承担（逐字比对，见该处理函数的说明）。
        // 注意方向：与小浣熊那条 `/api/session/login/callback` 相反，那条是
        // **我们自己的登录窗口**转交上来的，因此留在 protected 组。
        .route(
            "/api/session/login/catpaw-callback",
            post(api::session::login_catpaw_callback),
        );

    // 需鉴权：Node 版对这些路径都调用了 checkApiKey
    let protected = Router::new()
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
        // 三条 /api/stats/* 与两条 /api/retention 都挂 protected：
        // 它们能读到全部请求明细（含模型、账号、token 用量）并能删数据 / 改保留期，
        // 与 /api/logs 同级敏感，必须和日志接口一样走 API Key 检查。
        // 未知 /api/stats/* 子路径返回管理信封 404（照 logs_api::not_found 的做法，
        // 而不是全局兜底的 OpenAI 形状）—— 同一前缀下的 404 形状保持一致。
        .route("/api/stats/summary", get(api::stats_api::stats_summary))
        .route(
            "/api/stats/requests",
            get(api::stats_api::stats_requests).delete(api::stats_api::clear_stats_requests),
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
        .route("/api/session/refresh", post(api::session::session_refresh))
        .route("/api/session/logout", post(api::session::session_logout))
        .route("/auth/login", post(api::session::auth_login))
        .route("/auth/logout", post(api::session::auth_logout))
        // ── 对话链路（对照 server.mjs 977-981）──
        // Node 版这条查 API Key，所以挂 protected；/v1/models 不查，
        // 挂在上面 public 组（分组判据见各自注释）
        .route("/v1/chat/completions", post(api::chat::chat_completions))
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
        .route("/api/keys", get(api::keys_api::list_keys).post(api::keys_api::create_key))
        .route("/api/keys/{id}", patch(api::keys_api::update_key).delete(api::keys_api::delete_key))
        // ── 内容脱敏（对照 workbuddy-desensitize-routes.mjs）──
        // 八条端点全部走 checkApiKey，所以整组挂 protected。
        // 用 any(...) 注册三条入口（无尾段 + 尾斜杠 + 通配尾段），方法/路径判定
        // 交给 api::desensitize::entry —— 与账号路由同一个理由：Node 是
        // 「前缀命中 → 按 action 逐条判」的结构，拆成独立 axum 路由会让
        // `/api/desensitize/` 与未知子路径的 404 形状跟 Node 分叉。
        .route("/api/desensitize", any(api::desensitize::entry))
        .route("/api/desensitize/", any(api::desensitize::entry))
        .route("/api/desensitize/{*rest}", any(api::desensitize::entry))
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
        // 与 /api/accounts、/api/desensitize 同一取舍：拆成独立 axum 路由会让
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
        // accounts/proxies/billing/chat/desensitize/auto-checkin/update），
        // 路由表不再有缺口。切片 7 只需处理打包收尾：
        //   ① 从 package.json / resources 里摘掉旧的 node 后端产物
        //      （server.cjs + node.exe 的随包分发），确认 tauri.conf.json 的
        //      resources 与 bundle 配置同步；
        //   ② 清掉本文件与 server/mod.rs 里的 `#![allow(dead_code)]` 抑制，
        //      按 warning 逐条处理遗留的未使用公共设施；
        //   ③ 版本号与 `.zcode/release-notes` 更新（安装包名与 asset 命名约定
        //      要与 update 的 pickInstaller 规则相符：带 setup 的 .exe 优先）。
        .layer(middleware::from_fn(require_api_key));

    Router::new()
        .merge(public)
        .merge(protected)
        .fallback(not_found)
        // 方法不匹配的兜底：Node 版没有 405 概念，一律落到 404 分支，
        // 所以这里把 405 也转成同样的 Not found 文案，保持行为一致
        .method_not_allowed_fallback(method_not_allowed)
        // CORS 与 body 限制必须在最外层：404 与错误响应也要带 CORS 头
        .layer(middleware::from_fn(cors))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
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
async fn require_api_key(request: Request, next: Next) -> Response {
    // 每次都读内存快照（不是读文件），所以「刚保存的新 key」下一个请求就生效
    let snapshot = crate::server::config::current();
    let keys = snapshot.active_api_keys();
    if keys.is_empty() {
        return next.run(request).await;
    }

    if keys.iter().any(|expected| request_matches_key(&request, expected)) {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();
    let method = request.method().as_str().to_string();
    logging::log("[Security]", &format!("❌ 拒绝未授权的请求: {method} {path}"));
    errors::unauthorized_response()
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
fn request_matches_key(request: &Request, expected: &str) -> bool {
    let headers = request.headers();

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
