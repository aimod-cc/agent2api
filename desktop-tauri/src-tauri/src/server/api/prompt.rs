//! 系统提示词与内容拦截降级：`GET/PUT /api/prompt`。
//!
//! ── 这条端点管什么 ──────────────────────────────────────────
//! 两件相关的事（同一处读写，因为它们是同一套机制的开关与状态）：
//!
//!   ① **系统提示词模式**（`promptMode` / `promptFile`）：`passthrough`（默认，
//!      透传客户端 system）/ `custom`（用网关自有提示词替换 system 消息）/
//!      `append`（在开头连续 system 块之后追加一条）。它是脱敏之外的第二层
//!      防护 —— 从源头消灭 system 来源的指纹，见 `core::prompt` 的模块头。
//!   ② **降级状态**（只读 + 一个解除动作）：`passthrough` / `append` 模式下撞了
//!      内容拦截（HTTP 400 + 审核文案）时，转发层会立刻换中性提示词重试一次，
//!      并把「降级期」开到次日 00:00（UTC+8）—— 期间所有请求直接带中性提示词
//!      出门，不再先撞一次 400（见 `core::degrade`）。
//!      `degradeActive` / `degradeUntil` 让用户看得见这段状态；`clearDegrade`
//!      是参考项目没有的一个显式出口（它只能等到零点）：用户当场把提示词改好
//!      （例如切到 `custom`）之后，不该还被锁在降级期里。
//!
//! ── 为什么与 /api/sanitize 分成两条端点 ───────────────────────
//! 那条是**单个布尔开关**（同形于 /api/debug）；这条有枚举、有路径、还有一个
//! 运行期状态要一起返回 —— 形状不同，硬塞进一条会让两边的字段语义都变模糊。
//! 与 `/api/retry` 更接近（多字段 + 允许部分更新 + 返回生效后的全量值）。
//!
//! ── 文件校验为什么在写侧做（与读侧的回落口径不同）─────────────
//! 读侧（`config::prompt_from`）遇到读不到的文件会**回落内置默认 + 记一条原因**：
//! 桌面应用不能因为一个提示词文件启动不了、也不能因此拒绝转发。写侧则相反 ——
//! 用户就站在这里，路径打错要当场知道（400 + 可读原因），而不是保存成功后
//! 在界面上发现「来源：内置默认」才知道没生效。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{self, PromptSettings, KEY_PROMPT_FILE, KEY_PROMPT_MODE};
use crate::server::core::degrade;
use crate::server::core::prompt::PromptMode;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 降级解除动作的请求键（`PUT` 的布尔字段）
const KEY_CLEAR_DEGRADE: &str = "clearDegrade";

/// GET /api/prompt —— 当前模式 / 生效文本来源 / 降级状态
pub async fn get_prompt(State(_state): State<ServerState>) -> Response {
    ok_json(prompt_json(&config::current()))
}

/// PUT /api/prompt —— body `{promptMode?, promptFile?, clearDegrade?}`
///
/// 允许部分字段（未出现的项保持原值，**`null` 同义**）—— 与 `/api/retry` 同一口径。
/// 要**清除**提示词文件就传空串（或只含空白的串）：`promptFile: ""` 表示
/// 「改回内置默认提示词」。这个区分是必要的 —— 桥接层（`bridge.rs`）在
/// 「只解除降级」时会带上 `promptFile: null`，若把 `null` 也当成清除，
/// 点一下「立即解除」就会把用户配好的路径悄悄抹掉。
/// 校验全部通过才写盘：模式非法、或新模式要用的文件读不到 → 400 且什么都不改。
pub async fn put_prompt(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let current = config::current();
    let settings = current.prompt_settings();

    // ── 模式 ──────────────────────────────────────────────
    let mode = match object.get(KEY_PROMPT_MODE) {
        // 未出现 / null 都视为「这一项不改」
        None | Some(Value::Null) => settings.mode,
        Some(Value::String(text)) => match PromptMode::parse(text) {
            Some(mode) => mode,
            None => {
                return errors::management_error(
                    400,
                    format!(
                        "{KEY_PROMPT_MODE} 只认 {}（收到: {text}）",
                        allowed_modes()
                    ),
                )
            }
        },
        Some(value) => {
            return errors::management_error(
                400,
                format!("{KEY_PROMPT_MODE} 必须是字符串（收到: {value}）"),
            )
        }
    };

    // ── 提示词文件（空串 = 清除，null = 不改）─────────────────
    let file = match object.get(KEY_PROMPT_FILE) {
        None | Some(Value::Null) => settings.file.clone(),
        Some(Value::String(text)) => Some(text.clone()).filter(|text| !text.trim().is_empty()),
        Some(value) => {
            return errors::management_error(
                400,
                format!("{KEY_PROMPT_FILE} 必须是字符串或 null（收到: {value}）"),
            )
        }
    };
    // 新模式要用文件时才校验：passthrough 不读文件（与读侧一致），
    // 那时路径写错也不影响任何行为，拦下来只会挡住「先存着、以后切模式」的用法
    if matches!(mode, PromptMode::Custom | PromptMode::Append) {
        if let Some(path) = file.as_deref() {
            if let Err(message) = config::read_prompt_file(path) {
                return errors::management_error(400, message);
            }
        }
    }

    // ── 降级解除（可选动作，与上面的配置写入相互独立）──────────────
    let clear_degrade = object
        .get(KEY_CLEAR_DEGRADE)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if clear_degrade && degrade::active() {
        degrade::clear();
        logging::log(
            "[Config]",
            "内容拦截降级已手动解除：后续请求恢复本模式自己的系统提示词",
        );
    }

    // ── 落盘 ─────────────────────────────────────────────
    // 无论配置项有没有变都重跑一次解析：它顺带把**提示词文件此刻的内容**重新读
    // 一遍（用户刚改完文件、或刚把文件恢复出来，一次 PUT 就能生效）。
    if !config::set_prompt(mode, file) {
        logging::log(
            "[Config]",
            "⚠️  系统提示词设置写入失败，本次运行内仍生效",
        );
    }
    let after = config::current();
    let next = after.prompt_settings();
    // 只在**真的变了**的时候写运行日志：解除降级那一次 PUT 也会走到这里，
    // 而它一个配置项都没动 —— 为它写一行「系统提示词已更新」是误导。
    let changed = next.mode != settings.mode
        || next.file != settings.file
        || next.source != settings.source
        || next.file_error != settings.file_error;
    if changed {
        logging::log(
            "[Config]",
            &format!(
                "系统提示词已更新: {}（{}）{}",
                next.mode.label(),
                next.source.label(),
                match next.file_error.as_deref() {
                    Some(reason) => format!("；注意: {reason}"),
                    None => String::new(),
                },
            ),
        );
    }
    ok_json(prompt_json(&after))
}

/// 响应体（GET 与 PUT 共用；前端直接用响应刷新界面，不必再 GET 一次）。
///
/// 键名对前端是**契约**（`settings-panel.js` 逐字对齐），因此都用常量标识符
/// 而不是手写字符串（与 `retry_json` 同一手法）。
fn prompt_json(config: &config::RuntimeConfig) -> Value {
    let settings: &PromptSettings = config.prompt_settings();
    json!({
        KEY_PROMPT_MODE: settings.mode.as_str(),
        KEY_PROMPT_FILE: settings.file.clone().unwrap_or_default(),
        // 生效文本的来源：none（passthrough）/ builtin（内置默认）/ file（提示词文件）
        "promptSource": settings.source.as_str(),
        // 指定了文件但读不到时的原因（读侧回落内置默认，原因在这里显示给用户）
        "promptFileError": settings.file_error,
        // 生效文本的行数（界面提示「内置默认 N 行 / 文件 N 行」；不返回全文 ——
        // 提示词可能几百行，塞进每次界面刷新纯属浪费）
        "promptLines": settings.text.lines().count(),
        // 降级状态（见 `core::degrade`）：active = 现在处于降级期
        "degradeActive": degrade::active(),
        "degradeUntil": degrade::until_ms(),
        "degradeUntilText": degrade::until_text(),
    })
}

/// 400 文案里列出的合法模式
fn allowed_modes() -> String {
    PromptMode::ALL
        .iter()
        .map(|mode| mode.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}
