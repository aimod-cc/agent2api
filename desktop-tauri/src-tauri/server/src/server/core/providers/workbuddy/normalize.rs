//! WorkBuddy 出站请求体归一化管线（对照 Sliverkiss/workbuddy2api 的
//! `internal/upstream/payload.go` + `tool_pairing.go` + `cache_key.go` 移植）。
//!
//! ── 为什么需要这一层 ────────────────────────────────────────
//! 上游是「腾讯自己的 Go struct」，对请求体的宽容度远小于 OpenAI 规范：
//! role 白名单没有 `developer`、`tool_choice` 只认字符串、`image_url` 只认对象
//! 形态、输出上限只认 `max_tokens`。客户端（Codex / Claude Code / 各家 UI）
//! 按新规范发字段时上游直接 400，而**换任何账号都是同一个 400** ——
//! 这不是账号问题、也不是重试能解决的问题，只能在出站前把字节改对。
//!
//! 参考项目用一条**单上游**管线做这件事（所有请求都经 `prepareBody`）。本项目
//! 是七家多上游架构，「上游长什么样」是每家的知识（见 `adapter.rs` 模块头），
//! 所以这里只做 **WorkBuddy 这一家**的归一化，挂在本家适配器的
//! `build_chat_request` 上 —— 那个位置恰好就是「这一家即将发送之前」（与
//! `upstream::payload::send_body` 的时机契约一致），且天然对同一家同池的
//! 重试复用（`provider_loop` 的 `send_cache`）。
//!
//! ── 与 system 兜底注入的顺序（不可调换）────────────────────────
//! `normalize_roles` 必须**先**把 `developer` 归一成 `system`，随后适配器才调
//! `ensure_leading_system_message` —— 客户端若用 `developer` 打头，先归一成
//! `system` 才判得出「首条已是 system」，否则会多注入一条兜底 system
//! （上游对多 system 的行为未实测，不引入这个变量）。参考项目也是同一顺序
//! （`ensureConsoleSystem` 在 `prepareBody` 之后套用）。
//!
//! ── 只管「让请求通过」，不管内容 ────────────────────────────
//! 本文件的每一步都是**形态转换或剔除无法配对的条目**，不涉及语义改写：
//! 改 role 名（同义）、把对象拆成字符串（上游的等价表达）、把字符串包成对象
//! （规范形态）、把别名译成上游认的键（同名语义）。内容层面的处理在
//! `core::sanitize`（剥离审核指纹），两者互不替代。

use serde_json::{json, Map, Value};

/// 归一化做了什么（供调用方写一行详细日志）。
///
/// ── 为什么要有这份报告 ──────────────────────────────────────
/// 几个步骤是**静默修复**：修好了用户不会知道（请求从此不 400 了），但一旦
/// 出问题（工具上下文「少了一轮」、max_tokens 没生效）就需要能回答「网关对
/// 我的请求动了什么」。字段只统计**值得解释的修复**，不统计常规注入
/// （`prompt_cache_key` 几乎每次都会加，写进日志只是噪声）——唯一例外是
/// 它取不到账号 UID 的情形，那会让跨账号隔离失效，必须可见。
#[derive(Clone, Debug, Default)]
pub struct NormalizeReport {
    /// 归一成 system 的 developer 角色消息数
    pub roles_normalized: usize,
    /// 是否改写了 tool_choice（对象形态 → 上游认的字符串形态）
    pub tool_choice_rewritten: bool,
    /// 由字符串形态包装成对象形态的 image_url 数
    pub image_urls_wrapped: usize,
    /// 是否把 max_completion_tokens 译成了 max_tokens
    pub max_tokens_translated: bool,
    /// 剔除的孤儿 tool_call / tool 结果条目数
    pub orphans_removed: usize,
    /// 是否把插在 tool 结果中间的消息移到组后（配对断裂重排）
    pub tool_results_repacked: bool,
    /// 缓存键是否**缺少账号隔离段**（取不到账号 UID）——
    /// 这一条会让跨账号前缀缓存隔离失效，需要显式警告（见 `inject_prompt_cache_key`）
    pub cache_key_unscoped: bool,
}

impl NormalizeReport {
    /// 是否有值得写进详细日志的修复（常规的缓存键注入不算）。
    pub fn notable(&self) -> bool {
        self.roles_normalized > 0
            || self.tool_choice_rewritten
            || self.image_urls_wrapped > 0
            || self.max_tokens_translated
            || self.orphans_removed > 0
            || self.tool_results_repacked
            || self.cache_key_unscoped
    }

    /// 一行可读的中文描述（供 `logging::verbose`）。
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.roles_normalized > 0 {
            parts.push(format!("developer→system {} 条", self.roles_normalized));
        }
        if self.tool_choice_rewritten {
            parts.push("tool_choice 归一为上游字符串形态".to_string());
        }
        if self.image_urls_wrapped > 0 {
            parts.push(format!(
                "image_url 补对象形态 {} 处",
                self.image_urls_wrapped
            ));
        }
        if self.max_tokens_translated {
            parts.push("max_completion_tokens→max_tokens".to_string());
        }
        if self.tool_results_repacked {
            parts.push("tool 结果重排（配对断裂修复）".to_string());
        }
        if self.orphans_removed > 0 {
            parts.push(format!(
                "剔除无法配对的 tool 条目 {} 个",
                self.orphans_removed
            ));
        }
        if self.cache_key_unscoped {
            parts.push("警告：缓存键缺少账号隔离段（未取到账号 UID）".to_string());
        }
        parts.join("；")
    }
}

/// 归一化管线的总入口：在**已脱敏、已改写模型名**的 body 上做一轮出站归一。
///
/// `account` 是与 `build_chat_request` 同源的账号会话对象（用于取 UID 做缓存键
/// 的账号隔离段，见 `inject_prompt_cache_key`）。
///
/// 返回归一后的新 body 与报告；body 不是 JSON 对象时**原样返回**且报告为空
/// （坏 body 不在这里二次错误化：上游会给出比我们更准确的解析错误）。
pub fn normalize_outbound(body: &Value, account: &Value) -> (Value, NormalizeReport) {
    let Some(object) = body.as_object() else {
        return (body.clone(), NormalizeReport::default());
    };
    let mut report = NormalizeReport::default();
    let mut next = object.clone();
    normalize_roles(&mut next, &mut report);
    normalize_tool_choice(&mut next, &mut report);
    normalize_image_url(&mut next, &mut report);
    translate_max_completion_tokens(&mut next, &mut report);
    ensure_stream_options(&mut next);
    repair_tool_pairing(&mut next, &mut report);
    inject_prompt_cache_key(&mut next, account, &mut report);
    (Value::Object(next), report)
}

/// 把 `developer` 角色归一为 `system`。
///
/// 上游对 messages 的 role 字段做白名单校验，`developer` 不在白名单内，命中即
/// HTTP 400 code=11128。`developer` 是 OpenAI 新规范里 system 的别名（Codex /
/// Cursor 等新客户端用它承载 system 级指令），改写不丢语义。
///
/// 只认 `developer` 这一个值：其余 role（system/user/assistant/tool/任意未知值）
/// 一律原样保留，**不合并、不重排、不删除**任何消息 —— 上游对多 system 的行为
/// 未实测，合并会引入新变量。
fn normalize_roles(object: &mut Map<String, Value>, report: &mut NormalizeReport) {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        let Some(fields) = message.as_object_mut() else {
            continue;
        };
        let is_developer = fields
            .get("role")
            .and_then(Value::as_str)
            .map(|role| role.trim().eq_ignore_ascii_case("developer"))
            .unwrap_or(false);
        if is_developer {
            fields.insert("role".to_string(), Value::String("system".to_string()));
            report.roles_normalized += 1;
        }
    }
}

/// 按上游 Go struct（`tool_choice` 是 string 类型）改写 OpenAI 的 `tool_choice`。
///
/// 对象形态透传会让上游 400 code=11101（cannot unmarshal object into string）。
/// 映射规则（与参考项目逐条对齐）：
///   - `"none"` / `{"type":"none"}` → 删 `tool_choice` **并删 tools/functions**
///     （上游语义：不调用工具时连工具声明一起撤掉，否则仍可能触发工具调用）；
///   - `{"type":"auto"|"required"}` → 字符串 `"auto"` / `"required"`；
///   - `{"type":"function","function":{"name":"x"}}` → 字符串 `"x"`
///     （上游把指定函数表达成裸函数名）；
///   - 其余对象 / 非标量 → 删 `tool_choice`（宁可不指定，也不发一个必定 400 的值）。
fn normalize_tool_choice(object: &mut Map<String, Value>, report: &mut NormalizeReport) {
    let Some(choice) = object.get("tool_choice").cloned() else {
        return;
    };
    match choice {
        // 字符串形态上游本就认（auto/required/裸函数名），只在 none 时做撤除
        Value::String(text) => {
            if text.trim().eq_ignore_ascii_case("none") {
                object.remove("tool_choice");
                object.remove("tools");
                object.remove("functions");
                report.tool_choice_rewritten = true;
            }
        }
        Value::Object(fields) => {
            let kind = fields
                .get("type")
                .and_then(Value::as_str)
                .map(|text| text.trim().to_ascii_lowercase())
                .unwrap_or_default();
            match kind.as_str() {
                "none" => {
                    object.remove("tool_choice");
                    object.remove("tools");
                    object.remove("functions");
                }
                "auto" | "required" => {
                    object.insert("tool_choice".to_string(), Value::String(kind));
                }
                "function" => {
                    // 函数名可能在 function.name（规范形态）或顶层 name（部分客户端）
                    let name = fields
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| fields.get("name").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|name| !name.is_empty());
                    match name {
                        Some(name) => {
                            object
                                .insert("tool_choice".to_string(), Value::String(name.to_string()));
                        }
                        // 指定了 function 却没给名字：退回 auto 比删掉更接近意图
                        None => {
                            object.insert(
                                "tool_choice".to_string(),
                                Value::String("auto".to_string()),
                            );
                        }
                    }
                }
                _ => {
                    object.remove("tool_choice");
                }
            }
            report.tool_choice_rewritten = true;
        }
        // 数字 / 数组 / null：不是上游认的任何形态，撤掉比发过去强
        _ => {
            object.remove("tool_choice");
            report.tool_choice_rewritten = true;
        }
    }
}

/// 兼容 OpenAI 多模态内容的两种 `image_url` 写法。
///
/// OpenAI Chat 规范用对象形态 `{"url":"...","detail":"..."}`，部分客户端（以及
/// Responses → Chat 转换器）会发字符串形态 `"data:..."` / `"https://..."`。
/// 上游只接受对象形态，字符串会 400 code=11101（cannot unmarshal string into
/// ImageContent）。这里只做形状转换：字符串转 `{"url": 原值}`；已是对象、空串、
/// 字段缺失一律不动（让上游返回真实错误，而不是我们伪造一个 url）。
fn normalize_image_url(object: &mut Map<String, Value>, report: &mut NormalizeReport) {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        let Some(parts) = message
            .as_object_mut()
            .and_then(|fields| fields.get_mut("content"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(fields) = part.as_object_mut() else {
                continue;
            };
            if fields.get("type").and_then(Value::as_str) != Some("image_url") {
                continue;
            }
            let Some(url) = fields
                .get("image_url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|url| !url.is_empty())
            else {
                continue;
            };
            fields.insert("image_url".to_string(), json!({ "url": url }));
            report.image_urls_wrapped += 1;
        }
    }
}

/// 把 OpenAI 别名 `max_completion_tokens` 翻译为上游认的 `max_tokens`。
///
/// OpenAI 规范里 `max_tokens` 已 deprecated、`max_completion_tokens` 是新字段
/// （o-series 起引入），DeepSeek Harness 等新客户端只发别名。WorkBuddy 上游只认
/// `max_tokens` —— 别名透传会被**静默忽略**后回落默认输出上限，长流任务被截断
/// （不报错，比 400 更难排查）。
///
/// 规则：显式 `max_tokens` 优先（别名只删不译）；别名值为 0/null/负数/非数值
/// 一律不翻译（0/null 语义是「未设置」，负数是非法值，翻译等于把垃圾搬过去）。
fn translate_max_completion_tokens(object: &mut Map<String, Value>, report: &mut NormalizeReport) {
    let Some(alias) = object.remove("max_completion_tokens") else {
        return;
    };
    if object.contains_key("max_tokens") {
        return; // 显式 max_tokens 优先：别名只删不译
    }
    // 整数（含 JSON 里恰好是整数的浮点）才译；1.5 这类上游 struct 也收不了
    let translated = alias
        .as_i64()
        .filter(|number| *number > 0)
        .or_else(|| {
            alias
                .as_f64()
                .filter(|number| *number > 0.0 && number.fract() == 0.0)
                .map(|number| number as i64)
        });
    if let Some(number) = translated {
        object.insert("max_tokens".to_string(), Value::from(number));
        report.max_tokens_translated = true;
    }
}

/// 仅在 body 未显式携带时补 `stream_options.include_usage = true`。
///
/// 官方 CLI 流式必发该字段，上游据此在**末帧**返回 usage 用量（OpenAI 流的
/// usage 只在最后一帧出现）。我们本就在 SSE 旁路里提取 usage 做请求统计
/// （`upstream::usage`），补上它能让统计拿到真实 token 数而不是 null。
///
/// **显式带则绝不覆盖**：客户端可能显式要 `include_usage: false`，那是它的选择。
/// 末帧是否带 choices 对我们的下游无影响：SSE 状态机照原样透传（`sse.rs` 对无
/// choices 的帧只旁路提取 usage、不做改写），非流式聚合器也已在 `choices` 缺失时
/// 提前返回（`aggregate.rs` 的 `consume_chunk`）—— 两侧都不会因为多这一帧出错。
fn ensure_stream_options(object: &mut Map<String, Value>) {
    if object.contains_key("stream_options") {
        return;
    }
    object.insert(
        "stream_options".to_string(),
        json!({ "include_usage": true }),
    );
}

/// tool 配对修复：先「重排」再「清理」（对照参考项目 `tool_pairing.go` 两个函数）。
///
/// ── 为什么这是「让请求通过」的安全网 ─────────────────────────
/// OpenAI 兼容协议要求带 `tool_calls` 的 assistant 消息，其每个 `tool_call.id`
/// 都必须有对应的 `role:"tool"` 结果消息，反之亦然。缺任一侧，上游都以 HTTP 400
/// 拒绝**整个请求**。工具执行失败时（参数非法、超时、工具不存在）客户端会把
/// `tool_calls` 持久化进会话历史，却写不回结果消息 —— 这条坏历史此后被每次请求
/// 原样重放，上游对**之后每一条用户消息**都返回 400，整条会话报废。
/// 网关是最后一道防线：宁可丢一轮工具上下文，也好过整条会话死亡。
fn repair_tool_pairing(object: &mut Map<String, Value>, report: &mut NormalizeReport) {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    repack_tool_results(messages, report);
    cleanup_orphan_tools(messages, report);
}

/// 把插在 `assistant.tool_calls` 与其 tool 结果之间的非 tool 消息挪到整组之后。
///
/// 背景：Codex 的 `image_resize_notice` 等特性会把一条 developer/system 消息插在
/// tool 输出中间，并行调用时它落在两份 tool 结果**之间**：
///
/// ```text
/// assistant tool_calls=[c00 c01] → tool c00 → developer <notice> → tool c01
/// 改写为：assistant tool_calls=[c00 c01] → tool c00 → tool c01 → developer <notice>
/// ```
///
/// 只调顺序、不改内容。同批 tool 结果的原相对顺序保持不变（不引入新的顺序敏感
/// 问题）；无插入消息时不做任何改动（不重排、不分配新 vec）。
fn repack_tool_results(messages: &mut Vec<Value>, report: &mut NormalizeReport) {
    if messages.len() < 3 {
        return;
    }
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut changed = false;
    let mut index = 0usize;
    while index < messages.len() {
        let calls = assistant_call_ids(&messages[index]);
        if calls.is_empty() {
            out.push(messages[index].clone());
            index += 1;
            continue;
        }
        out.push(messages[index].clone());
        index += 1;
        let mut results: Vec<Value> = Vec::new();
        let mut between: Vec<Value> = Vec::new();
        let mut saw_non_tool = false;
        while index < messages.len() {
            let Some(fields) = messages[index].as_object() else {
                break;
            };
            let role = fields.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "tool" {
                let id = fields
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !calls.iter().any(|call| call == id) {
                    break; // 不属于本批：交还外层
                }
                results.push(messages[index].clone());
                if saw_non_tool {
                    changed = true; // 前面插过东西 = 确实需要重排
                }
                index += 1;
                continue;
            }
            if results.is_empty() {
                break; // assistant 后没有结果：交给 cleanup 处理
            }
            // 下一组 assistant.tool_calls 是新组头，绝不能当插入物吞掉：
            // 一旦收进 between，它自己那批结果就永远得不到重排。
            if role == "assistant" && !assistant_call_ids(&messages[index]).is_empty() {
                break;
            }
            between.push(messages[index].clone());
            saw_non_tool = true;
            index += 1;
        }
        out.extend(results);
        out.extend(between);
    }
    if changed {
        *messages = out;
        report.tool_results_repacked = true;
    }
}

/// 剔除无法配对的 `tool_call` 与 tool 结果（双侧按同一份 keep 集对称裁剪）。
///
///   - 收集全线 `role:"tool"` 的 `tool_call_id`（结果集）与
///     `assistant.tool_calls[].id`（调用集）；
///   - `assistant.tool_calls` 按 keep 集裁剪：只留有结果配对的调用，裁空则删掉
///     整个 `tool_calls` 键；
///   - `role:"tool"` 只在对应调用被保留时才保留，孤儿结果整条删除。
///
/// **两侧必须共用同一份 keep 集**：若只裁调用侧（保留整个 tool_calls 键或整批删除），
/// 会留下「无 tool_calls 的 assistant + 孤儿 tool」这种半截配对，上游判 11148
/// （tool calls and tool results do not match）并顶死会话。
fn cleanup_orphan_tools(messages: &mut Vec<Value>, report: &mut NormalizeReport) {
    if messages.is_empty() {
        return;
    }
    let mut call_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for message in messages.iter() {
        let Some(fields) = message.as_object() else {
            continue;
        };
        match fields.get("role").and_then(Value::as_str).unwrap_or("") {
            "tool" => {
                if let Some(id) = non_empty_str(fields.get("tool_call_id")) {
                    result_ids.push(id);
                }
            }
            "assistant" => {
                if let Some(calls) = fields.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(id) = call
                            .as_object()
                            .and_then(|call| non_empty_str(call.get("id")))
                        {
                            call_ids.push(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if call_ids.is_empty() && result_ids.is_empty() {
        return; // 无工具流量：零改动
    }
    // keep 集 = 调用与结果双侧齐全的 id
    let keep: Vec<&String> = call_ids
        .iter()
        .filter(|id| result_ids.iter().any(|result| result == *id))
        .collect();
    // 1) 调用侧裁剪
    for message in messages.iter_mut() {
        let Some(fields) = message.as_object_mut() else {
            continue;
        };
        if fields.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = fields.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        if calls.is_empty() {
            continue;
        }
        let kept: Vec<Value> = calls
            .iter()
            .filter(|call| {
                call.as_object()
                    .and_then(|call| non_empty_str(call.get("id")))
                    .map(|id| keep.iter().any(|kept| **kept == id))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if kept.len() == calls.len() {
            continue; // 整批齐全：零改动
        }
        report.orphans_removed += calls.len() - kept.len();
        if kept.is_empty() {
            fields.remove("tool_calls");
        } else {
            fields.insert("tool_calls".to_string(), Value::Array(kept));
        }
    }
    // 2) 结果侧裁剪：孤儿 tool 消息整条删除
    let before = messages.len();
    messages.retain(|message| {
        let Some(fields) = message.as_object() else {
            return true;
        };
        if fields.get("role").and_then(Value::as_str) != Some("tool") {
            return true;
        }
        non_empty_str(fields.get("tool_call_id"))
            .map(|id| keep.iter().any(|kept| **kept == id))
            .unwrap_or(false)
    });
    report.orphans_removed += before - messages.len();
}

/// 注入上游前缀缓存键 `prompt_cache_key`（费用优化）。
///
/// ── 为什么值得做（参考项目的逆向实测）────────────────────────
/// 同一段 8k token 前缀：不带该字段 `prompt_cache_hit_tokens=0, credit≈0.34`；
/// 带上 `prompt_cache_hit_tokens=7808, credit≈0.02` —— **费用降约 17 倍**。
/// 上游服务端支持按此键复用前缀缓存，同一客户端对同一账号的连续请求因此
/// 不必重复计费整段历史。
///
/// ── 为什么必须按账号隔离 ────────────────────────────────────
/// 键格式 `wb2a-<uid8>-<convHex>`：`uid8` 是账号 UID 前 8 字符，**跨账号绝不相同**。
/// 若两个账号共用同一 cache key，上游会命中**另一个账号**的前缀缓存 —— 那不只是
/// 计费错乱，更是对话内容泄露。因此 uid 是硬隔离因子，不是可选的熵来源。
/// 取不到 UID 时仍注入（段位用 `-` 占位）但标记 `cache_key_unscoped` 让调用方警告。
///
/// 优先级：客户端已带非空 `prompt_cache_key` → **原值保留，绝不覆盖**
/// （客户端自知要复用哪个键）；否则依次取 body 的 `conversation_id` /
/// `conversationId` / `metadata.conversation_id` / `metadata.conversationId`
/// 作为会话段；全都取不到时会话段由 UID 单独哈希得到 —— 账号隔离段照常生效，
/// 只是同一账号的不同「无会话标识」请求会共享前缀缓存，那对纯单轮客户端
/// 反而是更优的选择。
fn inject_prompt_cache_key(
    object: &mut Map<String, Value>,
    account: &Value,
    report: &mut NormalizeReport,
) {
    let existing = object
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty());
    if existing.is_some() {
        return; // 优先级 1：客户端自带 → 原样保留
    }
    let uid = account_uid(account);
    report.cache_key_unscoped = uid.is_empty();
    let conversation = conversation_key(object);
    object.insert(
        "prompt_cache_key".to_string(),
        Value::String(build_cache_key(&uid, &conversation)),
    );
}

/// 生成 `wb2a-<uid8>-<convHex>` 形态的稳定缓存键。
///
/// `uid8` 提供账号隔离段；`convHex = sha256(uid + "|" + conversation)` 前 16 字节
/// 的十六进制提供会话段（同账号同会话稳定、不同会话不同）。会话源为空时 convHex
/// 仍由 uid 单独哈希得到 —— 跨账号绝不碰撞，且同账号的同类请求稳定复用。
///
/// 用 sha256（不是 md5）：与项目既有派生（`login.rs` 的登录态、`api_keys.rs`
/// 的密钥指纹）保持同一套实现，不新增哈希依赖（`sha2` 已是直接依赖）。
fn build_cache_key(uid: &str, conversation: &str) -> String {
    use sha2::{Digest, Sha256};

    let uid8: String = uid.chars().take(8).collect();
    let uid8 = if uid8.is_empty() {
        "-".to_string()
    } else {
        uid8
    };
    let mut hasher = Sha256::new();
    hasher.update(uid.as_bytes());
    hasher.update(b"|");
    hasher.update(conversation.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("wb2a-{uid8}-{hex}")
}

/// 从 body 里取会话标识（缓存键的会话段来源）。
///
/// 顺序与参考项目的 `InjectPromptCacheKey` 一致：顶层优先于 metadata，
/// 每级内 snake_case 优先于 camelCase。全部取不到返回空串。
fn conversation_key(object: &Map<String, Value>) -> String {
    let direct = non_empty_str(object.get("conversation_id"))
        .or_else(|| non_empty_str(object.get("conversationId")));
    if let Some(text) = direct {
        return text;
    }
    let Some(metadata) = object.get("metadata").and_then(Value::as_object) else {
        return String::new();
    };
    non_empty_str(metadata.get("conversation_id"))
        .or_else(|| non_empty_str(metadata.get("conversationId")))
        .unwrap_or_default()
}

/// 取账号 UID：账号会话对象里的 `account.uid`（`build_chat_request` 拿到的
/// session 就是 `{ account: { uid, ... }, auth: { ... } }` 形态，见
/// `AuthService::build_auth_headers` 读同一路径）。
///
/// 取不到时回落顶层 `uid`，再取不到返回空串（缓存键段位退化为 `-` 占位，
/// 见 `inject_prompt_cache_key` 的说明）。
fn account_uid(account: &Value) -> String {
    account
        .get("account")
        .and_then(|inner| inner.get("uid"))
        .and_then(Value::as_str)
        .or_else(|| account.get("uid").and_then(Value::as_str))
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
        .unwrap_or("")
        .to_string()
}

/// 取一个非空字符串字段（`tool_calls[].id` / `tool_call_id` 的判据）。
fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 取 assistant 消息里 `tool_calls[].id` 的集合（非 assistant 或无 tool_calls 则空 vec）。
fn assistant_call_ids(message: &Value) -> Vec<String> {
    message
        .as_object()
        .and_then(|fields| {
            if fields.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            fields.get("tool_calls").and_then(Value::as_array)
        })
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.as_object().and_then(|call| non_empty_str(call.get("id"))))
                .collect()
        })
        .unwrap_or_default()
}
