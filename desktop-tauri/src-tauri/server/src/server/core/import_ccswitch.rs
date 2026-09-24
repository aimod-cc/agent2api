//! 「从其他工具导入」的第一个来源：**cc-switch** 的供应商扫描。
//!
//! ── cc-switch 是什么、为什么能直接读它的文件 ──────────────────
//! cc-switch 是本机桌面工具（Tauri + SQLite），管理 Claude Code / Codex /
//! Gemini CLI 等客户端的供应商配置，数据全部在 `~/.cc-switch/cc-switch.db`。
//! 它没有对外接口，但路径固定、本网关也是本机 Rust 进程 —— 直接以只读方式
//! 打开同一个 SQLite 文件即可，用户零配置。参考实现（cc-switch
//! `src-tauri/src/provider.rs` 的 `resolve_usage_credentials`）给出了每个
//! app_type 从 `settings_config` JSON 里提取 base_url / api_key 的规则，
//! 这里的提取逻辑逐条对应（claude / codex 两类；其余类型按产品决定灰显）。
//!
//! ── 扫描结果的三类 ──────────────────────────────────────────
//!   · `importable = true` —— claude / codex 且 base_url 与 api_key 齐全、
//!     本地没有同名提供商：前端可勾选，「导入所选」按这份字段创建自定义
//!     提供商 + 首个账号（复用 `POST /api/custom-providers`，不在本模块）。
//!   · 同名（`reason` = 本地已存在）—— 与已建自定义提供商同名的跳过并提示
//!     （产品决定：同名跳过）。
//!   · 其他 —— cc-switch 的其余 app_type（gemini / grokbuild / opencode /
//!     openclaw / hermes / pi / claude-desktop）与缺少端点或凭证的条目：
//!     `importable = false` + 原因文案，前端灰显标注「不支持」，让用户知道
//!     为什么列表里少了谁，而不是悄悄消失。
//!
//! ── 明文 api_key 为什么可以出现在响应里 ────────────────────────
//! 这是本机进程间的读取：请求方（本机 WebView）与读取方（本机网关）在同一
//! 台机器上，Key 不离开本机；而导入动作本来就要把它写进账号库。接口仍挂在
//! protected（登录态）之后，与「读本机客户端登录态导入」的各家入口同一档。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 绝不 panic（release 是 panic=abort）：SQLite 打不开、行数据是坏 JSON、
//! TOML 解析失败一律按「该条不可导入」降级，扫描整体永不失败。
//! 只读打开（`SQLITE_OPEN_READ_ONLY`）：cc-switch 可能正在运行，绝不能
//! 对它的数据库产生任何写入（连 WAL 文件都不该由我们触发）。

use std::path::PathBuf;

use rusqlite::OpenFlags;
use serde_json::{json, Map, Value};

use crate::server::core::custom_providers;

/// 可导入的 app_type（SQLite 里 `providers.app_type` 的实际取值，与 cc-switch
/// `AppType::as_str()` 一致）。claude-desktop 与 claude 是**同一形状**的配置
/// （都是 Anthropic 形态的 env map，cc-switch 的凭证提取也是两类同一分支），
/// 所以一并可导。
const SUPPORTED_TYPES: [&str; 3] = ["claude", "codex", "claude-desktop"];

/// 其余 app_type → 界面上的显示名（灰显行的「不支持」说明用）。
fn unsupported_label(app_type: &str) -> &'static str {
    match app_type {
        "gemini" => "Gemini CLI",
        "grokbuild" => "Grok Build",
        "opencode" => "OpenCode",
        "openclaw" => "OpenClaw",
        "hermes" => "Hermes",
        "pi" => "Pi",
        _ => "该客户端",
    }
}

/// 用户主目录（Windows 优先 USERPROFILE，与壳侧 / 各家凭证读取同一顺序，
/// 不引入 dirs crate）。
fn home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// cc-switch 的数据库路径（`~/.cc-switch/cc-switch.db`）。
///
/// 不复刻它 v3.10.3 的 `HOME` 环境变量回退分支：那是一次历史事故的兼容，
/// 默认位置读不到就如实报「未找到」，比猜一个旧位置更诚实 —— 猜错了等于
/// 把另一个用户的配置静默导入。
fn cc_switch_db_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".cc-switch").join("cc-switch.db"))
}

/// JSON 字符串字段（缺失 / 非字符串 → 空串）
fn string_field(value: Option<&Value>, key: &str) -> String {
    value
        .and_then(|item| item.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 多个候选键里第一个非空字符串（与 cc-switch 的 `first_non_empty` 同一语义：
/// preset 会把键留成空串占位，所以必须「跳过空串」而不是「跳过缺失」）
fn first_non_empty(env: Option<&Value>, keys: &[&str]) -> String {
    let Some(env) = env else { return String::new() };
    for key in keys {
        let text = string_field(Some(env), key);
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// Codex 的 `config` 字段是 **TOML 字符串**。提取激活 provider 的 base_url：
/// 优先 `model_provider`（激活名）→ `model_providers.<名>.base_url`，回退顶层
/// `base_url`。规则抄自 cc-switch `codex_config::extract_codex_base_url`
/// —— 非激活的 `[model_providers.*]` 段绝不读（遗留段不该泄进导入结果）。
///
/// 解析走 `toml::from_str::<toml::Table>`（serde 的**文档级**入口）。
/// 不能用 `toml::Value::from_str`：toml 0.9 里它的语义是「单个内联值」，
/// 对文档级输入（多行 `key = value`）一律报错 —— 真实踩过：Codex 条目的
/// base_url 因此全部提取为空，列表里整批标「未配置自定义端点」。
fn extract_codex_base_url(config_text: &str) -> String {
    let Ok(doc) = toml::from_str::<toml::Table>(config_text) else {
        return String::new();
    };
    if let Some(active) = doc.get("model_provider").and_then(|v| v.as_str()) {
        if let Some(url) = doc
            .get("model_providers")
            .and_then(|providers| providers.get(active))
            .and_then(|provider| provider.get("base_url"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            return url.to_string();
        }
    }
    doc.get("base_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Codex 的 API Key：`auth.OPENAI_API_KEY` 优先，回退 config TOML 里激活
/// provider（或顶层）的 `experimental_bearer_token`（cc-switch 的两级顺序）。
fn extract_codex_api_key(settings: &Value, config_text: &str) -> String {
    let from_auth = string_field(settings.get("auth"), "OPENAI_API_KEY");
    if !from_auth.is_empty() {
        return from_auth;
    }
    let Ok(doc) = toml::from_str::<toml::Table>(config_text) else {
        return String::new();
    };
    if let Some(active) = doc.get("model_provider").and_then(|v| v.as_str()) {
        let token = doc
            .get("model_providers")
            .and_then(|providers| providers.get(active))
            .and_then(|provider| provider.get("experimental_bearer_token"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if !token.is_empty() {
            return token.to_string();
        }
    }
    doc.get("experimental_bearer_token")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Codex 的默认模型（config.toml 顶层 `model`）：导入时作为**初始模型清单**
/// 带给新家 —— 模型登记是路由的前提，不带的话导入的家要先去模型管理页登记
/// 才能用（见前端 runImport 的说明）。
fn extract_codex_model(config_text: &str) -> String {
    let Ok(doc) = toml::from_str::<toml::Table>(config_text) else {
        return String::new();
    };
    doc.get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Claude Code 的 `[1M]` 后缀是**本地能力标记**（声明 100 万上下文），上游
/// 一般都不接受，转发前必须剥离 —— cc-switch 自己也这么做（`proxy/model_mapper.rs`
/// 的 `strip_one_m_suffix_for_upstream`，原注释：「上游 API 通常不接受这个本地
/// 能力标记，转发前需要剥离」）。
///
/// 返回 `(上游模型名, 需要映射的客户端名)`：客户端名只在**确实带标记**时非空
/// —— 客户端（Claude Code）发过来的正是带标记的那个名字，没有 alias → 上游名
/// 这条映射，它会因为「模型不在清单里」被直接拒掉（见
/// `custom_providers::bindings::resolve`）。
fn split_one_m_marker(model: &str) -> (String, String) {
    /// 与 cc-switch 的 `ONE_M_CONTEXT_MARKER` 同一字面量（大小写不敏感匹配）
    const MARKER: &str = "[1M]";
    let trimmed = model.trim();
    if trimmed.len() > MARKER.len()
        && trimmed.to_ascii_lowercase().ends_with(&MARKER.to_ascii_lowercase())
    {
        let clean = trimmed[..trimmed.len() - MARKER.len()].trim_end().to_string();
        if !clean.is_empty() {
            return (clean, trimmed.to_string());
        }
    }
    (trimmed.to_string(), String::new())
}

/// 一个 claude 类条目要登记的模型：`(清单, 映射)`。
///
/// 取值的两个来源，顺序即优先级：
///   1. `ANTHROPIC_MODEL` —— 主模型，用户在用的那个；
///   2. `ANTHROPIC_DEFAULT_*_MODEL` 的值 —— Claude Code 的档位覆盖，非接管
///      模式下就是上游真实模型名（实测有主模型为空、档位里却有真名的条目）。
/// `*_MODEL_NAME` 一律不取：那是 Claude Code 模型菜单里的**显示名**
/// （cc-switch 注释：「否则 Claude Code 模型菜单会残留上一个供应商的显示名称」），
/// 不是发给上游的 id。
///
/// 上限 6 个：这是一份「开箱能用」的起步清单，不是上游目录（拉全量有
/// 「获取模型」）。清单为空是合法结果（该条目没配任何模型覆盖）。
fn collect_claude_models(env: Option<&Value>) -> (Vec<String>, Vec<Value>) {
    /// 起步清单的上限（档位通常 3~4 个，6 足够且不失控）
    const MAX_MODELS: usize = 6;
    let mut models: Vec<String> = Vec::new();
    let mut mappings: Vec<Value> = Vec::new();
    let mut push = |raw: &str| {
        let (clean, alias) = split_one_m_marker(raw);
        if clean.is_empty() {
            return;
        }
        if !models.iter().any(|known| known.eq_ignore_ascii_case(&clean)) {
            if models.len() >= MAX_MODELS {
                return;
            }
            models.push(clean.clone());
        }
        if !alias.is_empty() {
            let known = mappings.iter().any(|item| {
                item.get("alias").and_then(Value::as_str)
                    .is_some_and(|text| text.eq_ignore_ascii_case(&alias))
            });
            if !known && mappings.len() < MAX_MODELS {
                mappings.push(json!({ "alias": alias, "target": clean }));
            }
        }
    };
    push(&string_field(env, "ANTHROPIC_MODEL"));
    if let Some(env) = env.and_then(Value::as_object) {
        for (key, value) in env {
            if !key.starts_with("ANTHROPIC_DEFAULT_") || !key.ends_with("_MODEL") {
                continue;
            }
            if let Some(text) = value.as_str() {
                push(text);
            }
        }
    }
    (models, mappings)
}

/// 一条 cc-switch 供应商记录 → 扫描条目（含可导入判定与原因）。
fn scan_entry(
    id: &str,
    app_type: &str,
    name: &str,
    notes: &str,
    settings: &Value,
    local_names: &std::collections::HashSet<String>,
) -> Value {
    let mut base_url = String::new();
    let mut api_key = String::new();
    let mut protocol = String::new();
    // 初始模型清单与配套映射（claude 类从 env 收集；codex 只有单个 model）
    let mut models: Vec<String> = Vec::new();
    let mut model_mappings: Vec<Value> = Vec::new();

    match app_type {
        // Anthropic 形态：Claude Code 与 Claude Desktop 的配置同一形状
        // （env 里的 BASE_URL + 多候选 Key，顺序抄 cc-switch 的同类分支）
        "claude" | "claude-desktop" => {
            let env = settings.get("env");
            base_url = string_field(env, "ANTHROPIC_BASE_URL");
            api_key = first_non_empty(
                env,
                &[
                    "ANTHROPIC_AUTH_TOKEN",
                    "ANTHROPIC_API_KEY",
                    "OPENROUTER_API_KEY",
                    "GOOGLE_API_KEY",
                ],
            );
            protocol = custom_providers::PROTOCOL_ANTHROPIC.to_string();
            (models, model_mappings) = collect_claude_models(env);
        }
        // Codex 形态：key 在 auth / TOML，base_url 在 TOML 字符串里
        "codex" => {
            let config_text = string_field(Some(settings), "config");
            base_url = extract_codex_base_url(&config_text);
            api_key = extract_codex_api_key(settings, &config_text);
            protocol = custom_providers::PROTOCOL_RESPONSES.to_string();
            let model = extract_codex_model(&config_text);
            if !model.is_empty() {
                models.push(model);
            }
        }
        _ => {}
    }

    // 不可导入的原因（按确定性排序：类型 → 端点 → 凭证 → 同名）
    let reason = if !SUPPORTED_TYPES.contains(&app_type) {
        format!("cc-switch 的 {} 配置，暂不支持导入", unsupported_label(app_type))
    } else if base_url.is_empty() {
        if app_type.starts_with("claude") {
            "官方直连配置，没有可导入的自定义端点".to_string()
        } else {
            "未配置自定义端点".to_string()
        }
    } else if api_key.is_empty() {
        "缺少 API Key".to_string()
    } else if local_names.contains(name) {
        "本地已存在同名提供商，跳过".to_string()
    } else {
        String::new()
    };
    let importable = reason.is_empty();

    json!({
        "id": id,
        "appType": app_type,
        "name": name,
        "notes": notes,
        // 明文字段只对可导入的条目给出（灰显行不需要，少一处凭证流转）
        "baseUrl": if importable { base_url.as_str() } else { "" },
        "apiKey": if importable { api_key.as_str() } else { "" },
        "protocol": protocol,
        // 初始模型清单与配套映射（模型登记是路由的前提，见前端 runImport）。
        // `models` 里是剥掉本地标记的上游名；`modelMappings` 是「带标记的
        // 客户端名 → 上游名」，客户端发带标记的名字也能路由到。两者都可能为空。
        "models": if importable { models } else { Vec::new() },
        "modelMappings": if importable { model_mappings } else { Vec::new() },
        "importable": importable,
        "reason": reason,
    })
}

/// 扫描 cc-switch 数据库，返回 API 响应体（永不失败：错误都在响应里）。
pub fn scan() -> Value {
    let Some(path) = cc_switch_db_path() else {
        return json!({
            "available": false,
            "reason": "无法定位用户主目录，找不到 cc-switch 的数据文件",
        });
    };
    let path_text = path.to_string_lossy().to_string();
    if !path.exists() {
        return json!({
            "available": false,
            "path": path_text,
            "reason": "未找到 cc-switch 的数据库（~/.cc-switch/cc-switch.db）——请确认本机安装了 cc-switch，并在里面添加过供应商",
        });
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &path,
        // 只读 + 不创建：cc-switch 可能正在运行，我们对它的库零写入
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            return json!({
                "available": false,
                "path": path_text,
                "reason": format!("无法读取 cc-switch 数据库（可能正被占用）: {error}"),
            });
        }
    };

    // 本地已建自定义提供商的名字集合（同名跳过判据）。list() 按盘上数据
    // 现读，成本可忽略（条目是个位数）。
    let local_names: std::collections::HashSet<String> = custom_providers
        ::list()
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();

    let mut stmt = match conn.prepare(
        "SELECT id, app_type, name, settings_config, COALESCE(notes, '') FROM providers",
    ) {
        Ok(stmt) => stmt,
        Err(error) => {
            return json!({
                "available": false,
                "path": path_text,
                "reason": format!("cc-switch 数据库结构与预期不符（{error}）：可能是不受支持的旧版本"),
            });
        }
    };

    let mut rows: Vec<(String, String, String, String, String)> = Vec::new();
    let mapped = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0).unwrap_or_default(),
            row.get::<_, String>(1).unwrap_or_default(),
            row.get::<_, String>(2).unwrap_or_default(),
            row.get::<_, String>(3).unwrap_or_default(),
            row.get::<_, String>(4).unwrap_or_default(),
        ))
    });
    let Ok(mapped) = mapped else {
        return json!({
            "available": false,
            "path": path_text,
            "reason": "读取 cc-switch 数据失败：数据库可能正被写入，稍后重试",
        });
    };
    for row in mapped.flatten() {
        rows.push(row);
    }

    let mut entries: Vec<Value> = Vec::new();
    for (id, app_type, name, settings_text, notes) in &rows {
        // name 缺失的行没有展示价值，跳过（与读取容错的既有取向一致）
        if name.trim().is_empty() {
            continue;
        }
        let settings: Value = serde_json::from_str(settings_text).unwrap_or(Value::Null);
        let settings = if settings.is_object() {
            settings
        } else {
            Value::Object(Map::new())
        };
        entries.push(scan_entry(id, app_type, name.trim(), notes.trim(), &settings, &local_names));
    }

    // 排序：可导入的在前（claude → codex → 名称），不支持 / 跳过的在后按
    // app_type 归堆 —— 用户要勾的东西先看到，灰显的原因集中可读。
    let rank = |entry: &Value| -> (usize, String, String) {
        let importable = entry.get("importable").and_then(Value::as_bool) == Some(true);
        (
            if importable { 0 } else { 1 },
            string_field(Some(entry), "appType"),
            string_field(Some(entry), "name").to_lowercase(),
        )
    };
    entries.sort_by(|a, b| rank(a).cmp(&rank(b)));

    json!({
        "available": true,
        "path": path_text,
        "total": entries.len(),
        "providers": entries,
    })
}
