//! 机器人校验开关：`GET/PUT /api/captcha`。
//!
//! ── 这个开关做什么 ──────────────────────────────────────────
//! 控制面板登录 / 注册端点的 ALTCHA proof-of-work 校验（见 `server::altcha`）：
//! 开启时，登录页必须先领题、浏览器后台算完、随请求提交 payload 才能进门；
//! 关闭时两个端点不再校验（challenge 端点也回 400）。**默认开启** ——
//! 登录与注册是公开的认证边界，脚本暴破 / 抢注的门槛宁可多一道不可少一道。
//!
//! 与 `/api/sanitize` 同形：GET 读、PUT 写、响应体就是新状态，设置页开关
//! 的读写代码共用一套模式。写盘位置是 config 的 `captchaEnabled` 键，
//! 改完下一个请求立即生效，不重启进程。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

fn captcha_json() -> Value {
    json!({ "captchaEnabled": config::current().captcha_enabled() })
}

/// GET /api/captcha —— 当前开关状态
pub async fn get_captcha(State(_state): State<ServerState>) -> Response {
    ok_json(captcha_json())
}

/// PUT /api/captcha —— body `{captchaEnabled: true|false}`
pub async fn put_captcha(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let Some(value) = object.get(config::KEY_CAPTCHA_ENABLED) else {
        return errors::management_error(400, "缺少 captchaEnabled 字段");
    };
    let Some(enabled) = value.as_bool() else {
        return errors::management_error(400, "captchaEnabled 必须是布尔值");
    };
    if !config::set_captcha_enabled(enabled) {
        return errors::management_error(500, "写入配置失败，请重试");
    }
    logging::log(
        "[Security]",
        if enabled {
            "机器人校验已开启：登录 / 注册需要 ALTCHA proof-of-work"
        } else {
            "⚠️ 机器人校验已关闭：登录 / 注册不再校验 proof-of-work"
        },
    );
    ok_json(captcha_json())
}
