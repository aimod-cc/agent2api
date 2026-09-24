//! 配置字段的**解析**与环境变量兜底：`Map` 进去、解析结果出来。
//!
//! ── 为什么单独一层 ──────────────────────────────────────────
//! `mod.rs` 装的是「配置的读写入口」（内存快照、各 setter、落库），而这一组
//! 函数是**不依赖全局状态**的纯变换。它们被 `build()`（构造快照时全量解析
//! 一遍）与各 `set_*` 的回读逻辑调用，与库、与文件都没有关系 —— 抽出来之后
//! `mod.rs` 不必为了容纳这段注释与实现而挤到 900 行以上（本项目的单文件行数
//! 约定），而且「配置项的取值口径只有这一处」这件事在文件结构上直接可见。
//!
//! ── 读侧的统一口径（改造前后逐字一致）───────────────────────
//! 配置是**手改得到**的东西（改造前是 `config.json`，现在仍可用 `sqlite3`
//! 改库里的那一行），所以每个取值函数都必须回答「值缺失 / 类型不符 / 越界时
//! 怎么办」，且口径统一：**一律回落到默认值，不报错、不夹紧**。
//!   - 不报错：一个写坏的字段不该让整个网关起不来；
//!   - 不夹紧：静默把 5000 改成 3650 会让用户以为「设置生效了」，而回落到
//!     默认值至少与他看到的值不一致，好排查。
//! 走接口写入的值在各自的 PUT 处理器里已按同一范围校验过（那里是 400 报错）——
//! 两侧范围常量共用（`RETENTION_MIN_DAYS` 等），所以不会出现「接口拒绝 60
//! 而手改库接受它」这种两套口径。
//!
//! ── 环境变量为什么也在这里 ──────────────────────────────────
//! 「配置文件的值 > 环境变量 > 内置默认值」这条优先级由 `mod.rs` 的 `build()`
//! 落地，而 `env_api_key()` / `env_text()` 是它的组成部分 —— 它们与取值函数
//! 一起构成「一个字段最终怎么被解析出来」这一个问题的答案，分散两处会让改
//! 优先级的人只改一半。

use serde_json::{Map, Value};

use super::types::*;

/// 环境变量里的 API Key（新名优先，旧名兼容读；去空白，空串当未配置）
pub(super) fn env_api_key() -> Option<String> {
    env_api_key_value()
}

/// 两个候选名按序取第一个有效值（`AGENT2API_PROXY_API_KEY` > 旧名）
pub(crate) fn env_api_key_value() -> Option<String> {
    for name in ["AGENT2API_PROXY_API_KEY", "WORKBUDDY_PROXY_API_KEY"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

/// 环境变量里的非空字符串
pub(super) fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 登录页人机验证组件环境变量，默认为1开启，0为关闭。
///
/// 只有配置里**没有** `captchaEnabled`（设置页从未改过）时才兜底 —— 优先级
/// 「配置里的值 > 环境变量 > 内置默认」见模块头。除字面 `0` 之外一律按开启
/// 处理：它是登录 / 注册的防爆破开关，写坏了宁可多一道校验。
pub(super) fn env_captcha_enabled() -> bool {
    std::env::var("AGENT2API_CAPTCHA_ENABLED")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

/// 从原始 JSON 里取非空字符串字段
pub(super) fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 从原始 JSON 里取「带范围边界的整数」：缺字段 / 类型不符 / 非整数 / 越界
/// 一律回落到 `default`。
///
/// **越界回落而非夹紧**：手改 config.json 写了天文数字时，静默夹紧会让人
/// 以为「设置生效了」，回默认值至少与用户在设置页看到的不一致时好排查。
/// 走接口写入的值在各自的 PUT 处理器里已按同一范围校验过（那里是 400 报错）。
///
/// 接受 `30.0` 这种整值浮点（与 `parse_days` 同一口径）：JSON 里
/// `30` 与 `30.0` 都是合法数字，为后者回落到默认值会显得莫名其妙
/// （用户手改文件时把 30 写成 30.0 是完全可能的事）。
pub(super) fn bounded_int_field(map: &Map<String, Value>, key: &str, default: i64, min: i64, max: i64) -> i64 {
    let parsed = map.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        }),
        _ => None,
    });
    parsed.filter(|value| (min..=max).contains(value)).unwrap_or(default)
}

/// 从原始 JSON 里取天数（`bounded_int_field` 的保留期特化）
pub(super) fn days_field(map: &Map<String, Value>, key: &str, default: i64) -> i64 {
    bounded_int_field(map, key, default, RETENTION_MIN_DAYS, RETENTION_MAX_DAYS)
}

/// 由原始 JSON 解析三档保留天数（缺字段各自用默认值）
pub(super) fn retention_from(map: &Map<String, Value>) -> RetentionSettings {
    let defaults = RetentionSettings::default();
    RetentionSettings {
        log_days: days_field(map, KEY_LOG_RETENTION_DAYS, defaults.log_days),
        request_days: days_field(map, KEY_REQUEST_RETENTION_DAYS, defaults.request_days),
        daily_days: days_field(map, KEY_DAILY_RETENTION_DAYS, defaults.daily_days),
    }
}

// ─── 间隔型定时任务的解析（scheduledTasks.*）──────────────────

/// 从 `scheduledTasks` 里取一条任务的原始子对象；缺失 / 类型不符当空对象
/// （于是 `enabled` 用默认值、`interval` 也用默认值，与「用户没配过」等价）。
pub(super) fn task_object(map: &Map<String, Value>, key: &str) -> Map<String, Value> {
    map.get(KEY_SCHEDULED_TASKS)
        .and_then(Value::as_object)
        .and_then(|tasks| tasks.get(key))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// 从一条任务的子对象里取间隔值。
///
/// 与 `days_field` 同一口径（**越界回落而非夹紧**、接受整值浮点、只在
/// `min..=max` 内才采纳），只是范围由调用方给：两类任务的合理区间差着量级
/// （后端维护按分钟、前端刷新按秒），共用一份范围常量会逼其中一类放宽边界。
pub(super) fn interval_field(task: &Map<String, Value>, default: i64, min: i64, max: i64) -> i64 {
    let parsed = task.get("interval").and_then(|value| match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        }),
        _ => None,
    });
    parsed.filter(|value| (min..=max).contains(value)).unwrap_or(default)
}

/// 从一条任务的子对象里取开关。
///
/// **缺字段按开启**：这些任务在本次改造前都是无条件运行的（凭证维护每 10 分钟、
/// 两个前端面板每 10 秒、模型刷新随 /v1/models 触发；定时查询积分是后来新增的，
/// 它没有「改造前」—— 缺字段同样按开启，与新装用户第一次打开的行为一致），
/// 升级上来的 config.json 里没有 `scheduledTasks` —— 若把「没配过」读成「关闭」，
/// 用户什么也没动，凭证却不再自动续期了。写侧（`PUT /api/scheduled-tasks`）
/// 则要求显式布尔值。
pub(super) fn task_enabled(task: &Map<String, Value>, default: bool) -> bool {
    task.get("enabled").and_then(Value::as_bool).unwrap_or(default)
}

/// 由原始 JSON 解析六条间隔型任务（缺字段各自用默认值）
pub(super) fn scheduled_from(map: &Map<String, Value>) -> ScheduledSettings {
    let defaults = ScheduledSettings::default();
    let task = |key: &str, interval: i64, min: i64, max: i64| {
        let object = task_object(map, key);
        IntervalTask {
            enabled: task_enabled(&object, true),
            interval: interval_field(&object, interval, min, max),
        }
    };
    ScheduledSettings {
        credential_maintenance: task(
            KEY_CREDENTIAL_MAINTENANCE,
            defaults.credential_maintenance.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
        model_refresh: task(
            KEY_MODEL_REFRESH,
            defaults.model_refresh.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
        logs_auto_refresh: task(
            KEY_LOGS_AUTO_REFRESH,
            defaults.logs_auto_refresh.interval,
            INTERVAL_MIN_SECONDS,
            INTERVAL_MAX_SECONDS,
        ),
        requests_auto_refresh: task(
            KEY_REQUESTS_AUTO_REFRESH,
            defaults.requests_auto_refresh.interval,
            INTERVAL_MIN_SECONDS,
            INTERVAL_MAX_SECONDS,
        ),
        report_auto_refresh: task(
            KEY_REPORT_AUTO_REFRESH,
            defaults.report_auto_refresh.interval,
            INTERVAL_MIN_SECONDS,
            INTERVAL_MAX_SECONDS,
        ),
        update_check: task(
            KEY_UPDATE_CHECK,
            defaults.update_check.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
        usage_query: task(
            KEY_USAGE_QUERY,
            defaults.usage_query.interval,
            INTERVAL_MIN_MINUTES,
            INTERVAL_MAX_MINUTES,
        ),
    }
}

// ─── 请求重试设置的解析（retryCount / retryCrossProviderCount / retryIntervalSeconds / noRetryStatusCodes）──

/// 由原始 JSON 解析请求重试设置（缺字段各自用默认值）
pub(super) fn retry_from(map: &Map<String, Value>) -> RetrySettings {
    let defaults = RetrySettings::default();
    RetrySettings {
        count: bounded_int_field(
            map,
            KEY_RETRY_COUNT,
            defaults.count,
            RETRY_MIN_COUNT,
            RETRY_MAX_COUNT,
        ),
        account_switch_count: bounded_int_field(
            map,
            KEY_RETRY_ACCOUNT_SWITCH_COUNT,
            defaults.account_switch_count,
            RETRY_MIN_COUNT,
            RETRY_MAX_COUNT,
        ),
        interval_seconds: bounded_int_field(
            map,
            KEY_RETRY_INTERVAL_SECONDS,
            defaults.interval_seconds,
            RETRY_MIN_INTERVAL_SECONDS,
            RETRY_MAX_INTERVAL_SECONDS,
        ),
        no_retry_codes: no_retry_codes_field(map, KEY_RETRY_NO_RETRY_CODES),
    }
}

/// 「指定错误码直接换号」名单的解析。
///
/// ── 各形态的落点 ────────────────────────────────────────────
///   - 键缺失 / 不是数组 → **默认名单**（[402]）：配置从没写过这一项是常态，
///     默认值就该在这时出现；整体写坏（手改成了字符串）与「没配过」无法区分，
///     按同一口径处理（回落默认，见模块头）。
///   - 空数组 → **空名单**：这是用户的明确选择（任何错误都照常重试），
///     必须与「没配过」区分开。
///   - 数组里的垃圾项（非整数 / 越出 100–599）→ **丢弃该项**：接口写入侧
///     已整单校验（400），能到这里的坏项只来自手改库，跳过比整单回落默认
///     更接近手改者的本意。排序去重：判定只问 contains，顺序无意义，
///     存一份规整形态让手改过的 config.json 也保持可读。
fn no_retry_codes_field(map: &Map<String, Value>, key: &str) -> std::sync::Arc<[u16]> {
    let Some(Value::Array(items)) = map.get(key) else {
        return std::sync::Arc::from(DEFAULT_NO_RETRY_CODES);
    };
    let mut codes: Vec<u16> = items
        .iter()
        .filter_map(|item| {
            let number = item.as_i64().or_else(|| {
                item.as_f64()
                    .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                    .map(|raw| raw as i64)
            });
            u16::try_from(number?)
                .ok()
                .filter(|code| (RETRY_CODE_MIN..=RETRY_CODE_MAX).contains(code))
        })
        .collect();
    codes.sort_unstable();
    codes.dedup();
    std::sync::Arc::from(codes)
}

// ─── 上游请求超时的解析（四项，单位秒）────────────────────────

/// 由原始 JSON 解析四项超时（缺字段各自用默认值；越界回落默认，同模块头口径）。
///
/// 四项的键名与默认值见 `types.rs`——「范围 1~3600」在写侧（timeouts_api）
/// 是 400 报错，在这里是回落默认：手改库把 0 或 99999 写进去时，宁可回到
/// 30/300 也不要让转发层拿到一个必然坏事的值（0 毫秒等于禁用该阶段保护）。
pub(super) fn timeouts_from(map: &Map<String, Value>) -> TimeoutSettings {
    let defaults = TimeoutSettings::default();
    TimeoutSettings {
        connect_seconds: bounded_int_field(
            map,
            KEY_TIMEOUT_CONNECT_SECONDS,
            defaults.connect_seconds,
            TIMEOUT_MIN_SECONDS,
            TIMEOUT_MAX_SECONDS,
        ),
        headers_seconds: bounded_int_field(
            map,
            KEY_TIMEOUT_HEADERS_SECONDS,
            defaults.headers_seconds,
            TIMEOUT_MIN_SECONDS,
            TIMEOUT_MAX_SECONDS,
        ),
        stream_idle_seconds: bounded_int_field(
            map,
            KEY_TIMEOUT_STREAM_IDLE_SECONDS,
            defaults.stream_idle_seconds,
            TIMEOUT_MIN_SECONDS,
            TIMEOUT_MAX_SECONDS,
        ),
        body_seconds: bounded_int_field(
            map,
            KEY_TIMEOUT_BODY_SECONDS,
            defaults.body_seconds,
            TIMEOUT_MIN_SECONDS,
            TIMEOUT_MAX_SECONDS,
        ),
    }
}

// ─── 历史路由优先级（providerRoute，只读，供账号迁移）───────────
