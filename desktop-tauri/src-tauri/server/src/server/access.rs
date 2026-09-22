//! 管理面板的访问控制：管理员注册、双令牌会话、失败锁定与 /v1 fail-closed。
//!
//! ── 令牌模型（照 OmniProxy 的短效 + 长效语义）────────────────
//!   · access token：**短效**（2 小时），HttpOnly cookie（path=/），
//!     只存进程内存 —— 泄露窗口小，过期即废；
//!   · refresh token：**长效**（30 天），HttpOnly cookie（path=/api/panel，
//!     缩小暴露面），库里只存 **sha256 哈希**；携带它到
//!     `/api/panel/refresh` 轮换出新的 access + 新 refresh（同一会话链，
//!     旧条目标记 rotated）。access 在进程重启后丢失没关系 —— 浏览器凭
//!     refresh 静默换新，用户无感。
//!   · 撤销：登出按会话链整链撤销；**已轮换的旧 refresh 再次出现即视为
//!     泄露**（重放检测），整条会话链作废，需要重新登录。
//!
//! ── 管理员的两种来路 ─────────────────────────────────────────
//!   · 面板首次注册（推荐）：全新部署时登录页出现「创建管理员账号」，
//!     写入 `kv` 表的 `panelAdmin` 键 —— 不需要预先在部署配置里放密码；
//!   · 环境变量预置（`AGENT2API_ADMIN_USER` + 密码，跳过注册流程，
//!     无人值守 / IaC 部署用）：密码填 `AGENT2API_ADMIN_PASSWORD`（明文，
//!     内存里现场转成 bcrypt 哈希）或 `AGENT2API_ADMIN_PASSWORD_HASH`
//!     （已是 bcrypt 哈希，优先于明文）。启动时把哈希同步进库 ——
//!     **任何落盘形态都只有哈希**，明文只出现在部署配置里。
//!
//! ── 何时启用面板认证 ────────────────────────────────────────
//! 注册过（或环境变量预置了）管理员即启用：`/api/*` 需要会话或 API Key。
//! 桌面壳不注册也不设置 —— 访问控制维持原有免鉴权语义，桌面行为零变化。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;

use crate::server::db::Db;

/// 会话 cookie 名（access，面板登录后由浏览器自动携带；登出清 cookie 用同一名字）
pub const ACCESS_COOKIE: &str = "agent2api-panel";
/// 刷新 cookie 名（path 限定在 /api/panel，缩小暴露面 —— 照 OmniProxy 的做法）
pub const REFRESH_COOKIE: &str = "agent2api-panel-rt";

/// access token 有效期（短效）
const ACCESS_TTL: Duration = Duration::from_secs(2 * 3600);
/// refresh token 有效期（长效）
const REFRESH_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// 登录失败锁定：同一来源连续失败 5 次，锁 5 分钟（按 IP 不是全局 ——
/// 全局锁会把「攻击者锁死管理员」变成一种攻击，Tinyauth 那个 CVE 的教训）
const LOCKOUT_THRESHOLD: u32 = 5;
const LOCKOUT_DURATION: Duration = Duration::from_secs(300);

const KV_ADMIN_KEY: &str = "panelAdmin";
const KV_TOKENS_KEY: &str = "panelTokens";

// ── 数据库句柄（启动时注入；kv 读写都走它）────────────────────

static STORE_DB: OnceLock<Option<Db>> = OnceLock::new();

/// 把库句柄交给本模块（`ServerState::bootstrap` 完成后调用一次）。
/// `None` = 库不可用：注册与刷新令牌的落盘能力随之降级
/// （环境变量预置的管理员仍可登录，但会话只存在于内存里）。
pub fn attach_db(db: Option<Db>) {
    STORE_DB.set(db).ok();
}

fn db() -> Option<&'static Db> {
    STORE_DB.get().and_then(|slot| slot.as_ref())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── 管理员（环境变量优先，其次库里的注册记录）────────────────

static ENV_ADMIN: OnceLock<Option<(String, String)>> = OnceLock::new();

fn env_admin() -> Option<&'static (String, String)> {
    // OnceLock<Option<(String, String)>>：首次调用时读入，此后返回静态引用。
    // 密码有两个变量：HASH（已是 bcrypt 哈希）优先；PASSWORD（明文）则
    // 现场 bcrypt::hash 一次 —— 内存与落库从此都只有哈希形态。
    ENV_ADMIN.get_or_init(|| {
        let user = std::env::var("AGENT2API_ADMIN_USER")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let from_hash = std::env::var("AGENT2API_ADMIN_PASSWORD_HASH")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            // 兼容 `htpasswd -nBC` 的整行输出（"user:$2y$…"）：冒号后面是
            // bcrypt 哈希就只取哈希 —— 部署者把生成命令的输出整行粘进来
            // 就能用，不必手工剥前缀
            .map(|value| match value.split_once(':') {
                Some((_, hash)) if hash.starts_with("$2") => hash.to_string(),
                _ => value,
            });
        let hash = match from_hash {
            Some(hash) => Some(hash),
            None => std::env::var("AGENT2API_ADMIN_PASSWORD")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .and_then(|plain| match bcrypt::hash(plain, 10) {
                    Ok(hash) => Some(hash),
                    // bcrypt 拒绝的密码（如超过 72 字节）：报出来，别静默
                    // 失效让部署者以为账号已预置成功
                    Err(error) => {
                        eprintln!("❌ AGENT2API_ADMIN_PASSWORD 无法转成哈希: {error}");
                        None
                    }
                }),
        };
        match (user, hash) {
            (Some(user), Some(hash)) => Some((user, hash)),
            _ => None,
        }
    })
    .as_ref()
}

/// 库里的注册记录（`kv` 的 `panelAdmin`：{username, hash}）。
fn store_admin() -> Option<(String, String)> {
    let db = db()?;
    db.with(|conn| {
        conn.query_row(
            "SELECT value FROM kv WHERE key = ?1",
            rusqlite::params![KV_ADMIN_KEY],
            |row| row.get::<_, String>(0),
        )
        .ok()
    })
    .flatten()
    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .and_then(|object| {
        let username = object.get("username")?.as_str()?.to_string();
        let hash = object.get("hash")?.as_str()?.to_string();
        Some((username, hash))
    })
}

/// 面板认证是否启用（注册过或预置过管理员）。
pub fn panel_auth_enabled() -> bool {
    env_admin().is_some() || store_admin().is_some()
}

/// 管理员是否已经存在（登录页据此显示「注册」还是「登录」）。
pub fn admin_registered() -> bool {
    panel_auth_enabled()
}

/// 首次注册管理员（只在无人注册时成功 —— 幂等安全：
/// 竞争下只有第一个写入者生效，后到的会看到「已注册」）。
///
/// 返回 Ok(false) = 已有管理员（环境变量或库），拒绝覆盖。
pub fn setup_admin(username: &str, password_hash: &str) -> Result<bool, String> {
    if panel_auth_enabled() {
        return Ok(false);
    }
    let Some(db) = db() else {
        return Err("数据库不可用，无法保存管理员账号".to_string());
    };
    let payload = serde_json::json!({ "username": username, "hash": password_hash });
    // ON CONFLICT DO **NOTHING**（不是 DO UPDATE）：面板注册是**竞争面** ——
    // 两个请求同时闯过上面的 panel_auth_enabled() 检查时，DO UPDATE 会让
    // 后写者覆盖先写者（先注册的管理员被凭空换人）。DO NOTHING 让竞态下
    // 只有一个写入者生效，后到者拿到 changed=0 → Ok(false) → 409。
    // （env 预置管理员的覆盖式落库走 `sync_env_admin_to_store`，与本函数无关。）
    let changed = db
        .with_mut(|conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO NOTHING",
                rusqlite::params![KV_ADMIN_KEY, payload.to_string()],
            )
        })
        .and_then(|result| result.ok())
        .ok_or_else(|| "写入管理员账号失败".to_string())?;
    Ok(changed > 0)
}

/// 环境变量预置的管理员同步进库（`kv` 的 `panelAdmin`，与面板注册同一处）：
/// env_admin() 里明文已转成 bcrypt 哈希，落库的自然也是哈希 —— 数据库的
/// 任何角落都不会出现明文密码。env 是显式的部署意图，覆盖式写入（改了
/// env 重启即生效）；没配 env 则什么都不做，走面板注册路径。
/// 启动时（`attach_db` 之后）调用一次。
pub fn sync_env_admin_to_store() {
    let Some((user, hash)) = env_admin() else {
        return;
    };
    let Some(db) = db() else {
        return;
    };
    let payload = serde_json::json!({ "username": user, "hash": hash });
    let _ = db.with_mut(|conn| {
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![KV_ADMIN_KEY, payload.to_string()],
        )
    });
}

/// 校验账号密码。bcrypt 哈希兼容 `htpasswd -nBC` 的输出格式。
fn verify_credentials(username: &str, password: &str, expected: &(String, String)) -> bool {    let user_ok = constant_time_eq(username.as_bytes(), expected.0.as_bytes());
    let pass_ok = bcrypt::verify(password, &expected.1).unwrap_or(false);
    // 两个都算完再返回，避免「用户名对不对」的时序差异
    user_ok && pass_ok
}

/// 按面板提交的凭证找管理员并校验（环境变量优先，其次库里的注册记录）。
pub fn verify_login(username: &str, password: &str) -> bool {
    if let Some(expected) = env_admin() {
        return verify_credentials(username, password, expected);
    }
    store_admin().is_some_and(|expected| verify_credentials(username, password, &expected))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── access token（短效，只存内存）─────────────────────────────

/// access token → (所属会话链, 过期时刻)。关联会话链是为了
/// 「重放检测 / 登出」能把同一登录签发的所有 access 一并作废。
type AccessTable = HashMap<String, (String, Instant)>;

static ACCESS_TOKENS: OnceLock<Mutex<AccessTable>> = OnceLock::new();

fn access_tokens() -> &'static Mutex<AccessTable> {
    ACCESS_TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 生成随机 hex 字符串（会话令牌 / altcha 的 salt 等都用它）。
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("系统随机源不可用");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 读一个 kv 固定键（altcha 密钥等零散状态用；不存在返回 None）。
pub fn kv_get(key: &str) -> Option<String> {
    let db = db()?;
    db.with(|conn| {
        conn.query_row(
            "SELECT value FROM kv WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    })
    .flatten()
}

/// 写一个 kv 固定键（upsert）。调用方只传「RESERVED_KV_KEYS 里登记过的
/// 固定名」—— 配置写入按那个集合排除，这里写错键会被一次配置改动删掉。
pub fn kv_put(key: &str, value: &str) {
    let Some(db) = db() else { return };
    let _ = db.with_mut(|conn| {
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )
    });
}

// ── refresh token（长效，库里只存 sha256）─────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct RefreshRecord {
    /// sha256(token)
    hash: String,
    /// 会话链 id：同一登录的轮换链共享，撤销按链整条作废
    session: String,
    /// 过期时刻（毫秒）
    expires_at: i64,
    /// 已被轮换（旧令牌不能再换；再次出现 = 泄露，整链作废）
    rotated: bool,
}

static REFRESH_TOKENS: OnceLock<Mutex<Vec<RefreshRecord>>> = OnceLock::new();

fn refresh_tokens() -> &'static Mutex<Vec<RefreshRecord>> {
    REFRESH_TOKENS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 启动时把库里的刷新令牌载入内存（`bootstrap` 完成后调用一次）。
pub fn load_refresh_tokens() {
    let Some(loaded) = db().and_then(|db| {
        db.with(|conn| {
            conn.query_row(
                "SELECT value FROM kv WHERE key = ?1",
                rusqlite::params![KV_TOKENS_KEY],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .flatten()
    }) else {
        return;
    };
    let Ok(records) = serde_json::from_str::<Vec<RefreshRecord>>(&loaded) else {
        return;
    };
    match refresh_tokens().lock() {
        Ok(mut table) => *table = records,
        Err(poisoned) => *poisoned.into_inner() = records,
    }
}

fn persist_refresh_tokens(table: &mut Vec<RefreshRecord>) {
    // 过期条目顺手清掉（条目量级：每设备一条活链 + 若干轮换遗留）
    let now = now_ms();
    table.retain(|record| record.expires_at > now);
    if let Some(db) = db() {
        let payload = serde_json::to_string(&*table).unwrap_or_else(|_| "[]".to_string());
        let _ = db.with_mut(|conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![KV_TOKENS_KEY, payload],
            )
        });
    }
}

// ── 会话签发 / 校验 / 轮换 / 撤销 ─────────────────────────────

/// 一次登录的产出：两段 Set-Cookie 值交给 handler 下发。
pub struct IssuedSession {
    pub access_cookie: String,
    pub refresh_cookie: String,
}

impl IssuedSession {
    fn issue(session: String) -> Self {
        let access_token = random_hex(32);
        let refresh_token = random_hex(48);
        match access_tokens().lock() {
            Ok(mut table) => {
                table.insert(access_token.clone(), (session.clone(), Instant::now() + ACCESS_TTL));
            }
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .insert(access_token.clone(), (session.clone(), Instant::now() + ACCESS_TTL));
            }
        }
        match refresh_tokens().lock() {
            Ok(mut table) => {
                table.push(RefreshRecord {
                    hash: sha256_hex(&refresh_token),
                    session,
                    expires_at: now_ms() + REFRESH_TTL.as_millis() as i64,
                    rotated: false,
                });
                persist_refresh_tokens(&mut table);
            }
            Err(poisoned) => {
                let mut table = poisoned.into_inner();
                table.push(RefreshRecord {
                    hash: sha256_hex(&refresh_token),
                    session,
                    expires_at: now_ms() + REFRESH_TTL.as_millis() as i64,
                    rotated: false,
                });
                persist_refresh_tokens(&mut table);
            }
        }
        Self {
            access_cookie: format!(
                "{ACCESS_COOKIE}={access_token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
                ACCESS_TTL.as_secs()
            ),
            refresh_cookie: format!(
                "{REFRESH_COOKIE}={refresh_token}; Path=/api/panel; HttpOnly; SameSite=Lax; Max-Age={}",
                REFRESH_TTL.as_secs()
            ),
        }
    }

    /// 登录：开一条新会话链。
    pub fn new_session() -> Self {
        Self::issue(random_hex(16))
    }
}

/// 请求是否携带有效 access token。
pub fn session_valid(headers: &HeaderMap) -> bool {
    let Some(token) = cookie_value(headers, ACCESS_COOKIE) else {
        return false;
    };
    let now = Instant::now();
    let mut table = match access_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.retain(|_, (_, expiry)| *expiry > now);
    table.contains_key(&token)
}

/// 用 refresh token 轮换出新一组令牌（同一会话链）。
///
/// 返回 `None` = 刷新令牌无效 / 过期 / 已被轮换。**重放检测**：拿一条
/// 已轮换的旧令牌来换，说明它泄露了（活链上新令牌在浏览器手里）——
/// 整条会话链作废，逼着重新登录。
pub fn rotate_session(refresh_cookie: Option<&str>) -> Option<IssuedSession> {
    let presented = cookie_value_from(refresh_cookie, REFRESH_COOKIE)?;
    let presented_hash = sha256_hex(&presented);
    let now = now_ms();
    let mut table = match refresh_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.retain(|record| record.expires_at > now);
    let Some(index) = table
        .iter()
        .position(|record| record.hash == presented_hash)
    else {
        return None;
    };
    if table[index].rotated {
        // 重放：整条会话链作废（refresh 链 + 该链签发过的所有 access）
        let session = table[index].session.clone();
        table.retain(|record| record.session != session);
        persist_refresh_tokens(&mut table);
        revoke_access_of_session(&session);
        return None;
    }
    let session = table[index].session.clone();
    table[index].rotated = true;
    drop(table);
    Some(IssuedSession::issue(session))
}

/// 登出：按 refresh cookie 找到会话链，整链撤销 + 清掉对应 access token。
pub fn revoke_session(refresh_cookie: Option<&str>, access_headers: &HeaderMap) {
    let presented_refresh = cookie_value_from(refresh_cookie, REFRESH_COOKIE);
    let presented_access = cookie_value(access_headers, ACCESS_COOKIE);
    let presented_hash = presented_refresh.as_deref().map(sha256_hex);
    let target_session;
    {
        let mut table = match refresh_tokens().lock() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        target_session = table
            .iter()
            .find(|record| Some(record.hash.as_str()) == presented_hash.as_deref())
            .map(|record| record.session.clone());
        if let Some(session) = &target_session {
            table.retain(|record| record.session != *session);
            persist_refresh_tokens(&mut table);
        }
    }
    // access 一并作废：带 token 的按值删，能定位会话链的按链删
    let mut table = match access_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(token) = presented_access {
        table.remove(&token);
    }
    if let Some(session) = &target_session {
        table.retain(|_, (session_of, _)| session_of != session);
    }
}

/// 把某条会话链签发过的所有 access token 作废（重放检测用）。
fn revoke_access_of_session(session: &str) {
    let mut table = match access_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.retain(|_, (session_of, _)| session_of != session);
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie_value_from_str(header, name)
}

fn cookie_value_from(raw: Option<&str>, name: &str) -> Option<String> {
    cookie_value_from_str(raw?, name)
}

fn cookie_value_from_str(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let part = part.trim();
        let value = part.strip_prefix(name)?.strip_prefix('=')?;
        Some(value.trim().to_string())
    })
}

// ── 登录失败锁定（按来源 IP）────────────────────────────────

struct Attempt {
    failures: u32,
    locked_until: Option<Instant>,
}

static ATTEMPTS: OnceLock<Mutex<HashMap<IpAddr, Attempt>>> = OnceLock::new();

fn attempts() -> &'static Mutex<HashMap<IpAddr, Attempt>> {
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 该来源当前是否处于登录锁定中。
pub fn login_locked(source: IpAddr) -> bool {
    let table = match attempts().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table
        .get(&source)
        .and_then(|attempt| attempt.locked_until)
        .is_some_and(|until| Instant::now() < until)
}

/// 登录失败：累计；达到阈值就锁定一段时间。
pub fn record_login_failure(source: IpAddr) {
    let mut table = match attempts().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    let attempt = table
        .entry(source)
        .or_insert(Attempt { failures: 0, locked_until: None });
    attempt.failures += 1;
    if attempt.failures >= LOCKOUT_THRESHOLD {
        attempt.locked_until = Some(Instant::now() + LOCKOUT_DURATION);
        attempt.failures = 0;
    }
}

/// 登录成功：清空该来源的失败记录。
pub fn clear_login_failures(source: IpAddr) {
    match attempts().lock() {
        Ok(mut table) => {
            table.remove(&source);
        }
        Err(poisoned) => {
            poisoned.into_inner().remove(&source);
        }
    };
}

// ── /v1 fail-closed（headless 未配 Key 时拒绝转发，见 bin）────

static V1_FAIL_CLOSED: AtomicBool = AtomicBool::new(false);

/// headless 形态标记：未注册管理员时 `/api/*`（除注册/状态两个认证边界
/// 端点）一律拒绝 —— 否则面板前端拿到 200 会直接进主界面，把「部署完
/// 必须先注册」变成可绕过的一步。桌面壳不设置它（桌面免鉴权语义不变）。
static PANEL_GATE: AtomicBool = AtomicBool::new(false);

/// headless 形态是否处于「未注册 /api/* 全拒」闸门下。
pub fn panel_gate() -> bool {
    PANEL_GATE.load(Ordering::Relaxed)
}

/// headless 启动时打开（面板认证启用后即形同虚设，保留开关是为了
/// 语义清晰：两态各自独立判断，不隐式推导）。
pub fn set_panel_gate(on: bool) {
    PANEL_GATE.store(on, Ordering::Relaxed);
}

/// `/v1/*` 是否处于 fail-closed（未配置任何 Key 的 headless 形态）。
pub fn v1_fail_closed() -> bool {
    V1_FAIL_CLOSED.load(Ordering::Relaxed)
}

/// headless 启动时按「有没有 Key」决定是否进入 fail-closed。
pub fn set_v1_fail_closed(on: bool) {
    V1_FAIL_CLOSED.store(on, Ordering::Relaxed);
}

// 注意：`panelAdmin` / `panelTokens` 两个 kv 键已登记进
// `db::schema::RESERVED_KV_KEYS`（配置写侧据它排除）—— 新增键时必须两处
// 同步，否则用户改一次配置就会把管理员与令牌静默删掉。
