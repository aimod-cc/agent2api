//! `{request_stats_dir}/requests.jsonl` + `request-daily.jsonl` → `requests` /
//! `request_daily` 两张表。
//!
//! 两个旧文件拆成**两个迁移项**（而不是一项里处理两个文件），理由有三条：
//!   1. **失败面独立**：明细动辄几十 MB、聚合只有几十 KB，读大文件的失败
//!      （超时、被占用）不该让聚合那份也一起跳过一次启动；
//!   2. **幂等闸门各自独立**：两张表各有各的「空不空」，合成一项就得自己
//!      维护两套判断与两条备份，等于把框架已经给的东西在项内重写一遍；
//!   3. **有真实依赖**：聚合项要在导入后做一次口径回填（读 `requests` 表），
//!      所以它必须排在明细项之后 —— 拆成两项时这条依赖写在注册表的注释里
//!      一眼可见，合成一项就藏在函数体内部了。
//!
//! ── 旧文件在**自定义**目录时怎么办（与 `logs` 项同一问题）────────
//! `config::storage_dirs().request_stats_dir` 是用户在设置页可能改过的位置，
//! 而配置目录是后备 —— `requestStatsDir` 是后加的键，早期版本一直写在配置
//! 目录里。两个候选都试，优先用户设置的那个（与 `logs` 项的取舍逐字一致）。
//!
//! ── 解析口径 ────────────────────────────────────────────────
//! 逐行解析、坏行跳过、字段缺失走 serde 的 `default`（`attempts` 按 1、
//! token 按 0、`id` 空串…）。这两个解析函数**不在这里**，而在
//! `request_stats` 模块里（`parse_requests_jsonl` / `parse_daily_jsonl`）：
//! 「一行 JSON 长什么样」是数据契约的一部分（`record.rs` 的 serde 注解就是
//! 契约），与运行期的读取口径**必须**共用同一份实现 —— 各写一份迟早分叉，
//! 而分叉的后果是「同一条旧记录经迁移落库与经运行期写入得到不同字段」。
//! 这里只负责「文件在哪、什么时候导、导完备份谁」。
//!
//! ── 幂等 ────────────────────────────────────────────────────
//! 各自判自己的表非空即跳过（模块头的原则），检查在**备份之前** ——
//! 表非空时连旧文件都不动，用户仍能在目录里看到它。
//!
//! ── 导入后为什么要裁一遍 ────────────────────────────────────
//! 旧实现只在**启动载入**时把内存裁到合规（明细按保留期与容量、聚合按保留期
//! 与天数上限），盘上的文件下一次写入才收敛 —— 所以旧文件里留着超期行、
//! 超限行是常态。裁法与运行期同源：复用 `RequestStats::legacy_bounds()` 算边界
//! （它内部走 `RetentionBounds::of`，与运行期是同一段代码）、
//! 复用同一个 `import_legacy_*` 写库函数（内部调 `sql::` 的同一组删除语句）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::logging;
use crate::server::request_stats::{self, RequestEntry};

/// 请求明细旧文件名（`{request_stats_dir}/requests.jsonl`）。
///
/// 字面量在本文件里写一次而不是引 `request_stats` 的常量：那些常量随本次改造
/// 已经**不存在了**（统计进了数据库），而迁移项处理的正是它们 ——
/// 名字是历史事实，不会随代码演进变化。
const REQUESTS_FILE_NAME: &str = "requests.jsonl";
/// 按天聚合旧文件名（`{request_stats_dir}/request-daily.jsonl`）
const DAILY_FILE_NAME: &str = "request-daily.jsonl";

/// 两项迁移的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一批字面量）。
///
/// 两个旧文件对应注册表里的**两个独立项**（理由见模块头），所以有两个名字，
/// 它们各自与自己的定位器配对。
pub(super) const LABEL: &str = "请求明细";
pub(super) const DAILY_LABEL: &str = "请求聚合";

/// 找旧文件：`storage_dirs()` 给出的目录优先（那是用户设置的位置），
/// 配置目录作后备。
///
/// 为什么需要后备：`requestStatsDir` 是**后来**才加的可配置项，早期版本的明细
/// 一直在配置目录里；只看配置好的目录会漏掉那批用户的历史数据。
/// `storage_dirs()` 在快照缺失/写坏/锁中毒时自带「回落配置目录」的兜底，
/// 所以拿不到自定义目录也不会走空（退化成只看配置目录）。
///
/// 本函数同时是两项的**定位器**（注册表 `locate` 与各自的导入函数共用）：
/// 界面「还有没有待迁移数据」的判定与真正导入找的是同一批路径，
/// 不会出现「界面说有、点升级却找不到文件」。
fn locate(dir: &Path, name: &str) -> Option<PathBuf> {
    let configured = crate::server::config::storage_dirs().request_stats_dir;
    let candidates: [PathBuf; 2] = [configured.join(name), dir.join(name)];
    candidates.into_iter().find(|item| item.is_file())
}

/// 明细旧文件的位置（注册表定位器）
pub(super) fn requests_file(dir: &Path) -> Option<PathBuf> {
    locate(dir, REQUESTS_FILE_NAME)
}

/// 聚合旧文件的位置（注册表定位器）
pub(super) fn daily_file(dir: &Path) -> Option<PathBuf> {
    locate(dir, DAILY_FILE_NAME)
}

/// 读旧文件并解析成明细列表。
///
/// 读失败与解析失败都只记一行控制台日志并返回 `None` —— 与 `logs` 项同一口径
/// （模块头的「不阻断」原则）。解析本身「跳过坏行」而不是整批失败：
/// 几十 MB 的文件里有一行损坏是可达的，丢掉它比让用户整批历史都进不来好。
fn read_requests(path: &Path) -> Option<Vec<RequestEntry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 请求明细迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    Some(request_stats::legacy::parse_requests_jsonl(&text))
}

/// 旧明细 `{request_stats_dir}/requests.jsonl` → `requests` 表。
///
/// ── `id` 可以重复，不需要改派 ────────────────────────────────
/// 与事件日志那一项**关键的区别**：`logs.id` 是主键，所以那边要对文件里重复的
/// id 做改派（见 `logs` 项的说明）。`requests.id` **不是主键** ——
/// 主键是自增的 `row_id`（物理行号），`id` 只是请求的关联键（与调试报文同值），
/// 旧数据里可能是空串、且**不保证唯一**（重试、去重排队的若干路径会产生同 id
/// 的记账，见 `db/schema.rs` 的 DDL 注释）。所以这里原样照抄 `id`，
/// 重复多少条都合法；`row_id` 由库自增，导入后仍是稳定顺序。
/// ── 幂等：为什么**不能**判「表非空」（T11 之后本项最容易搞错的一处）──
/// 框架模块头的原则是「目标数据单元非空即跳过」，那在**迁移自动跑在启动期**
/// 时是对的。但 T11 把迁移改成**用户在界面点「升级」**触发，于是本项跑在应用
/// 起来之后 —— 用户完全可能在那之前已经用网关转发过请求（明细表里已有行），
/// 此时「表非空即跳过」会把本项判成「已迁过」，那份 `requests.jsonl` 里的
/// 历史明细**永远进不来**（旧文件也不改名，升级弹窗一直提示「仍有部分数据
/// 未能导入」）。`logs` 项已经线上复现过这个死循环，本项是同一类缺陷。
///
/// 所以改成**标记键 + 同事务**（`request_stats::legacy::MARKER_REQUESTS`，
/// 论证见那个常量的说明）：明细表没有可用的业务唯一键来做 `INSERT OR IGNORE`
/// （主键是自增 `row_id`，而 `id` 列是请求关联 id、旧行可能是空串）。
pub(super) fn import_requests(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = requests_file(dir)?;
    match request_stats::legacy::marker_present(conn, request_stats::legacy::MARKER_REQUESTS) {
        Ok(true) => return None,
        Ok(false) => {}
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 请求明细迁移失败：读取迁移标记失败（{error}）"),
            );
            return None;
        }
    }
    let entries = read_requests(&path)?;
    let bounds = request_stats::legacy::legacy_bounds();
    if let Err(error) = request_stats::legacy::import_legacy_requests(conn, &entries, &bounds) {
        logging::console_line(
            "[Storage]",
            &format!("❌ 请求明细迁移失败：写入数据库失败（{error}）"),
        );
        return None;
    }
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        // 报的是**解析出的条数**（文件里有多少条就报多少条），不是落库后的行数 ——
        // 导入时的时间/容量两道裁剪会再删掉超期与超限的行（旧文件里带这些是常态），
        // 而裁剪是**存储规则**在生效、不是迁移的动作，混进这个数字会让用户以为迁移丢数据。
        imported: entries.len(),
        backup,
    })
}

/// 旧聚合 `{request_stats_dir}/request-daily.jsonl` → `request_daily` 表，
/// 并在导入后做一次**口径回填**（把缺账号维度的历史日子用明细重算）。
///
/// ── 为什么回填在这里（差异 6 的判断）──────────────────────────
/// 「聚合行缺 `accountStats`」只可能来自**旧文件**：现在写出的每一行都由
/// `fold_into_daily` 逐条累加，它恒为三个维度建组（身份是空串也建），
/// 所以库里的行不会再出现「缺一维」这个状态。也就是说回填是**一次性的升级
/// 动作**，只对「刚从旧文件导进来的那批聚合行」有意义 —— 归迁移所有最合适，
/// 运行期不再需要（旧实现的 `load()` 里那次已随 T4 消失）。
///
/// 留在运行期的代价：每次启动都要把聚合整表读出来、再判一遍「有没有候选日」，
/// 而这个判断在升级之后的每一次启动里都必然是「没有」。而**能力不能丢**：
/// 升级用户的报表不允许出现「未知账号」的巨大块，所以回填不是被删掉，
/// 而是换了调用时机（见 `request_stats::legacy::import_legacy_daily` 的说明）。
/// 它内部的「拿不准就不动」判据（明细条数 >= 聚合行 requests 才重算）原样保留。
///
/// 回填需要明细已入库，所以本项必须排在 `import_requests` 之后（注册表注释）。
pub(super) fn import_daily(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = daily_file(dir)?;
    // 幂等判据与明细项同构（标记键 + 同事务，见 `import_requests` 的幂等段）
    match request_stats::legacy::marker_present(conn, request_stats::legacy::MARKER_DAILY) {
        Ok(true) => return None,
        Ok(false) => {}
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 请求聚合迁移失败：读取迁移标记失败（{error}）"),
            );
            return None;
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 请求聚合迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    let daily = request_stats::legacy::parse_daily_jsonl(&text);
    let bounds = request_stats::legacy::legacy_bounds();
    let (imported, backfilled) = match request_stats::legacy::import_legacy_daily(conn, &daily, &bounds) {
        Ok(result) => result,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 请求聚合迁移失败：写入数据库失败（{error}）"),
            );
            return None;
        }
    };
    // 回填过的日子逐日列出：这几天的数字与用户昨天看到的可能不同（重算用的是
    // 今天的口径，见 `backfill` 模块头的代价分析），出问题时日志里要有线索。
    // 走控制台通道 —— 迁移发生在 `logging::init_store` 之前（见框架模块头）。
    if !backfilled.is_empty() {
        logging::console_line("[Storage]", &request_stats::legacy::backfill_report_line(&backfilled));
    }
    let backup = backup_legacy_file(&path);
    Some(LegacyOutcome {
        label: DAILY_LABEL,
        source: path,
        // 报解析出的天数（同明细项的理由：导入后的裁剪不算迁移丢数据）
        imported,
        backup,
    })
}
