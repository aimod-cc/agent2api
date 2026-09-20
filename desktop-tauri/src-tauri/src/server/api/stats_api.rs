//! 报表 API 与数据保留策略（设置页「数据保留」）。
//!
//! ```text
//! GET    /api/stats/summary     报表聚合（概览 / 热力图 / 缓存命中率 / 趋势）
//! GET    /api/stats/requests    请求日志（分页 + 模型 / 状态 / 时间区间过滤）
//! DELETE /api/stats/requests    清空明细与按天聚合
//! GET    /api/retention         三档保留天数（事件日志 / 请求日志 / 按天聚合）
//! PUT    /api/retention         更新保留天数并**立即**触发清理
//! ```
//!
//! ── 为什么两条前缀放在一个文件里 ─────────────────────────────
//! `/api/retention` 是这三条数据保留期的**统一入口**（事件日志在 `logs_store`、
//! 请求日志与聚合在 `request_stats`），其中两条属于请求统计；而改完设置必须
//! 立刻裁剪，裁剪又要拿 `request_stats` 句柄 —— 两者共享同一份校验与清理逻辑，
//! 拆成两个文件只会让「校验 → 写配置 → 立即清理」这条链断成两段。
//! 文件名沿用 `logs_api.rs` / `config_api.rs` 的 `*_api.rs` 约定。
//!
//! ── 参数解析的取舍（与 logs_api 同一口径）────────────────────
//!   - `range` 非法 → **400**（与存储层「非法按 7 处理」不同）：报表区间是用户
//!     在界面上显式选的值，静默换一个区间会让页面显示的数据与选项对不上，
//!     报错比默默改口径更容易排查。此处的合法值清单与 `report::normalize_range`
//!     保持一致（那边是存储层的归一实现，这里是路由层的校验白名单）。
//!   - `start` / `end` / `limit` / `offset` 非法 → **忽略**（存储层各自兜底）：
//!     这些是筛选器，前端可能传空串（「不限时间」）；为一次筛选参数不合法
//!     让整页报错，不如把该维度当作没筛。
//!   - `status` 只接受 `ok` / `error`，其余忽略 —— 归一逻辑在存储层
//!     （`report::normalize_status_filter`），这里只做透传，不重复实现一套。
//!
//! ── 与既有 API 的关系 ───────────────────────────────────────
//! 全部是**新增**端点与新增可选查询参数：`/api/logs` 只多认 `start` / `end`
//! 两个可选参数，不传时行为与以前完全一致（见 `logs_api::query_logs`）。
//!
//! ── provider / account 维度（Agent2API W4：T-e1）────────────
//! 两条报表路由的响应各多一处 **provider 维度**，都由存储层组装完再透传
//! （路由层不加工，理由同上）：
//!   - `/api/stats/requests`：每行含 `provider`（id，旧行/未承载为空串）
//!     与派生的 `providerLabel`（id → 中文展示名；未知 id 原样回显 id，
//!     空 id 回显空串，由展示层用「—」占位）。
//!   - `/api/stats/summary`：新增顶层 `providers` 数组
//!     `[{id,label,requests,success,failures,totalTokens}]`，按 requests 降序，
//!     统计区间与 `overview` / `topModel` 完全一致；空 id 的组 label 为「未知」。
//!
//! **account 维度**与 providers 同形并列（`accounts` 数组，同样的字段与排序），
//! 是「具体哪个登录态在出力」的那一维 —— 一家提供商可以挂多个账号，
//! 所以它比 providers 更细。两点差异：`label` 是**聚合时留下的名字快照**
//! 而不是现算（账号可能已被删除，注册表里查不到），取「快照名 → id」，
//! 两者都空的那一组 label 为「未知账号」。
//!
//! 以上都是**新增键**，既有字段的键名、类型、语义一个都没动
//! （前端按「providers / accounts 存在则展示、缺失则隐藏」消费）。

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config::{
    self, RetentionPatch, RetentionSettings, KEY_DAILY_RETENTION_DAYS, KEY_LOG_RETENTION_DAYS,
    KEY_REQUEST_RETENTION_DAYS, RETENTION_MAX_DAYS, RETENTION_MIN_DAYS,
};
use crate::server::errors;
use crate::server::http::{ok_json, parse_body, parse_query_ms};
use crate::server::logging;
use crate::server::request_stats::RequestQuery;
use crate::server::ServerState;

/// `/api/stats/summary` 的合法 range 取值（与 `request_stats::report` 的
/// `normalize_range` 白名单逐字一致；那边是存储层的归一，这里是路由层的校验）
const RANGES: [&str; 5] = ["today", "7", "30", "month", "all"];
/// range 缺省值：与存储层「非法/缺省按 7 处理」的默认口径相同
const DEFAULT_RANGE: &str = "7";

/// `/api/stats*` 上未匹配子路径的 404。
///
/// 照 `logs_api::not_found` 的做法：管理 API 的 404 是信封形状
/// （`{success:false,error:"Not found: <METHOD> <path>"}`），
/// 与全局兜底的 OpenAI 形状（`{error:{message}}`）不同。
pub async fn not_found(request: axum::extract::Request) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    errors::management_error(404, format!("Not found: {method} {path}"))
}

/// 查询串（与 logs_api 同一写法：HashMap 比给每个参数定义结构体更贴合
/// 「有则用、无则忽略」的语义）
type Params = HashMap<String, String>;

/// 取「有效」的文本参数：空串与全空白一律当缺省，返回值已 trim。
///
/// 前端把输入框清空时会发出 `?model=&start=`，这类值若交给下游过滤
/// 会变成「模型名等于空串」这种永不命中的条件（页面看起来是空的）。
/// 顺带 trim：模型名不会有前后空白，容忍用户从别处复制粘贴带进来的空白
/// 比严格相等更符合「筛选」的语义（存储层做的是精确匹配）。
fn text_of(params: &Params, key: &str) -> Option<String> {
    params
        .get(key)
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 分页偏移：空串 / 非数字 / 负数 → 0（存储层的 `skip` 以 0 为起点）
fn parse_offset(value: Option<&String>) -> usize {
    let Some(text) = value.map(|text| text.trim()).filter(|text| !text.is_empty()) else {
        return 0;
    };
    text.parse::<usize>().unwrap_or(0)
}

/// 每页条数：空串 / 非数字 / 0 → None（用存储层默认值 50）。
///
/// 0 当「没传」而不是「要 0 条」（与 logs_api 的 `0 || 200` 同一口径）；
/// 超上限的夹紧在存储层（`query_requests` 里 `clamp(1, MAX_LIMIT)`，即 500），
/// 这里只负责把字符串解析成数字 —— 夹紧逻辑只有一处，两层各写一份会漂。
fn parse_limit(value: Option<&String>) -> Option<usize> {
    let text = value.map(|text| text.trim()).filter(|text| !text.is_empty())?;
    text.parse::<usize>().ok().filter(|size| *size > 0)
}

/// GET /api/stats/summary?range=7
pub async fn stats_summary(State(state): State<ServerState>, Query(params): Query<Params>) -> Response {
    // 先 trim 再比对：存储层的 `normalize_range` 也是先 trim 后匹配，
    // 若这里比对未 trim 的值，`?range=%207%20` 会被判 400 而存储层本可接受它 ——
    // 两层的容错度不一致时，用户看到的 400 会显得莫名其妙
    let requested = text_of(&params, "range").unwrap_or_else(|| DEFAULT_RANGE.to_string());
    // 校验放在路由层：非法值给 400 并列出合法值（见文件头的取舍说明）
    if !RANGES.contains(&requested.as_str()) {
        return errors::management_error(
            400,
            format!("range 取值非法: {requested}（合法值: {}）", RANGES.join("、")),
        );
    }
    // 原样透传存储层的报表结果（`{range, startDate, endDate, overview, providers,
    // heatmap, cacheRates, cacheTrend24h, dailyTrend}`），路由层不加工，避免两处口径。
    // `providers` 是 W4 新增的 provider 维度汇总（见文件头的说明）
    ok_json(state.request_stats().usage_summary(&requested))
}

/// GET /api/stats/requests?offset=&limit=&model=&status=&start=&end=
///
/// 每行含 `provider`（id）与 `providerLabel`（展示名），由存储层的 `entry_json`
/// 在序列化后派生填入；**不新增查询参数**（按 provider 筛选不在本期契约里，
/// 前端要筛就在本地按 id 过滤已有的 `provider` 字段）。
pub async fn stats_requests(State(state): State<ServerState>, Query(params): Query<Params>) -> Response {
    let filter = RequestQuery {
        offset: parse_offset(params.get("offset")),
        limit: parse_limit(params.get("limit")),
        // 模型名精确匹配；空串（输入框清空）当没筛
        model: text_of(&params, "model"),
        // 只认 ok / error，其余值在存储层被忽略（不在这里重复实现归一）
        status: text_of(&params, "status"),
        // 闭开区间 [start, end)：翻页时上一页末尾的 ts 可直接当下页的 end
        start: parse_query_ms(params.get("start")),
        end: parse_query_ms(params.get("end")),
    };
    ok_json(state.request_stats().query_requests(&filter))
}

/// DELETE /api/stats/requests?model=&status=&start=&end=&all=
///
/// 带筛选参数时**只删命中的明细**并重算受影响日期的聚合（「清空筛选结果」，
/// 与 GET 同一份过滤链）；清空全部必须**显式**带 `all=1`（不带的空请求给 400）——
/// 与 `logs_api::clear_logs` 同一道护栏（理由见那边的注释：筛选清空漏传参数
/// 的代价是全部明细没了）。响应带删除后的存储概况，有删除时另带 `removed`。
/// 清空**不动保留期设置**。
pub async fn clear_stats_requests(
    State(state): State<ServerState>,
    Query(params): Query<Params>,
) -> Response {
    let filter = RequestQuery {
        offset: 0,
        limit: None,
        model: text_of(&params, "model"),
        status: text_of(&params, "status"),
        start: parse_query_ms(params.get("start")),
        end: parse_query_ms(params.get("end")),
    };
    let has_filters =
        filter.model.is_some() || filter.status.is_some() || filter.start.is_some() || filter.end.is_some();
    if !has_filters {
        let explicit_all = params.get("all").map_or(false, |value| {
            let text = value.trim();
            text == "1" || text.eq_ignore_ascii_case("true")
        });
        if !explicit_all {
            return errors::management_error(
                400,
                "未指定筛选条件：要清空全部请求日志请显式带 all=1",
            );
        }
    }
    let stats = if has_filters {
        state.request_stats().clear_where(&filter)
    } else {
        state.request_stats().clear()
    };
    // 全量清空时连调试模式的原始报文一起清：那些报文是按 id 关联到明细的，
    // 明细没了它们就成了永远取不到的孤儿，白占磁盘。
    // **按筛选条件清空时不动**：筛选清的是部分明细，报文可能还对应着留下的行，
    // 且筛选语义（模型 / 状态 / 时间）在报文侧没有对应字段可判。
    if !has_filters {
        crate::server::core::debug_traffic::clear();
    }
    let removed = stats.get("removed").and_then(Value::as_u64);
    match removed {
        Some(count) => {
            logging::log("[Stats]", &format!("已按筛选条件清空 {count} 条请求明细（受影响日期的聚合已重算）"));
        }
        None => {
            logging::log("[Stats]", "请求统计已清空（明细 + 按天聚合）");
        }
    }
    ok_json(stats)
}

/// GET /api/retention
pub async fn get_retention(State(_state): State<ServerState>) -> Response {
    ok_json(retention_json(config::retention_settings()))
}

/// PUT /api/retention —— body `{logRetentionDays?, requestRetentionDays?, dailyRetentionDays?}`
///
/// 允许部分字段（未出现的项保持原值）。校验通过后：写 config.json → 立即裁剪
/// → 返回**生效后**的值（前端直接用响应刷新界面，不必再 GET 一次）。
///
/// 立即裁剪的必要性：保留期只在「记账 / 写日志 / 启动载入」时顺带生效，
/// 用户把 30 天改成 7 天之后如果不动，旧数据要等到下一条请求或下次重启才消失 ——
/// 那时的界面会显示「设置是 7 天，日志里还有 30 天前的条目」，看起来像没生效。
pub async fn put_retention(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };

    // ── 校验：全部先过一遍再写盘 ──────────────────────────────
    // 为什么先收集再写：三项里有一项非法时整体不落盘，避免出现
    // 「日志天数改了、明细天数没改」的半套设置（用户以为保存失败，实际生效了一半）
    let mut patch = RetentionPatch::default();
    let mut targets: [(&str, Option<&Value>, &mut Option<i64>); 3] = [
        (KEY_LOG_RETENTION_DAYS, object.get(KEY_LOG_RETENTION_DAYS), &mut patch.log_days),
        (
            KEY_REQUEST_RETENTION_DAYS,
            object.get(KEY_REQUEST_RETENTION_DAYS),
            &mut patch.request_days,
        ),
        (
            KEY_DAILY_RETENTION_DAYS,
            object.get(KEY_DAILY_RETENTION_DAYS),
            &mut patch.daily_days,
        ),
    ];
    for (key, value, slot) in targets.iter_mut() {
        let Some(value) = value else {
            continue;
        };
        // null 视为「这一项不改」：前端把整份设置对象回传时未填项常见为 null，
        // 为它报 400 会逼前端做无意义的清理
        if value.is_null() {
            continue;
        }
        match parse_days(key, value) {
            Ok(days) => **slot = Some(days),
            Err(message) => return errors::management_error(400, message),
        }
    }

    // 三项都没给：不写盘（空 PUT 不该在 config.json 里留下痕迹），
    // 也不记「已更新」日志（什么都没改，日志里不该出现一次伪更新），但仍返回当前值 ——
    // 空 PUT 的语义就是「读一次当前设置」，调用方不用为此换成 GET
    let updated =
        patch.log_days.is_some() || patch.request_days.is_some() || patch.daily_days.is_some();
    if updated {
        if !config::set_retention(patch) {
            // 写盘失败：内存快照已更新（本次运行仍生效），但重启后会回到旧值 ——
            // 必须让用户知道，否则「改了设置重启又变回去」会被当成玄学问题
            logging::log("[Config]", "⚠️  保留期写入 config.json 失败，本次运行内仍生效");
        }
        // ── 立即清理（两种数据各自的时机不同，这里一次触发）──
        // 请求统计：prune() 内部**动态取**保留期回调，所以取到的就是刚写入的值
        state.request_stats().prune();
        // 事件日志：LogStore 的保留天数同样走回调（见 `LogStore::new` 的注释），
        // prune 返回裁掉的条数，打一行控制台便于确认「改小天数确实删了东西」
        if let Some(store) = logging::store_ref() {
            let removed = store.prune();
            if removed > 0 {
                logging::console_line("[Logs]", &format!("按保留期清理了 {removed} 条过期日志"));
            }
        }
    }

    let settings = config::retention_settings();
    if updated {
        logging::log(
            "[Config]",
            &format!(
                "数据保留期已更新: 事件日志 {} 天 / 请求日志 {} 天 / 按天聚合 {} 天",
                settings.log_days, settings.request_days, settings.daily_days
            ),
        );
    }
    ok_json(retention_json(settings))
}

/// 三档保留天数的响应体（GET 与 PUT 共用，保证两边键名完全一致）。
///
/// 键用**常量标识符**而不是手写字符串：`json!` 会把标识符当表达式求值再
/// `.into()` 成键名，所以这里落到 JSON 里的就是常量的值（`"logRetentionDays"` 等）。
/// 好处是键名只在 `config.rs` 定义一次，读侧（`days_field`）、写侧
/// （`set_retention`）与响应体三处必然一致 —— 手写三遍字符串，拼错一处
/// 不会报错，只会静默地少一个字段。
fn retention_json(settings: RetentionSettings) -> Value {
    json!({
        KEY_LOG_RETENTION_DAYS: settings.log_days,
        KEY_REQUEST_RETENTION_DAYS: settings.request_days,
        KEY_DAILY_RETENTION_DAYS: settings.daily_days,
    })
}

/// 单个天数字段的校验：必须是 1–3650 的整数，否则给出可读的 400 文案。
///
/// 只认 JSON 数字（含 `30.0` 这种整数值的浮点，容忍 JS 的 number 形态）；
/// 字符串 `"30"` 视为非法 —— 那是调用方把配置当字符串传，静默接受会让
/// 「前端传错类型」这类 bug 一直藏着。
fn parse_days(key: &str, value: &Value) -> Result<i64, String> {
    let number = match value {
        Value::Number(number) => number,
        other => {
            return Err(format!(
                "{key} 必须是 {RETENTION_MIN_DAYS}-{RETENTION_MAX_DAYS} 的整数（收到: {other}）"
            ))
        }
    };
    let days = number.as_i64().or_else(|| {
        number
            .as_f64()
            .filter(|raw| raw.is_finite() && raw.fract() == 0.0)
            .map(|raw| raw as i64)
    });
    let Some(days) = days else {
        return Err(format!(
            "{key} 必须是 {RETENTION_MIN_DAYS}-{RETENTION_MAX_DAYS} 的整数（收到: {number}）"
        ));
    };
    if !(RETENTION_MIN_DAYS..=RETENTION_MAX_DAYS).contains(&days) {
        return Err(format!(
            "{key} 超出范围: {days}（合法范围 {RETENTION_MIN_DAYS}-{RETENTION_MAX_DAYS} 天）"
        ));
    }
    Ok(days)
}
