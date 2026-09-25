//! ZCode 的**登录**：服务端中介的 CLI 轮询流程。
//!
//! ── 与其余各家的最大不同：没有本地回调 ──────────────────────
//! AutoClaw / Accio / Qoder 的网页登录都在本机起一个回调口（或等设备授权码），
//! ZCode 不是 —— 参考实现（`Acankao/zcode-api` 的 `src/auth/oauth.ts`）把这条
//! 链路记为 `startOAuthWithPolling`，并专门写了一句：
//!
//! > No local callback server exists on this path. Building a direct
//! > chat.z.ai/bigmodel authorize URL with a localhost redirect_uri is
//! > rejected upstream (`Redirect URI not registered for this client`).
//!
//! 也就是说**不能**照抄 AutoClaw 那套「登记回调端口」的做法（`callback_server.rs`
//! 里那四个端口是 z.ai 给 AutoClaw 客户端登记的，与 ZCode 无关）。
//!
//! ── 四步 ────────────────────────────────────────────────────
//! ```text
//!   1. 生成 poll token（32 随机字节的十六进制）—— init 与 poll 都带它做 Bearer
//!   2. POST {zcode}/api/v1/oauth/cli/init   body {"provider": "bigmodel"|"zai"}
//!      → {flow_id, poll_token, authorize_url, expires_at, poll_interval_sec}
//!   3. 把 authorize_url 补上一个「中转页」参数后交给用户打开 ——
//!      浏览器**不会**回到本机，授权在服务端记完就结束了
//!   4. GET {zcode}/api/v1/oauth/cli/poll/{flow_id}（每 poll_interval_sec 一次）
//!      直到 status 变成 ready → {token, user, zai|bigmodel:{access_token}}
//! ```
//!
//! ── 第 3 步那个「中转页」参数是必须的（别省）─────────────────
//! `authorize_url` 是服务端给的，但它直接打开时浏览器最后会跳到一个本机
//! 收不到的 `zcode://` 协议上，**授权结果不会被服务端记下**，于是 poll 永远
//! 停在 pending。客户端补的是自己那个中转页：
//!
//! ```text
//!   {zcode}/app/oauth/login?redirect=zcode://oauth/callback&app_version={ver}
//! ```
//!
//! 参数名**两地不同**：国际版（zai）是 `redirect_uri`，国内版（bigmodel）是
//! `redirect`。写错一个，对应那一地的登录会静默停在 pending 直到超时 ——
//! 这是本文件最容易出错、且症状最不直观的一处。
//!
//! ── 轮询的错误语义（照抄参考实现，别自己发明）───────────────
//!   - 4xx（**408 / 429 除外**）、信封 `code != 0`、认不出的 status → **致命**，
//!     立刻结束登录并报错；
//!   - 5xx / 网络错误 / 200 但响应体畸形 → **当 pending 处理**（继续轮询），
//!     因为这三类都是「这次没问成」而不是「问成了但被拒」。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;

use super::claim::{app_version, urlencode};
use super::credentials::{new_device_mid, ZcodeCredentials};
use super::region::Region;

/// 单次请求超时（与领取链路同一口径；登录请求更不该挂住）
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 轮询间隔的兜底值（上游没给 `poll_interval_sec` 时用）
const DEFAULT_POLL_INTERVAL_SEC: i64 = 3;

/// 一条已发起的登录流程（`start` 的返回值）。
///
/// 调用方持有它、按 [`Self::poll_interval`] 的节奏调 [`Self::poll`]，
/// 直到拿到凭证或超时。`expires_at` 是**上游给的**截止时间（unix 秒），
/// 比本地超时更权威 —— 到点后上游会直接作废这个 flow，继续轮询只是白跑。
pub struct CliLogin {
    /// 本流程属于哪个地区（决定 `provider` 取值与结果解析分支）
    region: Region,
    /// 上游流程 id（轮询路径的一部分）
    flow_id: String,
    /// 交给用户打开的授权地址（**已补好中转页参数**）
    auth_url: String,
    /// 上游给的截止时间（unix 秒）
    expires_at: i64,
    /// 上游建议的轮询间隔（秒）
    poll_interval_sec: i64,
    /// 本次流程的 poll token（init 与 poll 共用的 Bearer）
    poll_token: String,
    /// 设备标识（随凭证落盘，见 `credentials.rs` 的模块头）
    device_mid: Option<String>,
}

impl CliLogin {
    /// 发起登录：拿到 `flow_id` 与**可直接交给用户打开**的授权地址。
    ///
    /// 登录时机还没有任何账号上下文，因此**直连**（与其余各家的登录链路
    /// 同一取舍：账号级代理是给转发长请求准备的出口）。
    pub async fn start(region: Region) -> Result<Self, GatewayError> {
        let poll_token = random_hex_32();
        let url = format!("{}/api/v1/oauth/cli/init", region.zcode_origin());
        let headers: Vec<(String, String)> = vec![
            ("authorization".to_string(), format!("Bearer {poll_token}")),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let body = json!({ "provider": region.upstream_provider() });
        let response = send_raw(
            "POST",
            &url,
            Some(&body),
            &headers,
            None,
            Some(REQUEST_TIMEOUT_MS),
        )
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "ZCode 登录发起超时")
            } else {
                GatewayError::with_status(502, format!("ZCode 登录发起失败: {error}"))
            }
        })?;

        let payload = response.payload.ok_or_else(|| {
            GatewayError::with_status(502, "ZCode 登录发起：上游响应不是 JSON")
        })?;
        if let Some(message) = envelope_error(&payload) {
            return Err(GatewayError::with_status(502, message));
        }
        let data = payload
            .get("data")
            .ok_or_else(|| GatewayError::with_status(502, "ZCode 登录发起：响应缺少 data"))?;
        let flow_id = required_str(data, "flow_id")?;
        let authorize_url = required_str(data, "authorize_url")?;
        // expires_at / poll_interval_sec 缺失时给兜底值而不是报错：
        // 它们是「调度建议」，不是凭证本身 —— 为它们失败会让一次本可成功的
        // 登录白跑（与参考实现「缺这两个字段就抛」不同，这里更宽容，
        // 因为本地已有 5 分钟总超时兜底，不会因此无限轮询）。
        let expires_at = data.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
        let poll_interval_sec = data
            .get("poll_interval_sec")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SEC);

        Ok(Self {
            region,
            auth_url: apply_interstitial(region, &authorize_url),
            flow_id,
            expires_at,
            poll_interval_sec,
            poll_token,
            device_mid: new_device_mid(),
        })
    }

    /// 交给用户打开的授权地址（已补中转页参数）
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    /// 本流程的关联标识（`flow_id`）。
    ///
    /// 登录任务表按它登记/注销任务（与其余各家的 CSRF state 同一个位置，
    /// 见 `core::login::LoginTasks::register`）。
    pub fn state(&self) -> &str {
        &self.flow_id
    }

    /// 本流程的地区（日志与前端回显用）
    pub fn region(&self) -> Region {
        self.region
    }

    /// 轮询间隔
    pub fn poll_interval(&self) -> std::time::Duration {
        // 下限 1 秒：上游理论上给 0 或负数的话，紧密轮询会打上游
        // （参考实现同样取 `max(1000ms, poll_interval_sec)`）
        let secs = self.poll_interval_sec.max(1);
        std::time::Duration::from_secs(secs as u64)
    }

    /// 上游给的截止时间（unix 秒；`0` 表示上游没给）
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// 轮询一次。
    ///
    /// 返回 `Ok(None)` = 还没授权完，继续等；`Ok(Some(_))` = 拿到凭证；
    /// `Err(_)` = **致命**失败（语义见模块头），调用方应结束登录并报错。
    pub async fn poll(&self) -> Result<Option<ZcodeCredentials>, GatewayError> {
        let url = format!(
            "{}/api/v1/oauth/cli/poll/{}",
            self.region.zcode_origin(),
            self.flow_id
        );
        let headers: Vec<(String, String)> = vec![(
            "authorization".to_string(),
            format!("Bearer {}", self.poll_token),
        )];
        let response = match send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS))
            .await
        {
            Ok(response) => response,
            // 网络错误当 pending（模块头第 2 条）：一次没问成不是「被拒」
            Err(_) => return Ok(None),
        };

        // 5xx 当 pending；4xx 里 408（超时）/ 429（限流）也当 pending ——
        // 它们是「这次没问成」，其余 4xx 是「问成了但被拒」
        if response.status >= 500
            || response.status == 408
            || response.status == 429
        {
            return Ok(None);
        }
        let payload = match response.payload {
            Some(payload) => payload,
            // 200 但响应体畸形 → pending
            None => return Ok(None),
        };
        if let Some(message) = envelope_error(&payload) {
            // 信封 code != 0 → 致命
            return Err(GatewayError::with_status(502, message));
        }
        if response.status >= 400 {
            return Err(GatewayError::with_status(
                502,
                format!("ZCode 登录轮询被拒（HTTP {}）", response.status),
            ));
        }
        let data = match payload.get("data") {
            Some(data) => data,
            None => return Ok(None),
        };
        let status = data.get("status").and_then(Value::as_str).unwrap_or("");
        match status {
            "ready" => {
                let credentials = self.parse_ready(data)?;
                Ok(Some(credentials))
            }
            "failed" => Err(GatewayError::with_status(
                401,
                "ZCode 授权被拒绝或已失效，请重新发起登录",
            )),
            // pending / 其它认不出的 status：参考实现把「认不出的 status」
            // 记为致命，这里从宽当 pending —— 理由是上游将来可能新增中间态
            // （例如「已授权待激活」），把中间态当失败会让登录在升级后的
            // 上游上整体不可用；而本地有 5 分钟总超时，不会无限等。
            _ => Ok(None),
        }
    }

    /// `status: "ready"` 的响应体 → 凭证。
    ///
    /// 三个字段来自三个位置（参考实现 `CliPollData`）：
    ///   - `token` → 套餐 JWT（顶层，不是 `data.jwt`）
    ///   - `user.user_id` → 上游用户 id
    ///   - `data[provider].access_token` → 推理访问令牌（`zai` 或 `bigmodel`，
    ///     键名是**上游的 provider 名**，不是我们的 id）
    fn parse_ready(&self, data: &Value) -> Result<ZcodeCredentials, GatewayError> {
        let key = self.region.upstream_provider();
        let access_token = data
            .get(key)
            .and_then(|value| value.get("access_token"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                GatewayError::with_status(
                    502,
                    format!("ZCode 登录响应缺少 {key}.access_token"),
                )
            })?;
        let jwt = data
            .get("token")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let user_id = data
            .get("user")
            .and_then(|value| value.get("user_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        Ok(ZcodeCredentials {
            region: self.region,
            access_token,
            jwt,
            user_id,
            device_mid: self.device_mid.clone().unwrap_or_default(),
        })
    }
}

/// 把中转页参数补进服务端给的 `authorize_url`（见模块头的第 3 步）。
///
/// ── 语义是「set」不是「append」────────────────────────────
/// 参考实现走 `URL.searchParams.set(param, …)`：**同名参数替换**。上游给的
/// 两个地址里本来就各带一个同名参数（国际版 `redirect_uri=…/cli/callback/zai`、
/// 国内版 `redirect=…/cli/callback/bigmodel`），所以这一步必然是替换。裸拼
/// `&{param}=…` 会留下两个同名参数，取哪个由授权端点自己决定 —— 取到原值
/// 就等于没补中转页，而症状是 poll 一直停在 pending（见模块头那段警告）。
///
/// ── 中转页地址要**整体**编码，不能裸拼 ──────────────────────
/// 参考实现把中转页塞进 `searchParams.set` 时，整串会被 form-urlencoded
/// 再编一次（`&` → `%26`、`%` → `%25`）。少这一次编码有两处后果：
/// `&app_version=…` 会被**外层**查询吃掉 —— 它变成 `authorize_url` 上的
/// 独立参数（中转页收不到版本号），而授权端点也多收到一个自己没定义的
/// 参数。因此这里对内层已编码的 `zcode%3A%2F%2F…` 再编一次，得到与参考
/// 逐字节相同的 `%253A%252F%252F…`：中转页解一次仍是 `zcode://oauth/callback`。
///
/// 这里用字符串拼接而不是 `url` crate：该 crate 不在依赖里，而这一步只需要
/// 「切出查询、替换一段、拼回去」——`authorize_url` 由上游给出、结构可控，
/// 不值得为它引一个依赖。
fn apply_interstitial(region: Region, authorize_url: &str) -> String {
    let param = match region {
        // 国际版（zai）
        Region::Intl => "redirect_uri",
        // 国内版（bigmodel）
        Region::Cn => "redirect",
    };
    let target = urlencode(&format!(
        "{}/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version={}",
        region.zcode_origin(),
        app_version()
    ));
    let prefix = format!("{param}=");
    let replacement = format!("{prefix}{target}");
    // 片段（`#…`）先摘下来：它不参与查询，替换完原样拼回
    let (head, fragment) = match authorize_url.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (authorize_url, None),
    };
    let (path, query) = match head.split_once('?') {
        Some((path, query)) => (path, query),
        None => (head, ""),
    };
    let mut replaced = false;
    let mut segments: Vec<String> = Vec::new();
    for segment in query.split('&').filter(|value| !value.is_empty()) {
        if segment.starts_with(&prefix) {
            // 重名参数只留一个（与 `set` 一致：它会把同名的全部删掉再放一个）
            if !replaced {
                segments.push(replacement.clone());
                replaced = true;
            }
            continue;
        }
        segments.push(segment.to_string());
    }
    if !replaced {
        segments.push(replacement);
    }
    let mut out = String::from(path);
    out.push('?');
    out.push_str(&segments.join("&"));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// 信封错误：`code` 存在且不为 0 时返回一句人话，否则 None。
///
/// 上游的 `{code, data, msg}` 信封里 `code == 0` 才是成功（参考实现的
/// `requestZcodeEnvelope` 同此判定）。
fn envelope_error(payload: &Value) -> Option<String> {
    let code = payload.get("code").and_then(Value::as_i64)?;
    if code == 0 {
        return None;
    }
    let message = payload
        .get("msg")
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("上游拒绝");
    Some(format!("ZCode 登录失败（{code}）：{message}"))
}

/// 取一个必填字符串字段（缺失或空白 → 报错）
fn required_str(data: &Value, key: &str) -> Result<String, GatewayError> {
    data.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            GatewayError::with_status(502, format!("ZCode 登录响应缺少 {key}"))
        })
}

/// 32 随机字节的十六进制串（poll token；参考实现 `randomBytes(32).toString("hex")`）。
///
/// 取不到随机数时回落到时间戳派生的串：poll token 只是本次流程的一个
/// 关联标识（上游不拿它做鉴权，只用来把 init 与 poll 归到同一个流程），
/// 因此可预测性的风险是「别人猜到你的 flow_id」—— 那也读不到别人的令牌
/// （poll 还要带这个 token，而 flow 本身有 5 分钟寿命）。宁可弱一点，
/// 也不在这里 panic（release 是 `panic=abort`）。
fn random_hex_32() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        let fallback = format!(
            "{:x}{:x}{:x}{:x}",
            std::process::id(),
            crate::server::logging::now_ms(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.subsec_nanos())
                .unwrap_or(0),
            crate::server::logging::now_ms() >> 3
        );
        return format!("{fallback:0>64}").chars().take(64).collect();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
