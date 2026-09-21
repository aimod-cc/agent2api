//! 模型管理 API（模型管理页）：
//!
//! - `GET  /api/models/manage`          → `{models, mappings, reasoningLevels}`（含禁用 / 隐藏条目）
//! - `POST /api/models/state`           `{id, enabled?, hidden?}` 启停 / 隐藏（删除）/ 恢复
//! - `POST /api/models/mappings`        `{alias, target, reasoning?}` 新增映射 / 改思考等级
//! - `POST /api/models/mappings/remove` `{alias, target, provider?}` 删除映射
//!
//! 写接口都返回最新的 `{models, mappings, reasoningLevels}`，前端就地重绘、不必再拉一次。
//!
//! `reasoning` 是「照抄 OmniProxy 的手动思考等级绑定」（表见
//! `model_rules::REASONING_LEVELS`，随 manage 响应一并发给前端，前端不自己
//! 抄一份）。**绑定已接入转发**：等级跟着它所在的那条映射走，由承载那家的
//! 适配器翻译成本家上游认识的档位字段（各家的规则与「哪些情况故意不注入」见
//! `model_rules::reasoning` 的模块头）。接口形状与转发无关 —— 转发侧读的是
//! `ModelRules` 里的同一条映射，这里一行都不用改。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::Value;

use crate::server::core::model_rules;
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
///
/// 新增映射（照抄 OmniProxy 的模型映射）：`{alias, target, provider?, reasoning?}`。
/// `alias` 是对外名，**自由命名** —— 允许与任何上游模型 id 同名（同名时该
/// 上游的原生路由仍然优先，映射是追加的兜底路）；同一 alias 可以在不同提供商
/// 各建一条，路由时一起进候选链主备切换。`provider` 是 target 所属的家；
/// 旧版前端不传（全局语义：由所有承载 target 的家接收），行为不变。
///
/// ── `reasoning` 的三态（与 `provider` 的二态写法不同，别简写）──────
/// 前端在**改已有映射的等级**时走的也是这条接口（三元组相同 = 命中同一条），
/// 所以「请求体里有没有这个键」必须能被区分出来：
///   - 键**缺失**     → `None`，不动已有的等级（旧前端与老调用点的行为）；
///   - 键给空串/null  → `Some(None)`，显式清成「不覆盖」；
///   - 键给字符串     → `Some(Some(x))`，设成 x（表外自定义值也放行，
///                       由 `model_rules::normalize_reasoning` 只做长度约束）。
/// 一律按「空 = 清空」处理会让旧前端（它不传这个键）每次建映射都把可能存在的
/// 绑定顺手清掉 —— 那是静默的数据丢失。
pub async fn add_mapping(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let alias = text_field(&object, "alias");
    let target = text_field(&object, "target");
    let provider = text_field(&object, "provider");
    if !model_rules::alias_valid(&alias) {
        return errors::management_error(400, "映射名只能包含字母、数字与 - _ . / :，且不超过 128 个字符");
    }
    if target.is_empty() {
        return errors::management_error(400, "缺少目标上游模型");
    }
    let provider_opt = if provider.is_empty() { None } else { Some(provider.as_str()) };
    if let Some(kind) = provider_opt {
        // provider 必须是注册表里的家：它决定候选链里追加谁，写错名字会让
        // 映射悄悄变成一条永远路由不到的死路
        if !crate::server::core::providers::kind_from_id(kind).is_some() {
            return errors::management_error(400, format!("未知的提供商: {kind}"));
        }
    }
    // 思考等级的三态见函数头；超长的值在这里就拒掉（落盘前拦截，不留一条
    // 读回来会被 `normalize_reasoning` 丢掉的脏数据）
    let reasoning = match object.get("reasoning") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                Some(None)
            } else if model_rules::normalize_reasoning(text).is_none() {
                return errors::management_error(400, "思考等级过长（最多 32 个字符）");
            } else {
                Some(Some(text))
            }
        }
        Some(_) => return errors::management_error(400, "reasoning 必须是字符串或 null"),
    };
    model_rules::add_mapping(&alias, &target, provider_opt, reasoning);
    let subject = match provider_opt {
        Some(name) => format!("{alias} → {target}（{name}）"),
        None => format!("{alias} → {target}"),
    };
    // 日志把等级一并写出来（改等级走的也是这条接口，不说出来日志里看不出区别）
    let reasoning_text = match reasoning.flatten() {
        Some(level) => format!("，思考等级 {level}"),
        None => String::new(),
    };
    logging::log("[Models]", &format!("保存映射 {subject}{reasoning_text}"));
    ok_json(catalog::manage_view(state.store()))
}

/// POST /api/models/mappings/remove
///
/// 删除一条映射。同名映射允许多条后按 alias 删会有歧义，所以按
/// `{alias, target, provider?}` 三元组精确定位（与新增同一套键）。
///
/// 旧版全局条目（provider 缺失）对任何家都命中（与展示同口径），指名某家
/// 删除时由 `remove_mapping` 展开成其余承载家 —— 所以这里要传「当前清单里
/// 同样承载 target 的其他提供商」，与 `set_state` 取 `others` 同一处。
pub async fn remove_mapping(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let alias = text_field(&object, "alias");
    let target = text_field(&object, "target");
    let provider = text_field(&object, "provider");
    if alias.is_empty() || target.is_empty() {
        return errors::management_error(400, "缺少映射名或目标上游模型");
    }
    let provider_opt = if provider.is_empty() { None } else { Some(provider.as_str()) };
    let others: Vec<String> = if provider_opt.is_some() {
        crate::server::core::providers::catalog::providers_for_model(&target)
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
    let (_, removed) = model_rules::remove_mapping(&alias, &target, provider_opt, &others);
    if !removed {
        return errors::management_error(404, format!("映射不存在: {alias} → {target}"));
    }
    logging::log("[Models]", &format!("删除映射 {alias} → {target}"));
    ok_json(catalog::manage_view(state.store()))
}
