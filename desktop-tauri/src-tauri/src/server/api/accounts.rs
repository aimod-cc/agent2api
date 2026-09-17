//! 账号管理路由（对照 src/workbuddy-account-routes.mjs 逐条实现）。
//!
//!   GET    /api/accounts                账号列表（含当前账号标记、优先级、启用状态、代理）
//!   POST   /api/accounts                手动添加账号（accessToken/refreshToken JSON）
//!   GET    /api/accounts/export         导出全部账号（含 token，换机器后导入继续用）
//!   POST   /api/accounts/import         导入账号（merge：按 uid 匹配，命中更新、未命中追加）
//!   POST   /api/accounts/current        把账号置顶（即切换当前账号）{ id }
//!   POST   /api/accounts/batch          批量操作 { action, ids, proxy? }
//!   POST   /api/accounts/refresh        刷新指定（或当前）账号的 token { id? }
//!   GET    /api/accounts/usage          逐账号查询积分/额度（并发，单账号失败不拖垮整批）
//!   POST   /api/accounts/checkin        签到（串行，跳过已禁用与国际版账号）{ id? }
//!   PATCH  /api/accounts/{id}           修改账号属性 { name?, priority?, enabled?, proxy? }
//!   POST   /api/accounts/{id}/move      与相邻账号交换优先级 { direction: 'up' | 'down' }
//!   DELETE /api/accounts/{id}           删除账号
//!
//! 出网代理的两条（/api/proxies、/api/proxies/test）在 `api::proxies`，
//! 但它们的入口 `proxies_entry` 留在本文件 —— 与账号入口挨着，便于对照
//! Node 版 `tryHandle` → `tryHandleProxies` 的判定顺序。
//!
//! ── 分发方式：与 Node 版同构的「一个大入口 + 按路径判定」────────
//! Node 版是 `tryHandle(req,res,path)`：先按完整路径匹配固定子路径，都不命中
//! 才把剩余段当账号 id。这些判定顺序**是可观察行为**，例如：
//!   DELETE /api/accounts/export → 走 `<id>` 分支 → 404「账号不存在」
//!   GET    /api/accounts/xxx/yyy → 谁都不命中 → 404「Not found: GET ...」
//! 若改用 axum 的静态路由 + `{id}`（matchit 静态优先），前者会变成 405 兜底，
//! 与 Node 分叉。因此这里刻意保留 Node 的判定结构：一条 `/api/accounts/{*rest}`
//! 通配入口 + 一个方法/路径分发函数，注册顺序不再重要，行为逐条对齐。
//!
//! ── usage / checkin 两条的两处易错点 ─────────────────────────
//!   ① `skipped` 的口径在两条路径上**不同**（一个是「可用账号中被禁用的数」，
//!      另一个是「可用账号总数 − 可签到数」），详见 `resolve_checkin_targets`；
//!   ② `usage` 的「没有可用凭证」分支**不带 name 键**（Node 那条早退 return
//!      就没带），别顺手补齐，详见 `query_usage_for`。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::Method;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStoreError;
use crate::server::core::auth::WorkBuddyAuthError;
use crate::server::core::billing::checkin;
use crate::server::core::proxies::ProxyConfigError;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

fn store_error(error: AccountStoreError) -> Response {
    logging::log("[Accounts]", &format!("❌ {}", error.message));
    management_error(error.status_code, error.message)
}

fn auth_error(error: WorkBuddyAuthError) -> Response {
    logging::log("[Accounts]", &format!("❌ {}", error.message));
    management_error(error.http_status(), error.message)
}

pub(super) fn proxy_error(error: ProxyConfigError) -> Response {
    logging::log("[Accounts]", &format!("❌ {}", error.message));
    management_error(error.status_code, error.message)
}

/// 解析请求体：空 body 视为 `{}`；非法 JSON 按各处的原始文案报 400。
///
/// Node 版不同分支的文案不一样（「上传内容不是有效 JSON」/「请求内容不是有效 JSON」），
/// 所以文案由调用方传入，不统一成一句话。
fn parse_json_body(body: &Bytes, fallback_message: &str) -> Result<Value, Response> {
    match parse_body(body) {
        Ok(value) => Ok(value),
        Err(_) => Err(management_error(400, fallback_message)),
    }
}

/// 百分号解码（对应 Node 版 `decodeURIComponent(path.slice(...))`）。
///
/// axum 已在提取参数时解过一次；账号 id 形如 `user-<uid>`、正常不含特殊字符，
/// 所以这一步基本是恒等变换 —— 保留它是因为 Node 版就是这么写的，
/// 手工构造的 URL 里带 `%2F` 这类编码时行为才不会分叉。
fn decode_segment(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|text| u8::from_str_radix(text, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

// ─── axum 入口 ──────────────────────────────────────────────

/// `/api/accounts`（无尾段）与 `/api/accounts/{*rest}` 共用的入口。
///
/// 用 `any(...)` 注册（接受任意方法），因为 Node 的判定里有「PATCH/DELETE 落到
/// `<id>` 分支」这种跨方法的路径匹配 —— 交给 axum 的方法路由反而会把它拆错。
pub async fn accounts_entry(State(state): State<ServerState>, request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let full_path = request.uri().path().to_string();
    let body = match axum::body::to_bytes(request.into_body(), crate::server::http::MAX_BODY_SIZE)
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => return management_error(413, format!("请求体读取失败或过大: {error}")),
    };
    // `suffix` 为空 = 精确命中 `/api/accounts`（列表/新增）；其它情况（含 `/api/accounts/`
    // 这个只有尾斜杠的形态）都是子路径 —— Node 版用 `path === '/api/accounts'` 严格
    // 判等，所以 `/api/accounts/` 落到最后的 404 分支，这里必须保留这个区分
    let suffix = full_path.strip_prefix("/api/accounts").unwrap_or("");
    let rest = suffix.strip_prefix('/').map(str::to_string);
    dispatch(state, method, rest.as_deref(), &full_path, &body).await
}

/// `/api/proxies` 与 `/api/proxies/test` 的入口（同样接受任意方法）
pub async fn proxies_entry(State(state): State<ServerState>, request: axum::extract::Request) -> Response {
    let method = request.method().clone();
    let full_path = request.uri().path().to_string();
    let rest = full_path
        .strip_prefix("/api/proxies")
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let body = match axum::body::to_bytes(request.into_body(), crate::server::http::MAX_BODY_SIZE)
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => return management_error(413, format!("请求体读取失败或过大: {error}")),
    };
    match (method.as_str(), rest.as_str()) {
        ("GET", "") => super::proxies::list_proxies(&state).await,
        ("POST", "test") => super::proxies::test_proxy(&state, &body).await,
        // 已注册路径上的其它方法：Node 的 tryHandleProxies 落到它自己的 404 信封
        _ => management_error(
            404,
            format!("Not found: {} {full_path}", method.as_str()),
        ),
    }
}

// ─── 路径分发（与 Node 版 tryHandle 的判定顺序逐条对齐）──────

/// 账号路由的路径分发。
///
/// `rest` 为 `None` 表示精确命中 `/api/accounts`；`Some(...)` 是去掉一层前导斜杠后的
/// 剩余段（`/api/accounts/` 得到的是 `Some("")` —— Node 严格判等，它属于子路径而非列表）。
/// `full_path` 是原始完整路径（404 文案里要原样回显）。判定顺序**照抄 Node 版**：
///
///   ① 固定子路径（无尾段的一批）：GET/POST ``、GET export、POST import、
///      POST current、POST batch、POST refresh、GET usage、POST checkin
///   ② POST + 以 `/move` 结尾 → 调整顺序
///   ③ PATCH / DELETE + 有剩余段 → 当成账号 id（**不做白名单校验**，
///      所以 `DELETE /api/accounts/export` 是「删一个叫 export 的账号」）
///   ④ 谁都不命中 → 404「Not found: <METHOD> <path>」（管理 API 信封）
///
/// 第 ③ 步的顺序很关键：Node 的 PATCH 分支只判 `startsWith('/api/accounts/')`，
/// 所以固定子路径在 GET/POST 之外的方法上会被当作账号 id，得到 404「账号不存在」。
pub async fn dispatch(
    state: ServerState,
    method: Method,
    rest: Option<&str>,
    full_path: &str,
    body: &Bytes,
) -> Response {
    // ① 精确命中 `/api/accounts`
    if rest.is_none() {
        match method.as_str() {
            "GET" => return ok_json(state.store().list_accounts()),
            "POST" => return add_account(&state, body).await,
            // Node 版对 `/api/accounts` 上的其它方法同样走 404 信封
            _ => {
                return management_error(404, format!("Not found: {} {full_path}", method.as_str()))
            }
        }
    }
    let rest = rest.unwrap_or("");

    // ② 固定子路径（Node 版把它们放在 `<id>` 通配之前的原因）
    match (method.as_str(), rest) {
        ("GET", "export") => return export_accounts(&state),
        ("POST", "import") => return import_accounts(&state, body).await,
        ("POST", "current") => return set_current(&state, body).await,
        ("POST", "batch") => return batch_accounts(&state, body).await,
        ("POST", "refresh") => return refresh_account(&state, body).await,
        ("GET", "usage") => return accounts_usage(&state).await,
        ("POST", "checkin") => return accounts_checkin(&state, body).await,
        _ => {}
    }

    // ③ POST + /move 结尾
    if method == Method::POST {
        if let Some(id) = rest.strip_suffix("/move") {
            let id = decode_segment(id);
            if !id.is_empty() {
                return move_account(&state, &id, body).await;
            }
        }
    }

    // ④ PATCH / DELETE → 把剩余段当账号 id。
    //    id 为空（即 `/api/accounts/`）也交给 handler：它返回 400「缺少账号 id」，
    //    这与 Node 版 PATCH 分支 `if (!id) throw` 的结果一致。
    //    注意与 `/api/accounts//` 的区别：那条解出的是 "/"（非空）→ 404「账号不存在」。
    match method.as_str() {
        "PATCH" => return patch_account(&state, &decode_segment(rest), body).await,
        "DELETE" => return delete_account(&state, &decode_segment(rest)),
        _ => {}
    }

    // ⑤ 兜底 404（Node 版 `tryHandle` 结尾那句）
    management_error(404, format!("Not found: {} {full_path}", method.as_str()))
}

// ─── POST /api/accounts ─────────────────────────────────────

pub async fn add_account(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "上传内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state.store().add_account(&payload, None) {
        Ok(account) => ok_json(json!({
            "account": account,
            "list": state.store().list_accounts(),
        })),
        Err(error) => store_error(error),
    }
}

// ─── GET /api/accounts/export ───────────────────────────────

pub fn export_accounts(state: &ServerState) -> Response {
    let data = crate::server::core::account_transfer::export_accounts(state.store());
    let count = data
        .get("accounts")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    logging::log("[Accounts]", &format!("📤 账号已导出: {count} 个"));
    ok_json(data)
}

// ─── POST /api/accounts/import ──────────────────────────────

pub async fn import_accounts(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match crate::server::core::account_transfer::import_accounts(state.store(), &payload) {
        Ok(result) => ok_json(result),
        Err(error) => store_error(error),
    }
}

// ─── POST /api/accounts/current ─────────────────────────────

pub async fn set_current(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let id = payload.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return management_error(400, "缺少账号 id");
    }
    match state.store().promote_to_front(id) {
        Ok(result) => {
            if result.get("changed").and_then(Value::as_bool) == Some(false) {
                logging::log("[Accounts]", &format!("已是当前账号，无需切换: {id}"));
            }
            ok_json(result)
        }
        Err(error) => store_error(error),
    }
}

// ─── POST /api/accounts/batch ───────────────────────────────

pub async fn batch_accounts(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    // ids 去重 + 过滤空串，逐字照抄 Node 版
    let mut ids: Vec<String> = Vec::new();
    if let Some(items) = payload.get("ids").and_then(Value::as_array) {
        for item in items {
            if let Some(text) = item.as_str() {
                if !text.is_empty() && !ids.iter().any(|existing| existing == text) {
                    ids.push(text.to_string());
                }
            }
        }
    }
    if ids.is_empty() {
        return management_error(400, "缺少要操作的账号 id");
    }
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let result = match action.as_str() {
        "remove" => state.store().batch_remove(&ids),
        "enable" | "disable" => {
            let enabled = action == "enable";
            state.store().batch_update(&ids, &json!({ "enabled": enabled }))
        }
        "proxy" => {
            // 允许 proxy=null（批量改为直连）；缺字段是显式报错
            let Some(target) = payload.get("proxy") else {
                return management_error(400, "批量修改代理时缺少 proxy 字段");
            };
            state.store().batch_update(&ids, &json!({ "proxy": target }))
        }
        other => {
            return management_error(
                400,
                format!(
                    "不支持的批量操作: {}",
                    if other.is_empty() { "(空)" } else { other }
                ),
            )
        }
    };

    match result {
        Ok(value) => {
            // 响应带上 action，与 Node 版 `{ action, ...result, list }` 一致
            let mut merged = Map::new();
            merged.insert("action".to_string(), Value::String(action));
            if let Some(object) = value.as_object() {
                for (key, item) in object {
                    merged.insert(key.clone(), item.clone());
                }
            }
            ok_json(Value::Object(merged))
        }
        Err(error) => store_error(error),
    }
}

// ─── POST /api/accounts/refresh ─────────────────────────────

pub async fn refresh_account(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    let id = match id {
        Some(value) => value,
        None => match state.store().get_active_entry() {
            Some(entry) => entry.id,
            None => return management_error(400, "当前没有可用账号"),
        },
    };
    logging::verbose("[Accounts]", &format!("刷新账号 token: {id}"));
    match state.auth().refresh_account(&id).await {
        Ok(_) => ok_json(json!({
            "refreshedId": id,
            "list": state.store().list_accounts(),
        })),
        Err(error) => auth_error(error),
    }
}

// ─── GET /api/accounts/usage 与 POST /api/accounts/checkin ──

/// 批量操作的目标集合：默认跳过已禁用账号（避免无谓地打上游）。
///
/// 对照 Node 版 `resolveBatchTargets`：给了 id 就只取该账号且**不看 enabled**
/// （用户显式指定就该执行）；没给 id 时取全部启用账号，并统计被跳过的数量。
/// 返回 `(targets, skipped)`；`Err` 是已经构造好的错误响应。
///
/// 返回 `Box<Response>`：axum 的 Response 有 128 字节，直接塞进 Result 会让
/// 这个「热路径上的小函数」每次返回都搬一大块（clippy 的 result_large_err）。
/// 包一层 Box 只在这条**错误**分支上多一次分配，正常路径零开销。
fn resolve_batch_targets(
    state: &ServerState,
    id: Option<&str>,
) -> Result<(Vec<Value>, usize), Box<Response>> {
    let snapshot = state.store().list_accounts();
    let accounts: Vec<Value> = snapshot
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_available = |account: &Value| {
        account
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    let is_enabled = |account: &Value| {
        account
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    if let Some(id) = id.filter(|value| !value.is_empty()) {
        let found: Vec<Value> = accounts
            .iter()
            .filter(|account| account.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
            .collect();
        if found.is_empty() {
            return Err(Box::new(management_error(404, "账号不存在")));
        }
        return Ok((found, 0));
    }
    let available: Vec<Value> = accounts.into_iter().filter(is_available).collect();
    let skipped = available.len();
    let targets: Vec<Value> = available.into_iter().filter(is_enabled).collect();
    let skipped = skipped - targets.len();
    Ok((targets, skipped))
}

/// ── 签到目标解析搬去了 `core::billing::checkin` ─────────────
/// `resolve_checkin_targets` / `checkin_for` / `runCheckin` 三段整体下沉到
/// core：定时签到（core::auto_checkin）与 `POST /api/accounts/checkin` 必须共用
/// 同一段逻辑（Node 版是把 accountRoutes.runCheckin 注入 createAutoCheckin）。
/// 这里只剩 `accounts_checkin` 一个转发壳，规则与 `skipped` 口径的说明见
/// `core::billing::checkin::resolve_checkin_targets` 的注释。
/// 注意上面的 `resolve_batch_targets` **仍留在这里**：
/// `/api/accounts/usage` 的并发查询要用它，且它的 `skipped` 口径与签到不同。

/// 积分查询失败的原因：区分「没有凭证」（Node 的早退分支，字段少 name）
/// 与「请求失败」（走 catch 分支，字段带 name）。
enum UsageFailure {
    /// 账号没有可用凭证 —— Node 里那个 `return { id, usage:null, error }`
    NoCredentials,
    /// 其余失败 —— Node 里 catch 里那句 `return { id, name, usage:null, error }`
    Request(String),
}

/// 单个账号的积分查询。token 失效时刷新后重试一次；
/// 任何失败都收敛为 `{error}` 而不是抛出，保证批量查询不被单个账号拖垮。
async fn query_usage_for(state: &ServerState, account: &Value) -> Value {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = account.get("name").cloned().unwrap_or(Value::Null);
    match query_usage_inner(state, &id).await {
        Ok(usage) => json!({ "id": id, "name": name, "usage": usage, "error": Value::Null }),
        Err(UsageFailure::NoCredentials) => {
            // Node 的这条早退 `return` **不含 name 键**（只有 catch 分支才带）
            json!({ "id": id, "usage": Value::Null, "error": "没有可用凭证" })
        }
        Err(UsageFailure::Request(message)) => {
            logging::verbose("[Accounts]", &format!("账号 {id} 积分查询失败: {message}"));
            json!({ "id": id, "name": name, "usage": Value::Null, "error": message })
        }
    }
}

/// 查询一个账号的积分简报，401 时刷新 token 后重试一次。
///
/// 「401 说明 token 被服务端拒绝而非临期」—— Node 版据此才走刷新重试；
/// 其它错误直接抛出（不浪费时间在刷新上）。
async fn query_usage_inner(state: &ServerState, id: &str) -> Result<Value, UsageFailure> {
    let Some(entry) = state.store().get_session_by_id(id) else {
        return Err(UsageFailure::NoCredentials);
    };
    match state
        .billing()
        .query_credits_summary(Some(&entry.session), None)
        .await
    {
        Ok(usage) => Ok(usage),
        Err(error) => {
            if error.status_code != 401 {
                return Err(UsageFailure::Request(error.message));
            }
            // token 被上游拒绝：刷新后重试一次
            let Some(creds) = state.store().get_credentials_by_id(id) else {
                return Err(UsageFailure::Request(error.message));
            };
            if creds.refresh_token.is_empty() {
                return Err(UsageFailure::Request(error.message));
            }
            if let Err(refresh_error) = state.auth().refresh_account(id).await {
                return Err(UsageFailure::Request(refresh_error.message));
            }
            let Some(entry) = state.store().get_session_by_id(id) else {
                return Err(UsageFailure::Request(error.message));
            };
            state
                .billing()
                .query_credits_summary(Some(&entry.session), None)
                .await
                .map_err(|retry| UsageFailure::Request(retry.message))
        }
    }
}

/// GET /api/accounts/usage
///
/// 逐账号并发查询积分汇总（`{ results: [{id,name,usage,error}], skipped }`）。
///
/// **并发**是关键：Node 版用 `Promise.all(targets.map(queryUsageFor))`，
/// 20 个账号串行查询会让前端转圈 20 次往返。这里用 `join_all` 在同一个任务里
/// 并发轮询（每个 future 都是网络等待，天然交错），并且**结果顺序与 targets
/// 一致** —— 与 `Promise.all` 的语义完全相同。目标集合最多是
/// MAX_ACCOUNTS(20) 个启用账号，不需要额外的并发限流。
pub async fn accounts_usage(state: &ServerState) -> Response {
    let (targets, skipped) = match resolve_batch_targets(state, None) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let futures: Vec<_> = targets
        .iter()
        .map(|account| query_usage_for(state, account))
        .collect();
    let results = futures::future::join_all(futures).await;
    ok_json(json!({ "results": results, "skipped": skipped }))
}

/// POST /api/accounts/checkin
///
/// 串行签到：避免多账号同时打上游触发 11128 风控。
/// id 为空时签全部符合条件的账号，给了 id 则只签该账号；
/// 返回 `{results, succeeded, total, skipped}`。
///
/// 执行体是 `core::billing::checkin::run_checkin` —— 与定时签到共用同一段逻辑
/// （Node 版也是 `createAutoCheckin({ runCheckin: accountRoutes.runCheckin })`）。
/// 差异只有一处：Node 的整个 handler 在 try 里，抛出的 AccountStoreError
/// 走 errorPayload 得到 **OpenAI 风格** body；Rust 侧本文件的历史实现用的是
/// 管理信封（`management_error`）。这里保持**本文件既有形状**不变，避免
/// 切片 6 顺手改掉已交付的契约 —— 两种信封都在 400/404 上，前端只看 message。
pub async fn accounts_checkin(state: &ServerState, body: &Bytes) -> Response {
    let id = match parse_body(body) {
        Ok(payload) => payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty()),
        Err(_) => return management_error(400, "请求内容不是有效 JSON"),
    };
    match checkin::run_checkin(state.store(), state.billing(), id.as_deref()).await {
        Ok(result) => ok_json(result),
        Err(error) => management_error(error.status_code, error.message),
    }
}

// ─── PATCH /api/accounts/{id} ───────────────────────────────

pub async fn patch_account(state: &ServerState, id: &str, body: &Bytes) -> Response {
    if id.is_empty() {
        return management_error(400, "缺少账号 id");
    }
    let patch = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !patch.is_object() {
        return management_error(400, "请求内容必须是 JSON 对象");
    }
    match state.store().update_account(&id, &patch) {
        Ok((account, changes)) => ok_json(json!({
            "account": account,
            "changes": changes,
            "list": state.store().list_accounts(),
        })),
        Err(error) => store_error(error),
    }
}

// ─── POST /api/accounts/{id}/move ───────────────────────────

pub async fn move_account(state: &ServerState, id: &str, body: &Bytes) -> Response {
    if id.is_empty() {
        return management_error(400, "缺少账号 id");
    }
    let payload = match parse_json_body(&body, "请求内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let direction = if payload.get("direction").and_then(Value::as_str) == Some("down") {
        "down"
    } else {
        "up"
    };
    match state.store().move_account(&id, direction) {
        Ok(result) => {
            if result.get("moved").and_then(Value::as_bool) == Some(false) {
                let reason = result
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("顺序未变");
                logging::log("[Accounts]", &format!("顺序未变（{reason}）"));
            }
            ok_json(result)
        }
        Err(error) => store_error(error),
    }
}

// ─── DELETE /api/accounts/{id} ──────────────────────────────

pub fn delete_account(state: &ServerState, id: &str) -> Response {
    if id.is_empty() {
        return management_error(400, "缺少账号 id");
    }
    match state.store().remove_account(id) {
        Ok(()) => ok_json(json!({ "list": state.store().list_accounts() })),
        Err(error) => store_error(error),
    }
}
