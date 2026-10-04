//! LobsterAI（网易有道）provider（feat/lobster-provider；PR-1 落注册表与凭证
//! 读取，PR-2 落转发 / 目录 / 额度 / 签到全链路）。
//!
//! ── 本模块负责什么 ─────────────────────────────────────────
//!   - **凭证**（`credentials.rs`）：本机 `~/Library/Application Support/
//!     LobsterAI/lobsterai.sqlite` 的 `kv.auth_tokens` 行（桌面端实时登录态，
//!     mtime+TTL 缓存读）或账号记录（手动粘贴）；临期主动刷新并回写。
//!   - **转发**：`POST {server}/api/proxy/v1/chat/completions`，OpenAI 兼容
//!     body，仅流式（上游不支持 stream:false —— 参考实现 实测 stream:false
//!     也回 SSE，统一按流式发避免歧义）。模型名对外带 `lobster-` 前缀，
//!     发送前剥掉（裸名与本网关其它上游大量撞名，见 `models.rs`）。
//!   - **目录 / 额度 / 签到**：`models.rs` / `balance.rs` / `checkin.rs`。
//!
//! ── 与 raccoon 适配器的关键差异（别照抄）─────────────────────
//!   1. **模型名要剥前缀**：本网关目录里的 id 是 `lobster-<裸名>`，上游只认
//!      裸名（`body.model` 直改）；raccoon 的模型 id 无前缀概念。
//!   2. **强制 stream:true**：上游仅流式，`stream:false` 的请求体照 参考实现
//!      的实测结论一律改成流式发（编排层按 SSE 收）。
//!   3. **没有环境变量旁路**：凭证只能来自 sqlite / 账号记录，
//!      `allows_anonymous_default_session` 保持 false（raccoon 有
//!      `RACCOON_TOKEN` 那条 CI 入口，LobsterAI 没有同款）。
//!   4. **403 也算凭证失效**：参考实现 把 401/403 一起走「重读 → 刷新 → 重试」
//!      自愈链，分类时两档都归 TokenExpired（raccoon 只认 401）。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic：取值走 Option 链与
//! `unwrap_or`，序列化失败一律转成 GatewayError。

pub mod balance;
pub mod checkin;
pub mod credentials;
pub mod models;

use std::pin::Pin;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use axum::http::HeaderMap;

use super::adapter::{ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass};
use super::ProviderKind;

/// 默认服务器地址（LLM 网关与鉴权/用户接口同域）
pub const DEFAULT_LLM_BASE_URL: &str = "https://lobsterai-server.youdao.com";

/// LobsterAI 适配器（无状态单例：凭证在账号存储/sqlite、目录在 `models.rs`
/// 的进程级句柄，适配器自己只持常量与纯函数）
pub struct LobsterAdapter;

/// 进程级实例
pub static LOBSTER_ADAPTER: LobsterAdapter = LobsterAdapter;

impl ProviderAdapter for LobsterAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Lobster
    }

    /// 模型清单（`models.rs` 的进程级句柄；id 带 `lobster-` 前缀）
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {server}/api/proxy/v1/chat/completions`。
    ///
    /// body 改写只有两处：剥模型前缀（上游只认裸 id）与强制 `stream:true`
    /// （仅流式）。其余字段原样透传 —— 上游是通用 OpenAI 兼容实现。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let token = account
            .get("auth")
            .and_then(|auth| auth.get("accessToken"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "LobsterAI 账号缺少 accessToken，无法转发（请重新登录或导入桌面端登录态）",
            ));
        }
        let mut outbound = body.clone();
        let Some(object) = outbound.as_object_mut() else {
            // 非对象 body（数组/字符串）没法做前缀剥离与 stream 改写，
            // 静默放行会把 stream:false 原样发给只流式的上游（协议错配且难定位）
            return Err(GatewayError::with_status(
                400,
                "请求体必须是 JSON 对象（LobsterAI 转发需要改写 model 与 stream 字段）",
            ));
        };
        let requested = object
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let stripped = models::strip_model_prefix(&requested);
        if stripped.is_empty() {
            // "lobster-"（剥完为空）或没有 model 字段:发空模型名只会换来上游 404
            return Err(GatewayError::with_status(400, "缺少有效的 model 字段"));
        }
        object.insert("model".to_string(), Value::String(stripped));
        // 上游仅流式（stream:false 也回 SSE）——统一按流式发，
        // 避免「客户端要聚合、上游回 SSE」的歧义
        object.insert("stream".to_string(), Value::Bool(true));
        let headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ];
        Ok(ChatRequestPlan::chat(
            format!("{}/api/proxy/v1/chat/completions", DEFAULT_LLM_BASE_URL),
            headers,
            outbound,
        ))
    }

    /// 上游错误分类（判定依据 参考实现 的自愈链）：
    ///   - 401 / 403 → TokenExpired（参考实现 把两档一起走刷新重试；
    ///     服务端侧失效发生在远未临期的 token 上，交给强制刷新处理）；
    ///   - 429 → QuotaLimited（积分耗尽/频率限制，换下一个账号有意义）；
    ///   - 其余 → Fatal 原样透传（含 4004 之类的业务码场景）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 || status == 403 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            // 上游未在响应体里给结构化恢复时间，reset_at 交给冷却兜底
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: error_body.get("code").and_then(Value::as_i64),
        }
    }

    /// 取可用 access token（临期主动刷新，余量 < 120 秒；刷新结果按来源回写）。
    ///
    /// `account_id` 为空 → LobsterAI 组内当前账号；没有账号记录时回落桌面端
    /// 实时登录态（`credentials::snapshot_for` 的兜底链）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credentials = credentials::snapshot_for(store, account_id)?;
            let refreshed = credentials::refresh(store, &credentials, false).await?;
            Ok(refreshed.access_token)
        })
    }

    /// 401 后的**强制**刷新：不看临期窗口直接续期（`refresh` 的 force 语义，
    /// 覆盖理由见 trait 的默认实现文档 —— 被拒的 token 可能时间上还很新）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credentials = credentials::snapshot_for(store, account_id)?;
            let refreshed = credentials::refresh(store, &credentials, true).await?;
            Ok(refreshed.access_token)
        })
    }

    /// 刷新模型目录：`models.rs` 的远程路径 + openclaw.json 本地回落。
    ///
    /// 目录刷新**不触发 token 刷新**（维护动作，与 raccoon 同一取舍）：
    /// 临期 token 在转发链路上该续期时自然会续。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>>
    {
        Box::pin(async move {
            let token = match credentials::snapshot_for(store, account_id) {
                Ok(credentials) => credentials.access_token,
                Err(error) => {
                    crate::server::logging::verbose(
                        "[Models]",
                        &format!("LobsterAI 模型目录刷新：{}", error.message),
                    );
                    String::new()
                }
            };
            models::refresh(&token, force).await
        })
    }

    /// LobsterAI 有远程目录（`/api/proxy/v1/models` + openclaw.json），支持刷新
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// SSE 帧的 model 名回写：上游回显的是裸 id（`glm-5.3-flash`），而客户端
    /// 认的是自己请求的 `lobster-glm-5.3-flash`（与 raccoon 的同款理由：
    /// 这是上游网关的行为特征，帧改写由通用 SSE 流按这个开关执行）。
    fn sse_model_rewrite(&self) -> bool {
        true
    }

    /// LobsterAI 支持主动刷新（`POST /api/auth/refresh`）
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 临期判定：账号记录存在且有 refreshToken、JWT 余量 < 120 秒。
    ///
    /// 桌面端来源的凭证实时读 sqlite，App 自己会刷新回写 —— 网关侧再主动刷
    /// 只会在 App 空闲时白打一次接口；但 App 不在运行时（登录态静止）仍需要
    /// 这条维护路径兜底，因此桌面来源同样参与判定。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.is_empty() || store.lobster_account_record(account_id).is_none() {
            return false;
        }
        match credentials::snapshot_for(store, account_id) {
            Ok(credentials) => credentials.can_refresh() && credentials.is_expiring(),
            Err(_) => false,
        }
    }

    /// LobsterAI 有余额 / 积分概念（`balance.rs`）
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查积分（`balance.rs`；60s TTL 缓存，401 原样透出走刷新重试）
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move { balance::query_usage(store, account_id).await })
    }
}
