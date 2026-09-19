//! AutoClaw 的 **token 刷新**（Agent2API 二期 T-c1；移植来源
//! `autoclaw-upstream-client.mjs` 的刷新链）。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 凭证的「读」（来源解析、解密、mtime 缓存）与「续」（换发 token、单飞、降级
//! 重试）是两件事：前者是同步的文件/解密逻辑，后者是异步的网络编排。
//! 拆开还有一个直接原因 —— 单文件行数约定（`credentials.rs` 加上这段会超过
//! 800 行）。拆分口径与 `raccoon/credentials.rs` + `raccoon/jwt.rs` 一致：
//! 「来源与解析」留在 `credentials.rs`，「刷新与单飞」搬到本文件。
//!
//! ── 刷新链（对照源实现，逐条）────────────────────────────────
//!   1. **单飞**：同一「账号 id + 来源 + refreshToken 指纹」并发只发一次请求，
//!      后到者等它的结果（源实现 `inflightRefresh` + `refreshKey` 的等价物；
//!      表由共享原语 `refresh_flight` 提供 —— 只保存进行中的刷新，本轮结束即
//!      释放，leader 的 future 被 drop 时 RAII 清理并唤醒等待者）。
//!   2. `POST {userapi}/userapi/v1/refresh`，头 = `signed_auth_headers`
//!      （`X-Auth-Sign = MD5(appId&ts&appKey)`），body
//!      `{refresh_token, source_id: "autoclaw", device_id?}`。
//!   3. 若 `code == 400002`（刷新签名校验失败）→ 降级
//!      `POST {userapi}/userapi/v1/agent-refresh` 再试一次。
//!   4. 校验 `code == 0 && data.access_token` 非空；否则报错（401/502 的
//!      状态码口径照抄源实现）。
//!   5. `refresh_token` 用响应里的新值，缺失时沿用旧的。
//!   6. 过期时间按**新 token** 的 JWT `exp` 重算（解不出时保留旧值）。
//!
//! ── 只读不回写（架构文档 §10.2 的取舍）───────────────────────
//! 刷新结果**不写回任何文件**，只进 `credentials.rs` 的进程级凭证缓存
//! （文件 mtime 一变即失效）。理由见 `credentials.rs` 模块头：
//! auth.json 是 safeStorage 加密格式，回写要重新加密并有写坏用户登录态的风险；
//! 且 AutoClaw 服务端会**轮换** refresh_token，网关回写会与桌面端自己的刷新
//! 互相顶掉（源实现把桌面态标成 `persistRefresh: false` 正是这个原因）。
//! 代价是「长时间运行的网关可能先用掉一次 refresh_token」；收益是绝不损坏
//! 用户的登录态文件。
//!
//! ── Send 硬约束 ────────────────────────────────────────────
//! 本文件所有 async fn 都必须产出 `Send` 的 future（转发跑在多线程运行时上）。
//! 因此 `MutexGuard` 一律在同步函数内取用并释放（单飞表与凭证缓存的锁都在
//! 共享原语 / `credentials.rs` 的同步函数里），**await 一定在锁外**。

use md5::{Digest as _, Md5};
use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::refresh_flight;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::crypto;
use super::credentials::{
    credentials_from_claims, js_text, number_value, store_cached_if_current, AutoClawCredentials,
    CredentialOrigin, REFRESH_FALLBACK_CODE, REQUEST_TIMEOUT_MS,
};

/// 签名用的 appId / appKey（源实现 `AUTH_APP_ID` / `AUTH_APP_KEY`）。
///
/// 与 AutoClaw 桌面端客户端内嵌值一致：`X-Auth-Sign = MD5(appId&ts&appKey)`。
/// 这是**客户端指纹**而不是我们的密钥，所以硬编码在这里是正确的做法
/// （换成可配置只会让签名对不上）。
const AUTH_APP_ID: &str = "100003";
const AUTH_APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";

// ─── 单飞表 ─────────────────────────────────────────────────

/// 进程级单飞表：**只保存进行中的刷新**（共享 `refresh_flight` 原语）。
///
/// key = `{账号 id}:{来源}:{refreshToken 指纹}`：
///   - 账号 id：不同账号绝不互相复用；
///   - 来源：桌面端来源的 id 固定是 `desktop-auth`（同一个 id 也可能是账号记录
///     来源，取决于调用方传的是哪份快照），两条来源的凭证与回写目标都不同，
///     混用会让一个来源的结果被当成另一个来源的；
///   - refreshToken 指纹：轮换/重导入后不会误取到另一轮刷新的结果。
///     指纹是截断 SHA-256，**完整 token 不进 key、不打印**。
fn inflight_table() -> &'static refresh_flight::Table<AutoClawCredentials> {
    static TABLE: std::sync::OnceLock<refresh_flight::Table<AutoClawCredentials>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(refresh_flight::Table::new)
}

/// 刷新 key（构成与理由见 `inflight_table` 的说明）。
///
/// 本地文件来源再带上 `cache_key`（含来源路径指纹与文件 mtime）：桌面端重新登录
/// 后即使 refresh_token 恰好没轮换，新登录态的刷新也不该与旧登录态的一轮合并。
fn refresh_key(credentials: &AutoClawCredentials) -> String {
    let source = match credentials.origin {
        CredentialOrigin::DesktopAuthFile => "auth",
        CredentialOrigin::GatewayConfig => "gw",
        CredentialOrigin::AccountStore => "store",
        CredentialOrigin::Environment => "env",
    };
    let mut key = format!(
        "{}:{source}:{}",
        credentials.id,
        refresh_flight::fingerprint(&credentials.refresh_token)
    );
    if let Some(cache_key) = credentials.cache_key.as_deref() {
        key.push(':');
        key.push_str(&refresh_flight::fingerprint(cache_key));
    }
    key
}

// ─── 对外：刷新 ─────────────────────────────────────────────

/// 刷新凭证（单飞；**本期不回写任何文件**，见模块头）。
///
/// `force = false` 时只在**临期**才刷新（`ensure_access_token` 的语义）；
/// `force = true` 时无条件刷新（401 之后的重试 —— 被拒的 token 可能时间上还
/// 很新，只看临期窗口会拿回同一个坏 token）。
///
/// 刷新成功后：本地文件来源（auth.json / openclaw.json）把结果写进**进程级
/// 凭证缓存**（`credentials.rs` 的 `store_cached_if_current`，只在该缓存仍是
/// 刷新前那份凭证时写入，文件 mtime 一变即失效）；账号来源的结果只返回给
/// 调用方，要不要落 accounts.json 由适配器决定。
///
/// ── 生命周期（本次修复）────────────────────────────────────
///   - 单飞表里没有「已完成」的记录：结果只交给**已加入这一轮的等待者**，
///     表项在本轮结束时释放 → `force = true` 的下一轮一定发起新请求，
///     失败也不会被长期缓存；
///   - leader 的 future 被 drop（客户端断开）时，RAII 守卫释放占位并唤醒等待者，
///     等待者拿到 409「已取消」，随后可以重试。
///
/// **持锁边界**：单飞表只在同步函数里取用并释放，网络请求与等待都在锁外。
pub async fn refresh(
    credentials: &AutoClawCredentials,
    force: bool,
) -> Result<AutoClawCredentials, GatewayError> {
    if !force && !credentials.is_expiring() {
        return Ok(credentials.clone());
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            match credentials.origin {
                CredentialOrigin::GatewayConfig => {
                    "AutoClaw 网关配置里的 token 没有 refreshToken，无法刷新，请在桌面端重新登录"
                }
                CredentialOrigin::DesktopAuthFile => {
                    "AutoClaw 桌面端登录态没有 refreshToken，请在桌面端重新登录"
                }
                _ => "该账号没有 refreshToken，无法刷新，请重新添加账号",
            },
        ));
    }
    let key = refresh_key(credentials);
    let guard = match inflight_table().join(&key) {
        refresh_flight::Join::Leader(guard) => guard,
        // 已加入同一轮：结果由 leader 负责落地（含缓存写回），这里只等它的结果
        refresh_flight::Join::Waiter(waiter) => return waiter.wait().await,
    };
    let result = match call_refresh_api_with_fallback(credentials).await {
        Ok(next) => {
            // 本地文件来源：刷新结果进凭证缓存（**不落盘**，见模块头）。
            // 比较-再写：登录态已被更换时**不覆盖**，并改用当前登录态 ——
            // 旧结果不得用于后续请求（桌面端刚重新登录，它的凭证才是有效的）
            if let (true, Some(cache_key)) = (next.origin.is_local_file(), next.cache_key.as_ref())
            {
                if !store_cached_if_current(cache_key, credentials, &next) {
                    logging::verbose(
                        "[AutoClaw]",
                        &format!(
                            "账号 {} 的刷新结果未写入缓存（登录态已被更换）",
                            next.id
                        ),
                    );
                    // 重新取当前登录态：桌面端刚重新登录时，它的凭证才是有效的。
                    // 读不到（文件被删/损坏）时**不重建、也不拿刷新结果顶替** ——
                    // 明确报错，让用户去桌面端重新登录
                    super::credentials::local_credentials().map_err(|error| {
                        GatewayError::with_status(
                            401,
                            format!(
                                "登录态在刷新期间被更换，且无法读取最新登录态：{}",
                                error.message
                            ),
                        )
                    })
                } else {
                    Ok(next)
                }
            } else {
                Ok(next)
            }
        }
        Err(error) => Err(error),
    };
    // 结果交给已加入这一轮的等待者，随后释放表项（先释放表项、再唤醒）
    guard.finish(result.clone());
    result
}

/// 取可用凭证：临期时主动刷新（源实现 `currentCredentials` 的语义）。
///
/// 适配器的 `ensure_access_token` 直接用它；`force = false` 表示「只在临期时刷」。
///
/// ── 回写判定为什么在调用方 ─────────────────────────────────
/// 本函数只负责「该刷就刷」，不碰账号文件：**没发生刷新时不写盘**（返回的凭证与
/// 传入的是同一份快照，token 逐字相同），真的刷新了才由适配器走比较-再写。
pub async fn ensure_fresh(
    credentials: &AutoClawCredentials,
) -> Result<AutoClawCredentials, GatewayError> {
    if !credentials.is_expiring() || !credentials.can_refresh() {
        return Ok(credentials.clone());
    }
    logging::verbose(
        "[AutoClaw]",
        &format!("账号 {} token 临期，主动刷新", credentials.id),
    );
    refresh(credentials, false).await
}

// ─── 刷新请求 ───────────────────────────────────────────────

/// 刷新请求的签名头（源实现 `signedAuthHeaders`）：
/// `X-Auth-Sign = MD5("{appId}&{ts}&{appKey}")`，`X-Auth-TimeStamp` 是**秒**。
///
/// 这套头是 AutoClaw 桌面端客户端的固定指纹（appId/appKey 内嵌在客户端里），
/// 用于让刷新接口认出来自官方客户端；缺任一个都会被判签名失败（code 400002，
/// 于是降级 agent-refresh）。
///
/// ── 为什么可见性是 `pub(super)`：余额查询也要这一份（余额功能移植）──
/// `autoclaw/balance.rs` 的积分与订阅请求走的是源实现里同一对
/// `signedAuthHeaders`（`userapiGetFor` / `userapiPostFor` 都用它）。签名保持
/// **一份**实现是正确的做法 —— 复制进 balance.rs 会让 appId/appKey、时间戳单位
/// （这里是**秒**，很容易和毫秒搞混）与品牌头在两条链路上各自演进，而签名一旦
/// 分叉就是稳定 400002 且只在余额页暴露。因此放宽可见性而不是复制。
pub(super) fn signed_auth_headers(token: &str) -> Vec<(String, String)> {
    let timestamp = (logging::now_ms() / 1000).max(0).to_string();
    let mut hasher = Md5::new();
    hasher.update(format!("{AUTH_APP_ID}&{timestamp}&{AUTH_APP_KEY}").as_bytes());
    // `{:x}` = 小写十六进制、无填充 —— 与 Node `digest('hex')` 逐字节一致
    let sign = format!("{:x}", hasher.finalize());
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "*/*".to_string()),
        // 品牌头：与桌面端一致的客户端指纹（源实现 `brandHeaders`）
        ("X-Product".to_string(), "autoclaw".to_string()),
        ("X-Client-Type".to_string(), "pc".to_string()),
        ("X-Harness-Type".to_string(), "zcode".to_string()),
        (
            "X-Tm".to_string(),
            if cfg!(windows) { "win" } else { "linux" }.to_string(),
        ),
        ("X-Lang".to_string(), "zh-CN".to_string()),
        ("X-Channel".to_string(), "official".to_string()),
        ("X-Auth-Appid".to_string(), AUTH_APP_ID.to_string()),
        ("X-Auth-TimeStamp".to_string(), timestamp),
        ("X-Auth-Sign".to_string(), sign),
        (
            "X-Trace-Id".to_string(),
            crate::server::core::upstream::request::new_request_id(),
        ),
    ];
    if !token.is_empty() {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}

/// 调一次刷新接口（源实现 `callRefreshApi`）：`POST {userapi}{path}`，
/// body `{refresh_token, source_id: "autoclaw", device_id?}`。
///
/// 鉴权接口**不走账号级代理**：账号代理是给转发（流式长请求）准备的出口，
/// 鉴权域与 LLM 域是两个站点（与 raccoon 刷新同一取舍）。
async fn call_refresh_api(
    credentials: &AutoClawCredentials,
    path: &str,
) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", super::credentials::userapi_base_url());
    let headers = signed_auth_headers(&credentials.token);
    let mut body = json!({
        "refresh_token": credentials.refresh_token,
        "source_id": "autoclaw",
    });
    if !credentials.device_id.is_empty() {
        if let Some(map) = body.as_object_mut() {
            map.insert(
                "device_id".to_string(),
                Value::String(credentials.device_id.clone()),
            );
        }
    }
    let response = send_raw(
        "POST",
        &url,
        Some(&body),
        &headers,
        None,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| GatewayError::with_status(502, format!("AutoClaw 刷新请求失败: {error}")))?;
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok {
        return Err(GatewayError::with_status(
            if response.status == 401 { 401 } else { 502 },
            format!(
                "AutoClaw 刷新接口 {path} 失败（HTTP {}）",
                response.status
            ),
        ));
    }
    Ok(payload)
}

/// 刷新入口：按 400002 降级重试（源实现 `refreshCredentials` 的调用链）。
async fn call_refresh_api_with_fallback(
    credentials: &AutoClawCredentials,
) -> Result<AutoClawCredentials, GatewayError> {
    let mut payload = call_refresh_api(credentials, "/userapi/v1/refresh").await?;
    if payload.get("code").and_then(number_value).map(|code| code as i64)
        == Some(REFRESH_FALLBACK_CODE)
    {
        logging::verbose(
            "[AutoClaw]",
            "refresh 签名校验失败 (400002)，降级 agent-refresh",
        );
        payload = call_refresh_api(credentials, "/userapi/v1/agent-refresh").await?;
    }
    let code = payload.get("code").and_then(number_value);
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let access_token = data
        .get("access_token")
        .map(js_text)
        .map(|text| crypto::strip_bearer(&text))
        .unwrap_or_default();
    if code != Some(0.0) || access_token.is_empty() {
        // 状态码口径照抄源实现：`code === 41e4 || code === 401` → 401，其余 502。
        // **注意 `41e4` 是 JS 的科学计数法 = 410000**（不是 41000000），
        // 写成数字字面量时极易看错位数，这里显式写成 410_000 并注明来源。
        let unauthorized = code == Some(401.0) || code == Some(410_000.0);
        return Err(GatewayError::with_status(
            if unauthorized { 401 } else { 502 },
            format!(
                "AutoClaw token 刷新失败 code={} msg={}",
                payload
                    .get("code")
                    .map(js_text)
                    .unwrap_or_else(|| "null".to_string()),
                payload
                    .get("msg")
                    .or_else(|| payload.get("message"))
                    .map(js_text)
                    .unwrap_or_else(|| "服务器未返回新 token".to_string()),
            ),
        ));
    }
    let next_refresh = data
        .get("refresh_token")
        .map(js_text)
        .map(|text| crypto::strip_bearer(&text))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| credentials.refresh_token.clone());
    let claims = crypto::decode_jwt_claims(&access_token);
    let mut next = credentials_from_claims(
        credentials.id.clone(),
        access_token,
        next_refresh,
        credentials.device_id.clone(),
        claims.as_ref(),
        credentials.origin,
        credentials.cache_key.clone(),
    );
    // JWT 解不出 exp 时保留旧的过期时间（与 raccoon 的
    // `or(credentials.expires_at)` 同一取舍）
    if next.expires_at.is_none() {
        next.expires_at = credentials.expires_at;
    }
    if next.user_id.is_empty() {
        next.user_id = credentials.user_id.clone();
    }
    logging::verbose(
        "[AutoClaw]",
        &format!("账号 {} 的 token 已刷新（仅内存生效）", next.id),
    );
    Ok(next)
}
