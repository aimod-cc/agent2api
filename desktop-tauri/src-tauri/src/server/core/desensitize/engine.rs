//! 内容脱敏引擎（对照 Node 版 src/workbuddy-desensitize.mjs 全量移植）。
//!
//! ── 干什么 ────────────────────────────────────────────────
//! 命中词内部插入零宽空格（U+200B），打断上游审核的关键词匹配，
//! 人眼与模型读到的仍是原词（"DoS" → "D\u{200b}oS"）。
//!
//! ── 为什么不用 regex crate ─────────────────────────────────
//! Node 版把词表编译成一条 `new RegExp(词1|词2|…, 'gi')`，匹配语义有三个
//! 非平凡点，用通用正则引擎反而不好控：
//!   ① 大小写折叠是 **JS 非 unicode 模式**的 Canonicalize（不是 Rust 的
//!      `to_lowercase` 全串映射，两者在 U+0130/U+017F/U+212A 等处不一致）；
//!   ② 交替是「最左优先 + 分支书写顺序」，不是最长匹配；
//!   ③ 词边界是 `(?<![A-Za-z0-9])` / `(?![A-Za-z0-9])`，仅纯 ASCII 词带。
//! 本模块用「UTF-16 码元数组 + 首码元倒排索引 + 顺序试探」等价实现：
//! 逐位置按交替顺序试词，命中即消费该词长度，完全复刻 ①②③
//! （推导见 `canonicalize` 与 `find_at` 的注释）。零依赖、零 panic 风险。
//!
//! ── 与 Node 版的两处有意偏离 ────────────────────────────────
//! （「词表文件的字段顺序与缩进字节对齐 Node」那条要求**已随本次改造消失**：
//! 词表从 `desensitize.json` 进了 `kv` 表，那份状态没有第二个读写方了，
//! 本文件里与它配套的手工序列化函数一并删除；完整论证见 `mod.rs` 模块头。）
//!   - **词条以星面字符开头**时（emoji 等代理对），Node 的
//!     `term[0]` 取到的是高代理（半个字符），`zeroWidthSplit` 就把 ZWSP 插在
//!     代理对中间，产出一个含孤立代理的非法 UTF-16 串；JSON 化后它是
//!     `"\ud83d​\ude00…"`，任何严格解析器都读不回来。Rust 的 String 必须是
//!     合法 UTF-8，物理上无法表示这种串，因此这里按「首个**完整字符**之后」
//!     插入，得到合法且人眼一致的 `😀\u200bDoS`。默认词表全是纯 ASCII，
//!     触不到这条路径；只有用户自己往词表里加 emoji 开头的词才会遇到。
//!   - 上限判定与词表校验用 UTF-16 长度（JS 的 `str.length`），不是字符数。

use std::collections::HashMap;

use serde_json::Value;

/// 零宽空格：插在词内部打断关键词匹配，不影响阅读
pub const ZWSP: char = '\u{200b}';

/// 默认处理这两种角色：system 是客户端合规模板的集中地，user 是用户真实输入
pub const DEFAULT_ROLES: &[&str] = &["system", "user"];
/// 角色白名单（归一化按这个表的顺序输出，不是按入参顺序）
pub const VALID_ROLES: &[&str] = &["system", "user", "assistant", "tool", "developer"];

/// 词表上限（与路由校验共用同一个常量）
pub const MAX_TERMS: usize = 2000;
/// 单词上限（UTF-16 长度口径）
pub const MAX_TERM_LENGTH: usize = 200;

/// 默认词表：客户端固定 system 模板里的合规声明高频词。
/// 这些词取自真实被上游审核拦下的模板（"拒绝协助 DoS 攻击 / 漏洞利用开发 /
/// 凭证测试 / C2 框架…"），属于「拒绝作恶」的声明而非有害输入，却会被误判。
/// 末三条是客户端 system 模板里被上游审核误拦的真实文案（整句/整段作为词条），
/// 其中两个「自称句」实测是 Claude Code 客户端 system 首段被拦的触发点。
/// 注意：这里必须是纯文本，零宽空格是运行时由脱敏逻辑插入的。
pub const DEFAULT_TERMS: &[&str] = &[
    "DoS",
    "DDoS",
    "exploit",
    "credential testing",
    "credential stuffing",
    "supply chain compromise",
    "supply-chain compromise",
    "detection evasion",
    "C2 frameworks",
    "C2 framework",
    "command and control",
    "malicious purposes",
    "malicious intent",
    "mass targeting",
    "brute force",
    "brute-force",
    "privilege escalation",
    "reverse shell",
    "remote code execution",
    "SQL injection",
    "XSS",
    "CSRF",
    "phishing",
    "malware",
    "ransomware",
    "keylogger",
    "rootkit",
    "backdoor",
    "botnet",
    "zero-day",
    "0day",
    "Main branch (you will usually use this for PRs)",
    "You are Claude Code",
    "Anthropic's official CLI for Claude",
];

/// 默认词表当前版本：每次往 DEFAULT_TERMS 追加新词条时 +1，并在迁移表登记
pub const DEFAULT_TERMS_VERSION: u32 = 3;

/// 默认词表补词迁移表：版本号 → 该版本新增的默认词条。
///
/// 老用户的词表（原先是 `desensitize.json`，现在是库里那份状态）记着「已合并到
/// 哪个版本」（`defaultsVersion`），默认词表更新后不会自动生效，因此按版本做
/// 一次性合并：启动读状态时把缺失的新词条按忽略大小写补进去，合并完落库并更新
/// 版本标记。用「只补登记词条」而不是「与 DEFAULT_TERMS 求并集」，是为了保护
/// 用户的删除权。
pub const DEFAULT_TERM_MIGRATIONS: &[(u32, &[&str])] = &[
    (2, &["Main branch (you will usually use this for PRs)"]),
    (3, &["You are Claude Code", "Anthropic's official CLI for Claude"]),
];

// ─── JS 语义小工具 ──────────────────────────────────────────

/// JS `String.prototype.trim` 的空白集合 = White_Space ∪ {U+FEFF}。
/// Rust 的 `char::is_whitespace` 不含 U+FEFF（ZWNBSP），差这一个字符就会让
/// 「词首带 BOM」这类输入在两边归一成不同的词，所以显式补上。
///
/// 对同层模块可见：`remove_terms` 的删词集合也走同一个 trim（Node 两处都用
/// `String.prototype.trim`），共用它才能保证「加进去的词 == 删得掉的词」。
pub(super) fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// 词的「长度」按 JS 口径：UTF-16 码元数（星面字符算 2）。
/// 对外开放：路由的长度校验（`MAX_TERM_LENGTH`）必须用同一口径。
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// JS 非 unicode 正则的 Canonicalize（ES 22.2.2.9 的简化实现）。
///
/// ```text
/// u = toUpperCase(码元)
/// 若 u 不是单个码点            → 原码元（如 'ß'.toUpperCase() === 'SS'）
/// 若 码元 >= 128 且 u < 128    → 原码元（如 U+017F LONG S 不折到 'S'）
/// 否则                        → u
/// ```
/// 两条守卫正是 V8 与「ASCII 之外不跨段折叠」的分界：没有它们，U+017F 会
/// 折成 'S'、U+212A（KELVIN）会折成 'K'，与 Node 版的实际匹配结果不符。
/// 已对 BMP 全码点 + 星面（0x10000-0x1FFFF）逐码点与 V8 实测比对，零差异。
/// 孤立代理码元（`char::from_u32` 失败）按恒等处理，与 JS 一致。
fn canonicalize(unit: u16) -> u16 {
    let Some(ch) = char::from_u32(unit as u32) else {
        return unit;
    };
    let mut upper = ch.to_uppercase();
    let Some(first) = upper.next() else {
        return unit;
    };
    if upper.next().is_some() {
        return unit;
    }
    if unit >= 128 && (first as u32) < 128 {
        return unit;
    }
    u16::try_from(first as u32).unwrap_or(unit)
}

/// 词边界字符集 `[A-Za-z0-9]`。
///
/// 该字符类在 `i` 标志下也**只有 ASCII 会被匹配**：非 ASCII 码元若其大写形
/// 是 ASCII，`canonicalize` 的守卫会把它折回自身（U+017F/U+0131 就是这样），
/// 所以边界判定直接按码元判 ASCII 即可，无需再折叠。
fn is_ascii_alnum(unit: u16) -> bool {
    matches!(unit, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a)
}

// ─── 词表清洗 ───────────────────────────────────────────────

/// 清洗词表：去空白、丢弃空词与超长词、按忽略大小写去重（保留首次出现的写法）、
/// 截断到上限。顺序即交替顺序的基底（等长词保持这里的先后）。
pub fn normalize_terms(input: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut result: Vec<String> = Vec::new();
    for raw in input {
        let term = js_trim(raw);
        if term.is_empty() || utf16_len(term) > MAX_TERM_LENGTH {
            continue;
        }
        let key = term.to_lowercase();
        if seen.iter().any(|item| item == &key) {
            continue;
        }
        seen.push(key);
        result.push(term.to_string());
        if result.len() >= MAX_TERMS {
            break;
        }
    }
    result
}

/// 角色白名单过滤：非数组走默认；数组则按 VALID_ROLES 顺序取交集；
/// 交集为空也走默认（对应 Node 的 `roles.length ? roles : DEFAULT_ROLES`）。
pub fn normalize_roles(input: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = input else {
        return DEFAULT_ROLES.iter().map(|role| role.to_string()).collect();
    };
    let roles: Vec<String> = VALID_ROLES
        .iter()
        .filter(|role| items.iter().any(|item| item.as_str() == Some(**role)))
        .map(|role| role.to_string())
        .collect();
    if roles.is_empty() {
        return DEFAULT_ROLES.iter().map(|role| role.to_string()).collect();
    }
    roles
}

// ─── 匹配构造（compileTerms 的等价实现）────────────────────

/// 编译后的词条：大写折叠后的码元序列 + 是否带 ASCII 词边界
struct CompiledTerm {
    units: Vec<u16>,
    bounded: bool,
}

/// 词表 → 匹配器。词表为空返回 None（调用方据此跳过脱敏）。
pub struct TermMatcher {
    terms: Vec<CompiledTerm>,
    /// 首码元 → 词条下标（按交替顺序）。
    /// 用来把「逐位置试全部词条」降到「只试首码元相同的候选」——
    /// 首码元不同的词条在该位置必然不匹配，剪枝不改变匹配结果。
    by_first_unit: HashMap<u16, Vec<usize>>,
}

/// 编译词表（对应 Node 的 compileTerms）。
///
/// 与 Node 的两处对照：
///   - 长度排序用 **UTF-16 长度降序 + 稳定排序**（Node 的
///     `sort((a,b) => b.length - a.length)` 已由规范保证稳定，等长词保序）；
///   - ASCII 判定是 `^[\x20-\x7e]+$`（纯可打印 ASCII），含换行/中文/emoji
///     的词只做子串匹配（中文没有词边界）。
pub fn compile_terms(terms: &[String]) -> Option<TermMatcher> {
    let list = normalize_terms(terms);
    if list.is_empty() {
        return None;
    }
    let mut ordered: Vec<&String> = list.iter().collect();
    ordered.sort_by(|left, right| utf16_len(right).cmp(&utf16_len(left)));

    let mut compiled: Vec<CompiledTerm> = Vec::with_capacity(ordered.len());
    let mut by_first_unit: HashMap<u16, Vec<usize>> = HashMap::new();
    for term in ordered {
        let units: Vec<u16> = term.encode_utf16().map(canonicalize).collect();
        let Some(first) = units.first().copied() else {
            continue; // 空词已被 normalize_terms 滤掉，这里只是防御
        };
        let bounded = !term.is_empty()
            && term
                .chars()
                .all(|c| ('\u{20}'..='\u{7e}').contains(&c));
        by_first_unit.entry(first).or_default().push(compiled.len());
        compiled.push(CompiledTerm { units, bounded });
    }
    if compiled.is_empty() {
        return None;
    }
    Some(TermMatcher { terms: compiled, by_first_unit })
}

impl TermMatcher {
    /// 位置 `pos`（UTF-16 码元下标，`haystack` 已折叠）处按交替顺序找词。
    ///
    /// 返回命中的词条下标。逐分支试探的顺序就是 Node 正则的交替顺序：
    /// 「最左优先」由逐位置推进保证，「分支书写顺序」由本循环保证
    /// （长词已排序在前，短词不会抢在长词前面匹配）。
    fn find_at(&self, haystack: &[u16], pos: usize) -> Option<usize> {
        let first = *haystack.get(pos)?;
        let candidates = self.by_first_unit.get(&first)?;
        for index in candidates {
            let Some(term) = self.terms.get(*index) else {
                continue;
            };
            let end = pos + term.units.len();
            // 走 get() 而不是直接切片：界内判断与切片绑在一处，
            // 不给「以后有人调宽了条件」留 panic 的口子（release 是 panic=abort）
            let Some(window) = haystack.get(pos..end) else {
                continue;
            };
            if term.units != window {
                continue;
            }
            if term.bounded {
                // (?<![A-Za-z0-9]) / (?![A-Za-z0-9])：不含前瞻自身的消费，
                // 命中位置只看词首前一个码元与词尾后一个码元
                if pos > 0 && is_ascii_alnum(haystack[pos - 1]) {
                    continue;
                }
                if let Some(next) = haystack.get(end) {
                    if is_ascii_alnum(*next) {
                        continue;
                    }
                }
            }
            return Some(*index);
        }
        None
    }

    /// 该词条在 UTF-16 口径下的长度（= 匹配消费的码元数）
    pub(super) fn length_of(&self, index: usize) -> usize {
        self.terms.get(index).map(|term| term.units.len()).unwrap_or(0)
    }
}

// ─── 命中计数 ───────────────────────────────────────────────

/// 一次请求内的命中统计。键是**原文里匹配到的串**（不是词条：
/// 忽略大小写匹配时 "DoS" 与 "dos" 是两个不同的键，与 Node 的 Map 一致），
/// 顺序为首次命中顺序；同次数排序时保持该顺序（稳定排序）。
#[derive(Default, Clone)]
pub struct Counter {
    pub total: usize,
    entries: Vec<(String, usize)>,
}

impl Counter {
    fn bump(&mut self, matched: &str) {
        self.total += 1;
        if let Some(item) = self.entries.iter_mut().find(|(term, _)| term == matched) {
            item.1 += 1;
            return;
        }
        self.entries.push((matched.to_string(), 1));
    }

    /// 按命中次数降序（稳定排序：同次数保持首次命中顺序）
    pub(super) fn term_counts(&self) -> Vec<(String, usize)> {
        let mut list = self.entries.clone();
        list.sort_by(|left, right| right.1.cmp(&left.1));
        list
    }

    /// 命中的词列表，**首次命中顺序**（对应 Node 的 `[...counter.terms.keys()]`）。
    /// 与 term_counts 不同：那个按次数降序，这个保持 Map 的插入顺序，两者都要照抄。
    pub(super) fn matched_terms(&self) -> Vec<String> {
        self.entries.iter().map(|(term, _)| term.clone()).collect()
    }
}

/// 单段文本脱敏；matcher 为 None 时原样返回。
pub fn desensitize_text(text: &str, matcher: Option<&TermMatcher>, counter: &mut Counter) -> String {
    let Some(matcher) = matcher else {
        return text.to_string();
    };
    if text.is_empty() {
        return text.to_string();
    }
    // 折叠后的码元序列 + 码元下标 → 字节偏移表（末位是 text.len()）。
    // 用 UTF-16 而不是 char 索引，是因为非 unicode 正则的每个「字符」
    // 就是一个码元（星面字符占两个位置）。
    let haystack: Vec<u16> = text.encode_utf16().map(canonicalize).collect();
    let mut offsets: Vec<usize> = Vec::with_capacity(haystack.len() + 1);
    for (byte_index, ch) in text.char_indices() {
        for _ in 0..ch.len_utf16() {
            offsets.push(byte_index);
        }
    }
    offsets.push(text.len());

    let mut out = String::with_capacity(text.len() + 16);
    let mut copied = 0usize;
    let mut pos = 0usize;
    while pos < haystack.len() {
        let Some(index) = matcher.find_at(&haystack, pos) else {
            pos += 1;
            continue;
        };
        let length = matcher.length_of(index);
        if length == 0 {
            pos += 1;
            continue;
        }
        let (Some(&start), Some(&end)) = (offsets.get(pos), offsets.get(pos + length)) else {
            pos += 1;
            continue;
        };
        // 所有切片都走 get()：词条本身是合法 UTF-8，正常不会落在字符中间，
        // 但 release 是 panic=abort，宁可跳过一次命中也不冒切片 panic 的风险
        let (Some(matched), Some(rest)) = (text.get(start..end), text.get(start..end)) else {
            pos += 1;
            continue;
        };
        counter.bump(matched);
        out.push_str(text.get(copied..start).unwrap_or(""));
        // ZWSP 插在**首个完整字符**之后（等价于 Node 的 zeroWidthSplit，
        // 星面词首的差异见模块头注释）。单字符命中直接追加，与 Node 一致。
        // 注意 tail 只能取自**命中片段内部**（rest 本身就是命中片段）——
        // 若按「起点到文末」切片，会把后续内容重复写进输出。
        let head = rest.chars().next().map(char::len_utf8).unwrap_or(0);
        let (Some(lead), Some(tail)) = (rest.get(..head), rest.get(head..)) else {
            pos += 1;
            continue;
        };
        out.push_str(lead);
        out.push(ZWSP);
        out.push_str(tail);
        copied = end;
        pos += length;
    }
    out.push_str(text.get(copied..).unwrap_or(""));
    out
}

/// 处理 OpenAI content：字符串，或 `[{type:'text', text}, ...]` 多模态数组。
/// 返回 (新值, 是否改动)。
///
/// 非字符串且非数组的 content（null / 数字 / 对象 / content 缺失）原样返回，
/// 与 Node 的 `typeof content === 'string' || !Array.isArray(content)` 分支一致。
fn scrub_content(content: &Value, matcher: &TermMatcher, counter: &mut Counter) -> (Value, bool) {
    match content {
        Value::String(text) => {
            let next = desensitize_text(text, Some(matcher), counter);
            if next == *text {
                (content.clone(), false)
            } else {
                (Value::String(next), true)
            }
        }
        Value::Array(blocks) => {
            let mut touched = false;
            let mut next: Vec<Value> = Vec::with_capacity(blocks.len());
            for block in blocks {
                // 只处理 { type: 'text', text: '...' }：图片/工具调用等其它分片不动
                let Some(object) = block.as_object() else {
                    next.push(block.clone());
                    continue;
                };
                let is_text = object.get("type").and_then(Value::as_str) == Some("text");
                let Some(Value::String(text)) = object.get("text") else {
                    next.push(block.clone());
                    continue;
                };
                if !is_text {
                    next.push(block.clone());
                    continue;
                }
                let scrubbed = desensitize_text(text, Some(matcher), counter);
                if scrubbed == *text {
                    next.push(block.clone());
                    continue;
                }
                touched = true;
                // 对应 Node 的 `{ ...block, text }`：键集合不变，只换 text 值
                let mut copy = object.clone();
                copy.insert("text".to_string(), Value::String(scrubbed));
                next.push(Value::Object(copy));
            }
            if touched {
                (Value::Array(next), true)
            } else {
                (content.clone(), false)
            }
        }
        _ => (content.clone(), false),
    }
}

/// 对指定角色的消息做脱敏。返回 Some(新 messages) 表示有改动。
///
/// role 缺失 / 非字符串 / 不在 roles 内 → 该条不动（对应 `roles.includes(role)`）。
pub fn desensitize_messages(
    messages: &Value,
    matcher: Option<&TermMatcher>,
    roles: &[String],
    counter: &mut Counter,
) -> Option<Value> {
    let Some(matcher) = matcher else {
        return None;
    };
    let Some(list) = messages.as_array() else {
        return None;
    };
    let mut touched = false;
    let mut next: Vec<Value> = Vec::with_capacity(list.len());
    for message in list {
        let Some(object) = message.as_object() else {
            next.push(message.clone());
            continue;
        };
        let role_allowed = object
            .get("role")
            .and_then(Value::as_str)
            .map(|role| roles.iter().any(|item| item == role))
            .unwrap_or(false);
        if !role_allowed {
            next.push(message.clone());
            continue;
        }
        let content = object.get("content").cloned().unwrap_or(Value::Null);
        let (scrubbed, changed) = scrub_content(&content, matcher, counter);
        if !changed {
            next.push(message.clone());
            continue;
        }
        touched = true;
        let mut copy = object.clone();
        copy.insert("content".to_string(), scrubbed);
        next.push(Value::Object(copy));
    }
    if touched {
        Some(Value::Array(next))
    } else {
        None
    }
}

/// 对请求体做脱敏（浅拷贝），返回 Some 表示 messages 有改动。
/// 无 messages 数组时原样返回（对应 Node 的 `!Array.isArray(body.messages)`）。
pub fn desensitize_body(
    body: &Value,
    matcher: Option<&TermMatcher>,
    roles: &[String],
    counter: &mut Counter,
) -> Option<Value> {
    let messages = body.get("messages")?;
    if !messages.is_array() {
        return None;
    }
    let scrubbed = desensitize_messages(messages, matcher, roles, counter)?;
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    object.insert("messages".to_string(), scrubbed);
    Some(next)
}
