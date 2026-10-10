//! 账号的**公开形态**与按 provider 的查询（从 store.rs 拆出，单文件行数约定）。
//!
//!   to_public_account      workbuddy 形状（对照 Node 版 toPublicAccount）
//!   public_account         按 provider **分派**到各家的形状，并注入跨家事实
//!   snapshot               /api/accounts 的列表快照（含 providers 摘要）
//!   provider_summary       provider 摘要 `{id,label,count}`
//!   accounts_for_provider  指定 provider 的启用账号（聚合目录判可用性）
//!   list_accounts          快照入口（取锁 + 读全部账号）
//!
//! ── 为什么拆出来 ──────────────────────────────────────────
//! store.rs 在账号存储改造后要承载的东西变多了（句柄、锁、连接、装载/保存的
//! 两个端点），而「怎么把一条记录渲染成给前端/给选路层的形状」是另一件事：
//! 前者的读者关心并发与存储语义，后者关心字段口径。按项目的单文件行数约定
//! 与既有的 `store_*` 拆分风格（`store_crud` / `store_batch` / `store_admin`）
//! 单独成文件，改字段时不必在存储语义里翻找。
//!
//! 这些方法仍是 `impl AccountStore` 的分块（同一个类型，跨文件不改变可见性）。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::server::core::account_store::state::{AccountState, StoredAccount};
use crate::server::core::account_store::store::{forwards_requests, AccountStore};
use crate::server::core::account_store::store_util::{js_truthy, max_concurrent_public, value_or, value_or_nullish};
use crate::server::core::endpoints::{resolve_edition, EditionInfo};
use crate::server::core::proxies::describe_account_proxy;

impl AccountStore {
    // ─── 公开形态 ────────────────────────────────────────────

    /// 记录 → 公开形态（对照 Node 版 `toPublicAccount`，字段逐个对齐）。
    ///
    /// 注意几处 `||` / `??` 的差别：`prefixPath` 是 `??`（空串有效），
    /// `endpoint` 是 `||`（空串回落版本默认值）；`name` 在 Node 里没有兜底，
    /// 因此这里也是「缺失则不出键」而不是补空串。
    pub(crate) fn to_public_account(&self, record: &StoredAccount) -> Value {
        let edition: &'static EditionInfo = resolve_edition(record.edition().as_deref());
        let fields = record.fields();
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        // 所属提供商（Agent2API 改造新增字段）：缺失时按 workbuddy 兜底，
        // 保证界面拿到的每条账号都能直接分组，不必自己判断「没有 provider = 老数据」
        public.insert(
            "provider".to_string(),
            Value::String(record.provider()),
        );
        // name 无兜底：原样透出（含非字符串的脏值），缺失时不出现该键
        if let Some(value) = fields.get("name") {
            public.insert("name".to_string(), value.clone());
        }
        public.insert(
            "uid".to_string(),
            value_or(fields.get("uid"), Value::String(String::new())),
        );
        public.insert(
            "nickname".to_string(),
            value_or(fields.get("nickname"), Value::String(String::new())),
        );
        public.insert(
            "type".to_string(),
            value_or(fields.get("type"), Value::String("personal".to_string())),
        );
        public.insert(
            "enterpriseId".to_string(),
            value_or(fields.get("enterpriseId"), Value::String(String::new())),
        );
        public.insert(
            "enterpriseName".to_string(),
            value_or(fields.get("enterpriseName"), Value::String(String::new())),
        );
        public.insert(
            "tokenTail".to_string(),
            value_or(fields.get("tokenTail"), Value::String(String::new())),
        );
        public.insert(
            "expiresAt".to_string(),
            value_or(fields.get("expiresAt"), Value::Null),
        );
        public.insert(
            "hasRefreshToken".to_string(),
            Value::Bool(
                fields
                    .get("refreshToken")
                    .map(js_truthy)
                    .unwrap_or(false),
            ),
        );
        public.insert(
            "prefixPath".to_string(),
            value_or_nullish(
                fields.get("prefixPath"),
                Value::String(edition.prefix_path.to_string()),
            ),
        );
        public.insert(
            "endpoint".to_string(),
            value_or(
                fields.get("endpoint"),
                Value::String(edition.endpoint.to_string()),
            ),
        );
        public.insert("edition".to_string(), Value::String(edition.id.to_string()));
        public.insert(
            "editionLabel".to_string(),
            Value::String(edition.label.to_string()),
        );
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            describe_account_proxy(Some(&value_or(fields.get("proxy"), Value::Null))),
        );
        // Node 是 `record.rateLimits || {}`：任何真值都原样透出
        // （手工写成数组/字符串时也照透，前端按对象读会得到 undefined，
        // 与 Node 的行为保持一致比「顺手修正」重要）
        public.insert(
            "rateLimits".to_string(),
            value_or(fields.get("rateLimits"), Value::Object(Map::new())),
        );
        // 本切片所有账号都视为可用（限额/可用性判定属切片 3 的转发层）
        public.insert("available".to_string(), Value::Bool(true));
        // 单账号并发上限（所有家通用的账号属性）：0 = 不限，记录里没有该键
        // （旧数据 / 从未设置过）同样输出 0 —— 兜底在 `max_concurrent_public`
        // 一处实现，其余各家的公开形态同用它（选路消费方按 0 解释为不限）
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(fields.get("maxConcurrent"))),
        );
        Value::Object(public)
    }

    /// 账号列表快照 `{ currentAccountId, currentAccountIds, accounts: [...], providers: [...] }`。
    ///
    /// accounts 按**存储顺序**返回（与 Node 版按文件顺序返回一致）—— 界面自己按
    /// 优先级排序，不要在这里改成优先级序，否则与 Node 版的行为就分叉了。
    /// 「存储顺序」在 SQLite 下是 `rowid` 升序，它与旧的 accounts.json 数组顺序
    /// 在三类场景下逐字对应（迁移按序导入、新增落到末尾、原地修改不改位置），
    /// 推演见 `sql.rs` 模块头「顺序」一节。
    ///
    /// ── 「当前账号」的两个字段 ──────────────────────────────
    ///   · `currentAccountId`：**全局队首**（`pick_current`：启用 + 有可用凭证 +
    ///     优先级序），与 `/api/session` 的 `session.currentAccountId` 同源。
    ///     账号页的 ★ / 「首选」读这个。
    ///   · `currentAccountIds`：provider → 该家队首账号 id（逐家派生，
    ///     `pick_current_for_provider`）。全局队列之后它只剩「这一家有没有可用账号」
    ///     的参考意义，保留是为了不破坏读它的旧客户端。
    ///
    /// `providers` 是 provider 摘要（`{id,label,count}`）：
    /// 界面用它渲染账号页的分组标题与「作用提供商」多选，因此**必须在列表接口
    /// 上就给出**，前端不必再发第二个请求。计数是各 provider 的账号**总数**
    /// （含禁用账号）—— 与分组标题显示的「N 个账号」口径一致。
    pub fn list_accounts(&self) -> Value {
        let _guard = self.guard();
        let state = self.load(&_guard);
        self.snapshot(&state)
    }

    /// 账号 id → 该记录最近一次改动的时刻（`max(addedAt, updatedAt)`，毫秒）。
    ///
    /// 给「这条结论现在还算不算数」的时效判定用（见 `core::usage_query` 的快照
    /// 出口过滤）。判据刻意只看记录自己的时间戳：重新登录、重新导入、token 被
    /// 刷新、改设置都会把它往前推，而**凭证内容本身不必为了这个判定被读出来**
    /// —— 比指纹更省事，也不给「把 token 拼进一个新结构」多开一条口子。
    ///
    /// 代价是改动类型区分不出来（换凭证与改备注名同样是「记录变了」）：判定方
    /// 按「记录变了就作废旧结论」处理，多作废一条失败提示，比留着一条过期结论
    /// 更轻 —— 用户手点一次查询就能得到新的（见 `usage_query` 的说明）。
    pub fn account_change_times(&self) -> HashMap<String, i64> {
        let _guard = self.guard();
        let state = self.load(&_guard);
        state
            .accounts
            .iter()
            .map(|record| {
                (
                    record.id().to_string(),
                    record.added_at().max(record.updated_at()),
                )
            })
            .collect()
    }

    /// 已持锁时的列表快照（CRUD 内部要在同一次锁里连做「写入 + 取快照」）
    pub(crate) fn snapshot(&self, state: &AccountState) -> Value {
        // 逐家派生队首：provider 取值的来源就是账号数据里出现过的家
        // （含手工塞进来的未知 id —— 它们也有自己的队首，不该被静默忽略）
        let mut current_ids = Map::new();
        for record in &state.accounts {
            let provider = record.provider();
            if current_ids.contains_key(&provider) {
                continue;
            }
            let head = Self::pick_current_for_provider(&state.accounts, &provider)
                .map(|record| Value::String(record.id().to_string()))
                .unwrap_or(Value::Null);
            current_ids.insert(provider, head);
        }
        let global_current = Self::pick_current(&state.accounts)
            .map(|record| Value::String(record.id().to_string()))
            .unwrap_or(Value::Null);
        json!({
            "currentAccountId": global_current,
            "currentAccountIds": Value::Object(current_ids),
            "accounts": state
                .accounts
                .iter()
                .map(|record| self.public_account(record))
                .collect::<Vec<_>>(),
            "providers": self.provider_summary(state),
        })
    }

    /// 记录 → 公开形态（**按 provider 分派**）。
    ///
    /// 各家的公开字段集不同：workbuddy 有 uid/nickname/edition/enterprise…，
    /// 小浣熊有 userId/tokenExpiresAt/desktop 且**没有**积分字段（架构文档 §5/§6），
    /// CatPaw 有 uid/loginName/tokenTail/desktop 且**没有**积分/签到（W5-T-d4）。
    /// 分派点放在这里，调用方（列表快照、批量目标解析、限额事件日志）
    /// 一行都不用改 —— 它们拿到的就是各自 provider 该有的形状。
    ///
    /// 兜底分支给的是 **workbuddy 形状**，这对**未知** provider id 是刻意的
    /// （与 `StoredAccount::provider()` 的容错口径一致：手改文件塞进来的陌生 id
    /// 至少还能在界面上显示出来）。AutoClaw 现在**加不了账号**
    /// （`api::accounts::add_account` 显式 400），将来它的公开形态在适配器接线
    /// 波次里补一个分支即可 —— 那条分支落地前，万一有手改的账号记录落进这里，
    /// 走 workbuddy 形状总比整条列表报错好。
    ///
    /// ── `hasCredentials` / `chatSupported`：统一注入的**跨家事实** ──────
    /// 在这里**统一注入**而不是改各家的公开形态函数：判据只有一条
    /// （`has_credentials` / 适配器的 `supports_chat`），而「谁有凭证」「谁能转发」
    /// 正是转发选路与「当前账号」派生共用的那两道闸门。前端要按模型
    /// 自行推算「这一家此刻会走谁」时（后端只给不限模型的队首），必须拿得到同一
    /// 事实，否则会出现「界面标 ★ 的账号其实转发时会因无凭证被跳过」的分歧。
    /// 放在分派点让各家形状**同字段名、同语义**，前端不必按 provider 查表。
    ///
    /// 纯新增字段：各家的既有字段一个不动，旧客户端忽略它即可。
    pub(crate) fn public_account(&self, record: &StoredAccount) -> Value {
        let shaped = if record.provider() == super::RACCOON_PROVIDER_ID {
            self.to_raccoon_public_account(record)
        } else if record.provider() == super::CATPAW_PROVIDER_ID {
            self.to_catpaw_public_account(record)
        } else if super::is_autoclaw_family(&record.provider()) {
            // 两个地区（`autoclaw` / `autoclaw-intl`）共用这一份公开形态：
            // 账号字段、桌面端判定、deviceId 语义两地完全一致，
            // 差别只在域名（那是转发与凭证层的事，公开形态不体现）
            self.to_autoclaw_public_account(record)
        } else if super::is_qoder_family(&record.provider()) {
            // 两个地区（`qoder` / `qoder-intl`）共用这一份公开形态：地区由
            // provider id（兜底 `mode` 字段）派生进 `edition` / `editionLabel`
            self.to_qoder_public_account(record)
        } else if record.provider() == super::codearts_accounts::CODEARTS_PROVIDER_ID {
            self.to_codearts_public_account(record)
        } else if super::is_cline_family(&record.provider()) {
            // 两个池（`cline-free` / `cline-pass`）共用这一份公开形态
            self.to_cline_public_account(record)
        } else if super::is_accio_family(&record.provider()) {
            // 两个地区（`accio` / `accio-cn`）共用这一份公开形态：账号字段、
            // 续期语义两地完全一致，差别只在登录站点与区域头（那是转发与
            // 凭证层的事，公开形态不体现）
            self.to_accio_public_account(record)
        } else if super::is_zcode_family(&record.provider()) {
            // 两个地区（`zcode` / `zcode-intl`）共用这一份公开形态：账号字段两地
            // 完全一致，差别只在推理域名与领取时的上游 provider 取值（那是转发
            // 与领取层的事，公开形态只用 `edition` 把地区标出来供界面显示）
            self.to_zcode_public_account(record)
        } else if record.provider() == super::TRAE_PROVIDER_ID {
            // Trae 只有一家（国内 SOLO）：国际版是**另一套协议**而不是一个地区，
            // 所以这里没有 `is_trae_family` —— 将来接国际版时另立 kind、
            // 另开一个分支，不要往本家的记录上挂 `region` 字段。
            self.to_trae_public_account(record)
        } else if record.provider() == super::LOOMY_PROVIDER_ID {
            // Loomy（讯飞）：单一入口（手机验证码登录），公开形态带 userId /
            // phone / session 尾四位的展示字段（见 `loomy_accounts.rs`）
            self.to_loomy_public_account(record)
        } else if super::is_monkeycode_family(&record.provider()) {
            // MonkeyCode 的两个站点（`monkeycode` / `monkeycode-intl`）共用这一份
            // 公开形态：字段两地完全一致，差别只在站点域名（那是转发与凭证层的
            // 事，公开形态只用 `edition` 把地区标出来供界面显示）
            self.to_monkeycode_public_account(record)
        } else if record.provider() == super::kuku_accounts::KUKU_PROVIDER_ID {
            // KukuAI（百度文库库库 AI）：单一入口（粘贴 Cookie / 导入本机登录态），
            // 公开形态带 uid(=uk) / loginName(=昵称) / tokenTail（见
            // `kuku_accounts.rs`）
            self.to_kuku_public_account(record)
        } else if record.provider() == super::COMMANDCODE_PROVIDER_ID {
            // Command Code：单一入口（粘贴 `user_` API Key），公开形态去掉 key
            // 本体、只留尾号与「有没有 key」（见 `commandcode_accounts.rs`）
            self.to_commandcode_public_account(record)
        } else if record.provider() == super::ANTIGRAVITY_PROVIDER_ID {
            // Antigravity（Google）：单一入口（粘贴 Google refresh token），
            // 公开形态去掉 token 本体、只留 refresh token 尾号与
            // 「有没有 refreshToken / accessToken」（见 `antigravity_accounts.rs`）
            self.to_antigravity_public_account(record)
        } else if record.provider() == super::OFFICEACE_PROVIDER_ID {
            // OfficeAce（华为云果办）：只透出标识与到期时刻，网关 Basic 那一对
            // 与控制面临时凭据都不出这个函数（见 `to_officeace_public_account`）。
            // 漏这一支时它会掉进下面的 workbuddy 兜底形状：界面多出一批 uid/edition
            // 之类与它无关的空字段，而「刷新 Token」按钮靠的 hasRefreshToken
            // 只是**恰好**在兜底形状里也算得出来 —— 那不是设计，是巧合。
            self.to_officeace_public_account(record)
        } else if record
            .provider()
            .starts_with(crate::server::core::custom_providers::ID_PREFIX)
        {
            // 自定义提供商的账号。判据用**前缀**而不是 `custom_providers::
            // is_custom_provider_id`（后者还要求提供商仍在配置里）：形状分派
            // 是纯展示决定，提供商被删后残留的账号（迁移/手改残留）也应如实
            // 显示出来，而不是退化成 workbuddy 形状让界面出现 uid/edition 等
            // 与它无关的字段
            self.to_custom_public_account(record)
        } else {
            self.to_public_account(record)
        };
        match shaped {
            Value::Object(mut fields) => {
                fields.insert(
                    "hasCredentials".to_string(),
                    Value::Bool(record.has_credentials()),
                );
                // 没有转发能力的家：界面据此说明「启用了也不会被转发」，
                // 而不是把一个失效的启用开关当成正常账号展示。
                // 各家现在都能转发，所以正常配置下这里恒为 true ——
                // 保留这个字段是因为「能用账号管理、但转发还没接上」这种过渡期
                // 状态将来还会出现，而界面需要有办法如实说出来。
                fields.insert(
                    "chatSupported".to_string(),
                    Value::Bool(forwards_requests(record)),
                );
                // 最近一次签到成功的时刻（0 = 从未签过）。跨家统一注入的理由与上面
                // 两条相同：它是「这条账号的签到状态」这一个事实，各家的存放位置
                // 一致（`checkinAt`），界面不必按 provider 查表。
                //
                // 给出的是**时间戳**而不是「今天签过没」的布尔：自然日的边界要按
                // 用户本地时区算，而那个判定在界面上已有同款实现（限流恢复时间的
                // 「今天 / 明天」也是本地自然日，见 accounts-model.js 的 startOfDay）。
                // 传原始时间戳还让界面能显示「今天 08:30 已签到」这类信息。
                //
                // 恒为数字（无记录时 0）而不是缺键：界面按 `Number(...) || 0` 读，
                // 两种形态都能吃，但恒定的形状让「字段缺失」与「值为 0」不再需要分开判。
                fields.insert("checkinAt".to_string(), Value::from(record.checkin_at()));
                // 用户显式设置过备注名（update_account 真正改到 name 时打的标，
                // 见 apply_patch）。恒为布尔而不是缺键：界面据此决定「备注名赢过
                // 邮箱 / 昵称等默认口径」还是「维持原展示行为」，两种形态都不必
                // 再判「字段缺失」。
                fields.insert(
                    "nameCustom".to_string(),
                    Value::Bool(
                        record
                            .fields()
                            .get("nameCustom")
                            .map(js_truthy)
                            .unwrap_or(false),
                    ),
                );
                // 每账号的余额查询设置（自动查询 + 余额不足处理）：跨家统一注入
                // 的理由与 hasCredentials 相同 —— 心跳调度、选路跳过 / 禁用与前端
                // 弹窗、徽章读的是**同一份事实**，不能各家形状一个口径。
                // 恒为对象（未配置 = 各家缺省档的规范化形状，与写入侧
                // `apply_patch` 的归一化一致），读侧不必判「键缺失」。
                fields.insert(
                    "usageQuery".to_string(),
                    usage_query_public(record.fields().get("usageQuery")),
                );
                fields.insert(
                    "lowBalance".to_string(),
                    low_balance_public(&record.provider(), record.fields().get("lowBalance")),
                );
                // 每账号的「限制器」有效规则（余额 / Token 规则列表）：恒为数组 ——
                // 记录上没有 `limiters` 键（旧版写的记录）时由 lowBalance / provider
                // 缺省**推导**（见 `limiter::effective_rules`），前端因此不必自带
                // 推导逻辑，弹窗、徽章与后端选路读到的永远是同一份规则。
                fields.insert(
                    "limiters".to_string(),
                    crate::server::core::limiter::effective_rules_json_in(record.fields()),
                );
                Value::Object(fields)
            }
            // 各家形状恒为对象；真出现异常形态时原样透出，不在这里改语义
            other => other,
        }
    }

    /// provider 摘要：注册表顺序 + 各 provider 的账号总数。
    ///
    /// 计数在已读出的账号列表上跑（不再次访问数据库、不再取锁）——
    /// `snapshot` 的调用方已经持锁，且已经把那份列表读在手上了。
    fn provider_summary(&self, state: &AccountState) -> Value {
        let counts: Vec<(String, usize)> = state
            .accounts
            .iter()
            .fold(Vec::new(), |mut acc, record| {
                let provider = record.provider();
                match acc.iter_mut().find(|(id, _)| *id == provider) {
                    Some((_, count)) => *count += 1,
                    None => acc.push((provider, 1)),
                }
                acc
            });
        let value = crate::server::core::providers::summary_json(|id| {
            counts
                .iter()
                .find(|(known, _)| known == id)
                .map(|(_, count)| *count)
                .unwrap_or(0)
        });
        Value::Array(value)
    }

    // ─── 按 provider 查询 ────────────────────────────────────

    /// 指定 provider 的启用账号（公开形态，按 priority、addedAt 升序）。
    ///
    /// 消费方：聚合目录判断「这一家现在有没有可用登录态」
    /// （`catalog::provider_available`）—— 直接调适配器会让「有清单但没账号」
    /// 的家被广告给客户端，所以聚合层必须能按 provider 查可用性。
    ///
    /// 转发选路**不用**本函数：`rotate::provider_accounts` 走的是公开快照
    /// 过滤（那份要**看得见禁用账号**，第三级「全禁用 → 503」的文案依赖它）。
    ///
    /// 「启用」口径与选路一致：`enabled !== false`**且**有凭证
    /// （`has_credentials`：小浣熊的桌面端实时账号记录里没有 token，但凭证在
    /// auth.json，同样算「有凭证」，见 `state.rs` 的说明）。返回值是**排序后**的：
    /// 调用方按序尝试即可，不必自己再排一遍 —— 优先级的相对顺序就是主备顺序。
    pub fn accounts_for_provider(&self, provider: &str) -> Vec<Value> {
        let _guard = self.guard();
        let records = self.records_for_provider(&_guard, provider);
        let mut records: Vec<StoredAccount> = records
            .into_iter()
            .filter(|record| record.enabled() && record.has_credentials())
            .collect();
        records.sort_by_key(StoredAccount::order_key);
        records
            .iter()
            .map(|record| self.public_account(record))
            .collect()
    }

    /// 自定义提供商账号的公开形态（`custom_accounts` 写入的记录 → 界面形状）。
    ///
    /// ── 为什么不是 workbuddy 兜底形状 ────────────────────────────
    /// 兜底形状（`to_public_account`）会补 uid / nickname / edition /
    /// enterprise 这些**只对 WorkBuddy 有意义**的字段 —— 自定义账号拿到它们
    /// 只会在界面上渲染出一排空值，还会诱导前端去读一个不存在的 edition。
    /// 独立形状让「这条账号有什么」如实反映「这条记录存了什么」。
    ///
    /// ── `baseUrl` 的缺省语义 ────────────────────────────────────
    /// 记录里的 `baseUrl` 是**覆盖项**（缺省时转发回落到提供商的 baseUrl），
    /// 因此记录里没有这个键时公开形态也不带它 —— 前端据「键是否存在」区分
    /// 「未覆盖（跟随提供商）」与「覆盖成了某个值」，不能把空串/null 塞进来
    /// 让两种语义混在一起。
    ///
    /// `hasCredentials` / `chatSupported` / `checkinAt` 由 [`Self::public_account`]
    /// 统一注入（跨家事实，见那里的说明）；自定义账号的 `hasCredentials`
    /// 判据是「`apiKey` 非空 **或** 声明了无需鉴权」（见 `state::no_auth`），
    /// 两者都没有 = 未配置凭证，界面据此标出「转发会被跳过」。
    pub(crate) fn to_custom_public_account(&self, record: &StoredAccount) -> Value {
        let fields = record.fields();
        let mut public = Map::new();
        public.insert("id".to_string(), Value::String(record.id().to_string()));
        public.insert("provider".to_string(), Value::String(record.provider()));
        public.insert("name".to_string(), Value::String(record.name()));
        // apiKey 的尾号（与各家的 tokenTail 同一展示语义；空 key 时是空串）
        public.insert(
            "tokenTail".to_string(),
            value_or(fields.get("tokenTail"), Value::String(String::new())),
        );
        // ── 凭证形态的两个读数（界面「设置」里改凭证要用）──────────
        // `hasApiKey` 恒给一个布尔：输入框的提示文案要区分「已配置，留空则
        // 不修改」与「还没配」；`noAuth` 恒给布尔（缺键即 false）—— 开关的
        // 初值由它决定。**apiKey 本身绝不透出**（值与尾号两条管道分开，
        // 见 `custom_credential_by_id` 的说明）。
        public.insert("hasApiKey".to_string(), Value::Bool(record.has_api_key()));
        public.insert("noAuth".to_string(), Value::Bool(record.no_auth()));
        // 覆盖项：记录里**写了键**才透出（缺键 = 未覆盖，语义见函数头）
        if let Some(base_url) = fields.get("baseUrl") {
            public.insert("baseUrl".to_string(), base_url.clone());
        }
        public.insert("source".to_string(), Value::String(record.source()));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert("proxy".to_string(), describe_account_proxy(Some(&record.proxy())));
        // 「可用」= 凭证完整（与各家同一语义，见 `accounts-shared` 的 available 说明）。
        // 自定义账号这一项以前恒 true，于是「没填 Key 也没勾无需鉴权」的账号照样出现在
        // 「模型来源」下拉里（界面按 `available !== false` 过滤，见 models-fetch-modal）
        // ——它去拉清单必然报「请先添加账号」，那条选项本身就是误导。
        // 现在按 `has_credentials` 如实给：这类账号在界面上显示为「未配置凭证」。
        public.insert(
            "available".to_string(),
            Value::Bool(record.enabled() && record.has_credentials()),
        );
        // 单账号并发上限（与 to_public_account 同口径，兜底共用
        // `max_concurrent_public`）：0 = 不限，缺键同样输出 0
        public.insert(
            "maxConcurrent".to_string(),
            Value::from(max_concurrent_public(fields.get("maxConcurrent"))),
        );
        Value::Object(public)
    }
}

/// 记录上的 `usageQuery` → 公开形态的规范化形状（恒为对象）。
///
/// 读侧只做**容错展开**、不校验：合法性由写入侧（`apply_patch` 的
/// `normalize_usage_query`）保证，这里负责把缺失 / 脏值收敛成缺省形状，让
/// 调度与前端都按「恒定形状」读。缺省与 `usage_records::query_interval_of`
/// 同一口径 —— **未配置 = 开启、1 分钟**（对齐全局任务时代「默认就在查」的
/// 行为，升级后余额列不停更），只有显式 `{enabled:false}` 才是关。
fn usage_query_public(value: Option<&Value>) -> Value {
    use crate::server::core::usage_records::{
        DEFAULT_QUERY_INTERVAL_SECONDS, MAX_QUERY_INTERVAL_SECONDS, MIN_QUERY_INTERVAL_SECONDS,
    };
    let Some(fields) = value.and_then(Value::as_object) else {
        return json!({ "enabled": true, "interval": DEFAULT_QUERY_INTERVAL_SECONDS });
    };
    let enabled = fields.get("enabled").map(js_truthy).unwrap_or(true);
    let interval = fields.get("interval").and_then(Value::as_i64).unwrap_or(0);
    // 开着但间隔缺失 / 越界（脏值）→ 缺省间隔；关着则原样透出（0 = 未设过）
    let interval = if enabled
        && !(MIN_QUERY_INTERVAL_SECONDS..=MAX_QUERY_INTERVAL_SECONDS).contains(&interval)
    {
        DEFAULT_QUERY_INTERVAL_SECONDS
    } else {
        interval
    };
    json!({ "enabled": enabled, "interval": interval })
}

/// 记录上的 `lowBalance` → 公开形态的规范化形状。
///
/// 缺省（无配置）按 provider 区分（与 `usage_records::balance_blocked` 同一口径，
/// 见 `default_low_balance_mode`）：Cline 免费池 = `off`（不处理），其余 = `skip`、
/// 阈值 1；显式配置一律原样尊重 —— 那是用户选过的，缺省不能覆盖它。skip / disable
/// 档下阈值缺失或非法时回落缺省 1。
fn low_balance_public(provider: &str, value: Option<&Value>) -> Value {
    use crate::server::core::usage_records::{default_low_balance, default_low_balance_mode, DEFAULT_LOW_BALANCE_THRESHOLD};
    let default_mode = default_low_balance_mode(provider);
    let Some(fields) = value.and_then(Value::as_object) else {
        return default_low_balance(provider);
    };
    let mode = fields.get("mode").and_then(Value::as_str).unwrap_or(default_mode);
    let threshold = fields
        .get("threshold")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_LOW_BALANCE_THRESHOLD);
    match mode {
        "skip" => json!({ "mode": "skip", "threshold": threshold }),
        "disable" => json!({ "mode": "disable", "threshold": threshold }),
        _ => json!({ "mode": "off", "threshold": 0.0 }),
    }
}
