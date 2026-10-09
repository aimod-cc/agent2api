//! Qoder 上游错误分类：状态码 + 业务码 → 网关该走哪个动作。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 判定链与码表本身有三十来行注释要说清「为什么是这个顺序」，放进
//! `protocol.rs`（协议转换）会把那个文件的主题冲淡，也越过本项目的单文件
//! 行数约定。这里只放**分类**：上游的排队 / 额度 / 鉴权信号 → 语义分类；
//! 协议转换仍在 `protocol.rs`，退避重试与文案成形在 `chat.rs`。
//!
//! ── 上游错误长什么样（分类要顺着钻的缘由）────────────────────
//! 上游用 **403 同时表达三种完全不同的处置**，而且正文常常是**多层嵌套的
//! JSON 字符串**：
//!
//! ```text
//! {"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,…}\"}"}
//! ```
//!
//!   排队中（10605）→ 等一会儿重发；额度不足（110 / 112 / 113…）→ 换账号；
//!   登录态失效（105）或裸 403 → 刷新凭证重试。只看状态码必然把「排队」
//!   判成「登录失效」—— 那正是 10605 被误报成鉴权失败那条回归的根因。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::Value;

use super::protocol::truthy;

/// 上游状态码/错误文本 → 下游能理解的语义分类（源实现 `classifyUpstreamError`）。
///
/// ── 顺序很重要 ──────────────────────────────────────────────
/// **先看响应体里的语义特征，再看状态码**：上游用 403 表达多种情况 ——
/// 带上 pricingUrl 是套餐/额度不足，裸 403 才是鉴权问题。只按状态码判断
/// 会把「该充值」误报成「登录失效」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpstreamKind {
    /// 额度/套餐不足（可换账号重试）
    Quota,
    /// 触发限流（可换账号重试）
    Rate,
    /// 鉴权失败（**明确的**：HTTP 401 或业务码 105「Login or access token expired」）
    ///
    /// 它在会话式转发里会触发「强制续期凭证 + 同账号重试一次」（见
    /// `chat::AttemptError::Auth`）：`dt-` / `drt-` 这一族凭证可能在时间上还
    /// 很新的时候就被服务端作废（被顶下线、轮换过），此时只调 `ensure` 会拿回
    /// 同一个被拒的 token。判据必须收紧到「确定的凭证问题」—— 裸 403 不在其列，
    /// 见 [`UpstreamKind::Forbidden`]。
    Auth,
    /// 上游**拒绝访问**（裸 403，且没有排队 / 额度特征）：权限、地区或未开通。
    ///
    /// 与 [`UpstreamKind::Auth`] 分开的理由：这档刷凭证是白刷 —— 凭证本身是好的，
    /// 换一个新的也还是被拒。文案沿用改造前那句「可能是登录态失效或权限不足」，
    /// 因为只凭一个裸 403 确实分不出是哪一种。
    Forbidden,
    /// 上游服务异常（可重试）
    Server,
    /// **模型排队中**（业务码 10605）：不是错误，是「暂时排不上号」。
    ///
    /// 上游对免费模型（`qfmodel` = Qwen3.8-Flash）走排队制：空闲时几秒内直接
    /// 出字，繁忙时用 403 承载 `{"isQueued":true,"serviceAvailable":false,
    /// "queueType":"p3","retryAfterSeconds":30}`（业务码 10605，官方错误码表
    /// 写作 "Model request is queued"）。官方 CLI 的做法是等一会儿再发同一
    /// 请求（其二进制里有 `queuePollCount` / `queueRecoveryAttempt` 这类计数），
    /// 参考实现（CLIProxyAPI 的 qoder2api 插件、9router 的队列增强分支）同样
    /// 按上游建议时长退避重试。
    ///
    /// **它必须与 Quota / Auth 分开**：排队既不是账号额度耗尽（不该落冷却、
    /// 不该换账号 —— 换谁都一样在排队），也不是登录态失效（不该刷新凭证，
    /// 更不该给用户一句「登录态已失效」把人引向重新登录）。见 `chat::Queued`。
    Queued,
    /// 其它（换账号也没用）
    Unknown,
}

/// 上游排队态的信号（业务码 10605 那一族字段）。
///
/// `retryAfterSeconds` 与 `waitTime` 是上游**建议**的重试间隔（实测 9～30 秒
/// 浮动）；两个都没有时由调用方给缺省值，不在这里猜。
#[derive(Clone, Debug, Default)]
pub struct QueueInfo {
    /// 上游建议的重试间隔（秒；`retryAfterSeconds` 优先，其次 `waitTime`）
    pub retry_after_secs: Option<u64>,
    /// 队列档位（`queueType`，实测 "p3"），排障用
    pub queue_type: Option<String>,
    /// 上游声称模型服务是否可用（`serviceAvailable`）
    pub service_available: Option<bool>,
    /// 队列里有多少条（`queueCount`），排障用
    pub queue_count: Option<i64>,
}

/// 分类结果的文案与可重试性
pub struct ClassifiedError {
    /// 分类（决定编排层走哪个动作：换账号 / 刷新重试 / 原样透传）
    pub kind: UpstreamKind,
    /// 面向客户端的人话（源实现 `classifyUpstreamError` 的 `message`）
    pub message: String,
    /// 上游给出的套餐/定价页链接（有的话拼进提示）
    pub pricing_url: Option<String>,
    /// 排队信号（`kind == Queued` 时有值）：退避时长与排障信息
    pub queue: Option<QueueInfo>,
}

impl ClassifiedError {
    fn new(kind: UpstreamKind, message: impl Into<String>, pricing_url: Option<String>) -> Self {
        Self { kind, message: message.into(), pricing_url, queue: None }
    }
}

// ─── 上游业务码表（官方 SDK 错误码 + 参考实现实测）─────────────────
//
// 这些码**不体现在 HTTP 状态码上**：上游把业务错误放在 SSE 信封的
// `statusCodeValue` 里（或与 403 一起放在响应体里），正文常常是**多层嵌套的
// JSON 字符串**（`{"code":"403","message":"{\"code\":\"10605\",…}"}`）。
// 因此取值要顺着 `message` / `body` / `data` 一路钻进去，见 `scan_signals`。

/// 排队中：官方错误码表 "10605 Model request is queued"
const QUEUE_CODES: &[&str] = &["10605"];
/// 登录态失效：官方错误码表 "105 Login or access token expired"
const AUTH_CODES: &[&str] = &["105"];
/// 额度类：110 每日用量上限、112 额度耗尽、113 用量配额耗尽、114 试用额度用完、
/// 115 免费用户配额用完、116/117/118 团队 / 成员 / 个人 Credits 用完、
/// 119 所选模型的免费额度用完、122 计费组上限（110 / 112 另有 9router 实测佐证）
const QUOTA_CODES: &[&str] =
    &["110", "112", "113", "114", "115", "116", "117", "118", "119", "122"];

/// 嵌套 JSON 的钻取深度上限（防病态输入下的环；实测两层就到底了）
const SIGNAL_SCAN_DEPTH: usize = 5;

/// 扫出来的判定信号
#[derive(Default)]
struct ErrorSignals {
    /// 各层 `code` 字段的原样文本（上游数字与字符串两种形态都有）
    codes: Vec<String>,
    queue: QueueInfo,
    /// 见过 `"isQueued":true`
    is_queued: bool,
}

impl ErrorSignals {
    fn has_code(&self, table: &[&str]) -> bool {
        self.codes.iter().any(|code| table.contains(&code.as_str()))
    }

    /// 是不是排队态：业务码 10605 是硬信号；`isQueued` 为真；
    /// 或「声明服务不可用 + 带了队列档位」这一组合（上游换码时的兜底）
    fn queued(&self) -> bool {
        self.has_code(QUEUE_CODES)
            || self.is_queued
            || (self.queue.queue_type.is_some() && self.queue.service_available == Some(false))
    }
}

/// 顺着嵌套 JSON 抠出业务码与排队信号（见上方码表的说明）。
///
/// 解析失败不再往下钻：能拿到多少算多少，最后由 `classify_upstream_error`
/// 的**裸文本兜底**（`raw_has_code`）兜住「正文不是 JSON」的情形。
fn scan_signals(raw: &str) -> ErrorSignals {
    let mut signals = ErrorSignals::default();
    let mut current = Some(raw.to_string());
    let mut depth = 0usize;
    while let Some(text) = current.take() {
        depth += 1;
        if depth > SIGNAL_SCAN_DEPTH {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            break;
        };
        if let Some(code) = value.get("code") {
            match code {
                Value::String(text) => signals.codes.push(text.trim().to_string()),
                Value::Number(number) => signals.codes.push(number.to_string()),
                _ => {}
            }
        }
        if value.get("isQueued").map(truthy).unwrap_or(false) {
            signals.is_queued = true;
        }
        if signals.queue.retry_after_secs.is_none() {
            signals.queue.retry_after_secs = ["retryAfterSeconds", "waitTime"]
                .iter()
                .find_map(|key| value.get(*key).and_then(seconds_of));
        }
        if signals.queue.queue_type.is_none() {
            signals.queue.queue_type = value
                .get("queueType")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if signals.queue.service_available.is_none() {
            signals.queue.service_available = value.get("serviceAvailable").map(truthy);
        }
        if signals.queue.queue_count.is_none() {
            signals.queue.queue_count = value.get("queueCount").and_then(Value::as_i64);
        }
        // 下一层：内嵌的 JSON 字符串（上游一层套一层地放 message / body / data）
        current = ["message", "body", "data", "error"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(str::to_string)
            .filter(|text| text.trim_start().starts_with('{'));
    }
    signals
}

/// 秒数的两种形态（数字 / 数字字符串）
fn seconds_of(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 裸文本兜底：正文不是 JSON 时也能认出业务码（`\"code\":\"10605\"` 这类
/// 转义形态把反斜杠去掉就与普通形态同形）。
fn raw_has_code(raw: &str, code: &str) -> bool {
    let flat: String = raw.chars().filter(|ch| *ch != '\\').collect();
    flat.contains(&format!("\"code\":\"{code}\""))
        || flat.contains(&format!("\"code\":{code}"))
        || flat.contains(&format!("\"code\": \"{code}\""))
}

/// 从文本里抠出定价页链接（源实现用正则 `/https?:\/\/[^"\\]*\/pricing[^"\\]*/i`）
///
/// ── 折叠为什么必须是 `to_ascii_lowercase` ────────────────────────
/// 这里拿**折叠后的下标**回切**原文**（`&raw[start..]`），前提是两者逐字节对齐。
/// `to_lowercase` 是 Unicode 折叠、**不保字节长度**：`ẞ`(U+1E9E) 三字节折成 `ß`
/// 两字节、`İ`(U+0130) 两字节折成 `i`+U+0307 三字节。下标一回切就可能落在字符
/// 中间或直接越界 —— release 是 `panic=abort`，进程当场死。上游错误正文完全
/// 不可控（`http_error` 把整段错误体原样喂进来），这条路径等于把「上游文案里
/// 出现一个小写化会变长的字符」变成「网关整体宕机」。
/// `to_ascii_lowercase` 只动 ASCII 字节、长度恒定，下标对 `raw` 恒有效；而要匹配
/// 的 `http` / `/pricing` 都是 ASCII，非 ASCII 字符的每个字节都 >= 0x80、折叠后
/// 不可能等于 ASCII 字节，所以只折叠 ASCII 与源实现正则的 `/i` 等价。
/// （同款坑的既有正确写法：`autoclaw/prompt.rs` 的 `find_ignore_ascii_case`、
/// `zcode/reasoning.rs` 的 `to_ascii_lowercase`。）
fn pricing_url_of(raw: &str) -> Option<String> {
    let lowered = raw.to_ascii_lowercase();
    let mut search_from = 0usize;
    while let Some(offset) = lowered[search_from..].find("http") {
        let start = search_from + offset;
        let rest = &raw[start..];
        let end = rest
            .find(|ch: char| ch == '"' || ch == '\\' || ch.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.to_ascii_lowercase().contains("/pricing") {
            return Some(candidate.to_string());
        }
        search_from = start + 4;
        if search_from >= raw.len() {
            break;
        }
    }
    None
}

/// 分类上游错误（源实现 `classifyUpstreamError` 的判定链，逐条对应）。
///
/// ── 顺序要点：排队 → 业务码 → 额度关键词 → 状态码 ──────────────
///   - **排队特征前置于额度关键词**：额度那一串泛词（`plan` / `trial` /
///     `credit`…）只要命中就判额度，而排队正文里带了 `serviceAvailable` 这类
///     字段，先判排队才不会被泛词截胡；
///   - **业务码前置于状态码**：上游用 403 同时表达「排队」「额度不足」
///     「登录态失效」三种完全不同的处置，只看状态码必然误判（这正是
///     业务码 10605 被当成鉴权失败那条回归的根因）。
pub fn classify_upstream_error(status: u16, text: &str) -> ClassifiedError {
    let body = text.to_lowercase();
    let pricing = pricing_url_of(text);
    let signals = scan_signals(text);

    // ① 排队中（10605）：既不是额度也不是鉴权，只有「等一会儿再发」这一个动作
    if signals.queued() || raw_has_code(text, QUEUE_CODES[0]) {
        let queue = signals.queue;
        return ClassifiedError {
            kind: UpstreamKind::Queued,
            message: queued_message(&queue),
            pricing_url: None,
            queue: Some(queue),
        };
    }
    // ② 业务码：登录态失效（105）与额度类（110 / 112 / 113 …）
    if signals.has_code(AUTH_CODES) || AUTH_CODES.iter().any(|code| raw_has_code(text, code)) {
        return ClassifiedError::new(
            UpstreamKind::Auth,
            "登录态已失效，请重新登录",
            None,
        );
    }
    if signals.has_code(QUOTA_CODES)
        || QUOTA_CODES.iter().any(|code| raw_has_code(text, code))
    {
        return ClassifiedError::new(
            UpstreamKind::Quota,
            "当前账号额度不足或套餐不支持该模型",
            pricing,
        );
    }
    // ③ 额度关键词 / 定价页链接（源实现的判定链）
    let quota_signals = [
        "pricingurl",
        "insufficient",
        "no_quota",
        "quota_exceed",
        "exceed_quota",
        "exceeded",
        "credit",
        "upgrade",
        "subscription",
        "plan",
        "trial",
    ];
    if pricing.is_some() || quota_signals.iter().any(|signal| body.contains(signal)) {
        return ClassifiedError::new(
            UpstreamKind::Quota,
            "当前账号额度不足或套餐不支持该模型",
            pricing,
        );
    }
    if status == 429 || body.contains("rate limit") || body.contains("too many") {
        return ClassifiedError::new(UpstreamKind::Rate, "请求过于频繁，请稍后重试", None);
    }
    if status == 401 {
        return ClassifiedError::new(UpstreamKind::Auth, "登录态已失效，请重新登录", None);
    }
    if status == 403 {
        // 走到这里说明没有排队、没有额度特征：按权限问题处理（**不**触发刷凭证，
        // 见 `UpstreamKind::Forbidden`）
        return ClassifiedError::new(
            UpstreamKind::Forbidden,
            "上游拒绝访问，可能是登录态失效或权限不足",
            None,
        );
    }
    if status >= 500 {
        return ClassifiedError::new(UpstreamKind::Server, "上游服务异常", None);
    }
    ClassifiedError::new(UpstreamKind::Unknown, "", None)
}

/// 排队态面向客户端的文案（说明「不是登录态 / 额度问题」是关键：
/// 改造前这句被写成「登录态已失效」，用户会去重新登录一个完全正常的账号）
fn queued_message(queue: &QueueInfo) -> String {
    match queue.retry_after_secs {
        Some(seconds) => format!(
            "上游模型排队中（模型暂不可服务，上游建议 {seconds} 秒后重试）：这不是登录态或额度问题"
        ),
        None => "上游模型排队中（模型暂不可服务）：这不是登录态或额度问题".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::pricing_url_of;

    /// 回归：折叠改变字节长度时，`pricing_url_of` 不得 panic。
    ///
    /// 旧实现取 `raw.to_lowercase()` 的 `find("http")` 下标去切**原文**，而 Unicode
    /// 折叠不保字节长度 —— 上游错误正文里只要在 `http` 之前出现一个「小写化后变短」
    /// 的字符，`&raw[start..]` 就落在多字节字符中间（`panic: not a char boundary`）；
    /// 出现「变长」的字符则下标越过原文末尾（`panic: index out of bounds`）。
    /// release 是 `panic = "abort"`，两种都是**进程当场退出**，而这条路径由
    /// `classify_upstream_error` 对上游错误体无条件触发。
    #[test]
    fn pricing_url_survives_case_folding_that_changes_byte_length() {
        // 变短：`ẞ`(U+1E9E, 3 字节) 折成 `ß`(2 字节) → 旧实现下标前移 1，落进字符中间
        assert_eq!(
            pricing_url_of("ẞhttp://x/pricing").as_deref(),
            Some("http://x/pricing"),
        );

        // 变长：`İ`(U+0130, 2 字节) 折成 `i`+U+0307(3 字节) → 旧实现下标越过原文末尾
        let grows = format!("{}http://x/pricing", "İ".repeat(17));
        assert_eq!(pricing_url_of(&grows).as_deref(), Some("http://x/pricing"));
    }

    /// 只折叠 ASCII 必须与源实现正则的 `/i` 等价：大小写混写的链接照样命中。
    #[test]
    fn pricing_url_matches_ascii_case_insensitively() {
        assert_eq!(
            pricing_url_of("Upgrade at HTTPS://QODER.COM/Pricing now").as_deref(),
            Some("HTTPS://QODER.COM/Pricing"),
        );
    }

    /// 非定价页的链接不能被当成定价页（判定是 `/pricing`，不是「有链接」）。
    #[test]
    fn pricing_url_ignores_links_that_are_not_the_pricing_page() {
        assert_eq!(pricing_url_of("see https://qoder.com/docs for help"), None);
    }
}
