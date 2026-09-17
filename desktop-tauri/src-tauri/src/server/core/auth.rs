//! 登录态、token 刷新与鉴权头（对照 src/workbuddy-auth.mjs 的会话部分）。
//!
//! 与 Node 版的分工差异：Node 的 `createWorkBuddyAuth` 把「登录态存储 + 上游管理
//! 请求 + 无头登录 + token 刷新 + 头构造」全放在一个模块里；Rust 侧拆三块：
//!   - `auth_http.rs`：传输层（请求发送 / 响应解包 / 鉴权错误类型）
//!   - 本模块：会话读取（队首账号派生）、getStatus、buildAuthHeaders、
//!     token 刷新与临期主动刷新、清会话
//!   - `login.rs`：无头登录流程与登录任务表
//! 拆分的理由是刷新与登录对「取消 / 并发防重」的要求不同，混在一起会让
//! 两者的锁与任务表互相牵扯。
//!
//! ── 三类前缀（务必区分）──────────────────────────────────
//!   1. 登录/账号：`{endpoint}/v2{prefixPath}/...`（国内版 = /v2/plugin/...）
//!   2. 计费/签到：`{endpoint}/v2/...`（不带 prefixPath）
//!   3. LLM 对话：`{endpoint}/v2/chat/completions`（不带 prefixPath）
//!
//! ── 出网（切片 3 已接入）────────────────────────────────
//! Node 版的请求一律走 `proxyFetch`（按账号挂 undici dispatcher），出口缓存
//! 在 `core::egress`；本模块的 token 刷新按**账号自己的** outlet 走
//! （`session.proxy` 解析出的出口），否则会出现「能转发但刷不了 token」。
//! 登录流程（login.rs）没有账号上下文，因此固定直连 —— 与 Node 版一致。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::server::core::account_store::state::json_number;
use crate::server::core::account_store::AccountStore;
use crate::server::core::endpoints::{resolve_edition, Context, ANONYMOUS_HEADERS};
use crate::server::core::proxies::{session_proxy, ResolvedProxy};
use crate::server::logging;

// 传输层与错误类型对本模块的调用方再导出一次：login.rs 与账号路由原来从
// `core::auth` 取这些名字，保留这条路径可以避免它们跟着拆分而改动。
// 登录轮询码 SERVER_CODE_RETRY_FETCH_TOKEN 也在其中 —— login.rs 用它判「token 未就绪」。
// `send_request` 保留导出：登录流程（login.rs）与切片 4 的转发都会用它走直连档。
#[allow(unused_imports)]
pub use crate::server::core::auth_http::{
    send_public_request, send_request, send_request_via, unwrap_public_response,
    unwrap_response, WorkBuddyAuthError, REFRESH_WINDOW_MS, SERVER_CODE_RETRY_FETCH_TOKEN,
};

// ─── 会话与凭证 ─────────────────────────────────────────────

/// 鉴权服务句柄：账号存储 + 默认上下文 + 刷新防重表。
///
/// 防重表 `inflight` 记录「正在刷新的账号 id」：Node 版的刷新是「读账号 →
/// 发请求 → 写回」，两个并发请求会各自发一次上游刷新、后写的覆盖先写的；
/// 更糟的是部分上游会对同一 refreshToken 的并发使用做失效处理。
/// 因此这里用一张占位表把并发收敛成一次真实刷新（后到者拿到同一个结果）。
#[derive(Clone)]
pub struct AuthService {
    store: AccountStore,
    context: Context,
    inflight: Arc<Mutex<HashMap<String, ()>>>,
}

/// 刷新结果（供路由层与登录流程共用）
#[derive(Clone, Debug)]
pub struct RefreshedAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<f64>,
    pub refresh_expires_at: Option<f64>,
}

impl AuthService {
    pub fn new(store: AccountStore, context: Context) -> Self {
        Self { store, context, inflight: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// 默认上下文（未登录态展示与 /api/endpoints 用）
    pub fn default_context(&self) -> &Context {
        &self.context
    }

    /// 账号列表文件（Node 版 `auth.authFile` —— store 模式下就是 accounts.json）。
    ///
    /// Windows 下把分隔符统一成 `\`：Node 的 `path.join` 在 Windows 上就是这么输出的，
    /// 而这个路径会直接显示给用户（面板上的「凭证文件」）并给壳侧打开目录用，
    /// 反斜杠形态才与 Node 版一致。
    pub fn auth_file(&self) -> String {
        normalize_display_path(&self.store.file_string())
    }

    // ── 环境变量凭证（优先级最高）────────────────────────────
    //
    // Node 版：`WORKBUDDY_TOKEN` 存在时直接用它组会话，完全不看账号库。
    // 这是给脚本/CI 用的旁路入口，保持一致。

    fn env_session() -> Option<Value> {
        let token = std::env::var("WORKBUDDY_TOKEN").ok()?.trim().to_string();
        if token.is_empty() {
            return None;
        }
        let refresh = std::env::var("WORKBUDDY_REFRESH_TOKEN")
            .ok()
            .unwrap_or_default();
        let uid = std::env::var("WORKBUDDY_USER_ID").ok().unwrap_or_default();
        Some(json!({
            "auth": with_expires_at(json!({
                "accessToken": token,
                "refreshToken": refresh,
                "tokenType": "Bearer",
            })),
            "account": { "uid": uid },
        }))
    }

    /// 当前可用凭证；临期自动刷新并回写。
    ///
    /// 优先级：环境变量 WORKBUDDY_TOKEN > 账号存储当前账号。
    /// 返回 None 表示没有任何可用凭证（未登录）。
    pub async fn get_current_session(&self) -> Result<Option<Value>, WorkBuddyAuthError> {
        if let Some(session) = Self::env_session() {
            return Ok(Some(session));
        }
        let Some(entry) = self.store.get_current_entry() else {
            return Ok(None);
        };
        let expires_at = entry
            .session
            .get("auth")
            .and_then(|auth| auth.get("expiresAt"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let refresh_token = entry
            .session
            .get("auth")
            .and_then(|auth| auth.get("refreshToken"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if is_token_expiring(expires_at) && !refresh_token.is_empty() {
            logging::verbose(
                "[Auth]",
                &format!("token 临期，自动刷新（账号 {}）…", entry.id),
            );
            // 续期走该账号自己的端点与出口，否则会出现「能转发但刷不了 token」
            let refreshed = self.refresh_account(&entry.id).await?;
            let mut session = entry.session;
            if let Some(auth) = session.get_mut("auth") {
                if let Some(object) = auth.as_object_mut() {
                    object.insert(
                        "accessToken".to_string(),
                        Value::String(refreshed.access_token),
                    );
                    if let Some(token) = refreshed.refresh_token {
                        object.insert("refreshToken".to_string(), Value::String(token));
                    }
                    if let Some(value) = refreshed.expires_at {
                        object.insert("expiresAt".to_string(), json_number(value));
                    }
                    if let Some(value) = refreshed.refresh_expires_at {
                        object.insert("refreshExpiresAt".to_string(), json_number(value));
                    }
                }
            }
            return Ok(Some(session));
        }
        Ok(Some(entry.session))
    }

    /// 会话状态：已登录/未登录两分支的字段逐个对齐 Node 版 `getStatus()`。
    ///
    /// 未登录分支**不含** accountUid/nickname/tokenExpiresAt/canRefresh 等字段 ——
    /// 那些只在已登录分支出现，前端用 `session.nickname || ...` 兜底。
    pub async fn get_status(&self) -> Value {
        let session = self.get_current_session().await.ok().flatten();
        let Some(session) = session else {
            return self.unconfigured_status();
        };
        let expires_at = session
            .get("auth")
            .and_then(|auth| auth.get("expiresAt"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let refresh_token = session
            .get("auth")
            .and_then(|auth| auth.get("refreshToken"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let account = session.get("account").cloned().unwrap_or(Value::Null);
        // 这几个字段 Node 用的是 `|| null` —— 空串也会变成 null（而不仅缺失时）。
        // 界面据此判断「是不是企业账号」（enterpriseId 为 null 就不显示企业标记），
        // 给空串会让所有个人账号都带上一个空的企业标记。
        let nullable_text = |key: &str| -> Value {
            account
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| Value::String(value.to_string()))
                .unwrap_or(Value::Null)
        };
        json!({
            "loggedIn": true,
            "endpoint": session
                .get("endpoint")
                .and_then(Value::as_str)
                .unwrap_or(&self.context.base_url),
            "prefixPath": session
                .get("prefixPath")
                .and_then(Value::as_str)
                .unwrap_or(&self.context.prefix),
            "edition": session
                .get("edition")
                .and_then(Value::as_str)
                .unwrap_or(&self.context.edition),
            "platform": session
                .get("platform")
                .and_then(Value::as_str)
                .unwrap_or(&self.context.platform),
            "accountUid": nullable_text("uid"),
            "nickname": nullable_text("nickname"),
            "accountType": account
                .get("type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("personal"),
            "enterpriseId": nullable_text("enterpriseId"),
            "currentAccountId": self.store.current_account_id(),
            "tokenExpiresAt": if expires_at > 0.0 {
                crate::server::core::account_store::state::json_number(expires_at)
            } else {
                Value::Null
            },
            "canRefresh": !refresh_token.is_empty(),
            "authFile": self.auth_file(),
        })
    }

    /// 未登录态（`/health`、`/api/session` 的 login 字段用它）
    pub fn unconfigured_status(&self) -> Value {
        json!({
            "loggedIn": false,
            "endpoint": self.context.base_url,
            "prefixPath": self.context.prefix,
            "edition": self.context.edition,
            "platform": self.context.platform,
            "authFile": self.auth_file(),
            "reason": "尚未登录或凭证缺失",
        })
    }

    /// 上游配置摘要（对照 workbuddy-upstream-client.mjs 的 `getConfigSummary`）。
    ///
    /// **纯本地**：只查凭证是否存在，不发任何网络请求 —— 所以本切片就能给真值。
    /// 它不做 token 临期刷新（那是 get_status 的职责），否则 /health 会被
    /// 上游网络状况拖慢。
    pub fn get_config_summary(&self) -> Value {
        let Some(entry) = self.store.get_current_entry() else {
            return json!({
                "configured": false,
                "baseUrl": self.context.base_url,
                "unavailableReason": UNCONFIGURED_REASON,
            });
        };
        let auth = entry.session.get("auth").cloned().unwrap_or(Value::Null);
        let expires_at = auth.get("expiresAt").and_then(Value::as_f64).unwrap_or(0.0);
        let refresh_token = auth
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("");
        let account_uid = entry
            .session
            .get("account")
            .and_then(|account| account.get("uid"))
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "configured": true,
            "baseUrl": entry
                .session
                .get("endpoint")
                .and_then(Value::as_str)
                .unwrap_or(&self.context.base_url),
            "authApiBase": self.context.auth_api_base(),
            "chatApiUrl": self.context.chat_api_url(),
            "authSource": if refresh_token.is_empty() {
                "环境变量/静态 token"
            } else {
                "auth.json（可自动刷新）"
            },
            "currentAccountId": account_uid,
            "tokenExpiresAt": if expires_at > 0.0 { json_number(expires_at) } else { Value::Null },
            "canRefresh": !refresh_token.is_empty(),
        })
    }

    /// 转发 LLM 请求所需的完整鉴权头（对齐桌面端 buildAuthHeaders）。
    ///
    /// 逐个复刻，**包括条件头**：X-Enterprise-Id / X-Tenant-Id 只在企业账号给、
    /// X-Domain 只在 auth.domain 存在时给、X-Department-Info 只在有
    /// departmentFullName 时给。少给一个头不会被本地测试发现，但上游会拒绝。
    pub fn build_auth_headers(session: &Value) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        let auth = session.get("auth").cloned().unwrap_or(Value::Null);
        let account = session.get("account").cloned().unwrap_or(Value::Null);

        if let Some(uid) = account.get("uid").and_then(Value::as_str) {
            if !uid.is_empty() {
                headers.push(("X-User-Id".to_string(), uid.to_string()));
            }
        }
        if let Some(token) = auth.get("accessToken").and_then(Value::as_str) {
            if !token.is_empty() {
                headers.push(("Authorization".to_string(), format!("Bearer {token}")));
            }
        }
        if let Some(enterprise_id) = account.get("enterpriseId").and_then(Value::as_str) {
            if !enterprise_id.is_empty() {
                headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
                headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
            }
        }
        if let Some(domain) = auth.get("domain").and_then(Value::as_str) {
            if !domain.is_empty() {
                headers.push(("X-Domain".to_string(), domain.to_string()));
            }
        }
        if let Some(department) = account.get("departmentFullName").and_then(Value::as_str) {
            if !department.is_empty() {
                headers.push(("X-Department-Info".to_string(), department.to_string()));
            }
        }
        headers
    }

    /// 企业扩展头（对照 `enterpriseHeaders`）：总是带 X-Domain
    /// （取 auth.domain，缺失时回退端点 authority）。
    fn enterprise_headers(auth: &Value, context: &Context) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(enterprise_id) = auth
            .get("account")
            .and_then(|account| account.get("enterpriseId"))
            .and_then(Value::as_str)
        {
            if !enterprise_id.is_empty() {
                headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
                headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
            }
        }
        let domain = auth
            .get("domain")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| context.authority());
        if !domain.is_empty() {
            headers.push(("X-Domain".to_string(), domain));
        }
        headers
    }

    // ─── token 刷新 ──────────────────────────────────────────

    /// 单次 token 续期（对照 `refreshSession`）：不读盘、不写回，纯请求。
    ///
    /// ctx 决定向哪个端点续期 —— 多账号（国内版/国际版）必须用该账号自己的
    /// endpoint，否则会拿 A 站点的 refreshToken 打 B 站点。
    ///
    /// `proxy` 是该账号的出网代理（Node 版 `refreshSession(auth, ctx, proxy)`），
    /// 传 None 表示直连 —— 续期与转发共用同一出口，否则会出现
    /// 「能转发但刷不了 token」。
    pub async fn refresh_session(
        &self,
        auth: &Value,
        context: &Context,
        proxy: Option<&ResolvedProxy>,
    ) -> Result<RefreshedAuth, WorkBuddyAuthError> {
        let refresh_token = auth
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("");
        if refresh_token.is_empty() {
            return Err(WorkBuddyAuthError::new(
                "缺少 refreshToken，无法续期，请重新登录",
            ));
        }
        let mut headers = Self::enterprise_headers(auth, context);
        headers.push(("X-Refresh-Token".to_string(), refresh_token.to_string()));
        headers.push(("X-Auth-Refresh-Source".to_string(), "plugin".to_string()));
        let url = context.auth_url("/auth/token/refresh");
        let response =
            send_request_via("POST", &url, Some(&json!({})), &headers, proxy, None).await?;
        let data = unwrap_response(&response, "auth/token/refresh")?;
        let access_token = data
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if access_token.is_empty() {
            return Err(WorkBuddyAuthError::new("token 刷新响应缺少 accessToken"));
        }
        Ok(refresh_result_from(&data))
    }

    /// 按账号 id 刷新并回写（对照 account-routes.mjs 的 `refreshById`）。
    ///
    /// 并发防重：同一账号同时只允许一次真实刷新，后到者直接返回先到者的结果
    /// —— 与 Node 版「单线程 + 无并发刷新」的实际效果一致，但显式化了。
    pub async fn refresh_account(&self, id: &str) -> Result<RefreshedAuth, WorkBuddyAuthError> {
        // 不做「等前一个完成再复用结果」的复杂等待：这里用占位表把重复请求
        // 快速拒掉（返回明确错误），因为刷新本身很快，重复触发通常来自
        // 用户连点或启动维护，报错比静默复用更利于定位问题。
        {
            let mut guard = match self.inflight.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if guard.contains_key(id) {
                return Err(WorkBuddyAuthError::with_status(
                    409,
                    format!("账号 {id} 的 token 正在刷新中，请稍后重试"),
                ));
            }
            guard.insert(id.to_string(), ());
        }
        let result = self.refresh_account_inner(id).await;
        if let Ok(mut guard) = self.inflight.lock() {
            guard.remove(id);
        }
        result
    }

    async fn refresh_account_inner(&self, id: &str) -> Result<RefreshedAuth, WorkBuddyAuthError> {
        let creds = self
            .store
            .get_credentials_by_id(id)
            .ok_or_else(|| WorkBuddyAuthError::with_status(404, "账号不存在"))?;
        if creds.refresh_token.is_empty() {
            return Err(WorkBuddyAuthError::with_status(
                400,
                format!("账号 {} 没有 refreshToken，无法刷新", creds.name),
            ));
        }
        // 按账号自己的端点续期（国内版/国际版不同站点），出口也按账号的代理走
        let context = Context::make(
            Some(&creds.endpoint),
            Some(&creds.prefix_path),
            Some(&creds.platform),
            Some(&creds.edition),
        );
        // 用账号的完整会话形态作为续期入参：enterpriseId / domain 会变成
        // X-Enterprise-Id / X-Domain 条件头，企业账号缺了它们会被上游拒绝
        let session = self
            .store
            .get_session_by_id(id)
            .map(|entry| entry.session)
            .unwrap_or(Value::Null);
        let auth = session.get("auth").cloned().unwrap_or(Value::Null);
        // 续期也走该账号自己的出口（账号没配代理 → 直连）
        let proxy = session_proxy(&session);
        let next = self.refresh_session(&auth, &context, proxy.as_ref()).await?;
        if next.access_token.is_empty() {
            return Err(WorkBuddyAuthError::new("token 刷新响应缺少 accessToken"));
        }
        self.store.update_account_tokens(
            id,
            Some(&next.access_token),
            next.refresh_token.as_deref(),
            next.expires_at,
            next.refresh_expires_at,
        );
        logging::log(
            "[Accounts]",
            &format!(
                "✅ 账号 token 已刷新: {}（{}…）",
                creds.name,
                crate::server::core::account_store::store_util::truncate_text(&creds.uid, 8)
            ),
        );
        Ok(next)
    }

    /// 手动刷新：当前账号（对照 `refreshStoredSession` 的 store 分支）。
    pub async fn refresh_stored_session(&self) -> Result<Value, WorkBuddyAuthError> {
        let Some(entry) = self.store.get_current_entry() else {
            return Err(WorkBuddyAuthError::new("当前没有可刷新的登录态"));
        };
        let refresh_token = entry
            .session
            .get("auth")
            .and_then(|auth| auth.get("refreshToken"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if refresh_token.is_empty() {
            return Err(WorkBuddyAuthError::new("当前没有可刷新的登录态"));
        }
        let refreshed = self.refresh_account(&entry.id).await?;
        let mut session = entry.session;
        if let Some(object) = session.get_mut("auth").and_then(Value::as_object_mut) {
            object.insert("accessToken".to_string(), Value::String(refreshed.access_token));
            if let Some(token) = refreshed.refresh_token {
                object.insert("refreshToken".to_string(), Value::String(token));
            }
            if let Some(value) = refreshed.expires_at {
                object.insert("expiresAt".to_string(), json_number(value));
            }
            if let Some(value) = refreshed.refresh_expires_at {
                object.insert("refreshExpiresAt".to_string(), json_number(value));
            }
        }
        Ok(session)
    }

    // ─── 账号/登录账号接口（登录流程与转发共用）───────────────

    /// 拉取账号列表（对照 `fetchAccounts`）
    pub async fn fetch_accounts(
        &self,
        auth: &Value,
        context: &Context,
        proxy: Option<&ResolvedProxy>,
    ) -> Result<Vec<Value>, WorkBuddyAuthError> {
        let mut headers = Self::enterprise_headers(auth, context);
        let token = auth
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("");
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        let url = context.auth_url("/accounts");
        let response = send_request_via("GET", &url, None, &headers, proxy, None).await?;
        let data = unwrap_response(&response, "accounts")?;
        Ok(data
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// 登录后账号详情（对照 `fetchLoginAccount`，④ GET /v2{prefix}/login/account）
    pub async fn fetch_login_account(
        &self,
        auth: &Value,
        state: &str,
        context: &Context,
        proxy: Option<&ResolvedProxy>,
    ) -> Result<Value, WorkBuddyAuthError> {
        let mut headers = Self::enterprise_headers(auth, context);
        let token = auth
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("");
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        headers.push(("X-No-User-Id".to_string(), "true".to_string()));
        headers.push(("X-No-Enterprise-Id".to_string(), "true".to_string()));
        headers.push(("X-No-Department-Info".to_string(), "true".to_string()));
        let url = context.auth_url(&format!("/login/account?state={}", urlencoding(state)));
        let response = send_request_via("GET", &url, None, &headers, proxy, None).await?;
        unwrap_response(&response, "login/account")
    }

    /// 清空登录态。
    ///
    /// store 模式下「清空」= 删掉当前账号（Node 版 `clearSession` 同样如此）：
    /// 当前账号由优先级派生，删掉它之后自动变成下一个可用账号。
    /// 没有任何可用账号时（无操作可做）返回 false，让调用方能如实写日志。
    pub fn clear_session(&self) -> bool {
        let Some(entry) = self.store.get_current_entry() else {
            logging::verbose("[Auth]", "当前没有可用账号，无需清理登录态");
            return false;
        };
        match self.store.remove_account(&entry.id) {
            Ok(()) => {
                logging::log(
                    "[Auth]",
                    &format!("已清除当前登录态（账号 {} 已从列表移除）", entry.id),
                );
                true
            }
            Err(error) => {
                logging::log("[Auth]", &format!("清理登录态失败: {error}"));
                false
            }
        }
    }
}

/// 把路径分隔符统一成 Windows 形态（`C:/a/b` → `C:\a\b`）。
///
/// `PathBuf::to_string_lossy` 在 Windows 上给的是 `C:\a\b`，但 config_dir 可能是
/// 从环境变量（WORKBUDDY_PROXY_HOME）拼出来的正斜杠形式。这个串会显示给用户、
/// 也会被壳侧用来打开目录，统一成 Node 的 `path.join` 输出形态最不容易出错。
fn normalize_display_path(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// 未登录原因文案（getConfigSummary 用）。
/// Node 版这条是「尚未登录（node server.mjs --login）」；壳内 Rust 版没有
/// 那个命令行入口（登录走 UI 的 /api/session/login/*），用它会指向一个不存在
/// 的命令，因此取与启动横幅一致的措辞。
pub const UNCONFIGURED_REASON: &str = "暂无可用登录态";

/// 在 auth 对象上补 expiresAt / refreshExpiresAt / lastRefreshTime。
/// Node 版 `withExpiresAt`：已有值优先，否则拿 expiresIn（秒）换算。
pub fn with_expires_at(mut auth: Value) -> Value {
    let now = logging::now_ms() as f64;
    let Some(object) = auth.as_object_mut() else {
        return auth;
    };
    let expires_in = object
        .get("expiresIn")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let refresh_expires_in = object
        .get("refreshExpiresIn")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let expires_at = object
        .get("expiresAt")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or_else(|| if expires_in != 0.0 { now + expires_in * 1000.0 } else { 0.0 });
    let refresh_expires_at = object
        .get("refreshExpiresAt")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or_else(|| {
            if refresh_expires_in != 0.0 {
                now + refresh_expires_in * 1000.0
            } else {
                0.0
            }
        });
    let last_refresh = object
        .get("lastRefreshTime")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or(now);
    // 时间戳写成整数形态（json_number）：Node 的 `JSON.stringify` 对
    // `Date.now() + n*1000` 这种整数值不会输出 `.0`，浮点会让 accounts.json
    // 与本机展示的时间戳出现「看起来一样但文本不同」的差异
    object.insert("expiresAt".to_string(), json_number(expires_at));
    object.insert("refreshExpiresAt".to_string(), json_number(refresh_expires_at));
    object.insert("lastRefreshTime".to_string(), json_number(last_refresh));
    auth
}

/// token 是否临期（过期前 5 分钟内视为需要刷新）
pub fn is_token_expiring(expires_at: f64) -> bool {
    if expires_at <= 0.0 {
        return false;
    }
    expires_at - REFRESH_WINDOW_MS <= logging::now_ms() as f64
}

/// 把刷新响应包成 RefreshedAuth（字段缺失时给 None，让调用方保留原值）
fn refresh_result_from(data: &Value) -> RefreshedAuth {
    let enriched = with_expires_at(data.clone());
    RefreshedAuth {
        access_token: enriched
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        refresh_token: enriched
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        expires_at: enriched
            .get("expiresAt")
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0),
        refresh_expires_at: enriched
            .get("refreshExpiresAt")
            .and_then(Value::as_f64)
            .filter(|value| *value > 0.0),
    }
}

/// 查询串值的百分号编码（对照 JS 的 `encodeURIComponent`）。
///
/// 自己实现而不是引 url crate 的 form 编码：`encodeURIComponent` 不转义
/// `!'()*` 之外的可打印 ASCII，而 form 编码会把空格写成 '+'。state 是
/// 服务端下发的随机串，两者都能用，但保持与 Node 完全一致的编码更稳妥。
pub fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 匿名请求头（登录流程用）
pub fn anonymous_headers() -> Vec<(String, String)> {
    ANONYMOUS_HEADERS
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 版本解析的薄封装（登录流程按 edition 决定端点）
pub fn context_for_edition(edition: Option<&str>, endpoint: Option<&str>) -> Context {
    let info = resolve_edition(edition);
    Context::make(
        endpoint.or(Some(info.endpoint)),
        None,
        Some(info.platform),
        Some(info.id),
    )
}
