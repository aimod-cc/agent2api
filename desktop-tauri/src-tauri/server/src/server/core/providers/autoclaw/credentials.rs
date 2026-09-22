//! AutoClaw 凭证来源与刷新（Agent2API 二期 T-c1；移植来源
//! `D:\APP\AutoClaw\autoclaw-local-proxy\autoclaw-local-auth.mjs`
//! + `autoclaw-upstream-client.mjs`）。
//!
//! ── 两个凭证来源与优先级（源实现 `getCredentials`）───────────
//!   1. **主来源**：`%APPDATA%/AutoClaw/auth.json` 的 `token` / `refreshToken`
//!      字段（`enc:` 前缀 = safeStorage 密文，解密链见 `crypto.rs`）。
//!      能解出 refreshToken，因此**支持自主刷新**。
//!   2. **备来源**：`~/.openclaw-autoclaw/openclaw.json` 里
//!      `models.providers.*.models[].headers['X-Authorization']` 的明文 JWT。
//!      只有 access token，**无法刷新**。
//!
//! 优先级与源实现逐字一致：**先试来源 1，失败才试来源 2**；两个都失败时抛出
//! **来源 1 的错误**（源实现 `throw authFileError`）—— 那条错误更能说明问题
//! （「没登录」比「网关配置里没有 X-Authorization」更接近用户要做的动作）。
//!
//! 环境变量旁路（源实现上游客户端 `resolveCredentials` 的第二优先级，
//! 在「账号列表选中项」之后）由 `env_credentials` 提供，供适配器在
//! 没有可用账号时兜底 —— 与 workbuddy 的 `WORKBUDDY_TOKEN`、小浣熊的
//! `RACCOON_TOKEN` 同一地位。
//!
//! ── 实时读盘 + mtime 缓存（模式对照 `raccoon/credentials.rs`）────
//! 两个来源都**实时走磁盘**、按 mtime 缓存解析结果：AutoClaw 桌面端重新登录后
//! 会重写 auth.json（mtime 变），网关下一次请求即拿到新登录态，不需要重启。
//! 缓存是**进程级**的（`OnceLock` + `Mutex`）：多个调用点共享一份。
//!
//! 与小浣熊那套的区别：AutoClaw 的 auth.json 是**加密**的，解密（DPAPI + AES-GCM）
//! 比读 JSON 贵得多，所以缓存里存的是**解密后的凭证**（小浣熊缓存的是文件 JSON）。
//! 判定键是「来源 + 路径指纹 + mtime」，文件没变就不重复做那件贵的事。
//!
//! ── 刷新：第一期**只读不回写**（架构文档 §10.2 的取舍）─────────
//! 刷新的网络行为与源实现一致（Bearer + refresh_token，失败按 400002 降级
//! `agent-refresh`），实现落在 **`refresh.rs`**；但**刷新结果不落盘**：
//!   - auth.json 是 safeStorage **加密**格式，回写要用同一把 os_crypt 密钥
//!     重新加密，还要与桌面端进程争抢同一个文件；一旦写坏，用户桌面端都登不进去。
//!   - 更关键的是 refresh_token 轮换：服务端每次刷新会换发新的 refresh_token，
//!     网关回写会与桌面端自己的刷新互相顶掉（源实现把桌面态标成
//!     `persistRefresh: false` 正是这个原因）。
//! 因此行为是：**刷新只在内存生效**（写进下文的凭证缓存，文件 mtime 一变即失效），
//! 进程重启后重新解密原文件。代价是「长时间运行的网关可能比桌面端先用掉一次
//! refresh_token」；收益是**绝不损坏用户的登录态文件**。缓存写入走比较-再写
//! （`store_cached_if_current`）：登录态在刷新期间被更换时不覆盖新值。
//!
//! ── Send 硬约束（与 `raccoon/credentials.rs` 同一坑）──────────
//! 刷新链（`refresh.rs`）的 future 必须是 `Send`（跑在多线程运行时上）。因此
//! **任何 async fn 内部都不得持有非 Send 的守卫**：`MutexGuard` 一律在同步
//! 函数内取用并释放（本文件的 `lookup_cached` / `store_cached` /
//! `store_cached_if_current`，以及共享单飞原语 `refresh_flight` 里的表操作），
//! await 一定在锁外。本文件自身没有 async fn（刷新编排全在 `refresh.rs`）。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::server::errors::GatewayError;
use crate::server::logging;

use super::crypto;
use super::region::Region;

/// 桌面端实时账号的固定 id（对照源实现 `account-store.mjs` 的
/// `DESKTOP_ACCOUNT_ID = 'desktop-auth'`）。
///
/// 用源实现的同一个字符串：账号导入/前端展示都以它为主键，
/// 换一个值会让「同一个账号」在两边看起来是两个。
pub const DESKTOP_ACCOUNT_ID: &str = "desktop-auth";

/// 凭证文件大小上限（照抄源实现 `MAX_AUTH_FILE_SIZE`）
const MAX_AUTH_FILE_SIZE: u64 = 256 * 1024;

/// 鉴权接口请求超时（源实现 `REQUEST_TIMEOUT_MS`）
pub(crate) const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 临期主动刷新窗口：过期前 5 分钟（本次修复，对齐官方
/// `DESKTOP_REFRESH_AHEAD_MS = 5 * 60 * 1000`）。
///
/// ── 为什么从 2 分钟改成 5 分钟 ──────────────────────────────
/// 原值是 120 秒（移植自源项目 `PROACTIVE_REFRESH_MARGIN_MS`），但那个值在官方
/// 客户端里是**配合 60 秒一轮的扫描**用的：窗口 2 分钟、每 60 秒看一次，
/// 进入窗口后最多等一轮就被刷掉，不会漏。
///
/// 网关没有那个 60 秒定时器，负责这条判定的是**维护任务**，默认间隔 10 分钟 ——
/// 比窗口本身还宽。于是「token 进入 2 分钟窗口」与「下一轮维护到来」之间经常
/// 错过：一个刚过窗口的 token 最长要等一整轮（10 分钟）才被处理，而它可能
/// 早就过期了。窗口调到 5 分钟与官方口径一致（`DESKTOP_REFRESH_AHEAD_MS`），
/// 同时也让小浣熊 / Cline 两家的窗口与维护间隔重新匹配（那两家是 5 / 10 分钟）。
///
/// 代价是更早用掉一次 refresh_token 轮换 —— 与「漏刷导致 401」相比，
/// 这个代价是可接受的（官方的 5 分钟正是这个取舍的结果）。
pub(crate) const PROACTIVE_REFRESH_MARGIN_MS: f64 = 300_000.0;

/// 刷新签名校验失败的业务码（源实现 `REFRESH_FALLBACK_CODE`）：
/// 收到它说明 `/userapi/v1/refresh` 的签名校验没过，改用 `agent-refresh` 再试一次。
pub(crate) const REFRESH_FALLBACK_CODE: i64 = 400_002;

// ── 签名常量（AUTH_APP_ID / AUTH_APP_KEY）不在这里 ─────────────
// 它们的使用点是**刷新请求的头**（`X-Auth-Sign = MD5(appId&ts&appKey)`），
// 因此只在 `refresh.rs` 里声明一份。本文件曾留过一份同名副本，
// 接线（T-c2）后编译器把它报成死代码 —— 删掉，避免两处 appId/appKey
// 各自演进导致签名与刷新配置分叉。

/// 一份解析好的 AutoClaw 凭证（源实现 `credentials` 对象的等价物）
#[derive(Clone, Debug, Default)]
pub struct AutoClawCredentials {
    /// 账号 id：`desktop-auth`（桌面端登录态）或 `user-<userId>`（手动账号）
    pub id: String,
    /// **这份凭证属于哪个地区**（国内版 / 国际版）。
    ///
    /// ── 为什么地区是凭证的一部分，而不是「调用方知道就行」──────
    /// 凭证的每一步都要它：刷新打哪个 userapi 域、积分查哪个站、转发发到哪个
    /// 代理、环境变量读哪一组前缀。把这些散到调用方等于让每个调用点都自己
    /// 决定「这个凭证是哪一家的」—— 而凭证是从账号记录 / 桌面端文件 / 环境
    /// 变量三条来源解析出来的，只有**解析点**知道它属于谁。放进来之后，
    /// 下游（refresh / balance / checkin / adapter）一律读 `credentials.region`，
    /// 不再各猜一次。
    ///
    /// 缺省 [`Region::Cn`]：与 provider id 的历史口径一致（`autoclaw` 是历史
    /// 已有的那一家，存量账号都是它）。
    pub region: Region,
    /// access token（已去 `Bearer ` 前缀）
    pub token: String,
    /// refresh token（已去 `Bearer ` 前缀）；桌面端来源与手动账号都可能为空
    pub refresh_token: String,
    /// 设备 id（刷新接口要带；来源 = auth.json 的 deviceId ?? JWT 的 device_id）
    pub device_id: String,
    /// 用户 id（JWT 的 `user_id` 声明）
    pub user_id: String,
    /// 过期时间（毫秒）；JWT 解不出 exp 时为 None
    pub expires_at: Option<f64>,
    /// 凭证来源：决定刷新结果往哪回写（本期一律不回写，见模块头）
    pub origin: CredentialOrigin,
    /// **内部用**：本地来源的内存刷新覆盖键（`auth:<mtime>` / `gw:<mtime>`）。
    /// 账号来源（accounts.json / 环境变量）为 None —— 它们的刷新结果本来就不
    /// 落在进程缓存里，而是由调用方决定要不要持久化。
    pub cache_key: Option<String>,
}

/// 凭证来源
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CredentialOrigin {
    /// `%APPDATA%/AutoClaw/auth.json`（safeStorage 加密，需 DPAPI 密钥）
    #[default]
    DesktopAuthFile,
    /// `~/.openclaw-autoclaw/openclaw.json`（明文 JWT，只有 access token）
    GatewayConfig,
    /// accounts.json 里的手动账号
    AccountStore,
    /// `AUTOCLAW_TOKEN` 环境变量（脚本 / CI 入口）
    Environment,
}

impl CredentialOrigin {
    /// 是否为「本地文件实时登录态」来源（决定刷新结果是否进内存覆盖缓存）
    pub(crate) fn is_local_file(self) -> bool {
        matches!(self, Self::DesktopAuthFile | Self::GatewayConfig)
    }
}

/// 每小时**强制**刷新的间隔（本次修复；对齐官方 `HOURLY_REFRESH_INTERVAL_MS`）。
///
/// ── 为什么需要这个「不看窗口」的刷新 ─────────────────────────
/// 官方桌面端除了「临期前 5 分钟刷」，还有一条**每小时无条件刷一次**的调度
/// （`reason: "hourly_forced"`，本机 auth 日志里能看到 `09:12→10:12→11:12`
/// 这样的整点记录）。它的作用不是「token 快过期了」，而是把 refresh_token
/// **温着**：AutoClaw 服务端会轮换 refresh_token，长期不用的那一份可能被判定
/// 失效，此后网关再拿它去刷就是硬失败、只能重新登录。
///
/// 网关原先只有「进入 2 分钟窗口才刷」这一条路，而维护任务的间隔是 10 分钟 ——
/// 窗口比间隔还窄，长期闲置的账号可能连续几天不触发一次刷新。这里补上官方那条
/// 每小时强制刷新的语义。
const HOURLY_FORCED_REFRESH_INTERVAL_MS: i64 = 60 * 60 * 1000;

/// 某账号是否到了「每小时强制刷新」的时刻（进程级节流）。
///
/// 键是账号记录 id：刷新失败时也要被节流住，否则会退化成每轮维护都打一次上游。
/// 首次调用即视为「到期」（账号刚出现，刷一次是合理的）。
///
/// ── 为什么状态在进程里而不落盘 ──────────────────────────────
/// 它是「上次刷了没有」的调度状态，不是凭证的一部分：重启后重新计一小时，
/// 最坏后果只是多重启几次就多刷几次，不会让凭证失效或丢失。
pub(crate) fn hourly_forced_refresh_due(account_id: &str, now_ms: i64) -> bool {
    use std::collections::HashMap;
    static LAST: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    let table = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match table.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.get(account_id) {
        Some(last) if now_ms.saturating_sub(*last) < HOURLY_FORCED_REFRESH_INTERVAL_MS => false,
        _ => {
            guard.insert(account_id.to_string(), now_ms);
            true
        }
    }
}

impl AutoClawCredentials {
    /// 现在是否需要刷新（源实现 `currentCredentials` 的 `expiring` 判定）。
    ///
    /// `expires_at` 缺失时不刷新：没有依据就说「临期」会让每个请求都去打一次
    /// 刷新接口（源实现同样是 `credentials.expiresAt && ...` 的短路写法）。
    /// 「判不出过期时间也要定期刷」的兜底走 [`hourly_forced_refresh_due`]，
    /// 只挂在维护任务那条低频路径上 —— 见那里的说明。
    pub fn is_expiring(&self) -> bool {
        match self.expires_at {
            Some(expires_at) => expires_at - PROACTIVE_REFRESH_MARGIN_MS <= logging::now_ms() as f64,
            None => false,
        }
    }

    /// 能否刷新：有 refreshToken 才行
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.is_empty()
    }
}

// ─── 路径与环境变量 ─────────────────────────────────────────

/// 用户主目录（Windows 优先 USERPROFILE，其它平台 HOME）
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// AutoClaw 用户数据目录（源实现 `defaultUserDataDir`）：
/// `AUTOCLAW_USER_DATA_DIR` > `%APPDATA%/AutoClaw`。
///
/// 源实现在非 Windows 上返回 null（整条 auth.json 来源不可用）——
/// 这里保持同一语义：**返回 None 而不是猜一个路径**。下游会给出
/// 「仅支持 Windows」的中文错误，随后回落到 openclaw.json 来源。
fn default_user_data_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("AUTOCLAW_USER_DATA_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if !cfg!(windows) {
        return None;
    }
    std::env::var("APPDATA")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|appdata| appdata.join("AutoClaw"))
        .or_else(|| Some(home_dir().join("AppData").join("Roaming").join("AutoClaw")))
}

/// `auth.json` 路径（导入与状态查询用；非 Windows 上为 None）
pub fn desktop_auth_file() -> Option<PathBuf> {
    default_user_data_dir().map(|dir| dir.join("auth.json"))
}

/// `Local State` 路径（DPAPI 密钥所在；与 auth.json 同目录）
fn local_state_file() -> Option<PathBuf> {
    default_user_data_dir().map(|dir| dir.join("Local State"))
}

/// 网关配置路径（源实现 `gatewayConfigPath`）：
/// `~/.openclaw-autoclaw/openclaw.json`
pub fn gateway_config_file() -> PathBuf {
    home_dir().join(".openclaw-autoclaw").join("openclaw.json")
}

/// 用户中心基址（源实现 `AUTOCLAW_USERAPI_BASE_URL` 可覆盖；国际版是
/// `AUTOCLAW_INTL_USERAPI_BASE_URL`）。
///
/// ── 为什么必须带 region 参数 ─────────────────────────────────
/// 改造前这里是「读一个全局环境变量、回落到唯一那个默认域名」的写法 ——
/// 那套写法默认了「AutoClaw 只有一个站点」。国际版接入后，同一个函数要回答
/// 「哪个地区的 userapi」，因此地区是**入参**而不是隐含的全局状态。
pub fn userapi_base_url(region: Region) -> String {
    region
        .env_override("USERAPI_BASE_URL")
        .unwrap_or_else(|| region.userapi_base_url().to_string())
}

/// 上游 LLM 代理基址（源实现 `AUTOCLAW_UPSTREAM_BASE_URL` 可覆盖；国际版是
/// `AUTOCLAW_INTL_UPSTREAM_BASE_URL`）。
pub fn upstream_base_url(region: Region) -> String {
    region
        .env_override("UPSTREAM_BASE_URL")
        .unwrap_or_else(|| region.upstream_base_url().to_string())
}

// ─── 文件读取（形态防御 + mtime）─────────────────────────────

/// 一次文件读取的结果：JSON 对象 + 修改时间（毫秒）
struct FileSnapshot {
    json: serde_json::Map<String, Value>,
    modified_at: i64,
}

/// 读一个凭证 JSON 文件（源实现 `readJsonFile` 的逐条防御）。
///
/// 防御项：路径必须是**普通文件**（不是符号链接 —— 拒绝把凭证交给一个指向
/// 别处的链接）、大小在 `(0, 256KB]`、根必须是 JSON 对象。
fn read_json_file(path: &Path, label: &str) -> Result<FileSnapshot, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("未找到{label}")
        } else {
            format!("无法访问{label}: {error}")
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(format!("{label}路径是符号链接，已拒绝读取"));
    }
    if !meta.is_file() {
        return Err(format!("{label}路径不是普通文件"));
    }
    if meta.len() == 0 || meta.len() > MAX_AUTH_FILE_SIZE {
        return Err(format!("{label}文件大小异常"));
    }
    let text = std::fs::read_to_string(path).map_err(|error| format!("无法读取{label}: {error}"))?;
    let value =
        serde_json::from_str::<Value>(&text).map_err(|_| format!("{label}无法解析（不是有效 JSON）"))?;
    let Value::Object(json) = value else {
        return Err(format!("{label}根节点不是 JSON 对象"));
    };
    Ok(FileSnapshot {
        json,
        modified_at: mtime_ms(&meta),
    })
}

/// 文件的修改时间（毫秒）；取不到给 0
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// ─── 凭证缓存（一份缓存承担两件事）───────────────────────────

/// 进程级凭证缓存：`(缓存键, 解析好的凭证)`，**按地区各一格**。
///
/// 缓存键是「来源 + 地区 + 路径指纹 + mtime」，一份缓存同时承担两件事 ——
/// 这正是源实现 `cache = { key, value }` 的语义：
///
///   1. **mtime 缓存**：文件没变就不重复解密/解析。AutoClaw 的 auth.json 是加密的，
///      每请求一次 DPAPI + AES-GCM 是纯浪费；mtime 变了（桌面端重新登录）自然失配。
///   2. **刷新结果覆盖**：刷新成功后把新凭证写进同一个键。文件一变（mtime 变）
///      这条覆盖就自动作废，回到「重新解密原文件」—— 正是「刷新只在内存生效、
///      重启后回到原文件」想要的语义，不需要额外的失效逻辑。
///
/// ── 为什么从「一格」改成「按地区各一格」（本次修正）────────────
/// 源实现是单槽（它只有一家）。两地接入后，两家的**桌面端来源读的是同一个
/// 文件**（同一个路径、同一个 mtime），而它们的缓存键（在带地区之前）逐字相同 ——
/// 后果有两个，都是真的：
///   1. **串味**：先解析的那家把凭证连同**它自己的 region** 缓存进去，另一家
///      命中后拿到「带着对方域名的凭证」→ 转发打到错误站点、稳定 401，
///      且日志上完全看不出原因（缓存命中，没有任何解析痕迹）；
///   2. **抖动**：即便键带上了地区，若缓存只有一格，两家交替请求会**互相顶掉**
///      对方的条目，于是每次请求都重跑一遍 DPAPI + AES-GCM（那正是这层缓存
///      存在的意义）。
/// 两格之后两个问题一起消失，而每格内部仍是「一格 + 比较再写」的既有语义。
fn credentials_cache(region: Region) -> &'static Mutex<Option<(String, AutoClawCredentials)>> {
    static CN: OnceLock<Mutex<Option<(String, AutoClawCredentials)>>> = OnceLock::new();
    static INTL: OnceLock<Mutex<Option<(String, AutoClawCredentials)>>> = OnceLock::new();
    let slot = match region {
        Region::Cn => &CN,
        Region::Intl => &INTL,
    };
    slot.get_or_init(|| Mutex::new(None))
}

/// 查缓存（锁在返回前释放 → 调用方不会持锁跨 await）
fn lookup_cached(region: Region, cache_key: &str) -> Option<AutoClawCredentials> {
    let guard = match credentials_cache(region).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .as_ref()
        .filter(|(key, _)| key == cache_key)
        .map(|(_, credentials)| credentials.clone())
}

/// 写缓存（解析成功时与刷新成功时共用）
pub(crate) fn store_cached(region: Region, cache_key: &str, credentials: &AutoClawCredentials) {
    let mut guard = match credentials_cache(region).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some((cache_key.to_string(), credentials.clone()));
}

/// **比较-再写**：只有缓存里仍是刷新前那份凭证、且来源文件也没变过时才写入。
///
/// 返回 `true` = 已写入；`false` = 登录态已被替换（桌面端重新登录 → mtime 变 →
/// 缓存键变，或另一轮刷新先落地，或来源文件已不可读），此时**不写**。
///
/// 为什么必须比较：`auth.json` 是网关与桌面端共用的登录态。一次更早发起、更晚
/// 返回的刷新如果无条件写进缓存，就会用**旧凭证**盖掉用户刚登录的新凭证。
///
/// 为什么还要看文件：缓存里的旧键可能一直没被读盘路径刷新，此时「缓存键 + 内容
/// 都还是旧值」会骗过纯缓存比较；多查一次文件 mtime 才能在落缓存前确认来源确实
/// 没变。文件被删/损坏时同样拒绝写入（没有可确认的原文件，不替用户猜）。
pub(crate) fn store_cached_if_current(
    region: Region,
    cache_key: &str,
    expected: &AutoClawCredentials,
    credentials: &AutoClawCredentials,
) -> bool {
    if !local_source_unchanged(expected) {
        return false;
    }
    let mut guard = match credentials_cache(region).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some((current_key, current)) = guard.as_ref() else {
        // 缓存已被清空：无法确认当前登录态是否仍是刷新前那份，不写入
        return false;
    };
    if current_key != cache_key
        || current.token != expected.token
        || current.refresh_token != expected.refresh_token
    {
        return false;
    }
    *guard = Some((cache_key.to_string(), credentials.clone()));
    true
}

/// 本地文件来源的「文件仍是刷新前那份」判定（缓存键里带着路径指纹与 mtime）。
/// 账号来源（`cache_key == None`）没有可比较的文件，返回 `true`。
fn local_source_unchanged(credentials: &AutoClawCredentials) -> bool {
    let Some(cache_key) = credentials.cache_key.as_deref() else {
        return true;
    };
    let Some((prefix, stamp)) = cache_key.rsplit_once(':') else {
        return false;
    };
    let Some(expected_mtime) = stamp.parse::<i64>().ok() else {
        return false;
    };
    let path = match prefix.split_once(':').map(|(kind, _)| kind) {
        Some("auth") => match desktop_auth_file() {
            Some(path) => path,
            None => return false,
        },
        Some("gw") => gateway_config_file(),
        _ => return false,
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            mtime_ms(&meta) == expected_mtime
        }
        // 文件被删/换成了非常规文件：没有可确认的原文件 → 拒绝写入
        _ => false,
    }
}

// ─── 来源 1：auth.json（加密）────────────────────────────────

/// 来源 1：从 `auth.json` 解出凭证（源实现 `fromAuthFile`）。
///
/// 关键行为（与源实现逐条对齐）：
///   - `token` / `refreshToken` 都是**可选**的字符串字段，两者全空时报
///     「AutoClaw auth.json 中没有 token」；
///   - **DPAPI 密钥取不到时不立刻失败**：只有当 `token` 确实以 `enc:` 开头才
///     把密钥错误升级成失败（明文 token 在这条路径上照样能用）——
///     源实现 `if (rawToken.startsWith('enc:')) throw` 的语义；
///   - 解密结果为空 → 「token 解密结果为空」（源实现同款）；
///   - `userId` 取 JWT 的 `user_id`，`expiresAt` 取 `exp` × 1000；
///   - `deviceId` 优先文件里的 `deviceId`，回落 JWT 的 `device_id`。
fn from_auth_file(region: Region) -> Result<AutoClawCredentials, String> {
    let Some(auth_path) = desktop_auth_file() else {
        return Err("AutoClaw 桌面端登录态仅支持 Windows".to_string());
    };
    let snapshot = read_json_file(&auth_path, "AutoClaw auth.json")?;
    // 缓存键带上**来源路径**：`AUTOCLAW_USER_DATA_DIR` 被改过、或换了一台机器上
    // 的同名文件时，mtime 有可能撞上，路径不同就不该复用同一格。
    //
    // ── 为什么还要带地区（本次修正的一处真 bug）────────────────────
    // 两地的 `auth.json` 是**同一个文件**（同一个目录、同一个路径、同一个
    // mtime），若键里不带地区，先解析的那一家会把凭证连同**它自己的 region**
    // 一起缓存进去，另一家读到后拿到的是「带着对方域名的凭证」——
    // 表现是转发打到错误站点、稳定 401，而且日志上完全看不出原因
    // （缓存命中，没有任何解析痕迹）。
    let cache_key = format!(
        "auth:{}:{}:{}",
        region.provider_id(),
        crate::server::core::providers::refresh_flight::fingerprint(&auth_path.to_string_lossy()),
        snapshot.modified_at
    );
    if let Some(cached) = lookup_cached(region, &cache_key) {
        return Ok(cached);
    }
    let text_field = |key: &str| -> String {
        snapshot
            .json
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let raw_token = text_field("token");
    let raw_refresh = text_field("refreshToken");
    let device_id = text_field("deviceId").trim().to_string();
    if raw_token.is_empty() && raw_refresh.is_empty() {
        return Err("AutoClaw auth.json 中没有 token".to_string());
    }
    let mut key_error: Option<String> = None;
    let aes_key = match local_state_file() {
        Some(local_state) => match crypto::os_crypt_aes_key(&local_state) {
            Ok(key) => Some(key),
            Err(reason) => {
                key_error = Some(reason);
                None
            }
        },
        None => {
            key_error = Some("AutoClaw 桌面端登录态仅支持 Windows".to_string());
            None
        }
    };
    // 加密 token 但没有密钥 → 用密钥那一步的错误（比「解密结果为空」更能说明问题）
    if aes_key.is_none() && raw_token.starts_with("enc:") {
        return Err(format!(
            "AutoClaw token 是加密存储且解密失败: {}",
            key_error.unwrap_or_else(|| "没有可用的 os_crypt 密钥".to_string())
        ));
    }
    let token = crypto::strip_bearer(&crypto::decrypt_enc_value(&raw_token, aes_key.as_deref())?);
    let refresh_token =
        crypto::strip_bearer(&crypto::decrypt_enc_value(&raw_refresh, aes_key.as_deref())?);
    if token.is_empty() {
        return Err("AutoClaw auth.json token 解密结果为空".to_string());
    }
    let claims = crypto::decode_jwt_claims(&token);
    let credentials = credentials_from_claims(
        DESKTOP_ACCOUNT_ID.to_string(),
        region,
        token,
        refresh_token,
        device_id,
        claims.as_ref(),
        CredentialOrigin::DesktopAuthFile,
        Some(cache_key.clone()),
    );
    // 解密结果进缓存：下一次同一 mtime 的请求不再做 DPAPI + AES-GCM
    store_cached(region, &cache_key, &credentials);
    Ok(credentials)
}

// ─── 来源 2：openclaw.json（明文 JWT）────────────────────────

/// 来源 2：从网关配置里取明文 JWT（源实现 `fromGatewayConfig`）。
///
/// 遍历 `models.providers.*.models[].headers`，取第一个非空的
/// `X-Authorization` / `x-authorization`（遍历顺序即源实现的 `for...of` 顺序：
/// providers 的插入序 → models 数组序 → headers 里的两个键名）。
/// 只有 access token，所以 `refresh_token` 恒为空 —— 也就意味着这条来源
/// **天然不可刷新**（`can_refresh()` 返回 false）。
fn from_gateway_config(region: Region) -> Result<AutoClawCredentials, String> {
    let path = gateway_config_file();
    let snapshot = read_json_file(&path, "AutoClaw 网关配置 openclaw.json")?;
    // 缓存键带上来源路径与地区（理由同 `from_auth_file`：两地共用同一个文件）
    let cache_key = format!(
        "gw:{}:{}:{}",
        region.provider_id(),
        crate::server::core::providers::refresh_flight::fingerprint(&path.to_string_lossy()),
        snapshot.modified_at
    );
    if let Some(cached) = lookup_cached(region, &cache_key) {
        return Ok(cached);
    }
    let providers = snapshot
        .json
        .get("models")
        .and_then(|models| models.get("providers"))
        .and_then(Value::as_object);
    let mut token = String::new();
    'outer: for provider in providers.into_iter().flat_map(|map| map.values()) {
        let models = match provider.get("models").and_then(Value::as_array) {
            Some(models) => models,
            None => continue,
        };
        for model in models {
            let headers = match model.get("headers").and_then(Value::as_object) {
                Some(headers) => headers,
                None => continue,
            };
            for name in ["X-Authorization", "x-authorization"] {
                if let Some(Value::String(candidate)) = headers.get(name) {
                    let normalized = crypto::strip_bearer(candidate);
                    if !normalized.is_empty() {
                        token = normalized;
                        break 'outer;
                    }
                }
            }
        }
    }
    if token.is_empty() {
        return Err("openclaw.json 中没有可用的 X-Authorization token".to_string());
    }
    let claims = crypto::decode_jwt_claims(&token);
    let credentials = credentials_from_claims(
        DESKTOP_ACCOUNT_ID.to_string(),
        region,
        token,
        String::new(),
        String::new(),
        claims.as_ref(),
        CredentialOrigin::GatewayConfig,
        Some(cache_key.clone()),
    );
    store_cached(region, &cache_key, &credentials);
    Ok(credentials)
}

/// 按 JWT claims 补齐一份凭证（`user_id` / `device_id` / `expires_at` 的口径
/// 在三个来源之间共用，避免各写一遍导致字段规则分叉）。
///
/// `region` 由**来源**决定（账号记录读它自己的 `provider`、桌面端文件与
/// 环境变量按「哪一家的调用方在问」传入），见 [`AutoClawCredentials::region`]。
pub(crate) fn credentials_from_claims(
    id: String,
    region: Region,
    token: String,
    refresh_token: String,
    device_id: String,
    claims: Option<&Value>,
    origin: CredentialOrigin,
    cache_key: Option<String>,
) -> AutoClawCredentials {
    let user_id = claims
        .and_then(|claims| claims.get("user_id"))
        .map(js_text)
        .unwrap_or_default();
    let expires_at = claims
        .and_then(|claims| claims.get("exp"))
        .and_then(number_value)
        .filter(|value| *value > 0.0)
        .map(|value| value * 1000.0);
    let device_id = if !device_id.is_empty() {
        device_id
    } else {
        claims
            .and_then(|claims| claims.get("device_id"))
            .map(js_text)
            .unwrap_or_default()
    };
    AutoClawCredentials {
        id,
        region,
        token,
        refresh_token,
        device_id,
        user_id,
        expires_at,
        origin,
        cache_key,
    }
}

/// JS `String(x)`（claims 里的数字 user_id 也要能取出来）
pub(crate) fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// JS `Number(x)`（只给 JWT 的 exp 用；非有限值当没有）
pub(crate) fn number_value(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

// ─── 对外：本地登录态（两个来源 + 优先级）────────────────────

/// 本地登录态凭证：**先 auth.json，失败回落 openclaw.json**（源实现
/// `getCredentials` 的优先级），两个都失败时抛出**auth.json 的错误**。
///
/// 同步函数：解密链（读文件 / DPAPI / AES-GCM）全是同步的，没有 await 点，
/// 所以调用它的 async 链路不会因此持有非 Send 状态。
///
/// ── 为什么只有国内版能用（本次新增地区时的硬判断）─────────────
/// 两个构建的 Electron 应用名都是 `autoclaw`、userData 都落在
/// `%APPDATA%/AutoClaw`（Windows 大小写不敏感，实测是同一个目录），而
/// `auth.json` 里**没有任何地区标记**。也就是说这个文件属于哪个地区，
/// 只取决于用户装的是哪个构建 —— 本机无法判断。
///
/// 处置是**不猜**：这个文件归国内版（历史行为，`autoclaw` 一直读它），
/// 国际版明确报错并指路（手机验证码登录 / 填写凭证，那两条把凭证落在账号
/// 记录里，与桌面端文件无关，因此两地可以并存）。反过来若让国际版也读它，
/// 一个只装了国际版客户端的用户会得到一个「国际版」账号却拿着国内版的域名
/// 去请求 —— 上游 401，而错误信息会把排查方向带偏到「token 过期」上。
/// 见 `region.rs` 模块头的完整讨论。
pub fn local_credentials(region: Region) -> Result<AutoClawCredentials, GatewayError> {
    if region != Region::Cn {
        return Err(GatewayError::with_status(
            401,
            "AutoClaw 桌面端登录态文件（auth.json）没有地区标记，只归国内版使用；\
             国际版请用「手机验证码登录」或「填写凭证」添加账号",
        ));
    }
    match from_auth_file(region) {
        Ok(credentials) => Ok(credentials),
        Err(auth_file_error) => match from_gateway_config(region) {
            Ok(credentials) => Ok(credentials),
            Err(_) => Err(GatewayError::with_status(
                401,
                format!("AutoClaw 桌面端登录态不可用：{auth_file_error}"),
            )),
        },
    }
}

/// 环境变量凭证（源实现 `envCredentials`）：`{prefix}TOKEN` 必填，
/// `{prefix}REFRESH_TOKEN` / `{prefix}DEVICE_ID` 可选；未配置时 None。
///
/// 前缀由地区决定（国内 `AUTOCLAW_` / 国际 `AUTOCLAW_INTL_`，见
/// [`Region::env_prefix`]）—— 两地共用一个变量名会让「只想给国际版配一个
/// token」变成「两地一起改」。
///
/// 优先级在源实现里是「账号列表选中项 > 环境变量」，由适配器负责排序
/// （本函数只回答「环境变量里有没有可用凭证」）。
pub fn env_credentials(region: Region) -> Option<AutoClawCredentials> {
    let token = region.env_override("TOKEN")?;
    let token = crypto::strip_bearer(&token);
    if token.is_empty() {
        return None;
    }
    let refresh_token = region
        .env_override("REFRESH_TOKEN")
        .map(|value| crypto::strip_bearer(&value))
        .unwrap_or_default();
    let device_id = region.env_override("DEVICE_ID").unwrap_or_default();
    let claims = crypto::decode_jwt_claims(&token);
    Some(credentials_from_claims(
        "env".to_string(),
        region,
        token,
        refresh_token,
        device_id,
        claims.as_ref(),
        CredentialOrigin::Environment,
        None,
    ))
}

/// 从 accounts.json 的一条账号记录构造凭证（手动添加 / 旧数据导入的账号）。
///
/// 记录里的 token 允许两种形态（源实现 `account-store.mjs` 的 `upload`）：
///   - 明文 token；
///   - **`enc:` 密文**（用户直接粘贴 auth.json 的原值）—— 自动走解密链。
///
/// `desktop: true` 的记录（桌面端实时登录态）**不走本函数**：那种账号的凭证
/// 按设计不落 accounts.json，应由调用方改调 `local_credentials`
/// （`snapshot_for` 已把这层判断收进去了）。
///
/// ── `region` 的取值口径 ─────────────────────────────────────
/// 优先读记录自己的 `provider` 字段（那才是这条账号属于哪一家的**事实**），
/// 读不出来才用调用方给的 `fallback`。两者不一致时以记录为准 —— 传参只是
/// 「调用方以为它属于谁」，而记录里的 provider 是落盘契约。
pub fn credentials_from_record(
    record: &Value,
    fallback: Region,
) -> Result<AutoClawCredentials, GatewayError> {
    let region = record
        .get("provider")
        .and_then(Value::as_str)
        .and_then(Region::from_provider_id)
        .unwrap_or(fallback);
    // 源实现的取法是「token 非空就用 token，否则看 accessToken」——
    // **不是** `token ?? accessToken`：`token` 存在但是空串/非字符串时也要落到
    // `accessToken`（账号记录被手工编辑过就是这个形态），
    // 用 `.or_else` 链只会在键缺失时回落，那种记录会被误判成「没有 token」。
    let pick = |key: &str| -> String {
        record
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let mut raw_token = pick("token");
    if raw_token.is_empty() {
        raw_token = pick("accessToken");
    }
    let raw_refresh = pick("refreshToken");
    if raw_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "AutoClaw 账号缺少 token，请重新添加",
        ));
    }
    let needs_decrypt = raw_token.starts_with("enc:") || raw_refresh.starts_with("enc:");
    let aes_key = if needs_decrypt {
        let key = local_state_file()
            .ok_or_else(|| GatewayError::with_status(401, "AutoClaw 加密凭证仅支持 Windows"))?;
        let key = crypto::os_crypt_aes_key(&key).map_err(|reason| {
            GatewayError::with_status(401, format!("AutoClaw 加密凭证解密失败：{reason}"))
        })?;
        Some(key)
    } else {
        None
    };
    let token = crypto::strip_bearer(
        &crypto::decrypt_enc_value(&raw_token, aes_key.as_deref())
            .map_err(|reason| GatewayError::with_status(401, reason))?,
    );
    let refresh_token = crypto::strip_bearer(
        &crypto::decrypt_enc_value(&raw_refresh, aes_key.as_deref())
            .map_err(|reason| GatewayError::with_status(401, reason))?,
    );
    if token.is_empty() {
        return Err(GatewayError::with_status(401, "AutoClaw 账号 token 为空"));
    }
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            let claims = crypto::decode_jwt_claims(&token);
            let user_id = claims
                .as_ref()
                .and_then(|claims| claims.get("user_id"))
                .map(js_text)
                .unwrap_or_default();
            format!("user-{user_id}")
        });
    let device_id = record
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let claims = crypto::decode_jwt_claims(&token);
    let mut credentials = credentials_from_claims(
        id,
        region,
        token,
        refresh_token,
        device_id,
        claims.as_ref(),
        CredentialOrigin::AccountStore,
        None,
    );
    // 账号记录里的过期时间优先（可能是上次刷新后写回的值，比 JWT 更贴近实际）
    if let Some(expires_at) = record.get("tokenExpiresAt").and_then(number_value) {
        credentials.expires_at = Some(expires_at);
    }
    Ok(credentials)
}

/// 凭证快照（三个来源的**统一入口**，适配器只调这一个函数）。
///
/// `record` 语义：
///   - `None` → 本地实时登录态（`local_credentials`）没有时回落环境变量；
///   - `Some(record)` 且 `desktop: true` 或 id 为 `desktop-auth` → 本地实时登录态
///     （凭证不落 accounts.json，见架构文档 §3.2 的桌面态约定）；
///   - `Some(record)` 其余 → 记录里的 token（含 `enc:` 自动解密）。
///
/// `region` 是**调用方问的是哪一家**：记录形态下以记录自己的 `provider` 为准
/// （见 [`credentials_from_record`]），只有「无记录」的本地 / 环境变量来源
/// 才真正按这个参数取域名与变量前缀。
pub fn snapshot_for(
    record: Option<&Value>,
    region: Region,
) -> Result<AutoClawCredentials, GatewayError> {
    let Some(record) = record else {
        return local_credentials(region)
            .or_else(|error| env_credentials(region).ok_or(error));
    };
    let is_desktop = record
        .get("desktop")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || record.get("id").and_then(Value::as_str) == Some(DESKTOP_ACCOUNT_ID);
    if is_desktop {
        return local_credentials(region);
    }
    credentials_from_record(record, region)
}

/// 本地登录态的**摘要**（账号导入与状态展示用；**不含 token 本身**）。
///
/// 字段对照源实现 `account-store.mjs` 的 `desktopEntry()`：
/// `userId` / `tokenTail` / `tokenExpiresAt` / `hasRefreshToken` / `source`，
/// 另加 `mtime`（`updatedAt` 用）与 `canRefresh`（其实等于 hasRefreshToken，
/// 保留它是因为前端状态接口用的是这个名字）。
///
/// **两个地区都能调**：这个文件没有地区标记，两地的调用方各自传入自己那一项
/// （见 [`local_credentials`]）。返回里带 `region`，让调用方能确认拿到的是
/// 哪一家的凭证。
pub fn local_summary(region: Region) -> Result<Value, String> {
    let credentials = local_credentials(region).map_err(|error| error.message)?;
    Ok(json!({
        "ok": true,
        // 这里是**凭证对象**的 id（`desktop-auth`），不是账号记录的 id ——
        // 账号记录那边按地区取 `autoclaw-desktop` / `autoclaw-intl-desktop`
        // （见 `account_store::autoclaw_accounts`）。两者不是一回事，见 mod.rs
        // 的「两个 id 别搞混」。
        "id": DESKTOP_ACCOUNT_ID,
        "region": region.provider_id(),
        "userId": credentials.user_id,
        "tokenTail": token_tail(&credentials.token),
        "tokenExpiresAt": credentials
            .expires_at
            .map(crate::server::core::account_store::state::json_number)
            .unwrap_or(Value::Null),
        "hasRefreshToken": !credentials.refresh_token.is_empty(),
        "canRefresh": credentials.can_refresh(),
        "deviceId": credentials.device_id,
        "source": match credentials.origin {
            CredentialOrigin::DesktopAuthFile => "autoclaw-auth-file",
            CredentialOrigin::GatewayConfig => "autoclaw-gateway-config",
            CredentialOrigin::AccountStore => "account-store",
            CredentialOrigin::Environment => "environment",
        },
    }))
}

/// token 尾 4 字符（源实现 `credentials.token.slice(-4)`）
fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
