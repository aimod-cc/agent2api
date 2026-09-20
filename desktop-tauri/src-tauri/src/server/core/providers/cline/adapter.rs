//! Cline 适配器：把凭证层接进 provider 注册表（无状态，OpenAI 兼容）。
//!
//! ── 上游协议（全部实测核对，2026-09）─────────────────────────
//!   - LLM：`POST {apiBase}/chat/completions`，base 是
//!     `https://api.cline.bot/api/v1`（完整路径 `/api/v1/chat/completions`）。
//!     上游用 AI SDK 的 `createOpenAICompatible`，把 baseURL 当**根**拼 ——
//!     所以路径里**只有一个 `v1`**。**发错路径的表现是 404 `{"error":"Not Found"}`**：
//!     实测 `/api/v1/v1/chat/completions`（照 base 含 v1 直觉拼的）就是 404。
//!   - 认证：`Authorization: Bearer <token>`，token **含 `workos:` 前缀**
//!     （见 `credentials.rs`）。
//!   - **产品面校验头 `X-CLIENT-TYPE: cline-sdk`（本集成的关键）**：
//!     不带这个头时，**免费池模型一律 403**
//!     （`only available via Cline product surfaces`）；订阅池模型则报
//!     `ENTITLEMENT_ERROR`（那是账号确实没订阅，与头无关）。
//!     实测：带上 `X-CLIENT-TYPE: cline-sdk` 后免费池返回 200。
//!     `User-Agent: Cline/...` **不是**必需（实测只带 X-CLIENT-TYPE 就通过），
//!     但一并带上更贴近官方客户端（上游的行为可能随版本收紧，带上没有代价）。
//!   - **响应形态的不对称（必须记住）**：
//!       - `stream: true` → **裸 `chat.completion.chunk` 帧**（与 OpenAI 逐字同形），
//!         可以直接透传，**不需要解包**；
//!       - 非流式 → **信封** `{"data":{...},"success":true}`，内层才是
//!         `chat.completion`。
//!     网关**总是以 `stream: true` 请求上游**（`upstream::mod` 的既有编排），
//!     因此转发链路上只走前者；信封形态只影响「我们自己打非流式调试」的场景。
//!     `balance.rs` 那几个管理接口同样要解信封（见那里）。
//!   - SSE 帧里的 `model` 是**上游内部名**（实测：请求
//!     `cline-free/deepseek-v4.1-flash`，回帧的 model 是 `deepseek/deepseek-v4.1-flash`
//!     —— 它背后走 OpenRouter 那类聚合，回的是真实承载模型的 slug）。
//!     因此 `sse_model_rewrite()` 必须为 **true**，否则客户端会看到
//!     `deepseek/deepseek-v4.1-flash` 这种它没请求过的名字。
//!
//! ── 错误分类（实测的三种错误体）──────────────────────────────
//!   - 401 `{"error":"Unauthorized: Please make sure you're using the latest
//!     version of Cline and re-authenticate your Cline account."}`
//!     → `TokenExpired`（刷新后同账号重试一次）；
//!   - 403 `{"error":{"code":"ENTITLEMENT_ERROR","message":"...not subscribed
//!     to required model plan"}}` 或 `{"error":{"code":"API_REQUEST_ERROR_CODE",
//!     "message":"... only available via Cline product surfaces"}}`
//!     → **`Fatal`（不是 QuotaLimited！）** —— 这两个都是「这个账号对**这个模型**
//!     没有权限」，换账号重试同样会失败（除非另一个账号有订阅），而把它当
//!     限额冷却会让账号被无意义地冷却一整个窗口。**403 一律不重试**是这里
//!     刻意的判断，理由见 `classify_error`；
//!   - 404 `{"error":"model not found"}` → `Fatal`（模型名不对，换账号无用）；
//!   - 429 → `QuotaLimited`（上游未给结构化恢复时间，`reset_at` 为 None）。
//!
//! ── 500 `empty response content` 不是错误分类问题 ────────────
//! 实测：`max_tokens` 给小了（如 10）而模型要先输出一大段 reasoning 时，
//! 上游回 500 `{"error":"empty response content","success":false}`。
//! 这**不是**本适配器要处理的（我们不限制客户端的 max_tokens），记在这里
//! 是为了避免后来者把它误判成「上游不稳定」。
//!
//! ── 与另外几家的一致性 ──────────────────────────────────────
//!   - **不做 system 注入**：上游是 OpenAI 兼容网关，源实现从不改消息序列；
//!   - **不认「默认模型」**（`supports_default_model` = false）：客户端的
//!     `defaultModel` 是 workbuddy 语义的配置项，Cline 的默认是它自己目录里的
//!     第一个推荐模型 —— 注入一个 workbuddy 模型名再路由到 Cline 只会 404；
//!   - **没有环境变量旁路**（`CLINE_TOKEN` 这类约定不存在），因此
//!     `allows_anonymous_default_session` 保持默认 false；
//!   - **`supports_model_refresh` = true**：上游确实有目录接口
//!     （`GET {apiBase}/ai/cline/recommended-models`，实测**无需鉴权**即可拉）。
//!
//! ── 超时 ────────────────────────────────────────────────────
//! 与另外四家共用 `core::egress` 的出网点（connect 30s + read 600s，
//! **不设总超时**）—— LLM 是长回答场景，与 autoclaw 同一取舍。
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

use super::credentials;
use super::models::{self, Pool};
use super::refresh;

/// 产品面标识头（**本集成的关键**，见模块头）。
///
/// 上游按这个头判定「调用方是不是 Cline 自家产品」，缺了它免费池模型一律 403。
/// 取 `cline-sdk` 而不是 `cline-cli`：实测 `cline-cli` 会让上游走另一条
/// 兼容路径（回 500 `empty response content`），`cline-sdk` 是干净通过的那个。
pub const CLIENT_TYPE: &str = "cline-sdk";

/// 客户端版本（进 User-Agent；实测非必需，但更贴近官方客户端形态）
const CLIENT_VERSION: &str = "3.0.62";

/// 客户端版本上报头（上游用它判定版本过旧）
const CLIENT_VERSION_HEADER: &str = "X-CLIENT-VERSION";

/// 桌面端版本（上游用它判定版本过旧）
const CLIENT_VERSION_CODE: &str = "3.0.62";

/// Cline 适配器：**按池参数化**（同一套实现，两个实例）。
///
/// ── 为什么是两个实例而不是两个 impl ──────────────────────────
/// `cline-free` 与 `cline-pass` 是同一上游、同一协议、同一凭证格式，差别只有
/// 「收哪个池的模型」。把整份实现复制一份只为了换一个过滤条件，正是要在
/// 这里避免的事：凭证续期、错误分类、SSE 回写、余额查询任何一处改动都要记得
/// 改两遍，漏一次就是两个 provider 行为不一致。
///
/// 因此实现共用，实例由池决定（[`CLINE_FREE_ADAPTER`] / [`CLINE_PASS_ADAPTER`]，
/// `adapter::adapter_for` 按 kind 给出对应那一个）。
pub struct ClineAdapter {
    /// 这个实例服务哪个池（= 哪家 provider，见 `Pool::kind`）
    pool: Pool,
}

/// 免费池实例（provider id `cline-free`）
pub static CLINE_FREE_ADAPTER: ClineAdapter = ClineAdapter { pool: Pool::Free };

/// 订阅池实例（provider id `cline-pass`）
pub static CLINE_PASS_ADAPTER: ClineAdapter = ClineAdapter { pool: Pool::Pass };

impl ProviderAdapter for ClineAdapter {
    fn kind(&self) -> ProviderKind {
        self.pool.kind()
    }

    /// Cline 的模型清单（**本池那一批**，见 `models::list`）。
    ///
    /// 池是身份而不是账号属性，所以这里已经是收窄后的结果，不需要
    /// `advertise_models` 再收一层（那个方法的默认实现就是原样透传）。
    fn list_models(&self) -> Vec<Value> {
        models::list(self.pool)
    }

    /// 构造 `POST {apiBase}/chat/completions`。
    ///
    /// body **透传**（不改任何字段）：上游是 OpenAI 兼容的，模型名、tools、
    /// stream 全部原样 —— 与小浣熊/AutoClaw 同档。模型名**含池前缀**
    /// （`cline-free/...`），那是上游的通道选择器，绝不能剥（见 `models.rs`）。
    ///
    /// `account` 是**会话形态**（`store.get_session_by_id` /
    /// `auth.get_current_session` 的返回值）：token 从 `auth.accessToken` 取，
    /// 与另外五家同一约定。桌面端账号（实时登录态）的 token 由账号存储的
    /// `session_from_record` 在构造会话时实时读入，因此这里不需要知道账号来源。
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
            .trim();
        if token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "Cline 账号缺少 accessToken，无法转发（请重新登录或导入桌面端登录态）",
            ));
        }
        let headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", credentials::ensure_token_prefix(token)),
            ),
            // 产品面标识（见模块头：缺了它免费池一律 403）
            ("X-CLIENT-TYPE".to_string(), CLIENT_TYPE.to_string()),
            ("User-Agent".to_string(), format!("Cline/{CLIENT_VERSION}")),
            (
                CLIENT_VERSION_HEADER.to_string(),
                CLIENT_VERSION_CODE.to_string(),
            ),
            // 官方客户端会带这两个（OpenRouter 那套来源标记），带上更贴近官方面
            ("HTTP-Referer".to_string(), "https://cline.bot".to_string()),
            ("X-Title".to_string(), "Cline".to_string()),
        ];
        Ok(ChatRequestPlan {
            url: format!("{}/chat/completions", credentials::API_BASE_URL),
            headers,
            body: body.clone(),
        })
    }

    /// 上游错误分类（判定依据全部来自实测，见模块头）。
    ///
    /// ── 403 为什么不重试（本适配器最需要说清的一个判断）─────────
    /// Cline 的 403 有两种业务码，**都表示「这个账号对这个模型没有权限」**：
    ///   - `ENTITLEMENT_ERROR`：账号没订阅该模型所属的套餐（如 pass 池模型）；
    ///   - `API_REQUEST_ERROR_CODE`：模型只对官方产品面开放（免费池）；
    /// 二者都是**账号 × 模型**维度的确定性拒绝，与「用太猛被限流」是两件事：
    ///   - 当作 `QuotaLimited` 会把这个账号冷却一整个窗口 —— 但换一个账号重试
    ///     同样会 403（除非那个账号恰好有订阅），冷却只是把「确定失败」变成
    ///     「一段时间内静默跳过」，用户更难看出真正原因；
    ///   - 当作 `Fatal` 则原样把 403 文案透给客户端，用户立刻看到
    ///     「not subscribed to required model plan」，知道该去订阅或换个模型。
    /// 因此这里归 `Fatal`。**注意这与 429 不同**：429 是真限流，归 `QuotaLimited`
    /// （换账号有意义）。
    ///
    /// ── `upstream_code` 取什么 ──────────────────────────────────
    /// 上游错误体是 `{"error":{"code":"ENTITLEMENT_ERROR","message":"..."}}`
    /// 的**嵌套**形态（code 是字符串，不是数字），而本项目的
    /// `UpstreamErrorClass` 的 `upstream_code` 是 `Option<i64>`（为 workbuddy 的
    /// 数字业务码设计的）。字符串码放不进去，硬编成数字只会是编造的假码 ——
    /// 因此一律 `None`，把可读的字符串码留在 `message` 里（`ErrorCode` 那层
    /// 文案本来就会带出上游原文）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = upstream_message(error_body);
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                // 上游不给结构化的恢复时间（实测错误体里没有任何时间字段）
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: None,
        }
    }

    /// 取可用 access token：**凭证快照 + 临期主动刷新**（10 分钟窗口，
    /// 见 `credentials::PROACTIVE_REFRESH_MARGIN_MS`）。
    ///
    /// 刷新结果回写 accounts.json（桌面端来源按设计不回写，见 `refresh::persist_refresh`）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let credentials = refresh::ensure_fresh(store, account_id, false).await?;
            Ok(credentials.bearer_token())
        })
    }

    /// 401（token 被上游拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 必须覆盖默认实现：`ensure_access_token` 只在临期时刷新，而 401 完全可能
    /// 发生在一个时间上还很新的 token 上（服务端侧失效、会话被顶下线）。
    /// 此时只调 ensure 会拿回同一个被拒的 token，「刷新后重试一次」就退化成
    /// 「用同一个坏 token 再打一次」。
    ///
    /// 没 refreshToken 时 `ensure_fresh(force=true)` 会返回 400 —— 那是对的：
    /// 编排层据此把「无法续期」如实告诉客户端，而不是静默重试。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let credentials = refresh::ensure_fresh(store, account_id, true).await?;
            Ok(credentials.bearer_token())
        })
    }

    /// 拉取远程模型目录（`GET {apiBase}/ai/cline/recommended-models`）。
    ///
    /// ── 无鉴权也能拉（实测）─────────────────────────────────────
    /// 因此这里**不依赖账号状态**：一个账号都没有时也能刷新目录（与另外几家
    /// 「没登录态就跳过」的取向不同 —— 那几家的目录接口要凭证）。这不违反
    /// 「刷新失败不返回错误」的约定，只是少了一个失败原因。
    ///
    /// `force` 参数的语义与另外几家一致（`true` = 用户手动点了刷新），
    /// 但本家**没有 TTL 早退**：上游接口轻量且无鉴权，缓存的价值主要是
    /// 「避免每次 /v1/models 都打一次上游」，那由更上层的调用频率控制，
    /// 不需要在这里再压一层时间窗（多一个旋钮就多一处不一致）。
    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            match models::refresh().await {
                Ok(count) => {
                    // 清单变了就补种默认映射（新增的带前缀模型自动获得友好名）。
                    // **只种本池那一批**：远程接口一次拉回两个池，但种子按
                    // (provider, id) 记账，越界种到另一个池的 id 会让那家的
                    // seeded 里出现不属于它的键。
                    if let Some(summary) = seed_defaults(self.pool) {
                        logging::log("[Models]", &summary);
                    }
                    ModelRefreshOutcome::refreshed(count)
                }
                Err(reason) => {
                    logging::verbose(
                        "[Models]",
                        &format!("Cline 模型目录刷新失败：{reason}"),
                    );
                    ModelRefreshOutcome::failed(format!("Cline 目录刷新失败：{reason}"))
                }
            }
        })
    }

    /// Cline **有**远程模型目录（`GET {apiBase}/ai/cline/recommended-models`，实测）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// Cline **没有**「默认模型」概念：不注入 workbuddy 语义的默认模型名
    /// （理由见模块头）。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// SSE 帧的 model 名回写：**要写**。
    ///
    /// 实测：请求 `cline-free/deepseek-v4.1-flash`，回帧的 model 是
    /// `deepseek/deepseek-v4.1-flash`（上游背后走聚合通道，回的是真实承载
    /// 模型的 slug）。客户端按自己请求的名字识别响应，不回写会让它看到
    /// 一个它从没请求过的模型名。
    ///
    /// ── 关于思考帧的字段名（**本家不回写、也不改写，这是对的**）────
    /// Cline 的思考增量用的是 OpenRouter 那套 **`delta.reasoning`**
    /// （外加一个 `reasoning_details` 数组），而本项目 SSE 合并器认的是
    /// **`reasoning_content`**（workbuddy / 小浣熊的形态）。
    ///
    /// 两者对不上**不影响透传**：合并器的判据是「有 `reasoning_content`
    /// 且没有 content/tool_calls」，Cline 的帧不命中，于是走「其余事件 →
    /// 原样透传」那一支 —— 客户端收到的是 Cline 自己的 `reasoning` 字段，
    /// 逐字节未改。这正好是我们要的：**不替客户端翻译上游的字段名**，
    /// 那属于客户端 SDK 的事，网关擅自改名反而会让它认不出。
    /// 代价只是少了「把碎思考帧攒成一块」这一个优化（Cline 的思考帧本来就
    /// 比 workbuddy 粗），不影响正确性。
    fn sse_model_rewrite(&self) -> bool {
        true
    }

    /// Cline 支持主动刷新（`POST {apiBase}/auth/refresh`，实测）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 临期判定：取凭证快照，用凭证自己的 `is_expiring()`（10 分钟窗口）与
    /// `can_refresh()` 判一次。
    ///
    /// **`can_refresh()` 是必需的**：用户手填 access token 而不给 refreshToken
    /// 时，凭证天然不可刷新 —— 少这道判定会被维护任务每轮都算成「待刷新」，
    /// 然后稳定失败并往日志里灌错误（与 AutoClaw 的 `openclaw.json` 来源同一
    /// 取舍）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.is_empty() {
            return false;
        }
        match refresh::snapshot(store, account_id) {
            Ok(credentials) => credentials.can_refresh() && credentials.is_expiring(),
            Err(_) => false,
        }
    }

    /// Cline 有 credit 余额与订阅概念（`cline/balance.rs`）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查余额 + 用量（`GET /users/{id}/balance` + `/users/me/plan`）。
    ///
    /// ── 为什么不在这里触发刷新 ──────────────────────────────────
    /// 与另外几家同一分工：适配器只提供「怎么查」，401 之后的刷新重试由
    /// `api::accounts::query_usage_inner` 统一处置（它调 `refresh_access_token`）。
    /// 本家 `supports_refresh = true`，因此那条链路对它是有效的。
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

/// 上游错误体 → 可读文案（本项目的归一化只保证 `message` 键，但 Cline 的
/// 两种形态是 `{"error":"..."}` 与 `{"error":{"code":..,"message":..}}`）。
///
/// `error_body` 是**已归一化**的错误对象（见 trait 契约：至少含 `code` 与
/// `message`）。归一化层取的是 `message` / `msg` / `error.message`，
/// 因此嵌套形态的 message 已经在 `message` 里了；纯字符串形态
/// （`{"error":"Unauthorized: ..."}`）取不到 `error.message`，归一化层会把
/// 整个 JSON 的截断原文放进去 —— 这里做最后一层适配：优先 `message`，
/// 空则回落到 `error` 的字符串形态。
fn upstream_message(error_body: &Value) -> String {
    if let Some(text) = error_body
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    if let Some(text) = error_body
        .get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    "上游错误".to_string()
}

/// 对本池清单补种默认映射（刷新落地后调用）。
///
/// 用当前清单（含静态兜底）而不是只传远程结果：静态兜底里也有带前缀的模型，
/// 一次种完更省事，且种子本身幂等（种过的跳过）。
///
/// ── 为什么不需要「按用户所在的池排序」了（拆分后删掉的一段）────
/// 早先两池共用一个 provider，同名模型（`deepseek-v4.1-flash` 两池都有）的
/// 短名归属由**账号里有哪个池**决定 —— 只加了免费池账号的用户必须拿到
/// 免费池那条，否则短名会稳定 403（`ENTITLEMENT_ERROR`）。那套排序连同
/// 「池是账号属性」这个前提一起退场了。
///
/// 现在两家各记各的 seeded，先刷到的池拿到「去前缀」那条；两家的短名都由
/// `model_rules::EXTRA_ALIASES` 点名补挂，因此**无论哪家先刷，短名都能路由到
/// 两个池**（各有一条映射进候选链，发送名跟着实际承载的 provider 走，见
/// `catalog::wire_model_for_provider` 的 ②）。短名最终落在谁家只影响
/// 「候选链里哪个在前」，而那由账号优先级与候选链的既有规则决定，不再是
/// 一个藏在种子里、随刷新顺序漂移的隐规则。
pub(crate) fn seed_defaults(pool: Pool) -> Option<String> {
    models::seed_cline_defaults(pool, &models::ids_of(pool))
}
