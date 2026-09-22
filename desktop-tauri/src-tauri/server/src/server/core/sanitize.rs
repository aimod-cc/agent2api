//! 出站请求体脱敏：剥离上游内容审核黑名单指纹（照搬 workbuddy2api 的
//! `internal/upstream/sanitize.go`，规则与顺序一字不改）。
//!
//! ── 背景 ────────────────────────────────────────────────────
//! 客户端（Claude Code 类 CLI）在 system prompt 注入若干固定模板句，
//! 上游内容审核按**逐字精确匹配**拦截（非语义审核），一字改动即可绕过。
//! 策略：键值/header 型指纹整段剥离；承载语义的模板句最小改写（换一词），
//! 语义不变。
//!
//! ── 与改造前（词表 + 零宽空格）的关系：整体替换 ──────────────
//! 改造前是「可维护词表 + 命中处插零宽空格（U+200B）」：词表能从仓库远程拉、
//! 用户在界面上增删词、按 provider 与 role 限定作用范围，还带命中统计。
//! 那一整套（词表 / 远程同步 / 状态持久化 / 独立设置页）**已随本次改造删除**，
//! 换成这份硬编码规则集。取舍的理由是**实测口径**：
//!   - 零宽空格对数字类指纹**无效**（上游会归一化），`11128` 必须改成
//!     `11-128` 这种真正的字面改动 —— 词表+ZWSP 这条路在数字场景根本走不通；
//!   - 模板句要的是「换一个词」，ZWSP 只是打断匹配，两者适用的指纹形态不同；
//!   - 词表方案的词条是**通用安全词**（malware / phishing / ransomware 等），
//!     与实测触发拦截的**具体模板串**是两码事，覆盖面看着大、命中率反而低。
//! 于是规则集从「35 条通用词 + ZWSP」换成「7 条特征串 + 3 条正则 + 5 条改写」。
//!
//! ── 本模块只做纯文本变换 ─────────────────────────────────────
//! 开关（`sanitizeBlacklistFingerprints`）与作用时机的判定**不在本模块**：
//! 转发层在每次出站前问一次配置，开着才调 [`sanitize_body`]。本模块无状态、
//! 无锁、无 IO，因此没有句柄也没有初始化顺序问题。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// 特征预检：任一命中才进入净化（`str::contains` 快速路径，
/// 普通请求全不中 → 原样返回，零分配）。
///
/// 匹配是**大小写敏感**的（对应 Go 的 `strings.Contains`）：这里列的都是
/// 小写/原样形态，混合大小写变体由 [`bare_hdr_re`] 兜底。
const SANITIZE_FEATURES: &[&str] = &[
    "x-anthropic-billing-header", // header 键值段键名
    "cc_entrypoint=",             // 尾随裸键值（截断前缀即可命中）
    "You are Claude Code",        // 身份句（截断前缀即可命中）
    "Main branch (",              // 注入指令句（截断前缀即可命中）
    "You are a coding agent running in the Codex CLI", // Codex instructions 首段（截断前缀即可命中）
    "github.com/anthropics/",     // 反馈句里的 Anthropic 仓库链接
    "11128",                      // 上游反探测：裸数字错误码
];

/// 改写层：全模板句逐字替换（每句只改一个词，语义不变）。
///
/// 身份句的匹配串**不带结尾标点**（只到 "…for Claude" 为止）：
/// CLI 版这句以句号收尾（"…for Claude."），桌面版（claude-desktop-3p / Agent SDK）
/// 以逗号接后继内容（"…for Claude, running within the Claude Agent SDK."）。
/// 带句号的整句只匹配前者，桌面版会漏网、指纹原样发上游 → 400 code=11128。
/// 去掉结尾标点后两种形态一并覆盖（替换串同样不带标点，让原有标点原样保留）。
/// 注意仍要求 "You are Claude Code, " 前缀，不做更宽的子串替换，
/// 以免误伤零散文本。
///
/// `ReplaceAll` 是**大小写敏感**的字面替换（对应 Go 的 `strings.ReplaceAll`），
/// 与预检同一口径。
const SANITIZE_REWRITES: &[(&str, &str)] = &[
    (
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    (
        "You are a coding agent running in the Codex CLI, a terminal-based coding assistant.",
        "You are a coding agent running in the Codex CLI tool, a terminal-based coding assistant.",
    ),
    (
        // 反馈句：整句带 Anthropic 仓库链接，上游按整句拦截（只留链接或只留半边均不拦，
        // 实测需整句同时出现）。give→provide 一词之差即可绕过，语义不变。
        "To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
        "To provide feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
    ),
    (
        // 上游反探测：只要请求体里出现裸数字 11128 就整单拦截（与该数字的上下文无关——
        // "code=11128" / 裸 "11128" / "错误码 11128" / "Code=11128" 全部命中；
        // 相邻的 11148 / 11101 / 11115 / 99999 均放行）。11128 正是本类拦截自身的错误码，
        // 上游据此识别"在讨论/回显其内部错误码"的请求。
        // 代价：用户对话中任何 11128 都会被改写——但这串数字出现在请求里本身就是拦截条件，
        // 不改写必然失败。插入连字符保留可读性与指代（零宽空格无效，实测上游会归一化）。
        "11128",
        "11-128",
    ),
];

/// 命中标签：`11128` 与表头类指纹在界面上要有个短名字（完整模板句太长，
/// 请求日志的悬停面板里会糊成一片）。改写规则取 `from` 原串，正则类取这里的短名。
const LABEL_BARE_HDR: &str = "x-anthropic-billing-header";
const LABEL_KV: &str = "cc_*=";

// ─── 正则（惰性编译一次）────────────────────────────────────
//
// `\s` 写成显式的 `[\t\n\f\r ]`、`\b` 用 `(?-u:...)` 关掉 Unicode 模式：
// Go 的 regexp 里这两个类都是**纯 ASCII** 语义，而 Rust regex 默认 Unicode 感知
// （`\b` 会把中文当词字符，与 Go 在 `中文cc_x=` 这种输入上分叉）。三处都要
// 与 Go 逐字对齐，否则「上游实测能绕过」这条结论就不再成立。

fn hdr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // 剥离层：header 键名即触发（与值无关），整段删除。
        Regex::new(r"(?i)x-anthropic-billing-header:[^;\n]*;?[\t\n\f\r ]*")
            .expect("header 剥离正则必须可编译")
    })
}

fn bare_hdr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // 兜底层：裸键名（无冒号无值）同样是指纹——实测证实 assistant 消息里
        // 反引号引用裸键名即触发 11128，而剥离层要求冒号、对裸串无效。
        // 键值形态被整段删除后，残留的裸键名做最小缩写（header→hdr）：破坏逐字
        // 匹配、语义不变、保留可读性。大小写不敏感，覆盖 X-Anthropic-... 变体。
        //
        // 注意该正则不要求冒号，是 hdr_re 的**超集**——[`has_fingerprint`] 与
        // [`sanitize_text`] 中两者并用：先删键值形态（hdr_re），再缩写残留裸键名
        // （本正则），替换语义不同（整段删除 vs 最小缩写），不可合并为一个正则。
        Regex::new(r"(?i)x-anthropic-billing-header").expect("裸 header 正则必须可编译")
    })
}

fn kv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // 剥离层：尾随裸键值（cc_xxx=...;）循环清理。
        Regex::new(r"(?i)(?-u:\b)cc_[a-z0-9_]+=[^;\n]*;?[\t\n\f\r ]*")
            .expect("cc_ 键值正则必须可编译")
    })
}

/// 特征预检：先走 `contains` 快速路径（零分配）；
/// header 键名有大小写变体（X-Anthropic-...）且可能以裸键名形态出现（无冒号），
/// `contains` 大小写敏感、`hdr_re` 要求冒号——两者都会漏掉「混合大小写 + 裸键名」，
/// 必须再用不要求冒号的 `(?i)` 正则兜底（[`bare_hdr_re`]），否则整条净化被跳过。
/// `bare_hdr_re` 不要求冒号，是 `hdr_re` 的超集，故无需再单独匹配后者。
pub fn has_fingerprint(text: &str) -> bool {
    SANITIZE_FEATURES.iter().any(|feature| text.contains(feature))
        || bare_hdr_re().is_match(text)
}

/// 一次净化里命中的规则标签与次数（喂给请求日志的 `sensitiveHits` 列）。
///
/// 改造前这一列装的是「命中了哪个敏感词」；规则集换成硬编码之后，装的是
/// **命中了哪条规则**（`11128` / `cc_*=` / 模板句原文）。请求日志的「敏」标签
/// 与悬停面板因此照常工作，不必动库表结构与前端。
#[derive(Default)]
pub struct Hits {
    entries: Vec<(String, usize)>,
}

impl Hits {
    fn bump(&mut self, label: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|(item, _)| item == label) {
            entry.1 += 1;
            return;
        }
        self.entries.push((label.to_string(), 1));
    }

    /// 按命中次数降序（稳定排序：同次数保持首次命中顺序，与旧实现同口径）
    pub fn into_sorted(self) -> Vec<(String, usize)> {
        let mut list = self.entries;
        list.sort_by(|left, right| right.1.cmp(&left.1));
        list
    }
}

/// 单段文本净化：预检不中 → 返回原串（零分配）。命中时把规则标签记进 `hits`。
///
/// 顺序照抄 Go 的 `sanitizeText`：改写层 → 表头剥离层 → cc_ 键值循环清理 →
/// 裸键名缩写兜底 → 去首尾空白。**顺序不能换**：改写在前是因为它要求原串
/// 完整（表头剥离会动到同一段文本的其它位置）；`cc_` 循环在裸键名缩写之前，
/// 是因为缩写会把键名改掉、再清 `cc_` 就认不出来了。
pub fn sanitize_text(text: &str, hits: &mut Hits) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (from, to) in SANITIZE_REWRITES {
        if out.contains(from) {
            hits.bump(from);
            out = out.replace(from, to);
        }
    }
    let hdr = hdr_re();
    if hdr.is_match(&out) {
        hits.bump(LABEL_BARE_HDR);
        out = hdr.replace_all(&out, "").into_owned();
    }
    if out.contains("cc_") {
        let kv = kv_re();
        let mut prev = String::new();
        // 清尾随裸 kv（cc_version=...; cc_entrypoint=...;）：一次替换可能因为
        // 相邻键值的新边界又暴露出下一个，循环到不动为止
        while prev != out {
            prev = out.clone();
            let next = kv.replace_all(&out, "").into_owned();
            if next != out {
                hits.bump(LABEL_KV);
            }
            out = next;
        }
    }
    // 兜底：键值形态已在上面整段删除，这里只剩裸键名（引用/示例文本形态）。
    if bare_hdr_re().is_match(&out) {
        hits.bump(LABEL_BARE_HDR);
        out = bare_hdr_re().replace_all(&out, "x-anthropic-billing-hdr").into_owned();
    }
    out.trim().to_string()
}

/// 净化 content：字符串，或多模态数组（只动带 `text` 字符串字段的分片）。
///
/// 与 Go 一致，**不判 `type`**：任何带 `text` 字符串的分片都过一遍。
/// 返回是否发生变化。
fn sanitize_content(value: &mut Value, hits: &mut Hits) -> bool {
    match value {
        Value::String(text) => {
            let next = sanitize_text(text, hits);
            if next != *text {
                *text = next;
                true
            } else {
                false
            }
        }
        Value::Array(parts) => {
            let mut changed = false;
            for part in parts.iter_mut() {
                let Some(object) = part.as_object_mut() else {
                    continue;
                };
                let Some(Value::String(text)) = object.get_mut("text") else {
                    continue;
                };
                let next = sanitize_text(text, hits);
                if next != *text {
                    *text = next;
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

/// 净化 `assistant.tool_calls[].function.arguments`。
///
/// arguments 是**字符串化的 JSON**（不是对象），因此按文本走 [`sanitize_text`] 即可。
/// 这块长期是盲区：工具调用消息的 content 通常是 null，早期实现在 content 缺失时
/// 直接跳过整条消息，于是历史里任何写进工具参数的被拦字符串（文件名、命令、
/// 写入内容）都会原样漏出。
fn sanitize_tool_calls(value: &mut Value, hits: &mut Hits) -> bool {
    let Some(call_list) = value.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for call in call_list.iter_mut() {
        let Some(call) = call.as_object_mut() else {
            continue;
        };
        let Some(function) = call.get_mut("function").and_then(Value::as_object_mut) else {
            continue;
        };
        let Some(Value::String(arguments)) = function.get_mut("arguments") else {
            continue;
        };
        let next = sanitize_text(arguments, hits);
        if next != *arguments {
            *arguments = next;
            changed = true;
        }
    }
    changed
}

/// 净化 messages 中的 content、reasoning_content 与 tool_calls；任一命中返回 true。
fn sanitize_messages(messages: &mut Value, hits: &mut Hits) -> bool {
    let Some(list) = messages.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for message in list.iter_mut() {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        // content 与 tool_calls 各自独立判断：content 可以为 null（工具调用轮），
        // 早期实现在这里跳过整条消息，导致这类消息的 tool_calls 完全不被净化。
        if let Some(content) = object.get_mut("content") {
            if sanitize_content(content, hits) {
                changed = true;
            }
        }
        // reasoning_content（思维链回填字段）实测同样携带指纹，与 content 同等净化。
        if let Some(Value::String(reasoning)) = object.get_mut("reasoning_content") {
            let next = sanitize_text(reasoning, hits);
            if next != *reasoning {
                *reasoning = next;
                changed = true;
            }
        }
        if let Some(tool_calls) = object.get_mut("tool_calls") {
            if sanitize_tool_calls(tool_calls, hits) {
                changed = true;
            }
        }
    }
    changed
}

/// 净化整个请求体（浅拷贝一份再改）。
///
/// 返回 `None` = **一个字节都没改**（调用方据此零拷贝沿用客户端原始 body）；
/// `Some((body, hits))` = 改过的副本 + 命中的规则标签。
///
/// 无 `messages` 数组时原样返回 `None`：上游只认 messages 形态，别处（顶层
/// `system`、`input` 等）不是本规则集的作用面，与 Go 的调用点一致。
pub fn sanitize_body(body: &Value) -> Option<(Value, Vec<(String, usize)>)> {
    let mut next = body.clone();
    let mut hits = Hits::default();
    let changed = match next
        .as_object_mut()
        .and_then(|object| object.get_mut("messages"))
    {
        Some(messages) => sanitize_messages(messages, &mut hits),
        None => false,
    };
    if !changed {
        return None;
    }
    Some((next, hits.into_sorted()))
}
