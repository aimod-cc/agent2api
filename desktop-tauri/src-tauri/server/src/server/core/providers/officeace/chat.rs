//! OfficeAce 的 chat 请求构造：**无状态**，一次请求一次回答（OpenAI 协议）。
//!
//! ── 上游形状（两份独立实现一致，见 `cpa-deploy/notes/officeace-agent2api-plan.md`）──
//!   · URL：`{base}/chat/completions`，其中 `base` 是 `model_api_url_base` 规范化
//!     成 `https://<host>/v2` 之后的形态（`chat_endpoint` 负责补 `/v2`）。
//!   · 头：`Authorization: Basic base64(model_app_key:model_app_secret)`，
//!     外加桌面端那三个关联头（`Chat-Id` / `Session-Id` / `lang: en`）。
//!   · 体：标准 OpenAI chat；上游**恒流式**，所以要 `stream:true` +
//!     `stream_options.include_usage`；非流式的客户端请求由本仓聚合器拼回。
//!
//! ── 两个实测坑（都写在这里，别在别处重推）──────────────────────
//!   1. **推理预算**：上游把 `max_tokens` 当「推理 + 正文」的总预算，给小了会
//!      HTTP 200 但正文空串（`finish_reason=length`、`reasoning_tokens` 吃掉全部）。
//!      这里的做法是**追加**一个下限（默认 1024），不是 `max()` —— `max()` 会把
//!      「只想要 1 个 token」的意图改掉（见 `REASONING_ALLOWANCE`）。
//!      `max_tokens < 16` 原样透传：那种请求本来就要最小输出。
//!   2. **`thinking` 参数**：`glm-5.3` 系列不收 `thinking:{"type":"disabled"}`，
//!      上游回 400。这里不预先删（不知道哪些模型不收），交给上游报错后剥参重试 ——
//!      与 officeace2api 同策（它的 `unsupportedParam` 分支）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{Value, json};

use super::credentials::OfficeAceCredential;

/// 推理预算的追加量（默认值；上游把 `max_tokens` 当总预算，见模块头）。
pub const REASONING_ALLOWANCE: i64 = 1024;
/// 小于这个值的 `max_tokens` 原样透传（客户端要的就是最小输出）。
pub const TINY_MAX_TOKENS: i64 = 16;

/// 模型网关基址的规范形态：`https://<host>/v2`。
///
/// 输入可能是 `https://host`、`https://host/v2`、`https://host/v2/`，
/// 也可能是桌面端 `models.json` 里的 `api_base`（就带 `/v2`）。
pub fn normalize_base(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.ends_with("/v2") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v2")
    }
}

/// chat 端点：`{规范化基址}/chat/completions`。
pub fn chat_endpoint(base_url: &str) -> String {
    let base = normalize_base(base_url);
    if base.is_empty() {
        String::new()
    } else {
        format!("{base}/chat/completions")
    }
}

/// 把 `<app_key>:<app_secret>` 编成 Basic 头值（不引 base64 crate 之外的东西）。
pub fn basic_authorization(app_key: &str, app_secret: &str) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let raw = format!("{app_key}:{app_secret}");
    format!("Basic {}", STANDARD.encode(raw.as_bytes()))
}

/// 按推理预算规则改写 `max_tokens`（返回改后的值；`None` = 客户端没给）。
///
/// 规则：给了且 `>= TINY_MAX_TOKENS` 时**追加** [`REASONING_ALLOWANCE`]；
/// `< TINY_MAX_TOKENS` 或没给时原样不动。
pub fn apply_reasoning_allowance(body: &mut Value) -> Option<(i64, i64)> {
    let requested = body.get("max_tokens").and_then(Value::as_i64)?;
    if requested < TINY_MAX_TOKENS {
        return None;
    }
    let raised = requested + REASONING_ALLOWANCE;
    body["max_tokens"] = json!(raised);
    Some((requested, raised))
}

/// 流式补 `stream_options.include_usage`（**不覆盖**客户端已有的取值）。
pub fn ensure_include_usage(body: &mut Value) {
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let object = match body.as_object_mut() {
        Some(object) => object,
        None => return,
    };
    let options = object
        .entry("stream_options".to_string())
        .or_insert_with(|| json!({}));
    if let Some(options) = options.as_object_mut() {
        options
            .entry("include_usage".to_string())
            .or_insert(Value::Bool(true));
    }
}

/// 构造一次 chat 请求的计划（URL + 头 + 体）。
///
/// 上游**恒流式**（依据：两份实现都强制 `stream:true`，非流式由本地聚合），
/// 所以这里不按客户端的 `stream` 分流，由编排层决定聚合与否。
#[derive(Clone, Debug)]
pub struct ChatPlan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

/// 上游那三个关联头。`Chat-Id` / `Session-Id` 由我们生成（桌面端也是自己生成的，
/// 不是上游下发），`lang` 固定 `en`。
fn correlation_headers() -> Vec<(String, String)> {
    vec![
        ("Chat-Id".to_string(), random_hex(16)),
        ("Session-Id".to_string(), random_hex(16)),
        ("lang".to_string(), "en".to_string()),
    ]
}

/// 16 字节随机数的十六进制串（`Chat-Id` / `Session-Id` 的形态）。
///
/// 随机源用 `getrandom`（与 `access::random_hex`、`zcode::credentials` 同一处依赖，
/// 不引入新 crate）。取不到随机源时退化成时间戳（这两个头只是关联 id，
/// 不与任何上游状态绑定；报错让请求发不出去反而是更坏的结果）。
fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if getrandom::getrandom(&mut buffer).is_err() {
        let now = crate::server::logging::now_ms();
        for (index, slot) in buffer.iter_mut().enumerate() {
            *slot = ((now >> (index % 8 * 8)) & 0xff) as u8;
        }
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 由凭证与客户端体构造上游请求。
///
/// `client_body` 是**已经过入口协议翻译**的标准 chat 体（本仓编排层给的就是这个形态）。
pub fn build_chat_plan(
    credential: &OfficeAceCredential,
    client_body: &Value,
) -> Result<ChatPlan, String> {
    if !credential.can_forward() {
        return Err("OfficeAce 凭据不完整（要 baseUrl 与网关 Basic 那一对）".to_string());
    }
    let url = chat_endpoint(&credential.base_url);
    if url.is_empty() {
        return Err("OfficeAce 账号缺少模型网关基址".to_string());
    }
    let mut body = client_body.clone();
    // 上游恒流式：强制 true（非流式由编排层聚合）
    if let Some(object) = body.as_object_mut() {
        object.insert("stream".to_string(), Value::Bool(true));
    }
    apply_reasoning_allowance(&mut body);
    ensure_include_usage(&mut body);

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "*/*".to_string()),
        (
            "Authorization".to_string(),
            basic_authorization(&credential.model_app_key, &credential.model_app_secret),
        ),
    ];
    headers.extend(correlation_headers());
    Ok(ChatPlan { url, headers, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn credential() -> OfficeAceCredential {
        OfficeAceCredential {
            id: "a1".to_string(),
            base_url: "https://modelgw-0004.example.com".to_string(),
            model_app_key: "app-key".to_string(),
            model_app_secret: "app-secret".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn normalizes_the_gateway_base() {
        assert_eq!("https://h/v2", normalize_base("https://h"));
        assert_eq!("https://h/v2", normalize_base("https://h/"));
        assert_eq!("https://h/v2", normalize_base("https://h/v2"));
        assert_eq!("https://h/v2", normalize_base("https://h/v2/"));
        assert_eq!("", normalize_base("   "));
        assert_eq!("https://h/v2/chat/completions", chat_endpoint("https://h"));
    }

    #[test]
    fn basic_header_is_base64_of_key_colon_secret() {
        // "app-key:app-secret" 的 base64（手算并与实现对照）
        assert_eq!("Basic YXBwLWtleTphcHAtc2VjcmV0", basic_authorization("app-key", "app-secret"));
    }

    #[test]
    fn reasoning_allowance_is_appended_not_maxed() {
        // 64 → 1088（追加），不是 max(64, 1024)
        let mut body = json!({"max_tokens": 64});
        assert_eq!(Some((64, 1088)), apply_reasoning_allowance(&mut body));
        assert_eq!(1088, body["max_tokens"]);
        // 1 → 原样（小于 TINY_MAX_TOKENS 就透传，客户端要的就是最小输出）
        let mut tiny = json!({"max_tokens": 1});
        assert_eq!(None, apply_reasoning_allowance(&mut tiny));
        assert_eq!(1, tiny["max_tokens"]);
        // 15 也透传，16 才追加
        let mut fifteen = json!({"max_tokens": 15});
        assert_eq!(None, apply_reasoning_allowance(&mut fifteen));
        let mut sixteen = json!({"max_tokens": 16});
        assert_eq!(Some((16, 1040)), apply_reasoning_allowance(&mut sixteen));
        // 没给就什么都不做
        let mut absent = json!({});
        assert_eq!(None, apply_reasoning_allowance(&mut absent));
        assert!(absent.get("max_tokens").is_none());
    }

    #[test]
    fn usage_flag_respects_the_client_value() {
        let mut body = json!({"stream": true});
        ensure_include_usage(&mut body);
        assert_eq!(Value::Bool(true), body["stream_options"]["include_usage"]);
        // 客户端显式给 false 时不覆盖
        let mut explicit = json!({"stream": true, "stream_options": {"include_usage": false}});
        ensure_include_usage(&mut explicit);
        assert_eq!(Value::Bool(false), explicit["stream_options"]["include_usage"]);
        // 非流式不补
        let mut not_stream = json!({"stream": false});
        ensure_include_usage(&mut not_stream);
        assert!(not_stream.get("stream_options").is_none());
    }

    #[test]
    fn plan_forces_streaming_and_sets_the_three_headers() {
        let plan = build_chat_plan(&credential(), &json!({"model": "glm-5.3", "messages": []}))
            .expect("齐备凭据要能构造");
        assert_eq!("https://modelgw-0004.example.com/v2/chat/completions", plan.url);
        assert_eq!(Value::Bool(true), plan.body["stream"], "上游恒流式");
        let header = |name: &str| {
            plan.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(Some("Basic YXBwLWtleTphcHAtc2VjcmV0".to_string()), header("Authorization"));
        assert_eq!(Some("en".to_string()), header("lang"));
        assert_eq!(32, header("Chat-Id").unwrap_or_default().len(), "Chat-Id 是 16 字节 hex");
        assert_eq!(32, header("Session-Id").unwrap_or_default().len());
        // 两次构造的 id 不同（不是常量）
        let again = build_chat_plan(&credential(), &json!({"messages": []})).expect("再来一次");
        let chat_id = |plan: &ChatPlan| {
            plan.headers
                .iter()
                .find(|(key, _)| key == "Chat-Id")
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        assert_ne!(chat_id(&plan), chat_id(&again));
    }

    #[test]
    fn incomplete_credential_is_refused() {
        let broken = OfficeAceCredential {
            base_url: "https://h".to_string(),
            ..Default::default()
        };
        assert!(build_chat_plan(&broken, &json!({})).is_err());
    }
}
