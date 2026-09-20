//! 请求统计存储：`{config_dir}/requests.jsonl`（明细）+ `request-daily.jsonl`（按天聚合）。
//!
//! ── 内部分层（拆开是为了每片都保持单一职责、单文件不过长）─────
//! ```text
//! server/
//!   request_stats.rs     存储本体：载入 / 记账 / 落盘 / 裁剪 / 查询（本文件）
//!   request_stats/
//!     record.rs   数据类型与 JSON 契约（RequestEntry / DailyEntry / Retention…）
//!     report.rs   报表计算：纯函数（区间、补零、命中率、topModel、连续天数）
//!     clock.rs    本地时区工具（chrono::Local 在整模块的唯一使用点）
//! ```
//!
//! ── 读侧接口的接线状态 ──────────────────────────────────────
//! 读侧（`usage_summary` / `query_requests` / `prune` / `stats` / `clear`）
//! 已由 `api::stats_api` 的三条路由接上，这些接口上的 `#[allow(dead_code)]`
//! 已全部移除：新增的公开接口若没人调用会直接报 warning，便于及时发现漏接的路由。
//! 仍保留 allow 的只有 `file()` / `daily_file()` 两个「排障直读」访问器
//! （理由写在各自的注释里）。
//!
//! ── 为什么是两个文件 ────────────────────────────────────────
//!   - `requests.jsonl`：请求日志，一行一条 JSON。短窗口指标（缓存命中率的
//!     10 分钟 / 1 小时窗口、近 24 小时趋势）必须逐条算，按天的聚合行给不出
//!     这种精度。默认只留 30 天，避免文件无限增长。
//!   - `request-daily.jsonl`：按天聚合，一行一天。热力图固定 365 天、`all`
//!     区间可能跨年，都超出明细的保留期，所以这份**寿命独立于明细**：
//!     明细被裁掉后当天的聚合行仍在，历史曲线不会因为裁明细而出现空洞。
//!
//! ── 落盘策略（两套，取舍见各自函数注释）──────────────────────
//!   - 明细：追加写为主；仅当「超期裁剪」或「攒够一批」才整文件重写
//!     （照抄 `logs_store.rs` 的 dirty + `COMPACT_STEP` 模式）。
//!   - 聚合：延迟落盘（变更满 N 次或距上次落盘超 M 秒才重写整个文件）。
//!
//! ── 并发模型（与 `logs_store.rs` 同构）────────────────────────
//! 所有公开方法取 `&self`，内部一把 `Mutex` 包住「内存快照 + 落盘」整体操作。
//! 锁中毒时 `poisoned.into_inner()` 继续用（与 `core::egress` / `core::update`
//! 等处一致）：统计数据的完整性远不如「服务不因统计而崩」重要。

mod backfill;
mod clock;
mod record;
// 报表计算层（纯函数）与「模型请求日志」的查询实现。
//
// 这里**不放模块级 allow**：本层已由 `api::stats_api` 的报表路由接线，
// 于是「新增了一个算好却没人用的函数」会直接报 warning —— 那正是我们想知道的。
// 模块内部各函数仍应是 `pub(super)`：只有 `request_stats.rs` 能调到它们，
// 对外暴露面收敛在这一层。
mod report;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use chrono::Duration as ChronoDuration;
use serde::Serialize;
use serde_json::{json, Value};

use crate::server::logging;

use clock::{date_key, day_of, local_midnight_ms, today};
use report::{
    build_accounts, build_providers, build_top_model, cache_rates, cache_trend_24h, daily_trend,
    entry_json, heatmap, normalize_range, normalize_status_filter, push_account_accum,
    push_model_accum, push_provider_accum, range_bounds, range_totals, streak,
};
use record::{DailyEntry, MAX_DAILY_DAYS, MAX_ENTRIES, COMPACT_STEP};

pub use record::{
    NewRequestEntry, RequestEntry, RequestQuery, Retention, DEFAULT_LIMIT, MAX_LIMIT,
};

/// 明细文件名
pub const FILE_NAME: &str = "requests.jsonl";
/// 按天聚合文件名
pub const DAILY_FILE_NAME: &str = "request-daily.jsonl";

/// 聚合延迟落盘：累计变更满这么多次（每次 `record` 记一次）就重写整个文件
const DAILY_FLUSH_CHANGES: usize = 50;
/// 聚合延迟落盘：距上次落盘超过这么久就重写整个文件
const DAILY_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// 请求统计存储本体。所有公开方法取 `&self`，内部 `Mutex` 串行化。
pub struct RequestStats {
    /// 保存目录。包 `RwLock` 是为了**运行中换目录**（迁移，见 `relocate`）：
    /// 与 `LogStore::directory` 同一做法 —— 写路径都持 `inner` 锁后才碰文件，
    /// 锁序恒为 inner → directory；明细 / 聚合两个文件路径都由目录派生，
    /// 不另存字段（「目录换了文件跟着换」只此一份事实）。
    directory: RwLock<PathBuf>,
    /// 保留期取值回调。**每次裁剪时动态调用**，于是设置改完天数下一次裁剪
    /// 就用新值，不需要重启进程（这正是把保留期做成回调而非构造参数的原因）。
    get_retention: Arc<dyn Fn() -> Retention + Send + Sync>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// 明细，**恒按 ts 升序**（见 `insert_sorted`）
    entries: Vec<RequestEntry>,
    /// 按本地自然日聚合，键 `YYYY-MM-DD`。
    ///
    /// 用 `BTreeMap` 而非 `Vec`：日期键定长且字典序即时间序，
    /// 于是「区间求和」「逐天补零」「连续天数」都能直接按范围取，
    /// 不必每次报表都线性扫一遍全表再自己找日期。
    daily: BTreeMap<String, DailyEntry>,
    /// 下次写入时整文件重写明细（启动超限 / 攒够一批 / 裁剪过）
    dirty: bool,
    appends_since_compact: usize,
    /// 聚合：自上次落盘以来的变更天数
    daily_changes: usize,
    /// 聚合上次落盘时刻。None = 本次进程还没落过盘（首次记录时立刻写一次，
    /// 免得进程在启动后头一分钟被强杀丢掉当天聚合）
    daily_flushed_at: Option<Instant>,
}

impl RequestStats {
    /// 构造并载入历史。目录不存在不报错（首次写入时自动创建）。
    ///
    /// 签名与 `LogStore::new(directory, get_retention_days)` 对齐：两者都是
    /// 「数据目录 + 保留期回调」这一对参数，构造方式不该有两套写法。
    /// 传 `|| Retention::default()` 即可得到默认保留期
    /// （当前唯一调用点 `ServerState::bootstrap` 传的是读配置的闭包，见那边的注释）。
    pub fn new(
        directory: impl AsRef<Path>,
        get_retention: impl Fn() -> Retention + Send + Sync + 'static,
    ) -> Self {
        let store = Self {
            directory: RwLock::new(directory.as_ref().to_path_buf()),
            get_retention: Arc::new(get_retention),
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                daily: BTreeMap::new(),
                dirty: false,
                appends_since_compact: 0,
                daily_changes: 0,
                daily_flushed_at: None,
            }),
        };
        store.load();
        store
    }

    /// 当前保存目录（迁移会换它，读侧每次都取现值而不是缓存）
    fn directory_of(&self) -> PathBuf {
        match self.directory.read() {
            Ok(guard) => guard.clone(),
            // 中毒恢复：目录路径是纯数据，继续用内部值（与 inner 的取向一致）
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 当前明细文件完整路径（由目录派生）
    pub fn request_file(&self) -> PathBuf {
        self.directory_of().join(FILE_NAME)
    }

    /// 当前聚合文件完整路径（由目录派生）
    pub fn daily_file(&self) -> PathBuf {
        self.directory_of().join(DAILY_FILE_NAME)
    }

    /// 取锁；中毒时继续用内部值（统计不该让服务崩，取向与 logs_store 一致）
    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 当前保留期（每次调用都取回调的实时值并归一化）
    fn retention(&self) -> Retention {
        (self.get_retention)().normalized()
    }

    /// 保留期的两条边界：明细用毫秒下界，聚合用日期键下界。
    ///
    /// 两者都从「本地今天」往前推，且**每次调用都重算**（回调动态取），
    /// 于是设置里改完天数，下一次记账/裁剪就生效，不需要重启进程。
    fn retention_bounds(&self) -> RetentionBounds {
        let retention = self.retention();
        let now_day = today();
        RetentionBounds {
            // 保留 N 天 = 含今天在内的 N 个自然日，所以往前推 N-1 天
            requests_ms: local_midnight_ms(now_day - ChronoDuration::days(retention.request_days - 1)),
            daily_key: date_key(now_day - ChronoDuration::days(retention.daily_days - 1)),
        }
    }

    /// 载入历史：逐行解析、**跳过损坏行**（不影响其余数据），再按保留期裁剪。
    ///
    /// 明细与聚合各自独立载入、独立裁剪 —— 「聚合寿命独立于明细」这条契约
    /// 就是从加载路径开始成立的。
    fn load(&self) {
        let bounds = self.retention_bounds();

        let mut entries = load_requests(&self.request_file());
        // 手改或旧版本写出的文件可能不是有序的；载入时统一排一次序，
        // 之后由 insert_sorted 维持升序不变式
        entries.sort_by_key(|item| item.ts);
        // 被保留期裁掉、或超出环形上限：两种情况下**文件都比内存多**，
        // 与 logs_store 的 dirty 语义一致 —— 标上它，下一次写入把整文件收敛。
        // （只是裁掉内存里的旧行、文件还留着的话，重开程序又会载回来。）
        let entries_trimmed = trim_entries(&mut entries, bounds.requests_ms);
        let over_limit = trim_entries_to_limit(&mut entries);
        let needs_compact = entries_trimmed || over_limit;

        let mut daily = load_daily(&self.daily_file());
        let daily_trimmed = trim_daily(&mut daily, &bounds.daily_key);

        // 旧聚合行的口径回填：`accountStats` 是后加的键，上线前写出的聚合行
        // 没有账号维度，报表只走聚合（跨年区间超出明细的 30 天保留期），
        // 那段时间会整段落进「未知账号」。明细里有账号身份，可以真正重算回来。
        // 取舍与安全边界见 `backfill` 模块头。
        let backfilled = backfill::rebuild_legacy_days(&mut daily, &entries);

        {
            let mut guard = self.lock();
            // dirty 只管**明细**文件：它表示「文件内容比内存多」，
            // 下次写入时整文件收敛。聚合被裁剪不影响明细文件的正确性，
            // 不必因此多写一次明细（它可能很大）。
            guard.dirty = needs_compact;
            guard.appends_since_compact = 0;
            guard.daily_changes = 0;
            guard.daily_flushed_at = None;
            guard.entries = entries;
            guard.daily = daily;
        }
        // 聚合被裁掉过期行、或刚回填过历史日子时立即落盘一次：启动期写一次
        // 几十 KB 的成本可忽略，换来的是「文件里就是内存里那一份」——
        // 裁掉的行重开程序不会又回来，回填的结果也不会因为进程退出而白算一遍
        if daily_trimmed || !backfilled.is_empty() {
            let snapshot: Vec<DailyEntry> = self.lock().daily.values().cloned().collect();
            let text = render_jsonl(&snapshot);
            self.write_all(&self.daily_file(), &text);
        }
        let (count, days) = {
            let guard = self.lock();
            (guard.entries.len(), guard.daily.len())
        };
        // 只用控制台通道：往日志库里写「统计库已载入」会绕回统计自身
        // （logs_store 载入时同样只用 console_line）
        logging::console_line(
            "[Stats]",
            &format!("已载入请求统计 {count} 条明细 / {days} 天聚合"),
        );
        if !backfilled.is_empty() {
            // 逐日列出而不是只报个数：这几天的数字与用户昨天看到的可能不同，
            // 出问题时日志里要有「哪几天被改过」这条线索
            logging::console_line("[Stats]", &backfill::report_line(&backfilled));
        }
    }

    /// 记一条请求：写明细 + 更新当天聚合。
    ///
    /// 落盘在**持锁期间**完成，与 logs_store 同理：内存快照与文件内容要么
    /// 一起推进要么一起不动，否则并发记账时整文件重写会吃掉刚追加的行。
    /// 记账是低频事件（每个请求一次），持锁写盘的开销可以接受。
    pub fn record(&self, entry: NewRequestEntry) {
        let record = entry.normalize();
        let mut guard = self.lock();

        // ── 明细：二分插入保持 ts 升序 ──────────────────────────
        insert_sorted(&mut guard.entries, record.clone());

        // ── 聚合：按本地时区算日期，当天行累加 ──────────────────
        // （累计逻辑在 `fold_into_daily`，与 clear_where 的重算共用一份）
        let date = date_key(day_of(record.ts));
        {
            let day = guard.daily.entry(date.clone()).or_insert_with(|| DailyEntry::new(date));
            fold_into_daily(day, &record);
        }
        guard.daily_changes += 1;

        // ── 保留期：每次记账顺手把超期数据裁掉 ────────────────────
        // 只在启动与 prune 时裁是不够的：桌面端常驻数周不重启，
        // 那样明细会一直涨到环形上限（20k）才开始丢，用户设的 30 天等于没生效。
        let bounds = self.retention_bounds();
        let expired = trim_entries(&mut guard.entries, bounds.requests_ms);
        if expired {
            // 内存比文件少 → 标 dirty，让文件在本次写入时收敛掉这些行
            guard.dirty = true;
        }
        if trim_daily(&mut guard.daily, &bounds.daily_key) {
            guard.daily_changes += 1;
        }

        // ── 明细落盘：追加为主，攒够一批或已标 dirty 才整文件重写 ──
        // 与 logs_store 的 persist 判定一致：只有「文件比内存多」或
        // 「满额后已攒够 COMPACT_STEP 次追加」才重写，否则只追加一行
        let compact = guard.dirty || guard.appends_since_compact >= COMPACT_STEP;
        if compact {
            guard.dirty = false;
            guard.appends_since_compact = 0;
            let text = render_jsonl(&guard.entries);
            self.write_all(&self.request_file(), &text);
        } else {
            if guard.entries.len() >= MAX_ENTRIES {
                guard.appends_since_compact += 1;
            }
            if let Ok(line) = serde_json::to_string(&record) {
                self.write_append(&self.request_file(), &line);
            }
        }

        // ── 聚合落盘：延迟写（取舍见 should_flush_daily）──────────
        if should_flush_daily(&guard) {
            let snapshot: Vec<DailyEntry> = guard.daily.values().cloned().collect();
            let text = render_jsonl(&snapshot);
            self.write_all(&self.daily_file(), &text);
            guard.daily_changes = 0;
            guard.daily_flushed_at = Some(Instant::now());
        }
    }

    /// 报表聚合（纯内存计算，不碰磁盘）。
    ///
    /// `range` 的非法值一律按 `"7"` 处理，并在结果里回显归一化后的值：
    /// 报表是只读展示，为一次拼错的参数让整页报错，不如给个合理默认
    /// （前端也不用为这个场景做错误态）。路由层（`api::stats_api::stats_summary`）
    /// 另外对非法值给 400 —— 那是用户在选择器上显式选的值，静默换区间会
    /// 让页面显示的数据与选项对不上；两层各管一件事，这里保留兜底。
    pub fn usage_summary(&self, range: &str) -> Value {
        let guard = self.lock();
        let now_day = today();
        let range = normalize_range(range);
        let (start_date, end_date) = range_bounds(range, &guard.daily, now_day);

        // overview / dailyTrend / heatmap 走聚合（跨年；明细只有请求天数）
        let totals = range_totals(&guard.daily, &start_date, &end_date);
        let top_model = build_top_model(&totals.model_totals, totals.tokens);
        // providers 与 topModel **同源同区间**：都从这次的 `range_totals` 出，
        // 于是「按 provider 的请求数之和」必然等于 overview.requests，
        // 前端把它们并排显示时不会出现互相对不上的数
        let providers = build_providers(&totals.provider_totals);
        // accounts 与 providers / topModel **同源同区间**（同上）：账号排行的
        // 请求数之和也等于 overview.requests
        let accounts = build_accounts(&totals.account_totals);
        let trend = daily_trend(&guard.daily, &start_date, &end_date);
        let map = heatmap(&guard.daily, now_day);
        let consecutive = streak(&guard.daily, now_day);

        // cacheRates / cacheTrend24h 走明细（窗口 ≤7 天，明细够用）
        let now = clock::now_ms();
        let rates = cache_rates(&guard.entries, now);
        let cache_trend = cache_trend_24h(&guard.entries, now);

        json!({
            "range": range,
            "startDate": start_date,
            "endDate": end_date,
            "overview": {
                "requests": totals.requests,
                "successful": totals.successful,
                "tokens": totals.tokens,
                "activeDays": totals.active_days,
                "streak": consecutive,
                "topModel": top_model,
            },
            // 按 provider 维度的区间汇总（**新增字段，不改既有字段**）。
            // 前端按「存在则展示、缺失则隐藏」消费，所以旧前端拿到它只会忽略。
            // 恒为数组（无数据时是空数组而不是 null）：前端不必判两种空形态
            "providers": providers,
            // 按账号维度的区间汇总（**新增字段，不改既有字段**，与 providers 同形态）。
            // 账号是比 provider 更细的一维（一家可挂多个账号），所以这张排行回答的是
            // 「具体哪个登录态在出力」——同一家的多个账号会各占一行。
            "accounts": accounts,
            "heatmap": map,
            "cacheRates": rates,
            "cacheTrend24h": cache_trend,
            "dailyTrend": trend,
        })
    }

    /// 明细查询：倒序（新在前）分页。
    ///
    /// 内存里恒按 ts 升序，所以倒序 == 反向遍历：不需要每次查询都排序，
    /// 也不会出现「长请求晚收尾导致记录顺序飘忽」的翻页错乱。
    pub fn query_requests(&self, filter: &RequestQuery) -> Value {
        let guard = self.lock();
        let total = guard.entries.len();
        // limit 夹在 [1, MAX_LIMIT]：前端传 0 或十万都不该让接口躺平
        let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        // 过滤链在 `matches_filter`（与 clear_where 共用一份，「看到的」与「删的」
        // 必须是同一批，两处手写必然漂移）
        let matched: Vec<&RequestEntry> =
            guard.entries.iter().filter(|item| matches_filter(item, filter)).collect();

        let matched_count = matched.len();
        // 每行经 `entry_json` 补一个派生字段 `providerLabel`（id → label 的换算；
        // 换算处与汇总的 providers 数组同一个函数，两处名字必然一致）。
        // 汇总是**反序列化回 Value**，不是另一套结构：契约字段仍由 record.rs
        // 的 serde 注解决定，这里只加不删
        let entries: Vec<Value> = matched
            .into_iter()
            .rev()
            .skip(filter.offset)
            .take(limit)
            .map(entry_json)
            .collect();

        json!({
            "entries": entries,
            "total": total,
            "matched": matched_count,
        })
    }

    /// 按筛选条件清空明细，并**重算受影响日期**的按天聚合。
    /// 返回 `{ removed: 删除条数, ...stats() }`。
    ///
    /// 为什么聚合要重算而不是留着：聚合行是「当天全部明细」的累计，删掉其中
    /// 一部分后数字就对不上账（报表的请求数会大于明细能数出的请求数）。
    /// 受影响的日期（被删明细涉及的那些天）从**剩余明细**重新聚合 ——
    /// 聚合的全部字段都由明细逐条累加而来（`record` 与重算共用
    /// `fold_into_daily`），口径不会漂。
    ///
    /// 与 `clear()` 同一取舍：持锁写盘、两个文件都立即整份重写、不动保留期设置。
    /// 全部条件都缺省时不会走到这里（路由层直接走 `clear()`，语义 = 全清 +
    /// 聚合也全清，连 `next_id` 之类都不留）。
    pub fn clear_where(&self, filter: &RequestQuery) -> Value {
        let mut guard = self.lock();
        let before = guard.entries.len();
        // ① 摘掉命中的明细，同时记下它们涉及的日子（这些天的聚合要重算）
        let mut affected: BTreeSet<String> = BTreeSet::new();
        guard.entries.retain(|item| {
            if matches_filter(item, filter) {
                affected.insert(date_key(day_of(item.ts)));
                return false;
            }
            true
        });
        let removed = before - guard.entries.len();

        if removed > 0 {
            // ② 从剩余明细重算受影响的日子（单遍扫描，明细上限 2 万条，代价可忽略）
            let mut rebuilt: BTreeMap<String, DailyEntry> = BTreeMap::new();
            for item in &guard.entries {
                let key = date_key(day_of(item.ts));
                if !affected.contains(&key) {
                    continue;
                }
                let day = rebuilt
                    .entry(key.clone())
                    .or_insert_with(|| DailyEntry::new(key.clone()));
                fold_into_daily(day, item);
            }
            // 重算后还有剩余明细的日子 → 替换；一条不剩的日子 → 整天删除
            for key in &affected {
                match rebuilt.remove(key) {
                    Some(day) => {
                        guard.daily.insert(key.clone(), day);
                    }
                    None => {
                        guard.daily.remove(key);
                    }
                }
            }
            guard.daily_changes += 1;

            // ③ 两个文件都立即整份重写（内存里留下的就是文件里该有的）
            let text = render_jsonl(&guard.entries);
            self.write_all(&self.request_file(), &text);
            let snapshot: Vec<DailyEntry> = guard.daily.values().cloned().collect();
            let daily_text = render_jsonl(&snapshot);
            self.write_all(&self.daily_file(), &daily_text);
            guard.dirty = false;
            guard.appends_since_compact = 0;
            guard.daily_changes = 0;
            guard.daily_flushed_at = Some(Instant::now());
        }
        drop(guard);

        let mut stats = self.stats();
        if let Some(object) = stats.as_object_mut() {
            object.insert("removed".to_string(), Value::from(removed as u64));
        }
        stats
    }

    /// 迁移到新目录：明细与按天聚合各按内存整份写到新位置，成功后切换目录
    /// 并删除旧文件。与 `LogStore::relocate` 同一套语义（见那边的说明）：
    /// 内存即有效全集、按内存重写而非复制文件、持锁全程、失败不切目录。
    ///
    /// 进度分两段：明细占 0–90（它是大头），聚合占 90–100。
    pub fn relocate(
        &self,
        new_dir: &Path,
        progress: impl Fn(u64, u64) + Send + Sync,
    ) -> Result<(), String> {
        let old_dir = self.directory_of();
        let new_dir = new_dir.to_path_buf();
        if old_dir == new_dir {
            return Err("新目录与当前保存位置相同".to_string());
        }
        let old_request_file = old_dir.join(FILE_NAME);
        let old_daily_file = old_dir.join(DAILY_FILE_NAME);
        std::fs::create_dir_all(&new_dir).map_err(|error| format!("创建目录失败: {error}"))?;

        let Ok(mut guard) = self.inner.lock() else {
            return Err("统计库正被占用，请稍后重试".to_string());
        };
        // 明细：按内存整份写出（顺带把文件收敛到位）
        let text = render_jsonl(&guard.entries);
        progress(0, text.len() as u64);
        let new_request_file = new_dir.join(FILE_NAME);
        crate::server::logs_store::write_chunked(&new_request_file, text.as_bytes(), &progress)?;
        // 聚合通常很小，单独一段进度（映射到 90–100）
        let daily_progress = |written: u64, total: u64| {
            progress(90 + written * 10 / total.max(1), 100);
        };
        let snapshot: Vec<DailyEntry> = guard.daily.values().cloned().collect();
        let daily_text = render_jsonl(&snapshot);
        let new_daily_file = new_dir.join(DAILY_FILE_NAME);
        crate::server::logs_store::write_chunked(&new_daily_file, daily_text.as_bytes(), &daily_progress)?;
        // 两个文件都成功才切目录并收敛写盘计数
        if let Ok(mut directory) = self.directory.write() {
            *directory = new_dir.clone();
        }
        guard.dirty = false;
        guard.appends_since_compact = 0;
        guard.daily_changes = 0;
        guard.daily_flushed_at = Some(Instant::now());
        drop(guard);
        // 旧文件删掉（搬家不留歧义）；删失败不回滚 —— 数据已在新位置
        let _ = std::fs::remove_file(&old_request_file);
        let _ = std::fs::remove_file(&old_daily_file);
        Ok(())
    }

    /// 立即按当前保留期裁剪（供「改小保留天数后立即清理」用）。
    ///
    /// 裁剪会改写内存里的数据，所以顺手把文件也收敛掉 ——
    /// 否则文件里仍留着已被裁掉的旧条目，重开程序它们又回来了。
    /// （`record` 里也会顺手裁，但那只是「不涨过头」；这里是用户显式要求的清理，
    /// 所以要立刻落盘，而不是等下次攒批或延迟窗口。）
    pub fn prune(&self) {
        let bounds = self.retention_bounds();
        let mut guard = self.lock();

        let entries_changed = trim_entries(&mut guard.entries, bounds.requests_ms)
            || trim_entries_to_limit(&mut guard.entries);
        let days_changed = trim_daily(&mut guard.daily, &bounds.daily_key);

        // 有裁剪就立刻落盘：调用方（改小设置后点清理）期待的是「盘上也干净了」
        if entries_changed {
            let text = render_jsonl(&guard.entries);
            self.write_all(&self.request_file(), &text);
            guard.dirty = false;
            guard.appends_since_compact = 0;
        }
        if days_changed {
            let snapshot: Vec<DailyEntry> = guard.daily.values().cloned().collect();
            let text = render_jsonl(&snapshot);
            self.write_all(&self.daily_file(), &text);
            guard.daily_changes = 0;
            guard.daily_flushed_at = Some(Instant::now());
        }
    }

    /// 清空明细与按天聚合，返回清空后的存储概况（供「清空统计数据」按钮）。
    ///
    /// 为什么不是「用 `prune` 裁到 0 天」：保留期的下限是 1 天（`normalized()`
    /// 把 0 夹成 1），因此 `prune` 永远留得住今天的数据，表达不了「清空」。
    /// 这里直接清内存 + 覆盖写两个文件，语义明确：清空后立即读到的就是 0 条，
    /// 重开程序也不会把已删的数据载回来（文件同样被清掉，不是只清内存）。
    ///
    /// 持锁写完再放锁（与 `record` / `prune` 相同，而**不是**照 `LogStore::clear`
    /// 的「先放锁再写文件」）：本模块所有写路径都在持锁期间落盘，是因为一旦在锁外
    /// 覆盖写，并发 `record` 刚追加的那一行会被这次 `write_all("")` 吃掉
    /// （内存里留着、文件里没有，重开程序就少一条）。清空是低频的用户动作，
    /// 持锁写盘的开销可以接受。
    /// **不改保留期设置**：清数据与改配置是两件事。
    pub fn clear(&self) -> Value {
        let mut guard = self.lock();
        guard.entries.clear();
        // dirty 归零：内存与文件马上都由这次调用收敛成空，不存在「文件比内存多」
        guard.dirty = false;
        guard.appends_since_compact = 0;
        guard.daily.clear();
        guard.daily_changes = 0;
        guard.daily_flushed_at = Some(Instant::now());
        // 明细与聚合都写空串（与 render_jsonl 对空列表的产出一致），
        // 于是「空」在文件层面就是「零字节」这一个形态，没有第二种表示
        self.write_all(&self.request_file(), "");
        self.write_all(&self.daily_file(), "");
        drop(guard);
        self.stats()
    }

    /// 存储概况（排障与报表页脚用）
    pub fn stats(&self) -> Value {
        let retention = self.retention();
        let guard = self.lock();
        json!({
            "total": guard.entries.len(),
            "dailyDays": guard.daily.len(),
            "maxEntries": MAX_ENTRIES,
            "maxDailyDays": MAX_DAILY_DAYS,
            "file": self.request_file().to_string_lossy(),
            "dailyFile": self.daily_file().to_string_lossy(),
            "firstTs": guard.entries.first().map(|item| item.ts),
            "lastTs": guard.entries.last().map(|item| item.ts),
            "today": date_key(today()),
            "retention": {
                "requestDays": retention.request_days,
                "dailyDays": retention.daily_days,
            },
        })
    }

    /// 退出时补写：延迟落盘策略下内存里可能有未写盘的内容。
    ///
    /// 明细也要看一眼 —— 追加写路径每次都已落盘，但「攒够一批」那条路径
    /// 会先改内存、把整文件重写推到下一次；退出时把 dirty 收掉，
    /// 免得下次启动从旧文件载入时少掉刚记的那些条目。
    pub fn flush(&self) {
        let mut guard = self.lock();
        if guard.dirty {
            let text = render_jsonl(&guard.entries);
            self.write_all(&self.request_file(), &text);
            guard.dirty = false;
            guard.appends_since_compact = 0;
        }
        if guard.daily_changes > 0 {
            let snapshot: Vec<DailyEntry> = guard.daily.values().cloned().collect();
            let text = render_jsonl(&snapshot);
            self.write_all(&self.daily_file(), &text);
            guard.daily_changes = 0;
            guard.daily_flushed_at = Some(Instant::now());
        }
    }

    /// 整文件覆盖写
    fn write_all(&self, path: &Path, text: &str) {
        if let Err(error) = std::fs::create_dir_all(&self.directory_of()) {
            eprintln!("[Stats] 统计写入失败: {error}");
            return;
        }
        if let Err(error) = std::fs::write(path, text) {
            eprintln!("[Stats] 统计写入失败: {error}");
        }
    }

    /// 追加一行（正常路径）
    fn write_append(&self, path: &Path, line: &str) {
        use std::io::Write;
        if let Err(error) = std::fs::create_dir_all(&self.directory_of()) {
            eprintln!("[Stats] 统计写入失败: {error}");
            return;
        }
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()));
        if let Err(error) = result {
            eprintln!("[Stats] 统计写入失败: {error}");
        }
    }
}

// ─── 载入与落盘辅助 ─────────────────────────────────────────

/// 逐行解析明细；文件不存在或单行损坏都跳过，不打扰用户
/// （载入期的一条坏行不该让整份历史都读不进来）
fn load_requests(path: &Path) -> Vec<RequestEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(item) = serde_json::from_str::<RequestEntry>(trimmed) {
            out.push(item);
        }
    }
    out
}

/// 逐行解析聚合。**不拒绝未知字段**：后续给聚合行加字段时旧文件仍要能读。
///
/// 同一天出现多行（手工合并文件、异常退出留下的重复行）时按行合并，
/// 而不是「后者覆盖前者」—— 覆盖会静默吞掉前一份数据。
fn load_daily(path: &Path) -> BTreeMap<String, DailyEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut out: BTreeMap<String, DailyEntry> = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(item) = serde_json::from_str::<DailyEntry>(trimmed) else {
            continue;
        };
        // 日期为空的行走不了区间查询（也进不了 BTreeMap 的语义），丢掉
        if item.date.is_empty() {
            continue;
        }
        match out.get_mut(&item.date) {
            Some(existing) => {
                existing.requests += item.requests;
                existing.successful += item.successful;
                existing.tokens += item.tokens;
                existing.cache_hit_tokens += item.cache_hit_tokens;
                existing.cache_input_tokens += item.cache_input_tokens;
                for acc in item.model_tokens {
                    push_model_accum(&mut existing.model_tokens, &acc.model, acc.tokens, acc.requests);
                }
                // provider 维度同理逐行合并（同一天多行时两组都要合上，
                // 否则「按 provider 之和 = requests」这条对账关系会破）
                for acc in item.provider_stats {
                    push_provider_accum(
                        &mut existing.provider_stats,
                        &acc.provider,
                        acc.requests,
                        acc.successful,
                        acc.tokens,
                    );
                }
                // 账号维度同理（理由与 provider 那条一致：合并要不全做、要不全不做）
                for acc in item.account_stats {
                    push_account_accum(
                        &mut existing.account_stats,
                        &acc.account_id,
                        &acc.account_name,
                        acc.requests,
                        acc.successful,
                        acc.tokens,
                    );
                }
            }
            None => {
                out.insert(item.date.clone(), item);
            }
        }
    }
    out
}

/// 二分插入明细，维持 **ts 升序**不变式。
///
/// 为什么不能只 `push`：并发下先发起的长请求可能晚收尾 —— `record` 的调用
/// 顺序与请求发起顺序并不一致，push 会让 ts 出现逆序。而「新在前」的分页是
/// 按数组顺序反向取段的，一旦数组不有序，翻页就会漏条目、重复条目。
///
/// 插入点取**第一个 ts 大于新条目 ts** 的位置（即上界），于是同毫秒的条目
/// 保持「先到的在前」，与追加写的文件顺序也一致。
fn insert_sorted(entries: &mut Vec<RequestEntry>, record: RequestEntry) {
    let index = entries.partition_point(|item| item.ts <= record.ts);
    entries.insert(index, record);
    // 超出上限从**最旧**端裁（升序数组的头部）
    if entries.len() > MAX_ENTRIES {
        let overflow = entries.len() - MAX_ENTRIES;
        entries.drain(0..overflow);
    }
}

/// 这条明细是否命中查询条件。`query_requests` 与 `clear_where` 共用一份 ——
/// 「页面上筛出来的 N 条」与「清空删掉的那批」必须是同一个集合。
/// `offset` / `limit` 是分页参数，不是筛选条件，不在这里。
fn matches_filter(item: &RequestEntry, filter: &RequestQuery) -> bool {
    // 模型名精确匹配；空串（输入框清空）当没筛
    if let Some(want) = filter.model.as_deref().filter(|text| !text.is_empty()) {
        if item.model != want {
            return false;
        }
    }
    // 只认 ok / error，其余值在存储层忽略（与 query_requests 原口径一致）
    if let Some(want_ok) = normalize_status_filter(filter.status.as_deref()) {
        if item.is_success() != want_ok {
            return false;
        }
    }
    if let Some(from) = filter.start {
        if item.ts < from {
            return false;
        }
    }
    if let Some(to) = filter.end {
        // end 是**开**区间：与分页口径一致
        if item.ts >= to {
            return false;
        }
    }
    true
}

/// 把一条明细累加进当天的聚合行。
///
/// `record` 的记账与 `clear_where` 的重算共用这一段：聚合行的每个字段都
/// **只**由明细逐条累加而来，两条路径手写两遍必然漂移（对不上账）。
/// 三个维度（模型 / provider / 账号）与总量并列累计，各维求和都等于当天总量，
/// 这是报表之间能对账的前提。
fn fold_into_daily(day: &mut DailyEntry, item: &RequestEntry) {
    let success = item.is_success();
    day.requests += 1;
    if success {
        day.successful += 1;
    }
    day.tokens += item.total_tokens;
    day.cache_hit_tokens += item.cache_read_tokens;
    day.cache_input_tokens += item.prompt_tokens;
    push_model_accum(&mut day.model_tokens, &item.model, item.total_tokens, 1);
    // 空 provider 也建组（见 push_provider_accum 的注释）
    push_provider_accum(
        &mut day.provider_stats,
        &item.provider,
        1,
        i64::from(success),
        item.total_tokens,
    );
    push_account_accum(
        &mut day.account_stats,
        &item.account_id,
        &item.account_name,
        1,
        i64::from(success),
        item.total_tokens,
    );
}

/// 两个文件的保留边界（由 `retention_bounds` 每次动态算出）
struct RetentionBounds {
    /// 明细的毫秒下界（本地日期零点）
    requests_ms: i64,
    /// 聚合的日期键下界（`YYYY-MM-DD`）
    daily_key: String,
}

/// 按毫秒下界裁掉过期的明细，返回是否真的裁掉了东西。
///
/// 明细恒按 ts 升序，所以用 `partition_point` 一次定位（O(log n)）——
/// 这比逐条 `retain` 判日期（每条都格式化一次字符串）便宜得多，
/// 而它会在**每次记账**时执行，必须足够廉价。
/// 返回 `bool` 而不是条数：调用方只关心「有没有变」，不关心数量。
fn trim_entries(entries: &mut Vec<RequestEntry>, cutoff_ms: i64) -> bool {
    let expired = entries.partition_point(|item| item.ts < cutoff_ms);
    if expired == 0 {
        return false;
    }
    entries.drain(0..expired);
    true
}

/// 按环形上限裁掉最旧的明细（升序数组的头部），返回是否有裁剪
fn trim_entries_to_limit(entries: &mut Vec<RequestEntry>) -> bool {
    if entries.len() <= MAX_ENTRIES {
        return false;
    }
    let overflow = entries.len() - MAX_ENTRIES;
    entries.drain(0..overflow);
    true
}

/// 按日期键下界裁掉过期的聚合，返回是否真的裁掉了东西。
///
/// `BTreeMap` 按键升序，`split_off` 一次切掉此前所有日子 ——
/// 不需要逐条判断，也不必自己找「最早的那天」。
/// 顺带把兜底上限（`MAX_DAILY_DAYS`）一并收掉。
fn trim_daily(daily: &mut BTreeMap<String, DailyEntry>, cutoff_key: &str) -> bool {
    let mut changed = false;
    if daily.keys().next().is_some_and(|key| key.as_str() < cutoff_key) {
        *daily = daily.split_off(cutoff_key);
        changed = true;
    }
    while daily.len() > MAX_DAILY_DAYS {
        let Some(oldest) = daily.keys().next().cloned() else {
            break;
        };
        daily.remove(&oldest);
        changed = true;
    }
    changed
}

/// 聚合是否该落盘。
///
/// 取舍：一年最多 365 行，重写整个文件的成本可以忽略（几十 KB）；
/// 延迟纯粹是为了**避免高频请求下反复写盘** —— 每来一个请求就重写一次
/// 聚合文件，等于把 SSD 当草稿纸用。所以攒够 N 天次变更或超过 M 秒才写。
/// 代价是进程被强杀时最多丢一个延迟窗口的聚合（明细不受影响，
/// 它走追加写），这也是退出路径必须调 `flush()` 的原因。
fn should_flush_daily(inner: &Inner) -> bool {
    if inner.daily_changes == 0 {
        return false;
    }
    if inner.daily_changes >= DAILY_FLUSH_CHANGES {
        return true;
    }
    match inner.daily_flushed_at {
        Some(at) => at.elapsed() >= DAILY_FLUSH_INTERVAL,
        None => true,
    }
}

/// 把条目渲染成 JSONL 文本。空列表返回空串（与 logs_store 的 render_jsonl 同）。
/// 序列化失败的条目跳过：这里已是错误处理路径，不能让一条脏数据毁掉整份文件。
fn render_jsonl<T: Serialize>(entries: &[T]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut text = String::new();
    for item in entries {
        if let Ok(line) = serde_json::to_string(item) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}
