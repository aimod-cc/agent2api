//! 账号增删改查（从 store.rs 拆出，单文件行数约定）。
//!
//! 覆盖 Node 版 workbuddy-account-store.mjs 的写入侧全集：
//!   addAccount / removeAccount / promoteToFront / updateAccount(applyAccountPatch)
//!   / moveAccount / batchUpdate / batchRemove
//!
//! 三条不变量在这里落地，改代码前务必读 `super::mod` 的头部说明：
//!   1. **优先级全局唯一**（所有提供商共用一条队列）：写入侧遇到冲突一律 409，
//!      由用户显式选一个空闲值；冲突判定与号段分配都在全部账号上算
//!      （改造后各由一次投影列查询完成：`sql::priority_holder` /
//!      `sql::priorities_except`），整队重编号见 `renumber_consecutively`；
//!   2. **未知字段全量保留**：记录是 JSON 对象（`StoredAccount`），只改自己要改的键；
//!   3. **持锁期间不做网络请求**：本文件全是数据库读写，没有任何 await。
//!
//! ── 写入粒度：单条操作只碰一行（本切片的改造）────────────────
//! 改造前每条路径都是「load 全量 → 改内存里那一份 → save 全量」。现在的分布是：
//!   - 读：`record_by_id` / `records_for_provider` 按需取（不再全表解析）；
//!   - 判重与号段：`priority_holder` / `priorities_except`
//!     直接在投影列上查（不再把全部记录的 JSON 全解析出来数一遍）；
//!   - 写：`sql::put` / `sql::update_in_place` / `sql::delete` 单行落地。
//!
//! **两条例外**（都要整队或全局排序，见各自函数的说明）：
//!   - `promote_to_front`：置顶后其余账号必须整体让位，读全量算一遍新编号；
//!   - `move_account`：相邻位可能是**另一家**的账号（全局一条队列），
//!     必须按全局优先级序把它找出来。
//! 两者的**读**是全量（语义要求），但**写**仍只落真正变了的那一两行
//! （`move_account` 两次 UPDATE 在一个事务里；`promote_to_front` 经 `save`，
//! 而 `save` 是按差异写的，编号没变的行一条 UPDATE 都不发）。
//!
//! 新增账号的 `provider` 字段一律写默认 provider（`DEFAULT_PROVIDER_ID`）：
//! 本函数只服务 **workbuddy 那一条添加路径**（其余各家的添加入口在
//! `raccoon_accounts.rs`，以及 catpaw / autoclaw / cline / qoder 模块），且
//! **不读取 payload 里的 provider** —— 免得前端误传一个未知 provider 就把
//! 账号写进没人认的组里。分派发生在 `api::accounts::add_account`：
//! 先按注册表把 payload 的 provider 换算成 kind，再穷举分到各家的入口。

use serde_json::{json, Map, Value};

use crate::server::core::account_store::priority::{
    next_free_priority, normalize_priority, renumber_consecutively, DEFAULT_PRIORITY,
};
use crate::server::core::account_store::sql;
use crate::server::core::account_store::state::StoredAccount;
use crate::server::core::account_store::store::{AccountStore, AccountStoreError};
use crate::server::core::account_store::store_util::{
    js_string, number_or, object_or_empty, optional_text, pick_token, token_tail_of, truncate_chars,
    value_or, value_or_nullish,
};
use crate::server::core::account_store::MAX_TOKEN_LENGTH;
use crate::server::core::endpoints::resolve_edition;
use crate::server::core::providers::DEFAULT_PROVIDER_ID;
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
        let id = format!("user-{uid}");
        // 只读这一行（不再读全量）：既有记录是「更新」还是「新建」全靠它分支
        let existing = self.record_by_id(&_guard, &id);
        // ── 撞 id 保护（Agent2API W3-T4）────────────────────────────
        // 小浣熊账号的 id 也是 `user-<数字>` 形态（`add_raccoon_account` 与
        // 旧数据导入都这么生成），而它的 userId 可能正好与某个 workbuddy 账号的
        // uid 相同（两家 id 空间独立）。若这里沿用既有记录，这条 workbuddy 会话
        // 就会把**小浣熊账号**整条覆写成 workbuddy 记录 —— 账号与凭证一起丢。
        // 因此撞到一个**别的 provider** 的 id 时直接报错，让用户先处理那条记录。
        if let Some(existing) = existing.as_ref() {
            let existing_provider = existing.provider();
            if existing_provider != DEFAULT_PROVIDER_ID {
                return Err(AccountStoreError::bad_request(format!(
                    "账号 id「{id}」已被{}账号占用，无法用同一 uid 添加 workbuddy 账号（请先处理那个账号）",
                    existing_provider
                )));
            }
        }
        // 本账号所属 provider：更新既有记录时**沿用原值**，新建时用默认值。
        // 注：provider 字段缺失的历史记录由 `StoredAccount::provider()` 兜底成
        // workbuddy，所以这里不会读到空串。
        // 小浣熊的添加路径在 `raccoon_accounts::add_raccoon_account`（键名与
        // 校验都不同），本函数只服务 workbuddy；撞 id 的反向情况（先有
        // workbuddy 账号、再添加同 userId 的小浣熊账号）由那条路径的
        // 「保留既有未知字段」策略兜住：它不会把 workbuddy 记录改写成 raccoon。
        let provider = existing
            .as_ref()
            .map(StoredAccount::provider)
            .unwrap_or_else(|| DEFAULT_PROVIDER_ID.to_string());

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
        // 新账号默认排在末尾，避免凭空插队改变现有转发顺序；显式指定则校验唯一。
        // 号段与冲突看全部账号（全局一条队列，见模块头）——两者都只在投影列
        // `priority` 上做，不解析任何记录的 JSON：
        //   号段取 `priorities_except(&id)`（旧实现的 `others` 的优先级），
        //   冲突判定走 `priority_holder`（一条 SQL 换掉「读全量 + 内存里 find」）。
        let existing_priority = existing.as_ref().map(StoredAccount::priority);
        let priority = match payload_object.get("priority") {
            Some(value) => normalize_priority(
                Some(value),
                existing_priority.unwrap_or(DEFAULT_PRIORITY),
            ),
            None => match existing_priority {
                Some(value) => value,
                None => {
                    let used = self.with_conn(&_guard, |conn| sql::priorities_except(conn, &id))?;
                    next_free_priority(&used)
                }
            },
        };
        if let Some(holder_name) = self.with_conn(&_guard, |conn| {
            sql::priority_holder(conn, priority, &id)
        })? {
            return Err(AccountStoreError::new(
                format!(
                    "优先级 {priority} 已被账号「{holder_name}」占用，请换一个\
                     （优先级全局唯一）"
                ),
                409,
            ));
        }

        let mut record = Map::new();
        record.insert("id".to_string(), Value::String(id.clone()));
        // provider 是**已知字段**：写回时必须带上（不变量「未知字段全量保留」
        // 的同类要求 —— 已知字段同样不能在重建记录时丢掉）
        record.insert("provider".to_string(), Value::String(provider.clone()));
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
        // 单行落地：`put` 是 DELETE + INSERT，于是「更新既有记录时它在列表里
        // 往后挪」这个旧行为（`retain` + `push`）原样保留（见 `sql::put`）。
        self.with_conn(&_guard, |conn| sql::put(conn, &saved))?;
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
        Ok(self.public_account(&saved))
    }

    /// 删除账号（不存在 → 404；受保护的账号 → 400）。
    ///
    /// ── 桌面端账号**可以**删除（W3-T4 曾禁止，后放开）─────────────
    /// 小浣熊 / CatPaw / AutoClaw 的桌面端账号是「桌面端登录态文件」的**镜像**
    /// （架构文档 §3.2 / §9 / §10），早期实现把它判为不可删。那个保护是过度
    /// 设计：删的是**这条账号记录**，我们从不写也不删客户端自己的登录态文件，
    /// 所以删除安全且可逆（再点一次「导入桌面端登录态」就能加回来），
    /// 而「不可删」把用户锁死了 —— 他只能禁用，禁用后记录仍占着列表与优先级
    /// 序号。现在三家都不再保护（判定出口 `protected_from_removal` 保留为
    /// 扩展点，当前恒返回 None）。
    ///
    /// ── 删除后必须作废该账号的 CatPaw 会话映射（W5-T-d4）───────
    /// `conversationId` 属于**上游账号上下文**：账号没了，它建立的会话再也不能
    /// 续接（续接会打到别人的会话或直接报错）。原项目在账号切换时整表清
    /// （`notifySwitch` → `clearClientToolSessions`），这里按账号精细作废。
    /// provider 要在**删除之前**取好：记录删掉之后回读只能得到 None。
    pub fn remove_account(&self, id: &str) -> Result<(), AccountStoreError> {
        if let Some(reason) = self.protected_from_removal(id) {
            return Err(AccountStoreError::new(reason, 400));
        }
        let _guard = self.guard();
        // provider 要在**删除之前**取好（记录删掉之后回读只能得到 None）
        let provider = self
            .record_by_id(&_guard, id)
            .map(|record| record.provider())
            .unwrap_or_default();
        // 单行删除，`false` 表示本来就没有这一行 → 404（与旧实现数数组长度的
        // 判据等价：删之前找不到这个 id）
        let removed = self.with_conn(&_guard, |conn| sql::delete(conn, id))?;
        if !removed {
            return Err(AccountStoreError::not_found("账号不存在"));
        }
        // 落库已完成，账号锁在这里放开：注册表作废是另一把锁的操作，
        // 两者不必（也不该）嵌套（见 `invalidate_catpaw_sessions` 的说明）
        drop(_guard);
        self.invalidate_catpaw_sessions(id, &provider);
        Ok(())
    }

    /// 把账号移到全局队列第一位，其余账号的相对顺序保持不变。
    ///
    /// 置顶只调整优先级，不改变启用状态；禁用账号仍不参与转发。
    /// `currentAccountId` 保留「首个启用且有可用凭证的账号」的语义，
    /// 不一定指向本次置顶的账号。
    ///
    /// ── 为什么这一条必须读全量、写多行 ──────────────────────────
    /// 其余单条操作都只碰一两行，这一条不行：优先级唯一的前提下，把目标插到
    /// 队首后**其余账号必须整体让位**，否则会撞号（`renumber_consecutively`
    /// 给整队重新连续编号）。所以它按语义要求把全部账号读出来算一遍 ——
    /// 但**写**仍然只写真正变了的那些行：`save_state` 是按差异写的，
    /// 编号没变的账号一条 UPDATE 都不会发（例如 [0,1,2] 置顶末位时
    /// 2→0/0→1/1→2 三行都变，而 [0,5,9] 置顶末位时只有一行变）。
    pub fn promote_to_front(&self, id: &str) -> Result<Value, AccountStoreError> {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        if !state.accounts.iter().any(|item| item.id() == id) {
            return Err(AccountStoreError::not_found("账号不存在"));
        }

        let mut ordered = state.accounts.clone();
        ordered.sort_by_key(StoredAccount::order_key);
        let mut changes: Vec<String> = Vec::new();

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
            return Ok(json!({
                "id": id,
                "changed": false,
                "reason": "已在全局队列第一位",
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
        // 按优先级序交给保存层：`save_state` 只对 data 真的变了的行发 UPDATE，
        // 于是「不重写没变的账号」这条收益在这里同样成立。
        // 物理行序（列表顺序）**不动** —— 界面与选路都自己按优先级排，
        // 没有消费方观察数组顺序（推演见 `sql.rs` 模块头「顺序」一节）。
        state.accounts = ordered;
        if let Some(record) = state.accounts.iter_mut().find(|item| item.id() == id) {
            record.set_updated_at(logging::now_ms());
        }
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
    ///
    /// ── 返回的 `account` 用的是**改动前**的 updatedAt（易错点）──
    /// 改造前就是这么做的：`public_account` 在 `set_updated_at` **之前**取值，
    /// 所以响应里的 `updatedAt` 是旧值（下一次读列表才看到新的）。这里逐字保留
    /// —— 前端可能拿它做「这一条是不是刚改过」的判断，顺手「修正」会让响应
    /// 形状与 Node 版分叉。
    ///
    /// ── 禁用时作废 CatPaw 的会话映射（W5-T-d4）──────────────────
    /// 「禁用」的语义是「不再用它转发」。而注册表里属于它的 conversationId 是
    /// 上游账号上下文里的对象：继续留着，用户重新启用后那一轮的增量续接会
    /// 用一条可能早已过期的 conversation（上游 TTL / 账号侧状态都变过）。
    /// 因此禁用（`enabled` 变 false）与删除、重新导入一样要作废该账号的映射。
    /// 启用**不作废**（那会把用户刚恢复的账号的历史一起丢掉，而续接本身是安全的）。
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
        // 只读目标那一行：apply_patch 只改这一条，优先级冲突另走一次投影列查询
        // （旧实现为此要把全部账号读进内存再 find 一遍）
        let mut record = self
            .record_by_id(&_guard, id)
            .ok_or_else(|| AccountStoreError::not_found("账号不存在"))?;
        let provider = record.provider();
        let was_enabled = record.enabled();
        let changes = Self::apply_patch(&mut record, &patch, |next| {
            self.with_conn(&_guard, |conn| sql::priority_holder(conn, next, id))
        })?;
        let account = self.public_account(&record);
        if changes.is_empty() {
            return Ok((account, changes));
        }
        let now_disabled = !record.enabled();
        let name = record.name();
        record.set_updated_at(logging::now_ms());
        // 原地更新（物理位置不动）：旧实现在下标上改字段，数组顺序本来就不变 ——
        // 界面上的行序是按优先级排的，不该因为一次「改备注名」而跳动
        self.with_conn(&_guard, |conn| sql::update_in_place(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!("✏️  账号已更新: {name}（{}）", changes.join("，")),
        );
        if was_enabled && now_disabled {
            // 账号锁先放开再动作注册表（两把锁不嵌套，见 `remove_account` 的说明）
            drop(_guard);
            self.invalidate_catpaw_sessions(id, &provider);
        }
        Ok((account, changes))
    }

    /// 把 patch 应用到**单条记录**（纯内存操作：不落库、不动记录之外的东西）。
    ///
    /// `update_account` 与 `batch_update` 共用这里，保证单账号与批量的语义完全
    /// 一致。非法取值按错误抛出，调用方决定是整体失败还是记入 failed。
    ///
    /// ── 签名里的 `find_holder` 是什么（本切片的改造）────────────
    /// 优先级冲突判定要「在**全部**账号里找有没有别人占着这个号」，而本函数
    /// 手里只有一条记录 —— 改造前它拿的是整份 `AccountState`，于是两个调用点
    /// 都必须先把全部账号读进内存。现在把那次查询做成回调：调用方给出「按候选
    /// 优先级找占位者名字」的实现（单条走一次 SQL，批量两条也各走一次），
    /// 冲突判定与 409 文案仍**只在本函数里写一遍**。
    ///
    /// 回调只在优先级**确实要变**时才被调用（值没变就不用查），因此「patch 只
    /// 改备注名」这类常见情况下一次多余查询都不会发。
    ///
    /// `pub(crate)`：批量操作在 `store_batch.rs`（同一 `impl` 的另一个分块），
    /// 私有方法对**兄弟模块**不可见 —— 拆分时把可见性放宽到这里。
    pub(crate) fn apply_patch(
        record: &mut StoredAccount,
        patch: &Map<String, Value>,
        find_holder: impl FnOnce(i64) -> Result<Option<String>, AccountStoreError>,
    ) -> Result<Vec<String>, AccountStoreError> {
        let mut changes = Vec::new();

        if let Some(value) = patch.get("name") {
            let next = match value {
                Value::String(text) => truncate_chars(text.trim(), 100),
                _ => String::new(),
            };
            let current = record.name();
            if next != current {
                if next.is_empty() {
                    return Err(AccountStoreError::bad_request("备注名不能为空"));
                }
                record.set("name", Value::String(next.clone()));
                changes.push(format!("备注名 → {next}"));
            }
        }

        if let Some(value) = patch.get("priority") {
            let current = record.priority();
            let next = normalize_priority(Some(value), current);
            if next != current {
                // 冲突看全部账号（优先级全局唯一，见模块头）。
                // 排除自己：改自己的优先级不该被自己的旧值挡住
                if let Some(holder_name) = find_holder(next)? {
                    return Err(AccountStoreError::new(
                        format!(
                            "优先级 {next} 已被账号「{holder_name}」占用，请换一个\
                             （优先级全局唯一）"
                        ),
                        409,
                    ));
                }
                record.set_priority(next);
                changes.push(format!("优先级 → {next}"));
            }
        }

        if let Some(value) = patch.get("enabled") {
            let next = !matches!(value, Value::Bool(false));
            let current = record.enabled();
            if next != current {
                record.set_enabled(next);
                changes.push(if next { "已启用".to_string() } else { "已禁用".to_string() });
            }
        }

        if let Some(value) = patch.get("proxy") {
            let next = crate::server::core::proxies::normalize_account_proxy(value)
                .map_err(|error| AccountStoreError::new(error.message, error.status_code))?
                .unwrap_or(Value::Null);
            // Node 用 JSON.stringify 比较：形状相同（含键顺序）才算未变化。
            // serde_json 的序列化键序由插入序决定，与 JS 的对象字面量序一致。
            let before = value_or_nullish(record.get("proxy"), Value::Null);
            if next != before {
                record.set_proxy(next.clone());
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
    ///
    /// 相邻是**全局队列**里的相邻：四家账号混排在同一条队里，上一位 / 下一位
    /// 可能属于另一家，交换后两家的相对顺序随之改变 —— 这正是全局队列的语义。
    pub fn move_account(&self, id: &str, direction: &str) -> Result<Value, AccountStoreError> {
        let _guard = self.guard();
        // 相邻位要靠**全局优先级序**找出来：交换对象可能是另一家账号
        // （全局一条队列），所以这里必须把全部账号读出来排一次 —— 语义要求，
        // 与 `promote_to_front` 同理。但**写**只碰交换的那两行。
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
        let mine_id = ordered[index].id().to_string();
        let other_id = ordered[target].id().to_string();
        let mine_name = ordered[index].name();
        let theirs_name = ordered[target].name();
        // 只交换这两个账号的优先级；物理顺序保持原样 —— Node 版同样只改这两个
        // 对象的字段（state.accounts 的顺序不动），所以列表的行序不会因为一次
        // 「上移/下移」被整体重排。
        //
        // 两次 UPDATE 放在**一个事务**里：交换是「两行同时改」的原子操作，
        // 中途失败会留下两条记录优先级相同的中间态（破坏唯一性不变量）。
        let now = logging::now_ms();
        let mut mine_record = ordered[index].clone();
        let mut other_record = ordered[target].clone();
        mine_record.set_priority(theirs);
        mine_record.set_updated_at(now);
        other_record.set_priority(mine);
        other_record.set_updated_at(now);
        self.with_conn_mut(&_guard, |conn| {
            let tx = conn.transaction()?;
            sql::update_in_place(&tx, &mine_record)?;
            sql::update_in_place(&tx, &other_record)?;
            tx.commit()
        })?;
        // 快照用同一份内存状态（把那两行的新值写回去），不再多读一次库：
        // 其余账号这一轮没有被动过，库里的值与手里这份一致。
        for record in state.accounts.iter_mut() {
            if record.id() == mine_id {
                record.set_priority(theirs);
                record.set_updated_at(now);
            } else if record.id() == other_id {
                record.set_priority(mine);
                record.set_updated_at(now);
            }
        }
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
}
