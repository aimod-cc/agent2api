//! 网关配置在 `kv` 表里的**行级读写** —— 本模块唯一出现 SQL 的地方。
//!
//! ── 映射关系：一个顶层键一行 ────────────────────────────────
//! 改造前配置是 `{config_dir}/config.json` 的一整份 JSON 对象；现在它的每个
//! **顶层键**就是 `kv` 表的一行（键名与配置文件里的一字不差），值是该键那段
//! JSON 文本。于是：
//!   - 读 = 一次全取，跳过 `is_reserved` 的那些键（那些是别的模块的状态，
//!     见 `db::schema` 的键命名规范）；
//!   - 写 = 一个事务里「逐键 UPSERT + 删掉新配置里已经没有的键」。
//! 为什么整份配置不塞进**一个** `kv` 行（比如 `config: {...}`）：配置的顶层
//! 键同时是 HTTP API 的契约，也是别的模块单独读写的单元（`core::auto_checkin`
//! 只碰 `autoCheckin`、`core::model_rules` 只碰 `modelRules`）—— 拆成一行一
//! 键之后，这些模块的读改写互不干扰；整份塞一行则每次都要「读整份 → 改一项
//! → 写回整份」，任何一处忘记先读就会把别人的配置抹掉（那正是 `update()` 现在
//! 靠纪律维持、进库后可以直接由 UPSERT 规避的事）。
//!
//! ── 值的形态 ────────────────────────────────────────────────
//! `kv.value` 里存的就是那段 JSON（标量存 `30`、对象存 `{...}`、字符串存
//! `"zh-CN"`），与 `config.json` 里的写法一一对应 —— 排障时把库里的值直接
//! 贴进配置文件就能比对（约定见 `db::schema` 模块头）。
//!
//! ── 为什么一半函数收 `&Connection`、一半收 `&Db` ─────────────
//! 同一套 SQL 有两个调用方，入口形态不同：
//!   - **运行期**（`load` / `save_raw`）手里是 `Db`，要走 `Db::with` 拿锁；
//!   - **迁移项**（`db::migrate::config`）拿到的是 `&Connection`（框架签名
//!     如此，且它自己开事务）。
//! `*_conn` 后缀的那组是**实际实现**（裸连接，在调用方的锁/事务里跑），
//! 不带后缀的是运行期的薄包装（只取锁、把错误换成上层要的形状）。两处走同一份
//! SQL，于是「迁移进来的配置」与「运行期读到的配置」不可能因为实现分叉而漂移
//! （与 `request_stats` 同一取舍）。
//!
//! ── 「删除多余键」为什么只在 `write_conn` 里 ──────────────────
//! 这是**运行期整份写**才有的语义（对应文件形态的「整文件覆盖」），
//! 迁移是**导入**不是替换：迁移项用 [`import_conn`]，只做逐键 UPSERT。
//! 完整论证见 `db::migrate::config` 的模块头（为什么导入不能带删除）。
//!
//! ── 并发：本层不加锁、不打日志 ──────────────────────────────
//! 所有函数取裸 `&Connection`，串行化由 `Db` 那把 Mutex 负责。**硬约束**：
//! 持这把锁期间绝不能再调 `logging::log` / `verbose` —— 它们要写同一个库，
//! `std::sync::Mutex` 不可重入，会当场死锁（本层所有函数都不打日志，
//! 错误一律 `Err` 交回上层，由上层在锁外记）。

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::server::db::schema;
use crate::server::db::Db;

/// 配置迁移的完成标记键（`db::migrate::config` 的幂等闸门）。
///
/// 与 `settings` 那种「目标键在不在」的闸门不同 —— 这里**必须**用标记：
/// 配置项是**开放集合**（任何顶层键都算），没法问「配置迁过了没有」——
/// 「库里非保留键非空」既可能是迁移来的，也可能是运行期写进来的，据此跳过会
/// 把用户的旧配置永久留在文件里。
/// T6 论证过「不用 `migratedXxx: true` 标记」，理由是「标记与数据可能不一致
/// （标记写了但写入只落了一半）」—— 那条反对在这里**不成立**：标记与数据在
/// **同一个事务**里落（见 [`import_conn`]），不存在「标记写了数据没进去」的
/// 中间态。完整论证见 `db::migrate::config` 的模块头。
pub(crate) const KEY_CONFIG_MIGRATED: &str = "configMigrated";

/// 读出全部配置键（库里属于配置的那些行）。
///
/// 跳过 `is_reserved` 的键：`kv` 是**共享表**（账号的 `priorityScope`、日志的
/// `logsNextId`、桌面设置 `desktopSettings`、两个迁移标记，以及**遗留**的
/// `desensitize`（那个功能已删、数据留着）都在里面）。不排除它们的话，它们会被当成「配置里的未知字段」混进
/// `raw` 底稿 —— 后果不只是 GET /api/config 多露出几个字段：下一次运行期整份写
/// 会把这些键**当成配置项原样回写**，而整份写的删除语义是「库里也删掉」，
/// 于是某次写配置就可能把别人的状态删掉或覆盖（`db::schema` 模块头讲的
/// 「两类键名绝不能相撞」就是这条约束的另一面）。
pub(crate) fn read_conn(conn: &Connection) -> rusqlite::Result<Map<String, Value>> {
    let mut statement = conn.prepare("SELECT key, value FROM kv")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let mut raw = Map::new();
    for row in rows {
        let (key, value) = row?;
        if schema::is_reserved(&key) {
            continue;
        }
        // 值不是合法 JSON（手工改库、别的写入方用了别的编码）→ 跳过这一项
        // 而不是整份配置作废：跳过最坏是「这一项回落到默认值」，
        // 整份作废会让用户看到的所有配置一起变回默认。
        let Some(value) = value else { continue };
        if let Ok(parsed) = serde_json::from_str::<Value>(&value) {
            raw.insert(key, parsed);
        }
    }
    Ok(raw)
}

/// 运行期整份写：**逐键 UPSERT + 删掉新配置里已经没有的键**，整批一个事务。
///
/// ── 为什么必须删多余键（与文件形态的语义差异）────────────────
/// 文件形态下这是一次「整文件覆盖」，`raw` 里没有的键自然就不在文件里了 ——
/// 于是「删掉一个配置项」（`set_api_key(None)` 会 `raw.remove`）能生效。
/// 进库后如果只做 UPSERT，那些被删掉的键会**留在库里**：
/// 下一次启动读回来，用户会发现刚清掉的那项又回来了（而且删不掉）。
/// 所以删除这一步是**语义等价所必需**的，不是清理洁癖。
///
/// ── 删除的边界：只删「非保留键」─────────────────────────────
/// `schema::is_reserved` 为真的键不参与删除 —— 它们不属于配置。用「读出现有
/// 键、在 Rust 里算差集、逐键删」而不是一条 `DELETE ... WHERE key NOT IN (…)`：
/// 后者的排除列表要拼进 SQL，保留键一多就是动态 SQL；差集算法天然跟着
/// `RESERVED_KV_KEYS` 走，加固定键时不必改 SQL。配置迁移的标记键也在保留集合里，
/// 所以整份写不会把「已迁过」这个事实抹掉（否则下次启动会重导一遍旧文件）。
///
/// ── 事务边界（本模块最关键的一处）───────────────────────────
/// 「多行 UPSERT + 删除」必须有原子性，否则中途失败会留下**半新半旧**的配置：
/// 已写新值的那几项 + 还没轮到的旧值 + 该删没删的键。用户的感受是「保存之后
/// 一部分设置生效了、另一部分没生效」，且无从判断哪部分是哪部分。文件形态下
/// 不存在这个问题（一次 `fs::write` 覆盖），所以这是**改造引入的新要求**。
///
/// 用 `unchecked_transaction` 而不是 `transaction`：本函数要同时服务
/// 「`Db::with` 给的 `&Connection`」与「迁移项给的 `&Connection`」两处，
/// 而 `transaction()` 要 `&mut Connection`（理由与取舍见 `db::migrate` 模块头
/// 第三条）。调用方保证期间不借用同一连接发别的语句。
pub(crate) fn write_conn(conn: &Connection, raw: &Map<String, Value>) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("开启配置写入事务失败: {error}"))?;
    for (key, value) in raw {
        upsert(&tx, key, value)?;
    }
    // 差集：库里的非保留键 − 新配置的键 = 本次要删掉的项
    for key in stale_keys(&tx, raw)? {
        tx.execute("DELETE FROM kv WHERE key = ?1", rusqlite::params![key])
            .map_err(|error| format!("删除配置项 {key} 失败: {error}"))?;
    }
    tx.commit()
        .map_err(|error| format!("提交配置写入失败: {error}"))?;
    Ok(())
}

/// 迁移导入：**只做逐键 UPSERT（不删除）**，与完成标记在同一个事务里落。
///
/// ── 为什么导入不带删除 ──────────────────────────────────────
/// 迁移是「把旧文件里的东西搬进库」，不是「用旧文件替换库里的配置」。
/// 带上删除会有一个真实（虽然少见）的破坏路径：迁移项跑在 `bootstrap` 里，
/// 而运行期写配置的调用点全在它之后 —— 万一将来有人在迁移之前写了一次配置，
/// 「删掉旧文件里没有的键」就会把那次写入抹掉。不带删除则最坏是「旧文件里多出
/// 的几个键被并进来」，那正是迁移该做的事。
///
/// ── 为什么标记与数据必须同一个事务 ──────────────────────────
/// 标记表达的事实是「这份旧文件已经导入过了」。若分两个事务写，中途失败就会
/// 留下「标记在、数据缺一半」的状态：闸门认为迁过了，缺的那几项**永远不会**
/// 再被补上（用户的表现是「一部分设置莫名其妙回到默认」）。同一个事务里
/// 提交之后，二者要么都在、要么都不在。
///
/// 返回真正写进去的配置键数（不含标记），供调用点报「导入 N 条」。
pub(crate) fn import_conn(
    conn: &Connection,
    raw: &Map<String, Value>,
    marker: &str,
) -> Result<usize, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("开启配置迁移事务失败: {error}"))?;
    for (key, value) in raw {
        upsert(&tx, key, value)?;
    }
    tx.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![marker, "true"],
    )
    .map_err(|error| format!("写入配置迁移标记失败: {error}"))?;
    tx.commit()
        .map_err(|error| format!("提交配置迁移失败: {error}"))?;
    Ok(raw.len())
}

/// 单个键的 UPSERT（`write_conn` 与 `import_conn` 共用）。
///
/// 与保留键相撞时**报错而不是静默跳过**：静默跳过会让用户改了设置却看不到效果；
/// 报错至少能在日志里看到原因。这个分支在当前键集合下不可达（`db::schema`
/// 模块头有逐项核对），留着是因为它一旦可达就是数据事故（覆盖别人的状态）。
fn upsert(conn: &Connection, key: &str, value: &Value) -> Result<(), String> {
    if schema::is_reserved(key) {
        return Err(format!("配置键与保留键同名，拒绝写入: {key}"));
    }
    let text = serde_json::to_string(value)
        .map_err(|error| format!("配置项 {key} 序列化失败: {error}"))?;
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, text],
    )
    .map_err(|error| format!("写入配置项 {key} 失败: {error}"))?;
    Ok(())
}

/// 库里该删掉的配置键：非保留、且不在新配置里。
fn stale_keys(conn: &Connection, raw: &Map<String, Value>) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare("SELECT key FROM kv")
        .map_err(|error| format!("读取配置键清单失败: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("读取配置键清单失败: {error}"))?;
    let mut stale: Vec<String> = Vec::new();
    for row in rows {
        let key = row.map_err(|error| format!("读取配置键清单失败: {error}"))?;
        if !schema::is_reserved(&key) && !raw.contains_key(&key) {
            stale.push(key);
        }
    }
    Ok(stale)
}

/// 迁移完成标记在不在（迁移项的幂等闸门）。
///
/// 只取 `SELECT 1 ... LIMIT 1` 而不是把整份配置读出来：闸门只关心存在性，
/// `modelRules` 这类值可能有几十 KB，为了判「迁过没有」把它们全读进来是白费。
pub(crate) fn marker_present_conn(conn: &Connection, marker: &str) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM kv WHERE key = ?1 LIMIT 1",
            rusqlite::params![marker],
            |row| row.get(0),
        )
        .ok();
    Ok(found.is_some())
}

// ─── 运行期的薄包装（只取锁 + 换错误形状）────────────────────

/// 一次取锁读出「配置底稿 + 迁移标记在不在」。
///
/// 两者一起取而不是两次调用：`read_raw` 要用「标记在不在」决定要不要再回落
/// 读旧文件，分两次取锁会在中间留一个窗口 —— 若迁移恰好在那期间提交，
/// 第一次读到的 raw 是迁移前的、第二次读到的标记是迁移后的，于是**既不回落
/// 旧文件、又拿着迁移前那份不全的 raw**。虽然实际时序上不可能（迁移与配置
/// 初始化都在 `bootstrap` 的单线程路径上），但一次取锁就没有这个问题，
/// 且少一次锁往返。
pub(super) fn read_snapshot(db: &Db) -> Option<(Map<String, Value>, bool)> {
    db.with(|conn| {
        let raw = read_conn(conn).ok()?;
        let migrated = marker_present_conn(conn, KEY_CONFIG_MIGRATED).unwrap_or(false);
        Some((raw, migrated))
    })
    .flatten()
}

/// 写一份配置；库不可用按失败上报，由上层决定降级行为。
///
/// **持锁期间不写日志**：`Db::with` 拿的是全局唯一那把连接锁，而
/// `logging::log` 要往同一个库写 `logs` 表 —— `std::sync::Mutex` 不可重入，
/// 在闭包里打日志等于当场死锁。所以这里只把 `Result` 交出去，日志由调用方
/// 在锁外记（`config::save_raw` 就是这么做的）。
pub(super) fn write_all(db: &Db, raw: &Map<String, Value>) -> Result<(), String> {
    match db.with(|conn| write_conn(conn, raw)) {
        None => Err("数据库不可用".to_string()),
        Some(result) => result,
    }
}
