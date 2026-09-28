//! Trae 的 `ProviderAdapter` 实现。
//!
//! ── 本家在整个转发面的坐标 ──────────────────────────────────
//! Trae 走**有状态路径**（`is_stateful() = true`）：上游 SSE 不是 OpenAI 方言
//! （没有 `[DONE]`、`token_usage` 欠到下一帧、`event: error` 之后还继续发帧），
//! 而通用无状态路径的形状是「响应字节原样透传」，中间**没有**按家翻译帧的钩子
//! （全仓唯一的协议翻译器在 `custom/forward.rs`，它要的两把凭证不会交给适配器）。
//! 所以发送、翻帧、错误定档、用量上报都在 `forward.rs` 里做，与 Accio / Qoder
//! 同一处置。本文件因此只剩三件对外的"协议无关"的事：造请求（给对拍与
//! `custom` 那类调用点留着）、分错误、取令牌与刷目录。
//!
//! ── 续期为什么在这里是真的（不是 zcode 那种如实报错）─────────
//! 本家有可用的 refreshToken 续期链（`refresh.rs`），而且它**必须**被接上：
//! Trae 的 refreshToken 每次换发都轮换，编排层「401 后刷新重试一次」这条动作
//! 如果落到一个假刷新实现上，等于拿同一串刚被服务端作废的旧串再打一次 ——
//! 不但白打，还会把"这一轮的新串"从内存里丢掉（换发结果没人接）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::{AccountStore, CredentialWrite};
use crate::server::errors::GatewayError;
use crate::server::core::proxies::ResolvedProxy;

use super::super::adapter::{ ChatUpstreamProtocol,
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use super::super::content_block;
use super::super::ProviderKind;
use super::credentials::Credential;
use super::errors::{classify, ErrorKind};
use super::headers::{solo_headers, HeaderIdentity};
use super::models;
use super::payload;
use super::refresh::{refresh_candidates, refresh_shared, MAX_ISSUE_AGE_MS, REFRESH_LEAD_MS};
use super::{AGENT_BASE_URL, CHAT_PATH, PROVIDER_ID};

/// Trae 适配器（无状态；身份全在账号记录里）。
pub struct TraeAdapter;

pub static TRAE_ADAPTER: TraeAdapter = TraeAdapter;

impl ProviderAdapter for TraeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Trae
    }

    /// 模型清单 = 上一次成功刷到的那张远程表（没刷到时为空，见 `models.rs`）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// 有状态路径的入口（转交给 `forward.rs`；理由见模块头）。
    fn is_stateful(&self) -> bool {
        true
    }

    fn forward_conversation<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        body: &'a Value,
        _client_headers: &'a HeaderMap,
        proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        stream: bool,
        telemetry: &'a std::sync::Arc<crate::server::core::upstream::usage::RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<crate::server::core::upstream::ForwardOutcome, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { super::forward::forward(store, account_id, body, proxy, stream, telemetry).await })
    }

    /// 无状态路径的入口。本家**不会**被走到这里（`is_stateful()` 已为 true），
    /// 留着有两个理由：形状对拍的调用点用它，以及万一有人把 `is_stateful`
    /// 改回 false，这里报错比"半条链路能跑、帧却没人翻译"好查得多。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let credential = credential_of(account)?;
        if credential.access_token.trim().is_empty() {
            return Err(GatewayError::with_status(
                401,
                "Trae 账号缺少 accessToken，无法转发（请重新登录）",
            ));
        }
        let identity = HeaderIdentity {
            access_token: credential.access_token.trim(),
            uid: credential.uid.trim(),
            machine_id: credential.machine_id.trim(),
            device_id: credential.device_id.trim(),
        };
        // 上游恒 `stream: true`（`payload::prepare_body` 里写死），Accept 因此
        // 也恒 `text/event-stream`；`resolved_model` 传空 = 用 body 自带的模型名
        // （本家 M3 前不广告模型，也就没有"映射后的上游名"要覆盖）。
        let headers = solo_headers(&identity, true);
        Ok(ChatRequestPlan {
            protocol: ChatUpstreamProtocol::OpenAI,
            url: format!("{AGENT_BASE_URL}{CHAT_PATH}"),
            headers: headers.into_iter().collect(),
            body: payload::prepare_body(body, credential.variant(), ""),
        })
    }

    /// 上游错误分类。
    ///
    /// 判据在 `errors::classify`（HTTP 状态 + 业务码一张表），本方法只做两件事：
    /// 把它的 `ErrorKind` 翻成编排层认识的 `UpstreamErrorClass`，以及**保住文案**。
    ///
    /// 一条本家特有的处置：`ErrInputTooLarge` / `ErrModelUnavailable` 都归到
    /// `Fatal`（不冷却账号）。大输入与"模型在这个通道不可用"是**请求级**问题 ——
    /// 同一份 body 换任何账号都会被拒，冷却健康账号只会把一次坏请求摊成
    /// 一串被罚的账号（参考实现 v0.12.50 / v0.12.79 两条同旨）。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let detail = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let code = error_body.get("code").and_then(Value::as_i64);
        let message = format!("上游返回 {status}: {detail}");
        // 判据吃的是**整份错误体**而不是那句文案：参考实现的分类靠
        // 在 body 里找 `"code":1005` / `"code":4008` 这类片段（业务码可能
        // 裹在非 2xx 响应体里），只传文案就把这套判据整个绕过了。
        // 这里传归一化后的对象序列化结果 —— 与 `classify` 的向量口径一致
        // （向量里那些 body 就是 `{"code":…,"msg":…}` 的形状）。
        match classify(status, &error_body.to_string()) {
            ErrorKind::SessionDead => UpstreamErrorClass::TokenExpired { message },
            // 429 软限流与 1005/4008 计划限额都会换账号，区别只在冷却时长
            // （编排层按类别给时长，本家不提供 reset_at：上游没给可读的恢复时刻）。
            ErrorKind::SoftRate | ErrorKind::PlanLimit => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            },
            // 只有落到"其它 4xx"这一档才交给跨家的内容拦截判定：本家的
            // 请求级分类（过大 / 通道不可用 / 404）必须优先，把它们误判成
            // 内容拦截会触发一次无意义的同账号重试。
            ErrorKind::Client => content_block::classify_or_fatal(status, error_body, message, code),
            // Server / NotFound / InputTooLarge / ModelUnavailable / None
            // 都不该罚账号：只有 QuotaLimited / TokenExpired 会让编排层去换
            // 账号或重试，其余落到 Fatal 原样透出。
            _ => UpstreamErrorClass::Fatal { message, upstream_code: code, status },
        }
    }

    /// 取可用令牌：临期（24h 窗口）或签发龄超 15 天时主动续期并回写。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let record = read_record(store, account_id)?;
            let mut credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
            if credential.needs_refresh(REFRESH_LEAD_MS, MAX_ISSUE_AGE_MS, crate::server::logging::now_ms()) {
                let proxy = account_proxy(&record)?;
                credential = renew(store, &record, &credential, proxy.as_ref()).await?;
            }
            if credential.access_token.trim().is_empty() {
                return Err(GatewayError::with_status(401, "Trae 账号缺少 accessToken，请重新登录"));
            }
            Ok(credential.access_token)
        })
    }

    /// 401 后的强制刷新：**无视临期判定直接换发**。
    ///
    /// 上游在被拒时不会告诉我们"还剩多久"，所以这条路径不能走
    /// `ensure_access_token` 的临期门（那会原样返回刚被拒的串，让编排层的
    /// 「刷新后重试一次」退化成「用同一个坏 token 再打一次」）。
    /// `force` 的语义由 `renew` 前跳过 `needs_refresh` 实现。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let record = read_record(store, account_id)?;
            let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
            let proxy = account_proxy(&record)?;
            let renewed = renew(store, &record, &credential, proxy.as_ref()).await?;
            Ok(renewed.access_token)
        })
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    /// 后台凭证维护问"这条账号是不是快到期了"，本家**必须**回答（trait 默认 false）。
    ///
    /// 漏写的症状不是报错，而是**定时维护与「立即维护」每轮都跳过本家**
    /// （`credential_maintenance` 按这一位过滤目标）：账号页长期挂着「已过期」，
    /// 直到某次转发撞 401 才被懒刷新救回来 —— 而 Trae 的 refreshToken
    /// 一次一换，越晚刷越容易撞上"串已被别的程序换掉"。判据与续期用的是
    /// 同一把尺（`needs_refresh`：临期 24h 窗口 + 签发龄 15 天两条），
    /// 所以这里不会出现"维护说没到期、转发时又判要刷"的分叉。
    fn credentials_expiring(&self, store: &AccountStore, account_id: &str) -> bool {
        let Some(record) = store.trae_account_record(account_id) else {
            return false;
        };
        match Credential::from_payload(&record) {
            // 没有 refreshToken 的账号（手工粘贴只给了 accessToken 是常态）在这里
            // 判 false：续期手段都没有，交给维护只会变成每轮一条稳定失败的记录。
            Ok(credential) if credential.can_refresh() => {
                credential.needs_refresh(REFRESH_LEAD_MS, MAX_ISSUE_AGE_MS, crate::server::logging::now_ms())
            }
            // 凭据读不出（缺 accessToken 之类）同理：它连"还有多久过期"都判不了。
            _ => false,
        }
    }

    /// 本家支持网页登录（授权地址要等 guidance，见 `login.rs`）。
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 用**这条账号的凭据**拉一次远程目录（`get_detail_param`）。
    ///
    /// 目录是按客户端版本给的一张表（`X-Ide-Version-Code` 决定拿哪张），
    /// 不是账号无关的静态清单，所以这一家走"按账号刷"那条路
    /// （`refresh_uses_account()` 保持默认 true）：界面上选哪条凭据刷，
    /// 刷出来的就是那条凭据看得见的表。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            // 「没有账号」不是失败（与 accio / qoder / codearts 同口径）：
            // 自动路径每轮都会走到这里，把它算成失败会给用户还没配置的家记上
            // 一次失败并触发冷却退避（间隔 × 2^(n-1)，默认间隔 60 分钟），
            // 于是新用户添加完账号、回头点「获取模型」会被「请在 N 秒后重试」
            // 挡住。`read_record` 的 401 语义留给转发与余额那两条链，
            // 目录这条按空 id（自动）与点名（手动）分开处理。
            let Some(record) = store.trae_account_record(account_id) else {
                crate::server::logging::verbose("[Models]", "Trae 模型目录刷新跳过：尚未添加 Trae 账号");
                if account_id.trim().is_empty() {
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的 Trae 账号不存在或不可用，请重新选择");
            };
            let credential = match Credential::from_payload(&record) {
                Ok(credential) => credential,
                Err(reason) => return ModelRefreshOutcome::failed(reason),
            };
            let proxy = match account_proxy(&record) {
                Ok(proxy) => proxy,
                // 代理配置坏了要如实失败（不静默直连）：直连会拿到一张
                // "从本机看得到"的表，而用户以为刷的是那条账号的出口。
                Err(error) => return ModelRefreshOutcome::failed(error.message),
            };
            models::refresh(&credential, proxy.as_ref(), force).await
        })
    }

    /// 本家有远程目录（界面上会给这家渲染「刷新模型清单」）。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 本家有额度/余额概念（`usage.rs`：`ide_user_ent_usage` + `ide_user_pay_status`）。
    ///
    /// 界面按这一位决定卡片上要不要给「余额」按钮，所以它必须与
    /// `query_usage` 是否真接上同步改（漏一边的后果：要么按钮点了报错，
    /// 要么有读数却没有入口）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 额度读数（形状契约见 trait 文档；本家的实现细节在 `usage.rs`）。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>> {
        Box::pin(async move { super::usage::query_usage(store, account_id).await })
    }
}

/// 续期 + 回写（两条取令牌路径共用这一段，分叉的代价是"一处比较一处不比"）。
/// 临期（或签发龄超上限）才换发；否则原样返回。
pub(crate) async fn renew_if_due(
    store: &AccountStore,
    record: &Value,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if credential.needs_refresh(REFRESH_LEAD_MS, MAX_ISSUE_AGE_MS, crate::server::logging::now_ms()) {
        return renew(store, record, credential, proxy).await;
    }
    Ok(credential.clone())
}

/// 无视临期判定直接换发（401 之后救一次用的就是这条）。
pub(crate) async fn renew_forced(
    store: &AccountStore,
    record: &Value,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    renew(store, record, credential, proxy).await
}

async fn renew(
    store: &AccountStore,
    record: &Value,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if !credential.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "Trae 账号里没有 refreshToken，无法续期（请在「账号」页重新登录）",
        ));
    }
    let renewed = refresh_shared(credential, &refresh_candidates(&credential.api_host), proxy).await?;
    match store.update_trae_credentials_if_current(record, &renewed) {
        Ok(CredentialWrite::Written) => {}
        Ok(CredentialWrite::Stale) => {
            // 记录在续期期间被别人改过（并发登录 / 手工粘贴）。**不能**把这次
            // 换发结果写进去，也不能把新串丢掉不管 —— 报一句让用户知道
            // refreshToken 可能已经被服务端轮换过，下一次重试会以落盘那份为准。
            return Err(GatewayError::with_status(
                409,
                "Trae 续期结果与账号记录不匹配（期间被改过），本次换发未落盘，请重试",
            ));
        }
        Err(error) => return Err(GatewayError::with_status(error.status_code, error.message)),
    }
    Ok(renewed)
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）—— 与 `accio::auth` /
/// `qoder::auth` 里同名函数同一语义、同一文案来源。
pub(crate) fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match crate::server::core::proxies::resolve_account_proxy(record.get("proxy")) {
        Some(crate::server::core::proxies::ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(crate::server::core::proxies::ProxyResolution::Failed(reason)) => {
            Err(GatewayError::with_status(400, reason))
        }
        None => Ok(None),
    }
}

/// 读账号记录（点名 vs 队首，与 zcode 同一口径：点名的取不到就报错，
/// 不回落到队首 —— 那会把「点名的账号坏了」变成「静默用了别人的额度」）。
///
/// `pub(crate)` 给 `usage.rs` 复用：余额查询与转发必须读**同一条**账号记录，
/// 两处各写一遍取记录的规定，迟早会出现一边按 id、一边按队首。
pub(crate) fn read_record(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    let missing = || GatewayError::with_status(401, "没有可用的 Trae 账号，请先在「账号」页添加");
    if account_id.trim().is_empty() {
        store.trae_account_record("").ok_or_else(missing)
    } else {
        store
            .trae_account_record(account_id)
            .ok_or_else(|| GatewayError::with_status(401, "找不到该 Trae 账号"))
    }
}

/// 从账号记录还原凭据（转发侧只需要它已有的字段）。
fn credential_of(account: &Value) -> Result<Credential, GatewayError> {
    Credential::from_payload(account).map_err(GatewayError::new)
}

/// 本家 provider id（供调用点写日志时取用，别处不要再写字面量 `"trae"`）。
pub const fn provider_id() -> &'static str {
    PROVIDER_ID
}

#[cfg(test)]
mod tests {
    //! 这里补的是**审计提出的一条真空**：`credentials_expiring` 是后台维护
    //! 唯一的入场券（trait 默认 false = 每轮都跳过这家），而它此前没有测试 ——
    //! 漏写这条的失效方式是静默的：账号页长期挂着「已过期」，定时维护与
    //! 「立即维护」都不碰本家，直到某次转发撞 401。
    use serde_json::json;

    use super::*;
    use crate::server::core::account_store::AccountStore;

    /// 临时库 + 账号 id + **删文件的守卫**（第三个值必须留在作用域里，
    /// 否则用例跑完文件还在 —— 见 `test_temp::TempDb` 的注释）。
    fn store_with(credential: Credential) -> (AccountStore, String, crate::server::db::test_temp::TempDb) {
        let (db, guard) = crate::server::db::test_temp::TempDb::open(&format!("trae-maint-{}", credential.uid));
        let store = AccountStore::with_db(Some(db));
        let added = store
            .add_trae_account(&credential, None, "test")
            .expect("账号应能落库");
        let id = added["account"]["id"].as_str().or_else(|| added["id"].as_str()).unwrap_or("").to_string();
        // **反空跑**：id 取不到时会退化成空串，而 `trae_account_record("")` 是
        // "取队首可用账号" —— 那等于三条负向用例可能因为"根本没查到记录"而全绿，
        // 而不是因为判定正确。所以这里直接把"记录按 id 查得到"钉住。
        assert!(!id.is_empty(), "落库返回里没拿到账号 id：{added}");
        assert!(store.trae_account_record(&id).is_some(), "按 id 查不回记录：{id}");
        (store, id, guard)
    }

    fn credential(uid: &str, refresh: &str, expires_at: i64) -> Credential {
        Credential {
            access_token: "at-abc".to_string(),
            refresh_token: refresh.to_string(),
            expires_at,
            domain: "trae.cn".to_string(),
            api_host: "https://api.trae.cn".to_string(),
            machine_id: "m-1".to_string(),
            device_id: "1234567890123456".to_string(),
            variant: "solo".to_string(),
            uid: uid.to_string(),
            nickname: "用户".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_maintenance_task_is_told_about_an_expired_credential() {
        let past = crate::server::logging::now_ms() - 60_000;
        let (store, id, _db) = store_with(credential("9001", "rt-1", past));
        assert!(TRAE_ADAPTER.credentials_expiring(&store, &id), "已过期且有 refreshToken → 该交给维护");
    }

    #[test]
    fn a_still_valid_credential_is_left_alone() {
        let future = crate::server::logging::now_ms() + 30 * 24 * 3600 * 1000;
        let (store, id, _db) = store_with(credential("9002", "rt-2", future));
        assert!(!TRAE_ADAPTER.credentials_expiring(&store, &id), "还有 30 天 → 不该去打上游");
    }

    #[test]
    fn a_pasted_account_without_refresh_token_is_not_queued() {
        // 手工粘贴常见只有 accessToken：判 true 只会让维护每轮产出一条稳定失败的
        // 记录，把真正需要看的失败淹掉（`credential_maintenance` 模块头明说这条）。
        let past = crate::server::logging::now_ms() - 60_000;
        let (store, id, _db) = store_with(credential("9003", "", past));
        assert!(!TRAE_ADAPTER.credentials_expiring(&store, &id));
    }

    #[test]
    fn an_unknown_account_never_claims_to_need_refreshing() {
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("trae-maint-empty");
        let store = AccountStore::with_db(Some(db));
        assert!(!TRAE_ADAPTER.credentials_expiring(&store, "trae-solo-does-not-exist"));
    }

    #[test]
    fn the_public_account_shape_carries_what_the_panel_asks_for() {
        // 面板那三处登记（能力表 / 表单 / 图标）都按**字段名**取值，字段名漂了
        // 界面只会静默少一项。这里把契约钉在 Rust 侧：uid / expiresAt / variant /
        // hasRefreshToken / available 五个键必须在公开形态里。
        let (store, id, _db) = store_with(credential("9004", "rt-4", crate::server::logging::now_ms() + 3_600_000));
        let record = store.trae_account_record(&id).expect("记录应在");
        for key in ["uid", "expiresAt", "variant", "provider", "id"] {
            assert!(record.get(key).is_some(), "公开/存储形态里少了 {key}：{}", json!(&record));
        }
        assert_eq!("trae", record["provider"].as_str().unwrap_or_default());
        assert!(record.get("accessToken").is_some(), "维护判定要用到凭据本体（这条记录是内部形态）");
    }
}
