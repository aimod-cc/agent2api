//! OfficeAce 模型可用性探测：把「目录里列着、这个号实际打不通」的模型找出来，
//! 交给 [`super::adapter::OfficeAceAdapter::advertise_models`] 默认隐藏。
//!
//! ── 为什么需要它（依据：officeace2api 的 `modelProbe.mjs`）────────
//! 上游网关的目录（`GET {网关}/v1/models`）会列出 28~34 个名，但**实测一个号
//! 只有 11 个真能打通**，其余回 `81004`（这个模型你没权限）/ `81009`（这个名字
//! 上游不认）。以前这些名字照原样列在 `/v1/models` 里，客户端看到一个试一个、
//! 试一个错一个 —— 而且本仓的 `Fatal` 分类**不换家**，一个点名到无权限模型的
//! 请求会直接把上游错误透传给客户端。
//!
//! ── 为什么必须「主动探一遍」而不是「谁被打过就记下来」──────────
//! 学习式（被动）不成立：开机时那份黑名单是空的，而客户端恰恰是在开机后
//! **第一次打开下拉框**时读列表 —— 那一刻还没有任何一次失败可供学习。所以
//! 目录刷新成功后主动探一轮（与参考实现的 `probeOnStartup` 同一取舍）。
//!
//! ── 为什么用 `max_tokens: 1` ─────────────────────────────────
//! 探测只要「上游认不认这个模型」一个 bit，不需要它真回答。`max_tokens: 1` 会
//! 被 `chat` 的 `TINY_MAX_TOKENS` 规则原样透传（< 16 不追加推理预算），上游花
//! 一个 token 就回 `finish_reason: length`（正文为空，那是它应有的样子）——
//! 成本几乎为零。参考实现实测用这一招分辨可用/不可用 22/22 全对。
//!
//! ── 隐藏规则：只藏「上游明确说这个模型不行」，其余一律算能用 ──────
//! 这是与参考实现**有意不同**的一点，也是本模块最需要小心的地方：把用户真能用
//! 的模型挡在门外，比漏藏几个更坏（`adapter.rs` 的 `advertise_models` 文档说得
//! 很重）。所以只有响应体里出现明确的「模型级拒绝」标记才隐藏：
//!   · `81004` / `81009` —— 参考实现列出的两种（没权限 / 名字不认）
//!   · `81010` / `81016` —— 本仓 2026-10-10 实测这个通道上另两种模型级拒绝码
//!   · `Insufficient permission` / `Invalid model` —— 同义的英文文案
//! 其余全部**不藏**：`200`（哪怕正文为空 —— 那是推理预算吃满，见 `chat` 模块头）、
//! `429`（限流是暂时的，不是这个模型不行）、`5xx` / 超时 / 传输错误（拿不到结论
//! 就别下"不可用"的判断）、凭据类 4xx（那是账号问题，藏模型解决不了）。
//!
//! ── 落盘与新鲜度 ────────────────────────────────────────────
//! 结论写进 `kv` 的固定键（保留键，见 `db::schema::RESERVED_KV_KEYS`），进程重启
//! 后读回 —— 否则每次重启都要在用户打开下拉框前重新探一轮。TTL 6 小时：权限是
//! **活的**（用户可能刚兑换了邀请码开通了新模型），过期即重探。
//!
//! ── 触发点 ──────────────────────────────────────────────────
//! `models::refresh` 成功拉到目录之后调 [`maybe_spawn`]：目录刷新本身就是
//! 「10 分钟 TTL 的自动 / 手动强制」两档，探测挂在这条链后面既能借手动刷新给
//! 用户一个「重新探一次」的入口（`force = true`），又天然限频（探测自己有 6 小时
//! TTL + 在途标记，不会因为目录被反复拉而反复打上游）。
//!
//! ── 一处自觉的局限（多账号）────────────────────────────────
//! 目录缓存是**进程级单槽**（见 `models` 模块头），探测结论也只有一份。多个
//! OfficeAce 账号权限不同时，这里按**最近一次刷新用的那个账号**探，结论是那份
//! 账号的可用集。本仓当前只有单账号场景，等真出现多账号再按账号分槽 —— 现在就
//! 分槽没有调用方，属于设计未来。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。持锁期间不打日志
//! （`logging` 要往同一个库写，`std::sync::Mutex` 不可重入）。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::db::Db;
use crate::server::logging;

use super::chat::{basic_authorization, chat_endpoint};
use super::credentials::OfficeAceCredential;

/// `kv` 表里的键名（保留键，见 `db::schema::RESERVED_KV_KEYS`）
pub const KV_KEY: &str = "officeaceModelProbe";

/// 结论的新鲜期：超过它，下一次目录刷新会重新探一轮（6 小时）
const TTL_MS: i64 = 6 * 60 * 60 * 1000;
/// 探测请求的 `max_tokens`（只要一个 bit，见模块头）
const PROBE_MAX_TOKENS: i64 = 1;
/// 并发数（慢模型各要 10~14 秒，串行会把一轮拖到十分钟以上）
const CONCURRENCY: usize = 4;
/// 单个探测请求的超时
const PROBE_TIMEOUT_MS: u64 = 60_000;

/// 判「这个模型上游明确不给」的标记（对响应体做小写子串匹配，见模块头）
const REJECTION_MARKERS: &[&str] = &[
    "81004",
    "81009",
    "81010",
    "81016",
    "insufficient permission",
    "invalid model",
];

/// 一次探测的状态（在内存里；落盘只在探测收尾时做一次）
#[derive(Clone)]
struct ProbeState {
    /// 上游明确拒绝的模型名
    hidden: Vec<String>,
    /// 最近一次探测完成的时刻（毫秒；0 = 从未探过 ⇒ 不藏任何东西）
    probed_at: i64,
    /// 有没有一轮探测正在跑（防止目录被反复拉时并发探）
    probing: bool,
    /// 结论是哪份账号探出来的（换了账号要重探，不看 TTL）
    account: String,
}

static STATE: OnceLock<Mutex<Option<ProbeState>>> = OnceLock::new();
static DB: OnceLock<Option<Db>> = OnceLock::new();

fn state() -> &'static Mutex<Option<ProbeState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 装入库句柄（`ServerState::bootstrap` 里 `catalog_cache::install` 之后调用一次）；
/// 重复调用忽略。库打不开时探测照跑，只是结论不落盘（下次重启重探）。
pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

/// 从库读回上次的结论（没有 / 坏 / 键不存在都给 `None`）。
fn load() -> Option<ProbeState> {
    let db = DB.get().and_then(Option::as_ref)?;
    let text = db
        .with(|conn| {
            conn.query_row("SELECT value FROM kv WHERE key = ?1", [KV_KEY], |row| {
                row.get::<_, String>(0)
            })
            .ok()
        })
        .flatten()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let hidden = value
        .get("unavailable")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(ProbeState {
        hidden,
        probed_at: value.get("probedAt").and_then(Value::as_i64).unwrap_or(0),
        probing: false,
        account: value
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// 把结论落盘（读改写单键）。失败只影响「下次重启要重探」，不报错。
fn persist(snapshot: &ProbeState) {
    let Some(db) = DB.get().and_then(Option::as_ref) else {
        return;
    };
    let payload = json!({
        "unavailable": snapshot.hidden,
        "probedAt": snapshot.probed_at,
        "account": snapshot.account,
    });
    let Ok(text) = serde_json::to_string(&payload) else {
        return;
    };
    let _ = db.with(|conn| {
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![KV_KEY, text],
        )
    });
}

/// 取状态（首次访问时从库读回）。持锁期间**不**打日志（见模块头）。
fn with_state<R>(f: impl FnOnce(&mut Option<ProbeState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = load();
    }
    f(&mut guard)
}

/// 当前被隐藏的模型名（探测跑过才有内容；没跑过返回空 = 不藏）。
pub fn hidden_ids() -> Vec<String> {
    with_state(|slot| {
        slot.as_ref()
            .map(|state| state.hidden.clone())
            .unwrap_or_default()
    })
}

/// 这个模型名是否在被隐藏集合里（大小写不敏感；`advertise_models` 用它过滤）。
pub fn is_hidden(hidden: &[String], id: &str) -> bool {
    hidden.iter().any(|name| name.eq_ignore_ascii_case(id))
}

/// 目录刷新成功后调用：到了该重探的时候（或 `force`）就起一轮后台探测。
///
/// 顺带把「名字已经不在目录里」的旧结论丢掉（上游下架又上架同一个名字时，
/// 旧结论会一上来就把新名字判死 —— 与参考实现的 `prune` 同一考虑）。
pub fn maybe_spawn(credential: &OfficeAceCredential, models: &[Value], force: bool) {
    if !credential.can_forward() {
        return;
    }
    let ids: Vec<String> = models
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return;
    }
    let now = logging::now_ms();
    let account = credential.id.clone();
    let mut pruned: Option<ProbeState> = None;
    let go = with_state(|slot| {
        if slot.is_none() {
            *slot = Some(ProbeState {
                hidden: Vec::new(),
                probed_at: 0,
                probing: false,
                account: String::new(),
            });
        }
        let Some(state) = slot.as_mut() else {
            return false;
        };
        // 先丢掉不在当前目录里的旧结论
        let before = state.hidden.len();
        state
            .hidden
            .retain(|hidden| ids.iter().any(|id| id.eq_ignore_ascii_case(hidden)));
        if state.hidden.len() != before {
            pruned = Some(state.clone());
        }
        if state.probing {
            return false;
        }
        // TTL 没过、且还是同一个账号 ⇒ 沿用上次结论
        if !force
            && state.probed_at > 0
            && state.account == account
            && now - state.probed_at < TTL_MS
        {
            return false;
        }
        state.probing = true;
        true
    });
    if let Some(snapshot) = pruned {
        persist(&snapshot);
    }
    if !go {
        return;
    }
    let base_url = credential.base_url.clone();
    let app_key = credential.model_app_key.clone();
    let app_secret = credential.model_app_secret.clone();
    crate::spawn_task(async move {
        let hidden = run_probe(&base_url, &app_key, &app_secret, &ids).await;
        let snapshot = with_state(|slot| {
            let state = slot.as_mut()?;
            state.hidden = hidden.clone();
            state.probed_at = logging::now_ms();
            state.account = account.clone();
            state.probing = false;
            Some(state.clone())
        });
        if let Some(snapshot) = snapshot {
            persist(&snapshot);
        }
        logging::verbose(
            "[OfficeAce]",
            &format!(
                "模型可用性探测完成：目录 {} 个，其中 {} 个上游明确不可用（已默认隐藏）",
                ids.len(),
                hidden.len(),
            ),
        );
    });
}

/// 对一批模型各探一次，返回「上游明确拒绝」的那批（大小为 0 = 全能用）。
async fn run_probe(base_url: &str, app_key: &str, app_secret: &str, ids: &[String]) -> Vec<String> {
    use futures::StreamExt;

    let endpoint = chat_endpoint(base_url);
    if endpoint.is_empty() {
        return Vec::new();
    }
    let auth = basic_authorization(app_key, app_secret);
    let results = futures::stream::iter(ids.iter().cloned().map(|id| {
        let endpoint = endpoint.clone();
        let auth = auth.clone();
        async move {
            let rejected = probe_one(&endpoint, &auth, &id).await;
            (id, rejected)
        }
    }))
    .buffer_unordered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    results
        .into_iter()
        .filter_map(|(id, rejected)| rejected.then_some(id))
        .collect()
}

/// 探一个模型：`true` = 上游明确说这个模型不行（该隐藏）。
///
/// 只用网关 Basic 凭据（转发那条链的凭据，不需要控制面签名），与 `chat` 同源。
async fn probe_one(endpoint: &str, auth: &str, model: &str) -> bool {
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), auth.to_string()),
    ];
    let body = json!({
        "model": model,
        "max_tokens": PROBE_MAX_TOKENS,
        "messages": [{ "role": "user", "content": "x" }],
    });
    match crate::server::core::auth_http::send_raw(
        "POST",
        endpoint,
        Some(&body),
        &headers,
        None,
        Some(PROBE_TIMEOUT_MS),
    )
    .await
    {
        // 200 就算能用 —— 正文可能是空的（推理预算吃满），那是它应有的样子
        Ok(response) if response.ok => false,
        Ok(response) => is_model_rejection(&response.payload),
        // 传输 / 超时：拿不到结论 ⇒ 不藏（宁可不藏，不可错藏）
        Err(_) => false,
    }
}

/// 响应体里有没有「模型级拒绝」的标记（见模块头的隐藏规则）。
fn is_model_rejection(payload: &Option<Value>) -> bool {
    let text = payload
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default()
        .to_ascii_lowercase();
    REJECTION_MARKERS.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_only_on_explicit_model_rejection_markers() {
        // 参考实现列的两种 + 本仓实测的另两种
        assert!(is_model_rejection(&Some(json!({
            "error": {"code": "ModelArts.81009", "message": "Invalid model."}
        }))));
        assert!(is_model_rejection(&Some(json!({
            "error": {"code": "ModelArts.81004", "message": "Insufficient permission"}
        }))));
        assert!(is_model_rejection(&Some(json!({"error": {"code": "81010"}}))));
        assert!(is_model_rejection(&Some(json!({"error": {"code": "81016"}}))));
        // 空响应 / 非 JSON / 无关错误：不藏
        assert!(!is_model_rejection(&None));
        assert!(!is_model_rejection(&Some(json!({}))));
        // 体积超限（81113）、限流（81111）：是请求/账号问题，不是这个模型不行
        assert!(!is_model_rejection(&Some(json!({"error": {"code": "81113"}}))));
        assert!(!is_model_rejection(&Some(json!({"error": {"code": "81111"}}))));
        // 凭据类错误：藏模型解决不了
        assert!(!is_model_rejection(&Some(json!({"error": {"code": "APIG.1009"}}))));
    }

    #[test]
    fn is_hidden_matches_case_insensitively() {
        let hidden = vec!["glm-5.2".to_string(), "deepseek-v4-pro".to_string()];
        assert!(is_hidden(&hidden, "GLM-5.2"));
        assert!(is_hidden(&hidden, "glm-5.2"));
        assert!(!is_hidden(&hidden, "glm-5.3"));
        assert!(!is_hidden(&[], "glm-5.2"));
    }
}
