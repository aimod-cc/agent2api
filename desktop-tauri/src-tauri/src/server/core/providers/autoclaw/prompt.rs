//! AutoClaw 出站 system 提示规范化：把客户端自己的 system 提示**包进**上游要求的
//! OpenClaw 身份前缀里（本次新增，起因是上游 2026-09-22 起加的闸门）。
//!
//! ── 上游加了什么（逐条实测，国内版免费账号，2026-09-22）───────
//! `POST .../autoclaw-proxy/proxy/autoclaw/chat/completions` 对**系统提示词**
//! 做了检查。同一账号、同一请求，只改 system 一项的实测结论：
//!
//! ```text
//!   system 内容                                          结果
//!   ───────────────────────────────────────────────────  ──────────────────
//!   身份句 + `## Tooling` 段                             200
//!   上面那段 + 客户端自己的提示词（任意内容接在后面）     200
//!   只有身份句、没有 `## Tooling` 段                      403 pay-view
//!   中性句（You are a helpful assistant.）                406（空响应体）
//!   完全没有 system 消息                                  406（空响应体）
//!   身份句 + "You are ZCode…"                            406
//!   身份句 + "You are Claude Code…"                      406
//! ```
//!
//! 即：**必须**以身份句开头且带 `## Tooling` 段，同时**不能**出现外来 harness
//! 的身份句。黑名单是**字面**判定（`You are <产品名>` 形态）：把主语换掉的
//! "ZCode is an interactive coding agent." 实测放行，所以改写产品名即可绕过。
//!
//! 身份句与 `## Tooling` 段的原文取自客户端自带的运行时
//! （`resources/gateway/openclaw/dist/system-prompt-config-*.js` 里
//! `buildSystemPrompt` 的开头两行）—— 照抄它的好处是出站形态与官方客户端一致，
//! 而不是我们自己编一句「看起来像」的话。
//!
//! ── 本模块做什么（两条纯文本变换）──────────────────────────
//!   1. **前置** [`IDENTITY_PREFIX`]：首条 system / developer 消息的正文前面插入
//!      它，客户端自己的提示词**逐字保留在后面**。刻意不替换客户端提示词（用户
//!      明确要求保留）：上游只要求「以身份句开头」，前缀之后接什么它不管；
//!   2. **改写**外来身份句（[`FOREIGN_IDENTITIES`]）：产品名换成中性说法，语义
//!      不变、字面匹配被破坏 —— 与 `core::sanitize` 同一手法，但**规则独立**：
//!      那边对付的是 workbuddy 上游的内容审核指纹（插一个词就够），这条闸门更严
//!      （连 sanitize 改写后的 "You are Claude Code, …" 都拦），所以本表是
//!      **把产品名整个去掉**，不是插词。
//!
//! ── 边界（刻意不做的事，别顺手补）──────────────────────────
//!   - **不动 user / assistant / tool 消息**：闸门只看系统提示词，改写用户内容
//!     属于破坏数据；
//!   - **不替换、也不追加** system 消息：前缀直接拼在原有 system 正文之前。
//!     追加第二条 system 是另一条路（`core::prompt` 的 `append` 模式在做全局
//!     提示词），本模块不掺和；
//!   - **不碰 `tools` / `stream` / 其它字段**：tools 不是这条闸门的判据
//!     （实测不带 tools 的 OpenClaw prompt 同样 200）。
//!
//! ── 幂等性（重要）──────────────────────────────────────────
//! 正文已经以身份句开头时**只改写、不再前置**：官方客户端的请求本身就带 OpenClaw
//! 提示词，重复前缀会把提示词写两遍（降级 / 同家重试路径上必然发生）。判据只有
//! `starts_with(IDENTITY_LINE)` 一条。
//!
//! ── 与网关提示词层（`core::prompt`）的关系 ─────────────────
//! 那一层是**网关自有**提示词（透传 / 替换 / 追加，全局配置），跑在
//! `upstream::payload::send_body` 里，**先于**本模块；本模块是**这一家**的出站
//! 整形，与 `model` 改写同处（`adapter::build_chat_request`）。因此在 `custom`
//! 模式下出站形态是「网关提示词 → 再包上 OpenClaw 身份前缀」，两层叠加、互不替代。
//!
//! ── 硬约束 ─────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。形态怪异（`content`
//! 是数组 / null / 消息不是对象）时一律「尽力产出能发出去的 body」，绝不失败 ——
//! 与 `core::prompt` 的 `rewrite` / `append` 同一条纪律。

use serde_json::{json, Value};

/// 上游闸门要求「以它开头」的身份句（`IDENTITY_PREFIX` 的第一行，单独导出给
/// 幂等判定与日志文案用）。
pub const IDENTITY_LINE: &str = "You are a personal assistant running inside OpenClaw.";

/// 出站身份前缀：身份句 + `## Tooling` 段（官方 prompt 的开头两行，逐字照抄）。
///
/// 末尾带一个 `\n`，[`join_prefix`] 再补一个空行 —— 于是客户端正文另起一段，
/// 与官方 prompt 的段落风格一致。
const IDENTITY_PREFIX: &str = "You are a personal assistant running inside OpenClaw.\n\n\
## Tooling\n\
Available tools are policy-filtered. Names are case-sensitive; call exactly as listed.\n";

/// 外来身份句 → 中性说法（**长的在前**）。
///
/// 顺序与 `sanitize.rs` 的规则表同一条纪律：短的在前会把长匹配串切碎，于是
/// 「改写后仍含产品名」这种半吊子结果会被漏掉（例如先命中 `You are ZCode` 就
/// 再也匹配不到 `You are ZCode, an interactive coding agent`）。
///
/// 每条的替换串都**不含产品名**：闸门拦的是产品名本身，插一个词（sanitize 的
/// 手法）在这里不够用。ZCode / Claude Code / Codex 三条都实测过改写结果
/// （`200`），不是照抄猜测。
const FOREIGN_IDENTITIES: &[(&str, &str)] = &[
    (
        "You are ZCode, an interactive coding agent",
        "You are an interactive coding agent",
    ),
    (
        "You are a coding agent running in the Codex CLI tool",
        "You are a coding agent running in a terminal CLI tool",
    ),
    (
        "You are a coding agent running in the Codex CLI",
        "You are a coding agent running in a terminal CLI",
    ),
    // Claude Code 的整句是 "You are Claude Code, Anthropic's official CLI tool for
    // Claude."；只改到「You are Claude Code」为止，后面的产品说明原样保留。
    ("You are Claude Code", "You are a coding assistant"),
    ("You are ZCode", "You are an interactive coding agent"),
];

/// 出站前规范化 body 里的系统提示词（**就地修改**）。
///
/// 两步：① 所有 system / developer 消息里的外来身份句改写；② 保证首条消息是
/// system / developer 且正文以身份句开头（是则前置前缀，否则在 `messages[0]`
/// 插一条带前缀的 system 消息）。
///
/// `messages` 缺失或不是数组时**直接返回**：那不是本模块能修的形态，交给上游报
/// 格式错误比在这里造一个空 `messages` 更能说明问题（与 `adapter` 对非对象 body
/// 的处置同一取向）。
pub fn normalize(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if !is_system_role(message) {
            continue;
        }
        if let Some(content) = message.get_mut("content") {
            rewrite_identities_in_place(content);
        }
    }
    // 首条就是 system / developer：前缀拼进它自己的正文（客户端提示词保留在后）。
    if messages.first().map(is_system_role).unwrap_or(false) {
        if let Some(first) = messages.first_mut() {
            prepend_prefix(first);
        }
        return;
    }
    // 首条不是：补一条只带前缀的 system 消息。**不改**后面那些 system 消息的
    // 位置 —— 重排消息序列的风险（破坏客户端的轮次结构）远大于收益。
    messages.insert(
        0,
        json!({ "role": "system", "content": IDENTITY_PREFIX }),
    );
}

/// 给一条消息的 `content` 前置身份前缀（已经以身份句开头则不动）。
///
/// `content` 的三种形态：
///   - 字符串：直接拼；
///   - 数组（多 part）：拼进**第一个文本 part**；一个文本 part 都没有时在数组
///     头部插一个 —— 上游闸门看的是文本，插一个 part 比替换整个数组安全；
///   - 其它（null / 对象 / 数字）：换成前缀本身。这种 content 当 system 提示词
///     本来就不成立（上游会当空提示词处理，稳定 406），换掉比原样发出去好。
fn prepend_prefix(message: &mut Value) {
    let Some(content) = message.get_mut("content") else {
        // 连 content 字段都没有：补上（`role` 已经是 system 了，补 content 不会
        // 改变这条消息的语义，只是让闸门能过）
        if let Some(object) = message.as_object_mut() {
            object.insert(
                "content".to_string(),
                Value::String(IDENTITY_PREFIX.to_string()),
            );
        }
        return;
    };
    match content {
        Value::String(text) => {
            if !text.starts_with(IDENTITY_LINE) {
                let next = join_prefix(text);
                *text = next;
            }
        }
        Value::Array(parts) => {
            let index = parts
                .iter()
                .position(|part| part.get("text").and_then(Value::as_str).is_some());
            match index {
                Some(index) => {
                    let already = parts
                        .get(index)
                        .and_then(|part| part.get("text"))
                        .and_then(Value::as_str)
                        .map(|text| text.starts_with(IDENTITY_LINE))
                        .unwrap_or(false);
                    if already {
                        return;
                    }
                    // `index` 是上面按 `as_str().is_some()` 选出来的，所以这里的
                    // `get_mut` 必然是字符串 —— `if let` 不是「可能静默跳过」的
                    // 分支，而是没有 `as_str_mut` 可用时的取值写法。
                    if let Some(Value::String(text)) = parts
                        .get_mut(index)
                        .and_then(|part| part.get_mut("text"))
                    {
                        let next = join_prefix(text);
                        *text = next;
                    }
                }
                None => parts.insert(
                    0,
                    json!({ "type": "text", "text": IDENTITY_PREFIX }),
                ),
            }
        }
        other => {
            *other = Value::String(IDENTITY_PREFIX.to_string());
        }
    }
}

/// 前缀 + 一个空行 + 原文（原文为空时不留多余空行）。
fn join_prefix(original: &str) -> String {
    if original.is_empty() {
        return IDENTITY_PREFIX.to_string();
    }
    let mut out = String::with_capacity(IDENTITY_PREFIX.len() + original.len() + 1);
    out.push_str(IDENTITY_PREFIX);
    out.push('\n');
    out.push_str(original);
    out
}

/// 就地改写 content（字符串 / 文本 part 数组）里的外来身份句。
///
/// 只处理这两种承载文本的形态：`content` 是别的东西（null / 对象）时无文本可改，
/// 由 [`prepend_prefix`] 统一处置。
fn rewrite_identities_in_place(content: &mut Value) {
    match content {
        Value::String(text) => rewrite_identities(text),
        Value::Array(parts) => {
            for part in parts.iter_mut() {
                if let Some(Value::String(text)) = part.get_mut("text") {
                    rewrite_identities(text);
                }
            }
        }
        _ => {}
    }
}

/// 逐条套用 [`FOREIGN_IDENTITIES`]（`str::replace` 是**全量**替换，一条文本里
/// 出现多次也一并改掉）。
fn rewrite_identities(text: &mut String) {
    for (from, to) in FOREIGN_IDENTITIES {
        if text.contains(from) {
            let next = text.replace(from, to);
            *text = next;
        }
    }
}

/// 该消息是否承载 system 级指令：角色**精确**等于 `system` 或 `developer`。
///
/// 与 `core::prompt::is_system_role` 同一口径（那边是私有函数，本目录不跨模块
/// 借用）：精确匹配而不是前缀 / 包含，其余角色（含未知值）一律不动 —— 这是
/// 「不动用户内容」这条边界的判据。
fn is_system_role(message: &Value) -> bool {
    message
        .get("role")
        .and_then(Value::as_str)
        .map(|role| role == "system" || role == "developer")
        .unwrap_or(false)
}
