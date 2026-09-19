//! 模型管理 API（模型管理页）：
//!
//! - `GET  /api/models/manage`          → `{models, mappings}`（含禁用 / 隐藏条目）
//! - `POST /api/models/state`           `{id, enabled?, hidden?}` 启停 / 隐藏（删除）/ 恢复
//! - `POST /api/models/mappings`        `{alias, target}` 新增映射（同名 alias 覆盖）
//! - `POST /api/models/mappings/remove` `{alias}` 删除映射
//!
//! 写接口都返回最新的 `{models, mappings}`，前端就地重绘、不必再拉一次。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::Value;

use crate::server::core::model_rules;
use crate::server::core::models::model_id;
use crate::server::core::providers::catalog;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

fn body_object(body: &Bytes) -> Result<serde_json::Map<String, Value>, Response> {
    let payload = parse_body(body).map_err(|error| errors::management_error(400, error.message))?;
    payload
        .as_object()
        .cloned()
        .ok_or_else(|| errors::management_error(400, "请求体必须是 JSON 对象"))
}

fn text_field(object: &serde_json::Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 上游模型 id 是否存在于当前清单（忽略大小写）
fn model_exists(state: &ServerState, id: &str) -> bool {
    catalog::manage_view(state.store())
        .get("models")
        .and_then(Value::as_array)
        .map(|items| items.iter().any(|item| model_id(item).eq_ignore_ascii_case(id)))
        .unwrap_or(false)
}

/// GET /api/models/manage
pub async fn get_manage(State(state): State<ServerState>) -> Response {
    ok_json(catalog::manage_view(state.store()))
}

/// POST /api/models/state
pub async fn set_state(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let id = text_field(&object, "id");
    if id.is_empty() {
        return errors::management_error(400, "缺少模型 id");
    }
    let enabled = object.get("enabled").and_then(Value::as_bool);
    let hidden = object.get("hidden").and_then(Value::as_bool);
    if enabled.is_none() && hidden.is_none() {
        return errors::management_error(400, "enabled / hidden 至少给一项");
    }
    // 目标提供商：新版前端总是带着（启停粒度是「提供商 × 模型 id」）；
    // 缺省走旧版全局语义 —— 只有旧版前端（升级前）会这么传
    let provider = text_field(&object, "provider");
    let provider_opt = if provider.is_empty() { None } else { Some(provider.as_str()) };
    // 启用某一家时可能要把旧版的全局条目展开成「其余各家」，这里给出当前
    // 清单里同样承载该模型的其他提供商（目录的匹配口径：先 id 后 name）
    let others: Vec<String> = if enabled == Some(true) {
        crate::server::core::providers::catalog::providers_for_model(&id)
            .into_iter()
            .map(crate::server::core::providers::kind_id)
            .filter(|kind_provider| {
                Some(*kind_provider) != provider_opt.map(str::to_string).as_deref()
            })
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    model_rules::set_state(provider_opt, &id, enabled, hidden, &others);
    // 日志里把提供商带上：同名模型在多家同时存在时，单看 id 分不清动的是哪家
    let subject = match provider_opt {
        Some(name) => format!("[{name}] {id}"),
        None => id.clone(),
    };
    let what = match (enabled, hidden) {
        (_, Some(true)) => "已删除（隐藏）",
        (_, Some(false)) => "已恢复",
        (Some(true), _) => "已启用",
        (Some(false), _) => "已禁用",
        _ => "已更新",
    };
    logging::log("[Models]", &format!("模型 {subject} {what}"));
    ok_json(catalog::manage_view(state.store()))
}

/// POST /api/models/mappings
pub async fn add_mapping(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let alias = text_field(&object, "alias");
    let target = text_field(&object, "target");
    if !model_rules::alias_valid(&alias) {
        return errors::management_error(400, "映射名只能包含字母、数字与 - _ . / :，且不超过 128 个字符");
    }
    if target.is_empty() {
        return errors::management_error(400, "缺少目标上游模型");
    }
    if !model_exists(&state, &target) {
        return errors::management_error(400, format!("目标模型不存在: {target}"));
    }
    if model_exists(&state, &alias) {
        return errors::management_error(400, format!("映射名不能与上游模型 id 同名: {alias}"));
    }
    model_rules::add_mapping(&alias, &target);
    logging::log("[Models]", &format!("新增映射 {alias} → {target}"));
    ok_json(catalog::manage_view(state.store()))
}

/// POST /api/models/mappings/remove
pub async fn remove_mapping(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let alias = text_field(&object, "alias");
    if alias.is_empty() {
        return errors::management_error(400, "缺少映射名");
    }
    let (_, removed) = model_rules::remove_mapping(&alias);
    if !removed {
        return errors::management_error(404, format!("映射不存在: {alias}"));
    }
    logging::log("[Models]", &format!("删除映射 {alias}"));
    ok_json(catalog::manage_view(state.store()))
}
