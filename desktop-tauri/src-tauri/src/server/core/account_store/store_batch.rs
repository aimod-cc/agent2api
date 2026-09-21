//! 账号批量操作（Agent2API W3-T4 从 `store_crud.rs` 拆出，单文件行数约定）。
//!
//!   batch_update  批量修改（启用状态 / 代理；逐账号调 apply_patch）
//!   batch_remove  批量删除（不存在的 id 记入 failed，不中断整批）
//!
//! ── 与单账号操作的关系 ──────────────────────────────────────
//! 两者都依赖 `store_crud` 里的私有内核（`AccountStore::apply_patch`、
//! `to_raccoon_public_account` 等）—— 同一个 `impl AccountStore` 的分块，
//! 拆文件只影响可读性，不影响可见性（Rust 的私有项对同模块可见）。
//!
//! ── 删除保护（W3-T4）────────────────────────────────────────
//! `batch_remove` 会跳过「不可删除」的账号（小浣熊桌面端实时登录态），
//! 记入 `failed` 并附原因 —— 与单账号 `remove_account` 的拒绝口径一致。
//! **注意**：受保护判定必须在取账号锁之前做完（`protected_from_removal`
//! 自己取锁，`std::sync::Mutex` 不可重入，持锁再调会当场死锁）。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::truncate_chars;
use crate::server::core::proxies::describe_account_proxy;
use crate::server::logging;

impl AccountStore {
    /// 批量修改账号（目前支持启用状态与代理）。
    ///
    /// 语义上是「对每个选中账号各跑一次 update_account」，但整批只落盘一次。
    /// 单个账号失败不中断整批：结果里分别记 ok / failed（含原因）。
    pub fn batch_update(
        &self,
        ids: &[String],
        patch: &Value,
    ) -> Result<Value, AccountStoreError> {
        if ids.is_empty() {
            return Err(AccountStoreError::bad_request("缺少要操作的账号 id"));
        }
        let Some(patch_object) = patch.as_object() else {
            return Err(AccountStoreError::bad_request("批量修改内容必须是 JSON 对象"));
        };
        // 批量场景只开放「启用状态」与「代理」：优先级必须唯一，逐账号指定才有意义，
        // 备注名逐个改也说不通，都留给单账号设置面板
        let mut allowed = Map::new();
        let mut enabled_value = false;
        if let Some(value) = patch_object.get("enabled") {
            enabled_value = !matches!(value, Value::Bool(false));
            allowed.insert("enabled".to_string(), Value::Bool(enabled_value));
        }
        if let Some(value) = patch_object.get("proxy") {
            let normalized = crate::server::core::proxies::normalize_account_proxy(value)
                .map_err(|error| AccountStoreError::new(error.message, error.status_code))?
                .unwrap_or(Value::Null);
            allowed.insert("proxy".to_string(), normalized);
        }
        if allowed.is_empty() {
            return Err(AccountStoreError::bad_request("批量修改目前只支持启用状态与代理"));
        }

        let _guard = self.guard();
        // 批量操作按语义要遍历**选中的那些账号**（逐条 apply_patch、逐条记
        // ok/failed），所以这里是「按需取那几条」而不是读全量：ids 里的每一项
        // 走一次主键查询。选中 3 个账号时只读 3 行，而不是把 20 条记录的 JSON
        // 全解析一遍。
        let mut ok: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        // 要写的行（在内存里改好，最后一次性进事务）：只有改动过的才写，于是
        // 「批量点了确定但没有任何字段真的变化」不会产生任何写入
        // （旧实现在那种情况仍会整份重写文件）。
        let mut dirty: Vec<StoredAccount> = Vec::new();
        // 被禁用的 CatPaw 账号 (id, provider)：批量的「禁用」与单账号同语义 ——
        // 它们的会话映射必须作废（见 `store_crud::update_account` 的说明）。
        // 收集起来在写库、放锁之后再动作注册表（两把锁不嵌套）。
        let mut disabled_catpaw: Vec<(String, String)> = Vec::new();
        for id in ids {
            let Some(mut record) = self.record_by_id(&_guard, id) else {
                failed.push(json!({ "id": id, "error": "账号不存在" }));
                continue;
            };
            let name = record.name();
            let provider = record.provider();
            let was_enabled = record.enabled();
            // 优先级冲突判定：批量入口刻意**不开放** priority（见上面的 allowed
            // 注释），所以这个回调不会被调用 —— 给一个必然成功的实现，
            // 保持 `apply_patch` 的签名统一（它要能表达"这个 patch 可能改优先级"）。
            match Self::apply_patch(&mut record, &allowed, |_| Ok(None)) {
                Ok(changes) => {
                    if !changes.is_empty() {
                        record.set_updated_at(logging::now_ms());
                        if was_enabled && !record.enabled() {
                            disabled_catpaw.push((id.clone(), provider));
                        }
                        dirty.push(record);
                    }
                    ok.push(json!({ "id": id, "name": name, "changes": changes }));
                }
                Err(error) => {
                    failed.push(json!({ "id": id, "name": name, "error": error.message }));
                }
            }
        }
        if !dirty.is_empty() {
            // 整批一个事务：要么全成、要么全不成。批量改代理 / 启用状态时
            // 中途失败会留下「前几个改好了、后面没改」的中间态，用户看到的
            // 是「点了一次批量，只生效了一半」—— 事务把这种可能消掉。
            self.with_conn_mut(&_guard, |conn| {
                let tx = conn.transaction()?;
                for record in &dirty {
                    sql::update_in_place(&tx, record)?;
                }
                tx.commit()
            })?;
        }
        // 写库完成后再动注册表（放锁，见 `disabled_catpaw` 的说明）
        drop(_guard);
        for (id, provider) in disabled_catpaw {
            self.invalidate_catpaw_sessions(&id, &provider);
        }
        // 快照要在写入之后给（前端拿到的是最新状态）。批量改的是代理/启用状态，
        // 都不影响列表顺序与优先级，所以重新读一次全量拿到的就是正确快照。
        // 这里**重新取一次锁**而不是复用上面那把：那把已经在动注册表之前放掉了
        // （两把锁不嵌套的纪律，见 `disabled_catpaw` 的说明），而中间那段时间
        // 别的写者可能已经改过库 —— 重取 + 重读保证快照是**当下**的真相，
        // 而不是「放锁前那一瞬」的。读一次全量的成本在批量入口可接受
        // （批量本来就是低频、少量条目的操作）。
        let snapshot = {
            let guard = self.guard();
            self.snapshot(&self.load(&guard))
        };
        // 摘要文案：与 Node 版逐字一致（enabled 与 proxy 两种说法）
        let mut summary: Vec<String> = Vec::new();
        if patch_object.contains_key("enabled") {
            summary.push(format!(
                "启用状态 → {}",
                if enabled_value { "启用" } else { "禁用" }
            ));
        }
        if let Some(proxy) = allowed.get("proxy") {
            // 无代理时说「无代理（直连）」，否则用 describe 出来的 label
            let label = if proxy.is_null() {
                "无代理（直连）".to_string()
            } else {
                describe_account_proxy(Some(proxy))
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("已设置")
                    .to_string()
            };
            summary.push(format!("代理 → {label}"));
        }
        let changed_count = ok
            .iter()
            .filter(|item| {
                item.get("changes")
                    .and_then(Value::as_array)
                    .map(|changes| !changes.is_empty())
                    .unwrap_or(false)
            })
            .count();
        logging::log(
            "[Accounts]",
            &format!(
                "🔀 批量修改 {} 个账号（{}）: 成功 {changed_count} 个{}",
                ids.len(),
                summary.join("，"),
                if failed.is_empty() {
                    String::new()
                } else {
                    format!("，失败 {} 个", failed.len())
                }
            ),
        );
        Ok(json!({ "ok": ok, "failed": failed, "list": snapshot }))
    }

    /// 批量删除账号。不存在的 id 记入 failed 而不是整体报错 —— 用户重复点删除
    /// （或列表已刷新）不会看到莫名其妙的失败。当前账号由优先级派生，
    /// 删掉它之后自动变成下一个可用账号，无需额外处理。
    ///
    /// ── 删除后作废 CatPaw 的会话映射（W5-T-d4）──────────────────
    /// 与单账号 `remove_account` 同一语义：conversationId 属于上游账号上下文，
    /// 账号没了就再也不能续接。provider 在**删除之前**随记录一起收集
    /// （删掉之后回读只能得到 None），注册表动作放在落盘、放锁之后
    /// （两把锁不嵌套）。
    pub fn batch_remove(&self, ids: &[String]) -> Result<Value, AccountStoreError> {
        if ids.is_empty() {
            return Err(AccountStoreError::bad_request("缺少要删除的账号 id"));
        }
        // 受保护账号的判定必须在**取账号锁之前**做完（`protected_from_removal`
        // 自己会取锁，而 std::sync::Mutex 不可重入 —— 持锁再调它会当场死锁）
        let protected: Vec<(String, String)> = ids
            .iter()
            .filter_map(|id| {
                self.protected_from_removal(id)
                    .map(|reason| (id.clone(), reason))
            })
            .collect();
        let _guard = self.guard();
        let mut removed: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        // (账号 id, provider)：注册表作废要用的身份（记录删掉后就查不到了）
        let mut removed_owners: Vec<(String, String)> = Vec::new();
        // 待删的 id：先逐条校验（受保护 / 不存在），全部通过后一次性进事务。
        // 「存在性」用主键查询判（不再读全量），选中几个就查几行。
        let mut targets: Vec<String> = Vec::new();
        for id in ids {
            // 受保护的账号（三家桌面端实时登录态）与「不存在」一样记入 failed，
            // 但不中断整批 —— 与 remove_account 的拒绝口径一致
            if let Some((_, reason)) = protected.iter().find(|(target, _)| target == id) {
                failed.push(json!({ "id": id, "error": reason }));
                continue;
            }
            match self.record_by_id(&_guard, id) {
                Some(record) => {
                    removed.push(json!({ "id": id, "name": record.name() }));
                    removed_owners.push((id.clone(), record.provider()));
                    targets.push(id.clone());
                }
                None => failed.push(json!({ "id": id, "error": "账号不存在" })),
            }
        }
        if targets.is_empty() {
            // 没有任何一条可删：不写库，直接给当前快照
            let state = self.load(&_guard);
            return Ok(json!({ "removed": removed, "failed": failed, "list": self.snapshot(&state) }));
        }
        // 整批一个事务：删除要么全生效、要么一条都不删
        self.with_conn_mut(&_guard, |conn| {
            let tx = conn.transaction()?;
            for id in &targets {
                sql::delete(&tx, id)?;
            }
            tx.commit()
        })?;
        let state = self.load(&_guard);
        let snapshot = self.snapshot(&state);
        let names: String = removed
            .iter()
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("、");
        logging::log(
            "[Accounts]",
            &format!(
                "🗑️  批量删除 {} 个账号: {}{}",
                removed.len(),
                truncate_chars(&names, 200),
                if failed.is_empty() {
                    String::new()
                } else {
                    format!("（{} 个不存在，已跳过）", failed.len())
                }
            ),
        );
        // 落盘完成后作废这些账号的 CatPaw 会话映射（放锁，见本函数说明）
        drop(_guard);
        for (id, provider) in removed_owners {
            self.invalidate_catpaw_sessions(&id, &provider);
        }
        Ok(json!({ "removed": removed, "failed": failed, "list": snapshot }))
    }
}
