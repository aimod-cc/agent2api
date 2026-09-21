//! 定时任务注册表：设置页「定时任务」页的数据源 + 后端调度循环。
//!
//! ── 为什么要有这个模块 ──────────────────────────────────────
//! 改造前，几个「按固定间隔重复跑」的后台动作散落在各自的模块与调用点里，
//! 间隔全是硬编码常量：凭证维护 10 分钟（`credential_maintenance::
//! REFRESH_INTERVAL_SECS`）、模型目录刷新只在启动与客户端拉 `/v1/models` 时被动
//! 触发、两个前端面板各自 10 秒轮询。用户既看不到它们的存在，也改不了节奏。
//! 本模块把「有哪些周期任务、各自开着没、间隔多久、上次结果如何」收成一份清单，
//! 由 `/api/scheduled-tasks*` 三条路由读改，循环只负责按清单干活。
//!
//! ── 两类任务（区别是**谁来执行**，不是可配性）─────────────────
//!   - `Runner::Backend`：凭证自动维护、定时查询积分、模型目录刷新、软件版本检查。
//!     后端循环执行，因此有「上次执行 / 下次执行 / 立即执行」这些运行状态。
//!   - `Runner::Frontend`：日志页与请求日志页的自动刷新。定时器天然长在页面上
//!     （只在页面可见时该走），后端只存开关与间隔，界面自己读；
//!     因此它们没有运行状态，也不提供「立即执行」。
//!
//! ── 自动签到为什么不在这个清单里 ─────────────────────────────
//! 它是**每天定点**型（可指定 00:01 这类时刻），与本模块的「等间隔重复」不是
//! 同一个形状：时刻（JSON 字符串）、当天去重（`lastFiredDate`）、启动补签都是
//! 它独有的语义。硬塞进 `{enabled, interval}` 会逼着两边都变形。
//! 界面上它仍在同一页展示（「定时任务」页），只是读改走 `/api/auto-checkin`。
//!
//! ── 调度方式：轮询判定，不是「睡满间隔」────────────────────────
//! 与 `auto_checkin` 同一取舍：循环每 `TICK_MS` 醒一次，每次从**墙上时钟**重算
//! 「到点了吗、现在开着吗、间隔改成多少了」，而不是 `sleep(interval)` 一觉睡到
//! 底。两个后果都是想要的：
//!   - **改完设置下一轮就生效**（最多慢一个 tick）：睡满间隔的写法在把间隔从
//!     60 分钟改到 1 分钟时，最坏要等 60 分钟才醒来 —— 界面显示「已生效」而实际
//!     没有，正是最难排查的那种不一致；
//!   - **机器休眠 / 锁屏 / 改时钟后自愈**：单调时钟在休眠期间不推进，
//!     醒来的第一次 tick 比一下墙上时钟就把错过的时点补上。
//! 每次 tick 的代价是两次读锁（配置快照 + 运行状态），相对这几分钟级的间隔可忽略。
//!
//! ── 运行状态不落盘（有意的）─────────────────────────────────
//! `lastRunAt` / `lastResult` / `nextRunAt` 只在内存里，重启即空。理由：
//! 它们是**本次运行的观察值**（凭证维护的「上次刷新了 2 个账号」跨进程没有
//! 延续的意义），而落盘还会带来一个更糟的后果 —— 界面上显示一个上次运行时间，
//! 用户却无法判断那是本次启动后的还是几天前的。落盘的那一份状态在
//! `auto_checkin`（它要跨重启去重「今天签过没有」），那里有真实需求。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::config;
use crate::server::config::IntervalTaskPatch;
use crate::server::core::account_store::AccountStore;
use crate::server::logging;

/// 调度循环的判定间隔。
///
/// 10 秒：backend 两条任务的间隔下限是 1 分钟（见 `INTERVAL_MIN_MINUTES`），
/// 10 秒的判定粒度意味着「到点后最多晚 10 秒执行」——用户感知不到，
/// 而空转代价只是每 10 秒两次读锁。不做得更密：没有收益，只是让循环更忙。
pub const TICK_MS: u64 = 10_000;

/// 任务的执行方（决定界面上要不要显示运行状态与「立即执行」）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runner {
    /// 后端循环执行（本模块的调度器负责）
    Backend,
    /// 前端轮询执行（后端只存配置与间隔）
    Frontend,
}

/// 一条任务的静态定义（注册表项）。
///
/// `min` / `max` / `default_interval` 随定义一起下发：界面据此渲染范围提示与
/// 校验输入，不必在前端再抄一份数字 —— 两处各写一套迟早会漂
/// （界面上填得进去、后端却回落到默认值，是最难解释的一类「设置不生效」）。
pub struct TaskDef {
    /// 任务 id，**同时是 `config.json` 里 `scheduledTasks` 下的子键**
    /// （同一字符串两处用，改一处必须改两处 —— 所以这里直接引用 config 的常量）
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// 间隔单位：`"minutes"` | `"seconds"`（前后端契约，界面据此显示单位与文案）
    pub unit: &'static str,
    pub runner: Runner,
    pub min: i64,
    pub max: i64,
    pub default_interval: i64,
}

/// 凭证自动维护（遍历账号 → 刷新临期凭证）。任务 id 复用 config 的键常量，
/// 保证「配置文件里叫什么」与「接口上叫什么」不会分叉。
pub const TASK_CREDENTIAL_MAINTENANCE: &str = config::KEY_CREDENTIAL_MAINTENANCE;
/// 模型目录定时刷新
pub const TASK_MODEL_REFRESH: &str = config::KEY_MODEL_REFRESH;
/// 软件版本定时检查
pub const TASK_UPDATE_CHECK: &str = config::KEY_UPDATE_CHECK;
/// 定时查询积分
pub const TASK_USAGE_QUERY: &str = config::KEY_USAGE_QUERY;
/// 敏感词库更新（从仓库拉取 `sensitive-words.json`，按版本把新词合并进本地词表）
pub const TASK_SENSITIVE_SYNC: &str = config::KEY_SENSITIVE_SYNC;
/// 日志页自动刷新（前端定时器）
pub const TASK_LOGS_AUTO_REFRESH: &str = config::KEY_LOGS_AUTO_REFRESH;
/// 请求日志页自动刷新（前端定时器）
pub const TASK_REQUESTS_AUTO_REFRESH: &str = config::KEY_REQUESTS_AUTO_REFRESH;
/// 报表页自动刷新（前端定时器）
pub const TASK_REPORT_AUTO_REFRESH: &str = config::KEY_REPORT_AUTO_REFRESH;

/// 任务清单（顺序 = 界面上的显示顺序：先后端、后前端，同类按重要性）
pub const TASKS: [TaskDef; 8] = [
    TaskDef {
        id: TASK_CREDENTIAL_MAINTENANCE,
        label: "凭证自动维护",
        description: "定期遍历全部账号，刷新已过期或临期的凭证，让账号页显示的有效期保持准确。\
                      转发链路本身有懒刷新（请求时自动续期），所以这个任务的间隔只影响列表显示。",
        unit: "minutes",
        runner: Runner::Backend,
        min: config::INTERVAL_MIN_MINUTES,
        max: config::INTERVAL_MAX_MINUTES,
        default_interval: config::DEFAULT_CREDENTIAL_MAINTENANCE_MINUTES,
    },
    TaskDef {
        id: TASK_USAGE_QUERY,
        label: "定时查询积分",
        description: "定期查询全部已启用账号的余额 / 积分（四家的接口各不相同，由各自的适配器负责），\
                      结果自动更新到账号页；查询失败的账号会明显标成「查询失败」并给出原因，不会静默留旧值。\
                      一条查询就是逐账号打一次上游的积分接口，间隔设得过密可能触发上游风控。",
        unit: "minutes",
        runner: Runner::Backend,
        min: config::INTERVAL_MIN_MINUTES,
        max: config::INTERVAL_MAX_MINUTES,
        default_interval: config::DEFAULT_USAGE_QUERY_MINUTES,
    },
    TaskDef {
        id: TASK_SENSITIVE_SYNC,
        label: "敏感词库更新",
        description: "定期从本项目的 GitHub 仓库拉取最新默认敏感词库（sensitive-words.json），\
                      把新增词条合并进本地词表 —— 上游的敏感词审核规则随时在变，这条任务让词库\
                      不必等客户端升级就能跟上。只补不删：不会覆盖或删除你自己维护的词条。\
                      拉取失败不影响转发（词表保持原样），只在下一次成功时补上。",
        unit: "minutes",
        runner: Runner::Backend,
        min: config::INTERVAL_MIN_MINUTES,
        max: config::INTERVAL_MAX_MINUTES,
        default_interval: config::DEFAULT_SENSITIVE_SYNC_MINUTES,
    },
    TaskDef {
        id: TASK_MODEL_REFRESH,
        label: "模型目录刷新",
        description: "定期向各提供商拉取最新模型清单（不支持远程目录的提供商会自动跳过）。\
                      客户端每次拉取模型列表时也会顺带刷新一次，本任务负责没有客户端请求时的那一份。",
        unit: "minutes",
        runner: Runner::Backend,
        min: config::INTERVAL_MIN_MINUTES,
        max: config::INTERVAL_MAX_MINUTES,
        default_interval: config::DEFAULT_MODEL_REFRESH_MINUTES,
    },
    TaskDef {
        id: TASK_UPDATE_CHECK,
        label: "软件版本检查",
        description: "定期向 GitHub 查询最新发布版本，发现新版本时在侧栏「设置」上亮提示并记一条\
                      「软件版本检查」日志（已是最新则不打扰）。间隔设得过密可能触发 GitHub 匿名限额\
                      （60 次/小时），5 分钟一次是稳妥值。",
        unit: "minutes",
        runner: Runner::Backend,
        min: config::INTERVAL_MIN_MINUTES,
        max: config::INTERVAL_MAX_MINUTES,
        default_interval: config::DEFAULT_UPDATE_CHECK_MINUTES,
    },
    TaskDef {
        id: TASK_LOGS_AUTO_REFRESH,
        label: "日志页自动刷新",
        description: "停留在「日志」页时按此间隔重新拉取系统事件；页面不可见时不请求。",
        unit: "seconds",
        runner: Runner::Frontend,
        min: config::INTERVAL_MIN_SECONDS,
        max: config::INTERVAL_MAX_SECONDS,
        default_interval: config::DEFAULT_LOGS_AUTO_REFRESH_SECONDS,
    },
    TaskDef {
        id: TASK_REQUESTS_AUTO_REFRESH,
        label: "请求日志自动刷新",
        description: "停留在「请求日志」页时按此间隔重新拉取请求日志；页面不可见时不请求。",
        unit: "seconds",
        runner: Runner::Frontend,
        min: config::INTERVAL_MIN_SECONDS,
        max: config::INTERVAL_MAX_SECONDS,
        default_interval: config::DEFAULT_REQUESTS_AUTO_REFRESH_SECONDS,
    },
    TaskDef {
        id: TASK_REPORT_AUTO_REFRESH,
        label: "报表自动刷新",
        description: "停留在「报表」页时按此间隔重新拉取统计数据；页面不可见时不请求。",
        unit: "seconds",
        runner: Runner::Frontend,
        min: config::INTERVAL_MIN_SECONDS,
        max: config::INTERVAL_MAX_SECONDS,
        default_interval: config::DEFAULT_REPORT_AUTO_REFRESH_SECONDS,
    },
];

/// 按 id 查定义（认不出的 id 返回 None，由调用方转成 400）
pub fn find(id: &str) -> Option<&'static TaskDef> {
    TASKS.iter().find(|task| task.id == id)
}

// ─── 运行状态（内存态，见模块头「运行状态不落盘」）─────────────

/// 一条任务的运行状态
#[derive(Clone, Debug, Default)]
struct TaskRun {
    /// 正在执行中（防止长任务被下一次 tick 重入）
    running: bool,
    /// 上次执行完成的时刻（毫秒，0 = 本次进程还没跑过）
    last_run_at: i64,
    /// 上次执行结果的一句话摘要
    last_result: Option<String>,
    /// 下次执行时刻（毫秒，0 = 不排期：任务已关闭）
    next_run_at: i64,
}

/// 全局运行状态表。键是任务 id（`&'static str`，来自注册表，不会拼错）。
static RUNS: OnceLock<Mutex<HashMap<&'static str, TaskRun>>> = OnceLock::new();

fn runs() -> &'static Mutex<HashMap<&'static str, TaskRun>> {
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取一条任务的状态克隆（锁中毒时沿用中毒数据，与项目其它处一致：
/// 「少一次状态显示」远比「界面崩掉」轻）
fn snapshot(id: &str) -> TaskRun {
    runs()
        .lock()
        .map(|table| table.get(id).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// 改一条任务的状态（`mutate` 拿不到定义时就什么都不做）
fn with_run<F>(id: &'static str, mutate: F)
where
    F: FnOnce(&mut TaskRun),
{
    let Ok(mut table) = runs().lock() else {
        return;
    };
    let entry = table.entry(id).or_default();
    mutate(entry);
}

/// 标记「开始执行」（已经在跑则返回 false，调用方据此跳过本次）
fn begin(id: &'static str) -> bool {
    let Ok(mut table) = runs().lock() else {
        return false;
    };
    let entry = table.entry(id).or_default();
    if entry.running {
        return false;
    }
    entry.running = true;
    true
}

/// 标记「执行完成」：记时刻、结果，并按**当前**间隔排下一次。
///
/// 间隔在这里现读而不是由调用方传进来：凭证维护可能跨几十秒的网络请求，
/// 期间用户完全可能把间隔从 60 分钟改到 1 分钟；用开跑前那份旧值排期，
/// 界面显示「已生效」而实际要再等一小时才生效 —— 与模块头的取舍直接冲突。
fn finish(task: &TaskDef, result: String) {
    let interval = settings_of(config::scheduled_settings(), task.id).interval;
    let next = logging::now_ms() + interval_ms(task, interval) as i64;
    with_run(task.id, |entry| {
        entry.running = false;
        entry.last_run_at = logging::now_ms();
        entry.last_result = Some(result);
        entry.next_run_at = next;
    });
}

/// 标记「本次轮次跳过」（任务已关闭）：清掉排期，界面显示「不排期」。
///
/// **不动 `running`**：正在跑的那一次要继续标记为「执行中」——
/// 一旦在这里清掉，下一个 tick 就会因为「没在跑」而再起一次，
/// 于是同一个任务被并发跑两份（凭证维护要遍历全部账号发网络请求，
/// 并发两份既浪费又可能触发上游风控）。运行的收尾只由 `finish` 负责。
fn mark_idle(id: &'static str) {
    with_run(id, |entry| {
        entry.next_run_at = 0;
    });
}

/// 把下次执行时刻置为「现在」：用户在界面上刚开启任务时调用，
/// 于是下一个 tick（最多 TICK_MS 后）就跑一次，而不是干等一个完整间隔。
///
/// **无条件置位**，不看原来的值：任务被关闭时 `mark_idle` 已把排期清成 0，
/// 这里若照旧值判断（`> 0` 才提前），恰好会在「刚从关闭转开启」这个最主要的
/// 场景下什么都不做 —— 那正是本函数存在的理由。
fn schedule_now(id: &'static str) {
    with_run(id, |entry| {
        entry.next_run_at = logging::now_ms();
    });
}

/// 初始化一条后端任务的排期（首次进入循环时用）：
/// 尚未排过（`next_run_at == 0`）就排到「现在」，于是马上就跑一次 ——
/// 这替代了改造前 bootstrap 里「启动时刷新一次凭证 / 一次模型目录」的行为。
/// **已排过期则不动**（与 `schedule_now` 的区别）：循环每 tick 都会调它，
/// 无条件置位会让任务永远「马上就跑」，间隔也就废了。
fn seed_schedule(id: &'static str) {
    with_run(id, |entry| {
        if entry.next_run_at == 0 {
            entry.next_run_at = logging::now_ms();
        }
    });
}

/// 按给定间隔从此刻重新排期（改间隔后调用，理由见 `configure`）
fn schedule_after(id: &'static str, ms: u64) {
    with_run(id, |entry| {
        entry.next_run_at = logging::now_ms() + ms as i64;
    });
}

/// 间隔值 → 毫秒（按定义里的单位换算）
fn interval_ms(task: &TaskDef, interval: i64) -> u64 {
    let value = interval.max(1) as u64;
    match task.unit {
        "seconds" => value * 1000,
        // 默认按分钟：注册表里只有 minutes / seconds 两种，写错单位时按分钟更保守
        _ => value * 60_000,
    }
}

// ─── 读：清单 ────────────────────────────────────────────────

/// 从配置快照里取一条任务的当前设置
fn settings_of(settings: config::ScheduledSettings, id: &str) -> config::IntervalTask {
    if id == TASK_CREDENTIAL_MAINTENANCE {
        settings.credential_maintenance
    } else if id == TASK_MODEL_REFRESH {
        settings.model_refresh
    } else if id == TASK_UPDATE_CHECK {
        settings.update_check
    } else if id == TASK_USAGE_QUERY {
        settings.usage_query
    } else if id == TASK_SENSITIVE_SYNC {
        settings.sensitive_sync
    } else if id == TASK_LOGS_AUTO_REFRESH {
        settings.logs_auto_refresh
    } else if id == TASK_REQUESTS_AUTO_REFRESH {
        settings.requests_auto_refresh
    } else if id == TASK_REPORT_AUTO_REFRESH {
        settings.report_auto_refresh
    } else {
        // 注册表与这里的分支必须同步（两者都由上面的 TASK_* 常量驱动）。
        // 走不到：调用方都先用 `find` 查过 id。给个默认值而不是 panic
        // —— release 是 panic=abort，任何 panic 都会带走整个应用。
        config::IntervalTask { enabled: false, interval: 0 }
    }
}

/// 一条任务的完整形态（配置 + 运行状态 + 范围元信息），供接口与界面使用。
fn task_json(task: &TaskDef) -> Value {
    let settings = settings_of(config::scheduled_settings(), task.id);
    // 前端任务没有后端循环，因此没有运行状态（见模块头「两类任务」）
    let run = match task.runner {
        Runner::Backend => snapshot(task.id),
        Runner::Frontend => TaskRun::default(),
    };
    let backend = task.runner == Runner::Backend;
    json!({
        "id": task.id,
        "label": task.label,
        "description": task.description,
        "unit": task.unit,
        "runner": if backend { "backend" } else { "frontend" },
        "enabled": settings.enabled,
        "interval": settings.interval,
        "min": task.min,
        "max": task.max,
        "defaultInterval": task.default_interval,
        // 运行状态三件套：前端任务恒为 null（它由页面自己驱动）
        "running": run.running,
        "lastRunAt": if run.last_run_at > 0 { Value::from(run.last_run_at) } else { Value::Null },
        "lastResult": run.last_result.clone().map(Value::from).unwrap_or(Value::Null),
        "nextRunAt": if backend && settings.enabled && run.next_run_at > 0 {
            Value::from(run.next_run_at)
        } else {
            Value::Null
        },
        // 「立即执行」是否可用：只有后端任务能由后端代跑
        "canRun": backend,
    })
}

/// `GET /api/scheduled-tasks` 的数据体：`{ tasks: [...] }`。
///
/// 外面包一层对象而不是直接回数组：以后要加「全局开关」之类的字段时
/// 不必改响应形状（前后端契约一变，老版本前端就会崩在这一处）。
pub fn list() -> Value {
    json!({
        "tasks": TASKS.iter().map(task_json).collect::<Vec<_>>(),
    })
}

/// 单条任务（形状与 `list()` 里的一条完全一致），认不出的 id 返回 `Null`。
///
/// 供「立即执行」的响应带着刷新后的任务一起返回（界面点完按钮就地重绘，
/// 不必再跑一趟 GET）。走 `list()` 同一份 `task_json`，两处形状自动一致。
pub fn task_by_id(id: &str) -> Value {
    find(id).map(task_json).unwrap_or(Value::Null)
}

// ─── 写：改设置 ──────────────────────────────────────────────

/// 改一条任务（`PATCH` 语义：只改传进来的项）。
///
/// 返回改完后的那条任务；id 认不出 / 间隔越界都返回 `Err(可读原因)`，
/// 由路由层转成 400（走管理信封的 `{success:false,error}` —— 与保留期一致：
/// 这是管理 API 的参数校验失败，不是转发链路的错误）。
pub fn configure(id: &str, patch: IntervalTaskPatch) -> Result<Value, String> {
    let task = find(id).ok_or_else(|| format!("未知的定时任务: {id}"))?;
    if patch.enabled.is_none() && patch.interval.is_none() {
        return Err("没有需要更新的字段".to_string());
    }
    if let Some(interval) = patch.interval {
        if !(task.min..=task.max).contains(&interval) {
            let unit = if task.unit == "seconds" { "秒" } else { "分钟" };
            return Err(format!(
                "「{}」的间隔必须是 {}–{} {}",
                task.label, task.min, task.max, unit
            ));
        }
    }

    let before = settings_of(config::scheduled_settings(), task.id);
    config::set_scheduled_task(task.id, patch, task.min, task.max);
    let after = settings_of(config::scheduled_settings(), task.id);
    let unit = if task.unit == "seconds" { "秒" } else { "分钟" };
    if before.enabled != after.enabled {
        logging::log(
            "[Tasks]",
            &format!(
                "定时任务「{}」已{}（间隔 {} {}）",
                task.label,
                if after.enabled { "开启" } else { "关闭" },
                after.interval,
                unit,
            ),
        );
    } else if before.interval != after.interval {
        logging::log(
            "[Tasks]",
            &format!("定时任务「{}」间隔已改为 {} {}", task.label, after.interval, unit),
        );
    }
    // ── 改完设置后的排期调整（三条分支互斥）──────────────────
    // 目的只有一个：让用户立刻看到自己刚做的改动生效，而不是对着一个
    // 按旧值算出来的「下次执行」干等。
    if !after.enabled {
        // 关掉：清排期，界面显示「不排期」
        mark_idle(task.id);
    } else if !before.enabled {
        // 刚开启：下一个 tick 就跑一次（这是「开启」最符合直觉的效果）
        schedule_now(task.id);
    } else if before.interval != after.interval {
        // 间隔改了：按**新**间隔从此刻重新计时。
        // 不这么做的话，把 60 分钟改成 1 分钟时下一次仍排在 60 分钟后
        // ——界面上是新值、行为却是旧节奏，正是最难排查的那类不一致。
        // 注意这里是「重新计时」而不是「立刻跑」：改间隔的意图是调整节奏，
        // 不是要求马上执行一次（要马上跑有「立即执行」按钮）。
        schedule_after(task.id, interval_ms(task, after.interval));
    }
    Ok(task_json(task))
}

// ─── 执行 ────────────────────────────────────────────────────

/// 触发方式（决定「要不要强制刷新」这类动作上的差异）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trigger {
    /// 到点自动执行
    Scheduled,
    /// 用户在界面上点「立即执行」
    Manual,
}

/// 执行一条后端任务，返回给界面的一句话摘要。
///
/// `id` 必须是 `Runner::Backend` 的任务（前端任务由页面自己刷新，
/// 后端跑不了它们）；认不出 / 类型不符都返回 `Err`，由路由层转 400。
pub async fn run_now(
    store: &AccountStore,
    update: &crate::server::core::update::UpdateManager,
    id: &str,
) -> Result<String, String> {
    let task = find(id).ok_or_else(|| format!("未知的定时任务: {id}"))?;
    if task.runner != Runner::Backend {
        return Err(format!("「{}」由界面自己刷新，无法在后端立即执行", task.label));
    }
    // 与调度循环共用同一条执行路径：手动跑与定时跑的行为必须完全一致
    //（否则「立即执行成功、定时那次却失败」这类分叉极难排查）
    run_backend(store, update, task, Trigger::Manual)
        .await
        .ok_or_else(|| format!("「{}」正在执行中，请稍候", task.label))
}

/// 跑一条后端任务（调度循环与「立即执行」的共同入口）。
///
/// 调用方**必须**先确认 `task.runner == Runner::Backend`：本函数只认这几条
/// 分支的 id，认不出的会在置了「执行中」之后返回 None，把状态卡住。
/// 两条调用点（`spawn` 的循环与 `run_now`）都已在上游筛过。
///
/// 返回 `None` 表示这次被跳过（同一任务已在跑，或 id 不在后端任务之列）；
/// `Some(摘要)` 是执行结果的一句话。
///
/// **不返回错误**：后端任务自身的失败都已经收敛成摘要文案
/// （凭证维护逐条状态、模型刷新逐家结果、版本检查的 GitHub 侧错误），
/// 这里再抛一次只会让「部分失败」变成整条 HTTP 非 2xx，反而丢掉其余成功的信息。
async fn run_backend(
    store: &AccountStore,
    update: &crate::server::core::update::UpdateManager,
    task: &TaskDef,
    trigger: Trigger,
) -> Option<String> {
    if task.runner != Runner::Backend {
        return None;
    }
    if !begin(task.id) {
        return None;
    }
    let summary = match task.id {
        TASK_CREDENTIAL_MAINTENANCE => {
            let results =
                crate::server::core::credential_maintenance::refresh_expiring_accounts(store).await;
            let (refreshed, skipped, failed) =
                crate::server::core::credential_maintenance::summarize(&results);
            // 全零轮次（没有任何临期账号）是常态，不写日志库：
            // 每轮一条「什么都没做」会把日志页淹掉（与维护模块自身同一取舍）
            if refreshed > 0 || failed > 0 {
                // 全部成功是常态轮次：级别必须 info，不能让「失败 0 个」的文案把它抬成 error
                logging::log_with_level(
                    "[Maintenance]",
                    &format!("凭证自动维护：刷新 {refreshed} 个，跳过 {skipped} 个，失败 {failed} 个"),
                    if failed > 0 { "error" } else { "info" },
                );
            }
            format!("刷新 {refreshed} 个，跳过 {skipped} 个，失败 {failed} 个")
        }
        TASK_MODEL_REFRESH => {
            // 手动必须强制绕过缓存：用户按下按钮的全部预期是「现在真的去拉一次」，
            // 小浣熊的 10 分钟 TTL 会让「点了没反应、清单没变」与坏掉无法区分。
            // 自动路径相反 —— 本来就该「有缓存用缓存」，各家 TTL 自会判定。
            if trigger == Trigger::Manual {
                let results =
                    crate::server::core::providers::adapter::refresh_implemented_forced(store).await;
                let count = |status: &str| {
                    results
                        .iter()
                        .filter(|item| item.get("status").and_then(Value::as_str) == Some(status))
                        .count()
                };
                let (refreshed, skipped, failed) =
                    (count("refreshed"), count("skipped"), count("failed"));
                logging::log_with_level(
                    "[Models]",
                    &format!("定时任务触发刷新模型清单：成功 {refreshed}，跳过 {skipped}，失败 {failed}"),
                    if failed > 0 { "error" } else { "info" },
                );
                format!("成功 {refreshed} 家，跳过 {skipped} 家，失败 {failed} 家")
            } else {
                crate::server::core::providers::adapter::refresh_implemented(store).await;
                // 自动路径逐家的成败已由各适配器自己写进日志（`refresh_implemented`
                // 不看结果就是这个原因），这里只给一句「跑了」
                "已刷新（各家按自身缓存策略决定是否真拉取）".to_string()
            }
        }
        TASK_UPDATE_CHECK => {
            // 定时检查更新。结果缓存进 UpdateManager（前端轮询 /api/update/status
            // 亮侧栏徽标），「发现新版本」才写一条日志 —— 已是最新的轮次若也落库，
            // 日志页会被每 5 分钟一条的「无事发生」淹掉（与凭证维护的空轮次同一取舍）。
            // 失败收敛成摘要（GitHub 限额 / 网络），不打断调度。
            let current = crate::server::core::update::CURRENT_VERSION;
            match update.check(current).await {
                Ok(info) => {
                    let latest = info
                        .get("latestVersion")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let has_update = info.get("hasUpdate").and_then(Value::as_bool);
                    if has_update == Some(true) && !latest.is_empty() {
                        logging::log(
                            "[Update]",
                            &format!("发现新版本 {latest}（当前 {current}）"),
                        );
                        format!("发现新版本 {latest}（当前 {current}）")
                    } else {
                        "已是最新".to_string()
                    }
                }
                Err(error) => format!("检查失败：{}", error.message),
            }
        }
        TASK_USAGE_QUERY => {
            // 定时查询积分：查全部启用账号的余额，结果存进快照
            // （界面读 `/api/accounts/usage/snapshot` 拿它，不必用户点按钮）。
            //
            // 与手动那条（`GET /api/accounts/usage`）**共用 core::usage_query::query_all**：
            // 「查哪些账号、怎么查、失败怎么收敛」只有一份，两条入口只在「结果去哪」
            // 上不同 —— 手动是直接回给这一次请求，定时是存快照。
            //
            // **失败也进快照**（`store_snapshot` 里不筛）：这条任务的意义就是
            // 「不点按钮也知道现在好不好」，把失败丢掉会让界面停在旧余额上，
            // 比显示失败更误导。摘要里同样带上失败数，且失败时写一条日志 ——
            // 余额全线查不通（凭证过期 / 上游改接口）是用户需要知道的事。
            // `id = None`：定时这轮只查启用账号（禁用账号不参与定时轮询）。
            match crate::server::core::usage_query::query_all(store, None).await {
                Ok(report) => {
                    let (ok, failed) = crate::server::core::usage_query::store_snapshot(report);
                    if failed > 0 {
                        logging::log(
                            "[Usage]",
                            &format!("定时查询积分：成功 {ok} 个，失败 {failed} 个"),
                        );
                    }
                    format!("成功 {ok} 个，失败 {failed} 个")
                }
                Err(error) => format!("查询失败：{}", error.message),
            }
        }
        TASK_SENSITIVE_SYNC => {
            // 拉取仓库里的默认词库并按版本合并（只补不删，见
            // `core::desensitize::remote` 的模块头）。
            //
            // 手动必须 force：用户点「立即执行」的预期是「现在真的去拉一次」，
            // 而版本号没变时普通路径会直接返回「已是最新」—— 那与「任务坏掉了」
            // 在界面上无法区分。force 也**只补不删**，不会破坏用户词表。
            crate::server::core::desensitize::remote::sync(trigger == Trigger::Manual).await
        }
        _ => {
            with_run(task.id, |entry| entry.running = false);
            return None;
        }
    };
    finish(task, summary.clone());
    Some(summary)
}

/// 调度循环：把后端任务按各自配置的间隔重复执行。
///
/// 由 `ServerState::bootstrap` 起一次（进程内只有这一个循环，见那边的注释）。
/// 循环体只做三件事：读配置 → 看谁到点了 → 跑它。所有状态都在
/// `RUNS` 与 `config` 里，因此重启进程即从头开始（这是有意的，见模块头）。
///
/// **首轮就绪即跑**：spawn 出来的第一次循环不等间隔，直接跑一次各条任务 ——
/// 这替代了改造前 bootstrap 里那两处「启动时刷新一次」（凭证维护与模型目录），
/// 于是「关掉任务 = 启动也不刷」这条一致性成立；开启时行为与改造前相同。
/// 定时查询积分也走这条：启动即查一次，界面不用等满一个间隔才见到余额。
pub fn spawn(store: AccountStore, update: crate::server::core::update::UpdateManager) {
    tauri::async_runtime::spawn(async move {
        // 首轮：给各条后端任务排上「现在就执行」的期，于是紧接着的第一次循环
        // 立刻就跑一次。这替代了改造前 bootstrap 里那两处「启动时刷新一次」
        // （凭证维护与模型目录）—— 于是「关掉任务 = 启动也不刷」这条一致性成立，
        // 开启时的行为则与改造前完全相同。
        for task in TASKS.iter().filter(|task| task.runner == Runner::Backend) {
            seed_schedule(task.id);
        }
        loop {
            for task in TASKS.iter().filter(|task| task.runner == Runner::Backend) {
                let settings = settings_of(config::scheduled_settings(), task.id);
                if !settings.enabled {
                    // 关闭态：清掉排期（界面显示「不排期」），本轮到此为止。
                    // 间隔仍留在配置里，下次开启时接着用。
                    mark_idle(task.id);
                    continue;
                }
                let run = snapshot(task.id);
                if run.running {
                    // 上一次还没跑完（凭证刷新可能跨多个账号的网络请求）：
                    // 不重入，等它自己结束 —— 下一轮 tick 会重算排期
                    continue;
                }
                if run.next_run_at == 0 {
                    // 未排过期（本次运行还没跑过、或刚从关闭态转回开启）：排到「现在」
                    seed_schedule(task.id);
                    continue;
                }
                if logging::now_ms() < run.next_run_at {
                    continue;
                }
                // 到点：跑一次（`run_backend` 内部按当前间隔重新排期）。
                // 模型刷新的自动路径尊重各家 TTL，与「立即执行」的强制刷新分开，
                // 差异收在 `run_backend` 的 `Trigger` 分支里 —— 这里不重复一遍。
                run_backend(&store, &update, task, Trigger::Scheduled).await;
            }
            tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
        }
    });
}
