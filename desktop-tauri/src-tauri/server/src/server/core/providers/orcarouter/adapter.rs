//! OrcaRouter 的 `ProviderAdapter` 实现：Bearer 透传 + 目录刷新 + 终端 401。
//!
//! ── 上游协议（全部实测核对，2026-10-02）────────────────────────
//!   - 推理：`POST {api}/chat/completions`（默认
//!     `https://api.orcarouter.ai/v1/chat/completions`），body **逐字透传**
//!     （OpenAI 兼容，模型名 / tools / stream 全部原样 —— 与 Cline / 小浣熊同档）。
//!   - 认证：`Authorization: Bearer sk-orca-…`。
//!   - 模型名**保留 `vendor/model` 命名空间**（`deepseek/deepseek-v4-pro`）：
//!     上游按它路由与计费，剥前缀会打到别的模型上。
//!   - SSE 帧的 `model` 是**上游内部名**（实测请求 `deepseek/deepseek-v4-pro`，
//!     回帧的 `model` 是 `deepseek-v4-pro-ga-260813`）→ `sse_model_rewrite()`
//!     必须为 **true**，否则客户端会看到一个它从没请求过的模型名。
//!   - 思考增量：上游回的是 `message.reasoning_content`（实测），与本项目
//!     SSE 合并器认的字段**同名**，因此思考帧能正常攒块（不像 Cline 的
//!     `reasoning`），这里不需要任何改写。
//!
//! ── 错误分类（对齐 `UpstreamErrorClass` 的四档语义）─────────────
//!   - `401` → [`UpstreamErrorClass::TokenExpired`]，**但这一家的处置与别家不同**：
//!     OrcaRouter 没有 refresh grant，`refresh_access_token` 会返回「无法续期」
//!     并在这之前把**这一条账号的这一代凭证**标记成 `needsReauth`。
//!     规范要求「被吊销的长期 Key 进 needsReauth，不伪造 refresh」正是这条链。
//!   - `403` → `Fatal`（原样透出）：实测错误体是
//!     `{"error":{"code":"model_access_denied","message":"This API key does not
//!     have access to model …","metadata":{"reason":"block_key_scope"}}}` ——
//!     那是**账号 × 模型**维度的确定性拒绝（Key 的可见范围），换账号重试常常
//!     有效但换个模型立刻可用，因此不作限额冷却（与 Cline 的 403 同一取舍）。
//!   - `429` → `QuotaLimited`（换账号 / 稍后重试有意义）。
//!   - 其余 → 内容策略判定（`content_block::classify_or_fatal`）+ `Fatal`。
//!
//! ── 不注入任何头 ────────────────────────────────────────────
//! 上游**不读** OpenRouter 那套 `HTTP-Referer` / `X-Title` 的来源标记，也没有
//! 产品面校验头（与 Cline 不同）。多余的头只会让「发出去的请求跟别人的不一样」
//! 成为一处需要解释的差异，因此这里只发三样：`Authorization`、
//! `Content-Type`、`Accept`。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! 绝不 unwrap/expect/panic；不持锁穿越 await；Key 不进日志与错误。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use crate::server::core::providers::{content_block, ProviderKind};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::catalog::{self, ModelKind, Modality, Source};
use super::credentials::Credentials;
use super::login;
use super::{endpoints, reauth_hint, SOURCE_MANUAL, SOURCE_PKCE};

/// OrcaRouter 适配器（无状态：一次 HTTP 请求 = 一次对话）。
pub struct OrcaRouterAdapter;

/// 注册表用的静态实例（`adapter_for` 返回它）。
pub static ORCAROUTER_ADAPTER: OrcaRouterAdapter = OrcaRouterAdapter;

/// 客户端版本上报头。实测非必需；带上它让上游的日志里能看出调用方是谁
/// （不影响鉴权与计费）。这不是「产品面校验头」——OrcaRouter 没有那种东西。
const CLIENT_USER_AGENT: &str = concat!("Agent2API/", env!("CARGO_PKG_VERSION"));

impl ProviderAdapter for OrcaRouterAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OrcaRouter
    }

    /// 模型清单：**当前快照**（live 权威结果，或失败时的已验证种子）。
    ///
    /// 与 `/v1/models` 的出口同源：`catalog::snapshot()` 是目录读侧的唯一入口，
    /// 于是「界面里看得到什么」与「入口校验放行什么」永远一致。
    fn list_models(&self) -> Vec<Value> {
        catalog::snapshot().models
    }

    /// 构造 `POST {api}/chat/completions`。
    ///
    /// body 透传；Key 从会话形态的 `auth.accessToken` 取（与其余各家同一约定，
    /// 于是**凭据来源对这里完全不可见** —— 手填与 PKCE 走的是同一条路）。
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
                "OrcaRouter 账号缺少 API Key，无法转发（请重新连接或粘贴一把新 Key）",
            ));
        }
        let endpoints = endpoints();
        let headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {token}"),
            ),
            ("User-Agent".to_string(), CLIENT_USER_AGENT.to_string()),
        ];
        Ok(ChatRequestPlan::chat(
            endpoints.chat_endpoint(),
            headers,
            body.clone(),
        ))
    }

    /// 上游错误分类（判据全部来自实测，见模块头）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = upstream_message(error_body);
        if status == 401 {
            // 文案里带上「怎么修」，因为编排层会把它透给客户端
            return UpstreamErrorClass::TokenExpired {
                message: format!("上游返回 401：{raw}。{}", reauth_hint()),
            };
        }
        let message = format!("上游返回 {status}: {raw}");
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                // 上游的 429 不带结构化恢复时间（实测错误体里没有时间字段）
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        // 403（model_access_denied）与 404（模型名不对）都是确定性拒绝：
        // 原样透出，让用户看到「这把 Key 没有这个模型的权限」/「模型不存在」，
        // 而不是把它冷却成一段静默跳过。
        content_block::classify_or_fatal(status, error_body, message, None)
    }

    /// 取可用 Key（这一家**没有**临期概念：Key 不会过期，只会被吊销）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match store.orcarouter_api_key(account_id) {
                Ok(Some(credential)) => Ok(credential.api_key),
                Ok(None) => Err(GatewayError::with_status(
                    401,
                    "OrcaRouter 账号缺少可用的 API Key，请重新连接或粘贴一把新 Key",
                )),
                Err(reason) => Err(GatewayError::with_status(400, reason)),
            }
        })
    }

    /// 401 后的**终止性**处置：这一家没有 refresh grant，**不伪造刷新**。
    ///
    /// ── 这个函数**同时**是「精确标记被拒账号」的落点 ─────────────
    /// 编排层（`core::upstream::provider_loop`）只在收到上游 401、并且
    /// `supports_refresh()` 与调用链允许时才走到这里；而本家的
    /// `supports_refresh()` 恒 false，所以**唯一**会调用它的是 401 那条路
    /// （见 `core::usage_query::query_usage_inner` 与 `provider_loop` 的
    /// 「动作 2」）。因此这里做标记是准确的：调用 = 这次请求用的那把 Key
    /// 被上游拒绝了。
    ///
    /// 标记按（账号 id + 凭证指纹）落（见
    /// [`AccountStore::mark_orcarouter_needs_reauth`]）：账号 id 由 Key 派生，
    /// 因此**旧 401 永远污染不到新凭据**（换新 Key = 一条新记录）。
    /// 新登录成功时 `add_orcarouter_account` 会清掉旧记录上的标记。
    ///
    /// 返回 401 而不是尝试任何续期：用户会看到一条带「怎么修」的文案
    /// （`reauth_hint`），并且**不会**有人悄悄删掉旧密钥 ——
    /// 规范要求「新登录成功前不要静默删除旧 secret」。
    /// 这与 Cline / 小浣熊那几家「401 → 刷新后同账号重试一次」的链路刻意不同：
    /// 对 OrcaRouter 而言「刷新」这件事根本不存在，假装做一次只会浪费一次上游
    /// 往返并让用户误以为凭据可以自愈。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match store.mark_orcarouter_needs_reauth(account_id) {
                Ok(true) => logging::log(
                    "[Upstream]",
                    &format!("⚠️ OrcaRouter 账号 {account_id} 的密钥被上游拒绝（401），已标记为需要重新授权"),
                ),
                // 置位是**尽力而为**的旁路：标记不上（记录已被删、id 为空、
                // 库暂时写不进去）不该改变这条请求的结局 —— 用户仍需看到 401
                Ok(false) => logging::verbose(
                    "[Upstream]",
                    &format!("OrcaRouter 账号 {account_id} 无记录可标记（可能已被删除）"),
                ),
                Err(error) => logging::verbose(
                    "[Upstream]",
                    &format!("OrcaRouter 账号 {account_id} 的重新授权标记写入失败：{}", error.message),
                ),
            }
            Err(GatewayError::with_status(
                401,
                format!(
                    "OrcaRouter 的凭据是长期 API Key，没有刷新机制，无法通过续期恢复。{}",
                    reauth_hint()
                ),
            ))
        })
    }

    /// 本家**没有**主动刷新（`supports_refresh` 默认 false 已表达这一点，
    /// 这里显式覆写是为了让「为什么没有」在代码里可见）。
    fn supports_refresh(&self) -> bool {
        false
    }

    /// 凭据永不临期（Key 不过期，只会被吊销）。恒 false = 维护任务不会对它
    /// 做无意义的续期尝试。
    fn credentials_expiring(&self, _store: &AccountStore, _account_id: &str) -> bool {
        false
    }

    /// 拉取远程模型目录（`GET {api}/models?capability=chat`，**用账号的 Key**）。
    ///
    /// `force` 与 `account_id` 的语义与其余各家一致（见 trait 契约）：
    ///   - `force = true`（用户手动点刷新）绕过 TTL 与失败冷却；
    ///   - `account_id` 非空 = 用户点名的那条账号（目录是账号级的，不同 Key
    ///     看到的模型集合不同），空 = 该家队首账号。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            let outcome = catalog::refresh(store, account_id, force).await;
            match outcome.error {
                None => {
                    if force {
                        logging::log(
                            "[Models]",
                            &format!("OrcaRouter 模型目录已刷新（{} 条）", outcome.count),
                        );
                    }
                    ModelRefreshOutcome::refreshed(outcome.count)
                }
                Some(reason) => {
                    logging::verbose("[Models]", &reason);
                    ModelRefreshOutcome::failed(reason)
                }
            }
        })
    }

    /// 本家**有**远程目录（`GET {api}/models`，实测需要 Key）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 目录是账号级的（不同 Key 的可见范围不同），刷新走「账号」这一维。
    fn refresh_uses_account(&self) -> bool {
        true
    }

    /// **不认「默认模型」概念**：客户端的 `defaultModel` 是 workbuddy 语义的配置，
    /// 注入一个 workbuddy 模型名再路由到本家只会 404。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// SSE 帧的 model 名回写：**要写**（上游回的是内部承载名，见模块头）。
    fn sse_model_rewrite(&self) -> bool {
        true
    }

    // ─── 网页登录（PKCE）─────────────────────────────────────

    /// 本家支持「拉起授权页、由网关换码」的网页登录（OAuth 2.0 + PKCE S256）。
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 授权地址：`{auth}/auth?...`，回调是本进程的 loopback 端口。
    ///
    /// 端口未知（`ServerState` 还没登记）时返回 `None` —— 上层文案会如实说明
    /// 「未能生成授权地址」，而不是给一个必然连不上的地址。
    fn build_login_url(&self) -> Option<(String, String)> {
        let callback = login::callback_url()?;
        login::begin_login(&endpoints(), &callback)
    }

    /// 用回调里的一次性 `code` 换回长期 Key 并落账号。
    ///
    /// `state` 在这里**再校验一次**（深度防御）：真正可信的那份是 pending 表里
    /// 取出来的 verifier，取不到就是「不是本进程发起的那一轮」。
    /// 落账号走**与手填路径同一个**入口（`add_orcarouter_account`），
    /// 于是两条入口在下游完全等价。
    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(pending) = login::take_pending(state) else {
                return Err(GatewayError::with_status(
                    404,
                    "这次 OrcaRouter 登录已取消或已过期（授权码有效期 10 分钟），请重新发起",
                ));
            };
            let credentials = match login::exchange_code(&endpoints(), &pending, code, None).await {
                Ok(credentials) => credentials,
                Err(error) => {
                    // 这一轮已作废：pending 已在 take_pending 里取走，无需再清
                    logging::log(
                        "[Login]",
                        &format!("❌ OrcaRouter 网页登录换取凭据失败: {}", error.message),
                    );
                    return Err(error);
                }
            };
            let account = store
                .add_orcarouter_account(&credentials, None, SOURCE_PKCE)
                .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
            let account_id = account
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            logging::log(
                "[Login]",
                &format!(
                    "✅ OrcaRouter 网页登录完成，账号已加入列表（尾号 {}）",
                    credentials.token_tail()
                ),
            );
            Ok(account_id)
        })
    }
}

/// 上游错误体 → 可读文案。
///
/// OrcaRouter 的错误体是 `{"error": {"code": …, "message": …, "metadata": …}}`；
/// 项目的归一化层取 `message` / `msg` / `error.message`，因此嵌套形态已经落在
/// `message` 里。这里做最后一层适配：`message` 为空时回落到 `error` 的字符串形态，
/// 再回落到整份 JSON 的截断文本（便于定位非常规响应）。
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
    let text = error_body.to_string();
    text.chars().take(300).collect()
}

// ─── 供 HTTP 层调用的目录查询（GUI 模型下拉的数据源）─────────────

/// 某一用途下的模型选项（**这是模型下拉的唯一数据源**）。
///
/// `kind` = `"text"` / `"multimodal"` / `"embedding"` / `"image"` / `"video"` /
/// `"rerank"`，多模态时 `modality` 给 `"image"` / `"audio"` / `"video"`。
///
/// ── 为什么始终以账号级 live 目录为准 ──────────────────────────
/// 目录是账号级的：换一把 Key 就该看到不同的集合。因此这里**每次都用当前
/// 队首账号的 Key 真打一次上游**（不是读进程缓存）：下拉的语义是「我现在能调
/// 哪些模型」，读缓存会让用户在切换账号后看到上一个账号的集合。
/// 缓存只用于「live 失败时的兜底」，且必然带上 `degraded` 标记。
///
/// ── Key 绝不出后端 ──────────────────────────────────────────
/// 请求在**服务端**发出（Key 只在这里用），前端拿到的是最小模型元数据
/// （id / name / 上下文 / 模态 / 思考档位），不含任何凭据。
pub async fn list_models_for(
    store: &AccountStore,
    account_id: &str,
    kind_raw: &str,
    modality_raw: Option<&str>,
) -> Value {
    let modality = modality_raw.and_then(Modality::parse);
    let kind = match ModelKind::parse(kind_raw, modality) {
        Some(kind) => kind,
        None => {
            return catalog_payload(
                Vec::new(),
                "invalid",
                None,
                Some(format!(
                    "不认识的模型用途「{kind_raw}」（支持 text / multimodal / embedding / image / video / rerank）"
                )),
                store,
                account_id,
            )
        }
    };
    // 不带 capability 的用途（video / rerank 上游实测返回空集）也需要真打一次：
    // 空集是**上游的事实**，不是「没查」。
    let credential = store.orcarouter_api_key(account_id);
    let (api_key, proxy) = match credential {
        Ok(Some(credential)) => (credential.api_key, credential.proxy),
        Ok(None) => {
            return catalog_payload(
                Vec::new(),
                "unavailable",
                None,
                Some(
                    "请先添加一个 OrcaRouter 账号（填写 API Key 或 Connect with OrcaRouter）：\
                     模型目录需要账号的 Key 才能返回这台工作区真正可用的模型"
                        .to_string(),
                ),
                store,
                account_id,
            )
        }
        Err(reason) => {
            return catalog_payload(Vec::new(), "unavailable", None, Some(reason), store, account_id)
        }
    };
    match catalog::fetch_models_at(
        &endpoints(),
        &api_key,
        proxy.as_ref(),
        kind,
        modality,
    )
    .await
    {
        Ok(models) => {
            let count = models.len();
            catalog_payload(models, "live", Some(count), None, store, account_id)
        }
        Err(reason) => {
            // live 失败：给出**明确标注**的已验证兜底（只有文本用途有种子；
            // 其它用途没有可验证的种子，宁可给空列表 + 原因，也不编造模型名）。
            let fallback: Vec<Value> = if matches!(kind, ModelKind::Text | ModelKind::Multimodal(_)) {
                let snapshot = catalog::snapshot();
                snapshot
                    .models
                    .into_iter()
                    .filter(|item| kind.accepts(item))
                    .collect()
            } else {
                Vec::new()
            };
            let note = format!(
                "OrcaRouter 目录拉取失败（{reason}）；当前显示的是{}。请检查网络后重试。",
                if fallback.is_empty() {
                    "空列表（该用途没有经过验证的兜底清单）".to_string()
                } else {
                    format!("已验证的兜底清单（{} 条，可能已过期）", fallback.len())
                }
            );
            let source = if fallback.is_empty() { "fallback-empty" } else { "fallback" };
            catalog_payload(fallback, source, None, Some(note), store, account_id)
        }
    }
}

/// 组装给前端的目录响应（**不含任何凭据**）。
fn catalog_payload(
    models: Vec<Value>,
    source: &str,
    live_count: Option<usize>,
    note: Option<String>,
    store: &AccountStore,
    account_id: &str,
) -> Value {
    let endpoints = endpoints();
    let options: Vec<Value> = models
        .iter()
        .map(|item| {
            let id = catalog::item_id(item);
            let mut object = serde_json::Map::new();
            object.insert("id".to_string(), Value::String(id.clone()));
            object.insert(
                "name".to_string(),
                Value::String(catalog::display_name(item)),
            );
            if let Some(value) = item.get("maxInputTokens") {
                object.insert("maxInputTokens".to_string(), value.clone());
            }
            if let Some(value) = item.get("maxOutputTokens") {
                object.insert("maxOutputTokens".to_string(), value.clone());
            }
            object.insert(
                "supportedEndpointTypes".to_string(),
                item.get("supported_endpoint_types")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
            );
            object.insert(
                "inputModalities".to_string(),
                item.get("input_modalities")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
            );
            object.insert(
                "supportsReasoning".to_string(),
                Value::Bool(catalog::reasoning_supported(&id)),
            );
            let efforts: Vec<Value> = catalog::reasoning_efforts(&id)
                .iter()
                .map(|value| Value::String((*value).to_string()))
                .collect();
            if !efforts.is_empty() {
                object.insert("reasoningEfforts".to_string(), Value::Array(efforts));
            }
            Value::Object(object)
        })
        .collect();
    let count = options.len();
    let degraded = !matches!(source, "live");
    let snapshot = catalog::snapshot();
    serde_json::json!({
        "provider": "orcarouter",
        "providerLabel": "OrcaRouter",
        "source": source,
        "degraded": degraded,
        "catalogSource": endpoints.models_endpoint(),
        "authOrigin": endpoints.auth_base,
        "apiOrigin": endpoints.api_base,
        "count": count,
        "liveCount": live_count,
        "models": options,
        "note": note,
        "lastRefreshedAt": if snapshot.source == Source::Live { snapshot.fetched_at } else { 0 },
        "accountId": store
            .current_entry_for_provider("orcarouter")
            .map(|entry| entry.id)
            .filter(|_| account_id.is_empty())
            .unwrap_or_else(|| account_id.to_string()),
        "secret_masked": true,
    })
}

/// 把一次成功的手填凭据写回账号（HTTP 层用）。返回**实际生效**的账号（公开形态）。
///
/// 与 PKCE 入口共用 `add_orcarouter_account`，因此两条路的成果同形 —— 这是本集成
/// 的核心不变量（凭据来源对下游不可见），回归测试逐条盯着它。
pub fn save_manual_credentials(
    store: &AccountStore,
    payload: &Value,
    name: Option<&str>,
) -> Result<Value, crate::server::core::account_store::AccountStoreError> {
    let credentials = Credentials::from_manual(payload)
        .map_err(crate::server::core::account_store::AccountStoreError::bad_request)?;
    let credentials = match name {
        Some(name) if !name.trim().is_empty() => Credentials {
            name: name.trim().to_string(),
            ..credentials
        },
        _ => credentials,
    };
    store.add_orcarouter_account(&credentials, None, SOURCE_MANUAL)
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "live_tests.rs"]
mod live;
