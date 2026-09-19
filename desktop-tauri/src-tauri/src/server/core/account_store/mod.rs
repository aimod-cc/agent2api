//! 账号存储（对照 Node 版 src/workbuddy-account-store.mjs 全量移植）。
//!
//! 持久化文件 `{config_dir}/accounts.json`：
//!   `{ accounts: [{ id, provider, name, uid, nickname, type, enterpriseId,
//!     enterpriseName, accessToken, refreshToken, expiresAt, refreshExpiresAt,
//!     domain, tokenTail, prefixPath, endpoint, platform, edition, addedAt,
//!     updatedAt, priority, enabled, proxy, rateLimits: { [modelId]: {...} } }] }`
//!
//! ── 三条不变量（改代码前务必读）──────────────────────────────
//!   1. **未知字段全量保留**：账号记录用强类型字段 + `extra` 兜底，写回时合并。
//!      用户升级时绝不能丢账号里的任何字段（旧版遗留的 currentAccountId、
//!      手工加的备注、未来版本新增的字段都在这条兜底里）。
//!      新增的 `provider` 是**已知字段**，同样不许在重建记录时丢掉。
//!   2. **优先级全局唯一**（所有提供商共用一条队列）：数值小的先用（主备式）。
//!      写入侧冲突一律拒绝（409），启动时对既有数据做一次性迁移
//!      （`migrate_startup`：旧版按家分队的号码按旧顺序合并成全局队列 + 去重）。
//!      「当前账号」不是独立存储的手动选择，而是**由优先级派生** ——
//!      全局队列里第一个「已启用且有可用凭证」的账号（`pick_current`），所以不存在
//!      「手动选了 A、实际用 B」这种分叉。想换当前账号就把目标置顶。
//!      桌面端实时账号同样算「有凭证」。逐家的队首仍在快照的 `currentAccountIds`
//!      里给出（只有参考意义）。
//!   3. **provider 字段的兜底口径**：加载时缺失/为空一律补 `"workbuddy"`
//!      （历史数据都来自单上游时代），由 `migrate_startup` 与优先级去重
//!      同机一次落盘完成（架构文档 §3.2）。未知 provider 字符串**原样保留** ——
//!      可能是新版本写入的数据被旧版本读到，强行归一会让账号跑进错误的组。
//!
//! ── provider 白名单：没有第二份清单（W4a 明确）─────────────────
//! 本模块**不维护** `["workbuddy","raccoon"]` 这样的 id 清单，将来也不要加：
//! 「这个 provider id 认不认识」一律走
//! `providers::is_known_provider_id`（= `kind_from_id(id).is_some()`，
//! 事实来源是 `providers::PROVIDERS` 注册表）。于是「加一家 provider」只改
//! 注册表一处，账号层（添加/分支/校验/摘要）零改动 —— W4a 加 CatPaw / AutoClaw
//! 时正是这么做的。各家**专属**的行为（小浣熊的公开字段集、桌面端实时凭证、
//! 添加与导入路径）仍按 id 判定，那属于「这一家的特殊实现」而非白名单。
//!
//! ── 并发模型 ──────────────────────────────────────────────
//! Node 版是单线程事件循环；这里整个 store 用一把 `std::sync::Mutex` 包住
//! 全部状态与文件读写，锁粒度不精细但语义等价。硬约束：**持锁期间绝不做
//! 网络请求**（会阻塞所有管理 API）—— 需要出网的调用（token 刷新、积分查询）
//! 一律先在锁内取出快照，释放锁后再发请求，回头再单独写回。
//!
//! 子模块分工（对照 Node 版的单文件拆分）：
//!   priority.rs  优先级号段规则（归一/排序/找号/整队，按 provider 分组使用）
//!   state.rs     记录结构（JSON 原样持有 + 容错访问器 + 磁盘形态 + provider 分组工具）
//!   store_util.rs JS 语义工具（`x || y` / `Number(x)` / `JSON.stringify` 比较…）
//!   store.rs     AccountStore 句柄：打开/锁/读写、当前账号派生、公开形态、provider 摘要
//!   store_crud.rs 增删改查（add/remove/promote/update/move 与 apply_patch 内核）
//!   store_batch.rs 批量操作（batch_update / batch_remove）
//!   store_admin.rs token 回写、限额标记、启动迁移（provider 补齐 + 优先级去重）
//!   raccoon_accounts.rs 小浣熊账号（W3-T4）：手动添加、桌面端实时账号、
//!                刷新回写、小浣熊公开形态
//!   raccoon_import.rs   小浣熊旧数据一次性导入（旧网关 accounts.json +
//!                桌面端登录态；架构文档 §3.3）
//!   catpaw_accounts.rs  CatPaw 账号（W5-T-d4）：手动添加（扁平/auth.json/
//!                原项目账号记录三种形态）、桌面端实时账号、CatPaw 公开形态
//!   catpaw_import.rs    CatPaw 旧数据一次性导入（原项目 catpaw-proxy-accounts.json
//!                + ~/.meituan-catpaw/auth.json；§9）
//!   autoclaw_accounts.rs AutoClaw 账号（W4b-T-c2）：手动添加（扁平/auth.json 内容
//!                两种形态，enc: 密文自动解密）、桌面端实时账号、刷新回写、
//!                AutoClaw 公开形态（userId / deviceId / tokenTail）
//!   autoclaw_import.rs   AutoClaw 旧数据一次性导入（~/.autoclaw-proxy/accounts.json
//!                + 桌面端登录态；§10.2）

pub mod autoclaw_accounts;
pub mod autoclaw_import;
pub mod catpaw_accounts;
pub mod catpaw_import;
pub mod priority;
pub mod qoder_accounts;
pub mod raccoon_accounts;
pub mod raccoon_import;
pub mod state;
pub mod store;
pub mod store_admin;
pub mod store_batch;
pub mod store_crud;
pub mod store_util;

pub use store::{AccountStore, AccountStoreError};

/// 凭证回写的结果（**比较-再写**的返回值，刷新链路专用）。
///
/// 刷新是秒级的网络动作，期间凭证可能被换掉（用户重导入、换号、桌面端重新
/// 登录、另一轮刷新先落地）。回写方法因此不接受「无条件覆盖」：调用方给出
/// **刷新前那份凭证**，store 在**同一把账号锁内**比较后再写。判定不一致时返回
/// [`CredentialWrite::Stale`] —— 调用方必须改用最新快照，绝不能把旧结果当成功
/// 用于后续请求。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CredentialWrite {
    /// 已写入（记录里仍是刷新前那份凭证）
    Written,
    /// 未写入：记录里的凭证已被更换（或账号已被删除 / 同 id 重建），刷新结果作废
    Stale,
}

/// 账号数上限（对照 Node 版 MAX_ACCOUNTS）
pub const MAX_ACCOUNTS: usize = 20;
/// token 长度上限（对照 Node 版 MAX_TOKEN_LENGTH）
pub const MAX_TOKEN_LENGTH: usize = 8192;

/// CatPaw provider id（账号存储内部多处要用；**从注册表推导**，同
/// [`RACCOON_PROVIDER_ID`] 的口径 —— 注册表改了 id，这个常量跟着变）。
pub(crate) const CATPAW_PROVIDER_ID: &str = crate::server::core::providers::kind_id(
    crate::server::core::providers::ProviderKind::CatPaw,
);

/// AutoClaw provider id（账号存储内部多处要用；**从注册表推导**，同
/// [`RACCOON_PROVIDER_ID`] 的口径）。
pub(crate) const AUTOCLAW_PROVIDER_ID: &str = crate::server::core::providers::kind_id(
    crate::server::core::providers::ProviderKind::AutoClaw,
);

/// Qoder provider id（账号存储内部多处要用；**从注册表推导**，同
/// [`RACCOON_PROVIDER_ID`] 的口径）。Qoder 的账号形态与推理转发见
/// `qoder_accounts.rs` 与 `providers::qoder` 的模块头。
pub(crate) const QODER_PROVIDER_ID: &str = crate::server::core::providers::kind_id(
    crate::server::core::providers::ProviderKind::Qoder,
);

/// 小浣熊 provider id（账号存储内部多处要用）。
///
/// **从注册表推导**（W4a）：`providers::kind_id` 是 `const fn`，所以这里可以
/// `const` 声明而不再写第二份字面量 —— 注册表改了 id，这个常量跟着变，
/// 不会出现「账号存储按 `"raccoon"` 找、注册表里叫别的」这种静默失配。
///
/// ── 账号层的 provider 白名单在哪（W4a 的口径）─────────────────
/// 本模块**没有**一份 `["workbuddy", "raccoon"]` 式的清单，也刻意不再加：
/// 「这个 provider id 认不认识」一律走
/// `providers::is_known_provider_id`（= `kind_from_id(id).is_some()`，事实来源是
/// `providers::PROVIDERS` 注册表），需要**枚举**全部 id 时直接遍历那张表。
/// 于是以后加 provider（CatPaw / AutoClaw 已加）只改注册表，账号层零改动。
///
/// 各家**专属**的分支（小浣熊的公开字段集、桌面端实时凭证、添加/导入路径）
/// 仍然按 id 判定 —— 那是「这一家的特殊行为」，不是白名单，改动量随新家到来
/// 天然增加（每一家都有自己的一套字段与凭证链路）。
pub(crate) const RACCOON_PROVIDER_ID: &str =
    crate::server::core::providers::kind_id(
        crate::server::core::providers::ProviderKind::Raccoon,
    );
