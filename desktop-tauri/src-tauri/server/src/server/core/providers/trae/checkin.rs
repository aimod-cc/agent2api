//! Trae 的每日签到（`checkin_credits/status` + `.../claim`）。
//!
//! ── 移植来源与判据出处 ────────────────────────────────────
//! 参考实现 `cpa-multi-plugins/plugins/trae`：
//!   · 协议层 `upstream/client.go`（`CheckinStatus` :563-772、`CheckinClaim` :775-850、
//!     探测体表 :594-607、`NewCheckinDeviceID` :649-665）；
//!   · 节奏层 `scheduler/scheduler.go`（:20-39 的两段定案说明、:120-221 的一轮流程、
//!     :225-231 的退避表）；
//!   · 面板层 `management.go`（:542-741 的手动签到端点、:613-623 的无 deviceId 硬拦、
//!     :705-771 的「回查未翻转就不算成功」）。
//! 本模块按这三层的**结论**实现，并把它们各自的理由写在这里 —— 这一条链上
//! 每一步看着都可以"顺手简化"，而每一处简化的后果都是官方风控。
//!
//! ── 为什么以前这里写着「不做签到」，现在改了 ─────────────────
//! `usage.rs` 模块头留过一句「参考实现那条链有把出口 IP 打进封禁的前科，而且
//! 签到钱包与模型调用真正扣的积分池是两笔钱，本模块因此完全不碰它」。两笔钱
//! 这半句是对的（`management.go:260-261` 明说 v0.12.34 之前把两者混着显示是 bug），
//! 但**结论**下错了：`panel.html:304` 写的是「SOLO 已转积分制 —— 模型调用消耗
//! 签到钱包积分」，`scheduler.go:206` 给池子打分时也算上它（「池子积分 =
//! 套餐剩余 + 签到钱包」）。也就是说签到到账的那一份确实是 SOLO 转发在花的钱，
//! 不做签到等于每天白丢一笔额度。
//! 前科那半句的真正内容也不是"签到会被封"，而是**签到姿势**会被风控：
//!   · 设备号复用（`scheduler.go:36-39`：同号"可疑诱发"）；
//!   · `req_source` 与令牌的产品谱系错配（`scheduler.go:25-28`：v0.12.41 反编译
//!     官方双版定案，错配即 9074，并非单纯名额限流）；
//!   · 同一账号混用多套客户端身份（`upstream/headers.go:67`：画像对不上）。
//! 这三条都有确定的做对方式，下面逐条钉住，因此本家的态度从"不碰"改成
//! "**按定稿姿势碰，且每天只花该花的几次请求**"。
//!
//! ── 一轮到底发几个请求（这是本模块的安全边界）──────────────────
//! 状态 1 发 + （未签时）领取 1 发 + 回查 1 发 = **常态 3 发**，最坏（探测体
//! 全 9074 或鉴权方案要回退）每发内部最多 3 次尝试。本模块**不**实现参考实现
//! 那套当日指数退避重试定时器（1m→2m→…→2h，当日 10 次，`scheduler.go:225-231`）：
//! 那套是为「0 点整官方重置瞬间的洪峰」调的，而本仓的定时签到时刻由用户在
//! 设置页决定（默认 00:01），再叠一层后台重试会把「一天三次」变成「一天几十次」，
//! 恰好是上面第 1 条前科的成因。撞 9074 时本模块如实回报、当日不再自动重试，
//! 用户想再试就点面板那颗按钮 —— 一次点击一轮，行为可预期。
//!
//! ── claim 的 `code:0` 是**假成功**陷阱 ──────────────────────
//! `scheduler.go:37-38`：当日已签过的账号对**任何** device_id 都回 `code:0`。
//! 所以判"这次真领到了"不看 claim 的码，只看**同一 device_id 回查**之后
//! `checked_in` / `did_checked_in` 有没有翻转（`:186-196` 把未翻转明确计成一次
//! 失败并纳入重试）。本模块照做：回查没确认就报 `success:false`，绝不报成
//! 「签到成功」——那会让面板亮绿、当日台账落库，而实际什么也没到账。
//!
//! ── claim 形状与其他几家对齐 ───────────────────────────────
//! 返回 `{success, msg, ...}`，`billing::checkin` 的汇总与 `mark_checkin` 只认
//! 这两个字段（见那里的 `checkin_completed_today`），因此本家不另立口径：
//! 今日已签 → `success:false` + `alreadyCompleted:true` + msg 带「今日已签到」；
//! 真领到 → `success:true` + msg 带具体收益数字（与小浣熊/AutoClaw 同一取向，
//! 数字进日志才有排查价值）。

use std::time::Duration;

use serde_json::{Value, json};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::adapter::{account_proxy, read_record, renew_if_due};
use super::credentials::Credential;
use super::errors::classify;
use super::http::post_json;
use super::usage::{UG_HOST, ug_headers};

/// 签到状态查询（参考实现 `EpCheckinStatus`）。
const EP_CHECKIN_STATUS: &str = "/trae/api/v2/ug/checkin_credits/status";
/// 领取（参考实现 `EpCheckinClaim`）。
const EP_CHECKIN_CLAIM: &str = "/trae/api/v2/ug/checkin_credits/claim";

/// 业务码：成功。
const CODE_OK: i64 = 0;
/// 业务码：`当前参与用户太多，请稍后再试`。
///
/// ⚠️ 这个名字是本家踩过的最大的一个坑：它**不只是**名额限流。参考实现
/// v0.12.41 反编译官方两个版本后定案 —— `req_source` 与令牌的产品谱系错配
/// 同样回 9074（`scheduler.go:25-28`）。所以拿到 9074 的第一反应应该是
/// 「谱系探测对不对」，而不是「等一会儿再试」。本模块因此按参考实现的做法
/// 把探测体**逐个试**（见 `probe_bodies`），而不是只发一个就下结论。
const CODE_REJECTED: i64 = 9074;

/// 单次请求超时。与 `usage.rs` 同一个数：这条族没有流式，`egress` 默认的
/// 600 秒读超时（那是留给 SSE 的）会把一次批量签到拖成整页转圈。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 错误体进消息的截断长度（与 `usage.rs` 同一口径）。
const ERROR_BODY_HEAD: usize = 200;

/// 鉴权方案。参考实现 `client.go:563-566,639-641,692-696`：
/// `Cloud-IDE-JWT` 优先，`Bearer` 兜底；非 9074 的业务码才换方案（9074 换体）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Jwt,
    Bearer,
}

impl Scheme {
    fn next(self) -> Option<Scheme> {
        match self {
            Self::Jwt => Some(Self::Bearer),
            Self::Bearer => None,
        }
    }

    fn authorization(self, access_token: &str) -> String {
        match self {
            Self::Jwt => format!("Cloud-IDE-JWT {access_token}"),
            Self::Bearer => format!("Bearer {access_token}"),
        }
    }
}

/// 该谱系的探测体序列（参考实现 `client.go:594-607` 的字面对齐）。
///
/// `req_source` 是**客户端产品谱系**（1=TRAE IDE / 2=SOLO、TraeWork），
/// 不是用户的套餐档位（`:579-587`）。我们这条通道是 SOLO，所以 `2` 必须排第一；
/// 排在后面是兜底：万一某账号其实是 IDE 谱系签的发，第一条会 9074，第二条才对。
/// 空体 `{}` 也在序列里（上游对缺省值的处理是第三条退路）。
pub fn probe_bodies(variant: &str) -> Vec<Value> {
    match variant {
        "solo" => vec![json!({ "req_source": 2 }), json!({ "req_source": 1 }), json!({})],
        // cn（TRAE IDE 谱系）：1 优先，缺省次之，SOLO 号最后（参考实现同一顺序）
        _ => vec![json!({ "req_source": 1 }), json!({}), json!({ "req_source": 2 })],
    }
}

/// 一轮签到的**设备号**：16 位随机数字串。
///
/// 三个约束都来自参考实现的实测（`upstream/checkin_device_test.go` 钉住）：
///   · 登录那套 `hex32` 设备号或 `machineId` 去打签到 → **必败 9074**；
///   · 空值 → 9004；
///   · **复用**上一轮的号 → 风控画像里的可疑项（`scheduler.go:36-39`）。
/// 所以这里是每轮新生成，并在 status → claim → 回查这**三发之间保持不变**
/// （回查要按同一个号问「今天这个设备签过没有」，换号回查等于自证失败）。
pub fn checkin_device_id() -> String {
    let mut digits = String::with_capacity(16);
    let mut buffer = [0u8; 32];
    // 逐字节取模会偏向 0-5（256 不是 10 的整数倍），因此丢掉 250-255 这六个值
    // —— 这不是洁癖：设备号的空间是 10^16，分布歪了就是可被统计出来的画像特征。
    while digits.len() < 16 {
        if getrandom::getrandom(&mut buffer).is_err() {
            // 系统随机源不可用：退回**时间戳数字**（仍是一个一次性号）。
            // 宁可用一个熵低的号签一次，也不能复用旧号 —— 后者才是可疑项。
            let stamp = crate::server::logging::now_ms().to_string();
            return stamp
                .chars()
                .filter(|ch| ch.is_ascii_digit())
                .take(16)
                .collect();
        }
        for byte in buffer {
            if byte < 250 {
                digits.push(char::from(b'0' + byte % 10));
                if digits.len() == 16 {
                    break;
                }
            }
        }
    }
    digits
}

/// `checkin_credits/status` 响应里本模块要用的字段。
///
/// `credits` / `extra_credits` 是**本次奖励数额**（`management.go:659,703,931`
/// 反复强调它不是钱包余额、签到后不增长），界面与日志都按"今日奖励"来写，
/// 别把它当余额显示 —— 那是参考实现修过的一个真实显示 bug。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub checked_in: bool,
    pub did_checked_in: bool,
    pub enable: bool,
    pub credits: i64,
    pub extra_credits: i64,
    pub code: i64,
    pub message: String,
}

impl Status {
    /// 「今天这个账号已经算签过了」——两个维度任一为真即真。
    ///
    /// `did_checked_in` 是**设备维度**（这个号今天签过），`checked_in` 是账号维度。
    /// 参考实现两者都认（`scheduler.go:169`、`management.go` 的 `already_checked_in`）。
    pub fn done_today(&self) -> bool {
        self.checked_in || self.did_checked_in
    }

    /// 今日奖励的两个分量（合计口径见 `Award::total`）。
    pub fn award(&self) -> Award {
        Award { credits: self.credits, extra_credits: self.extra_credits }
    }
}

/// 响应体 → `Status`。缺字段一律按 `false` / `0` 收（**不猜**：`enable` 缺省
/// 按"没开"处理最保守，宁可少签一次也不多打一发请求）。
pub fn status_of(payload: &Value) -> Status {
    let number = |key: &str| payload.get(key).and_then(Value::as_i64).unwrap_or(0);
    Status {
        checked_in: flag(payload, "checked_in"),
        did_checked_in: flag(payload, "did_checked_in"),
        enable: flag(payload, "enable"),
        credits: number("credits"),
        extra_credits: number("extra_credits"),
        code: payload.get("code").and_then(Value::as_i64).unwrap_or(CODE_OK),
        message: payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
    }
}

fn flag(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 一次往返后的业务码判定：`true` = 采纳这份响应（码为 0 或体里有可用字段）。
///
/// 参考实现在这里分三路（`client.go:744-746,810-812`）：码 0 → 收；
/// 码 9074 → **换下一个探测体**（同方案）；其它非零 → **换下一个鉴权方案**。
/// 本函数把这个决策做成纯函数，好让那条分派被单测钉住 —— 它决定的是
/// 「谱系错配时能不能自己找回正确的 req_source」，写成循环里的 if 就没人测得到。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// 采纳这一发
    Accept,
    /// 9074：换下一个探测体（体用尽则整体失败）
    NextBody,
    /// 其它非零码：换下一个鉴权方案（方案用尽则整体失败）
    NextScheme,
}

pub fn probe_for(code: i64) -> Probe {
    match code {
        CODE_OK => Probe::Accept,
        CODE_REJECTED => Probe::NextBody,
        _ => Probe::NextScheme,
    }
}

/// 状态 → 领取 → 回查 的**结论**（与网络层解耦，便于把判定表单测钉死）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 上游没给这个账号开签到活动（中性，不算失败也不算成功）
    NotEnabled,
    /// 今天已经签过了（`already` 带的是当日奖励数额，可能为 0）
    AlreadyCheckedIn(Award),
    /// 本次真领到了（回查已确认），参数为奖励数额
    Claimed(Award),
    /// 领取被拒（`code` 是上游业务码，9074 时文案要说明"多半是谱系/风控"）
    Rejected(i64, String),
    /// claim 回了 code:0 但同设备号回查没翻转 —— 服务端静默拒签，**不算成功**
    Unconfirmed,
}

/// 拿到状态与（必要时）领取+回查后的判定。
///
/// `before` 是本轮第一发状态；`after` 是领取后用**同一个设备号**的回查；
/// `claim` 是领取那一发的状态（回查失败时为 None，此时不能假装成功）。
pub fn decide(before: &Status, claim: i64, after: Option<&Status>) -> Outcome {
    if !before.enable {
        return Outcome::NotEnabled;
    }
    if before.done_today() {
        return Outcome::AlreadyCheckedIn(before.award());
    }
    if claim != CODE_OK {
        return Outcome::Rejected(claim, String::new());
    }
    match after {
        // 回查确认：奖励数按回查那份读（官方在 claim 里并不保证给数额）
        Some(confirmed) if confirmed.done_today() => Outcome::Claimed(confirmed.award()),
        Some(_) | None => Outcome::Unconfirmed,
    }
}

/// Trae 账号的每日签到。
///
/// 与 `query_usage` 同一条凭证链：读记录 → 组凭据 → 解代理 → **临期先续期**
/// （参考实现 `scheduler.go:137-143` 的"签到前保鲜"：token 过期时签到必撞会话类
/// 错误，而那一发错误看起来像"账号不行了"，会把健康账号拖进冷却）。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let record = read_record(store, account_id)?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let proxy = account_proxy(&record)?;
    let credential = renew_if_due(store, &record, &credential, proxy.as_ref()).await?;
    run(&credential, proxy.as_ref(), UG_HOST).await
}

/// 一轮签到（凭证 → 结果行）。`base` 让单测能把这三发指向假上游：
/// 生产上它是 `UG_HOST` 常量，而**这条链最容易写错的就是 host**（打错 host 的
/// 失败形态是 401「unable to authenticate」而不是 404，看不出是 host 问题）。
async fn run(
    credential: &Credential,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
    base: &str,
) -> Result<Value, GatewayError> {
    let variant = credential.variant().to_string();
    if variant == "intl" || variant == "solo-intl" {
        // 参考实现里国际版**根本没有签到**（`intlupstream/` 无 checkin、
        // `intl_panel.html:138` 明写「无签到 · 无积分池」）。本仓也不接 intl 转发，
        // 走到这里只可能是手工粘进来的谱系，如实说明而不是打一发注定 404 的请求。
        return Ok(json!({
            "success": false,
            "msg": "国际版 Trae 没有签到通道（上游无此活动）",
        }));
    }
    if credential.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(401, "该 Trae 账号没有可用凭证，无法签到"));
    }
    if credential.device_id.trim().is_empty() {
        // 参考实现对缺 deviceId 的凭据是**硬拦**（`management.go:613-623`）：
        // 这类凭据多半是手工粘贴的，服务端侧没有设备绑定，签到正好落在
        // 「is_login=false / 绑定不一致 = 9074 高危画像」那一档（`management.go:875-890`）。
        // 与其打一发去撞风控，不如把话说明白：重新走一次网页登录再签。
        return Ok(json!({
            "success": false,
            "msg": "该账号没有 deviceId（多半是手工粘贴的凭据），官方按设备画像风控，请先重新登录再签到",
        }));
    }

    // 一轮一个号，三发之间不变（见 `checkin_device_id` 的说明）
    let device_id = checkin_device_id();
    let before = status(base, credential, &variant, &device_id, proxy).await?;
    if !before.enable || before.done_today() {
        return Ok(complete(decide(&before, CODE_OK, None), &device_id));
    }
    let claimed = claim(base, credential, &variant, &device_id, proxy).await?;
    if claimed != CODE_OK {
        return Ok(complete(decide(&before, claimed, None), &device_id));
    }
    // 同设备号回查。回查本身失败（网络/会话）时按"未确认"处理 ——
    // 一发没被确认的 claim 报成成功，就是给用户一个假成功。
    let after = status(base, credential, &variant, &device_id, proxy).await.ok();
    Ok(complete(decide(&before, claimed, after.as_ref()), &device_id))
}

/// 今日奖励的**两部分**（基础 + 加成）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Award {
    pub credits: i64,
    pub extra_credits: i64,
}

impl Award {
    /// 参考实现给界面看的合计数（`scheduler.go:181-184` 就是两者相加）。
    pub fn total(&self) -> i64 {
        self.credits + self.extra_credits
    }
}

/// 判定 → 界面与日志消费的 `{success, msg, ...}`。
///
/// 设备号一并带回（`checkinDeviceId`）：这一轮用的哪个号，是唯一能把
/// 「上游回了 9074」与「官方客户端同日替用户签了」这两件事在后端日志里
/// 分开的线索（参考实现的面板结果里也有这一项）。
///
/// ⚠️ 数字口径：`credits`/`extra_credits` 是上游**报出的当日奖励数额**，
/// 不是到账余额 —— 参考实现为此改过两版（`management.go:931`「仅作展示与
/// '已签'判定」，`panel.html:808` 把"code:0 但余额未增长"单列成一种结果）。
/// 2026-09-30 生产实测也是这个形状：一个 Free 的 SOLO 账号报 200（100+100），
/// 积分池只长了 100（`total_amount` 1850→1950）。所以这里把两个分量原样透出，
/// 文案说"官方报出的当日奖励"，把"实际到账看余额列"这件事留在界面上已经
/// 存在的余额列里，不用一句会撒谎的成功文案替它背书。
fn complete(outcome: Outcome, device_id: &str) -> Value {
    let mut row = serde_json::Map::new();
    let awarded = match outcome {
        Outcome::NotEnabled => {
            row.insert("success".into(), json!(false));
            row.insert(
                "msg".into(),
                json!("当前没有可领取的签到活动（上游未对该账号开放这一档）"),
            );
            None
        }
        Outcome::AlreadyCheckedIn(award) => {
            row.insert("success".into(), json!(false));
            // `alreadyCompleted` 是给 `billing::checkin::checkin_completed_today`
            // 看的：今天已经签过 = 当日台账要落库，定时链明天才会照常再试
            // 而不是把"已签"当成失败反复重打。
            row.insert("alreadyCompleted".into(), json!(true));
            row.insert(
                "msg".into(),
                json!(if award.total() > 0 {
                    format!("今日已签到（官方报出当日奖励 {} 积分）", award.total())
                } else {
                    "今日已签到".to_string()
                }),
            );
            Some(award)
        }
        Outcome::Claimed(award) => {
            row.insert("success".into(), json!(true));
            row.insert(
                "msg".into(),
                json!(if award.total() > 0 {
                    format!(
                        "签到成功（官方报出当日奖励 {} 积分；到账看余额列）",
                        award.total()
                    )
                } else {
                    // 回查确认了"今天签过"，但上游没给数额 —— 不编一个数字出来
                    "签到成功（上游未给出奖励数额）".to_string()
                }),
            );
            Some(award)
        }
        Outcome::Rejected(code, message) => {
            row.insert("success".into(), json!(false));
            row.insert("code".into(), json!(code));
            row.insert(
                "msg".into(),
                json!(if code == CODE_REJECTED {
                    format!(
                        "官方拒绝本次签到（{code}）：{}探测体与两种鉴权方案都试过；\
                         这一码多数是设备画像/产品谱系问题，不是名额拥挤，当日不再自动重试",
                        if message.is_empty() {
                            "上游未给文案"
                        } else {
                            message.as_str()
                        }
                    )
                } else {
                    format!("官方拒绝本次签到（{code}）：{message}")
                }),
            );
            None
        }
        Outcome::Unconfirmed => {
            row.insert("success".into(), json!(false));
            row.insert("claimUnconfirmed".into(), json!(true));
            row.insert(
                "msg".into(),
                json!("签到返回成功但同设备号回查未确认（服务端静默拒签），当日不再自动重试"),
            );
            None
        }
    };
    if let Some(award) = awarded {
        row.insert("awarded".into(), json!(award.total()));
        row.insert("checkinCredits".into(), json!(award.credits));
        row.insert("checkinExtraCredits".into(), json!(award.extra_credits));
    }
    row.insert("checkinDeviceId".into(), json!(device_id));
    Value::Object(row)
}

/// 状态查询（一轮里会发两次：领取前与领取后回查）。
async fn status(
    base: &str,
    credential: &Credential,
    variant: &str,
    device_id: &str,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
) -> Result<Status, GatewayError> {
    let payload = round_trip(base, EP_CHECKIN_STATUS, variant, device_id, credential, proxy).await?;
    Ok(status_of(&payload))
}

/// 领取。只回业务码 —— 上游在 claim 里不保证给奖励数额，数额由回查读。
async fn claim(
    base: &str,
    credential: &Credential,
    variant: &str,
    device_id: &str,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
) -> Result<i64, GatewayError> {
    let payload = round_trip(base, EP_CHECKIN_CLAIM, variant, device_id, credential, proxy).await?;
    Ok(payload.get("code").and_then(Value::as_i64).unwrap_or(CODE_OK))
}

/// 一发「方案 × 探测体」的往返，返回被采纳的那份响应体。
///
/// 分派规则照参考实现：`code:0` 采纳；`9074` 换下一个探测体（同方案）；
/// 其它非零码换下一个鉴权方案。全部用尽仍非零时**返回最后一份**响应，
/// 由调用方按业务码判定 —— 签到失败不是网关错误，不该冒成 5xx。
/// 只有 HTTP 层的会话失效（401 一类）才往上抛 `GatewayError`，让批量签到
/// 把这一行报成失败并给出去重登的提示。
async fn round_trip(
    base: &str,
    endpoint: &str,
    variant: &str,
    device_id: &str,
    credential: &Credential,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let url = format!("{base}{endpoint}");
    let bodies = probe_bodies(variant);
    let mut scheme = Scheme::Jwt;
    let mut last = json!({});
    loop {
        for body in &bodies {
            let reply =
                post_checkin(&url, body, variant, device_id, &credential.access_token, scheme, proxy)
                    .await?;
            let payload = reply.json().unwrap_or_else(|| json!({}));
            let code = payload.get("code").and_then(Value::as_i64).unwrap_or(CODE_OK);
            last = payload;
            match probe_for(code) {
                Probe::Accept => return Ok(last),
                Probe::NextBody => continue,
                Probe::NextScheme => break,
            }
        }
        let Some(next) = scheme.next() else {
            return Ok(last);
        };
        scheme = next;
    }
}

/// 真正发出去的那一发：ug 画像 + 指定鉴权方案 + 本轮固定设备号。
///
/// HTTP 层的状态码在这里就判成错误（与 `usage.rs` 的 `post_ug` 同一口径）：
/// ug 族带着 `Cloud-IDE-JWT` 在生产上是**通**的（余额读数实测），所以一个 401
/// 的含义是"这条会话真的死了"，而不是"该换 Bearer 了"——把会话失效伪装成
/// 一次方案回退，代价是批量签到里一行本该报"去重登"的记录变成两次无谓往返。
/// 业务码的分派（9074 换体、其它非零换方案）在 `round_trip` 那一层。
async fn post_checkin(
    url: &str,
    body: &Value,
    variant: &str,
    device_id: &str,
    access_token: &str,
    scheme: Scheme,
    proxy: Option<&crate::server::core::proxies::ResolvedProxy>,
) -> Result<super::http::Reply, GatewayError> {
    let mut headers = ug_headers(variant, access_token, device_id);
    // `ug_headers` 写的是 Cloud-IDE-JWT；换 Bearer 兜底时**只换这一个头**，
    // 其余画像头保持逐字一致（混改会撞上 headers.go:67 说的"画像对不上"）。
    headers.insert(
        "Authorization".to_string(),
        scheme.authorization(access_token),
    );
    let prepared: Vec<(String, String)> = headers.into_iter().collect();
    let pairs: Vec<(&str, String)> = prepared
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let reply = post_json(url, body, &pairs, REQUEST_TIMEOUT, proxy).await?;
    if reply.status >= 400 {
        let head: String = reply.body.chars().take(ERROR_BODY_HEAD).collect();
        let kind = classify(reply.status, &head);
        return Err(GatewayError::with_status(
            i32::from(super::forward::status_for(kind)),
            format!("Trae 签到接口返回 {}: {}", reply.status, head.trim()),
        ));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_solo_lineage_probes_the_solo_source_first() {
        // 我们这条通道只有 SOLO。探测顺序错了的后果不是报错，而是每天一发 9074
        //（参考实现 v0.12.41 的定案：req_source 必须与令牌谱系一致）。
        let solo = probe_bodies("solo");
        assert_eq!(Some(2), solo[0].get("req_source").and_then(Value::as_i64), "SOLO 首发必须是 req_source=2");
        assert_eq!(Some(1), solo[1].get("req_source").and_then(Value::as_i64));
        assert!(solo[2].as_object().is_some_and(|map| map.is_empty()), "第三条退路是空体");
        assert_eq!(3, solo.len());

        // IDE 谱系反过来（照参考实现的另一条序列）
        let cn = probe_bodies("cn");
        assert_eq!(Some(1), cn[0].get("req_source").and_then(Value::as_i64));
        assert_eq!(Some(2), cn[2].get("req_source").and_then(Value::as_i64));
    }

    #[test]
    fn every_round_gets_a_fresh_sixteen_digit_device_id() {
        let first = checkin_device_id();
        let second = checkin_device_id();
        assert_eq!(16, first.len(), "长度必须是 16（上游按这个形态校验）");
        assert!(
            first.chars().all(|ch| ch.is_ascii_digit()),
            "必须是**数字**串：登录那套 hex32 打签到实测必败 9004/9074"
        );
        assert_ne!(first, second, "同一轮之间可以相同，跨轮必须换号（复用号是风控可疑项）");
        assert_eq!(16, second.len());
    }

    #[test]
    fn code_9074_switches_the_probe_body_and_any_other_code_switches_the_scheme() {
        assert_eq!(Probe::Accept, probe_for(0));
        assert_eq!(Probe::NextBody, probe_for(CODE_REJECTED), "9074 先怀疑谱系，换体不换方案");
        assert_eq!(Probe::NextScheme, probe_for(1001));
        assert_eq!(Probe::NextScheme, probe_for(9004), "缺 did 那类码不是拥挤，换方案也无用但要按非拥挤路径收尾");
        assert_eq!(Some(Scheme::Bearer), Scheme::Jwt.next());
        assert_eq!(None, Scheme::Bearer.next(), "两种方案都试过就收，不能无限重试");
    }

    #[test]
    fn the_authorization_alternates_without_touching_the_profile() {
        assert_eq!(
            "Cloud-IDE-JWT T",
            Scheme::Jwt.authorization("T"),
            "ug 族默认形态（与转发用的同一串前缀）"
        );
        assert_eq!("Bearer T", Scheme::Bearer.authorization("T"));
    }

    #[test]
    fn status_fields_default_to_the_conservative_reading() {
        let absent = status_of(&json!({}));
        assert_eq!(Status::default(), absent, "缺字段一律按 false/0 收：enable 缺省=没开，宁可少签一次");
        let parsed = status_of(&json!({
            "code": 0,
            "message": "ok",
            "checked_in": true,
            "did_checked_in": false,
            "enable": true,
            "credits": 200,
            "extra_credits": 50,
        }));
        assert!(parsed.done_today());
        assert!(!parsed.did_checked_in);
        assert_eq!(200, parsed.award().credits);
        assert_eq!(50, parsed.award().extra_credits);
        assert_eq!(250, parsed.award().total(), "合计 = 基础 + 会员加成（参考实现的同一算式）");
        assert_eq!("ok", parsed.message);
    }

    #[test]
    fn a_device_dim_already_checked_in_still_counts_as_done_today() {
        // 这个字段的意义就是"这个设备号今天签过了"——回查确认走的就是它。
        let status = status_of(&json!({"enable": true, "did_checked_in": true, "credits": 100}));
        assert!(status.done_today());
        assert_eq!(
            Outcome::AlreadyCheckedIn(Award { credits: 100, extra_credits: 0 }),
            decide(&status, CODE_OK, None)
        );
    }

    #[test]
    fn an_disabled_activity_is_a_neutral_result_not_a_failure() {
        let status = status_of(&json!({"enable": false}));
        assert_eq!(Outcome::NotEnabled, decide(&status, CODE_OK, None));
        let row = complete(Outcome::NotEnabled, "1234567890123456");
        assert_eq!(Some(false), row["success"].as_bool());
        assert!(
            row.get("alreadyCompleted").is_none(),
            "没开活动不等于今天已签 —— 台账落错的那天第二天不会再试"
        );
    }

    #[test]
    fn a_claim_that_the_recheck_does_not_confirm_is_never_reported_as_success() {
        // 参考实现的原话：claim code:0 存在幂等假成功（当日已签账号任何 device_id
        // 都回 code:0），回查未翻转视同失败。这里三种"没确认"都必须落 Unconfirmed。
        let open = status_of(&json!({"enable": true}));
        let not_flipped = status_of(&json!({"enable": true, "checked_in": false}));
        assert_eq!(Outcome::Unconfirmed, decide(&open, CODE_OK, Some(&not_flipped)));
        assert_eq!(Outcome::Unconfirmed, decide(&open, CODE_OK, None), "回查本身失败也不算成功");

        let row = complete(Outcome::Unconfirmed, "1111222233334444");
        assert_eq!(Some(false), row["success"].as_bool());
        assert_eq!(Some(true), row["claimUnconfirmed"].as_bool());
        let msg = row["msg"].as_str().unwrap_or_default();
        assert!(msg.contains("回查"), "文案要说清为什么不算成功：{msg}");
    }

    #[test]
    fn a_confirmed_claim_reports_the_award_and_marks_success() {
        let open = status_of(&json!({"enable": true}));
        let after = status_of(&json!({"enable": true, "checked_in": true, "credits": 300, "extra_credits": 100}));
        assert_eq!(Outcome::Claimed(Award { credits: 300, extra_credits: 100 }), decide(&open, CODE_OK, Some(&after)));
        let row = complete(Outcome::Claimed(Award { credits: 300, extra_credits: 100 }), "1111111111111111");
        assert_eq!(Some(true), row["success"].as_bool());
        assert!(row["msg"].as_str().unwrap_or_default().contains("400"), "收益数字要进日志");
        assert_eq!(
            Some("1111111111111111"),
            row["checkinDeviceId"].as_str(),
            "本轮用的设备号要跟着结果走，后端日志才可能归因"
        );
    }

    #[test]
    fn a_zero_award_claim_does_not_invent_a_number() {
        // 上游回查确认了签过、却没给数额时，宁可只说"成功"。
        let row = complete(Outcome::Claimed(Award { credits: 0, extra_credits: 0 }), "2222222222222222");
        assert_eq!(Some(true), row["success"].as_bool());
        let msg = row["msg"].as_str().unwrap_or_default();
        assert!(!msg.contains('0'), "文案里不该出现一个凭空的 0 积分：{msg}");
        assert!(msg.contains("未给出奖励数额"));
    }

    #[test]
    fn the_9074_rejection_message_names_the_actual_cause() {
        let row = complete(Outcome::Rejected(CODE_REJECTED, String::new()), "3333333333333333");
        let msg = row["msg"].as_str().unwrap_or_default();
        assert!(msg.contains("设备画像") || msg.contains("谱系"), "9074 不能只翻成「请稍后再试」：{msg}");
        assert!(msg.contains("不再自动重试"), "要说清当日不重试，免得用户以为后台还在退避");
        assert_eq!(Some(CODE_REJECTED), row["code"].as_i64());
    }

    #[test]
    fn already_signed_rows_tell_the_batch_layer_to_stop_retrying_today() {
        // `billing::checkin::checkin_completed_today` 认 `alreadyCompleted` 或
        // msg 里的「已签到/已领取」——本家的文案必须满足其中一条，否则当日台账
        // 不落库，定时链第二天也会当成"没签过"再打一轮。
        let row = complete(Outcome::AlreadyCheckedIn(Award { credits: 120, extra_credits: 0 }), "4444444444444444");
        assert_eq!(Some(true), row["alreadyCompleted"].as_bool());
        assert!(row["msg"].as_str().unwrap_or_default().contains("已签到"));
    }

    #[test]
    fn the_success_wording_reports_the_official_figure_without_promising_a_balance() {
        // 生产实测（2026-09-30，nas-2.9.0-10）：上游对一个 Free 的 SOLO 账号报
        // 当日奖励 200（credits 100 + extra 100），而积分池只长了 100
        // （`total_amount` 1850→1950、available 527→627）。参考实现自己就把这个
        // 数当"仅作展示与已签判定"（`management.go:931`），并把"code:0 但余额
        // 未增长"单列成一种结果（`panel.html:808`）。所以文案不许说"获得了 200
        // 积分" —— 那是替上游的一个名义数做到账背书。
        let row = complete(
            Outcome::Claimed(Award { credits: 100, extra_credits: 100 }),
            "5555555555555555",
        );
        let msg = row["msg"].as_str().unwrap_or_default();
        assert!(msg.contains("官方报出"), "要标明这个数的出处：{msg}");
        assert!(!msg.contains("获得"), "不许写成到账断言：{msg}");
        assert_eq!(Some(100), row["checkinCredits"].as_i64(), "两个分量要各自可查");
        assert_eq!(Some(100), row["checkinExtraCredits"].as_i64());
        assert_eq!(Some(200), row["awarded"].as_i64(), "合计仍是别家用的那个口径");

        // 已签那一格同一口径
        let already = complete(
            Outcome::AlreadyCheckedIn(Award { credits: 100, extra_credits: 100 }),
            "6666666666666666",
        );
        let text = already["msg"].as_str().unwrap_or_default();
        assert!(text.contains("官方报出") && !text.contains("获得"), "{text}");
    }

    #[test]
    fn the_checkin_endpoints_are_the_ug_family_on_the_fixed_host() {
        // 域名与对话族不同（`trae-api-cn.mchost.guru`）：签到走 api.trae.cn，
        // 拼错 host 的失败形态是 401「unable to authenticate」而不是 404。
        assert_eq!("https://api.trae.cn", UG_HOST);
        assert!(EP_CHECKIN_STATUS.starts_with("/trae/api/v2/ug/"), "{EP_CHECKIN_STATUS}");
        assert!(EP_CHECKIN_CLAIM.starts_with("/trae/api/v2/ug/"), "{EP_CHECKIN_CLAIM}");
    }
    /// 线上形状的一整轮（假上游逐发记录请求）。判定表在上面那些纯函数用例里
    /// 已经钉过，这一组要钉的是**发出去的东西**：探测体的顺序、三发共用同一个
    /// 设备号、鉴权前缀、以及「9074 换体不换方案」。这四件事写错的症状都不是
    /// 报错，而是每天一次静默的 9074 —— 纯函数测试对它们完全无感。
    mod the_wire_shape {
        use super::*;
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Debug)]
        struct Captured {
            path: String,
            body: String,
            authorization: String,
            device_id: String,
        }

        struct FakeUpstream {
            base: String,
            seen: Arc<Mutex<Vec<Captured>>>,
        }

        impl FakeUpstream {
            /// 按脚本回答（一条回答对应一发请求），并记下每发的路径、请求体与
            /// 两个关键头。`status` 用来造 HTTP 层故障（会话真死了那一档）。
            async fn build(status: u16, script: Vec<&str>) -> Self {
                let queue: Arc<Mutex<VecDeque<String>>> =
                    Arc::new(Mutex::new(script.into_iter().map(str::to_string).collect()));
                let seen: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
                let reported = seen.clone();
                let handler = move |request: axum::extract::Request| {
                    let queue = queue.clone();
                    let seen = reported.clone();
                    async move {
                        let path = request.uri().path().to_string();
                        let headers = request.headers().clone();
                        let read = |name: &str| {
                            headers
                                .get(name)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string()
                        };
                        let authorization = read("authorization");
                        let device_id = read("x-device-id");
                        let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
                            .await
                            .unwrap_or_default();
                        let body = String::from_utf8_lossy(&bytes).to_string();
                        seen.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(Captured { path, body, authorization, device_id });
                        let answer = {
                            let mut queue = queue.lock().unwrap_or_else(|p| p.into_inner());
                            match queue.pop_front() {
                                Some(answer) => answer,
                                // 脚本用完还被打：如实回一个失败码，让"多花的那一发"
                                // 在用例里显形，而不是被静默吞成一次正常往返
                                None => r#"{"code":1,"message":"script exhausted"}"#.to_string(),
                            }
                        };
                        axum::response::Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(answer))
                            .expect("假上游响应可构造")
                    }
                };
                let app = axum::Router::new()
                    .route(EP_CHECKIN_STATUS, axum::routing::post(handler.clone()))
                    .route(EP_CHECKIN_CLAIM, axum::routing::post(handler));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("假上游监听");
                let address = listener.local_addr().expect("本地地址");
                tokio::spawn(async move { axum::serve(listener, app).await.ok(); });
                Self { base: format!("http://{address}"), seen }
            }

            async fn spawn(script: Vec<&str>) -> Self {
                Self::build(200, script).await
            }

            fn takes(&self) -> Vec<Captured> {
                self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
            }
        }

        fn credential() -> Credential {
            Credential::from_payload(&json!({
                "accessToken": "TOK",
                "refreshToken": "R",
                "deviceId": "login-device-id",
                "variant": "solo",
            }))
            .expect("测试里的凭据形状是固定的")
        }

        async fn one_round(script: Vec<&str>) -> (Value, Vec<Captured>) {
            let upstream = FakeUpstream::spawn(script).await;
            let row = run(&credential(), None, &upstream.base)
                .await
                .expect("假上游只回 200，不该冒出网关错误");
            (row, upstream.takes())
        }

        #[tokio::test]
        async fn a_healthy_round_is_three_calls_sharing_one_fresh_device_id() {
            let (row, seen) = one_round(vec![
                r#"{"code":0,"enable":true,"checked_in":false,"credits":300,"extra_credits":100}"#,
                r#"{"code":0,"message":"ok"}"#,
                r#"{"code":0,"enable":true,"checked_in":true,"did_checked_in":true,"credits":300,"extra_credits":100}"#,
            ])
            .await;

            assert_eq!(
                Some(true),
                row["success"].as_bool(),
                "回查确认了就该报成功（否则这一组用例是空跑）：{row}"
            );
            assert_eq!(Some(400), row["awarded"].as_i64());
            assert_eq!(3, seen.len(), "状态→领取→回查，正好三发：{seen:?}");
            assert!(seen[0].path.ends_with("/status"), "第一发必须是状态：{}", seen[0].path);
            assert!(seen[1].path.ends_with("/claim"), "第二发必须是领取：{}", seen[1].path);
            assert!(seen[2].path.ends_with("/status"), "第三发是回查：{}", seen[2].path);

            // SOLO 谱系必须第一个就发 req_source=2。顺序错了的失败形态是每天一发
            // 9074，而上游的文案是"参与用户太多" —— 会把人引向"等一等"，真因却是谱系
            assert_eq!(r#"{"req_source":2}"#, seen[0].body.replace(' ', ""), "SOLO 首发探测体");
            for call in &seen {
                assert_eq!("Cloud-IDE-JWT TOK", call.authorization, "ug 族的鉴权前缀");
            }
            // 三发共用**同一个本轮新号**：回查换号等于自证失败；用凭据里那个
            // 登录设备号去打则实测必败（9074 / 9004）
            let device = &seen[0].device_id;
            assert_eq!(16, device.len(), "本轮设备号必须是 16 位：{device}");
            assert!(device.chars().all(|ch| ch.is_ascii_digit()), "必须是数字串：{device}");
            assert_ne!("login-device-id", device, "不许复用凭据里那个登录设备号");
            for call in &seen[1..] {
                assert_eq!(device, &call.device_id, "三发必须同号：{}", call.path);
            }
        }

        #[tokio::test]
        async fn a_9074_retries_the_next_probe_body_on_the_same_scheme() {
            let (row, seen) = one_round(vec![
                r#"{"code":9074,"message":"当前参与用户太多，请稍后再试"}"#,
                r#"{"code":0,"enable":true,"checked_in":false}"#,
                r#"{"code":0}"#,
                r#"{"code":0,"enable":true,"checked_in":true,"credits":50}"#,
            ])
            .await;

            assert_eq!(Some(true), row["success"].as_bool());
            assert_eq!(Some(50), row["awarded"].as_i64());
            assert_eq!(4, seen.len(), "9074 换体后继续走领取+回查：{seen:?}");
            assert_eq!(r#"{"req_source":2}"#, seen[0].body.replace(' ', ""));
            assert_eq!(
                r#"{"req_source":1}"#,
                seen[1].body.replace(' ', ""),
                "9074 的下一步是换**探测体**（同谱系序列的第二条）"
            );
            for call in &seen[..2] {
                assert!(
                    call.authorization.starts_with("Cloud-IDE-JWT "),
                    "9074 不许换鉴权方案（那是另一种失败的分派）：{}",
                    call.authorization
                );
            }
            assert_eq!(seen[0].device_id, seen[3].device_id, "整轮共用一个号");
        }

        #[tokio::test]
        async fn an_idempotent_fake_success_is_never_reported_as_a_claim() {
            // 上游对**当日已签**的账号回 claim `code:0`（且对任何 device_id 都这样）。
            // 这一格是全模块最值钱的用例：少了同设备号回查，面板会亮绿、当日台账
            // 落库，而实际什么都没发生。
            let (row, seen) = one_round(vec![
                r#"{"code":0,"enable":true,"checked_in":false}"#,
                r#"{"code":0,"message":"ok"}"#,
                r#"{"code":0,"enable":true,"checked_in":false,"did_checked_in":false,"credits":0}"#,
            ])
            .await;

            assert_eq!(Some(false), row["success"].as_bool(), "回查没翻转就不能算成功：{row}");
            assert_eq!(Some(true), row["claimUnconfirmed"].as_bool());
            assert_eq!(3, seen.len());
            assert_eq!(seen[0].device_id, seen[2].device_id, "回查必须按同一个号问");
        }

        #[tokio::test]
        async fn an_activity_not_open_to_this_account_claims_nothing() {
            let (row, seen) = one_round(vec![r#"{"code":0,"enable":false}"#]).await;
            assert_eq!(Some(false), row["success"].as_bool());
            assert_eq!(1, seen.len(), "上游说没开活动就收摊，不许再去打领取：{seen:?}");
            assert!(row.get("alreadyCompleted").is_none(), "没开活动不等于今天已签");
        }

        #[tokio::test]
        async fn an_already_signed_account_does_not_claim_again() {
            let (row, seen) =
                one_round(vec![r#"{"code":0,"enable":true,"checked_in":true,"credits":300}"#]).await;
            assert_eq!(Some(true), row["alreadyCompleted"].as_bool());
            assert_eq!(1, seen.len(), "已签就不该再打领取（多一发就是多一次风控画像）");
            assert!(row["msg"].as_str().unwrap_or_default().contains("300"), "当日奖励要显示出来");
        }

        #[tokio::test]
        async fn an_http_level_session_death_is_reported_after_one_call() {
            // 非 2xx（会话真死了）要立刻冒错，而不是把三种探测体 × 两种鉴权方案
            // 都试一遍 —— 那会把一次「去重登」变成六发无谓请求，还顺带把面板的
            // 错误文案换成一句看不懂的"签到失败"。
            let upstream = FakeUpstream::build(
                401,
                vec![r#"{"code":1001,"message":"we are not able to authenticate you"}"#],
            )
            .await;
            let error = run(&credential(), None, &upstream.base)
                .await
                .expect_err("401 必须冒成错误");
            assert_eq!(401, error.status_code, "会话失效要按 401 报，界面才给得出重登提示");
            assert_eq!(1, upstream.takes().len(), "HTTP 层失败不许触发探测体重试");
        }

        #[tokio::test]
        async fn an_account_without_a_device_id_is_not_sent_to_the_upstream() {
            // 参考实现对缺 deviceId 的凭据是硬拦（`management.go:613-623`）：
            // 这类凭据多半手工粘贴、服务端没有设备绑定，打过去正好落在 9074
            // 风控画像那一档。少这一格，"要不要发这一发"就成了实现者的口味。
            let upstream = FakeUpstream::spawn(vec![]).await;
            let credential = Credential::from_payload(&json!({
                "accessToken": "TOK",
                "refreshToken": "R",
                "variant": "solo",
            }))
            .expect("测试里的凭据形状是固定的");
            let row = run(&credential, None, &upstream.base)
                .await
                .expect("没有 deviceId 是一次可解释的未领取，不是错误");
            assert_eq!(Some(false), row["success"].as_bool());
            assert!(row["msg"].as_str().unwrap_or_default().contains("deviceId"));
            assert!(upstream.takes().is_empty(), "这种凭据一发都不该打出去");
        }
    }
}
