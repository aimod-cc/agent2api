//! CatPaw 内容块与工具字段的归一化（`messages.rs` 的下半层；
//! 移植来源 `catpaw-upstream-messages.mjs` 第 41-158 行的
//! `stringifyToolData` / `normalizeImageBlock` / `normalizeContentBlock` /
//! `messageContent` / `validateToolArguments` / `normalizeToolCalls` /
//! `toolResultContent`）。**纯函数，不持锁不做 IO。**
//!
//! ── 为什么从 messages.rs 拆出来 ─────────────────────────────
//! 单一职责 + 单文件行数约定（架构文档 §8 第 1 条，≤ 800 行）：
//! `messages.rs` 管**消息级**的流水线（角色分派、相邻合并、工具配对、
//! system/developer 抽离），本文件管**块级**的构造与校验
//! （text / image_url / tool_use 字段映射、tool 参数 JSON 校验、
//! tool 结果文本提取）。两层之间只有几个函数调用，没有共享状态。
//!
//! ── 字段名映射（UPSTREAM_PROTOCOL §4 是唯一权威）────────────
//! | OpenAI 入站 | 上游块字段 |
//! |---|---|
//! | `image_url: {url, detail}` | `imageUrl: {url, detail}`（**蛇形→驼峰**） |
//! | `function.name` / `toolName` / `name` | `toolName` |
//! | `function.arguments` / `toolParams` / `arguments` | `toolParams`（JSON 文本） |
//! | `tool_call_id` / `toolCallId` | `toolCallId` |
//! | `reasoning_content` / `reasoningContent` | 挂在 text 块上的 `reasoningContent` |
//!
//! ── 为什么每个取值都分「真值判定」与「空值合并」两种 ──────────
//! 原实现是 JavaScript，`a || b`（真值）与 `a ?? b`（空值合并）语义不同：
//! `arguments: ""` 对 `||` 是假值会继续往后找，对 `??` 会**命中空串**；
//! `tool_calls: []` 空数组对 `||` 是**真值**（于是走数组分支而不是「没有
//! tool_calls」）。这些差异会改变归一化结果，进而改变发给上游的内容；
//! 因此每一处都按原实现的运算符选择对应的 Rust 写法，并在注释里标明是哪一个。

use serde_json::{json, Map, Value};

use crate::server::errors::GatewayError;

use super::fingerprint::js_truthy;

/// 图片 URL / Data URL 的长度上限（照抄原实现 `MAX_IMAGE_URL_LENGTH`，8MB）。
///
/// 与 `proxy-chat-utils.mjs` 的 `MAX_DATA_URL_LENGTH` 是同一个数：
/// **在归一化阶段**就挡掉超大图片，避免把几十 MB 的 base64 搬进内存
/// （真正的压缩在更后面 —— `image_compress.rs`，而压缩前必须先把 URL 读进来）。
const MAX_IMAGE_URL_LENGTH: usize = 8 * 1024 * 1024;

/// 允许的图片 detail 取值；其它一律回落 `auto`（原实现 `IMAGE_DETAILS`）
const IMAGE_DETAILS: &[&str] = &["auto", "low", "high"];

/// 消息 content → 上游内容块数组（原实现 `messageContent`）。
///
/// 三种形态：字符串（整条变一个 text 块）、`null` / 缺失（空数组）、
/// 数组（逐块归一化）。其它类型报 400。
pub(super) fn message_content(object: &Map<String, Value>) -> Result<Vec<Value>, GatewayError> {
    match object.get("content") {
        Some(Value::String(text)) => Ok(vec![json!({ "type": "text", "text": text })]),
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(blocks)) => blocks.iter().map(normalize_content_block).collect(),
        // 这条文案**不带 messages 下标**（原实现 `messageContent` 里没有 index
        // 参数，文案就这一句），照抄
        Some(_) => Err(GatewayError::bad_request("消息 content 必须是字符串、数组或 null")),
    }
}

/// 单个内容块归一化（原实现 `normalizeContentBlock`）。
///
/// 支持：字符串（→ text 块）、`text` / `output_text` 块、`image_url` 块。
/// `reasoningContent` / `reasoning_content` 两个字段名都认并统一成**驼峰**
/// `reasoningContent`（上游字段名）—— 客户端可能从上游原样回显（驼峰）
/// 或按 OpenAI 习惯写蛇形，两种都得接受。
fn normalize_content_block(block: &Value) -> Result<Value, GatewayError> {
    if let Some(text) = block.as_str() {
        return Ok(json!({ "type": "text", "text": text }));
    }
    let Some(object) = block.as_object() else {
        return Err(GatewayError::bad_request("消息 content block 必须是对象或字符串"));
    };
    match object.get("type").and_then(Value::as_str) {
        Some("text") | Some("output_text") => {
            let Some(text) = object.get("text").and_then(Value::as_str) else {
                return Err(GatewayError::bad_request("文本消息缺少 text"));
            };
            let mut normalized = Map::new();
            normalized.insert("type".to_string(), Value::String("text".to_string()));
            normalized.insert("text".to_string(), Value::String(text.to_string()));
            // 原实现用对象展开、先驼峰后蛇形：两者都在时**蛇形覆盖驼峰**
            // （后写的赢）。顺序照抄。
            if let Some(value) = object.get("reasoningContent").and_then(Value::as_str) {
                normalized
                    .insert("reasoningContent".to_string(), Value::String(value.to_string()));
            }
            if let Some(value) = object.get("reasoning_content").and_then(Value::as_str) {
                normalized
                    .insert("reasoningContent".to_string(), Value::String(value.to_string()));
            }
            Ok(Value::Object(normalized))
        }
        Some("image_url") => normalize_image_block(object),
        other => Err(GatewayError::bad_request(format!(
            "不支持的消息内容类型: {}",
            other.unwrap_or_default()
        ))),
    }
}

/// 图片块归一化：`{type:"image_url", image_url:{url,detail}}` →
/// `{type:"image_url", imageUrl:{url,detail}}`（原实现 `normalizeImageBlock`）。
///
/// ── 校验规则（逐条对照原实现）───────────────────────────────
///   - url 缺失 / 非字符串 / 全空白 → 400；
///   - 长度超过 8MB → 400（**data URL 与远程 URL 同一个上限**，与原实现一致；
///     `proxy-chat-utils.mjs` 那份把远程 URL 限到 2048 字节，是 server.mjs
///     另一条入口用的，不在这条路径上）；
///   - `data:` 开头：必须是 `data:image/(png|jpeg|jpg|gif|webp|bmp);base64,`
///     且正文非空、只含 base64 字符集与空白；
///   - 其它：必须是能解析的 http/https URL；
///   - `detail`：只从**对象形态**的 source 里取（裸字符串形态没有 detail），
///     不在 auto/low/high 里一律回落 `auto`。
///
/// data URL 用手写扫描而不是正则：Rust 标准库没有正则，
/// 为一个格式校验引入 regex 依赖不划算，而 base64 字符集判定本身就只有一行。
fn normalize_image_block(object: &Map<String, Value>) -> Result<Value, GatewayError> {
    // `block.image_url || block.imageUrl`：原实现先蛇形后驼峰（真值判定）
    let source = object
        .get("image_url")
        .filter(|value| js_truthy(value))
        .or_else(|| object.get("imageUrl").filter(|value| js_truthy(value)));
    let url = match source {
        Some(Value::String(text)) => Some(text.as_str()),
        Some(Value::Object(nested)) => nested.get("url").and_then(Value::as_str),
        _ => None,
    };
    let Some(url) = url else {
        return Err(GatewayError::bad_request("图片消息缺少 image_url.url"));
    };
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(GatewayError::bad_request("图片消息缺少 image_url.url"));
    }
    if trimmed.len() > MAX_IMAGE_URL_LENGTH {
        return Err(GatewayError::bad_request("图片 URL 或 Data URL 超过大小上限"));
    }
    // 前缀判定**大小写敏感**（原实现 `trimmed.startsWith('data:')`）：
    // `DATA:...` 会落到 URL 分支并被「仅支持 http/https」拒绝
    if trimmed.starts_with("data:") {
        if !is_base64_image_data_url(trimmed) {
            return Err(GatewayError::bad_request(
                "图片 Data URL 仅支持 png、jpeg、gif、webp、bmp 的 Base64 格式",
            ));
        }
    } else if let Ok(parsed) = url::Url::parse(trimmed) {
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(GatewayError::bad_request("图片 URL 仅支持 http/https 协议"));
        }
    } else {
        return Err(GatewayError::bad_request("图片 URL 格式无效"));
    }
    let detail = match source {
        Some(Value::Object(nested)) => nested.get("detail").and_then(Value::as_str),
        _ => None,
    }
    .filter(|value| IMAGE_DETAILS.contains(value))
    .unwrap_or("auto");
    Ok(json!({
        "type": "image_url",
        "imageUrl": { "url": trimmed, "detail": detail },
    }))
}

/// `data:image/(png|jpeg|jpg|gif|webp|bmp);base64,<正文>` 判定（对应原实现
/// 正则 `/^data:image\/(png|jpe?g|gif|webp|bmp);base64,[a-z0-9+/=\s]+$/i`）。
///
/// 类型部分大小写不敏感（正则的 `i`）；正文由 base64 字符集 + 空白构成，
/// 且**不能为空**（正则的 `+`）。正文里再出现逗号必然落在字符集之外，
/// 与正则末尾的 `$` 锚定等价。
fn is_base64_image_data_url(value: &str) -> bool {
    let Some((header, body)) = value.split_once(',') else {
        return false;
    };
    let Some(rest) = header.to_ascii_lowercase().strip_prefix("data:").map(str::to_owned) else {
        return false;
    };
    let Some(meta) = rest.strip_suffix(";base64") else {
        return false;
    };
    let Some(subtype) = meta.strip_prefix("image/") else {
        return false;
    };
    if !matches!(subtype, "png" | "jpeg" | "jpg" | "gif" | "webp" | "bmp") {
        return false;
    }
    !body.is_empty()
        && body.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '+'
                || character == '/'
                || character == '='
                || character.is_whitespace()
        })
}

/// 一次归一化后的工具调用（中间形态：id / name / arguments 文本）
pub(super) struct NormalizedToolCall {
    /// 原样保留（**不 trim**，原实现直接取 `call.id`）
    pub(super) id: String,
    pub(super) name: String,
    /// 已规范化的 JSON 文本（原实现 `validateToolArguments` 的返回）
    pub(super) arguments: String,
}

/// assistant 的 tool_calls 归一化（原实现 `normalizeToolCalls`）。
///
/// 同时兼容 `tool_calls` / `toolCalls` 两个字段名，以及三种调用形态：
///   - OpenAI 形态：`{id, type:"function", function:{name, arguments}}`
///   - 上游回显形态：`{toolCallId, toolName, toolParams}`
///   - 简写：`{id, name, arguments}`
///
/// `type` 只支持 `function`（缺失放行，其它值报 400）。
pub(super) fn normalize_tool_calls(
    object: &Map<String, Value>,
) -> Result<Vec<NormalizedToolCall>, GatewayError> {
    // `message.tool_calls || message.toolCalls`（真值判定：空数组是真值 → 走数组分支）
    let calls = object
        .get("tool_calls")
        .filter(|value| js_truthy(value))
        .or_else(|| object.get("toolCalls").filter(|value| js_truthy(value)));
    let Some(calls) = calls else {
        return Ok(Vec::new());
    };
    let Some(calls) = calls.as_array() else {
        return Err(GatewayError::bad_request("assistant.tool_calls 必须是数组"));
    };
    let mut normalized = Vec::with_capacity(calls.len());
    for (call_index, call) in calls.iter().enumerate() {
        let call_object = call.as_object();
        // `call?.type !== undefined && call.type !== 'function'`：
        // 只有**字符串且不等于 function** 才报错（null 视为未给，见报告差异说明）
        if let Some(kind) = call_object
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
        {
            if kind != "function" {
                return Err(GatewayError::bad_request(format!(
                    "assistant.tool_calls[{call_index}].type 只支持 function"
                )));
            }
        }
        // `call?.id || call?.toolCallId`（真值判定）
        let id = call_object
            .and_then(|value| value.get("id").filter(|item| js_truthy(item)))
            .or_else(|| {
                call_object.and_then(|value| value.get("toolCallId").filter(|item| js_truthy(item)))
            })
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| GatewayError::bad_request("tool_call 缺少 id"))?;
        // `call?.function?.name || call?.toolName || call?.name`（真值判定）
        let name = call_object
            .and_then(|value| value.get("function"))
            .and_then(|value| value.get("name").filter(|item| js_truthy(item)))
            .or_else(|| {
                call_object.and_then(|value| value.get("toolName").filter(|item| js_truthy(item)))
            })
            .or_else(|| {
                call_object.and_then(|value| value.get("name").filter(|item| js_truthy(item)))
            })
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| GatewayError::bad_request("tool_call 缺少 function.name"))?;
        // `call?.function?.arguments ?? call?.toolParams ?? call?.arguments ?? ''`
        // —— **空值合并**：`arguments: ""` 命中空串（不再往后找），
        // 显式 `null` 跳过；三个都没有时是 `''`
        let raw_arguments = call_object
            .and_then(|value| value.get("function"))
            .and_then(|value| value.get("arguments"))
            .filter(|value| !value.is_null())
            .or_else(|| {
                call_object.and_then(|value| value.get("toolParams")).filter(|item| !item.is_null())
            })
            .or_else(|| {
                call_object.and_then(|value| value.get("arguments")).filter(|item| !item.is_null())
            })
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        normalized.push(NormalizedToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: validate_tool_arguments(&raw_arguments, call_index)?,
        });
    }
    Ok(normalized)
}

/// 工具参数规范化（原实现 `validateToolArguments`）。
///
/// 契约：**必须是 JSON 对象**（字符串形态要先能 `JSON.parse` 成对象）。
/// 字符串输入**原样返回**（保留客户端的空白与键序 —— 它进 `toolParams`、
/// 也进指纹，重序列化会让两条本该相同的消息指纹不同）；
/// 对象输入序列化成紧凑 JSON 文本（原实现 `JSON.stringify(value)`，
/// serde 的紧凑输出与 JS 一致：无空格、键序保持插入顺序）。
fn validate_tool_arguments(value: &Value, index: usize) -> Result<String, GatewayError> {
    match value {
        Value::String(text) => {
            let parsed: Value =
                match serde_json::from_str(if text.is_empty() { "{}" } else { text }) {
                    Ok(parsed) => parsed,
                    Err(_) => return Err(invalid_arguments(index)),
                };
            if !parsed.is_object() {
                return Err(invalid_arguments(index));
            }
            // 嵌套过深 / 危险键：原实现的 `validateJsonValue` 抛的是**它自己**的
            // 文案，但 `validateToolArguments` 里那一整段被 try/catch 包着，
            // 于是最终对外仍然是「必须是 JSON 对象字符串」——
            // 照抄这个结果（细节差异见报告：原实现顺带把这类模型的报错原因
            // 抹掉了，这里保留同样口径以免客户端看到不同错误）。
            if validate_json_value(&parsed, index).is_err() {
                return Err(invalid_arguments(index));
            }
            Ok(text.clone())
        }
        Value::Object(_) => {
            // 对象形态没有 try/catch 包裹（原实现直接调 validateJsonValue 后
            // JSON.stringify），所以嵌套/危险键的**具体文案**会透出来
            validate_json_value(value, index)?;
            serde_json::to_string(value).map_err(|_| invalid_arguments(index))
        }
        // `value ?? {}`：null / 缺失 → 空对象；其余类型（数字/数组/布尔）报错。
        // 这条文案与字符串分支**不同**（原实现非字符串分支抛的是
        // 「...必须是 JSON 对象」，没有「字符串」二字），照抄。
        Value::Null => Ok("{}".to_string()),
        _ => Err(GatewayError::bad_request(format!(
            "assistant.tool_calls[{index}].function.arguments 必须是 JSON 对象"
        ))),
    }
}

fn invalid_arguments(index: usize) -> GatewayError {
    GatewayError::bad_request(format!(
        "assistant.tool_calls[{index}].function.arguments 必须是 JSON 对象字符串"
    ))
}

/// 递归校验一个 JSON 值（原实现 `validateJsonValue`）：嵌套深度 ≤ 12，
/// 且不含 `__proto__` / `constructor` / `prototype` 这类危险键。
///
/// Rust 侧本来就没有原型污染问题，但**深嵌套与危险键在工具参数里没有正当
/// 用途**，而它们会被原样转发给上游模型；保持与原实现相同的拒绝口径，
/// 可以避免「Node 版拒绝、Rust 版放行」的行为漂移。深度口径照抄
/// （从 0 起算，> 12 判超限）。
fn validate_json_value(value: &Value, index: usize) -> Result<(), GatewayError> {
    fn walk(value: &Value, depth: usize, index: usize) -> Result<(), GatewayError> {
        if depth > 12 {
            return Err(GatewayError::bad_request(format!(
                "assistant.tool_calls[{index}].function.arguments 嵌套过深"
            )));
        }
        match value {
            Value::Array(items) => items.iter().try_for_each(|item| walk(item, depth + 1, index)),
            Value::Object(object) => {
                for (key, item) in object {
                    if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
                        return Err(GatewayError::bad_request(format!(
                            "assistant.tool_calls[{index}].function.arguments 包含不允许的字段 {key}"
                        )));
                    }
                    walk(item, depth + 1, index)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(value, 0, index)
}

/// tool 消息的结果文本（原实现 `toolResultContent`）。
///
/// 取值优先级：`content` → `toolResult` → `result`（`??` 语义：
/// `content: ""` 命中空串；显式 null 跳过；三者都缺 → 空串）。
/// 数组形态：
///   - 空数组 → 空串；
///   - 全字符串 → `\n` 连接；
///   - 全是 text / output_text 块 → 取各块 `text` 后 `\n` 连接；
///   - 其它（或含非 text 块）→ 整体序列化成 JSON（`stringifyToolData`）。
pub(super) fn tool_result_content(
    object: &Map<String, Value>,
    index: usize,
) -> Result<String, GatewayError> {
    let value = object
        .get("content")
        .filter(|item| !item.is_null())
        .or_else(|| object.get("toolResult").filter(|item| !item.is_null()))
        .or_else(|| object.get("result").filter(|item| !item.is_null()));
    let Some(value) = value else {
        return Ok(String::new());
    };
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(items) => {
            if items.is_empty() {
                return Ok(String::new());
            }
            if items.iter().all(Value::is_string) {
                return Ok(items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            let all_text_blocks = items.iter().all(|item| {
                item.as_object().is_some_and(|block| {
                    matches!(
                        block.get("type").and_then(Value::as_str),
                        Some("text") | Some("output_text")
                    ) && block.get("text").and_then(Value::as_str).is_some()
                })
            });
            if all_text_blocks {
                return Ok(items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            stringify_tool_data(value, index)
        }
        // 非字符串标量（数字/布尔）：原实现 `stringifyToolData` 对它们抛 400
        // （文案里不带收到什么类型 —— 照抄，不加后缀）
        _ => Err(GatewayError::bad_request(format!(
            "messages[{index}].content 必须是字符串或 JSON 结构"
        ))),
    }
}

/// 任意 JSON 结构 → 紧凑 JSON 文本（`stringifyToolData` 的对象/数组分支）
fn stringify_tool_data(value: &Value, index: usize) -> Result<String, GatewayError> {
    validate_json_value(value, index)?;
    serde_json::to_string(value).map_err(|_| {
        GatewayError::bad_request(format!("messages[{index}].content 无法序列化为 JSON"))
    })
}
