//! `{config_dir}/accounts.json` → `accounts` 表（+ `kv.priorityScope`）。
//!
//! 迁移项的实现细节（投影列、幂等、失败处理）都在下面各函数的注释里；
//! 框架侧的原则（幂等、不阻断、一个事务）见 [`super`] 的模块头。

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::logging;

/// 账号旧文件名（`{config_dir}/accounts.json`）。
///
/// 字面量在本文件里写一次而不是引 `account_store` 的常量：那个常量随账号存储
/// 改造已经**不存在了**（账号不再有"自己的文件"），而迁移项处理的正是
/// 「改造之前的那份文件」—— 它的名字是历史事实，不会随代码演进变化。
/// 换句话说，这个字符串属于**迁移项**而不是存储层，放在这里最不容易被误改。
const ACCOUNTS_FILE_NAME: &str = "accounts.json";

/// 迁移项的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一个字面量）。
pub(super) const LABEL: &str = "账号";

/// 旧文件的位置（不在原处时 `None`）。
///
/// 迁移项与框架的「有没有待迁移的旧文件」判定**共用这一份定位逻辑**：
/// 各写一份的话，两处对「旧文件在哪」的理解迟早分叉（改了一处忘了另一处，
/// 症状是界面提示「有待迁移数据」但点升级什么都不搬）。
pub(super) fn legacy_file(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(ACCOUNTS_FILE_NAME);
    path.is_file().then_some(path)
}

/// 旧文件 `{config_dir}/accounts.json` → `accounts` 表（+ `kv.priorityScope`）。
///
/// 文件形态（见 `core::account_store::state::AccountState` 的说明）：
/// `{ accounts: [ {id, provider, priority, enabled, addedAt, …}, … ],
///    priorityScope: "global" }`。顶层只认识这两个键，其余顶层字段（旧版本的
/// `currentAccountId` 之类）**刻意丢弃** —— 那是 `AccountState` 的既有口径
/// （读文件时就不解析它们），保持一致比"顺手多留一份"重要：迁移后库里多出
/// 一个谁都不读的键，只会让下一次有人读库时怀疑它是不是有用。
///
/// ── 投影列怎么填 ────────────────────────────────────────────
/// `data` 是**整条记录的 JSON 原文**（一字不改地搬，见 `sql.rs` 模块头），
/// 五列从记录里取，且一律走 `StoredAccount` 的**容错访问器**而不是直接读 JSON
/// 字段 —— 于是「手工写成 `enabled: "false"` 的脏值」这类情况在列里存的也是
/// 访问器的取值（`enabled()` 的宽松语义 → 1），与后续运行期写入的口径完全一致。
/// 这也是本函数必须复用 `from_value` / `provider()` / `priority()` 等原因的
/// 方式之一：**迁移与运行期写的是同一套投影规则**，否则同一个账号经迁移落库
/// 与经界面新增，列里的值可能不一样。
///
/// ── 幂等 ────────────────────────────────────────────────────
/// 目标表非空即跳过（模块头的原则）。这里的具体含义是「用户在库里已经有账号
/// 了，旧文件不该再插一遍」—— 无论那些账号是迁移来的还是后来新增的。
/// 注意检查发生在**备份之前**：表非空时连旧文件都不动（不备份、不删除），
/// 用户仍能在配置目录里看到它（这也是一条"迁移没做过"的可见线索）。
///
/// ── 失败与错误处理 ──────────────────────────────────────────
/// 所有步骤的失败都只记一行控制台日志并返回 `None`（模块头的「不阻断」原则）：
/// 旧文件读不出来、JSON 坏了、根不是对象、写入失败 —— 没有一种应该让网关
/// 起不来（各 store 此时还是旧文件形态，能照常工作）。整个导入在**一个事务**里
/// （`unchecked_transaction`，理由见模块头第三条注意事项），因此「导入到一半
/// 失败」不会留下半份数据 —— 表要么空、要么完整，下次点「升级」能干净地重试。
///
/// ── 幂等：为什么**不能**判「表非空」（T11 之后本项最需要看清的一处）──
/// 框架模块头的原则是「目标数据单元非空即跳过」，那在**迁移自动跑在启动期**
/// 时是对的（那时表只可能由迁移写）。但 T11 把迁移改成**用户在界面点「升级」**
/// 触发，于是本项跑在应用起来之后 —— 用户完全可能在那之前手动加过账号
/// （表里已有行），此时「表非空即跳过」会把本项判成「已迁过」，
/// 那份 `accounts.json` 里的历史账号**永远进不来**（旧文件也不改名，
/// 升级弹窗一直提示「仍有部分数据未能导入」）。`logs` 项已经线上复现过
/// 这个死循环，本项是同一类缺陷。
///
/// 所以改成**按主键幂等**：照常解析、照常写，但用 `INSERT OR IGNORE`
/// （见 [`write_accounts`]）—— 已存在的 id 不动，新的照常进。
/// 两种情形都对：
///   - **首次升级**：历史账号的 id 与用户手动加的（时间戳 uuid）不冲突，
///     全部导入；
///   - **重复点击**：旧文件的 id 已在库里 → 全部 IGNORE，不重复插。
///
/// 「已存在就不动」还有一条**语义**上的正确性：库里那条可能是用户升级后
/// 改过的（改名、调优先级、换代理），拿旧文件覆盖会把这些改动抹掉 ——
/// 忽略正是要保护它们。
pub(super) fn import_accounts(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = legacy_file(dir)?;

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 账号迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 账号迁移失败：{} 不是合法 JSON（{error}）", path.display()),
            );
            return None;
        }
    };
    // 根必须是对象（数组/标量都当损坏），与旧读取路径逐字同一口径
    if !value.is_object() {
        logging::console_line(
            "[Storage]",
            &format!("❌ 账号迁移失败：{} 的顶层不是对象", path.display()),
        );
        return None;
    }

    // 逐条过滤：与运行期读文件时的 `filter_map(from_value)` 同一判据
    // （缺 id、id 非字符串、id 空串的任何一种都跳过）。坏条目跳过、不让它
    // 毁掉整批 —— 与 `decode_data` 的口径一致。
    let records: Vec<StoredAccount> = value
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .cloned()
                .filter_map(StoredAccount::from_value)
                .collect()
        })
        .unwrap_or_default();
    let priority_scope = value
        .get("priorityScope")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let (imported, skipped) = match write_accounts(conn, &records, priority_scope.as_deref()) {
        Ok(result) => result,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 账号迁移失败：写入数据库失败（{error}）"),
            );
            return None;
        }
    };
    if skipped > 0 {
        logging::console_line(
            "[Storage]",
            &format!("账号迁移：{imported} 条新导入、{skipped} 条已存在（重复导入自动跳过）"),
        );
    }
    // 成功之后才备份（失败时文件原样留着，下次点「升级」能重试）。
    // 注意即使是**零条账号**的文件也走到这里：那说明这个文件本身是空的、
    // 没有可迁移的内容 —— 改名掉它同样是对的（否则每次启动都会重新读一遍
    // 这个空文件），而 `imported: 0` 会让启动日志如实说出"导入了 0 条"。
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        imported: records.len(),
        backup,
    })
}

// 这里原本有一个 `count_accounts`（「表非空即跳过」的幂等闸门）。**已删除**：
// T11 把迁移改成用户点「升级」触发之后，那个判据不成立了 —— 用户可能在点之前
// 手动加过账号，于是本项被永远判成「已迁过」，旧文件里的历史账号再也进不来
// （`logs` 项已线上复现同类死循环）。现在靠 `INSERT OR IGNORE` 按主键幂等，
// 不需要「表空不空」这个查询 —— 完整论证见 `import_accounts` 的幂等段。

/// 把一批账号 + `priorityScope` 写进库（**一个事务**）。
///
/// 为什么不用 `account_store::sql::save_state`：那是「整份状态按差异写」的
/// 通用实现，而它拿的是 `&mut Connection`（内部要开事务）。本函数手里只有
/// `&Connection`（迁移框架的签名，见模块头），所以用 `unchecked_transaction`
/// 自己开事务 —— 这正是模块头第三条注意事项说的那个情形。
/// 两处写入的 SQL 与 `save_state` 逐字一致（同一张表的同一组列）。
///
/// ── 为什么是 `INSERT OR IGNORE` 而不是纯 `INSERT` ─────────────
/// 幂等靠它（理由见 `import_accounts` 的幂等段）：用户点「升级」时库里可能
/// 已经有账号（他在这之前手动加过），那些 id 与旧文件不冲突时应当并存、
/// 冲突时应当**保留库里那份**（它可能带着用户升级后的改动 —— 改名、调优先级、
/// 换代理），所以既不能整批跳过、也不能覆盖。
///
/// 返回 `(新导入条数, 因已存在而跳过的条数)`：调用方用后者判断这次点击
/// 是否什么都没做，并进详细日志。
fn write_accounts(
    conn: &Connection,
    records: &[StoredAccount],
    priority_scope: Option<&str>,
) -> rusqlite::Result<(usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for record in records {
        // 投影列一律取访问器的值（口径见 `import_accounts` 的说明）：
        // provider 缺失/为空 → 默认 provider；priority 越界 → 夹回区间；
        // enabled 非 bool false → 1；added_at 取不到 → 0（对应 schema 的 DEFAULT）
        let changed = tx.execute(
            "INSERT OR IGNORE INTO accounts (id, provider, priority, enabled, added_at, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.id(),
                record.provider(),
                record.priority(),
                if record.enabled() { 1 } else { 0 },
                record.added_at(),
                record.to_value().to_string(),
            ],
        )?;
        if changed > 0 {
            imported += 1;
        } else {
            skipped += 1;
        }
    }
    // `priorityScope` 只有文件里**显式有**才写：没有它表示「尚未做全局化迁移」，
    // 而那条迁移（`migrate_startup`）正是靠「读不到这个键」来决定要不要整队。
    // 这里若无条件写一个默认值，那条迁移就永远不会触发 —— 用户的转发顺序
    // 会在升级后突变。
    //
    // 用 `DO NOTHING` 而不是 `DO UPDATE`（与改造前的 `DO UPDATE` 不同）：
    // 库里已经有这个键时，它可能是运行期写下的（`migrate_startup` 已跑过），
    // 拿旧文件的值去覆盖会把它改回去。旧文件的值只在**键不存在**时才有意义。
    if let Some(scope) = priority_scope {
        tx.execute(
            "INSERT INTO kv (key, value) VALUES ('priorityScope', ?1)
             ON CONFLICT(key) DO NOTHING",
            params![scope],
        )?;
    }
    tx.commit()?;
    Ok((imported, skipped))
}
