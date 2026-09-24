//! 小浣熊（raccoon）适配器：Agent2API 的第二个 provider（架构文档 W3-T4）。
//!
//! ── 上游长什么样（移植来源 `raccoon-upstream-client.mjs`）────────
//!   - LLM 网关：`POST {llmBase}/chat/completions`（默认
//!     `https://xiaohuanxiong.com/api/web/llm/v2`），`Authorization: Bearer <JWT>`，
//!     `Content-Type: application/json`，OpenAI 兼容 body，SSE 流式。
//!   - 模型 id **无前缀概念**：客户端给什么就原样发给上游（`body.model` 直填）。
//!   - 鉴权：`POST {authBase}/refresh`，body `{"refresh_token": "..."}`
//!     （见 `credentials.rs`）。
//!   - 超时：源实现给 LLM 请求 15 分钟、鉴权/目录 30s / 10s。Rust 侧的抗超时
//!     机制在 `core::egress` 上（connect_timeout 30s + read_timeout 600s，
//!     **不设总超时**），与 workbuddy 共用同一个出网点 —— LLM 是长回答场景，
//!     「不设总超时」正是源实现 `bodyTimeout: 0` 的等价语义，因此这里不再
//!     叠加请求级总超时。
//!
//! ── 与 workbuddy 适配器的三处**关键差异**（别照抄）──────────────
//!   1. **不注入 system 消息**：`首条消息必须是 system prompt` 是 workbuddy
//!      上游的硬要求；小浣熊网关是通用 OpenAI 兼容实现，源实现从不改消息序列
//!      （只过滤空 content 的 assistant 历史 —— 那一步本期不做，见下）。
//!   2. **不认「默认模型」**：`supports_default_model()` 返回 false，
//!      于是 `api::chat` 不会把 config 的 `defaultModel`（workbuddy 语义）注入给
//!      小浣熊请求。源实现的默认模型回退发生在「上游客户端内部」，而网关侧的
//!      模型校验/路由在改造后是聚合层的事（架构文档 §4.4 末句明确这样收窄）。
//!   3. **环境变量旁路**：`RACCOON_TOKEN` 是小浣熊自己的脚本/CI 入口，
//!      与 workbuddy 的 `WORKBUDDY_TOKEN` 同一地位，因此
//!      `allows_anonymous_default_session()` 返回 true（实现见下）。
//!
//! ── 响应侧的 model 名回写 ─────────────────────────────────────
//! 上游网关会把响应 chunk 里的 `model` 换成它自己的内部名，而客户端认的是自己
//! 请求时给的名字。源实现（`raccoon-sse-pipe.mjs` 的 `pipeSseWithModelRewrite`）
//! 把下发帧的 `model` 改回客户端请求值 —— 这一段**不能只放在这里**：
//! 帧的改写发生在 SSE 透传流内部，而通用 `ForwardStream` 是协议无关的。
//! 落地方式见 `adapter.rs` 的 `sse_model_rewrite()` 扩展点：由适配器回答
//! 「要不要回写、回写成什么」，通用层只做一次字符串替换。
//!
//! ── 限额错误（classify_error 的判定依据）─────────────────────
//! 小浣熊**没有** workbuddy 那种业务码（6004 / 11128）：它的限额语义完全在
//! 源项目的 `credit-limit.mjs` 里（本地积分预算，本期明确不迁移），而那个模块
//! 的耗尽错误是**它自己抛的 429**（`CreditLimitExhaustedError`，statusCode 429），
//! 不是上游返回的。上游侧能观察到的额度/频率信号只有 HTTP 状态码本身
//! （源实现 `withChatUpstream` 对非 2xx 只做 `response.status` 透传，从错误体里
//! 只取 message 文案，不解析任何业务码）。因此本适配器的判定是：
//!   - 401 → TokenExpired（刷新后同账号重试一次）；
//!   - **429 → QuotaLimited**（按 HTTP 状态码，与 workbuddy 的 429 同档；
//!     上游未在响应体里给结构化恢复时间，reset_at 交给冷却兜底）；
//!   - 其余 → Fatal 原样透传。
//! 这条判定的依据文件与段落写进了交付报告。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   jwt.rs           JWT payload 解码（不验签，只读 exp/name/iss 等声明）
//!   credentials.rs   凭证来源（账号记录 / 桌面端实时登录态）、单飞刷新与回写
//!   models.rs        模型清单（静态兜底 5 个 + `/model_catalog` 远程刷新）
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic：取值走 Option 链与
//! `unwrap_or`，序列化失败一律转成 GatewayError。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::adapter::{ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass};
use super::content_block;
use super::{kind_id, ProviderKind};

pub mod balance;
pub mod credentials;
pub mod jwt;
pub mod models;
pub mod oauth;

/// 默认 LLM 网关地址（源实现 `DEFAULT_LLM_BASE_URL`）
const DEFAULT_LLM_BASE_URL: &str = "https://xiaohuanxiong.com/api/web/llm/v2";

/// 默认鉴权 API 前缀（源实现 `DEFAULT_AUTH_ORIGIN` + `DEFAULT_AUTH_API_PREFIX`）
const DEFAULT_AUTH_ORIGIN: &str = "https://xiaohuanxiong.com";
const DEFAULT_AUTH_API_PREFIX: &str = "/api/web/auth/v1";

/// Raccoon 适配器（无状态单例，见 `adapter::adapter_for`）
pub struct RaccoonAdapter;

/// 进程级实例：适配器无状态，静态实例即可（`adapter_for` 返回它的引用）
pub static RACCOON_ADAPTER: RaccoonAdapter = RaccoonAdapter;

impl ProviderAdapter for RaccoonAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Raccoon
    }

    /// 小浣熊的模型清单（`raccoon::models` 的进程级句柄）
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 构造 `POST {llmBase}/chat/completions`（头集合照抄源实现 `upstreamHeaders`）。
    ///
    /// body **原样透传**：源实现除了 `model` 走一次路由解析（未知模型回退默认，
    /// 本期由网关的模型校验/路由承担）之外不改任何字段。
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
            .to_string();
        if token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "小浣熊账号缺少 accessToken，无法转发",
            ));
        }
        let headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            // 源实现给的是 `Accept: */*`（LLM 网关两种响应形态都可能回）
            ("Accept".to_string(), "*/*".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ];
        Ok(ChatRequestPlan {
            url: format!("{}/chat/completions", self.llm_base_url()),
            headers,
            body: body.clone(),
        })
    }

    /// 上游错误分类（判定依据见模块头）：
    ///   - 401 → TokenExpired
    ///   - 429 → QuotaLimited（按 HTTP 状态码；上游不给结构化恢复时间）
    ///   - 其余 → Fatal
    ///
    /// 文案口径与 workbuddy 一致：`上游返回 {status}: {上游原文}`，
    /// 让客户端的错误展示在多提供商下保持同一种形状（架构文档 §4.2 的
    /// 「message 是客户端可见的最终文案」）。
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
            // 上游文案里没有可解析的恢复时间（小浣熊的限额语义在源项目的
            // credit-limit 模块里，是它自己抛的 429，本期不迁移），
            // reset_at 给 None 让冷却标记落 10 分钟兜底
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        // 内容策略拦截（审核文案）→ ContentBlocked：不罚账号，交给编排层换中性
        // 提示词重试一次 + 触发降级（见 `core::degrade`）
        content_block::classify_or_fatal(
            status,
            error_body,
            message,
            error_body.get("code").and_then(Value::as_i64),
        )
    }

    /// 取可用 access token（临期主动刷新；刷新结果按来源回写）。
    ///
    /// - `account_id` 非空 → 该账号的凭证（桌面端账号实时读 auth.json）
    /// - `account_id` 为空 → 环境变量凭证（若配置）→ 小浣熊组内当前账号 →
    ///   桌面端实时登录态（`credentials::snapshot_for` 的兜底链）
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // 环境变量凭证优先于账号列表（源实现 `resolveCredentials` 的
            // 真实优先级是「账号列表 > 环境变量」，但那条链里的「账号列表」
            // 指的是**用户显式选中的账号**；网关这里的 account_id 来自选路，
            // 非空时说明用户确实有账号可用，此时不该被环境变量顶掉。
            // 因此：仅在**没有指定账号**时才看环境变量）。
            if account_id.is_empty() {
                if let Some(token) = env_access_token() {
                    return Ok(token);
                }
            }
            let credentials = credentials::snapshot_for(store, account_id)?;
            let refreshed = credentials::refresh(store, &credentials, false).await?;
            Ok(refreshed.token)
        })
    }

    /// 401（token 被上游拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 为什么必须覆盖默认实现：`ensure_access_token` 只在「临期」时刷新，而 401
    /// 完全可能发生在一个时间上还很新的 token 上（服务端侧失效、账号被顶下线、
    /// refreshToken 轮换）。此时只调 ensure 会拿回同一个被拒的 token，
    /// 编排层的「刷新后同账号重试一次」就退化成「用同一个坏 token 再打一次」。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if account_id.is_empty() && env_access_token().is_some() {
                // 环境变量凭证是静态的，没有可续期的来源
                return Err(GatewayError::with_status(
                    401,
                    "环境变量凭证（RACCOON_TOKEN）无法续期，请改用账号列表添加账号",
                ));
            }
            let credentials = credentials::snapshot_for(store, account_id)?;
            let refreshed = credentials::refresh(store, &credentials, true).await?;
            Ok(refreshed.token)
        })
    }

    /// 刷新模型目录：`GET {llmBase}/model_catalog`（10 分钟缓存）。
    ///
    /// 用**小浣熊组内的当前账号**的 token（没有账号时用桌面端实时登录态，
    /// 都没有就不带 Authorization 请求 —— 源实现同样允许无 token 拉目录）；
    /// `account_id` 非空 = 用户在「获取模型」弹窗里点名的那条账号
    /// （`snapshot_for` 按 id 直取，取不到报失败而不是回落到队首）。
    ///
    /// `force` 一路透传给 `models::refresh`：`false` 时走 10 分钟 TTL 早退
    /// （自动路径，见 `ProviderAdapter::refresh_models`），`true` 时真打上游
    /// （用户手动点了「刷新模型清单」）。TTL 的判定与理由都在 `models.rs` 里，
    /// 本函数只负责取凭证并把参数带下去 —— 判断权在调用方，不在这一层。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            let (token, proxy) = match credentials::snapshot_for(store, account_id) {
                Ok(credentials) => {
                    // 目录刷新**不触发 token 刷新**：它只是维护动作，
                    // 让一个临期 token 在这里被续期会把日志搅乱（转发链路上
                    // 该续期的地方自然会续）。因此这里只用当前 token。
                    let proxy = proxy_of(store);
                    (credentials.token, proxy)
                }
                Err(error) => {
                    crate::server::logging::verbose(
                        "[Models]",
                        &format!("小浣熊模型目录刷新：{}", error.message),
                    );
                    (String::new(), None)
                }
            };
            // 返回值的三档（刷到了 / TTL 跳过 / 拉了但失败）由 `models::refresh`
            // 如实给出 —— 本函数不替它做二次判定，否则「没带 token 所以拉到了
            // 公开目录」这类正常情形会被误报
            models::refresh(&token, proxy.as_ref(), force).await
        })
    }

    /// 小浣熊有远程目录（`GET {llmBase}/model_catalog`），支持刷新模型清单。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 小浣熊有环境变量凭证旁路（`RACCOON_TOKEN`，源实现 `envCredentials`），
    /// 因此账号列表为空时仍可用默认登录态转发（脚本 / CI 用户的常规用法）。
    fn allows_anonymous_default_session(&self) -> bool {
        true
    }

    /// 环境变量旁路凭证此刻是否存在（聚合目录判「这家现在有没有可用登录态」用）。
    fn env_credentials_present(&self) -> bool {
        env_access_token().is_some()
    }

    /// 小浣熊**没有**「默认模型」概念：模型 id 就是上游的模型名，客户端不指定
    /// 时应该由上游用它自己的默认（架构文档 §4.4 末句的「不注入」分支）。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// SSE 帧的 model 名回写：把上游下发的 `model` 改成客户端请求的名字
    /// （源实现 `pipeSseWithModelRewrite` 的 `parsed.model = requestedModel`）。
    ///
    /// 为什么由适配器回答：这是**上游网关的行为特征**（小浣熊网关会回自己的
    /// 内部名），而帧改写发生在通用 SSE 流里，通用层不该知道是哪一家要求它。
    fn sse_model_rewrite(&self) -> bool {
        true
    }

    /// 小浣熊支持主动刷新（`POST {authBase}/refresh`，见 `credentials.rs`）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 临期判定：取该账号的凭证快照，用凭证自己的 `is_expiring()`（5 分钟窗口）
    /// 与 `can_refresh()` 判一次。
    ///
    /// ── 为什么直接复用凭证类型上的两个方法 ─────────────────────
    /// 它们是转发链路懒刷新（`credentials::refresh(force = false)`）用的同一对
    /// 判据，维护任务再写一遍就等于把「什么算临期」的规则复制出第二份。
    ///
    /// ── `can_refresh()` 在这里排掉的是什么 ─────────────────────
    /// 「快照里没有 refreshToken」的账号：环境变量旁路与「只有 token 的旧记录」
    /// 属于这一类，把它们算进待刷新集合只会每轮稳定失败一次。注意判据是
    /// **快照里有没有 refreshToken**，不是「账号是不是桌面端」——桌面端账号的
    /// 凭证实时读 auth.json，那份文件里通常带 refresh_token（源实现的刷新回写
    /// 目标就是它），因此它会被正常纳入并走比较-再写回写（见 `credentials.rs`
    /// 模块头）；只有 auth.json 里确实没有 refreshToken 时才返回 false。
    ///
    /// 取快照失败（凭证为空）返回 false（见 trait 契约）。
    ///
    /// ── 为什么先确认记录存在 ──────────────────────────────────
    /// `snapshot_for` 在**没有账号记录**时会回落到桌面端实时登录态 ——
    /// 那是它作为「取凭证」入口的合理兜底，但对本判定有害：一个已被删除的
    /// 账号 id 会拿到别人的凭证并据此回答「需要刷新」，维护任务随后就会拿
    /// 这个不存在的 id 去刷新。因此这里先确认记录属于小浣熊，再谈临期。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.is_empty() || store.raccoon_account_record(account_id).is_none() {
            return false;
        }
        match credentials::snapshot_for(store, account_id) {
            Ok(credentials) => credentials.can_refresh() && credentials.is_expiring(),
            Err(_) => false,
        }
    }

    /// 小浣熊支持网页登录（trait 扩展 7）：官方登录页在登录成功后跳转
    /// `office-raccoon://auth/callback?code=…`，网关用那个一次性 code 换凭证。
    ///
    /// 四家里只有它支持：workbuddy 的「网页登录」是上游 `auth/state` 那套无头流程
    /// （`core::login`，state 由上游发），CatPaw / AutoClaw 没有这条协议
    /// （AutoClaw 的凭证只能从本机 auth.json 解密读出）。三家都保持默认 false。
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 授权地址 + state（`raccoon::oauth`，移植源实现 `buildRaccoonAuthorizeUrl`）。
    ///
    /// state 的生成方式（`oauth::new_login_state` ← `new_request_id`）见 `oauth.rs`
    /// 模块头：项目里没有 `rand`/`uuid` 依赖，workbuddy 的 state 又来自上游，
    /// 所以复用既有的同一手法，为一个 state 引进依赖不合算。
    fn build_login_url(&self) -> Option<(String, String)> {
        let state = oauth::new_login_state();
        Some((oauth::build_authorize_url(&state), state))
    }

    /// 用回调里的一次性 code 换凭证并落账号（`raccoon::oauth::exchange_code`）。
    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { oauth::exchange_code(store, code, state).await })
    }

    /// 小浣熊有余额 / 积分概念：官方积分钱包 + 订阅权益（`raccoon/balance.rs`）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查余额 / 积分（移植源实现 `account-balance.mjs` 的 `queryPoints` +
    /// `querySubscription`，合成一次调用）。
    ///
    /// ── 为什么凭证是 `snapshot_for` 而不是 `ensure_access_token` ───
    /// `snapshot_for` 返回的是**完整凭证**（token + refreshToken + 来源），
    /// 而余额查询只要 token；更重要的是它不会在临期时触发刷新 ——
    /// 余额查询是只读的展示动作，为了它消耗一次 refreshToken 轮换不划算
    /// （过期了就让上游回 401，调用方再走刷新重试那条既有链路）。
    /// 刷新重试由 `api::accounts::query_usage_inner` 统一处置（它调
    /// `refresh_access_token`，那是 force 语义）。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { balance::query_usage(store, account_id).await })
    }
}

impl RaccoonAdapter {
    /// LLM 网关地址（`RACCOON_LLM_BASE_URL` 可覆盖，源实现同名环境变量）
    fn llm_base_url(&self) -> String {
        llm_base_url()
    }
}

/// LLM 网关地址（env 可覆盖；末尾斜杠去掉，源实现 `normalizeBaseUrl` 的同效处理）
pub(super) fn llm_base_url() -> String {
    base_url_from_env("RACCOON_LLM_BASE_URL", DEFAULT_LLM_BASE_URL)
}

/// 鉴权 API 基址（env 可覆盖；默认 `{origin}{prefix}`）
pub(super) fn auth_api_base() -> String {
    let fallback = format!("{DEFAULT_AUTH_ORIGIN}{DEFAULT_AUTH_API_PREFIX}");
    base_url_from_env("RACCOON_AUTH_API_BASE", &fallback)
}

/// 读一个「基址」环境变量：去空白、去末尾斜杠；为空则用默认值。
///
/// 源实现还做协议校验（只允许 https 或本机 http）并在非法时**抛错**；
/// 这里不抛错（release 是 panic=abort，且这是一个纯配置项）：非法值原样使用，
/// 请求失败时由传输层报出可读错误 —— 那种错误比启动期一条配置报错更容易发现。
fn base_url_from_env(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// 环境变量里的 access token（`RACCOON_TOKEN`，空白串视为未设置）
fn env_access_token() -> Option<String> {
    std::env::var("RACCOON_TOKEN")
        .ok()
        .map(|value| jwt::strip_bearer(&value))
        .filter(|value| !value.is_empty())
}

/// 小浣熊账号的出口（目录刷新用它，与转发链路一致）。
///
/// 账号级代理字段对**所有** provider 生效（架构文档 §2），但小浣熊的目录刷新
/// 是低频维护动作，这里只取「当前账号记录里配的 proxy」；解析失败回退直连
/// （`describe_account_proxy` 的 egress 逻辑在转发链路上另有处理，这里不重复）。
fn proxy_of(store: &AccountStore) -> Option<crate::server::core::proxies::ResolvedProxy> {
    let entry = store.current_entry_for_provider(kind_id(ProviderKind::Raccoon))?;
    crate::server::core::proxies::session_proxy(&entry.session)
}
