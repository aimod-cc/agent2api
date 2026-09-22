//! CatPaw 会话注册表（Agent2API 二期 W3p-T-d1；移植来源 `session-registry.mjs`）。
//! **内存数据结构：不读盘、不发网络请求。**
//!
//! ── 它解决什么问题 ──────────────────────────────────────────
//! CatPaw 上游是**有状态会话协议**（UPSTREAM_PROTOCOL §5）：一轮对话由
//! `round`（提交消息）+ `turn`（SSE 执行）+ 工具循环 + `event(completed)`
//! 组成，全程围绕一个 `conversationId`。而客户端看到的是无状态 OpenAI 协议，
//! 每轮都提交完整历史 —— 于是代理必须自己记住：
//!
//! | 记住什么 | 回答什么问题 |
//! |---|---|
//! | `x-session-id` → conversationId | 这次请求该复用哪个 conversation（还是新建） |
//! | 已同步消息的**指纹链** | 客户端历史里哪一段上游已经见过（增量从哪开始） |
//! | `modelType` | 模型换了没有（换了必须重建 conversation：不同模型不可续接） |
//! | 待响应的 `tool_call_id` | 这次请求是「工具续接」（turn 提交 tool 结果，不 round） |
//! | 在途占用 | 同 session 已有流式请求在跑（新请求要走独立 conversation） |
//!
//! 这些状态**只在进程内**：进程重启后注册表为空，客户端下一轮自然走
//! 「全新会话全量 round」（UPSTREAM_PROTOCOL §5 的规则 3），功能不受影响，
//! 只是那一轮多传一次历史。因此本模块没有持久化需求（原实现也是内存 Map）。
//!
//! ── 谁在用（分工）───────────────────────────────────────────
//! `conversation.rs`（T-d3）是唯一消费方：它按 §9.2 的三条规则判定轮次模式，
//! 过程中调 `resolve` / `locate_increment`（在 `fingerprint.rs`）/ `register`。
//! 本模块**不做轮次判定**（那是状态机的职责），只提供「查、存、失效、占用」
//! 四个动作 —— 于是并发占用与失效原因这类容易写错的部分只有一份实现。
//!
//! ── 账号身份归属（两个维度的「这条 conversation 属于谁」）─────
//! 一个 conversationId 建在**上游账号上下文**里，续接要求上下文完全一致。
//! 上下文由两个维度组成，两个都参与匹配，且都**只做相等判定**：
//!
//! | 维度 | 值 | 从哪来 | 什么时候变 |
//! |---|---|---|---|
//! | 账号 id | `SessionRecord::account_id`（空串 = 无账号身份：环境变量旁路 / 默认登录态） | 编排层选路结果 | 选到别的账号、账号被删/禁用 |
//! | 账号身份 | `AccountIdentity`（uid / loginName），存在 `Inner::record_identities`（按 conversationId）与 `Inner::current_identities`（按 account_id） | 本次请求**实际使用的凭证** | 同一 account_id 底下换了用户 |
//!
//! 空串是**显式身份**而不是通配：把空串当通配会让真实账号建的记录在账号被
//! 删/禁用、请求回落到默认登录态（account_id 为空）之后仍被复用，续接到一个
//! 已经不存在的账号上下文里。
//!
//! 第二个维度覆盖「本网关不知情的换号」：桌面端实时登录态
//! （`~/.meituan-catpaw/auth.json`）由 CatPaw 客户端维护，用户可以在客户端里
//! 直接换一个账号登录 —— 那时 account_id（`desktop-auth` 或空）不变，变的只有
//! uid/loginName，注册表按 account_id 看不出任何异常。因此：
//!   - [`SessionRegistry::mark_inflight`] 连同**本次请求的身份**一起占用；
//!   - [`SessionRegistry::register`] 把这条身份记到 conversationId 上；
//!   - [`SessionRegistry::resolve`] 比对「记录身份 ↔ 本次请求身份」，不一致即作废；
//!   - [`SessionRegistry::reconcile_identity`] 在转发选路时按账号扫一遍，
//!     把身份不符的记录一并作废（客户端换号后**下一次请求**就失效）。
//!
//! ── 锁与 panic 纪律 ────────────────────────────────────────
//! 单进程内存表，用一把 `std::sync::Mutex` 保护。**临界区里只做内存操作**
//! （克隆记录、增删表项），绝不做 IO、绝不调外部代码（架构文档 §8 第 7 条
//! 是同一条纪律的账号版）。锁中毒（持锁线程 panic）时取回内部数据继续用
//! （与 `egress.rs` 同一处理）——release 是 panic=abort，中毒路径本就不可达，
//! 但不能因为「不可达」就写 unwrap（§8 第 5 条）。
//!
//! ── TTL 与容量（常量照抄原实现）────────────────────────────
//! 原实现有两档 TTL：长会话 2 小时、工具会话 10 分钟（见下方常量）。
//! 容量上限 200，超限淘汰**最久未活动**的一条（LRU，原实现按 `updatedAt`
//! 排序取最小值）。
//!
//! ── 子模块分工（本文件原为单文件，按职责拆成下面三个）────────
//!   mod.rs       常量与记录结构（`SessionRecord` 等）、`SessionRegistry` 句柄与
//!                公开 API（占用 / 解析 / 登记 / 查询 / 排障）
//!   identity.rs  账号身份归属：`AccountIdentity` + 身份旁表判据 + 账号级身份对账
//!   cleanup.rs   移除与作废：TTL 清理、LRU 淘汰、索引与序号维护、四个作废入口、移除日志
//!
//! 拆分只做搬移（函数体逐字未改，只调整了必要的 `use` 与 `pub(super)` 可见性；
//! 唯一例外是 `cleanup.rs` 里一条文档链接的显式目标，见该文件的模块头）。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use crate::server::logging;

mod cleanup;
mod identity;

// `AccountIdentity` 的定义在子模块 `identity.rs`；re-export 出去，对外路径仍是
// `registry::AccountIdentity`（adapter.rs / conversation.rs 的 use 不用动）。
pub use identity::AccountIdentity;

/// 长会话（带 `x-session-id` 的普通轮次）的存活时间：2 小时
/// （原实现 `CLIENT_SESSION_TTL_MS = 2 * 60 * 60 * 1000`）。
///
/// 为什么是 2 小时：用户对话的「连续感」窗口。超过这个时长没有新轮次，
/// 那条客户端会话基本结束了，保留 conversationId 只会占内存。
/// 过期后客户端再来就是全新会话（全量 round）—— 行为正确，只是多传一次历史。
pub const CLIENT_SESSION_TTL_MS: i64 = 2 * 60 * 60 * 1000;

/// **工具续接**会话的存活时间：10 分钟（原实现
/// `CLIENT_TOOL_SESSION_TTL_MS = 10 * 60 * 1000`）。
///
/// 比长会话短得多，因为「工具结果」是**紧接着**执行完就要提交的东西：
/// 本地工具执行完立刻发下一条请求，中间不会有几分钟的空档。
/// 客户端超过 10 分钟没把工具结果交回来，说明这一轮已经放弃（用户打断并
/// 改问别的），继续保留那条待响应记录会让下一次请求误判成「工具续接」。
pub const CLIENT_TOOL_SESSION_TTL_MS: i64 = 10 * 60 * 1000;

/// 注册表容量上限（原实现 `MAX_CLIENT_SESSIONS = 200`）。
///
/// 每条记录 = conversationId + 指纹链（几十条消息的 64 字节摘要）；
/// 200 条的上界是 MB 量级，对桌面应用完全可接受。
/// 之所以要上限而不是无界：客户端每次都发一个新的 `x-session-id` 时
/// （比如某些工具型客户端），无界表会随请求数单调增长。
pub const MAX_CLIENT_SESSIONS: usize = 200;

/// 一条会话记录（对应原实现里 `clientSessions` / `pendingClientToolSessions`
/// 的会话对象 —— 两者在那边是两个 Map，字段却几乎相同，这里合成一个类型）。
#[derive(Clone, Debug)]
pub struct SessionRecord {
    /// 上游 conversationId（本轮之后要复用的那个）
    pub conversation_id: String,
    /// 已同步消息的指纹链（`fingerprint::message_fingerprint` 的产物，按序）。
    ///
    /// 语义：**最后一条已提交给上游的消息**的指纹（增量定位只看最后一条，
    /// 见 `fingerprint::locate_increment`）。每轮结束后由 T-d3 追加
    /// 「本轮提交的消息 + 上游返回的消息」的指纹。
    pub fingerprints: Vec<String>,
    /// 上游数字模型 ID（`modelType`）。与当前请求不一致 → 记录失效重建
    /// （`InvalidationReason::ModelMismatch`；不同模型不能续接同一 conversation）。
    pub model_type: i64,
    /// 这条会话是**哪个账号**建的。
    ///
    /// 原实现没有这个字段：它在账号切换时**主动**调 `clearClientToolSessions`
    /// 把表清空。Rust 侧保留字段多了两道保险（见 [`SessionRegistry::resolve`]）：
    /// 账号轮换可能发生在注册表不知情的地方（换账号重试、限额降级），
    /// 那时 conversationId 属于旧账号的上游，必须重建而不是续接。
    ///
    /// 空串 = **无账号身份**（环境变量旁路 / 默认登录态），是**显式身份**而不是
    /// 通配：空身份只匹配空身份（见 [`SessionRecord::belongs_to_account`] 与模块头
    /// 的身份归属一节）。账号身份的第二个维度 —— 同一 account_id 底下换了用户
    /// （桌面端实时登录态在客户端侧换号）—— 不在本字段里，由
    /// [`SessionRegistry::reconcile_identity`] 负责。
    pub account_id: String,
    /// 创建时间（毫秒）
    ///
    /// `#[allow(dead_code)]`：W5-T-d4 摘掉模块级抑制后编译器报出「只写不读」。
    /// 保留它的理由是**与另一家的会话记录同形**（`SessionRecord` 是给排障看的
    /// 快照，创建时间与最后活动时间是成对的信息），且它已经在 TTL 判定之外被
    /// `write_back` 写入落定 —— 删掉字段要改写入侧，而留着的代价只是一个 u64。
    /// 摘除条件：真正有人读它（例如「会话活了多久」的排障日志）时删掉本属性。
    #[allow(dead_code)]
    pub created_at: i64,
    /// 最后活动时间（毫秒）。TTL 与 LRU 都看它。
    pub last_active_at: i64,
    /// 是否被一个在途的流式请求占用（并发保护，见 [`SessionRegistry::mark_inflight`]）。
    ///
    /// **镜像字段**：权威来源是注册表内部的在途集合（新会话在登记记录之前
    /// 就已经占用了，那时还没有记录可写），本字段是「登记时把占用状态带进记录」
    /// 用的。判断「能不能发」请调 `mark_inflight`，不要读这个字段做决策。
    pub inflight: bool,
    /// 对应的客户端会话 id（`x-session-id`）。空串 = 匿名工具会话
    /// （客户端没给 x-session-id，只靠 tool_call_id 索引）。
    pub session_id: String,
    /// 待响应的工具调用 id（上游要求模型继续调工具时，T-d3 用它登记）。
    ///
    /// 非空 ⇔ 这条会话在等工具结果（原实现的 `session.running` 语义，
    /// 见 [`SessionRecord::is_awaiting_tool_results`]）。
    pub pending_call_ids: Vec<String>,
    /// 正在执行的那个 turn 的 id（用于打断：`turn/stop` 要带上它）。
    ///
    /// 场景（UPSTREAM_PROTOCOL §3.4）：上一轮的工具调用被客户端打断，
    /// 但 conversation 在上游仍处于「执行中」，必须先 `turn/stop` 掉旧轮次
    /// 才能创建新轮次（否则 round 被拒绝）。
    pub turn_request_id: Option<String>,
}

impl SessionRecord {
    /// 是否已过期（`now >= last_active_at + ttl`，与原实现的 `expiresAt <= now` 同）
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.last_active_at.saturating_add(self.ttl_ms())
    }

    /// 这条记录的 TTL。
    ///
    /// 规则照抄原实现：**带客户端会话 id 的一律 2 小时**（工具会话只要挂在
    /// 长会话上，就跟着长会话走）；只有**匿名工具会话**（客户端没给
    /// x-session-id，纯靠 tool_call_id 续接）才是 10 分钟。
    pub fn ttl_ms(&self) -> i64 {
        if self.session_id.is_empty() {
            CLIENT_TOOL_SESSION_TTL_MS
        } else {
            CLIENT_SESSION_TTL_MS
        }
    }

    /// 是否在等工具结果（原实现 `session.running`）
    pub fn is_awaiting_tool_results(&self) -> bool {
        !self.pending_call_ids.is_empty()
    }

    /// 该记录是否属于指定账号。
    ///
    /// **相等判定，空串不是通配**：空 = 无账号身份（环境变量 / 默认登录态），
    /// 只匹配空。这条修的是「真实账号建的记录在账号被删/禁用、请求回落到默认
    /// 登录态（account_id 为空）之后仍被复用」——`account_id.is_empty() ||` 那个
    /// 旧写法会让 [`SessionRegistry::resolve`] 的账号维度对空身份完全失效，
    /// 于是一条属于已消失账号的 conversationId 会被接着续接（模块头的身份归属
    /// 一节）。两个方向都不匹配：有账号的请求不复用无账号记录，反之亦然。
    fn belongs_to_account(&self, account_id: &str) -> bool {
        self.account_id == account_id
    }
}

/// 记录被作废/淘汰的原因（进日志；对应原实现传给 `removeClientSession` 的
/// `reason` 字符串 —— 那边的取值就这几个）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidationReason {
    /// 指纹对不上：客户端压缩/改写了历史（原实现 `sync-mismatch`）
    SyncMismatch,
    /// 换了模型：不同模型不能续接同一 conversation（原实现 `model-mismatch`）
    ModelMismatch,
    /// 账号对不上：conversationId 属于另一个账号的上游（本模块的相加保险）
    AccountMismatch,
    /// **同一个账号 id 底下换了用户**：桌面端实时登录态在客户端侧被换成了另一个
    /// 账号（uid / loginName 变了），该账号名下已有的 conversationId 属于上一个
    /// 用户的上游上下文（见 [`SessionRegistry::reconcile_identity`]）。
    /// 与 `AccountMismatch` 的区别：那条是「两个不同的 account_id」，这条是
    /// 「account_id 相同但凭证身份不同」—— 只有身份对账能发现。
    IdentityChanged,
    /// TTL 到期（原实现 `expired`）
    Expired,
    /// 容量淘汰（原实现 `evicted-capacity`）
    EvictedCapacity,
    /// 被同一客户端会话的新 conversation 顶替（原实现 `replaced-by-new-conversation`）
    Replaced,
    /// 账号切换：整表作废（原实现 `sessions-clear` + `account-switch`）
    AccountSwitch,
    /// 上游拒绝了这条 conversation，调用方要求作废（如 round 报「会话正在执行中」
    /// 且停止旧轮次也没救回来）；原实现没有这个原因，是给 T-d3 的显式失效入口
    UpstreamRejected,
    /// 调用方显式作废（工具续接结束、轮次终止时的清理）
    Explicit,
}

impl InvalidationReason {
    /// 日志与诊断用的字符串（与原实现的 reason 取值一致，便于对照日志）
    pub fn as_str(self) -> &'static str {
        match self {
            InvalidationReason::SyncMismatch => "sync-mismatch",
            InvalidationReason::ModelMismatch => "model-mismatch",
            InvalidationReason::AccountMismatch => "account-mismatch",
            InvalidationReason::IdentityChanged => "identity-changed",
            InvalidationReason::Expired => "expired",
            InvalidationReason::EvictedCapacity => "evicted-capacity",
            InvalidationReason::Replaced => "replaced-by-new-conversation",
            InvalidationReason::AccountSwitch => "account-switch",
            InvalidationReason::UpstreamRejected => "upstream-rejected",
            InvalidationReason::Explicit => "explicit",
        }
    }
}

/// 「这次请求该复用还是重建」的判定结果（[`SessionRegistry::resolve`] 的返回）。
#[derive(Clone, Debug)]
pub enum SessionResolution {
    /// 命中一条可用记录：复用它的 conversationId 与指纹链
    Reuse(SessionRecord),
    /// 没有可用记录（全新会话 / 记录已失效）：新建 conversationId 并**全量**提交。
    /// `reason` 仅供日志与排障（全新会话时为 None）。
    Rebuild {
        /// 导致重建的失效原因（全新会话 = None）
        reason: Option<InvalidationReason>,
    },
}

impl SessionResolution {
    /// 命中的记录（没有则为 None）
    ///
    /// ── 为什么保留一个「无人调用」的取值器（W5-T-d4 的口径）──────
    /// `decision.rs` 走的是 `match SessionResolution::Reuse(session)` 直接解构
    /// （它同时要拿 `Rebuild { reason }` 的那一支打日志），所以没走这两个取值器。
    /// 保留它们的理由与本文件其余查询接口一致：`SessionResolution` 是**给调用方
    /// 用的**公开返回类型，取值器是它天然的配套（没有它们，将来任何一个只想问
    /// 「命中了没有」的调用点都要自己写一遍 match）。摘除条件：明确决定只保留
    /// 解构写法时删掉本方法与 `is_rebuild`。
    #[allow(dead_code)]
    pub fn record(&self) -> Option<&SessionRecord> {
        match self {
            SessionResolution::Reuse(record) => Some(record),
            SessionResolution::Rebuild { .. } => None,
        }
    }

    /// 是否要求全量提交（同上：`decision.rs` 目前直接解构，保留这个语义化入口）
    #[allow(dead_code)]
    pub fn is_rebuild(&self) -> bool {
        matches!(self, SessionResolution::Rebuild { .. })
    }
}

/// 注册表快照（对应原实现 `stats()`；排障与日志用）
///
/// `#[allow(dead_code)]`：快照与下面的查询口是**排障设施**，与
/// `routing::describe_route_decision` 同一性质（那边的注释写着「生产路径用不到，
/// 但排障时能直接拿来比对」）。会话问题（增量提交错位、工具续接没命中）几乎
/// 都只能靠「捞一眼当前表里有几条、generation 变过没有」来定位，所以整组保留
/// —— 结构体与字段一起抑制，只写结构体上一条会把「字段没人读」拆成四条噪音。
/// 摘除条件：真接入一个排障端点（例如 /health 里带 CatPaw 会话计数）时删掉本属性。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct RegistryStats {
    /// 整表作废次数（每次 `clear_all` +1）。
    ///
    /// 用途：解析前后对比 generation 有没有变，可以发现「解析期间有人清了表」
    /// 这种竞态（原实现同样导出 generation）。
    pub generation: u64,
    /// 客户端会话数（按 x-session-id 索引）
    pub sessions: usize,
    /// 匿名工具会话数（按 conversationId 索引）
    pub anonymous_tool_sessions: usize,
    /// 在途占用的会话数
    pub inflight: usize,
}

/// 会话注册表（`Mutex<Inner>` 包一层，见模块头的锁纪律）。
pub struct SessionRegistry {
    inner: Mutex<Inner>,
}

/// 注册表的内部状态（所有字段只在本模块的临界区里动）
#[derive(Default)]
struct Inner {
    /// x-session-id → 记录
    sessions: HashMap<String, SessionRecord>,
    /// 匿名工具会话：conversationId → 记录（客户端没给 x-session-id 时）
    anonymous_tool_sessions: HashMap<String, SessionRecord>,
    /// tool_call_id → conversationId（工具续接的查找索引；规则 1 靠它命中）
    call_index: HashMap<String, String>,
    /// 在途占用的客户端会话 id（并发保护；**权威来源**，见 SessionRecord::inflight）
    inflight: HashSet<String>,
    /// conversationId → 这条会话**建立时**的账号身份（见 [`AccountIdentity`]）。
    ///
    /// 与 [`SessionRecord::account_id`] 的分工：那个是「哪个账号记录」，这个是
    /// 「哪个用户」（uid / loginName）。两个都参与复用判定 —— 只有 account_id
    /// 相同、uid 变了（桌面端在客户端侧换号）时，靠这个维度识别。
    ///
    /// 键用 conversationId 而不是 x-session-id：匿名工具会话没有 x-session-id
    /// （按 conversationId 索引），而 `register` 两条分支都要落定身份。
    record_identities: HashMap<String, AccountIdentity>,
    /// account_id → 该账号**本次请求实际使用**的身份
    /// （[`SessionRegistry::reconcile_identity`] 写）。
    ///
    /// 两个用途：① 判定「这个账号的身份变了没有」—— 没变时整条对账路径 O(1)，
    /// 不扫表（绝大多数请求走这一支）；② 无在途身份的登记（匿名工具会话）
    /// 按它落定身份。
    current_identities: HashMap<String, AccountIdentity>,
    /// x-session-id → 占用时捕获的身份（`mark_inflight` 写、`release_inflight` 清）。
    ///
    /// 为什么要**按占用捕获**而不是登记时现取：一条流式请求可能在「客户端换号」
    /// 之后才收尾登记，那时现取会拿到**新**身份，把一条用旧凭证建立的会话标成
    /// 新用户的 —— 那正是要防的串会话。捕获到的身份进
    /// [`Inner::record_identities`]，于是下一条请求的 `resolve` 会发现它属于
    /// 上一个用户并作废。
    inflight_identities: HashMap<String, AccountIdentity>,
    /// 淘汰用的单调序号：值越小越久没被写。
    ///
    /// 为什么不直接用 `last_active_at`：它是**毫秒**时间戳，同一毫秒内登记的
    /// 多条记录会打平，淘汰结果随 HashMap 迭代顺序变（原实现按 `updatedAt`
    /// 排序也有这个毛病）。单调序号让「淘汰哪一条」完全确定。
    sequence: u64,
    /// 每条记录最后一次登记的序号（键 = conversationId）
    last_sequence: HashMap<String, u64>,
    /// 整表作废次数（每次 `clear_all` +1；进 `RegistryStats` 供竞态排查）
    ///
    /// `#[allow(dead_code)]`：读它的唯一入口是 `stats()` / `generation()`，
    /// 那两个方法本身就是排障设施（见各自的说明）。写入侧（`clear_all`）是活的 ——
    /// 字段留着才能让将来接上观测点时立刻有数据，而不是先补三个写入点。
    #[allow(dead_code)]
    generation: u64,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    /// 建一个空注册表（原实现 `createSessionRegistry`）
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner::default()) }
    }

    /// 这次请求能不能占用该会话（原实现 `markInflight`）。
    ///
    /// 返回 `false` = **同一 x-session-id 已有一个流式请求在跑**，
    /// 调用方必须改走独立 conversation（不读也不写会话映射），
    /// 否则两个请求会互相覆盖 fingerprint 链、把上游的 conversation
    /// 搅成两条并行的历史（UPSTREAM_PROTOCOL §5「并发保护」）。
    ///
    /// 占用必须配对释放：调用方在流结束（含出错、客户端断开）时调
    /// [`SessionRegistry::release_inflight`]。T-d3 的转发入口把它包在
    /// 一个「RAII 式」的局部变量上，任何返回路径都会释放。
    ///
    /// `session_id` 为空串（客户端没给 x-session-id）时**不占用**、返回 true：
    /// 无状态请求本来就不该受并发保护约束（原实现同样只在 `clientSessionId`
    /// 非空时才 mark）。
    ///
    /// ── `identity` 为什么要在这里捕获 ────────────────────────────
    /// 占用与「本轮用哪个身份出网」是同一时刻的事，而登记（`register`）发生在
    /// 流结束之后 —— 中间用户可能在客户端换了号。按占用捕获，收尾时才知道
    /// 「这条会话是用**哪个身份**建的」，从而在换号后被 [`Self::resolve`] /
    /// [`Self::reconcile_identity`] 判为不可复用（见 [`AccountIdentity`]）。
    /// 已占用时**不覆盖**捕获值：先到的那个请求才是这条会话的建立者。
    pub fn mark_inflight(&self, session_id: &str, identity: &AccountIdentity) -> bool {
        if session_id.is_empty() {
            return true;
        }
        let mut inner = self.lock();
        if inner.inflight.contains(session_id) {
            return false;
        }
        inner.inflight.insert(session_id.to_string());
        inner.inflight_identities.insert(session_id.to_string(), identity.clone());
        // 记录里的 inflight 是镜像（可能还没有记录 —— 新会话在第一轮
        // 登记之前就已经占用了），所以这里只在记录存在时同步一下
        if let Some(record) = inner.sessions.get_mut(session_id) {
            record.inflight = true;
        }
        true
    }

    /// 释放占用（原实现 `releaseInflight`）。返回 false = 本来就没占用
    /// （重复释放；不报错，只是让调用方能记一笔日志）。
    pub fn release_inflight(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return false;
        }
        let mut inner = self.lock();
        let removed = inner.inflight.remove(session_id);
        // 占用捕获的身份随占用一起消失（登记时已经用掉；留着只会在
        // 同 session id 复用时把旧身份当成新请求的身份）
        inner.inflight_identities.remove(session_id);
        if let Some(record) = inner.sessions.get_mut(session_id) {
            record.inflight = false;
        }
        removed
    }

    /// 该客户端会话当前是否被占用。
    ///
    /// ── 为什么无人调用（W5-T-d4 的口径）────────────────────────
    /// 决策路径用的是 `mark_inflight`（占用并返回结果），因为「判断 + 占用」必须
    /// 在同一次加锁里完成，否则两个并发请求会同时看到「没被占用」。本方法是它
    /// 的**只读形态**，给排障与将来的观测点用（原实现也导出同名函数）。
    /// 摘除条件：真接入一个观测点（例如会话排障日志）时删掉本属性。
    #[allow(dead_code)]
    pub fn is_inflight(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return false;
        }
        self.lock().inflight.contains(session_id)
    }

    /// 解析「这次请求该复用还是重建」（原实现 `resolveSessionState` 的
    /// 长会话那一段；显式 conversationId / tool_call_id 两条分支由 T-d3 的
    /// 状态机按顺序调本模块的 `lookup_by_call_id` 等入口自己组合）。
    ///
    /// 顺序与失效规则：
    ///   1. **先清理过期**（整表扫一遍，原实现 `cleanupExpiredSessions`）；
    ///   2. 查 x-session-id（空 id 直接判重建）；
    ///   3. 模型不一致 → 作废（`model-mismatch`）后重建；
    ///   4. 账号不一致 → 作废（`account-mismatch`）后重建；
    ///   5. 身份不一致（account_id 相同但 uid 变了）→ 作废（`identity-changed`）后重建；
    ///   6. 命中 → 刷新 `last_active_at` 并返回记录副本。
    ///
    /// 第 4 条是相对原实现**多加的保险**（原实现没有账号字段，靠主动清表；
    /// 见 [`SessionRecord::account_id`] 的说明）。传入空 `account_id` 表示
    /// 「无账号身份」，它**只匹配空**（见 [`SessionRecord::belongs_to_account`]）——
    /// 于是真实账号建的记录不会在账号消失后（请求回落到默认登录态）被复用。
    ///
    /// 第 5 条覆盖「account_id 没变、用户变了」：拿**本次请求的身份**（由
    /// [`Self::reconcile_identity`] 在选路时写入 `current_identities[account_id]`，
    /// 值来自本次实际使用的凭证）与记录建立时的身份做相等判定。这条是「桌面端
    /// 在客户端侧换号」的最后一道网：`reconcile_identity` 已经在选路时把该账号
    /// 名下的陈旧记录清掉了，这里再按记录身份核一次，防止「对账之后、本请求
    /// 判定之前」的窗口里被写进一条旧身份的记录。
    ///
    /// 返回的是记录**副本**：调用方拿到后可以随便用，不会持锁
    /// （后续 `register` 会覆盖写回）。多线程下两次 resolve 可能都命中同一条，
    /// 由 `mark_inflight` 的占用标记保证只有一个真的续接（另一个走独立会话）。
    pub fn resolve(
        &self,
        session_id: &str,
        model_type: i64,
        account_id: &str,
    ) -> SessionResolution {
        let now = logging::now_ms();
        let mut inner = self.lock();
        inner.cleanup_expired(now);
        if session_id.is_empty() {
            return SessionResolution::Rebuild { reason: None };
        }
        let Some(record) = inner.sessions.get(session_id).cloned() else {
            return SessionResolution::Rebuild { reason: None };
        };
        if record.model_type != model_type {
            inner.invalidate(session_id, InvalidationReason::ModelMismatch);
            return SessionResolution::Rebuild {
                reason: Some(InvalidationReason::ModelMismatch),
            };
        }
        if !record.belongs_to_account(account_id) {
            inner.invalidate(session_id, InvalidationReason::AccountMismatch);
            return SessionResolution::Rebuild {
                reason: Some(InvalidationReason::AccountMismatch),
            };
        }
        // 身份维度：账号 id 对上了，但这条会话是用**另一个用户**的凭证建的
        // （桌面端在客户端侧换号，account_id 不变）。判据见
        // [`Inner::identity_consistent`]（两侧任一缺失即不判定）。
        if !inner.identity_consistent(&record) {
            inner.invalidate(session_id, InvalidationReason::IdentityChanged);
            return SessionResolution::Rebuild {
                reason: Some(InvalidationReason::IdentityChanged),
            };
        }
        let mut record = record;
        record.last_active_at = now;
        record.inflight = inner.inflight.contains(session_id);
        inner.sessions.insert(session_id.to_string(), record.clone());
        inner.touch(record.conversation_id.as_str());
        SessionResolution::Reuse(record)
    }

    /// 登记（覆盖写）一条会话记录（原实现 `registerClientSession` +
    /// `registerClientToolSession` 的合并形态）。
    ///
    /// 三件事：
    ///   1. **顶替**：同 x-session-id 的旧记录若 conversationId 不同，
    ///      先按 `replaced-by-new-conversation` 作废（清掉它的 call 索引）；
    ///   2. **淘汰**：表满（`>= MAX_CLIENT_SESSIONS`）且这条是新的时，
    ///      淘汰最久未活动的一条（LRU，`evicted-capacity`）；
    ///   3. **写入**：设置 `last_active_at`（TTL 从这一刻起算）、写表、
    ///      按 `pending_call_ids` 重建 call 索引。
    ///
    /// `record.session_id` 为空 → 进匿名表（按 conversationId 索引）。
    /// 调用方**不需要**自己先 set `last_active_at`（这里会覆盖成当前时间；
    /// `created_at` 由调用方给，跨轮次保持不变）。
    ///
    /// ── 身份落定规则（见 [`AccountIdentity`]）────────────────────
    /// 这里**不**接受身份参数（登记是 `turn_executor` 的收尾动作，调用方手上
    /// 没有凭证身份），身份从表里取，两条来源按优先级：
    ///   1. `inflight_identities`（`mark_inflight` 在占用时捕获的）—— 有会话 id
    ///      的记录走这条。登记发生在流结束之后，中间客户端可能已经换了号，
    ///      现取会把「用旧凭证建立的会话」标成新用户的（下一条请求就不会作废它
    ///      —— 正是要防的串会话）；
    ///   2. `current_identities[record.account_id]`（`reconcile_identity` 在选路时
    ///      写下的本次身份）—— 匿名工具会话（无 x-session-id，不占用）走这条。
    /// 两条都没有 → **不记身份**（比记错好：缺失时下游一律不作身份判定）。
    pub fn register(&self, mut record: SessionRecord) {
        let now = logging::now_ms();
        let mut inner = self.lock();
        record.last_active_at = now;
        let identity = inner
            .inflight_identities
            .get(&record.session_id)
            .cloned()
            .or_else(|| inner.current_identities.get(&record.account_id).cloned());
        if let Some(identity) = identity {
            inner
                .record_identities
                .insert(record.conversation_id.clone(), identity);
        }
        if record.session_id.is_empty() {
            inner.register_anonymous(record);
            return;
        }
        let session_id = record.session_id.clone();
        let conversation_id = record.conversation_id.clone();
        let previous = inner.sessions.get(&session_id).cloned();
        if let Some(previous) = previous {
            if previous.conversation_id != conversation_id {
                inner.invalidate(&session_id, InvalidationReason::Replaced);
            } else {
                // 同一条会话的续写：先撤离旧索引（call 索引可能变小），
                // 避免旧 tool_call_id 还指向它
                inner.clear_call_index(&previous);
            }
        }
        if inner.sessions.len() >= MAX_CLIENT_SESSIONS && !inner.sessions.contains_key(&session_id)
        {
            inner.evict_lru();
        }
        record.inflight = inner.inflight.contains(&session_id);
        inner.touch(&conversation_id);
        inner.index_calls(&record);
        inner.sessions.insert(session_id, record);
    }

    /// 按 tool_call_id 找待响应的会话（原实现 `clientToolSessionsByCallId`）。
    ///
    /// 用途：UPSTREAM_PROTOCOL §9.2 规则 1（历史末尾是
    /// `assistant(tool_calls) + tool 结果` 且 tool_call_id 命中注册表
    /// → 工具续接）。
    ///
    /// 注意**不做过期清理**（原实现这条路径上也没有）：过期判定交给调用方
    /// （`SessionRecord::is_expired`）—— 工具会话过期后应该走「全新会话」，
    /// 由 T-d3 的规则 3 兜住，与这里返回 None 等价。
    ///
    /// 身份不符的记录**不返回**（返回 None = 这条路径没命中，调用方按规则 3
    /// 走全新会话全量 round）：工具续接同样是把消息提交到一条 conversation 上，
    /// 那条 conversation 属于哪个用户的要求与长会话一致（见模块头的身份归属）。
    /// 命中一条已换号的记录会让客户端把 tool 结果提交到上一个用户的会话里 ——
    /// 比让它重来一轮危险得多。
    pub fn lookup_by_call_id(&self, call_id: &str) -> Option<SessionRecord> {
        if call_id.is_empty() {
            return None;
        }
        let inner = self.lock();
        let conversation_id = inner.call_index.get(call_id)?;
        let record = inner
            .sessions
            .values()
            .find(|record| record.conversation_id == *conversation_id)
            .or_else(|| inner.anonymous_tool_sessions.get(conversation_id))?;
        if !inner.identity_consistent(record) {
            return None;
        }
        Some(record.clone())
    }

    /// 按 conversationId 找记录（原实现 `findClientSessionByConversation`）。
    ///
    /// 用途：显式 `conversationId` 的续接入口（原实现 `resolveSessionState`
    /// 的第一条分支：优先查 pendingClientToolSessions，再查 clientSessions）。
    ///
    /// ── 为什么本网关的决策路径上没人调（W4b-T-d3 的裁剪，W5-T-d4 复核）──
    /// 那条「显式 conversationId」分支来自 CLI 的 Host 模式：调用方自己带
    /// conversationId 进来。OpenAI 兼容路径的客户端**没有这个字段**（它们只发
    /// `x-session-id`），因此本网关的规则 2/3 都用不上它 —— 保留方法是给
    /// 排障（按 id 捞一条记录看指纹链长度）与将来可能的多协议入口用。
    /// 摘除条件：真接入观测点，或明确决定不再支持显式 conversationId 续接。
    #[allow(dead_code)]
    pub fn lookup_by_conversation(&self, conversation_id: &str) -> Option<SessionRecord> {
        if conversation_id.is_empty() {
            return None;
        }
        let inner = self.lock();
        inner
            .sessions
            .values()
            .find(|record| record.conversation_id == conversation_id)
            .or_else(|| inner.anonymous_tool_sessions.get(conversation_id))
            .cloned()
    }

    /// 快照（原实现 `stats`）—— 排障设施，见 `RegistryStats` 的说明
    #[allow(dead_code)]
    pub fn stats(&self) -> RegistryStats {
        let inner = self.lock();
        RegistryStats {
            generation: inner.generation,
            sessions: inner.sessions.len(),
            anonymous_tool_sessions: inner.anonymous_tool_sessions.len(),
            inflight: inner.inflight.len(),
        }
    }

    /// 当前 generation（作废次数）：在「解析 → 提交」之间对比它，可以发现
    /// 「整表在中间被清了」，从而避免用一条已被作废的 conversationId 续接。
    ///
    /// `#[allow(dead_code)]`：本网关的会话状态机是**单线程顺序执行**的
    /// （`conversation::execute` 从 `resolve` 到 `register` 之间没有 await 之外的
    /// 并发写点，且期间不释放注册表锁），因此目前不需要这层竞态防护 ——
    /// 保留它与 `stats()` 一起作为排障设施（见 `RegistryStats` 的说明）。
    #[allow(dead_code)]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// 取锁；中毒时取回内部数据（见模块头）
    ///
    /// 可见性不必调整：`identity.rs` / `cleanup.rs` 是本模块的**子模块**，父模块里
    /// 私有的项对子模块可见，所以两个子模块照样能调它（加宽成 `pub(super)` 反而会
    /// 因为返回类型 `Inner` 比它更私有而触发 `private_interfaces` 警告）。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// 取前 8 个字符（不足则原样）；空串给 `-`
fn short_id(value: &str) -> &str {
    if value.is_empty() {
        return "-";
    }
    match value.char_indices().nth(8) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}
