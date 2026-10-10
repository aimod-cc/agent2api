//! **流内拒绝**的门控：HTTP 200 也可能是一次拒绝。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 有些上游（实测 OfficeAce / 华为 AgentArts 网关）把「内容审核拒绝」包装成一次
//! 正常答复：`HTTP 200`、SSE 正常收尾，但正文是一句固定的拒绝话术（如
//! 「作为一个人工智能语言模型，我还没学习如何回答这个问题…」），并且终帧的
//! `choices[].finish_reason` 是 `content_filter`。
//!
//! 只看 HTTP 状态，这就成了一次**假成功**：编排层不会换家，客户端拿到的
//! 「答案」是一句套话 —— 用户以为网关坏了（真实现象：ZCode 的 agentic 请求打到
//! 本家条条如此）。
//!
//! ── 判据与代价都在 provider 那边声明，本模块只负责「等」 ──────
//! 「哪些 `finish_reason` 算拒绝」是**上游侧知识**（与 81004/11-128 同类），
//! 由适配器的 `stream_refusal_finishes` 回答；本模块只做一件事：把响应流
//! **拉到能判定为止**，把预读到的字节、剩余流、以及看到的 `finish_reason`
//! 一起交回。**没有 provider 声明时本模块一个字节都不碰**（调用点直接短路，
//! 见 `provider_loop::gate_stream_refusal`）。
//!
//! ── 两种终止条件（都必要）────────────────────────────────────
//!   1. **看到 `finish_reason`**：拒绝话术很短，普通答案的终帧也迟早会到 ——
//!      到了就能立刻判定，不必再等。
//!   2. **缓冲到上限**：真答案可能很长，不能无限缓冲（否则等于把流式改成
//!      非流式）。超过上限即放行，把这家的字节按正常答案下发。
//! 拒绝话术只有几百字节，远在上限之内 ⇒ 这种「先缓冲再放行」对**拒绝**是必中的，
//! 对**真答案**只是把开头最多一节字节推迟到同时下发（上限内的延迟）。
//!
//! ── 为什么上限取 32 KiB ─────────────────────────────────────
//! 依据是实测：上游的拒绝正文约 50 token（≈300 字节），任何真实答复都会迅速
//! 超过它 —— 上限越小，真答案的「首字节延迟」越小；上限越大，能兜住的
//! 「拒绝前有长前言」的形态越多。32 KiB 是两者之间的折中：拒绝对它绰绰有余，
//! 真答案又只被推迟最多 32 KiB 的产出时间。
//!
//! ── 读不出来就当没看见（fail-open）────────────────────────────
//! 传输错误 / 空闲超时 / 流结束都没看到 `finish_reason` 时返回 `None`，调用点
//! 按「正常答案」放行 —— 门控判错的代价必须是「漏拦一次拒绝」，绝不能是
//! 「把一个正常答案判死」（那会把用户真能用的家挡在门外，比漏拦更坏）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::time::Duration;

use axum::http::HeaderMap;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;

use crate::server::config;
use crate::server::core::upstream::stall;

/// 门控在「看到 `finish_reason`」或「缓冲区达到上限」时结束（见模块头）。
const HOLD_CAP_BYTES: usize = 32 * 1024;

/// 一次预读的产物。
pub struct HeldHead {
    /// 上游 HTTP 状态码（预读前取，之后 `Response` 已被消费）
    pub status: u16,
    /// 上游响应头（同上；调试采集要它）
    pub headers: HeaderMap,
    /// 预读到的字节（**原样**，一个字节不丢）
    pub buffered: Vec<u8>,
    /// 剩余的上游字节流（预读之后的那部分）
    pub rest: BoxStream<'static, Result<Bytes, std::io::Error>>,
    /// 预读窗口内看到的 `finish_reason`（`None` = 没看到 / 读不出来）
    pub finish_reason: Option<String>,
}

/// `finish_reason` 是否在本家声明的「拒答」集合里（大小写不敏感）。
///
/// 集合来自适配器的 `stream_refusal_finishes`；空集恒为 `false`（默认不认）。
pub fn is_refusal(finishes: &[&str], finish: &str) -> bool {
    finishes
        .iter()
        .any(|marker| marker.eq_ignore_ascii_case(finish))
}

impl HeldHead {
    /// 把「预读到的字节 + 剩余流」拼成一条**完整**的下游字节流。
    ///
    /// 预读的字节原样在前（一个字节不丢），剩余流在后 —— 调用方拿到它就与
    /// 从未预读过视同上游流。`capture` 非空时逐片旁路给调试采集器（含预读
    /// 那一片）：预读发生在 `ForwardStream` 之前，采集得在这里补上，否则调试
    /// 模式会漏掉这一段的原始报文。
    pub fn into_stream(
        self,
        capture: Option<std::sync::Arc<crate::server::core::debug_traffic::TrafficCapture>>,
    ) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
        let head = (!self.buffered.is_empty()).then(|| Bytes::from(self.buffered));
        if let (Some(bytes), Some(capture)) = (head.as_ref(), capture.as_ref()) {
            capture.push(bytes);
        }
        let rest = self.rest.map(move |item| {
            if let (Ok(bytes), Some(capture)) = (&item, &capture) {
                capture.push(bytes);
            }
            item
        });
        futures::stream::iter(head.into_iter().map(Ok)).chain(rest).boxed()
    }
}

/// 从一段（可能不完整的）SSE 文本里找**第一个**非空 `choices[].finish_reason`。
///
/// 只认**成对换行结束**的帧（`\n\n` 分块）：尾部没结束的那行留给下一次读 ——
/// 否则会把半截 JSON 当成一帧解析失败，误判成「没看到 finish_reason」。
///
/// 大小写保留原值（判定由适配器按大小写不敏感比较）；`DONE` 哨兵与非 JSON 行跳过。
pub fn scan_finish_reason(text: &str) -> Option<String> {
    for block in text.split("\n\n") {
        for line in block.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" || !data.starts_with('{') {
                continue;
            }
            let Ok(frame) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let Some(choices) = frame.get("choices").and_then(Value::as_array) else {
                continue;
            };
            for choice in choices {
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    if !reason.is_empty() {
                        return Some(reason.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 把上游流拉到「能判定是否被拒」为止（见模块头）。
///
/// 入参是**未消费**的 `reqwest::Response`（调用点在本函数之后不再需要它）：
/// 状态码与响应头在这里取出，字节流被包上进空闲守卫（与 `ForwardStream::new`
/// 同一件事，只是提前到这里 —— 预读期间上游卡住也不能把请求挂死）。
pub async fn hold_head(response: reqwest::Response) -> HeldHead {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    // 与 `ForwardStream::new` / 聚合器同款：reqwest 错误就地描述成文案折进 io::Error
    let inner = response.bytes_stream().map(|item| {
        item.map_err(|error| {
            std::io::Error::other(crate::server::core::egress::describe_error_detail(&error))
        })
    });
    let mut source = stall::idle_guard(
        Box::pin(inner),
        Duration::from_millis(config::timeout_settings().stream_idle_ms()),
    );
    let mut buffered: Vec<u8> = Vec::new();
    let mut text = String::new();
    let mut finish_reason: Option<String> = None;
    loop {
        // 先看已缓冲的字节里有没有终帧（一次读可能带来好几帧）
        if let Some(reason) = scan_finish_reason(&text) {
            finish_reason = Some(reason);
            break;
        }
        if buffered.len() >= HOLD_CAP_BYTES {
            break;
        }
        match source.next().await {
            // 流结束 / 传输错误 / 空闲超时：拿不到结论 ⇒ 放行（fail-open，见模块头）
            None | Some(Err(_)) => break,
            Some(Ok(chunk)) => {
                buffered.extend_from_slice(&chunk);
                text.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
    }
    HeldHead {
        status,
        headers,
        buffered,
        rest: source,
        finish_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scans_the_first_non_empty_finish_reason() {
        // 普通流帧 finish_reason 为 null = 不算终结；终帧给出 stop
        let text = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        assert_eq!(Some("stop".to_string()), scan_finish_reason(text));
        // content_filter 原样取出（判定在适配器侧）
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"content_filter\"}]}\n\n";
        assert_eq!(Some("content_filter".to_string()), scan_finish_reason(text));
    }

    #[test]
    fn ignores_incomplete_trailing_frame_and_noise() {
        // 尾部没结束的那行不算一帧（半截 JSON 不能解析），这里因此看不到 finish
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"content_fil";
        assert_eq!(None, scan_finish_reason(text));
        // 只有 null / 空串 / 非 data 行 / [DONE]：都不算终结
        let text = concat!(
            ": keepalive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"\"}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"b\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
            "data: 不是一个 json\n\n",
        );
        assert_eq!(None, scan_finish_reason(text));
        // 多个 choice、终结在第二个时也能取到
        let text = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":null},{\"index\":1,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n";
        assert_eq!(Some("length".to_string()), scan_finish_reason(text));
    }

    #[test]
    fn scan_tolerates_real_chunk_shape_with_usage_tail() {
        // 收尾 usage 帧（choices 空数组）不该打断扫描
        let text = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":\"content_filter\"}],\"usage\":null}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":711006,\"completion_tokens\":47}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(Some("content_filter".to_string()), scan_finish_reason(text));
        // 顺带钉住 usage 帧的形状不会误报
        assert_eq!(
            None,
            scan_finish_reason("data: {\"choices\":[],\"usage\":{\"completion_tokens\":1}}\n\n")
        );
        let _ = json!({});
    }
}
