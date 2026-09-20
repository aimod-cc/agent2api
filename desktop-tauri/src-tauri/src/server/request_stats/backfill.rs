//! 旧聚合行的**口径回填**：把「没记账号维度」的历史日子用明细重算一遍。
//!
//! ── 要解决的问题 ────────────────────────────────────────────
//! `accountStats` 是后加的键。在它上线之前写出的聚合行都没有这一维，
//! 而报表的区间统计**只走聚合**（热力图固定 365 天、`all` 可能跨年，
//! 都超出明细的 30 天保留期，靠明细算会越算越少）。于是升级后打开报表，
//! 那段时间的请求全部落进「未知账号」组 —— 数据没丢，但用户看到的是
//! 一个巨大的未知块，账号排行整段失效。
//!
//! 而明细里其实**有**账号身份（旧版本就在记 `accountId` / `accountName`），
//! 只是聚合行当时没存。所以这段历史是可以真正救回来的：
//! 拿明细重算那些天的聚合行，而不是给它们猜一个值。
//!
//! ── 为什么是「整行重算」而不是只补账号那一列 ──────────────────
//! 只补 `accountStats` 就得手工改 `requests` / `tokens` 之外的字段，
//! 三个维度（模型 / provider / 账号）与总量的对账关系要手写第二遍 ——
//! 而「各维之和 = 当天总量」正是报表之间能对账的前提（见 `range_totals`）。
//! 走 `fold_into_daily` 单遍重算，这条不变式由**同一段代码**保证，
//! 与 `record` 记账、`clear_where` 重算完全同源。
//!
//! ── 代价：这些天的数字会有小变化 ─────────────────────────────
//! 重算用的是**今天的口径**，与旧行当时的口径可能不同，具体三处：
//!   ① 旧版本失败请求也照记 token（`normalize` 那时没有失败清零），
//!      重算前先过 `normalized_for_aggregate` 把失败行的 token 清零；
//!   ② 成功判定从「只看 2xx」收紧为「2xx 且无错误摘要」，个别行会由成功转失败；
//!   ③ 聚合是延迟落盘的（见 `should_flush_daily`），进程被强杀会丢掉最后一个
//!      窗口的累计，于是明细可能比聚合行**多**几条 —— 重算后当天总量随明细变大。
//! ① ② 让数字变小、③ 让它变大，量级都在「几条到几十条请求」这一档。
//! 这是**有意的取舍**：宁可让这几天的数字与明细对齐，也不留一个救不回来的
//! 未知块。升级那天恰好是进程退出时刻，所以 ③ 几乎必然发生一次。
//!
//! ── 拿不准就不动 ────────────────────────────────────────────
//! 明细会被保留期裁掉（默认 30 天），超出这个窗口的日子只剩聚合行 ——
//! 那些天**没有**重算的依据，只能继续留在「未知账号」组里。
//! 判据是 `明细条数 >= 聚合行的 requests`：少于它说明明细已被裁过，
//! 拿残缺的明细重算会让当天数字凭空缩水，那比留着未知更糟。
//! 这条判据也顺带让本函数**幂等**：回填过的行有了 `accountStats`，
//! 下次启动不再进候选，数字不会再动。

use std::collections::{BTreeMap, BTreeSet};

use super::clock::{date_key, day_of};
use super::fold_into_daily;
use super::record::{DailyEntry, RequestEntry};

/// 回填所有「聚合行没记账号维度、且明细足以重算」的日子。
///
/// 就地改写 `daily`，返回真正被重算的日期（升序，供调用方落盘与记日志）。
/// 返回空表 = 没有任何一天需要修，调用方不必写盘。
///
/// 只碰候选日：明细完整、且该行已经记过账号维度的日子一律不动 ——
/// 那些天的数字是当时逐条累加出来的，重算只会引入无谓的漂移。
pub(super) fn rebuild_legacy_days(
    daily: &mut BTreeMap<String, DailyEntry>,
    entries: &[RequestEntry],
) -> Vec<String> {
    // ① 候选日：有请求、却没有账号明细的行。
    //    只判 `account_stats.is_empty()` 而不看 `requests > 0` 之外的条件：
    //    全部请求都没有账号身份的日子，聚合行里也会留下一个空 id 的组
    //    （`fold_into_daily` 恒建组），所以「空表」干净地等价于「当时没记这一维」。
    let candidates: BTreeSet<String> = daily
        .iter()
        .filter(|(_, day)| day.requests > 0 && day.account_stats.is_empty())
        .map(|(key, _)| key.clone())
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    // ② 按本地日期把候选日的明细归堆，逐条过今天的口径再累加。
    //    这里**不**顺手把明细里出现的其它日期也建出来：只有聚合行里已有、
    //    且缺账号维度的日子才需要修，多建出来的行会变成「聚合比明细多一天」。
    let mut rebuilt: BTreeMap<String, DailyEntry> = BTreeMap::new();
    for item in entries {
        let key = date_key(day_of(item.ts));
        if !candidates.contains(&key) {
            continue;
        }
        let day = rebuilt
            .entry(key.clone())
            .or_insert_with(|| DailyEntry::new(key.clone()));
        fold_into_daily(day, &item.normalized_for_aggregate());
    }

    // ③ 只替换「明细足以覆盖当天总量」的日子。
    //    `rebuilt` 里没有的候选日（明细全被裁光）不进这一步，原行原样留着 ——
    //    绝不能让「明细为 0 条」被当成「那天没有请求」而把行删掉。
    let mut changed = Vec::new();
    for (key, day) in rebuilt {
        let Some(existing) = daily.get(&key) else {
            continue;
        };
        if day.requests < existing.requests {
            continue;
        }
        changed.push(key.clone());
        daily.insert(key, day);
    }
    changed
}

/// 回填的日志文案（`[Stats]` 通道）。
///
/// 逐日列出而不是只报个数：这几天的数字与用户昨天看到的可能不同，
/// 出问题时日志里要有「哪几天被改过」这条线索。
pub(super) fn report_line(changed: &[String]) -> String {
    format!(
        "已按明细回填 {} 天的账号维度（{}）：这些天此前没有账号明细，\
         重算后该日总量以明细为准，与旧值可能有小幅差异",
        changed.len(),
        changed.join(" / ")
    )
}
