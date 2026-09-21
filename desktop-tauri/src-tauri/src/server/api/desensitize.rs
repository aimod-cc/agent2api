//! 敏感词脱敏 — 词表维护路由（对照 src/workbuddy-desensitize-routes.mjs 逐条实现）。
//!
//!   GET    /api/desensitize              当前状态（开关 / 词表 / 角色 / 作用提供商 / 命中统计）
//!   POST   /api/desensitize/enabled      开关 { enabled }
//!   POST   /api/desensitize/roles        作用角色 { roles: ['system','user'] }
//!   PUT    /api/desensitize/providers    作用提供商 { providers: ['workbuddy'] }（Agent2API 新增）
//!   POST   /api/desensitize/providers    （同上；幂等全量替换）
//!   PUT    /api/desensitize/terms        全量替换词表 { terms: [...] }
//!   POST   /api/desensitize/terms        追加词 { terms: [...] | term: '...' }
//!   DELETE /api/desensitize/terms        删除词 { terms: [...] } 或 ?term=xxx
//!   POST   /api/desensitize/reset        恢复默认词表
//!   POST   /api/desensitize/stats/reset  清空命中统计
//!
//! ── providers 与 roles 的区别（别混）─────────────────────────
//! `roles` = 消息里的**哪些角色**参与脱敏（system/user/…），`providers` =
//! **哪些上游**的请求需要脱敏（架构文档 §3.5）。前者是报文维度的选择，
//! 后者是路由维度的选择；判定时机在转发前（候选链与作用集合有交集即脱敏），
//! 由 W2b 在 chat_completions 里接。
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

    // 作用提供商（Agent2API 改造 §3.5）：全量替换语义。
    // 两条路径都收：
    //   PUT /api/desensitize            { providers: ["workbuddy"] }（架构文档 §5 的
    //                                    「GET/PUT /api/desensitize*」字面形态）
    //   PUT/POST /api/desensitize/providers（与同组的 roles/terms 一致的子路径形态）
    // 两者走同一段逻辑。基础路径**只**接受 providers 字段（其余键忽略）：
    // 它原先对 PUT 是 404，新增这条分支不改动任何既有行为。
    let base_put = method == Method::PUT && action.is_empty();
    let sub_path = (method == Method::PUT || method == Method::POST) && action == "providers";
    if base_put || sub_path {
        let payload = match read_json(body) {
            Ok(payload) => payload,
            Err(response) => return response,
        };
        let providers = match validate_providers(payload.get("providers")) {
            Ok(providers) => providers,
            Err(response) => return response,
        };
        let next = desensitizer.set_providers(&providers);
        logging::log(
            "[Desensitize]",
            &format!("作用提供商: {}", providers.join("、")),
        );
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

    // 立即同步远程词库（force：忽略版本号比对，把远端词条全量补一遍）。
    //
    // 与定时任务那条走**同一个** `remote::sync`：两条入口的差异只有 force 这一个
    // 参数（定时走版本闸、手动是「现在真的去拉一次」），因此「定时能成功、手动却
    // 失败」这类分叉不可能出现。
    //
    // 挂在本模块的路径下而不是 /api/scheduled-tasks：它是**脱敏**这个领域里的动作
    //（同步完要刷新的也是脱敏页），与「定时任务的开关 / 间隔」不是一回事 ——
    // 定时任务页那边仍可通过「立即执行」按钮触发同一条链路。
    if method == Method::POST && action == "remote-sync" {
        let summary = crate::server::core::desensitize::remote::sync(true).await;
        let mut payload = desensitizer.state();
        if let Some(object) = payload.as_object_mut() {
            object.insert("message".to_string(), Value::String(summary));
        }
        return ok_json(payload);
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

/// 作用提供商校验（Agent2API 改造 §3.5）：数组元素必须是注册表里的已知 id。
///
/// 与词表校验同款「严格写侧」：**未知项一律 400**，不静默丢弃 —— 用户点了保存
/// 却没生效（因为拼错了一个 id）是最难排查的一类问题。读侧（手改库里的状态
/// 记录，或迁移进来的旧 `desensitize.json`）则是宽容的（`normalize_providers`
/// 丢弃未知项、空则回落缺省），两侧口径不同各有理由，与保留期天数的处理一致。
///
/// 空数组也拒绝：`normalize_providers` 对空数组会回落成缺省 `["workbuddy"]`，
/// 若在这里放行，用户写 `{"providers": []}` 会得到 200 但配置变成 workbuddy ——
/// 「停用脱敏」应当用 `enabled` 开关（那个语义准确且立即生效）。
///
/// ── W4a：白名单口径改为注册表（本函数一行判定都没改）──────────
/// 「已知 id」判定走 `providers::is_known_provider_id`（= `kind_from_id(id).is_some()`，
/// 唯一事实来源是 `PROVIDERS` 注册表）。于是 CatPaw / AutoClaw 一进注册表就
/// **自动**成为合法的脱敏作用提供商，本函数与配置层都不用改 —— 这正是当初
/// 把 id 白名单收敛到注册表的理由（旧写法会在这里漏掉新 provider，
/// 用户看到的是「未知的提供商: catpaw」这种莫名其妙的 400）。
fn validate_providers(value: Option<&Value>) -> Result<Vec<String>, Response> {
    let items = match value {
        Some(Value::Array(items)) => items,
        _ => {
            return Err(management_error(
                400,
                "缺少提供商列表：请提供 { providers: [\"workbuddy\"] }",
            ))
        }
    };
    if items.is_empty() {
        return Err(management_error(
            400,
            "提供商列表为空：至少需要一个（要停用脱敏请用开关）",
        ));
    }
    let mut providers: Vec<String> = Vec::new();
    for item in items {
        let Some(id) = item.as_str() else {
            return Err(management_error(400, "提供商只接受字符串"));
        };
        if !crate::server::core::providers::is_known_provider_id(id) {
            return Err(management_error(400, format!("未知的提供商: {id}")));
        }
        if !providers.iter().any(|known| known == id) {
            providers.push(id.to_string());
        }
    }
    Ok(providers)
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
