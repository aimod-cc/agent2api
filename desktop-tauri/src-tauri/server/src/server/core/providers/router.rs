//! 模型转发候选。只允许开启的默认绑定或别名进入全局账号队列。
//! provider 顺序仅用于稳定展示，实际尝试顺序仍由账号优先级决定。

use crate::server::core::providers::catalog::{forwarding_providers, providers_for_model};
use crate::server::core::providers::DEFAULT_PROVIDER_ID;

pub fn route_for_forward(model: &str) -> Vec<String> {
    // 只有完全未指定模型时保留旧版的默认上游行为。
    if model.trim().is_empty() {
        return vec![DEFAULT_PROVIDER_ID.to_string()];
    }
    let providers = forwarding_providers(model);
    if !providers.is_empty() {
        return providers;
    }
    // 有家承载、但没有任何开启的绑定 → 全部关闭，不回落默认家（否则
    // 「我明明关掉了」会变成一次落到 workbuddy 的莫名其妙成功）。
    if !providers_for_model(model).is_empty() {
        return Vec::new();
    }
    // 完全未知的名字：沿用既有语义，交给默认上游（resolve_model 在有可用
    // 提供商时已先一步以「模型不存在」拒绝，走到这里的多是没加账号的场景）。
    vec![DEFAULT_PROVIDER_ID.to_string()]
}
