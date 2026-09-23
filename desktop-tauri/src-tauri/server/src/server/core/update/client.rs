//! GitHub 出网：候选出口解析 + 请求头 + 带出口重试的 fetch。
//!
//! 对照 Node 版 workbuddy-update.mjs 的 `resolveEgressCandidates` /
//! `fetchWithEgress` / `githubHeaders` 三块。
//!
//! ─── 候选顺序（直连优先，Clash 兜底）────────────────────────
//! Node 是 `[{label:'直连', proxy:null}]`，若 `resolveAccountProxy({source:'clash',
//! listenerUid: CLASH_MIXED_UID})` 成功且没 error、有 port，就追加一个候选。
//! Rust 侧走 `core::proxies::resolve_account_proxy` 的同一条路（account_store 里
//! 组会话用的也是它），因此「Clash 未安装/未启用混合端口时只有一个候选」
//! 的行为与 Node 完全一致。
//!
//! ─── 只有网络层失败才换出口 ─────────────────────────────────
//! HTTP 状态码类错误（404 仓库没 Release、403 限额）直接交给调用方判断，
//! 否则 404 会被误当成「需要换代理」而白试一轮。这里 `fetch_with_egress`
//! 只在 `send()` 返回 Err（DNS/TLS/连接/超时）时切下一个候选。

use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::egress;
use crate::server::core::proxies::{resolve_account_proxy, ResolvedProxy, CLASH_MIXED_UID};
use crate::server::logging;

use super::version::UpdateError;

/// GitHub API 要求的头部集合（对应 Node 版 githubHeaders）。
///
/// token 解析顺序：`WORKBUDDY_GITHUB_TOKEN` > `GITHUB_TOKEN`（都去空白、
/// 空串当未配置）。配了 token 则限额更高（匿名 60 次/小时，带 token 5000）。
pub fn github_headers() -> Vec<(String, String)> {
    let mut headers = vec![
        ("Accept".to_string(), "application/vnd.github+json".to_string()),
        ("User-Agent".to_string(), "workbuddy-local-proxy".to_string()),
        ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
    ];
    let token = ["WORKBUDDY_GITHUB_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    if let Some(token) = token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}

/// 出网候选：`(标签, 代理配置)`；`proxy` 为 None 表示直连。
///
/// 对应 Node 版 resolveEgressCandidates —— Clash 配置不可读时只有一个候选，
/// 行为与纯直连一致（这里不吞掉整条链路：探测失败只在 verbose 里留痕）。
fn resolve_egress_candidates() -> Vec<(String, Option<ResolvedProxy>)> {
    let mut candidates = vec![("直连".to_string(), None)];
    let resolution = resolve_account_proxy(Some(&json!({
        "source": "clash",
        "listenerUid": CLASH_MIXED_UID,
    })));
    match resolution {
        Some(resolution) => {
            if let Some(proxy) = resolution.resolved() {
                if proxy.port.is_some() {
                    let label = if proxy.label.is_empty() {
                        "Clash 混合端口".to_string()
                    } else {
                        proxy.label.clone()
                    };
                    candidates.push((label, Some(proxy.clone())));
                }
            } else if let Some(error) = resolution.error() {
                // Node 在这条分支上静默（try/catch 吞掉），这里留 verbose 便于排障
                logging::verbose("[Update]", &format!("Clash 出口不可用（{error}），仅用直连"));
            }
        }
        None => {}
    }
    candidates
}

/// 依次尝试各出口，返回第一个**连得上**的响应（HTTP 状态码交给调用方判断）。
///
/// 全部出口都在网络层失败时返回 502「无法连接 GitHub：<错误链>」
/// （文案照抄 Node 的 `无法连接 GitHub：${proxyErrorDetail(lastError)}`）。
pub async fn fetch_with_egress(
    url: &str,
    headers: &[(String, String)],
    timeout_ms: Option<u64>,
) -> Result<reqwest::Response, UpdateError> {
    let candidates = resolve_egress_candidates();
    let multiple = candidates.len() > 1;
    let mut last_error: Option<reqwest::Error> = None;
    for (label, proxy) in &candidates {
        let client = egress::client_for(proxy.as_ref());
        let mut builder = client.get(url);
        // 请求级总超时只给 API 探测用（响应是小 JSON）。reqwest 0.12 的
        // `.timeout()` 覆盖到 body 读完为止 —— 下载传 None，安装包几十上百 MB
        // 不可能 30 秒内下完；卡死场景由 egress 客户端的 read_timeout 兜底
        if let Some(timeout_ms) = timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        }
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        match builder.send().await {
            Ok(response) => {
                if multiple && proxy.is_some() {
                    logging::verbose("[Update]", &format!("经 {label} 访问成功"));
                }
                return Ok(response);
            }
            Err(error) => {
                logging::verbose(
                    "[Update]",
                    &format!("经 {label} 访问失败: {}", egress::describe_error_detail(&error)),
                );
                last_error = Some(error);
            }
        }
    }
    let detail = match &last_error {
        Some(error) => egress::describe_error_detail(error),
        None => "未知错误".to_string(),
    };
    Err(UpdateError::new(format!("无法连接 GitHub：{detail}")))
}

/// 出网候选的展示形态（供日志/排障；不含凭证）。
/// 对应 Node 版 workbuddy-update.mjs 内部的 `resolveEgressCandidates()` 结果形态
/// （Node 侧为内部细节，这里作为排障口暴露，保留）。
#[allow(dead_code)]
pub fn describe_candidates() -> Value {
    let list: Vec<Value> = resolve_egress_candidates()
        .into_iter()
        .map(|(label, proxy)| {
            json!({
                "label": label,
                "proxy": egress::describe_public(proxy.as_ref()),
            })
        })
        .collect();
    Value::Array(list)
}
