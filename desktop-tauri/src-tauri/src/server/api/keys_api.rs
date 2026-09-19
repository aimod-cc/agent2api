//! 网关 API Key 管理（网关 Key 页）：
//!
//! - `GET    /api/keys`          → `{keys: [{id,name,key,masked,enabled,createdAt}], authRequired}`
//! - `POST   /api/keys`          `{name?, key?}` 新增；key 留空自动生成
//! - `PATCH  /api/keys/{id}`     `{name?, enabled?}`
//! - `DELETE /api/keys/{id}`
//!
//! 写接口都返回最新列表。明文 key 随列表返回：这是本机管理界面，用户要能随时复制
//! 某把 Key 给某个客户端；掩码只用于列表折叠展示。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::api_keys;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

fn list_json() -> Value {
    let keys: Vec<Value> = api_keys::list().iter().map(api_keys::ApiKeyEntry::public_json).collect();
    json!({
        "keys": keys,
        "authRequired": config::current().api_key_set(),
    })
}

fn body_object(body: &Bytes) -> Result<serde_json::Map<String, Value>, Response> {
    let payload = parse_body(body).map_err(|error| errors::management_error(400, error.message))?;
    payload
        .as_object()
        .cloned()
        .ok_or_else(|| errors::management_error(400, "请求体必须是 JSON 对象"))
}

/// GET /api/keys
pub async fn list_keys(State(_state): State<ServerState>) -> Response {
    ok_json(list_json())
}

/// POST /api/keys
pub async fn create_key(State(_state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let name = object.get("name").and_then(Value::as_str).unwrap_or("");
    let key = object.get("key").and_then(Value::as_str);
    match api_keys::add(name, key) {
        Ok(entry) => {
            logging::log("[Config]", &format!("✅ 新增 API Key「{}」，客户端需带 Authorization: Bearer <key>", entry.name));
            let mut payload = list_json();
            if let Some(map) = payload.as_object_mut() {
                map.insert("created".to_string(), entry.public_json());
            }
            ok_json(payload)
        }
        Err(message) => errors::management_error(400, message),
    }
}

/// PATCH /api/keys/{id}
pub async fn update_key(State(_state): State<ServerState>, Path(id): Path<String>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let name = object.get("name").and_then(Value::as_str);
    let enabled = object.get("enabled").and_then(Value::as_bool);
    match api_keys::update(&id, name, enabled) {
        Ok(entry) => {
            logging::log(
                "[Config]",
                &format!("API Key「{}」已更新（{}）", entry.name, if entry.enabled { "启用" } else { "停用" }),
            );
            ok_json(list_json())
        }
        Err(message) => errors::management_error(404, message),
    }
}

/// DELETE /api/keys/{id}
pub async fn delete_key(State(_state): State<ServerState>, Path(id): Path<String>) -> Response {
    match api_keys::remove(&id) {
        Ok(()) => {
            let remaining = config::current().api_key_set();
            logging::log(
                "[Config]",
                if remaining { "API Key 已删除" } else { "API Key 已删除，接口恢复免鉴权（仅监听 127.0.0.1）" },
            );
            ok_json(list_json())
        }
        Err(message) => errors::management_error(404, message),
    }
}
