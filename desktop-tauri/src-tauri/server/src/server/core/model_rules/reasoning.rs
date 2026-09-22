//! 思考等级绑定（`mappings[].reasoning`）—— R7，照抄 OmniProxy 的**手动绑定**形态。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 与 `cline.rs` 拆出去的同一理由（见那里的模块头）：这一段有自己的候选表、
//! 自己的归一规则、以及一段必须解释清楚的设计取舍（下面那节），与 `mod.rs`
//! 里的「规则机制本身」（禁用 / 隐藏 / 映射的增删改查）不是一回事；
//! 放一起会把 `mod.rs` 推过项目约定的 800 行。
//!
//! ── 这是什么 ──────────────────────────────────────────────
//! 参考 OmniProxy「模型管理 → 思考等级」的手动绑定：**每条映射**可以额外带一个
//! 等级字符串，值取自 [`REASONING_LEVELS`]（就是 OmniProxy 的
//! `GENERIC_REASONING_LEVELS`），也可以填表外的自定义等级（界面上有
//! 「自定义输入」入口）。**缺失 / 空串 = 不覆盖**，即不绑定。
//! 本项目明确**不引入 models.dev 那套自动匹配**（那份实现的另一半是用
//! models.dev 的模型库精确匹配出候选等级，OmniProxy 都没匹配上时才退回这张通用表）。
//!
//! ── 为什么挂在映射条目上（而不是另立一张表）───────────────────
//! 映射是「对外名 → 上游模型」的一条规则，思考等级是这条规则上的**附加改写**，
//! 两者同生共死（删掉映射，等级也就没有承载对象了）。OmniProxy 也是把
//! `reasoning_override` 挂在绑定行上（`model_gateway_mappings.reasoning_override`
//! 列，与 `downstream_model_id` / `provider_id` / `upstream_model` 同一行）。
//! 分开存会多出一张需要按 (alias, target, provider) 三元组同步的表，收益为零。
//! 「三元组相同 = 同一条」的语义也让改等级复用新增映射那条接口
//!（见 `api::model_manage::add_mapping` 的三态说明）。
//!
//! ── 绑定怎么生效（接入点是发送侧那一步按家改写）───────────────
//! 等级与「这一家收哪个模型名」是**同一条映射**上的两个属性，因此两者在**同一处
//! 解析**：`upstream::payload::send_body` 调
//! `catalog::wire_target_for_provider` 拿到 `WireTarget { model, reasoning }`
//! —— `reasoning` 就是**决定了这个发送名的那条映射**上绑的等级（映射改写时是
//! 那一条；本名直发时是名字与之相同的按家条目，见下面第 4 条）—— 再把它交给
//! 承载家的 `ProviderAdapter::reasoning_patch` 翻译成本家上游认识的字段。
//! 一次解析、两个属性同源，这是「A 家的等级不会用到 B 家」的**全部**保证：
//! 跨家串味唯一可能的来源是两处各自解析一次映射，而这里只有一个解析点。
//!
//! ── 各家的翻译规则（有证据的两家才接）────────────────────────
//! ```text
//!   catpaw   reasoning_effort ∈ {low, high, max}（上游 resolve_effort 硬校验，
//!            别的值当场 400）。6 档两两归并：
//!              minimal | low   → low
//!              medium  | high  → high
//!              xhigh   | max   → max
//!   qoder    写 reasoning_effort，值**原样**交给 qoder::protocol::resolve_thinking
//!            —— 它按模型自己声明的 efforts 归一与回退（minimal→low、
//!            high|max→xhigh、模型不支持的档位退回该模型默认档）。「这一家收
//!            哪个档位」在 Qoder 是模型级知识，翻译留在那边一份。
//!   workbuddy / raccoon / autoclaw / cline
//!            **不接**：四家都不注入档位字段（autoclaw 会改写 `model` 与 system
//!            提示前缀，但那两处与档位无关，见它的 `adapter` / `prompt`），项目里
//!            没有任何证据表明上游认识档位字段。塞一个上游不认识的键不叫
//!            「生效」，只是把未知参数推给上游 —— 适配器的默认实现就是
//!            「不接」，接一家要有一家的证据。
//! ```
//!
//! ── 哪些情况**故意不注入**（每一类都有具体理由）───────────────
//!   1. **`off` / `none`（关闭思考）**：本项目没有安全的表达方式 ——
//!      Qoder 的 `enable_thinking = false` 会让 Qwen3.8 系列行为异常
//!      （思考混进正文，或需要推理时直接断连，见 `qoder::protocol` 模块头），
//!      CatPaw 根本没有「关」这一档。这两档一律不注入（判据
//!      [`is_thinking_off`]），宁可不生效也不发一个会弄坏请求的字段。
//!   2. **表外的自定义等级**（界面提供「自定义输入」）：各家的能力范围不同，
//!      无法判断上游收不收；给 CatPaw 发一个错的档位是当场 400 —— 而这条请求
//!      在本功能之前是能用的。所以**只有** [`effort_rank`] 认得的正向档位会
//!      被翻译，表外值照旧保存、照旧显示，只是不参与转发。
//!   3. **客户端请求体里已经指定了思考档位**：那是用户的明确意图，绑定是
//!      「没指定时的默认」，不覆盖它。判定由各家适配器做（它只认自己那个
//!      resolver 读的键，不另抄一份键名）。这与 OmniProxy 的
//!      `reasoning_override` 的**覆盖**语义有意不同：本项目在绑定之前就已经有
//!      「客户端字段直达上游」的链路（CatPaw 的 `resolve_effort`、Qoder 的
//!      `resolve_thinking` 都从请求体里读），覆盖会把客户端显式传的值换成一条
//!      与本次请求无关的配置 —— 而用户给某一次请求指定档位，恰恰比映射上的
//!      默认值更具体。
//!   4. **本名直发**（该家原生承载请求名，`wire_target_for_provider` 的 ①）：
//!      名字不是任何映射给的，此时**只认「名字不变的那条按家映射」**（target
//!      与即将发出的名字相同）。于是：
//!        - 用户为「catpaw 的 kimi-k3」建的 `kimi-k3 → kimi-k3（catpaw）`
//!          这条**同名映射**上的等级**生效**（那是界面能表达「这个名字在这家
//!          用这个等级」的唯一入口）；
//!        - 而 `foo → bar（catpaw）` 这条映射在 `foo` 被 catpaw 原生承载时
//!          **并不生效**（原生路由优先，发出去的是 `foo`），它的等级也就不会
//!          被用到这次请求上 —— 否则就是一个没生效的映射去影响另一个模型，
//!          与「等级必须跟着那条映射走」自相矛盾。
//!      判据（名字是否一致）与理由写在 `catalog::wire_target_for_provider` 里。
//!   5. 这家翻译不了（见上表）：适配器返回 `Skip`，不注入、保持原有行为，
//!      并在详细日志里说明原因。
//!
//! 生效与不生效都走 `logging::verbose` 记一行（`upstream::payload` 里），
//! 于是「我绑了为什么不生效」在详细日志里能直接读到答案。

/// 思考等级的通用候选表（**照抄 OmniProxy 的 `GENERIC_REASONING_LEVELS`**）。
///
/// 那一份的用途是「models.dev 未精确匹配时的兜底候选 + 对话调试台选择器的
/// 全部可选值」；本项目不引入 models.dev 那半边，只用这张列表当作
/// 「手动绑定的可选项」—— 所以它是这份数据的**事实来源**，界面的下拉从这里取
///（`catalog::manage_view` 把它作为 `reasoningLevels` 一并发给前端，
/// 前端不自己抄一份，避免两处漂移）。
///
/// 顺序有意义（由弱到强，`off` 在最前）：界面上就按这个顺序铺选项。
/// 值本身是上游约定的字符串，**不要**改写大小写或翻译。
pub const REASONING_LEVELS: &[&str] =
    &["off", "none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 通用表里**可以翻译给上游**的正向档位（由弱到强，不含 `off` / `none`）。
///
/// 它由 [`REASONING_LEVELS`] 去掉「关闭思考」两档得到，是 [`effort_rank`] 的
/// 事实来源：**只有**这里的值会被注入上游（理由见模块头第 1、2 条）。
/// 数组顺序 = 强弱顺序，`effort_rank` 直接拿它当下标用。
const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// 等级字符串的长度上限（自定义等级也走这里）。
///
/// 32 个字符足够放下任何上游的档位名（`minimal-thinking-budget` 之类），
/// 而这个值会落进 config.json 并（将来）原样发给上游 ——
/// 允许无限长只是给配置里留一个能被拿来塞脏数据的位置。
const REASONING_MAX_CHARS: usize = 32;

/// 归一化一个思考等级：去首尾空白 → 过长或空串返回 None（= 不覆盖）。
///
/// **不校验是否在 [`REASONING_LEVELS`] 里**：界面提供「自定义等级」入口，
/// OmniProxy 同样允许表外的值（它把上游模型记忆过的自定义等级并进候选）。
/// 在这里按白名单挡掉会让那条路直接失效（值存不下来、也显示不出来）；
/// 而「表外的值不参与转发」这条**另有闸门**（[`effort_rank`] 返回 None，
/// 见模块头第 2 条），两道闸分工不同：这里管「能不能存」，那里管「能不能发」。
///
/// 过长的值返回 `None` 而不是报错：本函数是**读侧**的容错入口（旧配置、手改的
/// 配置文件都会经过它），读侧一律宽容（与 `api_keys` 那条硬不变量同一取舍）。
/// 写入侧（`api::model_manage::add_mapping`）会先拦下超长值并给 400。
pub fn normalize(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > REASONING_MAX_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// 该等级是不是通用表里的「关闭思考」两档（`off` / `none`）。
///
/// 这两档在本项目**一律不注入任何上游**（理由见模块头第 1 条：Qoder 关思考会
/// 让 Qwen3.8 异常，CatPaw 没有这一档）。调用方（`upstream::payload` 的注入点）
/// 用它把「关闭思考」与「表外值」分开措辞 —— 两者都不注入，但用户看到的原因
/// 完全不同，日志里含糊过去就等于没说。
///
/// 忽略大小写（用户在自定义输入框里写 `OFF` 显然也是这个意思）。
pub fn is_thinking_off(level: &str) -> bool {
    let trimmed = level.trim();
    trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("none")
}

/// 正向档位 → 序号（0 = 最弱，5 = 最强）。`off` / `none` / 表外值一律返回 None。
///
/// 返回 None 的含义是「**这个值不该被注入上游**」，不是「这个值不合法」——
/// 表外值合法地存在于配置与界面里（见 [`normalize`]），只是没有可翻译的目标。
///
/// 两个消费方：`upstream::payload` 用它做注入前的闸门；`catpaw::models` 用它把
/// 6 档折成上游那 3 档（`rank / 2`）。**不要**在这里做任何「按家归一」——
/// 每家能收什么由各家的适配器回答（`ProviderAdapter::reasoning_patch`）。
///
/// 忽略大小写（与 [`is_thinking_off`] 同口径）。
pub fn effort_rank(level: &str) -> Option<usize> {
    let trimmed = level.trim();
    if trimmed.is_empty() {
        return None;
    }
    EFFORT_LEVELS
        .iter()
        .position(|known| trimmed.eq_ignore_ascii_case(known))
}
