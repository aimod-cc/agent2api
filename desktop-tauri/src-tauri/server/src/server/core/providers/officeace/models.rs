//! OfficeAce 模型目录：`GET {网关}/v1/models`（OpenAI 格式）+ 持久化缓存。
//!
//! ── 上游形态（依据：officeace2api 的 `server.mjs` 用同一凭据打
//! `{baseUrl}/models`；桌面端 `models.json` 里每条也带 `model_name`）──
//! 标准 OpenAI `{data:[{id, object, created, owned_by}]}`。**只要网关 Basic
//! 凭据**，不需要控制面签名（那套只服务额度/签到）。
//!
//! ── 实测口径：目录 ≠ 可用 ────────────────────────────────────
//! officeace2api 实测：上游目录 34 个名，两个号实际只有 11 个能用，其余回
//! `81004`（没权限）/ `81009`（名字不认）。所以**目录照收**，把「哪些真能用」
//! 交给上游在转发时报错（它的 `model_not_available` 分类），本模块不猜、
//! 也不自作主张过滤 —— 过滤错了会把用户能用的模型挡在门外。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 与各家同款：内存缓存 + `providers::catalog_cache` 落盘（进程重启后读回）。
//! 自动路径 10 分钟 TTL；用户手动「刷新模型清单」时 `force = true` 绕过。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::server::core::providers::catalog_cache;
use crate::server::errors::GatewayError;

use super::credentials::{from_record, OfficeAceCredential};

/// 本家在 `catalog_cache` 里的 scope 名
pub const SCOPE: &str = catalog_cache::SCOPE_OFFICEACE;

/// 自动路径的缓存 TTL（与各家同档）
const TTL_MS: i64 = 10 * 60 * 1000;

/// 进程内缓存
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        if let Some(cached) = catalog_cache::load(SCOPE) {
            *guard = Some(CatalogState {
                models: cached.models,
                fetched_at: cached.fetched_at,
            });
        }
    }
    f(&mut guard)
}

/// 当前清单（内存 → 落盘缓存；都没有时为空，由刷新路径填）。
pub fn list() -> Vec<Value> {
    with_state(|slot| slot.as_ref().map(|state| state.models.clone()).unwrap_or_default())
}

/// 这份清单是否来自远程拉取（界面「来源」列的判据）。
pub fn remote_refreshed() -> bool {
    with_state(|slot| {
        slot.as_ref()
            .map(|state| !state.models.is_empty())
            .unwrap_or(false)
    })
}

/// 最近一次成功刷新的时刻（毫秒；没刷过为 0）。
pub fn last_refreshed_at() -> i64 {
    with_state(|slot| slot.as_ref().map(|state| state.fetched_at).unwrap_or(0))
}

/// 把一个上游条目映成目录条目（保留上游给的字段，缺的补 `officeace`）。
fn map_entry(entry: &Value) -> Option<Value> {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let mut out = json!({
        "name": id,
        "display_name": entry
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(id),
        "description": "",
        "context_length": entry
            .get("context_window")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        // 目录不给输出上限（officeace2api 的 `/v1/models` 也没这个字段）；
        // 留 0 = 「未声明」，界面显示 `—`，不编造。
        "max_output_tokens": 0,
        "credit_display": "",
        "id": id,
    });
    if let Some(object) = out.as_object_mut() {
        // `owned_by` 原样透传（我们的目录形状里叫 `owned_by` 的不多，留着不碍事）
        if let Some(owner) = entry.get("owned_by").and_then(Value::as_str) {
            object.insert("owned_by".to_string(), Value::String(owner.to_string()));
        }
        // 能力位：上游这条链**不报**（officeace2api 的目录也不带 vision/tool 位），
        // 所以不写 `supportsImages` —— 缺席 = 「不知道」，闸门据此不拦（见
        // `catalog/routing.rs` 的 `entry_refuses_images` 与它的模块头）。
        if let Some(developer) = entry.get("developer").and_then(Value::as_str) {
            object.insert("developer".to_string(), Value::String(developer.to_string()));
        }
    }
    Some(out)
}

/// 解析 `{data:[…]}` 或裸数组。
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 拉一次模型目录并落地（内存 + 持久化缓存）。
pub async fn refresh(credential: &OfficeAceCredential, force: bool) -> ModelRefreshOutcome {
    let now = crate::server::logging::now_ms();
    if !force {
        let fresh = with_state(|slot| {
            slot.as_ref()
                .map(|state| !state.models.is_empty() && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return ModelRefreshOutcome::unchanged();
        }
    }
    let url = format!("{}/models", super::chat::normalize_base(&credential.base_url));
    if url == "/models" {
        return ModelRefreshOutcome::failed("OfficeAce 账号缺少模型网关基址");
    }
    let headers = vec![
        (
            "Authorization".to_string(),
            super::chat::basic_authorization(&credential.model_app_key, &credential.model_app_secret),
        ),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let response = match crate::server::core::auth_http::send_raw(
        "GET",
        &url,
        None,
        &headers,
        None,
        Some(20_000),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            let text = if error.is_timeout() {
                "OfficeAce 模型目录查询超时".to_string()
            } else {
                format!("OfficeAce 模型目录查询失败：{error}")
            };
            return ModelRefreshOutcome::failed(text);
        }
    };
    if !response.ok {
        return ModelRefreshOutcome::failed(format!(
            "OfficeAce 模型目录返回 HTTP {}（网关不接受这条凭据？）",
            response.status
        ));
    }
    let models = parse_models(&response.payload.unwrap_or(Value::Null));
    if models.is_empty() {
        return ModelRefreshOutcome::failed("OfficeAce 模型目录为空或格式不认识（保留现有清单）");
    }
    let count = models.len();
    with_state(|slot| {
        *slot = Some(CatalogState {
            models: models.clone(),
            fetched_at: now,
        });
    });
    catalog_cache::save(SCOPE, &models, now);
    ModelRefreshOutcome::refreshed(count)
}

/// 从账号记录直接刷（适配器用）。
pub async fn refresh_from_record(record: Option<&Value>, force: bool) -> ModelRefreshOutcome {
    let credential = match from_record(record) {
        Ok(credential) => credential,
        Err(error) => return ModelRefreshOutcome::failed(error.message),
    };
    refresh(&credential, force).await
}

/// 目录为空时的兜底错误（供适配器用）。
pub fn unavailable_error() -> GatewayError {
    GatewayError::with_status(503, "OfficeAce 模型目录尚未拉取：请先在模型管理页点「刷新模型清单」")
}

/// 模型目录刷新结果（与各家同一类型）
pub use crate::server::core::providers::adapter::ModelRefreshOutcome;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_shape_and_skips_entries_without_id() {
        let payload = json!({
            "object": "list",
            "data": [
                {"id": "glm-5.3", "object": "model", "owned_by": "officeace", "display_name": "GLM 5.3", "context_window": 198000},
                {"object": "model"},
                {"id": "  "},
                {"id": "deepseek-v4-flash-0731"}
            ]
        });
        let models = parse_models(&payload);
        assert_eq!(2, models.len(), "只收有 id 的两条");
        assert_eq!("glm-5.3", models[0]["id"]);
        assert_eq!("GLM 5.3", models[0]["display_name"]);
        assert_eq!(198000, models[0]["context_length"]);
        assert_eq!("officeace", models[0]["owned_by"]);
        // 没给 display_name 时回落 id
        assert_eq!("deepseek-v4-flash-0731", models[1]["display_name"]);
        // 不写能力位：缺席 = 不知道（闸门不据此拦）
        assert!(models[0].get("supportsImages").is_none());
        assert_eq!(0, models[0]["max_output_tokens"], "目录不给输出上限，留 0");
    }

    #[test]
    fn accepts_a_bare_array_too() {
        let payload = json!([{"id": "a"}, {"id": "b"}]);
        assert_eq!(2, parse_models(&payload).len());
    }
}
