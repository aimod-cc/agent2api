//! 账号管理路由（对照 src/workbuddy-account-routes.mjs 逐条实现）。
//!
//!   GET    /api/accounts                账号列表（含当前账号标记、优先级、启用状态、代理、
//!                                       provider 与顶层 providers 摘要）
//!   POST   /api/accounts                手动添加账号（accessToken/refreshToken JSON）
//!   GET    /api/accounts/export         导出全部账号（含 token，换机器后导入继续用）
//!   POST   /api/accounts/import         导入账号（merge：按 uid 匹配，命中更新、未命中追加）
//!   POST   /api/accounts/current        把账号置顶（仅调整全局优先级）{ id }
//!   POST   /api/accounts/batch          批量操作 { action, ids, proxy? }
//!   POST   /api/accounts/refresh        刷新指定（或当前）账号的 token { id? }
//!   POST   /api/accounts/refresh-expiring 刷新**全部**已过期/临期的账号凭证（批量）
//!   GET    /api/accounts/usage          逐账号查询积分/额度（并发，单账号失败不拖垮整批）
//!                                       `?id=` 指定单个账号（**不看启用状态**，见 usage_query）
//!   GET    /api/accounts/usage/snapshot 最近一次**定时查询**的结果快照（形状同 usage）
//!   GET    /api/accounts/connections     逐账号活跃连接数（账号页「连接数」列，2 秒轮询）
//!   POST   /api/accounts/checkin        签到（串行，跳过已禁用与国际版账号）{ id? }
//!   PATCH  /api/accounts/{id}           修改账号属性 { name?, priority?, enabled?, proxy? }
//!   POST   /api/accounts/{id}/move      与相邻账号交换优先级 { direction: 'up' | 'down' }
//!   DELETE /api/accounts/{id}           删除账号
//!
//! ── Agent2API 改造在本文件的行为 ─────────────────────────────
//!   - 列表响应的**形状是增量的**：每个账号对象多一个 `provider`、顶层多一个
//!     `providers` 摘要（都由 `AccountStore::list_accounts` 组装，本文件不用改）；
//!   - `POST /api/accounts` 按 payload 的 provider **穷举分派**（见 `add_account`）；
//!   - `usage` 从「只服务 workbuddy」扩到**四家混查**（余额查询移植）：目标集合
//!     不再按 provider 过滤，逐账号分流到 `ProviderAdapter::query_usage`。
//!     `checkin` 仍然只服务 workbuddy；其余 CRUD 语义保持不变。
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
use crate::server::core::providers::adapter::adapter_for;
use crate::server::core::providers::ProviderKind;
use crate::server::errors::management_error;
use crate::server::http::{ok_json, parse_body};
use crate::server::logging;
use crate::server::ServerState;

/// 账号存储错误 → 管理信封（打日志 + 原状态码）。
///
/// 可见性是 `pub(super)`：`api::session` 的 AutoClaw 验证码登录要落账号
/// （`add_autoclaw_account`），失败时必须是同一个响应形状与同一条日志格式 ——
/// 让那边各写一份会让「登录失败」与「添加失败」在界面上长得不一样。
pub(super) fn store_error(error: AccountStoreError) -> Response {
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
    // 原始查询串（未解码的原文，由用到它的分支自行 `query_param` 解码）。
    // 目前只有 `GET /api/accounts/usage?id=<id>` 用它 —— 那一支是「用户手点某一行
    // 的积分按钮」，与批量的区别见 `core::usage_query::query_all` 的说明。
    let query = request.uri().query().unwrap_or("").to_string();
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
    dispatch(state, method, rest.as_deref(), &full_path, &query, &body).await
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
/// `full_path` 是原始完整路径（404 文案里要原样回显），`query` 是未解码的查询串
/// （只有 `usage` 那一支用它取 `id`，见 `accounts_usage::accounts_usage`）。判定顺序
/// **照抄 Node 版**：
///
///   ① 固定子路径（无尾段的一批）：GET/POST ``、GET export、POST import、
///      POST current、POST batch、POST refresh、GET usage、POST checkin
///   ② POST + 以 `/move` 结尾 → 调整顺序；以 `/rate-limits/clear` 结尾 → 清除限流标记
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
    query: &str,
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
        ("POST", "refresh-expiring") => return refresh_expiring_accounts(&state).await,
        // 余额 / 积分查询（四家混查）实现在 `api::accounts_usage`（拆分见那里的模块头）。
        // `?id=` 是「手点某一行积分按钮」的单查形态，语义见 accounts_usage 的说明。
        ("GET", "usage") => {
            return super::accounts_usage::accounts_usage(&state, query).await
        }
        // 定时查询那一轮的结果快照（形状同 usage，多一个 `at`）
        ("GET", "usage/snapshot") => {
            return super::accounts_usage::accounts_usage_snapshot().await
        }
        // 账号级活跃连接数（账号页「连接数」列；见 `core::upstream::connections`）
        ("GET", "connections") => return account_connections(&state),
        ("POST", "checkin") => return accounts_checkin(&state, body).await,
        _ => {}
    }

    // ③ POST + /move 结尾；POST + /rate-limits/clear 结尾
    if method == Method::POST {
        if let Some(id) = rest.strip_suffix("/move") {
            let id = decode_segment(id);
            if !id.is_empty() {
                return move_account(&state, &id, body).await;
            }
        }
        if let Some(id) = rest.strip_suffix("/rate-limits/clear") {
            let id = decode_segment(id);
            if !id.is_empty() {
                return clear_rate_limits(&state, &id, body);
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

/// 添加账号：**按 payload 的 provider 分派到各家的添加路径**（W3-T4 起，
/// W4a 改为注册表驱动的穷举分派）。
///
/// ── 分派规则（W4a：不再手写「workbuddy/raccoon」清单）──────────
/// provider 字段先过 `providers::kind_from_id` 换算（**注册表就是白名单**，
/// 见 `providers::is_known_provider_id` 的说明），再按 kind 穷举：
///   ① `Raccoon` → 小浣熊路径：`importDesktop === true` 导入桌面端实时登录态
///      （忽略 token 字段）；否则手动添加（token / refreshToken，或粘贴
///      auth.json 内容）；
///   ② `CatPaw` → CatPaw 路径（W5-T-d4）：`importDesktop === true` 导入桌面端
///      实时登录态（`~/.meituan-catpaw/auth.json`）；否则手动添加（token + uid/
///      loginName，兼容粘贴原项目账号记录与桌面端 auth.json 内容）；
///   ③ `AutoClaw` → AutoClaw 路径（W4b-T-c2）：`importDesktop === true` 导入
///      桌面端实时登录态（`%APPDATA%/AutoClaw/auth.json`，DPAPI 解密）；
///      否则手动添加（token/refreshToken + deviceId，`enc:` 密文自动解密）；
///   ④ `WorkBuddy` 或**字段缺失** → 既有 workbuddy 路径（`store.add_account`
///      不读 payload 里的 provider，见该函数的说明）。
///
/// ── AutoClaw 曾经在这里显式 400（历史，别改回去）─────────────────
/// W4a–W4b 之间它的凭证链路还没落地，那时这里对它的 payload 报 400：若让它落进
/// ④ 的兜底，用户会得到一条**200 + 存进 workbuddy 组**的账号（凭证是别家的、
/// 分组是错的、转发永远失败），界面上看不出异常。W4b-T-c2 起它有自己的分支
/// （③），该 400 随之退役；纪律不变 —— **绝不写进没人认的组**。分派用穷举
/// match：新增 provider 时编译器会强制给出分支；注册表里没有的 id（前端比后端
/// 新、或手改的请求）仍走 ④ —— 老客户端不带 provider 字段，未知 id 不能报错。
pub async fn add_account(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_json_body(&body, "上传内容不是有效 JSON") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let store = state.store();
    // 三家（小浣熊 / CatPaw / AutoClaw）的分派形状相同：`importDesktop: true`
    // 导入桌面端实时登录态，否则按 payload 手动添加。差异只在调哪个方法，
    // 因此这里把「取 flag + 取 name」收一次，各分支只留一行调用。
    let import_desktop = payload
        .get("importDesktop")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let import_name = name.as_deref();
    let result = match crate::server::core::providers::kind_from_id(provider) {
        Some(crate::server::core::providers::ProviderKind::Raccoon) => {
            if import_desktop {
                store.import_raccoon_desktop_account("manual")
            } else {
                store.add_raccoon_account(&payload, import_name)
            }
        }
        Some(crate::server::core::providers::ProviderKind::CatPaw) => {
            if import_desktop {
                store.import_catpaw_desktop_account("manual")
            } else {
                store.add_catpaw_account(&payload, import_name)
            }
        }
        // AutoClaw（W4b-T-c2）：粘贴 token / refreshToken（含 `enc:` 密文自动解密）
        // → 手动添加；`importDesktop: true` → 导入桌面端实时登录态（记录不落 token）
        Some(crate::server::core::providers::ProviderKind::AutoClaw) => {
            if import_desktop {
                store.import_autoclaw_desktop_account("manual")
            } else {
                store.add_autoclaw_account(&payload, import_name)
            }
        }
        Some(crate::server::core::providers::ProviderKind::Qoder) => {
            match crate::server::core::providers::qoder::auth::prepare_account(&payload).await {
                Ok(credentials) => store.add_qoder_account(&credentials, import_name, "manual"),
                Err(error) => Err(AccountStoreError::new(error.message, error.status_code)),
            }
        }
        // Cline：粘贴 accessToken / refreshToken → 手动添加；
        // `importDesktop: true` → 导入桌面端实时登录态（记录不落 token，
        // 实时读 `~/.cline/data/settings/providers.json`）。
        // 校验（token 非空 / 长度 / 补 `workos:` 前缀 / 从 JWT 取 expiresAt 与
        // 展示名）都在 `add_cline_account` 里，与设备授权登录共用同一入口。
        // **两个池各是一个 provider**（`cline-free` / `cline-pass`），添加时
        // 由 body 的 `provider` 决定进哪一家 —— 不需要额外的池参数。
        Some(kind @ (crate::server::core::providers::ProviderKind::ClineFree
            | crate::server::core::providers::ProviderKind::ClinePass)) => {
            let provider_id = crate::server::core::providers::kind_id(kind);
            if import_desktop {
                store.import_cline_desktop_account(provider_id, import_name)
            } else {
                store.add_cline_account(provider_id, &payload, import_name)
            }
        }
        Some(crate::server::core::providers::ProviderKind::WorkBuddy) | None => {
            store.add_account(&payload, None)
        }
    };
    match result {
        Ok(account) => ok_json(json!({
            "account": account,
            "list": store.list_accounts(),
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
                logging::log("[Accounts]", &format!("已在全局队列第一位，无需置顶: {id}"));
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

/// 刷新账号 token（**按账号所属 provider 分派**）。
///
/// workbuddy 账号走既有的 `AuthService::refresh_account`；小浣熊与 AutoClaw 账号
/// 各走自家适配器的 `refresh_access_token`；**CatPaw 账号没有可刷新的东西**
/// （§9.1：`X-Passport-Token` 过期只能在桌面端重新登录，没有 refreshToken），
/// 因此这里明确报 400 并说明做法 —— 静默走 workbuddy 的刷新会拿 CatPaw 的凭证
/// 去打腾讯的鉴权接口。
///
/// 为什么不统一到一个抽象：四家的刷新协议毫无共同点，而「刷新」是**管理动作**、
/// 不是转发链路上的 provider 契约（那条契约的 `refresh_access_token` 是转发时的
/// 同账号重试语义）。分派点只有这一处。
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
    if state.store().qoder_account_record(&id).is_some() {
        return refresh_provider_account(state, &id, ProviderKind::Qoder).await;
    }
    // Cline 账号：与 AutoClaw 同一取舍 —— **桌面端实时登录态不主动刷新**。
    // 网关与 Cline 客户端共用同一份 providers.json 里的 refreshToken，
    // 网关侧刷新会让客户端那边的会话作废（两边都在轮换，后写的赢）。
    // 手动账号（自己粘贴的凭证）走适配器的强制刷新链路。
    //
    // 「属于哪一家」由记录自己回答（两个池共用这条链路），因此判据是
    // `cline_account_provider` 而不是某个写死的 provider id —— 池是记录上的
    // 属性，刷新链路对它无差别。
    if let Some(provider_id) = state.store().cline_account_provider(&id) {
        if state.store().cline_is_desktop_account(&id) {
            return management_error(
                400,
                "Cline 桌面端登录态由 Cline 客户端维护，网关不主动刷新：\
                 请在 Cline 客户端重新登录后重新导入，或改用「填写凭证」添加",
            );
        }
        let kind = match crate::server::core::providers::kind_from_id(&provider_id) {
            Some(kind) => kind,
            None => return management_error(400, format!("未知的提供商：{provider_id}")),
        };
        return refresh_provider_account(state, &id, kind).await;
    }
    // 小浣熊账号：走它自己的刷新（结果由 `credentials::refresh` 回写）
    if state.store().raccoon_account_record(&id).is_some() {
        return refresh_provider_account(state, &id, ProviderKind::Raccoon).await;
    }
    // AutoClaw 账号：同一套「读凭证 → 强制刷新 → 按来源回写」的适配器链路。
    // **桌面端实时登录态不主动刷新**（原项目 `account-routes.mjs` 的同款拒绝）：
    // 网关与桌面端共用同一个 refresh_token，网关侧刷新会造成轮换竞态，
    // 因此这类账号明确报 400 并说明做法（`autoclaw::credentials` 模块头详述）。
    if let Some(record) = state.store().autoclaw_account_record(&id) {
        if record.get("desktop").and_then(Value::as_bool).unwrap_or(false) {
            return management_error(
                400,
                "AutoClaw 桌面端登录态由桌面客户端维护，网关不主动刷新：\
                 请在 AutoClaw 桌面端重新登录后重试",
            );
        }
        return refresh_provider_account(state, &id, ProviderKind::AutoClaw).await;
    }
    // CatPaw 账号：没有刷新机制（见本函数的说明）
    if state.store().catpaw_account_record(&id).is_some() {
        return management_error(
            400,
            "CatPaw 登录态没有刷新机制：请在 CatPaw 桌面端重新登录，\
             然后在本页重新导入桌面端登录态（或重新粘贴新的登录凭证）",
        );
    }
    match state.auth().refresh_account(&id).await {
        Ok(_) => ok_json(json!({
            "refreshedId": id,
            "list": state.store().list_accounts(),
        })),
        Err(error) => auth_error(error),
    }
}

/// 「读凭证 → 强制刷新 → 回写」这条适配器链路的手动刷新（小浣熊 / AutoClaw 共用）。
///
/// 两家的刷新协议完全不同（各自的 `credentials` / `refresh` 模块），但**管理动作
/// 的形状一致**：拿该账号的凭证、走 `refresh_access_token`（force 语义，不看临期
/// 窗口）、按来源回写。这里是唯一调用点，差异全在各家适配器里。
async fn refresh_provider_account(state: &ServerState, id: &str, kind: ProviderKind) -> Response {
    let adapter = adapter_for(kind);
    match adapter.refresh_access_token(state.store(), id).await {
        Ok(_) => ok_json(json!({
            "refreshedId": id,
            "list": state.store().list_accounts(),
        })),
        Err(error) => {
            logging::log("[Accounts]", &format!("❌ {}", error.message));
            management_error(i32::from(error.http_status().as_u16()), error.message)
        }
    }
}

// ─── POST /api/accounts/refresh-expiring ────────────────────

/// 刷新**全部**已过期 / 临期的账号凭证（批量维护动作）。
///
/// 与 `/api/accounts/refresh` 的分工：那条是**用户指定一个账号**的强制刷新
/// （不看临期窗口）；这条是**系统遍历全部账号**、只刷确实需要刷的那些。
/// 判定标准与刷新协议都在各家适配器里，本 handler 只透出结果 ——
/// 为什么这条判断不能留在壳侧、为什么失败不给非 2xx，
/// 见 `core::credential_maintenance` 的模块头与本文件那条 400 的说明。
pub async fn refresh_expiring_accounts(state: &ServerState) -> Response {
    let report =
        crate::server::core::credential_maintenance::refresh_expiring_report(state.store()).await;
    ok_json(report)
}

// ─── GET /api/accounts/usage 与 POST /api/accounts/checkin ──

/// ── 两条路径的目标解析都不在本文件了 ─────────────────────
/// · 余额查询的目标集合：`core::usage_query::resolve_batch_targets`
///   （它跟着查询逻辑一起下沉 —— 「定时查询积分」要用同一份口径）；
/// · 签到的目标集合：`core::billing::checkin::resolve_checkin_targets`
///   （定时签到与手动签到必须共用同一段逻辑）。
/// 本文件只剩 `accounts_checkin` 一个转发壳。


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
    // 批量路径的提供商范围取自动签到的同一份配置（两处入口一个口径）；
    // 指定 id 的单签不受范围限制（见 resolve_checkin_targets 的说明）
    let providers = state.auto_checkin().configured_providers();
    match checkin::run_checkin(state.store(), state.billing(), providers.as_slice(), id.as_deref())
        .await
    {
        Ok(result) => ok_json(result),
        Err(error) => management_error(error.status_code, error.message),
    }
}

// ─── GET /api/accounts/connections ──────────────────────────

/// 账号级活跃连接数（账号页「连接数」列）。
///
/// 响应 `{ counts: { <accountId>: <n> } }`：`counts` **只含非零项** ——
/// 界面按「缺失即 0」处理，于是不必为几十个空闲账号各送一个 0，
/// 轮询的响应体也小得多（2 秒一次）。
///
/// 口径是「此刻正在使用这个账号的下游请求数」（一个请求在账号间轮换时计数跟着走），
/// 见 `core::upstream::connections` 的模块头。数据是**进程内内存计数**，
/// 不读盘、不出网，所以这条接口足够便宜，能扛得住 2 秒一次的轮询。
///
/// 这条走 `protected` 分组（`http::router` 里 /api/accounts 整族都挂了 checkApiKey），
/// 与同族的 usage 一致 —— 界面在配置 Key 之前也读不到它。
pub fn account_connections(state: &ServerState) -> Response {
    let counts = state
        .upstream()
        .connections()
        .snapshot()
        .into_iter()
        .map(|(id, count)| (id, Value::from(count)))
        .collect::<Map<String, Value>>();
    ok_json(json!({ "counts": Value::Object(counts) }))
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
    // CatPaw 的余额查询凭证（`balanceToken`）先落盘：它是**这一家独有**的字段，
    // 通用 `update_account` 不认识它（见 `update_catpaw_balance_token` 的说明：
    // 不把它塞进四家共用的 apply_patch）。放在通用 patch 之前做，于是同一次
    // 「保存设置」里改备注名与改凭证都会生效，而通用 patch 不会因为多出来的
    // 键报错（它按字段白名单取值，未知键本就被忽略）。
    let mut balance_changes: Vec<String> = Vec::new();
    if let Some(value) = patch.get("balanceToken") {
        if state.store().catpaw_account_record(&id).is_none() {
            // 「账号不存在」与「账号存在但不是 CatPaw」要分开说：前者让用户刷新页面，
            // 后者告诉他这个字段不属于这家（静默忽略会让用户以为存进去了）。
            let known = state
                .store()
                .list_accounts()
                .get("accounts")
                .and_then(Value::as_array)
                .map(|accounts| {
                    accounts
                        .iter()
                        .any(|item| item.get("id").and_then(Value::as_str) == Some(id))
                })
                .unwrap_or(false);
            return if known {
                management_error(400, "只有 CatPaw 账号有余额查询凭证（balanceToken）")
            } else {
                store_error(AccountStoreError::new("账号不存在", 404))
            };
        }
        match state.store().update_catpaw_balance_token(&id, value) {
            Ok(changes) => balance_changes = changes,
            Err(error) => return store_error(error),
        }
    }
    match state.store().update_account(&id, &patch) {
        Ok((account, mut changes)) => {
            changes.extend(balance_changes);
            ok_json(json!({
                "account": account,
                "changes": changes,
                "list": state.store().list_accounts(),
            }))
        }
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

// ─── POST /api/accounts/{id}/rate-limits/clear ─────────────────

/// 清除账号的限流标记：body `{model?}`，给了模型只清那一个，否则全清。
///
/// 这是账号页「限流明细」面板上的动作。语义只是「让本机立刻重新尝试这个模型」：
/// 标记是本机从上游 429 推断出来的冷却期，清掉之后下一次请求若仍被上游限流，
/// 会再次被标记 —— 所以这个动作是安全的，不需要二次确认。
/// 返回 `{cleared: 清掉的模型数, list}`，前端拿 list 直接刷新。
pub fn clear_rate_limits(state: &ServerState, id: &str, body: &Bytes) -> Response {
    if id.is_empty() {
        return management_error(400, "缺少账号 id");
    }
    let payload = if body.is_empty() {
        Value::Object(Default::default())
    } else {
        match parse_json_body(body, "请求内容不是有效 JSON") {
            Ok(value) => value,
            Err(response) => return response,
        }
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let cleared = match model {
        Some(model) => usize::from(state.store().clear_rate_limit(id, model)),
        None => state.store().clear_all_rate_limits(id),
    };
    if cleared > 0 {
        logging::log(
            "[Accounts]",
            &format!(
                "🧹 已清除账号 {id} 的限流标记（{}）",
                model.map(|value| value.to_string()).unwrap_or_else(|| format!("{cleared} 个模型"))
            ),
        );
    }
    ok_json(json!({ "cleared": cleared, "list": state.store().list_accounts() }))
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
