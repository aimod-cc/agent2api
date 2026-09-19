//! CatPaw 适配器（Agent2API 二期 W5-T-d4）：把会话式协议层接进 provider 注册表。
//!
//! ── 这一层解决什么问题 ──────────────────────────────────────
//! CatPaw 的协议实现（消息归一化 / 指纹链 / 会话注册表 / 轮次状态机 / 翻译层）
//! 全部收在 `providers/catpaw/` 内部（架构文档 §9），对外只留两个口子：
//!   - `conversation::run_conversation(ConversationRequest) -> ForwardOutcome`；
//!   - `ProviderAdapter` 的 **有状态转发入口** `forward_conversation(...)`
//!     （架构文档 §4.2.1，由本文件覆写）。
//! 本文件就是那个「接线」：把转发编排给的散装入参（原始 body、客户端头、
//! 账号 id、出网代理、流式标志、记账槽）装配成 `ConversationRequest`，再转调
//! `run_conversation`。装配以外的逻辑一行都不在这里 —— 任何「CatPaw 该怎么做」
//! 的判断都属于协议层。
//!
//! ── 与其它两家的形态差异（为什么 trait 要多一个钩子）──────────
//! WorkBuddy / 小浣熊是「一次 HTTP 请求 = 一次对话」：适配器把
//! [`ChatRequestPlan`] 拼好，编排层的 `send_chat_request` 负责发。CatPaw 的一个
//! 客户端请求会变成好几个上游请求（round → event → turn → 工具循环 →
//! event），中间还有注册表与指纹链 —— 这些**不能**塞进单请求契约，所以：
//!   - `is_stateful()` 返回 true → 编排层走 `forward_conversation`；
//!   - `build_chat_request()` 返回 503（**防御性**报错：有状态 provider 不该
//!     被单请求路径调用，真被调用说明编排层的分流坏了，报错比发一个语义不对的
//!     请求安全得多）。
//!
//! ── 凭证：X-Passport-Token + user-uid，且**无法刷新**（§9.1）────
//! 与另外两家最本质的差别（详见 `credentials.rs` 的模块头）：
//! CatPaw 的凭证是 Cookie 形态的 `X-Passport-Token` 值，`uid` 是独立请求头
//! （它不在 token 里，不是 JWT），而**没有 refreshToken 概念** —— token 过期
//! 只能在桌面端重新登录（桌面端账号的凭证是实时读 `auth.json` 的，所以
//! 「重新登录」立即恢复）。因此：
//!   - `ensure_access_token` 只做「取凭证 + 存在性校验」；
//!   - `refresh_access_token` 沿用默认实现（= ensure），并在这里明确它的语义
//!     —— 401 之后「刷新再重试」这条链在 CatPaw 上**没有可刷新的东西**，
//!     重试会拿回同一个被拒的 token。编排层不会对有状态 provider 走那条链
//!     （见 `provider_loop` 的分流），这里保持默认实现只是为了让 trait 契约完整。
//!
//! ── 限额 / 轮换语义的核对结论（架构文档 §4.2 的三个动作）──────
//! **原项目没有多账号轮换**：`catpaw-local-proxy` 全仓没有任何 429 判定
//! （`grep -rn "429" *.mjs` 零命中），账号选择是「用户在账号页选中的那一个」
//! （`account-store.mjs` 的 `getCurrentCredentials`），切换账号只做一件事
//! —— `clearClientToolSessions()` 作废旧 conversation（`account-routes.mjs`
//! 的 `notifySwitch`），没有任何「限额 → 换下一个账号」的链路。
//! 因此本适配器对上游错误**一律 Fatal 透传**（`classify_error` 恒 Fatal，
//! 有状态路径连它都不会被调用：`run_conversation` 内部已经把手上的
//! HTTP 状态与上游文案归一到 `GatewayError`）。这条结论与依据写进了交付报告；
//! 编排层的 QuotaLimited 冷却逻辑对有状态 provider 保持不触发（没有可标记的
//! 「限额」信号），但**账号选路循环、provider 轮询、telemetry 记账照常共用**。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；本文件不持任何锁。

use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::ForwardOutcome;
use crate::server::errors::GatewayError;

// 跨层导入用全路径（本文件在 `providers/catpaw/` 下，`super` 是本目录）：
// trait 与注册表工具都在上一层，写全路径比堆两层 super 更好读。
use super::conversation::{run_conversation, CatPawCredentials, ConversationRequest};
use super::models::MODELS;
use super::registry::AccountIdentity;
use super::{conversation, credentials};
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::{kind_id, ProviderKind};

/// CatPaw 适配器（无状态单例：会话状态在 `conversation::registry()` 的进程级
/// 句柄里，凭证在账号存储 / auth.json / 环境变量里，本结构不持有任何字段）。
pub struct CatPawAdapter;

/// 进程级实例（`adapter_for` 返回它的 `&'static` 引用）
pub static CATPAW_ADAPTER: CatPawAdapter = CatPawAdapter;

impl ProviderAdapter for CatPawAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::CatPaw
    }

    /// 架构文档 §4.2.1：CatPaw 的上游是**多步会话协议**（固有性质）。
    ///
    /// 编排层据此把本 provider 分流到 [`Self::forward_conversation`]，
    /// 而不是 `build_chat_request` + `send_chat_request` 那条单请求路径。
    fn is_stateful(&self) -> bool {
        true
    }

    /// 静态模型清单（`models.rs` 的 `MODELS` 表 → 聚合目录认的条目形态）。
    ///
    /// ── 键名映射（对齐 `raccoon::models::listing_entry` 的口径）──
    ///   聚合层的 `models::list_item` 读的是：`id`（模型 id）+ `name`（展示名）
    ///   + `maxInputTokens` / `maxOutputTokens` / `supportsImages` /
    ///   `supportsReasoning` / `supportsToolCall` / `kind`。
    ///   本表提供的键是 `id` / `name` / `host_model_id` / `context_windows` /
    ///   `default_context_window`，于是这里做三件事：
    ///   1. `id` / `name` 直接搬（`glm-5.3-flash` / `kimi-k3` 就是**上游认的名字**，
    ///      `models::resolve_model_request` 按它反查 `modelType`，不做任何改写）；
    ///   2. 上下文窗口 → `maxInputTokens`：取该模型的档位上限
    ///      （`context_windows` 的最大值；没有档位的模型不输出该键 ——
    ///      `kimi-k3` 就是不支持 context 参数的那个，报 0 会让前端显示错误的容量）；
    ///   3. 能力位来自 `MODELS` 表（原表 `supportImage` / `supportThinking` 为真；
    ///      工具调用是这套协议的固有能力 —— round/turn 请求体每次都带
    ///      `toolConfigs` / `availableTools`，见 `conversation::build_turn_body`）。
    ///
    /// 为什么不做「远程刷新」：上游不提供模型目录接口（原项目也只有一个写死的
    /// `MODELS` 表），见 `refresh_models`。
    fn list_models(&self) -> Vec<Value> {
        MODELS
            .iter()
            .map(|model| {
                let mut entry = json!({
                    "id": model.id,
                    "name": model.name,
                    "supportsImages": model.support_image,
                    "supportsReasoning": model.support_thinking,
                    "supportsToolCall": true,
                    "kind": "chat",
                    // 上游数字 modelType：进清单条目只为排障（前端不渲染它），
                    // 但少了它「清单里的名字 ↔ 上游模型」这层对照只能翻源码
                    "modelType": model.host_model_id,
                });
                // 上下文窗口取档位上限：默认档位是「不显式请求时用哪个」，
                // 上限才是这个模型真正能吃下的量（客户端按 max_input_tokens 估算截断）
                if let Some(window) = model
                    .context_windows
                    .iter()
                    .filter_map(|text| text.parse::<i64>().ok())
                    .max()
                {
                    if let Some(object) = entry.as_object_mut() {
                        object.insert("maxInputTokens".to_string(), Value::from(window));
                    }
                }
                entry
            })
            .collect()
    }

    /// **防御性报错**：CatPaw 走会话式转发，不走单请求路径（模块头）。
    ///
    /// 走到这里说明编排层的 `is_stateful` 分流坏了（或将来有人误加了调用点）。
    /// 返回 503 而不是「尽力构造一个请求」：单请求构造不出 round/turn/工具循环
    /// 那套时序，硬发出去只会得到一个语义不明的上游错误。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            503,
            format!(
                "{} 走会话式转发，不走单请求路径（内部错误：编排层未按 is_stateful 分流）",
                kind_id(self.kind())
            ),
        ))
    }

    /// 一律 Fatal（模块头的核对结论）：原项目没有多账号轮换，也没有可解析的
    /// 限额码 —— 上游错误在 `run_conversation` 内部已经归一到 `GatewayError`
    /// （状态码与文案都按原实现的口径映射好），本方法在正常路径上走不到。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let message = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误")
            .to_string();
        UpstreamErrorClass::Fatal { status, message, upstream_code: None }
    }

    /// 取可用凭证（`X-Passport-Token` + `uid`），**含存在性校验**。
    ///
    /// `account_id` 为空表示「没有指定账号」：环境变量旁路（`CATPAW_COOKIE`）
    /// 优先，其次是桌面端实时登录态（`~/.meituan-catpaw/auth.json`）——
    /// 与 workbuddy / 小浣熊同一分工（适配器回答「此刻能不能拿到凭证」，
    /// 账号层的默认会话回答「有没有账号记录」）。
    ///
    /// 返回的是 **token 字符串**（trait 的签名如此）：uid 由
    /// [`Self::forward_conversation`] 单独取（它需要完整凭证）。这里不返回
    /// 空串假装成功 —— 拿不到就报错，且文案告诉用户该怎么补。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if account_id.is_empty() {
                if let Some(credentials) = credentials::env_credentials() {
                    return Ok(credentials.token);
                }
            }
            let credentials = credentials::snapshot_for(store, account_id)?;
            if credentials.token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    503,
                    "CatPaw 没有可用的登录凭证：请在账号页添加账号（粘贴登录态或导入桌面端登录态）",
                ));
            }
            credentials::log_source(
                if account_id.is_empty() { "默认登录态" } else { "账号记录" },
                account_id,
                &credentials.uid,
            );
            Ok(credentials.token)
        })
    }

    /// 无操作：清单是**静态表**（上游不提供模型目录接口，原项目同样写死
    /// `proxy-chat-utils.mjs` 的 `MODELS`）。模型 id（`kimi-k3` /
    /// `glm-5.3-flash`）是固定的，没有「重新拉取」这件事。
    ///
    /// §4.2 约定刷新失败不返回错误；这里连失败都谈不上 —— 返回
    /// `unchanged()`（没刷、也不是错误），让自动路径遍历到本家时零副作用。
    ///
    /// `force` 在本家**没有可绕过的缓存**：没有远程目录，就没有缓存。
    /// 即使调用方传 `true`（手动刷新路径），结果同样是「没有新清单可拿」——
    /// 手动路径不会走到这里（`supports_model_refresh()` 为 false，先被拦成
    /// `skipped`），本实现是给自动路径的零副作用保证，以及「万一有人直接调
    /// 也不会有副作用」的兜底。
    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async { ModelRefreshOutcome::unchanged() })
    }

    /// CatPaw **不支持**刷新模型清单：上游没有模型目录接口（美团 CatPaw 的
    /// 模型 id 是固定的，原项目也只有一个写死的 `MODELS` 表）。
    /// 保持默认 false，界面对这家如实说明「使用固定清单」。
    fn supports_model_refresh(&self) -> bool {
        false
    }

    /// 环境变量旁路（`CATPAW_COOKIE`，原项目 `runtimeConfig` 的 explicitAuth）。
    ///
    /// 脚本 / CI 用户不建账号记录也能用；`uid` 用 `CATPAW_USER_UID`（可空 ——
    /// 上游对 `user-uid` 头是「有就带」的语义，见 `upstream_http::request_headers`）。
    fn allows_anonymous_default_session(&self) -> bool {
        true
    }

    /// 环境变量凭证此刻是否存在（聚合目录判「这家现在有没有可用登录态」）。
    fn env_credentials_present(&self) -> bool {
        credentials::env_credentials_present()
    }

    /// CatPaw 没有「默认模型」概念（与其它两家同）：客户端不指定时应由上游用它
    /// 自己的默认，而不是被注入一个 workbuddy 语义的模型名。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// 不做 SSE 帧的 model 回写：CatPaw 的翻译层（`openai.rs`）自己产出 OpenAI
    /// chunk，`model` 字段填的就是 `prepared.resolution.display_name`（客户端请求
    /// 的那个名字，见 `turn_executor::TurnContext.model_name`）—— 上游原始事件里
    /// 根本没有 OpenAI 形态的 `model` 字段，通用回写层无事可做。
    fn sse_model_rewrite(&self) -> bool {
        false
    }

    // ─── 凭证自动维护：**不实现**（沿用 trait 的默认 `false`）─────
    //
    // `supports_refresh` / `credentials_expiring` 都保持默认值，这不是「还没做」，
    // 而是如实回答。依据（`credentials.rs` 的模块头与架构文档 §9.1）：
    //   1. **上游没有刷新接口**。CatPaw 的凭证是桌面端会话 Cookie 里的
    //      `X-Passport-Token` 值，不是 JWT 也不是 Bearer —— 它没有 `exp` 可解析，
    //      `CatPawCredentials` 里就只有 `token` + `uid` 两个字段（见
    //      `conversation.rs`），**没有 refreshToken，也没有任何过期时间**。
    //      原项目（`catpaw-local-auth.mjs`）同样只有「读凭证」，没有刷新调用。
    //   2. 因此 `credentials_expiring` 也**无从判定**：没有过期信息就是判不出来，
    //      按 trait 契约返回 false（凭猜测去打上游只会制造噪音）。
    //   3. token 真过期时的恢复路径是**用户在桌面端重新登录**：桌面端账号的凭证
    //      每次实时读 `~/.meituan-catpaw/auth.json`（`credentials.rs` 刻意不做
    //      mtime 缓存），所以「重新登录」立即恢复，不需要网关做任何事。
    //
    // 于是维护任务遍历到 CatPaw 时会先被 `supports_refresh()` 挡掉，一条失败日志
    // 都不会产生 —— 这正是「先过滤再刷新」的意义。
    // `POST /api/accounts/refresh` 上那条明确 400 的提示（`api::accounts`）承担
    // 「用户手动点刷新」时的说明职责，两者口径一致。

    /// CatPaw 有余额概念：`GET credit.catpaw.meituan.com/api/credit/balance`
    /// （`catpaw/balance.rs`）。
    ///
    /// ── 但这个 true 与「点了就能查到」不是一回事 ────────────────
    /// 该接口要的是**网页会话凭证 token2**（原项目单独配置的一项），落在这个
    /// 账号记录的 `balanceToken` 字段（或旧数据导入留下的 `balanceCookie.token2`）
    /// 上 —— 转发用的 `X-Passport-Token` 在那边不认。没配置时 `query_usage`
    /// 返回可识别的「未配置」（400 + `usage_not_configured`），前端显示成中性
    /// 提示而不是红色失败。
    ///
    /// 为什么仍然返回 true（而不是「没配就没有能力」）：`supports_usage` 回答的是
    /// **这一家有没有这个概念**（恒定事实），「这个账号配没配」是运行时状态，
    /// 由 `query_usage` 的结果表达。返回 false 会让前端根本不渲染「积分」按钮，
    /// 用户连「去配置它」的入口都看不到。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查余额（`catpaw/balance.rs`）。
    ///
    /// 凭证按「账号记录里的 `balanceToken` → 导入留下的 `balanceCookie.token2`」
    /// 取（见该模块头）；都没有时返回未配置错误。
    ///
    /// ── 401 之后**没有**刷新重试（这是本家与另外三家的关键差别）────
    /// 调用方（`api::accounts::query_usage_inner`）在 401 时会查
    /// `supports_refresh()` 再决定要不要刷新重试；本适配器 **`supports_refresh`
    /// 保持默认 false**（模块头的核对结论：CatPaw 上游没有刷新接口，
    /// `X-Passport-Token` 过期只能在桌面端重新登录），因此那条重试对本家
    /// 必然不做 —— 这不是漏掉的分支，是正确行为：拿一个必然失败的刷新去打上游
    /// 只会把一条「凭证过期」的错误变成两条。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { super::balance::query_usage(store, account_id).await })
    }

    /// 会话式转发入口（架构文档 §4.2.1）：装配 `ConversationRequest` 后转调
    /// `conversation::run_conversation`。
    ///
    /// ── 分工（哪些判断不属于这里）──────────────────────────────
    ///   账号选路、限额冷却、provider 轮询、telemetry 记账都在编排层
    ///   （`upstream::provider_loop`）；轮次判定、注册表、指纹链、图片压缩、
    ///   翻译层都在协议层。本函数只做**装配**：
    ///     `body` → `ConversationRequest.body`（原始 OpenAI 请求体，协议层自己归一化）；
    ///     `client_headers` → `session_id`（`x-session-id`，协议层的归一化规则）；
    ///     `account_id` / `proxy` / `stream` 原样传；
    ///     `base_url` 取 `CATPAW_UPSTREAM_BASE_URL`（可覆盖，默认 §9 的上游地址）。
    ///
    /// ── 凭证怎么取 ────────────────────────────────────────────
    /// 与 `ensure_access_token` 同一条链（账号记录 → 桌面端实时登录态），
    /// 但**没有指定账号时先看环境变量** —— 那条旁路要拿到 uid，而
    /// `ensure_access_token` 只回 token，所以这里重新取一次完整凭证。
    fn forward_conversation<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        body: &'a Value,
        client_headers: &'a HeaderMap,
        proxy: Option<ResolvedProxy>,
        stream: bool,
        telemetry: &'a Arc<RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ForwardOutcome, GatewayError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let credentials = resolve_credentials(store, account_id)?;
            // ── 账号身份对账（转发选路时，见 registry 的 `AccountIdentity`）──
            // 必须在这里做，而不是只在导入/添加账号时：桌面端实时登录态
            // （`~/.meituan-catpaw/auth.json`）由 CatPaw 客户端自己维护，用户可以在
            // 客户端里直接换一个账号登录 —— 那时 account_id（`desktop-auth` 或空）
            // 一个字都没变，账号层的作废入口（`invalidate_catpaw_sessions`，判据是
            // account_id）完全看不出异常。这里拿**本次实际使用的凭证**里的 uid 与
            // 注册表记录的该账号身份比对，不一致就把该账号名下的会话全部作废，
            // 于是客户端换号后的**下一次请求**就会走全新会话（全量 round），
            // 而不是续接到上一个用户的 conversationId 上。
            //
            // 开销：身份没变时注册表内部 O(1) 早退（一次 HashMap 查 + 一次比较），
            // 不扫表、不读盘、不发网络请求（凭证已在手上）。
            let identity =
                AccountIdentity::new(account_id, credentials.uid.as_str());
            conversation::registry().reconcile_identity(&identity);
            // 用 `ConversationRequest::new` 装配（它已经带好会话 id 的归一化与默认
            // 上游地址），再补两个「编排层才知道」的字段：base_url 走环境变量覆盖、
            // telemetry 是本次请求的记账槽
            let mut request = ConversationRequest::new(
                body.clone(),
                ConversationRequest::session_id_from_headers(client_headers),
                account_id.to_string(),
                credentials,
                proxy,
                stream,
            );
            request.base_url = credentials::upstream_base_url();
            request.telemetry = Some(telemetry.clone());
            run_conversation(request).await
        })
    }
}

/// 取本次会话转发的凭证（账号记录 / 环境变量旁路 / 桌面端实时登录态）。
///
/// 顺序：指定账号 → 该账号记录（桌面端账号实时读 auth.json）；没有指定账号 →
/// 环境变量旁路（脚本/CI）→ 桌面端实时登录态（本机的默认登录态就是它）。
/// 环境变量在**没有指定账号**时才参与，与 workbuddy / 小浣熊的
/// `ensure_access_token` 同一口径（用户有账号时不该被环境变量顶掉）。
fn resolve_credentials(
    store: &AccountStore,
    account_id: &str,
) -> Result<CatPawCredentials, GatewayError> {
    if account_id.is_empty() {
        if let Some(credentials) = credentials::env_credentials() {
            credentials::log_source("环境变量（CATPAW_COOKIE）", "", &credentials.uid);
            return Ok(credentials);
        }
    }
    let credentials = credentials::snapshot_for(store, account_id)?;
    credentials::log_source(
        if account_id.is_empty() { "默认登录态" } else { "账号记录" },
        account_id,
        &credentials.uid,
    );
    Ok(credentials)
}
