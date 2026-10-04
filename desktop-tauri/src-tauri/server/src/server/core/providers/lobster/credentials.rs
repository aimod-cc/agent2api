//! LobsterAI 凭证处理（feat/lobster-provider；PR-1 落地读取，PR-2 落刷新链路）。
//!
//! ── 本文件负责什么 ──────────────────────────────────────────
//!   1. **凭证来源**（两条，与 raccoon 的 CredentialOrigin 同构）：
//!      - **桌面端实时登录态**（`DesktopDb`）：`~/Library/Application Support/
//!        LobsterAI/lobsterai.sqlite` 的 `kv.auth_tokens` 行。App 自己会刷新并
//!        回写这份库（JWT 2h TTL），因此网关侧带 mtime+30s TTL 缓存读盘 ——
//!        既能拿到 App 刚回写的新 token，又不让每个请求都做一次文件 IO。
//!      - **账号记录**（`AccountStore`）：手动粘贴 accessToken + refreshToken
//!        落库的那份。
//!   2. **刷新与回写**：`POST /api/auth/refresh`，body `{"refreshToken"}`，
//!      响应 `{code:0, data:{accessToken, refreshToken}}`（refreshToken 缺失时
//!      沿用旧的 —— 服务端不轮换时如此）。单飞（`providers::refresh_flight`）
//!      防并发；结果按来源回写：桌面端回写 **sqlite**（事务原子
//!      `INSERT OR REPLACE`，服务端轮换 refreshToken 而不回写的话，App 下次
//!      刷新会被拒导致登出 —— 回写是安全必需，移植来源 参考实现 的原话），
//!      手动账号回写账号存储（比较-再写，凭证已被用户重导时改用最新快照）。
//!   3. **JWT 解码**（不验签，只读 exp）与状态摘要（`credentials_status`）。
//!
//! ── Send 纪律（与 raccoon/credentials.rs 同一条坑）──────────
//! 适配器的 async 方法返回的 future 必须是 `Send`：本文件所有 async fn 内部
//! 只调用**同步**函数（`std::sync` 的守卫在返回前必然析构），绝不在 await 点
//! 持有非 Send 的守卫。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，绝不 unwrap/expect/panic：取值走 Option 链与
//! `unwrap_or`，SQL/JSON 失败一律转成 GatewayError。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use rusqlite::OpenFlags;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::errors::GatewayError;
use crate::server::logging;

/// 共享单飞原语（`providers::refresh_flight`）：只保存进行中的刷新、leader 取消
/// 时 RAII 清理并唤醒等待者。
use crate::server::core::providers::refresh_flight;

/// 桌面端实时账号的固定 id（导入后的记录 id 由 jwt exp 派生，这个常量只作
/// 「桌面端来源」的语义标记与展示）
pub const DESKTOP_ACCOUNT_ID: &str = "lobster-desktop";

/// 鉴权/刷新接口超时（短请求，15 分钟的 LLM 超时不适用）
const AUTH_REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 临期主动刷新窗口：JWT 余量 < 120 秒时刷新（参考实现 `JWT_REFRESH_MARGIN_S`，
/// 避免请求中途过期）
pub const JWT_REFRESH_MARGIN_SECONDS: i64 = 120;

/// 凭证读盘缓存 TTL（参考实现 `CRED_CACHE_TTL_S`：跟住 App 端定期回写）
const CRED_CACHE_TTL_SECONDS: i64 = 30;

/// sqlite busy 等待（读与写都带上；libsqlite 的 busy handler）
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(3);

/// 刷新失败的终态文案（参考实现 同句）：用户可修复，说清该怎么做
const CREDENTIALS_INVALID_MESSAGE: &str =
    "LobsterAI 凭证失效，请打开 LobsterAI App 重新登录";

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// SQLite 文件路径（macOS 上）
pub fn desktop_db_file() -> Option<PathBuf> {
    let home = home_dir()?;
    Some(home.join("Library/Application Support/LobsterAI/lobsterai.sqlite"))
}

// ─── 读盘（mtime + TTL 缓存）────────────────────────────────

/// 进程级凭证缓存：`(db mtime, 加载时刻(秒), accessToken, refreshToken)`。
///
/// 新鲜判据与 参考实现 `_CredCache.is_fresh` 一致：TTL 未过 **且** db 的 mtime
/// 没变 —— App 刷新回写后 mtime 变化，缓存即刻失效。
fn cred_cache() -> &'static Mutex<Option<(i64, i64, String, String)>> {
    static CACHE: OnceLock<Mutex<Option<(i64, i64, String, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn db_mtime_seconds(path: &std::path::Path) -> i64 {
    std::fs::symlink_metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(-1)
}

/// 只读打开 sqlite 取 kv.auth_tokens 行（同步；守卫在返回前析构）
fn read_auth_tokens_raw(path: &std::path::Path) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let _ = conn.busy_timeout(SQLITE_BUSY_TIMEOUT);
    let value: String = conn
        .query_row(
            "SELECT value FROM kv WHERE key='auth_tokens'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str(&value).ok()
}

/// 读桌面端登录态（带 mtime+TTL 缓存）。`force` 绕过缓存（401 重试用 ——
/// App 可能刚刷新回写）。
fn load_desktop_tokens(force: bool) -> Option<(String, String)> {
    let path = desktop_db_file()?;
    let mtime = db_mtime_seconds(&path);
    let now = now_seconds();
    // 缓存命中：只做一次「取锁 → 比较 → 克隆」
    if !force {
        if let Ok(guard) = cred_cache().lock() {
            if let Some((cached_mtime, loaded_at, access, refresh)) = guard.as_ref() {
                if *cached_mtime == mtime && now - *loaded_at < CRED_CACHE_TTL_SECONDS {
                    return Some((access.clone(), refresh.clone()));
                }
            }
        }
    }
    let tokens = read_auth_tokens_raw(&path)?;
    let pair = extract_credentials(&tokens)?;
    if let Ok(mut guard) = cred_cache().lock() {
        *guard = Some((mtime, now, pair.0.clone(), pair.1.clone()));
    }
    Some(pair)
}

/// 解析出 (accessToken, refreshToken)；都非空才算成功
pub fn extract_credentials(tokens: &Value) -> Option<(String, String)> {
    let access = tokens
        .get("accessToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let refresh = tokens
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((access, refresh))
}

/// 只读打开 sqlite 取 kv.auth_tokens 行（PR-1 兼容入口，无缓存直读）
pub fn read_auth_tokens_from_db() -> Option<Value> {
    let path = desktop_db_file()?;
    read_auth_tokens_raw(&path)
}

/// 桌面端登录态摘要（401 文案用）：读不到时 None
pub fn desktop_tokens() -> Option<(String, String)> {
    load_desktop_tokens(false)
}

// ─── 回写 sqlite（刷新链路的安全必需）──────────────────────

/// 把刷新后的 token 对回写 App 的 sqlite（kv.auth_tokens 行，事务原子）。
///
/// 失败（库被锁/只读介质/App 已重登录换了内容）只返回 Err 由调用方决定：
/// 内存里的新 token 仍可用于本次请求，App 下次刷新会再落盘。
///
/// `previous_refresh` 是**刷新前**的 refreshToken 快照：比较写回比的是它
/// （文件当前值 == 本次刷新所用的那枚才允许写）。比刷新后的新值是错的——
/// 服务端轮换时（正是最需要回写的场景）新旧必然不等，会把唯一正确的回写拒掉。
/// 写回采用「读出当前整段 JSON → 只替换两个 token 键 → 写回整段」，
/// **保留 App 可能存进的其它登录态字段**（与 raccoon 的回写同一取向）。
fn write_auth_tokens_to_db(
    previous_refresh: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<(), String> {
    let path = desktop_db_file().ok_or("无法定位 LobsterAI 数据目录")?;
    write_auth_tokens_to_db_at(&path, previous_refresh, access_token, refresh_token)
}

/// 路径可注入的写回主体(单测喂临时 sqlite 用);语义见上面的包装。
fn write_auth_tokens_to_db_at(
    path: &std::path::Path,
    previous_refresh: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<(), String> {
    let conn = rusqlite::Connection::open(path)
        .map_err(|error| format!("打开 LobsterAI sqlite 失败: {error}"))?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT)
        .map_err(|error| format!("设置 sqlite busy 超时失败: {error}"))?;
    let updated_at = logging::now_ms();
    // BEGIN IMMEDIATE 拿写锁后再读再写，杜绝「读-改-写」竞态窗口
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| format!("sqlite 事务开启失败: {error}"))?;
    let current_row = conn
        .query_row(
            "SELECT value FROM kv WHERE key='auth_tokens'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let mut document = match current_row {
        // 行已存在：先校验它仍是刷新前那份（App 重登录换号时拒绝覆盖 ——
        // 一次更早发起、更晚返回的刷新把旧凭证盖回去会把用户登出）
        Some(Value::Object(fields)) => {
            let current_refresh = fields
                .get("refreshToken")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !previous_refresh.is_empty() && current_refresh != previous_refresh {
                let _ = conn.execute_batch("ROLLBACK");
                return Err("App 的登录态已变化（可能重新登录），拒绝覆盖".to_string());
            }
            fields
        }
        // 行不存在/不是对象：用最小文档初始化（等价于首次落登录态）
        _ => serde_json::Map::new(),
    };
    // 只替换两个 token 键,其余字段原样保留
    document.insert(
        "accessToken".to_string(),
        Value::String(access_token.to_string()),
    );
    document.insert(
        "refreshToken".to_string(),
        Value::String(refresh_token.to_string()),
    );
    let payload = serde_json::to_string(&Value::Object(document))
        .map_err(|error| format!("序列化凭证失败: {error}"))?;
    let written = conn.execute(
        "INSERT OR REPLACE INTO kv (key, value, updated_at) VALUES (?1, ?2, ?3)",
        rusqlite::params!["auth_tokens", payload, updated_at],
    );
    match written {
        Ok(_) => {}
        Err(reason) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(format!("回写凭证失败: {reason}"));
        }
    }
    conn.execute_batch("COMMIT")
        .map_err(|error| format!("sqlite 提交失败: {error}"))?;
    // 写盘后 mtime 变了，缓存必须失效（mtime 精度问题下同秒写入读旧值的窗口）
    if let Ok(mut guard) = cred_cache().lock() {
        *guard = None;
    }
    Ok(())
}

// ─── 凭证快照 ───────────────────────────────────────────────

/// 一份解析好的 LobsterAI 凭证
#[derive(Clone, Debug, Default)]
pub struct LobsterCredentials {
    /// 账号 id（`lobster-…` 或 `lobster-desktop` 语义标记）
    pub id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// JWT 的 exp（Unix 秒）；解不出为 None
    pub expires_at: Option<i64>,
    /// 凭证来源：决定刷新结果回写到哪里
    pub origin: CredentialOrigin,
}

/// 凭证来源（决定刷新后往哪回写）
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CredentialOrigin {
    /// App 的 sqlite（桌面端实时登录态）→ 回写 sqlite
    #[default]
    DesktopDb,
    /// 账号记录（手动粘贴）→ 回写账号存储
    AccountStore,
}

impl LobsterCredentials {
    /// 现在是否需要刷新（余量 < 120 秒；解不出 exp 时不刷 ——
    /// 没有依据就说「临期」会让每个请求都打一次刷新接口）
    pub fn is_expiring(&self) -> bool {
        match self.expires_at {
            Some(expires_at) => expires_at - now_seconds() < JWT_REFRESH_MARGIN_SECONDS,
            None => false,
        }
    }

    /// 能否刷新：有 refreshToken 才行
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.is_empty()
    }
}

/// 桌面端实时登录态 → 凭证（读不到给 401，文案说明该怎么做）
fn desktop_credentials() -> Result<LobsterCredentials, GatewayError> {
    let (access_token, refresh_token) = load_desktop_tokens(false).ok_or_else(|| {
        GatewayError::with_status(
            401,
            "LobsterAI 凭证缺失：未找到本机登录态，请先在 LobsterAI App 登录",
        )
    })?;
    let expires_at = jwt_exp_seconds(&access_token);
    Ok(LobsterCredentials {
        id: DESKTOP_ACCOUNT_ID.to_string(),
        access_token,
        refresh_token,
        expires_at,
        origin: CredentialOrigin::DesktopDb,
    })
}

/// 取指定账号的凭证快照（**同步**：不持锁跨 await）。
///
/// `account_id` 为空 → 用 LobsterAI 组内的当前账号；没有账号记录时回落到
/// 桌面端实时登录态（那台机器上的「默认登录态」就是 App 登录的那个）。
/// `desktop=true` 的记录实时读 sqlite（凭证按设计活在 App 的库里，记录里的
/// 副本只用于导入那一刻的展示）。
pub fn snapshot_for(
    store: &AccountStore,
    account_id: &str,
) -> Result<LobsterCredentials, GatewayError> {
    let record = store.lobster_account_record(account_id);
    let Some(record) = record else {
        return desktop_credentials();
    };
    let is_desktop = record.get("desktop").and_then(Value::as_bool).unwrap_or(false)
        || record.get("id").and_then(Value::as_str) == Some(DESKTOP_ACCOUNT_ID);
    if is_desktop {
        return desktop_credentials();
    }
    let access_token = record
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            format!(
                "账号 {} 没有可用凭证，请重新添加",
                record.get("id").and_then(Value::as_str).unwrap_or("(未知)")
            ),
        ));
    }
    let refresh_token = record
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_at = jwt_exp_seconds(&access_token);
    Ok(LobsterCredentials {
        id: record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        access_token,
        refresh_token,
        expires_at,
        origin: CredentialOrigin::AccountStore,
    })
}

// ─── 刷新（单飞 + 按来源回写）──────────────────────────────

/// 进程级单飞表：**只保存进行中的刷新**（`refresh_flight` 原语）。
/// key = `{账号 id}:{来源}:{refreshToken 指纹}[:{db mtime}]`（构成理由与
/// raccoon 的 `refresh_key` 相同：不同账号/来源/轮换后的凭证绝不互相复用）。
fn inflight_table() -> &'static refresh_flight::Table<LobsterCredentials> {
    static TABLE: OnceLock<refresh_flight::Table<LobsterCredentials>> = OnceLock::new();
    TABLE.get_or_init(refresh_flight::Table::new)
}

fn refresh_key(credentials: &LobsterCredentials) -> String {
    let source = match credentials.origin {
        CredentialOrigin::DesktopDb => "db",
        CredentialOrigin::AccountStore => "store",
    };
    let mut key = format!(
        "{}:{source}:{}",
        credentials.id,
        refresh_flight::fingerprint(&credentials.refresh_token)
    );
    if credentials.origin == CredentialOrigin::DesktopDb {
        if let Some(path) = desktop_db_file() {
            key.push(':');
            key.push_str(&db_mtime_seconds(&path).to_string());
        }
    }
    key
}

/// 刷新凭证（单飞；结果按来源回写）。
///
/// `force = false`：只在临期（余量 < 120 秒）时刷新（`ensure_access_token`）；
/// `force = true`：无条件刷新（401 之后的 `refresh_access_token`）。
pub async fn refresh(
    store: &AccountStore,
    credentials: &LobsterCredentials,
    force: bool,
) -> Result<LobsterCredentials, GatewayError> {
    if !force && !credentials.is_expiring() {
        return Ok(credentials.clone());
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(401, CREDENTIALS_INVALID_MESSAGE));
    }
    let key = refresh_key(credentials);
    let guard = match inflight_table().join(&key) {
        refresh_flight::Join::Leader(guard) => guard,
        refresh_flight::Join::Waiter(waiter) => return waiter.wait().await,
    };
    let result = match call_refresh_api(credentials).await {
        Ok(next) => apply_refresh(store, credentials, &next),
        Err(error) => Err(error),
    };
    guard.finish(result.clone());
    result
}

/// 调 `POST /api/auth/refresh` 换新 token 对（响应 `{code, data:{accessToken,
/// refreshToken}}`；refreshToken 缺失时沿用旧的）。
async fn call_refresh_api(
    credentials: &LobsterCredentials,
) -> Result<LobsterCredentials, GatewayError> {
    let url = format!("{}/api/auth/refresh", super::DEFAULT_LLM_BASE_URL);
    let headers = vec![(
        "Content-Type".to_string(),
        "application/json".to_string(),
    )];
    let body = json!({ "refreshToken": credentials.refresh_token });
    // 鉴权接口不走账号级代理（与 raccoon 同一取舍：代理是给流式长请求的出口）
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
        GatewayError::with_status(502, format!("LobsterAI 刷新请求失败: {error}"))
    })?;
    let payload = response.payload.unwrap_or(Value::Null);
    if !response.ok {
        // 401/403 = refreshToken 被服务端拒绝（过期/轮换/登出），是终态
        let terminal = response.status == 401 || response.status == 403;
        return Err(GatewayError::with_status(
            if terminal { 401 } else { 502 },
            if terminal {
                CREDENTIALS_INVALID_MESSAGE.to_string()
            } else {
                format!(
                    "LobsterAI 刷新接口失败（HTTP {}）: {}",
                    response.status,
                    payload
                        .get("message")
                        .or_else(|| payload.get("msg"))
                        .and_then(Value::as_str)
                        .unwrap_or("服务器未返回错误说明")
                )
            },
        ));
    }
    let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or("")
        .to_string();
    if code != 0 || access_token.is_empty() {
        return Err(GatewayError::with_status(401, CREDENTIALS_INVALID_MESSAGE));
    }
    let refresh_token = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| credentials.refresh_token.clone());
    let mut next = credentials.clone();
    next.access_token = access_token;
    next.refresh_token = refresh_token;
    next.expires_at = jwt_exp_seconds(&next.access_token).or(credentials.expires_at);
    logging::verbose(
        "[Lobster]",
        &format!("账号 {} 的 LobsterAI token 已刷新", credentials.id),
    );
    Ok(next)
}

/// 刷新结果落地：按来源回写。回写失败不阻断本次请求（内存 token 仍有效），
/// 但**桌面端 sqlite 的回写失败升为可见日志**——模块头把回写定义为「安全必需」，
/// 它的失败若只留 verbose，用户侧只看到「刷新成功」，随后 App 被登出时无从归因。
/// （账号存储侧的 `Ok(false)` 是并发保护命中，属正常路径，保持 verbose 并注明。）
fn apply_refresh(
    store: &AccountStore,
    previous: &LobsterCredentials,
    next: &LobsterCredentials,
) -> Result<LobsterCredentials, GatewayError> {
    let outcome: Result<(), String> = match next.origin {
        CredentialOrigin::DesktopDb => write_auth_tokens_to_db(
            &previous.refresh_token,
            &next.access_token,
            &next.refresh_token,
        ),
        CredentialOrigin::AccountStore => store
            .update_lobster_account_tokens_if_current(
                &next.id,
                &previous.access_token,
                &previous.refresh_token,
                &next.access_token,
                &next.refresh_token,
                next.expires_at,
            )
            .map(|written| {
                if !written {
                    // 凭证已被用户重导/换号：改用记录里的最新凭证，不用旧结果
                    logging::verbose(
                        "[Lobster]",
                        &format!("账号 {} 的刷新结果已过期（并发保护命中，凭证已被更换），改用当前凭证", previous.id),
                    );
                }
            })
            .map_err(|error| error.message),
    };
    match outcome {
        Ok(()) => Ok(next.clone()),
        Err(reason) => {
            let visible = next.origin == CredentialOrigin::DesktopDb;
            let line = format!(
                "账号 {} 刷新成功但回写失败: {reason}（桌面端登录态未更新，App 侧可能仍持有旧凭证）",
                previous.id
            );
            if visible {
                logging::log("[Lobster]", &line);
            } else {
                logging::verbose("[Lobster]", &line);
            }
            Ok(next.clone())
        }
    }
}

// ─── JWT 与状态摘要 ─────────────────────────────────────────

/// JWT 解码（不验签，只读 exp）。失败 → None。
pub fn jwt_exp_seconds(token: &str) -> Option<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload_b64 = parts[1];
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v.get("exp").and_then(|e| e.as_i64()))
}

/// 账号状态摘要（高频轮询路径用；不抛异常）
pub fn credentials_status() -> Value {
    let now = now_seconds();
    let Some((access, _refresh)) = load_desktop_tokens(false) else {
        return json!({
            "available": false,
            "message": "LobsterAI 登录态缺失：请安装并登录 LobsterAI App",
        });
    };
    let jwt_exp = jwt_exp_seconds(&access);
    let (jwt_expired, jwt_seconds_left) = match jwt_exp {
        Some(exp) => (Some(exp <= now), Some(exp - now)),
        None => (None, None),
    };
    json!({
        "available": true,
        "jwt_expires_at": jwt_exp,
        "jwt_seconds_left": jwt_seconds_left,
        "jwt_expired": jwt_expired,
        "message": "",
    })
}

/// 桌面端文件存在性 + 凭证字段摘要（用于 import 流；失败抛真实错误给 UI）
pub fn desktop_summary() -> Result<Value, String> {
    let path = desktop_db_file()
        .ok_or_else(|| "无法定位 LobsterAI 数据目录".to_string())?;
    if !path.exists() {
        return Err(format!("LobsterAI sqlite 不存在: {}", path.display()));
    }
    let tokens = read_auth_tokens_raw(&path)
        .ok_or_else(|| "LobsterAI 凭证缺失：未找到本机登录态".to_string())?;
    let (access, refresh) = extract_credentials(&tokens)
        .ok_or_else(|| "LobsterAI 凭证字段缺失".to_string())?;
    let token_tail = |s: &str| {
        // 按 char 切:token 理论上是 ASCII,但按字节切在多字节字符上会产乱码
        let chars: Vec<char> = s.chars().collect();
        let start = chars.len().saturating_sub(4);
        chars[start..].iter().collect::<String>()
    };
    Ok(json!({
        "path": path.display().to_string(),
        "accessToken_tail": token_tail(&access),
        "refreshToken_tail": token_tail(&refresh),
        "jwt_exp": jwt_exp_seconds(&access),
    }))
}

/// 取得当前凭证（手动添加账号时复用桌面端 sqlite 内容作模板）
pub fn snapshot_for_desktop() -> Result<(String, String), GatewayError> {
    let pair = load_desktop_tokens(true).ok_or_else(|| {
        GatewayError::with_status(
            401,
            "LobsterAI 凭证缺失：未找到本机登录态，请先在 LobsterAI App 登录",
        )
    })?;
    Ok(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用一个可本地构造的 JWT（header.payload.signature，payload 只放 exp）验证
    /// 解码与临期判定；不验签，所以内容可以随便造。
    fn jwt_with_exp(exp: i64) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let payload = serde_json::json!({ "exp": exp }).to_string();
        format!(
            "h.{}.s",
            URL_SAFE_NO_PAD.encode(payload.as_bytes())
        )
    }

    #[test]
    fn jwt_exp_roundtrip_and_bad_inputs() {
        assert_eq!(jwt_exp_seconds(&jwt_with_exp(1_900_000_000)), Some(1_900_000_000));
        assert_eq!(jwt_exp_seconds("not-a-jwt"), None);
        assert_eq!(jwt_exp_seconds(""), None);
        assert_eq!(jwt_exp_seconds("a.b"), None); // payload 不是 base64 JSON
    }

    #[test]
    fn expiring_follows_the_120s_margin() {
        let now = now_seconds();
        let far = LobsterCredentials {
            id: "x".into(),
            access_token: jwt_with_exp(now + 3600),
            refresh_token: "r".into(),
            expires_at: Some(now + 3600),
            origin: CredentialOrigin::DesktopDb,
        };
        assert!(!far.is_expiring());
        let near = LobsterCredentials { expires_at: Some(now + 60), ..far.clone() };
        assert!(near.is_expiring());
        // 解不出 exp 的凭证不算临期（无从判断就不刷）
        let unknown = LobsterCredentials { expires_at: None, ..far };
        assert!(!unknown.is_expiring());
        // 没有 refreshToken 的凭证不能刷
        assert!(!LobsterCredentials {
            refresh_token: String::new(),
            ..near
        }
        .can_refresh());
    }

    #[test]
    fn extract_requires_both_tokens() {
        assert!(extract_credentials(&json!({
            "accessToken": "a", "refreshToken": "r"
        }))
        .is_some());
        assert!(extract_credentials(&json!({ "accessToken": "a" })).is_none());
        assert!(extract_credentials(&json!({ "accessToken": "", "refreshToken": "r" })).is_none());
    }

    /// P1 回归(独立审查发现的两处回写缺陷:守卫比错对象 / 整行替换丢字段)。
    /// 三个场景全部对着修复后的语义。
    #[test]
    fn writeback_rotates_keeps_fields_and_refuses_stale() {
        let dir = std::env::temp_dir()
            .join(format!("lobster-wb-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).ok();
        let db = dir.join("test.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).ok().unwrap();
            let seed = r#"{"accessToken":"old-at","refreshToken":"old-rt","userId":"u-9"}"#;
            conn.execute_batch(&format!(
                "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
                 INSERT INTO kv (key, value) VALUES ('auth_tokens', '{seed}')",
            ))
            .ok()
            .unwrap();
        }
        // ① 服务端轮换 refreshToken:守卫比「刷新前」的旧值 → 允许写入
        let wrote = write_auth_tokens_to_db_at(&db, "old-rt", "new-at", "new-rt");
        assert!(wrote.is_ok(), "轮换场景必须允许回写: {:?}", wrote.err());
        let value: String = rusqlite::Connection::open(&db)
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM kv WHERE key='auth_tokens'",
                    [],
                    |r| r.get(0),
                )
                .ok()
            })
            .unwrap_or_default();
        let doc: serde_json::Value = serde_json::from_str(&value).ok().unwrap_or_default();
        assert_eq!(doc.get("accessToken").and_then(|v| v.as_str()), Some("new-at"));
        assert_eq!(doc.get("refreshToken").and_then(|v| v.as_str()), Some("new-rt"));
        // ② 只替换两个 token 键,App 的其它登录态字段保留
        assert_eq!(doc.get("userId").and_then(|v| v.as_str()), Some("u-9"));
        // ③ App 已重登录换了 refreshToken:previous 对不上 → 拒绝且不改文件
        assert!(write_auth_tokens_to_db_at(&db, "old-rt", "x-at", "x-rt").is_err());
        let after: String = rusqlite::Connection::open(&db)
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM kv WHERE key='auth_tokens'",
                    [],
                    |r| r.get(0),
                )
                .ok()
            })
            .unwrap_or_default();
        assert!(after.contains("new-rt"), "拒绝后文件内容不得被改动");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
