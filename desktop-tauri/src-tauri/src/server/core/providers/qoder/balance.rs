//! Qoder 个人额度和组织资源包，统一为账号页的余额形态。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::{auth, endpoints, refresh};

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse::<f64>().ok().filter(|value| value.is_finite()),
        _ => None,
    }
}

pub async fn query(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let mut credentials = refresh::ensure_fresh(store, account_id, false).await?;
    let (record, _) = refresh::snapshot(store, account_id)?;
    let proxy = auth::account_proxy(&record)?;
    let mut response = auth::request(
        "GET",
        &format!("{}{}", credentials.region.open_api(), endpoints::USAGE_PATH),
        None,
        &endpoints::open_api_headers(Some(&credentials.access_token)),
        proxy.as_ref(),
    ).await?;
    if response.status == 401 && credentials.can_refresh() {
        credentials = refresh::ensure_fresh(store, account_id, true).await?;
        response = auth::request(
            "GET",
            &format!("{}{}", credentials.region.open_api(), endpoints::USAGE_PATH),
            None,
            &endpoints::open_api_headers(Some(&credentials.access_token)),
            proxy.as_ref(),
        ).await?;
    }
    let raw = auth::payload(response, "额度查询")?;
    let quota = raw.get("userQuota");
    let remaining = quota.and_then(|quota| number(quota.get("remaining")));
    let total = quota.and_then(|quota| number(quota.get("total")));
    let used = quota.and_then(|quota| number(quota.get("used")));
    let unit = quota.and_then(|quota| quota.get("unit")).and_then(Value::as_str).unwrap_or("额度");
    let mut wallets = Vec::new();
    if let Some(value) = used {
        wallets.push(json!({ "type": "user_used", "displayName": "个人已用", "balance": value }));
    }
    if let Some(value) = total {
        wallets.push(json!({ "type": "user_total", "displayName": "个人总额", "balance": value }));
    }
    if let Some(package) = raw.get("orgResourcePackage") {
        if let Some(total) = number(package.get("total")).filter(|value| *value > 0.0) {
            let package_unit = package.get("unit").and_then(Value::as_str).unwrap_or(unit);
            wallets.push(json!({ "type": "org_total", "displayName": "组织总额",
                "balance": total, "balanceView": format!("{total} {package_unit}") }));
            if let Some(used) = number(package.get("used")) {
                wallets.push(json!({ "type": "org_used", "displayName": "组织已用",
                    "balance": used, "balanceView": format!("{used} {package_unit}") }));
            }
        }
    }
    let mut subscription = Map::new();
    if let Some(value) = raw.get("expiresAt").filter(|value| !value.is_null()) {
        subscription.insert("expireAt".to_string(), value.clone());
    }
    Ok(json!({
        "available": remaining,
        "unit": unit,
        "wallets": wallets,
        "subscription": subscription,
    }))
}
