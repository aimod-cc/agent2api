//! ZCode 的 `ProviderAdapter` 实现（无状态、OpenAI 兼容、按地区参数化）。
//!
//! ── 实现是**一套**，实例按地区给 ────────────────────────────
//! `ZcodeAdapter` 持有一个 [`Region`]，两个静态实例（`ZCODE_ADAPTER` 国内版 /
//! `ZCODE_INTL_ADAPTER` 国际版）由 `adapter_for` 按 kind 给出 —— 与
//! `autoclaw::adapter` / `accio::adapter` 同一手法（两个地区是两个 provider、
//! 同一份实现）。地区 → provider 的互查只在 `region.rs`，别处不要写
//! `"zcode-intl"` 这类字面量。
//!
//! ── 上游协议 ────────────────────────────────────────────────
//! 推理走 **OpenAI 兼容**端点：`POST {openai_base}/chat/completions`，
//! `Authorization: Bearer {token}`，body 原样透传（与 raccoon 同构）。
//! 因此 `is_stateful()` 保持默认 false（一次发送由通用编排层完成），
//! 与 CatPaw / Qoder / Accio 那三家「适配器自己发」的情形不同。
//!
//! ── 令牌来源与续期（**当前是已知缺口**）──────────────────────
//! 本适配器只从账号会话里读 `auth.accessToken`（与 raccoon 的
//! `build_chat_request` 同一取法），**没有**实现续期：
//! `refresh_access_token` 会如实报错让用户重新登录。
//!
//! 这不是偷懒而是划界：ZCode 的登录是**服务端中介的 CLI 轮询**
//! （`/oauth/cli/init` + `/oauth/cli/poll/{flow_id}`，见参考实现
//! `Acankao/zcode-api` 的 `src/auth/oauth.ts`），它要先有 `credentials.rs`
//! 落盘 JWT 与设备标识、再有前端登录页，续期才有东西可续。在那两件完成之前，
//! 这里返回一句可读的报错，比留一个「刷新了但没变」的假实现更诚实 ——
//! 后者会让编排层的「401 后刷新重试一次」变成「用同一个坏 token 再打一次」。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::super::adapter::{
    ChatRequestPlan, ChatUpstreamProtocol, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass,
};
use super::super::content_block;
use super::super::ProviderKind;
use super::models;
use super::region::Region;
use crate::server::core::protocol::anthropic_outbound;

/// 账号 `planMode` 字段的两个取值（账号级套餐通道，见 `build_chat_request`）。
/// 缺省 / 空值 / 认不出 = 编码套餐通道（与历史账号逐字兼容）。
pub const PLAN_MODE_CODING: &str = "coding-plan";
pub const PLAN_MODE_START: &str = "start-plan";

/// ZCode 适配器（无状态；地区是唯一的状态，构造期固定）
pub struct ZcodeAdapter {
    /// 本实例服务的地区
    region: Region,
}

/// 国内版实例（`adapter_for` 返回它的引用）
pub static ZCODE_ADAPTER: ZcodeAdapter = ZcodeAdapter { region: Region::Cn };

/// 国际版实例
pub static ZCODE_INTL_ADAPTER: ZcodeAdapter = ZcodeAdapter { region: Region::Intl };

impl ZcodeAdapter {
    /// 本实例的地区（供 `adapter_for` 之外的调用点自查，例如领取任务的选路）
    pub fn region(&self) -> Region {
        self.region
    }

    /// 推理基址（`ZCODE_OPENAI_BASE_URL` / `ZCODE_INTL_OPENAI_BASE_URL` 可覆盖）。
    ///
    /// 留覆盖口子是因为上游域名会变（智谱历史上换过编码套餐的域名），
    /// 而发版节奏跟不上域名变更时，用户至少能自己改环境变量救急 ——
    /// 与 autoclaw / raccoon 两家的 `env_override` 同一取舍。
    fn openai_base_url(&self) -> String {
        self.region
            .env_override("OPENAI_BASE_URL")
            .unwrap_or_else(|| self.region.openai_base_url().to_string())
    }

    /// 体验套餐（start-plan）通道的请求计划。
    ///
    /// ── 凭证为什么是 JWT 而不是 accessToken ───────────────────
    /// 两个凭证各管一张网（见 `credentials.rs` 的模块头）：`accessToken`（编码
    /// 套餐 API Key）只被开放平台的 coding 端点认；`zcode.z.ai` 的套餐网关认的
    /// 是**登录 JWT** —— 就是「领套餐」用的那一份。缺 JWT 时给出可操作的指引
    /// 而不是含混的 401：粘贴凭证添加的账号可能只填了一半。
    ///
    /// ── body 为什么要翻成 Anthropic messages ──────────────────
    /// 这条网关只说 Anthropic 协议（老 OpenAI 路由已退役，见 region 的说明）。
    /// 入站的 chat 体在这里出站翻译；回程由编排层把 Anthropic SSE 翻回 chat
    /// （`ChatRequestPlan::protocol` 位驱动，与 custom 家同一道翻译）。
    fn build_start_plan_request(
        &self,
        session: &Value,
        body: &Value,
    ) -> Result<ChatRequestPlan, GatewayError> {
        let jwt = session
            .get("jwt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if jwt.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "ZCode 账号缺少登录 JWT，无法走体验套餐（start-plan）通道：\
                 请在「账号」页对该账号重新登录，或补粘贴完整凭证",
            ));
        }
        let requested = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        // 出站翻译：chat 体 → Anthropic messages。stream 恒为 true（与
        // chat 协议同一条策略：上游恒流式，非流式客户端由聚合路径收流 ——
        // 转换器只如实搬运 chat 体上的 stream，这里统一覆写，不依赖注入约定）。
        let mut payload = anthropic_outbound::anthropic_request_from_chat(body, &requested)
            .map_err(|message| GatewayError::with_status(400, format!("{message}")))?;
        if let Some(object) = payload.as_object_mut() {
            object.insert("stream".to_string(), Value::Bool(true));
        }
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
            ("Authorization".to_string(), format!("Bearer {jwt}")),
            // Anthropic 网关的版本头（custom 家的 anthropic 上游同一取值）
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ];
        headers.extend(identity_headers());
        Ok(ChatRequestPlan {
            protocol: ChatUpstreamProtocol::Anthropic,
            url: self.region.start_plan_anthropic_url().to_string(),
            headers,
            body: payload,
        })
    }
}

impl ProviderAdapter for ZcodeAdapter {
    fn kind(&self) -> ProviderKind {
        self.region.kind()
    }

    /// 本家的模型清单（静态表，两地共用一份，见 `models.rs` 的模块头）
    fn list_models(&self) -> Vec<Value> {
        models::list(self.region)
    }

    /// 构造 `POST {openai_base}/chat/completions`。
    ///
    /// body **原样透传**：上游就是 OpenAI 协议，本家没有任何要改写的字段
    /// （不做模型改名、不注入思考等级 —— 后者靠 `reasoning_patch` 的默认
    /// `Skip`，那是「没证据就不注入」的正确默认）。
    ///
    /// ── 为什么要带一整套「客户端身份头」───────────────────────
    /// 编码套餐的入口是**给官方客户端用的**，上游按客户端形态识别请求
    /// （参考实现的 `buildLlmIdentityHeaders` 逐字复刻 bundle 的 `g6n`）。
    /// 只发一个光秃秃的 `Authorization` 也能过鉴权，但上游一旦按形态限流或
    /// 灰度，缺头就是难查的失败 —— 而这一套头是免费的。取值能对上的对上、
    /// 对不上的用参考实现自己的兜底（`unknown`）。
    ///
    /// 两处**故意**与参考不同（别当成漏抄）：
    ///   · 不发 `X-Os-Version`：参考实现取 `os.release()`，Rust 侧要为此引一个
    ///     系统信息 crate；它是可选头，参考实现取不到时同样省略；
    ///   · 不发 `X-Device-Mid`：参考实现明确注明推理路径**从不**发它
    ///     （那是领取路径的活动期要求，见 `claim.rs`）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        // ── 套餐通道分流（账号级字段 `planMode`，缺省 = 编码套餐）────────
        // start-plan 的额度不挂在编码套餐 API Key 上（见 `region::
        // start_plan_anthropic_url` 的说明）：走体验套餐通道时，请求改打
        // zcode.z.ai 的 Anthropic 网关、鉴权用登录 JWT，body 翻成 Anthropic
        // messages —— 回程翻译由编排层按 `protocol` 位接线。
        let plan_mode = account
            .get("planMode")
            .and_then(Value::as_str)
            .unwrap_or("");
        if plan_mode.eq_ignore_ascii_case(PLAN_MODE_START) {
            return self.build_start_plan_request(account, body);
        }
        let token = account
            .get("auth")
            .and_then(|auth| auth.get("accessToken"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(GatewayError::with_status(
                401,
                "ZCode 账号缺少推理凭证，请重新登录",
            ));
        }
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ];
        headers.extend(identity_headers());
        Ok(ChatRequestPlan {
            protocol: ChatUpstreamProtocol::OpenAI,
            url: format!("{}/chat/completions", self.openai_base_url()),
            headers,
            body: body.clone(),
        })
    }

    /// 上游错误分类。
    ///
    ///   - `401` → TokenExpired（编排层会刷新后同账号重试一次；本家当前的
    ///     `refresh_access_token` 会如实报错，见模块头）
    ///   - `429` → QuotaLimited（编码套餐是「5 小时 + 每周」双窗口限额，
    ///     上游不给结构化的恢复时间，`reset_at` 给 None 让冷却走兜底时长）
    ///   - 其余 → 交给共用的内容拦截判定（`content_block`），
    ///     与其余各家同一口径 —— 编码套餐同样会有内容策略拦截
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        content_block::classify_or_fatal(
            status,
            error_body,
            message,
            error_body.get("code").and_then(Value::as_i64),
        )
    }

    /// 取可用令牌：只读账号会话里的 `accessToken`，**不续期**（见模块头）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { session_access_token(store, self.region, account_id) })
    }

    /// 401 后的强制刷新：本家尚未实现续期，如实报错让用户重新登录。
    ///
    /// **不要**在这里回落到「再读一次会话」—— 那正是编排层已经做过的动作，
    /// 返回同一个被拒的 token 会让「刷新后重试一次」退化成一次无意义的重复请求。
    fn refresh_access_token<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move {
            Err(GatewayError::with_status(
                401,
                "ZCode 的登录态无法自动续期，请在「账号」页重新登录该账号",
            ))
        })
    }

    /// 本家没有远程模型目录（静态表，见 `models.rs`）：如实回答「没刷」，
    /// 不假装刷了一次。`supports_model_refresh()` 因此保持默认 false，
    /// 界面上不会给这家渲染「刷新模型清单」按钮。
    fn refresh_models<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    > {
        Box::pin(async move { ModelRefreshOutcome::unchanged() })
    }

    /// 编码套餐的网关会回自己的内部模型名，需要把 SSE 帧里的 `model` 改回
    /// 客户端请求的那个名字（与 raccoon 同一处境、同一处置）。
    fn sse_model_rewrite(&self) -> bool {
        true
    }
}

/// 从账号会话里读访问令牌。
///
/// `account_id` 为空时取本地区组内的当前账号（与 raccoon 的
/// `snapshot_for` 口径一致：非空 = 用户在界面上点名的那条账号，取不到就报错
/// 而不是回落到队首 —— 那会把「点名的账号坏了」变成「静默用了别人的额度」）。
fn session_access_token(
    store: &AccountStore,
    region: Region,
    account_id: &str,
) -> Result<String, GatewayError> {
    // 两个查询返回的是**两个不同的类型**（`CurrentEntry` / `SessionById`），
    // 它们都带一个 `session` 字段 —— 这里只取那一个字段，避免为一个取值
    // 动作引入第三种包装类型。
    let session = if account_id.trim().is_empty() {
        store
            .current_entry_for_provider(region.provider_id())
            .map(|entry| entry.session)
    } else {
        store.get_session_by_id(account_id).map(|entry| entry.session)
    };
    let session = session.ok_or_else(|| {
        GatewayError::with_status(401, "没有可用的 ZCode 账号，请先在「账号」页添加")
    })?;
    session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GatewayError::with_status(401, "ZCode 账号缺少推理凭证，请重新登录"))
}

/// 推理请求上的「客户端身份头」（参考实现 `buildLlmIdentityHeaders` 的移植）。
///
/// 取值口径与那边逐条对齐，`unknown` 是**上游文档化的兜底值**（参考实现自己也
/// 在拿不到语言/时区时发它）：
///   - `User-Agent` / `X-ZCode-App-Version` 用 `claim::app_version()`
///     （`ZCODE_APP_VERSION` 可覆盖，默认 ZCode 客户端版本）；
///   - `X-Platform` 用 `claim::platform()`（`win32-x64` 这类「平台-架构」）；
///   - `X-Title` 的 `@cli` 后缀对应参考实现的 `identity.sourceTitle` 默认值。
///
/// `X-Os-Category` 由编译期平台给出（与 `claim::platform()` 同源口径），
/// 不引系统信息 crate —— 理由见 `build_chat_request` 的注释。
fn identity_headers() -> Vec<(String, String)> {
    let version = super::claim::app_version();
    vec![
        (
            "HTTP-Referer".to_string(),
            "https://zcode.z.ai".to_string(),
        ),
        ("User-Agent".to_string(), format!("ZCode/{version}")),
        ("X-ZCode-App-Version".to_string(), version),
        ("X-Title".to_string(), "Z Code@cli".to_string()),
        ("X-Release-Channel".to_string(), "production".to_string()),
        ("X-Client-Language".to_string(), "unknown".to_string()),
        ("X-Client-Timezone".to_string(), "unknown".to_string()),
        ("X-ZCode-Agent".to_string(), "glm".to_string()),
        (
            "X-Platform".to_string(),
            super::claim::platform().to_string(),
        ),
        ("X-Os-Category".to_string(), os_category().to_string()),
    ]
}

/// `X-Os-Category` 的取值（参考实现 `normalizeOsCategory`：macos / windows /
/// linux，认不出的落 linux —— 与那边 `default` 分支同义）。
fn os_category() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一次编码套餐通道的会话（与 `session_from_record` 对 zcode 系产出的
    /// 形状一致：accessToken 在 auth 下、jwt / planMode 在顶层扩展字段里）
    fn session(plan_mode: Option<&str>, jwt: &str) -> Value {
        let mut session = json!({
            "auth": { "accessToken": "ak-test" },
            "jwt": if jwt.is_empty() { Value::Null } else { Value::String(jwt.to_string()) },
        });
        if let Some(mode) = plan_mode {
            session["planMode"] = Value::String(mode.to_string());
        }
        session
    }

    fn chat_body() -> Value {
        json!({
            "model": "glm-5.3-flash",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        })
    }

    /// 缺省（无 planMode 键）= 编码套餐通道：URL / 协议 / 鉴权头与历史行为
    /// 逐字一致 —— 这条是「老账号零迁移」的钉子。
    #[test]
    fn the_default_plan_mode_keeps_the_coding_route() {
        let plan = ZCODE_ADAPTER.build_chat_request(&session(None, ""), &chat_body(), &HeaderMap::new()).unwrap();
        assert_eq!(
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
            plan.url
        );
        assert_eq!(ChatUpstreamProtocol::OpenAI, plan.protocol);
        let (_, token) = plan
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .expect("缺 Authorization 头");
        assert_eq!("Bearer ak-test", token);
    }

    /// start-plan：改打 zcode.z.ai 的 Anthropic 网关，鉴权换成登录 JWT，
    /// body 翻成 Anthropic messages（model 原样、stream 恒 true）。
    #[test]
    fn start_plan_mode_routes_to_the_plan_gateway() {
        let plan = ZCODE_ADAPTER
            .build_chat_request(&session(Some("start-plan"), "jwt-test"), &chat_body(), &HeaderMap::new())
            .unwrap();
        assert_eq!("https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages", plan.url);
        assert_eq!(ChatUpstreamProtocol::Anthropic, plan.protocol);
        let header = |name: &str| {
            plan.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        assert_eq!("Bearer jwt-test", header("Authorization"), "鉴权用登录 JWT");
        assert_eq!("2023-06-01", header("anthropic-version"), "Anthropic 网关要版本头");
        assert_eq!("glm-5.3-flash", plan.body["model"].as_str().unwrap_or(""));
        assert_eq!(true, plan.body["stream"].as_bool().unwrap_or(false));
        assert!(plan.body["messages"].as_array().is_some_and(|items| !items.is_empty()));
    }

    /// 认不出的 planMode 一律回落编码套餐通道（store 侧已把非法值 400 挡住，
    /// 这里钉的是适配器自身的兜底：手工编辑落进来的脏值不打爆转发）。
    #[test]
    fn an_unknown_plan_mode_falls_back_to_the_coding_route() {
        let plan = ZCODE_ADAPTER
            .build_chat_request(&session(Some("plan-x"), "jwt-test"), &chat_body(), &HeaderMap::new())
            .unwrap();
        assert_eq!(ChatUpstreamProtocol::OpenAI, plan.protocol);
        assert!(plan.url.contains("open.bigmodel.cn"));
    }

    /// 缺 JWT 是用户可修的配置问题：文案要点名去哪补，而不是含混的 401。
    #[test]
    fn start_plan_without_a_jwt_says_what_to_do() {
        let error = ZCODE_ADAPTER
            .build_chat_request(&session(Some("start-plan"), ""), &chat_body(), &HeaderMap::new())
            .err()
            .expect("缺 JWT 该报错");
        assert_eq!(401, error.status_code);
        assert!(error.message.contains("JWT"), "{}", error.message);
        assert!(error.message.contains("重新登录") || error.message.contains("粘贴"), "{}", error.message);
    }
}
