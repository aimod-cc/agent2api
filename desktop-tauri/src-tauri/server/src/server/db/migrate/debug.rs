//! `{debug_dir}/debug-traffic.jsonl` → `debug_traffic` 表。
//!
//! ── 旧文件在**自定义**目录时怎么办 ───────────────────────────
//! `config::storage_dirs().debug_dir` 是用户在设置页可能改过的位置，而配置目录
//! 是后备 —— `debugDir` 是后加的键，早期版本一直写在配置目录里。两个候选都试，
//! 优先用户设置的那个（与 `logs` / `requests` 两项的取舍逐字一致）。
//!
//! ── 解析口径 ────────────────────────────────────────────────
//! 逐行解析、空行跳过、坏行跳过、**不得 panic**。解析函数在
//! `core::debug_traffic::parse_legacy_jsonl` 而不是本文件：「一行 JSON 长什么样」
//! 是数据契约（`TrafficEntry` 的 serde 注解就是契约），与运行期必须共用同一份
//! 实现 —— 各写一份迟早分叉，分叉的后果是「同一条旧记录经迁移落库与经运行期
//! 写入得到不同字段」。
//!
//! ── 三个可空列的映射（本项最需要小心的地方）─────────────────
//! `TrafficEntry` 的 `status` / `response_headers` / `response_body` 是 `Option`，
//! 表达「这一侧没采到」：请求发不出去时没有响应侧。它们必须落成表的**可空列**
//! （NULL），**不能用空串代替** —— 前端详情弹窗依赖这个区分
//! （`ui/requests-panel.js` 的 `renderDetail`：`data.status == null` 显示 `-`，
//! `data.responseBody` 为空则不渲染那一块）。写入侧的口径由
//! `debug_traffic::sql::encode` 一处决定，本项与运行期共用它，映射不会分叉。
//!
//! ── 幂等 ────────────────────────────────────────────────────
//! 表非空即跳过（框架的原则），检查在**备份之前** —— 表非空时连旧文件都不动。
//!
//! ── 导入后为什么要裁一遍 ────────────────────────────────────
//! 旧实现只在**启动载入**时把内存裁到合规（条数、总量两道闸），盘上的文件
//! 下一次写入才收敛 —— 所以旧文件里留着超限行是常态。裁法与运行期同源：
//! `debug_traffic::import_legacy` 内部调同一个 `sql::enforce_limits`
//! （两道闸、同一顺序），口径不可能漂。
//!
//! ── `id` 重复会怎样 ─────────────────────────────────────────
//! `id` 是这张表的主键（`schema.rs` 的说明：它与 `requests.id` 同值、是跨表
//! 关联键，一条请求只有一份报文），而旧文件是**追加写**的 —— 同 id 两行在
//! 理论上可达（改造前 `get` 取最后那条，重复行不影响读取）。
//! 这里用**纯 INSERT**：撞主键就让整批回滚、失败项记日志返回 `None`。
//! 为什么不改成 UPSERT（像运行期 `record` 那样）：「后写覆盖先写」会让迁移
//! 悄悄丢掉一份报文，而且用户看不出是哪一条 —— 宁可不导入、把旧文件原样留着
//! （`backup_legacy_file` 在成功之后才调，失败时文件不动），下次启动或人工
//! 核对时还能看到它。这在真实数据里几乎不会发生（同 id 的两次写入意味着同一
//! 请求被采了两次，正常路径由采集器的 `done` 挡住）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::core::debug_traffic;
use crate::server::logging;

/// 报文旧文件名（`{debug_dir}/debug-traffic.jsonl`）。
///
/// 字面量在本文件里写一次而不是引 `debug_traffic` 的常量：那个常量随本次改造
/// 已经**不存在了**（报文进了数据库），而迁移项处理的正是它 ——
/// 名字是历史事实，不会随代码演进变化。
const DEBUG_FILE_NAME: &str = "debug-traffic.jsonl";

/// 迁移项的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一个字面量）。
pub(super) const LABEL: &str = "调试报文";

/// 旧文件的位置（不在原处时 `None`）—— 定位器与 [`import_debug`] 共用这一份。
///
/// ── 两个候选目录（与 `logs` / `requests` 两项的取舍逐字一致）─────────
/// `config::storage_dirs().debug_dir` 是用户在设置页可能改过的位置，而配置目录
/// 是后备 —— `debugDir` 是后加的键，早期版本一直写在配置目录里。两个候选都试，
/// 优先用户设置的那个。
/// 为什么需要后备：只看配置好的目录会漏掉那批用户的历史数据。
/// `storage_dirs()` 在快照缺失/写坏/锁中毒时自带「回落配置目录」的兜底。
pub(super) fn legacy_file(dir: &Path) -> Option<PathBuf> {
    let configured = crate::server::config::storage_dirs().debug_dir;
    let candidates: [PathBuf; 2] = [configured.join(DEBUG_FILE_NAME), dir.join(DEBUG_FILE_NAME)];
    candidates.into_iter().find(|item| item.is_file())
}

/// 旧报文 `{debug_dir}/debug-traffic.jsonl` → `debug_traffic` 表。
///
/// ── 解析口径 ────────────────────────────────────────────────
/// 逐行解析、空行跳过、坏行跳过、**不得 panic**。解析函数在
/// `core::debug_traffic::parse_legacy_jsonl` 而不是本文件：「一行 JSON 长什么样」
/// 是数据契约（`TrafficEntry` 的 serde 注解就是契约），与运行期必须共用同一份
/// 实现 —— 各写一份迟早分叉，分叉的后果是「同一条旧记录经迁移落库与经运行期
/// 写入得到不同字段」。
///
/// ── 三个可空列的映射（本项最需要小心的地方）─────────────────
/// `TrafficEntry` 的 `status` / `response_headers` / `response_body` 是 `Option`，
/// 表达「这一侧没采到」：请求发不出去时没有响应侧。它们必须落成表的**可空列**
/// （NULL），**不能用空串代替** —— 前端详情弹窗依赖这个区分
/// （`ui/requests-panel.js` 的 `renderDetail`：`data.status == null` 显示 `-`，
/// `data.responseBody` 为空则不渲染那一块）。写入侧的口径由
/// `debug_traffic::sql::encode` 一处决定，本项与运行期共用它，映射不会分叉。
///
/// ── 幂等 ────────────────────────────────────────────────────
/// 表非空即跳过（框架的原则），检查在**备份之前** —— 表非空时连旧文件都不动。
///
/// ── 导入后为什么要裁一遍 ────────────────────────────────────
/// 旧实现只在**启动载入**时把内存裁到合规（条数、总量两道闸），盘上的文件
/// 下一次写入才收敛 —— 所以旧文件里留着超限行是常态。裁法与运行期同源：
/// `debug_traffic::import_legacy` 内部调同一个 `sql::enforce_limits`
/// （两道闸、同一顺序），口径不可能漂。
///
/// ── `id` 重复会怎样 ─────────────────────────────────────────
/// `id` 是这张表的主键（`schema.rs` 的说明：它与 `requests.id` 同值、是跨表
/// 关联键，一条请求只有一份报文），而旧文件是**追加写**的 —— 同 id 两行在
/// 理论上可达（改造前 `get` 取最后那条，重复行不影响读取）。
/// 这里用**纯 INSERT**：撞主键就让整批回滚、失败项记日志返回 `None`。
/// 为什么不改成 UPSERT（像运行期 `record` 那样）：「后写覆盖先写」会让迁移
/// 悄悄丢掉一份报文，而且用户看不出是哪一条 —— 宁可不导入、把旧文件原样留着
/// （`backup_legacy_file` 在成功之后才调，失败时文件不动），下次启动或人工
/// 核对时还能看到它。这在真实数据里几乎不会发生（同 id 的两次写入意味着同一
/// 请求被采了两次，正常路径由采集器的 `done` 挡住）。
/// ── 幂等：为什么**不能**判「表非空」（T11 之后本项最容易搞错的一处）──
/// 框架模块头的原则是「目标数据单元非空即跳过」，那在**迁移自动跑在启动期**
/// 时是对的。但 T11 把迁移改成**用户在界面点「升级」**触发，于是本项跑在应用
/// 起来之后 —— 用户完全可能在那之前开着调试模式发过请求（表里已有报文），
/// 此时「表非空即跳过」会把本项判成「已迁过」，那份 `debug-traffic.jsonl`
/// 里的历史报文**永远进不来**（旧文件也不改名，升级弹窗一直提示「仍有部分
/// 数据未能导入」）。`logs` 项已经线上复现过这个死循环，本项是同一类缺陷。
///
/// 所以改成**按主键幂等**：`debug_traffic.id` 是主键（与 `requests.id` 同值、
/// 是跨表关联键），用 `INSERT OR IGNORE` —— 已存在的 id 不动，新的照常进。
/// 两种情形都对：首次升级时历史报文与运行期采集的 id 不冲突（前者来自旧文件、
/// 后者是新请求），全部导入；重复点击时全部 IGNORE，不重复插。
pub(super) fn import_debug(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = legacy_file(dir)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 调试报文迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    let entries = debug_traffic::parse_legacy_jsonl(&text);
    let (imported, skipped) = match debug_traffic::import_legacy(conn, &entries) {
        Ok(result) => result,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 调试报文迁移失败：写入数据库失败（{error}）"),
            );
            return None;
        }
    };
    if skipped > 0 {
        logging::console_line(
            "[Storage]",
            &format!("调试报文迁移：{imported} 条新导入、{skipped} 条已存在（重复导入自动跳过）"),
        );
    }
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        // 报的是**解析出的条数**（文件里有多少条就报多少条），不是落库后的行数
        // —— 导入时的两道闸会再删掉超限的行（旧文件里带这些是常态），
        // 而裁掉是**存储规则**在生效、不是迁移的动作，混进这个数字会让用户
        // 以为迁移丢数据（与 `logs` / `requests` 两项的口径一致）。
        imported: entries.len(),
        backup,
    })
}
