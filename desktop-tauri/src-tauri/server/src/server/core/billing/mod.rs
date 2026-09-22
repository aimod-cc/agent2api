//! 计费/签到/运营活动客户端（对照 Node 版 src/workbuddy-billing.mjs 全量移植）。
//!
//! ── 端点（全部不带 prefixPath，直接挂 {endpoint}/v2/...）──────
//!   POST /v2/billing/meter/checkin-activity-status      签到活动状态
//!   POST /v2/billing/meter/daily-checkin                领取每日签到积分
//!   POST /v2/billing/meter/get-user-resource            个人积分包
//!   POST /v2/billing/meter/get-enterprise-user-usage    企业额度
//!   POST /v2/billing/meter/get-dosage-notify            用量提示
//!   GET  /v2/activity/workbuddy/banner                  运营 banner（需客户端白名单头）
//!   GET  /v2/activity/ambassador/status                 大使状态
//!
//! ── 出网 ──────────────────────────────────────────────────
//! 一律走 `core::egress`（按 `session.proxy` 挂出口），与转发保持同一出口；
//! session 由账号存储派生（`store.get_session_by_id`），它已经带上了
//! `proxy` / `proxyError` —— 解析失败时 `proxy` 为 null，这里自然回退直连。
//!
//! ── 请求超时 ──────────────────────────────────────────────
//! 20 秒（Node 版 REQUEST_TIMEOUT_MS），落在单请求总超时上，对应 Node 的
//! `signal: AbortSignal.timeout(20_000)`。
//!
//! ── 统一响应包 ────────────────────────────────────────────
//! `{ code, data, msg?, requestId? }`，code === 0 为成功。签到类接口允许
//! 非 0 code（重复领取要读 msg），因此 `expect_code_ok=false` 时原样返回。
//!
//! ── 文件分工（单文件行数约定）────────────────────────────
//!   mod.rs       服务句柄、错误类型、`call_billing` 请求核心、签到（本文件）
//!   checkin.rs   账号级签到（目标集合 + 串行执行，向定时签到暴露同一条路径）
//!   usage.rs     积分/额度查询与简报（get-user-resource / enterprise-usage）
//!   activity.rs  运营 banner / 大使状态 / 签到组合动作
//!   request.rs   端点表、调用选项、请求头、JS 语义工具
//!   commodity.rs 积分包商品码与套餐分类

pub mod checkin;
mod activity;
pub mod commodity;
mod request;
mod usage;

use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::auth::AuthService;
use crate::server::core::auth_http::{send_raw, ApiResponse};
use crate::server::core::endpoints::{normalize_endpoint, resolve_edition, RESPONSE_CODE_OK};
use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

// 请求构造素材对同层子模块可见（`super::X`），对外仍是模块私有
use request::{
    build_headers, whitelist_headers, BillingCall, BillingSpec, CallOptions,
    BILLING_CHECKIN_STATUS, BILLING_DAILY_CHECKIN,
};

/// 计费接口单请求总超时（对照 Node 版 REQUEST_TIMEOUT_MS）
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 计费/活动接口的错误（对应 Node 版 WorkBuddyBillingError）。
///
/// `status_code` 会原样变成 HTTP 状态码（504 超时 / 401 登录态过期 /
/// 上游 HTTP 状态码），所以它必须是 Option 之外的一个确定值 ——
/// Node 版构造函数默认 502。
#[derive(Clone, Debug)]
pub struct BillingError {
    pub message: String,
    pub status_code: i32,
    pub upstream_code: Option<i64>,
}

impl BillingError {
    pub fn new(message: impl Into<String>, status_code: i32) -> Self {
        Self { message: message.into(), status_code, upstream_code: None }
    }

    pub fn with_code(
        message: impl Into<String>,
        status_code: i32,
        upstream_code: Option<i64>,
    ) -> Self {
        Self { message: message.into(), status_code, upstream_code }
    }

    /// 转成统一的网关错误。
    ///
    /// 计费路由的失败 body 走 OpenAI 风格（`/api/usage` 那几条在 server.mjs
    /// 的最外层大 try 里 → errorPayload）—— 由路由层决定用哪个信封，
    /// 这里只提供转换。
    pub fn to_gateway_error(&self) -> crate::server::errors::GatewayError {
        let mut error = crate::server::errors::GatewayError::with_status(
            self.status_code,
            self.message.clone(),
        );
        if let Some(code) = self.upstream_code {
            error = error.upstream_code(code);
        }
        error
    }
}

impl std::fmt::Display for BillingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// 计费客户端句柄：持有鉴权服务（取会话用）。
///
/// Node 版是 `createWorkBuddyBilling({ auth, ... })`，靠 `auth.getCurrentSession()`
/// 拿默认会话。按账号查询（/api/accounts/usage、/api/accounts/checkin）需要
/// **指定账号**的会话，那条路径由 `billing::checkin` 的入参显式传入 store，
/// 因此这里不再持有账号存储句柄。
#[derive(Clone)]
pub struct BillingService {
    auth: AuthService,
}

impl BillingService {
    pub fn new(auth: AuthService) -> Self {
        Self { auth }
    }

    /// 计费接口语言（Accept-Language）：zh | en。
    ///
    /// Node 版 `resolveAcceptLanguage`：认 zh*/en* 前缀，其余（含空）一律 zh。
    /// 传入的 locale 来自 config.locale（默认 zh-CN）。不缓存（config 可热改），
    /// 每次读内存快照。
    fn accept_language(locale: Option<&str>) -> String {
        let Some(locale) = locale else {
            return "zh".to_string();
        };
        let normalized = locale.to_lowercase();
        if normalized.starts_with("zh") {
            return "zh".to_string();
        }
        if normalized.starts_with("en") {
            return "en".to_string();
        }
        "zh".to_string()
    }

    /// 当前配置里的 locale（/api/usage 与 claim-and-report 用）
    fn current_locale() -> String {
        config::current().locale().to_string()
    }

    /// 取默认会话（当前账号）；没有可用登录态时报 401。
    ///
    /// 文案照抄 Node 版，但去掉了「node server.mjs --login」那半句 ——
    /// 壳内 Rust 版没有那个命令行入口，指向不存在的命令会误导用户。
    async fn require_session(&self) -> Result<Value, BillingError> {
        let session = self.auth.get_current_session().await.map_err(|error| {
            let status = error.http_status();
            BillingError::with_code(error.message, status, error.upstream_code)
        })?;
        match session {
            Some(session) if has_access_token(&session) => Ok(session),
            _ => Err(BillingError::new("当前没有可用登录态：请先在桌面端完成登录", 401)),
        }
    }

    /// 会话里的出口（`session.proxy` → ResolvedProxy）。
    ///
    /// 会话里的 proxy 由账号存储组装，形态与 ResolvedProxy 的 JSON 完全一致；
    /// 数据坏了（缺主机/端口非法）时按直连处理并记一条 warn ——
    /// 出网代理是可选配置，它的故障不该让整条链路失败。
    fn proxy_of(session: &Value) -> Option<ResolvedProxy> {
        match ResolvedProxy::from_json(session.get("proxy").unwrap_or(&Value::Null)) {
            Ok(proxy) => proxy,
            Err(reason) => {
                logging::log(
                    "[Upstream]",
                    &format!("⚠️ 账号代理不可用（{reason}），本次回退直连"),
                );
                None
            }
        }
    }

    // ─── 核心请求 ───────────────────────────────────────────

    /// 调用计费/活动接口。
    ///
    /// 对照 Node 版 `callBilling(spec, { body, session, query, expectCodeOk, locale })`：
    ///   ① URL = normalizeEndpoint(session.endpoint) + spec.path + query
    ///   ② 头 = buildHeaders(session, extra)，extra 含 Accept-Language 与白名单头
    ///   ③ 非 GET/HEAD 一律带 body（spec.body 兜底，序列化成 JSON）
    ///   ④ 走 session.proxy 出口，超时 20 秒
    ///   ⑤ 解包：非 2xx → 抛错；code !== 0 且 expectCodeOk → 抛错
    ///
    /// 私有：只给本模块与其子模块（usage.rs / activity.rs）用 ——
    /// Rust 的私有项对**后代模块**可见，所以子模块能直接调它，
    /// 而 api/ 层看不到（它只该走那些语义化方法）。
    async fn call_billing(
        &self,
        spec: BillingSpec,
        options: CallOptions<'_>,
    ) -> Result<BillingCall, BillingError> {
        let session = match options.session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        // 账户级端点：国内版/国际版账号各走自己的站点。
        // 会话缺 endpoint 时回落到 auth 的默认 baseUrl（Node 的
        // `normalizeEndpoint(activeSession.endpoint || baseUrl)`）
        let endpoint = session
            .get("endpoint")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(normalize_endpoint)
            .unwrap_or_else(|| normalize_endpoint(self.auth.default_context().base_url.as_str()));
        let url = format!("{endpoint}{}{}", spec.path, options.query.unwrap_or(""));

        let mut extra: Vec<(String, String)> = Vec::new();
        if let Some(locale) = options.locale {
            extra.push(("Accept-Language".to_string(), Self::accept_language(Some(locale))));
        }
        if spec.whitelist_headers {
            extra.extend(whitelist_headers(&session));
        }
        // 头顺序与 Node 的 `{...base, ...extra, X-Enterprise-Id, ...}` 一致：
        // 条件头在 extra 之后（同名时后者胜，Node 的对象展开也是这个顺序）
        let headers = build_headers(&session, &extra);

        let body = if spec.method == "GET" || spec.method == "HEAD" {
            None
        } else {
            // Node 是 `JSON.stringify(body ?? spec.body ?? {})` ——
            // 缺省体一律是空对象，不能省掉（上游对缺 body 的 POST 会报 400）
            Some(options.body.cloned().unwrap_or_else(|| spec.body()))
        };

        let proxy = Self::proxy_of(&session);
        logging::verbose(
            "[Billing]",
            &format!(
                "{} {url}{}",
                spec.method,
                match &proxy {
                    Some(proxy) if !proxy.label.is_empty() => format!(" 经代理 {}", proxy.label),
                    Some(proxy) => format!(" 经代理 {}", proxy.host),
                    None => String::new(),
                }
            ),
        );

        let response: ApiResponse = send_raw(
            spec.method,
            &url,
            body.as_ref(),
            &headers,
            proxy.as_ref(),
            Some(REQUEST_TIMEOUT_MS),
        )
        .await
        .map_err(|error| {
            if error.is_timeout() {
                BillingError::new("计费接口请求超时", 504)
            } else {
                // 文案对齐 Node：`计费接口请求失败: ${error.message}`
                BillingError::new(format!("计费接口请求失败: {error}"), 502)
            }
        })?;

        let payload = response.payload;
        let code = payload
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_i64);
        // Node: `payload?.msg || payload?.message` —— 空串也当「没有」，
        // 所以这里要过滤空串，不能拿到 Some("") 就当命中了
        let msg = payload
            .as_ref()
            .and_then(|value| {
                value
                    .get("msg")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .or_else(|| {
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                    })
            })
            .map(str::to_string);
        // Node 的 `requestId: payload?.requestId` 在缺失时是 undefined，
        // JSON 化会**丢掉这个键**；用 Option 表达同一语义（None = 不出键）
        let request_id = payload
            .as_ref()
            .and_then(|value| value.get("requestId"))
            .cloned();

        if response.status == 401 || response.status == 403 {
            return Err(BillingError::with_code(
                "登录态已过期或被拒绝，无法调用计费接口",
                401,
                code,
            ));
        }
        if !response.ok {
            // Node: `计费接口返回 HTTP ${status}${payload?.msg || payload?.message ? `: ...` : ''}`
            let detail = payload
                .as_ref()
                .and_then(|value| {
                    value
                        .get("msg")
                        .and_then(Value::as_str)
                        .or_else(|| value.get("message").and_then(Value::as_str))
                })
                .unwrap_or("");
            let message = if detail.is_empty() {
                format!("计费接口返回 HTTP {}", response.status)
            } else {
                format!("计费接口返回 HTTP {}: {detail}", response.status)
            };
            return Err(BillingError::with_code(message, response.status as i32, code));
        }
        if let Some(code_value) = code {
            if code_value != RESPONSE_CODE_OK && options.expect_code_ok {
                // Node: `${payload.msg || payload.message || `计费接口返回 code=${code}`}`
                let message = msg
                    .clone()
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| format!("计费接口返回 code={code_value}"));
                return Err(BillingError::with_code(
                    message,
                    response.status as i32,
                    Some(code_value),
                ));
            }
        }

        let data = payload
            .as_ref()
            .and_then(|value| value.get("data"))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(BillingCall { code, msg, request_id, data, raw: payload })
    }

    // ─── 签到 ───────────────────────────────────────────────

    /// 签到活动状态（AuthService.getCheckinStatus）。
    /// 成功但 data 为空时返回 null（与桌面端一致）。
    pub async fn get_checkin_status(&self, session: Option<&Value>) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        assert_checkin_supported(&active)?;
        let result = self
            .call_billing(
                BILLING_CHECKIN_STATUS,
                CallOptions { session: Some(&active), expect_code_ok: false, ..Default::default() },
            )
            .await?;
        if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
            return Ok(Value::Null);
        }
        Ok(request::normalize_checkin(&result.data))
    }

    /// 领取每日签到积分（AuthService.claimDailyCheckin）。
    ///
    /// 幂等：已领取时上游返回非 0 code，这里原样返回 `{success:false, code, msg}` ——
    /// 「今天已签到」不是错误，前端面板会把它显示成一条 warn 提示。
    pub async fn claim_daily_checkin(&self, session: Option<&Value>) -> Result<Value, BillingError> {
        let active = match session {
            Some(session) => session.clone(),
            None => self.require_session().await?,
        };
        assert_checkin_supported(&active)?;
        let result = self
            .call_billing(
                BILLING_DAILY_CHECKIN,
                CallOptions { session: Some(&active), expect_code_ok: false, ..Default::default() },
            )
            .await?;
        if result.code == Some(RESPONSE_CODE_OK) && !result.data.is_null() {
            return Ok(json!({
                "success": true,
                "code": 0,
                "data": request::normalize_checkin(&result.data),
                "raw": result.data,
            }));
        }
        // Node 的失败分支：`{ success:false, code, msg, requestId: result.requestId }`。
        // `requestId` 在上游没给时是 undefined → JSON.stringify 会**丢掉这个键**，
        // 所以这里也用 Map 组装，None 时不出键（而不是给 null）
        let mut failure = Map::new();
        failure.insert("success".to_string(), Value::Bool(false));
        failure.insert("code".to_string(), Value::from(result.code.unwrap_or(-1)));
        failure.insert(
            "msg".to_string(),
            Value::String(result.msg.unwrap_or_else(|| "签到失败".to_string())),
        );
        if let Some(request_id) = result.request_id {
            failure.insert("requestId".to_string(), request_id);
        }
        Ok(Value::Object(failure))
    }

    /// 默认语言下的积分简报（路由层入口，对照 server.mjs
    /// `billing.queryCreditsSummary({ locale: opts.locale })`）
    pub async fn query_credits_summary_default(&self) -> Result<Value, BillingError> {
        let locale = Self::current_locale();
        self.query_credits_summary(None, Some(&locale)).await
    }

    /// 默认语言下的签到组合动作（路由层入口）
    pub async fn checkin_and_report_default(&self) -> Result<Value, BillingError> {
        let locale = Self::current_locale();
        self.checkin_and_report(None, Some(&locale)).await
    }
}

/// 会话是否带 accessToken（Node: `session?.auth?.accessToken` 真值判定）
fn has_access_token(session: &Value) -> bool {
    session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .map(request::js_truthy)
        .unwrap_or(false)
}

/// 国际版（www.workbuddy.ai）目前没有签到活动，调用上游只会拿到无意义的结果。
/// 这里统一拦截，避免各入口（CLI / HTTP / 桌面端）分别判断漏掉。
fn assert_checkin_supported(session: &Value) -> Result<(), BillingError> {
    let info = resolve_edition(session.get("edition").and_then(Value::as_str));
    if info.id == "intl" {
        return Err(BillingError::new("国际版账号暂无签到活动", 400));
    }
    Ok(())
}
