//! 模型的管理视图和对外视图。所有对外出口使用相同的开启绑定集合。

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::key_scope::{self, KeyScope};
use crate::server::core::model_rules;
use crate::server::core::models::{list_item, list_response_from, model_id, suggest_from};
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind};

use super::routing::{builtin_target, entry_id_in_manifest};
use super::{active_manifests, aggregate_source, manifest_for, refresh_meta};

/// 先在各提供商内判断绑定，再按对外名去重；不能先合并上游 ID 再拼别名，
/// 否则某家关闭的映射会借另一家的同名上游重新出现在列表中。
fn public_builtin_items(active: &[(ProviderKind, Vec<Value>)]) -> Vec<(String, Value)> {
    let rules = model_rules::current();
    let mut claimed = HashSet::new();
    let mut result = Vec::new();
    for (kind, manifest) in active {
        let provider = kind_id(*kind);
        let mut names: Vec<String> = manifest.iter().map(model_id).collect();
        names.extend(rules.mappings.iter()
            .filter(|mapping| mapping.provider.as_deref().map_or(true, |owner| owner == provider))
            .map(|mapping| mapping.alias.clone()));
        for name in names {
            let Some(wire) = builtin_target(&rules, *kind, manifest, &name) else { continue };
            if !claimed.insert(name.to_lowercase()) {
                continue;
            }
            let Some(upstream) = manifest.iter().find(|item| model_id(item) == wire.model) else { continue };
            let mut item = list_item(upstream, provider);
            if let Some(object) = item.as_object_mut() {
                object.insert("id".to_string(), Value::String(name.clone()));
                if !name.eq_ignore_ascii_case(&wire.model) {
                    object.insert("name".to_string(), Value::String(name));
                    object.insert("is_default".to_string(), Value::Bool(false));
                }
            }
            result.push((provider.to_string(), item));
        }
    }
    result
}

pub fn models_response(store: &AccountStore, scope: Option<&KeyScope>) -> Value {
    let active: Vec<_> = active_manifests(store).into_iter()
        .filter(|(kind, _)| key_scope::allows_provider(scope, kind_id(*kind)))
        .collect();
    let (source, refreshed_at) = aggregate_source(&active);
    let mut data: Vec<_> = public_builtin_items(&active).into_iter()
        .map(|(_, item)| item)
        .filter(|item| key_scope::allows_model(scope, &model_id(item)))
        .collect();
    let builtin_count = data.len();
    super::super::custom::append_models_response(store, scope, &mut data);
    // 自定义家贡献了条目时，这份列表就不再是单/多家内置的来源了 ——
    // 与「多家内置」同一个词（aggregate），下游据此知道列表是拼出来的。
    let source = if data.len() > builtin_count { "aggregate" } else { source };
    list_response_from(data, source, refreshed_at)
}

/// 一家可用提供商都没有时（没加账号），入口校验要跳过「广告里有才放行」与
/// Key 白名单两条判定，把「没有可用账号」那句更可操作的错误留给转发层 ——
/// 与改造前的两条例外逐字同源（见 `pipeline::resolve_model` 的说明）。
pub fn has_available_providers(store: &AccountStore) -> bool {
    !active_manifests(store).is_empty()
}

pub fn advertised_model_ids(store: &AccountStore) -> Vec<String> {
    models_response(store, None).get("data").and_then(Value::as_array)
        .map(|items| items.iter().map(model_id).collect()).unwrap_or_default()
}

pub fn advertised_manifest_contains(store: &AccountStore, model: &str) -> bool {
    advertised_model_ids(store).iter().any(|id| id.eq_ignore_ascii_case(model))
}

pub fn suggest_advertised(store: &AccountStore, model: &str, limit: usize) -> Vec<String> {
    suggest_from(advertised_model_ids(store), model, limit)
}

pub fn session_models(store: &AccountStore) -> Vec<Value> {
    models_response(store, None).get("data").and_then(Value::as_array)
        .cloned().unwrap_or_default().into_iter().map(|mut item| {
            let provider = item.get("owned_by").and_then(Value::as_str).unwrap_or("").to_string();
            let is_default = item.get("is_default").cloned().unwrap_or(Value::Bool(false));
            if let Some(object) = item.as_object_mut() {
                object.insert("providerLabel".to_string(), Value::String(super::super::label_of(&provider)));
                object.insert("provider".to_string(), Value::String(provider));
                object.insert("isDefault".to_string(), is_default);
            }
            item
        }).collect()
}

pub fn models_by_provider(store: &AccountStore) -> Value {
    let mut map = Map::new();
    for (kind, manifest) in active_manifests(store) {
        let mut names: Vec<_> = public_builtin_items(&[(kind, manifest)]).into_iter()
            .map(|(_, item)| model_id(&item)).collect();
        names.sort_by_key(|name| name.to_lowercase());
        map.insert(kind_id(kind).to_string(), json!(names));
    }
    for (provider, items) in super::super::custom::catalog_providers(store) {
        let mut names: Vec<_> = items.iter().map(model_id).collect();
        names.sort_by_key(|name| name.to_lowercase());
        map.insert(provider, json!(names));
    }
    Value::Object(map)
}

/// 每条原始 ID 都返回一个默认绑定；历史的同名映射合并进默认绑定，避免两个开关
/// 控制同一个请求名。默认绑定不可删除，关闭不影响这一行的其他别名。
pub fn manage_view(store: &AccountStore) -> Value {
    let rules = model_rules::current();
    let mut models = Vec::new();
    let mut mappings = Vec::new();
    let mut attached = HashSet::new();
    for (kind, mut manifest) in active_manifests(store) {
        let provider = kind_id(kind);
        manifest.sort_by_key(|item| !rules.default_enabled(provider, &model_id(item)));
        // 一次取「远程与否 + 拉取时刻」：两者是同一处状态，分两次取会多读一次锁
        let (remote, refreshed_at) = refresh_meta(kind);
        let source = if remote { "remote" } else { "builtin" };
        for item in manifest {
            let id = model_id(&item);
            if id.is_empty() { continue; }
            let default = rules.binding(provider, &id, &id);
            let enabled = rules.default_enabled(provider, &id);
            mappings.push(json!({
                "alias": id, "target": id, "provider": provider, "enabled": enabled,
                "reasoning": default.and_then(|mapping| mapping.reasoning.clone()),
                "isDefault": true, "dangling": false, "carried": true,
            }));
            let mut aliases = Vec::new();
            for (index, mapping) in rules.mappings.iter().enumerate() {
                if !mapping.target.eq_ignore_ascii_case(&id)
                    || !mapping.provider.as_deref().map_or(true, |owner| owner == provider)
                { continue; }
                attached.insert(index);
                if mapping.alias.eq_ignore_ascii_case(&id)
                    || aliases.iter().any(|alias: &String| alias.eq_ignore_ascii_case(&mapping.alias))
                { continue; }
                let Some(effective) = rules.binding(provider, &mapping.alias, &id) else { continue };
                aliases.push(mapping.alias.clone());
                mappings.push(json!({
                    "alias": effective.alias, "target": id, "provider": provider,
                    "enabled": effective.enabled, "reasoning": effective.reasoning,
                    "isDefault": false, "dangling": false, "carried": true,
                }));
            }
            let source = if rules.custom.iter().any(|custom| custom.matches(provider, &id)) {
                "manual"
            } else { source };
            models.push(json!({
                "id": id, "name": item.get("name"), "credits": item.get("credits"),
                "isDefault": item.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
                "provider": provider, "providerLabel": super::super::label_of(provider),
                "source": source, "enabled": enabled, "aliases": aliases,
                // 这一家的清单是什么时候拉到的（毫秒；0 = 从未成功拉过）。
                // 缓存恢复的清单与刚拉的清单**都是 `remote`**，时效只能靠这个
                // 时间戳说明（见 `providers::catalog_cache` 的模块头）。
                "refreshedAt": refreshed_at,
            }));
        }
    }
    for (index, mapping) in rules.mappings.iter().enumerate() {
        if attached.contains(&index) { continue; }
        let carried = match mapping.provider.as_deref() {
            Some(provider) => kind_from_id(provider).is_some_and(|kind| {
                entry_id_in_manifest(&manifest_for(kind), &mapping.target).is_some()
            }),
            None => !super::providers_for_model(&mapping.target).is_empty(),
        };
        mappings.push(json!({
            "alias": mapping.alias, "target": mapping.target, "provider": mapping.provider,
            "enabled": mapping.enabled, "reasoning": mapping.reasoning,
            "isDefault": mapping.alias.eq_ignore_ascii_case(&mapping.target),
            "dangling": true, "carried": carried,
        }));
    }
    json!({ "models": models, "mappings": mappings, "reasoningLevels": model_rules::REASONING_LEVELS })
}
