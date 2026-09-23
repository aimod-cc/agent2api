//! 导入记录的身份判定：provider 校验、业务身份提取、桌面引用识别、唯一 id 与
//! 优先级分配。
//!
//! ── 为什么身份必须带 provider 作用域 ─────────────────────────
//! 各家的 id / uid 空间互相独立：CatPaw 的账号 id 就是 uid 或 loginName，
//! WorkBuddy 的 uid 也可能是同一串数字。只按 uid 匹配会把别家的记录整条覆写
//! （连凭证一起丢）。因此这里所有判定都产出「provider + 身份」二元组，
//! 由调用方按二元组匹配本机记录。
//!
//! 身份字段（各家落盘字段的事实来源见 account_store 各模块）：
//!   WorkBuddy = uid；CatPaw = uid（缺则 loginName）；raccoon / AutoClaw =
//!   userId；Qoder = 地区 + userId（添加路径 `qoder_accounts` 按「地区 + userId」
//!   判重，这里必须同口径 —— 只按 userId 会把国际版 / 中国版两条记录并成一条）；
//!   Cline 两池 = account；自定义提供商（custom- 前缀）= apiKey（缺则账号 id，
//!   与 `custom_accounts` 「同 key 合并」的幂等口径一致）。

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::{
    normalize_priority_value, DEFAULT_PRIORITY, MAX_PRIORITY, MIN_PRIORITY,
};
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::providers::{
    is_known_provider_id, kind_id, ProviderKind, DEFAULT_PROVIDER_ID,
};

/// 桌面端实时登录态的固定账号 id（记录里按设计不落 token，凭证实时读客户端文件）。
///
/// **全局保留**：任何 provider 的导入记录用了这些 id 都跳过 —— 既避免把引用
/// 变成普通账号，也避免占住 id 让后续 importDesktop 无法创建/刷新。
///
/// AutoClaw 占两项（国内版 / 国际版各一个 id）：两地的桌面端账号是两条独立
/// 记录（读的是同一个 `auth.json`，但归不同的 provider），因此两个 id 都要
/// 保留 —— 漏了国际版那个，它的桌面端记录就能被当成普通账号导入，然后因为
/// 「不落 token」而永远 `available: false`。
pub(super) const RESERVED_DESKTOP_IDS: [&str; 4] = [
    crate::server::core::providers::raccoon::credentials::DESKTOP_ACCOUNT_ID,
    crate::server::core::providers::catpaw::credentials::DESKTOP_ACCOUNT_ID,
    crate::server::core::account_store::autoclaw_accounts::DESKTOP_ACCOUNT_ID,
    crate::server::core::account_store::autoclaw_accounts::INTL_DESKTOP_ACCOUNT_ID,
];

pub(super) fn is_reserved_desktop_id(id: &str) -> bool {
    if RESERVED_DESKTOP_IDS.iter().any(|known| *known == id) {
        return true;
    }
    // Cline 两个池的桌面端固定 id（`cline-free-desktop` / `cline-pass-desktop`，
    // 拼装规则见 `cline::credentials::desktop_account_id` —— 它返回 String，
    // 进不了上面的 const 数组，只能在这里现拼）。漏了它们，手工构造的
    // 「带 Cline 桌面 id 但不带 desktop 标记」的导入记录会占住这个 id，
    // 目标机器的「导入桌面端登录态」就建不出实时账号了。
    let suffix = crate::server::core::providers::cline::credentials::DESKTOP_ACCOUNT_SUFFIX;
    [
        kind_id(ProviderKind::ClineFree),
        kind_id(ProviderKind::ClinePass),
    ]
    .iter()
    .any(|provider| *id == format!("{provider}-{suffix}"))
}

/// 记录带 `desktop: true` 标记 = 桌面端实时登录态的引用，不是可迁移的普通账号。
pub(super) fn is_desktop_item(item: &Map<String, Value>) -> bool {
    matches!(item.get("desktop"), Some(Value::Bool(true)))
}

/// 解析记录声明的 provider。
///
/// 缺字段 / null / 空串 → 历史数据兼容为 WorkBuddy（旧导出文件没有 provider）；
/// 非空但不是注册表已知 id → Err（绝不把未知 id 当成 WorkBuddy 存进 WorkBuddy 组）。
///
/// **例外：`custom-` 前缀**（自定义提供商）。它是运行期数据、刻意不进注册表
/// （见 `custom_providers` 模块头），按注册表判会被整条拒掉 —— 那正是自定义
/// 账号「导得出、导不回」的根子。这里放行前缀，存在性校验由调用方在合并完
/// 导入文件里的提供商定义之后做（`custom_providers::get`）。
pub(super) fn resolve_provider(item: &Map<String, Value>) -> Result<String, String> {
    match item.get("provider") {
        None | Some(Value::Null) => Ok(DEFAULT_PROVIDER_ID.to_string()),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(DEFAULT_PROVIDER_ID.to_string());
            }
            if !trimmed.starts_with(crate::server::core::custom_providers::ID_PREFIX)
                && !is_known_provider_id(trimmed)
            {
                return Err(format!(
                    "未知的提供商 id「{trimmed}」：注册表不认识，不能当作 WorkBuddy 导入"
                ));
            }
            Ok(trimmed.to_string())
        }
        Some(_) => Err("provider 字段必须是字符串".to_string()),
    }
}

/// Qoder 记录 / 导入条目上的地区标识（与 `qoder::Region::from_payload` 同键序：
/// `mode` 优先、`edition` 兜底）。
///
/// 三态返回：
///   - `Ok(Some(region))`：解析成功；
///   - `Ok(None)`：键缺失 → 调用方按国际版（添加路径对缺失的缺省就是国际版，
///     两处缺省必须一致，否则同一条记录两边算出的身份对不上）；
///   - `Err(())`：值存在但不是已知地区 —— 无法识别（条目失败 / 记录不参与匹配）。
fn qoder_region(fields: &Map<String, Value>) -> Result<Option<String>, ()> {
    let raw = match fields
        .get("mode")
        .filter(|value| !value.is_null())
        .or_else(|| fields.get("edition"))
    {
        Some(raw) => raw,
        None => return Ok(None),
    };
    match raw.as_str().ok_or(())?.trim() {
        "" | "global" | "intl" => Ok(Some("global".to_string())),
        "cn" => Ok(Some("cn".to_string())),
        _ => Err(()),
    }
}

/// 导入记录的业务身份；无法确定身份时 Err（该条失败，不做猜测性匹配）。
pub(super) fn identity_of_item(provider: &str, item: &Map<String, Value>) -> Result<String, String> {
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let uid = text("uid");
    let login_name = text("loginName");
    let user_id = text("userId");
    if provider == kind_id(ProviderKind::WorkBuddy) {
        if uid.is_empty() {
            return Err("缺少 uid（无法标识 WorkBuddy 账号）".to_string());
        }
        return Ok(uid);
    }
    if provider == kind_id(ProviderKind::CatPaw) {
        if !uid.is_empty() {
            return Ok(uid);
        }
        if !login_name.is_empty() {
            return Ok(login_name);
        }
        return Err("缺少 uid 与 loginName（无法标识 CatPaw 账号）".to_string());
    }
    // Cline 的账号标识落在 `account`（邮箱或 `usr-…` id）而不是 `userId`
    // —— 见 `account_store::cline_accounts` 的公开形态。少了这一支，
    // Cline 账号导出后再导入会因「缺少 userId」整条失败。
    // **两个池共用这一支**（判据是「属于 Cline 系」，不是某个具体 provider id）。
    if crate::server::core::account_store::is_cline_family(provider) {
        let account = text("account");
        if !account.is_empty() {
            return Ok(account);
        }
        return Err("缺少 account（无法标识 Cline 账号）".to_string());
    }
    // Qoder：地区 + userId 两段身份（与 `qoder_accounts` 添加路径的判重口径
    // 一致 —— 只按 userId 会把同一个人在两个地区的账号并成一条）。
    if provider == kind_id(ProviderKind::Qoder) {
        if user_id.is_empty() {
            return Err("缺少 userId（无法标识 Qoder 账号）".to_string());
        }
        let region = match qoder_region(item) {
            Ok(Some(region)) => region,
            Ok(None) => "global".to_string(),
            Err(()) => {
                return Err("Qoder 地区无法识别（mode / edition 必须是 global 或 cn）".to_string())
            }
        };
        return Ok(format!("{region}:{user_id}"));
    }
    // 自定义提供商：凭证就是身份 —— apiKey 非空时与添加路径「同 key 合并」
    // 完全同口径（添加路径的账号 id 就是 key 的 SHA-256 前缀）；空 key 没有
    // 稳定标识，退到账号 id（空 key 的记录 id 是随机的，但全局唯一仍然成立）。
    if provider.starts_with(crate::server::core::custom_providers::ID_PREFIX) {
        let api_key = text("apiKey");
        if !api_key.is_empty() {
            return Ok(api_key);
        }
        let id = text("id");
        if !id.is_empty() {
            return Ok(id);
        }
        return Err("缺少 apiKey 与 id（无法标识自定义提供商账号）".to_string());
    }
    if user_id.is_empty() {
        return Err(format!("缺少 userId（无法标识 {provider} 账号）"));
    }
    Ok(user_id)
}

/// 本机记录的业务身份（与 `identity_of_item` 同一口径）；身份字段缺失时 None。
pub(super) fn identity_of_record(provider: &str, record: &StoredAccount) -> Option<String> {
    let login_name = record
        .get("loginName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if provider == kind_id(ProviderKind::WorkBuddy) {
        let uid = record.uid().trim().to_string();
        return (!uid.is_empty()).then_some(uid);
    }
    if provider == kind_id(ProviderKind::CatPaw) {
        let uid = record.uid().trim().to_string();
        if !uid.is_empty() {
            return Some(uid);
        }
        return (!login_name.is_empty()).then_some(login_name);
    }
    // Cline：身份在 `account` 键上（与 `identity_of_item` 同一口径，两池共用）
    if crate::server::core::account_store::is_cline_family(provider) {
        let account = record
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        return (!account.is_empty()).then_some(account);
    }
    // Qoder：地区 + userId（与 `identity_of_item` 同口径，地区取 `mode` /
    // `edition`，键缺失按国际版 —— 两处缺省必须一致）。地区值非法的记录视为
    // 无法识别身份（不参与匹配，导入按新增走；正常数据不会出现这种记录）。
    if provider == kind_id(ProviderKind::Qoder) {
        let user_id = record.user_id().trim().to_string();
        if user_id.is_empty() {
            return None;
        }
        let region = match qoder_region(record.fields()) {
            Ok(Some(region)) => region,
            Ok(None) => "global".to_string(),
            Err(()) => return None,
        };
        return Some(format!("{region}:{user_id}"));
    }
    // 自定义提供商：apiKey 非空即身份（同 key 合并），否则账号 id
    if provider.starts_with(crate::server::core::custom_providers::ID_PREFIX) {
        let api_key = record
            .get("apiKey")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !api_key.is_empty() {
            return Some(api_key);
        }
        let id = record.id().trim().to_string();
        return (!id.is_empty()).then_some(id);
    }
    let user_id = record.user_id().trim().to_string();
    (!user_id.is_empty()).then_some(user_id)
}

/// 分配一个**真正全局唯一**的账号 id：优先用导入原值，被占用时依次尝试
/// `<id>-2`、`<id>-3`…，再退到带时间戳的后备形态；实在分不出（几乎不可能）返回 None。
///
/// 为什么不能用 `user-<uid>` 这类派生值兜底：它同样可能已被别家账号占用
/// （id 空间是全局的），而覆盖他人 id 会连凭证一起丢。
pub(super) fn allocate_unique_id(preferred: &str, taken: &HashSet<String>) -> Option<String> {
    if !taken.contains(preferred) {
        return Some(preferred.to_string());
    }
    for suffix in 2..=9999 {
        let candidate = format!("{preferred}-{suffix}");
        if !taken.contains(&candidate) {
            return Some(candidate);
        }
    }
    let base = format!("{preferred}-{}", crate::server::logging::now_ms());
    if !taken.contains(&base) {
        return Some(base);
    }
    for suffix in 2..=9999 {
        let candidate = format!("{base}-{suffix}");
        if !taken.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 在全局队列里分配空闲优先级（排在现有账号之后）。
///
/// 优先级唯一性的作用域是**全部账号**（所有提供商共用一条转发队列，见
/// `priority.rs` 模块头），调用方必须把所有账号的优先级传进来 —— 只传同
/// provider 的会算出与别家冲突的值。号段满时返回 None，**调用方必须报错**：
/// 回落默认值会造成优先级冲突（`next_free_priority` 的兜底路径就是那样，
/// 通用导入不能接受）。
pub(super) fn allocate_priority(used: &[i64]) -> Option<i64> {
    let normalized: Vec<i64> = used
        .iter()
        .map(|value| normalize_priority_value(*value))
        .collect();
    let max = normalized
        .iter()
        .copied()
        .max()
        .unwrap_or(DEFAULT_PRIORITY - 1);
    let mut candidate = max.saturating_add(1);
    while candidate <= MAX_PRIORITY && normalized.contains(&candidate) {
        candidate += 1;
    }
    if candidate <= MAX_PRIORITY {
        return Some(candidate);
    }
    (MIN_PRIORITY..=MAX_PRIORITY).find(|value| !normalized.contains(value))
}
