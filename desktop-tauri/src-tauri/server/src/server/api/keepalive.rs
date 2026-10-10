//! 「丢弃保活换行分片」开关：`GET/PUT /api/keepalive-strip`。
//!
//! ── 这个开关做什么 ──────────────────────────────────────────
//! WorkBuddy 与 AutoClaw 的上游会在相邻正文分片之间
//! 插一帧 `delta.content` 只含换行的分片（同一批流里还在发 `: heartbeat` 注释行，
//! 那才是它的本意保活手段）。生产库两天 58 发 WorkBuddy 200 请求实测：20 发带
//! 这类分片（34.5%），脏请求里它们占正文分片中位 13.4%（区间 1.2%–30.8%），
//! 干净请求恒 0；分布是二值的 —— 一条请求要么零个，要么几乎每片之间都插一个。
//! 客户端按 markdown 渲染时一个换行就是一个 `<br>`，于是正文被排成一行一个词。
//! 开启后，转发层在**下发**前丢掉这类帧（判据与"宁可不丢"的取向见
//! [`crate::server::core::upstream::sse::is_newline_keepalive`]）。
//!
//! ── 为什么是「能力位 × 开关」两个条件 ───────────────────────
//! 适配器那位 `sse_strip_newline_chunks()` 回答的是「这家的上游会不会发这种东西」，
//! 依据只有实测过的两家；本开关回答的是「这台机器要不要为它改写下发字节」。
//! 两者相与，且**开关只能收窄不能扩张**：打开它不会让一家没实测过的上游开始被丢帧
//! （见 [`crate::server::core::upstream::sse::FramePolicy::assembled`]）。
//!
//! ── 为什么默认关、又为什么要能在线切 ───────────────────────
//! 丢帧的代价是"模型真有一个单独成片的换行会被一起吃掉"，收益是"整屏断句没了" ——
//! 这是一次外观取舍，而且只在带这类节拍的那一类请求上看得见（两天样本里 34.5%）。
//! 所以编译期不替用户定：
//! 默认逐字节透传，开关在配置里（`stripNewlineKeepalive`，未设置时环境变量
//! `AGENT2API_STRIP_NEWLINE_KEEPALIVE` 兜底）。留成在线端点是为了**能在真流量上
//! 验证并随时撤回**：开一下、看几条请求的下游字节、不对就关掉，不需要重新构建镜像。
//! 转发层逐请求读快照，改完下一个请求立即生效，不重启进程。
//!
//! ── 与 /api/sanitize、/api/debug 的关系 ─────────────────────
//! 形状刻意保持一致（GET 读 / PUT 写 / 响应体就是新状态），三者的差别只在默认值
//! 与代价方向：脱敏默认开（不开会撞 400）、调试模式默认关（报文可达数百 KB）、
//! 本开关默认关（它改的是下发字节）。设置页的开关因此能共用一套读写模式。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::errors;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// GET /api/keepalive-strip —— 当前开关状态
pub async fn get_keepalive_strip(State(_state): State<ServerState>) -> Response {
    ok_json(state_json())
}

/// PUT /api/keepalive-strip —— body `{stripNewlineKeepalive: true|false}`
///
/// 写配置，下一个请求立即生效（策略装配逐请求读快照，不重启进程）。
/// 开与关都记一条事件日志：开的那一刻起，那两家的下发帧不再是上游原样的字节，
/// 而关的那一刻起又变回去 —— 排查"这个换行怎么没了"时，日志里必须查得到这条线。
pub async fn put_keepalive_strip(State(_state): State<ServerState>, body: Bytes) -> Response {
    let payload = match parse_body(&body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(object) = payload.as_object() else {
        return errors::management_error(400, "请求体必须是 JSON 对象");
    };
    let key = config::KEY_STRIP_NEWLINE_KEEPALIVE;
    let Some(value) = object.get(key) else {
        return errors::management_error(400, format!("缺少 {key}"));
    };
    let Some(enabled) = value.as_bool() else {
        return errors::management_error(400, format!("{key} 必须是 true 或 false"));
    };
    if !config::set_strip_newline_keepalive(enabled) {
        logging::log("[Config]", "⚠️  保活换行开关写入配置失败，本次运行内仍生效");
    }
    logging::log(
        "[Config]",
        if enabled {
            "保活换行丢弃已开启：WorkBuddy / AutoClaw 的下发帧将丢掉「整片只有换行」的 content 分片"
        } else {
            "保活换行丢弃已关闭：下发帧恢复为上游原样字节（保活换行会一起透传给客户端）"
        },
    );
    ok_json(state_json())
}

/// 开关状态（GET 与 PUT 共用，前端直接用响应刷新界面）
fn state_json() -> Value {
    json!({
        config::KEY_STRIP_NEWLINE_KEEPALIVE: config::strip_newline_keepalive(),
    })
}
