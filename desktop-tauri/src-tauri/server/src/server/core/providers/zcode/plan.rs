//! ZCode 的**活动套餐通道**（Start Plan）：`{zcode}/api/v1/zcode-plan/anthropic/v1/messages`。
//!
//! ── 为什么需要第二条通道 ────────────────────────────────────
//! ZCode 有两份**互相独立**的额度，分别由两个上游端点承载：
//!
//! ```text
//!   编码套餐（Coding Plan，用户自己买的订阅）
//!     {openai_base}/chat/completions        ← OpenAI 协议，Bearer = accessToken
//!   活动套餐（Start Plan，官方限时发放的体验额度，见 claim.rs）
//!     {zcode}/api/v1/zcode-plan/anthropic/v1/messages
//!                                           ← Anthropic 协议，Bearer = 套餐 JWT
//! ```
//!
//! 走错门的症状是**上游报「套餐已到期」或限额**，而不是「没有权限」：编码套餐
//! 到期后 OpenAI 那条路恒回 429「您的GLM Coding Plan套餐已到期」，而当天领到的
//! 活动额度只能从 anthropic 那条路花掉（2026-09 用户实测）。因此「用哪条通道」
//! 是**账号级**设置（`zcode::PLAN_FIELD`），默认仍是编码套餐 —— 存量账号的行为
//! 逐字节不变。
//!
//! ── 为什么请求体要照抄官方客户端的系统提示词 ────────────────
//! 这条通道上有**内容检查**：网关会看 `system` 里有没有 ZCode 的身份块，缺了
//! 就回 `3012 method not allowed`（参考实现 `Acankao/zcode-api` 的实测记录）。
//! 所以本模块把官方客户端的装配逐块搬过来：
//!
//!   1. `system` = **恰好 3 块**（`cliPrefix` / 其余稳定段 `\n\n` 拼接 /
//!      动态段以 `\n\n` 开头），每块都带 `cache_control:{type:"ephemeral"}`；
//!      客户端自己的 system 排在这 3 块**之后**，且**去掉**它的 cache_control
//!      （Anthropic 的缓存断点上限是 4：官方 3 块 + 最后一条消息 1 个，客户端
//!      再带就超了，而真实客户端因为自己拥有整份 body 从不产生外来断点）；
//!   2. 动态段里的 Environment 段要用**真实运行值**（cwd / platform / shell /
//!      osVersion），并以 `- You are powered by the model named {provider}/{model}.`
//!      收尾（`{provider}` 由地区给出：国内 `bigmodel-api`、国际 `zai-api`）；
//!   3. `messages` 前面**加一条** user 消息：官方客户端的 `meta_user`
//!      context_prefix —— 今天的日期裹在 `<system-reminder>` 里；
//!   4. 最后一条非 system 消息的最后一个内容块打上缓存断点（其余清掉），
//!      工具声明上的 cache_control 一律删除；
//!   5. `metadata.user_id` = `{"device_id": <设备标识>, "account_uuid": "",
//!      "session_id": ""}`（`account_uuid` 官方**恒为空串**）。
//!
//! 静态段文本在 `zcode_system.json`（与参考实现同一份资源，`include_str!` 进
//! 二进制；`serde_json` 解析失败按 500 如实报错，不静默降级 —— 少了身份块的
//! 请求必然被上游 3012 拒掉，不如把根因说在网关这边）。
//!
//! ── 官方装配**可关、正文可改**（设置页「系统提示词 → 按提供商 → 网关自带」）──
//! 上面那套装配是「上游当下要求什么」的复刻，不是永恒真理：设置里有一个按家
//! 的开关（`KEY_PROMPT_GATEWAY`，默认**开** = 存量行为逐字节不变）。关掉时只
//! 跳过官方三段与 context prefix，客户端的 system 照发、缓存断点与 metadata
//! 照旧 —— 关掉后上游认不认，取决于它当前的口径（实测记录见
//! `super::OFFICIAL_PROMPT_NOTE`），网关不替上游下结论。
//!
//! 正文也能改（`KEY_PROMPT_GATEWAY_TEXT`）：改过的那一段以配置里的文本为准，
//! 没改的段继续用官方原文（逐段合并，见 `GatewayBlocks::or`）。**三段各自成块
//! 这件事不可协商**（结构就是上游的判据），所以界面与配置都是「分别编辑三段」，
//! 而不是一整块文本。用户正文里的 `{cwd}` / `{platform}` / `{shell}` /
//! `{os_version}` / `{git}` / `{provider}` / `{model}` 在发请求时换成真实值
//! （[`substitute`]）：Environment 段本来就是逐请求生成的，冻结成用户保存那天
//! 的工作目录只会让提示词说谎。
//!
//! ── 请求头（与编码套餐那条的两处差别）────────────────────────
//!   1. `User-Agent` 多一个 `ai-sdk/anthropic/3.0.81` 后缀（官方客户端的
//!      Anthropic SDK 会把身份拼进 UA；控制面请求没有这个后缀）；
//!   2. 多三个追踪头（`x-request-id` / `x-zcode-session-type: main` /
//!      `x-zcode-trace-id`）。**不发** `X-Device-Mid`（那是领取/余额那条
//!      控制面的要求），设备标识改为进请求体的 `metadata`。
//!   客户端若带了 `anthropic-beta`，原样透传：那是客户端自己点的特性开关，
//!   网关没有理由替它吞掉。另加两个**验证码头**（`…VERIFY_PARAM_HEADER` /
//!   `…VERIFY_REGION_HEADER`），理由见下面那一段。
//!
//! ── 人机验证：**要不要**令牌由上游配置说（见 [`super::captcha`]）──
//! 2026-09-28 实测：少 `X-Aliyun-Captcha-Verify-Param` 这个端点一律回
//! `400 {"code":3007,"msg":"captcha verify failed"}` —— **不是偶发挑战，是常规门禁**。
//! 令牌由桌面端的 WebView 静默铸造、推进 [`super::captcha`] 的池子，本模块取一个。
//!
//! 2026-10-01 起这一格改成**听上游的**：`GET /api/v1/client/configs` 里有
//! `configs.captcha.skip_model_request: true`（实测与客户端版本无关），也就是
//! 官方客户端此刻在推理请求上**不带**这两个头。[`super::config_sync`] 每 5 分钟
//! 同步一次，本模块照 [`super::captcha::required`] 决定「取一枚带上」还是「整个
//! 不带」；上游真回了 3007 就当场翻回要码（[`super::captcha::note_challenge_and_require_token`]），
//! 所以免码路线最坏的代价是**一次** 3007 往返。
//!
//! 池子空而配置说要码时，本模块**如实失败**并给出可执行的提示（headless / Docker
//! 没有 WebView，那条通道在那种部署上就是不可用）；上游回 3007 也记进池子计数，
//! 界面据此立刻补货。
//!
//! 池子空这一条给的是**专用状态码** `556`（判据与理由见 [`readiness`]）而不是
//! 503，并顺手登记一道 provider 级闸门：那是「本机发不出去」而不是「这家拒了
//! 请求」，编排层据此换下一个候选家，选路据此暂时绕开这一家。写死 503 的版本会
//! 把整份请求终止在这里 —— 生产上 `attempts=1`、别家明明能答的这个模型名直接报错。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic，取值一律走 Option 链。

use std::sync::OnceLock;

use axum::http::HeaderMap;
use serde_json::{json, Map, Value};

use crate::server::core::prompt::GatewayBlocks;
use crate::server::core::protocol::anthropic_outbound;
use crate::server::core::providers::adapter::{ChatRequestPlan, UpstreamResponse};
use crate::server::core::providers::readiness;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::region::Region;

/// 活动套餐推理路径（拼在 [`plan_base_url`] 之后）。
///
/// **不要**在这里再写一遍 `/api/v1/zcode-plan` 前缀：`plan_base_url` 已经是
/// 那条前缀（2026-09-28 实测踩过：两处各写一遍 = 路径里出现两次
/// `/api/v1/zcode-plan`，上游回 `404 page not found`，看起来像「接口没了」）。
const MESSAGES_PATH: &str = "/anthropic/v1/messages";

/// Anthropic 协议版本头（官方 SDK 现值）
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 官方客户端在**推理**请求上拼的 SDK 身份后缀（`ai-sdk/anthropic/{ver}`）
const ANTHROPIC_SDK_UA: &str = "ai-sdk/anthropic/3.0.81";

/// 官方系统提示词资源（静态段文本；见模块头）
const SYSTEM_JSON: &str = include_str!("zcode_system.json");

/// 解析一次、全局共用。`None` = 资源坏了（编译期常量坏掉只可能是打包事故）
static SYSTEM_DATA: OnceLock<Option<Value>> = OnceLock::new();

fn system_data() -> Option<&'static Value> {
    SYSTEM_DATA
        .get_or_init(|| serde_json::from_str::<Value>(SYSTEM_JSON).ok())
        .as_ref()
}

/// 官方三段装配里**静态文本**的字符数（设置页只读子行的「约 N 字符」）。
///
/// 只算身份句 + 稳定段 + 动态段两端；不含 Environment 段 —— 它逐请求生成
/// （工作目录 / 平台 / 模型名），长度随机器变，报一个固定数就是错的。
/// 资源解析失败时给 0（界面那一行整个不显示，而不是显示一个假数字）。
pub fn official_prompt_approx_chars() -> usize {
    let Some(data) = system_data() else {
        return 0;
    };
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0)
    };
    let stable: usize = data
        .get("stableSections")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::len).sum())
        .unwrap_or(0);
    let dynamic = data
        .get("dynamicSections")
        .map(|value| {
            ["beforeEnvironment", "afterEnvironment"]
                .iter()
                .filter_map(|key| value.get(*key).and_then(Value::as_str))
                .map(str::len)
                .sum::<usize>()
        })
        .unwrap_or(0);
    text("cliPrefix") + stable + dynamic
}

/// 业务码 → 可执行的提示（`None` = 没见过的码，按上游原文透传）。
///
/// 由 `ZcodeAdapter::classify_error` 读 —— 分类动作（换账号 / 冷却 / 透传）
/// 仍由编排层按状态码决定，这里只补一句「为什么」。
pub(super) fn code_hint(code: i64) -> Option<&'static str> {
    match code {
        // 风控门禁：缺验证码令牌或令牌已失效（见 `super::captcha`）
        3007 => Some(
            "上游要求人机验证（3007）：验证码令牌缺失或已失效。\
             令牌由桌面端界面自动铸造，请确认应用界面正在运行（只跑 headless 服务时无法铸造）；\
             若持续出现，请稍后重试",
        ),
        // 身份块缺失/不被接受：官方系统提示词没装配对（或上游改了检查口径）
        3012 => Some(
            "上游拒收了请求的身份信息（3012）：请把该账号的「使用套餐」切回编码套餐，\
             或升级应用后重试",
        ),
        // 参数错误：活动套餐通道上最常见的原因是设备标识（见模块头第 5 条）
        3001 => Some("上游判定参数错误（3001）：请重新登录该账号以刷新设备标识"),
        _ => None,
    }
}

/// 构造一次「活动套餐」转发请求（OpenAI Chat 体 → 上游 Anthropic Messages 体）。
///
/// 入参 `session` 是账号会话（带 `jwt` / `deviceMid` / `zcodePlan` 三个附加键，
/// 见 `AccountStore::session_from_record`）；`body` 是编排层定稿的发送体
/// （**已**含 `stream:true`，见 `UpstreamService::forward`）。
pub(super) fn build_request(
    region: Region,
    session: &Value,
    body: &Value,
    client_headers: &HeaderMap,
) -> Result<ChatRequestPlan, GatewayError> {
    let text_of = |key: &str| {
        session
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let jwt = text_of("jwt");
    if jwt.is_empty() {
        // 这条通道只认套餐 JWT（推理用的 accessToken 打这个端点必 401）。
        // 不是「未配置查询凭证」那种可选项，而是这条路走不通 —— 因此 401 +
        // 一句可执行的出路，与 `refresh_access_token` 的文案同一取向。
        return Err(GatewayError::with_status(
            401,
            "该 ZCode 账号缺少套餐登录态（jwt），无法使用活动套餐通道：\
             请重新登录该账号，或在账号设置里把「使用套餐」切回编码套餐",
        ));
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        // Anthropic 协议必填 model，而这条通道的模型只能由客户端点名
        // （与 chat 路径「没点名就用上游默认」不同：那边上游有默认值）
        return Err(GatewayError::with_status(
            400,
            "活动套餐通道需要明确的模型名：请在客户端指定模型后重试",
        ));
    }

    let mut payload = anthropic_outbound::anthropic_request_from_chat(body, &model)
        .map_err(|message| GatewayError::with_status(400, format!("请求体转换失败：{message}")))?;
    // ── 思考装配（GLM-5.3 家族）────────────────────────────────
    // 放在转换之后、官方段装配之前：它要**覆盖**转换层按通用档位表注入的
    // `thinking`（那张表服务所有上游，而 5.3 家族有官方目录给的三档，见
    // `super::reasoning` 的模块头），并且把预算加到 `max_tokens` 之上 ——
    // 少了这一步，思考会把客户端的输出额度吃光，上游回 200 + 空正文
    // （issue #52 / #54 的根因）。取值从**发送体**读：客户端原始请求里的
    // `reasoning_effort`，或映射上绑的默认档（适配器的 `reasoning_patch`
    // 在更早一步注进同一个键）—— 两条来路在这里汇成一个旋钮。
    super::reasoning::apply_to_anthropic(
        &mut payload,
        &model,
        body.get(super::reasoning::EFFORT_FIELD).and_then(Value::as_str),
        client_max_tokens(body),
    );
    let device_mid = text_of("deviceMid");
    // 官方装配是否启用（设置页「系统提示词 → 按提供商 → 网关自带提示词」，
    // 默认开）、以及用户改过的正文（同一处的「编辑正文」，逐段覆盖官方原文）。
    // **关掉不等于「什么都不做」**：只跳过官方那三段身份块与前缀消息，其余
    // （缓存断点整理、metadata 设备标识、头）照旧 —— 那些不是提示词，没有跟着
    // 一起关的理由。关掉后上游认不认由上游当下的口径决定，实测记录见
    // `super::OFFICIAL_PROMPT_NOTE`。
    let provider_id = region.provider_id();
    let override_blocks = crate::server::config::gateway_prompt_text(provider_id);
    apply_start_plan(
        &mut payload,
        &model,
        region,
        &device_mid,
        crate::server::config::gateway_prompt_enabled(provider_id),
        override_blocks.as_ref(),
    )?;

    // ── 人机验证令牌（要就要、不要就**整个不带**）────────────────
    // 要不要带由上游自己下发的那格配置决定（`configs.captcha.skip_model_request`，
    // 见 [`super::config_sync`] 与 [`super::captcha::required`]）：
    //   · 要 —— 取一枚令牌（一次一用）。取不到就**当场失败**而不是发一个缺头的
    //     请求：上游对缺令牌的响应是 400 + 3007（一句英文），用户看到的是「网关
    //     报了个英文错」；这里的文案才说得清「谁该补令牌、去哪儿补」。
    //     处置本身在 [`captcha_pool_empty`]。
    //   · 不要 —— 两个头**一个都不写**。这里必须是「键不存在」而不是「键在值为空」：
    //     空值会被序列化成 `""`，上游那侧把它当成一个畸形 proof（比缺头更容易触发
    //     风控），参考实现为此专门踩过一次。
    let captcha_headers: Vec<(String, String)> = if super::captcha::required() {
        match super::captcha::take() {
            Some((param, region)) => {
                let mut pairs = vec![(super::captcha::VERIFY_PARAM_HEADER.to_string(), param)];
                if !region.trim().is_empty() {
                    pairs.push((super::captcha::VERIFY_REGION_HEADER.to_string(), region));
                }
                pairs
            }
            None => return Err(captcha_pool_empty(provider_id)),
        }
    } else {
        Vec::new()
    };
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {jwt}")),
        ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
    ];
    headers.extend(captcha_headers);
    // 身份头与编码套餐那条同源（同一个函数、同一套取值），只是 UA 多一个
    // Anthropic SDK 后缀 —— 两处若各写一份，改一处必然漏另一处
    headers.extend(super::adapter::identity_headers(Some(ANTHROPIC_SDK_UA)));
    headers.extend(trace_headers());
    if let Some(beta) = client_headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        headers.push(("anthropic-beta".to_string(), beta.to_string()));
    }

    Ok(ChatRequestPlan {
        url: format!("{}{MESSAGES_PATH}", plan_base_url(region)),
        headers,
        body: payload,
        // 上游说 Anthropic：响应帧要先折成 chat SSE 再下发
        // （见 `upstream::translate`）
        response: UpstreamResponse::Anthropic,
    })
}

/// 令牌池为空的**处置**：登记本地闸门 + 给出本地前置条件错误。
///
/// 状态码用 [`readiness::LOCAL_PRECONDITION_STATUS`]（556）而不是 503：这一条是
/// **本地前置条件**（这台机器铸不出令牌），一次上游往返都没发生。编排层按这个码
/// 决定「顺延下一个候选家」而不是终止整份请求；取证上也只有专属码才能把「从未
/// 发出去的请求」与「上游真回了 503」分开（理由与实测形状见 `readiness` 模块头）。
///
/// 顺手登记闸门：headless 上令牌池**恒空**，不登记的话每条请求都要先撞一次本地
/// 失败才轮得到别家。`hold` 返回 `true` = 本次让它从放行变成挡住 —— 日志只在
/// 这一刻打，逐请求打会刷屏。
///
/// 单独成函数的理由：`build_request` 的其余部分要读系统配置、工作目录、OS 版本
/// 与那份官方提示词资源，为了一个状态码把它们全拖进测试不值得。判据在此，测试也
/// 在此（见文件末尾的 `local_precondition`）。
fn captcha_pool_empty(provider_id: &str) -> GatewayError {
    let message = "活动套餐通道需要人机验证令牌，当前令牌池为空：\
         令牌由桌面端界面自动铸造，请确认应用界面正在运行\
         （headless 部署无法铸造，请在账号设置里把「使用套餐」切回编码套餐）";
    if readiness::hold(provider_id, message) {
        logging::console_line(
            "[Routing]",
            &format!(
                "⚠️ {provider_id} 的活动套餐通道在本机发不出去（令牌池为空），\
                 选路暂时绕开这一家；桌面端界面开始铸造令牌后会自动恢复，\
                 否则最长 {} 秒重新探一次",
                readiness::GATE_TTL_MS / 1000
            ),
        );
    }
    GatewayError::with_status(i32::from(readiness::LOCAL_PRECONDITION_STATUS), message)
}

/// 客户端原始请求里的输出额度（chat 侧两个字段名都认，与
/// `anthropic_outbound` 的取值链同口径）。
///
/// 取的是**客户端给的那个数**，不是转换后 payload 里的——转换层可能已经按
/// 自己的通用档位表把额度抬过一次，那份额度是它为了让通用思考注入成立而加的，
/// 不能当客户端的意图再叠加一遍（会越叠越大）。
fn client_max_tokens(body: &Value) -> Option<i64> {
    ["max_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| {
            body.get(*key)
                .and_then(Value::as_i64)
                .filter(|value| *value > 0)
        })
}

/// 活动套餐网关的基址（环境变量可覆盖，理由与 `Region::openai_base_url` 同）。
///
/// 默认取 zcode 平面（两地相同，见 `region.rs`）+ `/api/v1/zcode-plan`。
fn plan_base_url(region: Region) -> String {
    region
        .env_override("PLAN_BASE_URL")
        .unwrap_or_else(|| format!("{}/api/v1/zcode-plan", region.zcode_origin()))
}

/// 官方三段的**模板**：Environment 段里的运行值换成占位符（`{cwd}` 等）。
///
/// 给设置页的编辑器当默认值用：用户看到的是「官方原文长这样」，改哪一段都行；
/// 发请求时占位符再由 [`substitute`] 换成真实值。资源解析失败时给 `None`
/// （界面此时不提供编辑入口，而不是给出一份空文本让用户把官方原文覆盖掉）。
pub fn official_blocks_template() -> Option<GatewayBlocks> {
    let data = system_data()?;
    Some(GatewayBlocks {
        identity: text_of(data, "cliPrefix"),
        stable: stable_joined(data),
        dynamic: [dynamic_before(data), environment_template(data), dynamic_after(data)]
            .join("\n\n"),
    })
}

/// 官方三段的**渲染值**（Environment 段用真实运行值）。
fn official_texts(data: &Value, model: &str, region: Region) -> GatewayBlocks {
    GatewayBlocks {
        identity: text_of(data, "cliPrefix"),
        stable: stable_joined(data),
        dynamic: [
            dynamic_before(data),
            environment_section(data, model, region),
            dynamic_after(data),
        ]
        .join("\n\n"),
    }
}

fn text_of(data: &Value, key: &str) -> String {
    data.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn stable_joined(data: &Value) -> String {
    data.get("stableSections")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn dynamic_before(data: &Value) -> String {
    data.get("dynamicSections")
        .and_then(|value| value.get("beforeEnvironment"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn dynamic_after(data: &Value) -> String {
    data.get("dynamicSections")
        .and_then(|value| value.get("afterEnvironment"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 用户正文里的占位符 → 真实运行值（模板形态与渲染形态**只差这一步**）。
///
/// 取值口径与官方客户端一致（见模块头第 2 条）：`{git}` 用资源里的 `gitNo`
/// （官方在非仓库目录下发的就是 "no"，理由见 [`environment_section`]）。
fn template_values(data: &Value, model: &str, region: Region) -> Vec<(&'static str, String)> {
    let git = data
        .get("environment")
        .and_then(|value| value.get("gitNo"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "no".to_string());
    vec![
        ("{cwd}", working_dir()),
        ("{git}", git),
        ("{platform}", os_platform().to_string()),
        ("{shell}", shell_name()),
        ("{os_version}", format!("{} {}", os_platform(), os_arch())),
        ("{provider}", provider_model_id(region).to_string()),
        ("{model}", model.to_string()),
    ]
}

fn substitute(text: &str, values: &[(&'static str, String)]) -> String {
    let mut out = text.to_string();
    for (token, value) in values {
        if out.contains(token) {
            out = out.replace(token, value);
        }
    }
    out
}

/// 把官方三段装配到 body 上（模块头 1~5 条）。
///
/// `official_prompt` = 是否装上官方那三段身份块（含 context prefix 消息），
/// 来自设置页的逐家开关（默认开）；`override_blocks` = 用户改过的正文（逐段
/// 合并，见 [`GatewayBlocks::or`]）。为假时**只**跳过这一段，其余装配照旧 ——
/// 缓存断点、工具声明清洗、`metadata` 与头都不是提示词（见调用点的说明）。
fn apply_start_plan(
    payload: &mut Value,
    model: &str,
    region: Region,
    device_mid: &str,
    official_prompt: bool,
    override_blocks: Option<&GatewayBlocks>,
) -> Result<(), GatewayError> {
    // 这份资源只在装官方段时才需要：关掉它之后（用户明确拨的开关），资源坏掉
    // 不该再让请求失败 —— 那是「用一个用不到的依赖挡路」。
    let data = if official_prompt {
        match system_data() {
            Some(data) => Some(data),
            None => {
                return Err(GatewayError::with_status(
                    500,
                    "内置的 ZCode 官方提示词资源不可用（解析失败）：请重新安装或升级应用",
                ))
            }
        }
    } else {
        None
    };
    let Some(object) = payload.as_object_mut() else {
        return Err(GatewayError::with_status(500, "请求体形态异常（不是对象）"));
    };

    if let Some(data) = data {
        let texts = merged_texts(data, model, region, override_blocks);
        object.insert(
            "system".to_string(),
            Value::Array(system_blocks(&texts, object.get("system"))),
        );
        // context_prefix：官方客户端在**每一条**请求的第一条 user 消息前都带它
        if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
            if !messages.is_empty() {
                messages.insert(0, context_prefix_message(data));
            }
        }
    } else {
        // 关掉官方段时**客户端自己的 system 仍然要发**（它是客户端的内容，不是
        // 网关的），只是把形态整理成块数组、并丢掉客户端自带的 cache_control ——
        // 断点预算与策略都由下面 `apply_cache_control` 统一管（它清掉非 system
        // 消息上的断点、只留最后一条）。丢掉客户端断点是有意的保守做法：
        // 客户端可能带满 4 个断点，再加网关这一个就超上限，而上游对此是**报错**
        // 而不是忽略。
        if let Some(existing) = object.get("system").cloned() {
            let blocks = user_system_blocks(Some(&existing));
            if blocks.is_empty() {
                object.remove("system");
            } else {
                object.insert("system".to_string(), Value::Array(blocks));
            }
        }
    }
    strip_tool_cache_control(object);
    apply_cache_control(object);
    if let Some(user_id) = metadata_user_id(device_mid) {
        let existing = object.get("metadata").filter(|value| value.is_object()).cloned();
        let mut metadata = existing.and_then(|value| value.as_object().cloned()).unwrap_or_default();
        metadata.insert("user_id".to_string(), Value::String(user_id));
        object.insert("metadata".to_string(), Value::Object(metadata));
    }
    Ok(())
}

/// 三段正文的**逐段合并**：用户改过的那一段用他的文本（占位符换成真实值），
/// 没改的段用官方原文。整份替换会让「只想改身份句」的用户被迫抄几千字符。
fn merged_texts(
    data: &Value,
    model: &str,
    region: Region,
    override_blocks: Option<&GatewayBlocks>,
) -> GatewayBlocks {
    let defaults = official_texts(data, model, region);
    let Some(over) = override_blocks else {
        return defaults;
    };
    let values = template_values(data, model, region);
    let pick = |field: &str| {
        let mine = over.get(field);
        if mine.trim().is_empty() {
            defaults.get(field).to_string()
        } else {
            substitute(mine, &values)
        }
    };
    GatewayBlocks {
        identity: pick("identity"),
        stable: pick("stable"),
        dynamic: pick("dynamic"),
    }
}

/// 官方三块 + 客户端自带 system（去掉缓存断点）。
fn system_blocks(texts: &GatewayBlocks, existing: Option<&Value>) -> Vec<Value> {
    let ephemeral = json!({ "type": "ephemeral" });
    let mut blocks = vec![
        text_block(texts.identity.clone(), Some(ephemeral.clone())),
        text_block(texts.stable.clone(), Some(ephemeral.clone())),
        // 动态块以 `\n\n` 开头，是官方装配的形状（不是排版疏忽）
        text_block(format!("\n\n{}", texts.dynamic), Some(ephemeral)),
    ];
    blocks.extend(user_system_blocks(existing));
    blocks
}

/// 客户端自带 system → 文本块数组（**丢弃** cache_control，理由见模块头）。
///
/// 三种输入形态都要认：字符串（本网关翻译器的常规输出）、块数组（客户端自己
/// 带的断点会走到这里）、以及 null / 其它（什么都不加）。
fn user_system_blocks(existing: Option<&Value>) -> Vec<Value> {
    let Some(existing) = existing else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    match existing {
        Value::String(text) => {
            let text = text.trim();
            if !text.is_empty() {
                blocks.push(text_block(text.to_string(), None));
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(text) = item.as_str() {
                    if !text.trim().is_empty() {
                        blocks.push(text_block(text.trim().to_string(), None));
                    }
                    continue;
                }
                let is_text = item.get("type").and_then(Value::as_str) == Some("text");
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if is_text && !text.trim().is_empty() {
                        blocks.push(text_block(text.trim().to_string(), None));
                    }
                }
            }
        }
        _ => {}
    }
    blocks
}

fn text_block(text: String, cache_control: Option<Value>) -> Value {
    let mut block = Map::new();
    block.insert("type".to_string(), Value::String("text".to_string()));
    block.insert("text".to_string(), Value::String(text));
    if let Some(cache) = cache_control {
        block.insert("cache_control".to_string(), cache);
    }
    Value::Object(block)
}

/// Environment 段（官方 `T9o`）：真实运行值 + 末尾的「由哪个模型承载」。
///
/// 实现是「模板 + 占位符替换」：模板（[`environment_template`]）就是设置页编辑器
/// 里显示的那份，两条路径因此共用同一批行与标签 —— 用户改一行、删一行都不会出现
/// 「界面上一套、实际发出去另一套」。
fn environment_section(data: &Value, model: &str, region: Region) -> String {
    substitute(&environment_template(data), &template_values(data, model, region))
}

/// Environment 段的模板形态（运行值换成占位符）。
fn environment_template(data: &Value) -> String {
    let env = |key: &str| {
        data.get("environment")
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let lines = vec![
        env("heading"),
        env("invokedLine"),
        format!("- {}: {{cwd}}", env("cwdLabel")),
        // 「是不是 git 仓库」官方给的是真实检测结果；本网关在**用户机器上**
        // 运行，但没有为一行提示词去调 git 的理由 —— 官方桌面端在非仓库目录
        // 下发的就是 "no"，这是合法且常见的形态
        format!("- {}: {{git}}", env("gitLabel")),
        format!("- {}: {{platform}}", env("platformLabel")),
        format!("- {}: {{shell}}", env("shellLabel")),
        // 官方是 `{platform} {release} {arch}`。本机读不到内核版本（Rust 标准库
        // 不给，引系统信息 crate 为一行提示词不值得 —— 与身份头里
        // `X-Os-Version` 的取舍同一条），因此只发平台 + 架构
        format!("- {}: {{os_version}}", env("osVersionLabel")),
        env("poweredByLine"),
    ];
    lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Environment 段里 `{provider}` 的取值（官方注册表 `p2`：zai → `zai-api`、
/// bigmodel → `bigmodel-api`）。
fn provider_model_id(region: Region) -> &'static str {
    match region {
        Region::Cn => "bigmodel-api",
        Region::Intl => "zai-api",
    }
}

/// 平台名（官方 `process.platform` 的取值域：win32 / darwin / linux）。
///
/// 与 `claim::platform()`（`win32-x64` 那种「平台-架构」，给头用）区分：
/// 提示词里官方发的是**裸平台名**，架构在 osVersion 那一行。
fn os_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn os_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    }
}

/// 当前工作目录（官方发客户端的 `process.cwd()`）。
///
/// 取不到时给 `.`：这是 shell 在已删除目录里的合法 pwd，比编一个绝对路径诚实。
fn working_dir() -> String {
    std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ".".to_string())
}

/// shell 名（官方 `basename(SHELL ?? ComSpec)`；Windows 上通常拿到 `cmd.exe`）。
fn shell_name() -> String {
    let raw = std::env::var("SHELL")
        .or_else(|_| std::env::var("ComSpec"))
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_default();
    let name = raw
        .rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        // 官方在探测失败时发 "unknown"，且这是**合法**取值（不是缺失）
        "unknown".to_string()
    } else {
        name
    }
}

/// `meta_user` 的 context_prefix（官方 `tct` + `Vre`）：一条 user 消息，
/// 内容是把「今天的日期」裹进 `<system-reminder>` 的一小段文本。
///
/// 日期用**本机时区**的今天（官方取客户端的本地日期；本网关就跑在用户机器上，
/// 两者同一个口径），不做 UTC 归一 —— 用户在东八区晚上 8 点发请求时，
/// UTC 口径会给出昨天。
fn context_prefix_message(data: &Value) -> Value {
    let text = |key: &str| {
        data.get("contextPrefix")
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let section = format!("{}\n{}", text("currentDateHeading"), text("currentDateLine").replace("{date}", &date));
    // 空串那一行是官方装配里的空行（[intro, section, "", outro].join("\n")）
    let body = [text("intro"), section, String::new(), text("outro")].join("\n");
    let open = data
        .get("systemReminder")
        .and_then(|value| value.get("open"))
        .and_then(Value::as_str)
        .unwrap_or("<system-reminder>");
    let close = data
        .get("systemReminder")
        .and_then(|value| value.get("close"))
        .and_then(Value::as_str)
        .unwrap_or("</system-reminder>");
    json!({
        "role": "user",
        "content": [{ "type": "text", "text": format!("{open}{body}{close}") }],
    })
}

/// 工具声明上的 cache_control 一律删掉（官方 3 块 + 最后一条消息已占满 4 个断点）。
fn strip_tool_cache_control(object: &mut Map<String, Value>) {
    let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut() {
        if let Some(tool) = tool.as_object_mut() {
            tool.remove("cache_control");
        }
    }
}

/// 缓存断点：先清掉非 system 消息上的（客户端可能带了），再打到最后一条
/// 非 system 消息的最后一个内容块上（官方 `zsi` + `Fsi` 两步）。
fn apply_cache_control(object: &mut Map<String, Value>) {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            continue;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut() {
            if let Some(block) = block.as_object_mut() {
                block.remove("cache_control");
            }
        }
    }
    for message in messages.iter_mut().rev() {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            continue;
        }
        match message.get_mut("content") {
            // 字符串内容：升格成块数组才能带断点（Anthropic 两种都认）
            Some(Value::String(text)) => {
                let block = text_block(text.clone(), Some(json!({ "type": "ephemeral" })));
                if let Some(slot) = message.get_mut("content") {
                    *slot = Value::Array(vec![block]);
                }
            }
            Some(Value::Array(blocks)) => {
                if let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) {
                    last.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
                }
            }
            _ => {}
        }
        return;
    }
}

/// `metadata.user_id`（官方 `UIo`）：`device_id` 缺失时**省掉那个键**。
fn metadata_user_id(device_mid: &str) -> Option<String> {
    let mut blob = Map::new();
    if !device_mid.trim().is_empty() {
        blob.insert(
            "device_id".to_string(),
            Value::String(device_mid.trim().to_string()),
        );
    }
    // `account_uuid` 官方**恒为空串**（真实流量从不带账号 uuid）；
    // `session_id` 本网关没有会话概念，同样是空串。
    blob.insert("account_uuid".to_string(), Value::String(String::new()));
    blob.insert("session_id".to_string(), Value::String(String::new()));
    serde_json::to_string(&Value::Object(blob)).ok()
}

/// 追踪头（官方 `createModelRequestAttributionHeaders` 在 start-plan 上的形态：
/// 会话类型恒 `main`，且**不带** `x-query-id` / `x-session-id`）。
///
/// 随机源失败（几乎不可能）时省掉那两个 id 头：上游只把它们当关联键，缺一个
/// 不会让请求失败，而编一个固定值反而会让不同请求在日志里串台。
fn trace_headers() -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if let Some(id) = super::credentials::new_uuid() {
        headers.push(("x-request-id".to_string(), id));
    }
    headers.push(("x-zcode-session-type".to_string(), "main".to_string()));
    if let Some(id) = super::credentials::new_uuid() {
        headers.push(("x-zcode-trace-id".to_string(), id));
    }
    headers
}

#[cfg(test)]
mod local_precondition {
    //! 令牌池为空那一条的**处置**：专属状态码、登记闸门、以及那句可执行的出路。
    //!
    //! ── 为什么测试要绕过 [`captcha_pool_empty`] 直接打 `build_request` ──
    //! 走内部函数等于只断言「我写的常数确实是 556」—— 那条用例在 `readiness`
    //! 里已经有更直白的版本。这里要钉的是**接线**：真的那条路径（账号走
    //! `start-plan`、模型名已点名、官方提示词资源可解析）在令牌池为空时确实落到
    //! 556 并挡住这一家。`build_request` 在取令牌之前不碰网络，代价只是一次
    //! 本地装配。
    //!
    //! ── 假令牌为什么写成那个形状 ────────────────────────────
    //! [`super::super::captcha::push`] 会丢掉「不足 5 个数字」的 param（阿里云
    //! V3 的 proof 是 JWT，里面一定有数字；畸形值入池只会在发出去之后换来一次
    //! 3007）。所以对照组里那颗假令牌必须**带数字**，否则 `take()` 返回 None，
    //! 正对照测的仍是失败分支 —— 那种绿是空跑。
    //!
    //! 令牌池与闸门表都是**进程级**状态：用例之间靠 [`readiness::lock_for_tests`]
    //! 串行，靠那颗假令牌把池子填满/取空（`take()` 一次一用，用完自然回到空池）。

    use axum::http::HeaderMap;
    use serde_json::json;

    use super::super::region::Region;
    use super::build_request;
    use crate::server::core::providers::readiness;
    use crate::server::core::providers::zcode::captcha;

    /// 一条「足够走到取令牌那一步」的请求：有 jwt、有点名的模型名。
    fn session_on_start_plan() -> serde_json::Value {
        json!({
            "jwt": "package-login-state",
            "deviceMid": "mid-under-test",
            "zcodePlan": "start-plan",
        })
    }

    fn chat_body() -> serde_json::Value {
        json!({
            "model": "glm-5.3-flash",
            "stream": true,
            "messages": [{"role": "user", "content": "只回一个字"}],
        })
    }

    /// 一颗形状合法的假令牌（含足量数字，见模块头那条说明）。
    fn fake_certificate() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(2026_09_30_001);
        let digits = 100_000_000_000_000_u64 + (now % 899_999_999_999_999);
        format!("fake-cert-{digits}.payload.sig")
    }

    #[test]
    fn an_empty_token_pool_fails_locally_and_gates_the_home() {
        let _slot = readiness::lock_for_tests();
        readiness::clear_for_tests();
        // 前提：池子是空的。本文件之外没有别的用例往这座池子里放东西，而
        // `take()` 一次一用，上一条的对照组必然把它取走 —— 这里仍显式确认一次，
        // 免得将来新增用例把这条变成「其实有令牌，所以断言的是成功路径」。
        assert_eq!(0, captcha::ready(), "对照组前提：令牌池为空");

        let error = build_request(
            Region::Cn,
            &session_on_start_plan(),
            &chat_body(),
            &HeaderMap::new(),
        )
        .err()
        .expect("没有令牌的 start-plan 请求必须失败");
        assert_eq!(
            i64::from(readiness::LOCAL_PRECONDITION_STATUS),
            i64::from(error.status_code),
            "本地发不出去要说成 556：编排层按这个数字决定顺延，取证按它分组"
        );
        assert_ne!(
            503, error.status_code,
            "与「上游真回了 503」/「选不出账号」分开：那两类都另有所指"
        );
        assert!(
            error.message.contains("切回编码套餐"),
            "那句唯一可执行的出路不能被改没：{message}",
            message = error.message
        );
        assert!(
            readiness::gated("zcode"),
            "当场登记闸门，下一条请求就不必再撞一次本地失败"
        );
        assert!(!readiness::gated("catpaw"), "另一家没道理被牵连");
        readiness::clear_for_tests();
    }

    #[test]
    fn the_same_request_builds_once_the_pool_has_a_token() {
        let _slot = readiness::lock_for_tests();
        readiness::clear_for_tests();
        // 正对照：同一份请求、同一个账号，只把令牌池从空改成有一颗 —— 于是上一条
        // 的失败确定是「缺令牌」造成的，而不是这条路径本身走不通（装配坏了会一路
        // 400/500，那也会让上一条看起来像成功修好了）。
        //
        // 先立一道闸门再推令牌：闸门「入库当场撤掉」这件事因此是被断言的。反过来
        // 直接断言「推完没闸门」会靠「本来就没有闸门」蒙绿（上一条用例已经清过表，
        // 那种绿什么都没说）。
        assert!(
            readiness::hold("zcode", "令牌池为空"),
            "前置：这一家正被本地闸门挡着"
        );
        captcha::push(&fake_certificate(), "cn");
        assert_eq!(
            1,
            captcha::ready(),
            "畸形值会被丢掉：入池没成功就等于没做正对照"
        );
        assert!(
            !readiness::gated("zcode"),
            "令牌入库当场撤闸门（不等 TTL 到期）"
        );

        let plan = build_request(
            Region::Cn,
            &session_on_start_plan(),
            &chat_body(),
            &HeaderMap::new(),
        )
        .expect("有一颗令牌时，同一条路径应当构造得出请求");
        assert!(
            plan.url.contains("/zcode-plan/anthropic/v1/messages"),
            "走的是活动套餐那条门：{}",
            plan.url
        );
        let header_names: Vec<String> = plan
            .headers
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect();
        assert!(
            header_names.contains(&"x-aliyun-captcha-verify-param".to_string()),
            "令牌真的挂在头上了：{header_names:?}"
        );
        assert_eq!(0, captcha::ready(), "一次一用：这条请求把那颗令牌吃掉了");
        readiness::clear_for_tests();
    }
}

#[cfg(test)]
mod token_free_channel {
    //! 上游说「推理不必附令牌」时，那两个头**整个不存在**。
    //!
    //! 为什么单独立一条：这一格由后台循环每 5 分钟同步，值会变；而"免码"落错的
    //! 形态很特别 —— 写成空头（`x-aliyun-captcha-verify-param: ""`）比不写更糟，
    //! 上游把它当畸形 proof，参考实现为此专门踩过一次。所以断言的是**键不存在**，
    //! 不是「值为空」。
    //!
    //! 池子在这一条里被显式留空（`ready == 0`）：那样「构造成功」这件事才只能是
    //! 免码带来的，而不是恰好有一颗令牌。与 `local_precondition` 共用同一把
    //! 全局槽锁（令牌池、闸门表、`required` 结论都是进程级状态）。

    use axum::http::HeaderMap;
    use serde_json::json;

    use super::super::region::Region;
    use super::build_request;
    use crate::server::core::providers::readiness;
    use crate::server::core::providers::zcode::captcha;

    #[test]
    fn a_token_free_request_carries_neither_captcha_header() {
        let _slot = readiness::lock_for_tests();
        readiness::clear_for_tests();
        assert_eq!(0, captcha::ready(), "前置：池子是空的");
        assert!(
            captcha::required(),
            "前置：默认结论是要码（免码结论由下面的同步写入）"
        );
        assert!(
            captcha::set_required(false),
            "同步到 skip_model_request=true"
        );

        let plan = build_request(
            Region::Cn,
            &json!({"jwt": "package-login-state", "deviceMid": "mid-under-test"}),
            &json!({
                "model": "glm-5.3-flash",
                "stream": true,
                "messages": [{"role": "user", "content": "只回一个字"}],
            }),
            &HeaderMap::new(),
        )
        .expect("免码 + 池子为空，这条请求也应当构造得出来（这正是 headless 上要解决的场景）");
        let names: Vec<String> = plan
            .headers
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect();
        assert!(
            !names.iter().any(|name| name.contains("captcha")),
            "两个验证码头都不该出现，连空值也不行：{names:?}"
        );
        assert!(
            names.contains(&"authorization".to_string())
                && names.contains(&"anthropic-version".to_string()),
            "其余头照旧（别把免码写成少发一组头）：{names:?}"
        );
        assert_eq!(0, captcha::ready(), "免码时不该消耗任何令牌");

        // 复原：这条用例之后跑的用例都假定「要码」是默认结论
        assert!(captcha::set_required(true));
    }
}
