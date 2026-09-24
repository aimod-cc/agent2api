//! 聚合模型目录：内置提供商的原始清单、对外绑定及发送名解析。
//!
//! 原始模型 ID 是默认绑定，别名是额外绑定。各绑定独立开关；关闭默认绑定
//! 不会禁用它指向的上游模型，也不会影响仍开启的别名。
//! 清单来源在本文件，绑定解析在 routing.rs，对外与管理视图在 view.rs。

mod routing;
mod view;

use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::model_rules;
use crate::server::core::models::{model_id, ModelCatalog};
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::{kind_from_id, kind_id, ProviderKind, PROVIDERS};

use super::autoclaw::region::Region;

pub use routing::{
    default_model_catalog, default_model_usable, forwarding_providers, model_blocked_everywhere,
    providers_for_model, wire_target_for_provider, WireTarget,
};
pub use view::{
    advertised_manifest_contains, advertised_model_ids, has_available_providers, manage_view,
    models_by_provider, models_response, session_models, suggest_advertised,
};

/// 原始能力清单不应用绑定开关；关闭原始 ID 后，别名仍需解析到这条上游记录。
fn manifest_for(kind: ProviderKind) -> Vec<Value> {
    let mut models = adapter_for(kind).list_models();
    for item in model_rules::custom_models_for(kind_id(kind)) {
        let id = model_id(&item);
        if !id.is_empty()
            && !models.iter().any(|existing| model_id(existing).eq_ignore_ascii_case(&id))
        {
            models.push(item);
        }
    }
    models
}

fn advertised_manifest_for(store: &AccountStore, kind: ProviderKind) -> Vec<Value> {
    adapter_for(kind).advertise_models(store, manifest_for(kind))
}

fn workbuddy_catalog() -> ModelCatalog {
    crate::server::core::models::global_catalog()
}

fn autoclaw_catalog_state(region: Region) -> (bool, i64) {
    (
        !super::autoclaw::catalog::remote_models(region).is_empty(),
        super::autoclaw::catalog::last_refreshed_at(region),
    )
}

fn refresh_meta(kind: ProviderKind) -> (bool, i64) {
    match kind {
        ProviderKind::WorkBuddy => (
            workbuddy_catalog().remote_refreshed(),
            workbuddy_catalog().last_refreshed_at(),
        ),
        ProviderKind::Raccoon => (
            super::raccoon::models::remote_refreshed(),
            super::raccoon::models::last_refreshed_at(),
        ),
        ProviderKind::Qoder => (
            super::qoder::models::remote_refreshed(super::qoder::endpoints::Region::Global)
                || super::qoder::models::remote_refreshed(super::qoder::endpoints::Region::Cn),
            super::qoder::models::last_refreshed_at(),
        ),
        ProviderKind::CatPaw => (
            !super::catpaw::catalog::remote_models().is_empty(),
            super::catpaw::catalog::last_refreshed_at(),
        ),
        ProviderKind::AutoClaw => autoclaw_catalog_state(Region::Cn),
        ProviderKind::AutoClawIntl => autoclaw_catalog_state(Region::Intl),
        ProviderKind::ClineFree | ProviderKind::ClinePass => (
            super::cline::models::remote_refreshed(),
            super::cline::models::last_refreshed_at(),
        ),
        // Accio 两个地区各有自己的目录缓存（上游按 `x-package-region` 给清单）：
        // 两家任一刷过就算「有远程来源」，时间取两者里更近的那次
        ProviderKind::Accio | ProviderKind::AccioCn => {
            let region = super::accio::endpoints::Region::from_kind(kind)
                .unwrap_or(super::accio::endpoints::Region::Global);
            (
                !super::accio::models::remote_models(region).is_empty(),
                super::accio::models::last_refreshed_at(region),
            )
        }
    }
}

fn all_kinds() -> Vec<ProviderKind> {
    PROVIDERS.iter().filter_map(|meta| kind_from_id(meta.id)).collect()
}

pub fn provider_available(store: &AccountStore, kind: ProviderKind) -> bool {
    !store.accounts_for_provider(kind_id(kind)).is_empty()
        || adapter_for(kind).env_credentials_present()
}

/// 管理视图需要完整清单；对外视图在此基础上再应用每条绑定的开关。
pub fn active_manifests(store: &AccountStore) -> Vec<(ProviderKind, Vec<Value>)> {
    all_kinds()
        .into_iter()
        .filter(|kind| provider_available(store, *kind))
        .filter_map(|kind| {
            let manifest = advertised_manifest_for(store, kind);
            (!manifest.is_empty()).then_some((kind, manifest))
        })
        .collect()
}

fn aggregate_source(active: &[(ProviderKind, Vec<Value>)]) -> (&'static str, i64) {
    match active {
        [(kind, _)] => {
            let (remote, refreshed_at) = refresh_meta(*kind);
            (if remote { "remote" } else { "builtin" }, refreshed_at)
        }
        [] => ("none", 0),
        _ => ("aggregate", active.first().map(|(kind, _)| refresh_meta(*kind).1).unwrap_or(0)),
    }
}
