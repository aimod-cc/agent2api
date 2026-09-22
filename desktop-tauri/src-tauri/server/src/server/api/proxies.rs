//! 出网代理路由（对照 workbuddy-account-routes.mjs 的 `tryHandleProxies`）。
//!
//!   GET  /api/proxies       可选项（Clash Verge 实时快照）+ 各账号当前出口
//!   POST /api/proxies/test  测试出口连通性并返回出口 IP
//!
//! 从 accounts.rs 拆出（单文件行数约定）。Node 版也把这两条放在
//! `tryHandleProxies` 里，与 `tryHandle`（账号 CRUD）分开 —— 拆分点���它一致。
//!
//! 入口 `proxies_entry` 仍在 accounts.rs：路由注册在 http.rs 里按模块分组，
//! 两条 proxies 路径与账号路径挨着注册更便于对照 Node 的判定顺序。

use axum::body::Bytes;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::normalize_account_proxy;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

use super::accounts::proxy_error;

// ─── GET /api/proxies ───────────────────────────────────────

/// 出网代理可选项。
///
/// `clash_proxy_options()` 实时读 Clash Verge 配置（三个候选目录 + 3 秒 TTL 缓存）：
/// 未安装 Clash 时 `clash.available=false` + `error="未找到 Clash Verge 配置"` +
/// `options=[]`；装了则给出「混合端口 + 各监听器」两份可选项
/// （前端 proxy-form.js 直接读 `.options.length`，所以空态也必须是数组）。
/// accounts 里每个账号带上 priority/enabled/proxy（proxy 已由 store 描述过，
/// 含「解析失败」分支）。
pub async fn list_proxies(state: &ServerState) -> Response {
    let clash = crate::server::core::proxies::clash_proxy_options();
    let snapshot = state.store().list_accounts();
    let accounts: Vec<Value> = snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|account| {
                    json!({
                        "id": account.get("id").cloned().unwrap_or(Value::Null),
                        "name": account.get("name").cloned().unwrap_or(Value::Null),
                        "priority": account.get("priority").cloned().unwrap_or(Value::Null),
                        "enabled": account.get("enabled").cloned().unwrap_or(Value::Null),
                        "proxy": account.get("proxy").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ok_json(json!({
        "clash": clash,
        "accounts": accounts,
        "note": "账号默认无代理（直连）；选择 Clash 项后端口由 Clash Verge 管理，这里实时读取",
    }))
}

// ─── POST /api/proxies/test ─────────────────────────────────

/// 出网连通性测试（对照 account-routes.mjs 的 tryHandleProxies test 分支）。
///
/// 两种用法：
///   `{ id }`     测该账号的出网代理（账号不存在 → 404；代理解析失败 → 400）
///   `{ proxy }`  测临时配置（含 `proxy: null` = 测直连）
///
/// 判据是「能否连上 WorkBuddy 上游」，而不是能否访问第三方站点（理由见
/// `core::egress::test_connectivity` 的注释）；出口 IP 是附加信息。
/// 响应固定 200 + `{success, status?, ip?, durationMs, error?, proxy}` ——
/// **测试失败也是 200**（`success:false` + error 文案），前端据此显示红色提示。
pub async fn test_proxy(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    let mut proxy: Option<crate::server::core::proxies::ResolvedProxy> = None;
    if let Some(id) = payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        let Some(creds) = state.store().get_credentials_by_id(id) else {
            return management_error(404, "账号不存在");
        };
        if let Some(error) = creds.proxy_error {
            return management_error(400, error);
        }
        // 账号记录里的出口已经是解析后的形态（带 label）
        match crate::server::core::proxies::ResolvedProxy::from_json(&creds.proxy) {
            Ok(resolved) => proxy = resolved,
            Err(reason) => return management_error(400, reason),
        }
    } else if let Some(config_value) = payload.get("proxy") {
        let config = match normalize_account_proxy(config_value) {
            Ok(value) => value,
            Err(error) => return proxy_error(error),
        };
        if let Some(config) = config {
            if let Some(resolution) =
                crate::server::core::proxies::resolve_account_proxy(Some(&config))
            {
                if let Some(error) = resolution.error() {
                    return management_error(400, error);
                }
                proxy = resolution.resolved().cloned();
            }
        }
        // config 为 None（payload.proxy === null）= 测直连，proxy 保持 None
    }

    let result = egress::test_proxy(proxy.as_ref()).await;
    let outcome = if result.success {
        format!("✅ 出口 IP {}", result.ip)
    } else {
        format!("❌ {}", result.error.clone().unwrap_or_default())
    };
    logging::log(
        "[Accounts]",
        &format!(
            "代理测试{}: {outcome}",
            match &proxy {
                Some(proxy) if !proxy.label.is_empty() => format!("（{}）", proxy.label),
                Some(proxy) => format!("（{}）", proxy.host),
                None => "（直连）".to_string(),
            }
        ),
    );
    let mut data = result.to_json();
    if let Some(object) = data.as_object_mut() {
        object.insert("proxy".to_string(), egress::describe_public(proxy.as_ref()));
    }
    ok_json(data)
}
