//! 导入记录的字段归一（merge 的「更新」与「新增」共用）。
//!
//! ── 三条口径 ─────────────────────────────────────────────────
//!   1. **未知字段双向保留**：更新时以本机记录为底、导入文件里出现过的业务字段
//!      覆盖其上；新增时保留导入文件里除内部字段外的全部业务字段。下划线开头的
//!      内部字段与 `rateLimits`（本机运行时限额标记）一律不参与导入，也不被覆盖。
//!   2. **本机运营字段不可被导入改写**：id / priority / addedAt / updatedAt /
//!      desktop 由调用方决定，这里不接收导入值。
//!   3. **腾讯端点字段只属于 WorkBuddy**：edition / prefixPath / endpoint /
//!      platform 是 WorkBuddy 的已知字段，只在 WorkBuddy 分支按 edition 归一；
//!      其余三家一律不写入（连导出文件里的残留值也丢掉）—— 三家适配器都不读
//!      这些键，把腾讯端点塞进别家记录只会让排障时看到假的归属。

use serde_json::{Map, Value};

use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store_util::{token_tail_of, truncate_text};
use crate::server::core::account_store::MAX_TOKEN_LENGTH;
use crate::server::core::endpoints::resolve_edition;
use crate::server::core::providers::{kind_id, ProviderKind};

/// 导入值不允许覆盖的键（本机运营字段；语义见模块头）。
const LOCAL_OWNED_KEYS: [&str; 6] = [
    "id",
    "priority",
    "addedAt",
    "updatedAt",
    "rateLimits",
    "desktop",
];

/// 备注名长度上限（与各家添加路径一致）
const MAX_NAME_LENGTH: usize = 100;

/// 值 → 文本（JS `String(x)` 的宽松口径：数字/布尔按字面量，null/缺失给空串）
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}

/// `Number(x) > 0` 的数字形态（已是 JSON 数字时原样保留整数形态，见 `state::json_number`）
fn positive_number_of(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::Number(number)) => {
            let parsed = number.as_f64().unwrap_or(0.0);
            (parsed.is_finite() && parsed > 0.0).then(|| value.cloned().unwrap_or(Value::Null))
        }
        Some(Value::String(text)) => match text.trim().parse::<f64>() {
            Ok(parsed) if parsed.is_finite() && parsed > 0.0 => {
                Some(crate::server::core::account_store::state::json_number(parsed))
            }
            _ => None,
        },
        _ => None,
    }
}

/// 该键是否参与导入（内部字段与运行时标记不参与）
fn importable_key(key: &str) -> bool {
    !key.starts_with('_') && !LOCAL_OWNED_KEYS.iter().any(|owned| *owned == key)
}

/// 归一一条导入记录。
///
/// `provider` 已由调用方按注册表校验过；`before` 为本机命中记录（新增传 None）。
/// 返回的字段表**不含** id / priority / addedAt / updatedAt / rateLimits ——
/// 那些由调用方按本机策略补齐。代理非法时返回 Err（该条失败，不清成直连）。
pub(super) fn normalize_imported(
    item: &Map<String, Value>,
    provider: &str,
    before: Option<&StoredAccount>,
) -> Result<Map<String, Value>, String> {
    let mut record = Map::new();

    // 更新：以本机记录为底（内部字段、运行时限额与本机运营字段除外），
    // 保住本机未知字段
    if let Some(before) = before {
        for (key, value) in before.fields() {
            if key.starts_with('_') || key == "rateLimits" || !importable_key(key) {
                continue;
            }
            record.insert(key.clone(), value.clone());
        }
    }
    // 导入文件里出现过的业务字段覆盖其上（null / 空串视为未提供，不洗掉本机值）
    for (key, value) in item {
        if !importable_key(key) || value.is_null() {
            continue;
        }
        if matches!(value, Value::String(text) if text.trim().is_empty()) {
            continue;
        }
        record.insert(key.clone(), value.clone());
    }

    // provider 先落位（后续 WorkBuddy 分支要按它判定；也保证重建记录不丢归属）
    record.insert("provider".to_string(), Value::String(provider.to_string()));

    // ── 凭证：非空才覆盖（导入值为空时保留本机旧值，避免账号被洗成空壳）──
    let access_token = {
        let incoming = text_of(item.get("accessToken"));
        if incoming.is_empty() {
            before.map(StoredAccount::access_token).unwrap_or_default()
        } else {
            incoming
        }
    };
    let refresh_token = {
        let incoming = text_of(item.get("refreshToken"));
        if incoming.is_empty() {
            before.map(StoredAccount::refresh_token).unwrap_or_default()
        } else {
            incoming
        }
    };
    if !access_token.is_empty() {
        record.insert("accessToken".to_string(), Value::String(access_token.clone()));
    }
    if !refresh_token.is_empty() {
        record.insert("refreshToken".to_string(), Value::String(refresh_token));
    }
    let token_tail = {
        let incoming = text_of(item.get("tokenTail"));
        if !incoming.is_empty() {
            incoming
        } else if access_token.is_empty() {
            String::new()
        } else {
            token_tail_of(&access_token)
        }
    };
    if !token_tail.is_empty() {
        record.insert("tokenTail".to_string(), Value::String(token_tail));
    }

    // ── 备注名：非空才覆盖，长度与各家添加路径一致 ──
    let name = {
        let incoming = truncate_text(&text_of(item.get("name")), MAX_NAME_LENGTH);
        if incoming.is_empty() {
            before.map(StoredAccount::name).unwrap_or_default()
        } else {
            incoming
        }
    };
    if !name.is_empty() {
        record.insert("name".to_string(), Value::String(name));
    }

    // ── 启用状态：出现才覆盖（缺省视为启用，与各家添加路径同）──
    if let Some(value) = item.get("enabled") {
        record.insert(
            "enabled".to_string(),
            Value::Bool(!matches!(value, Value::Bool(false))),
        );
    } else if before.is_none() {
        record.insert("enabled".to_string(), Value::Bool(true));
    }

    // ── 出网代理：出现才覆盖；非法输入该条失败，不静默清成直连 ──
    if item.contains_key("proxy") {
        let normalized = crate::server::core::proxies::normalize_account_proxy(
            item.get("proxy").unwrap_or(&Value::Null),
        )
        .map_err(|error| error.message)?
        .unwrap_or(Value::Null);
        record.insert("proxy".to_string(), normalized);
    } else if before.is_none() {
        record.insert("proxy".to_string(), Value::Null);
    }

    if provider == kind_id(ProviderKind::WorkBuddy) {
        normalize_workbuddy_known_fields(&mut record, item, before);
    } else {
        // 非 WorkBuddy：edition / prefixPath / endpoint / platform 是 WorkBuddy 的
        // 已知字段（腾讯端点身份），不属于其余各家的 schema。一律不写入 ——
        // 既不给新记录注入腾讯端点，也不让导出文件里的残留值覆盖别家记录。
        for key in ["edition", "prefixPath", "endpoint", "platform"] {
            record.remove(key);
        }
    }

    // 自定义提供商账号：apiKey / baseUrl / tokenTail 是本家 schema，
    // 通用保留兜不住校验（超长、非法 URL），走专属归一
    if provider.starts_with(crate::server::core::custom_providers::ID_PREFIX) {
        normalize_custom_known_fields(&mut record, item, before)?;
    }

    // provider 已在上面落位：这里不再重复写入
    Ok(record)
}

/// 自定义提供商账号的专属字段：apiKey（凭证）、baseUrl（账号级覆盖项）、
/// tokenTail（界面尾号）。
///
/// 三者都是 `custom_accounts::add_custom_account` 落盘的形状，导入沿用同一套
/// 规则：apiKey trim 后落盘（超长该条失败）、导入值为空时保留本机旧凭证
/// （与 accessToken / refreshToken 的合并纪律一致）；baseUrl 带键才动 ——
/// 空值清除覆盖项（回落提供商默认基址），非空值过 `normalize_base_url` 门禁；
/// tokenTail 显式值优先，否则按最终 apiKey 重新派生（尾号必须与凭证同源，
/// 用旧尾号配新 key 会让界面展示对不上号）。
fn normalize_custom_known_fields(
    record: &mut Map<String, Value>,
    item: &Map<String, Value>,
    before: Option<&StoredAccount>,
) -> Result<(), String> {
    let api_key = {
        let incoming = text_of(item.get("apiKey"));
        if incoming.is_empty() {
            before
                .and_then(|record| record.get("apiKey"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        } else {
            incoming
        }
    };
    if api_key.chars().count() > MAX_TOKEN_LENGTH {
        return Err("apiKey 过长".to_string());
    }
    record.insert("apiKey".to_string(), Value::String(api_key.clone()));

    let token_tail = {
        let incoming = text_of(item.get("tokenTail"));
        if !incoming.is_empty() {
            incoming
        } else if api_key.is_empty() {
            String::new()
        } else {
            token_tail_of(&api_key)
        }
    };
    if token_tail.is_empty() {
        record.remove("tokenTail");
    } else {
        record.insert("tokenTail".to_string(), Value::String(token_tail));
    }

    if item.contains_key("baseUrl") {
        let raw = item
            .get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if raw.is_empty() {
            // 带键但为空 = 清除覆盖项，回落提供商的 baseUrl（与添加路径同语义：
            // 「没有覆盖项」与「覆盖成空串」必须可区分，空串不是合法 URL 落不了盘）
            record.remove("baseUrl");
        } else {
            let base_url = crate::server::core::custom_providers::normalize_base_url(&raw)?;
            record.insert("baseUrl".to_string(), Value::String(base_url));
        }
    }
    Ok(())
}

/// WorkBuddy 的已知字段：端点/版本三件套按 edition 归一，运营字段沿用旧口径。
///
/// 非 WorkBuddy 记录**不会**走到这里（见模块头第 3 条）。
fn normalize_workbuddy_known_fields(
    record: &mut Map<String, Value>,
    item: &Map<String, Value>,
    before: Option<&StoredAccount>,
) {
    let edition = resolve_edition(
        item.get("edition")
            .filter(|value| !value.is_null())
            .map(|value| text_of(Some(value)))
            .filter(|value| !value.is_empty())
            .or_else(|| before.and_then(StoredAccount::edition))
            .as_deref(),
    );
    let explicit = |key: &str| item.get(key).filter(|value| !value.is_null());
    record.insert("edition".to_string(), Value::String(edition.id.to_string()));
    record.insert(
        "prefixPath".to_string(),
        Value::String(
            explicit("prefixPath")
                .map(|value| text_of(Some(value)))
                .or_else(|| before.and_then(StoredAccount::prefix_path))
                .unwrap_or_else(|| edition.prefix_path.to_string()),
        ),
    );
    record.insert(
        "endpoint".to_string(),
        Value::String(
            explicit("endpoint")
                .map(|value| text_of(Some(value)))
                .or_else(|| before.and_then(StoredAccount::endpoint))
                .unwrap_or_else(|| edition.endpoint.to_string()),
        ),
    );
    record.insert(
        "platform".to_string(),
        Value::String(
            explicit("platform")
                .map(|value| text_of(Some(value)))
                .or_else(|| before.and_then(StoredAccount::platform))
                .unwrap_or_else(|| edition.platform.to_string()),
        ),
    );

    let before_text = |getter: fn(&StoredAccount) -> String| -> String {
        before.map(getter).unwrap_or_default()
    };
    for (key, fallback) in [
        ("nickname", before_text(StoredAccount::nickname)),
        ("enterpriseId", before_text(StoredAccount::enterprise_id)),
        ("enterpriseName", before_text(StoredAccount::enterprise_name)),
        ("domain", before_text(StoredAccount::domain)),
    ] {
        let incoming = text_of(item.get(key));
        record.insert(
            key.to_string(),
            Value::String(if incoming.is_empty() { fallback } else { incoming }),
        );
    }
    let account_type = {
        let incoming = text_of(item.get("type"));
        if incoming.is_empty() {
            let fallback = before_text(StoredAccount::account_type);
            if fallback.is_empty() { "personal".to_string() } else { fallback }
        } else {
            incoming
        }
    };
    record.insert("type".to_string(), Value::String(account_type));
    for (key, getter) in [
        ("expiresAt", StoredAccount::expires_at as fn(&StoredAccount) -> Option<f64>),
        ("refreshExpiresAt", StoredAccount::refresh_expires_at),
    ] {
        let fallback = before
            .and_then(getter)
            .map(crate::server::core::account_store::state::json_number)
            .unwrap_or(Value::Null);
        let value = positive_number_of(item.get(key)).unwrap_or(fallback);
        record.insert(key.to_string(), value);
    }
}
