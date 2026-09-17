//! 进程内 HTTP 服务器：WorkBuddy 本地网关的 Rust 实现。
//!
//! ── 为什么有这个模块 ────────────────────────────────────────
//! 旧架构是「Tauri 壳 + 外部 node 进程」：壳把 server.cjs 和完整 node.exe
//! （88MB）一起分发，启动时拉起子进程监听 127.0.0.1:3065。
//! 现在把 Node 后端整体重写成 Rust，作为**壳进程内**的 HTTP 服务器，
//! 继续监听同一个端口、保持 HTTP 契约与 Node 版完全一致 —— 于是
//! UI（desktop-tauri/ui/）、bridge.rs、commands.rs、gateway.rs 一行都不用改。
//! 收益：安装包从 ~95MB 缩到 ~10MB，升级时也不再有「杀不掉 node 子进程」的问题。
//!
//! **行为变化（重要）**：旧的「探测到 3065 已在跑就复用外部服务」逻辑取消了。
//! 进程内服务器必须自己 bind 成功 —— 功能都在本进程里，复用别人的端口
//! 等于把自己的管理 API 交给一个不受控的进程。端口被占用时直接报错，
//! 提示用户先结束旧网关（详见 `describe_bind_error`）。
//!
//! ── 全景结构（切片 1 建骨架，切片 2 补账号与登录，切片 3 补出网代理与计费，
//!    切片 4 补对话主链路，切片 5 补内容脱敏，切片 6 补定时签到与软件更新）──
//! ```text
//! server/
//!   mod.rs          模块总装：ServerState 构造、start()/停机信号
//!   http.rs         axum Router 组装、CORS、API Key 检查、404 兜底
//!   logging.rs      双通道日志（控制台 + logs.jsonl）
//!   config.rs       config.json 读写（全量保留未知字段）
//!   logs_store.rs   logs.jsonl 存储（append/查询/统计/清空）
//!   errors.rs       网关错误类型 + OpenAI 风格错误 payload
//!   api/
//!     mod.rs        路由模块登记
//!     health.rs     GET /health
//!     session.rs    GET /api/session、/api/session/login/*、/api/session/refresh|logout、
//!                   POST /auth/login、POST /auth/logout
//!     config_api.rs GET/POST /api/config
//!     logs_api.rs   GET /api/logs、/stats、/download、DELETE
//!     accounts.rs   /api/accounts*（增删改查/切换/排序/批量/导入导出/刷新）
//!     proxies.rs    /api/proxies*（Clash 实时读取 + 出口连通性测试）
//!     billing.rs    /api/usage、/api/checkin*、/api/activity/*（对照 server.mjs 871-911）
//!     chat.rs       POST /v1/chat/completions、GET /v1/models（对话主链路）
//!     desensitize.rs /api/desensitize*（词表维护 / 开关 / 角色 / 命中统计）
//!     auto_checkin.rs /api/auto-checkin*（定时签到设置 / 手动执行）
//!     update.rs     /api/update/*（软件更新检查 / 下载 / 进度 / 取消）
//!     endpoints.rs  GET /api/endpoints（接口清单）
//!   core/
//!     endpoints.rs 端点/版本/UA/上下文（唯一事实来源）
//!     account_store/  账号存储（优先级、迁移、CRUD、限额标记）
//!     account_transfer.rs  账号导入导出
//!     auth.rs      会话读取、getStatus、鉴权头、token 刷新
//!     auth_http.rs 上游请求发送与解包（管理接口）+ 鉴权错误类型
//!     login.rs     无头登录与登录任务表
//!     proxies.rs   账号级出网代理解析（Clash 读取在 clash.rs）
//!     clash.rs     Clash Verge 配置读取与快照缓存
//!     egress.rs    出网点（按出口缓存 reqwest Client）+ 出口连通性测试
//!     billing/     积分 / 签到（checkin.rs） / 运营活动
//!     models.rs    模型目录（内置清单 + /v3/config 远程刷新）
//!     routing.rs   账号选路（严格优先级 + 限额冷却判定）
//!     auto_checkin.rs 定时签到调度（30 秒轮询 + 当天去重 + 启动补签）
//!     update/      软件更新：
//!       mod.rs       管理器句柄 / 下载状态机 / 进度与取消
//!       version.rs   版本比较、域名白名单、资产挑选、文件名安全化（纯函数）
//!       client.rs    出网候选（直连 → Clash）与 GitHub 请求头
//!     desensitize/ 内容脱敏：
//!       mod.rs       词表读写 / 默认词表迁移 / 命中统计（句柄）
//!       engine.rs    纯函数：词表编译、文本改写、content/messages/body 遍历
//!     upstream/    对话转发：
//!       mod.rs       转发主链路（选路循环 / 429 轮换 / 去重排队 / SSE 流）
//!       request.rs   请求构造（头集合、URL、system 注入、错误解析）
//!       sse.rs       SSE reasoning 帧合并（跨 chunk 半行缓冲）
//!       aggregate.rs 非流式聚合（SSE → 完整 chat.completion）
//! ```
//!
//! ── 给后续切片留的接入点 ────────────────────────────────────
//!   - 新增受保护路由：往 `http::router` 的 `protected` 分组里加 `.route(...)`
//!     即可自动带 API Key 检查；免鉴权路由放 `public` 分组。
//!     管理 API 已全部就位（切片 1-6），切片 7 只剩打包收尾。
//!   - 账号与鉴权：`ServerState::store()` / `auth()` / `login()` 三个克隆句柄。
//!   - 模型目录与转发：`ServerState::models()` / `upstream()`。
//!   - 脱敏：`ServerState::desensitize()`（路由用）；对话链路里是
//!     `api::chat::desensitize_body`，它取 `core::desensitize::global()`。
//!   - 定时签到：`ServerState::auto_checkin()`；停机清理走
//!     `core::auto_checkin::stop_global()`（backend::shutdown 里调用）。
//!   - 软件更新：`ServerState::update()`。
//!   - 配置读写：`config::current()`（不读盘）+ `config::apply_update()`（写盘）。
//!   - 日志：`logging::log` / `logging::verbose` / `logging::log_event`。
//!   - 出网代理：`core::egress::client_for` 是唯一出网点（按出口复用连接池；
//!     管理接口走 `core::auth_http::send_raw/request_via`，对话转发走
//!     `core::upstream::request::send_chat_request`，GitHub 走
//!     `core::update::client::fetch_with_egress`）。
//!
//! ── 关于 dead_code ─────────────────────────────────────────
//! 切片 7 已清掉本模块曾经的 `#![allow(dead_code)]` 抑制（它一度掩盖了
//! config/logs_store/errors/egress 里的若干未使用项）。现在**本模块零 warning**：
//! 真正没人用的函数已删；排障与路由登记等有意保留的设施逐个标注 `#[allow(dead_code)]`
//! 并写明保留理由，便于后续定位。

pub mod api;
pub mod config;
pub mod core;
pub mod errors;
pub mod http;
pub mod logging;
pub mod logs_store;

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::sync::oneshot;

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::AuthService;
use crate::server::core::auto_checkin::AutoCheckin;
use crate::server::core::billing::BillingService;
use crate::server::core::desensitize::Desensitizer;
use crate::server::core::login::LoginService;
use crate::server::core::models::ModelCatalog;
use crate::server::core::update::UpdateManager;
use crate::server::core::upstream::UpstreamService;

/// 服务器共享状态。handler 通过 `axum::extract::State` 拿到它的克隆。
///
/// `port` / `config_dir` 是启动即确定、全程不变的常量；
/// 可变状态一律**自带内部锁再放进来**（账号存储、鉴权、登录任务表、模型目录、
/// 转发器都是 `Clone` 的轻量句柄），而不是让本结构变成一堆 Mutex 字段 ——
/// 这样 handler 拿到状态就能直接用，也不必关心怎么加锁。
#[derive(Clone)]
pub struct ServerState {
    /// 监听端口（默认 3065，可用 WORKBUDDY_PROXY_PORT 覆盖）
    pub port: u16,
    /// 配置目录（`~/.workbuddy-proxy`），与壳侧 gateway::config_dir() 同源
    pub config_dir: PathBuf,
    /// 账号存储句柄（内部一把 Mutex，绝不在持锁时做网络请求）
    store: AccountStore,
    /// 鉴权服务句柄（会话读取 / token 刷新）
    auth: AuthService,
    /// 登录服务句柄（无头登录 + 登录任务表）
    login: LoginService,
    /// 计费服务句柄（积分 / 签到 / 运营活动；出网按 session.proxy 挂出口）
    billing: BillingService,
    /// 模型目录句柄（内置清单 + /v3/config 远程刷新；内部 RwLock）
    models: ModelCatalog,
    /// 对话转发器句柄（选路 / 429 轮换 / SSE 透传 / 去重排队）
    upstream: UpstreamService,
    /// 内容脱敏句柄（词表 / 开关 / 角色 / 命中统计；内部 RwLock）
    desensitize: Desensitizer,
    /// 定时签到句柄（轮询调度 + 启动补签；内部 Mutex + 后台任务）
    auto_checkin: AutoCheckin,
    /// 软件更新句柄（GitHub Release 检测 / 安装包下载；内部 Mutex）
    update: UpdateManager,
}

impl ServerState {
    /// 构造服务状态，并完成启动期的准备工作。
    ///
    /// 顺序很重要（对照 server.mjs 336-353 行）：
    ///   1. 先装日志库 —— 后面所有模块的日志才能入库；
    ///   2. 读配置 —— 启动横幅要打真实的默认模型/语言；
    ///   3. 账号库载入 + 旧版 auth.json 迁移（仅账号列表为空时）+ 优先级去重迁移。
    pub fn bootstrap(port: u16) -> Self {
        let config_dir = config::config_dir();
        // 与 Node 版一致：verbose 由环境变量 WORKBUDDY_VERBOSE=1 打开，
        // 决定 debug 级别日志要不要入库（默认只有 info 以上入库，避免刷屏）
        let verbose = std::env::var("WORKBUDDY_VERBOSE").map(|v| v.trim() == "1").unwrap_or(false);
        logging::init_store(&config_dir, verbose);
        let snapshot = config::init();

        // 账号库 + 鉴权 + 登录 + 计费（四者共享同一个 store 句柄）
        let store = AccountStore::with_config_dir();
        let context = core::endpoints::default_context();
        let auth = AuthService::new(store.clone(), context);
        let login = LoginService::new(auth.clone(), store.clone());
        let billing = BillingService::new(auth.clone());
        let models = ModelCatalog::new();
        let upstream = UpstreamService::new(store.clone(), auth.clone());
        // 内容脱敏：默认开启，词表与开关持久化在 {config_dir}/desensitize.json
        // （对照 server.mjs 398 行）。WORKBUDDY_DESENSITIZE=1/0 只覆盖**本次运行**，
        // 不写回文件（persist:false）—— 避免启动脚本顺带改掉用户在前端的设置
        let desensitize = core::desensitize::init(&config_dir);
        match std::env::var("WORKBUDDY_DESENSITIZE").as_deref() {
            Ok("0") => {
                desensitize.set_enabled(false, false);
            }
            Ok("1") => {
                desensitize.set_enabled(true, false);
            }
            // 未设置（或其它值）→ 沿用文件里的开关
            _ => {}
        }

        // ── 定时签到与软件更新（切片 6）──────────────────────────
        // 两者都是进程级句柄（同 config / logging / desensitize 的模式）：
        // 定时签到的 stop() 要从**停机路径**（backend::shutdown，只有 Tauri 的
        // AppState）调到，所以必须能从全局拿到；这里装入后 ServerState 里那份
        // 与全局那份是同一实例。
        let auto_checkin = core::auto_checkin::init_global(AutoCheckin::new(
            store.clone(),
            billing.clone(),
        ));
        // 更新管理器：下载目录 `{config_dir}/updates`，与壳侧 update::download_dir() 同源
        let update = core::update::init_global(UpdateManager::new(config_dir.clone()));

        // 旧版单账号 auth.json 迁移（仅当账号列表为空时导入一次）
        let legacy = read_legacy_session();
        if let Some(legacy) = legacy {
            if let Some(account) = store.import_legacy_session(&legacy) {
                let name = account
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("旧登录态");
                logging::log("[Accounts]", &format!("✅ 旧版登录态已迁移为账号: {name}"));
            }
        }
        // 优先级去重迁移：历史数据里并列的优先级会被重新编号成连续序号，
        // 相对顺序保持不变（所以升级后实际转发顺序不变）
        store.migrate_priorities();

        let state = Self {
            port,
            config_dir,
            store,
            auth,
            login,
            billing,
            models,
            upstream,
            desensitize,
            auto_checkin,
            update,
        };
        logging::log("[Server]", "WorkBuddy 本地代理（Rust 进程内服务）启动中…");
        logging::log("[Config]", &format!("API 端口: {}", port));
        logging::log("[Config]", "API 监听地址: 127.0.0.1");
        logging::log(
            "[Config]",
            &format!(
                "API Key 认证: {}",
                if snapshot.api_key_set() { "✅ 已启用" } else { "❌ 未启用" }
            ),
        );
        logging::log("[Config]", &format!("默认模型: {}", snapshot.default_model()));
        logging::log("[Config]", &format!("计费语言: {}", snapshot.locale()));
        // 对照 workbuddy-cli.mjs 的 logStartupConfig：脱敏状态一行（含词表路径），
        // 由环境变量强制覆盖时补一句说明（Node 同样如此）
        let d = state.desensitize.state();
        let enabled_flag = d.get("enabled").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let term_count = d.get("termCount").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let roles = d
            .get("roles")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、")
            })
            .unwrap_or_default();
        let forced = matches!(
            std::env::var("WORKBUDDY_DESENSITIZE").as_deref(),
            Ok("0") | Ok("1")
        );
        logging::log(
            "[Config]",
            &format!(
                "内容脱敏: {}（{term_count} 个词，作用角色 {roles}，词表 {}）{}",
                if enabled_flag { "✅ 已启用" } else { "❌ 已关闭" },
                state.desensitize.file().display(),
                if forced { "（本次运行由环境变量覆盖）" } else { "" },
            ),
        );
        logging::log("[Config]", &format!("配置目录: {}", state.config_dir.display()));
        let current = state.store.current_account_id();
        logging::log(
            "[Accounts]",
            &format!(
                "账号列表: {}（当前账号 {}）",
                state.store.file().display(),
                current.as_deref().unwrap_or("无")
            ),
        );
        // 对照 server.mjs 1028-1042：有可用登录态时补一行凭证来源，并异步刷新模型目录
        let summary = state.auth.get_config_summary();
        if summary
            .get("configured")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let source = summary
                .get("authSource")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("未知");
            let account = summary
                .get("currentAccountId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("未知");
            logging::log("[Init]", &format!("✅ 凭证来源: {source}（账号 {account}）"));
            if let Some(expires_at) = summary
                .get("tokenExpiresAt")
                .and_then(serde_json::Value::as_f64)
            {
                let left = expires_at - logging::now_ms() as f64;
                logging::log(
                    "[Init]",
                    &format!(
                        "   token {}",
                        if left > 0.0 {
                            format!("{} 分钟后过期", (left / 60_000.0).round() as i64)
                        } else {
                            "已过期（将自动刷新）".to_string()
                        }
                    ),
                );
            }
            // 启动时刷新一次目录：对照 Node 的 `void refreshModelCatalog()`
            let models = state.models.clone();
            let store = state.store.clone();
            let auth = state.auth.clone();
            tauri::async_runtime::spawn(async move {
                models.refresh_with_current_account(&store, &auth).await;
            });
        } else {
            let reason = summary
                .get("unavailableReason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(core::auth::UNCONFIGURED_REASON);
            logging::log("[Init]", &format!("⚠️  暂无可用登录态: {reason}"));
        }

        // 定时签到：开启时起调度，今天还没签且时间点已过则补签一次
        // （对照 server.mjs 1055-1058：日志文案逐字一致）。
        // 放在 bootstrap 末尾而不是 start() 里：调度循环与 HTTP 监听彼此独立，
        // 且 start() 的调用方（backend::ensure_ready）拿不到「配置里是否开启」的
        // 判定结果；Node 版也是在 server.listen 之前、同一个 main() 里做的。
        let checkin_state = state.auto_checkin.state();
        if checkin_state
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let time = checkin_state
                .get("time")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(core::auto_checkin::DEFAULT_TIME);
            logging::log("[Checkin]", &format!("定时签到已启用：每天 {time} 执行"));
            state.auto_checkin.start();
        }
        state
    }

    /// 账号存储句柄
    pub fn store(&self) -> &AccountStore {
        &self.store
    }

    /// 鉴权服务句柄
    pub fn auth(&self) -> &AuthService {
        &self.auth
    }

    /// 登录服务句柄
    pub fn login(&self) -> &LoginService {
        &self.login
    }

    /// 计费服务句柄（积分 / 签到 / 运营活动）
    pub fn billing(&self) -> &BillingService {
        &self.billing
    }

    /// 模型目录句柄（/v1/models、/api/session、/health 与聊天路由的模型校验共用）
    pub fn models(&self) -> &ModelCatalog {
        &self.models
    }

    /// 对话转发器句柄
    pub fn upstream(&self) -> &UpstreamService {
        &self.upstream
    }

    /// 内容脱敏句柄（词表路由、/api/config 与 /api/session 的摘要共用）
    pub fn desensitize(&self) -> &Desensitizer {
        &self.desensitize
    }

    /// 定时签到句柄（/api/auto-checkin* 三条路由 + 启动调度）
    pub fn auto_checkin(&self) -> &AutoCheckin {
        &self.auto_checkin
    }

    /// 软件更新句柄（/api/update/* 四条路由）
    pub fn update(&self) -> &UpdateManager {
        &self.update
    }
}

/// 读旧版单账号 auth.json（缺失/损坏都当没有，对应 Node 版 `loadStoredSession`）
fn read_legacy_session() -> Option<serde_json::Value> {
    let path = core::endpoints::legacy_auth_file();
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let token = value
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(serde_json::Value::as_str)?;
    if token.is_empty() {
        return None;
    }
    Some(value)
}

/// 启动进程内服务器：同步 bind（失败立刻返回可读错误）+ 异步 serve。
///
/// 返回停机信号发送端：调用方（state::BackendHandle）持有它，
/// 退出时 `send(())` 触发 graceful shutdown。
///
/// ── 为什么 bind 用同步 std 监听、而 tokio 包装放到 spawn 里 ──
/// `tokio::net::TcpListener::from_std` 会向 reactor 注册句柄，
/// **必须在 Tokio 运行时上下文里调用**，否则直接 panic
/// （本项目 release 是 panic=abort，那会带走整个桌面应用）。
/// 而本函数可能从任意线程被调用（Tauri 的 setup 钩子在主线程、
/// commands 在异步上下文），所以：
///   1. 同步段只做 `std::net::TcpListener::bind` —— 这一步不需要运行时，
///      且端口占用这个最常见的失败能立刻拿到 OS 错误码返回给调用方；
///   2. 需要运行时的 from_std 与 serve 都放进 `tauri::async_runtime::spawn`，
///      任务体一定跑在运行时的工作线程上，reactor 必然可用。
pub fn start(state: &ServerState) -> Result<oneshot::Sender<()>, String> {
    let router = http::router(state.clone());
    let addr = SocketAddr::from(([127, 0, 0, 1], state.port));

    let listener = std::net::TcpListener::bind(addr)
        .map_err(|error| describe_bind_error(state.port, &error))?;
    // 立刻转成非阻塞：从这一刻起到 serve 接手之间，若有连接进来，
    // 阻塞式 accept 会把工作线程卡住。转非阻塞放在同步段更安全。
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("设置监听为非阻塞失败: {error}"))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tauri::async_runtime::spawn(async move {
        // 运行时上下文在这里必然成立，from_std 不会 panic
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                logging::log("[Server]", &format!("❌ 创建异步监听失败: {error}"));
                return;
            }
        };
        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            // 收到停机信号（或发送端被丢弃）即结束 accept 循环，
            // 已在处理中的请求会跑完再退出
            let _ = shutdown_rx.await;
        });
        match server.await {
            Ok(()) => logging::log("[Server]", "服务已停止"),
            Err(error) => logging::log("[Server]", &format!("❌ 服务异常退出: {error}")),
        }
    });

    Ok(shutdown_tx)
}

/// 端口占用时的中文说明。
///
/// 旧版桌面端会「探测到 3065 已在跑就复用」，所以机器上很可能还留着
/// 一个 node server.mjs 或旧版桌面端；升级后进程内服务器必须自己 bind，
/// 这时把原因讲清楚，比抛一个裸的 "os error 10048" 有用得多。
fn describe_bind_error(port: u16, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        return format!(
            "{port} 端口被占用：可能有旧版本网关仍在运行\
             （如 node server.mjs 或旧版桌面端），请先结束它再启动。\
             也可用环境变量 WORKBUDDY_PROXY_PORT 换一个端口。\
             （系统错误: {error}）"
        );
    }
    format!("监听 127.0.0.1:{port} 失败: {error}")
}
