//! Accio 的**地区**与端点常量：两个地区共用一套网关，只有登录站点不同。
//!
//! ── 上游长什么样（逆向来源：Accio Work 桌面端安装包的 app.asar）──────
//! ```text
//!   业务网关（国际 / 国内同一个）  https://phoenix-gw.alibaba.com
//!     /api/oauth/token                 授权码 → accessToken / refreshToken
//!     /api/auth/refresh_token          续期（body 带 accessToken + refreshToken）
//!     /api/auth/userinfo               用户资料（GET，accessToken 走 query）
//!     /api/entitlement/quota           额度用量百分比 + 重置倒计时
//!     /api/entitlement/currentSubscription  订阅详情
//!     /api/llm/config/v2               模型目录（POST，body {token, supportAutoModel}）
//!   推理网关（ADK）                https://phoenix-gw.alibaba.com/api/adk/llm
//!     POST /generateContent?sg_k=<md5(requestId)>   SSE，Gemini 风格信封
//!   登录站点
//!     国际版  https://www.accio.com      （域名 accio.com → 区域 GLOBAL）
//!     国内版  https://www.accio-ai.com   （域名 accio-ai.com → 区域 CN）
//! ```
//!
//! 三个事实决定了本模块的形状（都是实测/读码得到的，不是推测）：
//!   1. **网关是同一个** —— 两个地区的账号打的是同一个 host，差别只在
//!      `x-package-region`（`GLOBAL` / `CN`）与登录站点。因此「这一家的域名」
//!      不能是模块级常量，必须由 `Region` 给。
//!   2. **`client_id` 两地逐字相同**（`accio-work`）：桌面端两个构建共用它。
//!   3. **token 不走 Authorization 头**：业务接口的 GET 把 `accessToken` 拼进
//!      query、POST 放进 JSON body；推理接口把 `token` 放进 body。这一点与
//!      项目里其它七家都不同，见 `credentials.rs` 与 `chat.rs`。
//!
//! ── 为什么地区是一等公民而不是「一家上的一个字段」────────────────
//! 与 `autoclaw::region` / Cline 两池同一条思路：地区做成账号上的字段，
//! 界面与选路就分不清「哪条账号走哪个站点」。两个地区 = 两家 provider
//! （`accio` / `accio-cn`），共用这一份实现。

use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::errors::GatewayError;

/// 业务网关基址（环境变量 `ACCIO_GATEWAY_BASE` 可覆盖，预发/自建代理用）
pub const DEFAULT_GATEWAY_BASE: &str = "https://phoenix-gw.alibaba.com";

/// 推理网关路径前缀（拼在网关基址之后）
pub const ADK_LLM_PATH: &str = "/api/adk/llm";

/// OAuth 授权码换令牌
pub const OAUTH_TOKEN_PATH: &str = "/api/oauth/token";
/// 刷新令牌（多账号场景桌面端用 `/api/auth/safe/refresh_token`，本网关用普通那条）
pub const REFRESH_TOKEN_PATH: &str = "/api/auth/refresh_token";
/// 用户资料
pub const USER_INFO_PATH: &str = "/api/auth/userinfo";
/// 额度用量
pub const QUOTA_PATH: &str = "/api/entitlement/quota";
/// 订阅详情
pub const SUBSCRIPTION_PATH: &str = "/api/entitlement/currentSubscription";
/// 模型目录
pub const MODEL_CONFIG_PATH: &str = "/api/llm/config/v2";

/// OAuth 客户端 id（桌面端常量；两地逐字相同）
pub const CLIENT_ID: &str = "accio-work";

/// ADK 推理请求的租户（桌面端默认值 `ADK_TENANT || "accio-agent"`）
pub const DEFAULT_TENANT: &str = "accio-agent";
/// ADK 推理请求的来源标记（桌面端默认值 `ADK_IAI_TAG || "phoenix-desktop"`）
pub const DEFAULT_IAI_TAG: &str = "phoenix-desktop";
/// 推理请求头里的客户端版本（桌面端取 appVersion；缺省给一个近版号）
pub const DEFAULT_APP_VERSION: &str = "0.32.6";

/// 业务请求的默认语言（桌面端按界面语言给 `zh` / `en`，网关侧固定 en ——
/// 额度与目录的文案不参与界面展示，语言只影响上游返回的消息文本）
pub const ACCEPT_LANGUAGE: &str = "en";

/// Accio 的两个地区。**两个地区 = 两个 provider**（见模块头）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Region {
    /// 国际版（`www.accio.com`，`x-package-region: GLOBAL`）
    Global,
    /// 国内版（`www.accio-ai.com`，`x-package-region: CN`）
    Cn,
}

impl Region {
    /// 全部地区（缓存数组、遍历用）
    pub const ALL: [Region; 2] = [Region::Global, Region::Cn];

    /// 本地区的 provider
    pub const fn kind(self) -> ProviderKind {
        match self {
            Self::Global => ProviderKind::Accio,
            Self::Cn => ProviderKind::AccioCn,
        }
    }

    /// 本地区的 provider id（`"accio"` / `"accio-cn"`）
    pub fn provider_id(self) -> &'static str {
        kind_id(self.kind())
    }

    /// provider id → 地区；不是 Accio 系时 None
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        match provider_id {
            "accio" => Some(Self::Global),
            "accio-cn" => Some(Self::Cn),
            _ => None,
        }
    }

    /// provider kind → 地区；不是 Accio 系时 None
    pub fn from_kind(kind: ProviderKind) -> Option<Self> {
        match kind {
            ProviderKind::Accio => Some(Self::Global),
            ProviderKind::AccioCn => Some(Self::Cn),
            _ => None,
        }
    }

    /// 落进账号记录的 `edition` 值（与其它家同一套取值：`intl` / `cn`）
    pub const fn edition(self) -> &'static str {
        match self {
            Self::Global => "intl",
            Self::Cn => "cn",
        }
    }

    /// 界面与日志里的名字
    pub const fn label(self) -> &'static str {
        match self {
            Self::Global => "国际版",
            Self::Cn => "国内版",
        }
    }

    /// 解析前端传来的 `mode` / `edition` 取值。缺省 = 国际版
    /// （老客户端不带这个字段时行为必须稳定）。
    pub fn parse(value: &str) -> Result<Self, GatewayError> {
        match value.trim() {
            "" | "global" | "intl" => Ok(Self::Global),
            "cn" => Ok(Self::Cn),
            _ => Err(GatewayError::with_status(
                400,
                "Accio 地区必须为 global（国际版）或 cn（国内版）",
            )),
        }
    }

    /// 从账号记录 / 添加请求里取地区（兼容 `mode` 与 `edition` 两个键名）
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, GatewayError> {
        match payload.get("mode").or_else(|| payload.get("edition")) {
            None | Some(serde_json::Value::Null) => Ok(Self::Global),
            Some(serde_json::Value::String(value)) => Self::parse(value),
            _ => Err(GatewayError::with_status(400, "Accio 地区必须是字符串")),
        }
    }

    /// 登录站点（OAuth 授权页所在域；`ACCIO_LOGIN_BASE` 可覆盖，预发用）
    pub fn login_base(self) -> String {
        if let Some(value) = env_text("ACCIO_LOGIN_BASE") {
            return value.trim_end_matches('/').to_string();
        }
        match self {
            Self::Global => "https://www.accio.com".to_string(),
            Self::Cn => "https://www.accio-ai.com".to_string(),
        }
    }

    /// 模型目录与推理请求的 `x-package-region` 头。
    ///
    /// 取值来自桌面端：区域名单就是 `["CN", "GLOBAL"]` 两项
    /// （`domain/config` 的返回与 `packageRegion` 校验都用它）。
    pub const fn package_region(self) -> &'static str {
        match self {
            Self::Global => "GLOBAL",
            Self::Cn => "CN",
        }
    }
}

/// 业务网关基址（环境变量可覆盖；末尾不带斜杠）
pub fn gateway_base() -> String {
    env_text("ACCIO_GATEWAY_BASE")
        .unwrap_or_else(|| DEFAULT_GATEWAY_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// 推理网关基址（`{gw}/api/adk/llm`）
pub fn llm_base() -> String {
    format!("{}{}", gateway_base(), ADK_LLM_PATH)
}

/// 读一个环境变量并去掉空白；空串按「没设」处理
fn env_text(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 业务接口的通用请求头（桌面端在拦截器里统一加的那一组里我们认得全的部分）。
///
/// ── 为什么只带这几项 ────────────────────────────────────────
/// 桌面端还带 `x-source` / `x-deploy-target` / `x-cna` / `bx-ua` 等，
/// 其中 `bx-ua` 由它内置的风控 SDK 生成（本网关没有那个 SDK，值也不可复现），
/// `x-cna` 是设备指纹、`x-deploy-target` 是部署形态标记 —— 都不是鉴权项。
/// **刻意不带 `Authorization`**：桌面端一个请求都不带它，鉴权全靠 body/query
/// 里的 `accessToken`；多一个上游没见过的头只会徒增风控面。
/// 未验证项与实测口径见 `mod.rs` 模块头的「未验证项」一节。
pub fn api_headers(region: Region) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("Accept-Language".to_string(), ACCEPT_LANGUAGE.to_string()),
        ("x-language".to_string(), ACCEPT_LANGUAGE.to_string()),
        ("x-platform".to_string(), "desktop".to_string()),
        ("x-os".to_string(), std::env::consts::OS.to_string()),
        (
            "x-app-version".to_string(),
            env_text("ACCIO_APP_VERSION").unwrap_or_else(|| DEFAULT_APP_VERSION.to_string()),
        ),
        ("x-package-region".to_string(), region.package_region().to_string()),
    ]
}
