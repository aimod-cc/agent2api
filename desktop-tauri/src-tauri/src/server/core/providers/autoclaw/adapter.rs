//! AutoClaw 适配器（Agent2API 二期 W4b-T-c2）：把凭证层接进 provider 注册表。
//!
//! ── 上游长什么样（移植来源 `autoclaw-upstream-client.mjs`，逐条核对）──
//!   - LLM 代理：`POST {upstreamBaseUrl}/chat/completions`
//!     （`forwardModelRequest` 里 `upstreamPath = transport === 'anthropic-messages'
//!     ? '/v1/messages' : '/chat/completions'`；本期只做 OpenAI 协议，取后者），
//!     base 默认 `credentials::DEFAULT_UPSTREAM_BASE_URL`（§10），
//!     `autoclaw-upstream-client.mjs` 第 21 行的常量与它逐字相同。
//!   - 头集合 = `modelProxyUpstreamHeaders`（源 258-281 行）：
//!     `Content-Type` / `Accept: *//*` / `brandHeaders()` /
//!     **`X-Authorization: Bearer <token>`** / `X-Request-Id` /
//!     `X-Request-Model: <routeId>`，另有三个可选的客户端透传头。
//!     **注意认证头是 `X-Authorization` 而不是 `Authorization`**
//!     （§10 的规格文字写的是 Authorization，源实现是 X-Authorization ——
//!     以源实现为准，见下「与规格文字的一处分歧」）。
//!   - 两个模型标识（`X-Request-Model` 填路由 ID、`body.model` 填剥前缀后的
//!     模型 ID）由 `models::resolve_model_route` 给出，见 `models.rs` 模块头。
//!   - SSE：上游会回自己的 model 名，源实现**逐帧回写**成客户端请求的名字
//!     （`pipeSseWithModelRewrite` → `rewriteSseLine` 的
//!     `parsed.model = requestedModel`），非流式响应体同样回写
//!     （`forwardChatCompletions` 的 `payload.model = requestedModel`）。
//!     因此本适配器的 `sse_model_rewrite()` 返回 **true** —— 通用 SSE 层的回写
//!     机制随之生效（该结论的核对依据写进交付报告）。
//!
//! ── 与规格文字的一处分歧（已核对源实现）─────────────────────
//! 架构文档 §10 与任务书写的是 `Authorization: Bearer <token>`，而源实现
//! `modelProxyUpstreamHeaders` 用的是 **`X-Authorization`**（源 263 行；
//! 同一份 `X-Authorization` 也正是 `openclaw.json` 里 AutoClaw 桌面端自己写的
//! 头名，见 `credentials.rs` 的来源 2）。这里按**源实现**发 `X-Authorization`：
//! 那是唯一事实来源，认证头名发错会稳定 401。
//!
//! ── 三处「不做什么」（与源实现对齐，别顺手补）────────────────
//!   1. **不做 system 注入**：首条消息必须是 system 是 workbuddy 上游的硬要求，
//!      AutoClaw 的代理是 OpenAI 兼容网关，源实现从不改消息序列。
//!   2. **不认「默认模型」**（`supports_default_model` = false）：客户端的
//!      `defaultModel` 是 config.json 里的 workbuddy 语义（§4.4 末句）；
//!      AutoClaw 自己的默认是**路由层**的 `zai_auto` 回退，由
//!      `models::resolve_model_route` 在「客户端没给 model」时自行兜底。
//!   3. **不做 `anthropic-messages` 那条 transport**：本网关只暴露 OpenAI 协议
//!      （架构文档 §1），因此源实现里 `anthropic-version` / `anthropic-beta`
//!      两个头不适用；但三个 **AutoClaw 会话透传头**（`X-Session-Id` /
//!      `X-Agent-Id` / `X-ZCode-Invocation-Id`）与 transport 无关，照实现（见
//!      `passthrough_session_headers`）。
//!
//! ── 超时（源实现 vs 本网关）──────────────────────────────────
//! 源实现给 LLM 请求 15 分钟 cap、`redirect: 'manual'`。Rust 侧的抗超时在
//! `core::egress` 上（connect_timeout 30s + read_timeout 600s，**不设总超时**），
//! 与另外三家共用同一个出网点 —— LLM 是长回答场景，「不设总超时」正是源实现
//! `LLM_TIMEOUT_MS` 想达到的效果（15 分钟的硬 cap 只是它的兜底），因此这里不
//! 叠加请求级总超时。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件绝不 unwrap/expect/panic，取值走 Option 链与
//! `unwrap_or`；不持有任何锁（刷新回写在 await 之后才做，见 `persist_refresh`）。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, AutoClawCredentials, CredentialOrigin};
use super::models;
use super::refresh;
use super::catalog;

/// 客户端版本号（源实现 `createUpstreamClient` 的 `desktopAppVersion` 默认值；
/// `server.mjs` 从不覆盖它，所以这里是常量而不是配置项）。
///
/// 它进 `X-Version` 头：上游按它识别 AutoClaw 客户端版本，改动会让上游按不同
/// 的兼容路径解释请求。
const DESKTOP_APP_VERSION: &str = "1.17.8";

/// 上游语言 / 通道（源实现的 `lang` / `channel` 默认值，`server.mjs` 同样不覆盖）
const CLIENT_LANG: &str = "zh-CN";
const CLIENT_CHANNEL: &str = "official";

/// 客户端透传头：`(上游头名, 客户端头名)`（源 `modelProxyUpstreamHeaders` 的表）。
///
/// 客户端带了才发（`typeof value === 'string' && value.trim()`），没带就整条不发
/// —— 这三个是 ZCode/Agent 会话的关联 id，空值发上去会让上游按「有会话」处理。
const PASSTHROUGH_HEADERS: &[(&str, &str)] = &[
    ("X-Session-Id", "x-autoclaw-session-id"),
    ("X-Agent-Id", "x-autoclaw-agent-id"),
    ("X-ZCode-Invocation-Id", "x-autoclaw-zcode-invocation-id"),
];

/// AutoClaw 适配器（无状态单例，见 `adapter::adapter_for`）
pub struct AutoClawAdapter;

/// 进程级实例：适配器无状态，静态实例即可（`adapter_for` 返回它的引用）
pub static AUTOCLAW_ADAPTER: AutoClawAdapter = AutoClawAdapter;

impl ProviderAdapter for AutoClawAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::AutoClaw
    }

    /// AutoClaw 的模型清单（`autoclaw::models` 的静态路由表，源
    /// `autoclaw-models.mjs` 的 `MODELS`）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {upstreamBaseUrl}/chat/completions`（头集合照抄源实现
    /// `modelProxyUpstreamHeaders`，见模块头）。
    ///
    /// body **透传 + 改写一个字段**：`model` 换成 `body_model_id`（剥掉路由前缀
    /// 的模型 ID）。源实现是 `JSON.stringify({ ...body, model: route.bodyModelId })`
    /// —— 除了 `model` 一个字段，其余原样（含 `stream` / `tools` / 未知字段）。
    ///
    /// `account` 是**会话形态**（`store.get_session_by_id` /
    /// `auth.get_current_session` 的返回值）：Authorization 从
    /// `auth.accessToken` 取，与另外三家同一约定。桌面端账号（实时登录态）的
    /// token 由账号存储的 `session_from_record` 在构造会话时实时读入，
    /// 因此这里不需要（也不该）知道账号是不是桌面端。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let token = account
            .get("auth")
            .and_then(|auth| auth.get("accessToken"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "AutoClaw 账号缺少 accessToken，无法转发",
            ));
        }
        // 客户端请求的模型名 → (路由 ID, body 模型 ID)。客户端没给 model 时
        // `resolve_model_route` 回落到默认路由 `zai_auto`（源实现 requestedModel 的
        // 同款兜底），`requested_model` 也一并得到。
        let requested = body.get("model").and_then(Value::as_str).unwrap_or("");
        let route = models::resolve_model_route(requested);
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            // 源实现给的是 `Accept: */*`（LLM 代理两种响应形态都可能回；
            // 不是 text/event-stream —— 客户端要不要流式由 body.stream 决定）
            ("Accept".to_string(), "*/*".to_string()),
        ];
        headers.extend(brand_headers());
        headers.push((
            // 认证头名照抄源实现（`X-Authorization`，不是 `Authorization`）：见模块头
            "X-Authorization".to_string(),
            format!("Bearer {token}"),
        ));
        headers.push((
            "X-Request-Id".to_string(),
            crate::server::core::upstream::request::new_request_id(),
        ));
        headers.push(("X-Request-Model".to_string(), route.route_model_id.clone()));
        headers.extend(passthrough_session_headers(client_headers));
        let mut out_body = body.clone();
        if let Some(object) = out_body.as_object_mut() {
            object.insert("model".to_string(), Value::String(route.body_model_id.clone()));
        }
        // body 不是对象时原样透传（chat.rs 已保证是对象；这里的兜底只为不 panic，
        // 上游会自己报格式错误 —— 比在网关里编一个空对象更能说明问题）
        Ok(ChatRequestPlan {
            url: format!("{}/chat/completions", credentials::upstream_base_url()),
            headers,
            body: out_body,
        })
    }

    /// 上游错误分类（判定依据见下，逐条对照源实现 `forwardModelRequest`）：
    ///   - 401 → `TokenExpired`（刷新后同账号重试一次）；
    ///   - **429 → `QuotaLimited`**（按 HTTP 状态码；上游不给结构化恢复时间）；
    ///   - 其余 → `Fatal` 原样透传。
    ///
    /// ── 429 的判定依据（核对结论）──────────────────────────────
    /// 源实现**没有任何限额码**：`autoclaw-local-proxy` 全仓 `grep -rn "429"` 零命中，
    /// 也没有 `rateLimit` / 限额文案的判定；`forwardModelRequest` 对非 2xx 只做
    /// 「读 message → 抛 UpstreamApiError」这一步。因此本适配器与 raccoon 同档：
    /// **只看 HTTP 状态码 429**，`reset_at` 给 `None`（让冷却标记落 10 分钟兜底），
    /// `upstream_code` 取错误体里可能存在的 `code`（源实现不解析它，这里只是
    /// 「有就带出去」，没有也不影响判定）。
    ///
    /// ── 一处分歧：错误状态码的透传口径 ─────────────────────────
    /// 源实现把非 4xx 的上游状态统一收敛成 **502** 抛给自己的客户端；本网关的
    /// 四家适配器（workbuddy / raccoon / catpaw）一律**原样透传上游状态码**
    /// （`上游返回 {status}: {上游原文}` 的文案契约，架构文档 §4.2）。这里跟随
    /// 网关的既有口径而不是源项目的 HTTP 层口径：客户端看到的是真实状态码，
    /// 文案前缀与另外三家逐字同形。
    ///
    /// `retry_advice` 不覆写：源实现唯一的重试是「401 刷新后重试一次」，
    /// 那是通用链路的动作（`TokenExpired` 分支），**没有 WAF / 退避码**。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
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

    /// 取可用 access token：**凭证快照 + 临期主动刷新**（源实现 `currentCredentials`）。
    ///
    /// 临期窗口是 **5 分钟**（`credentials::PROACTIVE_REFRESH_MARGIN_MS`，对齐官方
    /// `DESKTOP_REFRESH_AHEAD_MS`）—— 原先是移植源项目的 120 秒，但那个值在官方
    /// 客户端里配的是 60 秒一轮的扫描，而网关这条判定由 10 分钟一轮的维护任务
    /// 驱动，2 分钟的窗口会被整轮错过。见常量处的完整说明。
    ///
    /// 账号解析顺序（源实现 `resolveCredentials`）：指定账号 → 账号组内的当前
    /// 账号 → 桌面端实时登录态 → `AUTOCLAW_TOKEN` 环境变量。见
    /// [`resolve_credentials`]。
    ///
    /// ── 写盘条件（本次修复）──────────────────────────────────
    /// 只有**真的刷新了**（返回的 access/refresh token 与快照不同）才回写账号
    /// 文件，而且回写走比较-再写（见 `persist_refresh`）。旧实现无条件写盘，
    /// 于是每个请求都会整库重写 accounts.json，且并发刷新后可能用旧快照覆盖新
    /// token。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let credentials = resolve_credentials(store, account_id)?;
            let refreshed = refresh::ensure_fresh(&credentials).await?;
            // 手动账号的刷新结果回写 accounts.json（桌面端账号按设计不回写，
            // 见 `persist_refresh`）：不回写的话，rotate 过后的 refreshToken
            // 只活在这一次请求里，下一次请求又从旧值开始刷。
            persist_refresh(store, &credentials, &refreshed);
            Ok(refreshed.token)
        })
    }

    /// 401（token 被上游拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 为什么必须覆盖默认实现：`ensure_access_token` 只在「临期」时刷新，而 401
    /// 完全可能发生在一个时间上还很新的 token 上（服务端侧失效、账号被顶下线、
    /// refreshToken 轮换）。此时只调 ensure 会拿回同一个被拒的 token，
    /// 编排层的「刷新后同账号重试一次」就退化成「用同一个坏 token 再打一次」。
    ///
    /// 源实现的对应行为：`forwardModelRequest` 里 `response.status === 401 &&
    /// credentials.canRefresh && !refreshed` → `refreshCredentials` → 重发一次 ——
    /// 与通用链路的 `TokenExpired` 动作逐字同义（刷新协议与单飞在
    /// `refresh.rs`，那里按 400002 降级 `agent-refresh`）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let credentials = resolve_credentials(store, account_id)?;
            let refreshed = refresh::refresh(&credentials, true).await?;
            persist_refresh(store, &credentials, &refreshed);
            Ok(refreshed.token)
        })
    }

    /// 拉取远程模型目录（`GET .../proxy/autoclaw-model-config`，见 `catalog.rs`）。
    ///
    /// 凭证取**当前账号**（与转发同一套：Bearer token + `refresh` 的签名头）；
    /// 没有可用登录态时返回 `unchanged()` —— 脚本 / CI 用户走环境变量旁路时
    /// 本就不该刷目录，报红色失败只会让他以为哪里坏了。
    ///
    /// §4.2 约定刷新失败不返回错误：失败时保留现有清单（`catalog::refresh`
    /// 内部就是这么做的，与另外三家一致），用户该看到的是「为什么没变」。
    ///
    /// `force` 一路透传给 `catalog::refresh`：`false` 走 5 分钟 TTL 早退
    /// （自动路径，与桌面端自身的轮询周期对齐），`true` 真打上游
    /// （用户手动点了「刷新模型清单」）。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            let credentials = match resolve_credentials(store, "") {
                Ok(credentials) => credentials,
                Err(error) => {
                    logging::verbose(
                        "[Models]",
                        &format!("AutoClaw 模型目录刷新跳过：{}", error.message),
                    );
                    return ModelRefreshOutcome::unchanged();
                }
            };
            catalog::refresh(&credentials, force).await
        })
    }

    /// AutoClaw **有**远程模型目录（`GET .../proxy/autoclaw-model-config`）。
    ///
    /// 这条声明曾经是 false，理由是「模型是静态路由表，上游没有目录接口」——
    /// 那个前提后来被证伪：接口在同 host 的 `/proxy/` 一级（**不是**对话用的
    /// `/proxy/autoclaw`），顺着 `chat/completions` 找永远找不到
    /// （见 `catalog.rs` 模块头）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// AutoClaw 有环境变量旁路（`AUTOCLAW_TOKEN` + 可选的
    /// `AUTOCLAW_REFRESH_TOKEN` / `AUTOCLAW_DEVICE_ID`，源实现 `envCredentials`），
    /// 因此账号列表为空时仍可用默认登录态转发（脚本 / CI 用户的常规用法）。
    fn allows_anonymous_default_session(&self) -> bool {
        true
    }

    /// 环境变量旁路凭证此刻是否存在（聚合目录判「这家现在有没有可用登录态」用）。
    fn env_credentials_present(&self) -> bool {
        credentials::env_credentials().is_some()
    }

    /// AutoClaw 没有「默认模型」概念：不指定模型时应由**它自己的路由层**回落到
    /// `zai_auto`（`models::resolve_model_route`），而不是被注入一个 workbuddy
    /// 语义的模型名（架构文档 §4.4 末句的「不注入」分支）。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// SSE 帧的 model 名回写：**要写**（核对结论见模块头）。
    ///
    /// 源实现 `pipeSseWithModelRewrite` 把每个下发帧的 `model` 改成客户端请求的
    /// 名字（`rewriteSseLine`：`'model' in parsed && parsed.model !== requestedModel`
    /// 时才改），非流式响应体同样回写。上游代理会回自己的路由/内部名，而客户端
    /// 按自己请求的名字识别响应 —— 不回写会让客户端看到 `zai_auto` 这类名字。
    /// 帧改写发生在通用 SSE 层（`upstream::sse` 的 `ModelRewrite`），本方法只
    /// 回答「要不要写」，通用层不出现任何 provider 分支。
    fn sse_model_rewrite(&self) -> bool {
        true
    }

    /// AutoClaw 支持主动刷新（`POST {userapi}/refresh`，签名校验失败时降级
    /// `agent-refresh`；实现在 `refresh.rs`）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 临期判定：取凭证快照，用凭证自己的 `is_expiring()`（5 分钟窗口）与
    /// `can_refresh()` 判一次 —— 与 `ensure_access_token` 里
    /// `refresh::ensure_fresh` 用的是同一对判据。
    ///
    /// ── 为什么必须有 `can_refresh()` ────────────────────────────
    /// AutoClaw 的凭证来源里有一条**天然不可刷新**：`openclaw.json`
    /// （`CredentialOrigin::GatewayConfig`）里只有 access token，没有 refreshToken
    /// （见 `credentials.rs` 模块头的备来源说明）。少了这道判定，这类来源会被
    /// 维护任务每轮都算成「待刷新」，然后稳定失败并往日志里灌错误。
    ///
    /// 桌面端 / 环境变量来源同理：没有 refreshToken 就返回 false。
    /// 取快照失败（账号不存在 / 凭证不可用）返回 false（见 trait 契约）。
    ///
    /// ── 每小时强制刷新（本次修复）───────────────────────────────
    /// 除了临期窗口，这里还接上官方那条「每小时无条件刷一次」的语义
    /// （见 `credentials::HOURLY_FORCED_REFRESH_INTERVAL_MS`）：目的不是等 token
    /// 快过期，而是把 refresh_token 温着 —— AutoClaw 服务端会轮换它，长期闲置
    /// 的那一份可能被判失效，之后只能重新登录。
    ///
    /// 只在**本方法**做、不在 `is_expiring()` 里做：后者被转发链路的
    /// `ensure_fresh` 每请求调用一次，在那里「每小时强制一次」会退化成
    /// 「每个请求都刷」。维护任务每 10 分钟才问一次，节流后正好是每小时一次。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.is_empty() {
            return false;
        }
        match resolve_credentials(store, account_id) {
            Ok(credentials) => {
                if !credentials.can_refresh() {
                    return false;
                }
                credentials.is_expiring()
                    || credentials::hourly_forced_refresh_due(
                        account_id,
                        crate::server::logging::now_ms(),
                    )
            }
            Err(_) => false,
        }
    }

    /// AutoClaw 有积分 / 订阅概念：资产钱包 + `subscribe-info`（`autoclaw/balance.rs`）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查积分 + 订阅（移植源实现 `account-balance.mjs` 的 `queryPoints` +
    /// `querySubscription`，合成一次调用）。
    ///
    /// ── 鉴权与出网（核对结论，见 `balance.rs` 模块头）─────────────
    /// 两条链路都在 userapi 域、都带那套 `X-Auth-Sign` 签名头 + Bearer，
    /// 且源实现是**裸 fetch**（不走账号代理）—— 与刷新接口同一取舍。
    /// 签名复用 `refresh::signed_auth_headers`（不复制第二份，理由见那里）。
    ///
    /// ── 为什么不在这里触发刷新 ──────────────────────────────────
    /// 与另外三家同一分工：适配器只提供「怎么查」，401 之后的刷新重试由
    /// `api::accounts::query_usage_inner` 统一处置（它调 `refresh_access_token`，
    /// 那是 force 语义）。本家 `supports_refresh = true`，因此那条链路对它是
    /// 有效的 —— 这是四家里唯一「401 重试真的能救回来」的家之一（另一家是小浣熊）。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { super::balance::query_usage(store, account_id).await })
    }
}

/// 品牌头（源实现 `brandHeaders`，逐条照抄）。
///
/// `X-Tm` 是平台标识（源实现 `platformTm()`：darwin→mac、linux→linux、其余 win），
/// `X-Version` 是客户端版本。`x_trace_id` 的**下划线写法也是照抄**（源实现里就
/// 是这个键名，与后面那个 `X-Request-Id` 并存 —— 前者是客户端指纹的一部分，
/// 后者是本次请求的关联 id）。
fn brand_headers() -> Vec<(String, String)> {
    vec![
        ("X-Product".to_string(), "autoclaw".to_string()),
        ("X-Client-Type".to_string(), "pc".to_string()),
        ("X-Harness-Type".to_string(), "zcode".to_string()),
        ("X-Tm".to_string(), platform_tm().to_string()),
        ("X-Version".to_string(), DESKTOP_APP_VERSION.to_string()),
        ("X-Lang".to_string(), CLIENT_LANG.to_string()),
        ("X-Channel".to_string(), CLIENT_CHANNEL.to_string()),
        ("x_trace_id".to_string(), "autoclaw-desktop".to_string()),
    ]
}

/// 平台标识（源实现 `platformTm()`）
fn platform_tm() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "win"
    }
}

/// 可选的客户端透传头（源实现 `modelProxyUpstreamHeaders` 末尾那个循环）。
///
/// 客户端带了才发（去空白后非空）；三个头与 transport 无关，OpenAI 协议同样适用。
fn passthrough_session_headers(client_headers: &HeaderMap) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (upstream_name, client_name) in PASSTHROUGH_HEADERS {
        let value = client_headers
            .get(*client_name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(value) = value {
            headers.push(((*upstream_name).to_string(), value.to_string()));
        }
    }
    headers
}

/// 本次请求的凭证（源实现 `resolveCredentials` 的优先级）。
///
/// 顺序：
///   1. `account_id` 非空 → 该 AutoClaw 账号的记录（桌面端账号实时读 auth.json）；
///   2. `account_id` 为空 → AutoClaw 组内的当前账号（若有）；
///   3. 都没有 → 桌面端实时登录态；
///   4. 桌面端也读不到 → `AUTOCLAW_TOKEN` 环境变量。
///
/// 2→3→4 的兜底链在 `credentials::snapshot_for(None)` 里（它是凭证层的统一入口，
/// `local_credentials().or_else(env_credentials)`）—— 本函数只负责「先按账号收窄」。
///
/// **环境变量排在最后**是与源实现一致的有意取舍：源实现的优先级是「账号列表
/// 选中项 > 环境变量」，所以只有**一个账号记录都没有**时才轮到环境变量。
fn resolve_credentials(
    store: &AccountStore,
    account_id: &str,
) -> Result<AutoClawCredentials, GatewayError> {
    let record = store.autoclaw_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            401,
            format!("AutoClaw 账号 {account_id} 不存在或不属于 AutoClaw（请重新添加）"),
        ));
    }
    credentials::snapshot_for(record.as_ref())
}

/// 刷新结果回写（**只回写手动账号**，且只在真的刷新了的时候）。
///
/// ── 为什么只回写 `AccountStore` 来源 ──────────────────────────
/// 桌面端实时登录态（auth.json / openclaw.json）**按设计不回写**：那是
/// safeStorage 加密格式（回写要用同一把 os_crypt 密钥重新加密，写坏会让用户
/// 桌面端都登不进去），且服务端每次刷新会轮换 refresh_token，网关回写会与桌面端
/// 自己的刷新互相顶掉 —— 源实现把桌面态标成 `persistRefresh: false` 正是这个原因
/// （架构文档 §10.2 的取舍，`credentials.rs` / `refresh.rs` 的模块头详述）。
/// 手动账号（`persistRefresh: true`）在源实现里是**要**回写的
/// （`account-store.mjs` 的 `updateAccountTokens`），这里对齐它。
///
/// ── 三个写盘前提（本次修复）──────────────────────────────────
///   1. **没刷新就不写**：`refreshed` 与刷新前快照 `previous` 的 token 逐字相同
///      时直接返回 —— 旧实现无条件写盘，每个请求都整库重写一次 accounts.json
///      （明明没有任何新信息），还会把并发期间别处写入的字段覆盖掉；
///   2. **只在刷新成功时写**：调用方只在 `Ok` 分支调用本函数（失败路径根本不
///      进这里），因此失败不会被当成新凭证写进账号文件；
///   3. **刷新了也要比较-再写**：`update_autoclaw_account_tokens_if_current`
///      在同一把账号锁内确认记录里仍是刷新前那份凭证，才写入新值。期间用户
///      重导入 / 删除 / 换号、或另一轮刷新先落地时，旧结果**不得覆盖**新凭证
///      （那时返回 `Stale`，不会把旧 token 当成成功结果写进去）。
///
/// 失败只记日志：刷新本身已经成功，回写失败不该让本次请求失败（与 raccoon 的
/// 比较写回同一取向）。本函数在 await 之后调用 —— 它只做一次账号文件写盘，
/// **不持锁穿越任何网络等待**。
fn persist_refresh(
    store: &AccountStore,
    previous: &AutoClawCredentials,
    refreshed: &AutoClawCredentials,
) {
    if refreshed.origin != CredentialOrigin::AccountStore {
        return;
    }
    // 前提 1：没有发生刷新 → 不写盘
    if refreshed.token == previous.token && refreshed.refresh_token == previous.refresh_token {
        return;
    }
    match store.update_autoclaw_account_tokens_if_current(
        &refreshed.id,
        &previous.token,
        &previous.refresh_token,
        &refreshed.token,
        &refreshed.refresh_token,
        refreshed.expires_at,
        &refreshed.device_id,
    ) {
        Ok(crate::server::core::account_store::CredentialWrite::Written) => {}
        Ok(crate::server::core::account_store::CredentialWrite::Stale) => {
            // 账号已被删除 / 重建，或凭证已被用户换掉：刷新结果作废，
            // 不覆盖当前凭证
            logging::verbose(
                "[AutoClaw]",
                &format!(
                    "账号 {} 的刷新结果已过期（凭证已被更换），未回写账号文件",
                    refreshed.id
                ),
            );
        }
        Err(reason) => {
            logging::verbose(
                "[AutoClaw]",
                &format!("账号 {} 刷新结果回写失败: {reason}", refreshed.id),
            );
        }
    }
}
