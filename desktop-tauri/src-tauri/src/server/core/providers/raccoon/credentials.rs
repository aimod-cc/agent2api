//! 小浣熊的凭证处理（Agent2API 改造 W3-T4；移植来源 `raccoon-local-auth.mjs`）。
//!
//! ── 本文件负责什么 ──────────────────────────────────────────
//!   1. **凭证来源**：账号记录里的 token/refreshToken；或桌面端**实时登录态**
//!      （`~/.box-agent/config/auth.json`，小浣熊生态共享文件，客户端重新登录后
//!      下次请求即生效）。桌面端文件带 mtime 缓存，避免每个请求都读盘。
//!   2. **刷新与回写**：调鉴权 API；单飞（同一账号同一 refreshToken 并发只发一次
//!      真实请求，后到者复用结果）；刷新结果**按来源**回写 —— 桌面端回写 auth.json
//!      （保留 office_identity 等未知字段），手动账号回写账号存储。
//!   JWT 解码（不验签，只读 exp/name/iss）在 `jwt.rs`，本文件只用它的结果。
//!
//! ── 为什么与 workbuddy 的 auth.rs 不共用 ─────────────────────
//! 两者的凭证形态与刷新协议完全不同：workbuddy 是「账号记录 + /auth/token/refresh
//! 的 X-Refresh-Token 头 + 企业条件头」，小浣熊是「JWT + POST {authBase}/refresh
//! 的 `{refresh_token}` body + access_token/refresh_token 响应体」。共用会变成
//! 一堆 `if provider == ...`。共用的只有**不变量**：持账号锁期间绝不做网络请求、
//! 绝不 unwrap/expect。
//!
//! ── 与源实现的差异（有意）──────────────────────────────────
//!   - 源实现把解析出的凭证缓存在闭包里；这里每次都从账号存储取快照（账号文件
//!     本来就是「实时走磁盘」的语义），只有 auth.json 那一路有 mtime 缓存。
//!   - 源实现的 `persistMode: 'none'`（环境变量凭证）在本期不落地为「回写目标」
//!     —— 环境变量是静态的，没有可回写的来源（`snapshot_for` 也不会返回它）。
//!
//! ── 刷新生命周期（本次修复）─────────────────────────────────
//! 单飞表**只保存进行中的刷新**（`refresh_flight` 原语）：本轮结束即移除，因此
//! `force = true`（401 后的强制刷新）在上一轮完成后一定发起新请求，失败也不会被
//! 长期缓存；leader 的 future 被 drop（客户端断开）时 RAII 守卫释放占位并唤醒
//! 等待者，后续请求可以重试。回写改为**比较-再写**：桌面端登录态只在文件当前
//! token 仍等于刷新前那份快照时才写回，账号记录只在记录里仍是刷新前那份凭证时
//! 才写回（同一把账号锁内比较 + 写入）；被判定过期的结果改用最新快照。
//!
//! ── 关于 Send 的硬约束（踩过的坑）───────────────────────────
//! 适配器的 `ensure_access_token` / `refresh_access_token` 返回的 future 必须是
//! `Send`（转发跑在多线程运行时上，future 会跨 await 点被持有）。因此本文件
//! **任何 async fn 内部都不得持有非 Send 的守卫**：`auth_cache()` 的
//! `MutexGuard<Option<Map>>` 就不是 Send。做法是让「读 auth.json」这条链上
//! 的每个 async 函数都只调用**同步**函数（同步函数内的守卫在返回前必然析构），
//! 绝不在 async 块里直接 `if let Ok(guard) = cache.lock()`。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::account_store::CredentialWrite;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::jwt;

/// 共享单飞原语（`providers::refresh_flight`）：只保存进行中的刷新、leader 取消
/// 时 RAII 清理并唤醒等待者。两家的表按凭证类型分开，互不影响。
use crate::server::core::providers::refresh_flight;

/// 桌面端实时账号的固定 id（架构文档 §3.2：导入后为 raccoon 的一个普通列表项）
pub const DESKTOP_ACCOUNT_ID: &str = "raccoon-desktop";

/// 桌面端登录态文件的大小上限（照抄 `raccoon-local-auth.mjs` 的
/// `MAX_AUTH_FILE_SIZE`）。超限说明这个路径上是个异常文件，直接拒绝比把它
/// 整体读进内存安全。
const MAX_AUTH_FILE_SIZE: u64 = 256 * 1024;

/// 鉴权 API 请求超时（源实现 `REQUEST_TIMEOUT_MS`；鉴权接口是普通短请求，
/// 15 分钟的 LLM 超时在这里不适用）。
pub(super) const AUTH_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 临期主动刷新窗口：过期前 5 分钟（源实现 `PROACTIVE_REFRESH_MARGIN_MS`，
/// 与 box_agent 行为一致）。
pub(super) const PROACTIVE_REFRESH_MARGIN_MS: f64 = 300_000.0;

/// 一份解析好的小浣熊凭证（源实现 `credentials` 对象的等价物）。
#[derive(Clone, Debug, Default)]
pub struct RaccoonCredentials {
    /// 账号 id（`raccoon-desktop` 或 `user-<userId>`）
    pub id: String,
    pub token: String,
    pub refresh_token: String,
    /// 过期时间（毫秒）。JWT 解不出 exp 时为 None
    pub expires_at: Option<f64>,
    /// 用户 ID（JWT 的十六进制 id 约定，见 `jwt::extract_user_id`）
    pub user_id: String,
    /// 凭证来源：决定刷新结果回写到哪里
    pub origin: CredentialOrigin,
}

/// 凭证来源（决定刷新后往哪回写）
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CredentialOrigin {
    /// 账号记录里的 token（手动添加/旧数据导入）→ 回写账号存储
    #[default]
    AccountStore,
    /// `~/.box-agent/config/auth.json`（桌面端实时登录态）→ 回写 auth.json
    AuthFile,
}

impl RaccoonCredentials {
    /// 现在是否需要刷新（源实现 `isCredentialsExpiring`：expiresAt 缺失时
    /// 不刷新 —— 没有依据就说「临期」会让每个请求都去打一次刷新接口）。
    pub fn is_expiring(&self) -> bool {
        match self.expires_at {
            Some(expires_at) => {
                expires_at - PROACTIVE_REFRESH_MARGIN_MS <= logging::now_ms() as f64
            }
            None => false,
        }
    }

    /// 能否刷新：有 refreshToken 才行
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.is_empty()
    }
}

// ─── 桌面端登录态文件（~/.box-agent/config/auth.json）──────────

/// 桌面端登录态的读取结果：JSON 对象 + 文件修改时间
struct AuthFileSnapshot {
    json: Map<String, Value>,
    modified_at: i64,
}

/// box-agent 配置目录（源实现 `defaultBoxAgentConfigDir`）：
/// `RACCOON_BOX_AGENT_CONFIG_DIR` > `BOX_AGENT_CONFIG_DIR` > `~/.box-agent/config`。
fn box_agent_config_dir() -> PathBuf {
    for name in ["RACCOON_BOX_AGENT_CONFIG_DIR", "BOX_AGENT_CONFIG_DIR"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed);
            }
        }
    }
    home_dir().join(".box-agent").join("config")
}

/// 用户主目录（Windows 优先 USERPROFILE，其它平台 HOME）
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// auth.json 的完整路径（导入与回写都要用）
pub fn desktop_auth_file() -> PathBuf {
    box_agent_config_dir().join("auth.json")
}

/// mtime 缓存：`(路径, mtime, 文件内容)`。
///
/// 源实现按 mtime 做缓存（`cacheKey = "auth:${modifiedAt}"`）：桌面端/box-agent
/// 会刷新并回写这个文件，所以「按 mtime 实时读盘」既能拿到最新登录态，
/// 又不会让每个转发请求都做一次文件 IO。缓存是**进程级**的：多个调用点共享
/// 同一份，避免各自持有过期快照。
fn auth_cache() -> &'static Mutex<Option<(String, i64, Map<String, Value>)>> {
    static CACHE: OnceLock<Mutex<Option<(String, i64, Map<String, Value>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 文件形态防御（源实现照抄）：路径必须是**普通文件**（不是符号链接）、大小在
/// `(0, 256KB]` 之间。返回文件 mtime（毫秒）。
fn auth_file_meta(path: &Path) -> Result<i64, String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| format!("无法访问小浣熊登录态 {}: {error}", path.display()))?;
    // 符号链接与非常规文件一律拒绝（symlink_metadata 不会被解引用）
    if meta.file_type().is_symlink() {
        return Err("小浣熊登录态路径是符号链接，已拒绝读取".to_string());
    }
    if !meta.is_file() {
        return Err("小浣熊登录态路径不是普通文件".to_string());
    }
    if meta.len() == 0 || meta.len() > MAX_AUTH_FILE_SIZE {
        return Err("小浣熊登录态文件大小异常".to_string());
    }
    Ok(mtime_ms(&meta))
}

/// 读 auth.json（**不带缓存**）：形态防御 + 解析。
///
/// 比较写回（`persist_auth_file_if_current`）必须走这条路径：缓存按 mtime 命中，
/// 而「同一毫秒内的两次写入」在 Windows 上可能拿到相同 mtime，只有真读盘才能
/// 看到内容变化。
fn read_auth_file_raw(path: &Path) -> Result<AuthFileSnapshot, String> {
    let modified_at = auth_file_meta(path)?;
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("读取小浣熊登录态失败: {error}"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| "小浣熊登录态无法解析（不是有效 JSON）".to_string())?;
    let Value::Object(json) = value else {
        return Err("小浣熊登录态根节点不是 JSON 对象".to_string());
    };
    Ok(AuthFileSnapshot { json, modified_at })
}

/// 读 auth.json（带 mtime 缓存与文件形态防御；防御项见 `auth_file_meta`）。
///
/// **同步函数**（见模块头关于 Send 的说明）：守卫在本函数返回前析构，
/// 调用它的 async 函数因此不会持有非 Send 的跨 await 状态。
fn read_auth_file() -> Result<AuthFileSnapshot, String> {
    let path = desktop_auth_file();
    let modified_at = auth_file_meta(&path)?;
    let key = path.to_string_lossy().to_string();
    // 缓存命中：只做一次「取锁 → 比较 → 克隆」；锁在本表达式结束前释放
    let cached = match auth_cache().lock() {
        Ok(guard) => guard
            .as_ref()
            .filter(|(cached_key, cached_mtime, _)| {
                *cached_key == key && *cached_mtime == modified_at
            })
            .map(|(_, _, json)| json.clone()),
        Err(poisoned) => poisoned
            .into_inner()
            .as_ref()
            .filter(|(cached_key, cached_mtime, _)| {
                *cached_key == key && *cached_mtime == modified_at
            })
            .map(|(_, _, json)| json.clone()),
    };
    if let Some(json) = cached {
        return Ok(AuthFileSnapshot { json, modified_at });
    }
    let snapshot = read_auth_file_raw(&path)?;
    if let Ok(mut guard) = auth_cache().lock() {
        *guard = Some((key, snapshot.modified_at, snapshot.json.clone()));
    }
    Ok(snapshot)
}

/// 文件的修改时间（毫秒）；取不到给 0
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 auth.json 的字段读成凭证（源实现 `getCredentials` 的字段口径）：
/// `access_token` ?? `token` ?? `auth_token`；`refresh_token` 单独取。
fn credentials_from_auth_file(snapshot: &AuthFileSnapshot) -> Result<RaccoonCredentials, String> {
    let pick = |keys: &[&str]| -> String {
        for key in keys {
            if let Some(Value::String(text)) = snapshot.json.get(*key) {
                let value = jwt::strip_bearer(text);
                if !value.is_empty() {
                    return value;
                }
            }
        }
        String::new()
    };
    let token = pick(&["access_token", "token", "auth_token"]);
    if token.is_empty() {
        return Err("小浣熊登录态里没有 access_token（请在客户端重新登录）".to_string());
    }
    let refresh_token = pick(&["refresh_token"]);
    let claims = jwt::decode_jwt_claims(&token);
    let user_id = claims.as_ref().map(jwt::extract_user_id).unwrap_or_default();
    Ok(RaccoonCredentials {
        id: DESKTOP_ACCOUNT_ID.to_string(),
        expires_at: jwt::jwt_expiry_ms(&token),
        token,
        refresh_token,
        user_id,
        origin: CredentialOrigin::AuthFile,
    })
}

/// 桌面端登录态的**摘要**（导入/刷新账号记录时写进账号字段）。
///
/// 返回含 `userId` / `tokenTail` / `tokenExpiresAt` / `hasRefreshToken` / `mtime`
/// 的对象；读不到时 Err。**不含 token 本身**：桌面端账号的凭证不落账号记录
/// （架构文档 §3.2）。
pub fn desktop_summary() -> Result<Value, String> {
    let snapshot = read_auth_file()?;
    let credentials = credentials_from_auth_file(&snapshot)?;
    Ok(json!({
        "userId": credentials.user_id,
        "tokenTail": token_tail(&credentials.token),
        "tokenExpiresAt": credentials
            .expires_at
            .map(crate::server::core::account_store::state::json_number)
            .unwrap_or(Value::Null),
        "hasRefreshToken": !credentials.refresh_token.is_empty(),
        "mtime": snapshot.modified_at,
    }))
}

/// token 尾 4 字符（源实现 `token.slice(-4)`）
pub(super) fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// 回写桌面端登录态：**保留未知字段**（office_identity / office_org_name /
/// office_org_role…），只替换 access_token 与 refresh_token。
///
/// ── 比较写回（本次修复）────────────────────────────────────
/// 只有「文件当前的 access_token / refresh_token 仍等于刷新前那份快照」时才写回；
/// 文件已被桌面端改写（重新登录、客户端自己刷新）时**拒绝覆盖**并返回
/// [`STALE_REFRESH_MESSAGE`]，由调用方改用最新快照。
///
/// 为什么不能无条件写：网关与桌面端共用同一个登录态文件，用户重新登录后文件里
/// 是新凭证，而一次更早发起、更晚返回的刷新会把**旧凭证**盖回去。旧实现还会在
/// 文件缺失/损坏时用空对象重建（等于清掉 `office_identity` 等字段），这里一并
/// 去掉：读不到当前内容就报错，绝不替用户重建登录态文件。
///
/// 写盘仍用「临时文件 + 原子 rename」；rename 前**再确认一次** mtime 与内容，
/// 把「读盘 → 替换」之间桌面端重新登录的窗口挡住。
fn persist_auth_file_if_current(
    expected: &RaccoonCredentials,
    access_token: &str,
    refresh_token: &str,
) -> Result<(), String> {
    let path = desktop_auth_file();
    // 读盘原值（不走缓存）：比较的是**文件此刻的内容**。
    // 读不到（缺失/损坏/被删）时**不重建**：登录态文件是用户的，网关无权替
    // 用户造一份；此时刷新结果无处可落，按「过期」处理，由调用方改用最新快照。
    let current = read_auth_file_raw(&path).map_err(|_| STALE_REFRESH_MESSAGE.to_string())?;
    let stored = credentials_from_auth_file(&current)?;
    if stored.token != expected.token || stored.refresh_token != expected.refresh_token {
        return Err(STALE_REFRESH_MESSAGE.to_string());
    }
    let seen_mtime = current.modified_at;
    let mut current = current.json;
    if !access_token.is_empty() {
        current.insert(
            "access_token".to_string(),
            Value::String(jwt::strip_bearer(access_token)),
        );
    }
    if !refresh_token.is_empty() {
        current.insert(
            "refresh_token".to_string(),
            Value::String(jwt::strip_bearer(refresh_token)),
        );
    }
    // 缓存先失效：写盘后 mtime 会变，但同一毫秒内的两次写入可能拿到相同时间戳
    // （Windows 的文件时间精度），不失效就可能读到旧内容
    if let Ok(mut guard) = auth_cache().lock() {
        *guard = None;
    }
    let Some(parent) = path.parent() else {
        return Err("小浣熊登录态路径没有父目录".to_string());
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        return Err(format!("创建配置目录失败: {error}"));
    }
    let text = serde_json::to_string_pretty(&Value::Object(current))
        .map_err(|error| format!("序列化登录态失败: {error}"))?;
    let temp = temp_path(&path);
    if let Err(error) = std::fs::write(&temp, format!("{text}\n")) {
        return Err(format!("写入临时文件失败: {error}"));
    }
    set_owner_only(&temp);
    if !auth_file_still_matches(&path, seen_mtime, expected) {
        let _ = std::fs::remove_file(&temp);
        return Err(STALE_REFRESH_MESSAGE.to_string());
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("替换登录态文件失败: {error}"));
    }
    Ok(())
}

/// rename 前的最后确认：文件 mtime 未变，且内容仍是刷新前那份凭证。
/// 读不到（被删/损坏/变成非常规文件）时视为「已变」→ 不替换。
fn auth_file_still_matches(
    path: &Path,
    seen_mtime: i64,
    expected: &RaccoonCredentials,
) -> bool {
    match read_auth_file_raw(path) {
        Ok(latest) => {
            latest.modified_at == seen_mtime
                && credentials_from_auth_file(&latest)
                    .map(|latest| {
                        latest.token == expected.token
                            && latest.refresh_token == expected.refresh_token
                    })
                    .unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// 临时文件路径（与目标同目录 = 同卷，rename 才能原子生效）。
/// 文件名带上进程号：桌面端与网关同时刷新时不会互相踩掉对方的临时文件。
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "auth.json".to_string());
    let temp_name = format!("{name}.{}.tmp", std::process::id());
    match path.parent() {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    }
}

/// 收紧文件权限到「仅属主可读写」（源实现给的是 `mode: 0o600`）。
///
/// Windows 没有 POSIX 权限位（`set_permissions` 只能表达只读标志），因此只在
/// 非 Windows 上执行；Windows 侧依赖用户目录本身的 ACL —— 与源实现一致
/// （Node 在 Windows 上同样忽略 mode）。
#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        logging::verbose("[Raccoon]", &format!("收紧登录态文件权限失败: {error}"));
    }
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

// ─── 凭证快照（从账号存储或 auth.json 取）──────────────────────

/// 取指定账号的凭证快照（**同步**：不持锁跨 await，见模块头）。
///
/// `account_id` 为空 → 用小浣熊组内的当前账号（`raccoon_account_record` 会按
/// provider 收窄）；没有账号记录时回落到桌面端实时登录态 —— 那台机器上的
/// 「默认登录态」事实上就是客户端登录的那个。
///
/// ── 为什么判据是记录里的 `desktop` 标记而不是只看 id ──────────
/// id `raccoon-desktop` 只是缺省值。判据落在标记上更稳，id 只作兜底。
pub fn snapshot_for(
    store: &AccountStore,
    account_id: &str,
) -> Result<RaccoonCredentials, GatewayError> {
    let record = store.raccoon_account_record(account_id);
    let Some(record) = record else {
        // 没有账号记录：默认登录态 = 桌面端实时登录态
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
        .to_string();
    if token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            format!(
                "账号 {} 没有可用凭证，请重新添加",
                record
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("(未知)")
            ),
        ));
    }
    let refresh_token = record
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_at = record
        .get("expiresAt")
        .and_then(Value::as_f64)
        .or_else(|| jwt::jwt_expiry_ms(&token));
    Ok(RaccoonCredentials {
        id: record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        token,
        refresh_token,
        expires_at,
        user_id: record
            .get("userId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        origin: CredentialOrigin::AccountStore,
    })
}

/// 桌面端实时登录态的凭证（读不到时给 401，文案说明该怎么做）。
/// 公开（`pub`）是为了让账号存储构造会话时能取到实时 token（见 `store.rs`）。
pub fn desktop_credentials() -> Result<RaccoonCredentials, GatewayError> {
    let snapshot = read_auth_file().map_err(|reason| {
        GatewayError::with_status(401, format!("没有可用的小浣熊登录态：{reason}"))
    })?;
    credentials_from_auth_file(&snapshot).map_err(|reason| GatewayError::with_status(401, reason))
}

// ─── 刷新（单飞 + 比较写回）──────────────────────────────────

/// 进程级单飞表：**只保存进行中的刷新**（`refresh_flight` 原语，见模块头）。
///
/// key = `{账号 id}:{来源}:{refreshToken 指纹}`，三个成分各有作用：
///   - 账号 id：不同账号绝不互相复用；
///   - 来源（账号记录 / 桌面端登录态）：同一个 userId 在两条来源上是两份凭证、
///     两个回写目标，混用会让桌面端登录态的结果被写进账号记录；
///   - refreshToken 指纹：轮换后（或用户重导入换了 refreshToken）等待者不会误取
///     到另一轮刷新的结果。指纹是截断 SHA-256，**完整 token 不进 key、不打印**。
fn inflight_table() -> &'static refresh_flight::Table<RaccoonCredentials> {
    static TABLE: OnceLock<refresh_flight::Table<RaccoonCredentials>> = OnceLock::new();
    TABLE.get_or_init(refresh_flight::Table::new)
}

/// 刷新 key（构成与理由见 `inflight_table` 的说明）。
///
/// 桌面端来源额外带一个**文件版本语境**（路径指纹 + mtime）：客户端重新登录后
/// 即使 refreshToken 恰好还是旧值，新登录态的刷新也不该与旧登录态的一轮合并。
fn refresh_key(credentials: &RaccoonCredentials) -> String {
    let source = match credentials.origin {
        CredentialOrigin::AuthFile => "auth",
        CredentialOrigin::AccountStore => "store",
    };
    let mut key = format!(
        "{}:{source}:{}",
        credentials.id,
        refresh_flight::fingerprint(&credentials.refresh_token)
    );
    if credentials.origin == CredentialOrigin::AuthFile {
        let path = desktop_auth_file();
        let version = std::fs::symlink_metadata(&path)
            .ok()
            .map(|meta| mtime_ms(&meta))
            .unwrap_or(0);
        key.push(':');
        key.push_str(&refresh_flight::fingerprint(&format!(
            "{}:{version}",
            path.to_string_lossy()
        )));
    }
    key
}

/// 刷新凭证（单飞；结果按来源**比较-再写**）。
///
/// `force = false` 时只在**临期**才刷新（`ensure_access_token` 的语义）；
/// `force = true` 时无条件刷新（401 之后的 `refresh_access_token` —— 被拒的
/// token 可能时间上还很新，只看临期窗口会拿回同一个坏 token）。
///
/// ── 生命周期（本次修复）────────────────────────────────────
///   - 单飞表里没有「已完成」的记录：本轮结果只交给**已加入这一轮的等待者**
///     （结果放在 `Arc<Flight>` 里），表项在本轮结束时释放 → `force = true` 的
///     下一轮一定发起新请求，失败也不会被长期缓存；
///   - leader 的 future 被 drop（客户端断开）时，RAII 守卫释放占位并唤醒等待者，
///     等待者拿到 409「已取消」，随后可以重试；
///   - 写回是**比较-再写**（见 `apply_refresh`）：被判定过期的刷新结果不用于
///     后续请求，而是改用最新快照；拿不到最新快照就明确失败。
///
/// **持锁边界**：单飞表与账号锁都只在同步函数里取用并释放，网络请求与等待都在
/// 锁外 —— 与账号锁的硬约束同一精神。
pub async fn refresh(
    store: &AccountStore,
    credentials: &RaccoonCredentials,
    force: bool,
) -> Result<RaccoonCredentials, GatewayError> {
    if !force && !credentials.is_expiring() {
        return Ok(credentials.clone());
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            if credentials.origin == CredentialOrigin::AuthFile {
                "小浣熊桌面端登录态已过期且没有 refreshToken，请打开小浣熊客户端重新登录"
            } else {
                "该账号没有 refreshToken，无法刷新，请重新添加账号"
            },
        ));
    }
    let key = refresh_key(credentials);
    let guard = match inflight_table().join(&key) {
        refresh_flight::Join::Leader(guard) => guard,
        // 已加入同一轮：结果由 leader 负责落地（含回写），这里只等它的结果
        refresh_flight::Join::Waiter(waiter) => return waiter.wait().await,
    };
    // leader：真实刷新（await 期间不持任何 std 锁；guard 只负责取消清理）
    let result = match call_refresh_api(credentials).await {
        Ok(next) => apply_refresh(store, credentials, &next),
        Err(error) => Err(error),
    };
    // 结果交给已加入这一轮的等待者，随后释放表项（先释放表项、再唤醒）
    guard.finish(result.clone());
    result
}

/// 调鉴权 API 刷新一次（源实现 `callRefreshApi`）：
/// `POST {authBase}/refresh`，body `{"refresh_token": "..."}`，
/// 响应 `data.access_token` / `data.refresh_token`（后者为空时沿用旧的）。
async fn call_refresh_api(
    credentials: &RaccoonCredentials,
) -> Result<RaccoonCredentials, GatewayError> {
    let url = format!("{}/refresh", super::auth_api_base());
    let headers: Vec<(String, String)> = vec![(
        "Content-Type".to_string(),
        "application/json".to_string(),
    )];
    let body = json!({ "refresh_token": credentials.refresh_token });
    // 鉴权接口**不走账号级代理**：账号代理是给转发（流式长请求）准备的出口，
    // 而鉴权域与 LLM 域是两个不同的站点；源实现同样用裸 fetch。
    let response = send_raw(
        "POST",
        &url,
        Some(&body),
        &headers,
        None,
        Some(AUTH_REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        GatewayError::with_status(502, format!("小浣熊刷新 token 请求失败: {error}"))
    })?;
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok {
        return Err(GatewayError::with_status(
            if response.status == 401 { 401 } else { 502 },
            format!(
                "小浣熊刷新接口失败（HTTP {}）: {}",
                response.status,
                error_message(&payload).unwrap_or_else(|| "服务器未返回错误说明".to_string())
            ),
        ));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let access_token = data
        .get("access_token")
        .map(jwt::js_text)
        .map(|text| jwt::strip_bearer(&text))
        .unwrap_or_default();
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            format!(
                "小浣熊 token 刷新失败 code={} msg={}",
                payload
                    .get("code")
                    .map(jwt::js_text)
                    .unwrap_or_else(|| "null".to_string()),
                payload
                    .get("message")
                    .or_else(|| payload.get("msg"))
                    .map(jwt::js_text)
                    .unwrap_or_else(|| "服务器未返回新 token".to_string()),
            ),
        ));
    }
    let next_refresh = data
        .get("refresh_token")
        .map(jwt::js_text)
        .map(|text| jwt::strip_bearer(&text))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| credentials.refresh_token.clone());
    let mut next = credentials.clone();
    next.expires_at = jwt::jwt_expiry_ms(&access_token).or(credentials.expires_at);
    next.user_id = jwt::decode_jwt_claims(&access_token)
        .as_ref()
        .map(jwt::extract_user_id)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| credentials.user_id.clone());
    next.token = access_token;
    next.refresh_token = next_refresh;
    logging::verbose(
        "[Raccoon]",
        &format!("账号 {} 的小浣熊 token 已刷新", credentials.id),
    );
    Ok(next)
}

/// 刷新结果落地：**比较-再写**，过期结果改用最新快照（本次修复的核心）。
///
/// 三种结局：
///   1. 写回成功 → 用刷新结果（`Ok(next)`）；
///   2. 发现凭证已被换掉（用户重导入 / 换号 / 桌面端重新登录 / 另一轮刷新先落地）
///      → **不写**，改用当前最新快照（读不到就明确失败）—— 绝不把旧结果盖到
///      新凭证上，也绝不把旧结果当成这次请求的 token；
///   3. 其它回写错误（权限、磁盘满…）→ 只记日志，仍返回刷新结果（刷新本身成功，
///      回写失败不该让本次请求失败 —— 源实现同样只打一行 verbose）。
fn apply_refresh(
    store: &AccountStore,
    previous: &RaccoonCredentials,
    next: &RaccoonCredentials,
) -> Result<RaccoonCredentials, GatewayError> {
    let outcome: Result<CredentialWrite, String> = match next.origin {
        CredentialOrigin::AuthFile => {
            // 桌面端：比较文件当前内容后再写回 auth.json（**不碰账号记录**：
            // 桌面端账号的凭证按设计不落盘到 accounts.json）
            match persist_auth_file_if_current(previous, &next.token, &next.refresh_token) {
                Ok(()) => Ok(CredentialWrite::Written),
                Err(reason) if reason == STALE_REFRESH_MESSAGE => Ok(CredentialWrite::Stale),
                Err(reason) => Err(reason),
            }
        }
        CredentialOrigin::AccountStore => store.update_raccoon_account_tokens_if_current(
            &next.id,
            &previous.token,
            &previous.refresh_token,
            &next.token,
            &next.refresh_token,
            next.expires_at,
        ),
    };
    match outcome {
        Ok(CredentialWrite::Written) => Ok(next.clone()),
        Ok(CredentialWrite::Stale) => {
            logging::verbose(
                "[Raccoon]",
                &format!(
                    "账号 {} 的刷新结果已过期（凭证已被更换），改用当前凭证",
                    previous.id
                ),
            );
            // 旧结果不得用于后续请求：改用最新快照；拿不到就明确失败
            latest_credentials(store, previous).map_err(|error| {
                GatewayError::with_status(
                    error.status_code,
                    format!(
                        "{}（刷新结果已过期，且读取最新凭证失败：{}）",
                        STALE_REFRESH_MESSAGE, error.message
                    ),
                )
            })
        }
        Err(reason) => {
            logging::verbose(
                "[Raccoon]",
                &format!("账号 {} 刷新结果回写失败: {reason}", previous.id),
            );
            Ok(next.clone())
        }
    }
}

/// 重新取当前凭证（比较写回判定过期后的兜底）。
///
/// 桌面端来源**绕过 mtime 缓存重新读盘**（文件刚被改写，缓存里可能还是旧内容；
/// 顺便失效缓存，让后续读取也走新内容）；账号来源重新读账号记录。两者都会带上
/// **当前的** access/refresh token，因此不会把过期结果当成功用。
///
/// 账号来源额外要求记录**仍然存在**：`snapshot_for` 在没有账号记录时会回落到
/// 桌面端登录态，而「账号已被用户删除」绝不能变成「拿另一个账号的凭证继续」。
fn latest_credentials(
    store: &AccountStore,
    previous: &RaccoonCredentials,
) -> Result<RaccoonCredentials, GatewayError> {
    match previous.origin {
        CredentialOrigin::AuthFile => {
            if let Ok(mut guard) = auth_cache().lock() {
                *guard = None;
            }
            let snapshot = read_auth_file_raw(&desktop_auth_file()).map_err(|reason| {
                GatewayError::with_status(401, format!("没有可用的小浣熊登录态：{reason}"))
            })?;
            credentials_from_auth_file(&snapshot)
                .map_err(|reason| GatewayError::with_status(401, reason))
        }
        CredentialOrigin::AccountStore => {
            if previous.id.is_empty() || store.raccoon_account_record(&previous.id).is_none() {
                return Err(GatewayError::with_status(
                    401,
                    format!("账号 {} 已被删除，本次刷新结果作废", previous.id),
                ));
            }
            snapshot_for(store, &previous.id)
        }
    }
}

/// 刷新结果被判定过期时的统一文案（比较写回的失败原因；调用方据此改用最新快照）
pub(super) const STALE_REFRESH_MESSAGE: &str =
    "刷新结果已过期：凭证在此期间被更换（重新登录或重导入）";

/// 从上游错误体里取人类可读的文案（message / msg / error.message）
fn error_message(payload: &Value) -> Option<String> {
    for key in ["message", "msg"] {
        if let Some(text) = payload
            .get(key)
            .map(jwt::js_text)
            .filter(|text| !text.is_empty())
        {
            return Some(text);
        }
    }
    payload
        .get("error")
        .and_then(|error| error.get("message"))
        .map(jwt::js_text)
        .filter(|text| !text.is_empty())
}
