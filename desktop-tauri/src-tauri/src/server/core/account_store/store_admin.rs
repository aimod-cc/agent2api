//! 凭证回写、限额标记与启动迁移（从 store.rs 拆出，单文件行数约定）。
//!
//!   update_account_tokens  账号刷新成功后回写新 token
//!   mark_rate_limited      记录账号×模型的限额状态（上游 429 / code 6004）
//!   clear_rate_limit       清除某模型的限额记录
//!   import_legacy_session  旧版单账号 auth.json 迁移（仅账号列表为空时）
//!   clear_all_rate_limits  清除账号全部模型的限额记录（账号页「全部清除」）
//!   migrate_startup        启动时一次性数据迁移（provider 惰性补齐 + 优先级全局化
//!                          + 去重 + Cline 拆池改名）
//!   renumber_globally      优先级重编号内核（**全局一条队列**，migrate_startup 私有）
//!
//! 这些方法都是「低频、写入型」操作：放在单独文件是为了让 CRUD 文件聚焦在
//! 用户可见的账号操作上，避免两者混在一起时看不清哪些是启动期做的事。
//!
//! ── 几条迁移为什么合成一次落盘 ──────────────────────────────
//! `provider` 惰性补齐（架构文档 §3.2）、优先级去重（原 `migrate_priorities`）
//! 与 Cline 拆池改名都在启动时、都在同一份 state 上改字段。若各自读盘一次、
//! 写盘一次，后面那条迁移读到的就是前面写过的文件 —— 结果没错，但会多几次
//! 全文件重写，而且「补 provider 时把已经去重过的优先级又读成并列」这类交叉
//! 影响不容易看清。因此它们由 `migrate_startup` 串起来：**一次读、一次改、
//! 一次写**。

use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::account_store::priority::{
    normalize_priority_value, renumber_consecutively, PriorityAssignment,
};
use crate::server::core::account_store::state::{
    json_number, AccountState, StoredAccount, PRIORITY_SCOPE_GLOBAL,
};
use crate::server::core::account_store::store::AccountStore;
use crate::server::core::account_store::store_util::{token_tail_of, truncate_chars};
use crate::server::core::providers::{provider_index, DEFAULT_PROVIDER_ID};
use crate::server::logging;

impl AccountStore {
    // ─── 凭证回写与限额标记 ──────────────────────────────────

    /// 账号刷新成功后回写新 token（账号不存在返回 false）
    pub fn update_account_tokens(
        &self,
        id: &str,
        access_token: Option<&str>,
        refresh_token: Option<&str>,
        expires_at: Option<f64>,
        refresh_expires_at: Option<f64>,
    ) -> bool {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let Some(index) = state.accounts.iter().position(|item| item.id() == id) else {
            return false;
        };
        if let Some(token) = access_token.filter(|value| !value.is_empty()) {
            state.accounts[index].set("accessToken", Value::String(token.to_string()));
            state.accounts[index].set("tokenTail", Value::String(token_tail_of(token)));
        }
        if let Some(token) = refresh_token.filter(|value| !value.is_empty()) {
            state.accounts[index].set("refreshToken", Value::String(token.to_string()));
        }
        if let Some(value) = expires_at.filter(|value| *value > 0.0) {
            state.accounts[index].set("expiresAt", json_number(value));
        }
        if let Some(value) = refresh_expires_at.filter(|value| *value > 0.0) {
            state.accounts[index].set("refreshExpiresAt", json_number(value));
        }
        state.accounts[index].set_updated_at(logging::now_ms());
        self.save(&state, &_guard).is_ok()
    }

    /// 记录账号对某模型的限额状态（上游 429 / code 6004）。
    ///
    /// resetAt 为恢复时间戳；缺失时给 10 分钟兜底冷却，避免短时间内反复撞限额。
    ///
    /// ── 冷却键的最终形态：provider × 账号 × 模型（Agent2API W2b-T3 确认）──
    /// 落盘位置是 `accounts[i].rateLimits[model] = {status, code, resetAt, message, at}`，
    /// 即**冷却键挂在账号记录内部**，而每条账号记录只属于一个 provider
    /// （`record.provider()` 是单值）。所以「provider 维度」已经天然成立，
    /// **不需要改数据结构**：`accounts[i]` 这一个下标就同时确定了 provider 与账号，
    /// 加上 `rateLimits` 里的模型名，键即 `(provider, account_id, model)`。
    ///
    /// 选路时候选集合跨提供商（全局队列），但限额判定仍是「这条账号记录对这个
    /// 模型」—— 记录内的键天然不会串到别的账号上。
    pub fn mark_rate_limited(
        &self,
        id: &str,
        model: &str,
        status: i64,
        code: Option<i64>,
        reset_at: Option<f64>,
        message: &str,
    ) -> Option<Value> {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let index = state.accounts.iter().position(|item| item.id() == id)?;
        let now = logging::now_ms();
        let reset = match reset_at {
            Some(value) if value.is_finite() && value > now as f64 => value,
            _ => now as f64 + 10.0 * 60.0 * 1000.0,
        };
        let entry = json!({
            "status": if status == 0 { 429 } else { status },
            "code": code.map(Value::from).unwrap_or(Value::Null),
            "resetAt": json_number(reset),
            "message": truncate_chars(message, 300),
            "at": now,
        });
        let mut limits = match state.accounts[index].get("rateLimits") {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        limits.insert(model.to_string(), entry.clone());
        state.accounts[index].set("rateLimits", Value::Object(limits));
        state.accounts[index].set_updated_at(now);
        if self.save(&state, &_guard).is_err() {
            return None;
        }
        Some(entry)
    }

    /// 清除账号对某模型的限额记录（该模型请求成功时调用）。
    ///
    /// 冷却键形态见 `mark_rate_limited`：键在账号记录内，provider 由账号唯一确定。
    pub fn clear_rate_limit(&self, id: &str, model: &str) -> bool {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let Some(index) = state.accounts.iter().position(|item| item.id() == id) else {
            return false;
        };
        let Some(Value::Object(limits)) = state.accounts[index].get("rateLimits") else {
            return false;
        };
        if !limits.contains_key(model) {
            return false;
        }
        let mut limits = limits.clone();
        limits.remove(model);
        if limits.is_empty() {
            state.accounts[index].remove("rateLimits");
        } else {
            state.accounts[index].set("rateLimits", Value::Object(limits));
        }
        state.accounts[index].set_updated_at(logging::now_ms());
        self.save(&state, &_guard).is_ok()
    }

    /// 清除账号**全部**模型的限额记录（账号页限流明细的「全部清除」）。
    /// 返回被清掉的模型数；账号不存在或本来就没有记录返回 0。
    pub fn clear_all_rate_limits(&self, id: &str) -> usize {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let Some(index) = state.accounts.iter().position(|item| item.id() == id) else {
            return 0;
        };
        let count = match state.accounts[index].get("rateLimits") {
            Some(Value::Object(limits)) => limits.len(),
            _ => 0,
        };
        if count == 0 {
            return 0;
        }
        state.accounts[index].remove("rateLimits");
        state.accounts[index].set_updated_at(logging::now_ms());
        if self.save(&state, &_guard).is_err() {
            return 0;
        }
        count
    }

    /// 记录账号**今天已签到**（登录页那枚按钮据此置灰）。
    ///
    /// `at` 是这次签到成功的毫秒时间戳。判定「今天签过没」由**读侧**按自然日比
    /// （`checkinAt` 落在今天即算签过），所以这里只负责如实落时间 —— 不需要
    /// 在写入时判重，跨过 0 点后同一个字段自然失效，不必任何定时器去重置。
    ///
    /// 与限额标记同族：都是「账号级的事实、低频写、失败不致命」。调用方是签到
    /// 链路，写盘失败**不**影响签到结果本身（上游那边积分已经领到了），所以返回
    /// bool 而不是 Result —— 调用方拿到 false 时最多记一条日志，绝不因此把
    /// 一次成功的签到报成失败。
    pub fn mark_checkin(&self, id: &str, at: i64) -> bool {
        let _guard = self.guard();
        let mut state = self.load(&_guard);
        let Some(index) = state.accounts.iter().position(|item| item.id() == id) else {
            return false;
        };
        state.accounts[index].set_checkin_at(at);
        // 注意**不**动 updatedAt：那是「记录被改过」的时间，会显示在账号页的
        // 「更新于 …」上；签到不是用户改配置那种「更新」，写进去会让这一栏
        // 在每天自动签到后集体跳动一次。
        self.save(&state, &_guard).is_ok()
    }

    // ─── 迁移 ────────────────────────────────────────────────

    /// 启动时的一次性数据迁移（**唯一入口**，bootstrap 只调它）。
    ///
    /// 四步在同一份 state 上完成、只落一次盘：
    ///   ① provider 惰性补齐（缺失/空值一律补 workbuddy）；
    ///   ② 优先级作用域从「按 provider 各排各的队」迁到「全局一条队列」——
    ///      文件里没有 `priorityScope: "global"` 标记时做一次：按旧版实际的转发
    ///      顺序（provider 路由优先级 → 组内优先级 → 加入时间）排好，再连续编号，
    ///      于是升级前后实际先用哪个账号完全一致，用户不会感到突变；
    ///   ③ 全局去重（手工编辑出的并列号）；
    ///   ④ Cline 拆池改名（`provider: "cline"` + `pool` 字段 → 两家里的某一家）。
    ///
    /// 顺序上 ④ 必须在 ①②③ **之后**：前几步按 provider 分组时看到的还是旧 id
    /// （一条 `cline` 组），改名后这一组被拆成两条；若反过来先改名，优先级整队
    /// 会在两条新组上各排一遍，得出与升级前不同的相对顺序。放最后则与
    /// 「先整好旧队、再改名」等价 —— 名字变了，号码与顺序原样。
    /// 返回 `{providerAdded, priorityChanged, assignments}` 供启动日志展示。
    pub fn migrate_startup(&self) -> Value {
        let _guard = self.guard();
        let mut state = self.load(&_guard);

        // ① provider 惰性补齐（架构文档 §3.2）：缺失/空值一律补 workbuddy
        let mut provider_added = 0usize;
        for record in state.accounts.iter_mut() {
            if record.provider_explicit().is_none() {
                record.set_provider(DEFAULT_PROVIDER_ID);
                provider_added += 1;
            }
        }
        if provider_added > 0 {
            logging::log(
                "[Accounts]",
                &format!("🏷️  已为 {provider_added} 个历史账号补全 provider 字段（{DEFAULT_PROVIDER_ID}）"),
            );
        }

        // ② / ③ 优先级：首次进入全局队列时按旧顺序整队；之后只在有并列时整队
        let scope_migrated = state.priority_scope.as_deref() != Some(PRIORITY_SCOPE_GLOBAL);
        let assignments = Self::renumber_globally(&mut state, scope_migrated);
        if scope_migrated {
            state.priority_scope = Some(PRIORITY_SCOPE_GLOBAL.to_string());
        }

        // ④ Cline 拆池改名（见 `migrate_cline_accounts`）
        let cline_renamed = Self::migrate_cline_accounts(&mut state);

        if provider_added == 0
            && assignments.is_empty()
            && !scope_migrated
            && cline_renamed == 0
        {
            return json!({
                "providerAdded": 0,
                "priorityChanged": false,
                "assignments": [],
            });
        }
        if let Err(error) = self.save(&state, &_guard) {
            logging::log("[Accounts]", &format!("❌ 启动迁移落盘失败: {error}"));
            return json!({
                "providerAdded": 0,
                "priorityChanged": false,
                "assignments": [],
            });
        }
        if cline_renamed > 0 {
            logging::log(
                "[Accounts]",
                &format!(
                    "🔀 Cline 已拆为 Cline Free / Cline Pass 两家，{cline_renamed} 个账号已按额度池归位"
                ),
            );
        }
        if !assignments.is_empty() {
            logging::log(
                "[Accounts]",
                &format!(
                    "🔢 优先级已{}（{} 个账号重新编号，转发顺序保持不变）",
                    if scope_migrated { "合并为全局一条队列" } else { "去重" },
                    assignments.len()
                ),
            );
        } else if scope_migrated {
            logging::log("[Accounts]", "🔢 优先级已标记为全局一条队列（号码无需调整）");
        }
        json!({
            "providerAdded": provider_added,
            "priorityChanged": !assignments.is_empty(),
            "assignments": assignments
                .iter()
                .map(|item| json!({
                    "id": item.id,
                    "name": item.name,
                    "from": item.from,
                    "to": item.to,
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// Cline 拆池改名：`provider: "cline"` + `pool` 字段 → 两家里的某一家，
    /// 原地改 state（不落盘）。返回改名的记录条数。
    ///
    /// ── 为什么这一步不能省 ──────────────────────────────────────
    /// `"cline"` 这个 provider id **已经不存在**（拆分后是 `cline-free` /
    /// `cline-pass`）。不迁移的后果不是「少一条设置」而是**账号静默消失**：
    /// 每处按 provider 找账号的路径（`accounts_for_provider` 的选路、账号页
    /// 的分组、适配器按 id 找人）都拿不到它 —— 但它还在文件里，用户会看到
    /// 「账号明明在，转发却说没有可用账号」。
    ///
    /// ── 池怎么判 ────────────────────────────────────────────────
    /// 读记录里的 `pool` 字段，判据与口径**逐条沿用拆分前的实现**
    /// （`cline::models::Pool::parse`：`free` → 免费池，其余/缺失/非法 → 订阅池）。
    /// 用同一个函数而不是重写一个 match 是刻意的：池的归属直接决定用户能看到
    /// 哪些模型，迁移前后必须逐字一致，否则升级一次就换了池。
    ///
    /// ── id 为什么要一起改 ──────────────────────────────────────
    /// 旧 id 形如 `cline-usr-…`，**不带池**。若不改，同一个 Cline 账号之后想
    /// 两个池各加一份时，第二次添加会算出同一个 id 而被当成「更新同一条」，
    /// 第二个池永远加不进去。改成 `<provider>-<原后缀>`（`cline-free-usr-…`）
    /// 之后两条才能并存 —— 与 `cline_accounts::record_id` 的拼法一致。
    ///
    /// 改名的判据是**旧 provider 值**而不是 id 前缀：id 是用户可见标识，
    /// 有可能被手工改成别的样子（导出再导入也会重排），只有 provider 字段
    /// 是这一版写进去的事实。
    ///
    /// `pool` 字段在最后**删掉**：池已经是 provider 身份的一部分，留着它
    /// 就多了一处可能与 provider 不一致的事实（而读它的代码已经删光了）。
    ///
    /// 幂等：改完 provider 就不再命中第一条判据，再跑一遍无事发生。
    fn migrate_cline_accounts(state: &mut AccountState) -> usize {
        /// 拆分前的 Cline provider id（只出现在这里）
        const LEGACY_CLINE_ID: &str = "cline";
        let mut renamed = 0usize;
        for record in state.accounts.iter_mut() {
            if record.provider() != LEGACY_CLINE_ID {
                continue;
            }
            let pool = crate::server::core::providers::cline::models::Pool::parse(
                record.get("pool").and_then(Value::as_str).unwrap_or(""),
            );
            let provider = pool.provider_id();
            let id = record.id().to_string();
            // 已经被改成目标前缀的（理论不可能，但比对代价极低）就只换 provider，
            // 不重复叠前缀 —— 否则会拼出 `cline-free-free-…` 这种废物 id
            let new_id = if id.starts_with(&format!("{provider}-")) {
                id
            } else if let Some(rest) = id.strip_prefix(&format!("{LEGACY_CLINE_ID}-")) {
                format!("{provider}-{rest}")
            } else {
                format!("{provider}-{id}")
            };
            record.set_provider(provider);
            record.set("id", Value::String(new_id));
            record.remove("pool");
            renamed += 1;
        }
        renamed
    }

    /// 全局重新连续编号，**原地改 state**（不落盘）。
    ///
    /// `legacy_order = true`（首次从「按家分队」迁到全局）：排序键是旧版的实际转发
    /// 顺序 —— (旧路由优先级, 优先级, 加入时间)。旧路由优先级取 config.json 里
    /// `providerRoute` 的历史值，没有则按注册表顺序（与旧默认值 10/20/30/40 同序）。
    /// 这一档**无论有没有并列都整队**：跨家的号码原本互不相干（各家都有 P100），
    /// 即便碰巧不重号，数值顺序也未必等于旧的实际顺序。
    ///
    /// `legacy_order = false`（之后每次启动）：只在全局存在并列时按
    /// (优先级, 加入时间) 整队 —— 与单上游时代的去重逻辑逐条一致。
    ///
    /// 整队后整份数组按新顺序落盘。返回变更清单（未变化的项不列入）。
    fn renumber_globally(state: &mut AccountState, legacy_order: bool) -> Vec<PriorityAssignment> {
        if state.accounts.is_empty() {
            return Vec::new();
        }
        if !legacy_order {
            let mut unique: Vec<i64> = state
                .accounts
                .iter()
                .map(|record| normalize_priority_value(record.priority()))
                .collect();
            let total = unique.len();
            unique.sort_unstable();
            unique.dedup();
            if unique.len() == total {
                return Vec::new();
            }
        }

        let legacy_route = config::legacy_provider_route();
        let route_rank = |record: &StoredAccount| -> u32 {
            let provider = record.provider();
            legacy_route
                .iter()
                .find(|(id, _)| *id == provider)
                .map(|(_, rank)| *rank)
                .unwrap_or_else(|| (provider_index(&provider).unwrap_or(usize::MAX / 2) as u32 + 1) * 10)
        };
        let mut sorted = state.accounts.clone();
        if legacy_order {
            sorted.sort_by_key(|record| (route_rank(record), record.order_key()));
        } else {
            sorted.sort_by_key(StoredAccount::order_key);
        }
        let mut numbering: Vec<(String, String, i64)> = sorted
            .iter()
            .map(|record| (record.id().to_string(), record.name(), record.priority()))
            .collect();
        let assignments = renumber_consecutively(&mut numbering);
        // 只回写「确实改变」的账号：原本就没有 priority 字段的账号迁移后依然没有
        // （生效值仍是默认 100），不会凭空多出一批字段
        for assignment in &assignments {
            if let Some(record) = sorted.iter_mut().find(|record| record.id() == assignment.id) {
                record.set_priority(assignment.to);
            }
        }
        state.accounts = sorted;
        assignments
    }

    /// 旧版单账号 auth.json 迁移：仅当 accounts.json 尚无任何账号时导入一次。
    ///
    /// 返回导入后的公开形态（未迁移时返回 None，对应 Node 版返回 null）。
    pub fn import_legacy_session(&self, legacy: &Value) -> Option<Value> {
        let access_token = legacy
            .get("auth")
            .and_then(|auth| auth.get("accessToken"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if access_token.is_empty() {
            return None;
        }
        {
            let _guard = self.guard();
            let state = self.load(&_guard);
            if !state.accounts.is_empty() {
                return None;
            }
        }
        let uid = legacy
            .get("account")
            .and_then(|account| account.get("uid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if uid.is_empty() {
            logging::log("[Accounts]", "旧登录态缺少 uid，跳过迁移");
            return None;
        }
        let nickname = legacy
            .get("account")
            .and_then(|account| account.get("nickname"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let payload = json!({
            "auth": legacy.get("auth").cloned().unwrap_or(Value::Null),
            "account": legacy.get("account").cloned().unwrap_or(Value::Null),
            "prefixPath": legacy.get("prefixPath").cloned().unwrap_or(Value::Null),
            "endpoint": legacy.get("endpoint").cloned().unwrap_or(Value::Null),
            "platform": legacy.get("platform").cloned().unwrap_or(Value::Null),
            "edition": legacy.get("edition").cloned().unwrap_or(Value::Null),
        });
        match self.add_account(&payload, nickname.as_deref()) {
            Ok(account) => Some(account),
            Err(error) => {
                logging::log("[Accounts]", &format!("❌ 旧登录态迁移失败: {error}"));
                None
            }
        }
    }
}
