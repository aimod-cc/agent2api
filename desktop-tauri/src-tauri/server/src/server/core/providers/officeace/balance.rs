//! OfficeAce 余额：读订阅快照（`GET /v1/subscription`，V11 签名）并归一成
//! `adapter::ProviderAdapter::query_usage` 契约的形状（见 `subscription::summarize`）。
//!
//! 控制面临时凭据（AK/SK）缺失时回 `usage_not_configured`（400 + 中性标记）——
//! 手工只导入网关 Basic 凭据的账号就会走到这条，前端显示成「未配置查询」而不是失败。

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::{credentials, subscription};

/// 查询某 OfficeAce 账号的可领余额 / 积分。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let record = store
        .officeace_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(400, "OfficeAce 账号不存在或不可用，请重新选择"))?;
    let credential = credentials::from_record(Some(&record))?;
    let subscription = subscription::fetch_subscription(&credential).await?;
    Ok(subscription::summarize(&subscription))
}
