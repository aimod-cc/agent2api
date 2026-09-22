//! 端点与版本常量（对照 Node 版 src/workbuddy-endpoints.mjs 全量移植）。
//!
//! 这是整个后端的「唯一事实来源」：登录/账号接口带 prefixPath（`/v2/plugin/...`），
//! 计费/签到/LLM 接口不带（直接 `/v2/...`），两版（国内/国际）协议一致、
//! 仅端点与客户端身份不同。
//!
//! 本进程没有 CLI 参数，`WORKBUDDY_*` 环境变量承担 Node 版 `--edition/--endpoint`
//! 的角色（壳侧不传这些，所以实际就是「国内版 + 官方端点」）：
//!
//!   WORKBUDDY_EDITION      cn（默认）/ intl
//!   WORKBUDDY_ENDPOINT     覆盖端点（staging、自建反向代理等场景）
//!   WORKBUDDY_PREFIX_PATH  覆盖鉴权前缀
//!
//! ── 端点表（AUTH/LLM/BILLING/…）为什么用有序切片 ──
//! `/api/endpoints` 要把整张表原样吐给前端（排查用），键名与 Node 版对象键一致。
//! 用 `&[(&str, EndpointSpec)]` 而不是 HashMap，是为了让 JSON 键顺序稳定 ——
//! 同一份配置每次请求输出一致，比对 diff 时不会被顺序噪音干扰。

use std::path::PathBuf;

use serde_json::{json, Map, Value};

// ─── 端点与版本 ─────────────────────────────────────────────

/// 默认端点（国内版）
pub const DEFAULT_ENDPOINT: &str = "https://copilot.tencent.com";
/// 预发布端点
pub const STAGING_ENDPOINT: &str = "https://staging-copilot.tencent.com";
/// 国际版端点（CodeBuddy 海外站，prefixPath 为空）
pub const GLOBAL_ENDPOINT: &str = "https://www.codebuddy.ai";

/// 鉴权相关域名白名单（cli/product.json → authentication.attributes）。
/// internal 走国内端点，external 走海外端点，ioa 为企业 SSO 中转域名。
// Node 版 workbuddy-endpoints.mjs 同名导出 DOMAINS（internal/external/ioa 三段），
// 这里拆成三个常量：端点表事实来源的完整性保留，改端点时三处域名单一并核对。
#[allow(dead_code)]
pub const DOMAINS_INTERNAL: &[&str] = &[
    "copilot.tencent.com",
    "staging-copilot.tencent.com",
    "www.codebuddy.cn",
    "staging.codebuddy.cn",
    "www.workbuddy.cn",
    "staging.workbuddy.cn",
];
/// 海外站域名（DOMAINS.external）
#[allow(dead_code)]
pub const DOMAINS_EXTERNAL: &[&str] = &["www.codebuddy.ai", "staging-codebuddy.tencent.com"];
/// 企业 SSO 中转域名（DOMAINS.ioa）
#[allow(dead_code)]
pub const DOMAINS_IOA: &[&str] = &[
    "tencent.sso.copilot.tencent.com",
    "tencent.sso.copilot-staging.tencent.com",
    "tencent.sso.codebuddy.cn",
    "tencent.staging-sso.codebuddy.cn",
];

/// 桌面端版本（X-IDE-Version / User-Agent 用）
pub const WORKBUDDY_CLIENT_VERSION: &str = "5.5.4";
/// 产品名
// Node 版 workbuddy-endpoints.mjs 同名导出 WORKBUDDY_PRODUCT 的对等物，保留标注
#[allow(dead_code)]
pub const WORKBUDDY_PRODUCT: &str = "WorkBuddy";
/// 桌面端内置 CLI 版本（User-Agent 的 `CLI/<版本>` 段）。
///
/// 服务端按该段识别桌面 CLI 通道：缺失时 `/v3/config` 只下发旧版兼容模型清单
/// （37 个，无 v4.1/5.3/hy4/modelPromotions），带上才下发完整清单（51 个）。
pub const WORKBUDDY_CLI_VERSION: &str = "2.137.1";
/// 鉴权 platform 参数（auth/state?platform=…）
pub const WORKBUDDY_PLATFORM: &str = "workbuddy";
/// 鉴权路径前缀（国内版 /plugin）
pub const DEFAULT_PREFIX_PATH: &str = "/plugin";
/// 默认版本 id
pub const DEFAULT_EDITION: &str = "cn";
/// 管理/计费接口成功码
pub const RESPONSE_CODE_OK: i64 = 0;

/// 登录轮询「token 未就绪」的业务码（auth.token 的 retryCode）
pub const SERVER_CODES_RETRY_FETCH_TOKEN: i64 = 11217;
/// 登录轮询「账号未就绪」的业务码（auth.account 的 retryCode）
pub const SERVER_CODES_RETRY_FETCH_ACCOUNT: i64 = 12151;
/// 上游错误码（桌面端 ServerErrorCode）。
/// 本切片只登记登录轮询用到的两个；切片 4（转发/模型目录）会加 11101 / 11128。
// Node 版 workbuddy-endpoints.mjs 同名导出 SERVER_CODES 的对等物：这里单独登记
// 各个码值，整张表只在排障（/api/endpoints 之外的对照）时用，保留标注。
#[allow(dead_code)]
pub const SERVER_CODES: &[(&str, i64)] = &[
    ("RetryFetchToken", SERVER_CODES_RETRY_FETCH_TOKEN),
    ("RetryFetchAccount", SERVER_CODES_RETRY_FETCH_ACCOUNT),
    ("LicenseSeatLimit", 12005),
    ("LicenseExpired", 11212),
    ("TrialExpired", 11216),
    ("IpLimit", 10081),
    ("NonStreamNotSupported", 11101),
    ("RateLimited", 11128),
];

// ─── 版本（edition）配置 ────────────────────────────────────

/// 一个版本（国内版 / 国际版）的完整身份。
#[derive(Clone, Copy, Debug)]
pub struct EditionInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub endpoint: &'static str,
    pub staging_endpoint: &'static str,
    pub prefix_path: &'static str,
    /// 鉴权 platform 参数（auth/state?platform=…）
    pub platform: &'static str,
    /// UA 里的客户端段（两版都是 WorkBuddy）
    pub ua_platform: &'static str,
    /// UA 里的产品段（国际版是 WorkBuddy AI）
    pub product_name: &'static str,
    pub client_version: &'static str,
    pub cli_version: &'static str,
    /// 客户端数据目录名（本进程不使用，仅 /api/endpoints 展示与排查）
    pub data_folder_name: &'static str,
}

/// 国内版 WorkBuddy：copilot.tencent.com / platform workbuddy
const EDITION_CN: EditionInfo = EditionInfo {
    id: "cn",
    label: "国内版",
    endpoint: DEFAULT_ENDPOINT,
    staging_endpoint: STAGING_ENDPOINT,
    prefix_path: DEFAULT_PREFIX_PATH,
    platform: "workbuddy",
    ua_platform: "WorkBuddy",
    product_name: "WorkBuddy",
    client_version: WORKBUDDY_CLIENT_VERSION,
    cli_version: WORKBUDDY_CLI_VERSION,
    data_folder_name: ".workbuddy",
};

/// 国际版 WorkBuddy AI：www.workbuddy.ai / platform workbuddy-ai
const EDITION_INTL: EditionInfo = EditionInfo {
    id: "intl",
    label: "国际版",
    endpoint: "https://www.workbuddy.ai",
    staging_endpoint: "https://staging-codebuddy.tencent.com",
    prefix_path: DEFAULT_PREFIX_PATH,
    platform: "workbuddy-ai",
    ua_platform: "WorkBuddy",
    product_name: "WorkBuddy AI",
    client_version: "5.5.2",
    cli_version: WORKBUDDY_CLI_VERSION,
    data_folder_name: ".workbuddy-ai",
};

/// 全部版本，顺序与 Node 版 EDITIONS 的键顺序一致（cn → intl）
pub const EDITIONS: &[EditionInfo] = &[EDITION_CN, EDITION_INTL];

/// 宽松解析版本 id：接受 cn/intl 及若干常见写法，未知值回落国内版
/// （对照 Node 版 `resolveEdition`，别名名单逐字保留）。
pub fn resolve_edition(value: Option<&str>) -> &'static EditionInfo {
    let id = value.unwrap_or("").trim().to_lowercase();
    if id.is_empty() {
        return &EDITION_CN;
    }
    if id == "cn" {
        return &EDITION_CN;
    }
    if id == "intl" {
        return &EDITION_INTL;
    }
    if matches!(
        id.as_str(),
        "international" | "global" | "ai" | "workbuddy-ai" | "workbuddyai" | "en"
    ) {
        return &EDITION_INTL;
    }
    if matches!(
        id.as_str(),
        "china" | "mainland" | "workbuddy" | "zh" | "cn-intl"
    ) {
        return &EDITION_CN;
    }
    &EDITION_CN
}

/// 带 CLI 扩展段的完整 UA。
///
/// 产品名/客户端版本按版本区分（国际版 v5.5.2 / WorkBuddy AI），
/// platform 段两版都用 `WorkBuddy`，末尾固定追加 `CLI/<版本>`。
pub fn user_agent_for_edition(edition: Option<&str>) -> String {
    let info = resolve_edition(edition);
    format!(
        "{ua}/{version} {product}/{version} CLI/{cli}",
        ua = info.ua_platform,
        version = info.client_version,
        product = info.product_name,
        cli = info.cli_version,
    )
}

/// 去掉端点末尾的斜杠（对应 Node 版 `normalizeEndpoint`）
pub fn normalize_endpoint(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return DEFAULT_ENDPOINT.to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

/// 按端点推断鉴权前缀（海外站点不带 /plugin，对照 Node 版 `defaultPrefixPath`）
pub fn default_prefix_path(endpoint: &str) -> &'static str {
    let host = normalize_endpoint(endpoint);
    if host == GLOBAL_ENDPOINT || host.starts_with("https://staging-codebuddy") {
        ""
    } else {
        DEFAULT_PREFIX_PATH
    }
}

// ─── 请求上下文 ─────────────────────────────────────────────

/// 一个请求上下文：`{ baseUrl, prefix, platform, edition }`。
///
/// prefix 优先级与 Node 版 `makeContext` 一致：
///   显式 prefixPath > 显式版本的 prefixPath > 端点推断。
#[derive(Clone, Debug)]
pub struct Context {
    pub base_url: String,
    pub prefix: String,
    pub platform: String,
    pub edition: String,
}

impl Context {
    /// 显式构造上下文；`prefix_path` 为 None 时按「版本已显式给出则用它，
    /// 否则按端点推断」（Node 版用 `explicitEdition` 布尔区分两种来源）。
    ///
    /// `endpoint` 为 None/空时回落到版本的官方端点。
    pub fn make(
        endpoint: Option<&str>,
        prefix_path: Option<&str>,
        platform: Option<&str>,
        edition: Option<&str>,
    ) -> Context {
        let explicit_edition = edition.map(|value| !value.trim().is_empty()).unwrap_or(false);
        let info = resolve_edition(edition);
        let base_url = match endpoint.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => normalize_endpoint(value),
            None => info.endpoint.to_string(),
        };
        let prefix = match prefix_path {
            Some(value) => value.to_string(),
            None if explicit_edition => info.prefix_path.to_string(),
            None => default_prefix_path(&base_url).to_string(),
        };
        Context {
            base_url,
            prefix,
            platform: platform
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(info.platform)
                .to_string(),
            edition: info.id.to_string(),
        }
    }

    /// 鉴权/账号接口基址：`{endpoint}/v2{prefixPath}`
    pub fn auth_api_base(&self) -> String {
        format!("{}/v2{}", self.base_url, self.prefix)
    }

    /// 对话接口地址：`{endpoint}/v2/chat/completions`（不带 prefixPath）
    pub fn chat_api_url(&self) -> String {
        format!("{}/v2/chat/completions", self.base_url)
    }

    /// 带 prefixPath 的鉴权接口 URL（`/v2/plugin/...`）
    pub fn auth_url(&self, path: &str) -> String {
        format!("{}/v2{}{}", self.base_url, self.prefix, path)
    }

    /// 端点 authority（`host`），解析失败时为空串。
    /// 对应 Node 版 `safeAuthority`：`auth.domain` 缺省时用它兜底 X-Domain。
    pub fn authority(&self) -> String {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_string))
            .unwrap_or_default()
    }
}

/// 环境变量取非空字符串
fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 本进程的默认上下文：版本来自 WORKBUDDY_EDITION，端点/前缀可被环境变量覆盖。
pub fn default_context() -> Context {
    let edition = env_text("WORKBUDDY_EDITION");
    let endpoint = env_text("WORKBUDDY_ENDPOINT");
    let prefix = env_text("WORKBUDDY_PREFIX_PATH");
    Context::make(endpoint.as_deref(), prefix.as_deref(), None, edition.as_deref())
}

/// 旧版单账号登录态文件（仅用于首次启动迁移到账号列表，见 account_store）
pub fn legacy_auth_file() -> PathBuf {
    crate::server::config::config_dir().join("auth.json")
}

// ─── 匿名请求头 ─────────────────────────────────────────────

/// 匿名登录请求的跳过鉴权头（对照 `anonymousHeaders()`）
pub const ANONYMOUS_HEADERS: &[(&str, &str)] = &[
    ("X-No-Authorization", "true"),
    ("X-No-User-Id", "true"),
    ("X-No-Enterprise-Id", "true"),
    ("X-No-Department-Info", "true"),
];

// ─── 端点表 ─────────────────────────────────────────────────
//
// 这几张表**逐字段复刻 Node 版的 JSON 输出**，包括那些看起来像缺陷的细节：
//   - `path` 在 Node 里是函数（要拼 prefixPath）→ `JSON.stringify` 会**整个丢掉**
//     这个键。所以 auth.state / auth.token 这些条目的响应里没有 path，
//     而 llm.chatCompletions / billing.* 这类写死字符串的才有。
//     这不是笔误，是真实契约（前端与排障脚本看到的就是这样）。
//   - `query` / `body` 是对象（`{ platform: 'workbuddy' }`），不是字符串。
//   - `anonymous` / `whitelistHeaders` 是布尔，`retryCode` 是数字。
// 因此这里直接用 json! 字面量按 Node 的对象字面量顺序（method → path → 其余）
// 构造，而不是用「全字符串」的中间结构再转换。

/// 鉴权端点表（登录/账号接口都带 prefixPath，因此 path 在 Node 里是函数、已被丢弃）
pub fn auth_endpoints() -> Value {
    json!({
        "state": {
            "method": "POST",
            "query": { "platform": WORKBUDDY_PLATFORM },
            "anonymous": true,
            "note": "匿名：带 X-No-Authorization / X-No-User-Id / X-No-Enterprise-Id / X-No-Department-Info",
        },
        "token": {
            "method": "GET",
            "query": { "state": "<state>" },
            "anonymous": true,
            "retryCode": SERVER_CODES_RETRY_FETCH_TOKEN,
        },
        "account": {
            "method": "GET",
            "query": { "state": "<state>" },
            "retryCode": SERVER_CODES_RETRY_FETCH_ACCOUNT,
        },
        "refresh": {
            "method": "POST",
            "headers": { "X-Auth-Refresh-Source": "plugin" },
            "body": {},
            "note": "refreshToken 放在 X-Refresh-Token 头，不在 body",
        },
        "accounts": {
            "method": "GET",
            "note": "返回 data.accounts[]，含 uid / nickname / enterpriseId / type / pluginEnabled",
        },
        // 个人版走 /login/enterprise，企业版 /login/enterprise/{enterpriseId}
        "switchAccount": { "method": "POST", "body": {} },
        // 微信侧登录（tmpCode 换 token），WorkBuddy 桌面端不用，保留备查
        "wxToken": { "method": "POST", "query": { "tmpCode": "<code>" } },
    })
}

/// LLM 端点表（不带 prefixPath，path 是写死的字符串）
pub fn llm_endpoints() -> Value {
    json!({
        "chatCompletions": {
            "method": "POST",
            "path": "/v2/chat/completions",
            "accept": "text/event-stream",
            "note": "body 为 OpenAI 兼容结构；上游仅支持 stream:true（stream:false 会返回 code=11101）",
        },
    })
}

/// 计费 / 积分 / 签到端点表（不带 prefixPath）
pub fn billing_endpoints() -> Value {
    json!({
        "checkinStatus": {
            "method": "POST",
            "path": "/v2/billing/meter/checkin-activity-status",
            "body": {},
            "note": "AuthService.getCheckinStatus()；成功为 code===0 且 data 非空",
        },
        "dailyCheckin": {
            "method": "POST",
            "path": "/v2/billing/meter/daily-checkin",
            "body": {},
            "note": "AuthService.claimDailyCheckin()；返回 { code, msg, requestId }",
        },
        "userResource": {
            "method": "POST",
            "path": "/v2/billing/meter/get-user-resource",
            "body": {
                "PageNumber": 1,
                "PageSize": 100,
                "ProductCode": "p_tcaca",
                "Status": [0, 3],
                "OnlyValidPeriod": true,
            },
            "note": "AuthService.getPersonalUsage()；结果路径 data.Response.Data.Accounts[]",
        },
        "enterpriseUsage": {
            "method": "POST",
            "path": "/v2/billing/meter/get-enterprise-user-usage",
            "body": {},
            "note": "AuthService.getEnterpriseUsage()；limitNum + credit + cycleResetTime",
        },
        "dosageNotify": {
            "method": "POST",
            "path": "/v2/billing/meter/get-dosage-notify",
            "body": {},
        },
    })
}

/// 活动 / 用户端点表
pub fn activity_endpoints() -> Value {
    json!({
        "workbuddyBanner": {
            "method": "GET",
            "path": "/v2/activity/workbuddy/banner",
            "whitelistHeaders": true,
            "note": "AuthService.getActivityBanner()；缺 X-Product 等头会被白名单拦截",
        },
        "ambassadorStatus": { "method": "GET", "path": "/v2/activity/ambassador/status" },
        "updateNickname": {
            "method": "POST",
            "path": "/v2/as/wechatmp/user/profile/update",
            "body": { "nickname": "<new>" },
        },
    })
}

/// 配置 / 模型目录端点表（enterpriseModels 的 path 是函数 → Node 输出里被丢弃）
pub fn config_endpoints() -> Value {
    json!({
        "v3": { "method": "GET", "path": "/v3/config" },
        "v2": { "method": "GET", "path": "/v2/config" },
        "enterpriseModels": { "method": "GET" },
        "featureFlag": { "method": "GET", "path": "/v2/feature-flag/api/product-config" },
        "update": {
            "method": "GET",
            "path": "/v2/update",
            "query": { "platform": "workbuddy-win32-x64", "version": "<ver>" },
        },
    })
}

/// 云智能体端点表
pub fn cloud_agent_endpoints() -> Value {
    json!({
        "entitlement": { "method": "GET", "path": "/v2/user/cloudagent/entitlement" },
        "quota": { "method": "GET", "path": "/v2/user/cloudagent/quota" },
        "listAgents": { "method": "GET", "path": "/v2/user/cloudagent/agents" },
        "listInstances": { "method": "GET", "path": "/v2/user/cloudagent/instances" },
        "listConversations": { "method": "GET", "path": "/v2/user/cloudagent/conversations" },
        "listTasks": { "method": "GET", "path": "/v2/user/cloudagent/tasks" },
    })
}

/// 本地 CLI sidecar 端点表（不是上游接口）
pub fn sidecar_endpoints() -> Value {
    json!({
        "acp": { "method": "POST", "path": "/api/v1/acp", "note": "ACP 主通道，走 loopback 豁免" },
        "health": { "method": "GET", "path": "/api/v1/health" },
        "llmCompletions": {
            "method": "POST",
            "path": "/api/v1/llm/completions",
            "note": "内部摘要生成，不经 requireAuth",
        },
    })
}

/// 版本表 → JSON（对照 Node 版 EDITIONS，字段名逐字对齐 camelCase）
pub fn editions_to_json() -> Value {
    let mut map = Map::new();
    for info in EDITIONS {
        map.insert(
            info.id.to_string(),
            json!({
                "id": info.id,
                "label": info.label,
                "endpoint": info.endpoint,
                "stagingEndpoint": info.staging_endpoint,
                "prefixPath": info.prefix_path,
                "platform": info.platform,
                "uaPlatform": info.ua_platform,
                "productName": info.product_name,
                "clientVersion": info.client_version,
                "cliVersion": info.cli_version,
                "dataFolderName": info.data_folder_name,
            }),
        );
    }
    Value::Object(map)
}
