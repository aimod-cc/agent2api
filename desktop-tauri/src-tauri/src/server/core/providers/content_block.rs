//! 内容策略拦截的判定：**上游审核误报**的共用规则（provider 之间共用）。
//!
//! 照搬 workbuddy2api 的 `contentBlockedRule`（`internal/upstream/client.go`）：
//! HTTP 400 + 下列文案之一 = 内容策略拦截。
//!
//! ```text
//!   blocked by security policy
//!   unapproved channel
//!   illegal api invocation
//! ```
//!
//! ── 为什么这是「误报」而不是账号问题 ──────────────────────────
//! 上游按**逐字精确匹配**审核（不是语义审核），客户端注入的 system 模板句
//! 命中即整单拦截。此时账号本身健康：余额够、没限流、session 没死 —— 换账号
//! 再试是白扔另一个账号的额度（同一份 body 换谁发都会被拦）。所以分类结果
//! [`UpstreamErrorClass::ContentBlocked`] 在编排层**不罚账号**（无冷却、无熔断、
//! 不计错误），而是走「降级到中性提示词后同账号重试一次」。
//!
//! ── 为什么放在 providers 层而不是编排层 ───────────────────────
//! 「哪些文案算内容拦截」是**上游侧知识**（与 11-128、6004 同类），按本项目的
//! 分层约定属于适配器；编排层只认三档动作，不认任何文案。规则本身五家共用，
//! 所以放在 providers 下的独立小模块里，由各适配器在自家属判定之后调用。
//!
//! ── 与 workbuddy 的 11-128 是什么关系 ─────────────────────────
//! `11-128` 正是这类拦截的**业务码**（历史文案 `Illegal API invocation from an
//! unapproved channel`）。两条判据（业务码 / 文案）指向同一件事，适配器里
//! 取并集：文案兜住「上游改了码但文案没变」，业务码兜住「上游改了文案但码没变」。
//!
//! ── 大小写与状态码口径（与参考项目一致）───────────────────────
//! 文案**大小写不敏感**（Go 的 `matchLower`：先 `strings.ToLower(body)` 再
//! 子串匹配）；状态码只要求 `>= 400`（参考项目把这一条判在 `status >= 400`
//! 分支里，401 / 429 已在更前面被各自的分支接走）。

use serde_json::Value;

use super::adapter::UpstreamErrorClass;

/// 内容策略拦截的文案特征（全部小写；匹配前把上游文案转小写）
const PATTERNS: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
];

/// 客户端可见的**内容拦截提示**：附在 `上游返回 {status}: {上游原文}` 之后
/// （对应参考项目 `hint.go` 里 `ErrContentBlocked` 那句
/// `request content was rejected by content policy; adjust the prompt and retry`）。
///
/// 与参考项目同一用途：只回一句上游原文，用户不知道「这是我的问题还是网关的问题」，
/// 更不知道下一步该改什么。这里给两条**可执行**的路：
///   - system 模板指纹误报 → 切到「替换」模式（见 `core::prompt`）；
///   - 对话内容本身触发审核 → 调整内容后重试。
///
/// ── 措辞上刻意不断言「网关已经重试过」──────────────────────────
/// 同一个 message 会在多种情形下发给客户端：`custom` 模式**不做**降级补救
/// （system 已由网关接管，见 `core::prompt` 的 `degradable`），而降级期内
/// 首发的请求也没有「再试一次」这个动作。写死「已换中性提示词重试过一次」
/// 在这些情形下就是假话 —— 文案只描述用户能做的事，不描述网关做了什么
/// （网关做了什么在请求日志的重试链里，那里是逐请求事实）。
pub const CONTENT_BLOCK_HINT: &str = "；内容被上游内容策略拒绝（这类拦截按逐字匹配，不是账号问题）：\
若为客户端 system 模板误报，可在设置页「通用 → 系统提示词」切到「替换」模式；\
若为对话内容本身触发审核，请调整后重试";

/// 是否命中内容策略拦截的文案特征。
pub fn matched(status: u16, error_body: &Value) -> bool {
    if status < 400 {
        return false;
    }
    let Some(message) = error_body.get("message").and_then(Value::as_str) else {
        return false;
    };
    let lowered = message.to_lowercase();
    PATTERNS.iter().any(|pattern| lowered.contains(pattern))
}

/// 分类兜底：命中内容拦截文案 → [`UpstreamErrorClass::ContentBlocked`]
/// （并给文案补上 [`CONTENT_BLOCK_HINT`]），否则 → [`UpstreamErrorClass::Fatal`]。
///
/// `message` 由调用方给（各家的错误文案措辞不同），本函数负责归类与补提示。
pub fn classify_or_fatal(
    status: u16,
    error_body: &Value,
    message: String,
    upstream_code: Option<i64>,
) -> UpstreamErrorClass {
    if matched(status, error_body) {
        return UpstreamErrorClass::ContentBlocked {
            status,
            message: format!("{message}{CONTENT_BLOCK_HINT}"),
            upstream_code,
        };
    }
    UpstreamErrorClass::Fatal {
        status,
        message,
        upstream_code,
    }
}
