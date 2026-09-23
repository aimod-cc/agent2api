//! 账号级活跃连接计数（账号页「连接数」列的数据源）。
//!
//! ── 计数口径：一次下游请求 = 一条连接 ──────────────────────────
//! 数的是「此刻正在使用这个账号的请求数」，不是 TCP 连接数、也不是尝试次数：
//!   · 同一请求在账号间轮换（429 / 401 后顺延）时计数**从旧账号移到新账号**
//!     （[`ConnectionGuard::rebind`]）—— 任意时刻一个请求只占一个账号；
//!   · 一次请求内对上游的重试（11128 退避、401 刷新后重发）不额外计数，
//!     它还是同一个请求；
//!   · 流式请求计到**字节下发完**为止：凭证由 `ForwardStream` 持有，生命周期
//!     与去重槽位（`InFlightGuard`）完全一致。客户端断开、上游断开都靠 drop
//!     传播收尾，所以这里没有、也不需要显式的 release 调用点。
//!
//! ── 为什么单独成文件 ────────────────────────────────────────
//! 与 `rotate.rs` / `usage.rs` 同样的理由：转发编排（`provider_loop`）里只该出现
//! 「选谁去发」，而「谁正在发」是一份自洽的进程内状态 —— 一张表 + 两个原子动作
//! + 一个 Drop 凭证。摊在编排里会让这两件事互相淹没。
//!
//! ── 与 OmniProxy 上游管理页「连接」列的对应关系 ────────────────
//! 口径与视觉（有连接时显示数字、为 0 时留空）对齐那一列，仅把计数单位从
//! provider 换成 account —— 本项目的转发链路上「谁去发」的答案是账号
//! （provider 只是账号的属性，见 `provider_loop` 的模块头）。
//!
//! ── 并发模型 ──────────────────────────────────────────────
//! 一把 `Mutex` 包住一张 `HashMap`：增减都只是内存操作（不读盘、不出网），
//! 持锁时间在纳秒级，不会阻塞任何管理 API。锁中毒不致命，与 `mod.rs` 的
//! `lock_table` 同一策略（取回内部值继续用）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 账号 id → 当前正在使用该账号的下游请求数（0 的账号不留在表里）。
///
/// `Clone` 是浅拷贝（内部 `Arc`）：`ServerState` 的 handler 克隆状态后，
/// 拿到的仍是同一份计数。
#[derive(Clone)]
pub struct Connections {
    inner: Arc<Mutex<HashMap<String, usize>>>,
}

impl Connections {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// 全部**非零**计数（account_id → count），按 id 排序保证输出稳定。
    ///
    /// 计数为 0 的账号不出现在结果里：调用方（界面）按「缺失即 0」处理，
    /// 于是接口不必为几十个空闲账号各送一个 0。
    pub fn snapshot(&self) -> Vec<(String, usize)> {
        let guard = lock_counts(&self.inner);
        let mut entries: Vec<(String, usize)> = guard
            .iter()
            .map(|(id, count)| (id.clone(), *count))
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    /// 计数 +1（空 id 是「没有账号」的转发，不计数）
    fn add(&self, account_id: &str) {
        if account_id.is_empty() {
            return;
        }
        let mut guard = lock_counts(&self.inner);
        *guard.entry(account_id.to_string()).or_insert(0) += 1;
    }

    /// 计数 -1（减到 0 就删项；无记录时是空操作 —— 幂等，重复释放不会变负数）
    fn sub(&self, account_id: &str) {
        if account_id.is_empty() {
            return;
        }
        let mut guard = lock_counts(&self.inner);
        match guard.get_mut(account_id) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                guard.remove(account_id);
            }
            None => {}
        }
    }
}

impl Default for Connections {
    fn default() -> Self {
        Self::new()
    }
}

/// 取计数表锁；锁中毒不致命（与 `mod.rs` 的 `lock_table` 同一策略）
fn lock_counts<'a>(
    inner: &'a Mutex<HashMap<String, usize>>,
) -> std::sync::MutexGuard<'a, HashMap<String, usize>> {
    match inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 一条在途请求的计数凭证：**drop 即释放**，并可在轮换时**改绑**到另一个账号。
///
/// 由 `attempt_queue` 在循环外创建、循环内按选中的账号 rebind，成功转流式时
/// 被 move 进响应流 —— 于是「请求还在跑」这件事在两种形态（流式 / 非流式）
/// 上有同一个释放点：凭证析构。错误路径上的 `?` 提前返回也由 Drop 兜底，
/// 不需要每个分支各写一次释放。
///
/// 可见性是 `pub`（而非 `pub(super)`）：`ForwardStream::new` 的入参里有它，
/// 而那个构造函数在 `core` 内是公开的 —— 类型比调用面窄会触发
/// `private_interfaces` 告警。
pub struct ConnectionGuard {
    connections: Connections,
    /// 当前占用的账号（None = 这一次尝试没有指定账号，如环境变量旁路）
    account_id: Option<String>,
}

impl ConnectionGuard {
    pub(super) fn new(connections: Connections) -> Self {
        Self { connections, account_id: None }
    }

    /// 改绑到另一个账号：先放掉旧的，再给新的 +1（同一个账号则原样不动）。
    ///
    /// 空字符串与 None 等价（都表示「没有账号」），这样调用方可以直接把
    /// `RouteTarget::account_id` 传进来，不必先做一次归一。
    pub(super) fn rebind(&mut self, account_id: Option<String>) {
        let next = account_id.filter(|id| !id.is_empty());
        if self.account_id == next {
            return;
        }
        if let Some(previous) = self.account_id.as_deref() {
            self.connections.sub(previous);
        }
        if let Some(current) = next.as_deref() {
            self.connections.add(current);
        }
        self.account_id = next;
    }

    /// 把计数移交给响应流：返回一个持有当前账号的新凭证，自身转为已释放。
    ///
    /// 流式请求的成功路径要把计数活到「字节下发完」，而那时 `attempt_queue`
    /// 的栈帧早就退出了 —— 凭证必须被 move 进流对象。**不是** clone：同一个账号
    /// 被两份凭证各 +1 会让计数翻倍，所以这里用 `take()` 转移所有权，
    /// 本对象的 `Drop` 随即变成空操作。
    ///
    /// 可见性是 `pub(in crate::server::core)`（而非 `pub(super)`）：自定义
    /// 提供商的转发入口（`providers::custom::forward`）在成功转流式时同样要
    /// 移交计数 —— 它在 `core` 的后代模块里，`pub(super)`（只到 `upstream`
    /// 子树）够不到。调用面与 `attempt_queue` 的无状态路径逐字同构。
    pub(in crate::server::core) fn handoff(&mut self) -> Self {
        Self {
            connections: self.connections.clone(),
            account_id: self.account_id.take(),
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(account_id) = self.account_id.as_deref() {
            self.connections.sub(account_id);
        }
    }
}
