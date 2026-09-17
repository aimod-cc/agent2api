//! 账号增删改查（从 store.rs 拆出，单文件行数约定）。
//!
//! 覆盖 Node 版 workbuddy-account-store.mjs 的写入侧全集：
//!   addAccount / removeAccount / promoteToFront / updateAccount(applyAccountPatch)
//!   / moveAccount / batchUpdate / batchRemove
//!
//! 三条不变量在这里落地，改代码前务必读 `super::mod` 的头部说明：
//!   1. **优先级全局唯一**：写入侧遇到冲突一律 409，由用户显式选一个空闲值；
//!   2. **未知字段全量保留**：记录是 JSON 对象（`StoredAccount`），只改自己要改的键；
//!   3. **持锁期间不做网络请求**：本文件全是纯文件读写，没有任何 await。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::priority::{
    find_priority_holder, next_free_priority, normalize_priority, renumber_consecutively,
    DEFAULT_PRIORITY,
};
use crate::server::core::account_store::state::{AccountState, StoredAccount};
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::{
    js_string, number_or, object_or_empty, optional_text, pick_token, token_tail_of, truncate_chars,
    value_or, value_or_nullish,
};
use crate::server::core::account_store::{MAX_ACCOUNTS, MAX_TOKEN_LENGTH};
use crate::server::core::endpoints::resolve_edition;
use crate::server::core::proxies::describe_account_proxy;
use crate::server::logging;

impl AccountStore {
    // ─── CRUD ────────────────────────────────────────────────

    /// 添加/更新账号（对照 Node 版 `addAccount`）。
    ///
    /// 接受 session 形态（auth + account）或裸凭证形态（accessToken/refreshToken，
    /// 可含 uid/nickname/expiresAt/domain）。edition 决定端点/prefixPath/platform
    /// 默认值；显式传入的 endpoint/prefixPath/platform 优先。
    /// 新账号的优先级取「现有最大值 + 1」即排在队尾，不会抢占当前账号。
    pub fn add_account(
        &self,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<Value, AccountStoreError> {
        let Some(payload_object) = payload.as_object() else {
            return Err(AccountStoreError::bad_request("账号内容必须是 JSON 对象"));
        };
        let auth = object_or_empty(payload_object.get("auth"));
        let account = object_or_empty(payload_object.get("account"));

        let access_token = pick_token(&auth, &["accessToken", "token", "access_token"]);
        let refresh_token = pick_token(&auth, &["refreshToken", "refresh_token"]);
        if access_token.is_empty() {
            return Err(AccountStoreError::bad_request("缺少 accessToken"));
        }
        if access_token.chars().count() > MAX_TOKEN_LENGTH
            || refresh_token.chars().count() > MAX_TOKEN_LENGTH
        {
            return Err(AccountStoreError::bad_request("token 过长"));
        }
        let uid = {
            let candidate = value_or(
                account.get("uid").or_else(|| payload_object.get("uid")),
                Value::String(String::new()),
            );
            js_string(&candidate).trim().to_string()
        };
        if uid.is_empty() {
            return Err(AccountStoreError::bad_request("缺少 uid（无法标识账号）"));
        }

        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let id = format!("user-{uid}");
        let existing = state.accounts.iter().find(|item| item.id() == id).cloned();
        let others: Vec<StoredAccount> = state
            .accounts
            .iter()
            .filter(|item| item.id() != id)
            .cloned()
            .collect();
        if existing.is_none() && others.len() >= MAX_ACCOUNTS {
            return Err(AccountStoreError::bad_request(format!(
                "最多保存 {MAX_ACCOUNTS} 个账号"
            )));
        }

        // 代理/优先级校验放在写盘前：非法输入直接报错，不留下半成品记录
        let resolved_proxy = if payload_object.contains_key("proxy") {
            crate::server::core::proxies::normalize_account_proxy(
                payload_object.get("proxy").unwrap_or(&Value::Null),
            )
            .map_err(|error| AccountStoreError::new(error.message, error.status_code))?
            .unwrap_or(Value::Null)
        } else {
            existing
                .as_ref()
                .map(|record| value_or_nullish(record.get("proxy"), Value::Null))
                .unwrap_or(Value::Null)
        };
        let edition = resolve_edition(
            payload_object
                .get("edition")
                .filter(|value| !value.is_null())
                .map(js_string)
                .or_else(|| existing.as_ref().and_then(StoredAccount::edition))
                .as_deref(),
        );
        // 新账号默认排在末尾，避免凭空插队改变现有转发顺序；显式指定则校验唯一
        let existing_priority = existing.as_ref().map(StoredAccount::priority);
        let priority = match payload_object.get("priority") {
            Some(value) => normalize_priority(
                Some(value),
                existing_priority.unwrap_or(DEFAULT_PRIORITY),
            ),
            None => match existing_priority {
                Some(value) => value,
                None => next_free_priority(
                    &others.iter().map(StoredAccount::priority).collect::<Vec<_>>(),
                ),
            },
        };
        let holder_entries: Vec<(String, String, i64)> = others
            .iter()
            .map(|record| (record.id().to_string(), record.name(), record.priority()))
            .collect();
        if let Some((_, holder_name)) = find_priority_holder(&holder_entries, priority, None) {
            return Err(AccountStoreError::new(
                format!("优先级 {priority} 已被账号「{holder_name}」占用，请换一个（优先级需全局唯一）"),
                409,
            ));
        }

        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        // 备注名：显式传入优先，其次账号昵称、原有备注名，最后按 uid 生成
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, 100))
            .filter(|value| !value.is_empty());
        let record_name = explicit_name
            .or_else(|| optional_text(account.get("nickname")))
            .or_else(|| optional_text(payload_object.get("nickname")))
            .or_else(|| existing.as_ref().and_then(|item| optional_text(item.get("name"))))
            .unwrap_or_else(|| format!("账号 {}", truncate_chars(&uid, 8)));
        record.insert("name".to_string(), Value::String(record_name.clone()));
        record.insert(
            "uid".to_string(),
            Value::String(uid.clone()),
        );
        record.insert(
            "nickname".to_string(),
            Value::String(
                optional_text(account.get("nickname"))
                    .or_else(|| optional_text(payload_object.get("nickname")))
                    .or_else(|| existing.as_ref().map(StoredAccount::nickname))
                    .unwrap_or_default(),
            ),
        );
        record.insert(
            "type".to_string(),
            Value::String(
                optional_text(account.get("type"))
                    .or_else(|| optional_text(payload_object.get("type")))
                    .or_else(|| existing.as_ref().map(StoredAccount::account_type))
                    .unwrap_or_else(|| "personal".to_string()),
            ),
        );
        for (key, from_account, from_payload) in [
            ("enterpriseId", "enterpriseId", "enterpriseId"),
            ("enterpriseName", "enterpriseName", "enterpriseName"),
        ] {
            let value = optional_text(account.get(from_account))
                .or_else(|| optional_text(payload_object.get(from_payload)))
                .or_else(|| {
                    existing
                        .as_ref()
                        .and_then(|item| optional_text(item.get(key)))
                })
                .unwrap_or_default();
            record.insert(key.to_string(), Value::String(value));
        }
        record.insert("accessToken".to_string(), Value::String(access_token.clone()));
        record.insert(
            "refreshToken".to_string(),
            Value::String(if refresh_token.is_empty() {
                existing
                    .as_ref()
                    .map(StoredAccount::refresh_token)
                    .unwrap_or_default()
            } else {
                refresh_token
            }),
        );
        record.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&access_token)),
        );
        record.insert(
            "expiresAt".to_string(),
            number_or(
                auth.get("expiresAt").or_else(|| payload_object.get("expiresAt")),
                existing
                    .as_ref()
                    .and_then(StoredAccount::expires_at)
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            ),
        );
        record.insert(
            "refreshExpiresAt".to_string(),
            number_or(
                auth
                    .get("refreshExpiresAt")
                    .or_else(|| payload_object.get("refreshExpiresAt")),
                existing
                    .as_ref()
                    .and_then(StoredAccount::refresh_expires_at)
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            ),
        );
        record.insert(
            "domain".to_string(),
            Value::String(
                optional_text(auth.get("domain"))
                    .or_else(|| existing.as_ref().map(StoredAccount::domain))
                    .unwrap_or_default(),
            ),
        );
        record.insert("edition".to_string(), Value::String(edition.id.to_string()));
        // 先取出既有记录的这三个字段：payload 里没给就沿用原来的（Node 用 `??`，
        // 所以空串是有效值、只有 null/缺失才回落版本的默认值）
        let existing_prefix = existing.as_ref().and_then(|item| item.get("prefixPath")).cloned();
        let existing_endpoint = existing.as_ref().and_then(|item| item.get("endpoint")).cloned();
        let existing_platform = existing.as_ref().and_then(|item| item.get("platform")).cloned();
        record.insert(
            "prefixPath".to_string(),
            value_or_nullish(
                payload_object.get("prefixPath").or(existing_prefix.as_ref()),
                Value::String(edition.prefix_path.to_string()),
            ),
        );
        record.insert(
            "endpoint".to_string(),
            value_or_nullish(
                payload_object.get("endpoint").or(existing_endpoint.as_ref()),
                Value::String(edition.endpoint.to_string()),
            ),
        );
        record.insert(
            "platform".to_string(),
            value_or_nullish(
                payload_object.get("platform").or(existing_platform.as_ref()),
                Value::String(edition.platform.to_string()),
            ),
        );
        record.insert("priority".to_string(), Value::from(priority));
        let enabled = match payload_object.get("enabled") {
            Some(value) => !matches!(value, Value::Bool(false)),
            None => existing.as_ref().map(StoredAccount::enabled).unwrap_or(true),
        };
        record.insert("enabled".to_string(), Value::Bool(enabled));
        record.insert("proxy".to_string(), resolved_proxy);
        let added_at = existing
            .as_ref()
            .map(StoredAccount::added_at)
            .filter(|value| *value != 0)
            .unwrap_or_else(logging::now_ms);
        record.insert("addedAt".to_string(), Value::from(added_at));
        record.insert("updatedAt".to_string(), Value::from(logging::now_ms()));

        let saved = StoredAccount::from_map(record);
        let mut merged = others;
        merged.push(saved.clone());
        state.accounts = merged;
        self.save(&state, &_guard)?;
        logging::log(
            "[Accounts]",
            &format!(
                "✅ 账号已保存: {}（{}…，{}，优先级 {}）",
                record_name,
                truncate_chars(&uid, 8),
                edition.label,
                priority
            ),
        );
        Ok(self.to_public_account(&saved))
    }

    /// 删除账号（不存在 → 404）
    pub fn remove_account(&self, id: &str) -> Result<(), AccountStoreError> {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let before = state.accounts.len();
        state.accounts.retain(|item| item.id() != id);
        if state.accounts.len() == before {
            return Err(AccountStoreError::not_found("账号不存在"));
        }
        self.save(&state, &_guard)
    }

    /// 置顶账号：把它变成转发顺序第一位，也就是「当前账号」。
    ///
    /// 这是「设为当前」的实际动作 —— 优先级唯一的前提下，与其让用户手工去猜
    /// 一个比所有人都小的数，不如直接把目标移到队首再整队连续编号，其余账号
    /// 相对顺序保持不变。目标若处于禁用状态会一并启用：这个动作的语义是
    /// 「现在开始用它」，只置顶不启用只会让人以为没生效。
    pub fn promote_to_front(&self, id: &str) -> Result<Value, AccountStoreError> {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        if !state.accounts.iter().any(|item| item.id() == id) {
            return Err(AccountStoreError::not_found("账号不存在"));
        }

        let mut ordered = state.accounts.clone();
        ordered.sort_by_key(StoredAccount::order_key);
        let mut changes: Vec<String> = Vec::new();

        if let Some(record) = ordered.iter_mut().find(|item| item.id() == id) {
            if !record.enabled() {
                record.set_enabled(true);
                changes.push("已启用".to_string());
            }
        }

        let position = ordered.iter().position(|item| item.id() == id).unwrap_or(0);
        if position > 0 {
            let record = ordered.remove(position);
            ordered.insert(0, record);
            // 整队重新连续编号（而非只改目标账号）：优先级唯一的前提下，
            // 把目标插到队首后必须把其余账号整体让位，否则会撞号
            let mut numbering: Vec<(String, String, i64)> = ordered
                .iter()
                .map(|item| (item.id().to_string(), item.name(), item.priority()))
                .collect();
            let assignments = renumber_consecutively(&mut numbering);
            // 同上：只回写变化的账号（内置默认值等于自身时不产生字段）
            for assignment in &assignments {
                if let Some(record) = ordered.iter_mut().find(|item| item.id() == assignment.id) {
                    record.set_priority(assignment.to);
                }
            }
            changes.push(format!(
                "优先级 → {}（置顶）",
                ordered.first().map(StoredAccount::priority).unwrap_or(DEFAULT_PRIORITY)
            ));
        }
        if changes.is_empty() {
            // 「已是当前账号，无需切换」：Node 版的路由会把 list 补进这个结果，
            // 所以这里也带上 —— 前端拿到响应后可以直接用同一份快照刷新列表
            return Ok(json!({
                "id": id,
                "changed": false,
                "reason": "已是当前账号",
                "currentAccountId": Self::pick_current(&state.accounts)
                    .map(|record| Value::String(record.id().to_string()))
                    .unwrap_or(Value::Null),
                "list": self.snapshot(&state),
            }));
        }

        let display_name = ordered
            .iter()
            .find(|item| item.id() == id)
            .map(StoredAccount::name)
            .unwrap_or_default();
        if let Some(record) = ordered.iter_mut().find(|item| item.id() == id) {
            record.set_updated_at(logging::now_ms());
        }
        state.accounts = ordered;
        self.save(&state, &_guard)?;
        let current_id = Self::pick_current(&state.accounts).map(|record| record.id().to_string());
        logging::log(
            "[Accounts]",
            &format!("⬆️  账号已置顶: {display_name}（{}）", changes.join("，")),
        );
        let mut result = json!({
            "id": id,
            "changed": true,
            "changes": changes,
            "currentAccountId": current_id,
        });
        if let Some(object) = result.as_object_mut() {
            object.insert("list".to_string(), self.snapshot(&state));
        }
        Ok(result)
    }

    /// 修改账号的运营属性：备注名 / 优先级 / 启用状态 / 代理。
    ///
    /// 只处理显式传入的字段（patch 语义），未传字段保持不变。
    /// 返回 `(account, changes)`，changes 为字段变化列表（供日志与前端提示）。
    pub fn update_account(
        &self,
        id: &str,
        patch: &Value,
    ) -> Result<(Value, Vec<String>), AccountStoreError> {
        let patch = patch
            .as_object()
            .cloned()
            .ok_or_else(|| AccountStoreError::bad_request("请求内容必须是 JSON 对象"))?;
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let index = state
            .accounts
            .iter()
            .position(|item| item.id() == id)
            .ok_or_else(|| AccountStoreError::not_found("账号不存在"))?;

        let changes = Self::apply_patch(&mut state, index, &patch)?;
        let account = self.to_public_account(&state.accounts[index]);
        if changes.is_empty() {
            return Ok((account, changes));
        }
        let name = state.accounts[index].name();
        state.accounts[index].set_updated_at(logging::now_ms());
        self.save(&state, &_guard)?;
        logging::log(
            "[Accounts]",
            &format!("✏️  账号已更新: {name}（{}）", changes.join("，")),
        );
        Ok((account, changes))
    }

    /// 把 patch 应用到单条记录（纯内存操作：不落盘、不动 state 之外的东西）。
    ///
    /// `updateAccount` 与 `batchUpdate` 共用这里，保证单账号与批量的语义完全一致。
    /// 非法取值按错误抛出，调用方决定是整体失败还是记入 failed。
    fn apply_patch(
        state: &mut AccountState,
        index: usize,
        patch: &Map<String, Value>,
    ) -> Result<Vec<String>, AccountStoreError> {
        let mut changes = Vec::new();

        if let Some(value) = patch.get("name") {
            let next = match value {
                Value::String(text) => truncate_chars(text.trim(), 100),
                _ => String::new(),
            };
            let current = state.accounts[index].name();
            if next != current {
                if next.is_empty() {
                    return Err(AccountStoreError::bad_request("备注名不能为空"));
                }
                state.accounts[index].set("name", Value::String(next.clone()));
                changes.push(format!("备注名 → {next}"));
            }
        }

        if let Some(value) = patch.get("priority") {
            let current = state.accounts[index].priority();
            let next = normalize_priority(Some(value), current);
            if next != current {
                let entries: Vec<(String, String, i64)> = state
                    .accounts
                    .iter()
                    .map(|item| (item.id().to_string(), item.name(), item.priority()))
                    .collect();
                if let Some((_, holder_name)) =
                    find_priority_holder(&entries, next, Some(state.accounts[index].id()))
                {
                    return Err(AccountStoreError::new(
                        format!(
                            "优先级 {next} 已被账号「{holder_name}」占用，请换一个（优先级需全局唯一）"
                        ),
                        409,
                    ));
                }
                state.accounts[index].set_priority(next);
                changes.push(format!("优先级 → {next}"));
            }
        }

        if let Some(value) = patch.get("enabled") {
            let next = !matches!(value, Value::Bool(false));
            let current = state.accounts[index].enabled();
            if next != current {
                state.accounts[index].set_enabled(next);
                changes.push(if next { "已启用".to_string() } else { "已禁用".to_string() });
            }
        }

        if let Some(value) = patch.get("proxy") {
            let next = crate::server::core::proxies::normalize_account_proxy(value)
                .map_err(|error| AccountStoreError::new(error.message, error.status_code))?
                .unwrap_or(Value::Null);
            // Node 用 JSON.stringify 比较：形状相同（含键顺序）才算未变化。
            // serde_json 的序列化键序由插入序决定，与 JS 的对象字面量序一致。
            let before = value_or_nullish(state.accounts[index].get("proxy"), Value::Null);
            if next != before {
                state.accounts[index].set_proxy(next.clone());
                let label = if next.is_null() {
                    "代理 → 无代理（直连）".to_string()
                } else {
                    let described = describe_account_proxy(Some(&next));
                    let label = described
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("已设置");
                    format!("代理 → {label}")
                };
                changes.push(label);
            }
        }

        Ok(changes)
    }

    /// 沿优先级顺序把账号上移/下移一位（与相邻账号交换优先级数值）。
    ///
    /// 优先级唯一的前提下，交换能让用户点两下就完成重排序，不用手工去猜一个
    /// 空闲数字 —— 这是唯一性约束下的主要调整入口。
    /// 已在队首/队尾时返回 `{moved:false}`，不算错误。
    pub fn move_account(&self, id: &str, direction: &str) -> Result<Value, AccountStoreError> {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        if !state.accounts.iter().any(|item| item.id() == id) {
            return Err(AccountStoreError::not_found("账号不存在"));
        }
        let mut ordered = state.accounts.clone();
        ordered.sort_by_key(StoredAccount::order_key);
        let index = ordered.iter().position(|item| item.id() == id).unwrap_or(0);
        let down = direction == "down";
        let target = if down {
            index as i64 + 1
        } else {
            index as i64 - 1
        };
        if target < 0 || target >= ordered.len() as i64 {
            // Node 版的路由会给这个结果补上 list（`{ ...result, list }`），
            // 所以这里也带上 —— 前端拿到的响应形状与成功分支一致
            return Ok(json!({
                "moved": false,
                "reason": if down { "已是最后一位" } else { "已是第一位" },
                "list": self.snapshot(&state),
            }));
        }

        let target = target as usize;
        let mine = ordered[index].priority();
        let theirs = ordered[target].priority();
        // 只交换这两个账号的优先级；文件顺序保持原样 —— Node 版同样只改这两个
        // 对象的字段（state.accounts 的顺序不动），所以 accounts.json 的行序不会
        // 因为一次「上移/下移」被整体重排（用户手工编辑时不会被搅乱）。
        let now = logging::now_ms();
        let mine_id = ordered[index].id().to_string();
        let other_id = ordered[target].id().to_string();
        if let Some(record) = state.accounts.iter_mut().find(|item| item.id() == mine_id) {
            record.set_priority(theirs);
            record.set_updated_at(now);
        }
        if let Some(record) = state.accounts.iter_mut().find(|item| item.id() == other_id) {
            record.set_priority(mine);
            record.set_updated_at(now);
        }
        let mine_name = ordered[index].name();
        let theirs_name = ordered[target].name();
        self.save(&state, &_guard)?;
        logging::log(
            "[Accounts]",
            &format!("↕️  优先级交换: {mine_name}(P{mine}) ⇄ {theirs_name}(P{theirs})"),
        );
        Ok(json!({
            "moved": true,
            "id": id,
            "newPriority": theirs,
            "swappedWith": {
                "id": other_id,
                "name": theirs_name,
                "newPriority": mine,
            },
            "list": self.snapshot(&state),
        }))
    }

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
        let mut state = self.load(&_guard);
        let mut ok: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        let mut dirty = false;
        for id in ids {
            let Some(index) = state.accounts.iter().position(|item| item.id() == *id) else {
                failed.push(json!({ "id": id, "error": "账号不存在" }));
                continue;
            };
            let name = state.accounts[index].name();
            match Self::apply_patch(&mut state, index, &allowed) {
                Ok(changes) => {
                    if !changes.is_empty() {
                        state.accounts[index].set_updated_at(logging::now_ms());
                        dirty = true;
                    }
                    ok.push(json!({ "id": id, "name": name, "changes": changes }));
                }
                Err(error) => {
                    failed.push(json!({ "id": id, "name": name, "error": error.message }));
                }
            }
        }
        if dirty {
            self.save(&state, &_guard)?;
        }
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
        Ok(json!({ "ok": ok, "failed": failed, "list": self.snapshot(&state) }))
    }

    /// 批量删除账号。不存在的 id 记入 failed 而不是整体报错 —— 用户重复点删除
    /// （或列表已刷新）不会看到莫名其妙的失败。当前账号由优先级派生，
    /// 删掉它之后自动变成下一个可用账号，无需额外处理。
    pub fn batch_remove(&self, ids: &[String]) -> Result<Value, AccountStoreError> {
        if ids.is_empty() {
            return Err(AccountStoreError::bad_request("缺少要删除的账号 id"));
        }
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let mut removed: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        for id in ids {
            match state.accounts.iter().find(|item| item.id() == *id) {
                Some(record) => removed.push(json!({ "id": id, "name": record.name() })),
                None => failed.push(json!({ "id": id, "error": "账号不存在" })),
            }
        }
        if removed.is_empty() {
            return Ok(json!({ "removed": removed, "failed": failed, "list": self.snapshot(&state) }));
        }
        let wanted: Vec<&String> = ids.iter().collect();
        state
            .accounts
            .retain(|item| !wanted.iter().any(|id| id.as_str() == item.id()));
        self.save(&state, &_guard)?;
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
        Ok(json!({ "removed": removed, "failed": failed, "list": self.snapshot(&state) }))
    }
}
