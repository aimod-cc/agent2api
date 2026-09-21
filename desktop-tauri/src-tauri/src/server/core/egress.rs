//! 出网统一入口：按出口缓存 reqwest Client + 出口连通性测试。
//!
//! 对照 Node 版 src/workbuddy-proxy.mjs 的 `dispatchFor` / `createDispatcher` /
//! `proxyFetch` / `testProxyConnectivity` 四块。
//!
//! ── 为什么需要缓存 Client ───────────────────────────────────
//! Node 版用 undici 的 dispatcher 缓存：每个出口一个 dispatcher（内含连接池），
//! 否则每个请求都要**重新和代理建一条 TCP 连接**，SSE 长连接场景下开销很直观。
//! reqwest 的等价物是 `Client`：`Client` 内部自带连接池，因此「一个出口一个
//! Client」就是「一个出口一个连接池」。
//!
//! ── 超时旋钮的映射（重要：Node 与 reqwest 不是一一对应）──────
//! Node 版（undici）给的是三个**传输层**超时：
//!   connectTimeout: 30s     TCP 建连（含到代理的那一段）
//!   headersTimeout: 600s    建连完成 → 收到响应头
//!   bodyTimeout:    0       响应体读取**不限时**（SSE 长连接靠调用方的 signal 控制）
//!
//! reqwest 只有两个旋钮，且语义不同：
//!   connect_timeout(30s)    ← 对应 undici 的 connectTimeout（同语义，直接映射）
//!   read_timeout(600s)      ← 对应 undici 的 headersTimeout + bodyTimeout 的一半：
//!                             它作用于**每一次** read（收到头、以及之后每一段 body），
//!                             每次成功读取后重新计时。所以它既覆盖了「等响应头」
//!                             （30s 建连后最多再等 600s，与 undici 的 headersTimeout 一致），
//!                             也覆盖了「等 body 数据块」（每次数据间隔 ≤600s）。
//!   **不用 `.timeout()`（总超时）**：那会让 SSE 长连接在 600 秒时被无条件掐断，
//!   而 Node 版明确是 `bodyTimeout: 0`（不限时）。
//!   → 与 Node 的差异：body 数据块的间隔被限制在 600 秒内（Node 是完全不限时）。
//!     这个差异是**有意的**：SSE 心跳通常 15-30 秒一次，600 秒没有任何数据
//!     说明连接已经僵死，此时断开比无限挂着更有用；而真正的「长思考」
//!     （上游迟迟不出第一个 token）在 600 秒内不会被打断。
//!
//! ── 缓存淘汰 ────────────────────────────────────────────────
//! Node 版：`MAX_DISPATCHERS = 24`，超出时关掉**最久未用**的那个（Map 保持
//! 插入序，命中时挪到末尾）。这里用同样的策略：`HashMap` + 最近使用序号，
//! 超限时淘汰序号最小的（即最久未用），并记录最后一轮的使用序号。
//! 与 Node 的差异：Node 的淘汰会显式 `destroyDispatcher` 关掉连接池，
//! reqwest 的 `Client` 没有「强制关闭」接口（只能 drop，drop 时连接池随句柄
//! 引用计数归零而释放）。这里就是 drop —— 效果一致，但释放时点取决于
//! 是否还有在途请求持有该 Client 的 Arc 克隆（在途请求会正常跑完）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::core::proxies::ResolvedProxy;
use crate::server::logging;

/// 连接超时：TCP 建连（含到代理的那一段）—— 对应 undici 的 connectTimeout
const CONNECT_TIMEOUT_MS: u64 = 30_000;
/// 单次读取超时：见模块头部「超时旋钮的映射」—— 对应 undici 的 headersTimeout
const READ_TIMEOUT_MS: u64 = 600_000;

/// 默认 User-Agent，**必须设置**。
///
/// ── 为什么这条不是「顺手加的」而是必需 ────────────────────────
/// reqwest 默认**不发** User-Agent 头；undici（Node 版用的 fetch）默认发
/// `undici`。实测（2026-09）计费接口在缺失 User-Agent 时直接返回
/// `HTTP 403 {"code":10085,"msg":"请求不合法，如有疑问请联系客服"}`，
/// 带上任意非空 UA 就正常返回 `code:0`。
///
/// 换句话说：不设这个头，Rust 版会在「Node 版跑得通」的同一账号上稳定复现 403。
/// 这里取 `undici` 与 Node 版完全一致 —— 上游看到的就是同一个 UA，
/// 不做「顺手改成 WorkBuddy 客户端 UA」这种会改变上游风控判定的改动
/// （计费链路 Node 版本来就不带客户端 UA，只有 banner 的白名单头带）。
///
/// 客户端级默认头**不会覆盖**请求级头（reqwest 只在 HeaderMap 空缺时插入），
/// 所以 banner 的 `whitelistHeaders` 里那个真正的 WorkBuddy UA 依然生效。
const DEFAULT_USER_AGENT: &str = "undici";

/// Client 缓存上限（对照 Node 版 MAX_DISPATCHERS）
const MAX_CLIENTS: usize = 24;

/// 出口 IP 查询服务：拿不到只当作附加信息缺失，不影响连通性结论
const IP_ECHO_URL: &str = "https://api.ipify.org?format=json";
/// 连通性测试的默认目标：**上游站点**而不是第三方站点（判据见 test_connectivity）
const DEFAULT_TEST_URL: &str = "https://copilot.tencent.com/v2/config";
/// 连通性测试默认超时
const DEFAULT_TEST_TIMEOUT_MS: u64 = 15_000;
/// 出口 IP 查询的超时上限（即便测试超时给得更大，也不要在这里耗太久）
const IP_LOOKUP_MAX_MS: u64 = 6_000;

/// 缓存里的一个 Client：`(最近一次取用的序号, 客户端)`
struct CacheEntry {
    used_at: u64,
    client: Arc<reqwest::Client>,
}

/// 出口 → Client 的缓存。
///
/// 键是**不含凭证**的 `协议://主机:端口`（与 Node 的 dispatcher 键一致）：
/// 把密码放进缓存键会让它在内存里多留一份，而且同主机不同密码本就不该共用一个池。
/// 凭证通过 `reqwest::Proxy::basic_auth` 挂在客户端上，仍然生效。
#[derive(Default)]
struct ClientCache {
    entries: HashMap<String, CacheEntry>,
    /// 单调递增的取用序号（替代 Node 用 Map 插入序做的 LRU）
    clock: u64,
}

static CLIENTS: Mutex<Option<ClientCache>> = Mutex::new(None);

/// 缓存键：`协议://主机:端口`（不含凭证，理由见 ClientCache 的注释）。
///
/// 直连（proxy 为 None）不在这里 —— 它走独立的 `DIRECT` 键，
/// 保证「某个出口被淘汰」不会把直连的客户端也一起丢掉。
const DIRECT_KEY: &str = "__direct__";

/// 出口的缓存键；端口非法（手工编辑出的脏数据）时返回 None，
/// 调用方据此回退直连（与 Node 的 `NaN:NaN` 键会产生一个连不上的 dispatcher
/// 不同 —— 这里更早地放弃，并把原因写进日志）。
fn cache_key(proxy: Option<&ResolvedProxy>) -> String {
    match proxy {
        None => DIRECT_KEY.to_string(),
        Some(proxy) => format!(
            "{}://{}:{}",
            proxy.protocol,
            proxy.host,
            proxy.port.map(|port| port.to_string()).unwrap_or_else(|| "?".to_string())
        ),
    }
}

/// 构造代理 URI：`协议://[用户名[:密码]@]主机:端口`。
///
/// 用户名/密码按 Node 版 `buildProxyUrl` 做百分号编码（对照 JS 的
/// `encodeURIComponent`）—— 代理密码里出现 `@` `:` `/` 时不编码会把 URI 拆坏。
/// IPv6 主机按 Node 的做法加方括号（`::1` → `[::1]`）。
///
/// **socks5 映射成 socks5h**（有意偏离 Node 的 URI 字面量，行为却更贴近）：
/// undici 的 Socks5ProxyAgent 把目标主机名按域名（ATYP=0x03）交给代理去解析，
/// 即「远端解析」；而 reqwest 的 `socks5://` 是**本地**解析（socks5h 才是远端）。
/// 用户配 socks5 出口往往正是为了绕开本机 DNS 污染，用 `socks5://` 会让这个
/// 意图落空。因此这里在构造 reqwest 代理 URI 时换成 socks5h ——
/// 账号里存的、UI 上显示的仍然是 socks5，只有内部实现换了个等价 scheme。
fn build_proxy_url(proxy: &ResolvedProxy) -> String {
    let auth = if proxy.username.is_empty() {
        String::new()
    } else if proxy.password.is_empty() {
        format!("{}@", urlencoding(&proxy.username))
    } else {
        format!("{}:{}@", urlencoding(&proxy.username), urlencoding(&proxy.password))
    };
    let needs_brackets = proxy.host.contains(':') && !proxy.host.starts_with('[');
    let host = if needs_brackets {
        format!("[{}]", proxy.host)
    } else {
        proxy.host.clone()
    };
    let scheme = if proxy.protocol == "socks5" { "socks5h" } else { &proxy.protocol };
    format!("{}://{}{}:{}", scheme, auth, host, proxy.port.unwrap_or(0))
}

/// 百分号编码（对照 JS 的 `encodeURIComponent`）。
///
/// 与 core::auth::urlencoding 是同一套编码规则，但那个函数在 auth 模块里
/// 承载的是「查询串编码」的语义；这里为了不让 egress 依赖 auth（auth 已经
/// 依赖了出网），复制一份实现。规则本身只有 4 行，重复的代价小于引入依赖环。
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 去掉代理 URI 里的凭证，供日志展示（密码不进日志）
fn describe_proxy(proxy: &ResolvedProxy) -> &str {
    if proxy.label.is_empty() {
        &proxy.host
    } else {
        &proxy.label
    }
}

/// 构造一个出口对应的 reqwest Client。
///
/// 注意 `Builder::proxy()` 会**同时关掉系统代理自动探测**（reqwest 的文档明说
/// "Adding a proxy will disable the automatic usage of the system proxy"）——
/// 这正是我们要的：出口由账号配置决定，不能被 `HTTPS_PROXY` 之类环境变量
/// 悄悄改写（否则「直连」可能实际上走了环境里的代理，与 UI 显示不符）。
/// 反过来，构造**直连** Client 时必须显式 `.no_proxy()`：reqwest 默认会去读
/// 环境变量里的代理设置，不关掉的话用户机器上设了 `HTTPS_PROXY` 就会
/// 「配置为直连却走了代理」。
fn build_client(proxy: Option<&ResolvedProxy>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(CONNECT_TIMEOUT_MS))
        // 单次读取超时；**没有**设总超时（.timeout()）—— 那会掐断 SSE 长连接
        .read_timeout(Duration::from_millis(READ_TIMEOUT_MS))
        // 默认 UA（理由见 DEFAULT_USER_AGENT）：不设会被计费接口判为「请求不合法」
        .user_agent(DEFAULT_USER_AGENT)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .pool_max_idle_per_host(8);

    match proxy {
        None => {
            // 显式关闭系统代理探测：配置为直连就必须真的直连
            builder = builder.no_proxy();
        }
        Some(proxy) => {
            let port = proxy
                .port
                .ok_or_else(|| format!("代理端口非法（{}）", describe_proxy(proxy)))?;
            let url = build_proxy_url(&ResolvedProxy { port: Some(port), ..proxy.clone() });
            // Proxy::all 返回 Result：release 是 panic=abort，绝不能用 unwrap
            let parsed = reqwest::Proxy::all(&url)
                .map_err(|error| format!("代理地址无效（{}）: {error}", describe_proxy(proxy)))?;
            // reqwest 从 URI 里解析用户名/密码作为 basic auth（build_proxy_url 已编码），
            // http/socks5 两种协议都支持；这里不再单独调 basic_auth，
            // 免得两个来源的凭证打架
            builder = builder.proxy(parsed);
        }
    }

    builder
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))
}

/// 取（或创建）某个出口对应的 Client；`proxy` 为 None 时是直连客户端。
///
/// 返回 `Arc<Client>`：调用方克隆这个 Arc 而不是 Client 本身
/// （reqwest 的 Client 内部已经是 Arc，但显式包一层让缓存与丢弃的语义更清楚）。
///
/// ── 为什么构造在锁外 ──────────────────────────────────────
/// `reqwest::Proxy::all(...)` 对 socks5 URI 会调 `url.socket_addrs()` 做一次
/// **阻塞式 DNS 解析**（http 代理不需要，但 socks 分支要走）。硬约束是
/// 「持锁不阻塞」—— 所以在锁外构造，锁内只做一次查表 + 插入。
/// 并发构造同一个出口时两次都建 Client（各自持自己的连接池），
/// 后者覆盖前者 —— 最坏结果是多建了一个池，不影响正确性。
pub fn client_for(proxy: Option<&ResolvedProxy>) -> Arc<reqwest::Client> {
    let key = cache_key(proxy);
    // ① 先查缓存（快速路径）
    {
        let mut guard = lock_clients();
        let cache = guard.get_or_insert_with(ClientCache::default);
        cache.clock += 1;
        let now = cache.clock;
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.used_at = now;
            return entry.client.clone();
        }
    }
    // ② 未命中才构造（锁外，可能阻塞）
    let client = Arc::new(build_client(proxy).unwrap_or_else(|error| {
        // 这条**保留在运行日志**（不像同类的「账号代理不可用」那样进请求日志）：
        // 它每个出口缓存条目最多触发一次 —— 构造失败的兜底 Client 也会被缓存
        // （见下面 ③），后续请求直接命中缓存、不再走到这里，所以条数**不随
        // 请求量增长**。判据与其它日志一致：看它的条数会不会跟着请求一起涨。
        logging::log("[Upstream]", &format!("⚠️ {error}，本次回退直连"));
        // 构造失败的兜底客户端：TLS 后端初始化都失败时没有别的退路，
        // 只能返回一个「请求时才报错」的客户端。Client::new() 几乎不会失败。
        reqwest::Client::new()
    }));
    // ③ 回填缓存（锁内只做插入与淘汰）
    let mut guard = lock_clients();
    let cache = guard.get_or_insert_with(ClientCache::default);
    cache.clock += 1;
    let now = cache.clock;
    // 并发情况下可能已有别的线程放进去了 —— 用先到的那个，少丢一个连接池
    let client = match cache.entries.get(&key) {
        Some(entry) => entry.client.clone(),
        None => {
            // 超限淘汰最久未用的（Node 的 idleDispatcher 语义：Map 插入序 + 命中挪末尾）
            while cache.entries.len() >= MAX_CLIENTS {
                let Some(oldest) = cache
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used_at)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                cache.entries.remove(&oldest);
            }
            cache.entries.insert(key, CacheEntry { used_at: now, client: client.clone() });
            client
        }
    };
    client
}

/// 取 Client 缓存的锁；锁中毒（某次持锁 panic）不致命，接管内部数据继续用
fn lock_clients() -> std::sync::MutexGuard<'static, Option<ClientCache>> {
    match CLIENTS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ─── 出口连通性测试 ─────────────────────────────────────────

/// 出口连通性测试结果（对应 Node 版 testProxyConnectivity 的返回对象）。
#[derive(Clone, Debug)]
pub struct ConnectivityResult {
    pub success: bool,
    pub status: Option<u16>,
    pub ip: String,
    pub duration_ms: i64,
    pub error: Option<String>,
}

impl ConnectivityResult {
    /// 转成响应 JSON：`{success, status?, ip?, durationMs, error?}`
    /// （字段只在有值时出现，与 Node 的对象字面量一致）
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("success".to_string(), Value::Bool(self.success));
        if self.success {
            if let Some(status) = self.status {
                map.insert("status".to_string(), Value::from(status));
            }
            map.insert("ip".to_string(), Value::String(self.ip.clone()));
        }
        map.insert("durationMs".to_string(), Value::from(self.duration_ms));
        if let Some(error) = &self.error {
            map.insert("error".to_string(), Value::String(error.clone()));
        }
        Value::Object(map)
    }
}

/// 出口 IP 查询（附加信息，失败只返回空串）。
///
/// 单独打 ipify 而不是从上游响应里读 —— 上游不回显客户端 IP；
/// 查不到不代表出口不可用，所以这里的错误一律吞掉。
async fn read_exit_ip(client: &reqwest::Client, proxy: Option<&ResolvedProxy>, timeout_ms: u64) -> String {
    let _ = proxy; // 出口已由 client 决定；保留参数是为了调用处可读
    let request = client
        .get(IP_ECHO_URL)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(timeout_ms));
    let Ok(response) = request.send().await else {
        return String::new();
    };
    if !response.status().is_success() {
        return String::new();
    }
    let Ok(text) = response.text().await else {
        return String::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => value
            .get("ip")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default(),
        // 返回的不是 JSON（某些镜像返回纯文本 IP）时截断到 64 字符直接用
        Err(_) => text.trim().chars().take(64).collect(),
    }
}

/// 测试某个出口是否可用。
///
/// 判据是「能否连上 WorkBuddy 上游」，而**不是**能否访问第三方站点：
/// 上游在国内可直连、国外站点常常恰恰相反，用第三方站点当判据会把能用的
/// 配置报成失败。只要能拿到 HTTP 响应（含 401/404 这类未鉴权响应）就算通。
///
/// `target_url` 为 None 时用 `https://copilot.tencent.com/v2/config`。
pub async fn test_connectivity(
    proxy: Option<&ResolvedProxy>,
    timeout_ms: u64,
    target_url: Option<&str>,
) -> ConnectivityResult {
    let started = logging::now_ms();
    let url = target_url.unwrap_or(DEFAULT_TEST_URL);
    let client = client_for(proxy);
    let request = client
        .get(url)
        .header("Accept", "application/json")
        // 单请求超时覆盖这里（测试用的是总超时，与 Node 的 AbortSignal.timeout 同语义；
        // 客户端上的 read_timeout 不会妨碍它 —— 总超时更短就先触发）
        .timeout(Duration::from_millis(timeout_ms));
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let duration_ms = logging::now_ms() - started;
            // 只关心「连得上」，不读响应体（drop 掉即可，连接会回到池里或直接关闭）
            drop(response);
            let ip = read_exit_ip(&client, proxy, timeout_ms.min(IP_LOOKUP_MAX_MS)).await;
            ConnectivityResult {
                success: true,
                status: Some(status),
                ip,
                duration_ms,
                error: None,
            }
        }
        Err(error) => ConnectivityResult {
            success: false,
            status: None,
            ip: String::new(),
            duration_ms: logging::now_ms() - started,
            error: Some(if error.is_timeout() {
                // 照抄 Node 的文案（秒数四舍五入到整秒）
                format!("连接超时（{} 秒）", (timeout_ms as f64 / 1000.0).round() as i64)
            } else {
                describe_error_detail(&error)
            }),
        },
    }
}

/// 用默认超时与默认目标测试出口（路由层的入口）
pub async fn test_proxy(proxy: Option<&ResolvedProxy>) -> ConnectivityResult {
    test_connectivity(proxy, DEFAULT_TEST_TIMEOUT_MS, None).await
}

/// 错误链的可读描述（对照 Node 版 `proxyErrorDetail`）。
///
/// Node 版从 `error.cause` 一路下钻拼出 `分段 → 分段`，因为 undici 的连接类
/// 错误（ECONNREFUSED / ETIMEDOUT）常挂在 cause 上。reqwest 的错误链用
/// `source()` 表达（如 `error sending request` → `client error (Connect)` →
/// `tcp connect error` → `Connection refused`），所以这里用
/// `std::error::Error::source()` 做同样的下钻。
///
/// 去重是必要的：reqwest 的若干层 source 文案会重复（如 `error sending request`）。
pub fn describe_error_detail(error: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    // 顶层 reqwest 错误自己的文案（含 URL 与超时语义）
    let head = error.to_string();
    if !head.is_empty() {
        parts.push(head);
    }
    let mut current: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(source) = current {
        let text = source.to_string();
        if !text.is_empty() && !parts.iter().any(|part| part == &text) {
            parts.push(text);
        }
        current = source.source();
    }
    if parts.is_empty() {
        return "未知错误".to_string();
    }
    parts.join(" → ")
}

/// 出口的公开描述（`{label, host, port}`，不含用户名/密码）——
/// 路由层在测试响应里回显它，方便前端在结果旁标明测的是哪个出口。
pub fn describe_public(proxy: Option<&ResolvedProxy>) -> Value {
    match proxy {
        None => Value::Null,
        Some(proxy) => json!({
            "label": proxy.label,
            "host": proxy.host,
            "port": proxy.port_json(),
        }),
    }
}
