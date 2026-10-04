//! LobsterAI（网易有道）provider 模块骨架（feat/lobster-provider PR-1）。
//!
//! ── 本模块负责什么（本 PR-1 范围内）────────────────────────────
//!   - **凭证来源**：本机 `~/Library/Application Support/LobsterAI/lobsterai.sqlite`
//!     的 `kv` 表 `auth_tokens` 行（accessToken + refreshToken），桌面端实时登录态。
//!   - **账号存储**：`account_store::lobster_accounts` 的手动添加 + 桌面端导入
//!     共用同一落账入口。
//!
//! ── 不在本 PR-1 范围（PR-2 接）─────────────────────────────────
//!   - OpenAI 兼容 chat 转发流式（`build_chat_request` 仅占位返回 501）；
//!   - 模型目录远程刷新与兜底（`list_models` 当前返回空）；
//!   - 额度查询 + 每日签到；
//!   - Token 刷新回写 sqlite（PR-1 跳过 → App 端自行维持凭证）；
//!   - 自动签到守护线程。
//!
//! ── 上游长什么样（移植来源 sub2api/src/lobster_upstream.py）────────
//!   - LLM 网关：`POST https://lobsterai-server.youdao.com/api/proxy/v1/chat/completions`
//!     OpenAI 兼容 body，仅流式（SSE）；鉴权 `Authorization: Bearer <JWT>`。
//!   - 鉴权：`POST /api/auth/refresh`，body `{"refreshToken": "..."}`。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic：取值走 Option 链与
//! `unwrap_or`，序列化失败一律转成 GatewayError。

pub mod credentials;

use std::pin::Pin;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use axum::http::HeaderMap;

use super::adapter::{ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass};
use super::ProviderKind;

/// 默认 LLM 网关地址（PR-2 接转发使用）
pub const DEFAULT_LLM_BASE_URL: &str = "https://lobsterai-server.youdao.com";

/// LobsterAI 适配器（PR-1 占位：list_models 返回空、build_chat_request 返回 501）。
///
/// 仅用于让 `adapter_for` 穷举 match 编译通过；`implemented_kinds` **不列**它
/// （后台目录刷新跳过）；前台候选链在没有可用账号时也为空。
pub struct LobsterAdapter;

/// 进程级实例
pub static LOBSTER_ADAPTER: LobsterAdapter = LobsterAdapter;

impl ProviderAdapter for LobsterAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Lobster
    }

    /// 模型清单：PR-1 返回空（PR-2 接 `/api/proxy/v1/models` 远程刷新）
    fn list_models(&self) -> Vec<Value> {
        Vec::new()
    }

    /// PR-1 不实现转发；返回明确 501 让客户端看到“等待 PR-2”
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            501,
            "LobsterAI 转发尚未在本 PR 启用（PR-1 仅完成账号接入），请等待后续版本",
        ))
    }

    fn classify_error(&self, status: u16, _error_body: &Value) -> UpstreamErrorClass {
        UpstreamErrorClass::Fatal {
            status,
            message: format!("LobsterAI 上游返回 {status}"),
            upstream_code: None,
        }
    }

    fn ensure_access_token<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        // PR-1：直接返回 501 错误（与 build_chat_request 一致：转发能力未启用）
        Box::pin(async move {
            Err(GatewayError::with_status(
                501,
                "LobsterAI 转发尚未在本 PR 启用（PR-1 仅完成账号接入）",
            ))
        })
    }

    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        // PR-1：模型目录为空，刷新什么也不做
        Box::pin(async move { ModelRefreshOutcome::unchanged() })
    }
}