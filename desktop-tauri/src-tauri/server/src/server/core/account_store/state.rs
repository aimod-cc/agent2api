//! 账号记录的 JSON 形态与容错访问器。
//!
//! 持久化形态已从 `accounts.json` 换到 SQLite 的 `accounts` 表（见 `sql.rs`），
//! 但**记录本身的形状一字未变**：它仍是「原样持有的 JSON 对象 + 一组容错
//! 访问器」，`data` 列存的就是本文件的 `to_value()`。
//!
//! ── 为什么记录用 `serde_json::Map` 而不是 struct ──
//! Node 版把每条账号记录当普通对象读写（`record.anything`），字段集随版本演进，
//! 而我们的硬约束是「用户升级绝不能丢账号里的任何字段」。用 struct 有两处风险：
//!   1. 定义不全 → 反序列化时未知字段被丢弃，写回就永久丢了；
//!   2. 类型太严 → 手工编辑出的 `enabled: "false"` 这类脏值会让整条记录读不出来。
//! 因此这里保留 JSON 原样（`Map<String, Value>`），只在上层提供**容错的取值器**：
//! 值类型不对时按 Node 的宽松语义回落（`''`/`0`/`null`/默认启用），而不是报错。
//!
//! 代价是这里没有编译期的字段名检查 —— 所以所有字段名都在本文件的访问器里写一次，
//! 上层一律走访问器，不要再散落字符串字面量。

use serde_json::{Map, Value};

use crate::server::core::account_store::priority::normalize_priority_value;
use crate::server::core::providers::DEFAULT_PROVIDER_ID;

/// 一整份账号集合的内存形态（迁移期与「需要整份数据」的路径用）。
///
/// 顶层只认识 `accounts` 与 `priorityScope`；其余顶层字段（旧版本的
/// currentAccountId 之类）刻意不解析 —— 从旧文件迁移时丢弃，之后不再存在。
///
/// `priority_scope` 是优先级号段的作用域标记：`"global"` 表示账号数据已经按
/// 「全局一条队列」编号过。旧版本按 provider 各排各的队（文件里没有这个标记），
/// 启动迁移据此判断要不要做一次跨家的重新编号（见 `store_admin::migrate_startup`）。
#[derive(Clone, Debug, Default)]
pub struct AccountState {
    pub accounts: Vec<StoredAccount>,
    pub priority_scope: Option<String>,
}

/// `priority_scope` 的当前取值：全局一条队列
pub const PRIORITY_SCOPE_GLOBAL: &str = "global";

/// 一条账号记录：原样持有的 JSON 对象 + 容错访问器。
#[derive(Clone, Debug)]
pub struct StoredAccount {
    fields: Map<String, Value>,
}

/// 容错取字符串：非字符串一律当空串（对应 JS 里 `String(x || '')` 的常见用法）
fn as_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// 容错取可选字符串：非字符串/空串都当「未设置」
fn as_optional_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

impl StoredAccount {
    /// 用一份字段表构造（调用方保证 id 已就位）
    pub fn from_map(fields: Map<String, Value>) -> Self {
        Self { fields }
    }

    /// 从磁盘条目构造；缺 id（或 id 非字符串）时返回 None
    /// （对应 Node 版 `filter(item => typeof item.id === 'string')`）。
    pub fn from_value(value: Value) -> Option<Self> {
        let Value::Object(fields) = value else {
            return None;
        };
        match fields.get("id") {
            Some(Value::String(id)) if !id.is_empty() => Some(Self { fields }),
            _ => None,
        }
    }

    /// 序列化回 JSON（保留全部未知字段）
    pub fn to_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    /// 直接借用底层字段表（导出、导入合并等需要遍历键的地方用）
    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    /// 可变借用字段表（导入合并按 key 覆写）
    pub fn fields_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.fields
    }

    // ── 身份 ────────────────────────────────────────────────

    pub fn id(&self) -> &str {
        self.fields.get("id").and_then(Value::as_str).unwrap_or("")
    }

    /// 所属提供商 id（`"workbuddy"` / `"raccoon"`，契约见 `core::providers`）。
    ///
    /// **容错口径**：字段缺失、为空串、或不是字符串时一律回落
    /// `DEFAULT_PROVIDER_ID`（workbuddy）—— 历史数据都来自 workbuddy 单上游时代，
    /// 而「未知 provider 字符串」则原样返回（可能是新版本写入的账号被旧版本读到，
    /// 强行归一成 workbuddy 会让它出现在错误的组里；原样透出至少能在界面上看见）。
    /// 与 Node 版 `record.provider || 'workbuddy'` 的宽松语义一致。
    pub fn provider(&self) -> String {
        match self.fields.get("provider") {
            Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
            _ => DEFAULT_PROVIDER_ID.to_string(),
        }
    }

    /// 写入 provider 字段（惰性迁移与新增账号用）
    pub fn set_provider(&mut self, provider: &str) {
        self.fields
            .insert("provider".to_string(), Value::String(provider.to_string()));
    }

    /// 记录里**显式**写了 provider 字段吗（惰性迁移据此判断要不要补写）。
    ///
    /// 与 `provider()` 的区别：后者对缺失字段做默认值兜底，本函数只看字段本身，
    /// 所以「字段缺失」与「字段值就是 workbuddy」能区分开 —— 迁移只在缺失时才写库。
    pub fn provider_explicit(&self) -> Option<&str> {
        match self.fields.get("provider") {
            Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        }
    }

    pub fn name(&self) -> String {
        as_text(self.fields.get("name"))
    }

    pub fn uid(&self) -> String {
        as_text(self.fields.get("uid"))
    }

    pub fn nickname(&self) -> String {
        as_text(self.fields.get("nickname"))
    }

    /// 账号类型：缺省视为个人版（对应 Node 版 `type || 'personal'`）
    pub fn account_type(&self) -> String {
        let value = as_text(self.fields.get("type"));
        if value.is_empty() { "personal".to_string() } else { value }
    }

    pub fn enterprise_id(&self) -> String {
        as_text(self.fields.get("enterpriseId"))
    }

    pub fn enterprise_name(&self) -> String {
        as_text(self.fields.get("enterpriseName"))
    }

    // ── 凭证 ────────────────────────────────────────────────

    pub fn access_token(&self) -> String {
        as_text(self.fields.get("accessToken"))
    }

    pub fn refresh_token(&self) -> String {
        as_text(self.fields.get("refreshToken"))
    }

    /// 过期时间戳（毫秒）。非法/缺失都当「未提供」——
    /// 与 Node 版 `Number(x) || null` 一致（0 也归到未提供）。
    pub fn expires_at(&self) -> Option<f64> {
        positive_number(self.fields.get("expiresAt"))
    }

    pub fn refresh_expires_at(&self) -> Option<f64> {
        positive_number(self.fields.get("refreshExpiresAt"))
    }

    pub fn domain(&self) -> String {
        as_text(self.fields.get("domain"))
    }

    // ── 端点与版本 ──────────────────────────────────────────

    /// 记录里显式保存的版本 id；缺失时返回 None（由调用方按默认版本兜底）
    pub fn edition(&self) -> Option<String> {
        as_optional_text(self.fields.get("edition"))
    }

    /// 记录里显式保存的 prefixPath。
    /// 注意与 edition 的差别：Node 用 `??`（null/undefined 才兜底），
    /// 所以空串是**有效值**（国际版端点也有空串前缀的场景），不能当未设置。
    pub fn prefix_path(&self) -> Option<String> {
        match self.fields.get("prefixPath") {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        }
    }

    pub fn endpoint(&self) -> Option<String> {
        as_optional_text(self.fields.get("endpoint"))
    }

    pub fn platform(&self) -> Option<String> {
        as_optional_text(self.fields.get("platform"))
    }

    // ── 运营属性 ────────────────────────────────────────────

    pub fn priority(&self) -> i64 {
        let fallback = crate::server::core::account_store::priority::DEFAULT_PRIORITY;
        let value = self.fields.get("priority");
        let parsed = match value {
            Some(Value::Number(number)) => number.as_f64(),
            Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
            _ => None,
        };
        match parsed {
            Some(number) if number.is_finite() => {
                normalize_priority_value(number.round() as i64)
            }
            _ => fallback,
        }
    }

    pub fn set_priority(&mut self, priority: i64) {
        self.fields
            .insert("priority".to_string(), Value::from(priority));
    }

    /// 是否启用：缺省视为启用（对应 Node 版 `enabled !== false`）
    pub fn enabled(&self) -> bool {
        !matches!(self.fields.get("enabled"), Some(Value::Bool(false)))
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.fields
            .insert("enabled".to_string(), Value::Bool(enabled));
    }

    pub fn added_at(&self) -> i64 {
        integer_of(self.fields.get("addedAt"))
    }

    pub fn updated_at(&self) -> i64 {
        integer_of(self.fields.get("updatedAt"))
    }

    pub fn set_updated_at(&mut self, value: i64) {
        self.fields
            .insert("updatedAt".to_string(), Value::from(value));
    }

    /// 最近一次**签到成功**的时刻（毫秒时间戳，0 = 从未签过或记录里没有）。
    ///
    /// 与 `updatedAt` 分开记是必须的：`updatedAt` 在任何一次改动（改备注名、
    /// 切代理、限额标记）时都会刷新，拿它判「今天签过没」会把「今天改过设置」
    /// 误判成「今天签过」。签到是按自然日幂等的，需要的是**这件事本身**的
    /// 时间戳，所以单独一个字段。
    pub fn checkin_at(&self) -> i64 {
        integer_of(self.fields.get("checkinAt"))
    }

    pub fn set_checkin_at(&mut self, value: i64) {
        self.fields
            .insert("checkinAt".to_string(), Value::from(value));
    }

    /// 最近一次**领取 ZCode 套餐成功**的时刻（毫秒时间戳，0 = 从未领过）。
    ///
    /// 与 `checkinAt` 刻意分开：ZCode 没有签到，它的运营玩法是限时发放的套餐
    /// （2026 那期是「每天登录领 1 亿」，见 `providers::zcode::claim` 的模块头），
    /// 界面据此显示「今日已领」并在次日恢复按钮。混用签到那个字段会让两家的
    /// 状态互相污染 —— 改 ZCode 的领取状态会顺手点亮别家的签到按钮。
    pub fn claim_at(&self) -> i64 {
        integer_of(self.fields.get("claimAt"))
    }

    pub fn set_claim_at(&mut self, value: i64) {
        self.fields.insert("claimAt".to_string(), Value::from(value));
    }

    /// 最近一次领取到的套餐 id（`zcode-v3-start-plan-trust-0928` 这类带日期段的串）。
    ///
    /// 只用于展示与排障：判「今天领过没」用的是 [`Self::claim_at`] 的本地自然日，
    /// 而套餐 id 里的日期段是**上游的**日期（时区未必与本机一致），拿它比日期
    /// 会在跨时区时误判。
    pub fn claim_plan_id(&self) -> String {
        as_text(self.fields.get("claimPlanId"))
    }

    pub fn set_claim_plan_id(&mut self, value: &str) {
        self.fields
            .insert("claimPlanId".to_string(), Value::String(value.to_string()));
    }

    /// 领取台账：`{ "<plan_id>": <毫秒时刻> }` —— 哪几份套餐在什么时候领过
    /// （见 `AccountStore::mark_zcode_claim` 里「为什么记成一张表」那段）。
    ///
    /// 读不懂的形状（数组 / 字符串这类手工编辑出的脏值）一律当空表：台账只服务
    /// 「这份今天领过没」的展示，按「没领过」处理最多让用户多点一次（上游会如实
    /// 回「已领取过」并顺手把台账补上），而拿脏值去判会让整个账号页出错。
    pub fn claim_plans(&self) -> Map<String, Value> {
        let mut ledger = match self.fields.get("claimPlans") {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        // 老记录兼容：台账是后加的，更早的版本只记了「最近一次领到的套餐」
        // （`claimPlanId` + `claimAt`）。这里把它并进来 —— 否则升级后第一次点开
        // 弹窗，会把已经领过的那一份重新列成可选中，点下去必然拿回一句
        // 「已领取过」，用户以为自己白点了一次。
        if ledger.is_empty() {
            let last_plan = as_optional_text(self.fields.get("claimPlanId")).unwrap_or_default();
            let last_at = self.claim_at();
            if !last_plan.is_empty() && last_at > 0 {
                ledger.insert(last_plan, Value::from(last_at));
            }
        }
        ledger
    }

    pub fn set_claim_plans(&mut self, ledger: Map<String, Value>) {
        self.fields
            .insert("claimPlans".to_string(), Value::Object(ledger));
    }

    /// 小浣熊新手任务的**结算台账**：`{ "<task_key>": <毫秒时刻> }`。
    ///
    /// 键是 `providers::raccoon::onboarding` 的任务 key（`desktop_login_grant` /
    /// `mobile_login_grant`），值是这条**一次性**登录奖励结算的时刻 —— 「本轮刚
    /// 领到」（granted=true）与「探测确认早已发放过」（granted=false）都算结算：
    /// 一次性福利落定后不会再变，之后状态查询直接按「已领取」展示。不记的话，
    /// 奖励早被官方客户端领掉的老账号每次打开面板都是「待领取」，点一次领取拿回
    /// 一句「早已发放过」—— 与 `mark_zcode_claim` 头上那段是同一个教训。
    ///
    /// 读不懂的形状（数组 / 字符串这类脏值）一律当空表：台账只服务「这条结算过没」
    /// 的展示，按「没结算过」处理最多让用户多点一次领取（上游幂等，不会重复加分），
    /// 领取一步会顺手把台账补上。
    pub fn onboarding_grants(&self) -> Map<String, Value> {
        match self.fields.get("onboardingGrants") {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        }
    }

    pub fn set_onboarding_grants(&mut self, ledger: Map<String, Value>) {
        self.fields
            .insert("onboardingGrants".to_string(), Value::Object(ledger));
    }

    /// 新手任务的**一次性结算快照**（Loomy 8 条任务 / CodeArts 新人礼）：
    /// `{ "at": <ms>, "tasks": [...], "earned": n, "total": n }`，`tasks` 与
    /// 状态查询返回的行同形（此刻全部 `done`）。
    ///
    /// 与 [`Self::onboarding_grants`] 的分工：那份是**逐条**时刻（小浣熊的两条
    /// 首登奖励各有独立的探测口径），这份是**整份清单** —— 给那两家「任务表领完
    /// 就没有了」的家用：结算之后状态查询与一键领取都不必再打上游（零请求），
    /// 面板进页面也直接按记忆渲染。一次性福利不会再变，快照因此没有过期问题；
    /// 手动「查询任务」仍走强制刷新（`?refresh=1`），拿到新事实时覆盖它。
    ///
    /// 读在 [`crate::server::core::providers::onboarding_memory::snapshot`]：
    /// 那里的入参是**账号记录的原始 JSON**（几家 `*_account_record()` 的返回形态），
    /// 读侧因此只有一份实现，本访问器只负责写。
    pub fn set_onboarding_settled(&mut self, snapshot: Value) {
        self.fields
            .insert("onboardingSettled".to_string(), snapshot);
    }

    /// 账号级出网代理配置（缺失返回 Value::Null）
    pub fn proxy(&self) -> Value {
        self.fields.get("proxy").cloned().unwrap_or(Value::Null)
    }

    pub fn set_proxy(&mut self, proxy: Value) {
        self.fields.insert("proxy".to_string(), proxy);
    }

    // ── ZCode 账号字段 ──────────────────────────────────────

    /// 设备标识（`X-Device-Mid`）：领取与余额查询都要带，上游要求 **UUID 形态**
    /// 且**跨请求稳定**（风控把同一个设备标识的多次请求关联起来，每次现编一个
    /// 会被当成换设备）。登录时生成一次随凭证落盘，本访问器只读它。
    pub fn device_mid(&self) -> String {
        as_text(self.fields.get("deviceMid"))
    }

    pub fn set_device_mid(&mut self, value: &str) {
        self.fields
            .insert("deviceMid".to_string(), Value::String(value.to_string()));
    }

    /// 套餐 JWT（打 `zcode.z.ai` 的领取 / 余额 / 活动套餐通道用；可能为空）。
    ///
    /// 与 `accessToken` 分开：两者不能互相替代（见 `zcode::credentials` 的模块头）。
    pub fn jwt(&self) -> String {
        as_text(self.fields.get("jwt"))
    }

    /// ZCode 用哪条上游通道（`zcode::PLAN_FIELD`，取值见 `zcode::{PLAN_CODING,
    /// PLAN_START}`）。
    ///
    /// **原样存取**，归一化（认不出的值怎么落）在 `zcode::normalize_plan` 一处：
    /// 存储层不做取值白名单，否则将来加第三条通道要改两个地方。
    pub fn zcode_plan(&self) -> String {
        as_text(self.fields.get(crate::server::core::providers::zcode::PLAN_FIELD))
    }

    pub fn set_zcode_plan(&mut self, value: &str) {
        self.fields.insert(
            crate::server::core::providers::zcode::PLAN_FIELD.to_string(),
            Value::String(value.to_string()),
        );
    }

    /// 是否「有可用凭证」（当前账号派生、凭据查询都以此为准）
    pub fn has_token(&self) -> bool {
        !self.access_token().is_empty()
    }

    // ── 小浣熊账号字段（Agent2API W3-T4）─────────────────────

    /// 是否为「桌面端实时登录态」账号（架构文档 §3.2）。
    ///
    /// 判据是记录里的 `desktop: true` 标记，而不是硬编码 id ——
    /// id 只是缺省值（`raccoon-desktop`），标记才是语义。
    pub fn is_desktop(&self) -> bool {
        matches!(self.fields.get("desktop"), Some(Value::Bool(true)))
    }

    /// 是否**有可用凭证** —— 转发选路的统一判据。
    ///
    /// 与 `has_token()` 的差别：小浣熊的桌面端实时账号按设计**不落 token**
    /// （凭证在 `~/.box-agent/config/auth.json`，每次实时读），所以
    /// 「记录里有 accessToken」对它是恒假的。只认 `has_token()` 会让这类账号
    /// 在选路时被整体跳过，转发直接 401。
    ///
    /// workbuddy 账号与其它 provider 的普通账号不受影响（它们的 `desktop`
    /// 字段不存在 → 判据退化成 `has_token()`，与改造前逐字相同）。
    ///
    /// 第三条判据是自定义提供商账号（`account_store::custom_accounts`）：它
    /// 的凭证键是 `apiKey` 而不是 `accessToken` —— 不加这一条，自定义账号会
    /// 在「当前账号派生」「账号列表的 hasCredentials」两处被判成无凭证而
    /// 静默消失。判据只看「记录里有没有非空 `apiKey`」，不判 provider：
    /// 内置八家的记录里没有这个键，判定天然不受影响（也就不必在这里回头
    /// 依赖 `custom_providers`）。
    ///
    /// 第四条判据 `has_jwt()` 只对 ZCode 有效（`jwt` 是它独有的凭证键）：
    /// 那条链路上「粘贴凭证」允许只填套餐 JWT（用户可能只想测领取），而
    /// `plan: start-plan` 的转发**只用 JWT**（见 `zcode::plan`）—— 只认
    /// `accessToken` 会让这类账号在选路时被跳过，报成「没有可用账号」，
    /// 与「账号明明能用」矛盾。
    ///
    /// 第五条判据 `no_auth()` 同样只对自定义账号有效：上游本来就不要鉴权时
    /// （本地 Ollama、OpenCode Zen 的匿名免费档、自建无反代），「没有 key」
    /// 不是缺陷而是**这条账号的正常形态**。判据必须**显式**落在记录上 ——
    /// 只看「apiKey 为空」会把用户忘了填 key 的记录一起放行，表现成一条
    /// 看不懂的上游 401（见 [`Self::no_auth`]）。
    ///
    /// 第六条判据 `has_officeace_key()` 只对 OfficeAce 有效（`modelAppKey` /
    /// `modelAppSecret` 是它独有的凭证键）：那家不用 Bearer token，转发凭证是
    /// 一对网关 Basic key，漏了这一条它整家都会被静默过滤掉（见
    /// [`Self::has_officeace_key`]）。
    pub fn has_credentials(&self) -> bool {
        self.has_token() || self.is_desktop() || self.has_api_key() || self.has_jwt()
            || self.no_auth() || self.has_officeace_key()
    }

    /// 记录是否**显式声明「该上游无需鉴权」**（自定义账号的 `noAuth: true`）。
    ///
    /// 写入侧保证它与 `apiKey` 互斥（[`super::custom_accounts`] 的两个写入
    /// 入口都守着这条不变量）：勾了无需鉴权就没有 key 可存，存了 key 就摘掉
    /// 这个标记。于是「有没有凭证」与「发不发鉴权头」两件事都只有一种读法，
    /// 转发侧不必再判优先级。
    ///
    /// 内置八家的记录里没有这个键（它们各有登录态与刷新链路），判定天然不受
    /// 影响 —— 也就与 `has_api_key` 同一条「不判 provider」的取舍。
    pub fn no_auth(&self) -> bool {
        matches!(self.fields.get("noAuth"), Some(Value::Bool(true)))
    }

    /// 记录里是否有**非空**的 `jwt`（ZCode 的套餐令牌）
    pub fn has_jwt(&self) -> bool {
        !self.jwt().is_empty()
    }

    /// 记录里是否有**非空**的 `apiKey`（自定义提供商账号的凭证键，见上）
    pub fn has_api_key(&self) -> bool {
        self.fields
            .get("apiKey")
            .and_then(Value::as_str)
            .is_some_and(|key| !key.trim().is_empty())
    }

    /// 记录里是否有 OfficeAce 的**网关凭据**（`modelAppKey` + `modelAppSecret`）。
    ///
    /// ── 为什么要有这一支 ────────────────────────────────────────
    /// OfficeAce 的转发凭证是**一对**（`Bearer`/`accessToken` 那套它不用）：
    /// 网关 Basic 的 `modelAppKey` : `modelAppSecret`。没有这一支时
    /// `has_credentials()` 对它恒假 ⇒ 账号在 `accounts_for_provider` /
    /// `current_entry_for_provider` / `get_session_by_id` 三处全被过滤掉，
    /// 表现为「账号导入成功、目录也拉到了，但 `/v1/models` 里这条家一个模型都
    /// 没有、点名转发报没有可用账号」——把一整家静默吞掉。
    ///
    /// 两个 key 都要非空才算：只填一个的凭据转发必然 401，把它算作「有凭证」
    /// 只会让失败推迟到上游（与 `has_api_key` 的取舍同一条）。
    pub fn has_officeace_key(&self) -> bool {
        let non_empty = |key: &str| {
            self.fields
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        };
        non_empty("modelAppKey") && non_empty("modelAppSecret")
    }

    /// 小浣熊账号的用户 ID（`userId`；workbuddy 侧对应 `uid`）
    pub fn user_id(&self) -> String {
        as_text(self.fields.get("userId"))
    }

    /// 小浣熊账号的 token 过期时间（`tokenExpiresAt`，毫秒）
    pub fn token_expires_at(&self) -> Option<f64> {
        positive_number(self.fields.get("tokenExpiresAt"))
    }

    /// 账号来源（`imported` / `manual` / …；缺失给空串）
    pub fn source(&self) -> String {
        as_text(self.fields.get("source"))
    }

    /// 选路排序键：`(优先级, 加入时间)`
    pub fn order_key(&self) -> (i64, i64) {
        (self.priority(), self.added_at())
    }

    // ── 通用字段读写（导入合并、限额标记等需要直接改字段）──

    pub fn set(&mut self, key: &str, value: Value) {
        self.fields.insert(key.to_string(), value);
    }

    pub fn remove(&mut self, key: &str) {
        self.fields.remove(key);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }
}

/// 在记录上打「用户显式设置过备注名」标（`nameCustom`）。
///
/// 种子名（凭证账号名 / 昵称 / uid 兜底）与用户改的名落库后都是 `name`，无法
/// 事后区分 —— 只能在**用户显式给名的时刻**打标：设置弹窗真正改到 name 时
/// （`apply_patch`），以及添加表单显式填了备注名（各 provider 的添加路径）。
/// 界面据此分流：有标记备注名恒为主名，无标记维持历史口径（邮箱 / 昵称优先，
/// 见岛内 displayNameOf）—— 没有它，「备注名优先」会把未设备注账号的显示
/// 顶成建号时的种子值。
///
/// 重加（同 id 再走一次添加）没给新名字时，旧记录的标记要**跟过来**：备注名
/// 种子链会从旧记录把名字原样种回来，标记丢了它就又被默认口径压住。
pub(crate) fn mark_name_custom(
    record: &mut Map<String, Value>,
    explicit_name: bool,
    existing: Option<&StoredAccount>,
) {
    let carried = existing
        .and_then(|item| item.get("nameCustom"))
        .is_some_and(|value| matches!(value, Value::Bool(true)));
    if explicit_name || carried {
        record.insert("nameCustom".to_string(), Value::Bool(true));
    }
}

// ─── 优先级判定的数据源（全局一条队列）────────────────────

// 改造前这里有一个 `priority_peers(&[StoredAccount]) -> Vec<(id, name, priority)>`
// 的水位函数，供写入侧在内存里做「号段分配 + 冲突判定」。账号数据搬到 SQLite
// 之后这两件事各由一次投影列查询完成（`sql::priorities_except` 拿号段、
// `sql::priority_holder` 拿占位者名字），不再需要把全部记录先搬进内存 ——
// 于是它随改造一起删掉了（留一个无人调用的函数只会变成死代码）。
// 「优先级全局唯一」这条不变量的判据没变，只是执行者从 Rust 内存换成了 SQL。

/// 把数值转成 JSON：**整数形式的数写成整数**。
///
/// 为什么必须这样：`serde_json::Value::from(1e12_f64)` 会输出 `1000000000000.0`，
/// 而 Node 的 `JSON.stringify(1730000000000)` 输出 `1730000000000`。
/// 时间戳一律是整数毫秒，写成浮点会让账号记录里出现 `1730000000000.0` 这种
/// 无意义的形态（旧 accounts.json 的 diff 也会满是噪音），
/// 也可能让「按文本比对配置」的用法出现意外差异。
pub fn json_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// 正的数值（>0 才算，负数/0/非数字都当未提供）
fn positive_number(value: Option<&Value>) -> Option<f64> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64()?,
        Some(Value::String(text)) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if number.is_finite() && number > 0.0 {
        Some(number)
    } else {
        None
    }
}

/// 时间戳口径：Node 里 `addedAt || 0` 是「保留原值（含浮点毫秒），缺失当 0」，
/// 所以这里按 i64 取整保存（毫秒时间戳用整数表达即可）。
fn integer_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or_else(|| {
            number.as_f64().map(|float| float as i64).unwrap_or(0)
        }),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

// ─── 派生视图（对应 Node 版 getCurrentEntry / getCredentialsById / getSessionById）──

/// 当前账号（队首的可用账号）与其会话
#[derive(Clone, Debug)]
pub struct CurrentEntry {
    pub id: String,
    pub session: Value,
}

/// `getCredentialsById` 的返回形态（含 token 与端点身份）。
///
/// 字段与 Node 版 `workbuddy-account-store.mjs` 的同名导出逐一对齐：这是该导出的
/// 强类型对等物，续期（refresh_account）已用 name/uid/refresh_token/endpoint 等；
/// 下面几个字段当前无读取点，但保留以维持与 Node 返回形态的完整对应。
#[derive(Clone, Debug)]
pub struct CredentialsById {
    /// Node 版 `getCredentialsById().id` 的对等字段（调用方已持有 id，形态对齐保留）
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub uid: String,
    /// Node 版 `getCredentialsById().accessToken` 的对等字段（续期改用 refreshToken）
    #[allow(dead_code)]
    pub access_token: String,
    pub refresh_token: String,
    /// Node 版 `getCredentialsById().expiresAt` 的对等字段
    #[allow(dead_code)]
    pub expires_at: Option<f64>,
    pub endpoint: String,
    pub prefix_path: String,
    pub platform: String,
    pub edition: String,
    /// Node 版 `getCredentialsById().priority` 的对等字段
    #[allow(dead_code)]
    pub priority: i64,
    /// Node 版 `getCredentialsById().enabled` 的对等字段
    #[allow(dead_code)]
    pub enabled: bool,
    pub proxy: Value,
    pub proxy_error: Option<String>,
}

/// `getSessionById` 的返回形态（会话 + 该账号解析出的出口）
#[derive(Clone, Debug)]
pub struct SessionById {
    /// Node 版 `getSessionById()` 返回对象里的 id（调用方已持有 id，形态对齐保留）
    #[allow(dead_code)]
    pub id: String,
    pub session: Value,
    pub proxy: Value,
    pub proxy_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_name_or_carried_flag_marks_the_record() {
        // 添加表单显式给名：打标
        let mut record = Map::new();
        mark_name_custom(&mut record, true, None);
        assert_eq!(record.get("nameCustom"), Some(&Value::Bool(true)));

        // 种子名（非显式、旧记录无标）：不打 —— 未设备注的账号要维持原展示口径
        let mut record = Map::new();
        mark_name_custom(&mut record, false, None);
        assert_eq!(record.get("nameCustom"), None);

        // 重加没给新名字：旧记录已打的标要跟过来（备注名从旧记录种回来，标不能丢）
        let mut previous = Map::new();
        previous.insert("nameCustom".to_string(), Value::Bool(true));
        let existing = StoredAccount::from_map(previous);
        let mut record = Map::new();
        mark_name_custom(&mut record, false, Some(&existing));
        assert_eq!(record.get("nameCustom"), Some(&Value::Bool(true)));
    }

    #[test]
    fn officeace_gateway_pair_counts_as_credentials() {
        // 一对齐备的网关 Basic 凭据 ⇒ 有凭证（否则整家会被静默过滤掉）
        let mut fields = Map::new();
        fields.insert("modelAppKey".to_string(), Value::String("K".to_string()));
        fields.insert("modelAppSecret".to_string(), Value::String("S".to_string()));
        let record = StoredAccount::from_map(fields);
        assert!(record.has_officeace_key());
        assert!(record.has_credentials());

        // 只填一个：不算有凭证（转发必然 401，别把失败推到上游）
        let mut only_key = Map::new();
        only_key.insert("modelAppKey".to_string(), Value::String("K".to_string()));
        assert!(!StoredAccount::from_map(only_key).has_officeace_key());

        // 空串 / 全空白同样不算
        let mut blank = Map::new();
        blank.insert("modelAppKey".to_string(), Value::String("K".to_string()));
        blank.insert("modelAppSecret".to_string(), Value::String("  ".to_string()));
        assert!(!StoredAccount::from_map(blank).has_officeace_key());

        // 别家的记录不受影响：只有 accessToken 的记录仍是「有凭证」
        let mut plain = Map::new();
        plain.insert("accessToken".to_string(), Value::String("T".to_string()));
        let plain = StoredAccount::from_map(plain);
        assert!(plain.has_credentials());
        assert!(!plain.has_officeace_key(), "别家不该被判成有 OfficeAce 凭据");
    }
}
