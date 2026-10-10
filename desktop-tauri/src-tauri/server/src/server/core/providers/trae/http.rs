//! Trae 的出站 HTTP 小工具（登录与续期两条链共用）。
//!
//! 单独一个文件的原因：这两条链都要"依次试多个 host，第一个拿到令牌的算数"
//! （参考实现的 `exchangeTokenCandidates` / `buildAPIURLs`），
//! 那个循环如果每个调用点各写一遍，就会出现"一处超时 5s、另一处 30s"的漂移。

use std::time::Duration;

use serde_json::Value;

use crate::server::core::egress;
use crate::server::errors::GatewayError;
use crate::server::core::proxies::ResolvedProxy;

/// 一次 POST 的结果。
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// 上游响应体的读取上限。
///
/// 参考实现是 1 MiB（`io.LimitReader`）。留上限不是为了省内存，而是
/// `www.*` 那类 host 会回一整个 HTML 页面：没有截断的话，一次"试下一个候选"
/// 会把整页 HTML 拼进错误消息里，日志与客户端都会被淹掉。
const MAX_BODY_BYTES: usize = 1 << 20;

/// `POST` 一个 JSON 体。网络层错误归一成 502（"上游没答"），
/// HTTP 状态本身**不**在这里判成错误 —— 多候选轮询要靠状态码决定换不换 host。
pub async fn post_json(
    url: &str,
    body: &Value,
    headers: &[(&str, String)],
    timeout: Duration,
    proxy: Option<&ResolvedProxy>,
) -> Result<Reply, GatewayError> {
    let mut request = egress::client_for(proxy)
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(timeout);
    for (name, value) in headers {
        request = request.header(*name, value.clone());
    }
    let response = request
        .json(body)
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, format!("Trae 请求发不出去：{}", egress::describe_error_detail(&error))))?;
    let status = response.status().as_u16();
    let text = read_limited(response).await;
    Ok(Reply { status, body: text })
}

/// `GET` 一个 URL（目录对照那一发用的就是它）。
///
/// 与 `post_json` 同一套口径：网络层错误归一成 502，HTTP 状态**不**在这里判成
/// 错误（调用方要按状态码决定怎么说这句话），响应体同样只读 `MAX_BODY_BYTES`。
/// 单独写而不是复用 POST：GET 带 JSON 体在 upstream 那边是被拒的形态，
/// 而 remote 目录（`/api/remote/v1/models?functions=…`）本来就是查询串驱动的。
pub async fn get_json(
    url: &str,
    headers: &[(&str, String)],
    timeout: Duration,
    proxy: Option<&ResolvedProxy>,
) -> Result<Reply, GatewayError> {
    let mut request = egress::client_for(proxy)
        .get(url)
        .header("Accept", "application/json")
        .timeout(timeout);
    for (name, value) in headers {
        request = request.header(*name, value.clone());
    }
    let response = request
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, format!("Trae 请求发不出去：{}", egress::describe_error_detail(&error))))?;
    let status = response.status().as_u16();
    let text = read_limited(response).await;
    Ok(Reply { status, body: text })
}

/// 读响应体，最多 `MAX_BODY_BYTES`。
async fn read_limited(response: reqwest::Response) -> String {
    let limit = MAX_BODY_BYTES;
    match response.bytes().await {
        Ok(bytes) if bytes.len() <= limit => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(bytes) => String::from_utf8_lossy(&bytes[..limit]).into_owned(),
        Err(_) => String::new(),
    }
}

/// 把多个候选 host 的失败拼成一条**能看懂**的报错。
///
/// 只给最后一个错是不够的：登录换证失败最常见的原因是某个 host 在 TLB 边缘
/// 直接 404，用户看到"404 page not found"会以为链接错了，而实际是候选顺序问题。
pub fn describe_candidates(urls: &[String], errors: &[String]) -> String {
    let mut message = format!("Trae 换证失败（试过 {} 个地址）", urls.len());
    for (index, error) in errors.iter().take(3).enumerate() {
        message.push_str(&format!("；{} {}", index + 1, error));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_failure_summary_stays_readable() {
        let urls: Vec<String> = (0..6).map(|index| format!("https://h{index}/x")).collect();
        let errors: Vec<String> = (0..6).map(|index| format!("h{index} => HTTP 404")).collect();
        let text = describe_candidates(&urls, &errors);
        assert!(text.contains("试过 6 个地址"), "{text}");
        assert!(text.contains("h0") && text.contains("h2") && !text.contains("h5"), "只留前 3 条：{text}");
    }
}
