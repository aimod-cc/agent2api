//! Clash Verge 配置读取与快照缓存（对照 Node 版 src/workbuddy-proxy.mjs 的 Clash 部分）。
//!
//! 为什么单独一个模块：账号代理的解析（proxies.rs）只依赖「快照形状」，
//! 而快照自身的读取涉及三个候选目录、两个 YAML 文件与一层 TTL 缓存，
//! 属于「外部环境探测」，与归一/解析这类纯逻辑混在一起会让两边都难改。
//!
//! 关键设计：Clash Verge 侧的代理项是**严格镜像** —— 端口只在 Clash Verge 里改，
//! 这里每次都从 verge.yaml 实时读取，账号只记 listenerUid。Clash 里删掉了某个
//! 监听器，引用它的账号会解析失败并回退直连（提示由 describe/resolve 给出）。
//!
//! 缓存：解析账号代理要读 verge.yaml，桌面端列表也频繁刷新，因此加一层 3 秒
//! TTL 缓存 —— Clash 改端口最多延迟 3 秒生效，与 Node 版一致。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use serde_yaml::Value as Yaml;

use crate::server::logging;

/// Clash Verge 全局混合端口在账号配置里的保留 uid（与真实节点名不会冲突）
pub const CLASH_MIXED_UID: &str = "__mixed__";

/// Clash 配置未找到时的原因文案
/// （与「用户机器上没装 Clash Verge」时 Node 版的文案逐字一致）
pub const CLASH_UNAVAILABLE: &str = "未找到 Clash Verge 配置";

/// Clash Verge 的应用目录名（Windows 下 `%APPDATA%\<这个名字>`）
const CLASH_APP_ID: &str = "io.github.clash-verge-rev.clash-verge-rev";

/// 快照缓存 TTL：Clash 改端口最多延迟 3 秒生效（对照 Node 版 CLASH_CACHE_TTL_MS）
const CLASH_CACHE_TTL_MS: i64 = 3000;

/// Clash Verge 监听器快照。
///
/// 字段与 Node 版 `readClashVergeConfig()` 的返回一一对应
/// （available / dir / mixedPort / socksPort / httpPort / listeners /
/// currentProfileUid / error）。
#[derive(Clone, Debug)]
pub struct ClashSnapshot {
    pub available: bool,
    pub dir: Option<String>,
    pub mixed_port: Option<u16>,
    /// Node 版 `readClashVergeConfig()` 返回的 `socksPort` 对等字段：
    /// 代理下拉只用到 mixedPort / listeners，这两个端口保留以维持快照形态一致
    #[allow(dead_code)]
    pub socks_port: Option<u16>,
    /// Node 版 `readClashVergeConfig()` 返回的 `httpPort` 对等字段（同上）
    #[allow(dead_code)]
    pub http_port: Option<u16>,
    pub listeners: Vec<ClashListener>,
    pub current_profile_uid: Option<String>,
    pub error: Option<String>,
}

impl ClashSnapshot {
    /// 「找不到配置」快照（三个候选目录都没有 verge.yaml 时的形态）
    fn unavailable(error: &str, dir: Option<PathBuf>) -> Self {
        Self {
            available: false,
            dir: dir.map(|path| path.to_string_lossy().to_string()),
            mixed_port: None,
            socks_port: None,
            http_port: None,
            listeners: Vec::new(),
            current_profile_uid: None,
            error: Some(error.to_string()),
        }
    }
}

/// Clash Verge 的一个混合监听器
#[derive(Clone, Debug)]
pub struct ClashListener {
    pub uid: String,
    pub name: String,
    /// Node 版 `readClashVergeConfig()` 的 listeners[] 项里的 `proxy` 字段对等物
    /// （构造时用于 name/uid 兜底，之后不再读；保留以维持监听器形态一致）
    #[allow(dead_code)]
    pub proxy: String,
    pub port: u16,
    pub profile_uid: Option<String>,
    pub enabled: bool,
}

// ─── 配置目录与 YAML 读取 ───────────────────────────────────

/// 用户主目录。
///
/// Node 的 `os.homedir()` 在 Windows 上优先 USERPROFILE；这里按同一顺序取，
/// 与壳侧 gateway::config_dir 的做法保持一致（不引入 dirs crate）。
fn home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Clash Verge 配置目录：候选顺序照抄 Node 版 `clashConfigDir()`。
///
///   ① `%APPDATA%\<appId>`（Windows 的常规位置）
///   ② `~/AppData/Roaming/<appId>`（APPDATA 缺失时的回落，兼容 Linux 上跑 Wine / 容器）
///   ③ `~/.config/<appId>`（macOS / Linux）
///
/// 判定标准是「目录里存在 verge.yaml」——只判目录存在会把 Clash 刚装好、
/// 还没写配置的情况误判成可用。
pub fn clash_config_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        let trimmed = appdata.trim();
        if !trimmed.is_empty() {
            candidates.push(Path::new(trimmed).join(CLASH_APP_ID));
        }
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join("AppData").join("Roaming").join(CLASH_APP_ID));
        candidates.push(home.join(".config").join(CLASH_APP_ID));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("verge.yaml").exists())
}

/// 读一个 YAML 文件；缺失/解析失败/顶层不是映射都返回 None
/// （对应 Node 版 `readYaml` 的 `parsed && typeof parsed === 'object' && !Array.isArray(parsed)`）。
///
/// 刻意吞掉错误：Clash 的配置文件可能正被 Clash 自己写（读到半截），
/// 这时应当视为「本轮读不到」，而不是把异常抛给调用方 —— Node 版同样如此。
fn read_yaml(path: &Path) -> Option<Yaml> {
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: Yaml = serde_yaml::from_str(&text).ok()?;
    match parsed {
        Yaml::Mapping(_) => Some(parsed),
        _ => None,
    }
}

/// YAML 里的端口值 → 端口号。
///
/// 对照 Node 的 `isValidPort(value)`：`Number(value)` 后必须是 1..65535 的整数。
/// 因此数字与数字字符串都接受（Clash 的 verge.yaml 有时写成 `"7897"`），
/// 但布尔/映射这类会被 `Number()` 转成怪异值的类型不接受 —— 端口只可能来自
/// Clash 自己写入的配置，这里不做超纲的宽容。
fn yaml_port(value: Option<&Yaml>) -> Option<u16> {
    let number = match value? {
        Yaml::Number(number) => number.as_f64()?,
        Yaml::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !number.is_finite() || number.fract() != 0.0 || !(1.0..=65535.0).contains(&number) {
        return None;
    }
    Some(number as u16)
}

/// 取 YAML 字符串并去空白（非字符串一律当空串）
fn yaml_text(value: Option<&Yaml>) -> String {
    match value {
        Some(Yaml::String(text)) => text.trim().to_string(),
        _ => String::new(),
    }
}

/// 读取 Clash Verge 的监听器快照。
///
/// Clash 未安装或配置不可读时 `available=false`，不抛异常 —— 这是正常状态
/// （绝大多数用户机器上就是没装 Clash）。三种不可用形态与 Node 版逐字一致：
///   ① 三个候选目录里都没有 verge.yaml → dir=null + error=未找到 Clash Verge 配置
///   ② verge.yaml 解析失败 → dir 有值 + error=verge.yaml 解析失败
///   ③ 正常读取 → error=null + 监听器列表（可能是空的）
pub fn read_clash_verge_config() -> ClashSnapshot {
    let Some(dir) = clash_config_dir() else {
        return ClashSnapshot::unavailable(CLASH_UNAVAILABLE, None);
    };
    let Some(verge) = read_yaml(&dir.join("verge.yaml")) else {
        return ClashSnapshot::unavailable("verge.yaml 解析失败", Some(dir));
    };

    // 手动配置的监听器：每一项形如
    //   - { uid, name, port, proxy, enabled, profile_uid }
    // 脏项（缺 proxy / 端口非法 / 非映射）一律跳过，而不是让整个快照失败 ——
    // 一个坏项不该让其它可用出口从下拉里消失。
    let mut listeners: Vec<ClashListener> = Vec::new();
    if let Some(items) = verge.get("verge_mixed_listeners").and_then(Yaml::as_sequence) {
        for item in items {
            let Some(mapping) = item.as_mapping() else {
                continue;
            };
            let proxy = yaml_text(mapping.get("proxy"));
            if proxy.is_empty() {
                continue;
            }
            let Some(port) = yaml_port(mapping.get("port")) else {
                continue;
            };
            // uid 缺失时用「端口:proxy」兜底（Node 版同样如此），保证下拉项仍有稳定标识
            let uid = {
                let text = yaml_text(mapping.get("uid"));
                if text.is_empty() {
                    format!("{port}:{proxy}")
                } else {
                    text
                }
            };
            let name = {
                let text = yaml_text(mapping.get("name"));
                if text.is_empty() {
                    proxy.clone()
                } else {
                    text
                }
            };
            let profile_uid = {
                let text = yaml_text(mapping.get("profile_uid"));
                if text.is_empty() { None } else { Some(text) }
            };
            listeners.push(ClashListener {
                uid,
                name,
                proxy,
                port,
                profile_uid,
                // Node 是 `item.enabled !== false`：只有显式 false 才算禁用
                enabled: !matches!(mapping.get("enabled"), Some(Yaml::Bool(false))),
            });
        }
    }

    // profiles.yaml 只为拿「当前激活订阅」：监听器属于其它订阅时端口可能未生效，
    // 前端据此在下拉项上标注「（其他订阅，可能未生效）」
    let current_profile_uid = read_yaml(&dir.join("profiles.yaml")).and_then(|profiles| {
        let current = yaml_text(profiles.get("current"));
        if current.is_empty() { None } else { Some(current) }
    });

    ClashSnapshot {
        available: true,
        dir: Some(dir.to_string_lossy().to_string()),
        mixed_port: yaml_port(verge.get("verge_mixed_port")),
        socks_port: yaml_port(verge.get("verge_socks_port")),
        http_port: yaml_port(verge.get("verge_port")),
        listeners,
        current_profile_uid,
        error: None,
    }
}

// ─── 快照缓存 ───────────────────────────────────────────────

/// 进程内快照缓存：`(取快照的时刻, 快照)`。
///
/// 只缓存**读到的结果**（包括「不可用」这种结果），这样 Clash 没装的机器上
/// 也不会每次请求都去 stat 三个目录。持锁只做本地文件读取，不做网络 IO。
static CLASH_CACHE: Mutex<Option<(i64, ClashSnapshot)>> = Mutex::new(None);

/// 取快照（默认走 3 秒 TTL 缓存）
pub fn clash_snapshot() -> ClashSnapshot {
    clash_snapshot_with(false)
}

/// 取快照，`force=true` 时跳过缓存（前端「重新读取」按钮用得上）。
///
/// 与 Node 版 `clashSnapshot({ ttlMs, force })` 的差别：Node 还允许自定义 TTL，
/// 而 Rust 侧没有任何调用方需要改 TTL，故只保留 force 一个开关。
pub fn clash_snapshot_with(force: bool) -> ClashSnapshot {
    let now = logging::now_ms();
    // 锁中毒（某次持锁 panic）不致命：接管内部数据继续用，总好过整个 Clash
    // 读取链路永久失败
    let mut guard = match CLASH_CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !force {
        if let Some((at, snapshot)) = guard.as_ref() {
            if now - *at < CLASH_CACHE_TTL_MS {
                return snapshot.clone();
            }
        }
    }
    let snapshot = read_clash_verge_config();
    *guard = Some((now, snapshot.clone()));
    snapshot
}

/// Clash 代理可选项（GET /api/proxies 与 /api/session 用），对照 Node 版 `clashProxyOptions`。
///
/// 不可用分支返回 `{available:false, error, dir, options:[]}` —— options 是空数组
/// 而不是缺失，前端 proxy-form.js 直接读 `.options.length`。
///
/// `profileActive` 表示该监听器所属订阅就是当前激活订阅（端口实际生效）：
/// profile_uid 缺失时视为 true（老版本 Clash 不写这个字段，不能一律判成失效）。
pub fn clash_proxy_options() -> Value {
    let snapshot = clash_snapshot();
    if !snapshot.available {
        return json!({
            "available": false,
            "error": snapshot.error,
            "dir": snapshot.dir,
            "options": [],
        });
    }
    let mut options = Vec::new();
    if let Some(port) = snapshot.mixed_port {
        options.push(json!({
            "uid": CLASH_MIXED_UID,
            "name": "Clash 混合端口（按规则分流）",
            "port": port,
            "kind": "mixed",
            "enabled": true,
            "profileActive": true,
        }));
    }
    for listener in &snapshot.listeners {
        let active = listener
            .profile_uid
            .as_ref()
            .map(|uid| Some(uid) == snapshot.current_profile_uid.as_ref())
            .unwrap_or(true);
        options.push(json!({
            "uid": listener.uid,
            "name": listener.name,
            "port": listener.port,
            "kind": "listener",
            "enabled": listener.enabled,
            "profileActive": active,
        }));
    }
    json!({
        "available": true,
        "error": Value::Null,
        "dir": snapshot.dir,
        "options": options,
    })
}
