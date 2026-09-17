//! 敏感词脱敏 — 词表维护路由（对照 src/workbuddy-desensitize-routes.mjs 逐条实现）。
//!
//!   GET    /api/desensitize              当前状态（开关 / 词表 / 角色 / 命中统计）
//!   POST   /api/desensitize/enabled      开关 { enabled }
//!   POST   /api/desensitize/roles        作用角色 { roles: ['system','user'] }
//!   PUT    /api/desensitize/terms        全量替换词表 { terms: [...] }
//!   POST   /api/desensitize/terms        追加词 { terms: [...] | term: '...' }
//!   DELETE /api/desensitize/terms        删除词 { terms: [...] } 或 ?term=xxx
//!   POST   /api/desensitize/reset        恢复默认词表
//!   POST   /api/desensitize/stats/reset  清空命中统计
//!
//! ── 分发方式：与 Node 版同构的「一个入口 + 按剩余段判定」────────
//! Node 是 `tryHandle(req,res,path,url)`：前缀命中后按 `action` 逐条判方法 + 路径。
//! 这里刻意不做成 axum 的八条独立路由：那样每条都要重复一遍鉴权与 404 文案，
//! 且 `/api/desensitize/` 这种尾斜杠形态在 axum 里会落到全局兜底（OpenAI 形状的
//! 404），而 Node 走的是本模块自己的管理 API 形状 404。一个 `{*rest}` 通配入口
//! 能让判定顺序与错误文案逐条对齐。
//!
//! ── 全部需鉴权 ──────────────────────────────────────────────
//! Node 版每条都调了 checkApiKey，因此挂在 `http::router` 的 protected 组。
//!
//! ── 与 Node 版的一处结构性差异：没有 503 分支 ────────────────
//! Node 里 `desensitizer` 可能是 undefined（模块未装配），那种情况整段回
//! 503「脱敏模块未启用」。Rust 侧的脱敏器是进程级常驻句柄（见
//! `core::desensitize::global()`），构造上不可能是「未启用」——词表为空也
//! 只是一个空词表，请求照常处理。要复刻 503 就得人为造一个「未装配」状态，
//! 那是虚假分支；真需要停用脱敏应当用开关（enabled:false），语义还更准确。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::Method;
use axum::response::Response;
use serde_json::Value;

use crate::server::core::desensitize::{
    utf16_len, MAX_TERM_LENGTH, MAX_TERMS,
};
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::logging;
use crate::server::ServerState;

/// 请求体不是有效 JSON 时的文案（逐字照抄 Node 的 readJson）
const INVALID_JSON: &str = "请求体不是有效 JSON";

// ─── axum 入口 ──────────────────────────────────────────────

/// `/api/desensitize` 与 `/api/desensitize/{*rest}` 的共用入口（接受任意方法）。
pub async fn entry(State(state): State<ServerState>, request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let full_path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let body = match axum::body::to_bytes(request.into_body(), crate::server::http::MAX_BODY_SIZE)
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => return management_error(413, format!("请求体读取失败或过大: {error}")),
    };
    // action = 去掉 `/api/desensitize` 前缀后的剩余段（再剥掉前导斜杠），
    // 与 Node 的 `path.slice('/api/desensitize'.length).replace(/^\/+/, '')` 一致：
    // `/api/desensitize/` → ""（尾斜杠被剥掉后为空，等同状态查询）
    let suffix = full_path.strip_prefix("/api/desensitize").unwrap_or("");
    let action = suffix.trim_start_matches('/').to_string();
    dispatch(&state, method, &action, &full_path, &query, &body).await
}

/// 路径分发（判定顺序照抄 Node 的 tryHandle）。
async fn dispatch(
    state: &ServerState,
    method: Method,
    action: &str,
    full_path: &str,
    query: &str,
    body: &Bytes,
) -> Response {
    let desensitizer = state.desensitize();

    // 当前状态
    if method == Method::GET && action.is_empty() {
        return ok_json(desensitizer.state());
    }

    // 开关
    if method == Method::POST && action == "enabled" {
        let payload = match read_json(body) {
            Ok(payload) => payload,
            Err(response) => return response,
        };
        let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
            return management_error(400, "缺少 enabled（需要布尔值）");
        };
        let next = desensitizer.set_enabled(enabled, true);
        logging::log(
            "[Desensitize]",
            &format!("内容脱敏已{}", if enabled { "开启" } else { "关闭" }),
        );
        return ok_json(next);
    }

    // 作用角色
    if method == Method::POST && action == "roles" {
        let payload = match read_json(body) {
            Ok(payload) => payload,
            Err(response) => return response,
        };
        // 注意传的是 `body?.roles`：roles 缺失 → undefined → normalizeRoles 走默认角色
        let next = desensitizer.set_roles(payload.get("roles"));
        let roles = next
            .get("roles")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、")
            })
            .unwrap_or_default();
        logging::log("[Desensitize]", &format!("作用角色: {roles}"));
        return ok_json(next);
    }

    // 词表：全量替换 / 追加
    if (method == Method::PUT || method == Method::POST) && action == "terms" {
        let payload = match read_json(body) {
            Ok(payload) => payload,
            Err(response) => return response,
        };
        let terms = match validate_terms(extract_terms(&payload, query)) {
            Ok(terms) => terms,
            Err(response) => return response,
        };
        let before = desensitizer.term_count();
        let next = if method == Method::PUT {
            desensitizer.set_terms(&terms)
        } else {
            desensitizer.add_terms(&terms)
        };
        let after = next
            .get("termCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        logging::log(
            "[Desensitize]",
            &format!(
                "词表{}: {before} → {after} 个词",
                if method == Method::PUT { "更新" } else { "追加" }
            ),
        );
        return ok_json(next);
    }

    // 删词
    if method == Method::DELETE && action == "terms" {
        // Node 这里对非法 JSON 是 `.catch(() => ({}))` —— 吞掉解析错误继续走校验
        let payload = read_json(body).unwrap_or(Value::Object(serde_json::Map::new()));
        let terms = match validate_terms(extract_terms(&payload, query)) {
            Ok(terms) => terms,
            Err(response) => return response,
        };
        let next = desensitizer.remove_terms(&terms);
        let remaining = next
            .get("termCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        logging::log(
            "[Desensitize]",
            &format!("删除 {} 个词，剩余 {remaining} 个", terms.len()),
        );
        return ok_json(next);
    }

    // 恢复默认词表
    if method == Method::POST && action == "reset" {
        let next = desensitizer.reset_terms();
        let count = next.get("termCount").and_then(Value::as_u64).unwrap_or(0);
        logging::log("[Desensitize]", &format!("词表已恢复默认（{count} 个词）"));
        return ok_json(next);
    }

    // 清空命中统计
    if method == Method::POST && action == "stats/reset" {
        desensitizer.reset_stats();
        return ok_json(desensitizer.state());
    }

    // 已注册路径上的其它方法：Node 落到本模块自己的 404 信封
    management_error(404, format!("Not found: {} {full_path}", method.as_str()))
}

// ─── 请求解析与校验 ────────────────────────────────────────

/// 读 JSON 请求体。空 body 视为 `{}`（Node 的 `toString('utf8') || '{}'`），
/// 非法 JSON 给 400「请求体不是有效 JSON」（Node 在 readJson 里抛的就是这句）。
///
/// 判空只判**长度为 0**，不 trim：Node 的 `'' || '{}'` 里纯空白串是真值，
/// 会走进 `JSON.parse('   ')` 然后抛错 → 400。trim 后再判空会把
/// 「body 只写了几个空格」从 400 变成 200，那是契约分叉。
///
/// 不复用 `http::parse_body`：它的错误文案带 serde 的详细位置信息，与 Node 分叉。
fn read_json(body: &Bytes) -> Result<Value, Response> {
    let text = String::from_utf8_lossy(body);
    if text.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(&text).map_err(|_| management_error(400, INVALID_JSON))
}

/// 从请求体里取词：兼容 `{terms: [...]}`、`{term: '...'}`、裸数组与 `?term=xxx`。
///
/// 判定顺序照抄 Node 的 extractTerms：裸数组 → body.terms → body.term → query.term。
/// 返回 None 表示四处都没有（交给 validateTerms 报「缺少词表」）。
fn extract_terms(payload: &Value, query: &str) -> Option<Vec<String>> {
    if let Value::Array(items) = payload {
        return Some(string_items(items));
    }
    if let Some(Value::Array(items)) = payload.get("terms") {
        return Some(string_items(items));
    }
    if let Some(Value::String(text)) = payload.get("term") {
        return Some(vec![text.clone()]);
    }
    // query 里的 `?term=xxx`：Node 的 URLSearchParams 已做过百分号解码，
    // 这里自己解一次（axum 的 raw query 是解码前的原文）
    if let Some(value) = query_param(query, "term") {
        if !value.is_empty() {
            return Some(vec![value]);
        }
    }
    None
}

/// JSON 数组 → 字符串列表。
///
/// **不做类型过滤**：非字符串项以空串占位，后面的校验才能按 Node 的
/// `typeof item !== 'string' || !item.trim()` 报「词表只接受非空字符串」。
/// （若在这里静默丢掉非字符串项，用户传 `{terms:[123]}` 会得到 200 而不是 400。）
fn string_items(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            _ => String::new(),
        })
        .collect()
}

/// 从原始查询串里取参数（先按 `&` 切、再按 `=` 切，最后百分号解码）
fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if name == key {
            return Some(percent_decode(value));
        }
    }
    None
}

/// 百分号解码（对应 URLSearchParams 的解码；`+` 也算空格，与表单语义一致）
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|text| u8::from_str_radix(text, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 词表校验（对应 validateTerms）：缺参、空表、空串、长度、上限。
///
/// 顺序与文案逐字照抄 Node —— 前端按这些文案弹 toast，改一个字都算契约变更。
/// 长度按 **UTF-16 口径**（JS 的 `String.length`）：星面字符（emoji 等）算 2，
/// 用字符数会让边界上的词在两边一边拒一边收。
fn validate_terms(terms: Option<Vec<String>>) -> Result<Vec<String>, Response> {
    let Some(terms) = terms else {
        return Err(management_error(
            400,
            "缺少词表：请提供 { terms: [...] } 或 { term: \"...\" }",
        ));
    };
    if terms.is_empty() {
        return Err(management_error(400, "词表为空：至少需要一个词"));
    }
    if terms.iter().any(|item| item.trim().is_empty()) {
        return Err(management_error(400, "词表只接受非空字符串"));
    }
    if terms.iter().any(|item| utf16_len(item.trim()) > MAX_TERM_LENGTH) {
        return Err(management_error(
            400,
            format!("单个词长度不能超过 {MAX_TERM_LENGTH} 个字符"),
        ));
    }
    if terms.len() > MAX_TERMS {
        return Err(management_error(400, format!("词表最多 {MAX_TERMS} 个词")));
    }
    Ok(terms)
}
