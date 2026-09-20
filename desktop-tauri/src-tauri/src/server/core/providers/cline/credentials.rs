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
    /// 显示用账号标识（email 优先，其次 accountId）
    pub account: String,
    /// 备注名（用户可改；空则用 account）
    pub name: String,
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

/// 从 JWT 里解出用户标识（**用户名优先**，其次 email，最后账号 id）。
///
/// 只用于**展示与去重**，不参与鉴权判定。
///
/// 返回 `(展示名, 账号 id)`：
///   - 展示名：`lastName+firstName`（中文姓名习惯，实测 JWT 里是
///     `firstName:"亮"` / `lastName:"欧阳"`，拼成「欧阳亮」）→ email → 账号 id。
///     用户认得出的是自己的名字，其次是邮箱，最次才是 `usr-…` 那串 id；
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
    // 两者都空时留空串，交给下面的 email 兜底 —— 不拼出一个只有空格的假名。
    let person = person_name(&text("lastName"), &text("firstName"));
    let display = if !person.is_empty() {
        person
    } else if !email.is_empty() {
        email
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

/// 从账号记录（accounts.json 的一条）解析凭证。
///
/// ── 与其他几家的键名口径一致 ────────────────────────────────
/// 落盘用 `accessToken` / `refreshToken` / `expiresAt`（与 W3 为小浣熊确立的
/// 口径一致），读取侧同时兼容上游 providers.json 的原名（`accessToken` 同名，
/// refresh 的 snake_case 变体 `refresh_token` 也认）。
///
/// ── 展示名的来源顺序（用户名优先）────────────────────────────
/// 记录里没有 token 的 JWT 时（桌面端账号的凭证在客户端文件里）也要能拿到
/// 展示名，所以顺序是：记录里的 `displayName`（续期/登录时从 userInfo 抽出来
/// 落下的）→ JWT 的姓名 → 记录里的 `account`（email 或 `usr-…`）。
/// 桌面端那条链另外在 [`credentials_from_desktop_settings`] 里读
/// `metadata.userInfo.name`，见那里。
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
/// ── 展示名从 `metadata.userInfo` 取（用户名优先）──────────────
/// 桌面端登录态里有一份完整的 `userInfo`（实测字段：`firstName` / `lastName`
/// / `email` / `name` / `subject` / `clineUserId`），比 JWT 声明还全 ——
/// 而且**桌面端账号的记录里不落 token**，解析凭证时读的正是这份文件，
/// 顺手把姓名带出来最省事。顺序：`firstName`+`lastName` 拼 → `name` → email。
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
            let person = person_name(
                info.get("lastName").and_then(Value::as_str).unwrap_or(""),
                info.get("firstName").and_then(Value::as_str).unwrap_or(""),
            );
            if !person.is_empty() {
                person
            } else {
                // 上游的 `name` 是「名 姓」写法（实测「亮 欧阳」），只在拼不出
                // 姓名时兜底 —— 直接用它会让同一个人的名字在界面上换个写法
                pick_string(info, &["name", "email"])
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

/// 读取本机桌面端登录态文件并解析。
///
/// 返回 `Ok(None)` = 文件不存在（没装 / 没登录过 Cline，不是错误）；
/// `Ok(Some(..))` = 读到了；`Err` = 文件在但内容不可用（解析失败 / 缺凭证）。
pub fn read_desktop_credentials() -> Result<Option<ClineCredentials>, GatewayError> {
    let path = desktop_settings_path()?;
    if !path.exists() {
        return Ok(None);
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
    let credentials = credentials_from_desktop_settings(&root)?;
    // id 的填充留给调用方（账号层按 provider 拼 `cline-free-desktop` 这类 id，
    // 见 `desktop_account_id`）—— 桌面登录态本身不属于任何一个池，两个池都能用。
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
