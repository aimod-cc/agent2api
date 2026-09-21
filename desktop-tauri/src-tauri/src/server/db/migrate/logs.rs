//! `{log_dir}/logs.jsonl` → `logs` 表。
//!
//! 迁移项的实现细节（自定义目录、解析口径、导入后裁剪、id 改派）都在下面各
//! 函数的注释里；框架侧的原则见 [`super`] 的模块头。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use super::backup::backup_legacy_file;
use super::LegacyOutcome;
use crate::server::logs_store::{self, LogEntry};
use crate::server::logging;

/// 日志旧文件名（`{log_dir}/logs.jsonl`）。
///
/// 同 `ACCOUNTS_FILE_NAME`：`logs_store` 改造后不再有这个常量（日志进了数据库），
/// 而迁移项处理的正是它 —— 名字是历史事实，由迁移项自己持有。
const LOGS_FILE_NAME: &str = "logs.jsonl";

/// 迁移项的可读名（`LegacyOutcome.label` 与「待迁移项清单」共用同一个字面量）。
pub(super) const LABEL: &str = "事件日志";

/// 旧文件的位置（不在原处时 `None`）—— 定位器与 [`import_logs`] 共用这一份。
///
/// ── 旧文件在**自定义** log_dir 时怎么办（本项最需要解释的地方）──
/// 迁移框架的签名只给 `dir: &Path`（配置目录），而日志目录是用户在设置页可以
/// 改的（存在 config.json 的 `logDir`，由 `config::storage_dirs()` 解析）。
/// 两条路：
///   1. 让 `run_legacy_migrations` 也接收 `storage_dirs()` —— 要改框架签名、
///      改调用点（`ServerState::bootstrap`），而且**多传一个大多数迁移项用不上
///      的参数**（账号、请求统计、调试报文这三个的旧文件都在各自的目录里，
///      将来也一样要自己解析）；
///   2. **迁移项自己调 `config::storage_dirs()`** —— 框架签名不变。
/// 取 2。两个前提在这里都成立：
///   - `config::init()` 已经在 `Db::open` **之前**跑过（`bootstrap` 的顺序），
///     所以 `storage_dirs()` 读到的内存快照已经含用户的 `logDir`；
///   - 该函数在快照缺失/写坏/锁中毒时一律回落配置目录（见它的实现），
///     所以拿不到自定义目录也不会走空 —— 退化成方案 1 的默认行为。
/// 代价：`db/migrate` 里出现了一处对 `config` 的依赖，而它本来是纯存储层。
/// 这个代价是可接受的 —— 「旧文件在哪」本来就是**迁移项**的知识，不是框架的；
/// `requests` / `debug` 两项后来也按同一口径各自解析目录（见那两项的
/// `locate` / `legacy_file`），三处的写法与取舍逐字一致。
///
/// ── 两处都试 ────────────────────────────────────────────────
/// `storage_dirs()` 给出的目录优先（那是用户设置的位置），配置目录作后备 ——
/// 老版本的日志可能就落在那里，而用户后来改过设置。为什么需要后备：`logDir`
/// 是**后来**才加的可配置项，早期版本的日志一直在配置目录里；只看 `log_dir`
/// 会漏掉那批用户的历史日志。
///
/// 这个候选顺序**同时服务两件事**：导入时找文件，以及界面问「还有没有待迁移
/// 数据」。两者共用本函数，所以不会出现「界面说有、点升级却找不到文件」。
pub(super) fn legacy_file(dir: &Path) -> Option<PathBuf> {
    let log_dir = crate::server::config::storage_dirs().log_dir;
    let candidates: [PathBuf; 2] = [log_dir.join(LOGS_FILE_NAME), dir.join(LOGS_FILE_NAME)];
    candidates.into_iter().find(|item| item.is_file())
}

/// 旧文件 `{log_dir}/logs.jsonl` → `logs` 表。
///
/// ── 幂等：为什么**不能**判「表非空」（本项最容易搞错的一处）────────
/// 框架模块头的原则是「目标数据单元非空即跳过」，但对本项**不成立**：
/// `logs` 表不是「只有迁移才写」的表 —— **运行期一直在往里写**。应用启动后
/// 几毫秒内就有十几条启动日志入库（`[Server] …启动中`、`[Config] API 端口`
/// 等），而迁移现在由用户在界面点「升级」触发（T11 改的流程），那时表里
/// **必然已经有行**。于是「表非空即跳过」会把本项判成「已迁过」而静默跳过：
/// 旧文件没被改名 → `has_pending` 一直为 true → 弹窗永远提示「仍有部分数据
/// 未能导入」，但每次重试都跳过，用户看到的是一个**永远修不好的死循环**
/// （线上实测复现：7 项导入成功、唯独缺「事件日志」）。
///
/// 所以本项改成**按主键幂等**：照常解析旧文件、照常逐条 INSERT，但用
/// `INSERT OR IGNORE` —— 已存在的 id 不动、不报错，新的照常进。
/// 这样两种情形都对：
///   - **首次升级**：库里只有启动日志（id 从 1 起的小号），旧文件的 id 是
///     历史高位（实测 4297..4856），互不冲突，历史日志全部导入；
///   - **重复点击**：旧文件的 id 已在库里 → 全部 IGNORE，一条不重复插，
///     结果与「跳过」等价，但**不会漏**（这正是原判据做不到的）。
///
/// 为什么不用标记键（`kv.logsMigrated` 之类）解决：标记与数据可能不一致
/// （标记写了但写入只落一半就被中断，那时必须重跑而标记会阻止重跑），
/// 而「主键已存在」是数据自己的事实，不会说谎 —— 与框架模块头论证
/// `desensitize` 那条时同一取向。
///
/// ── 解析口径与运行期**逐字一致** ─────────────────────────────
/// 逐行解析、损坏行跳过；`message` 缺失按空串、`level`/`category` 走
/// `logs_store` 的归一函数、`data` 走 `normalize_data` 的裁剪、`id` 缺失时
/// 按**行号**兜底（旧 `load()` 就是 `parsed.len() as u64 + 1`）。
/// 这几个函数因此从 `logs_store` 的私有项提升为 `pub(crate)` 并共用它 ——
/// 各写一份迟早分叉，而分叉的后果是「同一条日志经迁移落库与经运行期写入
/// 得到不同的 level/category」。
///
/// ── 导入后为什么要裁一遍 ────────────────────────────────────
/// 旧文件里可能含着已经超出保留期、或超出 `MAX_ENTRIES` 的行（旧实现是靠
/// 启动时的 `load()` 把内存裁到合规，文件下一次写入才收敛 —— 所以**盘上
/// 留着超期行是常态**）。直接整批搬进来会让迁移后的库里立刻超限。
/// 裁法与运行期同源：先按时间（`logs_store::LogStore::retention_cutoff_ms`
/// 的口径，这里直接复用同一个函数，见下），再按容量。
///
/// ── 失败与错误处理 ──────────────────────────────────────────
/// 与 `import_accounts` 同构：所有失败只记一行控制台日志并返回 `None`
/// （模块头的「不阻断」原则）。整批在**一个事务**里 —— 中断不会留下半份数据，
/// 下次点「升级」能干净地重试。
pub(super) fn import_logs(conn: &Connection, dir: &Path) -> Option<LegacyOutcome> {
    let path = legacy_file(dir)?;

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 日志迁移失败：无法读取 {}（{error}）", path.display()),
            );
            return None;
        }
    };
    let entries = parse_jsonl(&text);
    let (imported, skipped) = match write_logs(conn, &entries) {
        Ok(result) => result,
        Err(error) => {
            logging::console_line(
                "[Storage]",
                &format!("❌ 日志迁移失败：写入数据库失败（{error}）"),
            );
            return None;
        }
    };
    let backup = backup_legacy_file(&path);
    // 跳过数进详细日志：重复点击「升级」时它是常态（全部 IGNORE），
    // 而「导入 0 条、跳过 560 条」正是「这次点击什么都没做」的准确表述 ——
    // 排障时比只看 `imported` 有用（`imported` 报的是文件里的条数，
    // 两次点击它都一样）。
    if skipped > 0 {
        logging::console_line(
            "[Storage]",
            &format!(
                "事件日志迁移：{imported} 条新导入、{skipped} 条已存在（重复导入自动跳过）"
            ),
        );
    }
    Some(LegacyOutcome {
        label: LABEL,
        source: path,
        // `imported` 报的是**解析出的条数**（旧文件里有多少条就报多少条），
        // 不是落库后的行数 —— 导入时的时间/容量两道裁剪会再删掉一些超期与
        // 超限的行（旧文件里带这些是常态，见 `write_logs` 的说明）。
        // 与 `import_accounts` 的口径一致（那边报的是文件里的账号数）：
        // 启动日志要说清「从旧文件搬了多少条」，而裁剪是**存储规则**在生效、
        // 不是迁移的动作，混进这个数字里会让用户以为迁移丢数据。
        imported: entries.len(),
        backup,
    })
}

/// 逐行解析旧 JSONL，返回归一后的条目（**与旧 `LogStore::load` 的口径逐字一致**）。
///
/// 三个「容错但不猜」的处理：空行跳过；JSON 解析失败或根不是对象跳过；
/// 各字段缺失时按旧实现的默认值（`message` 空串、`level` info、`category`
/// server、`ts` 0、`id` 按行号兜底）。
///
/// **注意这里（有意）不做 id 去重**：旧 `load()` 的 `Vec` 允许 id 重复，
/// 这里照抄它的口径；「重复 id 落到主键上会撞」这件事由 [`write_logs`] 在
/// 写库时改派解决（那边的说明写了为什么必须在那里处理、以及重复是怎么来的）。
/// 把去重挪到这里会让「解析」与「落库」两件事的口径分家 —— 解析就是要
/// 如实还原文件内容。
fn parse_jsonl(text: &str) -> Vec<LogEntry> {
    let mut parsed: Vec<LogEntry> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let message = object
            .get("message")
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .unwrap_or_default();
        let level = object.get("level").and_then(serde_json::Value::as_str).unwrap_or("info");
        let category = object
            .get("category")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("server");
        parsed.push(LogEntry {
            // id 缺失时按**行号**兜底 —— 旧 `load()` 就是
            // `parsed.len() as u64 + 1`（注意是「已收下的条数」而不是原始行号：
            // 坏行跳过后序号会往前挤，与旧实现一致）
            id: object
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(parsed.len() as u64 + 1),
            ts: object.get("ts").and_then(serde_json::Value::as_i64).unwrap_or(0),
            level: logs_store::normalize_level(level),
            category: logs_store::normalize_category(category),
            message: logs_store::clamp_message(&message),
            data: logs_store::normalize_data(object.get("data")),
        });
    }
    parsed
}

/// 把解析出的条目写进 `logs` 表（**一个事务**，含导入后的两道裁剪）。
///
/// 顺序：插入全部 → 按时间裁 → 按容量裁 → 把 `kv` 的高水位抬到
/// `MAX(id) + 1`。最后一步是**必须的**：运行期的 `append` 靠这个高水位分配 id，
/// 若它留在 1，迁移进来的历史行（id 可能到几百）会被新日志撞主键 ——
/// 那条新日志就静默丢了（`INSERT` 失败 → 整个事务回滚 → `append` 返回 None）。
///
/// ── id 为什么要「落地时体检」而不是直接照抄文件里的值 ────────
/// 旧 `load()` 把条目放进 `Vec`，id 只是元素上的一个字段 —— 重复、越界、
/// 为 0 都不影响它能读起来。库这边 `id` 是**主键**，照抄的后果不是「那一条
/// 有点怪」而是**整批导入失败**：`INSERT` 撞 UNIQUE → 事务回滚 → 用户的全部
/// 历史日志一条都进不来（探针实测到这条路径）。而旧文件里 id 重复是**可达**的：
/// `parse_jsonl` 的 id 兜底是「已收下的条数 + 1」，于是两段各自从 1 数起的
/// 日志被拼进同一个文件（手工拼接、或从别的机器拷一份追加进来）就会重叠。
/// 所以这里逐条体检，不合格的改派到**当前最大有效 id 之上**（`assign_log_id`）：
/// 不丢数据（旧实现两行都在，这里也两行都在），也不与历史 id 冲突。
///
/// ── 重复点击「升级」时靠什么不重复插（`INSERT OR IGNORE`）────────
/// 见 `import_logs` 的幂等段：本项**不能**判「表非空即跳过」（运行期一直在写
/// 这张表，判了就会永远跳过、旧文件永不改名、弹窗永远提示「仍有部分数据未能
/// 导入」）。改为按主键幂等 —— 写入用 `INSERT OR IGNORE`，已存在的 id 不动。
///
/// 由此推出一条**必须**的配套：`used` 集合要**预填库里已有的 id**。
/// 不预填的后果很具体：重复点击时 `assign_log_id` 看到的是「空集合」，
/// 于是把每条旧记录都判成「id 未占用」照抄 → 若这些 id 库里已存在，
/// `OR IGNORE` 会正确地忽略它们（这条没事）；但**文件内 id 重复**的那几条
/// 会被改派到 `max_valid + 1` 起的高位 —— 那个位置库里可能是空的，
/// 于是**重复内容真的插进去了**（每次点击都再插一遍，越点越多）。
/// 预填之后「库里已有的 id」也参与冲突判定，重复点击时全部落到 IGNORE 分支，
/// 一条都不重复。
///
/// 返回 `(导入条数, 因已存在而跳过的条数)`：后者供调用方判断
/// 「这次点击其实什么都没做」（重复点击的常态），进详细日志便于排障。
fn write_logs(conn: &Connection, entries: &[LogEntry]) -> rusqlite::Result<(usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    // 已用 id 集合 + 改派游标。游标从「最大有效 id + 1」起步，所以改派出来的
    // id 一定大于文件里出现过的任何一个合法 id —— 与旧实现「后写入的 id 更大」
    // 的取向一致（前端把 id 当已读水位，改派到高位不会让新日志被误判为已读）。
    let mut used: HashSet<i64> = HashSet::new();
    // 预填库里已有的 id（理由见函数说明「重复点击」一段）
    {
        let mut stmt = tx.prepare("SELECT id FROM logs")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            used.insert(row.get::<_, i64>(0)?);
        }
    }
    let max_valid = entries
        .iter()
        .map(|item| item.id)
        .filter(|id| *id > 0 && *id <= i64::MAX as u64)
        .max()
        .unwrap_or(0);
    let mut next_free = max_valid.saturating_add(1).min(i64::MAX as u64);
    // 库里已有的 id 也算「占位」，改派游标不能落在它们上面
    //（否则改派出来的 id 撞主键，那条记录会被 IGNORE 掉 —— 静默丢一条）
    while used.contains(&(next_free as i64)) && next_free < i64::MAX as u64 {
        next_free += 1;
    }
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for entry in entries {
        let id = assign_log_id(entry.id, &mut used, &mut next_free)?;
        // `data` 与运行期同一编码（JSON 文本 / NULL）。
        // `OR IGNORE`：id 已存在就跳过（重复点击的常态），不报错、不覆盖
        // —— 覆盖会丢掉库里那条（它可能带着运行期补充的信息），
        // 而两份内容本就同源，忽略是唯一安全的选择。
        let changed = tx.execute(
            "INSERT OR IGNORE INTO logs (id, ts, level, category, message, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                entry.ts,
                entry.level,
                entry.category,
                entry.message,
                entry.data.as_ref().map(|item| item.to_string()),
            ],
        )?;
        if changed > 0 {
            imported += 1;
        } else {
            skipped += 1;
        }
    }
    // 导入后收敛到合规（旧文件里带超期行/超限行是常态，见 `import_logs` 的说明）。
    // 时间口径复用运行期那个函数：`logs_store` 的 cutoff 靠保留天数回调，
    // 而这里没有 `LogStore` 实例 —— 所以直接调 `config::retention_settings()`
    // 按**同一段逻辑**算一遍。两处必须同步：这里若漏了夹紧（`clamp`），
    // 配置里写个天文数字就会让裁剪空转（那正是 `RETENTION_*_DAYS` 要防的）。
    let settings = crate::server::config::retention_settings();
    let days = settings
        .log_days
        .clamp(
            crate::server::config::RETENTION_MIN_DAYS,
            crate::server::config::RETENTION_MAX_DAYS,
        );
    let day = chrono::Local::now().date_naive() - chrono::Duration::days(days - 1);
    let cutoff = day
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| chrono::TimeZone::from_local_datetime(&chrono::Local, &naive).earliest())
        .map(|value| chrono::DateTime::timestamp_millis(&value))
        .unwrap_or(0);
    tx.execute("DELETE FROM logs WHERE ts < ?1", params![cutoff])?;
    tx.execute(
        "DELETE FROM logs WHERE id NOT IN (SELECT id FROM logs ORDER BY id DESC LIMIT ?1)",
        params![logs_store::MAX_ENTRIES as i64],
    )?;
    // 高水位抬到 MAX(id) + 1（理由见函数说明）。`raise_next_id` 只在更高时才写，
    // 所以这里给它「下一个可用 id」而不是「当前最大 id」。
    let max: Option<i64> = tx.query_row("SELECT MAX(id) FROM logs", [], |row| row.get(0))?;
    let next = max.unwrap_or(0).max(0) as u64 + 1;
    logs_store::sql_raise_next_id(&tx, next)?;
    tx.commit()?;
    Ok((imported, skipped))
}

/// 给一条日志定一个**库内可用**的 id：合法且未占用就照用，否则改派到空位。
///
/// 判「合法」的三条（对应 `decode_row` 能把值还原成 `u64` 的范围）：
/// `id > 0`（旧实现的 id 从 1 起，0 是「没写过」的哨兵值）、
/// `id <= i64::MAX`（SQLite 的 INTEGER 是有符号 64 位，超了存不进去）、
/// 且不在 `used` 里（主键冲突会让整批导入失败，见 `write_logs` 的说明）。
///
/// ── 改派为什么分两段（`next_free` 用尽后回头找洞）─────────────
/// 主路径是在 `next_free`（= 文件里最大有效 id + 1）之上**继续往后发**，
/// 于是改派出来的 id 比文件里的任何合法 id 都大，保持「后写入的 id 更大」
/// 这个前端依赖的取向。但游标可能一路撞到 `i64::MAX`（文件里真有一个
/// i64::MAX 的量级、或一堆同样巨大的 id）：那时若直接报错，用户的一整批
/// 历史日志就一条都进不来 —— 而表里其实还空着大片低位。所以第二段退回去
/// 从 1 起**找最低的洞**：此时放弃「改派的 id 更大」这条（它只是取向，
/// 不是契约），换来的是「不丢数据」—— 这更重要。
///
/// 只有两段都填满（即 id 空间真的一个空位都没有，不可能在真实文件里发生）
/// 才报错让整个事务回滚：宁可不导入，也不要写进去一批 id 相撞的行。
/// 用 `ToSqlConversionFailure` 而不是 `InvalidQuery`：后者在 rusqlite 里的
/// 语义是「这条 SQL 不是只读的 SELECT」，拿去描述「改派不出可用 id」会让人
/// 往 SQL 语句上查（错误文本会进控制台的迁移失败日志）。
fn assign_log_id(
    wanted: u64,
    used: &mut HashSet<i64>,
    next_free: &mut u64,
) -> rusqlite::Result<i64> {
    let usable = wanted > 0 && wanted <= i64::MAX as u64 && !used.contains(&(wanted as i64));
    let id = if usable {
        wanted as i64
    } else {
        match find_free_id(used, *next_free) {
            Some(candidate) => {
                *next_free = candidate as u64 + 1;
                candidate
            }
            // 高位段用尽 → 回头从 1 起找最低的洞（见上文的取舍）
            None => match find_free_id(used, 1) {
                Some(candidate) => {
                    *next_free = candidate as u64 + 1;
                    candidate
                }
                None => {
                    return Err(rusqlite::Error::ToSqlConversionFailure(
                        "旧日志文件里的 id 已占满可用范围，无法为冲突条目改派 id".into(),
                    ))
                }
            },
        }
    };
    used.insert(id);
    Ok(id)
}

/// 从 `start` 起找第一个不在 `used` 里的正整数 id（`None` = 从这个位置到
/// `i64::MAX` 都被占了）。逐号试探在真实数据上最多走几步 —— `used` 是
/// 文件里的行数（上限几百），空位遍地都是。
fn find_free_id(used: &HashSet<i64>, start: u64) -> Option<i64> {
    let mut candidate = start.max(1);
    while candidate <= i64::MAX as u64 {
        let value = candidate as i64;
        if !used.contains(&value) {
            return Some(value);
        }
        candidate += 1;
    }
    None
}

// 这里原本有一个 `count_logs`（「表非空即跳过」的幂等闸门）。**已删除**：
// 那个判据对 `logs` 表不成立 —— 运行期一直在往里写（启动日志），
// 于是它会把本项永远判成「已迁过」、旧文件永不改名、升级弹窗永远提示
// 「仍有部分数据未能导入」（线上实测复现的缺陷）。现在靠 `INSERT OR IGNORE`
// 按主键幂等，不需要「表空不空」这个查询 —— 完整论证见 `import_logs` 的幂等段。
