//! CatPaw 协议层错误与上游 `modelType` 映射表。
//!
//! ── 为什么错误类型单独定义（不直接用 GatewayError）───────────
//! 上游失败时除了文案还要回报 `event(status, failReason, failCode, unifyCode)`
//! （UPSTREAM_PROTOCOL §3.3），其中 failCode / unifyCode 来自上游错误体的
//! `code` / `unifyCode`（原实现 `error.details`）。`GatewayError` 只有一个
//! `upstream_code`，装不下这两个码，而上游侧的排障恰恰靠它们。
//! 因此协议层用自己的 [`CatPawError`]，在返回给客户端前才降级成 `GatewayError`
//! （`to_gateway`）。
//!
//! ── 模型表为什么是静态表而不是配置（以及它现在的分工）─────────
//! 数字 `modelType` 是**上游的内部 ID**，写错一个不会报错，只会让请求打到
//! 另一个模型 —— 所以这里刻意保留「证据」注释，改动前必须回去核对源文件。
//!
//! 这张表**不再是模型清单的唯一来源**：上游有远程目录
//! （`POST /api/agent/maas/model-types`，见 [`catalog`]），「有哪些模型、
//! 倍率多少」由它负责。静态表留在原地的理由是它能提供**远程条目给不出的
//! 两样东西**：
//!   1. `host_model_id` —— 实测坐实的上游数字 ID（远程条目也带 `modelTypeId`，
//!      但只有表里这两个是我们逐条验证过能力与档位的）；
//!   2. `context_windows` / `default_context_window` —— `context` 参数的
//!      合法档位（远程把这些藏在 `parameterDefinitions` 的 ENUM 里）。
//! [`resolve_model_request`] 因此是「静态表 → 远程目录 → 纯数字」三档判定。
//!
//! [`catalog`]: super::catalog

use serde_json::Value;

use crate::server::errors::GatewayError;

/// 协议层错误（客户端可见文案 + 上游业务码）
#[derive(Clone, Debug)]
pub struct CatPawError {
    /// 客户端可见的 HTTP 状态码（上游 4xx 原样，其余 502 —— 对照原实现
    /// `status >= 400 && status < 500 ? status : 502`）
    pub status: i32,
    /// 客户端可见文案
    pub message: String,
    /// 上游业务码（`failCode`）
    pub code: Option<i64>,
    /// 上游统一错误码（`unifyCode`）
    pub unify_code: Option<i64>,
}

impl CatPawError {
    /// 入站参数错误（400，文案照抄原实现）
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self { status: 400, message: message.into(), code: None, unify_code: None }
    }

    /// 上游/传输错误（502）
    pub fn upstream(message: impl Into<String>) -> Self {
        Self { status: 502, message: message.into(), code: None, unify_code: None }
    }

    /// 上游 HTTP 错误：状态码按原实现的归一规则映射
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        let mapped = if (400..500).contains(&status) { i32::from(status) } else { 502 };
        Self { status: mapped, message: message.into(), code: None, unify_code: None }
    }

    /// 挂上游业务码（`failCode` / `unifyCode`；`unwrap_api_data` 之外的上游错误
    /// 若将来能解析出码，用这个补上）
    ///
    /// `#[allow(dead_code)]`：W5-T-d4 摘掉模块级抑制后编译器报出「无人调用」——
    /// 当前唯一会给错误挂码的地方（`openai::unwrap_api_data`）是直接构造
    /// `CatPawError` 字面量写的，用不到这个 builder。保留它是因为「错误带码」
    /// 是上游协议的一部分（`failCode` / `unifyCode` 进终态上报，UPSTREAM_PROTOCOL
    /// §3.3），构造路径不该只有一处字面量。摘除条件：真有第二个构造点，
    /// 或明确决定只保留字面量写法（那时删掉本方法与上面两行注释）。
    #[allow(dead_code)]
    pub fn with_codes(mut self, code: Option<i64>, unify_code: Option<i64>) -> Self {
        self.code = code;
        self.unify_code = unify_code;
        self
    }

    /// 转成网关错误（转发编排只认这个类型）
    pub fn to_gateway(&self) -> GatewayError {
        GatewayError::with_status(self.status, self.message.clone())
            .with_optional_code(self.code)
    }
}

/// 归一化层（纯函数模块）报的错误：保留状态码与文案，丢弃网关错误码字符串。
///
/// 为什么可以丢 `code`：那是网关自定义的字符串码（目前只有模型不存在用
/// `model_not_found`），协议层不消费它；`upstream_code` 则**保留** ——
/// 归一化层不会产生它，但保留映射能让将来任何一层挂上的上游码不丢。
impl From<GatewayError> for CatPawError {
    fn from(error: GatewayError) -> Self {
        Self {
            status: error.status_code,
            message: error.message,
            code: None,
            unify_code: error.upstream_code,
        }
    }
}

// ─── 模型映射表（proxy-chat-utils.mjs 的 MODELS）────────────────

/// 一个 CatPaw 模型的静态事实。
///
/// ── 只收录网关路径真正消费的字段 ─────────────────────────────
/// 原表还有 `reasoningEffort` / `thinkingType` / `hostAdvertisedThinking` /
/// `verified` 四项，它们在 CLI 的 Host 模式下参与能力判定；OpenAI 兼容路径
/// （本模块与适配器的清单输出）只用得到下面这些。刻意不搬那四项：
/// 搬了也没人读，只会给「这张表是不是唯一事实来源」制造歧义。
///
/// `support_image` / `support_thinking` 在 W5-T-d4 补上：聚合目录的
/// `list_item` 要把它们映成 `supports_images` / `supports_reasoning`
/// （见 `adapter::CatPawAdapter::list_models`），不搬就没法如实声明能力 ——
/// 那时两个布尔在**每一个**模型上都恒为 true（原表如此），所以能力位的
/// 事实来源仍然只有这一张表。
#[derive(Clone, Copy, Debug)]
pub struct ModelSpec {
    /// 对外模型名（OpenAI 的 `model` 字段；也是聚合目录里的 id）
    pub id: &'static str,
    /// 展示名（原表 `name`，容忍 `GLM-5.3-Flash` 这类大小写/分隔符差异）
    pub name: &'static str,
    /// 上游 `modelType` 数字 ID
    pub host_model_id: i64,
    /// 是否支持图片输入（原表 `supportImage`；进清单的 `supports_images`）
    pub support_image: bool,
    /// 是否支持思考（原表 `supportThinking`；进清单的 `supports_reasoning`）
    pub support_thinking: bool,
    /// 该模型支持的 `context` 档位（空 = 不支持 context 参数）
    pub context_windows: &'static [&'static str],
    /// 未显式请求 context 时的默认档位（None = 不发送 context 字段）
    pub default_context_window: Option<&'static str>,
}

/// 模型注册表。
///
/// ── 每个数字 ID 的证据（原表头注释逐条对照，**不要凭名字猜**）──
///   - `83 = Kimi-K3`：长连接日志里 `model:83` 会话自述 Moonshot/Kimi，
///     且真实 Host 图片请求成功；
///   - `91 = GLM-5.3-Flash`：`model:91` 会话自述 Z.ai GLM、真实 Host 图片请求
///     成功，且桌面端持久化选择为
///     `{"modelId":91,"modelParams":{"context":"1024000","effort":"max"}}`。
///
/// 两个 ID 都经过 `agent_start`、图片与 `reasoningContent` 实测，不依据名称猜测。
/// `kimi-k3` 在原表里**没有** `contextWindows` 字段 ⇒ 不支持 context 参数
/// （传了就报 400），`glm-5.3-flash` 才有三档。
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "kimi-k3",
        name: "Kimi-K3",
        host_model_id: 83,
        support_image: true,
        support_thinking: true,
        context_windows: &[],
        default_context_window: None,
    },
    ModelSpec {
        id: "glm-5.3-flash",
        name: "GLM-5.3-Flash",
        host_model_id: 91,
        support_image: true,
        support_thinking: true,
        context_windows: &["204800", "512000", "1024000"],
        default_context_window: Some("1024000"),
    },
];

/// 模型名归一化（原实现 `normalizeModelName`）：小写 + 空白/下划线/点 → 连字符。
///
/// 用途是容忍客户端写 `GLM 5.3 Flash` / `glm_5.3_flash` 这类变体。
fn normalize_model_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_whitespace() || ch == '_' || ch == '.' {
            out.push('-');
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

/// 按名字或数字找模型（原实现 `findModelEntry`）
pub fn find_model_entry(model: &Value) -> Option<&'static ModelSpec> {
    match model {
        Value::Number(number) => {
            let id = number.as_i64()?;
            MODELS.iter().find(|entry| entry.host_model_id == id)
        }
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(id) = trimmed.parse::<i64>() {
                return MODELS.iter().find(|entry| entry.host_model_id == id);
            }
            let normalized = normalize_model_name(trimmed);
            MODELS.iter().find(|entry| {
                normalize_model_name(entry.id) == normalized
                    || normalize_model_name(entry.name) == normalized
            })
        }
        _ => None,
    }
}

/// 一次请求最终使用的上游模型（对应原实现的 `{ modelId, displayName, entry }`）
#[derive(Clone, Debug)]
pub struct ModelResolution {
    /// 上游 `modelType`（数字）
    pub model_type: i64,
    /// 客户端可见的模型名（原实现取 `entry.id`，表外数字 ID 则用请求原值）
    pub display_name: String,
    /// 命中的注册表条目（未命中为 None）
    pub entry: Option<&'static ModelSpec>,
}

/// 把客户端请求的 `model` 解析成上游数字 ID（原实现 `resolveModelRequest`
/// 在本网关语义下的裁剪版）。
///
/// ── 与原实现的两处裁剪（都是「网关侧不存在那个概念」）──────────
///   1. 没有 `--agent-model` 覆盖与 `requestedConfig.model` 回落：那两条来自
///      CLI 的启动参数，网关进程里没有；
///   2. 表外名字**直接报 400**而不是回落成「不发 model 字段」：网关必须先知道
///      数字 ID 才能建 conversation；回落成「不带数字 ID」时上游会用它自己的
///      默认模型 —— 客户端要的模型与实际跑的不是一个，属于静默错答。
///      客户端模型名的合法性在更早的入口（聚合目录）已校验过一次，走到这里
///      还失配说明目录与猫爪表不同步，报错比猜测好。
///
/// ── 三档判定顺序（远程目录接上后新增第 2 档）────────────────
///   1. **静态表**命中 → 用它实测过的数字 ID 与 context 档位（最可靠：
///      档位校验需要 `ModelSpec`，远程条目给不出）；
///   2. **远程目录**命中 → 用它自带的 `modelType`。没有这一档就会出现
///      「`/v1/models` 里列着、请求却报 400」的自相矛盾 —— 远程目录会广告
///      静态表里没有的模型；
///   3. 纯数字 → 原样当上游 ID（`entry` 为 None，context 校验会据此拒绝）。
pub fn resolve_model_request(model: &Value) -> Result<ModelResolution, CatPawError> {
    let requested = match model {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_string(),
        _ => String::new(),
    };
    if let Some(entry) = find_model_entry(model) {
        return Ok(ModelResolution {
            model_type: entry.host_model_id,
            display_name: entry.id.to_string(),
            entry: Some(entry),
        });
    }
    // 远程目录里的模型：数字 ID 用它自带的（entry 仍为 None —— 档位信息
    // 在 `ModelSpec` 里，远程条目没有同形态的数据）。
    // `display_name` 取**目录里的规范 id**：客户端可能传的是展示名，
    // 而回写与日志该用规范名（与静态表分支 `entry.id` 的取向一致）。
    if !requested.is_empty() {
        if let Some(item) = super::catalog::find_remote(&requested) {
            if let Some(model_type) = item.get("modelType").and_then(Value::as_i64) {
                let canonical = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(&requested)
                    .to_string();
                return Ok(ModelResolution {
                    model_type,
                    display_name: canonical,
                    entry: None,
                });
            }
        }
    }
    if !requested.is_empty() && requested.chars().all(|ch| ch.is_ascii_digit()) {
        if let Ok(model_type) = requested.parse::<i64>() {
            // 数字 ID 直接可用（上游按数字识别模型），只是我们不知道它的档位与
            // 能力 —— entry 为 None，context 参数校验会据此拒绝
            return Ok(ModelResolution {
                model_type,
                display_name: requested,
                entry: None,
            });
        }
    }
    let names = super::catalog::known_ids().join("、");
    Err(CatPawError::bad_request(format!(
        "CatPaw 上游不支持模型 {}（可用: {names}）",
        if requested.is_empty() { "(未指定)" } else { &requested },
    )))
}

/// `reasoning_effort` → `declarativeParams.effort`（原实现 `resolveEffort`）。
///
/// 取值只允许 low / high / max（上游枚举），其余报 400 —— 静默忽略会让
/// 「客户端以为调了思考等级、实际没生效」无从发现。
pub fn resolve_effort(body: &Value) -> Result<Option<String>, CatPawError> {
    // `body.reasoning_effort ?? body.reasoningEffort ?? body.effort`：**空值合并**
    // （空串也会命中并报错，不会被跳过）
    let raw = body
        .get("reasoning_effort")
        .filter(|value| !value.is_null())
        .or_else(|| body.get("reasoningEffort").filter(|value| !value.is_null()))
        .or_else(|| body.get("effort").filter(|value| !value.is_null()));
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = value_text(raw).trim().to_ascii_lowercase();
    if !matches!(value.as_str(), "low" | "high" | "max") {
        return Err(CatPawError::bad_request("reasoning_effort 仅支持 low / high / max"));
    }
    Ok(Some(value))
}

/// 上下文档位别名（原实现 `CONTEXT_WINDOW_ALIASES`）
const CONTEXT_WINDOW_ALIASES: &[(&str, &str)] = &[
    ("200k", "204800"),
    ("204800", "204800"),
    ("500k", "512000"),
    ("512000", "512000"),
    ("1m", "1024000"),
    ("1024k", "1024000"),
    ("1024000", "1024000"),
];

/// 解析 `context` 档位（原实现 `resolveContextWindow`）。
///
/// 顺序照抄原实现：**先取请求里的显式值**（六个候选字段里第一个非空），
/// 没有才用模型的默认档位；两者都没有就返回 None（不发 context 字段）。
/// 显式值必须在别名表里、且该模型支持这个档位，否则 400。
pub fn resolve_context_window(
    body: &Value,
    resolution: &ModelResolution,
) -> Result<Option<String>, CatPawError> {
    let candidates = [
        body.get("context_window"),
        body.get("contextWindow"),
        body.get("context_length"),
        body.get("contextLength"),
        body.pointer("/model_params/context"),
        body.pointer("/modelParams/context"),
        body.pointer("/request_context/modelParams/declarativeParams/context"),
        body.pointer("/requestContext/modelParams/declarativeParams/context"),
    ];
    let requested = candidates
        .into_iter()
        .flatten()
        .find(|value| !value.is_null() && !value_text(value).trim().is_empty());
    let raw = match requested {
        Some(value) => value_text(value),
        None => match resolution.entry.and_then(|entry| entry.default_context_window) {
            Some(default) => default.to_string(),
            None => return Ok(None),
        },
    };
    let text = raw.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Ok(None);
    }
    let Some((_, normalized)) = CONTEXT_WINDOW_ALIASES.iter().find(|(alias, _)| *alias == text)
    else {
        return Err(CatPawError::bad_request("context_window 仅支持 200K / 500K / 1M"));
    };
    let supported = resolution.entry.map(|entry| entry.context_windows).unwrap_or(&[]);
    if supported.is_empty() {
        return Err(CatPawError::bad_request(format!(
            "模型 {} 不支持 context_window 参数",
            resolution.display_name
        )));
    }
    if !supported.contains(normalized) {
        return Err(CatPawError::bad_request(format!(
            "模型 {} 不支持请求的上下文长度",
            resolution.display_name
        )));
    }
    Ok(Some(normalized.to_string()))
}

/// 值 → 字符串（原实现的 `String(value)`：数字给十进制、布尔给字面量）
pub(super) fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
