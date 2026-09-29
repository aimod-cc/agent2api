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
/// 会话传输探测 cookie（90 天寿命）：登录页连打两发 `/api/panel/cookie-probe`
/// 测「这个环境里 cookie 能不能往返」。中转入口（fnOS docker 管理页这类）下发/
/// 回带都活不成，探测失败时前端才显式要求令牌进响应体（见 api::panel::cookie_probe
/// 与 panel::issue_response）。它不关联任何会话，服务端只比对值、不认会话。
pub const PROBE_COOKIE: &str = "agent2api-panel-probe";
/// 探针的**第二枚** cookie —— 它存在的唯一理由：探针要测的是「面板登录那趟响应
/// 的 cookie 能不能存下」，而登录响应发的是**两条** `Set-Cookie`（access + refresh）。
/// 只种一条的探针预测不了两条的命运：实测 2026-09-29 那台中转把单条 Set-Cookie
/// 原样透传（探针值比对通过 → 服务端判「cookie 通道完好」→ 令牌不进响应体），
/// 却把登录响应的两条按逗号拼成一条，`Path=/` 从属性降级成一个叫 `path` 的 cookie，
/// 新会话没按 `/` 落进罐里，浏览器下一趟带的还是上一轮那把陈值 —— 于是"登录成功
/// 却被弹回"。**判据的形状必须和被预测的对象一致**：两条都往返才算通道好。
pub const PROBE_COOKIE_SECOND: &str = "agent2api-panel-probe-b";

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

/// 一次登录的产出：两段 Set-Cookie 值交给 handler 下发；裸令牌同时进响应体
/// （Cookie 在「反代/嵌入面板」环境会丢，见 `session_valid` 的头回退说明）。
pub struct IssuedSession {
    pub access_cookie: String,
    pub refresh_cookie: String,
    pub access_token: String,
    pub refresh_token: String,
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
            access_token,
            refresh_token,
        }
    }

    /// 登录：开一条新会话链。
    pub fn new_session() -> Self {
        Self::issue(random_hex(16))
    }
}

/// 面板 access 令牌的**请求头**取值：`x-panel-token` 头 → `Authorization: Bearer`。
///
/// 存在的理由：面板不总在直连环境里跑 —— 从 fnOS docker 管理页一类宿主进入
/// 时，请求经宿主侧中转，HttpOnly cookie 的两跳（下发 → 回带）都可能被掐掉。
/// 前端（web_shim / login.html）把登录响应体里的裸令牌存 localStorage，之后
/// 每个请求带头；服务端**cookie 在前、头在后**（见 `access_candidates`），
/// 直连环境行为不变。
/// 头里的令牌 JS 可读（localStorage），与 HttpOnly cookie 相比多暴露给面板
/// 自身的 XSS 面 —— 这是反代环境下的可用性换安全，面板是同源可信代码。
fn access_header_tokens(headers: &HeaderMap) -> Vec<String> {
    named_header_tokens(headers, "x-panel-token")
        .into_iter()
        .chain(bearer_tokens_of(headers))
        .collect()
}

/// 取一个（可能重复出现的）请求头的所有非空值。
///
/// 复数是必要的而非防御性的：合并型中转（把多条同名响应头拼成一条的那类）也会
/// 这样处理**请求**头，于是浏览器同一个 `Cookie` 头里可能出现两次 `agent2api-panel=`
/// —— 一次有效一次陈旧。只取第一条就等于"看运气"。
fn named_header_tokens(headers: &HeaderMap, name: &str) -> Vec<String> {
    let name = axum::http::HeaderName::from_bytes(name.as_bytes());
    let Ok(name) = name else { return Vec::new() };
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn bearer_tokens_of(headers: &HeaderMap) -> Vec<String> {
    named_header_tokens(headers, "authorization")
        .into_iter()
        .filter_map(|value| {
            let token = value.strip_prefix("Bearer ")?.trim();
            if token.is_empty() {
                return None;
            }
            Some(token.to_string())
        })
        .collect()
}

/// access 凭据的**全部候选**，按「同名 cookie 的每个值 → `x-panel-token` → Bearer」
/// 排列（cookie 在前，直连环境的判定结果与从前逐字一致）。
///
/// ── 为什么不再是「cookie 优先、命中即返」────────────────────
/// 实测（2026-09-29，fnOS docker 管理页 :5666 中转）：登录成功签发新会话后，
/// 下一个 `/api/session` 仍被判无效，而日志里两行的指纹对不上 —— 浏览器回带的是
/// **上一轮**那把（`7a951233`），刚发的（`84f8d751`）没被带上。成因是中转把登录
/// 响应的两条 `Set-Cookie` 按逗号拼成一条：`agent2api-panel=新值, Path=/, HttpOnly, …`
/// 里 `Path` 从属性降级成了一个独立 cookie（回带名单里那些 `path|domain|max-age|
/// expires|session|version` 就是证据），新会话因此没按 `/` 存下来，罐里盖着的还是
/// 直连那次登录留下的旧值。
///
/// 此时**唯一**还能救回这条会话的是请求头令牌，而头令牌只在「令牌进响应体」模式
/// 下存在（见 `api::panel::issue_response`）。所以这里的改动必须与探针改造
/// （`cookie_probe` 种**两枚** cookie，一次测出合并型中转）配套：探针把环境判成
/// 中转 → 客户端拿到 body 里的令牌并带头 → 服务端**不许**再被罐里那把陈旧 cookie
/// 短路。只改一侧的结果是「探针判通了但 cookie 存歪」的老死循环，或「带了头却
/// 永远轮不到试」。
///
/// cookie 仍然排在头前面：中转哪天修好了，用户回到直连 cookie 登录，localStorage
/// 里那条早已作废的旧链令牌不该抢跑（它验证不过，自然也不再影响判定 —— 判定看的是
/// 「有没有一把有效」，不是「第一把是谁」）。
fn access_candidates(headers: &HeaderMap) -> Vec<String> {
    dedup(
        cookie_values(headers, ACCESS_COOKIE)
            .into_iter()
            .chain(access_header_tokens(headers))
            .collect(),
    )
}

/// refresh 凭据的**全部候选**，顺序与 `access_candidates` 同形
/// （同名 cookie 每个值 → `x-panel-refresh` → Bearer）。
///
/// 与 access 共用一份「为什么不再短路」的理由：轮换请求同样会被罐里那把旧
/// refresh 短路，结果是把活链旁边的僵尸链拿去轮换 —— 撞上重放检测，整链作废，
/// 用户被迫重登（这条路径此前只在日志里见过"登录已过期"，没人看得出是被短路）。
pub fn refresh_candidates(headers: &HeaderMap) -> Vec<String> {
    dedup(
        cookie_values(headers, REFRESH_COOKIE)
            .into_iter()
            .chain(named_header_tokens(headers, "x-panel-refresh"))
            .chain(bearer_tokens_of(headers))
            .collect(),
    )
}

fn dedup(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

/// 请求是否携带有效 access token —— 候选里**任一把**在服务端会话表里就算有效。
///
/// 从前这里只取一把（cookie 有就只看 cookie），于是"罐里有一把陈年的同名 cookie"
/// 与"没带任何凭证"判定结果相同（都是 401），而现场完全不同 —— 见
/// `access_candidates` 的中转事故记录。
pub fn session_valid(headers: &HeaderMap) -> bool {
    let tokens = access_candidates(headers);
    if tokens.is_empty() {
        return false;
    }
    let now = Instant::now();
    let mut table = match access_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.retain(|_, (_, expiry)| *expiry > now);
    tokens.iter().any(|token| table.contains_key(token))
}

/// 本次请求里能看到的 access 指纹（诊断日志用，最多两把 —— 排查「同名 cookie
/// 重复上行」时要能看见每一把分别是什么，值本身绝不进日志）。
pub fn access_fingerprints(headers: &HeaderMap) -> Vec<String> {
    access_candidates(headers)
        .into_iter()
        .take(2)
        .map(|token| token_fingerprint(&token))
        .collect()
}

/// 用请求里的 refresh 候选轮换出新一组令牌（同一会话链）。
///
/// 候选按顺序试，**第一个仍活着的**（未轮换、未过期）就是当前会话 —— 陈旧 cookie
/// 与头里的旧令牌都不会把活链挤掉。返回 `None` = 没有一把能用。
///
/// **重放检测**照旧生效，但只在「没有任何一把候选是活的」时才判：此时若某把候选
/// 命中了一条已轮换记录，说明它泄露了（活链上新令牌在浏览器手里）—— 整条会话链
/// 作废，逼着重新登录。之所以要先穷尽活候选：罐里同时躺着新旧两把是**正常现象**
/// （合并型中转就是这么留脏的），把"读到了旧的那把"当成泄露会把好环境踢成反复重登。
pub fn rotate_session(headers: &HeaderMap) -> Option<IssuedSession> {
    let candidates = refresh_candidates(headers);
    if candidates.is_empty() {
        return None;
    }
    let hashes: Vec<String> = candidates.iter().map(|token| sha256_hex(token)).collect();
    let now = now_ms();
    let mut table = match refresh_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.retain(|record| record.expires_at > now);
    let live = hashes.iter().find_map(|hash| {
        table
            .iter()
            .position(|record| &record.hash == hash && !record.rotated)
    });
    if let Some(index) = live {
        let session = table[index].session.clone();
        table[index].rotated = true;
        drop(table);
        return Some(IssuedSession::issue(session));
    }
    // 一把活的都没有：按「候选里最先出现的已轮换记录」判重放，整条会话链作废
    // （refresh 链 + 该链签发过的所有 access）。
    let replay = hashes.iter().find_map(|hash| {
        table
            .iter()
            .position(|record| &record.hash == hash && record.rotated)
    });
    // 连已轮换记录都没命中 → 纯粹的无效令牌（过期 / 别的会话 / 拼歪的碎片）：
    // 按「刷新失败」处理，不动任何链。
    let index = replay?;
    let session = table[index].session.clone();
    table.retain(|record| record.session != session);
    persist_refresh_tokens(&mut table);
    drop(table);
    revoke_access_of_session(&session);
    None
}

/// 登出：按 refresh 候选找到会话链，整链撤销 + 清掉请求里能定位到的 access。
pub fn revoke_session(headers: &HeaderMap) {
    let hashes: Vec<String> = refresh_candidates(headers)
        .iter()
        .map(|token| sha256_hex(token))
        .collect();
    let presented_access = access_candidates(headers);
    let target_session;
    {
        let mut table = match refresh_tokens().lock() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        target_session = hashes
            .iter()
            .find_map(|hash| {
                table
                    .iter()
                    .find(|record| &record.hash == hash)
                    .map(|record| record.session.clone())
            });
        if let Some(session) = &target_session {
            table.retain(|record| record.session != *session);
            persist_refresh_tokens(&mut table);
        }
    }
    // access 一并作废：带 token 的按值删（候选里的每一把都删，包括陈旧那把 ——
    // 登出时没人希望罐里的旧令牌还活着），能定位会话链的按链删
    let mut table = match access_tokens().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    for token in presented_access {
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

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    cookie_values(headers, name).into_iter().next()
}

/// `Cookie` 头里某个名字的**全部**取值（按上行顺序）。
///
/// 同名两条是**实测会出现**的形态，不是理论：中转拼头、或同一主机不同端口/路径
/// 各存了一份（cookie 按主机算不分端口，fnOS :5666 中转与 :3065 直连共用一份罐）。
/// 只取第一条 = 判定结果取决于浏览器哪天先带哪条上行，"登录成功却被弹回"就是这么来的。
pub fn cookie_values(headers: &HeaderMap, name: &str) -> Vec<String> {
    let Some(header) = headers.get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return Vec::new();
    };
    cookie_values_from_str(header, name)
}

/// `Cookie` 头里出现过的**名字**（排序去重，**只给名字、不取值**）。
///
/// 为什么要这份名单：面板「登录成功却被弹回」这类现场，只看得到「cookie 有 / 无」
/// 是不够的 —— 中转入口（fnOS docker 管理页这类）常见的毛病是把多条 `Set-Cookie`
/// 合并或只透传一条，于是浏览器带回来的往往是**探针** cookie 或**上一轮**的会话
/// cookie：同样是"cookie=有"，但对不上这一把会话。名字列表能一次把三种情况分开
/// （没送到 / 送到但不是这把 / 两条都在却各自过期），值本身是凭据，进日志等于把
/// 令牌抄到磁盘上，所以这里刻意只出名字。
pub fn cookie_names(headers: &HeaderMap) -> Vec<String> {
    let Some(header) = headers.get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = header
        .split(';')
        .filter_map(|part| part.split('=').next().map(str::trim))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// 令牌的**短指纹**（sha256 前 8 位十六进制）—— 只为把两行日志对上，不还原令牌。
///
/// 日志里绝不能出现令牌本身（那等于把凭据抄进 SQLite 与备份文件）。但排查
/// 「登录明明成功、下一个请求却仍被判无效」时必须能回答一个问题：带回的那把
/// access cookie 是**这次发的**还是**上一轮残留的** —— 两者在"cookie=有"里
/// 长得一模一样。8 位十六进制（4×10⁸ 空间）足以区分同机先后两把，又不足以反推。
pub fn token_fingerprint(value: &str) -> String {
    sha256_hex(value).chars().take(8).collect()
}

/// 本次登录下发的 access 令牌指纹（登录日志与后续请求日志对得上用）。
pub fn access_fingerprint(session: &IssuedSession) -> String {
    token_fingerprint(&session.access_token)
}

fn cookie_values_from_str(header: &str, name: &str) -> Vec<String> {
    header
        .split(';')
        .filter_map(|part| {
            let part = part.trim();
            let value = part.strip_prefix(name)?.strip_prefix('=')?;
            Some(value.trim().to_string())
        })
        // 只收**真的有值**的那几条：中转把 Set-Cookie 拼歪时常见 `name=`（空值）
        // 留在罐里，它不是凭据，让它进候选只会多一次注定落空的查表。
        .filter(|value: &String| !value.is_empty())
        .collect()
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

#[cfg(test)]
mod credential_candidate_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    fn header_name(value: &str) -> HeaderName {
        HeaderName::from_bytes(value.as_bytes()).expect("测试里的头名必须是合法 token")
    }

    /// 组装上行请求头：`cookie` 原文（空串 = 不带 Cookie 头）+ 任意个
    /// `(头名, 值)`（同名可重复，走 append，因为被测的正是「同一名字两条上行」
    /// 这种中转产物）。
    fn request(cookie: &str, pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !cookie.is_empty() {
            headers.insert(
                axum::http::header::COOKIE,
                HeaderValue::from_str(cookie).unwrap(),
            );
        }
        for (name, value) in pairs {
            headers.append(header_name(name), HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    /// 候选顺序：同名 cookie 的每个值 → `x-panel-refresh` → Bearer，去重。
    ///
    /// cookie 仍在最前（直连环境的判定与从前同序）；改动只发生在「第一把无效时
    /// 不许就此收摊」。
    #[test]
    fn candidates_are_cookie_values_first_then_headers_and_deduplicated() {
        let headers = request(
            "agent2api-panel-rt=from-cookie-a; x=1; agent2api-panel-rt=from-cookie-b",
            &[("x-panel-refresh", "from-header"), ("authorization", "Bearer from-bearer")],
        );
        assert_eq!(
            vec![
                "from-cookie-a".to_string(),
                "from-cookie-b".to_string(),
                "from-header".to_string(),
                "from-bearer".to_string(),
            ],
            refresh_candidates(&headers)
        );

        // 同一把既在 cookie 又在头里：只试一次
        let once = request(
            "agent2api-panel-rt=same",
            &[("x-panel-refresh", "same")],
        );
        assert_eq!(vec!["same".to_string()], refresh_candidates(&once));

        // 空值不进候选（`name=` 是拼歪的 Set-Cookie 留下的碎片，不是凭据）
        let blanks = request("agent2api-panel-rt=;", &[("x-panel-refresh", "   ")]);
        assert!(refresh_candidates(&blanks).is_empty());
    }

    /// access 侧同一条序（cookie → x-panel-token → Bearer）。
    #[test]
    fn access_candidates_follow_the_same_order() {
        let headers = request(
            "agent2api-panel=cookie-one; agent2api-panel=cookie-two",
            &[("x-panel-token", "header-one"), ("authorization", "Bearer bearer-one")],
        );
        assert_eq!(
            vec![
                "cookie-one".to_string(),
                "cookie-two".to_string(),
                "header-one".to_string(),
                "bearer-one".to_string(),
            ],
            access_candidates(&headers)
        );
    }

    /// 本次事故的正对照（2026-09-29，fnOS :5666 中转）：罐里盖着一把**陈年**同名
    /// cookie、头里带着刚拿到的令牌 —— 旧实现在 cookie 那一步就短路了，于是
    /// 「登录成功 → 下一个请求 401 → 弹回登录页」死循环。现在每一把候选都要试。
    #[test]
    fn a_stale_cookie_cannot_shadow_the_header_token() {
        let session = IssuedSession::new_session();
        let stale = random_hex(32);

        let with_header = request(
            &format!("{ACCESS_COOKIE}={stale}"),
            &[("x-panel-token", &session.access_token)],
        );
        assert!(
            session_valid(&with_header),
            "陈 cookie 不许把有效的头令牌挡在门外"
        );

        // 同一条会话、同一份 cookie，只是**不带那个头** → 仍然无效。
        // 这一条控制项是上一条断言的全部意义：少了它，「上面变 true」既可能是
        // 「试了头」也可能是「判定被放宽成无条件放行」。
        let cookie_only = request(&format!("{ACCESS_COOKIE}={stale}"), &[]);
        assert!(
            !session_valid(&cookie_only),
            "只带陈 cookie 时必须仍然判无效（否则上一条断言什么都没证明）"
        );
    }

    /// 同名 cookie 重复上行（浏览器把不同 Path 的两份一起带上来）时，每一把
    /// 都会被试 —— 有效那把排在后面也一样。
    #[test]
    fn every_repeated_cookie_value_gets_tried() {
        let session = IssuedSession::new_session();
        let stale = random_hex(32);

        let stale_first = request(
            &format!("{ACCESS_COOKIE}={stale}; {ACCESS_COOKIE}={}", session.access_token),
            &[],
        );
        assert!(session_valid(&stale_first), "第二把同名 cookie 也必须试到");

        let live_first = request(
            &format!("{ACCESS_COOKIE}={}; {ACCESS_COOKIE}={stale}", session.access_token),
            &[],
        );
        assert!(session_valid(&live_first));

        // 两把都是陈的 → 无效（同上，给上一条做对照）
        let both_stale = request(
            &format!("{ACCESS_COOKIE}={stale}; {ACCESS_COOKIE}={}", random_hex(32)),
            &[],
        );
        assert!(!session_valid(&both_stale));
    }

    /// Bearer 通道同样要兜得住：连自定义请求头都被剥的中转只放标准头过去。
    #[test]
    fn the_bearer_channel_is_a_candidate_too() {
        let session = IssuedSession::new_session();
        let headers = request(
            &format!("{ACCESS_COOKIE}={}", random_hex(32)),
            &[("authorization", &format!("Bearer {}", session.access_token))],
        );
        assert!(session_valid(&headers));
    }

    /// 轮换：cookie 里那把**已轮换**的陈 refresh 排在前面时，要跳到头里那把活的，
    /// 不许把活链当成重放作废掉。
    #[test]
    fn rotation_skips_a_rotated_stale_refresh_to_the_live_one() {
        let first = IssuedSession::new_session();
        let second = rotate_session(
            &request(&format!("{REFRESH_COOKIE}={}", first.refresh_token), &[]),
        )
        .expect("首次轮换应成功");
        assert!(
            session_valid(&request(
                &format!("{ACCESS_COOKIE}={}", second.access_token),
                &[]
            )),
            "轮换签发的 access 应进会话表（否则后面的断言都在真空里跑）"
        );

        // 现在 first.refresh 是「已轮换」记录，second.refresh 是活链。
        // 罐里若还留着 first 那把（Path=/ 的残本）并排在其前，旧实现会命中它、
        // 判重放、整链作废 → 返回 None → 用户被迫重登。
        let mixed = request(
            &format!("{REFRESH_COOKIE}={}", first.refresh_token),
            &[("x-panel-refresh", &second.refresh_token)],
        );
        let third = rotate_session(&mixed).expect("陈旧那把不该挡住活链的轮换");

        // 链仍然连着：新签发的 access 有效、新 refresh 还能再轮换一次
        assert!(session_valid(&request(
            &format!("{ACCESS_COOKIE}={}", third.access_token),
            &[]
        )));
        assert!(rotate_session(&request(
            &format!("{REFRESH_COOKIE}={}", third.refresh_token),
            &[]
        ))
        .is_some());
    }

    /// 重放检测没被削弱：只拿**已轮换**的旧令牌来换（没有一把活的在场），
    /// 仍是整条会话链作废。
    #[test]
    fn a_replayed_refresh_alone_still_kills_its_chain() {
        let first = IssuedSession::new_session();
        let second = rotate_session(
            &request(&format!("{REFRESH_COOKIE}={}", first.refresh_token), &[]),
        )
        .expect("首次轮换应成功");

        // 旧令牌单独上行 → 判重放，None
        let replayed = rotate_session(
            &request(&format!("{REFRESH_COOKIE}={}", first.refresh_token), &[]),
        );
        assert!(replayed.is_none(), "已轮换令牌单独来换必须判重放");

        // 同链的**活**令牌也应随之作废（整链撤销）—— 这一条才是「整链」二字的意思
        assert!(
            rotate_session(&request(
                &format!("{REFRESH_COOKIE}={}", second.refresh_token),
                &[]
            ))
            .is_none(),
            "重放检测应作废整条会话链"
        );
        assert!(
            !session_valid(&request(
                &format!("{ACCESS_COOKIE}={}", second.access_token),
                &[]
            )),
            "该链签发过的 access 要一并作废"
        );
    }

    /// 登出：候选里任一把命中就整链撤销，且**每一把**上行的 access 都按值删。
    #[test]
    fn logout_revokes_through_the_stale_cookie_and_the_header_together() {
        let session = IssuedSession::new_session();
        let foreign_access = random_hex(32);
        let headers = request(
            &format!("{REFRESH_COOKIE}={}", session.refresh_token),
            &[("x-panel-token", &session.access_token)],
        );
        assert!(session_valid(&headers));
        revoke_session(&headers);

        // 链没了：refresh 换不出新令牌，access 也不再有效
        assert!(rotate_session(&request(
            &format!("{REFRESH_COOKIE}={}", session.refresh_token),
            &[]
        ))
        .is_none());
        assert!(!session_valid(&request(
            &format!("{ACCESS_COOKIE}={}", session.access_token),
            &[]
        )));
        // 对照：没被登出的另一把 access 不受牵连（撤的是链，不是全表）
        let other = IssuedSession::new_session();
        assert!(session_valid(&request(
            &format!("{ACCESS_COOKIE}={}", other.access_token),
            &[]
        )));
        assert!(!session_valid(&request(
            &format!("{ACCESS_COOKIE}={foreign_access}"),
            &[]
        )));
    }
}
