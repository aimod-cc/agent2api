//! WorkBuddy 适配器（Agent2API 改造 W2b-T3，架构文档 §4.2 末段）：
//! 把改造前散在 `upstream/request.rs` / `auth.rs` / `models` 里的
//! 「workbuddy 这一家长什么样」的知识收进一个 provider 实现。
//!
//! ── 为什么是「搬」而不是「重写」──────────────────────────────
//! 契约文档把这条写成了硬要求：现有 workbuddy 逻辑整体搬进本模块，
//! 不允许在 `upstream/` 里残留 provider 分支。因此本文件的每个函数都能
//! 对应回改造前的原始实现（注释里逐条标注来源），**语义逐字保持**：
//!   - 头集合与 URL        ← `upstream::request::{chat_headers, chat_completions_url}`
//!   - system 注入         ← `upstream::request::ensure_leading_system_message`
//!   - 429 / 6004 判定     ← `upstream::request::is_quota_limit_error` + `errors::parse_quota_reset_at`
//!   - 11-128 退避建议      ← 本文件 `RATE_LIMIT_CODE`（次数 / 间隔走全局重试设置）
//!   - token 临期刷新      ← `auth::{get_current_session, is_token_expiring, refresh_account}`
//!   - 模型清单            ← `core::models::global_catalog(region)`
//!
//! ── 出站归一化（2026-09 新增，子模块 `normalize`）──────────────
//! 上表里「system 注入」那一行的**职责被拆开了一部分**：`developer→system`
//! 角色归一现在发生在 `normalize::normalize_outbound` 里，且**必须先于**
//! system 兜底注入（顺序理由见 `build_chat_request` 的函数头）。同一管线还做
//! tool_choice / image_url / max_tokens / tool 配对修复与前缀缓存键注入。
//! 那是从参考项目 workbuddy2api 移植的本家形态适配，与「搬」进本模块的既有
//! 逻辑不同源 —— 见 `normalize.rs` 模块头。
//!
//! ── 国内版 / 国际版 = 两个实例（2026-10 拆分）─────────────────
//! 适配器持有一个 [`Region`]：两端协议完全相同，差别只在站点与客户端身份
//! （UA / platform / 产品名），以及**各自独立的模型目录与缓存槽**。
//! 因此本结构按地区参数化，两个静态实例由 `adapter_for` 按 kind 给出 ——
//! 与 `autoclaw::adapter` / `zcode::adapter` / `accio::adapter` 同一形态。
//! 拆家的完整理由见 `workbuddy::region` 的模块头（issue #74 / #89）。
//!
//! ── 默认登录态（`WORKBUDDY_TOKEN` 旁路）─────────────────────
//! workbuddy 独有的「一个账号都没有时也能转发」能力（脚本 / CI 用户）：
//! 反映在 `allows_anonymous_default_session()` 与 `ensure_access_token`
//! 的空 account_id 分支上。这是**行为兼容**的一部分，不是可选优化 ——
//! 去掉它会让 `WORKBUDDY_TOKEN` 用户的 `/v1/chat/completions` 直接 401。
//! 拆家后变量按地区分开（国内 `WORKBUDDY_TOKEN`、国际 `WORKBUDDY_INTL_TOKEN`，
//! 旧组合 `WORKBUDDY_EDITION=intl` + `WORKBUDDY_TOKEN` 仍可用），见
//! `Region::env_token`。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件在对话链路上，**绝不** unwrap/expect/panic：所有取值都走
//! `Option` 链与 `unwrap_or`，序列化失败一律转成 GatewayError。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth::{is_token_expiring, AuthService};
use crate::server::core::models::global_catalog;
use crate::server::errors::GatewayError;

use super::super::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, RetryAdvice, UpstreamErrorClass,
};
use super::super::content_block;
use super::super::ProviderKind;
use super::region::Region;
use super::normalize;

/// 上游错误码 11-128（历史文案：Illegal API invocation from an unapproved channel）。
///
/// 实测语义：多为提示词命中上游敏感词审核而被拦截（并非单纯的频率风控）。
/// 常量名沿用 Node 版的历史命名，不要据此理解成「频率限制」。
/// 拦截会持续一小段时间，期间客户端失败自动重试会形成重试风暴并给拦截续期，
/// 因此命中 11-128 时值得退避重试 —— 次数与间隔统一走设置页的「请求重试」
/// （历史硬编码是 10 秒 / 25 秒各一次；间隔设得过短会拉长拦截窗口，建议 ≥10 秒）。
const RATE_LIMIT_CODE: i64 = 11-128;

/// 上游要求首条消息必须是 system prompt，否则返回 400
/// （first message is not system prompt）。客户端没带 system 消息时注入一条兜底系统消息。
const DEFAULT_SYSTEM_PROMPT: &str = "你是一个得力助手";

/// 上游限额码 6004（HTTP 429）：账号×模型维度的用量限额，
/// msg 形如 `您的使用量已超出频率限制，将在 2026-09-11 19:43:46 UTC+8 重置…`
/// —— 恢复时间只在文本里，由 `errors::parse_quota_reset_at` 解析。
const QUOTA_LIMIT_CODE: i64 = 6004;

/// 11-128 透传给客户端时追加的敏感词指引（避免被误当成普通频率限制）
const WAF_HINT: &str = "；提示词可能命中上游敏感词，请检查提示词";

/// WorkBuddy 适配器（无状态单例，见 `adapter::adapter_for`）。
///
/// 唯一的字段是地区：两端协议一致，地区决定端点兜底、客户端身份（UA /
/// platform / 产品名）与**要读哪一份模型目录**。
pub struct WorkBuddyAdapter {
    /// 本实例服务的地区（国内版 / 国际版）
    region: Region,
}

impl WorkBuddyAdapter {
    /// 构造某个地区的适配器（`const`：两个实例都是静态量）
    pub const fn new(region: Region) -> Self {
        Self { region }
    }

    /// 本实例服务的地区
    pub const fn region(&self) -> Region {
        self.region
    }

    /// 本地区的模型目录句柄（进程级，与 `/v3/config` 刷新共用同一份状态）
    fn catalog(&self) -> crate::server::core::models::ModelCatalog {
        global_catalog(self.region)
    }
}

/// 进程级实例：适配器无状态（地区是构造期常量），静态实例即可
/// （`adapter_for` 返回它的引用）
pub static WORKBUDDY_ADAPTER: WorkBuddyAdapter = WorkBuddyAdapter::new(Region::Cn);
/// 国际版实例（拆家后与国内版各自独立，见模块头）
pub static WORKBUDDY_INTL_ADAPTER: WorkBuddyAdapter = WorkBuddyAdapter::new(Region::Intl);

impl ProviderAdapter for WorkBuddyAdapter {
    fn kind(&self) -> ProviderKind {
        self.region.kind()
    }

    /// workbuddy 的模型清单（`core::models` 的进程级句柄，与启动时 /v3/config
    /// 刷新的是同一份 RwLock 状态）。
    ///
    /// **按本实例的地区取**：两个地区各有一份（拆家前挤在同一份里，谁刷谁覆盖）。
    fn list_models(&self) -> Vec<Value> {
        self.catalog().list()
    }

    /// 构造 `POST {endpoint}/v2/chat/completions` 的请求（头集合与 URL 逐字沿用
    /// 改造前 `upstream::request` 的实现）。
    ///
    /// `account` 是账号会话形态：`core::auth` 的 `build_auth_headers` 直接读
    /// `auth.accessToken` / `account.uid` / `auth.domain` 等字段即可，
    /// 与改造前 `session_for` 拿到的对象完全同形。
    ///
    /// ── 出站归一化的位置与顺序（2026-09 新增）─────────────────────
    /// 本函数是「**这一家的**请求即将发出」的唯一汇聚点（`provider_loop` 对同一家
    /// 同池的重试复用同一份 body，见 `send_cache`），所以本家上游的形态要求
    /// （role 白名单 / tool_choice 只认字符串 / image_url 只认对象 / 只认
    /// max_tokens）在这里一次改对 —— 换了账号也不会重新踩同一批 400。
    ///
    /// **归一化必须在 `ensure_leading_system_message` 之前**：客户端用
    /// `developer` 打头时，先归一成 `system` 才判得出「首条已是 system」；
    /// 反过来会多注入一条兜底 system，而上游对多 system 的行为未实测。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let session = account;
        // 上游只支持流式：编排层在进来之前已把 body.stream 置 true（见
        // `upstream::forward` 的说明），这里不重复设置。
        //
        // 出站归一（本家形态要求，见函数头的顺序说明）。`notable()` 为假时
        // 不写日志：常规请求唯一的改动是注入了 prompt_cache_key（几乎每次都发生），
        // 为它写一行会让详细日志被淹没；真正的修复（角色/tool 配对/图片形态等）
        // 才值得留痕 —— 那是用户排查「网关对我的请求动了什么」的唯一入口。
        let (normalized, report) = normalize::normalize_outbound(body, session);
        if report.notable() {
            crate::server::logging::verbose(
                "[Upstream]",
                &format!("WorkBuddy 出站归一：{}", report.describe()),
            );
        }
        let with_system = match ensure_leading_system_message(&normalized) {
            Some(next) => {
                crate::server::logging::verbose(
                    "[Upstream]",
                    &format!("首条消息非 system，已注入系统消息：{DEFAULT_SYSTEM_PROMPT}"),
                );
                next
            }
            None => normalized,
        };
        let url = chat_completions_url(session, self.region);
        let headers = chat_headers(
            session,
            self.region,
            &crate::server::core::upstream::request::new_request_id(),
            Some("text/event-stream"),
        );
        Ok(ChatRequestPlan::chat(url, headers, with_system))
    }

    /// 上游错误分类（照抄改造前 `upstream` 的判定与文案）：
    ///   - 401 → TokenExpired（刷新后同账号重试一次）
    ///   - 429 或 code 6004 → QuotaLimited（冷却 + 换账号）
    ///   - 内容策略拦截（11-128 或审核文案）→ ContentBlocked（**不罚账号**：换中性提示词后
    ///     同账号重试一次 + 触发降级，见 `core::degrade`）
    ///   - 其余 → Fatal（原样透传）
    ///
    /// 文案逐字对齐改造前 `rotate::request_with_waf_retry` 的那三行
    /// `format!("上游返回 {status}: {message}{hint}")`。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let code = error_body.get("code").and_then(Value::as_i64);
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        if status == 401 {
            return UpstreamErrorClass::TokenExpired {
                message: format!("上游返回 {status}: {raw}"),
            };
        }
        if status == 429 || code == Some(QUOTA_LIMIT_CODE) {
            // 恢复时间只在上游文案里：解析失败给 None，由冷却标记落 10 分钟兜底
            let reset_at = crate::server::errors::parse_quota_reset_at(raw);
            return UpstreamErrorClass::QuotaLimited {
                reset_at: if reset_at > 0 { Some(reset_at) } else { None },
                message: format!("上游返回 {status}: {raw}"),
                upstream_code: code,
                status,
            };
        }
        // 11-128 透传时追加敏感词指引（改造前在 rotate::request_with_waf_retry 里）
        let is_waf_code = code == Some(RATE_LIMIT_CODE);
        let hint = if is_waf_code { WAF_HINT } else { "" };
        let message = format!("上游返回 {status}: {raw}{hint}");
        // 内容策略拦截（11-128 及其历史文案）：**不罚账号**，交给编排层降级到
        // 中性提示词后同账号重试一次（见 `core::degrade`）。判据取并集 ——
        // 业务码兜住「上游改了文案」、共用文案规则兜住「上游改了码」，两条
        // 指向的是同一件事（论证见 `providers::content_block` 模块头）。
        //
        // 提示文案分两种：11-128 用它自己那句既有措辞（`WAF_HINT`，改造前
        // 就在 —— 既有用户的错误文案逐字不变是本项目的约定，不并进共用提示）；
        // 只按文案命中的那一支才补共用提示（两条提示说的是同一件事，
        // 叠在一起只是噪音）。
        if is_waf_code {
            return UpstreamErrorClass::ContentBlocked {
                status,
                message,
                upstream_code: code,
            };
        }
        if content_block::matched(status, error_body) {
            return UpstreamErrorClass::ContentBlocked {
                status,
                message: format!("{message}{}", content_block::CONTENT_BLOCK_HINT),
                upstream_code: code,
            };
        }
        UpstreamErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    /// 取 access token（含临期主动刷新并回写 store）。
    ///
    /// - `account_id` 非空 → 该账号的 token；临期且可刷新时自动刷新
    ///   （改造前 `auth::get_current_session` 对「当前账号」做同一件事，
    ///   这里按**指定账号**做，因为编排层是逐账号尝试的）。
    /// - `account_id` 为空 → 本地区的默认登录态（环境变量或存储派生的当前账号）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if account_id.is_empty() {
                return default_session_token(store, self.region).await;
            }
            let entry = store.get_session_by_id(account_id).ok_or_else(|| {
                GatewayError::with_status(401, format!("账号 {account_id} 没有可用凭证"))
            })?;
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
                .unwrap_or("");
            // 临期主动刷新（改造前 `auth::get_current_session` 的行为，
            // 这里按指定账号走同一条路径）
            if is_token_expiring(expires_at) && !refresh_token.is_empty() {
                crate::server::logging::verbose(
                    "[Auth]",
                    &format!("token 临期，自动刷新（账号 {account_id}）…"),
                );
                let auth = AuthService::for_store(store.clone());
                let refreshed = auth.refresh_account(account_id).await.map_err(|error| {
                    GatewayError::with_status(error.http_status(), error.message)
                })?;
                return Ok(refreshed.access_token);
            }
            let token = entry
                .session
                .get("auth")
                .and_then(|auth| auth.get("accessToken"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if token.is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    format!("账号 {account_id} 的 accessToken 为空"),
                ));
            }
            Ok(token)
        })
    }

    /// 401（token 被服务端拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 为什么需要覆盖默认实现（默认 = 转调 `ensure_access_token`）：
    /// `ensure_access_token` 只在「临期」时才刷新，而 401 完全可能发生在一个
    /// 时间上还很新的 token 上（服务端侧失效、账号被顶下线、refreshToken 轮换）。
    /// 此时只调 ensure 会拿回同一个被拒的 token，编排层的「刷新后同账号重试一次」
    /// 就退化成「用同一个坏 token 再打一次」。
    ///
    /// 空 `account_id`（默认登录态）无账号记录可续期：环境变量凭证是静态的，
    /// 直接返回错误由编排层按普通失败处理（只重试一轮，不会空转）。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if account_id.is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "默认登录态的凭证无法续期（环境变量凭证是静态的），请重新登录",
                ));
            }
            let auth = AuthService::for_store(store.clone());
            match auth.refresh_account(account_id).await {
                Ok(refreshed) => Ok(refreshed.access_token),
                Err(error) if error.http_status() == 409 => {
                    // 并发刷新（同一账号正被另一处刷新）：那不算失败，只是
                    // 「刷新在别处进行中」。此刻重读 store —— 若那次刷新已落盘，
                    // 这里就能拿到新 token；还没落盘则返回错误，
                    // 编排层按普通失败处理（不会空转）。
                    store
                        .get_session_by_id(account_id)
                        .and_then(|entry| {
                            entry
                                .session
                                .get("auth")
                                .and_then(|auth| auth.get("accessToken"))
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                                .map(str::to_string)
                        })
                        .ok_or_else(|| {
                            GatewayError::with_status(
                                409,
                                format!("账号 {account_id} 的 token 正在刷新中"),
                            )
                        })
                }
                Err(error) => Err(GatewayError::with_status(error.http_status(), error.message)),
            }
        })
    }

    /// 刷新**本地区**的模型目录：沿用既有的「按当前账号刷新 /v3/config」实现
    /// （`core::models::ModelCatalog::refresh_with_current_account`）。
    ///
    /// ── `force` 对本家无差别（有意）─────────────────────────────
    /// workbuddy 的 `/v3/config` 拉取**没有缓存/TTL 早退**：每次调用都真打上游
    /// （`ModelCatalog::refresh` 的里里外外没有「刚刷过就跳过」的分支）。因此
    /// `force` 的「绕过缓存」语义在本家无事可绕 —— 自动路径与手动路径的行为
    /// 逐字相同，直接忽略该参数，不另加一层本地节流（加了反而会让用户按下按钮
    /// 后拿到一份没动的清单，那正是给 `force` 存在的理由）。
    ///
    /// ── 为什么能报回结果 ────────────────────────────────────────
    /// `refresh_with_current_account` 内部本来就算出了 `RefreshOutcome`
    /// （`{refreshed, count, source, reason}` 与 Node 版同形），只是过去直接
    /// 打日志、返回 `()`。改为把它**透出**（返回类型由 `()` 变成该结构，调用点
    /// 只有本函数）—— 这是最小改动：刷新的判定与落地逻辑一行未动，
    /// 「发生了什么都如实报出来」的翻译如下：
    ///   - 成功 → `ModelRefreshOutcome::refreshed(count)`；
    ///   - 失败 / 没刷 → `failed(reason)`（reason 里含真实原因：网络错误原文、
    ///     「企业模型列表为空」、「缺少登录态，使用内置模型目录」等）。
    /// 失败路径的既有行为（保留现有清单、打 verbose 日志）保持不变。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            let auth = AuthService::for_store(store.clone());
            // `account_id` 非空 = 用户在「获取模型」弹窗里点名的那条账号
            // （见 `refresh_with_current_account` 的三档选取）
            let outcome = self
                .catalog()
                .refresh_with_current_account(store, &auth, account_id)
                .await;
            if outcome.refreshed {
                ModelRefreshOutcome::refreshed(outcome.count)
            } else {
                // 本家没有「TTL 跳过」这一档：没刷就是真没拿到（网络 / 空清单 /
                // 没有登录态），全部如实报成失败原因，界面据此显示「为什么没变」
                ModelRefreshOutcome::failed(outcome.reason)
            }
        })
    }

    /// workbuddy 有远程目录（`GET /v3/config` 的 `data.models`），支持刷新模型清单。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 11-128 退避建议（改造前 `rotate::request_with_waf_retry` 的判定；
    /// 次数与间隔改由设置页的「请求重试」统一提供）。
    ///
    /// **循环**留在编排层（架构文档 §4.3「11-128 退避逻辑保持在转发层」），
    /// 这里只回答「这个错误要不要退避、退多久、为什么」——
    /// 「哪个码是敏感词拦截」是 workbuddy 的知识，不该漏进 `upstream/`。
    ///
    /// `budget` 是编排层按「这一轮是不是已经换过家」算出的预算
    /// （见 `ProviderAdapter::retry_advice` 的说明）—— 本函数只比 `attempt`
    /// 与它，不去读全局设置里的哪一档。
    ///
    /// 返回的 `reason` 不含「第 n/N 次」：那是编排层才知道的读数
    /// （见 `RetryAdvice` 的说明）。
    fn retry_advice(&self, error_body: &Value, attempt: usize, budget: usize) -> Option<RetryAdvice> {
        let code = error_body.get("code").and_then(Value::as_i64);
        if code != Some(RATE_LIMIT_CODE) {
            return None;
        }
        if attempt >= budget {
            return None;
        }
        let retry = crate::server::config::retry_settings();
        Some(RetryAdvice {
            delay_ms: retry.delay_ms(),
            reason: "上游敏感词拦截（11-128），请检查提示词中的敏感词（可在设置页「通用 → 指纹脱敏」开关）"
                .to_string(),
        })
    }

    /// workbuddy 有环境变量凭证旁路（国内版 `WORKBUDDY_TOKEN`、
    /// 国际版 `WORKBUDDY_INTL_TOKEN`），
    /// 因此账号列表为空时仍可用默认登录态转发（脚本 / CI 用户的常规用法）。
    fn allows_anonymous_default_session(&self) -> bool {
        true
    }

    /// 环境变量旁路凭证此刻是否存在（聚合目录判「这家现在有没有可用登录态」用）。
    /// 只判「存在且非空」，与 `Region::env_token` 同一判据 ——
    /// 按**本实例的地区**取变量（一个 token 只属于一个站点，见那里的说明）。
    fn env_credentials_present(&self) -> bool {
        self.region.env_token().is_some()
    }

    /// workbuddy 有「默认模型」概念（config.json 的 `defaultModel`，缺省 auto）；
    /// 客户端未指定 model 时网关据此注入（架构文档 §4.4 末句）。
    fn supports_default_model(&self) -> bool {
        true
    }

    /// 丢弃「整片只有换行」的 content 分片：**要丢**。
    ///
    /// 实测（NAS 生产库 `request_raw` 存的下发字节）：copilot.tencent.com 在
    /// 生成慢时会在相邻正文分片之间插一帧 `delta.content` 只含换行的分片 —— 
    /// 同一个流里还在发 `: heartbeat` 注释行，那才是它的本意保活手段。受影响
    /// 的请求里这类分片占正文分片的 36–39%，且二值分布（一条请求要么零个、
    /// 要么每片之间都插一个），干净的请求中位耗时 7.6 秒、脏的 15.3 秒 —— 
    /// 生成越慢越密。客户端按 markdown 渲染时一个换行就是一个 `<br>`，
    /// 「Let me run the unit tests」被排成一行一个词。
    ///
    /// ── 与本家「透传逐字节不变」那条硬要求的关系 ────────────────
    /// 那条要求说的是**网关不得擅自改写上游帧**；这里丢的正是上游擅自塞进
    /// 正文的节拍，而且是**显式开关**（判据见 `upstream::sse::is_newline_keepalive`，
    /// 关掉即回到逐字节一致）。取舍：模型若真有一个「单独成片」的换行，会被
    /// 一起吃掉 —— 少一个换行的外观损失，换掉整屏断句。
    fn sse_strip_newline_chunks(&self) -> bool {
        true
    }

    /// workbuddy 支持主动刷新（`/auth/token/refresh` + `X-Refresh-Token` 头，
    /// 实现见 `AuthService::refresh_account`）。
    fn supports_refresh(&self) -> bool {
        true
    }

    /// 临期判定：读该账号**会话里的** `auth.expiresAt`（不是 accounts.json 的
    /// 公开形态 —— 那份按设计只有 `tokenTail`），窗口用 `auth::is_token_expiring`
    /// 里那同一个 5 分钟常量。
    ///
    /// ── 为什么复用 `is_token_expiring` 而不是自己比一个数 ──────────
    /// 转发链路的懒刷新（`ensure_access_token`）用的就是它；维护任务若另写一个
    /// 窗口，会出现「维护认为不需要刷、请求链路却认为临期」的分叉 ——
    /// 两边都「没坏」，只是判断不一致，排障时极难发现。
    ///
    /// 取不到会话（账号不存在 / 已无凭证）时返回 false（见 trait 契约：
    /// 判不出来就不刷）。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        if account_id.is_empty() {
            return false;
        }
        let Some(entry) = store.get_session_by_id(account_id) else {
            return false;
        };
        if entry
            .session
            .get("auth")
            .and_then(|auth| auth.get("refreshToken"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return false;
        }
        let expires_at = entry
            .session
            .get("auth")
            .and_then(|auth| auth.get("expiresAt"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        is_token_expiring(expires_at)
    }

    /// workbuddy 有积分概念（腾讯计费接口，改造前唯一有的一家）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查积分简报 —— **转调既有计费服务，形状一个字不改**。
    ///
    /// ── 为什么形状不归一化（这是硬要求）─────────────────────────
    /// `query_credits_summary` 返回的
    /// `{kind, unlimited, totalLeft, planLeft, bonusLeft}` 是改造前就有的既有契约，
    /// 前端一直按它渲染（余额列的读数，见 `ui/accounts-table.js` 的 `usageSummary`）。
    /// 把它包装成 trait 文档里那套 `{available, unit, wallets, subscription}`
    /// 会让那一格的显示退化 —— 那是明令禁止的。四家的形状在
    /// `query_usage` 的文档里写清楚了：新移植的两家走统一形状，本家保持原样，
    /// 前端按字段探测两套形状（`totalLeft` 键就是本家形状的判别键）。
    ///
    /// ── 与 `api::accounts` 里那条老路径的关系 ────────────────────
    /// 这里构造的 `BillingService::new(AuthService::for_store(store.clone()))`
    /// 与 `ServerState::billing()` **是同一个东西**：后者是
    /// `BillingService::new(AuthService::new(store, default_context()))`，
    /// 而 `AuthService::for_store` 用的也是 `default_context()`
    /// （见 `core::auth::AuthService::for_store`）。因此接口层可以统一走适配器，
    /// 拿到的结果与改造前逐字相同 —— 不必按 provider 在接口层再分一条老路径。
    ///
    /// `session` 传 `None` 会让计费服务回落到 `auth.get_current_session()`
    /// （那是「当前账号」的语义）。这里必须传**指定账号**的会话：
    /// 批量查询是逐账号并发的，用「当前账号」会让所有账号都查成同一个人。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(entry) = store.get_session_by_id(account_id) else {
                return Err(GatewayError::with_status(
                    401,
                    "该账号没有可用凭证，无法查询积分",
                ));
            };
            let billing = crate::server::core::billing::BillingService::new(
                AuthService::for_store(store.clone()),
            );
            billing
                .query_credits_summary(Some(&entry.session), None)
                .await
                .map_err(|error| error.to_gateway_error())
        })
    }
}

/// 本地区默认登录态的 access token（环境变量优先，其次账号存储派生的当前账号）。
///
/// 与改造前 `auth::get_current_session` 的优先级一致：本地区对应的环境变量
/// 存在时直接用它、完全不看账号库（脚本/CI 用户的旁路入口）。
/// 「本地区」由调用方给的 `region` 决定 —— 变量名按地区分开，见
/// `Region::env_token`。
async fn default_session_token(store: &AccountStore, region: Region) -> Result<String, GatewayError> {
    if let Some(token) = region.env_token() {
        return Ok(token);
    }
    let auth = AuthService::for_store(store.clone());
    match auth.get_current_session().await {
        Ok(Some(session)) => session
            .get("auth")
            .and_then(|auth| auth.get("accessToken"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                GatewayError::with_status(401, "当前没有可用登录态：请先在桌面端完成登录")
            }),
        Ok(None) => Err(GatewayError::with_status(
            401,
            "当前没有可用登录态：请先在桌面端完成登录",
        )),
        Err(error) => Err(GatewayError::with_status(error.http_status(), error.message)),
    }
}

// ─── 头集合与 URL（从 upstream/request.rs 搬入，语义逐字保持）──────

/// 客户端身份 + 鉴权 + 会话追踪头（对照 Node 的 buildHeaders）。
///
/// `request_id` 同时充当 X-Request-ID / X-Conversation-Request-ID /
/// X-Conversation-ID / X-Session-ID —— Node 在没有 conversationId 时就是
/// 这四者取同一个值（追踪一轮对话用）。
///
/// ── 客户端身份按 `region` 取（拆家后的口径）────────────────────
/// 改造前这里读的是 `session.edition`（账号上的字段）。拆家后**provider id
/// 就是权威**：账号在哪个地区组里，就按哪个地区的身份发请求 —— 这与
/// `api::session` 里 ZCode 那段注释同一条理由（拿账号字段定地区，会让
/// 「落错家的账号」把请求稳定打到错域名上，而那种错没有任何日志会提示）。
/// 账号记录里的 `endpoint` 覆盖仍然优先（见 `chat_completions_url`）。
///
/// `pub(super)`：本目录的 `keepalive`（国际版每日活跃保活）与转发共用同一套
/// 头集合 —— 保活必须长得和真对话一模一样，抄一份必然漂移。
pub(super) fn chat_headers(
    session: &Value,
    region: Region,
    request_id: &str,
    accept: Option<&str>,
) -> Vec<(String, String)> {
    let edition = region.edition();
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        // 完整三段 UA（含 CLI 扩展段）：与桌面客户端一致，服务端按此识别通道
        (
            "User-Agent".to_string(),
            crate::server::core::endpoints::user_agent_for_edition(Some(edition.id)),
        ),
        // 客户端身份头：服务端按此做客户端识别与白名单校验
        ("X-IDE-Type".to_string(), edition.ua_platform.to_string()),
        ("X-IDE-Name".to_string(), edition.product_name.to_string()),
        ("X-IDE-Version".to_string(), edition.client_version.to_string()),
        ("X-Product".to_string(), edition.product_name.to_string()),
        ("X-Agent-Intent".to_string(), "craft".to_string()),
        // 会话追踪
        ("X-Request-ID".to_string(), request_id.to_string()),
        ("X-Conversation-Request-ID".to_string(), request_id.to_string()),
        ("X-Conversation-ID".to_string(), request_id.to_string()),
        ("X-Session-ID".to_string(), request_id.to_string()),
    ];
    headers.extend(AuthService::build_auth_headers(session));
    if let Some(accept) = accept {
        headers.push(("Accept".to_string(), accept.to_string()));
    }
    headers
}

/// 对话接口 URL：`{session.endpoint || 本地区端点}/v2/chat/completions`。
///
/// 兜底端点取**本实例的地区**（拆家前取 `endpoints::default_context()`，
/// 那是「本进程只有一个上游」时代的产物：它读 `WORKBUDDY_EDITION` 环境变量，
/// 而现在两个地区同时存在，环境变量不该再决定某个 provider 打哪个站）。
/// 账号记录里带 `endpoint` 时仍然以它为准 —— 那是 staging / 自建反向代理
/// 用户的落点，也是同一个账号级别的覆盖能力。
pub(super) fn chat_completions_url(session: &Value, region: Region) -> String {
    let fallback = region
        .env_endpoint_override()
        .unwrap_or_else(|| region.default_endpoint().to_string());
    let endpoint = session
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(crate::server::core::endpoints::normalize_endpoint)
        .unwrap_or_else(|| crate::server::core::endpoints::normalize_endpoint(&fallback));
    format!("{endpoint}/v2/chat/completions")
}

/// 首条消息不是 system 时，在头部补一条系统消息（不改原数组，返回新 body）。
///
/// 已经是 system 开头、messages 为空/缺失时返回 None（调用方保持原 body）——
/// 空 messages 属客户端异常输入，交给上游按原样报错。
pub fn ensure_leading_system_message(body: &Value) -> Option<Value> {
    let messages = body.get("messages").and_then(Value::as_array)?;
    if messages.is_empty() {
        return None;
    }
    // Node: `String(body.messages[0]?.role ?? '').toLowerCase()`
    let first_role = match messages[0].get("role") {
        Some(Value::String(text)) => text.to_lowercase(),
        Some(Value::Null) | None => String::new(),
        Some(other) => value_text(other).to_lowercase(),
    };
    if first_role == "system" {
        return None;
    }
    let mut next = body.clone();
    let Some(object) = next.as_object_mut() else {
        return None;
    };
    let mut with_system = Vec::with_capacity(messages.len() + 1);
    with_system.push(serde_json::json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT }));
    with_system.extend(messages.iter().cloned());
    object.insert("messages".to_string(), Value::Array(with_system));
    Some(next)
}

/// JS `String(x)`（role 这类字段的容错文本化）
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}
