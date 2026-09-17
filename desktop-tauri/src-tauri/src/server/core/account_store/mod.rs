//! WorkBuddy 账号存储（对照 Node 版 src/workbuddy-account-store.mjs 全量移植）。
//!
//! 持久化文件 `{config_dir}/accounts.json`：
//!   `{ accounts: [{ id, name, uid, nickname, type, enterpriseId, enterpriseName,
//!     accessToken, refreshToken, expiresAt, refreshExpiresAt, domain, tokenTail,
//!     prefixPath, endpoint, platform, edition, addedAt, updatedAt,
//!     priority, enabled, proxy, rateLimits: { [modelId]: {...} } }] }`
//!
//! ── 两条不变量（改代码前务必读）──────────────────────────────
//!   1. **未知字段全量保留**：账号记录用强类型字段 + `extra` 兜底，写回时合并。
//!      用户升级时绝不能丢账号里的任何字段（旧版遗留的 currentAccountId、
//!      手工加的备注、未来版本新增的字段都在这条兜底里）。
//!   2. **优先级全局唯一**：数值小的先用（主备式）。写入侧冲突一律拒绝（409），
//!      启动时对既有数据做一次性去重迁移（`migrate_priorities`）。
//!      「当前账号」不是独立存储的手动选择，而是**由优先级派生** ——
//!      转发顺序里第一个「已启用且有凭证」的账号，所以不存在
//!      「手动选了 A、实际用 B」这种分叉。想换当前账号就把目标置顶。
//!
//! ── 并发模型 ──────────────────────────────────────────────
//! Node 版是单线程事件循环；这里整个 store 用一把 `std::sync::Mutex` 包住
//! 全部状态与文件读写，锁粒度不精细但语义等价。硬约束：**持锁期间绝不做
//! 网络请求**（会阻塞所有管理 API）—— 需要出网的调用（token 刷新、积分查询）
//! 一律先在锁内取出快照，释放锁后再发请求，回头再单独写回。
//!
//! 子模块分工（对照 Node 版的单文件拆分）：
//!   priority.rs  优先级号段规则（归一/排序/找号/整队）
//!   state.rs     记录结构（JSON 原样持有 + 容错访问器 + 磁盘形态）
//!   store_util.rs JS 语义工具（`x || y` / `Number(x)` / `JSON.stringify` 比较…）
//!   store.rs     AccountStore 句柄：打开/锁/读写、当前账号派生、公开形态
//!   store_crud.rs 增删改查（add/remove/promote/update/move/batch）
//!   store_admin.rs token 回写、限额标记、启动迁移

pub mod priority;
pub mod state;
pub mod store;
pub mod store_admin;
pub mod store_crud;
pub mod store_util;

pub use store::{AccountStore, AccountStoreError};

/// 账号数上限（对照 Node 版 MAX_ACCOUNTS）
pub const MAX_ACCOUNTS: usize = 20;
/// token 长度上限（对照 Node 版 MAX_TOKEN_LENGTH）
pub const MAX_TOKEN_LENGTH: usize = 8192;
