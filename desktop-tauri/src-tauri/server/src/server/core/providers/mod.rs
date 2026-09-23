//! 提供商（provider）注册表：Agent2API 多上游架构的**身份与元数据**事实来源。
//!
//! ── 为什么要有这个模块 ──────────────────────────────────────
//! 改造前整个网关只有 WorkBuddy 一个上游，provider 概念是隐含的：账号就是
//! workbuddy 账号、端点常量写在 `endpoints.rs`、鉴权逻辑写在 `auth.rs`。
//! 多提供商（注册表现有七家：WorkBuddy / 小浣熊 raccoon / CatPaw / AutoClaw /
//! Qoder / Cline Free / Cline Pass，七家都参与推理转发，见各自的 `mod.rs`），
//! 后两家分别由 W5-T-d4 与 W4b-T-c2 接上各自的适配器）之后，
//! 「这个账号属于哪家」「这一家叫什么名字」需要一个
//! 全局唯一的定义点 —— 就是本模块。
//!
//! 账号数据里的 `provider` 字段存的是 **provider id 字符串**
//! （`"workbuddy"` / `"raccoon"` / `"catpaw"` / `"autoclaw"` / `"qoder"` /
//! `"cline-free"` / `"cline-pass"`）：它要落进 accounts.json、要出现在 HTTP 响应里、
//! 还要被前端当筛选条件用，所以
//! **字符串本身就是契约**，不能随手改。
//! 本模块负责 id ↔ `ProviderKind` ↔ `ProviderMeta` 三者互查，避免这些字符串
//! 散落到 account_store / api 各处各写一遍（写错一处不会报错，只会静默失配）。
//!
//! ── 本文件与其它模块的分工（W4a 起多家 provider）─────────────
//! 本文件只有**身份与元数据**：枚举、注册表、三个查询函数。
//! 架构文档 §4.2 的 `ProviderAdapter` trait 与适配器注册表在 `adapter.rs`，
//! 各家的实现分别在 `workbuddy.rs` / `raccoon/` / `catpaw/` / `autoclaw/` /
//! `qoder/`。过渡期用过的占位适配器（`pending.rs`，W6 删除）已随四家全部接上
//! 真身而退场 —— 现在 `adapter_for` 的 match 是穷举的，加新 kind 时编译器会强制
//! 给出分支，「注册了 provider 却忘了接线」在编译期就被拦住，不再需要运行期的
//! 占位实现兜底。
//!
//! **加新 provider 的最小改动面**：`ProviderKind` 加变体 + 本文件 `PROVIDERS`
//! 加条目 + `kind_from_id` / `kind_id` 各加一个分支 + `adapter.rs` 的
//! `adapter_for` 接上真身适配器。账号层的参与由「注册表 + 各层
//! 经 `kind_from_id` 判定」自动派生，不需要再改那些文件里的任何 id 清单。
//!
//! ── 静态注册表为什么用切片而不是 HashMap ─────────────────────
//! provider 是**编译期内置**的（内置五家，不是插件），数量个位数；
//! 用 `&'static [ProviderMeta]` 可以让 `meta()` 直接返回 `&'static` 引用
//! （没有生命周期纠缠、也没有锁），且列表顺序稳定 —— 前端拿到的 `providers`
//! 数组顺序稳定，便于比对与展示。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   adapter.rs  ProviderAdapter 契约：请求构造 / 错误分类 / 凭证 /
//!               模型刷新（架构文档 §4.2；转发编排只通过它认识 provider）
//!   workbuddy.rs WorkBuddy 实现（头集合、URL、system 注入、429/6004/11128、
//!               token 刷新、模型清单与远程刷新）
//!   raccoon/    小浣熊实现（W3-T4）：
//!                 mod.rs         适配实现（Bearer JWT、429 限额、目录刷新）
//!                 credentials.rs JWT 解码、桌面端实时登录态、单飞刷新与回写
//!                 models.rs      模型清单（静态兜底 5 个 + /model_catalog 刷新）
//!   catalog.rs  聚合模型目录：各家清单合并成 /v1/models 的单一视图，
//!               并回答「某模型名由哪些 provider 提供」（能力判定）
//!   router.rs   模型路由：候选集合 + config.json 的 providerRoute 优先级
//!               → 逐家尝试的候选链（转发编排消费）
//!   catpaw/      CatPaw（美团）实现（架构文档 §9）：上游不是 OpenAI 协议而是
//!               自有 conversation 协议，需要消息归一化 / 指纹链 / 会话注册表。
//!                 adapter.rs      ProviderAdapter 实现（W5-T-d4：is_stateful=true，
//!                                 会话式转发入口 forward_conversation）
//!                 credentials.rs  凭证（账号记录 / auth.json / CATPAW_COOKIE）
//!   autoclaw/    AutoClaw（智谱 autoglm）适配实现（架构文档 §10）：
//!                 adapter.rs     ProviderAdapter 实现（W4b-T-c2：无状态，
//!                                双模型标识头 + X-Authorization + SSE model 回写）
//!                 crypto.rs      Electron safeStorage 解密（DPAPI + AES-256-GCM）
//!                 credentials.rs 凭证来源（auth.json / openclaw.json / 环境变量
//!                                / 账号记录）+ mtime 缓存
//!                 refresh.rs     刷新（单飞 + 400002 降级；**只读不回写**）
//!                 models.rs      模型路由表（静态映射 + zai_auto 回退）
//!   qoder/       Qoder（账号管理 **+ 推理转发**）：
//!                 endpoints.rs   地区与鉴权端点（含推理网关基址）
//!                 machine.rs     PKCE 随机串与本机标识（getrandom）
//!                 oauth.rs       国际版 PKCE 设备授权
//!                 credentials.rs 凭证格式（兼容 Qoder-Proxy 的 access/refresh）
//!                 auth.rs        PAT 换取令牌 / 用户资料
//!                 refresh.rs     续期（单飞 + 比较再写）
//!                 balance.rs     额度查询（归一成账号页的统一形状）
//!                 cosy.rs        COSY 请求签名 + 请求体编码（鉴权核心）
//!                 models.rs      模型目录（两地区缓存 + 静态兜底 + 远程刷新）
//!                 protocol.rs    OpenAI ↔ Qoder 协议转换
//!                 stream.rs      上游 SSE 信封解包 + 思考标签拆解
//!                 chat.rs        转发编排（构造 → 发送 → 翻译）
//!   cline/       Cline（官方 api.cline.bot）：账号管理 **+ 推理转发**，
//!                按**两个提供商**接入（`cline-free` 免费池 / `cline-pass` 订阅池）。
//!                实现是参数化的：模块共用，实例按池给。
//!                 credentials.rs 凭证格式（workos: 前缀 token）+ 桌面端登录态读取
//!                 login.rs       设备授权登录（WorkOS RFC 8628）
//!                 refresh.rs     续期（refresh_token 轮换 + 单飞）
//!                 models.rs      模型目录（recommended-models + 两池清单 + 静态兜底）
//!                 balance.rs     额度查询（credit 余额归一）
//!                 adapter.rs     ProviderAdapter 实现（无状态，OpenAI 兼容，
//!                                按池参数化）
//! 本文件仍然只做「身份与元数据」这一件事，不认识磁盘也不认识账号。

pub mod adapter;
pub mod autoclaw;
pub mod catalog;
pub mod catpaw;
pub mod cline;
pub mod content_block;
/// 自定义提供商的**运行期接线**（目录聚合的追加段 + Chat Completions 协议
/// 转发）。它不进本文件的身份体系（`ProviderKind` / `PROVIDERS`，见
/// `custom_providers` 的模块头），但目录合并与转发的分派点都以 id 字符串
/// 形态调用它 —— 挂在这里与其它子模块并列，便于对照「内置家走适配器、
/// 自定义家走独立通道」的两条路径。
pub mod custom;
pub mod qoder;
pub mod raccoon;
pub mod refresh_flight;
pub mod router;
pub mod workbuddy;

use serde_json::{json, Value};

/// 内置提供商种类。
///
/// `Copy` 是刻意的：它只是个身份标签，各处传参、放集合里都不该有所有权负担。
/// `Hash` + `Eq` 供 `HashMap<ProviderKind, _>` 这类按 provider 分组的容器使用
/// （后续波次的限额冷却键、模型路由链都会用到）。
///
/// 变体顺序 = 注册表顺序（前端 providers 摘要、同优先级时的候选链次序都按它来），
/// 加新家请加在**末尾**并同步 `PROVIDERS`。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProviderKind {
    /// WorkBuddy（原唯一上游）
    WorkBuddy,
    /// 小浣熊（适配实现在 `raccoon/`：JWT 凭证 + `/model_catalog` + SSE 回写）
    Raccoon,
    /// CatPaw（美团；架构文档 §9）。适配实现在 `catpaw/adapter.rs`
    /// （W5-T-d4 接线）：有状态会话式转发，`is_stateful()` 为 true。
    CatPaw,
    /// AutoClaw（智谱 autoglm）**国内版**；架构文档 §10。适配实现在
    /// `autoclaw/adapter.rs`（W4b-T-c2 接线）：**无状态**（OpenAI 兼容 +
    /// `X-Authorization`，与 raccoon 同构），`sse_model_rewrite()` 为 true。
    ///
    /// ── 与 [`ProviderKind::AutoClawIntl`] 是同一套协议的两个地区 ────
    /// 两个构建（客户端里 `isOversea` 编译期常量）共用账号接口与签名指纹
    /// （appId/appKey 两地逐字相同，已实测），只有站点不同。
    /// **provider id 保持 `autoclaw` 不改名**：它是存量账号 `accounts.json` 里的
    /// 落盘契约，改名会让那些账号升级后变成「未知 provider」而静默消失
    /// （见 `autoclaw::region` 的模块头）。变的只有展示名。
    AutoClaw,
    /// AutoClaw **国际版**（`autoclaw-intl`）。与 [`ProviderKind::AutoClaw`]
    /// 同一套协议、不同站点（`autoglm-api.autoglm.ai`）。
    ///
    /// ── 为什么两个地区是两家 provider（与 Cline 两池同一思路）──────
    /// 把地区做成「一家的一个字段」的后果与 Cline 那次一模一样：地区成了
    /// **账号的属性**，界面上混在一起，而「哪个账号走哪个站点」在列表里
    /// 看不出来；账号记录也无法按地区隔离。按两家建模之后，各自有独立的账号、
    /// 清单、启停与映射，界面上各占一个分组、在添加弹窗里相邻。
    ///
    /// 实现是**一套**：`autoclaw::adapter::AutoClawAdapter` 持有一个
    /// `autoclaw::region::Region`，两个静态实例（`AUTOCLAW_ADAPTER` /
    /// `AUTOCLAW_INTL_ADAPTER`）由 `adapter_for` 按 kind 给出。地区 → provider
    /// 的互查在 `autoclaw::region::Region`（`kind` / `provider_id` /
    /// `from_provider_id`），别处不要再写 `"autoclaw-intl"` 这类字面量。
    AutoClawIntl,
    /// Qoder。适配实现在 `qoder/`：账号管理（国际版设备授权登录 / PAT /
    /// 凭证续期 / 额度查询）**加推理转发**。上游鉴权不是 Bearer 而是一套自签名
    /// 的 COSY 头、请求体要先编码再签名、响应还多包一层信封，因此
    /// `is_stateful()` 为 true（一次发送由适配器自己完成，见 `qoder/mod.rs`）；
    /// 它参与全局队列与模型广告，`supports_chat()` 为 true。
    Qoder,
    /// Cline **免费额度池**（`cline-free/...`）。官方 `api.cline.bot`。
    ///
    /// ── 为什么两池是两家而不是「一家的一个选项」（本次改动的核心）────
    /// 早先的实现把它们当成**同一个账号上的两个额度池**：账号记录带一个
    /// `pool` 字段，池过滤（`advertise_models`）按「账号库里有哪个池」决定
    /// 广告哪一批。那套做法的后果是：池是**账号的属性**，界面上一家 Cline
    /// 里混着两个池的模型（还随账号变化），而「哪个池能用」这件事在
    /// 模型列表里完全看不出来。
    ///
    /// 现在按**两个提供商**建模：各自有独立的账号、清单、启停规则与映射，
    /// 界面上各占一个分组。于是：
    ///   - `cline-free` 只列免费池的模型，`cline-pass` 只列订阅池的；
    ///   - 加了哪家的账号，哪家的模型才出现（`provider_available` 的既有口径）；
    ///   - 上游模型 id 的池前缀**仍是原样转发**的通道选择器（见下），
    ///     但「这家收哪个前缀的模型」已经由 provider 身份保证，
    ///     不再需要按账号里的 `pool` 字段做二次过滤。
    ///
    /// ── 模型名的两池前缀（转发侧仍然存在）──────────────────────
    /// 上游模型 id 形如 `cline-free/deepseek-v4.1-flash` 与
    /// `cline-pass/glm-5.3`，**必须原样发给上游** —— 前缀就是它的计费通道
    /// 选择器。对下游的友好名（剥掉前缀）由 `model_rules` 的默认映射种子给出，
    /// 见 `cline::models` 的模块头。
    ClineFree,
    /// Cline **订阅池**（`cline-pass/...`）。与 [`ProviderKind::ClineFree`]
    /// 同一上游、同一套协议，差别只有「收哪个前缀的模型」。
    ///
    /// ── 与 ClineFree 共享的东西（不要各自复制）──────────────────
    /// 凭证格式、API 基址、错误分类、SSE 回写、余额查询、远程目录接口
    /// （`GET {apiBase}/ai/cline/recommended-models` **拉一次就能拿到两个池**）
    /// 全部共用 —— 因此 `cline/` 下的实现是参数化的：
    /// `ClineAdapter` 持有一个 `Pool`，`adapter_for` 按 kind 给出该池的实例。
    /// 远程目录缓存也共用一份（`models::REMOTE`），两家只是按池过滤它。
    ClinePass,
}

/// 一个提供商的静态元数据。
///
/// 字段是 `&'static str`：全部来自本文件的常量，不需要 String 的分配与所有权。
///
/// 这里**没有**路由优先级：先用哪一家由账号优先级（全局一条队列）决定，
/// provider 只是账号的属性，不再有自己的一层排序。
pub struct ProviderMeta {
    /// provider id：落进 accounts.json、进 HTTP 响应、前端筛选都用它
    pub id: &'static str,
    /// 展示名（前端组头、日志里用中文/品牌名）
    pub label: &'static str,
}

/// 内置提供商注册表（顺序稳定，前端的 `providers` 摘要数组按此顺序输出）。
///
/// 新增 provider 时**只改这里** + 加一个 `ProviderKind` 分支：账号迁移的
/// 默认值（`DEFAULT_PROVIDER_ID`）、id 互查、账号层的 provider 白名单
/// （`kind_from_id` 的未知判定）都从本表推导 —— 各调用点不再各写一份 id 清单。
///
/// 注册表顺序只用于**展示**（providers 摘要、模型目录合并时同名模型的去重顺序）
/// 与旧数据迁移（把按家分队的优先级合并成全局队列时，作为旧默认路由顺序的依据）。
pub const PROVIDERS: &[ProviderMeta] = &[
    ProviderMeta { id: "workbuddy", label: "WorkBuddy" },
    ProviderMeta { id: "raccoon", label: "小浣熊" },
    ProviderMeta { id: "catpaw", label: "CatPaw" },
    // AutoClaw 两个地区**相邻**排列（本次改动的要求）：界面上它们是同一条产品线的
    // 两个版本，中间隔着别的家会让「找国际版」变成一次扫描。顺序也决定模型目录
    // 合并时同名模型先归谁家 —— 国内版在前，与存量账号的归属一致。
    ProviderMeta { id: "autoclaw", label: "AutoClaw 国内版" },
    ProviderMeta { id: "autoclaw-intl", label: "AutoClaw 国际版" },
    ProviderMeta { id: "qoder", label: "Qoder" },
    ProviderMeta { id: "cline-free", label: "Cline Free" },
    ProviderMeta { id: "cline-pass", label: "Cline Pass" },
];

/// provider id 在注册表里的下标（未知 id → None）。
/// 账号迁移用它还原旧版的默认路由顺序（10/20/30/40 与下标同序）。
pub fn provider_index(id: &str) -> Option<usize> {
    PROVIDERS.iter().position(|meta| meta.id == id)
}

/// 缺省 provider id：加载账号时发现记录里没有 `provider` 字段（或为空）
/// 一律补成它 —— 历史数据全部来自 workbuddy 单上游时代。
///
/// 定义成常量而不是散落的字面量：惰性迁移、`add_account` 写入、测试用值
/// 三处必须是同一个字符串。
///
/// **必须与 `PROVIDERS[0].id` 一致**（本值是同一件事的第二处声明）：
/// 默认为 workbuddy 是历史数据的语义（不是「注册表第一个」），所以不改成
/// 「取注册表首项」—— 那样将来有人在表头插一家新 provider 就会把全部历史
/// 账号静默改姓。这里改成常量 + 编译期无关的断言做不到（`const` 字符串比较
/// 在数组下标上不成立），由上面那条注释与本表的书写顺序负责。
pub const DEFAULT_PROVIDER_ID: &str = "workbuddy";

/// provider id → `ProviderKind`；未知 id 返回 `None`。
///
/// 走**注册表**做未知判定（不在这里另写一份 id 白名单），随后按 id 映射枚举。
/// 将来加 provider 时只改注册表 + 加一个 match 分支，不会出现两处清单不一致。
///
/// ── 为什么不再是 `_ => WorkBuddy` 兜底（W4a 的重要修正）──────
/// 改造初期注册表里只有 workbuddy + raccoon，于是写成了「raccoon 之外一律
/// workbuddy」的兜底。加入 catpaw / autoclaw 之后那个兜底**会把两家新 provider
/// 错吞进 WorkBuddy**：`/api/accounts` 里 `{"provider":"catpaw"}` 会被当成
/// workbuddy 账号存进 workbuddy 组，`providerRoute` 的键也会串味 —— 静默失配，
/// 界面上看不出任何异常。现在改成「**未知 id → None**」：不认识就是不认识，
/// 由各调用点按自己的语义处理（校验点 400、容错点跳过、展示点回显原文）。
///
/// ── 两层防线（说明它们各自能挡住什么，别误会成编译期保证）────
/// `&str` 的 match 永远需要兜底分支（Rust 无法对字符串做穷尽性检查），所以：
///   1. **注册表判定在最前**：id 不在 `PROVIDERS` 里直接 None。这是白名单的
///      事实来源，`PROVIDERS` 是唯一需要维护的清单；
///   2. **兜底分支只对「注册表里有、这里忘了分支」生效** —— 那种漂移无法在
///      编译期发现（这正是要警惕的），所以用 `debug_assert!` 在开发期喊出来，
///      release 里返回 None（safe side：新 provider 表现为「未知」而不是
///      「被误认成别家」）。将来 `ProviderKind` 加变体时，`kind_id` 的穷举
///      match 会先报编译错，提醒把这张表与这里一起更新。
///
/// 调用方对 `None` 的处理（W4a 已逐个核查，见各文件的注释）：
///   - **校验路径**（网关 Key 的可用提供商校验、
///     `api::config_api::parse_provider_route`）→ 400「未知的提供商」，本就如此；
///   - **容错路径**（`config.rs` 的优先级表解析、`catalog.rs` 的 `all_kinds`）
///     → `filter_map` 跳过该项（回落注册表默认值 / 不进候选链），不 panic；
///   - **展示路径**（`request_stats::report::provider_label`）→ 原样回显 id；
///   - **分派路径**（`api::accounts::add_account`）→ 走 workbuddy 分支
///     （老客户端不带 provider 字段的既有契约），已注册但未实现的两家在
///     那个 match 里被显式 400（不是新加的白名单，是穷举分支）；
///   - `router::route_for_forward` 的默认 provider 查不到 → 空链（调用点报
///     「没有可用的提供商」）。都是「跳过或报错」而非「当成别的家」。
pub fn kind_from_id(id: &str) -> Option<ProviderKind> {
    if !PROVIDERS.iter().any(|meta| meta.id == id) {
        return None;
    }
    match id {
        "workbuddy" => Some(ProviderKind::WorkBuddy),
        "raccoon" => Some(ProviderKind::Raccoon),
        "catpaw" => Some(ProviderKind::CatPaw),
        "autoclaw" => Some(ProviderKind::AutoClaw),
        "autoclaw-intl" => Some(ProviderKind::AutoClawIntl),
        "qoder" => Some(ProviderKind::Qoder),
        "cline-free" => Some(ProviderKind::ClineFree),
        "cline-pass" => Some(ProviderKind::ClinePass),
        // 走到这里 = 上面的注册表判定已放行、这个 match 却没有对应分支：
        // 只可能是有人给 `PROVIDERS` 加了条目忘了加这里。开发期喊出来；
        // release 返回 None（见上：宁可为「未知」，不可误认成别家）。
        other => {
            debug_assert!(false, "PROVIDERS 里的 id `{other}` 缺少 kind_from_id 分支");
            None
        }
    }
}

/// `ProviderKind` → provider id（`&'static str`，即注册表里那个字符串）。
///
/// `const fn`（W4a 起）：账号存储等模块要把 provider id 定义成**常量**
/// （例如 `account_store::RACCOON_PROVIDER_ID`），只有它能出现在 const 上下文里，
/// 于是那些常量也从注册表推导，而不是各处再写一份字面量。const fn 只做 match、
/// 不分配 —— 与普通调用完全同价。
pub const fn kind_id(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::WorkBuddy => "workbuddy",
        ProviderKind::Raccoon => "raccoon",
        ProviderKind::CatPaw => "catpaw",
        ProviderKind::AutoClaw => "autoclaw",
        ProviderKind::AutoClawIntl => "autoclaw-intl",
        ProviderKind::Qoder => "qoder",
        ProviderKind::ClineFree => "cline-free",
        ProviderKind::ClinePass => "cline-pass",
    }
}

/// 这个 id 是不是**注册表里已登记的 provider**（W4a 起的唯一「provider 白名单」口径）。
///
/// 语义与 `kind_from_id(id).is_some()` 完全相同，单独给个名字是为了让调用点
/// 表达出「这里在校验一个 id 认不认识」而不是「这里要拿枚举」：账号层的添加
/// 分支、配置层的 `providerRoute` 键校验、脱敏的 `providers` 数组校验都该用
/// 这一句 —— 于是将来加 provider 时**这些校验点一行都不用改**（注册表是唯一
/// 事实来源，见 `PROVIDERS`），也不会出现「某个模块忘了加新 id」的静默失配。
///
/// 反过来说：**不要**在任何地方另写 `matches!(id, "workbuddy" | "raccoon")`
/// 之类的清单 —— 那正是本函数要消灭的东西。
pub fn is_known_provider_id(id: &str) -> bool {
    kind_from_id(id).is_some()
}

/// provider id → 给人看的名字；未登记的 id 原样回显。
///
/// ── 为什么返回 `String` 而不是 `&str`（改动自有数据那天起就定了）────
/// 自定义提供商（`custom-` 前缀，见 `custom_providers`）的名字是**运行期
/// 数据**（存在 kv 配置里），拿不到 `&'static`。给 `&str` 凑寿命只有两条路：
/// leak 静态化（内存只进不出，禁用）或把名字缓存在某个全局里（与配置失同步
/// 的又一处风险）。返回 `String` 让调用点多付一次分配 —— 调用点全是日志与
/// HTTP 响应的展示路径，这点成本换「不泄漏、不失同步」是划算的。
///
/// ── 回退顺序 ──────────────────────────────────────────────
/// 1. `custom-` 前缀 → 先查 `custom_providers::label_of`（用户给的名字优先），
///    查不到（已被删除 / 手改数据塞进来的陌生 id）回显原 id；
/// 2. 其余走注册表：登记的返回 `label`，未登记的原样回显。
///
/// 回显成 id 而不是「未知」的理由不变：调用点手上的 id 来自真实数据（账号
/// 记录、`owned_by`、日志），真出现未登记的 id 时，回显原文比一句笼统的
/// 「未知」更能定位问题。前端也照这个口径做兜底（见 `ui/providers.js`）。
///
/// 这是「id → 给人看的名字」的**唯一入口** —— 别处不要再写
/// `match id { "catpaw" => "CatPaw", ... }`：那种表漏一家不会报错，
/// 只会让界面上少一个名字。
pub fn label_of(id: &str) -> String {
    if id.starts_with(crate::server::core::custom_providers::ID_PREFIX) {
        if let Some(label) = crate::server::core::custom_providers::label_of(id) {
            return label;
        }
        return id.to_string();
    }
    match kind_from_id(id) {
        Some(kind) => meta(kind).label.to_string(),
        None => id.to_string(),
    }
}

/// `ProviderKind` → 元数据。
///
/// 返回 `&'static`：元数据是编译期常量，调用方无需克隆或持锁。
/// 用 `kind_id` 反查注册表，保证 enum 与注册表不会各写一份 label/priority。
pub fn meta(kind: ProviderKind) -> &'static ProviderMeta {
    let id = kind_id(kind);
    PROVIDERS
        .iter()
        .find(|meta| meta.id == id)
        // 注册表里必然有（`kind_id` 的返回值就是注册表里的 id）。真出现不一致
        // （有人加了 enum 分支却忘了加注册表项）时回落到第一项而不是 panic ——
        // 本项目的 release 是 panic=abort，启动期 panic 会直接带走整个应用。
        .unwrap_or(&PROVIDERS[0])
}

/// 注册表的摘要 JSON 形态：`[{id, label, count}, ...]`。
///
/// `count` 由调用方传入的计数函数给出（账号存储那边按 provider 数账号总数）。
/// 用它而不是让本模块去读账号文件：本模块是**纯静态元数据**，不认识磁盘，
/// 于是它可以在任何初始化顺序下被调用。
pub fn summary_json<F>(count: F) -> Vec<Value>
where
    F: Fn(&str) -> usize,
{
    PROVIDERS
        .iter()
        .filter_map(|meta| kind_from_id(meta.id))
        .map(|kind| {
            let detail = meta(kind);
            json!({
                "id": detail.id,
                "label": detail.label,
                "count": count(detail.id),
            })
        })
        .collect()
}
