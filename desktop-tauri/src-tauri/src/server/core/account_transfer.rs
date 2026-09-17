//! 账号导入 / 导出（对照 src/workbuddy-account-transfer.mjs）。
//!
//! 这里是**纯逻辑模块**：不碰磁盘、不持状态，读盘/落盘/号段分配全部由 store 提供
//! （`AccountStore` 的 load_locked / save_locked / with_lock）。
//!
//! 导出形态刻意保留 accessToken / refreshToken：换机器后靠它们直接沿用登录态，
//! 不必再走一遍登录流程。反之 rateLimits 是本机的运行时限额标记、下划线开头的是
//! 内部字段，换机器都没有意义，一律不导出。
//!
//! 导入目前只有 merge 一种模式：按 uid 匹配本机账号 —— 命中则更新凭证与运营字段，
//! 未命中则追加为队尾新账号。**priority 与 addedAt 一律取本机值**：本机的转发顺序
//! 由本机决定，回灌一份导出文件不该把别人的顺序搬过来。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::priority::next_free_priority;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::{MAX_ACCOUNTS, MAX_TOKEN_LENGTH};
use crate::server::core::endpoints::resolve_edition;

/// 导出文件格式版本（导入端据此兼容后续格式变化）
pub const EXPORT_VERSION: i64 = 1;

/// 单条账号记录 → 导出形态：原样输出全部业务字段，剔除 rateLimits（本机运行时
/// 限额标记）与下划线内部字段。
fn to_export_record(record: &StoredAccount) -> Value {
    let mut item = Map::new();
    for (key, value) in record.fields() {
        if key.starts_with('_') || key == "rateLimits" {
            continue;
        }
        // `value === undefined ? null : value`：JSON 里不存在 undefined，
        // 因此这里只需原样搬运（缺键就是缺键）
        item.insert(key.clone(), value.clone());
    }
    // 统一形状：token 与代理即使缺失也显式给出，方便导入端与用户核对
    let access_token = record.access_token();
    let refresh_token = record.refresh_token();
    item.insert("accessToken".to_string(), Value::String(access_token));
    item.insert("refreshToken".to_string(), Value::String(refresh_token));
    item.insert("proxy".to_string(), record.proxy());
    Value::Object(item)
}

/// 导出全部账号（换机器后导入继续用）
pub fn export_accounts(store: &AccountStore) -> Value {
    store.with_lock(|guard| {
        let state = store.load_locked(guard);
        json!({
            "version": EXPORT_VERSION,
            "exportedAt": crate::server::logging::now_ms(),
            "accounts": state.accounts.iter().map(to_export_record).collect::<Vec<_>>(),
        })
    })
}

/// 导入记录的字段归一（merge 的「更新」与「新增」分支共用）。
///
/// 只认导入文件里出现过的键：出现才覆盖，未出现或为空则沿用 `before`（新增分支传
/// 空对象）—— 导出文件回流即原样还原，只写了 token 的残缺文件也不会把备注名、
/// 代理洗掉。priority / addedAt 刻意不在其中：本机转发顺序由本机决定。
fn normalize_imported(item: &Map<String, Value>, before: Option<&StoredAccount>) -> Map<String, Value> {
    let has = |key: &str| item.contains_key(key);
    let text = |key: &str, fallback: String| -> String {
        match item.get(key) {
            None | Some(Value::Null) => fallback,
            Some(Value::String(value)) => value.trim().to_string(),
            Some(other) => other.to_string().trim().to_string(),
        }
    };
    // Node 的 `Number(item[key]) > 0 ? value : fallback`；已是 JSON 数字时原样保留
    // （整数形态不能丢，见 state::json_number）
    let num = |key: &str, fallback: Value| -> Value {
        match item.get(key) {
            Some(Value::Number(number)) => {
                let parsed = number.as_f64().unwrap_or(0.0);
                if parsed.is_finite() && parsed > 0.0 {
                    item.get(key).cloned().unwrap_or(fallback)
                } else {
                    fallback
                }
            }
            Some(Value::String(text)) => match text.trim().parse::<f64>() {
                Ok(value) if value.is_finite() && value > 0.0 => {
                    crate::server::core::account_store::state::json_number(value)
                }
                _ => fallback,
            },
            _ => fallback,
        }
    };
    let before_text = |getter: fn(&StoredAccount) -> String| -> String {
        before.map(getter).unwrap_or_default()
    };

    let edition = resolve_edition(
        item.get("edition")
            .filter(|value| !value.is_null())
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .or_else(|| before.and_then(StoredAccount::edition))
            .as_deref(),
    );
    // token 与备注名以「非空」为准：导入值为空时保留本机旧值，避免账号被洗成空壳
    let access_token = {
        let incoming = text("accessToken", String::new());
        if incoming.is_empty() {
            before_text(StoredAccount::access_token)
        } else {
            incoming
        }
    };
    let refresh_token = {
        let incoming = text("refreshToken", String::new());
        if incoming.is_empty() {
            before_text(StoredAccount::refresh_token)
        } else {
            incoming
        }
    };
    let token_tail_value = {
        let incoming = text("tokenTail", String::new());
        if incoming.is_empty() {
            crate::server::core::account_store::store_util::token_tail_of(&access_token)
        } else {
            incoming
        }
    };
    let name = {
        let incoming = crate::server::core::account_store::store_util::truncate_text(&text(
            "name",
            before_text(StoredAccount::name),
        ), 100);
        if incoming.is_empty() {
            before_text(StoredAccount::name)
        } else {
            incoming
        }
    };

    let mut record = Map::new();
    record.insert("accessToken".to_string(), Value::String(access_token));
    record.insert("refreshToken".to_string(), Value::String(refresh_token));
    record.insert("tokenTail".to_string(), Value::String(token_tail_value));
    record.insert("edition".to_string(), Value::String(edition.id.to_string()));
    record.insert(
        "prefixPath".to_string(),
        item.get("prefixPath")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| before.and_then(StoredAccount::prefix_path).map(Value::String))
            .unwrap_or_else(|| Value::String(edition.prefix_path.to_string())),
    );
    record.insert(
        "endpoint".to_string(),
        item.get("endpoint")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| before.and_then(StoredAccount::endpoint).map(Value::String))
            .unwrap_or_else(|| Value::String(edition.endpoint.to_string())),
    );
    record.insert(
        "platform".to_string(),
        item.get("platform")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| before.and_then(StoredAccount::platform).map(Value::String))
            .unwrap_or_else(|| Value::String(edition.platform.to_string())),
    );
    record.insert("name".to_string(), Value::String(name));
    record.insert(
        "nickname".to_string(),
        Value::String(text("nickname", before_text(StoredAccount::nickname))),
    );
    record.insert(
        "type".to_string(),
        Value::String({
            let value = text("type", before_text(StoredAccount::account_type));
            if value.is_empty() { "personal".to_string() } else { value }
        }),
    );
    record.insert(
        "enterpriseId".to_string(),
        Value::String(text("enterpriseId", before_text(StoredAccount::enterprise_id))),
    );
    record.insert(
        "enterpriseName".to_string(),
        Value::String(text(
            "enterpriseName",
            before_text(StoredAccount::enterprise_name),
        )),
    );
    record.insert(
        "domain".to_string(),
        Value::String(text("domain", before_text(StoredAccount::domain))),
    );
    record.insert(
        "expiresAt".to_string(),
        num(
            "expiresAt",
            before
                .and_then(StoredAccount::expires_at)
                .map(crate::server::core::account_store::state::json_number)
                .unwrap_or(Value::Null),
        ),
    );
    record.insert(
        "refreshExpiresAt".to_string(),
        num(
            "refreshExpiresAt",
            before
                .and_then(StoredAccount::refresh_expires_at)
                .map(crate::server::core::account_store::state::json_number)
                .unwrap_or(Value::Null),
        ),
    );
    record.insert(
        "enabled".to_string(),
        Value::Bool(if has("enabled") {
            !matches!(item.get("enabled"), Some(Value::Bool(false)))
        } else {
            before.map(StoredAccount::enabled).unwrap_or(true)
        }),
    );
    record.insert(
        "proxy".to_string(),
        if has("proxy") {
            crate::server::core::proxies::normalize_account_proxy(
                item.get("proxy").unwrap_or(&Value::Null),
            )
            .map(|value| value.unwrap_or(Value::Null))
            .unwrap_or(Value::Null)
        } else {
            before.map(StoredAccount::proxy).unwrap_or(Value::Null)
        },
    );
    record
}

/// 导入账号（导出文件的回流），merge 语义见文件头。
///
/// 单条坏数据（缺 id / 缺 uid / 两个 token 都没有）只记一条 failed 并继续，
/// 不让一条脏数据毁掉整批导入。
/// 返回 `{ total, added, updated, skipped, failed, errors }`。
pub fn import_accounts(store: &AccountStore, payload: &Value) -> Result<Value, AccountStoreError> {
    let Some(root) = payload.as_object() else {
        return Err(AccountStoreError::new("导入内容必须是 JSON 对象", 400));
    };
    let Some(items) = root.get("accounts").and_then(Value::as_array) else {
        return Err(AccountStoreError::new("缺少 accounts 数组", 400));
    };
    if items.is_empty() {
        return Err(AccountStoreError::new("accounts 为空，没有可导入的账号", 400));
    }

    store.with_lock(|guard| {
        let mut state = store.load_locked(guard);
        // 按 uid 匹配本机账号；导入中新增的账号也进表，这样同批多条相同 uid
        // 会变成「第一条新增、后续更新同一条」
        let mut taken_ids: Vec<String> = state
            .accounts
            .iter()
            .map(|item| item.id().to_string())
            .collect();

        let mut errors: Vec<Value> = Vec::new();
        let mut added = 0usize;
        let mut updated = 0usize;
        for item in items {
            let valid = item.is_object();
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let uid = item
                .get("uid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let outcome: Result<bool, String> = (|| {
                if !valid {
                    return Err("账号记录必须是 JSON 对象".to_string());
                }
                if item_id.is_empty() {
                    return Err("缺少 id".to_string());
                }
                if uid.is_empty() {
                    return Err("缺少 uid（无法标识账号）".to_string());
                }
                let access_token = item
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let refresh_token = item
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if access_token.is_empty() && refresh_token.is_empty() {
                    return Err("缺少 accessToken 与 refreshToken".to_string());
                }
                if access_token.chars().count() > MAX_TOKEN_LENGTH
                    || refresh_token.chars().count() > MAX_TOKEN_LENGTH
                {
                    return Err("token 过长".to_string());
                }

                let item_object = item.as_object().cloned().unwrap_or_default();
                if let Some(index) = state.accounts.iter().position(|record| record.uid() == uid) {
                    // 本机的 priority / addedAt / id 一律保留，只回写凭证与运营字段
                    let before = state.accounts[index].clone();
                    let normalized = normalize_imported(&item_object, Some(&before));
                    let target = state.accounts[index].fields_mut();
                    for (key, value) in normalized {
                        target.insert(key, value);
                    }
                    state.accounts[index]
                        .set("updatedAt", Value::from(crate::server::logging::now_ms()));
                    return Ok(true);
                }
                if state.accounts.len() >= MAX_ACCOUNTS {
                    return Err(format!("最多保存 {MAX_ACCOUNTS} 个账号"));
                }
                let now = crate::server::logging::now_ms();
                let normalized = normalize_imported(&item_object, None);
                let mut record = Map::new();
                // id 沿用导入值，撞了别人的 id 就按 addAccount 的约定回退成 user-<uid>
                let id = if !item_id.is_empty() && !taken_ids.contains(&item_id) {
                    item_id.clone()
                } else {
                    format!("user-{uid}")
                };
                record.insert("id".to_string(), Value::String(id.clone()));
                for (key, value) in normalized {
                    record.insert(key, value);
                }
                record.insert("uid".to_string(), Value::String(uid.clone()));
                // priority 一律取队尾（忽略导入文件里的值），不打乱本机转发顺序
                record.insert(
                    "priority".to_string(),
                    Value::from(next_free_priority(
                        &state.accounts.iter().map(StoredAccount::priority).collect::<Vec<_>>(),
                    )),
                );
                record.insert("addedAt".to_string(), Value::from(now));
                record.insert("updatedAt".to_string(), Value::from(now));
                let mut saved = StoredAccount::from_map(record);
                if saved.name().is_empty() {
                    let fallback = {
                        let nickname = saved.nickname();
                        if nickname.is_empty() {
                            format!("账号 {}", crate::server::core::account_store::store_util::truncate_text(&uid, 8))
                        } else {
                            nickname
                        }
                    };
                    saved.set("name", Value::String(fallback));
                }
                taken_ids.push(id);
                state.accounts.push(saved);
                Ok(false)
            })();

            match outcome {
                Ok(true) => updated += 1,
                Ok(false) => added += 1,
                Err(message) => {
                    let id = if item_id.is_empty() { uid.clone() } else { item_id.clone() };
                    errors.push(json!({ "id": id, "message": message }));
                }
            }
        }

        store.save_locked(&state, guard)?;
        crate::server::logging::log(
            "[Accounts]",
            &format!(
                "📥 账号导入完成: 共 {} 条，新增 {added} 个，更新 {updated} 个{}",
                items.len(),
                if errors.is_empty() {
                    String::new()
                } else {
                    format!("，跳过 {} 条（数据不完整）", errors.len())
                }
            ),
        );
        Ok(json!({
            "total": items.len(),
            "added": added,
            "updated": updated,
            "skipped": 0,
            "failed": errors.len(),
            "errors": errors,
        }))
    })
}
