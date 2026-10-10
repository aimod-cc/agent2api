//! OfficeAce 适配器：**无状态** OpenAI 兼容转发 + 账号管理（手工导入 / 自助 OAuth）。
//!
//! ── 为什么是无状态 ──────────────────────────────────────────
//! 上游模型网关就是 OpenAI chat/completions（`Basic` 鉴权），一次请求一次回答，
//! 没有 CodeArts 那种「3 并发会话 + 心跳」的约束（两份公开实现都没有会话概念）。
//! 所以 `supports_chat` / `is_stateful` 走默认值，转发链只需 `build_chat_request`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;

use super::{chat, credentials, models};

/// 适配器实例（`adapter_for(ProviderKind::OfficeAce)` 返回这一个）。
///
/// **无状态**：不持有任何数据 —— 凭证在账号存储、目录在 `models.rs` 的进程级句柄。
pub struct OfficeAceAdapter;

/// 静态单例
pub static OFFICEACE_ADAPTER: OfficeAceAdapter = OfficeAceAdapter;

impl ProviderAdapter for OfficeAceAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OfficeAce
    }

    /// 模型清单来自远程目录（`models.rs` 的进程缓存 + 落盘缓存）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {网关}/v2/chat/completions`。
    ///
    /// 上游恒流式（见 `chat` 的模块头）；非流式客户端请求由编排层聚合。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credentials::from_record(Some(account))?;
        let plan = chat::build_chat_plan(&credential, body)
            .map_err(|error| GatewayError::with_status(400, error))?;
        Ok(ChatRequestPlan::chat(plan.url, plan.headers, plan.body))
    }

    /// 上游错误分类。依据 officeace2api 实测的错误码表（`upstream.mjs` 的
    /// `classifyUpstream`）：
    ///
    ///   - `81113` / `exceeds the maximum size` → `Fatal`（请求体超限，换账号无用）
    ///   - `81111` / `81112` / `81114` / `0308` / `TPM` / HTTP 429 → `QuotaLimited`
    ///     （限流，冷却「该凭据 + 该模型」；上游不给恢复时间，走兜底时长）
    ///   - `81004`（没权限）/ `81009`（名字不认）→ `Fatal`（**只该冷却这一对**，
    ///     不牵连整条凭据；4xx 本身不触发冷却，与 codearts 同一判据链）
    ///   - `APIG.1009` / `APIG.1001` / `APIG.1002` / 401 / 403 → `Fatal`
    ///     （凭据失效，用户需重新登录/导入；不自动冷却账号池）
    ///   - 其余 → `Fatal`
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let text = error_body.to_string();
        let lower = text.to_ascii_lowercase();
        let rate_limited = status == 429
            || ["81111", "81112", "81114", "0308", "tpm", "rate limit", "too many requests"]
                .iter()
                .any(|marker| lower.contains(marker) || text.contains(marker));
        if rate_limited {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {text}"),
                upstream_code: None,
                status,
            };
        }
        // 其余（体积超限、模型无权限、凭据失效）一律致命：不罚账号池
        UpstreamErrorClass::Fatal {
            status,
            message: format!("上游返回 {status}: {text}"),
            upstream_code: None,
        }
    }

    /// 本家**没有可刷的 token**：转发凭证是不用过期的网关 Basic 对，
    /// 控制面临时凭据只服务额度/签到（走了也换不来新的转发凭据）。
    /// 编排层只在 `TokenExpired` 分类上调用它，而本家从不那样分类 ——
    /// 这里如实报「没有可刷新项」。
    fn ensure_access_token<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Err(GatewayError::with_status(
                401,
                "OfficeAce 的转发凭据不过期、也没有可刷新的 token；凭证失效时请重新导入或重新登录",
            ))
        })
    }

    /// 刷新模型目录（`GET {网关}/v1/models`，只用网关 Basic 凭据）。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let record = store.officeace_account_record(account_id);
            if record.is_none() && !account_id.is_empty() {
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            }
            models::refresh_from_record(record.as_ref(), force).await
        })
    }

    /// 模型目录来自远程（`GET {网关}/v1/models`）⇒ 必须声明 true，否则
    /// 「获取模型」弹窗的模型来源下拉与后台目录刷新都不会调度到这家
    /// （与 Loomy / AutoClaw 同口径）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 有转发能力（无状态 OpenAI 兼容）。
    fn supports_chat(&self) -> bool {
        true
    }

    /// 支持续期：控制面临时凭据 2 小时到期，靠 refresh_token + DPoP 续
    /// （见 `credentials` 与后续的 `oauth` 模块）。**未登录/只导入 Basic 时
    /// 没有可续的东西** —— 那不属于故障，网关凭据不过期。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 控制面凭据临期的账号要进定时维护（与 `supports_refresh` 配对，
    /// 漏写这一位会让这家每轮静默跳过）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.officeace_account_record(account_id) else {
            return false;
        };
        match credentials::from_record(Some(&record)) {
            Ok(credential) => credential.has_control_plane() && credential.control_plane_expiring(),
            Err(_) => false,
        }
    }
}
