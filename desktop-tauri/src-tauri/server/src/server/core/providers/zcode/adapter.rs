//! ZCode 的 `ProviderAdapter` 实现（无状态、OpenAI 兼容、按地区参数化）。
//!
//! ── 实现是**一套**，实例按地区给 ────────────────────────────
//! `ZcodeAdapter` 持有一个 [`Region`]，两个静态实例（`ZCODE_ADAPTER` 国内版 /
//! `ZCODE_INTL_ADAPTER` 国际版）由 `adapter_for` 按 kind 给出 —— 与
//! `autoclaw::adapter` / `accio::adapter` 同一手法（两个地区是两个 provider、
//! 同一份实现）。地区 → provider 的互查只在 `region.rs`，别处不要写
//! `"zcode-intl"` 这类字面量。
//!
//! ── 上游协议 ────────────────────────────────────────────────
//! 推理走 **OpenAI 兼容**端点：`POST {openai_base}/chat/completions`，
//! `Authorization: Bearer {token}`，body 原样透传（与 raccoon 同构）。
//! 因此 `is_stateful()` 保持默认 false（一次发送由通用编排层完成），
//! 与 CatPaw / Qoder / Accio 那三家「适配器自己发」的情形不同。
//!
//! ── 令牌来源与续期（**当前是已知缺口**）──────────────────────
//! 本适配器只从账号会话里读 `auth.accessToken`（与 raccoon 的
//! `build_chat_request` 同一取法），**没有**实现续期：
//! `refresh_access_token` 会如实报错让用户重新登录。
//!
//! 这不是偷懒而是划界：ZCode 的登录是**服务端中介的 CLI 轮询**
//! （`/oauth/cli/init` + `/oauth/cli/poll/{flow_id}`，见参考实现
//! `Acankao/zcode-api` 的 `src/auth/oauth.ts`），它要先有 `credentials.rs`
//! 落盘 JWT 与设备标识、再有前端登录页，续期才有东西可续。在那两件完成之前，
//! 这里返回一句可读的报错，比留一个「刷新了但没变」的假实现更诚实 ——
//! 后者会让编排层的「401 后刷新重试一次」变成「用同一个坏 token 再打一次」。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::super::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use super::super::content_block;
use super::super::ProviderKind;
use super::models;
use super::region::Region;

/// ZCode 适配器（无状态；地区是唯一的状态，构造期固定）
pub struct ZcodeAdapter {
    /// 本实例服务的地区
    region: Region,
}

/// 国内版实例（`adapter_for` 返回它的引用）
pub static ZCODE_ADAPTER: ZcodeAdapter = ZcodeAdapter { region: Region::Cn };

/// 国际版实例
pub static ZCODE_INTL_ADAPTER: ZcodeAdapter = ZcodeAdapter { region: Region::Intl };

impl ZcodeAdapter {
    /// 本实例的地区（供 `adapter_for` 之外的调用点自查，例如领取任务的选路）
    pub fn region(&self) -> Region {
        self.region
    }

    /// 推理基址（`ZCODE_OPENAI_BASE_URL` / `ZCODE_INTL_OPENAI_BASE_URL` 可覆盖）。
    ///
    /// 留覆盖口子是因为上游域名会变（智谱历史上换过编码套餐的域名），
    /// 而发版节奏跟不上域名变更时，用户至少能自己改环境变量救急 ——
    /// 与 autoclaw / raccoon 两家的 `env_override` 同一取舍。
    fn openai_base_url(&self) -> String {
        self.region
            .env_override("OPENAI_BASE_URL")
            .unwrap_or_else(|| self.region.openai_base_url().to_string())
    }
}

impl ProviderAdapter for ZcodeAdapter {
    fn kind(&self) -> ProviderKind {
        self.region.kind()
    }

    /// 本家的模型清单（静态表，两地共用一份，见 `models.rs` 的模块头）
    fn list_models(&self) -> Vec<Value> {
        models::list(self.region)
    }

    /// 构造 `POST {openai_base}/chat/completions`。
    ///
    /// body **原样透传**：上游就是 OpenAI 协议，本家没有任何要改写的字段
    /// （不做模型改名、不注入思考等级 —— 后者靠 `reasoning_patch` 的默认
    /// `Skip`，那是「没证据就不注入」的正确默认）。
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
                "ZCode 账号缺少 accessToken，无法转发",
            ));
        }
        let headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ];
        Ok(ChatRequestPlan {
            url: format!("{}/chat/completions", self.openai_base_url()),
            headers,
            body: body.clone(),
        })
    }

    /// 上游错误分类。
    ///
    ///   - `401` → TokenExpired（编排层会刷新后同账号重试一次；本家当前的
    ///     `refresh_access_token` 会如实报错，见模块头）
    ///   - `429` → QuotaLimited（编码套餐是「5 小时 + 每周」双窗口限额，
    ///     上游不给结构化的恢复时间，`reset_at` 给 None 让冷却走兜底时长）
    ///   - 其余 → 交给共用的内容拦截判定（`content_block`），
    ///     与其余各家同一口径 —— 编码套餐同样会有内容策略拦截
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        content_block::classify_or_fatal(
            status,
            error_body,
            message,
            error_body.get("code").and_then(Value::as_i64),
        )
    }

    /// 取可用令牌：只读账号会话里的 `accessToken`，**不续期**（见模块头）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { session_access_token(store, self.region, account_id) })
    }

    /// 401 后的强制刷新：本家尚未实现续期，如实报错让用户重新登录。
    ///
    /// **不要**在这里回落到「再读一次会话」—— 那正是编排层已经做过的动作，
    /// 返回同一个被拒的 token 会让「刷新后重试一次」退化成一次无意义的重复请求。
    fn refresh_access_token<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Err(GatewayError::with_status(
                401,
                "ZCode 的登录态无法自动续期，请在「账号」页重新登录该账号",
            ))
        })
    }

    /// 本家没有远程模型目录（静态表，见 `models.rs`）：如实回答「没刷」，
    /// 不假装刷了一次。`supports_model_refresh()` 因此保持默认 false，
    /// 界面上不会给这家渲染「刷新模型清单」按钮。
    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move { ModelRefreshOutcome::unchanged() })
    }

    /// 编码套餐的网关会回自己的内部模型名，需要把 SSE 帧里的 `model` 改回
    /// 客户端请求的那个名字（与 raccoon 同一处境、同一处置）。
    fn sse_model_rewrite(&self) -> bool {
        true
    }
}

/// 从账号会话里读访问令牌。
///
/// `account_id` 为空时取本地区组内的当前账号（与 raccoon 的
/// `snapshot_for` 口径一致：非空 = 用户在界面上点名的那条账号，取不到就报错
/// 而不是回落到队首 —— 那会把「点名的账号坏了」变成「静默用了别人的额度」）。
fn session_access_token(
    store: &AccountStore,
    region: Region,
    account_id: &str,
) -> Result<String, GatewayError> {
    // 两个查询返回的是**两个不同的类型**（`CurrentEntry` / `SessionById`），
    // 它们都带一个 `session` 字段 —— 这里只取那一个字段，避免为一个取值
    // 动作引入第三种包装类型。
    let session = if account_id.trim().is_empty() {
        store
            .current_entry_for_provider(region.provider_id())
            .map(|entry| entry.session)
    } else {
        store.get_session_by_id(account_id).map(|entry| entry.session)
    };
    let session = session.ok_or_else(|| {
        GatewayError::with_status(401, "没有可用的 ZCode 账号，请先在「账号」页添加")
    })?;
    session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GatewayError::with_status(401, "ZCode 账号缺少 accessToken，请重新登录"))
}
