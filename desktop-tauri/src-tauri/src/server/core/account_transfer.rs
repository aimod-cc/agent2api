//! 账号导入 / 导出（对照 src/workbuddy-account-transfer.mjs）。
//!
//! 这里是**纯逻辑模块**：不碰磁盘、不持状态，读盘/落盘/号段分配全部由 store 提供
//! （`AccountStore` 的 load_locked / save_locked / with_lock）。
//!
//! 导出形态刻意保留 accessToken / refreshToken：换机器后靠它们直接沿用登录态，
//! 不必再走一遍登录流程。反之 rateLimits 是本机的运行时限额标记、下划线开头的是
//! 内部字段，换机器都没有意义，一律不导出。
//!
//! ── 导入的隔离规则（多提供商）────────────────────────────────
//!   1. **匹配必须同 provider**：identity 为「provider + 业务身份」（WorkBuddy
//!      uid / CatPaw uid|loginName / raccoon、AutoClaw userId）。跨家绝不合并，
//!      也绝不改写他人 provider —— 四家的 id 与 uid 空间互相独立，
//!      只按 uid 匹配会把别家记录整条覆写（连凭证一起丢）。
//!   2. 导入文件**显式**写了 provider 就必须是注册表已知 id（未知 → 该条失败）；
//!      缺 provider（旧导出文件）按 WorkBuddy 兼容，不继承命中记录的 provider。
//!   3. **id 与业务身份在本机一一对应**：更新按身份命中后保留本机 id（导出文件里
//!      的 id 只是新增时的取名建议，本机可能因全局冲突改过名）；新增时若 id 已被
//!      **同 provider 的另一身份**占用则该条失败（不覆盖、不静默改名），被别家
//!      占用则视为全局 id 冲突，分配真正唯一的新 id。
//!   4. **priority 与 addedAt 一律取本机值**：本机的转发顺序由本机决定，回灌一份
//!      导出文件不该把别人的顺序搬过来。新账号的优先级在**所属 provider 的号段内**
//!      分配（满号段明确失败，不回落默认值造成冲突）。
//!   5. **桌面端实时登录态**（`desktop: true` 或三家的固定桌面 id）不导入：它的
//!      凭证按设计不落账号文件，通用导入若凭空建一条「有身份无凭证」的账号只会
//!      得到 401。这类记录计 skipped 并说明改走已有的 importDesktop 入口。
//!
//! 单条坏数据只记一条 failed / skipped 并继续，不让一条脏数据毁掉整批导入。
//! 返回 `{ total, added, updated, skipped, failed, errors }`。
//!
//! 子模块：identity.rs 身份判定与 id/号段分配；normalize.rs 字段归一。

mod identity;
mod normalize;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::MAX_TOKEN_LENGTH;
use crate::server::core::providers::kind_id;
use crate::server::core::providers::ProviderKind;

use identity::{
    allocate_priority, allocate_unique_id, identity_of_item, identity_of_record,
    is_desktop_item, is_reserved_desktop_id, resolve_provider,
};
use normalize::normalize_imported;

/// 导出文件格式版本（导入端据此兼容后续格式变化）
pub const EXPORT_VERSION: i64 = 1;

/// 单条导入的处理结果
enum ImportOutcome {
    Updated,
    Added,
    Skipped(String),
}

/// 导入统计。
///
/// 跳过（桌面端引用等）与失败（坏数据）分开记：结果里的 `failed` 只数真正的
/// 失败，`errors` 先列失败、再列跳过（跳过项带 `skipped: true` 标记），
/// 于是前端「失败明细」不会把跳过原因当成失败展示。
#[derive(Default)]
struct ImportStats {
    added: usize,
    updated: usize,
    skipped: usize,
    failures: Vec<Value>,
    skipped_items: Vec<Value>,
}

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
    // provider 是匹配身份的一部分：显式写出，且以记录的实际归属为准
    // （字段缺失的历史记录按 provider() 的兜底口径写出 WorkBuddy）
    item.insert("provider".to_string(), Value::String(record.provider()));
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

/// 导入账号（导出文件的回流），merge 语义见文件头。
///
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

    let (result, catpaw_invalidations) = store.with_lock(|guard| {
        let mut state = store.load_locked(guard);
        let mut stats = ImportStats::default();
        let mut taken_ids: Vec<String> = state
            .accounts
            .iter()
            .map(|item| item.id().to_string())
            .collect();
        // 需要作废会话的 CatPaw 账号（锁内只收集，落盘并释放锁后再调用注册表）
        let mut catpaw_invalidations: Vec<String> = Vec::new();

        for item in items {
            let outcome = import_one_item(
                &mut state,
                item,
                &mut taken_ids,
                &mut catpaw_invalidations,
            );
            match outcome {
                Ok(ImportOutcome::Updated) => stats.updated += 1,
                Ok(ImportOutcome::Added) => stats.added += 1,
                Ok(ImportOutcome::Skipped(reason)) => {
                    let id = error_id_of(item);
                    stats.skipped += 1;
                    stats
                        .skipped_items
                        .push(json!({ "id": id, "message": reason, "skipped": true }));
                }
                Err(message) => {
                    let id = error_id_of(item);
                    stats.failures.push(json!({ "id": id, "message": message }));
                }
            }
        }

        store.save_locked(&state, guard)?;
        let failed = stats.failures.len();
        // errors 先失败后跳过（跳过项带 skipped 标记，前端可区分展示）
        let mut errors = stats.failures.clone();
        errors.extend(stats.skipped_items.iter().cloned());
        crate::server::logging::log(
            "[Accounts]",
            &format!(
                "📥 账号导入完成: 共 {} 条，新增 {} 个，更新 {} 个，跳过 {} 条，失败 {failed} 条",
                items.len(),
                stats.added,
                stats.updated,
                stats.skipped,
            ),
        );
        let result = json!({
            "total": items.len(),
            "added": stats.added,
            "updated": stats.updated,
            "skipped": stats.skipped,
            "failed": failed,
            "errors": errors,
        });
        Ok::<_, AccountStoreError>((result, catpaw_invalidations))
    })?;

    // 落盘已完成、账号锁已释放：再作废 CatPaw 会话映射（两把锁不嵌套，
    // 与 store_crud 的 remove_account / update_account 同一纪律）
    for id in catpaw_invalidations {
        store.invalidate_catpaw_sessions(&id, kind_id(ProviderKind::CatPaw));
    }
    Ok(result)
}

/// 失败条目回显用的标识：优先 id，其次 uid / userId
fn error_id_of(item: &Value) -> String {
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    for key in ["id", "uid", "userId", "loginName"] {
        let value = text(key);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

/// 处理单条导入记录。
///
/// 返回 `Ok(Updated/Added/Skipped)` 或 `Err(原因)`（该条失败，批次继续）。
fn import_one_item(
    state: &mut crate::server::core::account_store::state::AccountState,
    item: &Value,
    taken_ids: &mut Vec<String>,
    catpaw_invalidations: &mut Vec<String>,
) -> Result<ImportOutcome, String> {
    let Some(object) = item.as_object() else {
        return Err("账号记录必须是 JSON 对象".to_string());
    };
    let item_id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if item_id.is_empty() {
        return Err("缺少 id".to_string());
    }

    // ── 桌面端实时登录态的引用：不导入（见文件头第 5 条）──
    // 这道判定放在 provider 校验之前：桌面引用本来就不是可迁移账号，
    // 无论它的 provider 写的是什么，都按「跳过」而不是「失败」处理。
    if is_desktop_item(object) || is_reserved_desktop_id(&item_id) {
        return Ok(ImportOutcome::Skipped(
            "桌面端实时登录态账号不随导出文件迁移（凭证在本机客户端存储，不落账号文件）；\
             请在目标机器用「导入桌面端登录态」重新读取"
                .to_string(),
        ));
    }

    // ── provider 校验：显式 provider 必须是注册表已知 id ──
    let provider = resolve_provider(object)?;

    // ── 凭证：至少要有一个 token（只有引用字段的记录不构成可迁移账号）──
    let access_token = object
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let refresh_token = object
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

    // ── 身份：provider + 业务身份（四家字段不同，见 identity.rs）──
    let identity = identity_of_item(&provider, object)?;

    // 本机 id 撞上桌面端实时登录态时跳过：固定桌面 id 不可被普通导入记录顶替
    // （它的凭证在客户端存储，改写只会把实时登录态变成一份过期副本）
    if state
        .accounts
        .iter()
        .any(|record| record.id() == item_id && record.is_desktop())
    {
        return Ok(ImportOutcome::Skipped(format!(
            "账号 id「{item_id}」在本机是桌面端实时登录态（凭证在客户端存储），\
             通用导入不改写它；如需刷新请在目标机器用「导入桌面端登录态」"
        )));
    }

    // 同 provider 内业务身份匹配（带 provider 作用域：跨家同 uid 不会相遇）。
    // 桌面端记录不参与匹配：它与普通账号可以是同一个人的两份记录，
    // 拿它当合并目标就会把「实时登录态」改写掉。
    let matched = state.accounts.iter().position(|record| {
        !record.is_desktop()
            && record.provider() == provider
            && identity_of_record(&provider, record).as_deref() == Some(identity.as_str())
    });
    if let Some(index) = matched {
        // 同 provider 内「id ↔ 业务身份」必须一致：导入 id 若已指向本 provider 的
        // 另一条身份，说明导出文件与本地状态自相矛盾 —— 该条失败，不覆盖也不改名
        if state.accounts[index].id() != item_id {
            let points_elsewhere = state.accounts.iter().any(|record| {
                record.id() == item_id
                    && record.provider() == provider
                    && identity_of_record(&provider, record).as_deref() != Some(identity.as_str())
            });
            if points_elsewhere {
                return Err(format!(
                    "账号 id「{item_id}」在本提供商内指向另一条身份，与身份「{identity}」矛盾\
                     （不覆盖，请先核对导出文件）"
                ));
            }
        }
        // 同 provider + 同业务身份 → 合并到本机记录，**保留本机 id**：
        // 导出文件里的 id 只是新增时的取名建议（本机可能因全局冲突改过名），
        // 按它去改写本机 id 会破坏「id 全局唯一」与重导的幂等性。
        let before = state.accounts[index].clone();
        let normalized = normalize_imported(object, &provider, Some(&before))?;
        {
            let target = state.accounts[index].fields_mut();
            for (key, value) in normalized {
                target.insert(key, value);
            }
        }
        // 非 WorkBuddy 记录不该带腾讯端点字段：旧版导入曾把它们注入别家记录，
        // 这里在合并时清掉（三家适配器都不读这些键，见各家凭证模块）
        if provider != kind_id(ProviderKind::WorkBuddy) {
            for key in ["edition", "prefixPath", "endpoint", "platform"] {
                state.accounts[index].remove(key);
            }
        }
        state.accounts[index].set("updatedAt", Value::from(crate::server::logging::now_ms()));
        if provider == kind_id(ProviderKind::CatPaw) {
            // 凭证被更新：该账号的 CatPaw 会话映射必须在锁外作废
            catpaw_invalidations.push(state.accounts[index].id().to_string());
        }
        return Ok(ImportOutcome::Updated);
    }

    // ── 新增 ──
    // 走到这里说明本 provider 没有同身份的普通账号：若该 id 仍被本 provider 的
    // 别的身份占用，就是导出文件自相矛盾 —— 失败而不是覆盖，也不静默改名
    if let Some(existing) = state.accounts.iter().find(|record| record.id() == item_id) {
        if existing.provider() == provider {
            return Err(format!(
                "账号 id「{item_id}」在本提供商内已属于另一条身份「{}」，无法用于身份「{identity}」\
                 （不覆盖，请先核对导出文件）",
                existing.name()
            ));
        }
    }
    // 本机 id 空间是**全局**的（四家共用一份列表）：导入原值被别家记录占用时，
    // 分配一个真正唯一的 id（绝不覆盖他人，也不回落成同样可能冲突的派生值）
    let taken: std::collections::HashSet<String> = taken_ids.iter().cloned().collect();
    let Some(new_id) = allocate_unique_id(&item_id, &taken) else {
        return Err(format!("账号 id「{item_id}」冲突，且无法分配唯一 id"));
    };

    let normalized = normalize_imported(object, &provider, None)?;
    let now = crate::server::logging::now_ms();
    let mut record = Map::new();
    record.insert("id".to_string(), Value::String(new_id.clone()));
    // provider 与身份字段（uid / loginName / userId）已由 normalize 保留
    for (key, value) in normalized {
        record.insert(key, value);
    }
    // priority 在**所属 provider 的号段内**分配；满号段明确失败（不冲突不覆盖）
    let used: Vec<i64> = state
        .accounts
        .iter()
        .filter(|record| record.provider() == provider)
        .map(StoredAccount::priority)
        .collect();
    let Some(priority) = allocate_priority(&used) else {
        return Err(format!(
            "{provider} 的优先级号段（0-9999）已用满，无法为新账号分配优先级"
        ));
    };
    record.insert("priority".to_string(), Value::from(priority));
    record.insert("addedAt".to_string(), Value::from(now));
    record.insert("updatedAt".to_string(), Value::from(now));
    // 导入的普通账号没有桌面端标记：避免被当成本机实时登录态
    record.remove("desktop");
    let mut saved = StoredAccount::from_map(record);
    if saved.name().is_empty() {
        let fallback = {
            let nickname = saved.nickname();
            if nickname.is_empty() {
                format!(
                    "账号 {}",
                    crate::server::core::account_store::store_util::truncate_text(&identity, 8)
                )
            } else {
                nickname
            }
        };
        saved.set("name", Value::String(fallback));
    }
    state.accounts.push(saved);
    // id 记入占用集合：同批后续记录不会分到同一个 id
    taken_ids.push(new_id);
    Ok(ImportOutcome::Added)
}
