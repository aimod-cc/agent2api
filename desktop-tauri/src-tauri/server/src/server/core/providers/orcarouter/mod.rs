//! OrcaRouter：一个 OpenAI 兼容的聚合网关（多家模型在同一个端点之后）。
//!
//! ── 这一家与其它家的根本差别：**凭据只有一种，入口有两种** ────────
//! 其余各家的令牌有自己的生命周期（JWT 过期、refreshToken 轮换、设备绑定），
//! 因此各自带一套续期与导入链路。OrcaRouter 不是：
//!   - 用户手填的 API Key（`sk-orca-…`）与
//!   - 由 **OAuth 2.0 + PKCE** 网页登录换回来的 Key
//! **是同一件东西**（都是一把属于用户账号的长期 API Key，记账在用户头上、
//! 可在控制台随时吊销，见 `login.rs` 的模块头）。它没有 refresh grant，
//! 也不该被当成 refresh token 用 —— 被吊销只能重新走一次登录。
//!
//! 于是账号层只需要**一个**凭据入口（`account_store::orcarouter_accounts`），
//! 两条获取路径（粘贴 / PKCE）在它上面收敛；转发、目录、模型下拉都只认
//! 账号记录里的 apiKey，**不关心它是哪条路来的**（这是本集成的核心不变量，
//! 回归测试逐条盯着它）。
//!
//! ── 两个 origin，绝不互相推导（规范里反复强调的那一条）─────────
//!   · **认证**：默认 `https://www.orcarouter.ai`，授权页固定 `/auth`，
//!     换码固定 `/api/v1/auth/keys`；
//!   · **推理与模型目录**：默认 `https://api.orcarouter.ai/v1`。
//! `https://api.orcarouter.ai/v1/auth/keys` 是 404 —— 换码接口**不在** `/v1`
//! 底下。因此本模块把两个 origin 分别持有（[`Endpoints`]），任何一处都**不得**
//! 靠替换 hostname 或顺手拼 `/v1` 推导另一个。
//!
//! 自建部署可以只给一个共享地址，也可以用两个独立地址；显式覆盖优先：
//!   · `ORCA_BASE_URL`       两个 origin 共用的兜底；
//!   · `ORCA_AUTH_BASE_URL`  只覆盖认证 origin（优先于共享值）；
//!   · `ORCA_API_BASE_URL`   只覆盖推理/目录 origin（优先于共享值）。
//! 远程 origin 强制 HTTPS；HTTP 只放行 loopback（本机自建调试）。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   credentials.rs 账号记录的读写形状（apiKey / 备注名 / 生成方式）
//!   catalog.rs     模型目录：`GET {api}/models` 的解析、能力过滤与兜底种子
//!   login.rs       PKCE（S256）授权码流程：authorize URL、换码、pending 表
//!   adapter.rs     ProviderAdapter 实现（Bearer 透传 + 目录刷新 + 终端 401）
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块绝不 unwrap/expect/panic；不持锁穿越 await。

pub mod adapter;
pub mod catalog;
pub mod credentials;
pub mod login;

/// 注册表用的适配器静态实例（`adapter::adapter_for` 按 kind 给出它）。
pub use adapter::ORCAROUTER_ADAPTER;

use serde_json::{json, Value};

/// 官方认证 origin（授权页与换码）。
pub const DEFAULT_AUTH_BASE: &str = "https://www.orcarouter.ai";

/// 官方推理 origin（含 `/v1`；模型目录与对话都挂在它底下）。
pub const DEFAULT_API_BASE: &str = "https://api.orcarouter.ai/v1";

/// 用户手填 Key 的形态前缀。**只是形状校验**（挡粘贴错误），不是凭据有效性的
/// 证明 —— 规范明确说前缀不构成「能用」的判据，真正的有效性由第一次真实请求
/// 确立（本模块因此**不**发任何探测请求去「验一下」）。
pub const API_KEY_PREFIX: &str = "sk-orca-";

/// 控制台里的密钥管理页（账号卡片上的「管理密钥」链接）。
pub const KEY_MANAGEMENT_URL: &str = "https://www.orcarouter.ai/console/token";

/// 控制台里的「已授权应用」页。用户在这里一键吊销本应用签发的**全部** Key
/// （见规范「Revocation」一节），因此 401 的正确处置是重新登录而不是重试。
pub const AUTHORIZED_APPS_URL: &str = "https://www.orcarouter.ai/console/authorized-apps";

/// 本家两个 origin 的解析结果。
///
/// 刻意做成**两个独立字段**而不是一个 base + 路径开关：规范里最容易犯的错就是
/// 「把认证端点拼到推理 origin 底下」（`/v1/auth/keys` 恒 404）。两处分开持有
/// 之后，拼错需要显式写错一个字段名，而不是顺手少一次替换。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// 授权与换码的 origin（默认 `https://www.orcarouter.ai`）
    pub auth_base: String,
    /// 推理与模型目录的 origin（默认 `https://api.orcarouter.ai/v1`）
    pub api_base: String,
}

impl Endpoints {
    /// 从两个已归一的 origin 构造（调用方保证已过 [`normalize_origin`]）。
    pub fn new(auth_base: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            auth_base: auth_base.into(),
            api_base: api_base.into(),
        }
    }

    /// 授权页地址（**路径固定 `/auth`**，浏览器直接打开）。
    pub fn authorize_endpoint(&self) -> String {
        format!("{}/auth", self.auth_base)
    }

    /// 换码地址（**固定 `/api/v1/auth/keys`**，注意不是 `/v1/auth/keys`）。
    pub fn exchange_endpoint(&self) -> String {
        format!("{}/api/v1/auth/keys", self.auth_base)
    }

    /// 模型目录地址（`{api}/models`）。
    pub fn models_endpoint(&self) -> String {
        format!("{}/models", self.api_base)
    }

    /// 对话补全地址（`{api}/chat/completions`）。
    pub fn chat_endpoint(&self) -> String {
        format!("{}/chat/completions", self.api_base)
    }
}

/// 当前生效的两个 origin（读环境变量；显式覆盖优先于共享兜底）。
///
/// 取值顺序逐条对应规范「Make both origins configurable」：
///   1. `ORCA_AUTH_BASE_URL` / `ORCA_API_BASE_URL`（各自的显式覆盖，最高优先）；
///   2. `ORCA_BASE_URL`（自建部署一个地址两个平面时的共享兜底）；
///   3. 官方默认值。
///
/// 覆盖值非法（非 HTTPS 的远程地址、解析不出的串）时**回落到默认值**并打一条
/// 日志：登录/转发都不该因为一个环境变量拼错而整个不可用，但静默降级也不行
/// —— 用户会以为覆盖生效了。
pub fn endpoints() -> Endpoints {
    endpoints_with(|name| std::env::var(name).ok())
}

/// [`endpoints`] 的可注入版本（测试用假变量表，不碰进程环境）。
fn endpoints_with<F>(get: F) -> Endpoints
where
    F: Fn(&str) -> Option<String>,
{
    let shared = get("ORCA_BASE_URL").and_then(|raw| resolve_origin("ORCA_BASE_URL", &raw));
    let auth = get("ORCA_AUTH_BASE_URL")
        .and_then(|raw| resolve_origin("ORCA_AUTH_BASE_URL", &raw))
        .or_else(|| shared.clone())
        .unwrap_or_else(|| DEFAULT_AUTH_BASE.to_string());
    let api = get("ORCA_API_BASE_URL")
        .and_then(|raw| resolve_origin("ORCA_API_BASE_URL", &raw))
        .or_else(|| shared)
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
    Endpoints::new(auth, api)
}

/// 归一一个 origin 覆盖值：合法给 Some，非法给 None（调用方回落默认值）。
fn resolve_origin(name: &str, raw: &str) -> Option<String> {
    match normalize_origin(raw) {
        Ok(origin) => Some(origin),
        Err(reason) => {
            crate::server::logging::log(
                "[Provider]",
                &format!("忽略非法的 {name}（{reason}），回落到 OrcaRouter 官方地址"),
            );
            None
        }
    }
}

/// origin 归一与安全校验。
///
/// 规则来自规范的网络策略一节：
///   - 去掉首尾空白与结尾的 `/`（否则拼出来的路径会出现 `//auth`）；
///   - 必须是合法 URL 且有 host；
///   - **远程强制 HTTPS**；`http://` 只放行 loopback（`127.0.0.1` / `localhost`
///     / `[::1]`）—— 本机自建调试用，公网明文传输密钥一律拒绝；
///   - 不接受带 userinfo（`user:pass@`）的地址：那会把凭据写进日志与报错。
///
/// 路径前缀允许保留（自建实例挂在 `/gateway` 这类前缀下时 `{base}/auth` 仍然成立）。
pub fn normalize_origin(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("地址为空".to_string());
    }
    let parsed = url::Url::parse(trimmed).map_err(|error| format!("不是合法 URL: {error}"))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed
        .host_str()
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| "缺少主机名".to_string())?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("地址里不允许携带用户名/密码".to_string());
    }
    if scheme != "https" {
        let loopback = host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host == "[::1]";
        if scheme != "http" || !loopback {
            return Err("远程地址必须使用 HTTPS（HTTP 仅允许 127.0.0.1 / localhost）".to_string());
        }
    }
    // 交给 Url 做一次规范化（默认端口、大小写主机名），再去掉结尾斜杠
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

/// 账号公开形态里的「推理基址」——界面把它显示成只读信息，让用户看得见请求
/// 会发到哪儿（换 origin 自建时尤其重要）。
pub fn api_base_label() -> String {
    endpoints().api_base
}

/// 本家支持的两种**凭据获取方式**（与账号记录里的 `source` 字段同值）。
///
/// 集中在这里而不是在账号层各处写字符串：界面、日志、测试三处必须认同一个
/// 取值集合，写错一处不会报错，只会让某个入口在界面上「看起来没用过」。
pub const SOURCE_MANUAL: &str = "manual";
/// PKCE 网页登录换回来的凭据（见 `login.rs`）。
pub const SOURCE_PKCE: &str = "oauth";

/// 一次真实上游请求失败时给用户的**可操作**提示（账号层面的 401）。
///
/// 与 `core::disconnect_guard` 的分工：那个负责把「哪条账号的哪一代凭证被拒」
/// 精确标记成 `needsReauth`（多账号场景不许串味），本函数只负责把那件事翻译成
/// 人话。两个都要有：只标记不说话，用户只看到一条 401；只说话不标记，界面会
/// 继续拿死凭据打上游。
pub fn reauth_hint() -> String {
    format!(
        "OrcaRouter 凭据已被上游拒绝（401）：密钥可能已在控制台吊销。\
         请在账号卡片上重新「Connect with OrcaRouter」或粘贴一把新 Key；\
         已吊销的旧凭据不会被自动刷新，只在此处标记为需要重新授权。\
         吊销入口：{AUTHORIZED_APPS_URL}"
    )
}

/// 账号卡片的补充信息（前端「账号信息」区展示；键名与各家同形）。
pub fn account_meta_json(source_label: &str, key_tail: &str) -> Value {
    json!({
        "sourceLabel": source_label,
        "tokenTail": key_tail,
        "keyManagementUrl": KEY_MANAGEMENT_URL,
        "authorizedAppsUrl": AUTHORIZED_APPS_URL,
    })
}

#[cfg(test)]
mod tests {
    //! 两个 origin 的**分离**是本模块最要紧的一条不变量：认证只去
    //! `www.orcarouter.ai`、推理与目录只去 `api.orcarouter.ai/v1`，任何一处
    //! 靠替换 hostname / 顺手拼 `/v1` 推导另一个都必须在这里被拦住。
    //! 环境变量用可注入的 `endpoints_with` 喂进来，不碰进程环境（多线程测试
    //! 下 `set_var` 会串味）。
    use super::*;

    fn from_pairs(pairs: &[(&str, &str)]) -> Endpoints {
        let table: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        endpoints_with(|name| {
            table
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    #[test]
    fn defaults_are_the_two_official_origins_and_never_derive_one_from_the_other() {
        let endpoints = from_pairs(&[]);
        assert_eq!(endpoints.auth_base, "https://www.orcarouter.ai");
        assert_eq!(endpoints.api_base, "https://api.orcarouter.ai/v1");
        assert_eq!(
            endpoints.authorize_endpoint(),
            "https://www.orcarouter.ai/auth"
        );
        assert_eq!(
            endpoints.exchange_endpoint(),
            "https://www.orcarouter.ai/api/v1/auth/keys"
        );
        assert_eq!(
            endpoints.models_endpoint(),
            "https://api.orcarouter.ai/v1/models"
        );
        assert_eq!(
            endpoints.chat_endpoint(),
            "https://api.orcarouter.ai/v1/chat/completions"
        );
        // 反例（规范点名的那一条）：换码**不在**推理 origin 的 `/v1/auth/keys` 底下
        assert_ne!(
            endpoints.exchange_endpoint(),
            format!("{}/auth/keys", endpoints.api_base)
        );
        assert!(!endpoints.exchange_endpoint().starts_with(&endpoints.api_base));
        assert!(!endpoints.authorize_endpoint().contains("api.orcarouter.ai"));
        assert!(!endpoints.models_endpoint().contains("www.orcarouter.ai"));
    }

    #[test]
    fn explicit_overrides_win_over_the_shared_base() {
        let shared = from_pairs(&[("ORCA_BASE_URL", "https://gw.internal.example")]);
        assert_eq!(shared.auth_base, "https://gw.internal.example");
        assert_eq!(shared.api_base, "https://gw.internal.example");

        let split = from_pairs(&[
            ("ORCA_BASE_URL", "https://shared.example"),
            ("ORCA_AUTH_BASE_URL", "https://auth.example/"),
            ("ORCA_API_BASE_URL", "https://api.example/v1"),
        ]);
        assert_eq!(split.auth_base, "https://auth.example", "显式 auth 覆盖优先于共享值，且去尾斜杠");
        assert_eq!(split.api_base, "https://api.example/v1");
        assert_eq!(split.exchange_endpoint(), "https://auth.example/api/v1/auth/keys");
        assert_eq!(split.models_endpoint(), "https://api.example/v1/models");

        // 只覆盖 auth：推理仍回落共享值
        let auth_only = from_pairs(&[
            ("ORCA_BASE_URL", "https://shared.example"),
            ("ORCA_AUTH_BASE_URL", "https://auth.example"),
        ]);
        assert_eq!(auth_only.api_base, "https://shared.example");
    }

    #[test]
    fn illegal_override_falls_back_to_the_official_origin() {
        // 非法值（远程明文 HTTP / 带 userinfo / 空）一律回落官方地址，
        // 而不是把一个会明文传密钥的地址用于登录
        for bad in [
            "http://evil.example",
            "https://user:pass@www.orcarouter.ai",
            "not a url",
            "   ",
        ] {
            let endpoints = from_pairs(&[("ORCA_AUTH_BASE_URL", bad)]);
            assert_eq!(
                endpoints.auth_base, DEFAULT_AUTH_BASE,
                "非法覆盖「{bad}」必须回落到官方 auth origin"
            );
        }
    }

    #[test]
    fn remote_requires_https_and_loopback_http_is_allowed_for_self_hosted() {
        assert_eq!(
            normalize_origin("https://www.orcarouter.ai/").expect("合法"),
            "https://www.orcarouter.ai"
        );
        assert!(normalize_origin("http://127.0.0.1:8080").is_ok(), "本机自建调试允许明文");
        assert!(normalize_origin("http://localhost:8080").is_ok());
        assert!(normalize_origin("http://[::1]:8080").is_ok());
        assert!(normalize_origin("http://10.0.0.5:8080").is_err(), "私网远程也不许明文");
        assert!(normalize_origin("ftp://www.orcarouter.ai").is_err());
        assert!(normalize_origin("https://u:p@www.orcarouter.ai").is_err());
    }

    #[test]
    fn credential_sources_are_the_two_named_values() {
        assert_eq!(SOURCE_MANUAL, "manual");
        assert_eq!(SOURCE_PKCE, "oauth");
        // 401 的提示必须给出**可操作**的出口（吊销页），且不含任何凭据
        let hint = reauth_hint();
        assert!(hint.contains(AUTHORIZED_APPS_URL));
        assert!(hint.contains("401"));
    }
}
