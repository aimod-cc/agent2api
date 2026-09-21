//! 数据存储概况（设置页「保存位置」）。
//!
//! ```text
//! GET /api/storage   统一库的位置、大小与各表条数
//! ```
//!
//! ── 本切片起是**单库语义**（改造前是「三类数据各自一个目录」）────────
//! 改造前这里有两条写路由（`POST /api/storage/relocate` 搬家 +
//! `GET /api/storage/progress` 进度轮询），三类数据各自可换保存目录。
//! 数据全部进统一库 `{config_dir}/agent2api.db` 之后那套语义在数据模型上
//! 已经不成立：「把日志单独搬到另一个文件」只会造出第二份真相（库里的还在），
//! 下一个请求写日志时两份立刻分叉（论证见各 store 里被删掉的 `relocate`）。
//! 因此两条写路由连同三个 store 的 `relocate` 一起删除，本模块只剩**只读概况**。
//!
//! ── 想换位置怎么办（界面上要告诉用户的那句话）───────────────
//! 库的位置由配置目录决定，而配置目录由环境变量 `AGENT2API_PROXY_HOME`
//! 覆盖（旧名 `WORKBUDDY_PROXY_HOME` 仍可读，见 `gateway::config_dir`）。
//! 设置页因此只展示、不给「更改」按钮，并把这句话原样写给用户 ——
//! 一个点了必然报错的按钮比没有按钮更糟（这正是本切片要收掉的死路径）。
//!
//! ── 形状为什么是 `database: {...}` 而不是平铺 ────────────────
//! 改造前返回 `{logs: {...}, requests: {...}, debug: {...}}`，前端拿
//! `storage[target]` 逐项渲染。现在三类数据**没有各自的目录与文件**，能给的
//! 只有「同一个库 + 各表条数」——平铺成三个同形对象会诱导读者以为它们仍然
//! 各自独立（前端当年就是被这个形状带着把 `bytes + dailyBytes` 相加，
//! 而那两个字段指向同一个文件，字节数被算了两遍）。
//! 合成一个 `database` 对象之后，「只有一个文件、里面有若干张表」这件事
//! 在响应形状上直接可见。
//!
//! 保留 `configDir` 字段：前端用它拼「库在配置目录下」这句提示，
//! 也是排障时第一眼要看的位置。

use std::path::Path;

use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// 文件大小；不存在（还没写过盘）按 0 算
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// GET /api/storage —— 统一库的位置、大小与各表条数（设置页渲染用）。
///
/// `bytes` 是**库主文件**的大小。WAL 里的页不算在内（`RequestStats::flush`
/// 在停机时把 WAL 并回主库，正是为了让「占用多大」只有一个文件是真相）——
/// 所以运行期间这个数字可能略小于实际占用，属预期。
///
/// 条数一律取各 store 现有的计数入口，没有硬凑：
///   - `accounts` / `logs` / `requests` / `dailyDays` / `debug` 分别来自
///     `AccountStore` 的账号快照、`LogStore::stats()`、`RequestStats::stats()`
///     与 `debug_traffic::count()`；
///   - 库不可用时各计数回落 0（与各 store 自己的降级值一致），
///     `file` 给空串 —— 让界面显示「不可用」，而不是一个看着正常的 0 条。
pub async fn get_storage(State(state): State<ServerState>) -> Response {
    let config_dir = config::config_dir();
    // 库路径的事实来源是 `Db::file()`（打开失败时用约定路径兜底：排障时
    // 「该去哪找库」这个答案与库有没有打开无关，与 `AccountStore::file()`
    // 的处置一致）。
    let db_file = match state.db() {
        Some(db) => db.file().to_path_buf(),
        None => config_dir.join(crate::server::db::FILE_NAME),
    };
    let available = state.db().is_some();

    // 账号数：走账号快照（`list_accounts` 会取锁并读全表）。库不可用时它返回
    // 空列表，正好是「0 个账号」这个诚实的结果。
    let accounts = state
        .store()
        .list_accounts()
        .get("accounts")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);

    // 事件日志条数：`LogStore::stats()` 的 `total`（库不可用时它给 0）
    let logs = crate::server::logging::store_ref()
        .map(|store| store.stats().total)
        .unwrap_or(0);

    // 请求明细与聚合天数：`RequestStats::stats()` 一次给全（它自己会取锁读库）
    let stats = state.request_stats().stats();
    let requests = stats.get("total").and_then(Value::as_u64).unwrap_or(0);
    let daily_days = stats.get("dailyDays").and_then(Value::as_u64).unwrap_or(0);

    ok_json(json!({
        "configDir": config_dir.to_string_lossy(),
        "database": {
            "file": db_file.to_string_lossy(),
            "bytes": if available { file_size(&db_file) } else { 0 },
            // 条数只在库可用时才可信（不可用时各 store 都回落 0，与「真的没有
            // 数据」在数字上无法区分）—— 这个布尔让界面能说「不可用」而不是
            // 显示一排看着正常的 0
            "available": available,
            "accounts": accounts,
            "logs": logs,
            "requests": requests,
            "dailyDays": daily_days,
            "debug": crate::server::core::debug_traffic::count(),
        },
    }))
}
