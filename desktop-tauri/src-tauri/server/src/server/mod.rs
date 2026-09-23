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
//!   account_bootstrap.rs 账号侧的启动期一次性迁移（auth.json 旧登录态 /
//!                   优先级整队 / 三家旧项目数据导入）。**两处调用**：
//!                   启动时（仅当没有待迁移旧文件）与用户点完「升级」之后 ——
//!                   为什么不能无条件跑，见那个文件的模块头（有一段不可逆的
//!                   破坏路径：抢在数据迁移之前占住 `accounts` 表）
//!   http.rs         axum Router 组装、CORS、API Key 检查、404 兜底
//!   logging.rs      双通道日志（控制台 + 数据库的 logs 表）
//!   config.rs       config.json 读写（全量保留未知字段）
//!   logs_store/     事件日志存储（append/查询/统计/清空）：
//!     mod.rs        句柄与公开 API（幂等写入、降级、导出）
//!     sql.rs        行级 SQL 访问层 + 筛选条件编译
//!   errors.rs       网关错误类型 + OpenAI 风格错误 payload
//!   db/             本地存储的**唯一真相**（单文件 SQLite `{config_dir}/agent2api.db`）：
//!     mod.rs        连接与句柄（`Db::open` / `with` / `with_mut` / `file`）
//!     schema.rs     全部建表 DDL 与 `PRAGMA user_version` 逐版本升级
//!     migrate/      旧文件 → 库的一次性迁移（框架 + 每个迁移项一个文件）
//!                   八项已全部接入（配置 / 桌面设置 / 账号 / 日志 /
//!                   请求明细 / 请求聚合 / 调试报文 / 脱敏词表）；
//!                   **启动时只探测、不执行** —— 由用户在升级弹窗里点「升级」
//!                   触发 `POST /api/upgrade/run`（见 api/upgrade_api.rs）。
//!                   迁移成功后旧文件改名为 `*.migrated` 备份，**绝不删除**
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
//!     sanitize.rs   /api/sanitize（出站指纹脱敏开关）
//!     auto_checkin.rs /api/auto-checkin*（定时签到设置 / 手动执行）
//!     scheduled_tasks.rs /api/scheduled-tasks*（间隔型定时任务：开关 / 间隔 / 立即执行）
//!     update.rs     /api/update/*（软件更新检查 / 下载 / 进度 / 取消）
//!     endpoints.rs  GET /api/endpoints（接口清单）
//!     storage_api.rs GET /api/storage（统一库的位置、大小与各表条数，只读）
//!     upgrade_api.rs GET /api/upgrade、POST /api/upgrade/run（旧数据 → SQLite 库）
//!   core/
//!     endpoints.rs 端点/版本/UA/上下文（唯一事实来源）
//!     account_store/  账号存储（优先级、迁移、CRUD、限额标记）。
//!                 持久化已从 `{config_dir}/accounts.json` 换到
//!                 `agent2api.db` 的 `accounts` 表；`sql.rs` 是行级访问层，
//!                 `store_view.rs` 是公开形态与快照
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
//!     sanitize.rs  出站请求体指纹脱敏（硬编码规则集，纯函数）
//!     prompt.rs    网关自有系统提示词（透传 / 替换 / 追加三模式，纯函数）
//!     degrade.rs   内容拦截降级状态机（撞审核误报后到次日 00:00 用中性提示词）
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
//!   - 指纹脱敏：无句柄，转发层每次出站前读 `config::current().sanitize_fingerprints()`
//!     决定要不要调 `core::sanitize::sanitize_body`（纯函数，无状态）。
//!   - 系统提示词：同样无句柄，转发层逐请求取一次 `config::current().prompt_plan()`
//!     交给 `core::prompt` 落到出站副本上；撞内容拦截时由转发层触发
//!     `core::degrade`（进程级原子状态，无句柄）并就地换中性提示词重试一次。
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
//!     收尾处 `flush()`（把 WAL 并回主库，见那边的说明）。
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
mod account_bootstrap;
pub mod access;
pub mod altcha;
pub mod config;
pub mod config_migration;
pub mod core;
pub mod db;
pub mod errors;
pub mod http;
pub mod logging;
pub mod logs_store;
pub mod request_stats;
mod static_files;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::port_conflict::{ConflictKind, PortConflict};
use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::AuthService;
use crate::server::core::auto_checkin::AutoCheckin;
use crate::server::core::billing::BillingService;
use crate::server::core::login::LoginService;
use crate::server::core::models::ModelCatalog;
use crate::server::core::update::UpdateManager;
use crate::server::core::upstream::UpstreamService;
use crate::server::db::Db;
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
    /// 管理面板的独立监听端口（headless 设 `AGENT2API_PANEL_PORT` 开启）。
    /// `None` = 面板与网关同端口（默认形态与桌面壳）；`Some(p)` = 主端口只挂
    /// `/v1/*` 网关，静态界面与 `/api/*` 挂到 `p` —— 面板端口可以不暴露公网，
    /// 管理面整体留在内网。桌面壳不设置它。
    pub panel_port: Option<u16>,
    /// 监听地址。桌面壳固定 127.0.0.1（单机安全边界）；headless 二进制按
    /// `AGENT2API_HOST` 解析（默认 0.0.0.0，供容器端口映射）。
    pub host: IpAddr,
    /// 配置目录（`~/.agent2api`），与壳侧 gateway::config_dir() 同源
    pub config_dir: PathBuf,
    /// headless 下托管管理界面（`ui/` 静态目录）的根；`None` = 不托管
    /// （桌面形态由 Tauri 壳的 custom-protocol 出界面，网关只出 API）。
    /// 桌面壳不设置它；headless 二进制经 [`ServerState::set_ui_dir`] 打开。
    ui_dir: Option<PathBuf>,
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
    /// 定时签到句柄（轮询调度 + 启动补签；内部 Mutex + 后台任务）
    auto_checkin: AutoCheckin,
    /// 软件更新句柄（GitHub Release 检测 / 安装包下载；内部 Mutex）
    update: UpdateManager,
    /// 请求统计存储句柄（明细 + 按天聚合，现在同居统一库的两张表）。
    ///
    /// 外面包一层 `Arc` 而不是像其它 store 那样自带内部 `Arc<Inner>`：
    /// 存储本体的公开接口（`RequestStats::with_db`）已经定型，
    /// 而 `ServerState` 是 `Clone` 的（handler 靠克隆拿状态），
    /// 所以共享语义由这层 `Arc` 提供 —— 与 `AccountStore` 的 `Arc<Inner>`
    /// 是同一个效果，只是包的位置在外侧。
    /// 需要 `Arc`（而不是像 `LogStore` 那样靠 `OnceLock` 全局取）还有一个
    /// 具体原因：流式请求要把这个句柄**移进响应流**（记账发生在流跑完时），
    /// 所以 handler 必须能拿到一份所有权克隆（见 `request_stats()`）。
    request_stats: Arc<RequestStats>,
    /// SQLite 数据库句柄。`None` = 打开失败（启动时已记日志）。
    /// 为什么允许为 None 而不是直接让启动失败：release 构建是 panic=abort，
    /// 数据库问题不该让整个应用闪退；网关降级启动至少能给出可读错误。
    db: Option<Db>,
    /// 启动时发现「还有旧 JSON/JSONL 文件没搬进库」。
    ///
    /// ── 为什么是 `Arc<AtomicBool>` 而不是裸 `bool` ──────────────
    /// `ServerState` 是 `Clone` 的（handler 靠克隆拿状态），裸字段改了不会让
    /// 别的 handler 看见。而它**必须**能被改：`POST /api/upgrade/run` 跑完迁移
    /// 之后要把它落回 false，否则界面每次刷新都还会读到「有待迁移数据」，
    /// 用户会以为升级没生效。用一个原子布尔而不是 `Mutex<bool>`：这里只有
    /// 「读一个值 / 写一个值」两种操作，没有需要一起保护的第二个字段。
    ///
    /// ── 为什么在启动时算一次并记住，而不是每次请求现算 ──────────
    /// 它是**界面开弹窗的判据**（`GET /api/upgrade`），而每次请求都去挨个
    /// `is_file()` 一遍没有意义：迁移只可能由 `POST /api/upgrade/run` 改变，
    /// 那条路由跑完会自己刷新这个字段。代价是**用户手工把旧文件拷回配置目录
    /// 不会被感知**（要下次启动才发现）—— 这是可接受的：手工放回文件的人
    /// 本来就知道自己在做什么。
    upgrade_pending: Arc<AtomicBool>,
}

impl ServerState {
    /// 构造服务状态，并完成启动期的准备工作。
    ///
    /// 顺序很重要（对照 server.mjs 336-353 行）：
    ///   1. 开数据库 —— 配置、日志库与所有 store 的最终落点；
    ///   2. 读配置 —— 保留期（日志天数）要在装日志库之前就位，而配置现在
    ///      就存在库里，所以它必须排在①之后；
    ///   3. 探测旧文件（**不迁移**）—— 八项各自判自己的旧文件在不在，
    ///      结果存进 `upgrade_pending`，等用户在界面点「升级」才真正导入
    ///      （`POST /api/upgrade/run`，见 `api::upgrade_api` 的模块头）；
    ///   4. 装日志库 —— 后面所有模块的日志才能入库；
    ///   5. 账号库载入 + 旧版 auth.json 迁移（仅账号列表为空时）+ 优先级去重迁移。
    /// ①② 的顺序是本任务刚从「先读配置、再开库」调过来的：配置的真相来源进了
    /// 库，读它需要 `Db` 句柄。调过来带来的新问题（迁移还没跑时配置从哪来）由
    /// `config::read_raw` 的回落读旧文件解决 —— 那段注释是本顺序的关键，
    /// 改动①②之前**必须**先读它。
    ///
    /// ②/④ 的顺序是上一个切片刚从「先装日志库」调过来的：日志库载入历史时就会按
    /// 保留天数裁一次，而保留天数来自配置 —— 若配置还没读进来，那次裁剪会退回
    /// 默认 30 天，把用户设了更长保留期的旧日志当场裁掉并落盘（**不可逆的数据丢失**）。
    /// ①→② 的调整**没有**破坏这条：配置依然在 `logging::init_store` 之前就位
    /// （只是它的来源从文件换成了库 + 文件回落）。
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
    /// （`Db::open` → 建库目录；`config::init` → 建配置目录；`logging::init_store`
    /// → 建目录）就会把新目录建出来，让迁移永远失去重试机会。失败以 `Err`
    /// 往上传（`ensure_ready` 原样透给 UI 的 `backend:error`），不 panic、
    /// 也不静默降级。
    pub fn bootstrap(port: u16, host: IpAddr) -> Result<Self, String> {
        if let Some(reason) = config_migration::pending_reason() {
            return Err(reason);
        }
        let config_dir = config::config_dir();
        // 与 Node 版一致：verbose 由环境变量 AGENT2API_VERBOSE=1 打开
        // （旧名 WORKBUDDY_VERBOSE 仍可读，新名优先），
        // 决定 debug 级别日志要不要入库（默认只有 info 以上入库，避免刷屏）
        let verbose = env_flag(&["AGENT2API_VERBOSE", "WORKBUDDY_VERBOSE"]);
        // ── 数据库 ──────────────────────────────────────────────
        // 位置是刻意的：在 `config::init()` **之前**、`logging::init_store(...)`
        // **之前**。
        //   - 为什么必须在日志库之前：日志库写进数据库，数据库必须先就绪；
        //     顺序反了，日志库就没有可用的库可写 —— 而那时它已经按「库可用」
        //     的前提装好了，只能整轮重来。
        //   - 为什么必须在 `config::init()` 之前（**本切片刚调过来**）：
        //     配置的真相来源从 `config.json` 变成了统一库的 `kv` 表，读它需要
        //     `Db` 句柄。调过来之后下面那次 `config::init(db.clone())` 才能
        //     从库里读；而「迁移还没跑时读不到配置」这个新问题由
        //     `config::read_raw` 的**回落读旧文件**解决（完整论证与它为什么
        //     安全见那里的注释，以及 `config::init` 的签名说明）。
        //     这一步不能反：反了就是「先读配置、再开库」，配置只好去读旧文件
        //     —— 那正是本切片要消灭的形态，且迁移之后旧文件已被改名，
        //     第二次启动起配置就全空了。
        // 打开失败**不阻断启动**（`db = None`），理由见 ServerState.db 的字段注释。
        let db_path = config_dir.join(db::FILE_NAME);
        let db = match db::Db::open(&db_path) {
            Ok(db) => Some(db),
            Err(error) => {
                logging::console_line("[Storage]", &format!("❌ 数据库打开失败: {error}"));
                None
            }
        };
        // 配置快照：**在库就绪之后、迁移之前**装入（理由见上面那段注释）。
        // 搬回来之后它的降级链是「库 → 旧文件 → 默认值」（`config::read_raw`），
        // 于是库里还没配置时（迁移尚未跑、或全新安装）读到的是与改造前一致的
        // 结果 —— 老用户升级后第一次启动拿到的就是他真正的旧配置，
        // 下面日志裁剪天数与旧文件候选目录才不会用错值。
        let snapshot = config::init(db.clone());
        // ── 旧文件一次性迁移：**本切片起不再自动跑** ────────────────
        // 它现在由用户在升级弹窗里点「升级」触发（`POST /api/upgrade/run`）。
        // 为什么改成手动：需求是「弹窗告诉用户换了 SQLite，点升级才开始导」——
        // 自动跑会让弹窗永远来不及出现（第一次启动就搬完了，用户不会知道发生过
        // 什么），而用户对「我的数据去哪了」的知情权是这个改动的主要目的。
        //
        // ── 不跑迁移会不会让本次运行读到错的配置 ────────────────────
        // 不会，两条回落路径都已核实：
        //   - `config::init` 走 `config::read_raw`：迁移标记键不在时它会
        //     **回落读旧 `config.json`**（`read_raw` 的时序论证），所以快照里
        //     已经是用户真正的旧配置，`storage_dirs()` / `retention_settings()`
        //     拿到的值都对；
        //   - 壳侧 `settings::load()`（端口 / 关闭到托盘）同样是「库 → 旧文件 →
        //     默认值」三级回落（见 `settings` 模块头）。
        // 换句话说：迁移前与迁移后，读到的配置是同一份 —— 这正是当初把回落读
        // 设计出来的目的，也是「手动触发」能成立的前提。
        //
        // 日志此时还进不了日志库（它尚未初始化），走控制台。
        // 注意这里**不写**「待迁移」日志的详情：弹窗会列清单，控制台只留一句
        // 可排障的线索（哪几个文件还在），避免与弹窗的文案各说一套。
        let upgrade_pending = match db.as_ref() {
            Some(_) => {
                let pending = db::migrate::pending_items(&config_dir);
                if !pending.is_empty() {
                    logging::console_line(
                        "[Storage]",
                        &format!(
                            "⏳ 检测到待迁移的旧数据（{}），等待用户在界面点「升级」后导入",
                            pending.join("、")
                        ),
                    );
                }
                !pending.is_empty()
            }
            // 库不可用：没有可导入的目标，不该提示用户去点升级（点了必然失败）。
            // 那条 ❌ 已经在 `Db::open` 那一步打过，这里不重复。
            None => false,
        };
        // 日志库：把**同一个 `Db`** 传进去 —— 与账号存储同一形态（见下面
        // AccountStore 的说明）。`db` 是 `Option<Db>`：打不开时日志库照常装起来，
        // 但写入静默丢弃、读取返回空（日志丢一条不影响任何业务，降级值见
        // `LogStore` 各方法的说明）。
        logging::init_store(db.clone(), verbose);
        // 调试模式的原始报文存储：无论开关是否打开都初始化 —— 开关是**逐请求**
        // 判定的（改完设置下一个请求就生效），存储没就绪会让开启后的第一批
        // 请求无处可落。
        // 本切片起报文进统一库的 `debug_traffic` 表（旧 `debug-traffic.jsonl`
        // 由 `migrate::import_debug` 一次性搬入），所以传的是**同一个 `Db`** ——
        // `Option<Db>`：打不开时报文写入静默丢弃（丢一份调试报文不影响任何业务）。
        core::debug_traffic::init(db.clone());
        // 注意这里**不再有 `config::storage_dirs()`**：三类数据的保存目录
        // （`logDir` / `requestStatsDir` / `debugDir`）已经全部失去消费方 ——
        // 数据都在统一库的配置目录里，各 store 的构造只接 `Db`。
        // 那三个键与 `storage_dirs()` 仍然保留，但**只服务迁移项**：它们要去
        // 用户当年自定义过的目录里找旧文件（见 `db::migrate` 的
        // `logs` / `requests` / `debug` 三项）。设置页也不再展示它们（T8 收尾
        // 把那一节改成了单库只读概况），所以本文件里出现它们反而是不该发生的
        // —— 那意味着有人把「旧文件在哪」的知识复制到了第二个地方。

        // 账号库 + 鉴权 + 登录 + 计费（四者共享同一个 store 句柄）。
        // 账号数据现在落在上面那个库里（`accounts` 表），所以把**同一个 `Db`**
        // 传进去 —— 不再是「记住配置目录，自己要读写时现拼路径」。注意 `db` 是
        // `Option<Db>`：打开失败时 store 仍然装得起来，但每个账号操作都会以
        // 500「账号数据库不可用」拒绝（选择与理由见 `store.rs` 的 `Inner::db`）。
        // 这里**不做**「库不可用就整机不启动」的判断：转发的账号是核心，但网关
        // 的其余部分（日志、模型目录、健康检查、配置页）不依赖账号，让它们继续
        // 可用比整体拒绝启动更有用 —— 用户能在界面上看到那条 ❌ 日志并处理磁盘问题。
        let store = AccountStore::with_db(db.clone());
        let context = core::endpoints::default_context();
        let auth = AuthService::new(store.clone(), context);
        let login = LoginService::new(auth.clone(), store.clone());
        let billing = BillingService::new(auth.clone());
        // 模型目录：**进程级单例**（Agent2API 改造 W2a-T2）。聚合模型目录
        // （core::providers::catalog）只收 &AccountStore，不该让调用方层层传目录，
        // 因此目录自身也做成进程级句柄（与 config / auto_checkin
        // 同一模式）：`core::models::global_catalog()` 与这里的 `models` 是
        // **同一实例**（共享同一把 RwLock），刷新对两边同时可见。
        let models = core::models::global_catalog();
        let upstream = UpstreamService::new(store.clone(), auth.clone());

        // ── 定时签到与软件更新（切片 6）──────────────────────────
        // 两者都是进程级句柄（同 config / logging 的模式）：
        // 定时签到的 stop() 要从**停机路径**（backend::shutdown，只有 Tauri 的
        // AppState）调到，所以必须能从全局拿到；这里装入后 ServerState 里那份
        // 与全局那份是同一实例。
        let auto_checkin = core::auto_checkin::init_global(AutoCheckin::new(
            store.clone(),
            billing.clone(),
        ));
        // 更新管理器：下载目录 `{config_dir}/updates`，与壳侧 update::download_dir() 同源
        let update = core::update::init_global(UpdateManager::new(config_dir.clone()));

        // 请求统计：本切片起数据进统一库的 `requests` / `request_daily` 两张表
        // （旧 `requests.jsonl` / `request-daily.jsonl` 由 `migrate::import_requests`
        // / `migrate::import_daily` 一次性搬入），所以不再有「统计自己的目录」——
        // 路径由 `Db` 唯一持有，与 `LogStore` 同一形态。
        // `requestStatsDir` 这个键仍被迁移项读（去自定义目录里找那两个旧文件，
        // 见 `db::migrate::requests`），本文件不再碰它。
        //
        // 保留期走**回调**，每次裁剪时动态读配置：明细天数来自
        // `requestRetentionDays`、聚合天数来自 `dailyRetentionDays`
        // （`config::retention_settings()` 读的是 config::init() 装好的内存快照，
        // 而 `PUT /api/retention` 会同步刷新它）——
        // 于是设置页改完天数，下一次记账 / prune 立即生效，**不需要重启进程**。
        let request_stats = Arc::new(RequestStats::with_db(db.clone(), || {
            let settings = config::retention_settings();
            Retention {
                request_days: settings.request_days,
                daily_days: settings.daily_days,
            }
        }));
        // 启动即收尾上次运行遗留的进行中行（详情见方法注释）：不等到第一条
        // 请求进来才清理，界面打开时看到的就已经是「已中断」的终态
        request_stats.sweep_stale_running();

        // ── 账号侧的启动期一次性迁移：**有待迁移数据时整块跳过** ──────
        // 那一组迁移（auth.json 旧登录态、`migrate_startup` 的整队与拆池、
        // 三家旧项目数据导入）的判据全是「库里那份账号列表空不空」，而
        // **旧数据还没导入时 `accounts` 表是空的** —— 跑它们会有一条不可逆的
        // 破坏路径（抢先占住表，把 `import_accounts` 的幂等闸门永久关上）。
        // 完整论证、以及「为什么升级后必须补调一次」都在
        // `account_bootstrap` 的模块头里 —— 那一段是本次改动最容易读错的地方，
        // 值得一个能被直接找到的位置，所以整组搬到了那个文件。
        if !upgrade_pending {
            account_bootstrap::run(&store);
            // 统计侧的一次性口径订正：模型维度从「请求名（映射别名）」重算成
            // 「上游真名」（见 `RequestStats::remap_model_dimension_once`）。
            // 与 account_bootstrap 同一模式：待迁移旧数据没导入时跳过 ——
            // 跑了会把幂等标记打早，升级完成后由 `api::upgrade_api` 补调。
            request_stats.remap_model_dimension_once();
        }

        let state = Self {
            port,
            panel_port: None,
            host,
            config_dir,
            ui_dir: None,
            store,
            auth,
            login,
            billing,
            models,
            upstream,
            auto_checkin,
            update,
            request_stats,
            db,
            upgrade_pending: Arc::new(AtomicBool::new(upgrade_pending)),
        };
        logging::log("[Server]", "Agent2API 多提供商本地网关（Rust 进程内服务）启动中…");
        logging::log("[Config]", &format!("API 端口: {}", port));
        logging::log("[Config]", &format!("API 监听地址: {}", host));
        logging::log(
            "[Config]",
            &format!(
                "API Key 认证: {}",
                if snapshot.api_key_set() { "✅ 已启用" } else { "❌ 未启用" }
            ),
        );
        logging::log("[Config]", &format!("默认模型: {}", snapshot.default_model()));
        logging::log("[Config]", &format!("计费语言: {}", snapshot.locale()));
        // 出站指纹脱敏一行：开着时说明「出站会剥离审核指纹」，关着时点明后果
        // （客户端 system 模板会原样发上游，可能被 400 code=11128 误拦）
        logging::log(
            "[Config]",
            &format!(
                "出站指纹脱敏: {}",
                if snapshot.sanitize_fingerprints() {
                    "✅ 已启用（剥离上游审核黑名单指纹）"
                } else {
                    "❌ 已关闭（客户端 system 模板原样发送，可能被上游误拦）"
                },
            ),
        );
        // 系统提示词一行：模式 + 生效文本来源（内置默认 / 文件）+ 文件读不到时的
        // 原因。降级期是运行期状态（不是启动事实），只在设置页与日志里显示，
        // 这里不读它 —— 启动那一刻还没人触发过降级。
        logging::log(
            "[Config]",
            &format!(
                "系统提示词: {}（{}）{}",
                snapshot.prompt_settings().mode.label(),
                snapshot.prompt_settings().source.label(),
                match snapshot.prompt_settings().file_error.as_deref() {
                    Some(reason) => format!("；⚠️  {reason}"),
                    None => String::new(),
                },
            ),
        );
        logging::log("[Config]", &format!("配置目录: {}", state.config_dir.display()));        // 数据库状态一行（排障第一手信息：库在哪、有没有就绪）。
        // 放在「配置目录」之后：坏库时的第一句话就是「库在哪、能不能打开」，
        // 而 `Db::file()` 是唯一知道自己路径的对象（不让别处再拼一次
        // `config_dir.join(FILE_NAME)` —— 那是把路径知识复制到第二个地方）。
        // 打开失败的情形在 `Db::open` 那一步已经打过 ❌ 日志，这里不重复报错，
        // 只把「本次运行数据库不可用」这个后果说清楚（各 store 会降级回落）。
        match state.db() {
            Some(db) => {
                logging::log("[Storage]", &format!("数据库: {}", db.file().display()));
            }
            None => {
                logging::log(
                    "[Storage]",
                    "⚠️  数据库不可用：依赖数据库的功能本次运行将降级（详见上方报错）",
                );
            }
        }
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
        // 服务器停机时进程会结束，任务随之消失。用 crate::spawn_task
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

    /// SQLite 数据库句柄（本地存储的唯一真相）。
    ///
    /// 返回 `Option<&Db>`：`None` = 打开失败（启动时已记一行可读日志）。
    /// 后续各 store 的接线点必须显式处理这一分支 —— 数据库不可用时应当
    /// 降级回落（例如日志明细先不入库），而不是 panic。理由与字段注释同：
    /// release 是 panic=abort，数据库问题不该让整个应用闪退。
    pub fn db(&self) -> Option<&Db> {
        self.db.as_ref()
    }

    /// 还有没有待迁移的旧文件（升级弹窗的判据；见字段说明）。
    pub fn upgrade_pending(&self) -> bool {
        self.upgrade_pending.load(Ordering::Relaxed)
    }

    /// 重算「待迁移」并写回（`POST /api/upgrade/run` 跑完之后调）。
    ///
    /// 为什么重算而不是直接写 false：迁移**可能只成功了一部分**（某一项解析
    /// 失败、写库失败，旧文件留在原处），那时还有文件在、也确实还能再点一次
    /// 升级（迁移项各自幂等）。重算让界面如实反映这一点，用户不必猜
    /// 「刚才那次到底全成了没有」。
    ///
    /// 库不可用时按「没有待迁移」处理：没有可导入的目标，提示用户去点升级
    /// 只会得到一次必然失败的尝试（与 `bootstrap` 里的判据一致）。
    pub fn refresh_upgrade_pending(&self) -> bool {
        let pending = self.db.is_some() && db::migrate::has_pending(&self.config_dir);
        self.upgrade_pending.store(pending, Ordering::Relaxed);
        pending
    }

    /// 待迁移项的可读名清单（升级弹窗列清单用）。
    pub fn upgrade_items(&self) -> Vec<&'static str> {
        if self.db.is_none() {
            return Vec::new();
        }
        db::migrate::pending_items(&self.config_dir)
    }

    /// 打开 headless 形态的管理界面托管：`ui_dir` 指向 `ui/` 静态目录，
    /// 网关随 API 一并出面板（桌面形态不调用 —— 界面由 Tauri 壳出）。
    pub fn set_ui_dir(&mut self, dir: PathBuf) {
        self.ui_dir = Some(dir);
    }

    /// headless 是否托管管理界面（`http::router` 据此挂静态 fallback）。
    pub fn ui_dir(&self) -> Option<&PathBuf> {
        self.ui_dir.as_ref()
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
///   2. 需要运行时的 from_std 与 serve 都放进 `crate::spawn_task`，
///      任务体一定跑在运行时的工作线程上，reactor 必然可用。
///
/// ── 失败为什么返回 PortConflict 而不是裸字符串 ──
/// bind 失败的原因决定了界面该给什么出路：被别的进程占用可以「结束进程」，
/// 落在系统保留段里则只能「更换端口」。分类判据（`std::io::ErrorKind`）
/// 只有在这里才拿得到，所以在这里分好类往上带，别让界面去猜错误文案。
pub fn start(state: &ServerState) -> Result<oneshot::Sender<()>, PortConflict> {
    // ── 路由按形态拆分 ─────────────────────────────────────────
    // 默认（panel_port = None，桌面壳 / 未设 PANEL_PORT 的 headless）：单端口
    // 挂完整路由（面板 + 网关合并），行为与拆分前逐字一致。分端口形态
    // （headless 设 AGENT2API_PANEL_PORT）：主端口只挂网关（/v1/* + /health），
    // 面板（静态界面 + /api/*）单独监听 panel_port —— 把面板端口留在内网，
    // 公网只暴露网关端口。（panel_port == port 视为没配，兜底走合并。）
    let panel_port = match state.panel_port {
        Some(port) if port != state.port => Some(port),
        _ => None,
    };
    let main_router = match panel_port {
        Some(_) => http::gateway_router(state.clone()),
        None => http::router(state.clone()),
    };
    let panel_router = panel_port.map(|_| http::panel_router(state.clone()));

    let addr = SocketAddr::new(state.host, state.port);
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
    // 面板监听（分端口形态）：也在同步段 bind —— 两个端口都拿到手才起
    // serve，任一失败直接返回冲突（已 bind 的主监听随 drop 自动释放，
    // 不会留下半启动状态）。
    let panel_listener = match panel_port {
        Some(port) => {
            let addr = SocketAddr::new(state.host, port);
            let listener = std::net::TcpListener::bind(addr)
                .map_err(|error| PortConflict::from_bind_error(port, &error, None))?;
            listener.set_nonblocking(true).map_err(|error| {
                PortConflict::new(
                    ConflictKind::Other,
                    port,
                    None,
                    &format!("设置监听为非阻塞失败: {error}"),
                )
            })?;
            Some(listener)
        }
        None => None,
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // 停机广播：oneshot 只有一个 receiver，而分端口形态有两个 serve 都要
    // 收到停机信号。协调任务把 oneshot 信号（send 或发送端被 drop）转成
    // watch 置位，两个 serve 各拿一份 receiver；watch 发送端 drop 也会让
    // `wait_for` 返回，停机语义与单 oneshot 一致。
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    crate::spawn_task(async move {
        let _ = shutdown_rx.await;
        let _ = stop_tx.send(true);
    });

    // 停机收尾要用的统计句柄：先克隆出来再移进任务（`state` 是借用，不能
    // 随 async move 一起走）
    let request_stats = state.request_stats();
    let stop_main = stop_rx.clone();
    crate::spawn_task(async move {
        // 运行时上下文在这里必然成立，from_std 不会 panic
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                logging::log("[Server]", &format!("❌ 创建异步监听失败: {error}"));
                return;
            }
        };
        // into_make_service_with_connect_info：把客户端地址带进 handler
        // （面板登录的失败锁定按来源 IP 计；无 ConnectInfo 的提取会 panic，
        //  所以 serve 的形态必须与之一致）
        let mut stop = stop_main;
        let server = axum::serve(
            listener,
            main_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            // 收到停机广播（或广播端被丢弃）即结束 accept 循环，
            // 已在处理中的请求会跑完再退出
            let _ = stop.wait_for(|stop| *stop).await;
        });
        match server.await {
            Ok(()) => logging::log("[Server]", "服务已停止"),
            Err(error) => logging::log("[Server]", &format!("❌ 服务异常退出: {error}")),
        }
        // 统计收尾：放在 serve 返回**之后**（graceful shutdown 已等在途请求
        // 跑完），此时不会再有新的记账进来。
        // 注意它**不再是**「补写未落盘的内容」—— 每次记账都立即提交事务，
        // 没有延迟落盘的状态了（旧实现的一套延迟机制随 T4 消失）；
        // 它现在的职责是把 WAL 并回主库、清掉 `-wal` / `-shm` 残留，
        // 让用户备份时只面对一个文件（见 `RequestStats::flush` 的说明）。
        request_stats.flush();
    });

    // 面板 serve（分端口形态才有）：独立监听、同一份停机广播。
    // 统计收尾只归主 serve（上面那个任务），这里只管转发请求。
    if let Some(listener) = panel_listener {
        let router = panel_router.expect("panel_port 与 panel_router 必须成对");
        crate::spawn_task(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(error) => {
                    logging::log("[Server]", &format!("❌ 面板监听创建失败: {error}"));
                    return;
                }
            };
            let server = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let mut stop = stop_rx;
                let _ = stop.wait_for(|stop| *stop).await;
            });
            if let Err(error) = server.await {
                logging::log("[Server]", &format!("❌ 面板服务异常退出: {error}"));
            }
        });
    }

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
