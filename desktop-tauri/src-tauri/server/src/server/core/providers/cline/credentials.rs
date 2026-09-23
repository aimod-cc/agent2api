//! Cline 凭证：格式、桌面端登录态读取、账号记录解析。
//!
//! ── 上游凭证长什么样（实测，2026-09）─────────────────────────
//! Cline 的登录态是 **WorkOS 的 OAuth2 令牌**，落在 `~/.cline/data/settings/providers.json`
//! 的 `providers.cline.settings.auth`：
//!
//! ```jsonc
//! {
//!   "accessToken": "workos:eyJhbGciOiJSUzI1NiIs...",   // 注意 workos: 前缀
//!   "refreshToken": "N5EUfPqyERlpRhiF4WUXDPunz",
//!   "expiresAt": 1789842888000,                          // 毫秒时间戳
//!   "accountId": "usr-01M2VWEDWFWWRS3WQ7R425E817",
//!   "metadata": {
//!     "provider": "cline",
//!     "tokenType": "Bearer",
//!     "userInfo": { "subject": "user_...", "clineUserId": "usr-...",
//!                   "email": "...", "name": "..." }
//!   }
//! }
//! ```
//!
//! ── `workos:` 前缀（关键，实测踩出来的）─────────────────────
//! `accessToken` 带 **`workos:` 前缀**。发请求时**必须原样带上**（`Authorization:
//! Bearer workos:eyJ...`）—— 去掉前缀会 401。这个前缀是 Cline 用来标记「这是
//! WorkOS 令牌而不是自家旧版令牌」的，不是可选的装饰。
//!
//! ── 时间字段的口径 ──────────────────────────────────────────
//! `expiresAt` 是**毫秒**时间戳（与项目的 `logging::now_ms()` 同单位）。
//! 但账号记录里落盘用 [`crate::server::core::account_store::state`] 的通用键
//! `expiresAt`（数字，毫秒），与另外五家一致 —— 见 `add_cline_account` 的说明。
//!
//! ── 桌面端登录态（唯一来源，没有 env 旁路）────────────────────
//! Cline **没有环境变量旁路**（没有 `CLINE_TOKEN` 这类约定，官方 CLI 只认
//! providers.json），因此本模块只有「账号记录」与「桌面端文件」两个来源，
//! 与另外五家的三到四个来源相比少一层。`allows_anonymous_default_session`
//! 相应地返回 false。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件绝不 unwrap/expect/panic，取值走 Option 链。

use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::server::errors::GatewayError;

/// 上游 API 基址（实测：`api.cline.bot` 的 `/api/v1` 一级）。
///
/// 注意这个 base 后面接的不是 `/v1/chat/completions`，而是
/// `/chat/completions`（完整路径 `https://api.cline.bot/api/v1/chat/completions`）。
/// 上游走的是 AI SDK 的 `createOpenAICompatible`，它把 baseURL 当**根**拼。
pub const API_BASE_URL: &str = "https://api.cline.bot/api/v1";

/// WorkOS 端点（设备授权登录用，见 `login.rs`）
pub const WORKOS_BASE_URL: &str = "https://api.workos.com";

/// Cline 的 WorkOS client_id。
///
/// ── 这个值的来源（不是猜的）────────────────────────────────
/// 从本机已登录的 accessToken（JWT）的 `client_id` / `iss` 声明里读出，
/// 与 `@cline/core` 里 `workOsClientId` 的运行时取值一致：
/// `iss: https://api.workos.com/user_management/client_01K3A541FN8TA3EPPHTD2325AR`。
/// 实测用它打 `/user_management/authorize/device` 能正常拿到 user_code
/// （见 `login.rs` 的核对记录）。
///
/// ── 它会变吗 ────────────────────────────────────────────────
/// 理论上是 Cline 的固定应用标识（WorkOS 侧注册的 client），不随版本变；
/// 真有一天失效，表现是设备授权第一步返回 4xx，那时按 JWT 里的新值更新这里。
pub const WORKOS_CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";

/// 设备授权页（`verification_uri` 的兜底；正常情况用上游返回值）
pub const DEVICE_VERIFY_FALLBACK: &str = "https://authkit.cline.bot/device";

/// access token 的必需前缀（实测：去掉它上游 401）
pub const TOKEN_PREFIX: &str = "workos:";

/// 桌面端登录态文件（相对用户主目录）
pub const DESKTOP_SETTINGS_RELATIVE: &str = ".cline/data/settings/providers.json";

/// 桌面端实时登录态账号的 **id 后缀**（与另外几家的 `*-desktop` 命名同构）。
///
/// ── 为什么是后缀而不是完整 id ────────────────────────────────
/// Cline 拆成两家 provider 之后，同一个 Cline 桌面登录态**两个池各能导入一份**
/// （各服务一个池，见 `account_store::cline_accounts` 的模块头）。记录 id 因此
/// 必须带 provider 前缀，取值由 [`desktop_account_id`] 拼。
///
/// 这里不再留 `DESKTOP_ACCOUNT_ID` 那个旧常量：它写死的 `cline-desktop` 里
/// `cline` 这个 provider id **已经不存在**（拆分后是 `cline-free` /
/// `cline-pass`），留着它只会让后来者照着抄出一个永远匹配不上的记录 id。
pub const DESKTOP_ACCOUNT_SUFFIX: &str = "desktop";

/// 某个池的桌面端账号记录 id（`cline-free-desktop` / `cline-pass-desktop`）。
///
/// 拼法必须与 `account_store::cline_accounts` 的 `record_id` 一致
/// （那里是 `<provider>-<账号标识>`）—— 不一致的症状是「导入成功但账号列表里
/// 看不到」：导入按这里的 id 写记录，而各处按 id 找记录（续期回写、删除、
/// 桌面端实时凭证注入）会找不到它。
pub fn desktop_account_id(provider_id: &str) -> String {
    format!("{provider_id}-{DESKTOP_ACCOUNT_SUFFIX}")
}

/// 临期窗口：距过期不足这么久就主动续期（10 分钟）。
///
/// ── 为什么比另外几家大 ──────────────────────────────────────
/// 实测 Cline 的 accessToken 有效期**只有 1 小时**（`exp - iat = 3600`，
/// 见 JWT 声明），比小浣熊（数小时）与 AutoClaw 短得多。窗口取 10 分钟是
/// 「一小时有效期的 1/6」，在「不过度消耗 refresh 轮换」与「不把临期 token
/// 发上去挨 401」之间取平衡。
pub const PROACTIVE_REFRESH_MARGIN_MS: i64 = 10 * 60 * 1000;

/// token 长度上限（与账号存储的 MAX_TOKEN_LENGTH 同量级；JWT 通常 1-2KB）
pub const MAX_TOKEN_LENGTH: usize = 8192;

/// 一份 Cline 凭证（账号记录或桌面端登录态解析后的统一形态）。
///
/// 只 `PartialEq`（不 `Eq`）：`expires_at` 是 f64，f64 不实现 Eq —— 凭证比较
/// 本来就只用于「刷新前后有没有变化」（`refresh::persist_refresh` 的比较-再写），
/// 那里比的是 token 字符串而不是浮点。
#[derive(Clone, Debug, PartialEq)]
pub struct ClineCredentials {
    /// 账号记录 id（桌面端登录态是 [`DESKTOP_ACCOUNT_ID`]）
    pub id: String,
    /// access token（**含 `workos:` 前缀**，原样发给上游）
    pub access_token: String,
    /// refresh token（可能为空 —— 用户手填时可能只给 access token）
    pub refresh_token: String,
    /// 过期时间（毫秒时间戳；None = 无从判断）
    pub expires_at: Option<f64>,
    /// 账号 id（`usr-…` 形态，余额接口路径认的那个；payload 显式给的除外）
    pub account: String,
    /// 展示名（**email 优先**，其次姓名；用户显式给的备注名优先于它）
    pub name: String,
    /// **内部用**：桌面端登录态的内存刷新覆盖键（`providers:<文件 mtime>`）。
    /// 账号记录来源为 None —— 那条来源的刷新结果直接回写账号库，不走缓存。
    pub cache_key: Option<String>,
}

impl ClineCredentials {
    /// 是否可续期（有 refresh token）
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    /// 是否已过期或临期（判不出过期时间时返回 false —— 见 trait 契约：
    /// 无从判断就说「需要刷新」会让维护任务白跑一趟）
    pub fn is_expiring(&self) -> bool {
        self.is_expiring_at(crate::server::logging::now_ms())
    }

    /// 指定时刻的临期判定（把「现在」作参数是为了让判据可测、也是为了让
    /// 同一批判定在同一时刻下比较）
    pub fn is_expiring_at(&self, now_ms: i64) -> bool {
        match self.expires_at {
            Some(expires) => {
                let expires = expires as i64;
                expires.saturating_sub(now_ms) <= PROACTIVE_REFRESH_MARGIN_MS
            }
            None => false,
        }
    }

    /// 带 `workos:` 前缀的 access token（发请求用）。
    ///
    /// 幂等：已经带了就不再补一层（用户手填时可能带也可能不带）。
    pub fn bearer_token(&self) -> String {
        ensure_token_prefix(&self.access_token)
    }
}

/// 确保 token 带 `workos:` 前缀（幂等）。
///
/// ── 为什么两种形态都接受 ────────────────────────────────────
/// 上游返回的是 `workos:eyJ...`，但**用户手填**时可能从别处复制到不带前缀的
/// 裸 JWT（或者反过来，从我们界面复制的）。两种都规范化成带前缀，避免
/// 「看着填对了却稳定 401」这种最难排查的形态。
pub fn ensure_token_prefix(token: &str) -> String {
    let token = token.trim();
    if token.is_empty() {
        return String::new();
    }
    if token.starts_with(TOKEN_PREFIX) {
        token.to_string()
    } else {
        format!("{TOKEN_PREFIX}{token}")
    }
}

/// 从 JWT 里解出 `exp`（秒）→ 毫秒时间戳。
///
/// Cline 的 accessToken 是标准 JWT（`workos:` 前缀之后是 `header.payload.signature`），
/// payload 里带 `exp`。用它作为 `expiresAt` 的**兜底来源**：用户手填 token 时
/// 通常不会带 expiresAt，而 JWT 自己说了什么时候过期。
///
/// 解析失败返回 None（不报错：token 可能不是 JWT，那时只能靠 401 懒刷新）。
pub fn expires_at_from_jwt(token: &str) -> Option<f64> {
    let claims = jwt_claims(token)?;
    let exp = claims.get("exp")?.as_f64()?;
    if exp <= 0.0 {
        return None;
    }
    Some(exp * 1000.0)
}

/// 从 JWT 里解出 Cline 的**账号 id**（`usr-…` 形态，余额接口的路径参数）。
///
/// ── 这个值在哪个声明里（实测核对）────────────────────────────
/// WorkOS 签发的令牌把 Cline 的账号 id 放在 **`external_id`** 声明里
/// （实测：`external_id` 与桌面端 providers.json 的 `accountId` 逐字相同，
/// 都是 `usr-01M2VW...`；而 `sub` 是 WorkOS 自己的 `user_…`，**不能**用于
/// Cline 的 API 路径）。
///
/// ── 为什么值得单独取它 ──────────────────────────────────────
/// 余额接口的路径是 `/users/{userId}/balance`，那个 `userId` 要的正是这个值。
/// 不取它就得先打一次 `/users/me` 才知道 id —— 多一个往返、多一处失败面。
/// 取到了就能直接查余额（`balance.rs` 优先用它，取不到才回落 `/users/me`）。
pub fn account_id_from_jwt(token: &str) -> Option<String> {
    let claims = jwt_claims(token)?;
    let text = claims
        .get("external_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(text.to_string())
}

/// 从 JWT 里解出用户标识（**email 优先**，其次姓名，最后账号 id）。
///
/// 只用于**展示与去重**，不参与鉴权判定。
///
/// 返回 `(展示名, 账号 id)`：
///   - 展示名：email → `lastName+firstName`（中文姓名习惯，实测 JWT 里是
///     `firstName:"亮"` / `lastName:"欧阳"`，拼成「欧阳亮」）→ 账号 id。
///     展示口径是**邮箱优先**（界面要求 Cline 显示邮箱而不是姓名 —— 用户
///     认得出的是自己注册用的邮箱；姓名只是邮箱取不到时的兜底）；
///   - 账号 id 优先 `external_id`（`usr-…`，余额路径要的那个），
///     其次 `clineUserId`、最后 `sub`。
pub fn identity_from_jwt(token: &str) -> (String, String) {
    let Some(claims) = jwt_claims(token) else {
        return (String::new(), String::new());
    };
    let text = |key: &str| {
        claims
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let email = text("email");
    // 账号 id 的优先级：external_id（Cline 的 usr-…）> clineUserId > sub
    let external = text("external_id");
    let cline_user = text("clineUserId");
    let sub = text("sub");
    let account = if !external.is_empty() {
        external
    } else if !cline_user.is_empty() {
        cline_user
    } else {
        sub
    };
    // 姓名：中文习惯是「姓+名」（lastName 在前），但上游可能只给其一。
    // 两者都空时留空串，交给上面的 email 兜底 —— 不拼出一个只有空格的假名。
    // email 在前（见返回值说明）：取得到就不再用姓名。
    let person = person_name(&text("lastName"), &text("firstName"));
    let display = if !email.is_empty() {
        email
    } else if !person.is_empty() {
        person
    } else {
        account.clone()
    };
    (display, account)
}

/// 拼姓名（按语种决定排列顺序）。
///
/// ── 为什么要分语种 ──────────────────────────────────────────
/// `firstName` / `lastName` 是**语义名**（名 / 姓），不是排列顺序。
/// 实测同一账号：WorkOS 的 JWT 声明是 `firstName:"亮"`、`lastName:"欧阳"`，
/// 而桌面端 `userInfo.name` 给的是「亮 欧阳」（名在前）—— 同一个人的两种写法。
///
/// 中文习惯是**姓在前**（欧阳亮，与 Qoder 的账号名一致），西文习惯是
/// **名在前**（John Smith）。两种混着显示会让界面上一排名字看着像两套规则，
/// 所以按「含不含中日韩字符」分开拼：中文走 `姓+名`，西文走 `名 姓`。
///
/// 只有一边非空时直接用那一边（不补空格、不补占位）。
pub fn person_name(last: &str, first: &str) -> String {
    let last = last.trim();
    let first = first.trim();
    if last.is_empty() {
        return first.to_string();
    }
    if first.is_empty() {
        return last.to_string();
    }
    if has_cjk(last) || has_cjk(first) {
        format!("{last}{first}")
    } else {
        format!("{first} {last}")
    }
}

/// 是否含中日韩字符（用于判断姓名按哪种语序拼）
fn has_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(ch as u32,
            0x3040..=0x30FF        // 日文假名
            | 0x3400..=0x4DBF      // 中日韩扩展 A
            | 0x4E00..=0x9FFF      // 中日韩统一表意文字
            | 0xF900..=0xFAFF      // 兼容表意文字
            | 0xAC00..=0xD7AF      // 韩文音节
        )
    })
}

/// 解出 JWT 的 payload 声明（非 JWT / 解码失败 → None）。
///
/// 三个取值函数（`exp` / `external_id` / 身份）共用它，避免各写一遍 base64 解码。
fn jwt_claims(token: &str) -> Option<Value> {
    let token = token.trim().trim_start_matches(TOKEN_PREFIX);
    let payload = token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

/// URL-safe base64 解码（带不带 padding 都认）。
///
/// 不引入新依赖：JWT 的 payload 段就是 base64url，自己解几行就够
/// （项目里 autoqlaw 的 `crypto::decode_jwt_claims` 有同类实现，但那个
/// 依赖它的 JWT 结构约定，这里只要 payload）。
fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(cleaned.len() / 4 * 3 + 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in cleaned {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// 「判不出过期时间」账号的刷新节流窗口：25 分钟。
///
/// ── 这个数字来自官方（不是拍的）─────────────────────────────
/// 官方 CLI 的续期调度器对「JWT 里没有 `exp`、也没有 `expiresAt`」的凭证用
/// `UNKNOWN_EXP_REFRESH_INTERVAL_MS = 25 分钟` 兜底刷一次（本机 auth 日志里
/// 实测打印 `unknown_exp_refresh_interval_ms: 1500000`），而不是像对待已知
/// 过期时间那样只看临期窗口。网关没有那个常驻定时器，因此把同样的周期放在
/// **维护任务**这一层兜底（见 `adapter::credentials_expiring`）。
///
/// ── 为什么不能直接在 `is_expiring_at` 里返回 true ─────────────
/// `is_expiring_at` 还被转发链路的 `ensure_fresh`（每个请求一次）调用，
/// 在那里把「判不出」当成「已临期」会让**每个请求都去打一次续期接口**。
/// 官方的调用点是「每次会话启动」，不存在这个放大效应；网关必须分开处理，
/// 所以节流状态与判据都只挂在维护任务那一条路上。
const UNKNOWN_EXP_REFRESH_INTERVAL_MS: i64 = 25 * 60 * 1000;

/// 「判不出过期时间的账号」本轮是否该兜底刷一次（进程级节流）。
///
/// 键是账号记录 id（不是凭证指纹）：同一个账号反复刷新失败时也要被节流住，
/// 否则会退化成每轮维护都打一次上游。首次调用即视为「到期」（该账号刚出现，
/// 值得刷一次确认它到底还能不能用）。
pub(crate) fn unknown_exp_refresh_due(account_id: &str, now_ms: i64) -> bool {
    use std::collections::HashMap;
    static LAST: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    let table = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match table.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.get(account_id) {
        Some(last) if now_ms.saturating_sub(*last) < UNKNOWN_EXP_REFRESH_INTERVAL_MS => false,
        _ => {
            guard.insert(account_id.to_string(), now_ms);
            true
        }
    }
}

/// 从账号记录（accounts.json 的一条）解析凭证。
///
/// ── 与其他几家的键名口径一致 ────────────────────────────────
/// 落盘用 `accessToken` / `refreshToken` / `expiresAt`（与 W3 为小浣熊确立的
/// 口径一致），读取侧同时兼容上游 providers.json 的原名（`accessToken` 同名，
/// refresh 的 snake_case 变体 `refresh_token` 也认）。
///
    /// ── 展示名的来源顺序（email 优先）──────────────────────────
    /// 记录里没有 token 的 JWT 时（桌面端账号的凭证在客户端文件里）也要能拿到
    /// 展示名，所以顺序是：记录里的 `displayName`（登录/续期时按「email 优先」
    /// 抽出来落下的）→ JWT 的展示名（email → 姓名）→ 记录里的 `account`
    /// （`usr-…` 或 email）。
    /// 桌面端那条链另外在 [`credentials_from_desktop_settings`] 里读
    /// `metadata.userInfo`，见那里。
pub fn credentials_from_record(record: &Value) -> Result<ClineCredentials, GatewayError> {
    let access = pick_string(record, &["accessToken", "access_token", "token"]);
    if access.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Cline 账号缺少 accessToken，请重新登录或导入桌面端登录态",
        ));
    }
    if access.chars().count() > MAX_TOKEN_LENGTH {
        return Err(GatewayError::with_status(400, "Cline 的 accessToken 过长"));
    }
    let refresh = pick_string(record, &["refreshToken", "refresh_token"]);
    if refresh.chars().count() > MAX_TOKEN_LENGTH {
        return Err(GatewayError::with_status(400, "Cline 的 refreshToken 过长"));
    }
    let expires_at = record
        .get("expiresAt")
        .and_then(number)
        .or_else(|| expires_at_from_jwt(&access));
    let (identity, _) = identity_from_jwt(&access);
    let account = {
        let stored = pick_string(record, &["account", "userId", "accountId"]);
        if !stored.is_empty() {
            stored
        } else {
            identity.clone()
        }
    };
    // `displayName` 是登录 / 续期时抽出来落下的姓名（见 `name` 的说明）；
    // 没有它才回落到 JWT 里的姓名
    let name = {
        let stored = pick_string(record, &["displayName"]);
        if !stored.is_empty() {
            stored
        } else {
            identity
        }
    };
    let id = pick_string(record, &["id"]);
    Ok(ClineCredentials {
        id,
        access_token: ensure_token_prefix(&access),
        refresh_token: refresh.trim().to_string(),
        expires_at,
        account,
        name,
        // 账号记录来源的刷新结果直接回写账号库（见 `refresh::persist_refresh`），
        // 不经过进程缓存 —— 缓存只服务「凭证在客户端文件里」那条来源
        cache_key: None,
    })
}

/// 从上游 `providers.json` 的 `providers.cline.settings.auth` 解析凭证
/// （桌面端实时登录态用）。
///
/// ── 为什么单独一个函数 ──────────────────────────────────────
/// 那份文件的形状与我们的账号记录**完全不同**（外层有 `version` / `providers`
/// 两级，时间字段是毫秒 `expiresAt`，标识在 `metadata.userInfo` 里），
/// 硬塞进 `credentials_from_record` 的候选键只会让那个函数变成两套格式的混合体。
///
/// ── 展示名从 `metadata.userInfo` 取（email 优先）──────────────
/// 桌面端登录态里有一份完整的 `userInfo`（实测字段：`firstName` / `lastName`
/// / `email` / `name` / `subject` / `clineUserId`），比 JWT 声明还全 ——
/// 而且**桌面端账号的记录里不落 token**，解析凭证时读的正是这份文件，
/// 顺手把展示名带出来最省事。顺序：`email` → `firstName`+`lastName` 拼
/// → `name`（与 `identity_from_jwt` 的「email 优先」同一口径）。
pub fn credentials_from_desktop_settings(root: &Value) -> Result<ClineCredentials, GatewayError> {
    let auth = root
        .get("providers")
        .and_then(|providers| providers.get("cline"))
        .and_then(|cline| cline.get("settings"))
        .and_then(|settings| settings.get("auth"))
        .ok_or_else(|| {
            GatewayError::with_status(
                400,
                "本机 Cline 登录态文件里没有 cline 凭证（请先在 Cline 客户端登录）",
            )
        })?;
    let mut credentials = credentials_from_record(auth)?;
    if let Some(info) = auth
        .get("metadata")
        .and_then(|metadata| metadata.get("userInfo"))
    {
        let display = {
            let email = pick_string(info, &["email"]);
            if !email.is_empty() {
                email
            } else {
                let person = person_name(
                    info.get("lastName").and_then(Value::as_str).unwrap_or(""),
                    info.get("firstName").and_then(Value::as_str).unwrap_or(""),
                );
                if !person.is_empty() {
                    person
                } else {
                    // 上游的 `name` 是「名 姓」写法（实测「亮 欧阳」），只在
                    // 前两级都取不到时兜底 —— 直接用它会让同一个账号在界面
                    // 上多出一种写法
                    pick_string(info, &["name"])
                }
            }
        };
        if !display.is_empty() {
            credentials.name = display;
        }
        // 账号 id 也顺手校准：`userInfo.clineUserId` 与 auth 的 `accountId`
        // 是同一个值，但前者在 `metadata` 里更靠内，缺 accountId 时能兜住
        if credentials.account.is_empty() {
            credentials.account = pick_string(info, &["clineUserId", "subject"]);
        }
    }
    Ok(credentials)
}

// ─── 凭证缓存（桌面端登录态的内存刷新覆盖）───────────────────

/// 进程级凭证缓存：`(缓存键, 解析好的凭证)`，只有一格。
///
/// ── 这个缓存解决什么问题（本次修复）─────────────────────────
/// 桌面端账号的凭证**不回写文件**（`refresh::persist_refresh` 对这类账号早退，
/// 理由是 providers.json 属于 Cline 客户端，两边都轮换会互相顶掉）。但「不回写」
/// 不等于「刷新结果应该丢掉」：401 之后的重试链路刷新成功 → 重取会话 → 会话
/// 构造又去**实时读文件**（`account_store::store::live_desktop_credentials`）→
/// 拿回同一个刚被拒绝的旧 token → 必然再 401。用户看到的是「刷新了却还是失败」。
///
/// 因此刷新结果写进这份缓存：后续取凭证时先看缓存，命中就用它。**仍然不落盘**，
/// 所以「不与客户端抢文件」的设计意图原样保留；重启后缓存消失，回到读文件，
/// 也仍然能跟上客户端自己的续期。
///
/// ── 缓存键为什么带 mtime ────────────────────────────────────
/// 键是 `providers:<文件 mtime>`。文件一变（用户在 Cline 客户端重新登录、
/// 或客户端自己续期写盘）键就失配 → 自动回到「重新读文件」，不需要额外的
/// 失效逻辑。这正是 AutoClaw 那边 `auth:<mtime>` 的同款语义（见
/// `autoclaw::credentials` 的模块说明），两家的取舍保持一致。
///
/// 之所以是**一格**：桌面端登录态全局只有一份（`~/.cline/data/settings/providers.json`）。
fn credentials_cache() -> &'static Mutex<Option<(String, ClineCredentials)>> {
    static CACHE: OnceLock<Mutex<Option<(String, ClineCredentials)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 桌面端登录态文件的缓存键（`providers:<mtime>`）；文件取不到时给 None。
fn desktop_cache_key() -> Option<String> {
    let path = desktop_settings_path().ok()?;
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    Some(format!("providers:{modified}"))
}

/// 查缓存（锁在返回前释放 → 调用方不会持锁跨 await）
fn lookup_cached(cache_key: &str) -> Option<ClineCredentials> {
    let guard = match credentials_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .as_ref()
        .filter(|(key, _)| key == cache_key)
        .map(|(_, credentials)| credentials.clone())
}

/// 写缓存（**读盘成功后**与刷新成功后共用）。
///
/// 读盘路径必须写它 —— 这不只是为了省一次解析：`store_cached_if_current`
/// 的「比较」是拿缓存里的当前值当基准的，缓存为空时它会判定「无法确认当前
/// 登录态」而拒绝写入（见那里的说明）。少了这一步，刷新结果永远落不进缓存，
/// 本次修复就退化成没有效果。
fn store_cached(cache_key: &str, credentials: &ClineCredentials) {
    let mut guard = match credentials_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some((cache_key.to_string(), credentials.clone()));
}

/// **比较-再写**：只有缓存里仍是刷新前那份凭证、且来源文件也没变过时才写入。
///
/// 返回 `true` = 已写入；`false` = 登录态已被替换（客户端重新登录 → mtime 变 →
/// 缓存键变，或另一轮刷新先落地），此时**不写** —— 旧结果不得盖掉新登录态。
///
/// 为什么还要看文件：缓存里的键可能一直没被读盘路径刷新，此时「键 + 内容都还是
/// 旧值」会骗过纯缓存比较；多查一次文件 mtime 才能在落缓存前确认来源确实没变。
/// 文件被删/损坏时同样拒绝写入（没有可确认的原文件，不替用户猜）。
pub(crate) fn store_cached_if_current(
    cache_key: &str,
    expected: &ClineCredentials,
    credentials: &ClineCredentials,
) -> bool {
    if desktop_cache_key().as_deref() != Some(cache_key) {
        return false;
    }
    let mut guard = match credentials_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some((current_key, current)) = guard.as_ref() else {
        // 缓存已被清空：无法确认当前登录态是否仍是刷新前那份，不写入
        return false;
    };
    if current_key != cache_key
        || current.access_token != expected.access_token
        || current.refresh_token != expected.refresh_token
    {
        return false;
    }
    *guard = Some((cache_key.to_string(), credentials.clone()));
    true
}

/// 读取本机桌面端登录态文件并解析。
///
/// 返回 `Ok(None)` = 文件不存在（没装 / 没登录过 Cline，不是错误）；
/// `Ok(Some(..))` = 读到了；`Err` = 文件在但内容不可用（解析失败 / 缺凭证）。
///
/// ── 先查缓存（本次修复）─────────────────────────────────────
/// 刷新成功的结果落在进程缓存里（见 [`credentials_cache`]），命中就直接返回它，
/// 这样 401 后的重试才拿得到**新** token 而不是重读文件拿回旧的那份。
pub fn read_desktop_credentials() -> Result<Option<ClineCredentials>, GatewayError> {
    let path = desktop_settings_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let cache_key = desktop_cache_key();
    if let Some(key) = cache_key.as_deref() {
        if let Some(credentials) = lookup_cached(key) {
            return Ok(Some(credentials));
        }
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        GatewayError::with_status(
            500,
            format!("读取本机 Cline 登录态失败: {error}"),
        )
    })?;
    let root: Value = serde_json::from_str(&text).map_err(|error| {
        GatewayError::with_status(
            400,
            format!("本机 Cline 登录态文件格式无效: {error}"),
        )
    })?;
    let mut credentials = credentials_from_desktop_settings(&root)?;
    // id 的填充留给调用方（账号层按 provider 拼 `cline-free-desktop` 这类 id，
    // 见 `desktop_account_id`）—— 桌面登录态本身不属于任何一个池，两个池都能用。
    credentials.cache_key = cache_key.clone();
    // 读盘成功后落缓存：既省掉后续请求的重复解析，也让「比较-再写」有基准
    // （见 `store_cached` 的说明）。cache_key 为 None（文件不可 stat）时不写，
    // 那种情况下刷新结果也确实无处可落。
    if let Some(key) = cache_key.as_deref() {
        store_cached(key, &credentials);
    }
    Ok(Some(credentials))
}

/// 桌面端登录态文件的绝对路径（`~/.cline/data/settings/providers.json`）。
pub fn desktop_settings_path() -> Result<std::path::PathBuf, GatewayError> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| {
            GatewayError::with_status(500, "无法确定用户主目录，读取不到 Cline 登录态")
        })?;
    let mut path = std::path::PathBuf::from(home);
    for segment in DESKTOP_SETTINGS_RELATIVE.split('/') {
        path.push(segment);
    }
    Ok(path)
}

/// 取候选键里第一个非空字符串（去空白）
fn pick_string(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// 数字取值（容忍字符串形态的数字）
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}
