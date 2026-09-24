//! Qoder 适配器（Agent2API 多上游的第五家）：账号管理 **+ 推理转发**。
//!
//! ── 上游长什么样（移植来源 `Qoder-Proxy`）─────────────────────
//!   - 推理网关：`POST {gateway}algo/api/v2/service/pro/sse/agent_chat_generation`
//!     （`FetchKeys=llm_model_result&AgentId=agent_common&Encode=1`）。
//!     鉴权**不是 Bearer 令牌**，而是一套自签名的 COSY 头（见 `cosy.rs`）；
//!     请求体也不是裸 JSON，要先做一遍字节编码（`cosy::encode_body`），
//!     而签名覆盖**编码之后**的字节。
//!   - 模型目录：`GET {gateway}algo/api/v2/model/list?Encode=1`（同一套签名）。
//!   - 响应：SSE，但**多包了一层信封** —— `{statusCodeValue, body}`，
//!     真正的 OpenAI chunk 是 `body` 里的**内层 JSON 字符串**。
//!     业务错误也走这一层（HTTP 永远是 200，错误在 `statusCodeValue` 里）。
//!   - 地区：国际版 `api3.qoder.sh` / 中国版 `gateway.qoder.com.cn`，
//!     两边的模型目录与凭证**不通用**（见 `endpoints::Region`）。
//!
//! ── 为什么走会话式转发（`is_stateful`）────────────────────────
//! 无状态路径假设「请求体由通用层序列化后原样发出、响应是 OpenAI 协议的 SSE」。
//! Qoder 两条都不满足：请求体必须先编码再签名（通用层序列化出来的字节既没编码
//! 也没被签名覆盖），响应还要拆一层信封。因此这里实现 `forward_conversation`，
//! 把「构造 → 发送 → 翻译」整条链收进 `chat.rs`；产出仍是编排层认识的
//! `ForwardOutcome`，`chat.rs`（网关那个）的其余链路零改动。
//!
//! ── `is_stateful` 的语义在这里是什么 ────────────────────────
//! 与 CatPaw 的「多步会话协议」不同：Qoder 的上游仍是**一次请求一次回答**，
//! 我们这个标记只表示「一次发送要适配器自己完成」（架构文档 §4.2.1 把这条
//! 入口定义为「替换一次发送，不替换账号循环」）。账号选路、限额冷却、
//! telemetry 记账仍由编排层统一负责 —— 包括流内的业务错误触发的换账号
//! （见 `forward_conversation` 的返回：额度类错误按 429 交回编排层）。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   endpoints.rs   地区与端点（含推理网关基址）
//!   machine.rs     PKCE 随机串与本机标识（getrandom）
//!   oauth.rs       国际版 PKCE 设备授权
//!   credentials.rs 凭证格式（兼容 Qoder-Proxy 的 access/refresh）
//!   auth.rs        PAT 换取令牌 / 用户资料
//!   refresh.rs     续期（单飞 + 比较再写）
//!   balance.rs     额度查询（归一成账号页的统一形状）
//!   cosy.rs        COSY 请求签名 + 请求体编码（**推理链路的鉴权核心**）
//!   models.rs      模型目录（两地区缓存 + 静态兜底 + 远程刷新）
//!   protocol.rs    OpenAI ↔ Qoder 协议转换（消息/tools/思考档位/上游信封）
//!   stream.rs      上游 SSE 信封解包 + 思考标签拆解（跨分片）
//!   piping.rs      流式首帧预读 + 流式透传（issue #8 的换号修复在这）
//!   chat.rs        转发编排（构造 → 发送 → 翻译）与 delta 翻译器
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本模块在对话链路上，绝不 unwrap/expect/panic。

pub mod auth;
mod balance;
pub mod chat;
pub mod credentials;
pub mod cosy;
pub mod endpoints;
mod machine;
pub mod models;
pub mod oauth;
mod piping;
pub mod protocol;
mod refresh;
pub mod stream;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::content_block;
use crate::server::core::providers::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, ReasoningPatch, UpstreamErrorClass,
};
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;
use crate::server::logging;

use self::chat::Translator;
use self::stream::SseEvent;

pub struct QoderAdapter;
pub static QODER_ADAPTER: QoderAdapter = QoderAdapter;

/// 映射上绑的思考等级注入到请求体的哪个键。
///
/// `protocol::resolve_thinking` 认 `reasoning_effort` / `reasoning` / `thinking`
/// 三个键，选 `reasoning_effort` 的理由与 CatPaw 那边同：它是最标准、客户端最
/// 常传的那个，注入与「客户端已指定」在 body 上同形。
const REASONING_FIELD: &str = "reasoning_effort";

/// 一个上游响应 id（客户端按它关联同一次回答；形态照抄 OpenAI）
fn response_id() -> String {
    let uuid = cosy::random_uuid().unwrap_or_default().replace('-', "");
    let short: String = uuid.chars().take(24).collect();
    format!("chatcmpl-{short}")
}

/// 把「这个账号对这个模型已限额」记进账号库。
///
/// ── 为什么本适配器要自己做这件事（编排层不管吗）──────────────
/// 无状态路径里这个动作由编排层完成：它拿到适配器给的 `QuotaLimited` 分类，
/// 调 `rotate::mark_account_limited` 落冷却。但 Qoder 走**会话式转发**
/// （`is_stateful`），那条路径的适配器不返回 `UpstreamErrorClass` 分类
/// （「一次发送」整个在适配器内部），编排层只看得到一个 `Err`。而 Qoder 的
/// 限额信号**恰恰只出现在 SSE 信封里**（HTTP 永远是 200），也就是说：
/// 不在这里落冷却，这个额度已耗尽的账号会在**每一个后续请求**上被重新选中、
/// 重新打一次上游、再失败一次 —— 界面上看不到任何冷却标记。
///
/// 冷却键与编排层同源（`rateLimits[model]`，账号记录内的键天然带了 provider
/// 维度），因此界面上的「限流」列与自动降级逻辑对两家表现一致。
fn record_limited(
    store: &AccountStore,
    account_id: &str,
    model: &str,
    status: u16,
    kind: protocol::UpstreamKind,
    message: &str,
) {
    if !matches!(kind, protocol::UpstreamKind::Quota | protocol::UpstreamKind::Rate) {
        return;
    }
    if account_id.is_empty() {
        return;
    }
    // reset_at 给 None：上游没在响应里给结构化恢复时间，由存储层落 10 分钟兜底
    store.mark_rate_limited(account_id, model, status as i64, None, None, message);
    logging::log(
        "[Qoder]",
        &format!("⚠️ 账号 {account_id} 对模型 {model} 已限额，进入冷却"),
    );
}

impl ProviderAdapter for QoderAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Qoder
    }

    /// Qoder **有**推理转发能力（本波次接入）。
    ///
    /// 它决定这个家的账号会被派生成全局队首（`account_store::pick_current`）、
    /// 参与候选链与 `/v1/models` 广告。
    fn supports_chat(&self) -> bool {
        true
    }

    /// Qoder 的上游是「一次请求一次回答」，但**请求构造与响应翻译都必须由
    /// 适配器完成**（编码后签名、拆信封）—— 见模块头。
    fn is_stateful(&self) -> bool {
        true
    }

    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 防御性报错：Qoder 的请求体要先编码再签名，通用层的序列化路径产不出
    /// 可用的字节（见模块头）。走到这里说明编排层的 `is_stateful` 分流坏了。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            503,
            "Qoder 走会话式转发，不走单请求路径（内部错误：编排层未按 is_stateful 分流）",
        ))
    }

    /// 非 2xx 的上游响应分类。
    ///
    /// **注意**：Qoder 的业务错误不体现在 HTTP 状态码上（永远是 200，错误在
    /// SSE 信封里），所以这一层只处理真正的传输/网关层错误；流内的业务错误
    /// 由 `forward_conversation` 直接转成带状态码的网关错误交回编排层。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let classified = protocol::classify_upstream_error(status, raw);
        let message = format!("上游返回 {status}: {raw}");
        match classified.kind {
            protocol::UpstreamKind::Auth => UpstreamErrorClass::TokenExpired { message },
            protocol::UpstreamKind::Quota | protocol::UpstreamKind::Rate => {
                UpstreamErrorClass::QuotaLimited {
                    reset_at: None,
                    message,
                    upstream_code: error_body.get("code").and_then(Value::as_i64),
                    status: if status == 0 { 429 } else { status },
                }
            }
            // 内容策略拦截（审核文案）→ ContentBlocked：不罚账号，交给编排层换
            // 中性提示词重试一次 + 触发降级（见 `core::degrade`）
            _ => content_block::classify_or_fatal(
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
            refresh::ensure_fresh(store, account_id, false)
                .await
                .map(|credentials| credentials.access_token)
        })
    }

    /// 401 之后的**强制**续期：不看临期窗口。
    ///
    /// 与另外几家同一理由：401 完全可能发生在一个时间上还很新的 token 上
    /// （服务端侧失效 / 账号被顶下线 / refreshToken 轮换），此时只调 ensure
    /// 会拿回同一个被拒的 token。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            refresh::ensure_fresh(store, account_id, true)
                .await
                .map(|credentials| credentials.access_token)
        })
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        store
            .qoder_account_record(account_id)
            .and_then(|record| credentials::Credentials::from_payload(&record).ok())
            .is_some_and(|credentials| credentials.expiring())
    }

    fn supports_usage(&self) -> bool {
        true
    }

    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>> {
        Box::pin(balance::query(store, account_id))
    }

    /// Qoder 有远程目录（`GET {gateway}algo/api/v2/model/list`），支持刷新。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷新模型目录：用**库里第一个可用 Qoder 账号**的凭证（目录接口要签名，
    /// 没有账号就拿不到 —— 与源实现「必须登录」的前置条件一致）。
    ///
    /// `force` 一路透传给 `models::refresh`：`false` 走 1 小时 TTL 早退（自动
    /// 路径），`true` 真打上游（用户手动点刷新）。
    ///
    /// ── 没有账号时为什么返回「没刷」而不是「失败」──────────────────
    /// 手动刷新的逐家结果会显示在模型页上。一个从不用 Qoder 的用户点刷新时，
    /// 报一条红色失败会让他以为哪里坏了 —— 而事实只是「这家没有可刷的东西」。
    /// 三档语义里这正是 `unchanged`（skipped）的定义：**不是错误**。
    /// 真正该显示成失败的是「有账号但凭证坏了 / 上游拉不到」，那两种仍走
    /// `failed` 并带上原因。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            // 空 id = 队首可用账号（自动路径的默认）；非空 = 用户在弹窗里点名的
            // 那条 —— 点名取不到时按「没账号」处理（本家没有可回落的环境变量
            // 登录态），文案由上面那行「未添加账号」的回答覆盖不到，改用明确的失败。
            let Some(record) = store.qoder_account_record(account_id) else {
                // 自动路径每次拉目录/启动都会走到这里，所以只打 verbose：
                // 对不用 Qoder 的用户，这不是需要他关注的事
                logging::verbose("[Models]", "Qoder 模型目录刷新跳过：尚未添加 Qoder 账号");
                if account_id.is_empty() {
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的账号不存在或不可用，请重新选择");
            };
            let Ok(credentials) = credentials::Credentials::from_payload(&record) else {
                return ModelRefreshOutcome::failed("Qoder 账号凭证无效，请重新登录或更新 PAT");
            };
            let proxy = match auth::account_proxy(&record) {
                Ok(proxy) => proxy,
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            models::refresh(&credentials, proxy.as_ref(), force).await
        })
    }

    /// Qoder **没有**「默认模型」概念：不指定模型时应由它自己的目录决定，
    /// 而不是被注入一个别家语义的模型名（架构文档 §4.4 末句的「不注入」分支）。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// 思考等级绑定 → `reasoning_effort`（值**原样**交给本家的归一逻辑）。
    ///
    /// ── 为什么本家不做档位映射（与 CatPaw 的差别，别照抄那边）──────
    /// 「这一家收哪些档位」在 Qoder 是**模型级**知识：上游目录每个模型条目自己
    /// 声明 `thinking_config.enabled.efforts`（Qwen3.8 系列是 low / medium /
    /// xhigh），`protocol::resolve_thinking` 已经按它归一与回退（`minimal` →
    /// `low`、`high` / `max` → `xhigh`、模型不支持的档位退回该模型默认档）。
    /// 在这里再折一层等于把同一件事做两遍，而且**必然做错**：适配器这一层只看
    /// 得到 `_model` 这个名字，拿不到那个模型声明的档位表，只能瞎猜一个映射 ——
    /// 猜出来的映射与 `resolve_thinking` 里那份一旦不一致，同一档位会按不同
    /// 结果发出去（取决于哪一层先动手）。
    ///
    /// 所以这里只做两件事：判「客户端是不是已经指定了」、判「这个值值不值得
    /// 交给上游的归一逻辑」。第二件的判据是通用表的正向 6 档
    /// （`model_rules::reasoning_rank`）：
    ///   - 表内值 → 注入。归并与回退交给 `resolve_thinking`（它有模型上下文，
    ///     做得到）；
    ///   - 表外的自定义值 → 不注入。本家对未知档位**不报错**（回退到模型默认
    ///     档），所以注入它不会弄坏请求 —— 但也**不会生效**：上游收到的仍然是
    ///     模型默认档，而日志里会出现一行「已注入」。那正是「绑了没生效还查不出」
    ///     的典型来源，不如明确跳过并说清原因。
    ///   - `off` / `none` 在注入点就被拦下了（理由见 `model_rules::reasoning`：
    ///     本家关思考会让 Qwen3.8 系列行为异常），这里收不到。
    ///
    /// ── 客户端已指定时不覆盖 ────────────────────────────────────
    /// 判据走 `protocol::client_specified_reasoning` —— 它与
    /// `resolve_thinking` 的取值链**同一个函数**（只是把「值是 null」归到
    /// 「没指定」，理由写在那个函数的文档里），不存在「这里说没指定、
    /// 那边读出来一个值」。
    ///
    /// `_model` 不用（档位是模型级知识，但那份知识在 `models::resolve` 的结果
    /// 里，不在这条只有名字的路径上 —— 见上）。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> ReasoningPatch {
        if protocol::client_specified_reasoning(body) {
            return ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考档位，绑定不覆盖",
            };
        }
        if crate::server::core::model_rules::reasoning_rank(level).is_none() {
            return ReasoningPatch::Skip {
                reason: "该等级不在通用候选表内，本家无法判断上游收不收（档位由各模型自己声明）",
            };
        }
        ReasoningPatch::Set {
            field: REASONING_FIELD,
            value: Value::String(level.trim().to_string()),
        }
    }

    /// 从发送体读随行的思考等级：取值链复用 [`protocol::declared_reasoning`]。
    ///
    /// 显示的是**意图值**（客户端指定的原值，小写归一）而不是归一终值 ——
    /// 按「模型声明的档位表」归一（`minimal` → `low`、不支持档位退默认）那一步
    /// 需要 `resolve_thinking` 的模型目录上下文，发送体阶段拿不到（与
    /// `reasoning_patch` 只看得到名字是同一个约束，见上）。三档「不算等级」的
    /// 判定与 `resolve_thinking` 逐字同源：`off` / `none` / `disabled`（上游无法
    /// 真正关闭，不发档位）、布尔与 null（「开/关/默认」是开关语义，不是档位）。
    fn outbound_reasoning(&self, body: &Value) -> Option<String> {
        let raw = protocol::declared_reasoning(body)?;
        match raw {
            Value::String(text) => {
                let trimmed = text.trim().to_lowercase();
                match trimmed.as_str() {
                    "off" | "none" | "disabled" => None,
                    "" => None,
                    _ => Some(trimmed),
                }
            }
            // 布尔（开/关思考）与 null（未指定）都不构成「档位」
            _ => None,
        }
    }

    /// SSE 帧的 model 名回写由本适配器**自己做**（在 `forward_conversation` 里
    /// 用 `Translator::model_out`）—— 通用层的回写机制不认识 Qoder 的双层信封，
    /// 所以这里保持默认 false，避免通用层在错误的字节上做替换。
    fn sse_model_rewrite(&self) -> bool {
        false
    }

    /// 会话式转发入口：构造（编码 + 签名）→ 发送 → 翻译。
    ///
    /// ── 错误处理与编排层的分工 ──────────────────────────────────
    /// 上游的业务错误藏在 SSE 信封里（HTTP 200），所以「换账号重试」这个动作
    /// 必须由本函数在读到信封错误时**主动转成网关错误**交回编排层：额度/限流
    /// 报 429、鉴权报 401，编排层据此走它既有的三档动作（换账号 / 刷新重试）。
    /// 流已经开始下发之后的错误只能留在流里（HTTP 头早发出去了）——
    /// 那时补一帧 `{"error":…}` + `[DONE]`，与通用层的断流收尾同一形态。
    fn forward_conversation<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        body: &'a Value,
        _client_headers: &'a HeaderMap,
        proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        stream: bool,
        telemetry: &'a std::sync::Arc<crate::server::core::upstream::usage::RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::server::core::upstream::ForwardOutcome, GatewayError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let account_id = account_id.to_string();
            let model_name = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            // 凭证（含临期主动刷新）。显式账号必填 —— Qoder 没有环境变量旁路。
            if account_id.is_empty() {
                return Err(GatewayError::with_status(
                    503,
                    "没有可用的 Qoder 账号：请在账号页添加并启用账号",
                ));
            }
            let context = chat::account_context(store, &account_id, false).await?;
            let plan = chat::build_plan(&context.credentials, body, &model_name)?;
            // 代理：编排层给的优先（它与账号记录同源，但已经解析好），
            // 没有就用快照里的（例如目录刷新那条路径）
            let effective_proxy = proxy.or(context.proxy);

            logging::verbose(
                "[Qoder]",
                &format!(
                    "POST {} model={} upstream={} stream={} region={} account={}",
                    plan.url,
                    if model_name.is_empty() { "(默认)" } else { &model_name },
                    plan.upstream_key,
                    stream,
                    context.credentials.region.id(),
                    account_id,
                ),
            );

            // ── 调试模式：抓一份即将发出去的原始报文 ──────────────────
            // Qoder 是单次请求（一条对话 = 一次上游往返），与无状态路径同一
            // 时机：请求体已定稿、即将发送。开关关着时 capture 为 None。
            let capture = telemetry.capture();
            if let Some(capture) = capture.as_deref() {
                let headers: Vec<(String, String)> = plan.headers.clone();
                capture.reset_request(&plan.url, "qoder", &headers, body);
            }

            let response = chat::send(&plan, effective_proxy.as_ref()).await?;
            if let Some(capture) = capture.as_deref() {
                capture.attach_response(response.status().as_u16(), response.headers());
            }
            if !response.status().is_success() {
                // 只有失败响应才读体（成功的是 SSE 流，读了就没流了）
                let status = response.status().as_u16();
                let error = chat::http_error(status, response).await;
                // 限额信号落冷却（见 `record_limited`）：编排层在会话式路径上
                // 看不到分类，只有适配器知道这一条是额度类错误
                if error.status_code == 429 {
                    record_limited(
                        store,
                        &account_id,
                        &model_name,
                        status,
                        protocol::UpstreamKind::Quota,
                        &error.message,
                    );
                }
                return Err(error);
            }

            let translator = Translator::new(response_id(), plan.model_name.clone(), plan.thinking);
            // 限额记账的素材：流式分支的收尾发生在 handler 返回之后，那时
            // 这里的局部变量都还在（被 move 进后台任务），所以先把要用的
            // 那份句柄与标识克隆好（store 是 Arc 句柄，clone 很便宜）
            let limit_ctx = LimitContext {
                store: store.clone(),
                account_id: account_id.clone(),
                model: model_name.clone(),
            };
            if stream {
                // ── 首帧预读（issue #8 的修复，见 piping 模块头）────────
                // 拿到 HTTP 200 不能直接返回：Qoder 的额度错误恰恰写在 200
                // 的 SSE 信封里，改造前这里直接 Ok(Stream)，编排层退出后
                // 流内错误只能透传给客户端、换号无从谈起。预读把首个事件拦
                // 在返回之前 —— 业务错误转成带状态码的 Err 交回编排层换号
                // （此刻还没有任何字节下发，客户端的 200 头也没发出，换号
                // 无损）；拿到内容帧才返回 Ok(Stream)，预读帧随后补发。
                let (prefetched, source) =
                    piping::prefetch_stream_head(response, &limit_ctx, &telemetry).await?;
                let (sender, receiver) =
                    tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
                let telemetry = telemetry.clone();
                // 上游流必须被**拉到底**（源实现同样读完整条流再 cancel）：
                // 客户端断开时 tokio 的 channel 发送端会失败，循环随即退出，
                // drop 掉 source 就等价于断开上游连接。
                crate::spawn_task(async move {
                    piping::drive_stream(source, translator, telemetry, limit_ctx, sender, prefetched)
                        .await;
                });
                return Ok(crate::server::core::upstream::ForwardOutcome::Stream {
                    status: 200,
                    stream: Box::new(tokio_stream::wrappers::ReceiverStream::new(receiver)),
                });
            }

            // 非流式：内部仍走流式拉取（上游只支持流式），再聚合成完整响应
            // （流内的额度错误由 `drive_aggregate` 落冷却 —— 它就在错误现场，
            // 这里不再重复记一次）
            match drive_aggregate(response, translator, telemetry.clone(), &limit_ctx).await {
                Ok(body) => Ok(crate::server::core::upstream::ForwardOutcome::Completion { body }),
                Err(error) => Err(error),
            }
        })
    }
}

/// 流式链路的限额记账素材：预读的首帧错误（`piping::prefetch_stream_head`，
/// handler 返回前）与 `piping::drive_stream` 的中途错误都在错误现场落冷却；
/// 后者在 handler 返回之后才跑，那些标识必须随任务一起带走。
struct LimitContext {
    store: AccountStore,
    account_id: String,
    model: String,
}

/// 非流式：拉完整条上游流，聚合成一个完整 `chat.completion`。
///
/// 与流式路径的关键差别：这里的错误**还没有下发任何内容**（HTTP 头都没发），
/// 所以额度类错误可以带着状态码交回编排层，由编排层决定换账号还是刷新重试。
/// 冷却标记在调用方落（它持有 store 与账号标识的完整上下文）。
async fn drive_aggregate(
    response: reqwest::Response,
    mut translator: Translator,
    telemetry: std::sync::Arc<crate::server::core::upstream::usage::RequestTelemetry>,
    limit: &LimitContext,
) -> Result<Value, GatewayError> {
    use futures::StreamExt;

    let mut lines = stream::LineBuffer::new();
    let mut source = response.bytes_stream();
    let mut business_failure: Option<GatewayError> = None;
    // 调试模式的采集器（与 drive_stream 同一位置：解析之前采原始字节）
    let capture = telemetry.capture();

    'outer: while let Some(item) = source.next().await {
        let chunk = item.map_err(|error| {
            GatewayError::with_status(
                502,
                format!(
                    "Qoder 上游流式传输中断: {}",
                    crate::server::core::egress::describe_error_detail(&error)
                ),
            )
        })?;
        if let Some(capture) = capture.as_deref() {
            capture.push(&chunk);
        }
        for data in lines.push(&chunk) {
            match stream::parse_sse_line(&data) {
                SseEvent::Skip => {}
                SseEvent::Done => break 'outer,
                SseEvent::Error { status, kind, raw, message, pricing_url, .. } => {
                    record_limited(
                        &limit.store,
                        &limit.account_id,
                        &limit.model,
                        status,
                        kind,
                        &message,
                    );
                    business_failure = Some(chat::business_error(
                        status,
                        kind,
                        &raw,
                        &message,
                        pricing_url.as_deref(),
                    ));
                    break 'outer;
                }
                SseEvent::Chunk(chunk) => {
                    translator.consume(&chunk, Some(&telemetry));
                }
            }
        }
    }
    if business_failure.is_none() {
        for data in lines.finish() {
            if let SseEvent::Chunk(chunk) = stream::parse_sse_line(&data) {
                translator.consume(&chunk, Some(&telemetry));
            }
        }
    }
    // 聚合路径下这一档错误是**终态**：它能带着状态码交回编排层，由编排层决定
    // 换账号还是刷新重试（这是与流式路径的关键差别 —— 流式时 HTTP 头已发出）
    if let Some(error) = business_failure {
        return Err(error);
    }
    // 冲刷拆解器的尾巴，再成形（少了这一步，回答末尾会少几个字符）
    translator.finish();
    Ok(translator.completion_body())
}
