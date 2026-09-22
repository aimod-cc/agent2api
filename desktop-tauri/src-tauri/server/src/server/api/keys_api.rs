//! 网关 API Key 管理（网关 Key 页）：
//!
//! - `GET    /api/keys`          → `{keys: [{id,name,key,masked,enabled,createdAt,
//!                                  allowedProviders,allowedModels}], authRequired,
//!                                  providers, modelsByProvider}`
//! - `POST   /api/keys`          `{name?, key?, allowedProviders?, allowedModels?}` 新增；
//!                               key 留空自动生成
//! - `PATCH  /api/keys/{id}`     `{name?, enabled?, allowedProviders?, allowedModels?}`
//! - `DELETE /api/keys/{id}`
//!
//! 写接口都返回最新列表。明文 key 随列表返回：这是本机管理界面，用户要能随时复制
//! 某把 Key 给某个客户端；掩码只用于列表折叠展示。
//!
//! ── R9：可用提供商 / 可用模型（参考 OmniProxy）──────────────────
//! 两个白名单数组随记录读写，**空数组 = 不限制**（语义与字段说明见
//! `core::api_keys` 的模块头）。列表响应额外带两个候选表，让界面**不必维护
//! 第二份事实**：
//!   · `providers`（注册表摘要）—— 「可用提供商」多选的候选项。项目里明确禁止
//!     维护第二份 provider 清单（见 `account_store` 模块头那段），前端不该烤一份
//!     写死的 id 列表；
//!   · `modelsByProvider`（每家 → 对外名清单）—— 「可用模型」多选的候选项。
//!     界面按用户当前勾了哪几家取并集（一家没勾就是空，见 `keys-panel.js`）。
//!     这张表必须由后端给：模型名是各家清单 + 映射别名的合并结果，前端拿不到
//!     这层信息，而 `/api/session` 的 `models` 是跨家**去重**后的对外视图 ——
//!     按家过滤它会漏掉被别家认领的同名模型（论证见
//!     `catalog::models_by_provider` 的说明）。
//!
//! 写入侧在这一层做校验（`providers::is_known_provider_id` 判 id 认不认识、
//! 元素必须是字符串），读取侧则一律宽容（理由见 `core::api_keys` 的模块头那条
//! 硬不变量：升级绝不能让已有 Key 失效）。校验放在写入侧而不是 core：core 是
//! 「存取 + 判定」，把「哪些值算合法」集中在一处入口更好改。

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

fn list_json(state: &ServerState) -> Value {
    let keys: Vec<Value> = api_keys::list().iter().map(api_keys::ApiKeyEntry::public_json).collect();
    // 可用提供商多选的候选：**注册表**（唯一事实来源，加一家 provider 时这里
    // 自动多一项，前端零改动）。带上 count 让界面能显示「这家有几个账号」——
    // 用户在这里勾「允许哪几家」时，那几家有没有账号是他最需要知道的信息。
    //
    // 计数取自账号快照（一次读盘，与 `/api/session` 的 providers 摘要同源口径）；
    // 这里只为展示，不做任何判定，所以直接按 provider 字段数一遍即可。
    let accounts = crate::server::core::routing::accounts_of(&state.store().list_accounts());
    let providers = crate::server::core::providers::summary_json(|id| {
        accounts
            .iter()
            .filter(|account| crate::server::core::routing::provider_of(account).eq_ignore_ascii_case(id))
            .count()
    });
    json!({
        "keys": keys,
        "authRequired": config::current().api_key_set(),
        "providers": providers,
        // 「可用模型」多选的候选：**每家 → 对外名清单**，界面按当前勾了哪几家
        // 取并集（见模块头那一节与 catalog::models_by_provider 的说明）。
        // 一次给全表而不是「按勾选返回并集」：勾选要即时联动，本地取并集零往返，
        // 也避免了「响应回来时用户已经改了勾选」的竞态。
        "modelsByProvider": crate::server::core::providers::catalog::models_by_provider(&state.store()),
    })
}

/// 从请求体里读一个白名单数组（`None` = 请求体里没这个键 = **不动**）。
///
/// 三态与 `model_rules` 的 `reasoning` 同一取舍（见那个 handler 的说明）：
/// 键缺失 → `None`（不改），空数组 → `Some(vec![])`（清成不限制），
/// 有内容 → `Some(list)`。一律按「空 = 清空」处理会让旧版前端（它不发这两个键）
/// 每次改名都顺手把限制抹掉 —— 那是静默的数据丢失。
///
/// 元素校验（未知 provider id / 空串）：认不出就 400，而不是静默丢弃 ——
/// 用户勾了一个界面上不存在的 id 只可能是数据坏了，静默丢掉会让他以为设置生效了。
fn parse_allowlist(
    object: &serde_json::Map<String, Value>,
    key: &str,
    check_provider: bool,
) -> Result<Option<Vec<String>>, Response> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let items = match value {
        // null 与空数组同义（都是「清成不限制」）—— JSON 客户端两种写法都常见
        Value::Null => return Ok(Some(Vec::new())),
        Value::Array(items) => items,
        _ => return Err(errors::management_error(400, format!("{key} 必须是字符串数组"))),
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(errors::management_error(400, format!("{key} 的元素必须是字符串")));
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if check_provider && !crate::server::core::providers::is_known_provider_id(text) {
            return Err(errors::management_error(400, format!("未知的提供商: {text}")));
        }
        if out.iter().any(|known: &String| known.eq_ignore_ascii_case(text)) {
            continue;
        }
        out.push(text.to_string());
    }
    Ok(Some(out))
}

fn body_object(body: &Bytes) -> Result<serde_json::Map<String, Value>, Response> {
    let payload = parse_body(body).map_err(|error| errors::management_error(400, error.message))?;
    payload
        .as_object()
        .cloned()
        .ok_or_else(|| errors::management_error(400, "请求体必须是 JSON 对象"))
}

/// GET /api/keys
pub async fn list_keys(State(state): State<ServerState>) -> Response {
    ok_json(list_json(&state))
}

/// POST /api/keys
pub async fn create_key(State(state): State<ServerState>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let name = object.get("name").and_then(Value::as_str).unwrap_or("");
    let key = object.get("key").and_then(Value::as_str);
    // 新建时两个键缺失 = 空列表（= 不限制）；这里与 PATCH 的三态不同：
    // 新建的记录本来就没有旧值可保护，「没给」与「给空」结果相同
    let allowed_providers = match parse_allowlist(&object, "allowedProviders", true) {
        Ok(value) => value.unwrap_or_default(),
        Err(response) => return response,
    };
    let allowed_models = match parse_allowlist(&object, "allowedModels", false) {
        Ok(value) => value.unwrap_or_default(),
        Err(response) => return response,
    };
    match api_keys::add(name, key, allowed_providers, allowed_models) {
        Ok(entry) => {
            logging::log("[Config]", &format!("✅ 新增 API Key「{}」，客户端需带 Authorization: Bearer <key>", entry.name));
            let mut payload = list_json(&state);
            if let Some(map) = payload.as_object_mut() {
                map.insert("created".to_string(), entry.public_json());
            }
            ok_json(payload)
        }
        Err(message) => errors::management_error(400, message),
    }
}

/// PATCH /api/keys/{id}
pub async fn update_key(State(state): State<ServerState>, Path(id): Path<String>, body: Bytes) -> Response {
    let object = match body_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let name = object.get("name").and_then(Value::as_str);
    let enabled = object.get("enabled").and_then(Value::as_bool);
    // 三态：键缺失 → None（不动）。旧版前端只发 name/enabled，走的就是这条路
    //（所以升级后「改个名字」不会把限制清掉，见 `api_keys::update` 的说明）
    let allowed_providers = match parse_allowlist(&object, "allowedProviders", true) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let allowed_models = match parse_allowlist(&object, "allowedModels", false) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match api_keys::update(&id, name, enabled, allowed_providers, allowed_models) {
        Ok(entry) => {
            logging::log(
                "[Config]",
                &format!("API Key「{}」已更新（{}）", entry.name, if entry.enabled { "启用" } else { "停用" }),
            );
            ok_json(list_json(&state))
        }
        Err(message) => errors::management_error(404, message),
    }
}

/// DELETE /api/keys/{id}
pub async fn delete_key(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    match api_keys::remove(&id) {
        Ok(()) => {
            let remaining = config::current().api_key_set();
            logging::log(
                "[Config]",
                if remaining { "API Key 已删除" } else { "API Key 已删除，接口恢复免鉴权（仅监听 127.0.0.1）" },
            );
            ok_json(list_json(&state))
        }
        Err(message) => errors::management_error(404, message),
    }
}
