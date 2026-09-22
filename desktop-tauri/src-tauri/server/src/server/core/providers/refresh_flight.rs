//! 凭证刷新的**单飞原语**（Raccoon 与 AutoClaw 共用；刷新生命周期修复）。
//!
//! ── 它解决什么 ──────────────────────────────────────────────
//! 同一凭证的并发刷新只该发一次真实请求：后到者复用那一次的结果，而不是各自去
//! 打上游。但「单飞」在 Rust 里有两个必须一起关掉的坑，本模块把两者都关掉：
//!
//!   1. **旧结果不能留在表里当缓存**。旧实现把「刚落地的结果」写回进程级表并
//!      长期保留，于是 `force = true`（401 之后的强制刷新）会命中那条旧记录，
//!      拿回**同一个被拒的坏 token** —— 「刷新后重试一次」退化成「用同一个坏
//!      token 再打一次」。本模块**没有 Done 槽**：结果只经由 `Arc<Flight>`
//!      交给**已经加入这次 flight 的等待者**（他们手里有 `Arc`），表项在本轮
//!      结束（成功/失败/取消）时被移除。因此 `force = true` 在上一轮结束后一定
//!      发起新请求，失败也不会被无限缓存。
//!   2. **取消（future 被 drop）必须释放占位并唤醒等待者**。转发链路的 future
//!      随时可能被 drop（客户端断开、超时取消）。旧实现里 leader 的 future 一被
//!      drop，占位就永久留在表里 —— 后续请求全部走进等待循环，直到 30 秒超时
//!      才拿到 409，而且**再也刷不了**。本模块的 leader 持有一个 RAII 守卫
//!      （[`LeaderGuard`]）：无论正常结束、`?` 提前返回还是 future 被 drop，
//!      `Drop` 都会把这一格从表里移除并唤醒等待者（等待者拿到「已取消」，
//!      于是可以重试）。
//!
//! ── 锁与唤醒的纪律（踩过的坑）───────────────────────────────
//!   - **不在 await 点持有 `std::sync::MutexGuard`**：所有表操作都在同步函数里
//!     完成，锁在函数返回前释放 —— 否则 future 不是 `Send`（转发跑在多线程
//!     运行时上），而且会真的死锁。
//!   - **没有丢唤醒窗口**：等待者拿的是 join 时 `watch::Sender::subscribe()`
//!     得到的 `Receiver`，**订阅先于返回**；leader 结束时的信号一定发生在
//!     「表项移除」之后，因此「join 时表项还在」蕴含「订阅早于这次信号」，
//!     `changed()` 不会错过它。醒来后先查结果槽，再决定是否继续等。
//!   - **先移除表项、再写结果/发信号**：这样「结果可读」蕴含「表项已释放」，
//!     `force = true` 的下一轮不会撞上刚完成的那一格去误复用旧结果。
//!   - **表 key 绝不打印**：key 里含凭证指纹，只作为 `HashMap` 的键存在；
//!     日志只打账号 id 这种非敏感值。
//!
//! ── 为什么 key 由调用方构造 ─────────────────────────────────
//! 两家的凭证形态不同（Raccoon 的 `id` 是账号 id，AutoClaw 桌面来源的 `id` 是
//! 固定的 `desktop-auth`），key 必须带上「provider 内账号 / 凭证来源 + 该来源
//! 自己的版本语境」才不会被别的账号、或换号后的新凭证误复用。构造规则留在各自
//! 的 `credentials.rs`（那里才知道哪些字段参与身份判定，以及哪些字段只能以
//! 指纹形式进 key），本模块只保证「同 key 单飞、异 key 并行」。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use crate::server::errors::GatewayError;

/// 等一次并发刷新的上限（150 × 200ms = 30 秒）。
///
/// 刷新请求自身的超时是 30 秒（两家的 `AUTH_REQUEST_TIMEOUT_MS` /
/// `REQUEST_TIMEOUT_MS`），leader 通常先于本上限结束；这里是「leader 卡住」时
/// 的兜底，超时后等待者拿到 409，由上层决定是否重试。
const WAIT_STEPS: usize = 150;

/// 每次等待的间隔
const WAIT_STEP_MS: u64 = 200;

/// leader 被取消（future drop）时等待者看到的错误文案
pub const CANCELLED_MESSAGE: &str = "刷新已被取消，请稍后重试";

/// 等待超时的错误文案
pub const WAIT_TIMEOUT_MESSAGE: &str = "token 正在刷新中，请稍后重试";

/// 凭证指纹：截断的 SHA-256 十六进制（32 字符）。
///
/// 只用于单飞表的 key —— 既让「同一凭证」稳定命中同一格，又不在 key 里长期
/// 持有完整 token。它与 token 同等敏感：**绝不写进日志或错误文案**。
pub fn fingerprint(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        // 写进 String 不会失败（fmt::Write for String 恒为 Ok）
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 一次进行中的刷新。
///
/// 只被 leader 与**已加入的等待者**持有（`Arc`），不进任何长期缓存：
/// 表项一移除，最后一个 `Arc` 的持有者释放它，结果随之消失。
struct Flight<T> {
    /// 结果槽：`None` = 仍在刷新；`Some` = 已落地（成功或失败）
    done: Mutex<Option<Result<T, GatewayError>>>,
    /// 完成信号的版本号（自增；只用于唤醒 `watch` 订阅者）
    version: AtomicU64,
    /// 完成信号：`send` 唤醒所有订阅者
    signal: watch::Sender<u64>,
}

impl<T> Flight<T> {
    fn new() -> Self {
        let (signal, _receiver) = watch::channel(0u64);
        Self {
            done: Mutex::new(None),
            version: AtomicU64::new(0),
            signal,
        }
    }

    /// 取结果槽（锁在返回前释放）
    fn outcome(&self) -> Option<Result<T, GatewayError>>
    where
        T: Clone,
    {
        let guard = match self.done.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    }

    /// 写结果（先到者生效；一个 flight 只会有一次写入）
    fn set_outcome(&self, result: Result<T, GatewayError>) {
        let mut guard = match self.done.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            *guard = Some(result);
        }
    }

    /// 发完成信号（结果已经写好之后调用）
    fn signal(&self) {
        let next = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        // 没有订阅者时 `send` 返回 Err —— 不是错误（leader 自己不需要通知）
        let _ = self.signal.send(next);
    }
}

/// 单飞表：key → 正在进行的刷新。
///
/// 表里**只有进行中的 flight**：结束（成功/失败/取消）即移除，因此表的大小受
/// 「同时在飞的刷新数」限制，不会随请求量增长，也不会跨轮次留下旧结果。
pub struct Table<T> {
    entries: Mutex<HashMap<String, Arc<Flight<T>>>>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> Table<T> {
    /// 建一张空表（调用方通常放进 `OnceLock` 做进程级单例）
    pub fn new() -> Self {
        Self::default()
    }

    /// 取表锁（锁中毒不致命：与账号存储同一策略）
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Flight<T>>>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 移除表项（只移除仍是自己的那一格，绝不误删后来者的 flight）
    fn detach(&self, key: &str, flight: &Arc<Flight<T>>) {
        let mut entries = self.lock();
        if let Some(current) = entries.get(key) {
            if Arc::ptr_eq(current, flight) {
                entries.remove(key);
            }
        }
    }
}

impl<T: Clone + Send + 'static> Table<T> {
    /// 加入（或发起）一次单飞刷新。
    ///
    /// 返回 [`Join::Leader`] 时本调用抢到了刷新权：做完真实请求后**必须**调用
    /// `guard.finish(result)`（成功与失败都要）；直接 drop 等价于「取消」，
    /// 等待者会收到 [`CANCELLED_MESSAGE`]。
    ///
    /// 返回 [`Join::Waiter`] 时已有同 key 的刷新在飞，`waiter.wait().await` 等它。
    pub fn join(&self, key: &str) -> Join<'_, T> {
        let mut entries = self.lock();
        if let Some(flight) = entries.get(key) {
            // 订阅先于返回：命中这一格蕴含「信号尚未发出」（发出前表项已移除），
            // 所以这次订阅不会错过唤醒
            let receiver = flight.signal.subscribe();
            let flight = Arc::clone(flight);
            drop(entries);
            return Join::Waiter(Waiter { flight, receiver });
        }
        let flight = Arc::new(Flight::new());
        entries.insert(key.to_string(), Arc::clone(&flight));
        drop(entries);
        Join::Leader(LeaderGuard {
            key: key.to_string(),
            flight,
            table: self,
            finished: false,
        })
    }
}

/// `Table::join` 的结果
pub enum Join<'a, T> {
    /// 本调用是 leader（持有 RAII 守卫）
    Leader(LeaderGuard<'a, T>),
    /// 本调用是等待者
    Waiter(Waiter<T>),
}

/// leader 的 RAII 守卫：正常结束走 [`LeaderGuard::finish`]，被取消走 `Drop`。
///
/// 两条路径都会「移除表项 + 写结果 + 发信号」，所以不会留下永久占位，
/// 也不会让等待者永久挂起。
pub struct LeaderGuard<'a, T> {
    key: String,
    flight: Arc<Flight<T>>,
    table: &'a Table<T>,
    finished: bool,
}

impl<'a, T> LeaderGuard<'a, T> {
    /// 落地本轮结果（成功与失败都走这里）。
    ///
    /// 顺序是**先移除表项、再写结果**：这样 `force = true` 的下一轮一定发起
    /// 新请求，而不是撞上刚完成的这一格去复用旧结果。
    pub fn finish(mut self, result: Result<T, GatewayError>) {
        self.table.detach(&self.key, &self.flight);
        self.finished = true;
        self.flight.set_outcome(result);
        self.flight.signal();
    }
}

impl<'a, T> Drop for LeaderGuard<'a, T> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // 取消路径（future 被 drop / 提前返回）：释放占位并唤醒等待者，
        // 让后续请求可以重新发起刷新
        self.table.detach(&self.key, &self.flight);
        self.flight
            .set_outcome(Err(GatewayError::with_status(409, CANCELLED_MESSAGE)));
        self.flight.signal();
    }
}

/// 等待者句柄
pub struct Waiter<T> {
    flight: Arc<Flight<T>>,
    receiver: watch::Receiver<u64>,
}

impl<T: Clone> Waiter<T> {
    /// 等那一轮的结果（leader 成功/失败/取消都会结束等待）。
    ///
    /// 结果原样透传（含状态码）；等待超时给 409（leader 卡住，调用方可重试）。
    pub async fn wait(mut self) -> Result<T, GatewayError> {
        for _ in 0..WAIT_STEPS {
            if let Some(outcome) = self.flight.outcome() {
                return outcome;
            }
            let waited = tokio::time::timeout(
                Duration::from_millis(WAIT_STEP_MS),
                self.receiver.changed(),
            )
            .await;
            match waited {
                // 信号到达：下一轮读结果
                Ok(Ok(())) => {}
                // 发送端关闭：不会发生（leader 结束前一直持有 Arc），按超时继续轮询
                Ok(Err(_)) => {}
                // 本步超时：继续轮询（覆盖「leader 卡在网络上」的场景）
                Err(_) => {}
            }
        }
        Err(GatewayError::with_status(409, WAIT_TIMEOUT_MESSAGE))
    }
}
