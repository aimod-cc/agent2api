//! 账号表的**行级 SQL 访问层** —— 账号存储里唯一出现 SQL 的地方。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! 改造前账号数据在 `{config_dir}/accounts.json`：每次读写都是「整份 JSON 解析 →
//! 内存里改一条 → 整份重新序列化写回」。账号上限 20 条（`MAX_ACCOUNTS`），
//! 单看数据量不大，但「改一个字段要重写整份文件」意味着：改备注名要碰全部账号
//! 的凭证、一次写盘失败丢掉的是全部账号、且每次读都要把 20 条记录的 JSON
//! 全部解析一遍（含 rateLimits 这类嵌套结构）。
//!
//! 换到 SQLite 之后，单条增删改只该动**涉及的那一行**。本文件就是这件事的落点：
//! 把「一行 ↔ `StoredAccount`」的编解码、以及各种按需查询集中在这里，
//! 上层（`store_*`、各 provider 的账号模块）不再自己拼 SQL。
//!
//! ── `data` 是权威副本，五列是投影 ───────────────────────────
//! 表结构（见 `server/db/schema.rs`）里 `data` 列存**整条账号记录的 JSON 原文**，
//! 这是「用户升级绝不能丢账号里任何字段」这条不变量的实现方式 —— 含未知字段、
//! 用户手工加的备注、旧版本遗留的键。`id` / `provider` / `priority` / `enabled` /
//! `added_at` 五列是从 JSON 里**复制出来**的查询投影（排序、筛选、唯一性校验用）。
//!
//! 由此推出一条硬规则：**写的时候以 `data` 为准去派生投影列**，绝不让两者漂移。
//! 本文件的写入一律走 `record.provider()` / `record.priority()` / `record.enabled()`
//! / `record.added_at()` 这几个**容错访问器**（而不是直接取 JSON 字段），于是
//! 「手工写成 `enabled: "false"` 的脏值」这类情况在列与访问器之间也是同一个答案：
//! 列里存的是**访问器的取值**，不是字段的原样搬运。读的时候反过来 —— 只认
//! `data`，五列不参与重建（`decode_data` 只看 data 一列）。
//!
//! ── 顺序：`ORDER BY rowid` ──────────────────────────────────
//! `list_accounts` 的契约是「按**文件顺序**返回，界面自己按优先级排序」。
//! 表里没有「数组下标」这一列（也不该为它加一列：下标是文件的实现细节，
//! 不是数据本身），所以用 SQLite 的隐式 `rowid` 作为物理顺序，它与旧的
//! accounts.json 数组顺序在三类场景下逐字对应：
//!   - **迁移**：导入按数组序逐条 INSERT → rowid 升序 == 原数组序；
//!   - **新增/替换**：旧代码是 `retain(≠id)` + `push`（移到数组末尾），
//!     这里的 [`put`] 是 `DELETE` + `INSERT`（拿到新的最大 rowid）→ 同样落到末尾；
//!   - **原地修改**：旧代码在下标上改字段（位置不变），这里的
//!     [`update_in_place`] 是 `UPDATE`（rowid 不变）→ 位置同样不变。
//! 一处**有意的差异**：`promote_to_front` / `migrate_startup` 在旧实现里会把
//! 整份数组按优先级重排后落盘（文件行序随之改变），这里改成原地 UPDATE
//! （rowid 不变）、物理顺序不动。
//! **例外**：`migrate_startup` 的 Cline 拆池改名会改 `id`（主键），那几条在
//! `save_state` 里走「删旧 id + 插新 id」两个分支，于是**会**落到末尾 ——
//! 形状与旧实现「整份数组重排」不同，但同样无消费方可观察（理由见下）。
//! 上面这些差异都不会被调用方观察到 —— 两处消费方（界面、转发选路）都自己
//! 按优先级排（`accounts-view.js:144` 与 `accounts-groups.js:351` 的
//! `byPriorityOrder`、`routing::pick_account_by_priority` 的 `sort_by`），
//! 而账号数组的顺序本身**没有任何消费方**（不按位置取、不按下标索引）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本文件不做网络请求、不持锁（锁由调用方的 `AccountStore::guard` 保证），
//! 也绝不 panic：所有 SQL 失败都以 `rusqlite::Result` 往上传，由调用方转成
//! `AccountStoreError`（500）。

use rusqlite::{params, Connection, OptionalExtension};

use crate::server::core::account_store::state::{AccountState, StoredAccount};

/// 一行 `data` 列 → `StoredAccount`；JSON 坏了或不是对象/缺 id 时返回 None。
///
/// 与旧文件读取路径的口径**逐字一致**：那时也是 `filter_map(from_value)`
/// 逐条过滤（坏条目跳过、不拖垮整份数据）。差异只有一处：文件整体解析失败会
/// 退化成「空列表」，而这里坏的是单行、其余行照常读出 —— 比整体丢弃更好，
/// 也不是行为倒退（同一份数据在文件形态下本来就整体读不出来）。
fn decode_data(text: &str) -> Option<StoredAccount> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    StoredAccount::from_value(value)
}

/// 全部账号，按物理顺序（见模块头「顺序」一节）。
///
/// 这是「确实需要整份数据」的路径才用的读法：列表快照、导出、启动迁移、
/// 批量操作、`pick_current` 一类的全局派生。**单条增删改不该用它** ——
/// 那些路径走 [`load_by_id`] / [`count_except`] / [`priorities_except`]。
pub(crate) fn load_all(conn: &Connection) -> rusqlite::Result<Vec<StoredAccount>> {
    let mut stmt = conn.prepare("SELECT data FROM accounts ORDER BY rowid")?;
    let mut rows = stmt.query([])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        if let Some(record) = decode_data(&text) {
            records.push(record);
        }
    }
    Ok(records)
}

/// 按主键取一条（不存在返回 None）。单条读写路径的第一步。
pub(crate) fn load_by_id(conn: &Connection, id: &str) -> rusqlite::Result<Option<StoredAccount>> {
    let mut stmt = conn.prepare("SELECT data FROM accounts WHERE id = ?1")?;
    let mut rows = stmt.query(params![id])?;
    while let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        return Ok(decode_data(&text));
    }
    Ok(None)
}

/// 某个 provider 的全部账号（按物理顺序）。
///
/// 用投影列 `provider` 过滤：它是写入时从 `record.provider()` 派生的，
/// 而 `provider()` 对缺失/空/非字符串的字段一律兜底成默认 provider ——
/// 所以「历史账号没写 provider 字段」这种情况在列里已经是 `workbuddy`，
/// 按 `workbuddy` 查得到它，与旧代码在内存里过滤的结果一致。
pub(crate) fn load_by_provider(
    conn: &Connection,
    provider: &str,
) -> rusqlite::Result<Vec<StoredAccount>> {
    let mut stmt =
        conn.prepare("SELECT data FROM accounts WHERE provider = ?1 ORDER BY rowid")?;
    let mut rows = stmt.query(params![provider])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        if let Some(record) = decode_data(&text) {
            records.push(record);
        }
    }
    Ok(records)
}

/// 「除某 id 之外」的账号数 —— 新增路径的 `MAX_ACCOUNTS` 判定用它
/// （旧代码是 `others.len()`，其中 `others` 排除了同 id 的既有记录）。
pub(crate) fn count_except(conn: &Connection, id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM accounts WHERE id <> ?1",
        params![id],
        |row| row.get(0),
    )
}

/// 账号总数 —— `MAX_ACCOUNTS` 上限判定用（各家的添加路径都要问一次）。
pub(crate) fn count_all(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
}

/// 某个 provider 的账号数 —— 「这一家有没有账号」的判定
/// （三家旧数据导入的触发条件就是「本家账号列表为空」）。
///
/// 用投影列 COUNT 而不是读记录：这里只问「有没有 / 有几条」，
/// 不需要知道它们长什么样。
pub(crate) fn count_by_provider(conn: &Connection, provider: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM accounts WHERE provider = ?1",
        params![provider],
        |row| row.get(0),
    )
}

/// 全部账号的 id —— 旧数据导入的「同 id 跳过」判定用。
///
/// 与 `priorities_all` 同理：只取主键一列，不解析任何记录的 JSON。
/// 导入过程中新加的 id 由调用方自己往集合里补（同批内也不能重号）。
pub(crate) fn all_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM accounts")?;
    let mut rows = stmt.query([])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        values.push(row.get(0)?);
    }
    Ok(values)
}

/// 全部账号的优先级数值 —— `next_free_priority` 的入参。
///
/// 与 [`priorities_except`] 的区别只在于「要不要排除某个 id」：新增账号时
/// 要排除的那个 id 还不存在（新账号的 id 刚算出来），所以用这个；更新既有账号
/// 时要把它自己排除在外，用另一个。
pub(crate) fn priorities_all(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT priority FROM accounts")?;
    let mut rows = stmt.query([])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        values.push(row.get(0)?);
    }
    Ok(values)
}

/// 「除某 id 之外」的全部优先级数值 —— `next_free_priority` 的入参。
///
/// 只取一列：号段计算是纯数值运算，没必要把 20 条记录的完整 JSON（含
/// `rateLimits` 这类嵌套结构）都解析出来。冲突判定另走 [`priority_holder`]。
pub(crate) fn priorities_except(conn: &Connection, id: &str) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT priority FROM accounts WHERE id <> ?1")?;
    let mut rows = stmt.query(params![id])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        values.push(row.get(0)?);
    }
    Ok(values)
}

/// 占用某优先级的账号名（排除 `exclude_id` 自己），没有则 None。
///
/// 对应旧实现的 `find_priority_holder(&peers, priority, None)`：`peers` 是
/// 「除自己之外的全部账号」，判据是**归一后的优先级相等**。投影列 `priority`
/// 存的正是 `normalize_priority_value(record.priority())`，所以 SQL 里的
/// `priority = ?` 与内存里的判据等价，不需要把记录读出来再比一遍。
///
/// `ORDER BY rowid LIMIT 1`：多个账号占同一个号（手工编辑过的脏数据）时，
/// 取物理顺序里最靠前的那个 —— 也就是旧实现在数组里找到的第一个，
/// 于是 409 文案里出现的「占位者名字」与改造前一致。
pub(crate) fn priority_holder(
    conn: &Connection,
    priority: i64,
    exclude_id: &str,
) -> rusqlite::Result<Option<String>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT data FROM accounts WHERE priority = ?1 AND id <> ?2 ORDER BY rowid LIMIT 1",
            params![priority, exclude_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(text
        .as_deref()
        .and_then(decode_data)
        .map(|record| record.name()))
}

/// 写入（upsert）一条记录，**并把它移到物理顺序的末尾**。
///
/// 语义与旧实现的 `state.accounts.retain(≠id); push(record)` 逐字对应：
/// 先删同 id 的旧行、再插新行 → 新行拿到当前最大的 rowid，落到末尾。
/// 所有「添加/更新账号」的路径都用它（workbuddy / 小浣熊 / CatPaw / AutoClaw /
/// Cline / Qoder 六家的添加与桌面端导入），"更新既有记录让它在列表里往后挪"
/// 这个既有行为因此原样保留。
///
/// 为什么不用 `INSERT OR REPLACE`：它其实也会「删旧行 + 插新行」（同样落到末尾），
/// 但那条保证埋在 SQLite 文档的 REPLACE 算法里，读代码时看不出来；显式的
/// `DELETE` + `INSERT` 把这条语义写在脸上，也省得日后有人改成 `INSERT OR IGNORE`
/// 之类的变体却以为行为不变。
pub(crate) fn put(conn: &Connection, record: &StoredAccount) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![record.id()])?;
    insert_row(conn, record)
}

/// 原地更新一条记录（**物理位置不变**）。
///
/// 与 [`put`] 的区别只在顺序：`update_account`（改备注名/优先级/启用/代理）、
/// `move_account`（交换两个账号的优先级）、限额标记、token 回写这些「就地改
/// 字段」的操作走这里 —— 旧代码在下标上改字段，数组位置本来就不动，
/// 界面上的行序（优先级序）也不该因为一次「改备注名」而跳动。
///
/// 行不存在时退化成插入（拿到末尾位置）：那是「读出来之后被并发删掉了」的
/// 竞态窗口 —— 旧实现在那个窗口里是「把整份内存状态写回文件」，效果同样是
/// 让这条记录重新出现。这里保持同一结果（而不是静默丢弃这次写入），
/// 因为调用方大多已经把「写入成功」当作前提继续往下算了。
pub(crate) fn update_in_place(conn: &Connection, record: &StoredAccount) -> rusqlite::Result<()> {
    let changed = conn.execute(
        "UPDATE accounts SET provider = ?2, priority = ?3, enabled = ?4, added_at = ?5, data = ?6
         WHERE id = ?1",
        params![
            record.id(),
            record.provider(),
            record.priority(),
            enabled_flag(record),
            record.added_at(),
            record.to_value().to_string(),
        ],
    )?;
    if changed == 0 {
        insert_row(conn, record)?;
    }
    Ok(())
}

/// 删除一条记录，返回是否真的删掉了（`false` = 本来就没有这一行）。
pub(crate) fn delete(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])? > 0)
}

/// 一行 INSERT（[`put`] / [`update_in_place`] 的公共落点）
fn insert_row(conn: &Connection, record: &StoredAccount) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO accounts (id, provider, priority, enabled, added_at, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.id(),
            record.provider(),
            record.priority(),
            enabled_flag(record),
            record.added_at(),
            record.to_value().to_string(),
        ],
    )?;
    Ok(())
}

/// `enabled` 列取值：SQLite 没有布尔类型，按惯例存 0/1。
///
/// 取的是 `record.enabled()`（`enabled !== false` 的宽松语义）而不是 JSON 字段
/// 的原样值 —— 于是 `enabled: "false"` 这种手工脏值在列里是 1（启用），
/// 与访问器给上层看到的答案一致。
fn enabled_flag(record: &StoredAccount) -> i64 {
    if record.enabled() { 1 } else { 0 }
}

/// 把一整份 `AccountState` **按差异**写入（同一个事务内）。
///
/// 用于「本来就是整份集合的操作」：`batch_update` / `batch_remove`、
/// `migrate_startup`、三家旧数据导入、以及 `account_transfer` 的导入合并
/// （它经 `save_locked` 走到这里）。这些路径的算法本身要遍历全部账号
/// （去重编号、批量匹配、快照回显），读全量是**语义要求**而不是偷懒；
/// 但**写**不该因此重写整表 —— 因此这里先取现有行的 JSON，与目标状态逐条比对：
///   - 目标里有、库里没有 → INSERT
///   - 两边都有但 JSON 不同 → UPDATE（投影列一并刷新）
///   - 库里有的、目标里没有 → DELETE
///   - JSON 相同 → **一条语句都不发**（这就是「改了一个账号不重写其余 19 个」
///     的落地方式）
///
/// 顺序只在「新增」时体现（INSERT 按 `state.accounts` 的次序，于是新行的 rowid
/// 相对顺序与旧实现的 push 顺序一致）；既有行一律 UPDATE，物理位置不动。
///
/// 为什么拿 `&mut Connection`：调用方要么已经持有可变借用（`Db::with_mut`），
/// 要么在事务里。事务由本函数自己开（`conn.transaction()`），批量的原子性
/// 因此在最内层就成立 —— 调用方不需要记得包一层。
///
/// `priorityScope` 单独写在 `kv` 表（键名约定见 `server/db/schema.rs` 模块头）：
/// 它不属于任何一条账号记录，只在 `Some` 时写入（旧文件里也是「没有这个键」
/// 表示尚未迁移），读取见 [`load_priority_scope`]。
pub(crate) fn save_state(conn: &mut Connection, state: &AccountState) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    // 现有行：id → data 的**解析后**值。比字符串更可靠：两边序列化格式若有一丝
    // 差异（空格、键序）不会误判成「变了」而白写一遍
    let mut existing: Vec<(String, Option<serde_json::Value>)> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT id, data FROM accounts")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let text: String = row.get(1)?;
            existing.push((id, serde_json::from_str(&text).ok()));
        }
    }

    // 删除：库里在、目标状态里没有的
    for (id, _) in &existing {
        if !state.accounts.iter().any(|record| record.id() == *id) {
            tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        }
    }
    // 新增 / 更新
    for record in &state.accounts {
        let current = existing
            .iter()
            .find(|(id, _)| id == record.id())
            .map(|(_, value)| value.as_ref())
            .unwrap_or(None);
        match current {
            Some(value) if *value == record.to_value() => {}
            Some(_) => {
                tx.execute(
                    "UPDATE accounts SET provider = ?2, priority = ?3, enabled = ?4,
                     added_at = ?5, data = ?6 WHERE id = ?1",
                    params![
                        record.id(),
                        record.provider(),
                        record.priority(),
                        enabled_flag(record),
                        record.added_at(),
                        record.to_value().to_string(),
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO accounts (id, provider, priority, enabled, added_at, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        record.id(),
                        record.provider(),
                        record.priority(),
                        enabled_flag(record),
                        record.added_at(),
                        record.to_value().to_string(),
                    ],
                )?;
            }
        }
    }

    if let Some(scope) = &state.priority_scope {
        // 幂等写：值没变时 SQLite 也会重写这一行，但它只有一行、且这条路径
        // 本来就是「整份状态落盘」，不必为它再查一次
        tx.execute(
            "INSERT INTO kv (key, value) VALUES ('priorityScope', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![scope],
        )?;
    }
    tx.commit()
}

/// 读 `priorityScope`（`kv` 表；没有这个键时 None）。
///
/// 值是**裸字符串**而不是 JSON 编码的字符串：键名约定见 `server/db/schema.rs`
/// 模块头（「其它零散状态用固定名」一栏），`priorityScope` 是其中的固定名之一，
/// 存的就是 `"global"` 原文 —— 于是用 `sqlite3` 命令行直接看库时一眼能读懂。
pub(crate) fn load_priority_scope(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM kv WHERE key = 'priorityScope'",
        [],
        |row| row.get(0),
    )
    .optional()
}
