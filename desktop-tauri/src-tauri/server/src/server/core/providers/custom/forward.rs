//! 自定义提供商的 **Chat Completions 协议转发**（OpenAI 兼容透传，零转换）。
//!
//! ── 与内置家适配器的同与不同 ────────────────────────────────
//! 形状与「有状态适配器的转发入口」（`ProviderAdapter::forward_conversation`）
//! 同构：编排层（`provider_loop::attempt_custom`）完成选路、telemetry 记账，
//! 本函数只负责「一次发送」—— 读配置与凭证 → 改写请求体 → 发出 → 把响应
//! 包成 `ForwardOutcome`。它**不是** `ProviderAdapter` 的实现：自定义家不进
//! `ProviderKind`（理由见 `mod.rs` 的模块头），分派发生在 `attempt_queue`
//! 对 `is_custom_provider_id` 的显式分支上。
//!
//! slot / connections 作为参数进来（比编排概念的七参数签名多两个）：无状态
//! 路径在编排层就地构造 `ForwardStream`，而本函数在 `providers::custom` 下、
//! 离 `upstream` 模块树更远 —— 凭证的移交只能自己完成（`ForwardStream::new`
//! 需要它们）。多带两个 `&mut` 换来「与无状态路径逐字同构的交接时序」：
//! 流式时 `slot.take()` + `connections.handoff()` 移交给流，非流式时凭证留在
//! 调用方栈帧、随函数返回自然析构（与无状态路径同一条生命周期规则）。
//!
//! ── 透传语义（本协议的全部卖点，也是全部约束）─────────────────
//! 上游就是 OpenAI Chat Completions 形态，所以：
//!   - 请求体：客户端的字节**原样**出去（send_body 已做过系统提示词/脱敏两层
//!     处理），唯一的改写是 `model` 字段（映射 alias → 上游真名）与按需注入
//!     `reasoning_effort`（思考等级绑定）—— 都是「客户端语义」的修正，不是
//!     协议翻译。**唯一的例外**是记录显式开了 `clientEmulation`
//!     （见 [`emulation`]：OpenCode Zen 的免费档要求请求体是「智能体形态」，
//!     要补两个桩工具）—— 那是 opt-in 的形态伪装，默认一个字节都不动；
//!   - 响应：上游永远以 `stream:true` 被请求（编排层入口已强制注入），流式
//!     客户端拿 SSE 逐帧透传（reasoning 合并照走 `ForwardStream`），非流式
//!     客户端拿聚合后的完整 JSON（照走 `aggregate_sse_completion`）—— 两者的
//!     `model` 字段都改写回**客户端请求的名字**（参考 AutoClaw 的
//!     `sse_model_rewrite`：上游回显的名字以网关为准）。
//!   - usage 旁路提取在 `ForwardStream` / 聚合器内部完成（它们是 SSE 逐行
//!     解析的唯一入口），本函数不需要再读一遍响应体。
//!
//! ── 鉴权头怎么发（三档，见 [`ProviderQuirks`] 的两条拼装说明）────
//!   - 账号有 apiKey → 发 `Authorization: Bearer <key>`（或 anthropic 的
//!     `x-api-key`）；
//!   - 账号声明了无需鉴权（`noAuth`）→ **一个凭证头都不发**：空 Bearer 对
//!     严格的上游等同凭证错误，与「这家不要凭证」的语义正好相反；
//!   - 开了客户端伪装且没有 key → 用上游认的匿名凭证 `public`
//!     （OpenCode 免费档：不填 Key 也能跑，这就是那条路）。
//!
//! ── 协议分派（三种协议的分派点）─────────────────────────────
//!   - chat_completions：上游就是 chat 形态，**原样透传**（见「透传语义」）；
//!   - responses / anthropic：上游说另一种协议，走**翻译链** —— chat 体先
//!     过 [`rewrite_body`]（模型名 + 思考等级改写仍落在 chat 体上），再由
//!     `protocol::responses_outbound` / `protocol::anthropic_outbound` 翻成
//!     上游请求；上游的 SSE 事件流先过 [`ProtocolTranslateStream`] 折成标准
//!     chat SSE，之后流式照进 `ForwardStream`（reasoning 合并 / usage 提取 /
//!     model 回写三层不变），非流式照进聚合器。编排层零改动。
//! 分派点就是 [`forward`] 里 `protocol` 的 match：上游恒以 `stream:true`
//! 被请求的条策略对三种协议一体适用（翻译出的请求体上再覆写一次）。
//!
//! ── 错误分类（对齐 `UpstreamErrorClass` 的四档语义，让编排原样生效）──
//! 本函数不返回 `UpstreamErrorClass`（那个类型的构造与整条链的接口都收在
//! `providers::adapter`，自定义家不在那条链上），而是把分类结果**编码进
//! `GatewayError` 的状态码**，让编排层的两个既有判据原样生效：
//!   - 401 / 403 → 凭证错误（文案点明是 apiKey 的问题）。自定义账号没有可
//!     刷新的凭证（key 是用户填的），所以**不映射成 TokenExpired**（那会触发
//!     一次注定失败的刷新重试）—— 按普通失败落到编排层：记入 tried →
//!     顺延下一个账号；
//!   - 429 → 限额（`GatewayError::is_quota_limit` 的既有判据只认状态码 429）。
//!     恢复时间按 `Retry-After` 头解析（秒数或 HTTP 日期），解析不出再从
//!     错误文案里找（`errors::parse_quota_reset_at`）—— 解析结果以 UTC+8
//!     文案形态并回 message，于是 `rotate::mark_account_limited` 不用改签名
//!     也能把恢复时间读出来（那个函数内部再 parse 一次 message，两处同源）；
//!   - 5xx / 其它 4xx → 透传上游状态码与摘要。5xx 的「可重试」由编排层的
//!     「顺延下一个账号」承担（与有状态路径同一取向，不做原地退避）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 绝不 unwrap/expect（release 是 panic=abort）；serde 解析失败一律退化为
//! 「按原样透传」或「报一条明确错误」，绝不 panic。

use std::sync::Arc;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::custom_providers;
use crate::server::core::model_rules;
use crate::server::core::protocol::{anthropic_outbound, responses_outbound};
use crate::server::core::proxies::ResolvedProxy;
use crate::server::core::providers::custom::emulation;
use crate::server::core::upstream::connections::ConnectionGuard;
use crate::server::core::upstream::request::{read_upstream_error, send_chat_request, TransportRequest};
// `FramePolicy::with_rewrite(...)`：自定义家只带 model 回写，保活换行的丢弃
// 保持关闭 —— 那一项的依据是 WorkBuddy / AutoClaw 两条上游的实测，不能替
// 用户自配的上游猜。
use crate::server::core::upstream::sse::{FramePolicy, ModelRewrite};
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::{ForwardOutcome, ForwardStream, InFlightGuard};
use crate::server::errors::GatewayError;
use crate::server::logging;

/// 一次自定义家转发的入口（编排层的 `attempt_custom` 调用）。
///
/// `account_id` 是空串表示这一轮没有账号记录 —— 自定义家**没有环境变量旁路**
/// （凭证只来自账号记录，见 `custom_accounts` 的模块头），所以直接 503，
/// 不尝试任何默认登录态。
///
/// `body` 是已经过 `send_body` 处理的 chat 请求体（系统提示词/脱敏已落副本）；
/// 本函数只做「自定义语义」的最后一次改写（模型名 + 思考等级）再出门。
///
/// `pub(crate)` 而不是 `pub`：签名里的 `InFlightGuard` 是 `upstream` 的内部
/// 凭证类型（`pub(crate)`）—— 函数可见性不能超过它暴露的类型。调用面本来
/// 就只有 `upstream::provider_loop` 的编排层，`pub(crate)` 是准确的边界。
pub(crate) async fn forward(
    store: &AccountStore,
    provider_id: &str,
    account_id: &str,
    body: &Value,
    proxy: Option<ResolvedProxy>,
    stream: bool,
    telemetry: &Arc<RequestTelemetry>,
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
) -> Result<ForwardOutcome, GatewayError> {
    // ── 配置与凭证（顺序：先家后账号，错误文案各自指向要修的地方）──────
    let provider = custom_providers::get(provider_id).ok_or_else(|| {
        GatewayError::with_status(503, format!("自定义提供商不存在: {}", provider_id.trim()))
    })?;
    let protocol = provider
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or(custom_providers::PROTOCOL_CHAT_COMPLETIONS);
    // ── 协议分派（见模块头）────────────────────────────────────
    // 配置校验只放行三种协议，未知值仍 400 兜底 —— 明确的一句比「上游报了
    // 一个看不懂的错」有用得多。
    let kind = match protocol {
        custom_providers::PROTOCOL_CHAT_COMPLETIONS => None,
        custom_providers::PROTOCOL_RESPONSES => Some(OutboundKind::Responses),
        custom_providers::PROTOCOL_ANTHROPIC => Some(OutboundKind::Anthropic),
        other => {
            return Err(GatewayError::with_status(
                400,
                format!(
                    "自定义提供商「{}」的协议 {other} 无法识别，请检查提供商配置",
                    custom_providers::label_of(provider_id).unwrap_or_else(|| provider_id.to_string()),
                ),
            ));
        }
    };
    if account_id.trim().is_empty() {
        return Err(GatewayError::with_status(
            503,
            "没有可用账号，无账号可转发：请在账号页为该自定义提供商添加并启用账号",
        ));
    }
    // 凭证（内部形态：含 apiKey 明文与已解析代理）。取不到说明账号在两次
    // 读盘之间被删了 —— 报一条明确的错，与「顺延」语义同等可操作。
    let credential = store.custom_credential_by_id(account_id).ok_or_else(|| {
        GatewayError::with_status(503, format!("自定义账号不存在或已被删除: {account_id}"))
    })?;
    // baseUrl：账号覆盖项优先（缺省回落到提供商的配置）。两层都拦过空值，
    // 这里防御一次 —— 没有基址的转发只会变成一条难定位的连接错误
    let base_url = credential
        .base_url_override
        .clone()
        .or_else(|| {
            provider
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    if base_url.trim().is_empty() {
        return Err(GatewayError::with_status(
            400,
            "该自定义提供商没有可用的 baseUrl（提供商与账号上都没有配置）",
        ));
    }
    // 翻译协议在这里分出去（凭证与基址已就绪；chat 路径继续往下走）
    let quirks = ProviderQuirks::from_provider(&provider);
    if let Some(kind) = kind {
        return forward_translated(
            kind,
            provider_id,
            account_id,
            &credential.api_key,
            &base_url,
            &quirks,
            body,
            proxy,
            stream,
            telemetry,
            slot,
            connections,
        )
        .await;
    }
    // chat 透传出口：入口翻译（Anthropic / Responses）暂存的内部字段（`_wb_*`，
    // 见 `protocol::mod` 的说明）绝不发上游 —— 严格校验的 OpenAI 兼容上游会拒绝
    // 消息上的未知字段整轮 400。翻译分支（上方已 return）**不**剥：那些字段正是
    // 出站翻译要消费的（cache_control / is_error / encrypted_content 的恢复源）。
    //
    // 剥之前先过原生工具闸门：自定义家的契约是透传（chat 入口带上来的 chat 方言
    // 原生工具，如智谱的 `{"type":"web_search","web_search":{…}}`，原样上行），
    // 但**带标记的跨协议声明**（下游走 /v1/messages 或 /v1/responses 带过来的
    // `web_search_20250305` / `web_search` 那类）没有 chat 形态，硬发只会换来
    // 上游一次 400 —— 剔除 + 留痕，见 `protocol::native_tool` 模块头。
    let cross = crate::server::core::protocol::native_tool::strip_cross_protocol(
        body,
        crate::server::core::protocol::native_tool::ORIGIN_CHAT,
    );
    if let Some((_, dropped)) = &cross {
        logging::log(
            "[CustomProvider]",
            &dropped.describe(
                Some(provider_id),
                Some("该家按 chat 协议透传，承载不了别的协议的原生声明"),
            ),
        );
    }
    let stripped = crate::server::core::protocol::strip_internal_fields(
        cross.as_ref().map(|(next, _)| next).unwrap_or(body),
    );
    let body: &Value = &stripped;
    let url = format!(
        "{}/chat/completions{}",
        base_url.trim_end_matches('/'),
        quirks.url_suffix
    );

    // ── 请求体改写：模型名（映射 alias → 上游真名）+ 思考等级注入 ──────
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let (mut outbound, rewrite) = rewrite_body(body, provider_id, &requested);
    // 客户端形态伪装（OpenCode 免费档）：补桩工具。未开启时不碰请求体。
    // 放在 `rewrite_body` 之后、序列化之前 —— 会话种子取自**改写后**的体，
    // 与最终发出去的字节同源（模型名换了不影响 messages，两种取法等价，
    // 但同源更不容易在将来改坏）
    quirks.apply_emulation_to_body(&mut outbound);
    let payload = serde_json::to_string(&outbound)
        .map_err(|error| GatewayError::with_status(500, format!("请求体序列化失败: {error}")))?;
    // ── 出站头（三段拼装，顺序不可换）────────────────────────────
    //   ① 默认头：**只有 apiKey 非空才带凭证头** —— 无需鉴权的上游
    //      （`noAuth` 账号）拿到一个空 `Bearer ` 会当成凭证错误，
    //      与「这家不要凭证」的语义正好相反；
    //   ② 伪装头（OpenCode）：补齐官方客户端的头与匿名凭证 `public`；
    //   ③ 记录上的静态额外头（`quirks.apply_to`）：用户手写的最优先。
    let api_key = credential.api_key.trim();
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if !api_key.is_empty() {
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {api_key}"),
        ));
    }
    quirks.apply_emulation_headers(&mut headers, api_key, &outbound);
    quirks.apply_to(&mut headers);

    logging::verbose(
        "[CustomProvider]",
        &format!(
            "POST {url} model={} stream={stream} account={account_id} 出口={}{}",
            if requested.is_empty() { "(默认)" } else { &requested },
            describe_proxy(proxy.as_ref()),
            if quirks.emulate_opencode { " 伪装=opencode" } else { "" },
        ),
    );
    let transport = TransportRequest {
        url,
        headers,
        payload,
        proxy,
    };
    let response = send_chat_request(&transport)
        .await
        .map_err(|error| error.to_gateway_error())?;

    // ── 响应：2xx 打包 outcome；非 2xx 分类（见模块头）────────────────
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        // Retry-After 要在读 body **之前**取（read_upstream_error 会吃掉响应）
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let detail = read_upstream_error(response, telemetry.capture().as_deref()).await;
        return Err(classify_custom_error(
            status,
            &detail.message,
            retry_after.as_deref(),
        ));
    }

    if stream {
        // 槽位与连接计数移交给流（与无状态路径同一时序，见模块头）
        Ok(ForwardOutcome::Stream {
            status,
            stream: Box::new(ForwardStream::new(
                response,
                slot.take(),
                connections.handoff(),
                telemetry.clone(),
                FramePolicy::with_rewrite(rewrite),
            )),
        })
    } else {
        // 非流式：聚合内部同样按 `rewrite` 回写 body 的 model 字段；
        // 凭证留在调用方栈帧，随本函数返回析构（与无状态路径同）
        let aggregated = crate::server::core::upstream::aggregate::aggregate_sse_completion(
            response,
            telemetry.clone(),
            FramePolicy::with_rewrite(rewrite),
        )
        .await?;
        Ok(ForwardOutcome::Completion {
            body: aggregated.body,
        })
    }
}

/// 出站翻译协议（[`forward_translated`] 两个分支的差异点收敛在这个枚举上）
#[derive(Clone, Copy)]
enum OutboundKind {
    /// OpenAI Responses（`POST {base}/responses`）
    Responses,
    /// Anthropic Messages（`POST {base}/v1/messages`）
    Anthropic,
}

impl OutboundKind {
    /// 日志与文案里的协议名
    fn label(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
        }
    }
}

/// 提供商记录上的 **per-provider 特判**（`urlSuffix` / `headers` /
/// `anthropicToolType` / `clientEmulation`，形状见 `custom_providers` 的模块头）。
///
/// 这些字段由预置目录在创建时写入（参考实现 9Router 每家适配器里的修正：
/// GLM / MiniMax 的 `?beta=true` 与 `Anthropic-Beta`、OpenRouter 的来源头、
/// MiniMax 的工具 type 补齐），手写的家没有这几个键 —— 提取时缺省即「无特判」，
/// 旧记录不用迁移。
struct ProviderQuirks {
    /// 原样追加到出站 URL 末尾的查询串（如 `?beta=true`）
    url_suffix: String,
    /// 合并到默认头上的静态额外头（同名覆盖默认头）
    headers: Vec<(String, String)>,
    /// 发 anthropic 上游时给每个工具补 `type: "custom"`（MiniMax 拒绝无 type 工具）
    tool_type_custom: bool,
    /// 是否按 **OpenCode 官方 CLI 的形状**补齐出站请求（`clientEmulation`，
    /// 见 [`emulation`]）：无 key 用匿名凭证 `public`、补官方会话头、请求体补
    /// 两个桩工具 —— OpenCode Zen 的免费档只有这个形状才放行。**会改写请求体**，
    /// 所以只在记录显式声明时生效。
    emulate_opencode: bool,
}

impl ProviderQuirks {
    fn from_provider(provider: &Value) -> Self {
        // 盘上数据已经过读侧归一（custom_providers::item_of），这里只做防
        // 御性提取：类型不对的值按缺省跳过，绝不让特判把转发拖成 panic
        let headers = provider
            .get("headers")
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|text| (key.clone(), text.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            url_suffix: provider
                .get("urlSuffix")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            headers,
            tool_type_custom: provider
                .get("anthropicToolType")
                .and_then(Value::as_str)
                .is_some_and(|text| {
                    text.eq_ignore_ascii_case(custom_providers::TOOL_TYPE_CUSTOM)
                }),
            emulate_opencode: provider
                .get("clientEmulation")
                .and_then(Value::as_str)
                .is_some_and(|text| {
                    text.eq_ignore_ascii_case(custom_providers::CLIENT_EMULATION_OPENCODE)
                }),
        }
    }

    /// 把特判头按名合并进默认头：同名（ASCII 不区分大小写）的默认头被覆盖，
    /// 其余追加在后面。覆盖默认凭证头（Authorization / x-api-key）意味着换一种
    /// 鉴权方式 —— 那是记录持有者的选择，这里只负责如实执行。
    fn apply_to(&self, headers: &mut Vec<(String, String)>) {
        for (key, value) in &self.headers {
            match headers.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(key)) {
                Some(slot) => slot.1 = value.clone(),
                None => headers.push((key.clone(), value.clone())),
            }
        }
    }

    /// 伪装头与匿名凭证（只在 `clientEmulation: "opencode"` 时动手）。
    ///
    /// `api_key` 为空时用匿名凭证 `public` —— 这正是「不填 Key 也能用 OpenCode
    /// Zen 免费档」的落点；填了 key 就用 key（付费模型必须）。
    ///
    /// **调用顺序**：默认头 → 本函数 → `apply_to`（静态额外头）。放在 `apply_to`
    /// 之前是刻意的：手写在记录上的头优先级最高，伪装不该盖掉用户的显式选择。
    fn apply_emulation_headers(
        &self,
        headers: &mut Vec<(String, String)>,
        api_key: &str,
        body: &Value,
    ) {
        if !self.emulate_opencode {
            return;
        }
        let session = emulation::session_id(&emulation::conversation_seed(body));
        emulation::apply_headers(headers, api_key, &session);
    }

    /// 请求体补成「智能体形态」（`stream: true` + `bash` / `read` 桩工具）。
    /// 未开启伪装时**一个字节都不动** —— 透传是自定义家的默认语义。
    fn apply_emulation_to_body(&self, body: &mut Value) {
        if !self.emulate_opencode {
            return;
        }
        let _ = emulation::ensure_agent_shape(body);
    }
}

/// **翻译协议**（responses / anthropic）的一次转发。
///
/// 与 chat 分支同构的「一次发送」，差异只有三处（都在 [`OutboundKind`] 上
/// 分派）：URL 与鉴权头、请求体翻译（`protocol::*_outbound`）、上游 SSE 的
/// 翻译机（[`ProtocolTranslateStream`]）。其余环节 —— 模型名与思考等级的
/// 改写规则（[`rewrite_body`]，仍落在 chat 体上再整体翻译）、上游恒以
/// `stream:true` 被请求、非 2xx 的错误分类 —— 三种协议共用同一套。
async fn forward_translated(
    kind: OutboundKind,
    provider_id: &str,
    account_id: &str,
    api_key: &str,
    base_url: &str,
    quirks: &ProviderQuirks,
    body: &Value,
    proxy: Option<ResolvedProxy>,
    stream: bool,
    telemetry: &Arc<RequestTelemetry>,
    slot: &mut Option<InFlightGuard>,
    connections: &mut ConnectionGuard,
) -> Result<ForwardOutcome, GatewayError> {
    // ── 请求体：先在 chat 体上做自定义语义的改写，再整体翻译 ──────
    // （模型名映射与思考等级绑定是「客户端语义」的修正，与协议无关；
    // 两种上游各自的思考字段由转换器从改写后的 chat 体读取 —— responses
    // 读 reasoning_effort 进 reasoning.effort，anthropic 折成 thinking）
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let (mut outbound_chat, rewrite) = rewrite_body(body, provider_id, &requested);
    // 客户端形态伪装：与 chat 分支同一时机（改写后、翻译前）—— 桩工具要由
    // 转换器一起翻成上游协议的形态（anthropic 的 tools 数组）
    quirks.apply_emulation_to_body(&mut outbound_chat);
    // 发给上游的真名 = 改写后的 model 字段（rewrite_body 的产物；请求没带
    // model 时是空串，与 chat 分支「没有回写答案」的口径一致）
    let wire_model = outbound_chat
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // 出站头里的凭证（三段拼装的顺序说明见 chat 分支；这里按协议选凭证头的名字）
    let api_key = api_key.trim();
    let has_key = !api_key.is_empty();

    let (url, headers, payload) = match kind {
        OutboundKind::Responses => {
            let url = format!(
                "{}/responses{}",
                base_url.trim_end_matches('/'),
                quirks.url_suffix
            );
            let mut payload = responses_outbound::responses_request_from_chat(
                &outbound_chat,
                &wire_model,
            )
            .map_err(|message| GatewayError::with_status(400, message))?;
            // 上游恒以 stream:true 被请求（与 chat 协议同一条策略：流式才是
            // 完整能力，非流式下游由聚合路径收流）—— 转换器输出的 stream 只是
            // chat 体的如实搬运，这里统一覆写，不依赖编排层的注入约定
            if let Some(object) = payload.as_object_mut() {
                object.insert("stream".to_string(), Value::Bool(true));
            }
            let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
            if has_key {
                headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
            }
            quirks.apply_emulation_headers(&mut headers, api_key, &outbound_chat);
            quirks.apply_to(&mut headers);
            (url, headers, payload)
        }
        OutboundKind::Anthropic => {
            // anthropic 惯例 baseUrl 不带 /v1（官方是 https://api.anthropic.com），
            // 但兼容网关常按 OpenAI 习惯填带 /v1 的基址 —— 两种都认：已以 /v1
            // 结尾就不再重复拼。拼出 /v1/v1/messages 必然 404，而它看起来像
            // 「上游挂了」，排查方向会被带偏，所以在这里归一
            let base = base_url.trim_end_matches('/');
            let url = if base.ends_with("/v1") {
                format!("{base}/messages{}", quirks.url_suffix)
            } else {
                format!("{base}/v1/messages{}", quirks.url_suffix)
            };
            let mut payload = anthropic_outbound::anthropic_request_from_chat(
                &outbound_chat,
                &wire_model,
            )
            .map_err(|message| GatewayError::with_status(400, message))?;
            if let Some(object) = payload.as_object_mut() {
                object.insert("stream".to_string(), Value::Bool(true));
            }
            // 工具 type 补齐（MiniMax 的 Claude 兼容端点拒绝无 type 的工具，
            // 见 `custom_providers::TOOL_TYPE_CUSTOM`）：翻译器输出的是无 type
            // 的老式形态，特判开启时逐个补上 `type: "custom"`
            if quirks.tool_type_custom {
                if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
                    for tool in tools.iter_mut() {
                        if let Some(object) = tool.as_object_mut() {
                            object.insert(
                                "type".to_string(),
                                Value::String("custom".to_string()),
                            );
                        }
                    }
                }
            }
            // anthropic 的鉴权头不是 Bearer（与 `custom_providers::
            // fetch_upstream_models` 同一套），版本头按官方当前稳定值。
            // **凭证为空时不发 x-api-key**（理由见 chat 分支的①）
            let mut headers = vec![
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ];
            if has_key {
                headers.push(("x-api-key".to_string(), api_key.to_string()));
            }
            quirks.apply_emulation_headers(&mut headers, api_key, &outbound_chat);
            quirks.apply_to(&mut headers);
            (url, headers, payload)
        }
    };
    let payload = serde_json::to_string(&payload)
        .map_err(|error| GatewayError::with_status(500, format!("请求体序列化失败: {error}")))?;

    logging::verbose(
        "[CustomProvider]",
        &format!(
            "POST {url} protocol={} model={} stream={stream} account={account_id} 出口={}",
            kind.label(),
            if wire_model.is_empty() { "(默认)" } else { &wire_model },
            describe_proxy(proxy.as_ref()),
        ),
    );
    let transport = TransportRequest {
        url,
        headers,
        payload,
        proxy,
    };
    let response = send_chat_request(&transport)
        .await
        .map_err(|error| error.to_gateway_error())?;

    // ── 响应：2xx 打包 outcome；非 2xx 分类（与 chat 分支同一套）──────
    // 两种协议的错误体形状不同（responses 是 {error:{message}}、anthropic
    // 是 {type:"error",error:{message}}），read_upstream_error 的提取链
    // （message → msg → error.message → 前 500 字符兜底）两条都覆盖
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        // Retry-After 要在读 body **之前**取（read_upstream_error 会吃掉响应）
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let detail = read_upstream_error(response, telemetry.capture().as_deref()).await;
        return Err(classify_custom_error(
            status,
            &detail.message,
            retry_after.as_deref(),
        ));
    }

    // 上游字节流先过协议翻译（→ 标准 chat SSE），再交给既有消费层：
    // 流式进 ForwardStream（reasoning 合并 / usage 提取 / model 回写），
    // 非流式进聚合器 —— 槽位与连接计数的移交时序与 chat 分支逐字同构
    let translated =
        Box::pin(ProtocolTranslateStream::new(kind, &wire_model, response, telemetry));
    if stream {
        Ok(ForwardOutcome::Stream {
            status,
            stream: Box::new(ForwardStream::from_translated(
                translated,
                slot.take(),
                connections.handoff(),
                telemetry.clone(),
                FramePolicy::with_rewrite(rewrite),
            )),
        })
    } else {
        let aggregated = crate::server::core::upstream::aggregate::aggregate_frame_stream(
            translated,
            telemetry.clone(),
            FramePolicy::with_rewrite(rewrite),
        )
        .await?;
        Ok(ForwardOutcome::Completion { body: aggregated.body })
    }
}

/// 上游协议 SSE 字节流 → 标准 chat SSE 字节流（responses / anthropic 共用壳）。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// `ForwardStream` 与聚合器的输入都是**标准 chat SSE**（它们是 SSE 逐行
/// 解析的唯一入口，usage 提取 / model 回写 / reasoning 合并都挂在那两层）。
/// 翻译协议的上游吐的是自家协议的事件流，因此进入那两层之前先过一道
/// 「字节 → chat 帧」的翻译 —— 本结构只做管道（上游字节 → 转换器 → 帧队列），
/// 协议知识全在 `protocol::*_outbound` 的两个转换器里。
///
/// 上游断开时把 reqwest 错误描述成文案转成 `io::Error` 上抛：流式路径由
/// `ForwardStream` 补「错误帧 + [DONE]」收尾，聚合路径转成 502 —— 与 chat
/// 协议同一条错误语义。调试采集器在这里采**上游原始字节**（翻译前），
/// 与 chat 路径「采上游原样吐出的东西」的语义一致。
struct ProtocolTranslateStream {
    /// 上游字节流。错误在构造时就描述成文案折进 `io::Error`（与
    /// `ForwardStream::new` 同一手法）：空闲守卫的入参就是这个类型，
    /// 两个消费层（ForwardStream / 聚合器）拿到的也是同一形态。
    inner: futures::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
    machine: TranslateMachine,
    /// 已翻译待下发的帧（一个上游 chunk 可能产出多帧）
    pending: std::collections::VecDeque<bytes::Bytes>,
    /// 上游已结束（不再 poll 上游，把 pending 吐完即 None）
    upstream_done: bool,
    capture: Option<Arc<crate::server::core::debug_traffic::TrafficCapture>>,
}

/// 两个翻译状态机的分派壳（转换器接口同形，包一层枚举避免 dyn 分发）
enum TranslateMachine {
    Responses(responses_outbound::ChatFromResponsesStream),
    Anthropic(anthropic_outbound::ChatFromAnthropicStream),
}

impl TranslateMachine {
    fn push(&mut self, chunk: &[u8]) -> Vec<bytes::Bytes> {
        match self {
            Self::Responses(machine) => machine.push(chunk),
            Self::Anthropic(machine) => machine.push(chunk),
        }
    }

    fn finish(&mut self) -> Vec<bytes::Bytes> {
        match self {
            Self::Responses(machine) => machine.finish(),
            Self::Anthropic(machine) => machine.finish(),
        }
    }
}

impl ProtocolTranslateStream {
    fn new(
        kind: OutboundKind,
        model: &str,
        response: reqwest::Response,
        telemetry: &Arc<RequestTelemetry>,
    ) -> Self {
        use futures::StreamExt;
        // 上游字节 → io::Error（描述成文案）→ 空闲守卫（设置页「请求超时」
        // 的「流式响应空闲超时」，与 chat 路径同一份口径、同一个实现）
        let described = response.bytes_stream().map(|item| {
            item.map_err(|error| {
                std::io::Error::other(crate::server::core::egress::describe_error_detail(&error))
            })
        });
        let guarded = crate::server::core::upstream::stall::idle_guard(
            Box::pin(described),
            std::time::Duration::from_millis(
                crate::server::config::timeout_settings().stream_idle_ms(),
            ),
        );
        Self {
            inner: guarded,
            machine: match kind {
                OutboundKind::Responses => TranslateMachine::Responses(
                    responses_outbound::ChatFromResponsesStream::new(model),
                ),
                OutboundKind::Anthropic => TranslateMachine::Anthropic(
                    anthropic_outbound::ChatFromAnthropicStream::new(model),
                ),
            },
            pending: std::collections::VecDeque::new(),
            upstream_done: false,
            capture: telemetry.capture(),
        }
    }
}

impl futures::Stream for ProtocolTranslateStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures::StreamExt;
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return std::task::Poll::Ready(Some(Ok(frame)));
            }
            if self.upstream_done {
                return std::task::Poll::Ready(None);
            }
            match self.inner.poll_next_unpin(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => {
                    self.upstream_done = true;
                    // 上游 EOF：转换器收尾（finish_reason / usage / [DONE]）
                    for frame in self.machine.finish() {
                        self.pending.push_back(frame);
                    }
                }
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    // 调试模式：采上游原始字节（在翻译之前，与 chat 路径同语义）
                    if let Some(capture) = &self.capture {
                        capture.push(&bytes);
                    }
                    for frame in self.machine.push(&bytes[..]) {
                        self.pending.push_back(frame);
                    }
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    self.upstream_done = true;
                    // 文案已在构造时描述好（见 inner 字段说明）：reqwest 错误的
                    // 描述、空闲守卫的超时原文，原样上抛
                    return std::task::Poll::Ready(Some(Err(error)));
                }
            }
        }
    }
}

/// 请求体改写：返回（要发出去的体, SSE/聚合回写参数）。
///
/// ── 两道改写的细节（与内置家的对应关系）────────────────────────
///   - **模型名**：`custom_providers::wire_model_for` 一次解析出「上游真名 +
///     思考等级」（两者同源，见那个函数的说明）。真名与请求名忽略大小写相同
///     时**不复制**（零改写是常态）；不同才升级成副本；
///   - **思考等级**：三个条件同时满足才注入 `reasoning_effort` —— 绑定非空、
///     客户端没显式给 `reasoning_effort` / `reasoning`（用户显式意图优先）、
///     等级不是关闭思考（`model_rules::reasoning_is_off`；本网关不向任何
///     上游发「关闭思考」字段，判据与内置家共用）。
///
/// `rewrite` 是 SSE/聚合的 model 回写参数：请求带了 model 才给 ——
/// 客户端没点名模型时（上游用自家默认）没有「回写成什么」的答案。
fn rewrite_body(body: &Value, provider_id: &str, requested: &str) -> (Value, Option<ModelRewrite>) {
    let mut outbound = body.clone();
    if requested.is_empty() {
        return (outbound, None);
    }
    let (wire_model, reasoning) = custom_providers::wire_model_for(provider_id, requested);
    if !wire_model.eq_ignore_ascii_case(requested) {
        logging::verbose(
            "[CustomProvider]",
            &format!("provider={provider_id} 按映射改写模型名 {requested} → {wire_model}"),
        );
        if let Some(object) = outbound.as_object_mut() {
            object.insert("model".to_string(), Value::String(wire_model));
        }
    }
    if let Some(level) = reasoning
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        let explicit =
            outbound.get("reasoning_effort").is_some() || outbound.get("reasoning").is_some();
        if model_rules::reasoning_is_off(level) {
            logging::verbose(
                "[CustomProvider]",
                &format!(
                    "provider={provider_id} 映射 {requested} 绑定的思考等级为「{level}」\
                     （关闭思考），本网关不向任何上游发「关闭思考」字段，跳过注入"
                ),
            );
        } else if explicit {
            logging::verbose(
                "[CustomProvider]",
                &format!(
                    "provider={provider_id} 映射 {requested} 绑定的思考等级 {level} 未注入：\
                     客户端已显式指定思考字段"
                ),
            );
        } else if let Some(object) = outbound.as_object_mut() {
            logging::verbose(
                "[CustomProvider]",
                &format!(
                    "provider={provider_id} 映射 {requested} 注入思考等级 reasoning_effort={level}"
                ),
            );
            object.insert(
                "reasoning_effort".to_string(),
                Value::String(level.to_string()),
            );
        }
    }
    (
        outbound,
        Some(ModelRewrite {
            requested: requested.to_string(),
        }),
    )
}

/// 非 2xx 的响应 → 分类后的 `GatewayError`（分类语义见模块头）。
///
/// 三档处理，与 `UpstreamErrorClass` 的对应关系写在模块头：
///   - 401/403 → 凭证被拒；
///   - 429 → 限额；`Retry-After` 解析成功时把恢复时间以 UTC+8 文案并进
///     message（`rotate::mark_account_limited` 会从文案里再解析一次，
///     见模块头的说明）；
///   - 其余 → 原样透传状态码与上游原文摘要。
fn classify_custom_error(status: u16, raw: &str, retry_after: Option<&str>) -> GatewayError {
    let raw = raw.trim();
    let raw = if raw.is_empty() { "上游错误" } else { raw };
    match status {
        401 | 403 => GatewayError::with_status(
            i32::from(status),
            format!("上游凭证被拒绝（HTTP {status}）: {raw}"),
        ),
        429 => {
            let mut message = format!("上游返回 {status}: {raw}");
            // 恢复时间的解析链：Retry-After 头（秒数 / HTTP 日期）→
            // 错误文案里的日期形态（parse_quota_reset_at 的既有口径）。
            // 文案以「（<UTC+8 时间> 恢复）」附录形态并回 message ——
            // 与 rotate 的 reset_hint 同形状，用户与解析器都能读。
            if let Some(text) = retry_after
                .and_then(retry_after_reset_text)
                .or_else(|| text_of_reset_at(&message))
            {
                message.push_str(&format!("（{text} 恢复）"));
            }
            GatewayError::with_status(429, message)
        }
        _ => GatewayError::with_status(
            i32::from(status),
            format!("上游返回 {status}: {raw}"),
        ),
    }
}

/// `Retry-After` 头 → 恢复时间文案（UTC+8，`parse_quota_reset_at` 认的形态）。
///
/// 两种合法形态都认：delay-seconds（整数秒）与 HTTP-date（RFC 2822）。
/// 无法识别（负数、畸形日期）时给 None —— 调用方继续走文案解析的兜底。
fn retry_after_reset_text(retry_after: &str) -> Option<String> {
    let text = retry_after.trim();
    if let Ok(seconds) = text.parse::<i64>() {
        if seconds > 0 {
            return Some(reset_text_at(logging::now_ms() + seconds * 1000));
        }
        return None;
    }
    chrono::DateTime::parse_from_rfc2822(text)
        .ok()
        .map(|date| reset_text_at(date.timestamp_millis()))
        .filter(|text| !text.is_empty())
}

/// 错误文案里已有的恢复时间（`errors::parse_quota_reset_at` 的既有解析）。
/// 解析不出（0）时给 None —— 文案里不加空括号。
fn text_of_reset_at(message: &str) -> Option<String> {
    let reset_at = crate::server::errors::parse_quota_reset_at(message);
    (reset_at > 0).then(|| reset_text_at(reset_at))
}

/// 毫秒时间戳 → `YYYY-MM-DD HH:MM:SS UTC+8` 文案。
///
/// 与 `upstream::format_reset_text` 同口径（上游给的恢复时间按 UTC+8 标定，
/// 用本机时区会让用户对不上原文），只是格式改成连字符 + 显式 UTC+8 ——
/// `parse_quota_reset_at` 认的正是这一形态（斜杠形态它不认）。
/// 时间戳非法 / 偏移构造失败时给空串（这只是日志与文案，不能把请求带崩）。
fn reset_text_at(reset_at: i64) -> String {
    let Some(utc) = chrono::DateTime::from_timestamp_millis(reset_at) else {
        return String::new();
    };
    let Some(offset) = chrono::FixedOffset::east_opt(8 * 3600) else {
        return String::new();
    };
    utc.with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S UTC+8")
        .to_string()
}

/// 出口的可读描述（与 `upstream::describe_proxy` 同口径；那一个是
/// `pub(super)`，为了一行 verbose 日志不值得放宽 —— 留一份两行实现）。
fn describe_proxy(proxy: Option<&ResolvedProxy>) -> String {
    match proxy {
        Some(proxy) if !proxy.label.is_empty() => proxy.label.clone(),
        Some(proxy) => proxy.host.clone(),
        None => "直连".to_string(),
    }
}

/// 这一轮的**限额冷却键**（该家实际收到的上游真名；编排层 `attempt_custom` 用）。
///
/// 与 [`rewrite_body`] 内部同一解析（`wire_model_for` 是纯函数：读配置快照、
/// 无 IO、无账号参数，两次调用的结果必然相同）。单独开一个入口而不是让编排层
/// 直接调 `wire_model_for`：那个函数是「发送名怎么算」的实现细节，编排层不该
/// 认识它 —— 它只该问「这家的冷却键是什么」。
///
/// 请求体没带 model 时给空串（与 `SendBody::wire_model` 的空键口径一致）。
pub fn cooldown_model(provider_id: &str, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty() {
        return String::new();
    }
    custom_providers::wire_model_for(provider_id, requested).0
}
