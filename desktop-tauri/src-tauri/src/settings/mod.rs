//! 桌面端应用设置（关闭到托盘、开机自启、网关端口）的持久化。
//!
//! 放在配置目录而不是 WebView 的 localStorage：这些设置要影响进程自身行为
//! （关窗是否拦截、是否登记自启动、服务端 bind 哪个端口），必须能在窗口还没
//! 建好、甚至界面没跑起来时读到。
//!
//! ── 持久化：统一库的 `kv` 表（本切片从 desktop-settings.json 迁过来）──
//! 改造前是一整份 `{config_dir}/desktop-settings.json`；现在整份设置就是 `kv`
//! 表的 `desktopSettings` 键那一行（**整份一个键**，理由见 `sql.rs` 模块头）。
//! 旧文件由 `db::migrate::import_settings` 一次性搬入。
//!
//! ── 时序：先有鸡还是先有蛋（本模块最需要想清楚的一点）───────────
//! `proxy_port()` 决定服务端 bind 哪个端口，所以它必须在**任何数据库初始化
//! 之前**就有值；而设置现在存在库里，库由 `Db::open` 建 —— 这是本任务的核心
//! 时序难点。三处事实决定了它必须怎么解：
//!   1. `proxy_port()` **是缓存的**（`gateway::ACTIVE_PORT`，一个进程只解析
//!      一次），所以「每次调用都要读库」这个担忧不成立 —— 最多读一次；
//!   2. 端口读错**不是可以降级的错误**：回落到默认 3065 会与用户真正在用的
//!      端口不符（管理 API 客户端指向没人监听的端口），而 3065 若被占用则
//!      **应用直接起不来**（`backend::ensure_ready` 的 bind 失败路径）；
//!   3. 迁移框架跑在 `Db::open` **之内**，而 `load()` 在它**之前** ——
//!      所以「等迁移把设置搬进库再读」是做不到的顺序。
//!
//! 解法是**读侧分流**，一条就覆盖全部情形：
//!   - **读**：先读库。库里没有 → 回落读旧文件；旧文件也没有 → 默认值。
//!     于是「迁移前」与「迁移后」读到的是同一份值。
//!   - **写**：只由框架的迁移项负责（`db::migrate::import_settings`）。
//!     本模块**不**在读到旧文件时顺手写库 —— 理由见下面「为什么没有惰性迁移」。
//!
//! ── 为什么没有惰性迁移（本切片改掉的一处设计）──────────────────
//! 早先这里有一条「从旧文件读到值就顺手写进库」的惰性迁移，指望它给框架项
//! 兜底。**它在真实启动顺序下必然抢先，于是框架项永远轮不到执行**：
//! `lib.rs` 的 setup 第一步（第 196 行附近）就调 `settings::load()` 读
//! `closeToTray`，而 `Db::open` + 迁移框架在 `backend::ensure_ready` 里
//! （第 274 行的 `spawn`），**晚于**它。于是第一次启动的写入者是惰性迁移，
//! 框架项随后看到「`desktopSettings` 键已存在」直接跳过 —— 后果是旧文件
//! 永不改名为 `.migrated`、启动日志里那句「✅ 桌面设置 旧数据已导入数据库」
//! 永不出现，而 `import_settings` 的注释恰恰声称这两件事会发生。
//! 更麻烦的是「备份」这个用户可见的保证落了空：他按提示去找
//! `desktop-settings.json.migrated` 会找不到，只能看到一个没动过的旧文件。
//!
//! 删掉惰性写之后，各条路径的归属变得单一而明确：
//!   - 迁移**之前**（`load` 早于框架）：读旧文件，不写库 —— 结果正确，
//!     因为库里的值此时本该就是空的；
//!   - 迁移**之后**：读库命中，不再看旧文件；
//!   - 写入者只有框架项一个 → 备份与日志这两条保证都成立。
//! 代价是「框架项这次没跑成（比如库那一刻恰好锁住）」时设置只留在文件里，
//! 但那本来就是这个文件的职责（它是回落链的一环），下次启动框架项会重试
//! —— 而惰性写并不能让它更可靠，只会把「框架项有没有跑过」这个事实抹掉。
//!
//! 为什么不做成「端口留文件、其余两个进库」（另一条候选方案）：三分之二的设置
//! 进库、端口留文件，会让「设置保存在哪」这件事对用户与排障者分裂成两种说法，
//! 而这三个字段本来就是一个整体（`change_port` 与 `save_app_settings` 都要读
//! 改写同一份设置）。一致性风险（改一半、两个来源不同步）比一次短命连接的
//! 代价大得多 —— 而那个代价已被缓存（第 1 条）压到「一个进程一次」。
//!
//! ── 读写容错 ────────────────────────────────────────────────
//! 与改造前一致：解析不出（缺失、被手工改坏、权限不足）一律回落到默认值，
//! 绝不因为一个设置让应用起不来。**唯一的例外是「保存」**：那必须报错，
//! 因为用户点了保存却没存上是他需要知道的事（与 `load` 的取向相反）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gateway;

pub(crate) mod sql;

/// 旧设置文件名（`{config_dir}/desktop-settings.json`）。
///
/// 字面量在本文件里写一次而不是引别的常量：这个文件随本次改造已经**不再是
/// 真相来源**（设置进了数据库），而迁移项与下面的回落读路径处理的正是它 ——
/// 名字是历史事实，不会随代码演进变化（与 `db::migrate::debug` 的
/// `DEBUG_FILE_NAME` 同一处理）。
pub(crate) const LEGACY_FILE_NAME: &str = "desktop-settings.json";

/// 设置数据所在的文件（**就是库文件**；界面展示与排障用）。
///
/// 语义从「设置文件路径」变成「装着这份设置的库文件」—— 与 T2~T6 对
/// `AccountStore::file()` / `LogStore::file()` / `RequestStats::file()` 的
/// 处理一致，各 store 的说法统一。
pub fn file_path() -> PathBuf {
    gateway::config_dir().join(crate::server::db::FILE_NAME)
}

/// 旧设置文件的路径（迁移项与回落读路径共用）。
pub(crate) fn legacy_file_path() -> PathBuf {
    gateway::config_dir().join(LEGACY_FILE_NAME)
}

/// 应用设置。
///
/// 前后端之间以 camelCase JSON 传输（`closeToTray` / `autostart` / `proxyPort`），
/// 与 renderer 的字段名保持一致，界面无需做任何映射。
///
/// `default` 用在结构体上（而非逐字段）：这样后续新增字段时，**旧设置记录里
/// 缺这个键不会导致整份设置反序列化失败**——缺的字段各自取 Default。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// 关闭窗口时最小化到托盘而不是退出
    pub close_to_tray: bool,
    /// 开机自动启动
    pub autostart: bool,
    /// 网关监听端口。
    ///
    /// 0 = 未设置，回落到默认 3065（与 `gateway::proxy_port()` 的语义一致）。
    /// 存这里而不是后端配置：端口决定**壳侧**管理客户端的连接目标，且要在
    /// 服务端 bind 之前就读到，属于「应用级启动设置」而非网关业务配置。
    /// 改这个值需要重启进程才生效（服务端 bind 之后端口改不了）。
    pub proxy_port: u16,
}

impl Default for AppSettings {
    fn default() -> Self {
        // 关闭到托盘默认开启：网关的价值在于后台持续转发，
        // 用户点关闭通常只是想收起界面，而不是让转发中断
        Self { close_to_tray: true, autostart: false, proxy_port: 0 }
    }
}

/// 读取设置：库 → 旧文件 → 默认值，逐级回落（时序论证见模块头）。
///
/// 三级各自的作用：
///   - **库**：正常路径（迁移之后，唯一的真相来源）；
///   - **旧文件**：迁移**之前**的唯一来源 —— 老用户升级后第一次启动时端口还
///     只在文件里，读不到它就会回落到 3065（见模块头第 2 条，那可能直接让
///     应用起不来）；
///   - **默认值**：全新安装（两者都没有）。
///
/// 本函数**只读不写**：从旧文件读到值时不会顺手把它搬进库。搬运动作由框架的
/// 迁移项独占（`db::migrate::import_settings`），完整论证见模块头
/// 「为什么没有惰性迁移」—— 简言之：这里的读发生在 `Db::open` 之前，若顺手
/// 写库就会抢在框架项前面，让「旧文件改名为 `.migrated`」与「启动日志里那句
/// 已导入」两条保证同时落空。
pub fn load() -> AppSettings {
    if let Some(text) = sql::load(&file_path()) {
        // 库里那份读不懂（手工改库写坏）：不在这里回落旧文件 —— 迁移之后
        // 旧文件已改名，回落只会读到「没有」；统一走默认值，与改造前
        // 「文件写坏 → 默认值」的行为一致。
        return serde_json::from_str(&text).unwrap_or_default();
    }
    let Some(text) = std::fs::read_to_string(legacy_file_path()).ok() else {
        return AppSettings::default();
    };
    // 解析失败回落默认值（旧文件被手工改坏，不打扰用户）
    serde_json::from_str::<AppSettings>(&text).unwrap_or_default()
}

/// 覆盖写入设置（写进库），返回是否成功。
///
/// 与 [`load`] 的容错取向相反：**失败必须报出去** —— 用户点了「保存」却没存上
/// 是他需要知道的事。写入前不做目录准备：库文件的父目录由 `Db::open` 负责建，
/// 而配置目录在启动极早期已由 `config_migration` 建好；`sql::write` 自身也
/// 会带上幂等的建表（理由见那里 —— 本模块可能在 `Db::open` 之前被调用）。
///
/// 写入的是**库**；旧 `desktop-settings.json` 在迁移后已改名成 `.migrated`，
/// 本函数不再碰它 —— 若它还在（用户把备份改回原名），那是迁移项的事，
/// 不是每次保存都去同步两份（两份会各说各话，正是要避免的那种状态）。
pub fn save(settings: &AppSettings) -> Result<(), String> {
    let text = serde_json::to_string(settings)
        .map_err(|error| format!("设置序列化失败: {error}"))?;
    sql::write(&file_path(), &text)
}

/// 迁移项用：读旧文件的原文（**不解析**，保持原样交给迁移）。
///
/// 与 [`load`] 的区别：`load` 要的是「能用的设置」（解析失败就回落），
/// 迁移要的是「文件里到底写了什么」—— 解析与归一归运行期，迁移只负责搬运。
/// 整份文本搬过去还有个额外好处：用户手写的未知字段一并保留
/// （与配置项「全量保留未知字段」的不变量一致）。
pub(crate) fn read_legacy_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 迁移项用：把一份旧设置文本写进库（**用迁移框架给的连接**）。
///
/// 为什么不让迁移项直接调 [`save`]：那会自己开一条新连接（`sql::write`），
/// 于是迁移的这次写入**跑在框架那批之外** —— 它自己的原子性没问题（单行
/// UPSERT），但「写不进去时旧文件要不要保留」这个判断就无法与写入处在同一个
/// 事务里。用框架给的连接则与其余七项一致（都在 `Db::open` 的锁内）。
///
/// 返回的 `rusqlite::Result` 由迁移项转成一行 ❌ 日志并返回 `None`
/// （不备份旧文件 —— 那份文件是运行期回落链的最后保障）。
pub(crate) fn write_migrated(conn: &rusqlite::Connection, text: &str) -> rusqlite::Result<()> {
    sql::write_conn(conn, text)
}

/// 迁移项用：这份旧设置文本能不能解析成设置。
///
/// 为什么要先判一次：把一份读不懂的内容写进库，会让**运行期的回落链断掉**
/// —— 库里有了值（虽然读不懂），`load` 就不会再回落（`sql::load` 命中即返回，
/// 解析失败取默认），于是用户看到的是「设置全变回默认」，而旧文件又被改名成了
/// 备份。判一次则宁可让文件留在原处、库里不写（迁移项跳过 + 保留旧文件，
/// 与其它项遇到坏 JSON 时同一处理）。
pub(crate) fn legacy_parses(text: &str) -> bool {
    serde_json::from_str::<AppSettings>(text).is_ok()
}
