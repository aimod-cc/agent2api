//! 工具声明形态适配：把 Responses 的几种声明形态摊平成 Chat 能认的一层。
//!
//! ── 为什么需要摊平 ──────────────────────────────────────────
//! Chat 的 `tools` 是一层扁平数组，每项是 `{type:"function", function:{…}}`；
//! 而 Responses 允许两种「分组 / 变形」声明：
//!   · `type:"custom"`：自由文本工具，没有 JSON 参数表（细节见 `freeform` 模块）；
//!   · `type:"namespace"`：命名空间，把一组工具打包在一个名字下。Codex 的
//!     Responses Lite 路径用它承载 `functions` / `clock` / `collaboration` /
//!     `mcp__*` 等工具组。
//!
//! 直接把 namespace 转发过去上游不认，丢掉更糟：模型收不到任何工具声明，
//! 只能把调用意图当正文吐出来（2026-09 那次「Codex 调不动工具」的根因）。
//!
//! ── 摊平与还原 ──────────────────────────────────────────────
//! 子工具的名字按 `namespace + name` 直接拼接 —— 这是 Codex 自己的口径
//! （它的 MCP 命名空间自带 `mcp__server__` 尾缀，拼出来正是 `mcp__server__status`
//! 这个规范名）。同时把 `扁平名 → (namespace, 原名)` 记进 [`ToolPlan::namespaced`]，
//! 回程据此把调用还原成带 `namespace` 字段的形态：Codex 的工具路由按
//! `{name, namespace}` 精确查表，少了 `namespace` 会报 `unsupported call`。
//!
//! 还原时按 Codex 的同一条规则归一（名字已以 namespace 开头就原样用，否则补前缀），
//! 这样「展平 → 还原」是幂等的，历史来回几轮也不会越拼越长。
//!
//! ── defer_loading 怎么处理 ──────────────────────────────────
//! Responses 的 namespace 子工具常带 `defer_loading: true`，语义是「先不给模型
//! 完整定义，等它用 `tool_search` 搜到再加载」。上游 Chat 接口没有 tool_search，
//! 于是这里**忽略 defer_loading、把定义全部下发**：能少占一点上下文固然好，
//! 但工具根本拿不到就是彻底不可用 —— 两害相权取其轻。

use serde_json::{json, Map, Value};

use super::{freeform, is_truthy, string_field, string_value};
use crate::server::logging;

/// `additional_tools` 输入项的 type 字面量。
///
/// Codex 的 Responses Lite 路径用它承载工具声明：顶层 `tools` 置 null，工具
/// 改放在 `input[]` 的这一个项里（见 OpenAI 文档「Add tools at a specific
/// point in the input」）。本网关的上游是 Chat 接口，没有「对话中途追加工具」
/// 的概念，所以统一提升成请求级工具声明。
const ADDITIONAL_TOOLS_ITEM: &str = "additional_tools";

/// 命名空间声明的 type 字面量
const NAMESPACE_TOOL: &str = "namespace";

/// 一次请求的工具声明经过摊平后的结果。
#[derive(Default)]
pub struct ToolPlan {
    /// 可直接交给 `tool_to_chat` 的扁平声明（namespace 子工具的名字已加前缀）
    pub declarations: Vec<Value>,
    /// 扁平名 → (namespace, 原名)：回程还原 `namespace` 字段用
    pub namespaced: std::collections::BTreeMap<String, (String, String)>,
    /// 原本是 custom（自由文本）的**扁平名**：回程据此决定 item 类型
    pub custom_names: std::collections::BTreeSet<String>,
}

impl ToolPlan {
    /// 这个名字是否原本是 custom 工具
    pub fn is_custom(&self, name: &str) -> bool {
        self.custom_names.contains(name)
    }
}

/// 收集请求里的全部工具声明并摊平。
///
/// 三个来源合并：顶层 `tools`（标准形态）、`input[]` 里的 `additional_tools`
/// 项（Codex Lite 形态），以及 namespace 展开出来的子工具。顶层排在前面，
/// 它是请求级声明，语义上先于中途追加。
pub fn plan_tools(request: &Value) -> ToolPlan {
    let mut plan = ToolPlan::default();
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            push_tool(&mut plan, tool);
        }
    }
    let Some(items) = request.get("input").and_then(Value::as_array) else {
        return plan;
    };
    for item in items {
        if !string_field(item, "type").eq_ignore_ascii_case(ADDITIONAL_TOOLS_ITEM) {
            continue;
        }
        // 规范形态是 `tools: [完整工具定义]`。但 Lite 路径在不同 Codex 版本里
        // 出现过只给名字（`tool_names: [...]`）的变体 —— 那种形态没有可转换的
        // 定义，只能留痕跳过，硬编一份假 schema 会让模型按错误参数调用
        if let Some(tools) = item.get("tools").and_then(Value::as_array) {
            for tool in tools {
                push_tool(&mut plan, tool);
            }
        } else if let Some(names) = item.get("tool_names").and_then(Value::as_array) {
            let listed: Vec<String> = names.iter().map(|name| string_value(name)).collect();
            logging::log(
                "[Responses]",
                &format!(
                    "⚠️ additional_tools 只给了工具名、没有定义，无法转发：{}",
                    listed.join(", ")
                ),
            );
        }
    }
    plan
}

/// 单个声明进计划：namespace 展开成子工具，其余原样收下。
fn push_tool(plan: &mut ToolPlan, tool: &Value) {
    if string_field(tool, "type").eq_ignore_ascii_case(NAMESPACE_TOOL) {
        expand_namespace(plan, tool);
        return;
    }
    record_custom(plan, tool);
    plan.declarations.push(tool.clone());
}

/// namespace → 一组扁平子声明，名字加命名空间前缀。
fn expand_namespace(plan: &mut ToolPlan, namespace: &Value) {
    let ns_name = string_field(namespace, "name");
    let ns_desc = string_field(namespace, "description");
    let Some(children) = namespace.get("tools").and_then(Value::as_array) else {
        // 没有子工具的 namespace 是空壳（Codex 用它占位），没什么可转的
        return;
    };
    for child in children {
        // 嵌套 namespace：规范里没定义这种形态，不猜，留痕跳过
        if string_field(child, "type").eq_ignore_ascii_case(NAMESPACE_TOOL) {
            logging::log(
                "[Responses]",
                &format!("⚠️ 嵌套 namespace 无法展开，已跳过：{}", string_field(child, "name")),
            );
            continue;
        }
        let child_name = string_field(child, "name");
        if child_name.is_empty() {
            continue;
        }
        // 名字拼接口径见模块头（与 Codex 的归一规则一致）
        let flat = if ns_name.is_empty() {
            child_name.clone()
        } else {
            format!("{ns_name}{child_name}")
        };
        let mut decl = child.clone();
        if let Some(fields) = decl.as_object_mut() {
            fields.insert("name".to_string(), Value::String(flat.clone()));
            // 命名空间的描述折进子工具：模型只看到扁平名，不给分组说明就
            // 无从知道这组工具是干什么的
            if !ns_desc.is_empty() {
                append_description(fields, &format!("\n\n[所属分组 `{ns_name}`] {ns_desc}"));
            }
            // defer_loading 上游没有对应机制，去掉免得被当成未知字段
            fields.remove("defer_loading");
        }
        record_custom(plan, &decl);
        if !ns_name.is_empty() {
            plan.namespaced
                .insert(flat, (ns_name.clone(), child_name));
        }
        plan.declarations.push(decl);
    }
}

/// 记下这个声明是不是 custom（自由文本）工具
fn record_custom(plan: &mut ToolPlan, tool: &Value) {
    if !freeform::is_custom_tool(tool) {
        return;
    }
    let name = string_field(tool, "name");
    if !name.is_empty() {
        plan.custom_names.insert(name);
    }
}

/// 往 description 追加一段（没有 description 就新建）
fn append_description(fields: &mut Map<String, Value>, extra: &str) {
    let existing = string_field(&Value::Object(fields.clone()), "description");
    let merged = format!("{existing}{extra}");
    fields.insert("description".to_string(), Value::String(merged));
}

/// 工具调用里的名字还原成「展平名」。
///
/// 客户端回传的历史里，`function_call` 的 `name` 是**原名**、命名空间在
/// `namespace` 字段里；而给上游的 Chat 消息必须是展平后的名字，否则模型看到的
/// 工具名前后不一致。归一规则与 Codex 一致：已经带前缀就原样用（幂等）。
pub fn flatten_call_name(item: &Value) -> String {
    let name = string_field(item, "name");
    let namespace = string_field(item, "namespace");
    if namespace.is_empty() || name.is_empty() || name.starts_with(&namespace) {
        return name;
    }
    format!("{namespace}{name}")
}

/// 工具调用项 → 带 `namespace` 字段的 Responses item。
///
/// `plan` 里有这个扁平名的映射时补上 `namespace`：Codex 按
/// `{name, namespace}` 精确查表，缺了它直接报 `unsupported call`。
pub fn restore_namespace(mut item: Value, flat_name: &str, plan: &ToolPlan) -> Value {
    let Some((namespace, original)) = plan.namespaced.get(flat_name) else {
        return item;
    };
    if let Some(fields) = item.as_object_mut() {
        fields.insert("namespace".to_string(), Value::String(namespace.clone()));
        fields.insert("name".to_string(), Value::String(original.clone()));
    }
    item
}

/// 工具声明在日志里的可读标签（`function:bash` / `web_search` / …）。
///
/// 丢工具时必须留下它叫什么：只报「丢了 N 条」，排查的人无从判断丢的是不是
/// 关键能力 —— 2026-09 那次 Codex 工具失效，症状是模型把调用当正文吐出来，
/// 若当时有这行日志，一眼就能定位。
pub fn tool_kind_label(tool: &Value) -> String {
    let name = string_field(tool, "name");
    let kind = string_field(tool, "type");
    if name.is_empty() {
        if kind.is_empty() { "未命名工具".to_string() } else { kind }
    } else if kind.is_empty() || kind.eq_ignore_ascii_case("function") {
        name
    } else {
        format!("{kind}:{name}")
    }
}

/// 工具声明是否是「已经嵌套好的」Chat 形态（客户端混用两种形态时原样保留）
pub fn is_nested_function(tool: &Value) -> bool {
    string_field(tool, "type").eq_ignore_ascii_case("function") && tool.get("function").is_some()
}

/// 扁平 function 声明 → 嵌套 Chat 形态（`{type, name, parameters}` →
/// `{type:"function", function:{name, parameters}}`）。名字缺失时返回 `None`。
pub fn nested_from_flat(tool: &Value) -> Option<Value> {
    let name = string_field(tool, "name");
    if name.is_empty() {
        return None;
    }
    let parameters = tool
        .get("parameters")
        .filter(|value| is_truthy(value))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    let mut function = Map::new();
    function.insert("name".to_string(), Value::String(name));
    if let Some(description) = tool.get("description").filter(|value| is_truthy(value)) {
        function.insert("description".to_string(), description.clone());
    }
    function.insert("parameters".to_string(), parameters);
    // strict 是 Responses 的字段，Chat 的 function 里也认（部分上游支持）
    if let Some(strict) = tool.get("strict") {
        function.insert("strict".to_string(), strict.clone());
    }
    Some(json!({ "type": "function", "function": Value::Object(function) }))
}
