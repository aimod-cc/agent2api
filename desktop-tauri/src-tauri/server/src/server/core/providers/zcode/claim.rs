//! ZCode 的**周末套餐领取**（manual claim / weekend plan）。
//!
//! ── 这个功能替代了本家的「签到」────────────────────────────
//! 其余各家都有每日签到（`core::auto_checkin` 的提供商清单），ZCode 没有签到
//! 活动 —— 它的运营玩法是**限时发放的体验套餐**（周末套餐 / start-plan），
//! 用户在客户端里点一下就领，领到的额度在一段时间内可用。因此本家接进定时
//! 任务框架的是「领取」而不是「签到」，两者的调度形状相同、协议完全不同。
//!
//! ── 上游协议（两个接口，抄自 ZCode 3.12.3 客户端）────────────
//! 参考实现：`Acankao/zcode-api` 的 `src/claim/client.ts`，本模块是它的移植。
//!
//! ```text
//!   探测  GET  {zcode}/api/v1/zcode-plan/billing/preview?app_version=&platform=
//!   领取  POST {zcode}/api/v1/zcode-plan/billing/claim   body {"plan_id": "..."}
//! ```
//!
//! ── 头集合是「极简 + 两处例外」，别照抄别家 ──────────────────
//! preview 只带 `Authorization: Bearer {jwt}`（匿名时**一个头都不带**）；
//! claim 带 Authorization / Content-Type / 验证码参数 / 版本 / 平台。
//! 参考实现为此留了一条很长的注记：3.12.3 的客户端就是极简集合，早先版本
//! 发过整套身份头（`X-Device-Mid` 等），两者在不同活动期都出现过。
//!
//! **一处必须保留的例外**：`X-Device-Mid`（UUID 形态）。参考实现实测记录：
//! 2026-09-18 那一期活动，极简头会被网关以 biz 3001「参数错误」拒掉，
//! 加上这个头立刻 200；只加验证码头无效。因此当调用方提供了 device_mid 时，
//! **两个接口都要追加它**（位置在最后，与客户端的身份头顺序一致）。
//!
//! ── 验证码为什么是**参数**而不是本模块自己求解 ───────────────
//! claim 必须带 `X-Aliyun-Captcha-Verify-Param`（阿里云无痕验证）。
//! 参考实现在进程内用 happy-dom 跑阿里云官方混淆 SDK 求解 —— 那是 Node/Bun
//! 生态的产物（需要 DOM 桩、canvas/WebGL/Worker 垫片、同步 XHR 的 worker 变通），
//! 本网关是纯 Rust 且零 Node 依赖，**不重复实现那套求解器**。
//!
//! 本项目已有一条现成的等价链路：AutoClaw 的 OAuth 登录同样要过阿里云无痕
//! 验证，做法是**前端（Tauri webview）用官方 SDK 求解、把 verifyParam 交给
//! 后端**（见 `ui/autoclaw-oauth.js` 与 `api::session::login_oauth_captcha_config`）。
//! 领取走同一条路：前端解出参数 → 调后端的领取接口 → 本模块把它原样转发给上游。
//! 于是本模块的签名里 `captcha` 是**入参**，本模块只负责「带上它去领取」。
//!
//! ── 失败分类（biz code → 语义）──────────────────────────────
//! 与客户端 `$vt` 映射器逐条对齐（参考实现 `src/claim/types.ts`）：
//! `1001` 不存在 / `1002` 不可领（活动结束或未开始）/ `1003` 已领过 /
//! `1004` 不符合资格（账号或客户端版本不达标）/ `1005` 当日领取额度用尽 /
//! `3001` 参数错误 / `3007` 验证码失败 / `401` 未登录。
//! 这套分类是**调度语义的输入**：见 `outcome_hold`。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::server::core::auth_http::send_raw;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;

use super::region::Region;

/// 单次请求超时（参考实现 `DEFAULT_TIMEOUT_MS` 逐字相同）。
///
/// 必须显式设：`egress` 的默认 read_timeout 是 600 秒（给 SSE 长连接留的），
/// 不设的话一个挂住的领取请求能让定时任务转圈十分钟。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 客户端版本号。
///
/// 上游拿它判「这个客户端够不够新」（`1004 ineligible` 就包含版本不达标），
/// 活动也可能要求最低版本。参考实现把它做成配置项（`identity.appVersion`，
/// 环境变量 `ZCODE_APP_VERSION` 可覆盖），默认值是 **ZCode 客户端当前版本**
/// `3.14.0`。
///
/// ★ 别拿参考项目自己的版本号（它 `package.json` 里那个 `4.6.9`）—— 那个数字
/// 冒充不了客户端，`app_version` 与 `X-ZCode-App-Version` 都会对不上，症状是
/// 领取被判 `1004 ineligible`（而这看起来像「账号没资格」，极易误判）。
/// 上游抬门槛时用户可以用环境变量覆盖，不必等发版。
const DEFAULT_APP_VERSION: &str = "3.14.0";

/// 探测与领取的路径（参考实现 `client.ts` 的常量）
const PREVIEW_PATH: &str = "/api/v1/zcode-plan/billing/preview";
const CLAIM_PATH: &str = "/api/v1/zcode-plan/billing/claim";

/// 客户端配置（风控参数藏在它的 `data.configs.captcha` 里）
const CLIENT_CONFIGS_PATH: &str = "/api/v1/client/configs";

/// 阿里云无痕验证的配置（前端拿它去初始化 SDK）。
///
/// 三个字段都是 SDK 的必填项，任缺一个就没法初始化：
///   - `prefix` / `region` → `window.AliyunCaptchaConfig`（决定打哪个阿里云站点）
///   - `scene_id` → `initAliyunCaptcha({ SceneId })`
///
/// 参考实现从同一个接口取（`fetchCaptchaConfig`，带 60 秒缓存），本家让
/// 调用方决定要不要缓存 —— 这是低频动作（一次领取取一次），不值得在这里
/// 维护一份带 TTL 的进程级缓存。
#[derive(Clone, Debug)]
pub struct CaptchaConfig {
    /// 上游说这一家此刻要不要验证码。`false` 时前端**不该**弹滑块 ——
    /// 硬弹会让用户在本不需要验证的时候被拦一道
    pub enabled: bool,
    /// SDK 站点前缀（`window.AliyunCaptchaConfig.prefix`）
    pub prefix: String,
    /// 场景 id（`initAliyunCaptcha` 的 `SceneId`）
    pub scene_id: String,
    /// SDK 站点地区（`window.AliyunCaptchaConfig.region`）
    pub region: String,
}

/// 取风控配置（`GET {zcode}/api/v1/client/configs?app_version=&platform=`）。
///
/// 返回 `Ok(None)` = 上游没有下发 captcha 段（或字段不全）：调用方据此
/// **不弹验证码**，而不是弹一个初始化不出来的滑块（那种失败对用户来说
/// 完全无法理解：滑块都没出现就被报「验证码组件不可用」）。
pub async fn captcha_config(
    region: Region,
    proxy: Option<&ResolvedProxy>,
) -> Result<Option<CaptchaConfig>, GatewayError> {
    let url = format!(
        "{}{CLIENT_CONFIGS_PATH}?app_version={}&platform={}",
        region.zcode_origin(),
        urlencode(&app_version()),
        urlencode(platform())
    );
    let response = send_raw("GET", &url, None, &[], proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "获取风控配置超时")
            } else {
                GatewayError::with_status(502, format!("获取风控配置失败: {error}"))
            }
        })?;
    let payload = response.payload.unwrap_or(Value::Null);
    let captcha = payload
        .get("data")
        .and_then(|value| value.get("configs"))
        .and_then(|value| value.get("captcha"));
    let Some(captcha) = captcha else {
        return Ok(None);
    };
    let text = |key: &str| {
        captcha
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let prefix = text("prefix");
    let scene_id = text("sceneId");
    if prefix.is_empty() || scene_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(CaptchaConfig {
        enabled: captcha.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        prefix,
        scene_id,
        // 缺省 `ga`：与 AutoClaw 那条链的兜底一致（阿里云的默认站点）
        region: {
            let value = text("region");
            if value.is_empty() { "ga".to_string() } else { value }
        },
    }))
}

/// 领取出错时**优先复核风控配置**（模块头第 3007 档的处置，见 `claim` 的文档）。
///
/// 单独成一个函数是因为它有明确的前置条件与用途：只有「验证码失败」这一档
/// 才值得多花一次往返去问「现在还需要验证码吗」。其他失败（额度用尽、
/// 已领过）复核配置没有意义。
pub fn captcha_hint(config: &CaptchaConfig) -> String {
    if config.enabled {
        "请重新完成验证码后再试".to_string()
    } else {
        // 上游此刻说「不需要验证码」，而领取却因验证码被拒 —— 说明我们带的
        // 那个参数反而成了问题（可能是上一轮的陈旧解）。如实说明，让用户
        // 点一次重试（不带的路径见 `claim` 的调用方）。
        "上游当前未要求验证码，请直接重试一次领取".to_string()
    }
}

/// 客户端版本号（`ZCODE_APP_VERSION` 可覆盖）
pub fn app_version() -> String {
    std::env::var("ZCODE_APP_VERSION")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_APP_VERSION.to_string())
}

/// 平台标识（参考实现的 `${process.platform}-${process.arch}`）。
///
/// 上游把它当「客户端形态」的一部分。本网关只跑在这三端上，映射写死即可；
/// 未知平台回落到 `win32-x64`（上游对未知值不报错，只是可能不满足活动的
/// 客户端白名单，那时会如实回 `1004`）。
pub fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32-x64"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "darwin-arm64"
        } else {
            "darwin-x64"
        }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "aarch64") {
            "linux-arm64"
        } else {
            "linux-x64"
        }
    } else {
        "win32-x64"
    }
}

/// 一条可领取的套餐（上游 `plans[]` 的归一形态）。
///
/// 字段保留上游语义：`starts_at` / `ends_at` 是 **unix 秒**（不是毫秒）——
/// 领取链路全程用秒，换算成毫秒只在展示层做，混用会让「有效期到 1970 年」
/// 这类错误悄悄发生。
#[derive(Clone, Debug)]
pub struct ClaimablePlan {
    /// 上游套餐 id（领取时原样回传）
    pub plan_id: String,
    /// 展示名
    pub name: String,
    /// 描述
    pub description: String,
    /// 优先级（同一批多个套餐时，取最大的那个）
    pub priority: i64,
    /// 生效时间（unix 秒；周末套餐常常是「先领、稍后生效」，因此可能缺失）
    pub starts_at: Option<i64>,
    /// 失效时间（unix 秒）
    pub ends_at: Option<i64>,
    /// 权益条目（进日志与界面，让用户看到领到了什么）
    pub entitlements: Vec<PlanEntitlement>,
}

/// 套餐里的一条权益
#[derive(Clone, Debug)]
pub struct PlanEntitlement {
    /// 权益展示名（缺失时回落到 id）
    pub show_name: String,
    /// 计量单位（`tokens` / `requests` …）
    pub unit_type: String,
    /// 授予量（`0` = 上游没给数字）
    pub grant_units: i64,
    /// 生效时间（unix 秒；缺失表示随套餐立即生效）
    pub effective_at: Option<i64>,
}

/// 探测的结果。
///
/// ── 为什么 404 要单独成一档（这是最容易漏的一处）─────────────
/// 参考实现在 `scheduler.ts` 里明确写着：**404 是「活动还没上线」的正常状态**
/// （周末套餐开抢前，活动接口尚未部署），要按**正常节奏**轮询而不是走错误退避。
/// 若把 404 当失败处理，退避会让轮询节奏越来越慢，恰好错过开抢那一刻 ——
/// 这个功能的价值全在「上新瞬间抢到」，慢一拍就没意义了。
#[derive(Clone, Debug)]
pub enum PreviewOutcome {
    /// 拿到了可领取的套餐（可能是空列表：接口通了但当前没有可领的）
    Plans(Vec<ClaimablePlan>),
    /// 404：活动接口尚未部署（周末套餐开抢前的预期状态）
    NotDeployed,
}

/// 领取的失败语义（参考实现 `ClaimFailureKind` 的移植）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaimFailure {
    /// 套餐不存在（1001）
    NotFound,
    /// 不可领：活动已结束或还没开始（1002）
    Unavailable,
    /// 这个账号已经领过（1003）
    AlreadyClaimed,
    /// 不符合资格：账号或客户端版本不达标（1004）
    Ineligible,
    /// 当日领取额度用尽（1005）
    QuotaExhausted,
    /// 参数错误（3001）—— 最常见的成因是缺 `X-Device-Mid`
    InvalidRequest,
    /// 验证码校验失败（3007）
    Captcha,
    /// 未登录 / JWT 失效（401）
    LoginRequired,
    /// 其它 HTTP 错误
    HttpError,
    /// 认不出的业务码
    Unknown,
}

impl ClaimFailure {
    /// 给用户看的一句话（进日志与界面）
    pub fn label(self) -> &'static str {
        match self {
            Self::NotFound => "套餐不存在",
            Self::Unavailable => "活动已结束或尚未开始",
            Self::AlreadyClaimed => "该账号已领取过",
            Self::Ineligible => "账号或客户端版本不符合活动资格",
            Self::QuotaExhausted => "当日领取额度已用尽",
            Self::InvalidRequest => "参数错误（常见成因：缺少设备标识）",
            Self::Captcha => "验证码校验未通过",
            Self::LoginRequired => "登录态已失效，请重新登录",
            Self::HttpError => "上游 HTTP 错误",
            Self::Unknown => "未知失败",
        }
    }

    /// biz code → 语义（与客户端 `$vt` 映射器逐条对齐）
    fn from_code(code: i64) -> Self {
        match code {
            1001 => Self::NotFound,
            1002 => Self::Unavailable,
            1003 => Self::AlreadyClaimed,
            1004 => Self::Ineligible,
            1005 => Self::QuotaExhausted,
            3001 => Self::InvalidRequest,
            3007 => Self::Captcha,
            401 => Self::LoginRequired,
            _ => Self::Unknown,
        }
    }
}

/// 领取的结果
#[derive(Clone, Debug)]
pub enum ClaimOutcome {
    /// 领到了
    Claimed {
        /// 套餐 id
        plan_id: String,
        /// 生效时间（unix 秒；缺省表示立即生效）
        starts_at: Option<i64>,
        /// 失效时间（unix 秒）
        ends_at: Option<i64>,
    },
    /// 没领到（含「已领过」这类不算错误的结果）
    Failed {
        /// 套餐 id
        plan_id: String,
        /// 失败语义
        failure: ClaimFailure,
        /// 上游业务码（进日志）
        code: i64,
        /// 上游原文
        message: String,
        /// 上游给的下一个可尝试窗口（unix 秒；`1005` 额度用尽时通常有）
        failure_ends_at: Option<i64>,
    },
}

/// 一次领取尝试之后**下次什么时候再试**（调度语义，参考实现 `scheduler.ts`）。
#[derive(Clone, Copy, Debug)]
pub enum NextAttempt {
    /// 本账号在本期活动里无事可做：等到这个 unix 秒（套餐失效时间）之后再说。
    /// 用于 `Claimed` 与 `AlreadyClaimed` —— 两者都表示「这一期已经领到了」。
    HoldUntil(i64),
    /// 换一个窗口再试：等到这个 unix 秒（上游给的下一窗口）。
    /// 用于 `QuotaExhausted`。
    WaitWindow(i64),
    /// 退避：按常规冷却时长再试。用于验证码失败 / 网络错误 / 认不出的失败。
    Cooldown,
}

/// 从领取结果推出下一次尝试时机。
///
/// 为什么单独成函数：这段判断是**调度正确性**的核心（把「已领过」当成失败
/// 去重试会一直打上游、把「额度用尽」当成已领到会整期不再尝试），
/// 放在一处并配上测试视角的注释，比散在调度循环里更难写错。
pub fn outcome_hold(outcome: &ClaimOutcome) -> NextAttempt {
    match outcome {
        // 领到了：本期的目标已达成，等它过期（缺 ends_at 时给 0，由调用方
        // 回落成常规轮询 —— 没有结束时间的套餐不该被无限期 hold 住）
        // 解引用是必需的：这里匹配的是 `&ClaimOutcome`，字段拿到的是
        // `&Option<i64>`（`Option<i64>` 是 Copy，`*` 出来即可）
        ClaimOutcome::Claimed { ends_at, .. } => NextAttempt::HoldUntil((*ends_at).unwrap_or(0)),
        ClaimOutcome::Failed { failure, failure_ends_at, .. } => match failure {
            // 已领过 = 本期的目标其实也达成了（可能是用户手动领的 / 另一台设备领的）
            ClaimFailure::AlreadyClaimed => {
                NextAttempt::HoldUntil((*failure_ends_at).unwrap_or(0))
            }
            // 额度用尽：上游通常给出下一个窗口
            ClaimFailure::QuotaExhausted => match failure_ends_at {
                Some(at) => NextAttempt::WaitWindow(*at),
                None => NextAttempt::Cooldown,
            },
            // 活动没开始 / 不符合资格 / 参数错误 / 验证码 / 网络：都按冷却重试。
            // 其中 `Unavailable`（活动未开始）**不能**长退避 —— 它恰恰是开抢前
            // 最常见的一档，退避太久会错过上新。
            _ => NextAttempt::Cooldown,
        },
    }
}

/// 探测当前可领取的套餐。
///
/// `jwt` 为空时按上游语义发**匿名探测**（参考实现：匿名 preview 一个头都不带）。
/// 匿名也能看到「有没有活动」，因此调度可以在登录态失效时继续探测；
/// 但真正领取必须有 JWT（见 [`claim`]）。
pub async fn preview(
    region: Region,
    jwt: &str,
    device_mid: Option<&str>,
    proxy: Option<&ResolvedProxy>,
) -> Result<PreviewOutcome, GatewayError> {
    let url = format!(
        "{}{PREVIEW_PATH}?app_version={}&platform={}",
        region.zcode_origin(),
        urlencode(&app_version()),
        urlencode(platform())
    );
    let mut headers: Vec<(String, String)> = Vec::new();
    // 匿名探测：一个头都不带（**不是**带一个空 Bearer）
    if !jwt.trim().is_empty() {
        headers.push(("Authorization".to_string(), format!("Bearer {}", jwt.trim())));
    }
    if let Some(mid) = device_mid.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Device-Mid".to_string(), mid.to_string()));
    }

    let response = send_raw("GET", &url, None, &headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "套餐探测超时")
            } else {
                GatewayError::with_status(502, format!("套餐探测失败: {error}"))
            }
        })?;

    // 404 单独成一档（见 `PreviewOutcome::NotDeployed` 的文档）
    if response.status == 404 {
        return Ok(PreviewOutcome::NotDeployed);
    }
    if response.status == 401 {
        return Err(GatewayError::with_status(401, "登录态已失效，无法探测套餐"));
    }
    let payload = response
        .payload
        .ok_or_else(|| GatewayError::with_status(502, "套餐探测：上游响应不是 JSON"))?;
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
    let data = payload.get("data");
    if !(200..300).contains(&response.status) || code != 0 || data.is_none() {
        let message = error_message(&payload, response.status);
        return Err(GatewayError::with_status(
            502,
            format!("套餐探测失败（{code}）：{message}"),
        ));
    }
    let plans = data
        .and_then(|value| value.get("plans"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_plan).collect())
        .unwrap_or_default();
    Ok(PreviewOutcome::Plans(plans))
}

/// 领取一个套餐。
///
/// `captcha_verify_param` 由**前端**解出（见模块头：本网关不在 Rust 侧复刻
/// 阿里云 SDK 求解器）。`captcha_region` 是同一套 SDK 给出的地区标识，可选。
///
/// 返回 `Err` 只用于「请求本身没打成」（网络 / 超时 / 响应不是 JSON）；
/// 上游明确拒绝（含验证码失败、已领过）一律走 `Ok(ClaimOutcome::Failed)` ——
/// 那些是**业务结果**，调用方要靠 `failure` 决定下次何时再试。
pub async fn claim(
    region: Region,
    jwt: &str,
    plan_id: &str,
    captcha_verify_param: &str,
    captcha_region: Option<&str>,
    device_mid: Option<&str>,
    proxy: Option<&ResolvedProxy>,
) -> Result<ClaimOutcome, GatewayError> {
    if jwt.trim().is_empty() {
        return Ok(ClaimOutcome::Failed {
            plan_id: plan_id.to_string(),
            failure: ClaimFailure::LoginRequired,
            code: 401,
            message: "领取需要登录态（本账号没有 ZCode JWT）".to_string(),
            failure_ends_at: None,
        });
    }
    let url = format!("{}{CLAIM_PATH}", region.zcode_origin());
    // 头顺序与客户端一致：Authorization → Content-Type → 验证码 → 版本 → 平台
    // → 设备标识。顺序本身不是协议要求，但保持与参考实现同序便于逐条比对。
    //
    // ── 验证码头**只在有值时发**（一处容易想当然的地方）──────────
    // 上游的风控配置可能是 `enabled: false`（那一刻不要验证码），此时前端
    // 根本不会弹滑块、也就没有 verifyParam。若把空串也写成头
    // （`X-Aliyun-Captcha-Verify-Param: `），上游会把它当成「给了一个无效的
    // 验证串」而回 3007 —— 于是「不需要验证码」的那一刻反而领不了，
    // 而报错文案是「验证码校验未通过」，指向完全错误的方向。
    // 因此有值才发；没有就让上游按它自己的规则判（要就回 3007，不要就放行）。
    let mut headers: Vec<(String, String)> = vec![
        ("Authorization".to_string(), format!("Bearer {}", jwt.trim())),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if let Some(param) = Some(captcha_verify_param.trim()).filter(|value| !value.is_empty()) {
        headers.push(("X-Aliyun-Captcha-Verify-Param".to_string(), param.to_string()));
    }
    if let Some(value) = captcha_region.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Aliyun-Captcha-Verify-Region".to_string(), value.to_string()));
    }
    headers.push(("X-ZCode-App-Version".to_string(), app_version()));
    headers.push(("X-Platform".to_string(), platform().to_string()));
    if let Some(mid) = device_mid.map(str::trim).filter(|value| !value.is_empty()) {
        headers.push(("X-Device-Mid".to_string(), mid.to_string()));
    }

    let body = json!({ "plan_id": plan_id });
    let response = send_raw(
        "POST",
        &url,
        Some(&body),
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "套餐领取超时")
        } else {
            GatewayError::with_status(502, format!("套餐领取失败: {error}"))
        }
    })?;

    let payload = response.payload.unwrap_or(Value::Null);
    let biz_code = payload.get("code").and_then(Value::as_i64);
    let data = payload.get("data");
    let plan = data.and_then(|value| value.get("plan"));

    // 成功：HTTP 2xx + biz code 0 + data.plan 存在（三者缺一不可 —— 参考实现
    // 同样要求 data.plan 在场，因为只有它带 starts_at/ends_at）
    if (200..300).contains(&response.status) && biz_code == Some(0) {
        if let Some(plan) = plan {
            return Ok(ClaimOutcome::Claimed {
                plan_id: plan_id.to_string(),
                starts_at: plan.get("starts_at").and_then(Value::as_i64),
                ends_at: plan.get("ends_at").and_then(Value::as_i64),
            });
        }
    }

    // 失败：HTTP 层错误（无业务码）与业务码错误分开归类 —— 前者只有状态码
    // 可依（401 判未登录，其余算 HTTP 错误），后者走 biz code 映射
    let failure = match biz_code {
        Some(code) => ClaimFailure::from_code(code),
        None if response.status == 401 => ClaimFailure::LoginRequired,
        None => ClaimFailure::HttpError,
    };
    // 上游在失败响应里也可能带 plan（含 ends_at），那就是「下一个可尝试窗口」
    let failure_ends_at = plan
        .and_then(|value| value.get("ends_at"))
        .and_then(Value::as_i64);
    Ok(ClaimOutcome::Failed {
        plan_id: plan_id.to_string(),
        failure,
        code: biz_code.unwrap_or(i64::from(response.status)),
        message: error_message(&payload, response.status),
        failure_ends_at,
    })
}

/// 从上游响应里取一句人话（参考实现 `unwrapError` 的同效处理）
fn error_message(payload: &Value, status: u16) -> String {
    for key in ["msg", "message"] {
        if let Some(text) = payload.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    format!("HTTP {status}")
}

/// 上游 `plans[]` 里的一条 → [`ClaimablePlan`]（缺 `plan_id` 的条目丢弃）
fn parse_plan(raw: &Value) -> Option<ClaimablePlan> {
    let plan_id = raw.get("plan_id").and_then(Value::as_str)?.trim().to_string();
    if plan_id.is_empty() {
        return None;
    }
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&plan_id)
        .to_string();
    let entitlements = raw
        .get("entitlements")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let show_name = item
                        .get("show_name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())?
                        .to_string();
                    Some(PlanEntitlement {
                        show_name,
                        unit_type: item
                            .get("unit_type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                        grant_units: item.get("grant_units").and_then(Value::as_i64).unwrap_or(0),
                        effective_at: item.get("effective_at").and_then(Value::as_i64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ClaimablePlan {
        plan_id,
        name,
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        priority: raw.get("priority").and_then(Value::as_i64).unwrap_or(0),
        starts_at: raw.get("starts_at").and_then(Value::as_i64),
        ends_at: raw.get("ends_at").and_then(Value::as_i64),
        entitlements,
    })
}

/// 从一批可领套餐里挑出目标：指定了 `plan_id` 就取它，否则取优先级最高的。
///
/// 参考实现的选取口径：`planId` 配置为空时按 `priority` 降序取第一个。
/// 优先级相同的情形保持**上游给的顺序**（`max_by_key` 取最后一个的语义陷阱
/// 要避开：这里用显式比较，相等时不替换，于是先出现的那个胜出）。
pub fn pick_target<'a>(plans: &'a [ClaimablePlan], plan_id: &str) -> Option<&'a ClaimablePlan> {
    let wanted = plan_id.trim();
    if !wanted.is_empty() {
        return plans.iter().find(|plan| plan.plan_id == wanted);
    }
    let mut best: Option<&ClaimablePlan> = None;
    for plan in plans {
        match best {
            None => best = Some(plan),
            Some(current) if plan.priority > current.priority => best = Some(plan),
            Some(_) => {}
        }
    }
    best
}

/// 最小的 URL 查询参数编码（`url` crate 不在依赖里，这里只处理查询值）。
///
/// 版本号与平台标识都是 ASCII 字母数字与 `-` / `.`，理论上不需要转义；
/// 但 `ZCODE_APP_VERSION` 是用户可覆盖的，一个带 `&` 的值就能拼出畸形 URL。
/// 因此这里按 RFC 3986 的 unreserved 集合放行，其余一律百分号编码。
///
/// `pub(super)`：`oauth` 那边的中转页参数要把**整个地址**当成一个查询值编码
/// （见 `oauth::apply_interstitial`），同一套规则不必再写第二遍。
pub(super) fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
