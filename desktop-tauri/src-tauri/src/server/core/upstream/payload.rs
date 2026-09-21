//! 一次转发的只读输入与**发送体选择**（从 `provider_loop.rs` 拆出，单文件行数约定）。
//!
//! ── 发送体为什么在转发前决定 ────────────────────────────────
//! 请求体从 `api::chat` **原样**进来（去重键也取自原始请求体）。内容处理只在
//! **凭证已就绪、即将发送之前**发生，且是否处理由配置里的全局开关
//! `sanitizeBlacklistFingerprints` 决定（快照见
//! [`ProviderContext::sanitize_fingerprints`]）：
//!   - 开关关着 → 客户端**原始**请求体（不处理、不计数）；
//!   - 开关开着 → 用 `core::sanitize` 在**副本**上处理一次。
//! 于是首选与故障转移到的家拿到的都是同一份处理结果；同一 provider **同池**
//! 换账号重试复用同一份（不重复处理、不重复统计 —— 键是「家 × 账号池」，
//! 因为发送名跟着账号所在池走，见下）；「这一家没有可用凭证」时根本走不到
//! 处理点，不产生一次已转发的处理。
//!
//! ── 与改造前的差异：不再有「按 provider 作用范围」────────────────
//! 改造前脱敏是按 provider 逐家判定的（配置里勾了哪几家，只有那几家的请求
//! 过脱敏）。规则集换成硬编码之后这一维**整体去掉**：开关是全局的，
//! 要么所有出站请求都剥离指纹、要么都不剥离。理由见 `core::sanitize` 的
//! 模块头（规则是「上游会误拦的固定模板串」，与哪一家上游无关）。
//!
//! ── model 字段的按家改写（备援名）────────────────────────────
//! 候选链经备援扩池后，链上某家的目录里认的可能是**备援名**而不是请求名
//! （WorkBuddy 的 `deepseek-v4.1-flash` vs 小浣熊的 `sn-deepseek-v4-1-flash`）。
//! 上游只认识自己目录里的真名，所以这一家即将发送前，body 的 model 也要换成
//! 该家承载的那个名字（`catalog::wire_target_for_provider`）—— 同家多条映射
//! （Cline 两池同名的短名）时按**当前账号所在池**选，改写只发生在
//! **发出去的字节**上：`ctx.body`（记账 / 限额键 / 日志里的模型）保持客户端
//! 请求名不变。与脱敏同一个时机与缓存口径：同一家同池换账号重试复用同一份。
//!
//! ── 但「限额键」是个例外：它必须是改写后的真名 ──────────────────
//! 上面那句「限额键保持请求名不变」在 2026-09 之前是这么写的，也正是那次
//! 事故的根因：限额是**上游按真名记的**，冷却键用请求名会写在一个上游永远
//! 不认的名字上（映射别名），于是判定侧查不到、已限额的账号被反复选中。
//! 现在 [`send_body`] 把改写结果一并交出来（[`SendBody::wire_model`]），
//! 调用方拿它当冷却键 —— 记账 / 日志里的模型仍是请求名，只有 `rateLimits`
//! 的键跟着上游走。完整论证见 `routing::CooldownKeys`。
//!
//! ── 思考等级绑定（`mappings[].reasoning`）为什么也在这一步 ─────
//! 等级与「这一家收哪个模型名」是**同一条映射**上的两个属性，因此两者在同一次
//! 解析里一起取出（`catalog::wire_target_for_provider` 返回的 `WireTarget`
//! 同时带着 `model` 与 `reasoning`）—— 这是「A 家的等级不会用到 B 家」的保证：
//! 只要两处各解析一次映射，两处各自的候选选择规则迟早分叉。等级随后交给**承载
//! 那家**的 `ProviderAdapter::reasoning_patch` 翻译（每家自己能收什么由它回答），
//! 注入点不认识任何一家的字段名。哪些情况故意不注入（关闭思考、表外自定义值、
//! 客户端已显式指定、这家翻译不了）见 `model_rules::reasoning` 的模块头。
//!
//! 处理算法本身不在这里：指纹改写在 `core::sanitize`，模型名判定在
//! `core::providers::catalog`（本模块只决定「在什么时机、对哪一家、用哪一份」）。

use std::borrow::Cow;
use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::server::logging;

use super::usage::RequestTelemetry;

/// 一次转发的只读输入（打包传入，避免多参数函数在两层循环里各自展开）。
pub(super) struct ProviderContext<'a> {
    /// 客户端请求体（**原始**：已 stream:true；未做任何内容处理；未注入 system
    /// —— 那是适配器的事）。各 provider 的实际发送体由 [`send_body`] 决定。
    pub body: &'a Value,
    /// 客户端是否要流式（决定成功后的形态：SSE 透传 or 聚合）
    pub stream: bool,
    /// 客户端入站请求头（适配器契约的一部分；本期实现不读）
    pub client_headers: &'a HeaderMap,
    /// usage / 尝试次数旁路槽
    pub telemetry: &'a Arc<RequestTelemetry>,
    /// 本次请求的指纹脱敏开关**快照**（请求开始时取一次，见 `upstream::forward`）。
    ///
    /// 为什么随请求取快照而不是每家转发前现读：同一次请求内这个开关必须一致，
    /// 否则用户在请求进行中改了设置，会出现「前一家脱敏过、后一家没脱敏」
    /// 这类语义漂移；快照也让判定与日志用的是同一份值。
    pub sanitize_fingerprints: bool,
    /// 本次请求命中的网关 Key 的**可用提供商**白名单（R9；`None` = 不限制，
    /// 见 `core::key_scope` 模块头）。
    ///
    /// 为什么放在这里（转发上下文）而不是让选路层自己去读请求扩展：本结构就是
    /// 「一次转发的只读输入」的汇聚点（脱敏开关、客户端头、telemetry 都在这里），
    /// 候选链的过滤与它同源 —— 两个消费方（候选链过滤、选路循环）读同一份快照，
    /// 语义不会中途漂移。传引用是因为它由 `upstream::forward` 的栈帧持有，
    /// 生命周期覆盖整条转发链。
    pub key_scope: Option<&'a crate::server::core::key_scope::KeyScope>,
}

/// 某一家 provider 实际要发送的请求体（**每次转发前**决定，不做跨家复用），
/// 以及这次发送用的上游模型名。
///
/// ── 为什么把「上游模型名」和请求体绑在一个返回值里（别拆成两次解析）──
/// 这个名字是**限额冷却的键**：上游按它记额度，`rateLimits` 也按它落盘
/// （见 `routing::CooldownKeys`）。它必须与**真正发出去的字节**同源 ——
/// 若调用方自己再调一次 `wire_target_for_provider` 去算，就有了两处解析、
/// 两个可能分叉的答案，而分叉的表现正是本项目最忌讳的那类静默错误：
/// 「冷却写在一个键上、查在另一个键上，于是已限额的账号被反复选中」。
/// 一次解析、两个产物（字节 + 名字）同源，这种错在结构上就不可能发生。
///
/// `wire_model` 在请求体没有 `model` 字段时是空串（那时上游收到的是它自己的
/// 默认模型，网关无从知道名字，冷却也按空键走 —— 与改造前逐字一致）。
pub(super) struct SendBody<'a> {
    /// 实际要发出去的请求体：脱敏未命中且模型名无需改写时借用客户端原始
    /// body（零拷贝），否则是处理副本。
    pub body: Cow<'a, Value>,
    /// 该家实际收到的上游模型名 —— 也是它 `rateLimits` 冷却的键。
    pub wire_model: String,
}

/// 某一家 provider 实际要发送的请求体（**每次转发前**决定，不做跨家复用）。
///
/// 返回 [`SendBody`]：指纹脱敏未命中且模型名无需改写时零拷贝借出客户端原始
/// body；脱敏命中或需要把 model 换成该家真名时借出处理副本。判定与处理分别在
/// `core::sanitize` / `core::providers::catalog`，本函数只负责「在正确的
/// 时机问一次」—— 时机是「这一家即将发送之前」，所以同一家内部换账号重试
/// 不会重复处理、重复统计。
///
/// **顺带采集上游模型名**：发给该家的名字在这里定稿（无论是否改写），
/// 立即记入 telemetry（覆盖式，最后一次为准 —— 与 provider 字段同一口径，
/// 429 换家后留下的是实际承载那一次的名字）。请求日志的「上游模型」列
/// 因此不再需要猜测。
///
/// **顺带采集脱敏命中**：`sanitize_body` 返回的命中标签立即写进 telemetry，
/// 于是「这次请求命中了哪几条规则」跟着请求一起落进请求日志的 `sensitiveHits`。
/// 采集点只能是这里 —— 只有本函数拿得到那份命中明细。
///
/// ── 采集是**累计**而不是覆盖 ─────────────────────────────────
/// 与 `note_attempt` 的「最后一次为准」不同，命中按**并集**累加：候选链上
/// A 家处理过、降级到 B 家又处理一次，同一条规则会被两轮各命中一次。
/// 「这次请求命中了什么」才是用户要的答案（B 家只是同一份内容又匹配了一遍），
/// 所以同一家重复发送时不重复计（`send_cache` 已经保证了这一点：同一家同池
/// 的发送体只算一次），跨家则合并计数。
/// 实时性上也有必要：命中发生在**某一家即将发送时**，那时请求还没收尾，
/// telemetry 槽位还开着（记账点读快照在最后）。
pub(super) fn send_body<'a>(
    ctx: &'a ProviderContext<'_>,
    provider_id: &str,
    account: Option<&Value>,
) -> SendBody<'a> {
    let mut body = match ctx.sanitize_fingerprints {
        true => match crate::server::core::sanitize::sanitize_body(ctx.body) {
            Some((scrubbed, hits)) => {
                // 命中表可能为空：`sanitize_text` 末尾的去空白也能单独构成一次
                // 改动（预检命中、但没有任何规则真正替换）。那时不该往请求日志
                // 的「敏」标签里写一条空记录。
                if !hits.is_empty() {
                    ctx.telemetry.note_sensitive_hits(&hits);
                }
                Cow::Owned(scrubbed)
            }
            None => Cow::Borrowed(ctx.body),
        },
        false => Cow::Borrowed(ctx.body),
    };
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if requested.is_empty() {
        // 没有 model 字段：不改写，也没有可用的冷却键（空串，与改造前一致）
        return SendBody { body, wire_model: requested };
    }
    // 一次解析出两个属性：该家要收的名字 + 跟着那条映射走的思考等级
    // （同源，见模块头「思考等级绑定为什么也在这一步」）
    let wire = crate::server::core::providers::catalog::wire_target_for_provider(
        &requested,
        provider_id,
        account,
    );
    ctx.telemetry.note_upstream_model(&wire.model);
    rewrite_model(&mut body, &requested, &wire.model, provider_id);
    apply_reasoning(
        &mut body,
        provider_id,
        &requested,
        &wire.model,
        wire.reasoning.as_deref(),
    );
    SendBody { body, wire_model: wire.model }
}

/// 把发送体里的 model 字段换成该 provider 认识的真名（仅当需要换时才复制）。
///
/// `requested` 是请求名、`wire` 是 `catalog::wire_target_for_provider` 已经算好
/// 的该家真名（调用方算一次，这里不再查目录）；只有两者不同时才把 `Cow`
/// 升级成 Owned —— 常规路径（本名该家认识）保持借用零拷贝。
fn rewrite_model(body: &mut Cow<'_, Value>, requested: &str, wire: &str, provider_id: &str) {
    if wire.eq_ignore_ascii_case(requested) {
        return;
    }
    logging::verbose(
        "[Upstream]",
        &format!("provider={provider_id} 按该家目录改写模型名 {requested} → {wire}（备援名）"),
    );
    let object = body.to_mut().as_object_mut();
    if let Some(object) = object {
        object.insert("model".to_string(), Value::String(wire.to_string()));
    }
}

/// 把映射上绑的思考等级交给**承载这家**的适配器翻译，并按结果改写发送体。
///
/// ── 为什么整段注入都在这里（而不是各家适配器内部）─────────────
/// 适配器只回答「这一家怎么翻译」（`ProviderAdapter::reasoning_patch` 返回
/// 字段名与取值或一句「不注入」），改写动作、日志、以及「哪些等级根本不该问」
/// 这三件事是通用的，放在这一处：五家各写一遍同样的注入代码，漏掉任何一处
/// 都会变成「某家的绑定静默失效」。
///
/// ── 两道在**问适配器之前**就拦下的闸（顺序有意义）─────────────
///   1. `level = None`：这条映射没绑等级（或这次发送是原生直发且本家没有
///      对应条目）—— 绝大多数请求走这一条，直接返回；
///   2. `off` / `none`：**关闭思考**。本项目没有安全的表达方式（Qoder 的
///      `enable_thinking = false` 会让 Qwen3.8 系列异常，CatPaw 没有这一档），
///      所以根本不问适配器 —— 让每家自己判断会诱使某家实现成「发一个 false」，
///      而那正是本项目明令避免的。判据与理由在 `model_rules::reasoning`。
///
/// 其余情形（表外自定义等级、客户端已指定、这家不接）由适配器返回 `Skip`
/// 并给出原因 —— 那些都是**各家自己的知识**，这里不替它判断。
///
/// ── 日志与「不改写」的关系 ──────────────────────────────────
/// 成功与跳过都记一行 verbose（`Skip` 的那一行尤其重要：用户绑了不生效时，
/// 详细日志里能直接读到为什么）。`body` 只在 `Set` 分支才 `to_mut()` ——
/// 跳过的路径零拷贝、零分配，与 `rewrite_model` 的取舍一致。
///
/// ── 「注入」这个词的边界（读日志时别误解）─────────────────────
/// 这一行说的是「**把值写进了发出去的请求体**」，不是「上游一定会照它执行」：
/// 适配器给出 `Set` 时就已经确认了本家认这个字段（那是它的判断，见
/// `reasoning_patch` 的契约），但**值**仍可能被本家的协议层再加工 ——
/// Qoder 的 `protocol::resolve_thinking` 就会按模型自己声明的 efforts 归一与
/// 回退（例如 `minimal` → `low`、模型不支持的档位退回该模型默认档）。
/// 那一步的上下文（模型声明）只有协议层拿得到，所以它留在那边是对的；
/// 这里不做二次记录，免得两处日志各说一个值。
///
/// 文案里带上 `requested → wire_model`（与 `rewrite_model` 那条同一形状）：
/// 同名映射（对外名与上游 id 相同）时两者相同，用户仍能从这一行看出
/// 「这条等级来自哪条映射」；不同名时它就是「这条映射做了什么改写」的完整记录。
///
/// **不碰客户端自己传的思考字段**：覆盖与否是适配器的判断（它复用本家那个
/// resolver 读的键名），这里只往它指定的 `field` 上写。
fn apply_reasoning(
    body: &mut Cow<'_, Value>,
    provider_id: &str,
    requested: &str,
    wire_model: &str,
    level: Option<&str>,
) {
    let Some(level) = level.map(str::trim).filter(|text| !text.is_empty()) else {
        return;
    };
    if crate::server::core::model_rules::reasoning_is_off(level) {
        logging::verbose(
            "[Upstream]",
            &format!(
                "provider={provider_id} 映射 {requested} → {wire_model} 绑定的思考等级为\
                 「{level}」（关闭思考），本网关不向任何上游发「关闭思考」字段，跳过注入"
            ),
        );
        return;
    }
    // 未知 provider id 直接返回：选路早已按注册表校验过（`provider_loop` 对未知
    // id 直接 503），走到这里说明调用链坏了 —— 什么都不做比 panic 安全
    // （release 是 panic=abort）。
    let Some(kind) = crate::server::core::providers::kind_from_id(provider_id) else {
        return;
    };
    let adapter = crate::server::core::providers::adapter::adapter_for(kind);
    match adapter.reasoning_patch(level, wire_model, body.as_ref()) {
        crate::server::core::providers::adapter::ReasoningPatch::Set { field, value } => {
            // 先取可变对象再打日志：请求体不是 JSON 对象时写不进去（正常路径不会
            // 发生 —— chat 入口已校验过是对象），此时**不能**打「已注入」——
            // 「日志里说的就是字节里有的」是这一整段可观测性的全部价值，
            // 一句与字节不符的「已注入」比没有日志更坏。
            let Some(object) = body.to_mut().as_object_mut() else {
                logging::verbose(
                    "[Upstream]",
                    &format!(
                        "provider={provider_id} 映射 {requested} → {wire_model} \
                         绑定的思考等级 {level} 未注入：请求体不是 JSON 对象"
                    ),
                );
                return;
            };
            logging::verbose(
                "[Upstream]",
                &format!(
                    "provider={provider_id} 映射 {requested} → {wire_model} \
                     注入思考等级 {level} → {field}={value}"
                ),
            );
            object.insert(field.to_string(), value);
        }
        crate::server::core::providers::adapter::ReasoningPatch::Skip { reason } => {
            logging::verbose(
                "[Upstream]",
                &format!(
                    "provider={provider_id} 映射 {requested} → {wire_model} \
                     绑定的思考等级 {level} 未注入：{reason}"
                ),
            );
        }
    }
}
