//! CodeArts 的每日福利领取（`/v1/ops` delivery → claim → confirm）。
//!
//! ── 这不是「签到」，形状差得很远 ────────────────────────────
//! 别家的每日签到是「定点、无人值守、一次请求」。本家是**运营活动交付**：
//! 先读活动列表（delivery），挑出**该自动领的那几类**，逐个 claim，再 confirm，
//! 最后**回读列表二次确认**。三个接口、两套状态、一个幂等键。
//! 所以本家不进 `auto_checkin` 的提供商清单，面板上也是一个独立按钮。
//!
//! ── 为什么必须回读 delivery 才算成功 ────────────────────────
//! `claim` 与 `confirm` 都只回 `code:0`，那是「请求受理了」，不是「到账了」。
//! 参考实现为此写了一句很直的文案：`请求已提交，但官方活动列表尚未确认到账`。
//! 把受理当成功 = 面板显示一个没发生的领取，而用户下一次看到的余额还是旧的。
//!
//! ── 为什么幂等键要在发请求**之前**落盘 ──────────────────────
//! `idempotentKey` 是这一轮领取的唯一去重依据。崩溃在「请求已发出、响应没收到」
//! 之间时，内存里的键就没了；重启后换一个键再领一次，上游会当成**另一笔**请求。
//! 所以台账的写入顺序是刻意的：先存键 → 再领 → 领成功再存 `claimed`。
//!
//! ── 为什么本地台账不能当「全天跳过」 ────────────────────────
//! delivery 列表里的 `CONFIRMED` **不带确认日期**：上一期登录周期留下的 CONFIRMED
//! 会一直显示到今天。参考实现据此留了一条明确指令 —— 不要把观察到的状态变成本地
//! 的全天跳过，每次都要重新判资格。所以本地台账只用来**限流写请求**
//! （每天 ≤6 次、间隔 ≥10 分钟），手动点击永远重读资格，不受这两个闸约束。
//!
//! ── 领到的东西进哪个账户 ───────────────────────────────────
//! ops 福利领的是**套餐赠送积分**，**不增加福利模型的 token 池**（两个账户各记各的，
//! 见 `balance.rs` 模块头）。面板文案必须照实写，否则用户会以为领完就能多跑福利模型。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{balance, chat, credentials::Credential, redact, signer};

pub const DELIVERY_PATH: &str = "/v1/ops/delivery?channel=IDE";
pub const CLAIM_PATH: &str = "/v1/ops/claim";
pub const CONFIRM_PATH: &str = "/v1/ops/confirm";

/// 台账格式版本。参考实现对 `version != 2` 的记录**拒绝发送写请求** ——
/// 一个读不懂的台账不能当成「今天还没领过」。
pub const LEDGER_VERSION: i64 = 2;
/// 每天最多几次**写**尝试（读资格不算）。
pub const MAX_ATTEMPTS_PER_DAY: i64 = 6;
/// 两次写尝试的最小间隔。
pub const MIN_ATTEMPT_GAP: Duration = Duration::from_secs(10 * 60);
/// 活动列表按哪个时区算「今天」：上游的活动周期是北京时间零点。
const DAY_ZONE_OFFSET_SECONDS: i32 = 8 * 60 * 60;

/// 一个活动条目。`id` 保持上游给的**原始 JSON**（字符串或数字都出现过），
/// 发回 claim 请求时必须原样带回，转成字符串再发就会改变 JSON 类型。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Campaign {
    pub id: Value,
    pub kind: String,
    pub claimable: bool,
    pub status: String,
    pub benefit_amount: f64,
    pub benefit_unit: String,
}

impl Campaign {
    /// 活动标识的字符串形态（`123` 与 `"123"` 都算同一个活动）。空串 = 认不出，
    /// 认不出的条目**一律不自动领**（没有键就没法记台账，也没法回读确认）。
    pub fn key(&self) -> String {
        match &self.id {
            Value::String(text) => text.trim().to_string(),
            Value::Number(number) => number.to_string(),
            _ => String::new(),
        }
    }

    /// 官方口径的「已确认到账」：不可再领，且状态是 CONFIRMED / CONSUMED。
    pub fn is_confirmed(&self) -> bool {
        !self.claimable && (self.status == "CONFIRMED" || self.status == "CONSUMED")
    }

    /// 属于**可自动领**的那一类：每日登录送积分。注册礼 / 学生礼 / 邀请礼
    /// 都故意不自动领 —— 它们是一次性奖励，自动领等于替用户做决定。
    pub fn is_daily_login_credit(&self) -> bool {
        self.kind == "USER_LOGIN" && self.benefit_unit == "CREDIT" && !self.key().is_empty()
    }

    /// 这一条还有活要干吗（要么能领，要么领了没确认）。
    pub fn has_work(&self) -> bool {
        self.claimable || self.status == "CLAIMED"
    }
}

/// 一次领取的结果。四种都有意义，别压成一个 bool。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 官方列表已回读确认到账
    Confirmed,
    /// 本来就已是 CONFIRMED / CONSUMED（不是失败，也不该报「领取成功」）
    Already,
    /// 这个账号没有可自动领的活动
    NotEligible,
    /// 被本地限流挡住（当天次数用尽或间隔不足）
    Skipped,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Confirmed => "官方已确认到账",
            Outcome::Already => "已领取并确认",
            Outcome::NotEligible => "暂无可领取活动",
            Outcome::Skipped => "等待重试",
        }
    }
}

fn trim(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

/// 北京时间意义上的「今天」（`YYYY-MM-DD`）。台账按它判断是否新的一天。
///
/// 用固定偏移而不是时区名：上游的活动周期就是 UTC+8 零点，本家只需要这一个偏移，
/// 为它带一份 tzdata 不值得 —— 而且网关跑在 UTC 容器里时，「今天」也必须仍然是
/// 北京的那个今天（跟着机器时区走会让零点提前 8 小时）。
pub fn today(now_ms: i64) -> String {
    let seconds = now_ms / 1000 + i64::from(DAY_ZONE_OFFSET_SECONDS);
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|time| time.date_naive().format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string())
}

/// 一个北京时间自然日的长度（毫秒）。
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `now` 之后最近的**北京时间**零点（毫秒时间戳，纯函数）。
///
/// 给「福利池撞额度 ⇒ 冷却到本池重置点」用：上游不告诉我们日池什么时候重置，
/// 但它的一整套日口径（活动周期、`daily_token_limit` 的「今日」）都是 UTC+8 零点，
/// 所以那个零点就是唯一有据可依的重置时刻。用固定偏移而不是机器时区，理由与
/// [`today`] 完全相同 —— 网关跑在 UTC 容器里时，「明天零点」也必须仍然是北京的那个。
///
/// 边界：恰好落在零点上时给**次日**零点（刚过零点，下一个重置点是一整天之后）。
pub fn next_day_boundary_ms(now_ms: i64) -> i64 {
    let shifted = now_ms + i64::from(DAY_ZONE_OFFSET_SECONDS) * 1000;
    // div_euclid 而不是 `/`：负数时刻（1970 前）在 Rust 里默认向零取整会算错整天边界
    let start_of_today = shifted.div_euclid(DAY_MS) * DAY_MS;
    start_of_today + DAY_MS - i64::from(DAY_ZONE_OFFSET_SECONDS) * 1000
}

/// 一次 ops 请求：签名 GET/POST + `code == 0` 的 envelope 判定。
///
/// 头集合只有三项（`Content-Type` / `Agent-Type: PromptCenter` / `X-Language`），
/// 与目录、统计那两处都不同；区域 API 的签名**不带 Host**（参考实现默认
/// `sign_host:false`），福利网关那套才带 —— 别统一。
async fn welfare_request(
    base: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    credential: &Credential,
) -> Result<Value, GatewayError> {
    let url = format!("{}{path}", trim(base));
    let payload = body.clone().map(|value| value.to_string().into_bytes()).unwrap_or_default();
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Agent-Type".to_string(), "PromptCenter".to_string()),
        ("X-Language".to_string(), chat::DEFAULT_LANGUAGE.to_string()),
    ];
    let signing = super::oauth::signer_credential(credential);
    let signed = signer::sign(method, &url, &headers, &payload, &signing, false)
        .map_err(|reason| GatewayError::with_status(500, format!("CodeArts 活动请求签名失败：{reason}")))?;
    let built = match method {
        "POST" => egress::client_for(None)
            .post(&url)
            .timeout(std::time::Duration::from_secs(20))
            .body(payload.clone()),
        _ => egress::client_for(None).get(&url).timeout(std::time::Duration::from_secs(20)),
    };
    let mut request = built;
    for (name, value) in signed {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|error| {
        GatewayError::with_status(
            502,
            format!("CodeArts 活动请求失败：{}", egress::describe_error_detail(&error)),
        )
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if status != 200 {
        return Err(GatewayError::with_status(
            i32::from(status),
            format!("CodeArts 活动接口返回 HTTP {status}：{}", excerpt(&text, credential)),
        ));
    }
    // `code` 缺失与 `code != 0` 同罪：**没确认成功就不能记为已领取**
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|_| GatewayError::with_status(502, "CodeArts 活动接口响应不是合法 JSON，未记录为已领取"))?;
    match parsed.get("code").and_then(Value::as_i64) {
        Some(0) => {}
        other => {
            return Err(GatewayError::with_status(
                502,
                format!("CodeArts 活动接口未确认成功（需要 code=0，实际 {other:?}），未记录为已领取"),
            ));
        }
    }
    Ok(parsed.get("data").cloned().unwrap_or(Value::Null))
}

/// 错误体先脱敏再截断（与 `balance.rs` 同一口径）。
fn excerpt(text: &str, credential: &Credential) -> String {
    let cleaned = redact::redact(text, credential, false);
    let count = cleaned.chars().count();
    if count <= 300 {
        return cleaned;
    }
    cleaned.chars().take(300).collect::<String>() + "…"
}

/// 读活动列表。**缺少 `items` 判失败**（而不是当「今天没活动」）——
/// 上游换了形状时把「读不懂」显示成「没有活动」，用户会以为资格被取消了。
pub async fn delivery(base: &str, credential: &Credential) -> Result<Vec<Campaign>, GatewayError> {
    let data = welfare_request(base, "GET", DELIVERY_PATH, None, credential).await?;
    parse_delivery(&data)
}

fn parse_delivery(data: &Value) -> Result<Vec<Campaign>, GatewayError> {
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::with_status(502, "CodeArts 活动列表响应缺少 items"))?;
    Ok(items
        .iter()
        .map(|item| Campaign {
            id: item.get("campaignId").cloned().unwrap_or(Value::Null),
            kind: item.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
            claimable: item.get("claimable").and_then(Value::as_bool).unwrap_or(false),
            status: item.get("status").and_then(Value::as_str).unwrap_or("").to_string(),
            benefit_amount: item.get("benefitAmount").and_then(Value::as_f64).unwrap_or(0.0),
            benefit_unit: item.get("benefitUnit").and_then(Value::as_str).unwrap_or("").to_string(),
        })
        .collect())
}

/// 领一笔（`campaignId` 原样带回，不改类型）。
pub async fn claim(
    base: &str,
    credential: &Credential,
    campaign_id: &Value,
    idempotent_key: &str,
) -> Result<(), GatewayError> {
    let data = welfare_request(
        base,
        "POST",
        CLAIM_PATH,
        Some(json!({"campaignId": campaign_id, "idempotentKey": idempotent_key, "channel": "IDE"})),
        credential,
    )
    .await?;
    // 响应里的活动 id 必须与请求的一致，否则**不进入确认阶段**：
    // 确认错活动会把别人的到账当成自己的
    let returned = data.get("campaignId").cloned().unwrap_or(Value::Null);
    let expected = Campaign { id: campaign_id.clone(), ..Campaign::default() }.key();
    let actual = Campaign { id: returned, ..Campaign::default() }.key();
    if actual != expected || expected.is_empty() {
        return Err(GatewayError::with_status(
            502,
            format!("CodeArts 领取响应活动 ID 不匹配（要 {expected}，回 {actual}），未进入确认阶段"),
        ));
    }
    Ok(())
}

/// 确认一笔。
pub async fn confirm(base: &str, credential: &Credential, campaign_id: &Value) -> Result<(), GatewayError> {
    welfare_request(base, "POST", CONFIRM_PATH, Some(json!({"campaignId": campaign_id})), credential)
        .await
        .map(|_| ())
}

/// 台账（存在账号记录的 `codeartsWelfare` 里）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ledger {
    pub version: i64,
    pub day: String,
    pub attempts: i64,
    pub last_attempt_ms: i64,
    pub accepted: bool,
    pub campaigns: HashMap<String, Progress>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub idempotent_key: String,
    pub claimed: bool,
    pub confirmed: bool,
}

impl Ledger {
    fn from_value(value: &Value) -> Option<Ledger> {
        let day = value.get("day").and_then(Value::as_str)?.to_string();
        // 日期本身要能解析：一个读不懂的 day 不能当成「不是今天」
        chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").ok()?;
        let attempts = value.get("attempts").and_then(Value::as_i64)?;
        if value.get("version").and_then(Value::as_i64)? != LEDGER_VERSION || !(0..=MAX_ATTEMPTS_PER_DAY).contains(&attempts) {
            return None;
        }
        let mut campaigns = HashMap::new();
        if let Some(map) = value.get("campaigns").and_then(Value::as_object) {
            for (key, item) in map {
                campaigns.insert(
                    key.clone(),
                    Progress {
                        idempotent_key: item.get("idempotentKey").and_then(Value::as_str).unwrap_or("").to_string(),
                        claimed: item.get("claimed").and_then(Value::as_bool).unwrap_or(false),
                        confirmed: item.get("confirmed").and_then(Value::as_bool).unwrap_or(false),
                    },
                );
            }
        }
        Some(Ledger {
            version: LEDGER_VERSION,
            day,
            attempts,
            last_attempt_ms: value.get("lastAttemptMs").and_then(Value::as_i64).unwrap_or(0).max(0),
            accepted: value.get("accepted").and_then(Value::as_bool).unwrap_or(false),
            campaigns,
        })
    }

    fn to_value(&self) -> Value {
        let mut campaigns = serde_json::Map::new();
        for (key, progress) in &self.campaigns {
            campaigns.insert(
                key.clone(),
                json!({
                    "idempotentKey": progress.idempotent_key,
                    "claimed": progress.claimed,
                    "confirmed": progress.confirmed,
                }),
            );
        }
        json!({
            "version": self.version,
            "day": self.day,
            "attempts": self.attempts,
            "lastAttemptMs": self.last_attempt_ms,
            "accepted": self.accepted,
            "campaigns": Value::Object(campaigns),
        })
    }
}

/// 只读预览（面板上「先看一眼」那一步）：**一个写请求都不发**。
/// 返回今天的状态、可领的活动、以及限流闸门的剩余量。
pub async fn preview(store: &AccountStore, account_id: &str, base: &str, now_ms: i64) -> Result<Value, GatewayError> {
    let credential = current_credential(store, account_id).await?;
    let items = delivery(base, &credential).await?;
    let daily: Vec<&Campaign> = items.iter().filter(|item| item.is_daily_login_credit()).collect();
    let ledger = ledger_of(store, account_id, now_ms)?;
    Ok(json!({
        "day": ledger.day,
        "attempts": ledger.attempts,
        "attemptsLeft": (MAX_ATTEMPTS_PER_DAY - ledger.attempts).max(0),
        "nextAttemptInMs": gap_remaining(&ledger, now_ms).map(|millis| millis as i64).unwrap_or(0),
        "acceptedToday": ledger.accepted,
        "campaigns": daily
            .iter()
            .map(|item| json!({
                "id": item.key(),
                "status": item.status,
                "claimable": item.claimable,
                "benefitAmount": item.benefit_amount,
                "benefitUnit": item.benefit_unit,
                "confirmed": item.is_confirmed(),
                "localClaimed": ledger.campaigns.get(&item.key()).map(|p| p.claimed).unwrap_or(false),
            }))
            .collect::<Vec<_>>(),
        "eligible": daily.iter().any(|item| item.has_work()),
        "note": "领取到账的是套餐赠送积分，不增加福利模型 token 池",
    }))
}

/// 读一份可用的凭据（临期就续，与转发同一入口）。
pub(crate) async fn current_credential(store: &AccountStore, account_id: &str) -> Result<Credential, GatewayError> {
    let proxy = super::record_proxy(store, account_id)?;
    super::refresh::ensure_fresh(store, account_id, false, proxy.as_ref()).await
}

/// 取台账并归一到「今天」。
///
/// ── 三种"读不出来"必须分开 ──────────────────────────────────
///   * **没有台账**（新账号）→ 从今天的空台账开始，正常。
///   * **有台账但读不懂**（版本不对 / 日期解析不出来 / attempts 越界）→
///     **报错，一个写请求都不发**。参考实现同一条：`每日活动记录无效，未发送领取请求`。
///     当成空台账重来看着"自我修复"，实际是把上一轮的幂等键丢了 —— 崩溃后重试
///     会被上游当成**另一笔**领取，而这正是这份台账存在的唯一理由。
///   * **台账日期在今天之后** → 报错。要么系统时钟倒退过，要么记录被手工改过；
///     两种情况下"重来一天"都会把当天的次数与键一起丢掉。
fn ledger_of(store: &AccountStore, account_id: &str, now_ms: i64) -> Result<Ledger, GatewayError> {
    let day = today(now_ms);
    let fresh = || Ledger { version: LEDGER_VERSION, day: day.clone(), attempts: 0, last_attempt_ms: 0, accepted: false, campaigns: HashMap::new() };
    let Some(stored) = store.codearts_welfare_ledger(account_id) else {
        return Ok(fresh());
    };
    let Some(ledger) = Ledger::from_value(&stored) else {
        return Err(GatewayError::with_status(
            500,
            "CodeArts 领取台账无效，未发送领取请求（请先在账号页删除并重新添加该账号，或手工修正 codeartsWelfare 字段）",
        ));
    };
    match ledger.day.cmp(&day) {
        std::cmp::Ordering::Equal => Ok(ledger),
        // 新的一天：整份重来（昨天的 attempts / 幂等键与今天无关）
        std::cmp::Ordering::Less => Ok(fresh()),
        std::cmp::Ordering::Greater => Err(GatewayError::with_status(
            500,
            format!("系统日期（{day}）早于领取记录（{}），先纠正时钟再领取", ledger.day),
        )),
    }
}

/// 两次写尝试之间的剩余等待（None = 没在等）。
pub fn gap_remaining(ledger: &Ledger, now_ms: i64) -> Option<u64> {
    if ledger.last_attempt_ms == 0 {
        return None;
    }
    // 全程 saturating：`lastAttemptMs` 是从账号记录里读出来的（账号可以被导入 /
    // 手工编辑），一个极端值不该让减法绕回一个看起来"早就不等了"的小数。
    let elapsed = now_ms.saturating_sub(ledger.last_attempt_ms);
    let gap = i64::try_from(MIN_ATTEMPT_GAP.as_millis()).unwrap_or(i64::MAX);
    if elapsed >= gap { None } else { Some(gap.saturating_sub(elapsed) as u64) }
}

/// 走一遍完整的领取流程。**这是本模块唯一会发写请求的函数。**
///
/// `manual = true`（用户在面板上点）时**不受本地限流约束**：限流保护的是
/// 「无人值守的自动重试」，用户主动要看一眼不该被一句「等待重试」挡回去 ——
/// 但资格仍每次重读，见模块头那条「不要把 CONFIRMED 变成全天跳过」。
pub async fn claim_account(
    store: &AccountStore,
    account_id: &str,
    base: &str,
    now_ms: i64,
    manual: bool,
) -> Result<Outcome, GatewayError> {
    // 先取凭据（临期会真换发并写回账号记录），再往下走。台账的写回按 id 现读记录，
    // 所以这里**不需要**也不该提前抓一份记录快照 —— 上一版就是这么写的，
    // 结果每次在临期凭据上领取，全部台账写回都被判 Stale 并被静默吞掉。
    let credential = current_credential(store, account_id).await?;
    let mut ledger = ledger_of(store, account_id, now_ms)?;
    // 台账写不进去就**不能往下发写请求**：幂等键的意义就在于「崩溃后重试还是同一笔」，
    // 而键只存在于这份台账里。落不了盘还去领，等于放弃去重。
    let save = |ledger: &Ledger| -> Result<(), GatewayError> {
        store
            .put_codearts_welfare_ledger(account_id, &ledger.to_value())
            .map_err(|error| GatewayError::with_status(error.status_code, error.message))
    };

    let items = delivery(base, &credential).await?;
    let daily: Vec<Campaign> = items.into_iter().filter(Campaign::is_daily_login_credit).collect();
    if daily.is_empty() {
        return Ok(Outcome::NotEligible);
    }
    if daily.iter().all(Campaign::is_confirmed) {
        return Ok(Outcome::Already);
    }
    // 限流只管**写**：读资格、判确认永远放行
    if !manual && (ledger.attempts >= MAX_ATTEMPTS_PER_DAY || gap_remaining(&ledger, now_ms).is_some()) {
        return Ok(Outcome::Skipped);
    }
    if !daily.iter().any(Campaign::has_work) {
        return Ok(Outcome::NotEligible);
    }

    ledger.attempts += 1;
    ledger.last_attempt_ms = now_ms;
    ledger.accepted = false;
    save(&ledger)?;

    for item in &daily {
        let id = item.key();
        if item.is_confirmed() {
            continue;
        }
        let mut progress = ledger.campaigns.get(&id).cloned().unwrap_or_default();
        if item.status == "CLAIMED" {
            progress.claimed = true;
        }
        if !progress.claimed && item.claimable {
            if progress.idempotent_key.is_empty() {
                progress.idempotent_key = format!("claim_{id}_{now_ms}");
            }
            ledger.campaigns.insert(id.clone(), progress.clone());
            // 键先落盘再发写请求（见模块头「为什么幂等键要在发请求之前落盘」）
            save(&ledger)?;
            claim(base, &credential, &item.id, &progress.idempotent_key).await?;
            progress.claimed = true;
            ledger.campaigns.insert(id.clone(), progress.clone());
            save(&ledger)?;
        }
        if progress.claimed {
            ledger.campaigns.insert(id.clone(), progress.clone());
            confirm(base, &credential, &item.id).await?;
        }
    }

    // 回读二次确认：`code:0` 只是受理
    let verified = delivery(base, &credential).await?;
    for item in &daily {
        let id = item.key();
        let confirmed = verified.iter().any(|seen| {
            seen.key() == id && seen.kind == "USER_LOGIN" && seen.is_confirmed()
        });
        if !confirmed {
            return Err(GatewayError::with_status(
                502,
                "请求已提交，但官方活动列表尚未确认到账；将重试确认，不显示领取成功",
            ));
        }
        let mut progress = ledger.campaigns.get(&id).cloned().unwrap_or_default();
        progress.confirmed = true;
        ledger.campaigns.insert(id, progress);
    }
    ledger.accepted = true;
    save(&ledger)?;
    logging::log("[CodeArts]", "每日福利：官方活动列表已回读确认到账（计入套餐赠送积分，不增加福利模型 token 池）");
    Ok(Outcome::Confirmed)
}

/// 领取成功后顺手刷新一次余额（面板上「领完就能看到积分变了」）。
/// 失败不影响领取结果 —— 余额是可选信息。
pub async fn refresh_usage(store: &AccountStore, account_id: &str, now_ms: i64) -> Value {
    let Ok(credential) = current_credential(store, account_id).await else {
        return Value::Null;
    };
    let (statistics, benefit) = balance::fetch_both(
        super::models::DEFAULT_BASE_URL,
        super::models::DEFAULT_BENEFIT_GATEWAY_URL,
        &credential,
        chat::DEFAULT_LANGUAGE,
        chat::DEFAULT_PLUGIN_VERSION,
    )
    .await;
    // 形状与 `query_usage` 保持一致：福利那一侧的三种状态分开落
    // （读到 → wallets；上游明说没有 → benefitAbsent；读失败 → benefitError）。
    // 「领到了积分」与「有福利模型额度」是两本账 —— 后者没有不代表领取失败。
    let benefit_absent = matches!(&benefit, Ok(None));
    let benefit_value = benefit.as_ref().ok().and_then(|value| value.as_ref());
    let _ = now_ms;
    let mut document = balance::usage_document(statistics.as_ref().ok(), benefit_value);
    if benefit_absent {
        document["benefitAbsent"] = Value::Bool(true);
    }
    if let Err(error) = &statistics {
        document["statisticsError"] = Value::String(error.message.clone());
    }
    if let Err(error) = &benefit {
        document["benefitError"] = Value::String(error.message.clone());
    }
    document
}

#[cfg(test)]
mod tests {
    //! 领取状态机用**进程内 mock 上游**跑真流程（与 `session.rs` / `chat.rs` 同一手法）：
    //! 这里最容易出错的地方都不是解析，而是**顺序** —— 幂等键必须先落盘、
    //! 确认之后必须回读、回读不通过必须报错而不是显示成功。
    use std::sync::{Arc, Mutex};

    use crate::server::core::account_store::AccountStore;
    use crate::server::core::providers::codearts::credentials::{OAuthContext, PkcePair, Credential};
    use crate::server::db::Db;

    use super::*;

    fn campaign(id: &str, kind: &str, unit: &str, claimable: bool, status: &str) -> Campaign {
        Campaign {
            id: Value::String(id.to_string()),
            kind: kind.to_string(),
            claimable,
            status: status.to_string(),
            benefit_amount: 100.0,
            benefit_unit: unit.to_string(),
        }
    }

    #[test]
    fn campaign_keys_accept_both_upstream_id_types_but_never_garbage() {
        assert_eq!("abc", campaign("abc", "USER_LOGIN", "CREDIT", true, "").key());
        let numeric = Campaign { id: json!(123), ..campaign("", "USER_LOGIN", "CREDIT", true, "") };
        assert_eq!("123", numeric.key());
        let broken = Campaign { id: Value::Null, ..numeric.clone() };
        assert_eq!("", broken.key(), "认不出的 id 不能拿去记台账");
        assert!(!broken.is_daily_login_credit(), "没有键就绝不自动领");
    }

    #[test]
    fn only_daily_login_credits_are_eligible_for_auto_claim() {
        assert!(campaign("a", "USER_LOGIN", "CREDIT", true, "").is_daily_login_credit());
        // 注册礼 / 学生礼 / 邀请礼：一次性奖励，自动领等于替用户做决定
        assert!(!campaign("b", "REGISTER", "CREDIT", true, "").is_daily_login_credit());
        assert!(!campaign("c", "USER_LOGIN", "TOKEN", true, "").is_daily_login_credit());
    }

    #[test]
    fn confirmed_needs_both_halves_of_the_signal() {
        assert!(campaign("a", "USER_LOGIN", "CREDIT", false, "CONFIRMED").is_confirmed());
        assert!(campaign("a", "USER_LOGIN", "CREDIT", false, "CONSUMED").is_confirmed());
        assert!(!campaign("a", "USER_LOGIN", "CREDIT", true, "CONFIRMED").is_confirmed(), "还能领就说明没到账");
        assert!(!campaign("a", "USER_LOGIN", "CREDIT", false, "CLAIMED").is_confirmed());
        // 「还有活」与「已确认」是两回事：CLAIMED 未确认的要接着 confirm
        assert!(campaign("a", "USER_LOGIN", "CREDIT", false, "CLAIMED").has_work());
        assert!(!campaign("a", "USER_LOGIN", "CREDIT", false, "CONFIRMED").has_work());
    }

    #[test]
    fn delivery_parsing_requires_an_items_list() {
        let ok = parse_delivery(&json!({"items": [{"campaignId": "x", "type": "USER_LOGIN", "claimable": true, "status": "", "benefitUnit": "CREDIT"}]})).expect("合法列表");
        assert_eq!(1, ok.len());
        // 上游换形状时要报「读不懂」，不能显示成「今天没有活动」
        assert!(parse_delivery(&json!({})).is_err());
        assert!(parse_delivery(&json!({"items": "不是数组"})).is_err());
    }

    #[test]
    fn an_unreadable_ledger_is_refused_not_treated_as_fresh() {
        // version 不对 / 日期解析不出来 / attempts 越界 → 都不能当成「今天还没领过」
        assert!(Ledger::from_value(&json!({"version": 1, "day": "2026-09-27", "attempts": 0})).is_none());
        assert!(Ledger::from_value(&json!({"version": LEDGER_VERSION, "day": "昨天", "attempts": 0})).is_none());
        assert!(Ledger::from_value(&json!({"version": LEDGER_VERSION, "day": "2026-09-27", "attempts": 99})).is_none());
        let good = Ledger::from_value(&json!({"version": LEDGER_VERSION, "day": "2026-09-27", "attempts": 2, "campaigns": {"x": {"claimed": true}}})).expect("合法台账");
        assert_eq!(2, good.attempts);
        assert!(good.campaigns["x"].claimed);
        // 往返一次必须等价（台账是要落盘的，序列化不对称等于每次读回都漂一点）
        assert_eq!(good, Ledger::from_value(&good.to_value()).expect("往返后仍合法"));
    }

    #[test]
    fn the_rate_gate_only_bounds_writing_attempts() {
        let mut ledger = Ledger { version: LEDGER_VERSION, day: "2026-09-27".into(), attempts: 1, last_attempt_ms: 1_000, ..Default::default() };
        // 1 毫秒过去了，还差 599_999
        assert_eq!(Some(599_999), gap_remaining(&ledger, 1_001), "间隔不足要报剩余时间");
        assert_eq!(None, gap_remaining(&ledger, 1_000 + MIN_ATTEMPT_GAP.as_millis() as i64));
        ledger.last_attempt_ms = 0;
        assert_eq!(None, gap_remaining(&ledger, 1_000), "从没试过就没有等待");
        ledger.attempts = MAX_ATTEMPTS_PER_DAY;
        assert!(ledger.attempts >= MAX_ATTEMPTS_PER_DAY, "当天次数用尽");
    }

    #[test]
    fn the_day_rolls_over_in_beijing_time_not_utc() {
        // 2026-09-26T16:30Z = 北京时间 2026-09-27 00:30：UTC 还是 26 号，台账必须已经进 27 号
        let millis = chrono::DateTime::parse_from_rfc3339("2026-09-26T16:30:00Z").unwrap().timestamp_millis();
        assert_eq!("2026-09-27", today(millis));
        let earlier = millis - 45 * 60 * 1000;
        assert_eq!("2026-09-26", today(earlier), "北京 23:45 仍是前一天");
    }

    /// 日池冷却的重置点：北京时间的下一个零点，而不是「24 小时后」也不是 UTC 零点。
    #[test]
    fn next_day_boundary_is_the_coming_beijing_midnight() {
        let ms = |rfc: &str| {
            chrono::DateTime::parse_from_rfc3339(rfc).expect("合法时刻").timestamp_millis()
        };
        // 北京 23:45（= UTC 15:45）→ 15 分钟后就是次日零点
        let late = ms("2026-09-26T15:45:00Z");
        assert_eq!(late + 15 * 60 * 1000, next_day_boundary_ms(late));
        // 北京 08:00（= UTC 00:00）→ 16 小时后；UTC 零点此刻正是北京时间中午前后，
        // 拿 UTC 当天零点当答案会差 8 小时
        let morning = ms("2026-09-26T00:00:00Z");
        assert_eq!(morning + 16 * 60 * 60 * 1000, next_day_boundary_ms(morning));
        // 恰好落在零点上 → 给**下一个**零点（刚过零点，下一个重置点是一整天之后）
        let exactly = next_day_boundary_ms(late + 15 * 60 * 1000);
        assert_eq!(late + 15 * 60 * 1000 + DAY_MS, exactly);
    }

    /// 与 [`today`] 交叉核对：两条读法共用同一个偏移常量，所以「下一个零点」这一毫秒
    /// 之前仍应属于今天、它本身必须已经进明天 —— 偏移只错一处也会被这条抓住
    /// （而不是等到生产上冷却时间差 8 小时才发现）。全部比较走 `today()`，
    /// 不在测试里另算一遍时区，否则错的可能是测试自己。
    #[test]
    fn next_day_boundary_agrees_with_the_ledger_day() {
        let next_day = |text: &str| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .expect("today() 给的就是 YYYY-MM-DD")
                .succ_opt()
                .expect("有次日")
                .format("%Y-%m-%d")
                .to_string()
        };
        for hour in [0, 7, 8, 9, 15, 16, 23] {
            // UTC 的 2026-09-26 各整点：北京口径横跨 26/27 两天，两种都测到
            let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
                .expect("合法日期")
                .and_hms_opt(hour, 0, 0)
                .expect("合法时刻")
                .and_utc()
                .timestamp_millis();
            let boundary = next_day_boundary_ms(now);
            assert!(boundary > now, "重置点必须在未来：UTC hour={hour}");
            assert!(
                boundary - now <= DAY_MS,
                "重置点不该比一整天还远：UTC hour={hour}，差了 {} 小时",
                (boundary - now) / 3_600_000
            );
            assert_eq!(today(now), today(boundary - 1), "零点前一毫秒还属于今天：hour={hour}");
            assert_eq!(next_day(&today(now)), today(boundary), "零点整已经属于明天：hour={hour}");
        }
    }

    // ── 真流程：mock 上游 ─────────────────────────────────────

    /// 一个按脚本回放的 ops 上游，并把每次收到的请求按顺序记下来。
    struct Mock {
        base: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    /// `script` 里每个元素是一条请求要回的内容（按到达顺序消费）。
    async fn mock_upstream(script: Vec<Value>) -> Mock {
        use axum::body::Bytes;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let index = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let (seen_clone, index_clone, script_clone) = (seen.clone(), index.clone(), script.clone());
        // 用 fallback 接所有路径与方法：ops 那三个端点各有路径，测试只关心
        // 「按到达顺序回了什么、收到了什么」，不必为每条路径单独挂路由。
        let app = axum::Router::new().fallback(move |uri: axum::extract::OriginalUri, body: Bytes| {
            let (seen, index, script) = (seen_clone.clone(), index_clone.clone(), script_clone.clone());
            async move {
                let text = String::from_utf8_lossy(&body).to_string();
                let method = if text.is_empty() { "GET" } else { "POST" };
                seen.lock().unwrap().push(format!("{method} {} {text}", uri.path()));
                let slot = index.fetch_add(1, Ordering::SeqCst);
                let payload = script.get(slot).cloned().unwrap_or_else(|| json!({"code": 0, "data": {"items": []}}));
                Response::new((StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], payload.to_string()).into_response())
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Mock { base: format!("http://{addr}"), seen }
    }

    fn store_with(account_id: &str) -> AccountStore {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("codearts-welfare-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AccountStore::with_db(Some(Db::open(&dir.join("agent2api.db")).expect("临时库应当能建起来")));
        store
            .add_codearts_account(
                &Credential {
                    access_key_id: "AK".into(),
                    secret_access_key: "SK".into(),
                    security_token: "sts".into(),
                    // 离到期很远：领取流程不该在这里动网络
                    expires_at: (chrono::Utc::now() + chrono::TimeDelta::hours(6)).to_rfc3339(),
                    domain_id: "dom".into(),
                    user_id: account_id.into(),
                    refresh_token: "rt".into(),
                    oauth_context: Some(OAuthContext {
                        pkce_pair: PkcePair { code_verifier: "v".into(), ..Default::default() },
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
                "manual",
            )
            .expect("账号应当能添加");
        store
    }

    fn only_account_id(store: &AccountStore) -> String {
        store.codearts_account_record("").expect("刚添加的账号要能读回")["id"].as_str().unwrap().to_string()
    }

    fn delivery_of(items: Vec<Value>) -> Value {
        json!({"code": 0, "data": {"items": items}})
    }

    fn item(id: &str, claimable: bool, status: &str) -> Value {
        json!({"campaignId": id, "type": "USER_LOGIN", "claimable": claimable, "status": status, "benefitUnit": "CREDIT", "benefitAmount": 100})
    }

    #[tokio::test]
    async fn a_full_claim_persists_the_key_before_the_write_and_verifies_after() {
        let store = store_with("u1");
        let account_id = only_account_id(&store);
        let upstream = mock_upstream(vec![
            delivery_of(vec![item("c1", true, "")]),                        // 探测
            json!({"code": 0, "data": {"campaignId": "c1"}}),               // claim
            json!({"code": 0, "data": {}}),                                  // confirm
            delivery_of(vec![item("c1", false, "CONFIRMED")]),              // 回读二次确认
        ])
        .await;

        let outcome = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect("整条流程该走通");
        assert_eq!(Outcome::Confirmed, outcome);

        let seen = upstream.seen.lock().unwrap().clone();
        assert_eq!(4, seen.len(), "四步各一次：{:?}", seen.iter().map(|line| line.split_whitespace().take(2).collect::<Vec<_>>()).collect::<Vec<_>>());
        assert!(seen[0].starts_with("GET /v1/ops/delivery"), "第一步必须是只读探测");
        assert!(seen[1].contains("idempotentKey"), "领取请求要带幂等键");
        assert!(seen[2].starts_with("POST /v1/ops/confirm"), "领完必须 confirm");
        assert!(seen[3].starts_with("GET /v1/ops/delivery"), "confirm 之后必须回读列表");

        // 幂等键在写请求**之前**就得在盘上：崩溃后重启不能换一个键再领一次
        let ledger = store.codearts_welfare_ledger(&account_id).expect("台账要落盘");
        assert_eq!(1, ledger["attempts"].as_i64().unwrap());
        assert_eq!(true, ledger["accepted"].as_bool().unwrap());
        let progress = &ledger["campaigns"]["c1"];
        assert_eq!(true, progress["claimed"].as_bool().unwrap());
        assert_eq!(true, progress["confirmed"].as_bool().unwrap());
        assert!(progress["idempotentKey"].as_str().unwrap_or("").starts_with("claim_c1_"));
    }

    #[tokio::test]
    async fn accepted_but_not_credited_is_reported_as_failure() {
        // 三个写请求全部 code:0，但回读时活动还是 claimable —— 这就是「受理 ≠ 到账」
        let store = store_with("u2");
        let account_id = only_account_id(&store);
        let upstream = mock_upstream(vec![
            delivery_of(vec![item("c2", true, "")]),
            json!({"code": 0, "data": {"campaignId": "c2"}}),
            json!({"code": 0, "data": {}}),
            delivery_of(vec![item("c2", true, "")]),
        ])
        .await;
        let error = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect_err("没到账就不能报成功");
        assert!(error.message.contains("尚未确认到账"), "文案要说明卡在哪：{}", error.message);
        assert_eq!(502, error.status_code);
        let ledger = store.codearts_welfare_ledger(&account_id).unwrap();
        assert_eq!(false, ledger["accepted"].as_bool().unwrap(), "未确认不得记为已到账");
        assert_eq!(1, ledger["attempts"].as_i64().unwrap(), "这次写尝试要计进限流");
    }

    #[tokio::test]
    async fn a_mismatched_campaign_id_in_the_claim_response_stops_before_confirm() {
        let store = store_with("u3");
        let account_id = only_account_id(&store);
        let upstream = mock_upstream(vec![
            delivery_of(vec![item("c3", true, "")]),
            json!({"code": 0, "data": {"campaignId": "someone-else"}}),
        ])
        .await;
        let error = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect_err("活动 id 对不上必须失败");
        assert!(error.message.contains("不匹配"), "{}", error.message);
        assert_eq!(2, upstream.seen.lock().unwrap().len(), "绝不能去 confirm 别人的活动");
    }

    #[tokio::test]
    async fn a_nonzero_code_is_never_recorded_as_claimed() {
        let store = store_with("u4");
        let account_id = only_account_id(&store);
        let upstream = mock_upstream(vec![
            delivery_of(vec![item("c4", true, "")]),
            json!({"code": 4001, "data": null}),
        ])
        .await;
        let error = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect_err("code!=0 是失败");
        assert!(error.message.contains("未确认成功"), "{}", error.message);
        let ledger = store.codearts_welfare_ledger(&account_id).unwrap();
        assert_eq!(
            false,
            ledger["campaigns"]["c4"]["claimed"].as_bool().unwrap_or(false),
            "没领成就不能把 claimed 记成 true"
        );
    }

    #[tokio::test]
    async fn already_confirmed_upstream_short_circuits_without_any_write() {
        let store = store_with("u5");
        let account_id = only_account_id(&store);
        let upstream = mock_upstream(vec![delivery_of(vec![item("c5", false, "CONSUMED")])]).await;
        let outcome = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), false).await.expect("已确认不是错误");
        assert_eq!(Outcome::Already, outcome);
        assert_eq!(1, upstream.seen.lock().unwrap().len(), "只读一次，不发任何写请求");
    }

    #[tokio::test]
    async fn the_local_rate_gate_blocks_writes_but_manual_still_runs() {
        let store = store_with("u6");
        let account_id = only_account_id(&store);
        // 先把台账写成「今天已经试过 6 次」
        let account_id_for_ledger = only_account_id(&store);
        store
            .put_codearts_welfare_ledger(
                &account_id_for_ledger,
                &json!({"version": LEDGER_VERSION, "day": today(logging::now_ms()), "attempts": MAX_ATTEMPTS_PER_DAY,
                        "lastAttemptMs": logging::now_ms(), "campaigns": {}}),
            )
            .unwrap();
        // 脚本按「自动一次只读 + 手动一整轮」排：自动那趟只消费 1 条
        let upstream = mock_upstream(vec![
            delivery_of(vec![item("c6", true, "")]), // 自动：读列表后被限流挡住
            delivery_of(vec![item("c6", true, "")]), // 手动：重新读资格
            json!({"code": 0, "data": {"campaignId": "c6"}}),
            json!({"code": 0, "data": {}}),
            delivery_of(vec![item("c6", false, "CONFIRMED")]),
        ])
        .await;
        let outcome = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), false).await.expect("限流不是错误");
        assert_eq!(Outcome::Skipped, outcome);
        assert_eq!(1, upstream.seen.lock().unwrap().len(), "自动路径被挡住时只读列表，不发写请求");

        // 手动：不受这两个闸约束（读资格永远放行，写也允许用户主动补一次）
        let outcome = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect("手动该继续");
        assert_eq!(Outcome::Confirmed, outcome, "手动这一次要真的把流程走完");
        assert_eq!(5, upstream.seen.lock().unwrap().len(), "自动 1 次 + 手动 4 步");
    }

    #[tokio::test]
    async fn an_unreadable_ledger_refuses_to_claim_instead_of_starting_over() {
        // 「读不懂就当空台账」看起来是自我修复，实际是把上一轮的幂等键丢了 ——
        // 崩溃后重试会被上游当成另一笔领取。所以这里必须**一个请求都不发**。
        let store = store_with("u8");
        let account_id = only_account_id(&store);
        store
            .put_codearts_welfare_ledger(&account_id, &json!({"version": 999, "day": "2026-09-27", "attempts": 0}))
            .unwrap();
        let upstream = mock_upstream(vec![delivery_of(vec![item("c8", true, "")])]).await;
        let error = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true)
            .await
            .expect_err("无效台账必须拒绝");
        assert!(error.message.contains("台账无效"), "{}", error.message);
        assert_eq!(0, upstream.seen.lock().unwrap().len(), "连只读的探测都不该发出去");
    }

    #[tokio::test]
    async fn a_ledger_from_the_future_refuses_to_claim() {
        let store = store_with("u9");
        let account_id = only_account_id(&store);
        store
            .put_codearts_welfare_ledger(
                &account_id,
                &json!({"version": LEDGER_VERSION, "day": "2999-01-01", "attempts": 1, "campaigns": {}}),
            )
            .unwrap();
        let upstream = mock_upstream(vec![]).await;
        let error = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect_err("时钟倒退必须拒绝");
        assert!(error.message.contains("早于领取记录"), "{}", error.message);
        assert_eq!(0, upstream.seen.lock().unwrap().len());
    }

    #[tokio::test]
    async fn registration_rewards_are_left_alone() {
        let store = store_with("u7");
        let account_id = only_account_id(&store);
        let upstream = mock_upstream(vec![delivery_of(vec![json!({
            "campaignId": "r1", "type": "REGISTER", "claimable": true, "status": "", "benefitUnit": "CREDIT"
        })])])
        .await;
        let outcome = claim_account(&store, &account_id, &upstream.base, logging::now_ms(), true).await.expect("没有可领不是错误");
        assert_eq!(Outcome::NotEligible, outcome);
        assert_eq!(1, upstream.seen.lock().unwrap().len(), "一次性奖励绝不自动领");
    }
}
