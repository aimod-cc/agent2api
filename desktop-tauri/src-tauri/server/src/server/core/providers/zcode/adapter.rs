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
//! `Authorization: Bearer {token}`。模型名不改写；思考等级是唯一被改写的
//! 字段 —— 映射上绑定的默认档由 [`ProviderAdapter::reasoning_patch`] 注入，
//! 随后 `reasoning::apply_to_chat` 把它归一成上游认的档位（5.3 是
//! `low` / `high` / `max` 三档、5.2 是 `minimal` / `high` / `max` 三档；
//! 见 `super::reasoning` 的模块头），客户端没点名等级时那条链什么也不做、
//! 保持上游默认。
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
use serde_json::{json, Map, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::super::adapter::{
    ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, ReasoningPatch, UpstreamErrorClass,
};
use super::super::content_block;
use super::super::ProviderKind;
use super::models;
use super::region::Region;

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
}

impl ProviderAdapter for ZcodeAdapter {
    fn kind(&self) -> ProviderKind {
        self.region.kind()
    }

    /// 本家的模型清单（静态表，两地共用一份，见 `models.rs` 的模块头）
    fn list_models(&self) -> Vec<Value> {
        models::list(self.region)
    }

    /// 映射上绑定的思考等级作为**默认档**注入（客户端自己点名了就让位）。
    ///
    /// ── 为什么只接 GLM-5.2 / 5.3 家族 ─────────────────────────
    /// 有依据的只有这两个家族：5.3 是官方目录 + 实测（上游只认 `low` / `high` /
    /// `max`），5.2 是智谱开放文档给出的完整兼容映射（`none` / `minimal` 关思考、
    /// `low` / `medium` → `high`、`xhigh` → `max`）—— 两族的归一规则都在
    /// `super::reasoning`（模块头有出处）。别的模型一律 `Skip` —— 通用 8 档
    /// 表里没有「它们也收这个字段」的证据，塞一个上游不认识的键不叫生效，
    /// 只是把未知参数推给上游；而猜错档位的代价是把一条本来能用的请求打成 400。
    ///
    /// ── 注入的字段两条通道共用 ────────────────────────────────
    /// 写的是 `reasoning_effort`（[`super::reasoning::EFFORT_FIELD`]）：编码套餐
    /// 通道由 `reasoning::apply_to_chat` 归一后原样发上游；活动套餐通道由
    /// `plan::build_request` 从发送体同一处读出来，交给
    /// `reasoning::apply_to_anthropic` 折成 thinking 预算 + `output_config.effort`
    /// —— 所以绑一次，两条通道都生效，不需要各写一份（5.2 的差别只在活动套餐
    /// 通道走通用折算而不是 `apply_to_anthropic` 的三档装配，见 `super::reasoning`
    /// 模块头「只归一、不做预算」一段）。
    ///
    /// ── 「客户端指定过」的判据 ────────────────────────────────
    /// 与 CatPaw 同一口径：绑定是**默认值**，不覆盖用户的显式意图。这里比
    /// CatPaw 更宽一档 —— 键**在场**就算指定过（哪怕值是 null / 空串 / 一个
    /// 本家不认的自定义值）：`apply_to_chat` 对认不出的值会按官方默认档发
    /// `max`，此时再注绑定档会把用户的取值**悄悄换成另一个档位**，而请求照样
    /// 成功、用户看不出区别。让位之后，写进去的还是他自己那个值（归一结果与
    /// 不绑定时逐字相同）。`thinking.type = disabled` 是同一件事的另一种写法
    /// （chat 协议的客户端会这么关思考），同样让位。
    fn reasoning_patch(&self, level: &str, model: &str, body: &Value) -> ReasoningPatch {
        if body.get(super::reasoning::EFFORT_FIELD).is_some()
            || body
                .pointer("/thinking/type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("disabled"))
        {
            return ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考参数，绑定不覆盖",
            };
        }
        if !super::reasoning::is_glm53(model) && !super::reasoning::is_glm52(model) {
            return ReasoningPatch::Skip {
                reason: "该模型没有已确认的 ZCode 思考档位（目前只有 GLM-5.2 / 5.3 家族）",
            };
        }
        // 表外等级（界面的「自定义输入」）不注入：上游收不收没有证据，
        // 与 `model_rules` 的「表外值不参与转发」同一闸门（`off` / `none`
        // 在更早一步就被通用注入点拦下，这里兜的是直调本函数的路径）。
        if crate::server::core::model_rules::reasoning_rank(level).is_none() {
            return ReasoningPatch::Skip {
                reason: "该等级不在通用档位表内（自定义等级不参与转发）",
            };
        }
        match super::reasoning::target_effort(model, Some(level)) {
            Some(value) => ReasoningPatch::Set {
                field: super::reasoning::EFFORT_FIELD,
                value: Value::String(value.to_string()),
            },
            None => ReasoningPatch::Skip {
                reason: "该等级没有可翻译的 ZCode 目标值",
            },
        }
    }

    /// 随请求上行的思考等级（请求日志「上游等级」列的采集口）。
    ///
    /// 默认实现读的是**客户端原值**（并集键链），而本家对 GLM-5.2 / 5.3 家族
    /// 会把它归一成上游认的档位再发 —— 那一列于是会写出一个没发出去的档位
    /// （`xhigh` 而字节里是 `max`；5.2 上 `none` 而字节里是 `minimal`）。所以
    /// 这里按**本家 resolver 的同一条规则**读（[`super::reasoning::target_effort`]）：
    /// 模型属于受管家族且体里有 `reasoning_effort` 时报归一后的值；
    /// 其余情形回落到默认实现（别的键名原样随请求上行，见 trait 的说明）。
    fn outbound_reasoning(&self, body: &Value) -> Option<String> {
        let model = body.get("model").and_then(Value::as_str).unwrap_or("");
        let declared = body
            .get(super::reasoning::EFFORT_FIELD)
            .and_then(Value::as_str);
        if let Some(level) = super::reasoning::target_effort(model, declared) {
            return Some(level.to_string());
        }
        crate::server::core::model_rules::read_client_level(body)
            .filter(|level| !crate::server::core::model_rules::reasoning_is_off(level))
    }

    /// 构造上游请求：按账号的「使用套餐」（`zcodePlan`）**二选一**。
    ///
    ///   - `coding-plan`（默认）：`POST {openai_base}/chat/completions`，
    ///     `Authorization: Bearer {accessToken}`，模型名不改写；思考等级由
    ///     [`Self::reasoning_patch`] 注入映射绑定的默认档、再由
    ///     `reasoning::apply_to_chat` 归一后发出（客户端没绑定也没点名等级时
    ///     保持上游默认）；
    ///   - `start-plan`：`POST {zcode}/api/v1/zcode-plan/anthropic/v1/messages`，
    ///     `Authorization: Bearer {jwt}`，OpenAI 体翻成 Anthropic 并装配官方
    ///     系统提示词块 —— 细节全在 [`super::plan`]，本函数只做分派。
    ///
    /// 两条通道的凭证**不能互相替代**（编码套餐认 accessToken、活动套餐认套餐
    /// JWT），走错门的症状是「套餐已到期」这类业务拒绝而不是鉴权失败，所以
    /// 通道选择做成账号级设置、由用户明确指定（见 `zcode::plan` 的模块头）。
    /// 通道名从**会话**读（`AccountStore::session_from_record` 把记录上的
    /// `zcodePlan` 带了进来；缺失 = 编码套餐，与存量账号的行为逐字相同）。
    ///
    /// ── 为什么要带一整套「客户端身份头」───────────────────────
    /// 编码套餐的入口是**给官方客户端用的**，上游按客户端形态识别请求
    /// （参考实现的 `buildLlmIdentityHeaders` 逐字复刻 bundle 的 `g6n`）。
    /// 只发一个光秃秃的 `Authorization` 也能过鉴权，但上游一旦按形态限流或
    /// 灰度，缺头就是难查的失败 —— 而这一套头是免费的。取值能对上的对上、
    /// 对不上的用参考实现自己的兜底（`unknown`）。活动套餐通道用**同一套**
    /// 头（只多一个 Anthropic SDK 的 UA 后缀，见 `plan::build_request`）。
    ///
    /// 两处**故意**与参考不同（别当成漏抄）：
    ///   · 不发 `X-Os-Version`：参考实现取 `os.release()`，Rust 侧要为此引一个
    ///     系统信息 crate；它是可选头，参考实现取不到时同样省略；
    ///   · 不在这条路上发 `X-Device-Mid`：参考实现明确注明推理路径**从不**发它
    ///     （那是领取/余额那种控制面请求的要求，见 `claim.rs`）；活动套餐通道
    ///     把设备标识放进请求体的 `metadata.user_id`。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        if super::plan_of(account) == super::PLAN_START {
            return super::plan::build_request(self.region, account, body, client_headers);
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
        headers.extend(identity_headers(None));
        let mut outgoing = body.clone();
        // 思考等级：这条通道没有「思考预算」这个概念，`reasoning_effort` 是唯一
        // 的旋钮，注了才拦得住「小输出额度被思考吃光、正文空串」（见
        // `super::reasoning` 的模块头与 `apply_to_chat` 的说明）。
        let wire_model = outgoing
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        super::reasoning::apply_to_chat(&mut outgoing, &wire_model);
        // 流式补 `stream_options.include_usage`：上游只在显式要求时才回用量帧，
        // 不补的话这条通道的输入/输出/缓存三列恒为 0（面板看着像没计费）。
        // 与 workbuddy 的 `normalize` 同一手法：**不覆盖**客户端已有的取值。
        ensure_include_usage(&mut outgoing);
        Ok(ChatRequestPlan::chat(
            format!("{}/chat/completions", self.openai_base_url()),
            headers,
            outgoing,
        ))
    }

    /// 推理请求恒为 stream:true；ZCode 的 JSON 响应是业务拒绝，HTTP 200
    /// 也要先读错误体（实测 1005 / exceed quota limit），不能当 SSE 消费。
    fn is_error_response(&self, status: u16, headers: &HeaderMap) -> bool {
        if !(200..300).contains(&status) {
            return true;
        }
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }

    /// 上游错误分类。
    ///
    ///   - `401` → TokenExpired（编排层会刷新后同账号重试一次；本家当前的
    ///     `refresh_access_token` 会如实报错，见模块头）
    ///   - `429` → QuotaLimited（编码套餐是「5 小时 + 每周」双窗口限额，
    ///     上游不给结构化的恢复时间，`reset_at` 给 None 让冷却走兜底时长。
    ///     「套餐已到期」也是这条 —— 上游用 429 表达它，而**换通道**
    ///     （账号设置里的「使用套餐」）才是出路，见 `plan` 的模块头）
    ///   - 业务码 `1005` → QuotaLimited / 429（活动套餐可用 HTTP 200 返回
    ///     exceed quota limit；不猜测额度恢复时间）
    ///   - 其余 → 交给共用的内容拦截判定（`content_block`），
    ///     与其余各家同一口径 —— 编码套餐同样会有内容策略拦截
    ///
    /// ── 文案的取值链为什么要多一段 `error.message` ──────────────
    /// 活动套餐通道说 Anthropic 协议，它的错误体是
    /// `{"type":"error","error":{"type":"...","message":"..."}}` —— 顶层没有
    /// `message`/`msg`。不多认这一段，那条通道上所有错误都会退化成
    /// 「上游错误」，用户看不到「套餐已到期」「人机验证」这类关键原文。
    /// 少数业务码（3007 人机验证 / 3012 身份块 / 3001 参数）再补一句可执行的
    /// 提示（[`super::plan::code_hint`]）—— 分类动作不变，只是把「为什么」说清。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .or_else(|| {
                error_body
                    .get("error")
                    .and_then(|error| error.get("message"))
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            // 上游的 401 是**空体**（实测：`Content-Length: 0`），而这正是
            // 活动套餐通道上最常见的一种失败（套餐 JWT 用坏了）—— 照旧给
            // 「上游错误」四个字，用户只会看到「上游返回 401: 上游错误」，
            // 完全不知道下一步该做什么。这一档因此按状态码给一句可执行的话。
            .unwrap_or_else(|| {
                if status == 401 {
                    "凭证被上游拒绝（活动套餐认套餐 JWT、编码套餐认访问令牌）：\
                     请在「账号」页重新登录该账号"
                } else {
                    "上游错误"
                }
            });
        let code = error_body.get("code").and_then(Value::as_i64);
        // 3007 是「验证码令牌被拒」的信号：既回给用户（见 `plan::code_hint`），
        // 也记进令牌池 —— 界面据此把库存立刻补齐（见 `captcha` 的模块头）。
        //
        // 同时它是「推理免码」这件事的**唯一证伪手段**：上游哪天把
        // `skip_model_request` 收回去、或压根不认这个开关，我们手上不会有别的
        // 信号。所以让它当场把结论翻回「要码」，免码路线的代价就被钉在
        // 最多一次 3007 往返（见 `captcha::note_challenge_and_require_token`）。
        if code == Some(3007) {
            super::captcha::note_challenge_and_require_token();
        }
        let message = match code.and_then(super::plan::code_hint) {
            Some(hint) => format!("上游返回 {status}: {raw}（{hint}）"),
            None => format!("上游返回 {status}: {raw}"),
        };
        if status == 401 {
            return UpstreamErrorClass::TokenExpired { message };
        }
        if status == 429 || code == Some(1005) {
            return UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            };
        }
        // 被响应头检查拒绝的 2xx JSON 必须产生失败状态；保留原始 HTTP
        // 状态在 message 中，未知业务码不猜测为凭证失效或额度用尽。
        let status = if (200..300).contains(&status) { 502 } else { status };
        content_block::classify_or_fatal(status, error_body, message, code)
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

    /// 本家有余额概念：套餐额度（含活动发放的体验套餐）在 billing 网关上读得到，
    /// 见 `balance.rs`。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 查询套餐余额（`GET {zcode}/api/v1/zcode-plan/billing/balance`）。
    ///
    /// 走的是**套餐 JWT + X-Device-Mid**，与推理用的 `accessToken` 不是一套凭证；
    /// 缺 JWT 时返回可识别的「未配置」（400 + `usage_not_configured`），
    /// 由用户在账号里补上即可 —— 不是失败。取值与解析见 `balance.rs`。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        Box::pin(async move { super::balance::query_usage(store, account_id).await })
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
///
/// `user_agent_suffix` 是**两条通道唯一的一处头差别**：官方客户端的 Anthropic
/// SDK 会把 `ai-sdk/anthropic/{ver}` 拼进 UA（活动套餐通道带，编码套餐不带）。
/// 做成参数而不是两份头表，是为了让其余九个头的取值只有一处定义。
pub(super) fn identity_headers(user_agent_suffix: Option<&str>) -> Vec<(String, String)> {
    let version = super::claim::app_version();
    let user_agent = match user_agent_suffix.map(str::trim).filter(|text| !text.is_empty()) {
        Some(suffix) => format!("ZCode/{version} {suffix}"),
        None => format!("ZCode/{version}"),
    };
    vec![
        (
            "HTTP-Referer".to_string(),
            "https://zcode.z.ai".to_string(),
        ),
        ("User-Agent".to_string(), user_agent),
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

/// 流式请求补 `stream_options.include_usage = true`。
///
/// ── 为什么要补 ──────────────────────────────────────────────
/// OpenAI 协议的流式响应**默认不带用量帧**，上游只在客户端显式要求时才在流末
/// 补一帧 `usage`。不补的后果是这条通道的请求日志三列（输入 / 输出 / 缓存）
/// 恒为 0 —— 面板看着像「这条没计费」，也让「用量对不上上游账单」这类问题
/// （issue #56）失去参照物。非流式响应本来就带 usage，因此这里只动流式。
///
/// ── 为什么不覆盖客户端的取值 ────────────────────────────────
/// 客户端可能显式写了 `include_usage: false`（少数客户端拿它省一帧），
/// 那是它的选择；网关补的是**缺省值**，不是替它做决定 —— 与 workbuddy
/// `normalize` 的 `stream_options` 处理同一条口径。
fn ensure_include_usage(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let options = object
        .entry("stream_options".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(options) = options.as_object_mut() {
        options
            .entry("include_usage".to_string())
            .or_insert_with(|| json!(true));
    }
}
