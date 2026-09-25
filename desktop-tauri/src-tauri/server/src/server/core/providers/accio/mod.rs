//! Accio 适配器（`accio` 国际版 / `accio-cn` 国内版，同一份实现按地区参数化）：
//! 账号管理 **+ 推理转发**。
//!
//! ── 上游长什么样（逆向来源：Accio Work 桌面端安装包的 app.asar）──────
//!   - 业务网关：`https://phoenix-gw.alibaba.com`（两地区共用），鉴权**不走
//!     Authorization**：GET 把 `accessToken` 拼 query、POST 放进 body。
//!   - 推理网关：`POST {gw}/api/adk/llm/generateContent?sg_k=<md5(requestId)>`，
//!     SSE，body 是 **Gemini 风格**的 protobuf-JSON（`contents` /
//!     `system_instruction` / `tools`），账号凭证放在 body 的 `token` 字段里。
//!   - 登录：OAuth 2.0 授权码 + PKCE（`client_id=accio-work`），登录站点按地区
//!     分（`www.accio.com` / `www.accio-ai.com`）。
//!   - 额度：`GET /api/entitlement/quota` → `{usagePercent, refreshCountdownSeconds}`。
//!   - **没有签到**：上游没有那个活动（桌面端全包检索无「签到 / checkin」），
//!     因此 `billing::checkin` 的范围判定把这家排除（见那边的 accio 分支）。
//!
//! ── 为什么走会话式转发（`is_stateful`）────────────────────────
//! 无状态路径假设「请求体由通用层序列化后原样发出、响应是 OpenAI 的 SSE」。
//! Accio 两条都不满足（请求体要先转成 Gemini 信封、响应要拆 ADK 的帧）。
//! 因此这里实现 `forward_conversation`，把「构造 → 发送 → 翻译」收进
//! `chat.rs`；产出仍是 `ForwardOutcome`，网关其余链路零改动。
//! 与 Qoder 的差别：Accio 的鉴权只是「body 里放 token」，没有签名与编码，
//! 所以不需要 `cosy.rs` 那种签名层。
//!
//! ── 未验证项（接上线后第一次真实调用要盯的）──────────────────
//!   1. `utdid` / `version` / `x-package-region` 三个头是否被风控校验
//!      （桌面端都带；本实现按账号生成稳定的 utdid，可被 `ACCIO_*` 环境变量
//!      覆盖）—— **已实测（2026-09）**：三者都不是鉴权项，缺了也能跑通；
//!      真正必带的是 `appKey`（见 `endpoints::DEFAULT_APP_KEY`）。
//!   2. `redirect_uri` 白名单是否放行本网关的 loopback 地址（桌面端自己用的
//!      就是 `http://127.0.0.1:<port>/auth/callback`，形态一致，风险低）；
//!   3. 上游目录里 `visible: false` 的模型是否真的不可用（本实现按不可用过滤）。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本模块在对话链路上，绝不 unwrap/expect/panic。

pub mod auth;
pub mod balance;
pub mod chat;
pub mod credentials;
pub mod endpoints;
pub mod models;
pub mod oauth;
pub mod protocol;
pub mod refresh;
pub mod stream;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::content_block;
use crate::server::core::providers::adapter::{
    adapter_for, ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, ReasoningPatch,
    UpstreamErrorClass,
};
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::ForwardOutcome;
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;
use crate::server::logging;

use self::credentials::Credentials;
use self::endpoints::Region;

/// 国际版适配器实例（`adapter_for(ProviderKind::Accio)` 给出这一个）
pub static ACCIO_ADAPTER: AccioAdapter = AccioAdapter { region: Region::Global };
/// 国内版适配器实例（同一份实现，只有登录站点与 `x-package-region` 不同）
pub static ACCIO_CN_ADAPTER: AccioAdapter = AccioAdapter { region: Region::Cn };

/// 按地区参数化的适配器。**无状态**：它只持有一个地区标签，
/// 清单与 pending 都挂在进程级句柄上（见 `models.rs` / `oauth.rs`）。
pub struct AccioAdapter {
    region: Region,
}

/// 思考档位注入的字段名。
///
/// Accio 上游认的是 `properties.reasoning_effort` 与顶层的 `reasoning_effort`
/// 两种（按模型家族二选一，见 `protocol.rs`）。这里注入的 `reasoning_effort`
/// 是**客户端语义**的键：真正的落点在 `chat.rs` 构造请求体时按模型家族决定，
/// 因此 `Set` 让 body 顶层带上它、由协议层消费。
const REASONING_FIELD: &str = "reasoning_effort";

impl ProviderAdapter for AccioAdapter {
    fn kind(&self) -> ProviderKind {
        self.region.kind()
    }

    /// Accio 参与推理转发
    fn supports_chat(&self) -> bool {
        true
    }

    /// 上游是「一次请求一次回答」，但请求构造与响应翻译必须由适配器完成
    /// （Gemini 信封 + ADK 帧），见模块头。
    fn is_stateful(&self) -> bool {
        true
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 防御性报错：Accio 的请求体要先转成 Gemini 信封，通用层的序列化路径
    /// 产不出可用的字节。走到这里说明编排层的 `is_stateful` 分流坏了。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            503,
            "Accio 走会话式转发，不走单请求路径（内部错误：编排层未按 is_stateful 分流）",
        ))
    }

    /// 非 2xx 的分类。**注意**业务错误多数不体现在状态码上（在 SSE 帧里），
    /// 那些由 `forward_conversation` 直接转成带状态码的网关错误。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        match protocol::classify_upstream_error(status, raw) {
            protocol::UpstreamKind::Auth => UpstreamErrorClass::TokenExpired { message },
            protocol::UpstreamKind::Quota => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: error_body.get("code").and_then(Value::as_i64),
                status: if status == 0 { 429 } else { status },
            },
            // 内容策略拦截 → ContentBlocked（不罚账号，换中性提示词重试一次）
            protocol::UpstreamKind::Other => content_block::classify_or_fatal(
                status,
                error_body,
                message,
                error_body.get("code").and_then(Value::as_i64),
            ),
        }
    }

    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            refresh::ensure_fresh(store, account_id, self.region, false)
                .await
                .map(|credentials| credentials.access_token)
        })
    }

    /// 401 之后的**强制**续期：不看临期窗口（token 可能时间上还新但已被服务端
    /// 失效 —— 只调 ensure 会拿回同一个被拒的 token）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            refresh::ensure_fresh(store, account_id, self.region, true)
                .await
                .map(|credentials| credentials.access_token)
        })
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        store
            .accio_account_record(account_id, self.region.provider_id())
            .and_then(|record| Credentials::from_payload(&record).ok())
            .is_some_and(|credentials| credentials.expiring())
    }

    /// Accio **没有**「默认模型」概念：不指定模型时由它自己的目录决定
    fn supports_default_model(&self) -> bool {
        false
    }

    /// 思考等级绑定 → `reasoning_effort`（值原样交给协议层，按模型家族决定落点）。
    ///
    /// 客户端已显式指定时不覆盖（用户的明确意图比映射上的默认值更具体）；
    /// 档位不在通用 6 档表里时不注入（上游对未知档位要么忽略要么 400，都不如
    /// 明确跳过并说清原因）。`off` / `none` 在注入点就被拦下了（Accio 没有
    /// 可靠的「关闭思考」表达）。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> ReasoningPatch {
        if crate::server::core::model_rules::reasoning_is_off(level) {
            return ReasoningPatch::Skip { reason: "Accio 没有「关闭思考」的可靠表达，跳过注入" };
        }
        if crate::server::core::model_rules::read_client_level(body).is_some() {
            return ReasoningPatch::Skip { reason: "客户端请求体里已指定思考等级，绑定让位" };
        }
        if crate::server::core::model_rules::reasoning_rank(level).is_none() {
            return ReasoningPatch::Skip { reason: "该等级不在通用档位表内，Accio 不注入未知档位" };
        }
        ReasoningPatch::Set {
            field: REASONING_FIELD,
            value: Value::String(level.to_string()),
        }
    }

    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷新模型目录：用**库里第一个可用 Accio 账号**的凭证（目录接口要鉴权，
    /// 没有账号就拿不到）。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let Some(record) = store.accio_account_record(account_id, self.region.provider_id()) else {
                logging::verbose(
                    "[Models]",
                    &format!("Accio {}模型目录刷新跳过：尚未添加账号", self.region.label()),
                );
                if account_id.is_empty() {
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            };
            let Ok(credentials) = auth::snapshot(&record) else {
                return ModelRefreshOutcome::failed("Accio 账号凭证无效，请重新登录或粘贴凭证");
            };
            if credentials.region != self.region {
                return ModelRefreshOutcome::failed("指定的账号不属于这家（地区不匹配）");
            }
            let proxy = match auth::account_proxy(&record) {
                Ok(proxy) => proxy,
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            models::refresh(&credentials, proxy.as_ref(), force).await
        })
    }

    /// Accio 有余额 / 额度概念（额度用量百分比 + 订阅）
    fn supports_usage(&self) -> bool {
        true
    }

    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>> {
        Box::pin(async move { balance::query(store, account_id, self.region).await })
    }

    /// Accio 支持网页登录（OAuth 授权码 + PKCE，回调落在本机 loopback 端口）
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 授权地址：`{loginBase}/login?return_url=<本机回调>&state=…&code_challenge=…`
    ///
    /// 回调地址由本进程的监听端口拼出（`ServerState::bootstrap` 时写入，
    /// 见 `oauth::set_loopback_port`）。端口未知时返回 None —— 上层文案会把
    /// 「未能生成授权地址」透给用户，比拼一个必然 404 的地址要好。
    fn build_login_url(&self) -> Option<(String, String)> {
        let base = oauth::loopback_base()?;
        let return_url = format!("{base}{}", oauth::CALLBACK_PATH);
        Some(oauth::begin_login(self.region, &return_url))
    }

    /// 用回调里的一次性 `code` 换凭证并落账号。
    ///
    /// `state` 在这里再校验一次（深度防御：回调路由已比对过，但真正的凭据是
    /// pending 表里那份 —— 取不到就是「不是本进程发起的那一轮」）。
    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(pending) = oauth::take_pending(state) else {
                return Err(GatewayError::with_status(
                    404,
                    "这次登录已取消或已过期，请重新发起",
                ));
            };
            let credentials = oauth::exchange_code(
                pending.region,
                code,
                &pending.verifier,
                &pending.redirect_uri,
                None,
            )
            .await?;
            let account = store
                .add_accio_account(&credentials, None, "web")
                .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
            let account_id = account
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            logging::log(
                "[Login]",
                &format!(
                    "✅ Accio {}网页登录完成，账号已加入列表",
                    pending.region.label()
                ),
            );
            Ok(account_id)
        })
    }

    /// **会话式转发入口**：构造 → 发送 → 翻译。
    fn forward_conversation<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        body: &'a Value,
        _client_headers: &'a HeaderMap,
        proxy: Option<ResolvedProxy>,
        stream: bool,
        telemetry: &'a std::sync::Arc<RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ForwardOutcome, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let account_id = account_id.to_string();
            if account_id.is_empty() {
                return Err(GatewayError::with_status(
                    503,
                    "没有可用的 Accio 账号：请在账号页添加并启用账号",
                ));
            }
            let model_name = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // 凭证（含临期主动刷新）。显式账号必填 —— Accio 没有环境变量旁路。
            let context = account_context(store, &account_id, self.region, false).await?;
            // 思考档位：客户端显式指定 > 映射绑定（`body` 里已由 payload 层注入）
            let effort = protocol::client_effort(body);
            let plan = chat::build_plan(&context.credentials, body, &model_name, effort.as_deref())?;
            let effective_proxy = proxy.or(context.proxy);

            logging::verbose(
                "[Accio]",
                &format!(
                    "POST {} model={} upstream={} stream={} region={} account={}",
                    plan.url,
                    if model_name.is_empty() { "(默认)" } else { &model_name },
                    plan.upstream_key,
                    stream,
                    self.region.edition(),
                    account_id,
                ),
            );

            let capture = telemetry.capture();
            if let Some(capture) = capture.as_deref() {
                let headers: Vec<(String, String)> = plan.headers.clone();
                capture.reset_request(&plan.url, "accio", &headers, body);
            }

            let response = chat::send(&plan, effective_proxy.as_ref()).await?;
            if let Some(capture) = capture.as_deref() {
                capture.attach_response(response.status().as_u16(), response.headers());
            }
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error = chat::http_error(status, response).await;
                if error.status_code == 429 {
                    chat::record_limited(
                        &LimitContext {
                            store: store.clone(),
                            account_id: account_id.clone(),
                            model: model_name.clone(),
                        },
                        status,
                        &error.message,
                    );
                }
                return Err(error);
            }

            let translator = chat::Translator::new(chat::response_id(), plan.model_name.clone());
            let limit = chat::LimitContext {
                store: store.clone(),
                account_id: account_id.clone(),
                model: model_name.clone(),
            };
            if stream {
                // 首帧预读：额度/鉴权错误在 200 + 帧里，拦在返回之前换号无损
                let (prefetched, source) = chat::prefetch_stream_head(response, &limit).await?;
                let (sender, receiver) =
                    tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
                let telemetry = telemetry.clone();
                crate::spawn_task(async move {
                    chat::drive_stream(source, translator, telemetry, limit, sender, prefetched).await;
                });
                return Ok(ForwardOutcome::Stream {
                    status: 200,
                    stream: Box::new(tokio_stream::wrappers::ReceiverStream::new(receiver)),
                });
            }
            // 非流式：上游只支持流式，因此内部仍走流式拉取再聚合成完整响应
            match chat::drive_aggregate(response, translator, telemetry.clone(), &limit).await {
                Ok(body) => Ok(ForwardOutcome::Completion { body }),
                Err(error) => Err(error),
            }
        })
    }
}

/// 一次转发用的凭证 + 出口快照
struct AccountContext {
    credentials: Credentials,
    proxy: Option<ResolvedProxy>,
}

/// 取账号凭证（含临期主动刷新）与出口。
///
/// `force` 语义与 `refresh::ensure_fresh` 一致；401 之后的强制刷新由编排层经
/// `refresh_access_token` 调用，不在这里。
async fn account_context(
    store: &AccountStore,
    account_id: &str,
    region: Region,
    force: bool,
) -> Result<AccountContext, GatewayError> {
    let credentials = refresh::ensure_fresh(store, account_id, region, force).await?;
    let proxy = store
        .accio_account_record(account_id, region.provider_id())
        .map(|record| auth::account_proxy(&record))
        .transpose()?
        .flatten();
    Ok(AccountContext { credentials, proxy })
}

/// 会话式转发里的限额记账素材（`chat` 与 `protocol` 都要用，重导出避免深层路径）
pub use chat::LimitContext;

/// 供其它模块（登录路由）判断「这个 state 是否属于本家」用
pub fn region_from_provider_id(provider_id: &str) -> Option<Region> {
    Region::from_provider_id(provider_id)
}

/// 适配器实例自检（`implemented_kinds` 与 `adapter_for` 的一致性由编译期保证，
/// 这里只给一处便于排障的名字）
pub fn adapter_label() -> &'static str {
    "Accio"
}

/// 显式引用 `adapter_for`：让「加 kind 忘了接线」在本文件也会被编译器提醒
/// （模块头的约定，编译期断言的一种轻量形态）
pub fn adapter_for_self(region: Region) -> &'static dyn ProviderAdapter {
    adapter_for(region.kind())
}

