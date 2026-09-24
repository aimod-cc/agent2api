//! GET/PUT /api/retry —— 请求重试设置（两档次数 / 间隔 / 指定错误码直接换号）。
//!
//! 转发层对上游瞬时错误与传输层失败做「睡一个间隔再原样重发」，次数分两档
//! （见 `config.rs` 的说明）：
//!   - `retryCount`：在**同一个账号**上原地重发几次；
//!   - `retryCrossProviderCount`：失败后**最多换几个账号**再试（按账号计，
//!     不分家 —— 键名是旧措辞，语义见 `config.rs`）。
//!   - `noRetryStatusCodes`：指定上游状态码直接换号（不在同一账号重发，
//!     按队列换下一个账号继续试，见 `config.rs` 的 `KEY_RETRY_NO_RETRY_CODES`）。
//!
//! 循环见 `upstream::provider_loop`（原地重发在 `send_with_retry`，
//! 换账号在 `attempt_queue`）。
//!
//! 三项统一存在 config.json（键名契约见 `config.rs`），由设置页
//! 「通用 → 请求重试」经 HTTP 桥读写 —— 与 /api/retention 同一模式：
//! 独立端点、允许部分字段、返回生效后的全量值。
//!
//! 与 /api/retention 的差别：这条**没有副作用**（不清理数据、不重启进程），
//! 保存后对下一个失败请求立即生效（转发层每次重试判定都读内存快照，不读盘）。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{
    self, RetryPatch, RetrySettings, KEY_RETRY_ACCOUNT_SWITCH_COUNT, KEY_RETRY_COUNT,
    KEY_RETRY_INTERVAL_SECONDS, KEY_RETRY_NO_RETRY_CODES, RETRY_CODE_MAX, RETRY_CODE_MIN,
    RETRY_MAX_COUNT, RETRY_MAX_INTERVAL_SECONDS, RETRY_MAX_NO_RETRY_CODES, RETRY_MIN_COUNT,
    RETRY_MIN_INTERVAL_SECONDS,
};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/retry
pub async fn get_retry(State(_state): State<ServerState>) -> Response {
    ok_json(retry_json(config::retry_settings()))
}

/// PUT /api/retry —— body `{retryCount?, retryCrossProviderCount?, retryIntervalSeconds?, noRetryStatusCodes?}`
///
/// 允许部分字段（未出现的项保持原值，null 同义）。校验通过后：写 config.json
/// → 返回**生效后**的值（前端直接用响应刷新界面，不必再 GET 一次）。
pub async fn put_retry(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };

    // ── 校验：全部先过一遍再写盘（与 put_retention 同一顺序）─────────
    // 三项里有一项非法时整体不落盘，避免出现「次数改了、间隔没改」的半套设置
    let mut patch = RetryPatch::default();
    let mut targets = [
        (
            KEY_RETRY_COUNT,
            object.get(KEY_RETRY_COUNT),
            RETRY_MIN_COUNT,
            RETRY_MAX_COUNT,
            &mut patch.count,
        ),
        (
            KEY_RETRY_ACCOUNT_SWITCH_COUNT,
            object.get(KEY_RETRY_ACCOUNT_SWITCH_COUNT),
            RETRY_MIN_COUNT,
            RETRY_MAX_COUNT,
            &mut patch.account_switch_count,
        ),
        (
            KEY_RETRY_INTERVAL_SECONDS,
            object.get(KEY_RETRY_INTERVAL_SECONDS),
            RETRY_MIN_INTERVAL_SECONDS,
            RETRY_MAX_INTERVAL_SECONDS,
            &mut patch.interval_seconds,
        ),
    ];
    let mut updated = false;
    for (key, value, min, max, slot) in targets.iter_mut() {
        let Some(value) = value else {
            continue;
        };
        // null 视为「这一项不改」：前端整份回传时未填项常见为 null
        if value.is_null() {
            continue;
        }
        match parse_bounded_int(key, value, *min, *max) {
            Ok(number) => {
                **slot = Some(number);
                updated = true;
            }
            Err(message) => return errors::management_error(400, message),
        }
    }

    // ── 第四项：指定错误码直接换号（整数数组）─────────────────────
    // 与三个数字项同一顺序：先整单校验，非法整体不落盘。允许 null / 缺省
    // （这一项不动）；空数组是合法值 = 清空名单（任何错误都照常重试）。
    if let Some(value) = object.get(KEY_RETRY_NO_RETRY_CODES) {
        if !value.is_null() {
            match parse_no_retry_codes(value) {
                Ok(codes) => {
                    patch.no_retry_codes = Some(codes);
                    updated = true;
                }
                Err(message) => return errors::management_error(400, message),
            }
        }
    }

    if updated && !config::set_retry(patch) {
        // 写盘失败：内存快照已更新（本次运行仍生效），但重启后会回到旧值 ——
        // 必须让用户知道，否则「改了设置重启又变回去」会被当成玄学问题
        logging::log("[Config]", "⚠️  请求重试设置写入 config.json 失败，本次运行内仍生效");
    }

    let settings = config::retry_settings();
    if updated {
        logging::log(
            "[Config]",
            &format!(
                "请求重试已更新: 同一账号 {} 次 / 最多换 {} 个账号 / 间隔 {} 秒 / 直接换号错误码 {:?}",
                settings.count,
                settings.account_switch_count,
                settings.interval_seconds,
                settings.no_retry_codes,
            ),
        );
    }
    ok_json(retry_json(settings))
}

/// 四个重试字段的响应体（GET 与 PUT 共用，键名与 config.rs 的常量必然一致 ——
/// 与 `stats_api::retention_json` 同一手法：键用常量标识符而不是手写字符串）。
///
/// 键名对前端是**契约**（`settings-panel.js` 的 RETRY_FIELDS 逐字对齐），
/// 其中第二档仍是旧措辞 `retryCrossProviderCount`（配置兼容，见 config.rs）。
fn retry_json(settings: RetrySettings) -> Value {
    json!({
        KEY_RETRY_COUNT: settings.count,
        KEY_RETRY_ACCOUNT_SWITCH_COUNT: settings.account_switch_count,
        KEY_RETRY_INTERVAL_SECONDS: settings.interval_seconds,
        KEY_RETRY_NO_RETRY_CODES: settings.no_retry_codes.to_vec(),
    })
}

/// 「指定错误码直接换号」的校验：整数数组、每项 100–599、最多 50 项。
///
/// 与 `parse_bounded_int` 同一口径：只认 JSON 数字（含 `402.0` 这种整值浮点）；
/// 有一项非法就整单 400（不悄悄剔除），写盘前排序去重，落库形态规整。
fn parse_no_retry_codes(value: &Value) -> Result<Vec<u16>, String> {
    let Some(items) = value.as_array() else {
        return Err(format!("{KEY_RETRY_NO_RETRY_CODES} 必须是状态码数组（收到: {value}）"));
    };
    if items.len() > RETRY_MAX_NO_RETRY_CODES {
        return Err(format!(
            "{KEY_RETRY_NO_RETRY_CODES} 最多 {} 项（收到 {} 项）",
            RETRY_MAX_NO_RETRY_CODES,
            items.len()
        ));
    }
    let mut codes = Vec::with_capacity(items.len());
    for item in items {
        let number = item.as_i64().or_else(|| {
            item.as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        });
        let Some(number) = number else {
            return Err(format!("{KEY_RETRY_NO_RETRY_CODES} 里必须是整数（收到: {item}）"));
        };
        if !(i64::from(RETRY_CODE_MIN)..=i64::from(RETRY_CODE_MAX)).contains(&number) {
            return Err(format!(
                "{KEY_RETRY_NO_RETRY_CODES} 里的值必须是 {}-{} 的 HTTP 状态码（收到: {number}）",
                RETRY_CODE_MIN, RETRY_CODE_MAX
            ));
        }
        codes.push(number as u16);
    }
    codes.sort_unstable();
    codes.dedup();
    Ok(codes)
}

/// 单个数字字段的校验：必须是 min–max 的整数，否则给出可读的 400 文案。
///
/// 与 `stats_api::parse_days` 同一口径：只认 JSON 数字（含 `5.0` 这种整值
/// 浮点，容忍 JS 的 number 形态）；字符串 `"5"` 视为非法。
fn parse_bounded_int(key: &str, value: &Value, min: i64, max: i64) -> Result<i64, String> {
    let number = match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
                .map(|raw| raw as i64)
        }),
        _ => None,
    };
    match number {
        Some(number) if (min..=max).contains(&number) => Ok(number),
        _ => Err(format!("{key} 必须是 {min}-{max} 的整数（收到: {value}）")),
    }
}
