//! Qoder 的地区与鉴权端点；地区不接受任意 URL。

use serde_json::Value;

use crate::server::errors::GatewayError;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Global,
    Cn,
}

impl Region {
    pub fn parse(value: &str) -> Result<Self, GatewayError> {
        match value.trim() {
            "" | "global" | "intl" => Ok(Self::Global),
            "cn" => Ok(Self::Cn),
            _ => Err(GatewayError::with_status(400, "Qoder 地区必须为 global（国际版）或 cn（中国版）")),
        }
    }

    pub fn from_payload(payload: &Value) -> Result<Self, GatewayError> {
        let mode = payload.get("mode").or_else(|| payload.get("edition"));
        match mode {
            None | Some(Value::Null) => Ok(Self::Global),
            Some(Value::String(value)) => Self::parse(value),
            _ => Err(GatewayError::with_status(400, "Qoder 地区必须是字符串")),
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Cn => "cn",
        }
    }

    pub fn edition(self) -> &'static str {
        match self {
            Self::Global => "intl",
            Self::Cn => "cn",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "国际版",
            Self::Cn => "中国版",
        }
    }

    pub fn open_api(self) -> &'static str {
        match self {
            Self::Global => "https://openapi.qoder.sh",
            Self::Cn => "https://openapi.qoder.com.cn",
        }
    }

    pub fn center(self) -> &'static str {
        match self {
            Self::Global => "https://center.qoder.sh",
            Self::Cn => "https://gateway.qoder.com.cn",
        }
    }

    /// 网页门户基址（登录页、账号设置页所在的那台主机）。
    ///
    /// 与 [`Self::open_api`] 是两台不同的主机：门户负责「人看的页面」，
    /// openapi 负责「机器调的接口」，设备授权恰好两边都用（授权页在门户、
    /// 轮询在 openapi）。
    pub fn web_origin(self) -> &'static str {
        match self {
            Self::Global => "https://qoder.com",
            Self::Cn => "https://qoder.com.cn",
        }
    }

    /// 设备授权页的完整地址（含 [`DEVICE_LOGIN_PATH`]）。
    pub fn device_login_url(self) -> String {
        format!("{}{}", self.web_origin(), DEVICE_LOGIN_PATH)
    }

    /// 推理网关基址（`/algo/...` 这一族的根）。
    ///
    /// 它同时承载模型目录（`algo/api/v2/model/list`）与对话
    /// （`algo/api/v2/service/pro/sse/agent_chat_generation`）——
    /// 两者走同一套 COSY 签名（见 `cosy.rs`），所以共用这一个基址。
    pub fn gateway(self) -> &'static str {
        match self {
            Self::Global => "https://api3.qoder.sh/",
            Self::Cn => "https://gateway.qoder.com.cn/",
        }
    }
}

/// 设备授权页路径。**两站同名同参**：只有主机名不同（`qoder.com` / `qoder.com.cn`），
/// 实测 `challenge` / `challenge_method` / `machine_id` / `nonce` 四个 query 参数
/// 两站都认，未登录时都 302 到各自的 `/users/sign-in?oauth_callback=…`。
pub const DEVICE_LOGIN_PATH: &str = "/device/selectAccounts";
pub const DEVICE_POLL_PATH: &str = "/api/v1/deviceToken/poll";
/// 设备流（`drt-`）刷新令牌的续期端点，与 [`DEVICE_POLL_PATH`] 同族同主机
/// （都在 [`Region::open_api`] 上）。请求体是 snake_case 的 `refresh_token`，
/// 响应里访问令牌在 `device_token`。
pub const DEVICE_REFRESH_PATH: &str = "/api/v1/deviceToken/refresh";
pub const EXCHANGE_PATH: &str = "/api/v1/jobToken/exchange";
pub const USER_INFO_PATH: &str = "/api/v1/userinfo";
pub const USAGE_PATH: &str = "/api/v2/quota/usage";
/// 非设备流（既不是 `pat|` 也不是 `drt-`）的续期端点，走 [`Region::center`]。
/// 设备令牌打到这里会被 403 `Request discarded`，所以 `refresh.rs` 先按前缀
/// 分流到 [`DEVICE_REFRESH_PATH`]，这里只兜剩下的那一族。
pub const REFRESH_PATH: &str = "/algo/api/v3/user/refresh_token";

pub fn open_api_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Cosy-Version".to_string(), "1.0.1".to_string()),
        ("Cosy-ClientType".to_string(), "5".to_string()),
        ("User-Agent".to_string(), "qoder-local-proxy".to_string()),
    ];
    if let Some(token) = token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}
