//! 桌面设置在 `kv` 表里的**整份读写**（键名 `desktopSettings`）。
//!
//! ── 为什么整份一个键，而不是 `closeToTray` / `autostart` / `proxyPort` 三行 ──
//! 桌面设置是**整体读写单元**：前端一次 PUT 送整份、`AppSettings` 是一个
//! `Copy` 结构体、`save_app_settings` 是全量覆盖语义。拆成三行会让「保存一次」
//! 变成「三行 UPSERT + 可能删掉第四行」，而这三行之间没有任何独立读写的需求
//! —— 徒增事务面与「改一半」的中间态。约定见 `db::schema` 模块头的键命名规范
//! （那里把 `desktopSettings` 列为「整份对象」这一类的代表）。
//!
//! ── 为什么这边自己开连接（而不是走 `Db`）─────────────────────
//! 这是本模块与其余所有 store 最大的不同，值得说清：
//! **壳侧的启动顺序决定了这里必须能在 `Db::open` 之前读到设置。**
//! `lib.rs` 的 setup 里 `settings::load()` 读 `closeToTray`（决定关窗行为），
//! 而 `gateway::proxy_port()` 还要更早 —— 它决定服务端 bind 哪个端口，因此
//! 早于 `ServerState::bootstrap`（`Db::open` 在那里）。那时没有任何 `Db` 句柄
//! 可用，而端口又必须在 bind 之前就有值。
//!
//! 于是这里用**短命连接**：用到时开一个、读完/写完就关。代价与频率：
//!   - `proxy_port()` **是缓存的**（`gateway::ACTIVE_PORT` 原子量，
//!     `resolve_port` 一个进程只跑一次）→ 解析端口最多一次连接；
//!   - `lib.rs` 的 `settings::load()` 一次；
//!   - 其余全是用户点出来的（`get_app_settings` / `save_app_settings` /
//!     `change_port`），每次点击一两个连接。
//! 三条加起来对 SQLite 是噪音级开销（开连接 = 打开一个文件），换来的是
//! 「端口在库里的同时还早于库可用」这个能力 —— 这是本任务时序难点的解法。
//!
//! ── 建表为什么不在这里 ──────────────────────────────────────
//! 建表与 schema 版本推进归 `server::db::schema::migrate`（`Db::open` 里跑），
//! 本层只读写那一行。**库文件不存在时读会拿到「表不存在」的错误**，这不是
//! 缺陷而是分工：`settings::load` 把「读不出来」统一按「还没有这份设置」处理
//! （回落旧文件 / 默认值），而真正的建表在 `Db::open` 那一次完成。
//!
//! [`write`] 例外地带了一句 `CREATE TABLE IF NOT EXISTS`：它可能在 `Db` 存在
//! **之前**被调用（用户在应用刚起来、`Db::open` 还没跑完时点了「保存设置」），
//! 那时库里还没有这张表。这句 DDL 与 `schema::V1_SCHEMA` 里 `kv` 那段逐字一致，
//! 且**不碰 schema 版本号** —— 真正的版本推进仍归 `Db::open`。
//! （`load` 不需要这句：读失败本来就按「没有这份设置」处理，见上一段。）
//!
//! 另外：**不设 PRAGMA `journal_mode`**。WAL 是库文件的持久属性（写在 header
//! 里），`Db::open` 设过一次之后后续连接自动继承；在这里再设一次等于去改库
//! 的持久状态，那不是读一个设置该做的事。只设 `busy_timeout`（本连接级），
//! 避免与主连接撞上时报 SQLITE_BUSY（主连接持写锁时最多等 5 秒）。
//!
//! ── 本层不加锁、不打日志 ────────────────────────────────────
//! 与其余 `sql.rs` 同一约定：错误一律 `Err` 交回上层，由上层在锁外记。
//! 这里更要注意 —— 壳侧的 `settings` 在日志库初始化之前就可能被调用，
//! 那时 `logging::log` 的入库那一路会静默丢弃（只剩控制台）。

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

/// `kv` 里桌面设置的键名（约定见 `db::schema` 模块头，且它在
/// [`crate::server::db::schema::RESERVED_KV_KEYS`] 里 —— 配置写入不会碰它）。
pub(crate) const KEY: &str = "desktopSettings";

/// 开一个短命连接（用完即关；理由见模块头）。
fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // 本连接级：主连接正在写时等一会儿而不是立刻 SQLITE_BUSY
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    Ok(conn)
}

/// 读整份设置原文（键不存在、表还不存在都返回 `None`）。
///
/// 值按 `Option<Option<String>>` 读：`value` 列声明为 `NOT NULL`，但手工改库
/// 塞一个 NULL 是唯一能造出这种行的途径，而那种行按「没有记录」处理才安全
/// （回落旧文件 / 默认值，而不是把 NULL 当成一份空设置解析成别的什么）。
///
/// **表不存在**（全新安装、库还没建）在这里被归成 `None` 而不是错误上抛：
/// 调用方对「读不出来」的处理与「还没有这份设置」完全相同（回落），
/// 而上抛只会让每个调用点各写一遍同样的判断。
pub(super) fn load(path: &Path) -> Option<String> {
    let conn = open(path).ok()?;
    let value: Option<Option<String>> = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", rusqlite::params![KEY], |row| {
            row.get(0)
        })
        .optional()
        .ok()?;
    value.flatten()
}

/// 写入整份设置原文（UPSERT），**必要时先把表建出来**。
///
/// 用 `ON CONFLICT DO UPDATE` 而不是纯 `INSERT`：这个键可能已经存在（用户改过
/// 设置）也可能不存在（全新用户第一次保存），两条路径都要能跑。
/// **这一条语句本身就是原子的**，不需要再包事务（本模块的「一批」就是这一行）。
///
/// ── 为什么自带 `CREATE TABLE IF NOT EXISTS` ──────────────────
/// 建表归 `schema::migrate`（`Db::open` 里跑），但那需要 `Db` 句柄；而本模块
/// 在 `Db` 存在之前就可能被调用（端口 — 见模块头）。若那时库里还没这张表，
/// 写入会以「no such table」失败。补一句幂等的建表让「保存设置」在任何时刻都
/// 能成功，而**不碰 schema 版本号**：真正的版本推进仍由 `Db::open` 完成
/// （`CREATE TABLE IF NOT EXISTS` 幂等，之后它会补上其余表与索引；
/// 本文件的 DDL 与 `schema::V1_SCHEMA` 里 `kv` 那段逐字一致）。
///
/// ── 为什么失败要交回调用方（与 `load` 相反）──────────────────
/// 保存失败是用户点了「保存」却没存上，必须让他知道 —— 返回 `Result` 而不是
/// 静默吞掉（`load` 的「读不出来」则一律按默认值，因为那不值得打扰用户）。
pub(super) fn write(path: &Path, text: &str) -> Result<(), String> {
    let conn = open(path).map_err(|error| format!("打开数据库失败: {error}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kv (
           key   TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )
    .map_err(|error| format!("建表失败: {error}"))?;
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![KEY, text],
    )
    .map_err(|error| format!("写入设置失败: {error}"))?;
    Ok(())
}

