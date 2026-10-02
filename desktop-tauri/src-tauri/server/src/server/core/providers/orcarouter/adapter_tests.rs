//! OrcaRouter 适配器的验收测试：**两条入口端到端**、目录接线、错误分类、401 处置。
//!
//! ── 与另外三组的分工 ────────────────────────────────────────
//! `login.rs` 测协议、`credentials.rs` 测形状、`catalog_tests.rs` 测过滤；
//! 这里测**接线**：
//!   · 手填与 PKCE 是否落到同一种账号记录形状上、下游（取凭据 → 目录 → 请求体）
//!     是否真的不区分来源；
//!   · 模型下拉（`list_models_for`）是否真的用账号的 Key 打上游、失败时是否
//!     只给出**明确标注**的已验证兜底（而不是自由输入或编造的模型名）；
//!   · PKCE 是否真的走完 authorize → 回调 → 换码 → 落账号（本地假 auth 服务器，
//!     不碰真实上游、不需要人工同意）；
//!   · 401 是否只标记**那一条**账号，且不伪造刷新。
//!
//! ── 假上游的两条纪律 ───────────────────────────────────────
//!   1. 断言「请求打到哪个 path」：换码必须落**认证平面**的 `/api/v1/auth/keys`，
//!      目录必须落**推理平面**的 `/v1/models`，两者绝不可互换；
//!   2. 断言「请求体里带的是什么」：换码体必须含 `S256` 与 verifier，
//!      且 verifier 不得出现在 URL 里。
//!
//! 本文件所有凭据都是测试自己造的假值；真实 Key 的 live 检查在
//! `/work/verification-plan.json` 的 live 条目里，由独立验证器带环境变量跑。

use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use super::*;
use crate::server::core::account_store::orcarouter_accounts::{
    NEEDS_REAUTH_FIELD, ORCAROUTER_PROVIDER_ID,
};
use super::super::AUTHORIZED_APPS_URL;

/// 这些用例都要动**进程级**状态（`ORCA_AUTH_BASE_URL` / `ORCA_API_BASE_URL`
/// 两个环境变量），而 `cargo test` 默认并行跑同一个二进制里的用例 ——
/// 不串行化的话 A 用例注入的假上游会被 B 用例看到（实测踩过：换码打到了
/// 另一个用例的假服务器上，于是「窄授权」用例拿到了一把 scope=api 的 Key）。
/// 全局串行锁是本文件唯一正确的做法：环境变量没有「每个用例一份」的形态。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取串行锁（中毒不致命：锁里只有环境变量，没有需要保持一致的内存状态）。
pub(super) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

/// 测试用假 Key（形状像真的，值不是；绝不是真实凭据）。
const FAKE_KEY: &str = "sk-orca-testtesttesttesttesttesttesttest";
/// PKCE 那条路换回来的假 Key。与 [`FAKE_KEY`] **刻意不同**：本家按 Key 派生
/// 账号 id，同一把 Key 会合并成一条记录；两条入口要产出**两条**账号才能证明
/// 「下游不区分来源」。
const PKCE_KEY: &str = "sk-orca-pkcepkcepkcepkcepkcepkcepkcepkce";

/// 假上游上被记录下来的请求：`(method, path+query, body)`。
type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;
/// 起一个本地假上游，返回 `(base_url, 请求记录)`。
///
/// 认证平面与推理平面挂在**同一个** base 上（测试同时注入
/// `ORCA_AUTH_BASE_URL` 与 `ORCA_API_BASE_URL`），于是「哪个平面被打了哪个
/// path」在同一份记录里可查 —— 这正是规范里最要紧的那条不变量。
fn fake_router(key_to_return: &'static str) -> (Router, Seen) {
    let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let auth_seen = seen.clone();
    let models_seen = seen.clone();
    let chat_seen = seen.clone();
    let app = Router::new()
        .route(
            "/auth",
            get(|| async { axum::response::Html("<html>consent</html>") }),
        )
        .route(
            "/api/v1/auth/keys",
            post(move |axum::Json(body): axum::Json<Value>| {
                let seen = auth_seen.clone();
                async move {
                    record(&seen, "POST", "/api/v1/auth/keys", body.clone());
                    axum::Json(json!({
                        "key": key_to_return,
                        "user_id": "u-42",
                        "scope": "api",
                    }))
                }
            }),
        )
        .route(
            "/v1/models",
            get(move |uri: axum::http::Uri| {
                let seen = models_seen.clone();
                async move {
                    let path = match uri.query() {
                        Some(query) => format!("{}?{query}", uri.path()),
                        None => uri.path().to_string(),
                    };
                    record(&seen, "GET", &path, Value::Null);
                    axum::Json(json!({
                        "data": [
                            {
                                "id": "vendor/live-text",
                                "name": "Vendor: Live Text",
                                "context_length": 128000,
                                "architecture": { "input_modalities": ["text"] },
                                "supported_endpoint_types": ["openai"],
                            },
                            {
                                "id": "vendor/live-vision",
                                "name": "Vendor: Live Vision",
                                "context_length": 200000,
                                "architecture": { "input_modalities": ["text", "image"] },
                                "supported_endpoint_types": ["openai"],
                            },
                            {
                                "id": "vendor/live-embed",
                                "supported_endpoint_types": ["embeddings"],
                            },
                        ]
                    }))
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let seen = chat_seen.clone();
                async move {
                    record(&seen, "POST", "/v1/chat/completions", body.clone());
                    let authorization = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    axum::Json(json!({
                        "id": "chatcmpl-1",
                        "authorization_seen": authorization,
                        "choices": [{ "message": { "role": "assistant", "content": "pong" } }],
                    }))
                }
            }),
        );
    (app, seen)
}

fn record(seen: &Seen, method: &str, path: &str, body: Value) {
    seen.lock()
        .unwrap_or_else(|error| error.into_inner())
        .push((method.to_string(), path.to_string(), body));
}

/// 起服务并返回 `base`。
async fn spawn_upstream(app: Router) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("假上游监听");
    let address = listener.local_addr().expect("本地地址");
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    (format!("http://{address}"), tx)
}

/// 装一条 OrcaRouter 账号的临时库（真 SQLite；守卫必须持有到用例结束）。
fn store_with_key(label: &str, key: &str) -> (AccountStore, crate::server::db::test_temp::TempDb) {
    let (db, guard) = crate::server::db::test_temp::TempDb::open(label);
    let store = AccountStore::with_db(Some(db));
    save_manual_credentials(&store, &json!({ "apiKey": key }), Some("测试账号"))
        .expect("账号要能落库");
    (store, guard)
}

/// 本家名下**公开形态**的账号列表（顺序即优先级顺序）。
fn accounts_of(store: &AccountStore) -> Vec<Value> {
    store.accounts_for_provider("orcarouter")
}

fn field<'a>(account: &'a Value, key: &str) -> &'a str {
    account.get(key).and_then(Value::as_str).unwrap_or("")
}

fn model_ids(payload: &Value) -> Vec<String> {
    payload
        .get("models")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn seen_has(seen: &Seen, method: &str, path_prefix: &str) -> bool {
    seen.lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|(m, path, _)| m == method && path.starts_with(path_prefix))
}

fn seen_body(seen: &Seen, path: &str) -> Option<Value> {
    seen.lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .find(|(_, recorded, _)| recorded == path)
        .map(|(_, _, body)| body.clone())
}

/// 设好两个 origin 覆盖；调用方用例结束要 [`clear_origins`]。
fn set_origins(base: &str) {
    std::env::set_var("ORCA_AUTH_BASE_URL", base);
    std::env::set_var("ORCA_API_BASE_URL", format!("{base}/v1"));
}

fn clear_origins() {
    std::env::remove_var("ORCA_AUTH_BASE_URL");
    std::env::remove_var("ORCA_API_BASE_URL");
}

// ─── 双认证：两条入口 → 同一种记录、下游不区分来源 ─────────────────

#[tokio::test]
async fn manual_and_pkce_land_the_same_record_and_the_downstream_ignores_the_source() {
    let _env = env_guard();
    let (app, seen) = fake_router(PKCE_KEY);
    let (base, _shutdown) = spawn_upstream(app).await;
    set_origins(&base);

    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-dual-auth");
    let store = AccountStore::with_db(Some(db));

    // ① 手填入口
    let manual = save_manual_credentials(&store, &json!({ "apiKey": FAKE_KEY }), Some("手填"))
        .unwrap_or_else(|error| panic!("手填要能落库：{}", error.message));
    assert_eq!(field(&manual, "source"), SOURCE_MANUAL);

    // ② PKCE 入口：本地假 auth 服务器走完 authorize → 回调 → 换码 → 落账号
    let pkce = login_via_fake_auth(&store).await;
    assert_eq!(field(&pkce, "source"), SOURCE_PKCE);

    // 两条路产出**同一种记录形状**：公开形态的键集合逐字相同
    //（取值不同是应当的 —— 它们本来就是两条不同的凭据）
    let keys = |value: &Value| -> Vec<String> {
        let mut list: Vec<String> = value
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default();
        list.sort();
        list
    };
    assert_eq!(keys(&manual), keys(&pkce), "两条入口的公开形态必须同形");
    // 形状里**没有** apiKey（密钥绝不出后端）
    assert!(manual.get("apiKey").is_none() && pkce.get("apiKey").is_none());
    assert_eq!(field(&manual, "provider"), ORCAROUTER_PROVIDER_ID);
    assert_eq!(field(&pkce, "provider"), ORCAROUTER_PROVIDER_ID);
    // 只能看到尾号（三条字符的展示尾号，绝不是全量）
    for account in [&manual, &pkce] {
        let tail = field(account, "tokenTail");
        assert!(!tail.is_empty() && tail.len() < 16);
        assert!(!FAKE_KEY.contains("") || !tail.is_empty());
    }

    // PKCE 真的走了认证平面、且请求体里是 S256 + verifier
    let exchange = seen_body(&seen, "/api/v1/auth/keys").expect("必须有换码请求");
    assert_eq!(exchange.get("code_challenge_method").and_then(Value::as_str), Some("S256"));
    assert_eq!(exchange.get("code").and_then(Value::as_str), Some("fake-one-time-code"));
    let verifier = exchange.get("code_verifier").and_then(Value::as_str).unwrap_or("");
    assert!(verifier.len() >= 43, "verifier 必须是新生成的 32 字节 base64url");
    {
        let guard = seen.lock().unwrap_or_else(|error| error.into_inner());
        for (_, path, _) in guard.iter() {
            assert!(!path.contains(verifier), "verifier 不得出现在 URL 里：{path}");
        }
    }
    assert!(!seen_has(&seen, "GET", "/v1/models"), "登录过程中不该顺手拉目录");

    // 下游不区分来源：两条账号各自取回自己那把 Key，看到的目录完全一致
    assert_eq!(accounts_of(&store).len(), 2, "两把不同的 Key = 两条账号");
    assert_eq!(
        store
            .orcarouter_api_key(field(&manual, "id"))
            .expect("读取不应报错")
            .expect("账号存在")
            .api_key,
        FAKE_KEY
    );
    assert_eq!(
        store
            .orcarouter_api_key(field(&pkce, "id"))
            .expect("读取不应报错")
            .expect("账号存在")
            .api_key,
        PKCE_KEY,
        "PKCE 换回来的是换码响应里那把 Key，不是别的"
    );
    let via_manual = list_models_for(&store, field(&manual, "id"), "text", None).await;
    let via_pkce = list_models_for(&store, field(&pkce, "id"), "text", None).await;
    assert_eq!(via_manual["source"], "live");
    assert_eq!(via_pkce["source"], "live");
    assert_eq!(via_manual["models"], via_pkce["models"]);
    // 响应里不得出现任何凭据（两条都查）
    for body in [&via_manual, &via_pkce] {
        let text = body.to_string();
        assert!(!text.contains(FAKE_KEY) && !text.contains(PKCE_KEY), "目录响应里不得有凭据");
    }

    clear_origins();
}

/// 走完一轮完整 PKCE 登录，复用**产品代码**的每一步：
/// `build_login_url`（真生成 verifier/state 并登记 pending）→ `parse_callback`
/// （常量时间比对 state）→ `exchange_login_code`（真发换码请求、真落账号）。
async fn login_via_fake_auth(store: &AccountStore) -> Value {
    login::set_loopback_port(51733);
    let (auth_url, state) = ORCAROUTER_ADAPTER
        .build_login_url()
        .expect("端口已知时应当能生成授权地址");
    assert!(auth_url.contains("code_challenge_method=S256"));
    let callback = login::callback_url().expect("端口已登记");
    let encoded: String = callback
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            c.to_string()
        } else {
            format!("%{:02X}", c as u32)
        })
        .collect();
    assert!(
        auth_url.contains(&format!("callback_url={encoded}")),
        "授权地址里的回调必须是本进程的 loopback 地址：{auth_url}"
    );
    let mut query = std::collections::HashMap::new();
    query.insert("code".to_string(), "fake-one-time-code".to_string());
    query.insert("state".to_string(), state.clone());
    let code = login::parse_callback(&query, &state).expect("state 匹配时应当接受");
    let account_id = ORCAROUTER_ADAPTER
        .exchange_login_code(store, &code, &state)
        .await
        .expect("换码并落账号");
    accounts_of(store)
        .into_iter()
        .find(|item| field(item, "id") == account_id)
        .expect("刚落的账号要能在列表里找到")
}

// ─── PKCE 的失败路径 ────────────────────────────────────────────

#[test]
fn state_mismatch_and_replay_are_terminal_and_do_not_hang() {
    let _env = env_guard();
    login::set_loopback_port(51734);
    let (_, state) = ORCAROUTER_ADAPTER.build_login_url().expect("授权地址");
    // 攻击者把自己的 code 配一个不匹配的 state 塞进来 → 403，且 code 未被使用
    let mut query = std::collections::HashMap::new();
    query.insert("code".to_string(), "attacker-code".to_string());
    query.insert("state".to_string(), "not-our-state".to_string());
    let error = login::parse_callback(&query, &state).expect_err("state 不匹配必须拒绝");
    assert_eq!(error.status_code, 403);
    // 本进程登记的那一轮仍在表里（拒绝不消耗它）
    assert_eq!(login::pending_count(), 1);
    // 正确回调取走即删除（授权码一次性）
    let mut good = std::collections::HashMap::new();
    good.insert("code".to_string(), "good".to_string());
    good.insert("state".to_string(), state.clone());
    assert_eq!(login::parse_callback(&good, &state).expect("匹配"), "good");
    assert!(login::take_pending(&state).is_some());
    assert!(login::take_pending(&state).is_none(), "取走即删除，重放拿不到 verifier");
}

#[tokio::test]
async fn a_dropped_or_unknown_state_is_refused_and_cleans_up() {
    let _env = env_guard();

    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-unknown-state");
    let store = AccountStore::with_db(Some(db));
    let error = ORCAROUTER_ADAPTER
        .exchange_login_code(&store, "some-code", "never-issued-state")
        .await
        .expect_err("不是本进程发起的 state 必须拒绝");
    assert_eq!(error.status_code, 404);
    assert!(error.message.contains("已取消或已过期"));
    assert!(accounts_of(&store).is_empty(), "拒绝的登录不得落任何账号");
}

#[tokio::test]
async fn exchange_rejections_are_terminal_and_leave_no_account() {
    let _env = env_guard();

    // 假 auth 服务器对换码回 403（code 过期 / 已用过 / verifier 不符）
    let app = Router::new().route(
        "/auth",
        get(|| async { axum::response::Html("<html>consent</html>") }),
    ).route(
        "/api/v1/auth/keys",
        post(|| async { (axum::http::StatusCode::FORBIDDEN, "{\"error\":\"code expired\"}") }),
    );
    let (base, _shutdown) = spawn_upstream(app).await;
    set_origins(&base);
    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-exchange-403");
    let store = AccountStore::with_db(Some(db));
    login::set_loopback_port(51735);
    let (_, state) = ORCAROUTER_ADAPTER.build_login_url().expect("授权地址");
    let error = ORCAROUTER_ADAPTER
        .exchange_login_code(&store, "expired-code", &state)
        .await
        .expect_err("403 必须报错");
    assert_eq!(error.status_code, 403);
    assert!(error.message.contains("授权码已失效"));
    assert!(!error.message.contains("expired-code"), "错误文案不得回显授权码");
    assert!(accounts_of(&store).is_empty(), "失败的换码不得落账号");
    clear_origins();
}

#[tokio::test]
async fn exchange_429_explains_the_24h_key_limit() {
    let _env = env_guard();

    let app = Router::new().route(
        "/auth",
        get(|| async { axum::response::Html("<html>consent</html>") }),
    ).route(
        "/api/v1/auth/keys",
        post(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"limit\"}") }),
    );
    let (base, _shutdown) = spawn_upstream(app).await;
    set_origins(&base);
    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-exchange-429");
    let store = AccountStore::with_db(Some(db));
    login::set_loopback_port(51736);
    let (_, state) = ORCAROUTER_ADAPTER.build_login_url().expect("授权地址");
    let error = ORCAROUTER_ADAPTER
        .exchange_login_code(&store, "code", &state)
        .await
        .expect_err("429 必须报错");
    assert_eq!(error.status_code, 429);
    assert!(error.message.contains("10 把"), "要说清 24 小时限额");
    assert!(accounts_of(&store).is_empty());
    clear_origins();
}

#[tokio::test]
async fn a_scope_downgrade_is_reported_instead_of_silently_accepted() {
    let _env = env_guard();

    let app = Router::new().route(
        "/auth",
        get(|| async { axum::response::Html("<html>consent</html>") }),
    ).route(
        "/api/v1/auth/keys",
        post(|| async {
            axum::Json(json!({ "key": FAKE_KEY, "user_id": "u-1", "scope": "chat" }))
        }),
    );
    let (base, _shutdown) = spawn_upstream(app).await;
    set_origins(&base);
    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-scope-downgrade");
    let store = AccountStore::with_db(Some(db));
    login::set_loopback_port(51737);
    let (_, state) = ORCAROUTER_ADAPTER.build_login_url().expect("授权地址");
    let error = ORCAROUTER_ADAPTER
        .exchange_login_code(&store, "code", &state)
        .await
        .expect_err("窄授权必须如实拒绝");
    assert_eq!(error.status_code, 403);
    assert!(error.message.contains("chat"), "要说出实际授予的范围");
    assert!(error.message.contains("api"), "要说出需要什么范围");
    assert!(accounts_of(&store).is_empty(), "窄授权不得落账号（那会让推理必然 403）");
    clear_origins();
}

#[tokio::test]
async fn a_network_failure_ends_safely_with_an_actionable_message() {
    let _env = env_guard();

    // 指到一个必然连不上的端口：换码请求失败，不得挂起、不得泄露 verifier
    std::env::set_var("ORCA_AUTH_BASE_URL", "http://127.0.0.1:9");
    std::env::set_var("ORCA_API_BASE_URL", "http://127.0.0.1:9/v1");
    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-exchange-network");
    let store = AccountStore::with_db(Some(db));
    login::set_loopback_port(51738);
    let (_, state) = ORCAROUTER_ADAPTER.build_login_url().expect("授权地址");
    let error = ORCAROUTER_ADAPTER
        .exchange_login_code(&store, "code", &state)
        .await
        .expect_err("网络失败必须报错");
    assert!(error.status_code >= 500, "网络失败按上游不可达处理");
    assert!(error.message.contains("网络错误"));
    assert!(!error.message.contains("code_verifier"));
    assert!(accounts_of(&store).is_empty());
    clear_origins();
}

// ─── 真实拉目录：Key 只在服务端，失败只给明确标注的兜底 ────────────

#[tokio::test]
async fn model_dropdown_comes_from_the_api_and_never_falls_back_to_free_text() {
    let _env = env_guard();

    let (app, seen) = fake_router(FAKE_KEY);
    let (base, _shutdown) = spawn_upstream(app).await;
    set_origins(&base);
    let (store, _guard) = store_with_key("orca-catalog-live", FAKE_KEY);

    let live = list_models_for(&store, "", "text", None).await;
    assert_eq!(live["source"], "live", "有 Key 时必须以 live 目录为权威");
    assert_eq!(live["degraded"], false);
    assert_eq!(live["secret_masked"], true);
    assert_eq!(live["catalogSource"], format!("{base}/v1/models"));
    assert_eq!(live["authOrigin"], base, "auth origin 与 api origin 是两个字段");
    assert_eq!(live["apiOrigin"], format!("{base}/v1"));
    assert_eq!(model_ids(&live), vec!["vendor/live-text", "vendor/live-vision"]);
    assert_eq!(live["count"], 2);
    assert_eq!(live["liveCount"], 2);
    // live 成功时**不掺种子**：那两个 id 不是种子里的任何一个
    let seed_ids: Vec<String> = catalog::seed_catalog()
        .iter()
        .map(catalog::item_id)
        .collect();
    for id in model_ids(&live) {
        assert!(!seed_ids.contains(&id), "live 结果里不得混入种子条目");
    }
    assert!(!live.to_string().contains(FAKE_KEY), "目录响应里不得有凭据");
    assert!(seen_has(&seen, "GET", "/v1/models?capability=chat"));

    // 多模态：只留显式声明 image 输入的 chat 模型（fail closed）
    let vision = list_models_for(&store, "", "multimodal", Some("image")).await;
    assert_eq!(vision["source"], "live");
    assert_eq!(model_ids(&vision), vec!["vendor/live-vision"]);

    // 向量化：按 embeddings 端点类型挑（不进文本下拉）
    let embedding = list_models_for(&store, "", "embedding", None).await;
    assert_eq!(embedding["source"], "live");
    assert_eq!(model_ids(&embedding), vec!["vendor/live-embed"]);
    assert!(seen_has(&seen, "GET", "/v1/models?capability=embedding"));

    // 没有目录证据的用途 → 空列表（而不是把 chat 模型混进去）
    let image = list_models_for(&store, "", "image", None).await;
    assert!(model_ids(&image).is_empty());
    let rerank = list_models_for(&store, "", "rerank", None).await;
    assert!(model_ids(&rerank).is_empty());
    clear_origins();
}

#[tokio::test]
async fn a_failed_catalog_yields_only_the_verified_seed_marked_degraded() {
    let _env = env_guard();

    std::env::set_var("ORCA_AUTH_BASE_URL", "http://127.0.0.1:9");
    std::env::set_var("ORCA_API_BASE_URL", "http://127.0.0.1:9/v1");
    let (store, _guard) = store_with_key("orca-catalog-fallback", FAKE_KEY);

    let payload = list_models_for(&store, "", "text", None).await;
    assert_eq!(payload["source"], "fallback");
    assert_eq!(payload["degraded"], true, "兜底必须明确标注降级");
    assert!(payload["note"].as_str().unwrap_or("").contains("目录拉取失败"));
    let ids = model_ids(&payload);
    assert!(!ids.is_empty());
    let seed_ids: Vec<String> = catalog::seed_catalog().iter().map(catalog::item_id).collect();
    for id in &ids {
        assert!(seed_ids.contains(id), "{id} 不在已验证种子里（不许编造模型名）");
    }
    // 兜底也保真元数据：GPT-5.5 的思考档位与上下文
    let gpt = payload["models"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == "openai/gpt-5.5"))
        .expect("种子里的 gpt-5.5");
    assert_eq!(gpt["supportsReasoning"], true);
    assert_eq!(gpt["maxOutputTokens"], 128000);
    assert_eq!(
        gpt["reasoningEfforts"],
        json!(["low", "medium", "high", "xhigh"])
    );
    assert!(!payload.to_string().contains(FAKE_KEY));

    // 非文本用途没有已验证的种子 → 空列表 + 原因（而不是编造）
    let image = list_models_for(&store, "", "image", None).await;
    assert_eq!(image["source"], "fallback-empty");
    assert!(model_ids(&image).is_empty());
    assert!(image["note"].as_str().unwrap_or("").contains("空列表"));
    clear_origins();
}

#[tokio::test]
async fn catalog_without_an_account_says_how_to_add_one_instead_of_guessing() {
    let _env = env_guard();

    let (db, _guard) = crate::server::db::test_temp::TempDb::open("orca-catalog-empty");
    let store = AccountStore::with_db(Some(db));
    let payload = list_models_for(&store, "", "text", None).await;
    assert_eq!(payload["source"], "unavailable");
    assert!(model_ids(&payload).is_empty());
    assert!(payload["note"].as_str().unwrap_or("").contains("请先添加"));
    // 未知用途：明确拒绝，不悄悄退回文本口径（那会把图片模型混进对话下拉）
    let bad = list_models_for(&store, "", "nonsense", None).await;
    assert_eq!(bad["source"], "invalid");
    // 多模态但没给模态：同样是「不认识的用途」
    let no_modality = list_models_for(&store, "", "multimodal", None).await;
    assert_eq!(no_modality["source"], "invalid");
    // 点名一条不存在的账号：如实报错，**不回落队首**（那会变成「选了 A、用了 B」）
    let (store2, _guard2) = store_with_key("orca-catalog-named", FAKE_KEY);
    let missing = list_models_for(&store2, "orcarouter-doesnotexist", "text", None).await;
    assert_eq!(missing["source"], "unavailable");
    assert!(missing["note"]
        .as_str()
        .unwrap_or("")
        .contains("不存在或已被删除"));
}

// ─── 错误分类与 401 的终端处置 ──────────────────────────────────

#[test]
fn upstream_errors_map_to_the_documented_classes() {
    let _env = env_guard();
    match ORCAROUTER_ADAPTER
        .classify_error(401, &json!({ "error": { "message": "invalid key" } }))
    {
        UpstreamErrorClass::TokenExpired { message } => {
            assert!(message.contains("401"));
            assert!(message.contains(AUTHORIZED_APPS_URL), "要给出吊销入口");
        }
        other => panic!("401 必须是 TokenExpired，实际 {other:?}"),
    }
    // 403（model_access_denied）：**确定性拒绝**，原样透出，不当作限额冷却
    match ORCAROUTER_ADAPTER.classify_error(
        403,
        &json!({ "error": { "code": "model_access_denied", "message": "no access to model" } }),
    ) {
        UpstreamErrorClass::Fatal { status, message, .. } => {
            assert_eq!(status, 403);
            assert!(message.contains("no access") || message.contains("model_access_denied"));
        }
        other => panic!("403 必须是 Fatal，实际 {other:?}"),
    }
    // 429：限额（换账号 / 稍后重试有意义），且不编造恢复时间
    match ORCAROUTER_ADAPTER
        .classify_error(429, &json!({ "error": { "message": "rate limited" } }))
    {
        UpstreamErrorClass::QuotaLimited { status, reset_at, .. } => {
            assert_eq!(status, 429);
            assert!(reset_at.is_none(), "上游不带恢复时间时不得编造");
        }
        other => panic!("429 必须是 QuotaLimited，实际 {other:?}"),
    }
}

#[test]
fn adapter_capabilities_are_the_honest_set() {
    let _env = env_guard();
    assert!(ORCAROUTER_ADAPTER.supports_web_login());
    assert!(ORCAROUTER_ADAPTER.supports_model_refresh());
    assert!(ORCAROUTER_ADAPTER.refresh_uses_account(), "目录是账号级的");
    assert!(ORCAROUTER_ADAPTER.sse_model_rewrite(), "上游回的是内部承载名");
    assert!(!ORCAROUTER_ADAPTER.supports_refresh(), "本家没有续期手段");
    assert!(!ORCAROUTER_ADAPTER.credentials_expiring(&AccountStore::with_db(None), "x"));
    assert!(!ORCAROUTER_ADAPTER.supports_default_model(), "注入 workbuddy 的默认模型名只会 404");
    assert!(!ORCAROUTER_ADAPTER.allows_anonymous_default_session());
    assert!(!ORCAROUTER_ADAPTER.env_credentials_present());
    assert_eq!(ORCAROUTER_ADAPTER.kind(), ProviderKind::OrcaRouter);
}

#[tokio::test]
async fn a_revoked_key_is_marked_on_that_account_only_and_never_fakes_a_refresh() {
    let _env = env_guard();

    let (store, _guard) = store_with_key("orca-revoked", FAKE_KEY);
    // 再加一条**别的**账号（不同 Key），用来验证标记不会串味
    save_manual_credentials(
        &store,
        &json!({ "apiKey": "sk-orca-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" }),
        Some("另一条"),
    )
    .expect("第二条要能落库");
    let accounts = accounts_of(&store);
    assert_eq!(accounts.len(), 2);
    let target_id = field(&accounts[0], "id").to_string();
    let other_id = field(&accounts[1], "id").to_string();
    assert_ne!(target_id, other_id);

    // 「401 之后的处置」= 编排层调 refresh_access_token（它只在 401 之后被调）
    let error = ORCAROUTER_ADAPTER
        .refresh_access_token(&store, &target_id)
        .await
        .expect_err("本家不得伪造刷新");
    assert_eq!(error.status_code, 401);
    assert!(error.message.contains("没有刷新机制"));
    assert!(!error.message.contains(FAKE_KEY));

    let accounts = accounts_of(&store);
    let marked = |id: &str| {
        accounts
            .iter()
            .find(|item| field(item, "id") == id)
            .and_then(|item| item.get(NEEDS_REAUTH_FIELD))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    assert!(marked(&target_id), "被拒的账号必须标为需要重新授权");
    assert!(!marked(&other_id), "别的账号不得被牵连");
    // 公开形态里能读到这份标记（界面据此提示重连）
    let target = accounts
        .iter()
        .find(|item| field(item, "id") == target_id)
        .expect("目标账号");
    assert_eq!(target.get(NEEDS_REAUTH_FIELD).and_then(Value::as_bool), Some(true));

    // 重新连上（换一把新 Key）：新记录不带旧标记，旧记录也不影响它
    let fresh = save_manual_credentials(
        &store,
        &json!({ "apiKey": "sk-orca-cccccccccccccccccccccccccccccccc" }),
        Some("重连"),
    )
    .expect("重连要能落库");
    assert_eq!(fresh.get(NEEDS_REAUTH_FIELD).and_then(Value::as_bool), Some(false));
    assert_ne!(
        field(&fresh, "id"),
        target_id.as_str(),
        "换 Key = 新记录（id 由 Key 派生）：旧 401 天然污染不到新凭据"
    );

    // 空的 account_id 不猜：什么都不标
    assert!(!ORCAROUTER_ADAPTER
        .refresh_access_token(&store, "")
        .await
        .is_ok());
    // 别家的账号 id 不会被标记
    let (db, _guard2) = crate::server::db::test_temp::TempDb::open("orca-foreign-id");
    let foreign = AccountStore::with_db(Some(db));
    assert!(foreign.mark_orcarouter_needs_reauth("workbuddy-not-real").is_ok());
    assert!(!foreign
        .mark_orcarouter_needs_reauth("workbuddy-not-real")
        .expect("查询不应报错"));
}

#[tokio::test]
async fn chat_request_targets_the_api_origin_with_a_bearer_key() {
    let _env = env_guard();

    set_origins("https://api.orcarouter.ai");
    let plan = ORCAROUTER_ADAPTER
        .build_chat_request(
            &json!({ "auth": { "accessToken": FAKE_KEY } }),
            &json!({ "model": "vendor/live-text", "stream": true }),
            &axum::http::HeaderMap::new(),
        )
        .unwrap_or_else(|error| panic!("构造请求失败：{}", error.message));
    clear_origins();
    assert_eq!(plan.url, "https://api.orcarouter.ai/v1/chat/completions");
    assert!(!plan.url.contains("www.orcarouter.ai"), "推理绝不指向认证 origin");
    let names: Vec<&str> = plan.headers.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.iter().any(|name| name.eq_ignore_ascii_case("authorization")));
    assert!(!names.iter().any(|name| name.contains("HTTP-Referer")), "本家不读来源标记头");
    let authorization = plan
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone())
        .expect("必须带 Authorization");
    assert_eq!(authorization, format!("Bearer {FAKE_KEY}"));
    // 模型名逐字透传（保留 vendor/model 命名空间）
    assert_eq!(plan.body.get("model").and_then(Value::as_str), Some("vendor/live-text"));

    // 没有 Key 的账号 → 401 并说明怎么修
    let missing = match ORCAROUTER_ADAPTER
        .build_chat_request(&json!({ "auth": {} }), &json!({}), &axum::http::HeaderMap::new())
    {
        Ok(_) => panic!("缺 Key 必须拒绝"),
        Err(error) => error,
    };
    assert_eq!(missing.status_code, 401);
    assert!(missing.message.contains("缺少 API Key"));

    // ensure_access_token 走的是同一条记录（两条入口共用）
    let (store, _guard) = store_with_key("orca-ensure-token", FAKE_KEY);
    let accounts = accounts_of(&store);
    let token = ORCAROUTER_ADAPTER
        .ensure_access_token(&store, field(&accounts[0], "id"))
        .await
        .expect("取 Key");
    assert_eq!(token, FAKE_KEY);
    let none = ORCAROUTER_ADAPTER
        .ensure_access_token(&AccountStore::with_db(None), "")
        .await;
    // 库里取不到凭据时报错（不 panic、不回落）
    assert!(none.is_err());
}
