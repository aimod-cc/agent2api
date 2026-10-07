//! Trae SOLO 通道的出站请求体。
//!
//! ── 为什么是"白名单重建"而不是"改几个字段" ──────────────────
//! 上游 `llm_utils_chat` 的契约极小，而且**多带字段不是被忽略、是被拒**：
//! 实测带 `agent_type` / `device_id` / `ide_version` 会得到流内 `4023
//! "model is unknown"`，带 `thinking` / `stream_options` / `response_format`
//! 一族则会以别的方式炸流。所以这里反过来做：只把清单里的键搬过去，
//! 其余一概丢弃（参考实现 v0.12.37 起的定见）。
//!
//! 四处 SOLO 专属变形是"照 OpenAI 形状直发必定 4001"的那四处，
//! 每一处都有向量用例钉着：
//!   ① `tools[].function.parameters` 必须是 **JSON 字符串**（OpenAI 是对象）；
//!   ② `tool_choice` 必须是**裸字符串**（对象形态要按 type 折算）；
//!   ③ assistant 的 `tool_calls[].function` 要改名 **`function_call`**；
//!   ④ `developer` 角色上游不认（实测静默空流），降成 `system`。
//!
//! 另有两条"韧性"清理：孤儿 `tool` 结果（客户端修剪历史留下的悬空引用）
//! 与全被剔空的 assistant 占位消息 —— 上游对这两种都可能是空流而不是报错，
//! 空流在网关这边会被判成"成功但没话"，那比报错难查得多。

use serde_json::{Value, json};

/// 缺省模型（实测可用）。模型名解析不出来时兜底，而不是发一个空 `config_name`。
pub const DEFAULT_CONFIG_NAME: &str = "glm-5.2";

/// 采样与工具类白名单之外的键一律丢弃。
const SAMPLING_KEYS: [&str; 7] = [
    "temperature",
    "top_p",
    "max_tokens",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "n",
];

/// `max_tokens` 缺省值：上游会把输出截在 128k，客户端没给就补一个大上限，
/// 让长回复不被腰斩（显式给的值原样透传）。
const DEFAULT_MAX_TOKENS: i64 = 1_000_000;

/// variant → 上游 `function` 字段。
///
/// 现在四个取值都落到 `solo_work_lite`：`llm_utils_chat` **只认这一个**，
/// 参考实现早先给 `cn`/`intl` 发 `inline_chat`，结果是那两类凭据的每个模型
/// 都流内 4001（issue #9）。表留着而不是写死常量，是因为这四个 variant 的
/// 血统差异在别的端点上仍然有效，别处要按 variant 分支。
pub fn function_for(variant: &str) -> &'static str {
    match variant {
        "cn" | "solo" | "intl" | "solo-intl" => "solo_work_lite",
        _ => "solo_work_lite",
    }
}

/// 剥掉网关侧为"凭据分类"加的后缀，还原成上游目录认识的裸名。
///
/// 只剥**一层**（我们的命名只加一层），所以真叫 `x-solo` 的模型广告成
/// `x-solo-solo` 之后还能回到 `x-solo`。
/// ⚠️ 绝不按 `/` 切：上游配置名里 `/` 是合法字符（`deepseek-ai/deepseek-v4-pro`），
/// 切了就等于把一个可用模型变成一个 4001。
pub fn sanitize_model_name(model: &str, variant: &str) -> String {
    let mut name = model.trim();
    if variant == "solo" {
        name = name.strip_suffix("-solo").unwrap_or(name);
    }
    name = name.strip_suffix("-intl").unwrap_or(name);
    name.trim().to_string()
}

/// 入站请求体 → SOLO 出站体（对象形态；出站序列化由调用方做）。
pub fn prepare_body(source: &Value, variant: &str, resolved_model: &str) -> Value {
    let Value::Object(body) = source else {
        // 参考实现在 JSON 解析失败时**原样返回字节**；这里对应"不是对象就原样搬"，
        // 让调用方拿到一个可诊断的载荷而不是一层包装错误。
        return source.clone();
    };
    let mut obj: Value = json!(body.clone());

    normalize_messages(&mut obj);
    drop_orphan_tool_results(&mut obj);

    let mut model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let resolved = resolved_model.trim();
    if !resolved.is_empty() {
        // 宿主解析出的模型名赢：body 里的 `model` 可能带着凭据前缀（`tr/kimi-k2.6`），
        // 上游目录只认裸名。两者归一后不等时留一条日志线索（不是错误）。
        model = resolved.to_string();
    }
    let model = {
        let sanitized = sanitize_model_name(&model, variant);
        if sanitized.is_empty() {
            DEFAULT_CONFIG_NAME.to_string()
        } else {
            sanitized
        }
    };

    normalize_tool_choice(&mut obj);
    normalize_tools(&mut obj);

    let mut out = serde_json::Map::new();
    if let Some(messages) = obj.get("messages") {
        out.insert("messages".to_string(), messages.clone());
    }
    // `function` 按**模型**选，不按凭据谱系：目录实测 `chat_v3` 是另一张明细表
    // 的场景名（`models.rs` 的合并与探针），拿 `solo_work_lite` 发它专属的模型
    // 会得到流内 4001。目录里没有这个名字时回落到 variant 的老行为。
    out.insert("function".to_string(), json!(super::models::function_for_model(&model)));
    out.insert("stream".to_string(), json!(true));
    out.insert("config_name".to_string(), json!(model));
    out.insert("model".to_string(), json!(model));
    if let Some(tools) = obj.get("tools") {
        out.insert("tools".to_string(), tools.clone());
    }
    if let Some(tool_choice) = obj.get("tool_choice") {
        out.insert("tool_choice".to_string(), tool_choice.clone());
    }
    for key in SAMPLING_KEYS {
        // 只认数字（参考实现断言 float64）：字符串或布尔的 `temperature` 一律丢，
        // 上游对类型的容忍度是零。
        if let Some(number) = obj.get(key).filter(|value| value.is_number()) {
            out.insert(key.to_string(), number.clone());
        }
    }
    out.entry("max_tokens".to_string())
        .or_insert_with(|| json!(DEFAULT_MAX_TOKENS));
    if let Some(effort) = obj.get("reasoning_effort").and_then(Value::as_str) {
        // auto/none/off 不显式下发，与真实客户端一致；其余原样透传。
        if !matches!(effort.trim().to_lowercase().as_str(), "" | "auto" | "none" | "off") {
            out.insert("reasoning_effort".to_string(), json!(effort));
        }
    }
    match obj.get("stop") {
        Some(Value::String(_)) | Some(Value::Array(_)) => {
            out.insert("stop".to_string(), obj["stop"].clone());
        }
        _ => {}
    }
    Value::Object(out)
}

/// ① 角色归一 + ② assistant tool_calls 改名 + ③ 字符串 content 归一成 text 块。
fn normalize_messages(obj: &mut Value) {
    let Some(messages) = obj.get_mut("messages").and_then(|value| value.as_array_mut()) else {
        return;
    };
    for slot in messages.iter_mut() {
        // 占位标记要在借用结束后再写回（`*slot = Null` 与 `entry` 不能同时成立）。
        let mut mark_null = false;
        {
            let Value::Object(entry) = slot else {
                continue;
            };
            let content_present = entry.contains_key("content");
            let content_value = entry.get("content").cloned();
            let mut role = entry.get("role").and_then(Value::as_str).unwrap_or_default().to_string();
            if role == "developer" {
                entry.insert("role".to_string(), json!("system"));
                role = "system".to_string();
            }

            if role == "assistant" {
                let calls = entry.get("tool_calls").and_then(Value::as_array).cloned();
                if let Some(calls) = calls {
                    let mut kept: Vec<Value> = Vec::with_capacity(calls.len());
                    for call in calls {
                        let Value::Object(mut call) = call else {
                            continue;
                        };
                        if let Some(function) = call.get("function").cloned() {
                            call.insert("function_call".to_string(), function);
                            call.remove("function");
                        }
                        // 上游要求 function_call.name 必填：没名字的调用整条剔除。
                        let named = call
                            .get("function_call")
                            .and_then(|fc| fc.get("name"))
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.trim().is_empty());
                        if named {
                            kept.push(Value::Object(call));
                        }
                    }
                    if kept.is_empty() {
                        entry.remove("tool_calls");
                        // 全被剔且本来就没正文的 assistant 占位消息整条丢掉：
                        // 上游对这种消息可能回空流。
                        let blank_content = content_value.as_ref().is_none_or(Value::is_null);
                        if !content_present || blank_content {
                            mark_null = true;
                        }
                    } else {
                        entry.insert("tool_calls".to_string(), Value::Array(kept));
                    }
                }
            }

            // 纯 tool_calls 的 assistant（没有 content 键）：字段留着，content 不改写。
            // ⚠️ 这里刻意不用 `continue`：它在嵌套块里会跳过本回合**块之后**的
            // `mark_null` 写回，占位消息就漏掉了（第一版就是这么错的）。
            // 已是数组（多模态）→ 原样透传：这里只匹配 String，非字符串落到
            // 什么都不做的那一支。参考实现注明"未实测，保守透传"，我们同样不猜：
            // 不改写就不会把客户端的图注弄坏。
            if let Some(Value::String(text)) = content_value {
                entry.insert("content".to_string(), json!([{"type": "text", "text": text}]));
            }
        }
        if mark_null {
            *slot = Value::Null;
        }
    }
}

/// 丢掉引用了不存在的 tool_call 的 `role=tool` 消息，以及上一步标记的占位。
fn drop_orphan_tool_results(obj: &mut Value) {
    let Some(messages) = obj.get("messages").and_then(Value::as_array).cloned() else {
        return;
    };
    let mut known: Vec<String> = Vec::new();
    for message in &messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    known.push(id.to_string());
                }
            }
        }
    }
    let kept: Vec<Value> = messages
        .into_iter()
        .filter(|message| {
            if message.is_null() {
                return false; // normalize_messages 的占位标记
            }
            if message.get("role").and_then(Value::as_str) != Some("tool") {
                return true;
            }
            let id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or_default();
            known.iter().any(|known_id| known_id == id)
        })
        .collect();
    obj["messages"] = Value::Array(kept);
}

/// ④ `tool_choice` 折算成上游要的裸字符串；`"none"` 还要连带把 tools 删掉。
fn normalize_tool_choice(obj: &mut Value) {
    let Some(tool_choice) = obj.get("tool_choice").cloned() else {
        return;
    };
    let suppress_tools = |obj: &mut Value| {
        if let Value::Object(map) = obj {
            map.remove("tools");
            map.remove("functions");
        }
    };
    match tool_choice {
        Value::String(name) => {
            if name.trim().eq_ignore_ascii_case("none") {
                if let Value::Object(map) = obj {
                    map.remove("tool_choice");
                }
                suppress_tools(obj);
            }
        }
        Value::Object(ref wrapper) => {
            let kind = wrapper.get("type").and_then(Value::as_str).unwrap_or_default().trim().to_lowercase();
            match kind.as_str() {
                "none" => {
                    if let Value::Object(map) = obj {
                        map.remove("tool_choice");
                    }
                    suppress_tools(obj);
                }
                "auto" | "required" => {
                    obj["tool_choice"] = json!(kind);
                }
                "function" => {
                    let name = wrapper
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| wrapper.get("name").and_then(Value::as_str))
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    obj["tool_choice"] = json!(if name.is_empty() { "auto".to_string() } else { name });
                }
                _ => {
                    if let Value::Object(map) = obj {
                        map.remove("tool_choice");
                    }
                }
            }
        }
        _ => {
            if let Value::Object(map) = obj {
                map.remove("tool_choice");
            }
        }
    }
}

/// ① `tools[].function.parameters` 对象 → JSON 字符串；不是这个形状的工具整条剔除。
fn normalize_tools(obj: &mut Value) {
    let Some(raw) = obj.get("tools").cloned() else {
        return;
    };
    let Value::Array(list) = raw else {
        return;
    };
    if list.is_empty() {
        return;
    }
    let mut kept: Vec<Value> = Vec::with_capacity(list.len());
    for item in list {
        let Value::Object(mut tool) = item else {
            continue;
        };
        let Some(Value::Object(mut function)) = tool.get("function").cloned() else {
            continue;
        };
        if let Some(parameters) = function.get("parameters") {
            if parameters.is_object() {
                // 上游的 Go struct 把这个字段声明成 string，直发对象会让整个
                // 请求反序列化失败（不是"这个工具没了"，是"这一轮全没了"）。
                function.insert("parameters".to_string(), json!(parameters.to_string()));
            }
        }
        tool.insert("function".to_string(), Value::Object(function));
        kept.push(Value::Object(tool));
    }
    if kept.is_empty() {
        if let Value::Object(map) = obj {
            map.remove("tools");
        }
        return;
    }
    obj["tools"] = Value::Array(kept);
}

#[cfg(test)]
mod tests {
    //! 全部断言读同一份向量：参考实现算出来的答案，而不是我以为的规则。
    use super::*;

    const VECTORS: &str = include_str!("vectors/trae-vectors.json");

    fn document() -> Value {
        serde_json::from_str(VECTORS).expect("向量文件必须是合法 JSON")
    }

    /// 比对解析后的 JSON —— Go 的 map 序列化按 key 排序，serde 按插入顺序，
    /// **键顺序不是契约**，比字节会把人引到歧路上去。
    fn assert_same_json(wanted: &str, got: &Value, label: &str) {
        let parsed: Value = serde_json::from_str(wanted).unwrap_or_else(|_| panic!("{label}: 向量 output 不是 JSON"));
        assert_eq!(parsed, *got, "用例：{label}");
    }

    #[test]
    fn prepared_bodies_match_the_reference_implementation() {
        let document = document();
        let cases = document["payload"].as_array().expect("payload 段是数组");
        assert!(cases.len() > 20, "向量应当覆盖每个分支，实际 {}", cases.len());
        for case in cases {
            let name = case["name"].as_str().unwrap_or("?");
            let input: Value = match serde_json::from_str(case["input"].as_str().unwrap()) {
                Ok(parsed) => parsed,
                // Go 的"非 JSON 原样返回"发生在**解析之前**，而本家的 body 到
                // 达时已是解析后的 Value（非法 body 在 API 层就被拒了），所以这条
                // 用例没有可执行的对应分支。原先只是 `continue` —— 等于卷上有的
                // 这条断言从不执行。现在改为把参考实现的那份输出**当作事实核一遍**：
                // 它必须是输入原样，从而把"本家不适用"这个判断本身也钉住
                // （哪天上游改成别的行为，这条会先红）。
                Err(_) => {
                    assert!(name.contains("非法"), "{name} 不该是非 JSON");
                    assert_eq!(
                        case["input"].as_str().unwrap_or_default(),
                        case["output"].as_str().unwrap_or_default(),
                        "{name}：参考实现是原样返回，不是改写或包一层"
                    );
                    continue;
                }
            };
            let got = prepare_body(&input, case["variant"].as_str().unwrap(), case["resolvedModel"].as_str().unwrap_or(""));
            assert_same_json(case["output"].as_str().unwrap(), &got, name);
        }
    }

    #[test]
    fn model_name_sanitizing_matches_the_reference_implementation() {
        for case in document()["sanitizeModelName"].as_array().expect("段存在") {
            let model = case["model"].as_str().unwrap();
            let variant = case["variant"].as_str().unwrap();
            assert_eq!(
                case["output"].as_str().unwrap(),
                sanitize_model_name(model, variant),
                "model={model:?} variant={variant}"
            );
        }
    }

    #[test]
    fn a_slash_inside_a_config_name_survives() {
        // 参考实现特意写了"绝不按 / 切"：`deepseek-ai/deepseek-v4-pro` 是合法裸名。
        assert_eq!("deepseek-ai/deepseek-v4-pro", sanitize_model_name("deepseek-ai/deepseek-v4-pro", "solo"));
        assert_eq!("tr/kimi", sanitize_model_name("tr/kimi", "solo"));
    }

    #[test]
    fn only_one_namespacing_suffix_is_stripped() {
        assert_eq!("x-solo", sanitize_model_name("x-solo-solo", "solo"));
        assert_eq!("", sanitize_model_name("-solo", "solo"), "整串就是后缀时得到空串，由 prepare_body 兜默认模型");
    }

    #[test]
    fn an_empty_resolved_model_leaves_the_body_model_in_charge() {
        let body = json!({"model":"kimi-k2.6","messages":[{"role":"user","content":"q"}]});
        let prepared = prepare_body(&body, "solo", "   ");
        assert_eq!("kimi-k2.6", prepared["config_name"]);
        assert_eq!("kimi-k2.6", prepared["model"], "config_name 与 model 必须同值");
    }

    #[test]
    fn a_blank_model_falls_back_to_the_default_config() {
        let body = json!({"model":"","messages":[{"role":"user","content":"q"}]});
        assert_eq!(DEFAULT_CONFIG_NAME, prepare_body(&body, "solo", "")["config_name"]);
    }

    #[test]
    fn non_numeric_sampling_fields_are_dropped_not_forwarded() {
        // 上游对类型零容忍：`"temperature":"0.3"` 直发会炸整轮，而不是忽略这一个字段。
        let body = json!({"model":"m","messages":[],"temperature":"0.3","top_p":true,"seed":null,"n":2});
        let prepared = prepare_body(&body, "solo", "");
        assert!(prepared.get("temperature").is_none());
        assert!(prepared.get("top_p").is_none());
        assert!(prepared.get("seed").is_none());
        assert_eq!(2, prepared["n"]);
    }

    #[test]
    fn a_tool_call_without_a_name_takes_its_placeholder_message_down() {
        // 三条链一起验：无名调用被剔 → tool_calls 空了被删 → 没正文的 assistant 占位被丢
        let body = json!({"model":"m","messages":[
            {"role":"user","content":"q"},
            {"role":"assistant","tool_calls":[{"id":"c1","function":{"arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"orphan now"}
        ]});
        let prepared = prepare_body(&body, "solo", "");
        let messages = prepared["messages"].as_array().unwrap();
        assert_eq!(1, messages.len(), "占位与随之变孤儿的 tool 结果都不该留：{messages:?}");
        assert_eq!("user", messages[0]["role"]);
    }
}
