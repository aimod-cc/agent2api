//! CodeArts 的**函数名超限改写**：上游对 `tools[].function.name` 有 64 个字符的硬限。
//!
//! ── 这条限制长什么样（2026-10-10 在 dev 的一批 codearts 独占模型上量的）──
//! 超限时报的错误码与「体积超 6 MiB」「输出上限超 65536」都不同，是第三个：
//!
//! ```text
//! InferHub.001001017.413：Function name [<名字>] length over limit, maximum is 64
//! ```
//!
//! 发出后约 0.3 秒回来，属于**收请求前**的预校验：被拒的那几发在 `requests` 里
//! `prompt/completion/total_tokens` 全是 0，不消耗产出。实测 64 过 / 66 拒（两边都打过）；
//! 65 没单独打点，但上游文案自己说 `maximum is 64`。
//!
//! 撞上的真实形状就是 MCP 插件名：`mcp__plugin_android-emulator_android-emulator__` 光前缀
//! 就 47 字符，插件里任何超过 17 字符的函数名必然超 —— 一发真实编码会话（出站体 139 KB）
//! 的名单里，超 64 字符的至少 12 个，最长那个 71 字符（`…android_discover_project`）。
//!
//! ── 为什么在网关这一侧改名 ──────────────────────────────────────────
//! 参考实现（CPA 的 codearts 插件与它上游的那份 TS）**都没处理这条**：它们要么原样发
//! `tools`，要么走 DSML 那条路整个**不发** `tools`。DSML 为的是「标准 tool_calls 要一次性
//! 打包参数 ⇒ SSE 长时间无数据 ⇒ APIG 约 60 秒空闲断连」，不是名字长度 —— TS 里
//! `needsDsmlToolMode(model, _toolNames)` 收了工具名参数却刻意不用，判据只按模型分。
//! 所以这里没有可照抄的先例，只能自己定三条口径：
//!
//!   · **只在真需要时动手**：名单里没有一个超限名 ⇒ [`shorten`] 返回 `None`，
//!     请求体与响应流都逐字节原样走（改流的风险远大于「少改一次名」的收益）；
//!   · **纯截断不安全**：上游**不判重名** —— `tools` 里放两个完全同名的函数也回 200，
//!     于是截断撞名不会报错，只会让模型无从区分（比报错更坏）。所以截短后补哈希后缀，
//!     并且对「客户端本来就有的名字」一起查重；
//!   · **只改 `tools` 定义，不改历史**：`messages` 里 assistant 的
//!     `tool_calls[].function.name` 给 71 字符照样 200，而模型回的是 `tools` 里那份短名
//!     ⇒ 还原只需覆盖响应侧。
//!
//! ── 另一道墙：指名某函数的 `tool_choice`（不由本模块治）───────────────
//! `tool_choice` 用**指名某函数**的对象形态（`{"type":"function","function":{"name":…}}`）
//! 在这条通道必 502，错误码是 `InferHub.001001005.400 The request param is invalid`
//! —— **不是**名字超限那个 `001001017.413`，与名字长短无关（同一套短名换成 `auto` 就 200）。
//! 那一格由 [`super::chat::fold_forced_tool_choice`] 治：出站前折成字符串 `required`。
//! 两道都在签名之前，改名在前（`forward_conversation` 里）、折叠在后
//! （`build_upstream_request` 里），所以点名那份不会被改名搅浑。
//!
//! ── 一条已知缺口 ───────────────────────────────────────────────────
//! 上限按**字符**数判（实测全是 ASCII，字符与字节分不开）。非 ASCII 的名字上游按哪个算
//! 没测过 —— 生成的短名保持 ASCII，所以不会新增这一类；客户端给的超长非 ASCII 名
//! 若上游按字节判，可能被我们放过而后被拒，那种失败会原样透传，不会更坏。

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::Value;
use sha1::{Digest, Sha1};

/// 上游允许的函数名长度（字符数）。依据见模块头。
pub const MAX_NAME_CHARS: usize = 64;

/// 哈希后缀的宽度序列：先试 7 位，撞上已有的名字就加长。
/// 每一档都保证「前缀 + 一个下划线 + 哈希」正好落在上限内。
const HASH_WIDTHS: [usize; 3] = [7, 10, 14];

/// 一个 SSE 事件（以空行收尾）允许缓冲的上限。超过就整段原样放行 ——
/// 宁可不还原，也不把流卡住、更不让上游的一帧决定我们的内存占用。
/// 与 `chat::UsageSniffer::MAX_LINE_BYTES` 同一个方向；区别是那边只旁路看流，
/// 这边改的就是流本身，所以「丢缓冲」必须等于「放行」而不是丢数据。
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// 把超限的 `tools[].function.name` 改短，并交出还原器。
///
/// 返回 `None` 表示**什么都没改**（一个超限名都没有）：调用方据此走逐字节透传那条路。
pub fn shorten(body: &mut Value) -> Option<Restorer> {
    // 没有 tools 就直接不动（`?` 而不是 let-else：这里两种情况的结论都是「返回 None」）
    let tools = body.get_mut("tools").and_then(Value::as_array_mut)?;
    // 判重的底账：客户端本来就有的名字一个都不许占用 —— 生成的短名撞上真名，
    // 等于把两个不同工具并成一个，而上游不报错（见模块头第二条）
    let mut taken: HashSet<String> = tools.iter().filter_map(function_name).map(str::to_owned).collect();
    let mut sent_to_original = HashMap::new();
    for tool in tools.iter_mut() {
        let Some(original) = function_name(tool).map(str::to_owned) else {
            continue;
        };
        if original.chars().count() <= MAX_NAME_CHARS {
            continue;
        }
        let Some(sent) = short_name(&original, &mut taken) else {
            // 三档哈希都没避开（要三个不同名字共享前 50 来字符并且哈希也撞）：
            // **不改**它。让上游照旧拒这一发，比替客户端造一个可能错的名字安全
            continue;
        };
        if let Some(object) = tool.get_mut("function").and_then(Value::as_object_mut) {
            object.insert("name".to_string(), Value::String(sent.clone()));
        }
        sent_to_original.insert(sent, original);
    }
    (!sent_to_original.is_empty()).then_some(Restorer { sent_to_original })
}

/// `tools[i].function.name`（形状不对或空串就当没有）
fn function_name(tool: &Value) -> Option<&str> {
    tool.get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
}

/// 截短到上限以内并带一个可判重的哈希后缀；避不开返回 `None`（调用方就不改这个名字）。
fn short_name(original: &str, taken: &mut HashSet<String>) -> Option<String> {
    let digest = Sha1::digest(original.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    for width in HASH_WIDTHS {
        // 前缀留出的字符数：上限减去「一个下划线 + width 位哈希」
        let head = MAX_NAME_CHARS.checked_sub(width + 1)?;
        let prefix: String = original.chars().take(head).collect();
        let candidate = format!("{prefix}_{}", &hex[..width]);
        if taken.insert(candidate.clone()) {
            return Some(candidate);
        }
    }
    None
}

/// 响应侧的名字还原器：`我们发出去的名字 -> 客户端原来的名字`。
#[derive(Debug, Clone)]
pub struct Restorer {
    sent_to_original: HashMap<String, String>,
}

impl Restorer {
    /// 还原一个 chunk 里的工具名（流式的 `choices[].delta.tool_calls[]` 与非流式的
    /// `choices[].message.tool_calls[]` 两种形状都覆盖）。返回是否动过。
    pub fn apply(&self, chunk: &mut Value) -> bool {
        let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) else {
            return false;
        };
        let mut touched = false;
        for choice in choices.iter_mut() {
            for slot in ["delta", "message"] {
                let Some(calls) = choice
                    .get_mut(slot)
                    .and_then(|part| part.get_mut("tool_calls"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                for call in calls.iter_mut() {
                    let Some(object) = call.get_mut("function").and_then(Value::as_object_mut) else {
                        continue;
                    };
                    // 先借出去查，拿到所有权再写回：同一处两次借用会打架
                    let original = object
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(|sent| self.sent_to_original.get(sent))
                        .cloned();
                    if let Some(original) = original {
                        object.insert("name".to_string(), Value::String(original));
                        touched = true;
                    }
                }
            }
        }
        touched
    }

    /// 非流式那份折叠好的回答（`chat::aggregate_sse` 的产物）。
    pub fn apply_completion(&self, completion: &mut Value) {
        self.apply(completion);
    }

    /// 一个 SSE 事件的字节 → 还原后的字节。
    ///
    /// **只有当事件里确实出现「我们发出去的那个短名」时才重新序列化**，其余一律返回原字节。
    /// 这条是默认路径零改动的保障：重新序列化会吃掉上游的空白形状，而 `[DONE]`、心跳、
    /// 正文帧本来跟我们这件事无关。
    fn rewrite_event(&self, event: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(event);
        // 快速排除：绝大多数帧里根本没有 tool_calls
        if !text.contains("tool_calls") {
            return event.to_vec();
        }
        let mut out = Vec::with_capacity(event.len());
        let mut touched = false;
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            // 行尾那几个换行要按「去掉 body 之后剩下的后缀」取：写成
            // `&line[..len-body.len()]` 会拿到行首，每帧尾巴就变成 `d`（"data:" 的首字母）
            let trailing = &line[body.len()..];
            let Some(payload) = body.strip_prefix("data:") else {
                // 不是 data 行（`id:`、注释、收尾空行）：原样
                out.extend_from_slice(line.as_bytes());
                continue;
            };
            // `data:` 后面那几个空格是上游的形状，替换时要原样还回去
            let spaces = payload.len() - payload.trim_start().len();
            let value_text = payload.trim_start();
            let replaced = value_text
                .parse::<Value>()
                .ok()
                .and_then(|mut value| self.apply(&mut value).then_some(value));
            match replaced {
                Some(value) => {
                    out.extend_from_slice(b"data:");
                    out.extend(std::iter::repeat_n(b' ', spaces));
                    out.extend_from_slice(value.to_string().as_bytes());
                    out.extend_from_slice(trailing.as_bytes());
                    touched = true;
                }
                // 这一行没命中：连上游的空白都不动
                None => out.extend_from_slice(line.as_bytes()),
            }
        }
        if touched { out } else { event.to_vec() }
    }
}

/// 把 [`Restorer`] 挂到一条流上的成帧缓冲。
///
/// 上游的 SSE 事件以空行收尾，但一次读到的字节可能停在事件中间 —— 那时必须攥着等下一片，
/// 否则改名会看漏半条 JSON。没有 restorer 时**完全不缓冲**（原样返回借来的字节）。
pub struct StreamFixer {
    restorer: Option<Restorer>,
    pending: Vec<u8>,
}

impl StreamFixer {
    pub fn new(restorer: Option<Restorer>) -> Self {
        Self { restorer, pending: Vec::new() }
    }

    /// 有没有活要干。`true` 时调用方可以照旧逐字节透传，不必走 [`Self::push`]。
    pub fn idle(&self) -> bool {
        self.restorer.is_none()
    }

    /// 吃进一段字节，返回现在可以发出去的那部分。
    pub fn push<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(restorer) = self.restorer.as_ref() else {
            return Cow::Borrowed(bytes);
        };
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(edge) = find_event_end(&self.pending) {
            let event: Vec<u8> = self.pending.drain(..edge).collect();
            out.extend(restorer.rewrite_event(&event));
        }
        if self.pending.len() > MAX_EVENT_BYTES {
            // 一帧长到没边（或上游不用空行收尾）：原样放行，别攥着
            out.extend(std::mem::take(&mut self.pending));
        }
        Cow::Owned(out)
    }

    /// 流结束时把最后一段交出去。半条事件还原不了就**原样放行**：客户端至少不少字节。
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// 事件结尾（空行）之后的位置；`None` = 还没收全。
fn find_event_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|pair| pair == b"\n\n").map(|at| at + 2)
}

#[cfg(test)]
mod name_limit {
    use super::*;

    fn body_with(names: &[&str]) -> Value {
        let tools: Vec<Value> = names
            .iter()
            .map(|name| serde_json::json!({ "function": { "name": name } }))
            .collect();
        serde_json::json!({ "tools": tools })
    }

    /// 一个流式帧：`choices[0].delta.tool_calls[0].function.name` = 给定名字。
    /// 用 json! 造而不是手搓转义 —— 上一版手写花括号数错，三条用例全在测"坏 JSON"。
    fn delta_frame(name: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "function": {"name": name, "arguments": "{\"q\":\"ok\"}"}
            }]}}]
        });
        format!("data: {chunk}\n\n")
    }

    fn names_of(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    const PREFIX: &str = "mcp__plugin_android-emulator_android-emulator__";
    const LONG: &str = "mcp__plugin_android-emulator_android-emulator__android_discover_project";

    #[test]
    fn nothing_is_touched_when_no_name_is_over_the_limit() {
        // 默认路径必须零改动：这条钉的是「shorten 交回 None」，而不是「交了个空还原器」
        // —— 后者会让流白白走一遍成帧与 JSON 解析
        let mut body = body_with(&["short_enough", "a_bit_longer_but_still_inside_the_limit_ok"]);
        let before = body.clone();
        assert!(shorten(&mut body).is_none());
        assert_eq!(before, body, "没超限就不该动请求体一个字节");
    }

    #[test]
    fn the_boundary_is_the_limit_itself() {
        // 到线值原样走、超一个才改 —— 两边都钉住，否则判据写成 >= 也照样全绿
        let exactly = "x".repeat(MAX_NAME_CHARS);
        let over = format!("{exactly}y");
        let mut body = body_with(&[&exactly, &over]);
        let restorer = shorten(&mut body).expect("有超限的名字就该给还原器");
        let sent = names_of(&body);
        assert_eq!(exactly, sent[0], "到线值不该被动");
        assert_eq!(1, restorer.sent_to_original.len(), "只改那一个超限的");
        assert!(sent[1].chars().count() <= MAX_NAME_CHARS, "改完必须落在上限内：{}", sent[1]);
        assert_eq!(over, restorer.sent_to_original[&sent[1]]);
    }

    #[test]
    fn two_names_sharing_a_long_prefix_do_not_collapse_into_one() {
        // 上游不判重名：截断撞名不会报错，只会让模型无从区分 —— 所以必须自己分开
        let a = format!("{PREFIX}android_start_emulator");
        let b = format!("{PREFIX}android_stop_emulator");
        assert_eq!(a[..47], b[..47], "夹具得真的共享前缀，否则这条测的是别的东西");
        let mut body = body_with(&[&a, &b]);
        let restorer = shorten(&mut body).expect("两个都超限");
        let sent = names_of(&body);
        assert_ne!(sent[0], sent[1], "共享前缀的两个名字改完必须还彼此不同：{sent:?}");
        assert_eq!(a, restorer.sent_to_original[&sent[0]], "还原要各回各自的原名");
        assert_eq!(b, restorer.sent_to_original[&sent[1]]);
    }

    #[test]
    fn a_generated_name_never_borrows_a_name_the_client_already_sent() {
        // 真实风险：客户端自己就有一个 64 字符的名字，恰好等于我们会给别的名字生成的那个。
        // 先按算法算出「victim 会生成成什么」，再把它当成客户端已有名塞回去 —— 于是判重
        // 必须把它绕开（换成更长的哈希档）。
        let victim = format!("{}{}", "p".repeat(MAX_NAME_CHARS - 8), "over_the_limit_name_here");
        assert!(victim.chars().count() > MAX_NAME_CHARS);
        let first_choice = short_name(&victim, &mut HashSet::new()).expect("单次生成不该失败");
        assert_eq!(MAX_NAME_CHARS, first_choice.chars().count());
        let mut body = body_with(&[&first_choice, &victim]);
        let restorer = shorten(&mut body).expect("victim 超限");
        let sent = names_of(&body);
        assert_eq!(first_choice, sent[0], "没超限的真名原样留着");
        assert_ne!(sent[0], sent[1], "生成的名字不许撞上客户端本来就有的名字：{sent:?}");
        assert_eq!(victim, restorer.sent_to_original[&sent[1]], "换了哈希档也要能还原回去");
    }

    #[test]
    fn the_restored_stream_frame_carries_the_clients_own_name() {
        let mut body = body_with(&[LONG]);
        let restorer = shorten(&mut body).expect("该改");
        let sent = names_of(&body)[0].clone();
        let frame = delta_frame(&sent);
        assert!(serde_json::from_str::<Value>(frame[6..].trim_end()).is_ok(), "夹具自己得是合法 JSON：{frame}");
        let out = String::from_utf8(restorer.rewrite_event(frame.as_bytes())).unwrap();
        assert!(out.contains(LONG), "还原后要看见客户端原来的名字：{out}");
        assert!(!out.contains(&format!("\"{sent}\"")), "改短的那个名字不许留在流里");
        assert!(out.contains("ok"), "同帧里的 arguments 不能被弄丢");
        assert!(out.starts_with("data: "), "`data: ` 的空格形状要保持（上游就是这么写的）");
    }

    #[test]
    fn frames_without_a_hit_are_returned_byte_for_byte() {
        // 「默认路径零改动」的另一半：没真命中时连空白形状都不许变
        let mut body = body_with(&[LONG]);
        let restorer = shorten(&mut body).expect("该改");
        let odd = "data:{\"choices\":[{\"delta\":{\"content\":\"tool_calls 这个词出现在正文里也不算命中\"}}]}\n\n";
        assert_eq!(
            restorer.rewrite_event(odd.as_bytes()).as_slice(),
            odd.as_bytes(),
            "含 tool_calls 字样但没有真名字命中 —— 必须逐字节原样"
        );
        let done = b"data: [DONE]\n\n";
        assert_eq!(restorer.rewrite_event(done).as_slice(), done, "[DONE] 不该被重新序列化");
    }

    #[test]
    fn a_completion_is_restored_in_the_message_shape_too() {
        let mut body = body_with(&[LONG]);
        let restorer = shorten(&mut body).expect("该改");
        let sent = names_of(&body)[0].clone();
        let mut completion: Value =
            serde_json::json!({"choices": [{"message": {"tool_calls": [{"function": {"name": sent}}]}}]});
        restorer.apply_completion(&mut completion);
        assert_eq!(LONG, completion["choices"][0]["message"]["tool_calls"][0]["function"]["name"]);
    }

    #[test]
    fn a_frame_split_across_reads_is_still_restored() {
        // 上游一次 write 的边界不由我们定：半帧必须攥着等下一片
        let mut body = body_with(&[LONG]);
        let restorer = shorten(&mut body).expect("该改");
        let sent = names_of(&body)[0].clone();
        let frame = delta_frame(&sent);
        let bytes = frame.as_bytes();
        let cut = bytes.len() / 2;
        let mut fixer = StreamFixer::new(Some(restorer));
        let mut out = fixer.push(&bytes[..cut]).into_owned();
        assert!(out.is_empty(), "半帧不该先放出去（客户端会看到半条 JSON）");
        out.extend(fixer.push(&bytes[cut..]).into_owned());
        out.extend(fixer.finish());
        assert!(out.starts_with(b"data: ") && out.ends_with(b"\n\n"), "成帧形状要保持原样");
        let restored = String::from_utf8(out).unwrap();
        assert!(restored.contains(LONG) && !restored.contains(&format!("\"{sent}\"")), "{restored}");
    }

    #[test]
    fn the_idle_fixer_buffers_nothing_and_copies_nothing() {
        // 没改过名字时（绝大多数请求）这一路必须与原样透传逐字相同，且不吞字节
        let mut fixer = StreamFixer::new(None);
        assert!(fixer.idle());
        let bytes = b"data: {\"x\":1}\n\ndata: partial";
        let out = fixer.push(bytes);
        assert!(matches!(out, Cow::Borrowed(inner) if inner == &bytes[..]), "idle 时不该走缓冲那条路");
        assert!(fixer.finish().is_empty(), "idle 时不该攥着任何字节");
    }

    #[test]
    fn an_overlong_event_is_flushed_untouched_instead_of_held() {
        // 上游给一个大到没边的帧：宁可不还原，也不能把流卡住或把内存交出去
        let mut body = body_with(&[LONG]);
        let restorer = shorten(&mut body).expect("该改");
        let mut fixer = StreamFixer::new(Some(restorer));
        let huge = vec![b'a'; MAX_EVENT_BYTES + 10];
        let out = fixer.push(&huge).into_owned();
        assert!(fixer.pending.is_empty(), "超上限的缓冲要清空");
        assert_eq!(huge.len(), out.len(), "整段放行，不是丢掉");
        assert!(out.iter().all(|byte| *byte == b'a'));
    }

    #[test]
    fn the_real_shape_from_the_field_all_names_survive_untangled() {
        // 一手形状：13 个真实函数名共用那个 47 字符前缀，逐个都超 64
        let tails = [
            "android_discover_project",
            "android_create_app",
            "android_build_and_run",
            "android_list_devices",
            "android_start_emulator",
            "android_stop_emulator",
            "android_create_avd",
            "android_install_app",
            "android_launch_app",
            "android_terminate_app",
            "android_screenshot",
            "android_ui_describe",
            "android_ui_resolve",
        ];
        let names: Vec<String> = tails.iter().map(|tail| format!("{PREFIX}{tail}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let over = names.iter().filter(|name| name.chars().count() > MAX_NAME_CHARS).count();
        assert_eq!(13, over, "夹具要真的是那 13 个超限名");
        let mut body = body_with(&refs);
        let restorer = shorten(&mut body).expect("这一组里确有超限名");
        let sent = names_of(&body);
        assert!(sent.iter().all(|name| name.chars().count() <= MAX_NAME_CHARS));
        let unique: HashSet<&String> = sent.iter().collect();
        assert_eq!(refs.len(), unique.len(), "改完的名字两两不许相同：{sent:?}");
        for (index, name) in names.iter().enumerate() {
            if name.chars().count() > MAX_NAME_CHARS {
                assert_eq!(*name, restorer.sent_to_original[&sent[index]], "{name} 还原不回去");
            } else {
                assert_eq!(name, &sent[index], "到线的名字不许被改");
            }
        }
    }
}
