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

/// 上游「请求太大」的判据 —— `classify_error` 与 `resends_same_body_in_place`
/// **共用**这一个函数：两处各写一遍迟早分叉，分叉的症状是「文案写着超限、
/// 却还是把整份超大 body 原地重发好几遍」。
///
/// 取值集合抄自 officeace2api 的 `classifyUpstream`（同一份实测）：`81113` 是
/// 条数超限那条线给的码，其余四条是同一件事在 APIG / MaaS 两层的不同措辞。
/// 参数**要已小写**。
fn request_is_too_large(lower: &str) -> bool {
    [
        "81113",
        "exceeds the maximum size",
        "requestentitytoolarge",
        "payload too large",
        "request body is too large",
        "request body is too long",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

impl ProviderAdapter for OfficeAceAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OfficeAce
    }

    /// 模型清单来自远程目录（`models.rs` 的进程缓存 + 落盘缓存）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 对外广告前把「这个号实际打不通」的模型收窄掉（默认隐藏）。
    ///
    /// ── 为什么本家要覆写它（其余家都是恒等）──────────────────────
    /// 上游目录（`GET {网关}/v1/models`）会列出 28~34 个名，但**实测一个号只有
    /// 11 个真的能打通**，其余回 `81004`（没权限）/ `81009`（名字不认）。本仓的
    /// 适配器把这两类归成 `Fatal` —— **不换家、不冷却**，一个点名到无权限模型的
    /// 请求会直接把这个上游错误透传给客户端。收窄是**门禁**（见 trait 文档）：
    /// 收窄掉的名字客户端看不到、也点不动（400 `model_not_found`），于是「照着一个
    /// 一个试、试一个错一个」在入口就被挡住。
    ///
    /// ── 谁提供这份隐藏集合 ──────────────────────────────────────
    /// `probe` 模块：目录刷新后主动探一轮（`max_tokens: 1`，只问「认不认这个名字」），
    /// 结论落盘、6 小时 TTL。**没探过时隐藏集合为空 = 不藏任何东西** —— 这是刻意的：
    /// 宁可漏藏几个让客户端多试一次，也不在还没有结论时把用户真能用的模型挡在门外
    /// （trait 文档那条「写错会把用户真能用的模型挡在门外」）。
    fn advertise_models(&self, _store: &AccountStore, manifest: Vec<Value>) -> Vec<Value> {
        let hidden = super::probe::hidden_ids();
        if hidden.is_empty() {
            return manifest;
        }
        manifest
            .into_iter()
            .filter(|item| {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                !super::probe::is_hidden(&hidden, id)
            })
            .collect()
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

    /// 上游错误分类。**判据**取自 officeace2api 实测的码表（`upstream.mjs` 的
    /// `classifyUpstream`），**动作**按本仓四档语义走（见 `adapter::UpstreamErrorClass`）：
    ///
    ///   - `81113` / `exceeds the maximum size` → `Fatal`，**状态码折成 413**：上游给这条
    ///     带的是 HTTP 429，不先判掉就会被下面的限额档吞掉（见 `request_is_too_large`）
    ///   - `81111` / `81112` / `81114` / `0308` / `TPM` / `rate limit` / `too many requests`
    ///     / HTTP 429 → `QuotaLimited`（冷却「该凭据 + 该模型」；上游不给恢复时间，走兜底时长。
    ///     后两条措辞与参考实现同源，不是这里自加的）
    ///   - `81004`（这个模型没权限）/ `81009`（名字不认）→ `Fatal`。⚠️ 本仓的 `Fatal`
    ///     **什么都不冷却**（冷却与换家只由 `QuotaLimited` 触发，`Fatal` 是原地重发后透传），
    ///     参考实现那句「只冷却凭据×模型这一对」在这里不是靠分类做到的 —— 承担它的是
    ///     `probe.rs` 的可用性收窄：这两个码被探到就把该模型默认隐藏，客户端点不到它。
    ///     结论一致、机制不同，别把这句话读成「这里会去冷却某个模型」
    ///   - `APIG.1009` / `APIG.1001` / `APIG.1002` / 401 / 403 → `Fatal`
    ///     （凭据失效，用户需重新登录/导入；不自动冷却账号池）
    ///   - 其余 → `Fatal`
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let text = error_body.to_string();
        let lower = text.to_ascii_lowercase();
        // ── 体积超限**必须先于** `status == 429` 那一条判掉 ──────────────
        // 上游给这条的 HTTP 码就是 429（参考实现踩过同一个坑，见
        // `officeace2api/docs/技术笔记.md` 的 requestTooLarge 一节）。落进限额档的后果
        // 有两层，都不是「多试一次」能兜住的：
        //   ① 账号被记一次限额冷却并换号 —— 可它拒的是这份 body 的条数，跟这个号无关，
        //      换号后同一份 body 照样被拒，等于把下一个号也拖进同一笔白账；
        //   ② 客户端收到透传的 429，按「暂时忙」继续无限重试一个**必然失败**的请求。
        // 上游原文自报了家门（`messages max size is: 5000`），所以对策写得具体：
        // 少的是条数，不是每条的长度 —— 参考实现实测 token 窗口远够不到。
        if request_is_too_large(&lower) {
            return UpstreamErrorClass::Fatal {
                status: 413,
                message: format!(
                    "请求超过上游上限：{text}（这条拒的是 messages 的条数，上游实测上限 5000 条，\
                     与 token 数无关：把历史清短一些 —— 要减少条数，光把每条改短没用）"
                ),
                upstream_code: None,
            };
        }
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
        // 其余（模型无权限、凭据失效）一律致命：不罚账号池
        UpstreamErrorClass::Fatal {
            status,
            message: format!("上游返回 {status}: {text}"),
            upstream_code: None,
        }
    }

    /// 体积超限不在同一账号上原地重发（与 zcode 的 `model not allowed` 同构的否决）。
    ///
    /// 上面那条分类把它折成了 `Fatal`，而编排层对 `Fatal` 的默认承诺是
    /// 「先在这个账号上多试几次再换人」（见 `provider_loop::fallback_retry_advice`）——
    /// 这条对体积判定是纯浪费：它拒的是这份 body 的条数，重发一次就要把整份
    /// 超大请求体**再上传一遍**（参考实现实测那一次是 5.5 MB ×3 遍）。
    /// 判据与分类侧共用 `request_is_too_large`，两处不会分叉。
    fn resends_same_body_in_place(&self, _status: u16, error_body: &Value) -> bool {
        !request_is_too_large(&error_body.to_string().to_ascii_lowercase())
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

    /// 本家把**内容审核拒绝**包成 `HTTP 200 + finish_reason=content_filter` 的
    /// 假答复（正文是一句固定拒绝话术，见 `upstream::refusal` 模块头）。
    ///
    /// 实测（2026-10-10，dev）：ZCode 的 agentic 请求（巨型 system/工具/历史）
    /// 打到本家的 `deepseek-v4.1-flash` 时，上游条条回这句套话 + 该 finish_reason，
    /// 而同形状请求走 catpaw / codearts 都正常作答 ⇒ 是本家上游的输入审核拦截。
    /// 声明它之后编排层会把它当**可轮换失败**：队列里还有别的账号/家就换，
    /// 只剩这一家时给客户端一句可读错误（不再把套话当答案）。
    ///
    /// ⚠️ 只声明这一个取值：`length`（推理吃光预算）是**正常**收尾，不该换家。
    fn stream_refusal_finishes(&self) -> &'static [&'static str] {
        &["content_filter"]
    }

    /// 有余额查询能力：读订阅快照（`GET /v1/subscription`，V11 签名）并归一成
    /// `query_usage` 契约的形状（见 `balance` 模块头）。前端「积分」按钮与批量查询
    /// 都按它是否 true 决定要不要算这一家。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查余额 / 积分（`balance::query_usage`）。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(super::balance::query_usage(store, account_id))
    }

    /// **支持续期**：控制面临时凭据约 2 小时到期，用落盘的一次性 refresh token +
    /// 当初登录那把 DPoP 私钥重打令牌端点（`oauth::LoginFlow::refresh`）换新的
    /// AK/SK。刷新链的实现见 `refresh_control_plane`。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 该账号的控制面凭据是否临期（到期前 30 分钟算临期）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        store
            .officeace_account_record(account_id)
            .and_then(|record| credentials::from_record(Some(&record)).ok())
            .is_some_and(|credential| credential.control_plane_expiring())
    }

    /// 控制面续期：refresh token + DPoP 私钥 → 新的临时 AK/SK，写回账号记录。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move { super::refresh_control_plane(store, account_id).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::core::providers::adapter::adapter_for;
    use serde_json::json;

    /// 上游给「请求太大」时的**真实形态**（officeace2api 实测，`upstream.mjs` 的
    /// `requestTooLarge` 说明）：HTTP 状态是 **429**，业务码才是 81113，
    /// 原文自报了家门 —— 上限说的是 messages 条数。
    fn too_large() -> Value {
        json!({
            "error_code": "ModelArts.81113",
            "error_msg": "Invalid request body, exceeds the maximum size , messages max size is: 5000."
        })
    }

    /// 真限流（81111）与裸 429：修体积那条不许把它们一起误伤（参考实现在
    /// `test-admin.mjs` 里为这同一件事钉了 8 条反向断言）。
    #[test]
    fn real_rate_limits_still_cool_the_account() {
        let adapter = adapter_for(ProviderKind::OfficeAce);
        for body in [
            json!({"error_code": "ModelArts.81111", "error_msg": "Too many requests"}),
            json!({"error": {"message": "throttled"}}),
        ] {
            match adapter.classify_error(429, &body) {
                UpstreamErrorClass::QuotaLimited { status, .. } => assert_eq!(status, 429),
                other => panic!("这条应当按限额处理（冷却 + 换号），实际 {other:?}"),
            }
        }
    }

    /// 分类侧与免重发侧读的是**同一份判据**：只在一处认出来，就会留下
    /// 「文案写着超限、却还是把整份超大 body 白重发几遍」的分裂结论。
    #[test]
    fn the_size_rejection_becomes_a_readably_413_and_is_not_resent() {
        let adapter = adapter_for(ProviderKind::OfficeAce);
        let body = too_large();
        let (status, message) = match adapter.classify_error(429, &body) {
            UpstreamErrorClass::Fatal { status, message, .. } => (status, message),
            other => panic!("体积超限不该罚账号池，实际 {other:?}"),
        };
        assert_eq!(status, 413, "客户端要看到「请求太大」而不是「暂时忙」：{message}");
        assert!(
            message.contains("exceeds the maximum size"),
            "文案要保住上游原话：{message}"
        );
        assert!(message.contains("条数"), "对策要说清是减少条数：{message}");
        assert!(
            !adapter.resends_same_body_in_place(429, &body),
            "确定性失败，重发只会把整份超大 body 再传一遍"
        );
    }

    /// 反向：正常的 429 限额**仍然**保留原地重发的可能性（本家它不重发是因为
    /// 分类落 QuotaLimited，与这条否决无关）。
    #[test]
    fn the_veto_is_limited_to_the_size_markers() {
        let adapter = adapter_for(ProviderKind::OfficeAce);
        assert!(adapter.resends_same_body_in_place(429, &json!({"error_code": "ModelArts.81112"})));
        assert!(adapter.resends_same_body_in_place(400, &json!({"error_msg": "Invalid model"})));
    }
}
