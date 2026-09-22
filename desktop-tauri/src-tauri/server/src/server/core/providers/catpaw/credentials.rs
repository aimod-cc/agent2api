//! CatPaw 凭证来源（Agent2API 二期 W5-T-d4；移植来源 `catpaw-local-auth.mjs`
//! 与 `account-store.mjs`）。
//!
//! ── 三处凭证来源（优先级照抄原项目 `resolveCredentials`）───────
//! ```text
//!   1. 账号记录（add_catpaw_account 手动添加 / 旧数据导入）
//!      → record.accessToken + record.uid|loginName
//!   2. 桌面端实时登录态：~/.meituan-catpaw/auth.json
//!      → auth.accessToken + account.uid|loginName（原项目 DESKTOP_ACCOUNT_ID）
//!   3. 环境变量旁路：CATPAW_COOKIE（+ CATPAW_USER_UID）
//!      → 脚本 / CI 用户的常规用法（原项目 `runtimeConfig` 的 explicitAuth）
//! ```
//! 与 workbuddy / 小浣熊的**本质差别**：CatPaw 的凭证不是 Bearer 也不是 JWT，
//! 而是桌面端会话 Cookie 里的 `X-Passport-Token` 值，且 `uid` 是**独立的请求头**
//! （不在 token 里，所以必须与 token 一起取到，见架构文档 §9.1）。
//! 因此本模块的产出不是一个字符串，而是 [`CatPawCredentials`]（token + uid）。
//!
//! ── 没有刷新机制（架构文档 §9.1；原项目同）───────────────────
//! CatPaw 没有 refreshToken 概念：`X-Passport-Token` 过期后只能**在桌面端重新
//! 登录**（auth.json 会被客户端刷新，所以桌面端账号是「重新登录即恢复」的）。
//! 本模块因此只做「读凭证」，没有任何刷新接口 —— 账号记录里的 token 过期后
//! 由用户重新导入（适配器的 `refresh_access_token` 会明确报这一点，不假装能续期）。
//!
//! ── 桌面端登录态文件（原项目 `createLocalAuthProvider` 的唯一事实来源）──
//! 路径 `~/.meituan-catpaw/auth.json`（与代理账号文件同目录）；
//! 字段 `auth.accessToken` / `auth.loginType`（非空且不是 `passport` 时拒绝）/
//! `account.uid` / `account.loginName`。读取带原项目的三处防御：必须是**普通文件**
//! （符号链接拒绝）、大小在 `(0, 64KB]`、根必须是 JSON 对象。
//!
//! ── 为什么不缓存（与小浣熊的 mtime 缓存不同）─────────────────
//! 原项目每次调用都重新 `open + read`（`readAuthJson` 没有缓存），而桌面端会在
//! 用户重新登录后立刻改写这个文件 —— 缓存的收益（省一次几十 KB 的读盘）远小于
//! 「读到旧 token」的代价。这里保持原实现：每次实时读。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本文件只有文件读取与环境变量读取：**不发网络请求、不持锁**；
//! 绝不 unwrap/expect（release 是 panic=abort）。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::conversation::CatPawCredentials;
use super::{DEFAULT_BASE_URL, APP_KEY};

/// 桌面端实时账号的固定 id（原项目 `account-store.mjs` 的 `DESKTOP_ACCOUNT_ID`）。
///
/// 与原项目逐字一致（不是小浣熊那种 `<provider>-desktop` 形态）：账号 id 是
/// 落进 accounts.json 与前端列表的契约，沿用原值让从旧代理迁移过来的用户
/// 一眼能认出同一条记录。
pub const DESKTOP_ACCOUNT_ID: &str = "desktop-auth";

/// 登录态文件大小上限（原项目 `MAX_AUTH_FILE_SIZE`：64KB）
const MAX_AUTH_FILE_SIZE: u64 = 64 * 1024;

/// 凭证字段长度上限（原项目 `MAX_TOKEN_LENGTH` / `MAX_USER_UID_LENGTH`）
const MAX_TOKEN_LENGTH: usize = 8192;
const MAX_USER_UID_LENGTH: usize = 256;

// ─── 路径 ──────────────────────────────────────────────────────

/// CatPaw 配置目录（`~/.meituan-catpaw`，原项目账号文件与登录态都在这里）
///
/// 环境变量覆盖沿用原项目的写法：`CATPAW_HOME` 不是原项目的变量，因此**不引入**
/// —— 路径的唯一事实来源就是 `homedir()/.meituan-catpaw`。
pub fn catpaw_home() -> PathBuf {
    home_dir().join(".meituan-catpaw")
}

/// 桌面端登录态文件（`~/.meituan-catpaw/auth.json`）
pub fn desktop_auth_file() -> PathBuf {
    catpaw_home().join("auth.json")
}

/// 用户主目录（Windows 优先 USERPROFILE，其它平台 HOME）
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 上游 base URL：`CATPAW_UPSTREAM_BASE_URL` 可覆盖（原项目同名环境变量），
/// 末尾斜杠去掉、空白串视为未设置。
///
/// 保留这个覆盖点而不是写死常量：原项目用它做本地联调（指向本机 http 服务），
/// 网关的 `ConversationRequest.base_url` 也留着同一个口子（见那边的字段说明）。
pub fn upstream_base_url() -> String {
    std::env::var("CATPAW_UPSTREAM_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

// ─── 环境变量旁路 ──────────────────────────────────────────────

/// 环境变量凭证（`CATPAW_COOKIE` + `CATPAW_USER_UID`）；没配时 None。
///
/// `CATPAW_COOKIE` 在原项目里是**整个 Cookie 头的值**（形如
/// `X-Passport-Token=xxx; other=yyy`），直接拼进 `headers.Cookie`。
/// 本网关的传输层只发 `Cookie: X-Passport-Token=<token>`（架构文档 §9.1 的头集合），
/// 所以这里把 token 从 cookie 串里**解析出来**：认得出 `X-Passport-Token=…` 就用
/// 它的值，认不出就把整串当 token（原项目也允许只给裸 token 的场景）。
pub fn env_credentials() -> Option<CatPawCredentials> {
    let cookie = std::env::var("CATPAW_COOKIE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let uid = std::env::var("CATPAW_USER_UID")
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    Some(CatPawCredentials::new(passport_token_of(&cookie), uid))
}

/// 环境变量旁路凭证此刻是否存在（聚合目录的可用性判据用）
pub fn env_credentials_present() -> bool {
    env_credentials().is_some()
}

/// 从 Cookie 串里取 `X-Passport-Token` 的值；取不到时原样返回整串。
///
/// 匹配规则：分号分隔的每一段去空白后前缀匹配 `X-Passport-Token=`（大小写不敏感），
/// 值取到下一个分号为止。没有匹配项时把传入串当 token —— 原项目允许
/// `CATPAW_COOKIE` 直接是 `X-Passport-Token=<token>` 也可能是裸 token。
///
/// ── 为什么用 `get(..)` 而不是 `split_at`（panic=abort 的硬约束）────
/// `split_at(17)` 在**第 17 个字节落在一个多字节字符中间**时会 panic
/// （例如整串以四个字节的 emoji 开头），而这是用户可粘贴的任意文本。
/// `str::get(..n)` 越界或不在字符边界都只返回 None。本项目 release 是
/// panic=abort，一次 panic 会带走整个桌面应用，所以这类切片一律走安全取值。
fn passport_token_of(cookie: &str) -> String {
    const KEY: &str = "x-passport-token=";
    for segment in cookie.split(';') {
        let trimmed = segment.trim();
        let Some(head) = trimmed.get(..KEY.len()) else {
            continue;
        };
        if head.eq_ignore_ascii_case(KEY) {
            let value = trimmed[KEY.len()..].trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    cookie.trim().to_string()
}

// ─── 桌面端登录态 ─────────────────────────────────────────────

/// 一次登录态读取的产物（原项目 `getCredentials()` 的返回形态）
#[derive(Clone, Debug, Default)]
pub struct DesktopLogin {
    /// `auth.accessToken`
    pub token: String,
    /// `account.uid` 或 `account.loginName`（原项目 `userUid`）
    pub uid: String,
    /// `account.loginName`（展示名用；可能为空）
    pub login_name: String,
    /// 文件修改时间（毫秒；原项目 `modifiedAt`）
    pub modified_at: i64,
}

impl DesktopLogin {
    /// token 尾 4 字符（原项目 `tokenTail`）
    pub fn token_tail(&self) -> String {
        let chars: Vec<char> = self.token.chars().collect();
        let start = chars.len().saturating_sub(4);
        chars[start..].iter().collect()
    }

    /// 转成适配器要的凭证形态
    pub fn credentials(&self) -> CatPawCredentials {
        CatPawCredentials::new(self.token.clone(), self.uid.clone())
    }
}

/// 读桌面端登录态（`~/.meituan-catpaw/auth.json`）。
///
/// 失败原因直接是**用户可见的中文文案**（原项目 `LocalAuthError` 的措辞），
/// 调用方（账号导入 / 转发）把它包进自己的错误里。
pub fn read_desktop_login() -> Result<DesktopLogin, String> {
    let path = desktop_auth_file();
    read_login_file(&path)
}

/// 读一个「CatPaw 登录态」文件（路径参数化：桌面端与将来可能的多来源共用）。
///
/// 防御项逐条照抄原项目 `readAuthJson`：普通文件（符号链接拒绝）、
/// 大小在 `(0, 64KB]`、可解析且根是 JSON 对象。
fn read_login_file(path: &std::path::Path) -> Result<DesktopLogin, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "未找到 CatPaw 本地登录态（{}），请先在 CatPaw 桌面端登录",
                path.display()
            )
        } else {
            format!("无法访问 CatPaw 本地登录态（{}）: {error}", path.display())
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err("CatPaw 本地登录态路径是符号链接，已拒绝读取".to_string());
    }
    if !meta.is_file() {
        return Err("CatPaw 本地登录态路径不是普通文件".to_string());
    }
    if meta.len() == 0 || meta.len() > MAX_AUTH_FILE_SIZE {
        return Err("CatPaw 本地登录态文件大小异常".to_string());
    }
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("读取 CatPaw 登录态失败: {error}"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| "CatPaw 本地登录态文件无法解析".to_string())?;
    let Value::Object(root) = value else {
        return Err("CatPaw 本地登录态根节点不是 JSON 对象".to_string());
    };
    let auth = root.get("auth").and_then(Value::as_object);
    let account = root.get("account").and_then(Value::as_object);
    // loginType 只允许空或缺省 / passport（原项目同）
    let login_type = auth
        .and_then(|auth| auth.get("loginType"))
        .map(text_value)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !login_type.is_empty() && login_type != "passport" {
        return Err(format!("当前 CatPaw 登录方式 {login_type} 暂不支持自动直连"));
    }
    let token = safe_field(
        auth.and_then(|auth| auth.get("accessToken"))
            .map(text_value)
            .unwrap_or_default(),
        "auth.accessToken",
        MAX_TOKEN_LENGTH,
    )?;
    let uid_raw = account
        .and_then(|account| account.get("uid"))
        .filter(|value| !value.is_null())
        .map(text_value)
        .unwrap_or_default();
    let login_raw = account
        .and_then(|account| account.get("loginName"))
        .filter(|value| !value.is_null())
        .map(text_value)
        .unwrap_or_default();
    let uid = if uid_raw.trim().is_empty() {
        String::new()
    } else {
        safe_field(uid_raw, "account.uid", MAX_USER_UID_LENGTH)?
    };
    let login_name = if login_raw.trim().is_empty() {
        String::new()
    } else {
        safe_field(login_raw, "account.loginName", MAX_USER_UID_LENGTH)?
    };
    if uid.is_empty() && login_name.is_empty() {
        return Err("CatPaw 本地登录态缺少 account.uid 或 account.loginName".to_string());
    }
    Ok(DesktopLogin {
        token,
        uid: if uid.is_empty() { login_name.clone() } else { uid },
        login_name,
        modified_at,
    })
}

/// 字段校验（原项目 `safeString`）：非空、不超长、不含 CR/LF/分号。
///
/// 为什么要拦 `[\r\n;]`：这些值最终进 HTTP 头（Cookie / user-uid），换行是头注入，
/// 分号会让 Cookie 串被解析成另一段 —— 原项目因此在这里直接拒绝。
fn safe_field(value: String, field: &str, max_length: usize) -> Result<String, String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Err(format!("CatPaw 本地登录态缺少 {field}"));
    }
    if trimmed.chars().count() > max_length
        || trimmed.contains(['\r', '\n', ';'])
    {
        return Err(format!("CatPaw 本地登录态中的 {field} 格式无效"));
    }
    Ok(trimmed)
}

/// 桌面端登录态的**摘要**（导入账号记录 / 公开形态展示用）。
///
/// 返回 `{uid, loginName, tokenTail, modifiedAt, name}`；读不到时 Err（原因给用户看）。
/// **不含 token 本身**：桌面端账号的凭证不落账号记录（每次实时读 auth.json），
/// 这与小浣熊桌面账号同一条纪律。
pub fn desktop_summary() -> Result<Value, String> {
    let login = read_desktop_login()?;
    Ok(json!({
        "uid": login.uid,
        "loginName": login.login_name,
        "tokenTail": login.token_tail(),
        "modifiedAt": login.modified_at,
    }))
}

/// 桌面端凭证（转发路径用；读不到时给 503 + 可读原因）。
///
/// 状态码用 **503** 而不是 401：这不是「网关拒绝了你的凭证」，而是「本机此刻
/// 没有可用的 CatPaw 登录态」（原项目 `LocalAuthError.statusCode = 503` 同）。
pub fn desktop_credentials() -> Result<CatPawCredentials, GatewayError> {
    read_desktop_login()
        .map(|login| login.credentials())
        .map_err(|reason| {
            GatewayError::with_status(503, format!("没有可用的 CatPaw 登录态：{reason}"))
        })
}

// ─── 统一入口（账号记录 / 桌面端 / 环境变量）───────────────────

/// 取一次会话转发要用的凭证。
///
/// 顺序（与 `run_conversation` 的入参构造一致）：
///   1. `account_id` 非空 → 该账号记录；记录带 `desktop` 标记（或 id 就是
///      [`DESKTOP_ACCOUNT_ID`]）→ 实时读 auth.json（记录里按设计不落 token）；
///   2. `account_id` 为空 → 桌面端实时登录态（本机的默认登录态就是它）；
///   3. 都没有 → Err（文案告诉用户该怎么补）。
///
/// 环境变量旁路**不在这里**：那条路径由适配器的 `ensure_access_token` 在
/// 「没有指定账号」时优先尝试（与 workbuddy / 小浣熊同一分工），本函数只认
/// 账号记录与桌面端文件。
pub fn snapshot_for(
    store: &AccountStore,
    account_id: &str,
) -> Result<CatPawCredentials, GatewayError> {
    let record = store.catpaw_account_record(account_id);
    let Some(record) = record else {
        return desktop_credentials();
    };
    let is_desktop = record
        .get("desktop")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || record.get("id").and_then(Value::as_str) == Some(DESKTOP_ACCOUNT_ID);
    if is_desktop {
        return desktop_credentials();
    }
    let token = record
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        let id = record.get("id").and_then(Value::as_str).unwrap_or("(未知)");
        return Err(GatewayError::with_status(
            503,
            format!("CatPaw 账号 {id} 没有可用凭证，请重新导入登录态"),
        ));
    }
    let uid = record
        .get("uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            record
                .get("loginName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();
    Ok(CatPawCredentials::new(token, uid))
}

/// 适配器请求头要用的 `M-APPKEY` 值（架构文档 §9.1）。
///
/// 放在本模块只是因为它与「这是哪一家」的常量挨着；真正的头集合在
/// `upstream_http::request_headers`，本函数只供排障与将来的可覆盖点使用。
#[allow(dead_code)]
pub fn app_key() -> &'static str {
    APP_KEY
}

/// 值 → 字符串（JS `String(x)`：数字/布尔按字面量、null 给空串）
fn text_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// 打一条凭证来源日志（排障用；不含 token 本身）
pub fn log_source(source: &str, account_id: &str, uid: &str) {
    logging::verbose(
        "[CatPaw]",
        &format!(
            "凭证来源 {source}（账号 {}，uid {}）",
            if account_id.is_empty() { "(默认登录态)" } else { account_id },
            if uid.is_empty() { "-" } else { uid },
        ),
    );
}
