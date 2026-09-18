//! 本地时区工具 —— 与 `core::auto_checkin` 同一套做法（`chrono` + `Local`）。
//!
//! 为什么单独一层：全模块对「一天」「一小时」的切分口径必须**只有一处实现**。
//! 热力图按天、缓存趋势按整点、保留期按天裁剪，只要有一处用了 UTC 或自己算
//! 偏移，报表内部就会自相矛盾（比如热力图的 9 月 18 日与「今天」不是同一天）。
//! 所以这里只放纯函数，且只依赖 `chrono::Local`。
//!
//! 与 `core::auto_checkin::local_date_key` 的一致性：那是既有先例 ——
//! `DateTime<Local>` + `format("%Y-%m-%d")`，**不能用 UTC**（UTC+8 的凌晨
//! 按 UTC 归档会落到前一天，整份报表的日期口径都会偏一格）。
//! 这里沿用同一套 API，不引入新依赖、不自己算时区偏移。

use chrono::{DateTime, Local, NaiveDate, TimeZone, Timelike};

/// 当前毫秒 Unix 时间戳（复用 logging 的实现，避免两处写法漂移）
pub(super) fn now_ms() -> i64 {
    crate::server::logging::now_ms()
}

/// 本地时区的今天
pub(super) fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// 毫秒时间戳 → 本地时区时间。
///
/// `single()` 对任何合法时间戳恒有值；只有手改文件塞进越界数值才会返回 None，
/// 这时回落到 epoch 而不是 unwrap panic —— 一条脏数据的代价不该是整页崩掉
/// （`DateTime<Local>` 是 DST 模糊时段的唯一取舍点，用 `single()` 而不是
/// `earliest()` 是因为时间戳本身无歧义，不需要再挑一个）。
pub(super) fn ms_to_local(ts: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .or_else(|| Local.timestamp_millis_opt(0).single())
        .unwrap_or_else(Local::now)
}

/// 毫秒时间戳落在**本地时区**的哪一天（明细归档、按天裁剪都用它）
pub(super) fn day_of(ts: i64) -> NaiveDate {
    ms_to_local(ts).date_naive()
}

/// 本地日期键 `YYYY-MM-DD`。
///
/// 定长 + 字典序即时间序，所以日期比较可以直接用字符串比较
/// （聚合表用 `BTreeMap<String, _>` 的键就是它）。
pub(super) fn date_key(day: NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

/// 本地整点键 `YYYY-MM-DDTHH`（近 24 小时趋势用）。
/// 取 `%H` 前先把时间戳还原成 `DateTime<Local>`，于是「14 点」就是用户
/// 时钟上的 14 点，不是 UTC 的 14 点。
pub(super) fn hour_key(now: DateTime<Local>) -> String {
    now.format("%Y-%m-%dT%H").to_string()
}

/// 本地整点（分/秒/纳秒清零）。
/// `with_*` 在夏令时切换当天可能返回 None，这时退回原值并只取到小时 ——
/// 宁可用一个近似值也不要让趋势曲线少一格（中国无夏令时，实际走不到）。
pub(super) fn hour_floor(now: DateTime<Local>) -> DateTime<Local> {
    now.with_minute(0)
        .and_then(|value| value.with_second(0))
        .and_then(|value| value.with_nanosecond(0))
        .unwrap_or(now)
}

/// 某个本地自然日的零点对应的毫秒时间戳。
///
/// 为什么需要它：明细按 ts 升序排好，要按「保留 N 天」裁掉头部时，
/// 把日期边界换算成一个**数值**下界就能用 `partition_point` 一次定位
/// （O(log n)、不逐条分配字符串），比拿日期串逐条比较省得多 ——
/// 这个裁剪在每次记账时都会跑，必须足够便宜。
///
/// 夏令时切换当天零点可能不存在（`LocalResult::None`），这时取当天最早
/// 的合法时刻（`earliest()`）而不是猜一个偏移；中国无夏令时，实际走不到。
pub(super) fn local_midnight_ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .and_then(|naive| Local.from_local_datetime(&naive).earliest())
        .map(|value| value.timestamp_millis())
        // 兜底：连当天零点都构造不出来时退到 epoch（日期极端越界才会发生）
        .unwrap_or(0)
}
