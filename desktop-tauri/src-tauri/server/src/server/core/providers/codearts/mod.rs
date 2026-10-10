//! CodeArts（华为云 AI 代码助手 / snap-access）适配器。
//!
//! 上游不是 OpenAI 协议：请求要用华为云 **SDK-HMAC-SHA256**（AK/SK + STS）签名，
//! 会话是有状态的（每个对话一个 `session_id`，且上游对**每账号只有 3 路并发**），
//! 凭据是一次 OAuth/PKCE 登录换来的**临时**三元组、到期前 15 分钟就要续。
//! 移植的语义来源与逐条依据见 `cpa-deploy/notes/agent2api-codearts-port-plan.md`。
//!
//! 全链已接齐：签名（`signer`）/ 凭据 + DPoP + OAuth + 单飞续期（`credentials`、
//! `dpop`、`oauth`、`refresh`）/ 错误信封（`stream_fault`）/ 脱敏（`redact`）/
//! 请求构造 + 聚合 + 首包门（`chat`）/ 会话心跳与准入（`session`）/
//! 目录三源（`models`）/ 余额两份账（`balance`）/ 每日福利领取（`welfare`）/
//! provider 体系与账号存储接线；转发入口 `forward_conversation` 已在沙箱网关上
//! 真上游跑通（流式 + 非流式）。

pub mod balance;
pub mod chat;
pub mod onboarding;
pub mod welfare;
pub mod credentials;
pub mod dpop;
pub mod models;
pub mod oauth;
pub mod redact;
pub mod refresh;
pub mod session;
pub mod signer;
pub mod size_gate;
pub mod stream_fault;
pub mod tool_names;

use std::pin::Pin;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::upstream::ForwardOutcome;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::adapter::{ChatRequestPlan, ProviderAdapter, UpstreamErrorClass};

/// 进程级本地准入闸（同一上游身份的并发会话共享一个上限）。
static SESSION_GATE: std::sync::OnceLock<session::SessionGate> = std::sync::OnceLock::new();
use crate::server::core::providers::ProviderKind;

/// CodeArts 适配器（无状态字段：目录缓存在 `models` 里，凭据在账号存储里）。
pub struct CodeArtsAdapter;

/// 静态实例：`adapter_for` 要的是 `&'static dyn ProviderAdapter`。
pub static CODEARTS_ADAPTER: CodeArtsAdapter = CodeArtsAdapter;

impl CodeArtsAdapter {
    /// 从账号记录读回凭据（账号存储写的就是 `Credential` 的字段名）。
    pub fn credential(record: &Value) -> Result<credentials::Credential, GatewayError> {
        credentials::Credential::from_payload(record).map_err(|reason| GatewayError::with_status(503, reason))
    }
}

/// 登记尺寸门（体积墙可能走在 HTTP 状态上，也可能走在 SSE 第一帧里，三处出口共用这份）。
///
/// 只在**从放行变成挡住**那一次打一行终端日志：门存续期内每发大请求都会命中判据，
/// 逐请求打就是刷屏。
fn note_size_gate(reason: Option<&'static str>, wire_bytes: i64) {
    let Some(reason) = reason else { return };
    if size_gate::hold(wire_bytes, logging::now_ms()) {
        logging::console_line(
            "[Routing]",
            &format!(
                "⚠️ CodeArts 的尺寸门已登记：{}（本次发出 {} 字节），同尺寸或更大的请求暂时绕开这一家",
                reason, wire_bytes
            ),
        );
    }
}

/// 纯函数：给定目录与本次真名，返回整组冷却名单。福利判定**大小写无关**
/// （客户端的大小写变体也算福利）；命中 → 全部福利模型 id，并把**本次发送名
/// 原文**也补进名单 —— 它与目录 id 只是大小写不同时，判定侧对这类「映射没
/// 解析出来」的请求读的是请求原文键（`routing::CooldownKeys` 的 ④ 兜底），
/// 不补就会对同一形态的下一个请求漏命中。非福利/未知 → 只记本次名，
/// 与 trait 默认行为一致。
fn benefit_cooldown_group(catalog: &models::Catalog, wire_model: &str) -> Vec<String> {
    let is_benefit = catalog.models.iter().any(|model| {
        model.id.eq_ignore_ascii_case(wire_model) && model.source == models::ModelSource::Benefit
    });
    if !is_benefit {
        return vec![wire_model.to_string()];
    }
    let mut names: Vec<String> = catalog.models.iter()
        .filter(|model| model.source == models::ModelSource::Benefit)
        .map(|model| model.id.clone())
        .collect();
    if !names.iter().any(|name| name == wire_model) {
        names.push(wire_model.to_string());
    }
    names
}

impl ProviderAdapter for CodeArtsAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::CodeArts
    }

    /// 清单来自刷新链路落的进程内缓存（`list_models` 是同步契约，发不了网络请求）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 防御性报错：CodeArts 的对话要先占会话槽（每账号 3 路并发）、再签一次名、
    /// 再按流内信封判成败，单请求构造路径容纳不了 —— 与 Qoder / Accio 同一处境。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            503,
            "CodeArts 走会话式转发，不走单请求路径（内部错误：编排层未按 is_stateful 分流）",
        ))
    }

    fn is_stateful(&self) -> bool {
        true
    }

    /// 会话式转发：占槽 → 签名发送 → 首包门 → 透传/折叠，结束时释放会话。
    ///
    /// 顺序是有意的：**先过本地准入再占上游会话**。反过来的话，一个满员的账号会
    /// 白占一个上游槽位并立刻释放，把并发闸变成噪声。
    ///
    /// 两条收尾路径都必须走到 `ChatSession::stop()`（等 idle 真发出去）：
    /// 中途 `?` 提前返回时由 `ChatSession` 的 `Drop` 发注销信号，但那条不等
    /// idle 落地 —— 所以出错分支上显式 `stop().await`。
    fn forward_conversation<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        body: &'a Value,
        _client_headers: &'a HeaderMap,
        proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        stream: bool,
        telemetry: &'a std::sync::Arc<RequestTelemetry>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ForwardOutcome, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            // ① 凭据（含临期主动续期与写回）；代理沿用编排层为本账号解析出的那份
            let credential = refresh::ensure_fresh(store, account_id, false, proxy.as_ref()).await?;
            if !credential.valid() {
                return Err(GatewayError::with_status(503, "CodeArts 账号缺少可用的临时凭据，请重新登录"));
            }

            // ② 模型名归一：客户端习惯小写，上游要真名；福利模型要带 maas_type
            let requested = body.get("model").and_then(Value::as_str).unwrap_or("").trim();
            let catalog = models::cached_catalog().ok_or_else(|| GatewayError::with_status(
                503,
                "CodeArts 模型目录还没拉取过：请先在模型页对该账号执行一次「获取模型」",
            ))?;
            let Some(model) = catalog.resolve(requested) else {
                return Err(models::unknown_model_error(requested, &catalog));
            };
            let benefit = model.source.needs_benefit_header();
            let upstream_model = model.id.clone();

            // ②.5 尺寸门：这一家刚在「体 ≥ N 字节」上被上游拒过（TTL 内），同尺寸的
            // 就别再发出去 —— 上游那道墙按字节判，重发同一份体结论不变（实测见
            // [`chat::size_rejection`]：6 MB 级回 400 PARSE_REQUEST_DATA_EXCEPTION、
            // 12 MB 级回 413，两者都在出门之后才发生，白烧一次往返）。
            //
            // 估的是客户端 body 的字节数（**只在有门时**才付这一次序列化：无门是常态，
            // 常态路径零额外开销），比真实发出量小（本家还要塞会话 id、可能抬 max_tokens），
            // 所以直接 `estimate >= floor` 会在墙边上漏放一发 ——
            // dev 实测就是这个 32 字节：估 8,146,988 vs 真值 8,147,020，门形同虚设。
            // 留 64 KiB 的余量（远大于本家自己加的那些字段），代价是「比上界小不到
            // 64 KiB」的请求会被保守挡一次；比起每 120 秒白撞一发 8 MB 往返，这笔账划算。
            const ESTIMATE_SLACK_BYTES: i64 = 64 * 1024;
            let now_ms = logging::now_ms();
            if let Some(floor) = size_gate::floor(now_ms) {
                let estimate = serde_json::to_string(body)
                    .map(|text| text.len() as i64)
                    .unwrap_or_default();
                if estimate + ESTIMATE_SLACK_BYTES >= floor {
                    return Err(GatewayError::with_status(
                        i32::from(size_gate::BLOCKED_STATUS),
                        format!(
                            "CodeArts 发不出这么大的请求体（本次约 {} 字节，这一家已知 ≥ {} 字节会被上游拒）：\
                             本轮绕开这一家，最长 {} 秒后会重新探一次；\
                             要立刻可用就减小上下文（少带历史 / 别整份贴文件）",
                            estimate,
                            floor,
                            size_gate::ms_left(now_ms) / 1000
                        ),
                    ));
                }
            }

            // ③ 本地准入（满员回 409，不冷却账号）。
            // 身份优先用凭据里的 domain+user；两个都取不到时退到**账号行 id**
            // （不是 AK —— 它每次续期都换，用它当键等于每刷一次期就把并发上限清零）。
            let gate_identity = if credential.domain_id.trim().is_empty() && credential.user_id.trim().is_empty() {
                format!("account:{account_id}")
            } else {
                credential.identity()
            };
            // 面板那颗「并发上限」旋钮写的是账号上的 `maxConcurrent`，这里必须读它：
            // 只按硬编码默认值准入的话，那个数字对本家就只是装饰（口径与
            // `session::SessionGate::limit_for` 里写的「0 = 继承默认」一致）。
            let gate_override = store
                .codearts_account_record(account_id)
                .and_then(|record| record.get("maxConcurrent").and_then(Value::as_u64));
            let permit = SESSION_GATE
                .get_or_init(|| session::SessionGate::new(session::DEFAULT_SESSION_LIMIT))
                .acquire(models::DEFAULT_BASE_URL, &gate_identity, gate_override)?;

            // ④ 占一个上游会话槽（心跳 busy + 后台续期）
            let mut options = session::SessionOptions::new(
                models::DEFAULT_BASE_URL,
                &credential,
                chat::DEFAULT_LANGUAGE,
            );
            options.interval = session::HEARTBEAT_INTERVAL;
            let session = match session::ChatSession::begin(options).await {
                Ok(session) => session,
                Err(error) => {
                    drop(permit);
                    return Err(error);
                }
            };
            let session_id = session.id().to_string();

            // ⑤ 构造并发出（签名要覆盖到刚生成的 session id，所以顺序在后）
            let profile = chat::HeaderProfile {
                chat_session_id: Some(session_id),
                ..Default::default()
            };
            let (url, headers, payload, restorer) = {
                // 客户端额度太小、而这个模型一定先思考时，抬到 +预留（实测形状与
                // 三条边界见 `chat::reserve_for_thinking`）：不抬的话上游会把额度全
                // 花在 reasoning 上，客户端收到一次"成功的空回答"。
                let mut outbound = body.clone();
                if let Some((from, to)) =
                    chat::reserve_for_thinking(&mut outbound, model.max_output_tokens)
                {
                    logging::verbose(
                        "[CodeArts]",
                        &format!(
                            "客户端 max_tokens {from} 撑不下这个模型的思考，抬到 {to}（该模型上限 {}）",
                            if model.max_output_tokens > 0 {
                                model.max_output_tokens.to_string()
                            } else {
                                "未声明".to_string()
                            }
                        ),
                    );
                }
                // 函数名超上限的那道改写也必须发生在签名之前：改名动了 `tools`，
                // 而签名覆盖的就是出门那串字节（判据与还原的口径见 `tool_names`）
                let restorer = tool_names::shorten(&mut outbound);
                match chat::build_upstream_request(
                    models::DEFAULT_BASE_URL,
                    &upstream_model,
                    outbound,
                    true,
                    benefit,
                    &profile,
                    Some(&credential),
                ) {
                    Ok((url, headers, payload)) => (url, headers, payload, restorer),
                    Err(error) => {
                        session.stop().await;
                        drop(permit);
                        return Err(error);
                    }
                }
            };
            let mut request = crate::server::core::egress::client_for(proxy.as_ref()).post(&url);
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            // 真实发出量：只有这里知道（上游那道墙按它判），也是观测要的那个数
            let wire_bytes = payload.len() as i64;
            let response = match request.body(payload).send().await {
                Ok(response) => response,
                Err(error) => {
                    session.stop().await;
                    drop(permit);
                    return Err(GatewayError::with_status(
                        502,
                        format!("CodeArts 对话请求失败：{}", crate::server::core::egress::describe_error_detail(&error)),
                    ));
                }
            };

            // ⑥ 非 2xx：原样带状态码 + 脱敏诊断体
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                // 有预算地读：见 `chat::read_error_body`（它同时把 truncated 如实带出来，
                // 让脱敏那条"末尾正好是秘密前缀"的分支真的会被走到）
                let (error_body, truncated) = chat::read_error_body(response).await;
                let error_text = String::from_utf8_lossy(&error_body).to_string();
                let size_reason = chat::size_rejection_any(status, &error_text, wire_bytes);
                // 判据没命中但状态是 400/413/502 时，把原始诊断体露一小截：这道墙的
                // 症状散在不同层（HTTP 状态 vs SSE 帧），靠猜改不动它。
                // 只在 verbose 下打（生产默认关），截 200 字，绝不整份进日志。
                if matches!(status, 400 | 413 | 502) && size_reason.is_none() {
                    let preview: String = error_text.chars().take(200).collect();
                    logging::verbose(
                        "[CodeArts]",
                        &format!("非 2xx 诊断体（尺寸判据未命中，前 200 字）：{preview}"),
                    );
                }
                // 体积被拒 ⇒ 按**真值**登记尺寸门，下一轮同尺寸的在 ②.5 就被挡在本地。
                note_size_gate(size_reason, wire_bytes);
                session.stop().await;
                drop(permit);
                telemetry.note_attempt_body_bytes(wire_bytes);
                return Err(chat::upstream_http_error(status, &error_body, &credential, truncated));
            }

            // 发出量落账（成功与失败都记：这条要回答的是「这类会话到底多大」，
            // 只记失败的那次就永远看不到分布）
            telemetry.note_attempt_body_bytes(wire_bytes);

            // ⑦ 首包门：一个字节都没下发之前就决定"交出去"还是"换账号"
            let (prefetched, rest) = match chat::prefetch_head(response).await {
                Ok(head) => head,
                Err(error) => {
                    // 首包门就把故障帧判掉了 —— 尺寸墙正是从这里出去的（dev 实测：
                    // 8 MB 那发在 `prefetch_head` 里就成了故障），所以登记必须在这里；
                    // 放在 ⑦.5 的那些分支上永远摸不到，门一次也不会立起来。
                    note_size_gate(
                        chat::size_rejection_any(0, &error.message, wire_bytes),
                        wire_bytes,
                    );
                    session.stop().await;
                    drop(permit);
                    return Err(error);
                }
            };

            // ⑦.5 体积墙也走在**流里**：HTTP 200 + 一帧 `InferHub.001001005.400`
            // 才是真结论（dev 实测 8 MB 那发就是这样回来的，标记只在
            // `details[].error_code`）。尺寸类失败必然出现在第一帧，所以扫头部字节就够。
            note_size_gate(
                chat::size_rejection_any(0, &String::from_utf8_lossy(&prefetched), wire_bytes),
                wire_bytes,
            );

            if !stream {
                // 非流式：把剩下的读完再折叠（上游只有流式，与参考实现同一做法）
                use futures::StreamExt;
                let mut all = prefetched;
                let mut rest = rest;
                let read_error = loop {
                    match rest.next().await {
                        None => break None,
                        Some(Err(error)) => break Some(error),
                        Some(Ok(chunk)) => all.extend_from_slice(&chunk),
                    }
                };
                session.stop().await;
                drop(permit);
                if let Some(error) = read_error {
                    return Err(GatewayError::with_status(502, format!("CodeArts 上游流中断：{error}")));
                }
                // 折叠前用完整体再判一次（第一帧没中时兜住；同扇门内重复登记不会再打日志）
                note_size_gate(
                    chat::size_rejection_any(0, &String::from_utf8_lossy(&all), wire_bytes),
                    wire_bytes,
                );
                let mut completion = chat::aggregate_sse(&all, &upstream_model)?;
                if let Some(restorer) = restorer.as_ref() {
                    // 客户端要认的是它自己那份名字，折叠好的回答里得换回去
                    restorer.apply_completion(&mut completion);
                }
                // 用量旁路记账：聚合体里那份 usage 是上游给的，报一次进请求日志
                // （流式那份由 `UsageSniffer` 负责，两条路都缺了就又是恒 0）
                if let Some(usage) = completion.get("usage") {
                    telemetry.report_usage(usage);
                }
                return Ok(ForwardOutcome::Completion { body: completion });
            }

            // ⑧ 流式：透传（上游已是 OpenAI chunk 形状），结束时释放会话与许可
            let (sender, receiver) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
            // usage 嗅探要活到这个任务里，而本函数是借用签名 —— 克隆一份 Arc
            // （不是所有权转移：编排层那一份还要在收尾时读同一槽位）
            let sniff_telemetry = telemetry.clone();
            // 改过函数名才需要还原响应里的名字；没改过（绝大多数请求）时这个 fixer
            // 全程 idle，一个字节都不碰，走的还是下面那条原样透传
            let mut fixer = tool_names::StreamFixer::new(restorer);
            crate::spawn_task(async move {
                use futures::StreamExt;
                let mut sniffer = chat::UsageSniffer::default();
                if !prefetched.is_empty() {
                    sniffer.feed(&prefetched, &sniff_telemetry);
                    let head = if fixer.idle() {
                        bytes::Bytes::from(prefetched)
                    } else {
                        bytes::Bytes::from(fixer.push(&prefetched).into_owned())
                    };
                    if sender.send(Ok(head)).await.is_err() {
                        session.stop().await;
                        return;
                    }
                }
                let mut rest = rest;
                while let Some(item) = rest.next().await {
                    // 只读地看一眼这一片里有没有 usage 帧，字节原样转发
                    if let Ok(bytes) = item.as_ref() {
                        sniffer.feed(bytes, &sniff_telemetry);
                    }
                    let to_send = if fixer.idle() {
                        item
                    } else {
                        match item {
                            Ok(bytes) => Ok(bytes::Bytes::from(fixer.push(&bytes).into_owned())),
                            Err(error) => Err(error),
                        }
                    };
                    if sender.send(to_send).await.is_err() {
                        break;
                    }
                }
                // 流结束时把成帧缓冲里剩下的一段交出去（半条事件还原不了也照原样放行）
                let tail = fixer.finish();
                if !tail.is_empty() {
                    let _ = sender.send(Ok(bytes::Bytes::from(tail))).await;
                }
                sniffer.finish(&sniff_telemetry);
                // 客户端断开也会走到这里：idle 必须发，否则上游槽位悬着
                session.stop().await;
                drop(permit);
            });
            Ok(ForwardOutcome::Stream {
                status: 200,
                stream: Box::new(tokio_stream::wrappers::ReceiverStream::new(receiver)),
            })
        })
    }

    /// 非 2xx 的分类。
    ///
    /// **注意**：CodeArts 的业务失败常常不体现在状态码上（额度耗尽是 HTTP 200
    /// SSE 里的信封），那部分由 `stream_fault` 负责；这里只管真的是状态码的错。
    /// 三条口径与参考实现一致：403 = 额度、429 = 限流、其余透传。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("error_msg"))
            .or_else(|| error_body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = format!("上游返回 {status}{}", raw_part(raw));
        match status {
            // 403：额度耗尽 —— 标记账号冷却并换下一账号。恢复时刻只在**认得出日池**
            // 时给（下一个北京零点），其余仍走存储层的 10 分钟兜底，见
            // [`daily_pool_reset_at`]。
            403 => UpstreamErrorClass::QuotaLimited {
                reset_at: daily_pool_reset_at(logging::now_ms(), &message),
                message,
                upstream_code: None,
                status,
            },
            429 => UpstreamErrorClass::QuotaLimited {
                // 429 是分钟级限流，与日池无关：不给它零点，交给 10 分钟兜底
                reset_at: None,
                message,
                upstream_code: None,
                status,
            },
            // 401：临时凭据过期/被换掉 —— 刷一次再用同一账号重试
            401 => UpstreamErrorClass::TokenExpired { message },
            _ => UpstreamErrorClass::Fatal {
                status,
                message,
                upstream_code: None,
            },
        }
    }

    /// 会话式路径的分类：429 无歧义（限流）→ 记账；403 只有在**认得出额度**
    /// 时才记账 —— 流内额度信封由首包门按 `insufficient quota` 折成 403，
    /// 消息必带这串原文，HTTP 403 的诊断体带了也算数。认不出的 403（内容
    /// 闸门 / 权限 / 签名这类）一律 Fatal 透传顺延、不罚账号：403 在本家是
    /// 个粗状态码（`stream_fault` 给客户端补 `code` 正是为这个歧义），而福利
    /// 池按整组记账，误罚的爆炸半径是全部福利模型 —— 宁缺勿滥。
    ///
    /// 首包门把流内 `insufficient quota` 信封折成 403 后落在这里 —— 之前它走
    /// 「一律透传」，福利池耗尽后每个请求都白撞一遍全部账号、一个冷却都不落
    /// （实测三条 codearts 账号每请求约 1.2s 的顺延噪声，见 2026-09-28 取证）。
    ///
    /// 401 故意**不**交 TokenExpired：会话式路径没有「刷凭证后同账号重试」
    /// 的编排，透传让调用方顺延下一个账号（跟现状一致）。
    fn classify_conversation_error(&self, error: &GatewayError) -> UpstreamErrorClass {
        let quota_403 = error.status_code == 403
            && error.message.to_lowercase().contains("insufficient quota");
        match error.status_code {
            429 => UpstreamErrorClass::QuotaLimited {
                // 429 是分钟级限流，与「今天没额度」是两回事 —— 给它零点会把一个
                // 只是暂时繁忙的账号打死一整天，所以交给存储层的 10 分钟兜底。
                reset_at: None,
                message: error.message.clone(),
                upstream_code: error.upstream_code,
                status: u16::try_from(error.status_code).unwrap_or(429),
            },
            403 if quota_403 => UpstreamErrorClass::QuotaLimited {
                // 认得出额度 ⇒ 日池打满 ⇒ 冷却到下一个北京零点（同一判据见
                // [`daily_pool_reset_at`]）；这里已经要求文案带 `insufficient quota`，
                // 所以给的一定是零点而不是 10 分钟。
                reset_at: daily_pool_reset_at(logging::now_ms(), &error.message),
                message: error.message.clone(),
                upstream_code: error.upstream_code,
                status: 403,
            },
            _ => UpstreamErrorClass::Fatal {
                status: u16::try_from(error.status_code).unwrap_or(500),
                message: error.message.clone(),
                upstream_code: error.upstream_code,
            },
        }
    }

    /// 福利池是**账号级**日额度：本次失败的模型是福利源时，整组福利模型一起
    /// 记冷却 —— 不然池子已经空了，换一个福利模型名照样从头撞一遍。
    /// 真名以目录缓存的 `id` 为准（转发的发送名就是它）；模型不在缓存里
    /// （目录还没拉过 / 已下架）或不是福利源时，只记本次的真名，与默认行为
    /// 一致。纯逻辑在模块级 [`benefit_cooldown_group`]（吃目录参数，可测）。
    fn quota_cooldown_models(&self, _account_id: &str, wire_model: &str) -> Vec<String> {
        match models::cached_catalog() {
            Some(catalog) => benefit_cooldown_group(&catalog, wire_model),
            None => vec![wire_model.to_string()],
        }
    }

    fn supports_chat(&self) -> bool {
        true
    }

    /// 一次性 refresh token 决定了这条开关的分量：刷一次就轮换，所以
    /// `supports_refresh` 与「把新串写回账号存储」必须同时成立，否则等于烧掉凭据
    /// 且新串丢失（账号直接判死刑）。写回通路在 `refresh::ensure_fresh` →
    /// `update_codearts_credentials_if_current`（按凭据形态比对后再写，见 store 侧）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 取一个**可用**的 access key：临期就真刷并写回，与 cline/qoder 同口径。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let proxy = record_proxy(store, account_id)?;
            Ok(refresh::ensure_fresh(store, account_id, false, proxy.as_ref())
                .await?
                .access_key_id)
        })
    }

    /// 面板上那条「刷新凭据」走的是这条（`refresh_access_token`，不看临期窗口）。
    ///
    /// **不能省**：漏了它就落到 trait 的默认实现（= `ensure_access_token`），而默认
    /// 实现早期版本只回读存储、不碰上游 —— 沙箱实测就是那样：接口回 `success` +
    /// `refreshedId`，库里的 AK 一个字符都没变。用户点了刷新、看着成功，临期凭据
    /// 照旧临期。这类「报成功但什么都没发生」比报错更难查。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let proxy = record_proxy(store, account_id)?;
            Ok(refresh::ensure_fresh(store, account_id, true, proxy.as_ref())
                .await?
                .access_key_id)
        })
    }

    /// 临期判定要与 `ensure_fresh` 用同一个提前量：批量维护循环据此决定要不要动
    /// 这个账号，回 false 就等于把它从维护范围里永久剔除（本家凭据默认只活一小时，
    /// 后台不刷的话，闲置一阵后的第一个请求要先付一次换证的延时）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        store
            .codearts_account_record(account_id)
            .and_then(|record| credentials::Credential::from_payload(&record).ok())
            .is_some_and(|credential| credential.can_refresh() && credential.needs_refresh(refresh::REFRESH_LEAD_MS, crate::server::logging::now_ms()))
    }

    /// 余额可查（两份账，见 `balance.rs` 模块头：订阅统计 + 福利网关）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// **两个源各自成功各自失败**：福利网关是另一台机器，它挂了不该让订阅统计
    /// 显示不出来（反过来也是）。只有两边都失败才整次失败 —— 而那通常意味着
    /// 凭据本身已经不行了，错误文案照 `statistics` 那侧给（它才是主账）。
    ///
    /// 失败的一侧仍写进 `benefitError` / `statisticsError`：界面上「读到 0」与
    /// 「没读到」必须能区分开，这是本家面板最容易误判的一处。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let credential = welfare::current_credential(store, account_id).await?;
            let (statistics, benefit) = balance::fetch_both(
                models::DEFAULT_BASE_URL,
                models::DEFAULT_BENEFIT_GATEWAY_URL,
                &credential,
                chat::DEFAULT_LANGUAGE,
                chat::DEFAULT_PLUGIN_VERSION,
            )
            .await;
            // ── 「无 benefit 档案」先自动注册一次再重查 ─────────────────
            // benefit 记录不是开账号就有的：官方客户端每次启动都 POST claim
            // （幂等建档，见 `balance::claim_benefit` 的模块注释），没走过这一步
            // 的账号 InferHub 一律 `4004.200 benefit not found` 拒答。官方客户端
            // 「登录用一下就好了」正是这条在起作用 —— 余额查询是每分钟自动跑的，
            // 把注册挂在这里，新账号在一次查询内自愈，不必再借官方客户端渡一次。
            // 去抖：注册是写语义的幂等调用，没必要每分钟补一发（10 分钟一次足够）。
            let mut benefit = benefit;
            if matches!(&benefit, Ok(None)) && benefit_claim_due(account_id) {
                match balance::claim_benefit(models::DEFAULT_BENEFIT_GATEWAY_URL, &credential).await {
                    Ok(_) => {
                        benefit = balance::fetch_benefit_balance(
                            models::DEFAULT_BENEFIT_GATEWAY_URL,
                            &credential,
                        )
                        .await;
                    }
                    Err(error) => {
                        // 注册失败（网络/签名/上游）不该把「无档案」升级成整次失败：
                        // 下一轮余额查询会再试（去抖窗过后）。原样保留 absent 结果。
                        crate::server::logging::verbose(
                            "[CodeArts]",
                            &format!("benefit 自动注册失败（账号 {account_id}）：{}", error.message),
                        );
                    }
                }
            }
            // 两边都失败才算整次失败（一边失败不抹掉另一边）。
            // 注意「无福利」（`Ok(None)`）不是失败 —— 它是账号的正常状态
            // （福利按活动下发，Free 账号常常没有），见 balance.rs 的
            // `BENEFIT_ABSENT_CODE`。
            if statistics.is_err() && benefit.is_err() {
                let (first, second) = (statistics.unwrap_err(), benefit.unwrap_err());
                return Err(GatewayError::with_status(
                    first.status_code,
                    format!("{}（福利网关：{}）", first.message, second.message),
                ));
            }
            let benefit_value = benefit.as_ref().ok().and_then(|value| value.as_ref());
            let mut document = balance::usage_document(statistics.as_ref().ok(), benefit_value);
            match &benefit {
                // 上游明说没有该账号的福利数据：落一个**中性**标记而不是错误，
                // 界面上「没有福利」与「没读到」才分得开（前者不该报警告）。
                Ok(None) => {
                    document["benefitAbsent"] = Value::Bool(true);
                }
                Ok(Some(_)) => {}
                Err(error) => {
                    document["benefitError"] = Value::String(error.message.clone());
                }
            }
            if let Err(error) = &statistics {
                document["statisticsError"] = Value::String(error.message.clone());
            }
            Ok(document)
        })
    }

    /// 刷新要按账号取凭据（CodeArts 没有环境变量旁路：临时凭据必须配着
    /// oauth_context 才成立）。
    fn refresh_uses_account(&self) -> bool {
        true
    }

    /// 网页登录：授权页 + 本机 loopback 回调（`oauth::begin_login` 与
    /// `core::login::codearts`）。
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 返回 `(授权地址, state)`，`state` 就是这一轮的 `ticket_id`。
    ///
    /// 待办条目（PKCE verifier + DPoP 私钥）留在 `oauth` 的进程级表里 —— 换码时
    /// 只有那里有这两样，而它们**不能**进任务表（任务表会被 `/wait` 序列化给界面）。
    ///
    /// 收尾**不实现 trait 的 `exchange_login_code`**，写在 `core::login::codearts`：
    /// 那家的 portal 可能只回带一个 `code`、不带任何配对信息，收尾要「逐个候选试
    /// verifier」并把待办**取走**，而 trait 那条按 state 精确取一次 —— 两条路都留着
    /// 会互相把对方的待办取空（先到的赢、后到的 404）。所以入口只有一个。
    fn build_login_url(&self) -> Option<(String, String)> {
        let (url, pending) = oauth::begin_login(
            chat::DEFAULT_PLUGIN_NAME,
            chat::DEFAULT_PLUGIN_VERSION,
            chat::DEFAULT_LANGUAGE,
        )
        .ok()?;
        Some((url, pending.ticket_id))
    }

    /// 本家的目录**能远程刷**（三源发现），必须显式打开：
    ///
    /// 这个开关在 `refresh_models` 之前就被刷新循环判掉——漏了它，接口实现了
    /// 也永远不会被调用，界面还会反过来报"该提供商使用固定模型清单"。
    /// （沙箱端到端实测抓到的：加上这条之前 `/v1/models` 里一个 codearts 模型都没有。）
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 目录刷新：三源发现 → 落进程内缓存（M4 的 `models` 就是为这一步准备的）。
    ///
    /// 目录本身只读，但**取凭据要走 `ensure_fresh`**：本家的临时凭据只活一小时
    /// 左右，用户很可能在闲置很久之后才第一次点「获取模型」——直接用盘上那份会
    /// 撞 401，而 401 在目录这条路上只会显示成"刷新失败"，看不出是凭据临期。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = super::adapter::ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            // 空 id = 队首可用账号（自动路径的默认）；非空 = 用户在弹窗里点名的
            // 那条。**「没有账号」不是失败**：自动路径每轮（启动、定时、被动拉
            // /v1/models）都会走到这里，把它算成失败会给一家用户还没配置的家记上
            // 一次失败、进而触发「间隔 × 2^(n-1)」的冷却退避（模型刷新间隔默认
            // 60 分钟 ⇒ 一次失败就是 1 小时）。实测踩到过：用户 10:36 登录添加
            // 完 CodeArts，点「获取模型」却被「请求处于冷却期，请在 2977 秒后
            // 重试」挡住 —— 那次「失败」发生在账号存在之前（10:27）。
            // 与 accio / qoder 逐字同口径：自动路径 `unchanged()`（没刷，
            // 不是失败），点名取不到才 `failed()`。
            if store.codearts_account_record(account_id).is_none() {
                crate::server::logging::verbose("[Models]", "CodeArts 模型目录刷新跳过：尚未添加 CodeArts 账号");
                if account_id.trim().is_empty() {
                    return super::adapter::ModelRefreshOutcome::unchanged();
                }
                return super::adapter::ModelRefreshOutcome::failed("指定的 CodeArts 账号不存在或不可用，请重新选择");
            };
            let credential = match proxy_and_fresh(store, account_id).await {
                Ok(credential) => credential,
                Err(error) => return super::adapter::ModelRefreshOutcome::failed(error.message),
            };
            let endpoints = models::CatalogEndpoints {
                base_url: models::DEFAULT_BASE_URL,
                benefit_gateway_url: Some(models::DEFAULT_BENEFIT_GATEWAY_URL),
                plugin_version: chat::DEFAULT_PLUGIN_VERSION,
                language: chat::DEFAULT_LANGUAGE,
            };
            let catalog = models::discover(&endpoints, &credential).await;
            if catalog.models.is_empty() {
                return super::adapter::ModelRefreshOutcome::failed(
                    catalog.warnings.first().cloned().unwrap_or_else(|| "该账号没有返回任何可用模型".to_string()),
                );
            }
            let count = catalog.models.len();
            models::store_catalog(catalog);
            super::adapter::ModelRefreshOutcome::refreshed(count)
        })
    }
}

/// 账号自己配的代理，口径与 qoder 的 `auth::account_proxy` 一致：
/// **解析失败就报错**，不静默降级成直连（直连出不了内网机器的话，用户只会看到
/// "上游超时"，而不是"你配的代理解析不了"）。
fn record_proxy(
    store: &AccountStore,
    account_id: &str,
) -> Result<Option<crate::server::core::proxies::ResolvedProxy>, GatewayError> {
    let record = store
        .codearts_account_record(account_id)
        .ok_or_else(|| GatewayError::with_status(503, "没有可用的 CodeArts 账号：请在账号页添加并启用账号"))?;
    use crate::server::core::proxies::ProxyResolution;
    match crate::server::core::proxies::resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 没有编排层代理解析的那两条钩子（面板点刷新 / 目录刷新）用的组合。
async fn proxy_and_fresh(store: &AccountStore, account_id: &str) -> Result<credentials::Credential, GatewayError> {
    let proxy = record_proxy(store, account_id)?;
    refresh::ensure_fresh(store, account_id, false, proxy.as_ref()).await
}

/// benefit 自动注册（claim）的**进程级去抖**：距上次尝试不足窗口期返回 false。
///
/// 自动余额查询默认每分钟跑一次，「无档案」的账号若不加去抖就会每分钟被补发一次
/// claim —— 调用幂等但没必要（官方客户端也就是每次启动一发）。10 分钟取的是
/// 「自愈要快，但也不在网关故障时把节奏打满」的中间值；表只增不减，量级就是
/// 本机的 CodeArts 账号数，不值得做清理。返回 true 表示这次该发（并记下时刻）。
fn benefit_claim_due(account_id: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    const WINDOW: Duration = Duration::from_secs(600);
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match map.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.get(account_id).is_some_and(|at| at.elapsed() < WINDOW) {
        return false;
    }
    guard.insert(account_id.to_string(), Instant::now());
    true
}

/// 把上游原文拼成统一文案的一截（空则不拼）。
fn raw_part(raw: &str) -> String {
    if raw.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", raw.trim())
    }
}

/// 认得出「福利日池打满」的文案 ⇒ 冷却到**北京时间下一个零点**；认不出 ⇒ None
/// （由存储层落 10 分钟兜底）。
///
/// ── 为什么不是 10 分钟 ──────────────────────────────────────
/// `InferHub.4291.200 / insufficient quota` 是本家**按日**的 token 池打满时那一帧
/// （实测 2026-10-02：pri=2 的 `daily_tokens_used` 到 10,028,800 / 上限 10,000,000
/// 的那一刻起，每条请求都只回这一帧）。上游不告诉我们什么时候重置，但它整套日口径
/// 都是 UTC+8 零点（见 `welfare::today` 的说明），所以零点是有据可依的那个答案。
/// 10 分钟兜底的实际后果也量过：池子当天不会再回来，于是直到次日零点之前，这个已耗尽
/// 的账号大约每 10 分钟被白撞一次（一次往返 + 一条 403 痕迹 + 一次顺延噪声）。
///
/// 形状与 Trae 的计划限额同一套（`trae::forward::next_local_midnight_ms`），差别只在
/// 本家用**固定 UTC+8** 而不是机器时区：NAS 容器跑在 UTC，跟机器时区走会早 8 小时
/// 放开（那一小时上游还在扣当天的账），又会在北京时间的白天里挡掉本来能用的额度。
///
/// 只认这两串原文，不认状态码：429 是限流（分钟级，10 分钟兜底正是它要的），
/// 认不出额度的 403 是内容闸门/权限/签名那一类（本来就不该罚账号，见
/// [`CodeArtsAdapter::classify_conversation_error`]）。把非日池的失败冷却一整天，
/// 爆炸半径是这一家的全部福利模型 —— 宁缺勿滥。
fn daily_pool_reset_at(now_ms: i64, message: &str) -> Option<i64> {
    let text = message.to_lowercase();
    if text.contains("insufficient quota") || text.contains("4291.200") {
        Some(welfare::next_day_boundary_ms(now_ms))
    } else {
        None
    }
}

#[cfg(test)]
mod store_hooks {
    //! 三个存储侧钩子的接线验收。
    //!
    //! 为什么单独钉这一组：`ProviderAdapter` 的 `refresh_access_token` **有默认实现**
    //! （转发到 `ensure_access_token`），忘了覆盖不会编译失败，只会让面板上那条
    //! 「刷新凭据」变成一个报成功的空动作 —— 沙箱端到端实测就是这样：接口回
    //! `success` + `refreshedId`，库里的 AK 一个字符都没变。这种失败没有任何报错，
    //! 所以用一条「不联网、但默认实现与覆盖实现结果不同」的断言把它钉住。
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::server::core::account_store::AccountStore;
    use crate::server::core::providers::codearts::credentials::{OAuthContext, PkcePair, Credential};
    use crate::server::db::Db;

    use super::{benefit_cooldown_group, daily_pool_reset_at, models};
    use super::{CODEARTS_ADAPTER, CodeArtsAdapter, ProviderAdapter};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn store() -> AccountStore {
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("codearts-hooks-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AccountStore::with_db(Some(Db::open(&dir.join("agent2api.db")).expect("临时库应当能建起来")))
    }

    /// `minutes` 之后到期（负数即已过期）。
    fn expiry(minutes: i64) -> String {
        (chrono::Utc::now() + chrono::TimeDelta::minutes(minutes)).to_rfc3339()
    }

    fn credential(ak: &str, user: &str, minutes: i64, refreshable: bool) -> Credential {
        Credential {
            access_key_id: ak.to_string(),
            secret_access_key: "SK".to_string(),
            security_token: "sts".to_string(),
            expires_at: expiry(minutes),
            domain_id: "dom".to_string(),
            user_id: user.to_string(),
            refresh_token: if refreshable { "jwt".to_string() } else { String::new() },
            oauth_context: refreshable.then(|| OAuthContext {
                pkce_pair: PkcePair { code_verifier: "verifier".to_string(), ..Default::default() },
                ..Default::default()
            }),
            ..Credential::default()
        }
    }

    #[test]
    fn expiring_follows_the_lead_window_and_a_usable_refresh_chain() {
        let store = store();
        // 三条各代表一种「后台维护该不该动手」：远端到期 / 只剩五分钟 / 临期但刷不了
        store.add_codearts_account(&credential("AK_FAR", "far", 120, true), None, "manual").unwrap();
        store.add_codearts_account(&credential("AK_NEAR", "near", 5, true), None, "manual").unwrap();
        store.add_codearts_account(&credential("AK_BARE", "bare", 5, false), None, "manual").unwrap();
        let id_of = |user: &str| {
            store
                .list_accounts()["accounts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .find(|a| a["provider"] == serde_json::json!(crate::server::core::account_store::codearts_accounts::CODEARTS_PROVIDER_ID) && a["userId"] == serde_json::json!(user))
                .unwrap_or_else(|| panic!("没有 userId={user} 这条账号"))
                ["id"]
            .as_str()
            .unwrap_or_default()
            .to_string()
        };
        // 「刷不了」不该变成「每轮都失败」：没有续期链的账号直接排除在维护之外
        assert!(CODEARTS_ADAPTER.supports_refresh(), "写回通路已就绪，这条必须开着，否则临期凭据没人管");
        assert!(!CODEARTS_ADAPTER.credentials_expiring(&store, &id_of("far")), "离到期两小时不该动手");
        assert!(CODEARTS_ADAPTER.credentials_expiring(&store, &id_of("near")), "只剩十五分钟窗口内就该动手");
        assert!(!CODEARTS_ADAPTER.credentials_expiring(&store, &id_of("bare")), "没有续期链就别让它进维护队列（每轮刷一次失败日志）");
    }

    #[tokio::test]
    async fn force_refresh_is_the_override_not_the_default_no_op() {
        let store = store();
        store.add_codearts_account(&credential("AK_BARE", "bare", 120, false), None, "manual").unwrap();
        let id = store.codearts_account_record("").expect("账号应当可读")["id"].as_str().unwrap().to_string();

        // 非强制那条：没临期就原样返回，**不发网络请求**（临时库里是假串，真刷必炸）
        let token = CODEARTS_ADAPTER.ensure_access_token(&store, &id).await.expect("未临期应当直接用盘上这份");
        assert_eq!("AK_BARE", token);

        // 强制那条必须真的走 store 路径：续期链不全时**本地就报错**，
        // 而 trait 的默认实现只会把当前 AK 原样回给你（= 什么都没做却报成功）
        let error = match CODEARTS_ADAPTER.refresh_access_token(&store, &id).await {
            Ok(_) => panic!("刷新落到了默认实现（空动作）：覆盖被删掉了？"),
            Err(error) => error,
        };
        assert_eq!(400, error.status_code, "缺续期链是用户可修的本地错误，不是上游错误");
        assert!(error.message.contains("refresh token"), "文案要点名缺什么：{}", error.message);
    }

    /// 会话式分类：429 与「认得出额度」的 403 交回 QuotaLimited（都要落冷却，
    /// 但**冷却长度不同**：认得出日池的给下一个北京零点，429 留给存储层兜底）；
    /// 认不出额度的 403（内容闸门/权限/签名）与 401/502 一律 Fatal —— 特别是
    /// 401：会话式路径没有「刷凭证后同账号重试」的编排，交 TokenExpired 会把
    /// 错误吞进一条根本不存在的重试链里。
    #[test]
    fn conversation_classification_marks_quota_only_for_403_and_429() {
        use crate::server::errors::GatewayError;
        use super::UpstreamErrorClass;
        let class = |status: i32, message: &str| {
            CODEARTS_ADAPTER
                .classify_conversation_error(&GatewayError::with_status(status, message))
        };
        // 流内折叠的额度信封：首包门按 insufficient quota 折的 403，消息必带原文。
        // 带上 `reset_at: Some(_)`：改前这里是 `None` ⇒ 存储层落 10 分钟兜底 ⇒
        // 日池当天不会回来，于是直到次日零点前每 10 分钟白撞一次这个账号。
        assert!(matches!(
            class(403, "上游报告 InferHub.4291.200：insufficient quota"),
            UpstreamErrorClass::QuotaLimited { status: 403, reset_at: Some(_), .. }
        ));
        // HTTP 403、诊断体里带额度原文 → 也认（同一判据，文案里有 quota 原文）
        assert!(matches!(
            class(403, "CodeArts 上游返回 HTTP 403：{error_msg: insufficient quota}"),
            UpstreamErrorClass::QuotaLimited { reset_at: Some(_), .. }
        ));
        // 认不出额度的 403 → 透传，不罚账号
        assert!(matches!(
            class(403, "CodeArts 上游返回 HTTP 403：permission denied"),
            UpstreamErrorClass::Fatal { status: 403, .. }
        ));
        // 429 无歧义（限流）→ 记账，但**不给零点**：它是分钟级的，罚一整天会把
        // 一个只是暂时繁忙的账号打死
        assert!(matches!(
            class(429, "too many requests"),
            UpstreamErrorClass::QuotaLimited { status: 429, reset_at: None, .. }
        ));
        assert!(matches!(class(401, "x"), UpstreamErrorClass::Fatal { status: 401, .. }));
        assert!(matches!(class(502, "x"), UpstreamErrorClass::Fatal { status: 502, .. }));
        // 其它家的默认实现必须还是 Fatal（CatPaw 的行为逐字不变）
        use crate::server::core::providers::catpaw::adapter::CATPAW_ADAPTER;
        assert!(matches!(
            CATPAW_ADAPTER.classify_conversation_error(&GatewayError::with_status(403, "x")),
            UpstreamErrorClass::Fatal { status: 403, .. }
        ));
    }

    /// 本地合成目录（不碰全局缓存）：三份真实 fixture 走与生产同一合并链，
    /// 得到 4 个福利模型的目录。
    fn local_catalog() -> models::Catalog {
        const AGENT_DETAIL: &str = include_str!("catalog_fixtures/agent-detail.json");
        const BUILTIN: &str = include_str!("catalog_fixtures/builtin.json");
        const BENEFIT_CONFIG: &str = include_str!("catalog_fixtures/benefit-gateway-config.json");
        let agent = models::parse_agent_detail(AGENT_DETAIL, "en-us").unwrap();
        let builtin = models::parse_builtin(BUILTIN).unwrap();
        let merged = models::merge_agent_and_builtin(agent, builtin);
        models::Catalog {
            models: models::merge_benefit(merged, models::parse_benefit(BENEFIT_CONFIG).unwrap()),
            warnings: Vec::new(),
        }
    }

    /// 福利池是账号级日额度：本次失败的模型是福利源时，整组福利模型一起进
    /// 冷却名单；非福利模型（或不认识的名字）只记自己。吃**本地合成**目录
    /// —— models.rs 的测试会并发改写全局目录缓存（实测全量跑必红、单跑绿），
    /// 所以纯逻辑测试不读缓存；生产读取路径（`quota_cooldown_models` 的
    /// `cached_catalog` 分支）只做一层委托，本文件不重复测它。
    #[test]
    fn benefit_quota_marks_the_whole_benefit_group() {
        let catalog = local_catalog();
        let group = benefit_cooldown_group(&catalog, "glm-5.3-flash");
        assert_eq!(4, group.len(), "整组福利模型：{group:?}");
        for expected in ["deepseek-v4-flash-0731", "glm-5.3-flash", "deepseek-v4-pro-0813", "deepseek-v4.1-flash"] {
            assert!(group.iter().any(|name| name == expected), "缺 {expected}：{group:?}");
        }
        // 大小写变体也算福利：整组照记，并补上本次发送名原文的键（判定侧对
        // 解析不出映射的请求读的是请求原文键，见 benefit_cooldown_group 注释）
        let variant = benefit_cooldown_group(&catalog, "GLM-5.3-Flash");
        assert_eq!(5, variant.len(), "整组 4 条 + 变体原文 1 条：{variant:?}");
        assert!(variant.iter().any(|name| name == "GLM-5.3-Flash"), "缺变体原文键：{variant:?}");
        // 非福利源（agent/builtin）与不在目录里的名字只记自己 —— 不波及别人
        assert_eq!(vec!["GLM-5.2".to_string()], benefit_cooldown_group(&catalog, "GLM-5.2"));
        assert_eq!(vec!["ghost".to_string()], benefit_cooldown_group(&catalog, "ghost"));
    }

    /// 端到端语义（不联网、不发请求）：福利模型撞限额后按**整组**落冷却，
    /// 之后换**另一个福利模型名**来请求也能命中冷却记录。改造前只记单模型
    /// 键，这正是「福利池空了还把请求打过去」的缺口。
    ///
    /// 刻意**不**走 `CooldownKeys`/全局 manifest 解析：models.rs 的测试会并发
    /// 改写 codearts 目录缓存，读侧解析在并发下不确定（实测全量跑必红、单跑
    /// 绿）。生产里键的同源性由 `routing::CooldownKeys` 的既有机制与默认启用
    /// 的模型保证；这里钉的是本次改动的语义 —— **写入侧的键集合**。
    #[test]
    fn whole_group_cooldown_blocks_a_different_benefit_model() {
        let catalog = local_catalog();

        let store = store();
        store.add_codearts_account(&credential("AK_BARE", "bare", 120, false), None, "manual").unwrap();
        let id = store.codearts_account_record("").expect("账号应当可读")["id"].as_str().unwrap().to_string();

        // 与 provider_loop 会话式分支同一动作：按整组名单逐个记账，恢复时刻也走
        // 同一判据（认得出日池 ⇒ 下一个北京零点，见 `daily_pool_reset_at`）。
        let group = benefit_cooldown_group(&catalog, "glm-5.3-flash");
        let reset_at = daily_pool_reset_at(
            crate::server::logging::now_ms(),
            "上游报告 InferHub.4291.200：insufficient quota",
        )
        .expect("这条文案就该给出日池重置点");
        for name in &group {
            store.mark_rate_limited(
                &id,
                name,
                403,
                None,
                Some(reset_at as f64),
                "上游报告 InferHub.4291.200：insufficient quota",
            );
        }

        let now = crate::server::logging::now_ms();
        let limits = store.codearts_account_record("").unwrap()["rateLimits"].clone();
        let limits = limits.as_object().expect("rateLimits 应当是对象");
        // 别的福利模型名（本次没撞的那个）也必须有自己的冷却记录，且恢复时刻
        // 是日池重置点而不是 10 分钟兜底
        for expected in ["deepseek-v4-flash-0731", "glm-5.3-flash", "deepseek-v4-pro-0813", "deepseek-v4.1-flash"] {
            let entry = limits.get(expected).unwrap_or_else(|| panic!("缺整组键 {expected}：{:?}", limits.keys().collect::<Vec<_>>()));
            let reset = entry["resetAt"].as_f64().unwrap_or(0.0);
            assert!(reset > now as f64, "{expected} 的 resetAt 应当在未来，实际 {reset}");
            assert!(
                (reset - reset_at as f64).abs() <= 2.0,
                "{expected} 的 resetAt 应当是日池重置点 {reset_at}，实际 {reset}"
            );
        }
        // 对照组：非福利模型键没有被整组波及 —— 冷却只盖福利池，不把整号打死
        assert!(!limits.contains_key("GLM-5.2"), "非福利模型不该被整组标记");
    }

    /// 冷却长度判据本身（纯函数，不吃时钟）：认得出日池的两串原文给下一个北京零点，
    /// 其它文案（429 的 `too many requests`、内容闸门的 `permission denied`）给 None
    /// 让存储层兜 10 分钟。
    ///
    /// 固定 `now` 才谈得上「零点」：拿 `logging::now_ms()` 进出的话，这条用例在
    /// 北京零点前后各跑一次会给出不同的期望值，等于没钉住任何事。
    #[test]
    fn only_the_daily_pool_envelope_cools_until_midnight() {
        // 2026-09-26T07:00Z = 北京 15:00，离下一个零点 9 小时
        let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
            .expect("合法日期")
            .and_hms_opt(7, 0, 0)
            .expect("合法时刻")
            .and_utc()
            .timestamp_millis();
        let want = now + 9 * 60 * 60 * 1000;
        for text in [
            "上游报告 InferHub.4291.200：insufficient quota",
            "CodeArts 上游返回 HTTP 403：insufficient quota",
            "INFERHUB.4291.200 only",
        ] {
            assert_eq!(
                Some(want),
                daily_pool_reset_at(now, text),
                "认得出日池的文案要冷却到下一个北京零点：{text}"
            );
        }
        for text in [
            "too many requests",
            "上游返回 403: permission denied",
            "上游返回 403: content policy blocked",
            "",
        ] {
            assert_eq!(None, daily_pool_reset_at(now, text), "认不出日池就不许冷却一整天：{text}");
        }
    }
}
