//! 远程模型清单的持久化缓存：进程重启后不再退回静态兜底清单。
//!
//! ── 解决什么问题 ────────────────────────────────────────────
//! 各家的远程清单此前只活在内存里（每家一个 `OnceLock<RwLock<…>>`）：进程一
//! 重启就清零，读取逻辑回落到代码里写死的静态兜底清单。启动那次刷新若失败
//! （网络 / 无可用账号 / 上游报错），目录就停在兜底清单上 —— 上一次拉到的
//! 新清单整个丢失：上游新增的模型在管理页与 `/v1/models` 里一起消失，
//! 上游已下架的模型又回来被广告出去（客户端选中它必然失败）。
//!
//! 本模块把「远程刷新成功落地的那份清单」写进统一库的 `kv` 表，各家的目录
//! 句柄在**首次初始化时**读回它 —— 于是重启后的起点是上次的远程清单。
//!
//! ── 缓存不带有效期（有意，不要顺手加）───────────────────────
//! 只要成功拉到过一次，这份清单就一直保留到下一次刷新成功覆盖它。理由：
//! 本模块存在的全部意义是「宁可给一份旧清单，也不要退回静态兜底」——
//! 加一个有效期等于到点后主动退回兜底，与目的相反。时效性由刷新链路负责
//! （各家的 TTL + 定时任务 + 客户端拉 `/v1/models` 时的被动刷新），
//! 缓存只负责「别丢」。`fetchedAt` 仍然存下来，界面与日志据此说明
//! 「这份清单是什么时候拉到的」。
//!
//! ── 为什么十份清单挤在一个键里 ──────────────────────────────
//! `kv` 的固定键必须登记进 `db::schema::RESERVED_KV_KEYS`（配置写入靠它排除
//! 「不归我管」的键，见那里的「两类键名绝不能相撞」）。一家一个键要登记十项，
//! 撞名核对面随之扩大；整份存一个对象只加一项，键名也不会随 provider 增减而
//! 变。写入是「读改写整份」，但整段在一把锁内完成（`Db::with`），不会丢别家
//! 的更新 —— 刷新本身也是低频动作（各家 TTL 5 分钟起）。
//!
//! 读取按各家的初始化时机走（各家目录句柄首次被访问时读自己那一份，见各家的
//! `restored_state` / `restored`）：启动时的恢复因此是十次「读同一行、取自己
//! 那个键」—— 每次都要把整行解析一遍。这是有意留着的简单形态：改成「读一次
//! 缓存到内存」就要在每次写入后同步那份内存副本，多一处可能与库不一致的状态。
//! 十次解析发生在启动的一次性路径上，量级是几毫秒。
//!
//! ── 值的形态 ────────────────────────────────────────────────
//! ```json
//! { "raccoon": { "models": [ … ], "fetchedAt": 1758000000000 } }
//! ```
//! `models` 存的是**各家内存状态的形态**（即落地后的形态），恢复时原样装回。
//! 不在这里做任何字段映射：映射只属于各家的落地函数，两处各写一份迟早分叉，
//! 而分叉的表现是「重启后的清单与刷新后的清单字段不一样」—— 最难查的一类。
//!
//! ── 硬约束：持锁期间不打日志 ────────────────────────────────
//! 与 `config::sql` 同一条：`Db::with` 拿的是全局唯一的连接锁，而
//! `logging::log` / `verbose` 要往同一个库写 `logs` 表 —— `std::sync::Mutex`
//! 不可重入，在闭包里打日志会当场死锁。本模块的日志一律在 `with` 之外打。

use std::sync::OnceLock;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use crate::server::db::Db;
use crate::server::logging;

/// `kv` 表里的键名（保留键，见 `db::schema::RESERVED_KV_KEYS`）。
pub const KV_KEY: &str = "modelCatalogCache";

// ─── 缓存条目的标识符（`kv` 值里的键，不是 `kv` 的键名）─────────
//
// 集中在这里而不是各家自己拼字符串：写错一个字的后果是「那家的缓存永远存不
// 进去、也读不回来」，而症状只是「重启后又变回内置清单」—— 与「缓存没生效」
// 无法区分，排查要翻数据库。
// 粒度是**一个远程目录一条**：按地区分开的目录（Qoder / AutoClaw / Accio）
// 各自一条，与各家内存状态的粒度一致。

/// WorkBuddy（`/v3/config`）
pub const SCOPE_WORKBUDDY: &str = "workbuddy";
/// 小浣熊（`/model_catalog`）
pub const SCOPE_RACCOON: &str = "raccoon";
/// Qoder 国际版
pub const SCOPE_QODER_GLOBAL: &str = "qoderGlobal";
/// Qoder 中国版
pub const SCOPE_QODER_CN: &str = "qoderCn";
/// CatPaw（`/api/agent/maas/model-types`）
pub const SCOPE_CATPAW: &str = "catpaw";
/// AutoClaw 国内版
pub const SCOPE_AUTOCLAW_CN: &str = "autoclawCn";
/// AutoClaw 国际版
pub const SCOPE_AUTOCLAW_INTL: &str = "autoclawIntl";
/// Cline（两个池共用一份远程清单）
pub const SCOPE_CLINE: &str = "cline";
/// Accio 国际版
pub const SCOPE_ACCIO_GLOBAL: &str = "accioGlobal";
/// Accio 国内版
pub const SCOPE_ACCIO_CN: &str = "accioCn";

/// 全部 scope（事实来源：`cached_scopes` 按它遍历；新增一家时加在这里）。
pub const ALL_SCOPES: &[&str] = &[
    SCOPE_WORKBUDDY,
    SCOPE_RACCOON,
    SCOPE_QODER_GLOBAL,
    SCOPE_QODER_CN,
    SCOPE_CATPAW,
    SCOPE_AUTOCLAW_CN,
    SCOPE_AUTOCLAW_INTL,
    SCOPE_CLINE,
    SCOPE_ACCIO_GLOBAL,
    SCOPE_ACCIO_CN,
];

/// 一份清单缓存
pub struct CachedCatalog {
    /// 落地后的清单（各家内存状态的形态）
    pub models: Vec<Value>,
    /// 那次成功刷新的时刻（毫秒）
    pub fetched_at: i64,
}

/// 进程级库句柄（与 `config` / `core::debug_traffic` 同一模式）
static DB: OnceLock<Option<Db>> = OnceLock::new();

/// 装入库句柄。`bootstrap` 里 `Db` 就绪后调用一次；重复调用忽略（`OnceLock`）。
///
/// 收 `Option<Db>` 与 `config::init` / `logging::init_store` 同一形态：库打不开
/// 时也照常调用，缓存整项降级为「不读也不写」，其余功能不受影响。
pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn db() -> Option<Db> {
    DB.get().and_then(|slot| slot.as_ref()).cloned()
}

/// 读一份缓存。库不可用 / 键不存在 / 值不是合法 JSON / 这一家没有条目 → `None`。
///
/// `None` 的语义是「没有可用的缓存」，调用方据此保持原状（各家回落到静态
/// 兜底清单，与改造前一致）。**不返回 `Err`**：缓存读不到不是需要调用方分支
/// 处理的异常，它只有「有」与「没有」两种结果。
pub fn load(scope: &str) -> Option<CachedCatalog> {
    let db = db()?;
    let all = db.with(read_all).flatten()?;
    let entry = all.get(scope)?;
    let models = entry.get("models").and_then(Value::as_array).cloned()?;
    // 空清单不算缓存：它表达的是「这次没拉到东西」，装回去等于把目录清空
    if models.is_empty() {
        return None;
    }
    let fetched_at = entry.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0);
    Some(CachedCatalog { models, fetched_at })
}

/// 写一份缓存（读改写整份，在同一把锁内完成）。
///
/// 失败（库不可用 / 序列化失败）只记一条 verbose 日志、不影响调用方 ——
/// 缓存写不进去的后果是「下次启动可能回落到内置清单」，不该让一次成功的
/// 模型目录刷新因此报错。
pub fn save(scope: &str, models: &[Value], fetched_at: i64) {
    if models.is_empty() {
        return;
    }
    let Some(db) = db() else { return };
    let written = db
        .with(|conn| {
            let mut all = read_all(conn).unwrap_or_default();
            all.insert(
                scope.to_string(),
                json!({ "models": models, "fetchedAt": fetched_at }),
            );
            write_all(conn, &all)
        })
        .unwrap_or(false);
    if !written {
        logging::verbose(
            "[Models]",
            &format!("模型清单缓存写入失败（{scope}）：下次启动可能回落到内置清单"),
        );
    }
}

/// 库里有缓存的 scope 及各自的拉取时刻（启动日志用）。
///
/// 让「这次启动的清单是从缓存恢复的、是什么时候拉的」在日志里留痕 ——
/// 没有它，排障时「清单是刚拉的还是上次的」只能靠翻数据库回答。
/// 一次读库（不是逐 scope 调 [`load`]）：整份缓存本来就在一行里。
pub fn cached_scopes() -> Vec<(&'static str, i64)> {
    let Some(db) = db() else { return Vec::new() };
    let Some(all) = db.with(read_all).flatten() else { return Vec::new() };
    ALL_SCOPES
        .iter()
        .filter_map(|scope| {
            let entry = all.get(*scope)?;
            let models = entry.get("models").and_then(Value::as_array)?;
            if models.is_empty() {
                return None;
            }
            let at = entry.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0);
            Some((*scope, at))
        })
        .collect()
}

/// 拉取时刻 → 「多久之前」的粗粒度文案（日志用）。
///
/// 用相对时间而不是绝对时刻：这条日志要回答的是「这份清单旧不旧」，
/// 「3 天前」比「2026-09-22 14:03:11」直接；且不必为日志引入一处时间格式化。
pub fn age_text(fetched_at: i64) -> String {
    if fetched_at <= 0 {
        return "未知".to_string();
    }
    let age = logging::now_ms() - fetched_at;
    if age < 60_000 {
        "刚刚".to_string()
    } else if age < 3_600_000 {
        format!("{} 分钟前", age / 60_000)
    } else if age < 86_400_000 {
        format!("{} 小时前", age / 3_600_000)
    } else {
        format!("{} 天前", age / 86_400_000)
    }
}

/// 读出整份缓存对象（键不存在 / 值坏 → `None`，由调用方按「没有缓存」处理）。
fn read_all(conn: &Connection) -> Option<Map<String, Value>> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", params![KV_KEY], |row| {
            row.get(0)
        })
        .ok();
    serde_json::from_str::<Value>(&text?).ok()?.as_object().cloned()
}

/// 整份写回（单行 UPSERT）。
fn write_all(conn: &Connection, all: &Map<String, Value>) -> bool {
    let Ok(text) = serde_json::to_string(&Value::Object(all.clone())) else {
        return false;
    };
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![KV_KEY, text],
    )
    .is_ok()
}
