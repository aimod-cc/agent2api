//! 自定义提供商的对外绑定；原始 ID 和别名各自控制可见性及路由。

use serde_json::{json, Value};

use crate::server::core::models::model_id;

pub fn resolve(provider: &Value, requested: &str) -> Option<(String, Option<String>)> {
    if !provider.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let models = provider.get("models").and_then(Value::as_array)?;
    let mappings = provider.get("mappings").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let level = |entry: &Value| entry.get("reasoning").and_then(Value::as_str)
        .map(str::trim).filter(|text| !text.is_empty()).map(str::to_string);
    if let Some(model) = models.iter().find(|model| model_id(model).eq_ignore_ascii_case(requested)) {
        let default = mappings.iter().find(|mapping| {
            mapping.get("alias").and_then(Value::as_str).is_some_and(|alias| alias.eq_ignore_ascii_case(requested))
                && mapping.get("target").and_then(Value::as_str).is_some_and(|target| target.eq_ignore_ascii_case(requested))
        });
        if model.get("enabled").and_then(Value::as_bool).unwrap_or(true)
            && default.map_or(true, |mapping| mapping.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        {
            return Some((model_id(model), default.and_then(level).or_else(|| level(model))));
        }
    }
    mappings.iter().find_map(|mapping| {
        let alias = mapping.get("alias")?.as_str()?;
        let target = mapping.get("target")?.as_str()?;
        if !alias.eq_ignore_ascii_case(requested) || alias.eq_ignore_ascii_case(target)
            || !mapping.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        {
            return None;
        }
        // target 的 enabled 只控制原始 ID，不能阻止别名调用它。
        let model = models.iter().find(|model| model_id(model).eq_ignore_ascii_case(target))?;
        Some((model_id(model), level(mapping)))
    })
}

pub fn public_models(provider: &Value) -> Vec<Value> {
    let mut names = Vec::new();
    if let Some(models) = provider.get("models").and_then(Value::as_array) {
        names.extend(models.iter().map(model_id));
    }
    if let Some(mappings) = provider.get("mappings").and_then(Value::as_array) {
        names.extend(mappings.iter().filter_map(|mapping| mapping.get("alias").and_then(Value::as_str).map(str::to_string)));
    }
    let mut result = Vec::new();
    for name in names {
        if !name.is_empty() && resolve(provider, &name).is_some()
            && !result.iter().any(|model| model_id(model).eq_ignore_ascii_case(&name))
        {
            result.push(json!({ "id": name }));
        }
    }
    result
}
