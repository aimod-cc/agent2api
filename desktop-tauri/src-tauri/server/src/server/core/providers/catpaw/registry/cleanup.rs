//! 会话注册表的**移除与作废**（`registry/mod.rs` 的子模块）：记录从表里消失的全部路径。
//!
//! 内容：
//!   - TTL 过期清理（`cleanup_expired`）与容量淘汰（`evict_lru`）；
//!   - 私有移除内核：`Inner::invalidate`（键 = x-session-id）与 `Inner::remove_anonymous`
//!     （键 = conversationId）—— 两者同形，都负责撤 call 索引、清序号与身份旁表、打移除日志；
//!   - 四个公开作废入口：[`SessionRegistry::invalidate`]（decision.rs / conversation.rs 的
//!     常规路径）、[`SessionRegistry::invalidate_by_conversation`]、
//!     [`SessionRegistry::clear_all`]（全局重置语义）、[`SessionRegistry::clear_account`]
//!     （账号切换的精细版）；
//!   - 匿名工具会话的登记（`register_anonymous`）与 call 索引 / 序号维护（`index_calls` /
//!     `clear_call_index` / `touch`）。
//!
//! 职责边界：本文件只执行移除，**不判定**该不该作废（判据在 `identity.rs` 与父模块的
//! `resolve`），按调用方给的 [`InvalidationReason`] 落日志。
//!
//! 搬移说明：内容与原单文件版逐字相同，唯一例外是 `clear_account` 的说明里那条
//! `[`AccountIdentity`]` 链接 —— 该类型现在定义在兄弟模块 `identity.rs`，所以给它补了
//! 显式目标 `(super::AccountIdentity)`（只补链接目标，文案不变；单独 import 那个类型
//! 会被 `unused_imports` 报未使用，因为在代码里用不到它）。

use super::{short_id, Inner, InvalidationReason, SessionRecord, SessionRegistry};

use crate::server::logging;

impl SessionRegistry {
    /// 作废一条客户端会话（原实现 `removeClientSession`）。返回被移除的记录。
    ///
    /// 同时清掉 call 索引 —— 少了这一步，下次带旧 tool_call_id 的请求会
    /// 命中一条已经不存在的 conversationId，直接发给上游得到 4xx/5xx。
    pub fn invalidate(
        &self,
        session_id: &str,
        reason: InvalidationReason,
    ) -> Option<SessionRecord> {
        if session_id.is_empty() {
            return None;
        }
        self.lock().invalidate(session_id, reason)
    }

    /// 按 conversationId 作废（工具会话的清理路径；原实现
    /// `forgetClientToolSession` 按 conversationId 删 pending 表）。
    ///
    /// ── 为什么 W5-T-d4 之后仍无人调用 ─────────────────────────
    /// `conversation.rs` 的作废路径都按 `x-session-id` 走（`invalidate`）：
    /// 它手上总有会话 id，且 call 索引与匿名表由 `Inner::invalidate` 统一清。
    /// 本方法服务的是「只拿得到 conversationId」的入口 —— 原实现的
    /// `forgetClientToolSession` 就是那种形态（工具会话的收尾只知道
    /// conversationId）。本网关的失败路径不需要它（客户端会重发完整历史，
    /// 规则 3 自然接手），保留是为了与账号切换/工具会话收尾的将来入口对齐，
    /// 以及排障时手工清一条会话。摘除条件：明确决定只按 session-id 作废时删掉。
    #[allow(dead_code)]
    pub fn invalidate_by_conversation(
        &self,
        conversation_id: &str,
        reason: InvalidationReason,
    ) -> Option<SessionRecord> {
        if conversation_id.is_empty() {
            return None;
        }
        let mut inner = self.lock();
        let session_id = inner
            .sessions
            .values()
            .find(|record| record.conversation_id == conversation_id)
            .map(|record| record.session_id.clone());
        if let Some(session_id) = session_id {
            return inner.invalidate(&session_id, reason);
        }
        inner.remove_anonymous(conversation_id, reason)
    }

    /// 整表作废（原实现 `clearClientToolSessions`；**账号切换**时调用）。
    ///
    /// 为什么账号切换必须整表清：conversationId 是**上游账号上下文里**的
    /// 对象，换账号后旧 id 要么不存在、要么属于别的用户，续接必然失败
    /// （轻则报错，重则把两个账号的会话搅在一起）。
    /// 返回值是清掉的记录条数（供日志）；`generation` +1。
    ///
    /// ── 为什么本网关的账号切换点用的是 `clear_account` 而不是它 ──
    /// 原项目是单账号语义（整表清是对的）；本网关多账号并存，整表清会误伤
    /// 其他账号的会话，所以 W5-T-d4 的四个挂点（删除 / 禁用 / 重新导入 /
    /// 批删批禁）都走 `clear_account`。本方法保留给「全局重置」语义 ——
    /// 原项目 `notifySwitch` 的等价物（例如将来一个「清空全部会话」的维护动作），
    /// 以及排障。它的行为已被 `clear_account` 完全覆盖，删掉不影响任何生产路径。
    /// 摘除条件：明确决定只保留按账号作废时删掉。
    #[allow(dead_code)]
    pub fn clear_all(&self) -> usize {
        let mut inner = self.lock();
        let count = inner.sessions.len() + inner.anonymous_tool_sessions.len();
        inner.sessions.clear();
        inner.anonymous_tool_sessions.clear();
        inner.call_index.clear();
        inner.last_sequence.clear();
        // 身份旁表整表清（它按 conversationId 索引，记录没了就不该留着）
        inner.record_identities.clear();
        inner.generation = inner.generation.wrapping_add(1);
        count
    }

    /// 只作废属于某账号的记录（账号切换的**精细版**：其他账号的会话不受影响）。
    ///
    /// 与 `clear_all` 的关系：原实现只有整表清（那时是单账号语义）。
    /// 多账号并存时整表清会误伤其他账号的会话，因此额外提供这一条；
    /// 调用方（T-d4 的账号切换点）按「切走的那个账号」精细作废即可。
    /// 返回清掉的记录条数。
    ///
    /// `account_id` 为空 → 作废**无账号身份**的记录（环境变量 / 默认登录态建的）：
    /// 空是显式身份（见 [`AccountIdentity`](super::AccountIdentity)），所以这条路径是「默认登录态换了
    /// 用户」的清理口。改动前它直接返回 0（空当通配 → 没有「空账号」这个语义），
    /// 于是那批记录永远没人能作废。
    pub fn clear_account(&self, account_id: &str) -> usize {
        let mut inner = self.lock();
        let session_ids: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(_, record)| record.account_id == account_id)
            .map(|(session_id, _)| session_id.clone())
            .collect();
        let mut count = 0;
        for session_id in session_ids {
            if inner.invalidate(session_id.as_str(), InvalidationReason::AccountSwitch).is_some() {
                count += 1;
            }
        }
        let anonymous: Vec<String> = inner
            .anonymous_tool_sessions
            .iter()
            .filter(|(_, record)| record.account_id == account_id)
            .map(|(conversation_id, _)| conversation_id.clone())
            .collect();
        for conversation_id in anonymous {
            if inner
                .remove_anonymous(conversation_id.as_str(), InvalidationReason::AccountSwitch)
                .is_some()
            {
                count += 1;
            }
        }
        count
    }
}

impl Inner {
    /// 清掉已过期的记录（原实现 `cleanupExpiredSessions`，现在是整表扫）
    pub(super) fn cleanup_expired(&mut self, now: i64) {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, record)| record.is_expired(now))
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in expired {
            self.invalidate(session_id.as_str(), InvalidationReason::Expired);
        }
        let expired_anonymous: Vec<String> = self
            .anonymous_tool_sessions
            .iter()
            .filter(|(_, record)| record.is_expired(now))
            .map(|(conversation_id, _)| conversation_id.clone())
            .collect();
        for conversation_id in expired_anonymous {
            self.remove_anonymous(conversation_id.as_str(), InvalidationReason::Expired);
        }
    }

    /// 移除一条客户端会话（含 call 索引、序号与身份记录），打日志
    pub(super) fn invalidate(
        &mut self,
        session_id: &str,
        reason: InvalidationReason,
    ) -> Option<SessionRecord> {
        let removed = self.sessions.remove(session_id)?;
        self.clear_call_index(&removed);
        self.last_sequence.remove(removed.conversation_id.as_str());
        self.forget_identity(removed.conversation_id.as_str());
        log_removal(&removed, reason);
        Some(removed)
    }

    /// 移除一条匿名工具会话（含 call 索引、序号与身份记录），打日志。
    /// 与 [`Inner::invalidate`] 同形，只是索引键是 conversationId。
    pub(super) fn remove_anonymous(
        &mut self,
        conversation_id: &str,
        reason: InvalidationReason,
    ) -> Option<SessionRecord> {
        let removed = self.anonymous_tool_sessions.remove(conversation_id)?;
        self.clear_call_index(&removed);
        self.last_sequence.remove(conversation_id);
        self.forget_identity(conversation_id);
        log_removal(&removed, reason);
        Some(removed)
    }

    /// 表满时淘汰最久未活动的一条（原实现按 `updatedAt` 取最小）
    pub(super) fn evict_lru(&mut self) {
        let victim = self
            .sessions
            .values()
            .min_by_key(|record| {
                self.last_sequence
                    .get(record.conversation_id.as_str())
                    .copied()
                    .unwrap_or(u64::MIN)
            })
            .map(|record| record.session_id.clone());
        if let Some(victim) = victim {
            self.invalidate(victim.as_str(), InvalidationReason::EvictedCapacity);
        }
    }

    /// 登记匿名工具会话（按 conversationId 索引）。
    ///
    /// 匿名表只有「工具会话」一种用途（客户端没给 x-session-id），
    /// 同一 conversationId 被重复登记就是覆盖写 —— 覆盖前要把旧记录的
    /// call 索引撤掉（`pending_call_ids` 可能变了，留着旧 id 会让
    /// 后续请求命中一个已经不存在的待响应集合）。
    pub(super) fn register_anonymous(&mut self, record: SessionRecord) {
        let conversation_id = record.conversation_id.clone();
        if let Some(previous) = self.anonymous_tool_sessions.insert(conversation_id.clone(), record) {
            self.clear_call_index(&previous);
        }
        if let Some(current) = self.anonymous_tool_sessions.get(&conversation_id).cloned() {
            self.touch(conversation_id.as_str());
            self.index_calls(&current);
        }
    }

    /// 把记录的待响应 tool_call_id 写进索引
    pub(super) fn index_calls(&mut self, record: &SessionRecord) {
        for call_id in &record.pending_call_ids {
            if !call_id.is_empty() {
                self.call_index.insert(call_id.clone(), record.conversation_id.clone());
            }
        }
    }

    /// 撤掉记录的 call 索引（只删仍指向它的那些）
    pub(super) fn clear_call_index(&mut self, record: &SessionRecord) {
        for call_id in &record.pending_call_ids {
            if self.call_index.get(call_id).map(String::as_str) == Some(record.conversation_id.as_str())
            {
                self.call_index.remove(call_id);
            }
        }
    }

    /// 打一次活动时间戳（LRU 的单调序号 +1）
    pub(super) fn touch(&mut self, conversation_id: &str) {
        self.sequence = self.sequence.wrapping_add(1);
        self.last_sequence.insert(conversation_id.to_string(), self.sequence);
    }
}

/// 会话被移除时的日志（对应原实现 `writeDiagnostic({event:'session-remove'})`）。
///
/// 只打**元数据**（会话 id 前 8 位、conversationId 前 8 位、指纹条数、原因），
/// 不含消息正文 —— 与 UPSTREAM_PROTOCOL §8 的隐私口径一致。
/// 会话 id 截前 8 位同样是原实现的做法（够区分、又不落完整标识）。
fn log_removal(record: &SessionRecord, reason: InvalidationReason) {
    logging::verbose(
        "CatPaw",
        &format!(
            "session-remove reason={} sessionId={} conversationId={} syncedCount={}",
            reason.as_str(),
            short_id(record.session_id.as_str()),
            short_id(record.conversation_id.as_str()),
            record.fingerprints.len(),
        ),
    );
}
