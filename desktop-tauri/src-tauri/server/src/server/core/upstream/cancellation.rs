//! 手动终止：在途请求的取消令牌与进程内注册表（参考 OmniProxy 的
//! `utils/requestTermination.ts`）。
//!
//! ── 它解决什么 ──────────────────────────────────────────────
//! 请求日志页能看到一条请求「进行中」，但此前没有任何手段让它停下来：
//! 上游挂住时它只能等超时（或客户端的重试风暴把列表塞满僵尸行）。本模块
//! 给每条在途请求发一张**取消令牌**并登记在进程级表里（键 = 明细的关联 id），
//! 详情页的「终止请求」按 id 找到令牌并置位；转发链在每一个等待点 select
//! 令牌，置位后立即放弃等待、按「手动终止」收尾（错误帧 + 408 终态）。
//!
//! ── 为什么不用 tokio_util::sync::CancellationToken ──────────
//! 那个 crate 只为一个类型进来不划算（本项目的依赖面刻意收窄）。这里的
//! 语义需求只有两条 ——「置位后所有等待者立即醒来」与「幂等置位」——
//! `AtomicBool + Notify` 十行就能覆盖，且与 `upstream::mod` 里 `InFlight`
//! （去重信号）用的是同一套写法，读代码的人不必为这一处另学一套心智模型。
//!
//! ── 生命周期（谁登记、谁注销）───────────────────────────────
//!   - `register`：三个对话入口在 `record_started` 之后调（那时才有 id）；
//!   - `unregister`：**收尾完成时**由最后一道收尾路径注销 —— 普通路径是
//!     `api::pipeline::DisconnectGuard`（handler 结束时），流式路径是
//!     `RecordingStream::settle`（流结束时）。于是「注册表里有它」恒等于
//!     「这条请求此刻还能被终止」，终止接口据此给 404 / 受理。
//!   - 进程崩溃后注册表自然清空（内存表），重启遗留的行由僵尸清扫收尾
//!     （`RequestStats::sweep_stale_running`），不归本模块管。
//!
//! ── 只控制本进程 ────────────────────────────────────────────
//! 与 OmniProxy 的 `registerActiveRequest` 同一取舍：表只登记本进程发起的
//! 请求（桌面端只有一个进程；即使未来多开，另一个实例的请求在它自己的表里，
//! 这里查不到就如实说「不在进行中」—— 比伪造一个「已受理」诚实）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tokio::sync::Notify;

use crate::server::errors::GatewayError;

/// 手动终止的统一文案（明细的 error 列 / 客户端的错误帧 / 终止接口的提示共用）。
pub const MANUAL_TERMINATED: &str = "请求已被手动终止";

/// 手动终止在明细里的状态码。
///
/// 与「中断」同一族（客户端断开、僵尸清扫都记 408）：对报表而言它们都是
/// 「这一轮没有跑完」，用同一个码聚合不会把「用户主动放弃」误判成上游故障
/// （那要靠 error 文案区分，口径与 `finish_stale_running` 一致）。
pub const MANUAL_TERMINATED_STATUS: i64 = 408;

/// 手动终止的网关错误（转发链的出口形态；客户端与明细拿到的都是它）。
pub fn cancelled_error() -> GatewayError {
    GatewayError::with_status(MANUAL_TERMINATED_STATUS as i32, MANUAL_TERMINATED)
}

/// 一条在途请求的取消令牌。
///
/// 置位是**单向且幂等**的：置位后所有当前与将来的 `cancelled()` 都立即返回，
/// 重复 `cancel()` 不再有额外效果（终止接口因此天然幂等，不必自己做去重）。
pub struct CancelToken {
    /// 已置位标志（`cancelled()` 先查它，兜住「置位早于等待者登记」）
    flag: AtomicBool,
    /// 唤醒信号（`notify_waiters` 只叫醒**当时已登记**的等待者，所以
    /// `cancelled()` 必须「先登记、再查标志」，两个顺序合起来才不漏）
    notify: Notify,
}

impl CancelToken {
    fn new() -> Self {
        Self { flag: AtomicBool::new(false), notify: Notify::new() }
    }

    /// 置位并唤醒所有等待者（幂等；与 `InFlight::finish` 同一手法）
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 是否已置位（同步判据，供转发链的循环顶检查）
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 挂起直到被取消（已取消则立即返回）。
    ///
    /// 「先登记兴趣、再查标志」的顺序与 `upstream::mod` 的 `wait_for_in_flight`
    /// 逐字同源：`notify_waiters` 只唤醒当时已登记的等待者，先查标志会漏掉
    /// 「查完到 await 之间被置位」的窗口。
    pub async fn cancelled(&self) {
        let mut notified = std::pin::pin!(self.notify.notified());
        // enable() 返回 true = 在本次登记之前就已收到过通知（这里不可能，
        // 因为 notify 只在 cancel 时触发且标志优先；仍按既有写法短路）
        if notified.as_mut().enable() {
            return;
        }
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// 在途请求表：关联 id → 取消令牌。
fn registry() -> &'static Mutex<HashMap<String, Arc<CancelToken>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<CancelToken>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取表锁；锁中毒不致命（与账号存储 / 去重表同一策略：保护的是表结构，
/// 没有会被 panic 打断的中间态）
fn lock_registry() -> MutexGuard<'static, HashMap<String, Arc<CancelToken>>> {
    match registry().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 登记一条在途请求，返回它的取消令牌。
///
/// `id` 为空返回 None（那种请求没有进行中行，也无从被终止 —— 与
/// `record_started` 对空 id 的处理同源）。同 id 重复登记会覆盖旧令牌：
/// id 是每请求新生成的 UUID，正常路径不会撞；真撞上（手改库）时以最新一次
/// 转发为准，旧令牌的持有者（上一轮的转发链）读到的是自己的 Arc，不受影响。
pub fn register(id: &str) -> Option<Arc<CancelToken>> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let token = Arc::new(CancelToken::new());
    lock_registry().insert(id.to_string(), token.clone());
    Some(token)
}

/// 注销一条在途请求（幂等：不存在时什么都不做）。
pub fn unregister(id: &str) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    lock_registry().remove(id);
}

/// 受理一次手动终止。
///
/// 返回 `false` = 这条请求不在在途表里 —— 已结束、来自上次启动的遗留行，
/// 或 id 打错。调用方（终止接口）据此给 404 而不是谎报「已受理」。
pub fn cancel(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() {
        return false;
    }
    // 取出令牌后**立刻放锁**再置位：置位会唤醒等待者，不该在持锁期间跑
    // 别人的唤醒逻辑（与 `flush_live` 对 `live` 锁的处置同一纪律）
    let token = lock_registry().get(id).cloned();
    match token {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}
