//! 网关自有系统提示词：`passthrough` / `custom` / `append` 三模式。
//!
//! 照搬 workbuddy2api 的 `internal/prompt/prompt.go`（模式语义、插入位置、守卫
//! 逐条对齐），它是脱敏之外的第二层防护 —— 两层**叠加、互不替代**：
//!
//! ```text
//!   ① 提示词层（本模块）：出站前用网关自有提示词**替换/追加** system 消息，
//!      从源头消灭「system 来源」的指纹 —— 客户端注入的模板句根本没机会出站。
//!   ② 指纹脱敏层（core::sanitize）：改写/剥离请求体里残留的指纹，覆盖
//!      user / assistant / tool_calls / reasoning_content 这些**用户内容**，
//!      以及网关提示词自己万一命中的情况（两层的顺序见 `upstream::payload`）。
//! ```
//!
//! ── 为什么要有提示词层（而不是只靠脱敏）─────────────────────
//! 上游按**逐字精确匹配**拦截（非语义审核），所以「把模板句改一个字」能绕过
//! ——那是脱敏层在做的事。但模板句是**成片**的（Claude Code / Codex 的 system
//! 提示词有几十句），脱敏只能覆盖实测命中过的那几句；换一个客户端版本就可能
//! 冒出新的一句。`custom` 模式从根上解决：客户端 system 整段换成网关自己那份，
//! 上游看到的 system 永远是网关写的、可控的一段文本。
//!
//! ── 三模式的语义（与参考项目逐条一致）───────────────────────
//!   - `passthrough`（**默认**）：不动 system，客户端原样出站 —— 与改造前
//!     的行为逐字相同。此时唯一生效的是脱敏层；
//!   - `custom`：删除 messages 里**所有** `system` / `developer` 消息，在头部
//!     插入一条网关自有提示词（客户端项目规范随之消失，适合「我要用网关的提示词」
//!     的用户）；
//!   - `append`：在**开头连续**的 `system` / `developer` 块之后插入一条网关
//!     自有提示词，既有消息逐字不动（客户端项目规范与网关提示词并用）。
//!
//! ── 降级（`core::degrade`）只作用于 passthrough / append ─────
//! `custom` 模式下 system 已经由网关接管，再撞内容拦截就不是 system 指纹的问题
//! 了（多半是用户内容本身触发审核），所以它**不进降级路径**（`degradable`）。
//! `append` 在降级期退化为替换（`rewrite` + 中性提示词）：带着指纹原文重试是
//! 确定性再撞墙，替换是一次性最小抢救。
//!
//! ── 本模块只做纯文本变换 ─────────────────────────────────────
//! 模式与提示词文本从哪来（配置文件键 / 提示词文件 / 内置默认）由 `config` 决定，
//! 开关时机由转发层决定；本模块无状态、无锁、无 IO，只回答「给定 body 与提示词，
//! 出站的那份应该长什么样」。

use serde_json::{json, Value};

/// 降级提示词：内容拦截误报时用的**最小中性提示词**（对应参考项目的
/// `prompt.Degraded`）。
///
/// 刻意极简：降级期间要的是「尽量不含任何会被逐字匹配的模板句」，同时不改变
/// 用户指令的合法性语义 —— 它不是对抗审核的手段，只是绕开**误报**。
pub const DEGRADED_PROMPT: &str = "You are a helpful assistant. Answer in the user's language, \
follow the user's instructions, and keep replies direct and concise.";

/// 内置默认提示词：`custom` / `append` 模式**未指定文件**时用它。
///
/// 与参考项目一样「网关自己有一份能直接用的提示词」，省得用户为了试一下
/// `custom` 模式还得先准备文件。文本是**我们自己写的**通用编码助手提示词
/// （参考项目那份是它自己的 300 行提示词，与本项目的产品定位无关，故不照搬）：
/// 要点是「不包含任何会被上游逐字拦截的指纹句」，用户想换成自己的提示词就把
/// 路径填进 `promptFile`（指向任何 UTF-8 文本文件即可）。
pub const BUILT_IN_PROMPT: &str = "\
You are a helpful coding assistant working inside the user's local development environment.

- Answer in the user's language unless asked otherwise.
- Be direct and concise: lead with the answer, then the details that matter.
- Use the tools you are given to read and change files, and never claim to have run \
something you did not actually run.
- Prefer the smallest change that solves the problem, and match the style of the surrounding code.
- When something is uncertain, say so and check instead of guessing.
- Do not run destructive commands (deleting data, resetting repositories, force-pushing) \
without explicit confirmation from the user.";

/// 系统提示词的三种模式（配置文件里的字符串就是契约，见 `config::KEY_PROMPT_MODE`）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptMode {
    /// 透传客户端 system（默认；与改造前行为逐字相同）
    #[default]
    Passthrough,
    /// 用网关自有提示词替换客户端的 system / developer 消息
    Custom,
    /// 在开头连续的 system / developer 块之后追加一条网关自有提示词
    Append,
}

impl PromptMode {
    /// 全部合法值（界面下拉与 400 文案共用一份，避免两处各写一遍）
    pub const ALL: [PromptMode; 3] = [
        PromptMode::Passthrough,
        PromptMode::Custom,
        PromptMode::Append,
    ];

    /// 配置值与接口响应里的字符串形态
    pub fn as_str(self) -> &'static str {
        match self {
            PromptMode::Passthrough => "passthrough",
            PromptMode::Custom => "custom",
            PromptMode::Append => "append",
        }
    }

    /// 解析配置值：大小写不敏感 + 去首尾空白（对应 Go 的
    /// `strings.ToLower(strings.TrimSpace(...))`）。空串按**默认**处理，
    /// 与参考项目「缺省 passthrough」一致；非法值给 `None`（由调用方决定
    /// 报错还是回落 —— 手改文件回落、走接口 400）。
    pub fn parse(text: &str) -> Option<Self> {
        let normalized = text.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Some(PromptMode::Passthrough);
        }
        PromptMode::ALL
            .into_iter()
            .find(|mode| mode.as_str() == normalized)
    }

    /// 界面与日志用的中文标签
    pub fn label(self) -> &'static str {
        match self {
            PromptMode::Passthrough => "透传客户端 system",
            PromptMode::Custom => "替换为网关提示词",
            PromptMode::Append => "追加网关提示词",
        }
    }

    /// 该模式是否会在降级时改用中性提示词。
    ///
    /// `custom` 为假：system 已由网关接管，内容拦截不再指向 system 指纹
    /// （见模块头「降级只作用于 passthrough / append」）。
    pub fn degradable(self) -> bool {
        self != PromptMode::Custom
    }
}

/// 提示词文本的来源（界面与日志要能回答「这次用的到底是哪一份」）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSource {
    /// 没有提示词文本（`passthrough` 模式）
    None,
    /// 内置默认（`custom` / `append` 且未指定文件）
    BuiltIn,
    /// 用户指定的提示词文件
    File,
}

impl PromptSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptSource::None => "none",
            PromptSource::BuiltIn => "builtin",
            PromptSource::File => "file",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PromptSource::None => "不透传（passthrough）",
            PromptSource::BuiltIn => "内置默认提示词",
            PromptSource::File => "提示词文件",
        }
    }
}

/// 一次请求的提示词决定（模式 + 文本的**借用**视图）。
///
/// 借用而不是克隆：本结构由 `upstream::forward` 从配置快照里取一次，随
/// `ProviderContext` 借给整条转发链 —— 提示词可能有几百行，逐请求克隆一份
/// 纯属浪费（`config::current()` 已经克隆了整份配置，那份克隆在本函数栈帧里
/// 活得比 ctx 久，所以这里借得到）。
#[derive(Clone, Copy, Debug)]
pub struct PromptPlan<'a> {
    pub mode: PromptMode,
    /// `custom` / `append` 要用的提示词文本（`passthrough` 下为空串）
    pub text: &'a str,
    /// 这段文本的来源（内置默认 / 提示词文件 / 无）—— 详细日志与设置页要能回答
    /// 「这次用的到底是哪一份」
    pub source: PromptSource,
}

impl<'a> PromptPlan<'a> {
    /// 降级期该用哪份文本：可降级的模式用中性提示词，否则用模式自己的文本
    pub fn text_for(&self, degraded: bool) -> &str {
        if degraded && self.mode.degradable() {
            DEGRADED_PROMPT
        } else {
            self.text
        }
    }

    /// 出站前把提示词落到 body 上：`Some(副本)` = 改过，`None` = **不动**
    /// （`passthrough` 且未降级 —— 默认路径，调用方据此零拷贝沿用客户端原始体）。
    ///
    /// `degraded` 是**本次请求**是否已进入降级（请求开始时状态机已生效，或
    /// 本次请求撞了内容拦截后由转发层置位，见 `upstream::provider_loop`）。
    pub fn apply(&self, body: &Value, degraded: bool) -> Option<Value> {
        let text = self.text_for(degraded);
        if text.is_empty() {
            return None;
        }
        match self.mode {
            // 透传且未降级：没有任何要动的地方（上面 text 为空已经返回了，
            // 这里再挡一次，语义上「passthrough 就是不动」）
            PromptMode::Passthrough => {
                if degraded {
                    rewrite(body, text)
                } else {
                    None
                }
            }
            // 降级期：append 也退化为替换（见模块头）
            PromptMode::Append if !degraded => append(body, text),
            PromptMode::Custom | PromptMode::Append => rewrite(body, text),
        }
    }
}

/// 替换：删除 messages 里**所有** `system` / `developer` 消息，头部插入一条
/// 网关提示词（对应 Go 的 `prompt.Rewrite`）。
///
/// 无 `messages` 字段（或不是数组）→ 只补一条 `messages: [网关 system]`，
/// 其余字段原样保留 —— 与参考项目同口径：转发链的关键路径上「绝不失败」，
/// 形态怪异时也要产出一份能发出去的 body。
///
/// 与 Go 的一处差异：Go 在 JSON 解析失败时返回原 body；本模块拿到的已经是
/// `Value`，没有解析这一步。
pub fn rewrite(body: &Value, text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    let gateway = gateway_message(text);
    match object.get_mut("messages").and_then(Value::as_array_mut) {
        Some(messages) => {
            // 先放网关提示词，再逐个保留非 system/developer —— 插入位置与
            // 过滤在同一次遍历里完成（参考项目也是这么写的）
            let mut kept: Vec<Value> = Vec::with_capacity(messages.len() + 1);
            kept.push(gateway);
            kept.extend(
                messages
                    .iter()
                    .filter(|message| !is_system_role(message))
                    .cloned(),
            );
            *messages = kept;
        }
        None => {
            object.insert("messages".to_string(), Value::Array(vec![gateway]));
        }
    }
    Some(next)
}

/// 追加：在**开头连续**的 `system` / `developer` 块之后插入一条网关提示词，
/// 既有消息逐字不动（对应 Go 的 `prompt.Append`）。
///
/// 「开头连续块」的判定与 [`rewrite`] 的删除口径**逐条一致**：从 `messages[0]`
/// 起向后只要是 system/developer 就继续，遇到第一条非 system/developer
/// （含非对象消息、无 role 消息）即停。
pub fn append(body: &Value, text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    let gateway = gateway_message(text);
    match object.get_mut("messages").and_then(Value::as_array_mut) {
        Some(messages) => {
            let insert_at = messages
                .iter()
                .take_while(|message| is_system_role(message))
                .count();
            messages.insert(insert_at, gateway);
        }
        None => {
            object.insert("messages".to_string(), Value::Array(vec![gateway]));
        }
    }
    Some(next)
}

/// 网关提示词的消息形态。
///
/// 角色用 `system` 而不是 `developer`：上游的 role 白名单里没有 `developer`
/// （见 `providers::workbuddy::normalize` 的角色归一），插 `developer` 等于
/// 制造一次必然归一 + 多余的拦截风险窗口。参考项目也是同一条理由。
fn gateway_message(text: &str) -> Value {
    json!({ "role": "system", "content": text })
}

/// 该消息是否承载 system 级指令：角色**精确**等于 `system` 或 `developer`。
///
/// 精确匹配（不是前缀/包含）：其余角色（user / assistant / tool / 任意未知值）
/// 一律不动 —— 这是「既有消息逐字不动」的判据，宽松一点就会误删用户内容。
fn is_system_role(message: &Value) -> bool {
    message
        .get("role")
        .and_then(Value::as_str)
        .map(|role| role == "system" || role == "developer")
        .unwrap_or(false)
}
