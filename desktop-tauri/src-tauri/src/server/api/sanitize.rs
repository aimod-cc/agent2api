//! 出站指纹脱敏开关：`GET/PUT /api/sanitize`。
//!
//! ── 这个开关做什么 ──────────────────────────────────────────
//! 开启后转发层在每次出站前剥离上游内容审核的黑名单指纹（见 `core::sanitize`）：
//! 表头键值整段删除、承载语义的模板句最小改写（每句只换一个词，语义不变）。
//! 关掉它等于把客户端 system 模板原样发给上游 —— 那正是模板句被误拦
//! （HTTP 400 code=11128）的原因，所以**默认开启**。
//!
//! ── 为什么只有一个开关，没有词表端点 ─────────────────────────
//! 规则集是硬编码的（照搬 workbuddy2api 的 `sanitize.go`），用户没有可维护的
//! 词表：改造前的 `/api/desensitize*` 八条端点（词表增删改、作用角色、作用
//! 提供商、命中统计、远程同步）**已随本次改造整体删除**。留下的是一个与
//! `/api/debug` 同形的单开关端点 —— 两者的形态刻意保持一致（GET 读、PUT 写、
//! 响应体就是新状态），设置页那两个开关的读写代码因此能共用一套模式。
//!
//! ── 写盘位置 ────────────────────────────────────────────────
//! `config` 的 `sanitizeBlacklistFingerprints` 键（统一库 `kv` 表里的配置行）。
//! 转发层逐请求读快照，改完下一个请求立即生效，不重启进程。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/sanitize —— 当前开关状态
pub async fn get_sanitize(State(_state): State<ServerState>) -> Response {
    ok_json(sanitize_json())
}

/// PUT /api/sanitize —— body `{sanitizeBlacklistFingerprints: true|false}`
///
/// 写配置，下一个请求立即生效（转发层逐请求读快照，不重启进程）。
/// 关闭时在事件日志里留一条记录 —— 关掉它意味着客户端模板原样出站，
/// 用户该在日志里看得到这件事发生过。
pub async fn put_sanitize(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let key = config::KEY_SANITIZE_FINGERPRINTS;
    let Some(value) = object.get(key) else {
        return errors::management_error(400, format!("缺少 {key}"));
    };
    let Some(enabled) = value.as_bool() else {
        return errors::management_error(400, format!("{key} 必须是 true 或 false"));
    };
    if !config::set_sanitize_fingerprints(enabled) {
        logging::log(
            "[Config]",
            "⚠️  出站指纹脱敏设置写入失败，本次运行内仍生效",
        );
    }
    logging::log(
        "[Config]",
        if enabled {
            "出站指纹脱敏已开启：剥离上游审核黑名单指纹"
        } else {
            "出站指纹脱敏已关闭：客户端 system 模板将原样发送，可能被上游误拦"
        },
    );
    ok_json(sanitize_json())
}

/// 开关状态（GET 与 PUT 共用，前端直接用响应刷新界面）
fn sanitize_json() -> Value {
    json!({
        config::KEY_SANITIZE_FINGERPRINTS: config::current().sanitize_fingerprints(),
    })
}
