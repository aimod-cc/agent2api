//! 定时签到 —— 每天在指定时刻执行一次「全部账号签到」。
//!
//! 对照 Node 版 src/workbuddy-auto-checkin.mjs 全量移植。与 /api/accounts/checkin
//! 用的是同一段签到逻辑（`core::billing::checkin::run_checkin`），
//! 因此限额跳过、国际版排除、串行防风的规则完全一致，不存在两套行为。
//!
//! ── 触发方式：轮询 + 当天去重（不是单个定时器）────────────────
//! Node 版用 `setInterval(tick, 30s)`：单个 setTimeout 在系统休眠、锁屏、时钟被改
//! 之后会漂移甚至整段错过，而每 30 秒比一次「当前是否已过今天的触发点」，
//! 唤醒后自然补上。Rust 侧用等价的 `sleep(30s)` 循环，每个 tick 都从**墙上时钟**
//! （`chrono::Local::now()`）重新判定，因此与 Node 一样具备自愈能力：
//! libuv 与 tokio 的定时器都基于单调时钟，机器休眠期间都不推进，
//! 醒来的第一次 tick 会把错过的时点补上 —— 两者在这点上是同一套语义。
//!
//! ── 补签（有意的行为，别「顺手优化」）────────────────────────
//! Node 版 `start()` 的条件是**「今天还没签过」**，并不判「时间点是否已过」：
//! 开着自动签到却在 00:01 时没开机、白天才启动 → 立刻补一次。
//! 签到的每日额度按自然日重置，用户开着开关却整天没签到才是反直觉的结果；
//! 上游签到接口幂等，重复调用只返回「已领取」。实测（2026-09-17）在 18:44
//! 把 enabled 设为 true、time 设为 23:00（未来时点）时，Node 版立即执行了
//! 一次 reason='启动补签' 的签到 —— 所以这里的条件同样只看日期键。
//!
//! ── 配置读写 ────────────────────────────────────────────────
//! 状态存在 config.json 的 `autoCheckin` 字段（`{enabled, time, lastFiredDate,
//! lastResult}`）。Node 每次 getState/tick 都重读 config.json；Rust 侧读
//! `config::current()` 的内存快照（config.rs 已经全量保留未知字段，写回也走它），
//! 于是「别人写进 config.json 的字段不会被吃掉」这条契约继续成立。
//! 差异：Node 会看到用户手工改盘上的 config.json 后的新值，Rust 的内存快照
//! 只在启动与写入时刷新 —— 手工改文件需要重启（与 config.rs 的既有设计一致）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::billing::checkin;
use crate::server::core::billing::BillingService;
use crate::server::logging;

/// 轮询间隔：30 秒足够精确到分钟，又不会让计时器显得忙（Node 版 TICK_MS）
pub const TICK_MS: u64 = 30_000;

/// 默认触发时刻：零点一分（Node 版 DEFAULT_TIME）
pub const DEFAULT_TIME: &str = "00:01";

/// 可勾选的签到提供商（界面上的复选框）。默认全选。
///
///   - **WorkBuddy**：腾讯的每日签到接口；
///   - **小浣熊**：「桌面登录积分」链路（`providers::raccoon` 的每日积分发放）；
///   - **AutoClaw 国内版 / 国际版**：通用任务接口的 `daily_signin` 任务
///     （`providers::autoclaw::checkin`）。两个地区**都支持** —— 任务接口在
///     两地是同一套路径、同一套任务 id，只是站点不同（已实测），因此两家
///     都列进来；地区由 `billing::checkin` 从账号的 provider 反查。
///
/// 这是「有签到活动」的清单，不是「有积分概念」的清单：CatPaw / Qoder 有积分
/// 查询但没有签到，因此不在此列 —— 它们的账号在批量签到里被算作 `skipped`。
/// 加一家之前先确认它的签到链路真的存在（一个点了必然报错的复选框比没有更糟）。
pub const CHECKIN_PROVIDERS: [&str; 4] = ["workbuddy", "raccoon", "autoclaw", "autoclaw-intl"];

/// 缺省的签到提供商集合（全选）
pub fn default_providers() -> Vec<String> {
    CHECKIN_PROVIDERS.iter().map(|id| id.to_string()).collect()
}

/// 提供商的展示名（从注册表查，查不到就原样回显 id）。
///
/// ── 为什么 WorkBuddy 要带上「国内版」────────────────────────
/// 这个标签只出现在**签到语境**（提供商复选框、配置错误提示、签到范围变更日志），
/// 而签到对 WorkBuddy 是**有版本限定**的：只有国内版有签到活动，国际版账号
/// 一律被 `billing::checkin::supports_checkin` 排除（上游事实：腾讯的每日签到
/// 接口只有国内站有）。注册表里的 `label` 是这家在**所有语境**下的通用展示名
/// （账号卡片、筛选、模型清单都用它），改成「WorkBuddy 国内版」会让那些地方
/// 出现一个没头没尾的版本后缀。
///
/// 因此在这里覆盖而不是改注册表：分叉的原因不是「名字不一样」，而是
/// 「签到这条链路只有国内版能走」—— 标签替用户把这件事讲清楚，
/// 他勾上它时就知道国际版账号不会参与，而不是签完发现被跳过了才回来查。
///
/// 另外几家没有这个后缀：小浣熊没有版本区分（`edition` 概念不适用于它），
/// AutoClaw 两地的签到链路都存在且同形 —— 它的展示名已经带「国内版 / 国际版」
/// 后缀（注册表里就是），因此不需要在这里再补。
fn provider_label(id: &str) -> &str {
    match id {
        "workbuddy" => "WorkBuddy 国内版",
        other => crate::server::core::providers::PROVIDERS
            .iter()
            .find(|meta| meta.id == other)
            .map(|meta| meta.label)
            .unwrap_or(other),
    }
}

/// 归一化配置里的提供商清单：只认 CHECKIN_PROVIDERS 里的 id（去重、保持顺序），
/// 缺失 / 空数组 / 全是非法值都回落到「全选」—— 旧配置文件里没有这个字段，
/// 读出来必须是合法的默认行为。
pub fn normalize_providers(value: Option<&Value>) -> Vec<String> {
    let Some(list) = value.and_then(Value::as_array) else {
        return default_providers();
    };
    let picked: Vec<String> = CHECKIN_PROVIDERS
        .iter()
        .filter(|id| {
            list.iter()
                .any(|item| item.as_str() == Some(*id))
        })
        .map(|id| id.to_string())
        .collect();
    if picked.is_empty() {
        default_providers()
    } else {
        picked
    }
}

/// config.json 里承载定时签到状态的键
const CONFIG_KEY: &str = "autoCheckin";

/// 配置类错误（对应 Node 版 AutoCheckinConfigError）。
///
/// **注意 statusCode 是 400 而不是 409**：Node 版 `AutoCheckinConfigError`
/// 构造里写死 `statusCode = 400`，于是 server.mjs 的 `throw new
/// AutoCheckinConfigError('签到正在执行中，请稍候')`（run 路由）经 errorPayload
/// 得到的是 **400 + OpenAI 风格 body**，而不是 409。实测（2026-09-17）用非法
/// time 打 POST /api/auto-checkin 拿到的正是
/// `400 {"error":{"message":"时间超出范围（00:00 - 23:59）","type":"invalid_request_error"}}`。
/// 契约以 Node 版为准，这里保持一致（`CHECKIN_BUSY_STATUS` 单独抽出，
/// 万一确认要用 409，改一处即可）。
#[derive(Clone, Debug)]
pub struct AutoCheckinConfigError {
    pub message: String,
    pub status_code: i32,
}

/// run 路由「正在执行中」的状态码：与 Node 的 AutoCheckinConfigError 一致为 400
pub const CHECKIN_BUSY_STATUS: i32 = 400;

impl AutoCheckinConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), status_code: 400 }
    }
}

impl std::fmt::Display for AutoCheckinConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

// ─── 时间工具（对照 Node 的同名函数）────────────────────────

/// JS `String(value)` 的近似：只用于 normalizeTime 的输入转换。
/// `value ?? ''` 在 Node 里把 null/undefined 变成空串，其余走 String()：
/// 数字/布尔取字面量，对象/数组在 JS 里是 "[object Object]" / 逗号拼接 ——
/// 这些形态只可能来自手改的请求体或 config.json，最终都会被正则拒掉。
fn js_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Object(_)) => "[object Object]".to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// 校验并归一化 HH:MM，返回补零后的文本（对应 Node 版 normalizeTime）。
///
/// 正则 `^(\d{1,2}):(\d{1,2})$` 手写等价实现：冒号两侧各 1-2 位 ASCII 数字，
/// 且**整串**必须如此（`\d` 在 JS 里不含全角数字、不允许前后空白 ——
/// 前后空白在 Node 里由 `String(value).trim()` 提前去掉）。
pub fn normalize_time(value: Option<&Value>) -> Result<String, AutoCheckinConfigError> {
    let text = js_text(value);
    let text = text.trim();
    let parsed = text
        .split_once(':')
        .filter(|(head, tail)| {
            let digits = |part: &str| {
                !part.is_empty()
                    && part.len() <= 2
                    && part.bytes().all(|byte| byte.is_ascii_digit())
            };
            digits(head) && digits(tail) && !tail.contains(':')
        })
        .and_then(|(head, tail)| Some((head.parse::<u32>().ok()?, tail.parse::<u32>().ok()?)));
    let Some((hour, minute)) = parsed else {
        return Err(AutoCheckinConfigError::new("时间格式应为 HH:MM（例如 00:01）"));
    };
    if hour > 23 || minute > 59 {
        return Err(AutoCheckinConfigError::new("时间超出范围（00:00 - 23:59）"));
    }
    Ok(format!("{hour:02}:{minute:02}"))
}

/// 本地日期键 YYYY-MM-DD：用于「今天是否已触发」的判定。
/// Node 用 `getFullYear/getMonth/getDate`（**本地时区**），不能用 UTC ——
/// UTC+8 的凌晨 00:01 在 UTC 下还是前一天，会让补签判重算错一整天。
pub fn local_date_key(now: DateTime<Local>) -> String {
    now.format("%Y-%m-%d").to_string()
}

/// 今天的触发时刻（本地时区）；已过则返回 Some，未到返回 None。
/// 对应 Node 版 dueNow：非法的 time 文本在这里同样表现为 None（Node 是抛异常，
/// 但调用方一律 catch 后 return，结果一致）。
fn due_now(time_text: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let target = today_at(time_text, now)?;
    if now >= target {
        Some(target)
    } else {
        None
    }
}

/// 距离下一次触发的毫秒数（用于界面显示「下次执行」）。
/// 对应 Node 版 msUntilNext：目标时刻已过则顺延到明天同一时刻。
pub fn ms_until_next(time_text: &str, now: DateTime<Local>) -> i64 {
    let Some(mut target) = today_at(time_text, now) else {
        return 0;
    };
    if target <= now {
        target += chrono::Duration::days(1);
    }
    (target - now).num_milliseconds()
}

/// 今天的 HH:MM 时刻（本地时区）；time 文本非法时 None。
///
/// Node 用 `target.setHours(hour, minute, 0, 0)`（就地改到当天）；
/// chrono 没有 set_* 的就地 API，用 `with_ymd_and_hms` 重建。
/// 夏令时切换当天若该时刻不存在（LocalResult::None），按 None 处理 ——
/// 跳过这一次而不是猜一个偏移，比 Node 的 setHours 更保守（中国无夏令时，
/// 这条分支实际不会走到）。
fn today_at(time_text: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let normalized = normalize_time(Some(&Value::String(time_text.to_string()))).ok()?;
    let (hour, minute) = normalized.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    let naive = now.date_naive().and_hms_opt(hour, minute, 0)?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|value| value.with_timezone(&Local))
}

// ─── 状态读写 ───────────────────────────────────────────────

/// 从 config.json 读出的调度状态（对应 Node 版 readState 的返回）
struct CheckinState {
    enabled: bool,
    time: String,
    /// 要签到的提供商（见 CHECKIN_PROVIDERS；缺省全选）
    providers: Vec<String>,
    last_fired_date: Option<String>,
    last_result: Option<Value>,
}

/// `autoCheckin` 字段的原始对象（非对象/缺失都给空对象）
fn raw_object() -> Map<String, Value> {
    match config::current().raw().get(CONFIG_KEY) {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// 读状态。time 被手工改坏时回落到 DEFAULT_TIME，不让调度起不来（Node 同）。
fn read_state() -> CheckinState {
    let raw = raw_object();
    let enabled = raw.get("enabled").and_then(Value::as_bool) == Some(true);
    let time = raw
        .get("time")
        .filter(|value| !value.is_null())
        .and_then(|value| normalize_time(Some(value)).ok())
        .unwrap_or_else(|| DEFAULT_TIME.to_string());
    let last_fired_date = raw
        .get("lastFiredDate")
        .and_then(Value::as_str)
        .map(str::to_string);
    let last_result = raw.get("lastResult").filter(|value| value.is_object()).cloned();
    CheckinState {
        enabled,
        time,
        providers: normalize_providers(raw.get("providers")),
        last_fired_date,
        last_result,
    }
}

/// 合并写回 `autoCheckin`（对应 Node 版 writeState 的 `{...current, ...patch}`）。
/// 走 config.rs 的 `update_raw_field`，config.json 里的其它字段原样保留。
fn write_state(patch: Value) {
    let mut merged = raw_object();
    if let Value::Object(patch) = patch {
        for (key, value) in patch {
            merged.insert(key, value);
        }
    }
    config::update_raw_field(CONFIG_KEY, Value::Object(merged));
}

// ─── 服务句柄 ───────────────────────────────────────────────

/// 句柄内部状态：下载/签到这类长任务只在这里留「是否在跑」与调度任务句柄。
struct Inner {
    /// 是否已有一次签到在执行（对应 Node 的闭包变量 running）
    running: bool,
    /// 调度循环任务句柄（对应 Node 的 timer）
    task: Option<tokio::task::JoinHandle<()>>,
    /// 停机标志：stop() 置位后循环退出（幂等；再次 start 会重置）
    stopped: Arc<AtomicBool>,
}

/// 定时签到服务句柄：内部一把锁 + Clone，与 update / models 同构。
///
/// 锁只用来读写 running 与任务句柄这类微秒级操作；签到执行全程在锁外
/// （硬约束：持锁不做网络请求、不 await）。`running` 用 CAS 抢锁的方式表达
/// 「同时只跑一次」，与 Node 单线程下 `if (running) return` 的语义一致。
#[derive(Clone)]
pub struct AutoCheckin {
    store: AccountStore,
    billing: BillingService,
    inner: Arc<Mutex<Inner>>,
}

/// 抢到「执行中」标记的守卫：无论正常返回还是中途被取消（任务 abort），
/// Drop 时都会把 running 复位 —— 对应 Node 的 `finally { running = false }`，
/// 同时避免 abort 把 running 永久卡在 true。
struct RunningGuard {
    inner: Arc<Mutex<Inner>>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        match self.inner.lock() {
            Ok(mut guard) => guard.running = false,
            Err(poisoned) => poisoned.into_inner().running = false,
        }
    }
}

impl AutoCheckin {
    pub fn new(store: AccountStore, billing: BillingService) -> Self {
        Self {
            store,
            billing,
            inner: Arc::new(Mutex::new(Inner {
                running: false,
                task: None,
                stopped: Arc::new(AtomicBool::new(false)),
            })),
        }
    }

    /// 取锁；锁中毒（持锁 panic）不致命，接管内部数据继续用
    /// （与账号存储、脱敏器同一策略）
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 是否正在执行签到（getState().running）
    fn is_running(&self) -> bool {
        self.lock().running
    }

    // ─── 对外状态 ───────────────────────────────────────────

    /// 对外暴露的状态（对应 Node 版 getState，含界面要显示的「下次执行」）
    pub fn state(&self) -> Value {
        let state = read_state();
        let now = Local::now();
        let next_run_at = if state.enabled {
            let next = logging::now_ms() + ms_until_next(&state.time, now);
            Value::from(next)
        } else {
            Value::Null
        };
        json!({
            "enabled": state.enabled,
            "time": state.time,
            // 界面按这份清单勾选复选框；顺序即 CHECKIN_PROVIDERS 的注册顺序
            "providers": state.providers,
            "providerOptions": CHECKIN_PROVIDERS.iter().map(|id| {
                json!({ "id": id, "label": provider_label(id) })
            }).collect::<Vec<_>>(),
            "lastFiredDate": state.last_fired_date,
            "lastFiredToday": state.last_fired_date.as_deref() == Some(local_date_key(now).as_str()),
            "nextRunAt": next_run_at,
            "lastResult": state.last_result,
            "running": self.is_running(),
        })
    }

    // ─── 执行 ───────────────────────────────────────────────

    /// 执行一次签到并记录结果；失败只记日志，不影响下一次调度。
    ///
    /// 返回 None 的两种情形（Node 同）：① 已有一次在跑（本次跳过）；
    /// ② 执行抛错。路由层据此把 ① 翻成「签到正在执行中，请稍候」。
    pub async fn fire(&self, reason: &str) -> Option<Value> {
        // 先抢「执行中」标记；抢不到说明上一次还在跑
        let guard = {
            let mut inner = self.lock();
            if inner.running {
                drop(inner);
                logging::verbose("[Checkin]", "上一次签到仍在执行，本次跳过");
                return None;
            }
            inner.running = true;
            RunningGuard { inner: self.inner.clone() }
        };

        let today = local_date_key(Local::now());
        // 先落日期再执行：即便签到中途进程被杀，也不会在重启后反复补签
        write_state(json!({ "lastFiredDate": today }));
        logging::log("[Checkin]", &format!("⏰ 定时签到开始（{reason}）"));

        let outcome = match checkin::run_checkin(
            &self.store,
            &self.billing,
            read_state().providers.as_slice(),
            None,
        )
        .await
        {
            Ok(result) => Some(self.record_success(&result, &today, reason)),
            Err(error) => {
                // 失败也写 lastResult（failed 里放错误文案），与 Node 的 catch 分支一致
                write_state(json!({
                    "lastResult": {
                        "at": logging::now_ms(),
                        "date": today,
                        "reason": reason,
                        "succeeded": 0,
                        "total": 0,
                        "skipped": 0,
                        "failed": [error.message.clone()],
                        "failedCount": 1,
                    }
                }));
                logging::log("[Checkin]", &format!("❌ 定时签到失败: {}", error.message));
                None
            }
        };
        drop(guard);
        outcome
    }

    /// 成功分支：汇总 + 记录 lastResult + 日志（对应 Node fire 里的 try 主体）
    fn record_success(&self, result: &Value, today: &str, reason: &str) -> Value {
        let number = |key: &str| {
            result
                .get(key)
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let succeeded = number("succeeded");
        let total = number("total");
        let skipped = number("skipped");
        // 失败明细：`名字（错误）`，名字缺失时退到 id，再退到「未知账号」
        let failures: Vec<String> = result
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let error = item.get("error").and_then(Value::as_str)?;
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .or_else(|| item.get("id").and_then(Value::as_str))
                            .unwrap_or("未知账号");
                        Some(format!("{name}（{error}）"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let summary = json!({
            "at": logging::now_ms(),
            "date": today,
            "reason": reason,
            "succeeded": succeeded,
            "total": total,
            "skipped": skipped,
            // 只留前 5 条：面板展示用，避免 config.json 被长列表撑大（Node 同）
            "failed": failures.iter().take(5).cloned().collect::<Vec<_>>(),
            "failedCount": failures.len(),
        });
        write_state(json!({ "lastResult": summary }));
        logging::log(
            "[Checkin]",
            &format!(
                "定时签到完成: {succeeded}/{total} 个账号成功领取{}{}",
                if skipped > 0 { format!("，跳过 {skipped} 个") } else { String::new() },
                if failures.is_empty() {
                    String::new()
                } else {
                    format!("，失败 {} 个", failures.len())
                },
            ),
        );
        summary
    }

    // ─── 调度 ───────────────────────────────────────────────

    /// 轮询回调：到点且今天没签过就执行（对应 Node 版 tick）
    pub async fn tick(&self) {
        let state = read_state();
        if !state.enabled {
            return;
        }
        let now = Local::now();
        // 未到点（含 time 非法）直接返回
        if due_now(&state.time, now).is_none() {
            return;
        }
        if state.last_fired_date.as_deref() == Some(local_date_key(now).as_str()) {
            return;
        }
        self.fire("到点触发").await;
    }

    /// 起调度循环（对应 Node 版 schedule 的 `if (timer) return`）：
    /// 已有存活任务就不重复起，避免 configure 每次保存都叠一个循环。
    fn schedule(&self) {
        {
            let mut inner = self.lock();
            if let Some(task) = &inner.task {
                if !task.is_finished() {
                    return;
                }
            }
            inner.stopped = Arc::new(AtomicBool::new(false));
            let stopped = inner.stopped.clone();
            let service = self.clone();
            // 用 crate::spawn_task：本函数可能从 setup 钩子（主线程）或 axum handler
            // 调用，`crate::spawn_task` 兜底进全局运行时，
            // 不会像裸 `tokio::spawn` 那样在非运行时上下文 panic（release 是 panic=abort）
            inner.task = Some(crate::spawn_task(async move {
                loop {
                    // 先睡再 tick：与 setInterval 的「间隔后首次触发」一致，
                    // 也避开 start() 里那次补签刚发起的窗口
                    tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    service.tick().await;
                }
            }));
        }
    }

    /// 启动调度（对应 Node 版 start）。
    ///
    /// 开启状态下立即检查一次：**只看「今天签过没有」，不看时点是否已过**
    /// （理由见模块头部「补签」）。
    pub fn start(&self) {
        let state = read_state();
        if !state.enabled {
            return;
        }
        self.schedule();
        if state.last_fired_date.as_deref() != Some(local_date_key(Local::now()).as_str()) {
            let service = self.clone();
            crate::spawn_task(async move {
                service.fire("启动补签").await;
            });
        } else {
            logging::verbose(
                "[Checkin]",
                &format!("定时签到已于今日执行，下次 {}", state.time),
            );
        }
    }

    /// 停调度（进程退出时用）。幂等：重复调用不会报错。
    ///
    /// 与 Node 的 `clearInterval` 语义对齐：只结束**循环**，不打断正在跑的
    /// 那一次签到（abort 只会让任务在下一个 await 点结束，config.json 的写入是
    /// 同步的，已落盘的 lastFiredDate 不会被回滚）。
    pub fn stop(&self) {
        let task = {
            let mut inner = self.lock();
            if inner.task.is_none() {
                return;
            }
            inner.stopped.store(true, Ordering::SeqCst);
            inner.task.take()
        };
        if let Some(task) = task {
            task.abort();
        }
    }

    // ─── 设置 ───────────────────────────────────────────────

    /// 保存设置（对应 Node 版 configure）。关闭时循环归还调度循环继续跑，
    /// 但 tick 读到 enabled=false 会直接返回 —— 与 Node 一致（Node 的
    /// configure 也只 start 不 stop，关闭靠 tick 早退生效）。
    pub fn configure(&self, payload: &Value) -> Result<Value, AutoCheckinConfigError> {
        let mut patch = Map::new();
        if let Some(value) = payload.get("enabled") {
            // Node: `enabled !== undefined` → `patch.enabled = enabled === true`，
            // 也就是说 null / 字符串 / 0 都会把开关置为 false（不是「忽略」）
            patch.insert("enabled".to_string(), Value::Bool(value == &Value::Bool(true)));
        }
        if let Some(value) = payload.get("time") {
            patch.insert("time".to_string(), Value::String(normalize_time(Some(value))?));
        }
        if let Some(value) = payload.get("providers") {
            // 勾选清单：只认注册过的提供商 id（去重、按注册顺序落盘）；
            // 一个都不勾没有意义 —— 定时到点也无账号可签，直接拒绝并说明。
            let Some(list) = value.as_array() else {
                return Err(AutoCheckinConfigError::new("providers 必须是字符串数组"));
            };
            let picked: Vec<Value> = CHECKIN_PROVIDERS
                .iter()
                .filter(|id| list.iter().any(|item| item.as_str() == Some(*id)))
                .map(|id| Value::String(id.to_string()))
                .collect();
            if picked.is_empty() {
                let names = CHECKIN_PROVIDERS
                    .iter()
                    .map(|id| provider_label(id))
                    .collect::<Vec<_>>()
                    .join("、");
                return Err(AutoCheckinConfigError::new(format!(
                    "至少勾选一家签到提供商（{names}）"
                )));
            }
            patch.insert("providers".to_string(), Value::Array(picked));
        }
        if patch.is_empty() {
            return Err(AutoCheckinConfigError::new("没有需要更新的字段"));
        }

        let before = read_state();
        write_state(Value::Object(patch));
        let after = read_state();
        if before.enabled != after.enabled {
            logging::log(
                "[Checkin]",
                &format!(
                    "定时签到已{}（{}）",
                    if after.enabled { "开启" } else { "关闭" },
                    after.time
                ),
            );
        } else if before.time != after.time {
            logging::log("[Checkin]", &format!("定时签到时间已改为 {}", after.time));
        }
        if before.providers != after.providers {
            let names = |list: &[String]| {
                list.iter()
                    .map(|id| provider_label(id))
                    .collect::<Vec<_>>()
                    .join("、")
            };
            logging::log(
                "[Checkin]",
                &format!("签到范围已改为 {}", names(&after.providers)),
            );
        }
        if after.enabled {
            self.start();
        }
        Ok(self.state())
    }

    /// 当前配置的签到提供商（账号页批量签到与定时签到共用同一份口径）。
    pub fn configured_providers(&self) -> Vec<String> {
        read_state().providers
    }

    /// 立即执行一次（界面上的「立即签到」按钮走这里，与定时触发同一条路径）。
    /// 正在执行中返回 None —— 路由层翻成「签到正在执行中，请稍候」。
    pub async fn run_now(&self) -> Option<Value> {
        self.fire("手动触发").await
    }
}

// ─── 进程级句柄（供停机路径调用 stop）─────────────────────────

/// 进程级定时签到句柄。
///
/// 为什么不用 ServerState 就够了：停机入口是 `backend::shutdown(&AppState)`，
/// 那里只有 Tauri 的应用状态，拿不到 ServerState；而 Node 版退出时明确会
/// `autoCheckin.stop()`（server.mjs 1047-1049 的 closeAll）。用进程级句柄
/// （与 config / logging 同一个模式）让停机路径能直接拿到它。
/// ServerState 里那份与全局这份是**同一实例**（bootstrap 时装入）。
static GLOBAL: OnceLock<AutoCheckin> = OnceLock::new();

/// 初始化进程级句柄（启动时调用一次，幂等）
pub fn init_global(service: AutoCheckin) -> AutoCheckin {
    let _ = GLOBAL.set(service.clone());
    service
}

/// 停掉进程级调度；未初始化时什么都不做（路由以外的调用点用）
pub fn stop_global() {
    if let Some(service) = GLOBAL.get() {
        service.stop();
    }
}
