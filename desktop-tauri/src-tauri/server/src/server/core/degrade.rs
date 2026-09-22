//! 内容拦截降级状态机（照搬 workbuddy2api 的 `internal/server/degrade.go`）。
//!
//! ── 它解决什么 ───────────────────────────────────────────────
//! `passthrough` / `append` 模式下，请求被上游内容策略拦截（HTTP 400 + 审核
//! 文案）时，绝大多数是**指纹误报**：客户端注入的 system 模板句被逐字匹配拦下，
//! 而账号本身健康（余额够、没限流、session 没死）。此时的动作是：
//!
//! ```text
//!   ① 立即换最小中性提示词（`prompt::DEGRADED_PROMPT`）同账号重试一次；
//!   ② 同时**触发降级**：接下来一段时间内，passthrough/append 模式的请求
//!      直接带着中性提示词出门，不再先撞一次 400 再补救。
//! ```
//!
//! 降级期到**次日 00:00（UTC+8）**结束 —— 与参考项目同一个重置口径：拦截窗口
//! 通常与「当天」绑定（上游侧的风控按天统计），次日零点是一次天然的重置点，
//! 而「无限期降级」会让用户在问题早就消失之后仍然拿不到自己的 system 提示词。
//!
//! ── 状态放内存、重启清零（与参考项目一致）─────────────────────
//! 它是**运行期状态**而不是配置：进程重启等于一次人工介入（用户多半正是
//! 因为撞了 400 才重启），把降级带过去没有意义。落盘还会引入「什么时候该清」
//! 的第二套判据（跨天？跨版本？），得不偿失。
//!
//! ── 触发不续期 ───────────────────────────────────────────────
//! 降级期内再次触发**不延长**截止时间（保持最早那个零点）：否则一条持续报错的
//! 客户端会把降级期无限推后，等于永久降级。这是参考项目明确写下的语义
//! （`Trigger` 的「已在降级期内则不续期」）。
//!
//! ── 谁在读它 ─────────────────────────────────────────────────
//!   - `upstream::provider_loop`：请求开始时取一次（决定首发的提示词文本），
//!     撞内容拦截时触发它并立即重发一次；
//!   - `api::prompt`：设置页要显示「现在是不是降级期、到什么时候」——
//!     用户改了提示词却看不到效果时，这个读数就是答案。

use std::sync::atomic::{AtomicI64, Ordering};

/// UTC+8 的固定偏移（毫秒）。
///
/// 用固定偏移而不是时区库：中国标准时间没有夏令时，`Asia/Shanghai` 在
/// 1970 年之后恒为 +08:00 —— 参考项目也是用 `time.FixedZone("CST", 8*3600)`
/// 算的（理由相同：不依赖宿主机时区配置）。
const CST_OFFSET_MS: i64 = 8 * 3600 * 1000;

/// 一天的毫秒数
const DAY_MS: i64 = 86_400_000;

/// 降级截止时刻（UTC 毫秒）；`0` = 未触发。
///
/// 用 `AtomicI64` 而不是 `Mutex<Option<...>>`：状态就是一个数，读写都是
/// 「取一次 / 存一次」，没有需要原子维护的复合不变量。并发触发最坏是
/// 两个线程算出同一个值（同一天、同一个零点），无害。
static UNTIL_MS: AtomicI64 = AtomicI64::new(0);

/// 当前是否处于降级期。
pub fn active() -> bool {
    until_ms() > 0
}

/// 降级截止时刻（UTC 毫秒）；`0` = 当前不在降级期。
///
/// 过期的值在这里就被视为「没有」——于是调用方不必自己比较时间，
/// 也不会出现「状态机说降级中、界面说已结束」这类分叉。
pub fn until_ms() -> i64 {
    let until = UNTIL_MS.load(Ordering::SeqCst);
    if until > 0 && crate::server::logging::now_ms() < until {
        until
    } else {
        0
    }
}

/// 触发降级（返回本次生效的截止时刻，UTC 毫秒）。
///
/// 已在降级期内**不续期**：直接返回原来的截止时刻（见模块头）。
pub fn trigger() -> i64 {
    let now = crate::server::logging::now_ms();
    let current = UNTIL_MS.load(Ordering::SeqCst);
    if current > now {
        return current;
    }
    let next = next_midnight_cst_ms(now);
    UNTIL_MS.store(next, Ordering::SeqCst);
    next
}

/// 解除降级（把截止时刻清零）。
///
/// 参考项目没有这个动作（它只能等到零点）；本项目留一个显式出口，是因为
/// 用户可能**当场就把提示词改好了**（比如切到 `custom` 模式），这时还要等到
/// 零点才恢复自己的 system 提示词，纯属折磨。由设置页的接口调用。
pub fn clear() {
    UNTIL_MS.store(0, Ordering::SeqCst);
}

/// `now` 之后最近的 UTC+8 零点（纯函数，便于核对边界）。
///
/// 边界语义（与参考项目逐条一致）：
///   - `23:59` → 次日 `00:00`（几分钟后）；
///   - 恰好 `00:00` → **次日** `00:00`（刚过零点，下个零点是明天）。
///
/// 算法：把时刻平移到 CST 视角（+8h），当天零点就是 `div_euclid(DAY) * DAY`，
/// 再加一天即为「最近的未来零点」；`div_euclid` 保证 1970 年之前的负数时刻
/// 也落在正确的整天边界上（负数除法在 Rust 里默认向零取整，直接 `/` 会算错）。
fn next_midnight_cst_ms(now_ms: i64) -> i64 {
    let shifted = now_ms + CST_OFFSET_MS;
    let start_of_today = shifted.div_euclid(DAY_MS) * DAY_MS;
    start_of_today + DAY_MS - CST_OFFSET_MS
}

/// 降级截止时刻的本地化展示（UTC+8，与上游给的恢复时间同口径）。
/// `0` 或无法格式化时给空串 —— 这只是界面文案，绝不能因为它把接口带崩
/// （release 是 panic=abort）。
pub fn until_text() -> String {
    let until = until_ms();
    if until <= 0 {
        return String::new();
    }
    let Some(utc) = chrono::DateTime::from_timestamp_millis(until) else {
        return String::new();
    };
    let Some(offset) = chrono::FixedOffset::east_opt(8 * 3600) else {
        return String::new();
    };
    utc.with_timezone(&offset)
        .format("%Y/%m/%d %H:%M")
        .to_string()
}
