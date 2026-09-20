//! Cline 凭证续期：`POST /api/v1/auth/refresh` + 单飞 + 比较再写。
//!
//! ── 协议（实测核对，2026-09）─────────────────────────────────
//! ```text
//! POST https://api.cline.bot/api/v1/auth/refresh
//! Content-Type: application/json
//! body: {"refreshToken":"<rv>","grantType":"refresh_token"}
//!
//! 200 → {"data":{"accessToken":"eyJ...（裸 JWT，不带 workos: 前缀）",
//!                "refreshToken":"<同一个 rv>",
//!                "expiresAt":"2026-09-19T19:30:04Z",   ← ISO 8601 字符串
//!                "tokenType":"Bearer",
//!                "userInfo":{...}},
//!         "success":true}
//! ```
//!
//! ── 三处实测踩出来的口径（别按直觉改）───────────────────────
//!   1. **`grantType` 是 camelCase**（不是 OAuth 标准的 `grant_type`）。
//!      源实现 `@cline/core` 的 `gB()` 发的就是这个键名。
//!   2. **返回的 accessToken 不带 `workos:` 前缀**（登录返回的带），
//!      而发请求时**必须带** —— 因此写入前统一过
//!      [`credentials::ensure_token_prefix`]。
//!   3. **`expiresAt` 是 ISO 字符串**，不是毫秒数。与桌面端 providers.json 里
//!      那个毫秒 `expiresAt` 是**两种形态**（同名不同型），解析要分开处理 ——
//!      见 [`parse_expires_at`]。
//!
//! ── 单飞的意义（本家尤其需要）───────────────────────────────
//! refresh_token 在服务端是**一次性轮换**语义的（虽然实测这次返回了同一个值，
//! 但协议本身允许轮换）。并发的多个请求同时发现 token 临期时，若各自去打一次
//! 续期，后到的那次可能拿着已经作废的 refresh_token → 401 → 用户被踢下线。
//! 因此走项目统一的 `refresh_flight::Table`（键含凭证指纹，凭证一变就是新的一轮）。
//!
//! ── 与其他几家的分工 ────────────────────────────────────────
//! 本模块只做「怎么刷」。`is_stateful` / `supports_refresh` 之类的**能力声明**
//! 在 `adapter.rs`，落盘在 `account_store::cline_accounts`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：绝不 unwrap/expect/panic；不持锁跨 await
//! （快照在 await 前取完，回写在 await 之后）。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::core::egress;
use crate::server::core::providers::refresh_flight::{self, Join, Table};
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::{self, ClineCredentials};

/// 续期请求超时（管理接口，与另外几家的 30 秒同档）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

static FLIGHTS: OnceLock<Table<ClineCredentials>> = OnceLock::new();

/// 取某账号的凭证快照（读账号记录 → 解析成凭证形态）。
///
/// `account_id` 为空 → 取 Cline 系（两个池里）优先级最小的启用账号；
/// 桌面端实时登录态由 `snapshot_for(None)` 那条链兜底。
///
/// **不看池**：本链路（续期 / 余额 / 维护）只关心「这条记录的凭证能不能用」，
/// 池只在发上游时的模型名前缀上体现（见 `providers::cline::models` 的模块头）。
/// 同一个 Cline 账号在两个池各有一条记录时，谁先被选中都无所谓 ——
/// 两条记录的凭证是同一份。
pub fn snapshot(store: &AccountStore, account_id: &str) -> Result<ClineCredentials, GatewayError> {
    let record = store.cline_any_account_record(account_id);
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            401,
            format!("Cline 账号 {account_id} 不存在或不属于 Cline（请重新添加）"),
        ));
    }
    snapshot_for(record.as_ref())
}

/// 按账号记录取凭证；没有记录时回落桌面端实时登录态。
///
/// ── 桌面端账号的凭证**不落盘** ──────────────────────────────
/// 与另外几家的「导入桌面端登录态」同一纪律：账号记录里只有 `desktop: true`
/// 标记，真正的 token 每次实时读 `~/.cline/data/settings/providers.json`。
/// 好处是 Cline 客户端自己续期后网关立刻跟上（不需要我们同步），
/// 代价是这条来源无法回写（见 [`persist_refresh`]）。
pub fn snapshot_for(record: Option<&Value>) -> Result<ClineCredentials, GatewayError> {
    match record {
        Some(record) => {
            // 桌面端账号：实时读文件
            if record.get("desktop").and_then(Value::as_bool) == Some(true) {
                return match credentials::read_desktop_credentials()? {
                    Some(credentials) => Ok(credentials),
                    None => Err(GatewayError::with_status(
                        400,
                        "本机 Cline 登录态不存在（请先在 Cline 客户端登录，或改用「填写凭证」添加）",
                    )),
                };
            }
            credentials::credentials_from_record(record)
        }
        None => match credentials::read_desktop_credentials()? {
            Some(credentials) => Ok(credentials),
            None => Err(GatewayError::with_status(
                401,
                "没有可用的 Cline 登录态（请先添加账号或导入桌面端登录态）",
            )),
        },
    }
}

/// 取可用凭证（临期主动刷新）；`force = true` 时不看临期窗口直接续期。
///
/// ── `force` 的两个消费方 ────────────────────────────────────
///   - 401 之后的强制续期（`adapter::refresh_access_token`）：token 完全可能
///     在时间上还很新的时候被上游拒绝（服务端侧失效），此时只看临期窗口会
///     拿回同一个坏 token；
///   - 维护任务的主动续期。
pub async fn ensure_fresh(
    store: &AccountStore,
    account_id: &str,
    force: bool,
) -> Result<ClineCredentials, GatewayError> {
    let mut credentials = snapshot(store, account_id)?;
    // 把「这是谁的凭证」钉到**账号记录 id** 上（覆盖来源里带来的那个 id）。
    // 桌面端来源尤其需要：它的 id 来自 providers.json 里的 auth 对象，
    // 与账号记录 id 毫无关系，而下游的 `persist_refresh` 靠这个字段认人
    // （见那里的说明）。
    credentials.id = account_id.to_string();
    if !force && !credentials.is_expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        // 没 refresh token：无法续期。**不是错误**（用户可能只粘贴了 access token），
        // 但不该让调用方以为拿到的是新鲜凭证 —— 返回 400 让维护任务记一条可读原因。
        return Err(GatewayError::with_status(
            400,
            "Cline 账号没有 refreshToken，无法自动续期（请重新登录或导入桌面端登录态）",
        ));
    }
    let key = format!(
        "{}:{}:{}:{}",
        store.file_string(),
        account_id,
        refresh_flight::fingerprint(&credentials.access_token),
        refresh_flight::fingerprint(&credentials.refresh_token),
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(store, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 真的去打一次续期，并把结果比较-再写回账号文件。
async fn refresh_and_save(
    store: &AccountStore,
    credentials: &ClineCredentials,
) -> Result<ClineCredentials, GatewayError> {
    let fresh = refresh_request(credentials).await?;
    persist_refresh(store, credentials, &fresh);
    Ok(fresh)
}

/// 打一次续期请求（不落盘，纯网络 + 解析）。
async fn refresh_request(credentials: &ClineCredentials) -> Result<ClineCredentials, GatewayError> {
    let client = egress::client_for(None);
    let response = client
        .post(format!("{}/auth/refresh", credentials::API_BASE_URL))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .json(&json!({
            // 键名照抄源实现：camelCase 的 grantType（不是 grant_type）
            "refreshToken": credentials.refresh_token,
            "grantType": "refresh_token",
        }))
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("Cline 续期请求失败: {}", egress::describe_error_detail(&error)),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status == 401 || status == 403 {
        return Err(GatewayError::with_status(
            401,
            "Cline 的登录态已失效（refreshToken 被拒绝），请重新登录",
        ));
    }
    if !(200..300).contains(&status) {
        // 上游错误体是 `{"error":"...","success":false}` 或
        // `{"error":{"code":..,"message":..}}` 两种形态
        let detail = upstream_error_message(&payload)
            .unwrap_or_else(|| crate::server::core::account_store::store_util::truncate_text(&text, 200));
        return Err(GatewayError::with_status(
            502,
            format!("Cline 续期失败（{status}）: {detail}"),
        ));
    }
    // 解包 `{"data":{...},"success":true}` 信封
    let data = payload.get("data").unwrap_or(&payload);
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "Cline 续期响应缺少 accessToken，旧凭证未被覆盖")
        })?;
    let refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(&credentials.refresh_token)
        .to_string();
    let expires_at = parse_expires_at(data.get("expiresAt"))
        .or_else(|| credentials::expires_at_from_jwt(access));
    // 显示名优先用续期返回的 userInfo.email，其次 JWT 里的 claims
    let (identity, _) = credentials::identity_from_jwt(access);
    let account = data
        .get("userInfo")
        .and_then(|info| info.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if identity.is_empty() {
                credentials.account.clone()
            } else {
                identity
            }
        });
    Ok(ClineCredentials {
        id: credentials.id.clone(),
        // 续期返回的是**裸 JWT**（登录返回的带 workos: 前缀），统一补上
        access_token: credentials::ensure_token_prefix(access),
        refresh_token: refresh,
        expires_at,
        account,
        name: credentials.name.clone(),
    })
}

/// 解析续期响应里的 `expiresAt`。
///
/// ── 两种形态（同名不同型，必须分开处理）─────────────────────
///   - **ISO 8601 字符串**（`"2026-09-19T19:30:04Z"`）：续期接口返回的形态；
///   - **毫秒数字**（`1789842888000`）：桌面端 providers.json 的形态。
///
/// 上游这两种都可能出现（不同接口给了不同形态），所以两种都认。
/// 解析失败返回 None（不是错误：过期时间判不出来时，靠 401 懒刷新兜底）。
pub fn parse_expires_at(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|value| *value > 0.0),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            // 数字字符串（毫秒）
            if let Ok(number) = text.parse::<f64>() {
                return (number > 0.0).then_some(number);
            }
            // ISO 8601 → 毫秒
            parse_iso8601_ms(text)
        }
        _ => None,
    }
}

/// ISO 8601（`2026-09-19T19:30:04Z` / 带毫秒 / 带时区偏移）→ 毫秒时间戳。
///
/// 手写解析而不引 chrono：项目依赖里没有日期库，而这里只需要「解析一个固定的
/// ISO 形态」这一件事。支持 `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`；
/// 解析不出来返回 None（调用方按「判不出」处理，不会 panic）。
fn parse_iso8601_ms(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let number = |start: usize, end: usize| -> Option<i64> {
        text.get(start..end)?.parse::<i64>().ok()
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    // 时区偏移（默认 UTC；`Z` / 无标记都按 UTC）
    let mut offset_minutes: i64 = 0;
    let mut rest = text.get(19..).unwrap_or("");
    if let Some(stripped) = rest.strip_prefix('.') {
        // 跳过小数秒
        let digits = stripped
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .count();
        rest = stripped.get(digits..).unwrap_or("");
    }
    if let Some(stripped) = rest.strip_prefix('+').or_else(|| rest.strip_prefix('-')) {
        let sign = if rest.starts_with('-') { -1 } else { 1 };
        let mut parts = stripped.split(':');
        let hours = parts.next().and_then(|value| value.parse::<i64>().ok())?;
        let minutes = parts
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        offset_minutes = sign * (hours * 60 + minutes);
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some((seconds as f64) * 1000.0)
}

/// 公历日期 → 1970-01-01 起的天数（Howard Hinnant 的 `days_from_civil` 算法）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 从上游错误体里取可读文案（两种形态都认）。
fn upstream_error_message(payload: &Value) -> Option<String> {
    if let Some(text) = payload.get("error").and_then(Value::as_str) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    payload
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 刷新结果回写（**只回写手动账号**，且只在真的刷新了的时候）。
///
/// ── 桌面端账号拒绝回写（与另外几家同一纪律）─────────────────
/// 桌面端来源的凭证在 `~/.cline/data/settings/providers.json`，那是 **Cline 客户端
/// 自己的文件**：网关回写会与客户端自己的续期互相顶掉（两边都在轮换，
/// 后写的赢，先写的那个 session 就废了）。因此这条来源只读。
/// 判据是**记录上的 `desktop` 标记**（`store.cline_is_desktop_account`），
/// 不是 id 字面量 —— 拆分后桌面端账号的 id 带池前缀（`cline-free-desktop`
/// 这类），写死任何一个 id 都会漏判。
///
/// ── 三个写盘前提 ────────────────────────────────────────────
///   1. **没刷新就不写**：token 逐字相同直接返回 —— 避免每个请求都整库重写；
///   2. **只在成功时写**：调用方只在 Ok 分支调用本函数；
///   3. **比较-再写**：`update_cline_account_tokens_if_current` 在同一把账号锁内
///      确认记录里仍是刷新前那份凭证才写入（期间用户换了账号 / 重导入时，
///      旧结果不得覆盖新凭证）。
///
/// 失败只记日志：刷新本身已经成功，回写失败不该让本次请求失败。
fn persist_refresh(store: &AccountStore, previous: &ClineCredentials, refreshed: &ClineCredentials) {
    if previous.id.is_empty() || store.cline_is_desktop_account(&previous.id) {
        return;
    }
    // 前提 1：没有任何新信息 → 不写盘
    if refreshed.access_token == previous.access_token
        && refreshed.refresh_token == previous.refresh_token
    {
        return;
    }
    match store.update_cline_account_tokens_if_current(
        &previous.id,
        &previous.access_token,
        &previous.refresh_token,
        &refreshed.access_token,
        &refreshed.refresh_token,
        refreshed.expires_at,
    ) {
        Ok(CredentialWrite::Written) => {}
        Ok(CredentialWrite::Stale) => {
            logging::verbose(
                "[Cline]",
                &format!(
                    "账号 {} 的续期结果已过期（凭证已被更换），未回写账号文件",
                    previous.id
                ),
            );
        }
        Err(reason) => {
            logging::verbose(
                "[Cline]",
                &format!("账号 {} 续期结果回写失败: {reason}", previous.id),
            );
        }
    }
}
