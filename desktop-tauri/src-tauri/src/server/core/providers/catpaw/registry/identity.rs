//! 会话注册表的**账号身份归属**（`registry/mod.rs` 的子模块）。
//!
//! 「这条 conversation 属于哪个用户」是复用判定的第二个维度：`account_id`
//! 是网关照账号记录生成/选路的标识，而桌面端实时登录态
//! （`~/.meituan-catpaw/auth.json`）由 CatPaw 客户端自己维护 —— 用户可以在客户端里
//! 换一个账号登录，那时 account_id（`desktop-auth` 或空）不变，变的只有 uid/loginName。
//! 本模块负责这个维度的全部判定与清理。
//!
//! 内容：
//!   - [`AccountIdentity`]：身份定义（账号 id + 凭据里的用户标识），只做相等判定；
//!   - `Inner` 的身份旁表与判据：`recorded_identity` / `current_identity` /
//!     `identity_consistent`（唯一判据）/ `forget_identity`；
//!   - [`SessionRegistry::reconcile_identity`]：转发选路时的**账号级**对账，把
//!     「account_id 没变但用户变了」名下已建的会话一并作废。
//!
//! 身份归属的完整说明（为什么要有第二个维度、锁与 panic 纪律）见父模块 `registry/mod.rs`
//! 的模块头；本文件只调 `Inner` 的方法与父模块的 `short_id`。

use super::{short_id, Inner, InvalidationReason, SessionRecord, SessionRegistry};

use crate::server::logging;

/// 账号身份的第二个维度：**这条 conversation 属于哪个用户**。
///
/// 与 `account_id` 的区别：account_id 是网关照账号记录生成/选路的标识，
/// 而桌面端实时登录态（`~/.meituan-catpaw/auth.json`）由 CatPaw 客户端自己维护
/// —— 用户可以在客户端里换一个账号登录，而账号记录（`desktop-auth`）与
/// 「无账号 → 默认登录态」的选路结果都不变。那时只有 uid / loginName 变了。
///
/// 值取本次请求**实际使用的凭证**里的用户标识（`CatPawCredentials.uid`，
/// 上游 `user-uid` 头的同一个值）。空串 = 本次凭证里没有用户标识（例如
/// `CATPAW_COOKIE` 没配 `CATPAW_USER_UID`），同样是显式身份，只匹配空。
///
/// 为什么不直接拿 token 比对：token 每次登录都会换（即使还是同一个人），
/// 拿它比对会把「同一个人重新登录」也判成换号，白白重建一轮会话。uid 才是
/// 「这是哪个用户」的稳定标识 —— 它也是上游用来区分账号的请求头。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountIdentity {
    /// 账号 id（空 = 无账号身份）
    pub account_id: String,
    /// 用户标识（uid / loginName；空 = 凭证里没有用户标识）
    pub user_id: String,
}

impl AccountIdentity {
    /// 从账号 id 与凭证里的用户标识构造
    pub fn new(account_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self { account_id: account_id.into(), user_id: user_id.into() }
    }

    /// 两个身份是否一致（相等判定；空串参与比较，不是通配）
    pub fn matches(&self, other: &Self) -> bool {
        self.account_id == other.account_id && self.user_id == other.user_id
    }

    /// 日志用（只打 id 与 uid 的短前缀，与 `log_removal` 同一条隐私口径）
    fn describe(&self) -> String {
        format!(
            "account={} uid={}",
            if self.account_id.is_empty() { "-" } else { short_id(&self.account_id) },
            if self.user_id.is_empty() { "-" } else { short_id(&self.user_id) },
        )
    }
}

impl SessionRegistry {
    /// **身份对账**（转发选路时调，见模块头的身份归属一节）。
    ///
    /// 语义：`identity` 是本次请求**实际使用的凭证身份**（uid / loginName）。
    /// 与注册表里记着的该账号身份比对，不一致 = 用户在客户端侧换了号
    /// （桌面端实时登录态由 CatPaw 客户端维护，account_id 不变），
    /// 该账号名下的会话全部作废（`identity-changed`）—— 它们的 conversationId
    /// 属于上一个用户的上游上下文。
    ///
    /// ── 为什么要有它（`resolve` 的身份判定不够吗）───────────────
    /// `resolve` 只看得见**当前这条** x-session-id：客户端换号后如果换了
    /// 一个 session id（新对话），旧记录不会被它碰到，就一直躺在表里等着
    /// 被某个复用同一 session id 的请求（或工具续接的 `lookup_by_call_id`）
    /// 捡走。选路时的对账是**账号级**的清扫：换号后的第一次请求就把该账号
    /// 名下所有陈旧会话清干净。
    ///
    /// ── 性能（每个请求都调，但不能是重活）───────────────────────
    /// 第一次对账记下身份；**后续请求身份不变时 O(1) 早退**（一次 HashMap 查 +
    /// 一次比较），不扫表。只有真的变了才扫一遍该账号的记录（≤ MAX_CLIENT_SESSIONS
    /// = 200 条，纯内存比较）。身份从**本次请求的凭证**来（调用方已经在手上，
    /// 没有额外读盘），因此持锁期间仍然只有内存操作。
    ///
    /// ── 与「账号被删/禁用」路径的关系 ───────────────────────────
    /// 那条路径由账号层的 `invalidate_catpaw_sessions` → `clear_account` 负责
    /// （作废的判据是 account_id）；本条负责 account_id **看不出变化**的换号。
    /// 两者互补，都不覆盖对方。
    ///
    /// 返回作废的记录条数（供日志）。
    /// ── 锁与 IO（如实说明，不要照抄模块头的理想说法）────────────
    /// 本方法**新增**的日志（那条汇总）在放锁之后打，临界区里只有内存操作。
    /// 但逐条移除仍走 [`Inner::invalidate`] / [`Inner::remove_anonymous`]，
    /// 它们内部的 `log_removal` 会在持锁期间经 `logging::verbose` 落一条日志
    /// —— 这是**所有**作废路径的既有行为（TTL 清理、LRU 淘汰、模型不符都这样），
    /// 不是本方法引入的，也没有在本方法里放大：只有身份**真的变了**才会走到。
    /// 要彻底消除它得改 `log_removal` 的调用约定（作废收集 + 放锁后统一打日志），
    /// 那是注册表全体的改动，不在本次「身份归属」的最小修复范围内。
    pub fn reconcile_identity(&self, identity: &AccountIdentity) -> usize {
        let count = self.reconcile_identity_locked(identity);
        if count > 0 {
            // 克隆与格式化都只在这条「真的换了号」的支路上发生
            let account_id = identity.account_id.as_str();
            logging::verbose(
                "[CatPaw]",
                &format!(
                    "账号 {} 的登录身份已变化（{}），作废其名下会话映射 {} 条",
                    if account_id.is_empty() { "(默认登录态)" } else { account_id },
                    identity.describe(),
                    count,
                ),
            );
        }
        count
    }

    /// [`Self::reconcile_identity`] 的临界区部分（持锁期间只碰内存）
    fn reconcile_identity_locked(&self, identity: &AccountIdentity) -> usize {
        let mut inner = self.lock();
        let account_id = identity.account_id.as_str();
        // 身份没变 → O(1) 早退（绝大多数请求走这一支；这一步不分配）
        if inner.current_identities.get(account_id).map(|known| known.matches(identity))
            == Some(true)
        {
            return 0;
        }
        // 身份变了（或第一次见到该账号）：先记下新身份，再清掉属于旧身份的会话
        let previous = inner
            .current_identities
            .insert(account_id.to_string(), identity.clone());
        if previous.is_none() {
            // 第一次见到该账号：表里那些记录**没有身份信息**（本进程还没学过
            // 它的身份），不能假定它们属于谁 —— 只记身份，不动记录。
            // 它们会被 `resolve` 按账号维度判定（account_id 空/非空精确匹配）。
            return 0;
        }
        let mut count = 0;
        // 1) 长会话表：属于该账号、且身份与本次不一致的记录
        let victims: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(_, record)| record.account_id == account_id)
            .filter(|(_, record)| {
                inner
                    .record_identities
                    .get(record.conversation_id.as_str())
                    .map(|recorded| !recorded.matches(identity))
                    // 没记身份的记录：账号级对账**不动**它（无从判断属于谁），
                    // 交给 `resolve` 的身份判定按需处理
                    .unwrap_or(false)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in victims {
            if inner.invalidate(session_id.as_str(), InvalidationReason::IdentityChanged).is_some() {
                count += 1;
            }
        }
        // 2) 匿名工具会话（无 x-session-id 的那批，同样按账号 + 身份筛）
        let anonymous: Vec<String> = inner
            .anonymous_tool_sessions
            .iter()
            .filter(|(_, record)| record.account_id == account_id)
            .filter(|(_, record)| {
                inner
                    .record_identities
                    .get(record.conversation_id.as_str())
                    .map(|recorded| !recorded.matches(identity))
                    .unwrap_or(false)
            })
            .map(|(conversation_id, _)| conversation_id.clone())
            .collect();
        for conversation_id in anonymous {
            if inner
                .remove_anonymous(conversation_id.as_str(), InvalidationReason::IdentityChanged)
                .is_some()
            {
                count += 1;
            }
        }
        count
    }
}

impl Inner {
    /// 一条会话**建立时**的账号身份（没记录过 → None，见 `register` 的说明）
    fn recorded_identity(&self, conversation_id: &str) -> Option<&AccountIdentity> {
        self.record_identities.get(conversation_id)
    }

    /// 某账号**本次请求**的身份（`reconcile_identity` 写入；没见过 → None）
    fn current_identity(&self, account_id: &str) -> Option<&AccountIdentity> {
        self.current_identities.get(account_id)
    }

    /// 一条记录的身份是否与「本次请求的身份」一致。
    ///
    /// 这是身份维度的**唯一判据**：`resolve`（按 session-id 续接）与
    /// `lookup_by_call_id`（按 tool_call_id 续接）都走它 —— 两条路径必须给同一
    /// 结论，否则同一条记录会在「长会话新轮次」与「工具续接」上有两种判定。
    /// （`lookup_by_conversation` 是排障用的只读入口，不设门槛：诊断时要能捞出
    /// 一条已换号的记录看指纹链。）
    ///
    /// 两侧任一缺失即视为一致（**不做身份判定**）：记录没被记过身份（旧记录 /
    /// 本进程还没学到），或该账号本次还没对过账（`reconcile_identity` 没被调过）
    /// —— 这两种情况下都没有可比对的两方，退回账号维度的判定比误判成「换号」安全。
    pub(super) fn identity_consistent(&self, record: &SessionRecord) -> bool {
        match (
            self.recorded_identity(record.conversation_id.as_str()),
            self.current_identity(record.account_id.as_str()),
        ) {
            (Some(recorded), Some(current)) => recorded.matches(current),
            _ => true,
        }
    }

    /// 忘掉一条会话的身份（记录被移除时调，否则 `record_identities` 会随
    /// 「建了又删」的会话单调增长 —— 表本身有 TTL/LRU 上限，这张旁表没有）
    pub(super) fn forget_identity(&mut self, conversation_id: &str) {
        self.record_identities.remove(conversation_id);
    }
}
