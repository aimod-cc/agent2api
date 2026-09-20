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
//!     stats_api.rs  GET /api/stats/summary、/api/stats/requests、DELETE /api/stats/requests、
//!                   GET/PUT /api/retention（统计报表 + 数据保留策略）
//!     accounts.rs   /api/accounts*（增删改查/切换/排序/批量/导入导出/刷新）
//!     proxies.rs    /api/proxies*（Clash 实时读取 + 出口连通性测试）
//!     billing.rs    /api/usage、/api/checkin*、/api/activity/*（对照 server.mjs 871-911）
//!     chat.rs       POST /v1/chat/completions、GET /v1/models（对话主链路）
//!     desensitize.rs /api/desensitize*（词表维护 / 开关 / 角色 / 命中统计）
//!     auto_checkin.rs /api/auto-checkin*（定时签到设置 / 手动执行）
//!     scheduled_tasks.rs /api/scheduled-tasks*（间隔型定时任务：开关 / 间隔 / 立即执行）
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
//!     models/      模型目录（workbuddy 单家：内置清单 + /v3/config 远程刷新）
//!     routing.rs   账号选路（严格优先级 + 限额冷却判定）
//!     auto_checkin.rs 定时签到调度（30 秒轮询 + 当天去重 + 启动补签）
//!     credential_maintenance.rs 凭证自动维护（遍历账号 → 刷新临期凭证；
//!                     调度由 `scheduled_tasks` 按配置的开关与间隔驱动）
//!     scheduled_tasks.rs 间隔型定时任务注册表与调度循环（凭证维护 / 模型刷新；
//!                     配置文件在 config.json 的 scheduledTasks，路由见 api::scheduled_tasks）
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
//!     sse.rs       SSE reasoning 帧合并（跨 chunk 半行缓冲）
//!     aggregate.rs 非流式聚合（SSE → 完整 chat.completion）
//!     usage.rs     usage 旁路槽（token 用量 / 承载账号 / 尝试账号数）
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
//!   - 间隔型定时任务：`core::scheduled_tasks`（注册表 + 调度循环，循环在
//!     `bootstrap` 末尾起一次）；开关与间隔来自 `config::scheduled_settings()`
//!     （内存快照，循环每轮现读 —— 所以改完设置下一轮生效，不重启进程）。
//!     本模块不再持有那两条任务的间隔常量，加一条新任务只需改注册表。
//!   - 软件更新：`ServerState::update()`。
//!   - 请求统计：写入侧是 `api::chat` 的记账点 → `RequestStats::record`；
//!     读取侧是 `api::stats_api` 的三条路由（报表 / 明细查询 / 清空），
//!     句柄取 `ServerState::request_stats()`；退出时在 `start()` 的 serve 任务
//!     收尾处 `flush()`。
//!   - 数据保留期：`config::retention_settings()`（内存快照，热路径用）与
//!     `config::set_retention()`（写盘）；三个消费点都走**回调动态取值**
//!     （`RequestStats` / `LogStore` / `PUT /api/retention` 的立即清理），
//!     所以改完设置不需要重启进程。
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
pub mod config_migration;
pub mod core;
pub mod errors;
pub mod http;
pub mod logging;
pub mod logs_store;
pub mod request_stats;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::port_conflict::{ConflictKind, PortConflict};
use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::AuthService;
use crate::server::core::auto_checkin::AutoCheckin;
use crate::server::core::billing::BillingService;
use crate::server::core::desensitize::Desensitizer;
use crate::server::core::login::LoginService;
use crate::server::core::models::ModelCatalog;
use crate::server::core::update::UpdateManager;
use crate::server::core::upstream::UpstreamService;
use crate::server::request_stats::{RequestStats, Retention};

/// 服务器共享状态。handler 通过 `axum::extract::State` 拿到它的克隆。
///
/// `port` / `config_dir` 是启动即确定、全程不变的常量；
/// 可变状态一律**自带内部锁再放进来**（账号存储、鉴权、登录任务表、模型目录、
/// 转发器都是 `Clone` 的轻量句柄），而不是让本结构变成一堆 Mutex 字段 ——
/// 这样 handler 拿到状态就能直接用，也不必关心怎么加锁。
#[derive(Clone)]
pub struct ServerState {
    /// 监听端口（默认 3065，可用 AGENT2API_PROXY_PORT 覆盖；旧名 WORKBUDDY_PROXY_PORT 仍可读）
    pub port: u16,
    /// 配置目录（`~/.agent2api`），与壳侧 gateway::config_dir() 同源
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
    /// 请求统计存储句柄（明细 + 按天聚合）。
    ///
    /// 外面包一层 `Arc` 而不是像其它 store 那样自带内部 `Arc<Inner>`：
    /// 存储本体的公开接口（`RequestStats::new`）已经定型，本切片不再改它，
    /// 而 `ServerState` 是 `Clone` 的（handler 靠克隆拿状态），
    /// 所以共享语义由这层 `Arc` 提供 —— 与 `AccountStore` 的 `Arc<Inner>`
    /// 是同一个效果，只是包的位置在外侧。
    request_stats: Arc<RequestStats>,
}

impl ServerState {
    /// 构造服务状态，并完成启动期的准备工作。
    ///
    /// 顺序很重要（对照 server.mjs 336-353 行）：
    ///   1. 读配置 —— 保留期（日志天数）要在装日志库之前就位；
    ///   2. 装日志库 —— 后面所有模块的日志才能入库；
    ///   3. 账号库载入 + 旧版 auth.json 迁移（仅账号列表为空时）+ 优先级去重迁移。
    ///
    /// ①/② 的顺序是本切片刚从「先装日志库」调过来的：日志库载入历史时就会按
    /// 保留天数裁一次，而保留天数来自配置 —— 若配置还没读进来，那次裁剪会退回
    /// 默认 30 天，把用户设了更长保留期的旧日志当场裁掉并落盘（**不可逆的数据丢失**）。
    /// 两个函数都不写日志，交换它们不会让任何一行启动日志丢失。
    ///
    /// ── 一次性目录迁移（`~/.workbuddy-proxy` → `~/.agent2api`）不在这里 ──
    /// 它必须早于**任何**会写配置目录的动作，而 `bootstrap` 已经是「桌面设置
    /// 已读过、窗口已建好、日志库即将装」的阶段 —— 放在这里就晚了：迁移失败
    /// 后只要有人写一次盘（settings::save / config::save_raw / 日志库），
    /// 新目录就被建出来，`target.exists()` 从此为真，迁移再也无法重试。
    /// 因此调用点上移到壳侧 `lib.rs` 的 setup 第一步（`settings::load()` 之前），
    /// 那里失败会提示用户并结束本次启动，不写任何配置文件。
    ///
    /// 这里保留一道**防御性校验**（`config_migration::pending_reason`）：若旧目录
    /// 仍在、新目录仍不存在，说明本该执行的迁移没有执行（例如后续有人挪动了
    /// 调用点）。此时直接返回错误、不做任何初始化 —— 继续下去的第一件事
    /// （`config::init` → 读盘；`logging::init_store` → 建目录）就会把新目录
    /// 建出来，让迁移永远失去重试机会。失败以 `Err` 往上传（`ensure_ready`
    /// 原样透给 UI 的 `backend:error`），不 panic、也不静默降级。
    pub fn bootstrap(port: u16) -> Result<Self, String> {
        if let Some(reason) = config_migration::pending_reason() {
            return Err(reason);
        }
        let config_dir = config::config_dir();
        // 与 Node 版一致：verbose 由环境变量 AGENT2API_VERBOSE=1 打开
        // （旧名 WORKBUDDY_VERBOSE 仍可读，新名优先），
        // 决定 debug 级别日志要不要入库（默认只有 info 以上入库，避免刷屏）
        let verbose = env_flag(&["AGENT2API_VERBOSE", "WORKBUDDY_VERBOSE"]);
        let snapshot = config::init();
        // 两类数据的保存目录：config.json 里写了绝对路径（用户在设置页改过位置）
        // 就用它，否则（缺省 / 写坏）都落配置目录。必须用**解析后**的目录构造
        // 两个存储 —— 它们的目录由此定死，运行期搬家靠 relocate（storage_api）。
        let dirs = config::storage_dirs();
        logging::init_store(&dirs.log_dir, verbose);
        // 调试模式的原始报文存储：无论开关是否打开都初始化 —— 开关是**逐请求**
        // 判定的（改完设置下一个请求就生效），存储没就绪会让开启后的第一批
        // 请求无处可落
        core::debug_traffic::init(dirs.debug_dir.clone());

        // 账号库 + 鉴权 + 登录 + 计费（四者共享同一个 store 句柄）
        let store = AccountStore::with_config_dir();
        let context = core::endpoints::default_context();
        let auth = AuthService::new(store.clone(), context);
        let login = LoginService::new(auth.clone(), store.clone());
        let billing = BillingService::new(auth.clone());
        // 模型目录：**进程级单例**（Agent2API 改造 W2a-T2）。聚合模型目录
        // （core::providers::catalog）只收 &AccountStore，不该让调用方层层传目录，
        // 因此目录自身也做成进程级句柄（与 config / desensitize / auto_checkin
        // 同一模式）：`core::models::global_catalog()` 与这里的 `models` 是
        // **同一实例**（共享同一把 RwLock），刷新对两边同时可见。
        let models = core::models::global_catalog();
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

        // 请求统计：数据目录默认与 LogStore **同源**（都是 config_dir）—— 明细
        // `requests.jsonl` 与聚合 `request-daily.jsonl` 落在配置目录里，与
        // logs.jsonl 并排；用户在设置页改过「请求日志保存位置」时用的是自定义目录
        // （`config::storage_dirs()` 已按配置解析好）。两个目录互不约束。
        //
        // 保留期走**回调**，每次裁剪时动态读配置：明细天数来自
        // `requestRetentionDays`、聚合天数来自 `dailyRetentionDays`
        // （`config::retention_settings()` 读的是 config::init() 装好的内存快照，
        // 而 `PUT /api/retention` 会同步刷新它）——
        // 于是设置页改完天数，下一次记账 / prune 立即生效，**不需要重启进程**，
        // 也不必改这里的构造方式。这也正是 RequestStats::new 把保留期做成
        // 回调而不是构造参数的原因（见那边的注释）。
        let request_stats = Arc::new(RequestStats::new(dirs.request_stats_dir, || {
            let settings = config::retention_settings();
            Retention {
                request_days: settings.request_days,
                daily_days: settings.daily_days,
            }
        }));

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
        // 启动期数据迁移（一次读、一次写，见 store_admin::migrate_startup）：
        //   ① 历史账号补 `provider: "workbuddy"`（Agent2API 惰性迁移，§3.2）
        //   ② 优先级去重 —— 逐 provider 分组重编号，相对顺序保持不变
        //      （所以升级后实际转发顺序不变）
        //   ③ Cline 拆池改名（`provider: "cline"` + `pool` → cline-free / cline-pass）
        store.migrate_startup();
        // 模型规则的同一轮迁移（`modelRules` 里的 disabled / hidden / mappings /
        // seeded 都按 provider id 记账，`"cline"` 这个 id 一没就全部匹配不上）。
        // 与账号迁移分开读一次 config.json：两者在不同的文件（config / accounts），
        // 而且**必须各自幂等**（只有一个存在迁移内容时不该拖着另一个一起写盘）。
        // 放这里而不是更早：它要读 modelRules，而那时 config 已经 init 完毕。
        if let Some(summary) = core::model_rules::migrate_cline_split() {
            logging::log("[Models]", &summary);
        }
        // 小浣熊旧数据一次性导入（架构文档 §3.3，W3-T4）：
        //   `~/.raccoon-proxy/accounts.json`（旧网关多账号）+
        //   `~/.box-agent/config/auth.json`（桌面端实时登录态）→ raccoon 账号。
        // 触发条件是「raccoon 账号列表为空 **且** config 里没有 raccoonImported」，
        // 幂等且失败不阻断启动（两个来源各自缺失就跳过，见该方法的说明）。
        // 放在 migrate_startup 之后：迁移已把历史账号归好组，导入的新账号
        // 不会影响这一轮的去重判定（导入本身按 provider 内号段追加）。
        store.import_legacy_raccoon_data();
        // CatPaw 旧数据一次性导入（架构文档 §9，W5-T-d4）：
        //   `~/.meituan-catpaw/catpaw-proxy-accounts.json`（原项目多账号）+
        //   `~/.meituan-catpaw/auth.json`（桌面端实时登录态）→ catpaw 账号。
        // 触发条件是「catpaw 账号列表为空 **且** config 里没有 catpawImported」，
        // 与上面那条同构（标记键、幂等性、失败不阻断启动都一致）。
        store.import_legacy_catpaw_data();
        // AutoClaw 旧数据一次性导入（架构文档 §10.2 末条，W4b-T-c2）：
        //   `~/.autoclaw-proxy/accounts.json`（原项目多账号）+
        //   `%APPDATA%/AutoClaw/auth.json`（桌面端实时登录态；备来源
        //   `~/.openclaw-autoclaw/openclaw.json`）→ autoclaw 账号。
        // 触发条件是「autoclaw 账号列表为空 **且** config 里没有 autoclawImported」，
        // 与上面两条同构（标记键、幂等性、失败不阻断启动都一致）。
        // 三条导入**追加式共存**：各看自己那一家的账号列表与自己那个标记键，
        // 互不影响，先后顺序不改变任何结果（后一条只增不改前面的账号）。
        store.import_legacy_autoclaw_data();

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
            request_stats,
        };
        logging::log("[Server]", "Agent2API 多提供商本地网关（Rust 进程内服务）启动中…");
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
            // 启动时的模型目录刷新**不在这里**：它已归入定时任务注册表
            // （`core::scheduled_tasks` 的首轮跑一次），于是「在定时任务页关掉
            // 模型目录刷新 = 启动也不刷」这条一致性成立。改造前这里是无条件
            // spawn 一次 `refresh_implemented`（对照 Node 的
            // `void refreshModelCatalog()`），那个行为在开关默认开启时保持不变。
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
        //
        // 注意它**不在** `scheduled_tasks` 的清单里：自动签到是「每天定点」型，
        // 与那个模块的「等间隔重复」不是同一个形状（理由见那边的模块头）。
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

        // 间隔型定时任务（凭证自动维护 / 模型目录刷新）：
        // 起一个循环，按各自配置的开关与间隔重复执行。
        //
        // ── 改造前后的行为对照 ────────────────────────────────────
        // 改造前这里是两处硬编码：凭证维护 `loop { 刷; sleep(600s) }`（spawn 出来
        // 立刻刷一次）、模型目录在「有可用登录态」的分支里 spawn 一次启动刷新。
        // 现在两者都由注册表驱动，启动时的那一次变成「首轮排期立刻到点」——
        // 开关默认开启，因此**默认行为一致**，且关掉任务后启动也不刷。
        //
        // 一处有意的差异：模型目录的启动刷现在不再被「有无登录态」挡住
        // （原先写在 `configured` 分支里）。于是全新安装、还没登录时也会拉一次
        // 各家的清单 —— 小浣熊的公开目录本来就不需要凭证（见其 `refresh_models`
        // 里空 token 的分支），workbuddy 无登录态则早退返回「缺少登录态」而不打
        // 网络，CatPaw / AutoClaw 是静态清单直接跳过。代价只是首启多一次
        // 无害的请求，换来的是「这一页的开关说了算」这条一致性。
        //
        // ── 为什么循环里不处理停机信号 ────────────────────────────
        // 服务器停机时进程会结束，任务随之消失。用 tauri 的 spawn
        // （与 auto_checkin 同一理由）保证从非 tokio 上下文调用也能进入全局运行时。
        core::scheduled_tasks::spawn(state.store.clone(), state.update.clone());

        Ok(state)
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

    /// 请求统计句柄（对话链路的记账点写入；报表/明细 API 由后续切片接线）
    ///
    /// 返回 `Arc<RequestStats>` 的所有权克隆（而不是像其它 store 那样返回
    /// `&T`）：流式请求要把它移进**响应流**里，而响应流活得比 handler 的
    /// 栈帧久，拿不到借用。
    pub fn request_stats(&self) -> Arc<RequestStats> {
        self.request_stats.clone()
    }
}

/// 读布尔型环境变量开关：**按候选名依次取第一个被设置的**（前一个是新名）。
///
/// 口径与 Node 版一致：只有值为 `"1"` 才算开启，其余（含 `"true"`/`"0"`/空串）
/// 都按关闭处理 —— 这样「设成 0 关掉」与「完全没设」不会分叉。
/// 一个都没设时返回 false。
fn env_flag(names: &[&str]) -> bool {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            return value.trim() == "1";
        }
    }
    false
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
///
/// ── 失败为什么返回 PortConflict 而不是裸字符串 ──
/// bind 失败的原因决定了界面该给什么出路：被别的进程占用可以「结束进程」，
/// 落在系统保留段里则只能「更换端口」。分类判据（`std::io::ErrorKind`）
/// 只有在这里才拿得到，所以在这里分好类往上带，别让界面去猜错误文案。
pub fn start(state: &ServerState) -> Result<oneshot::Sender<()>, PortConflict> {
    let router = http::router(state.clone());
    let addr = SocketAddr::from(([127, 0, 0, 1], state.port));

    let listener = std::net::TcpListener::bind(addr)
        .map_err(|error| PortConflict::from_bind_error(state.port, &error, None))?;
    // 立刻转成非阻塞：从这一刻起到 serve 接手之间，若有连接进来，
    // 阻塞式 accept 会把工作线程卡住。转非阻塞放在同步段更安全。
    listener.set_nonblocking(true).map_err(|error| {
        PortConflict::new(
            ConflictKind::Other,
            state.port,
            None,
            &format!("设置监听为非阻塞失败: {error}"),
        )
    })?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // 停机收尾要用的统计句柄：先克隆出来再移进任务（`state` 是借用，不能
    // 随 async move 一起走）
    let request_stats = state.request_stats();
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
        // 统计补写：放在 serve 返回**之后**（graceful shutdown 已等在途请求
        // 跑完），此时不会再有新的记账进来，这一次 flush 的结果就是终态。
        // 为什么必须显式调用：聚合行是延迟落盘的（变更满 50 次或距上次落盘
        // 超 60 秒才重写整个文件），不补这一次，用户刚看到的请求在下次启动后
        // 会少一截（明细走追加写，不受影响）。
        request_stats.flush();
    });

    Ok(shutdown_tx)
}

/// 启动前的端口自检：能不能真的 bind 上这个端口。
///
/// 与 `start()` 里的 bind 是同一个判据（都走 `std::net::TcpListener::bind`），
/// 但**不改动任何状态**：探测完立刻释放。界面在「更换端口」时用它校验用户
/// 填的端口，好在保存之前就给出「这个端口也被占了」而不是等到重启后才发现。
///
/// 返回 Ok(()) 表示可用；Err 是分类好的冲突描述（文案与启动失败共用一套）。
pub fn probe_port(port: u16) -> Result<(), PortConflict> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpListener::bind(addr) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(error) => Err(PortConflict::from_bind_error(port, &error, None)),
    }
}
