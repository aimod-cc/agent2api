//! ProviderAdapter 契约（Agent2API 改造 W2b-T3，架构文档 §4.2）。
//!
//! ── 这一层解决什么问题 ──────────────────────────────────────
//! 改造前，转发链路的每一步都默认「上游就是 WorkBuddy」：URL 是写死的
//! `/v2/chat/completions`、鉴权头按 workbuddy 的字段拼、429/6004 与 11128 的
//! 判定散在 `upstream/request.rs` 与 `rotate.rs`。多提供商之后，`upstream/`
//! 只做**协议无关的编排**（去重、逐家轮询、SSE 透传、聚合），
//! 所有「这一家长什么样」的知识收进各自的适配器 —— 本模块定义的就是那份契约。
//!
//! ── 三个动作（语义不得改，架构文档 §4.2 的硬要求）────────────
//!   1. [`UpstreamErrorClass::QuotaLimited`] → 标记账号对该模型冷却 + 换下一个账号；
//!   2. [`UpstreamErrorClass::TokenExpired`] → 刷新凭证后**同一账号**重试一次；
//!   3. [`UpstreamErrorClass::Fatal`]        → 原样透传给客户端。
//!
//! 这三档是转发编排唯一认识的错误处理方式，适配器只能在这三档里归类 ——
//! 多出来的分类在编排层没有对应分支，只会静默落进 Fatal，
//! 等于把「该轮换账号」的错误当成终态发给用户。
//!
//! ── 注册表为什么是「静态实例 + match」而不是 HashMap ─────────
//! 适配器是**无状态单例**（凭证在账号存储、目录在 `core::models` 的进程级句柄，
//! 适配器自己只持有常量与纯函数）。因此 `&'static dyn ProviderAdapter` 就够了：
//! 没有锁、没有初始化顺序、`adapter_for` 是 O(1) 且绝不失败。
//! 用 `OnceLock<HashMap<…>>` 反而要处理「注册表还没装好时被调用」的路径，
//! 而那条路径在 panic=abort 的 release 里只能 panic —— 得不偿失。
//!
//! ── 契约之外的九处扩展（都是带默认实现的加法，不改 §4.2 的方法）──
//!   1. `retry_advice`：识别「可退避重试的错误」并给出间隔与日志文案。
//!      §4.3 要求「11128 退避逻辑保持在转发层」——**循环**留在编排层，
//!      但「哪个码要退避、退多久」是 provider 知识（11128 是 workbuddy 的
//!      敏感词拦截码），放在这里才能让 upstream/ 一个 provider 分支都不留。
//!   2. `refresh_access_token`：401 后的**强制**刷新。`ensure_access_token`
//!      的语义是「取可用 token（含临期主动刷新）」，而 401 意味着 token
//!      被服务端拒绝 —— 它可能远未到临期窗口，只调 ensure 不会真的刷新。
//!      默认实现直接转调 `ensure_access_token`，provider 需要区分时再覆盖。
//!   3. `allows_anonymous_default_session`：本 provider 是否允许「一个账号
//!      都没有时用环境变量凭证/默认登录态转发」。workbuddy 有这条旁路
//!      （`WORKBUDDY_TOKEN`），小浣熊也有（`RACCOON_TOKEN`，见 `raccoon/mod.rs`）。
//!   4. `sse_model_rewrite`（W3-T4 追加）：SSE 透传**要不要把下发帧的 `model`
//!      改写成客户端请求的那个名字**。小浣熊上游网关会回自己的内部模型名
//!      （源实现 `raccoon-sse-pipe.mjs` 的 `pipeSseWithModelRewrite` 专门处理），
//!      而 workbuddy 上游原样回显。帧改写发生在 `upstream::ForwardStream`
//!      内部（那里就是 SSE 逐帧下发的唯一出口），所以这里只回答「要不要写」，
//!      通用层不出现任何 provider 分支 —— 与 `retry_advice` 同一分工。
//!   5. `supports_refresh` + `credentials_expiring`（凭证自动维护）：
//!      「这家能不能主动续期」「这个账号的凭证此刻算不算临期」是**各家的知识**
//!      （过期时间在哪个字段、用什么窗口，四家各不相同），因此由适配器回答，
//!      维护任务（`core::credential_maintenance`）只负责遍历与调用。
//!      放在这里而不是壳侧：壳侧只能按账号公开形态里的字段名判断，而那个字段名
//!      四家不同（小浣熊的过期时间在 `tokenExpiresAt`，公开形态里没有 `expiresAt`）
//!      —— 写死一个名字就会让别家的账号永远被判成「无需刷新」。
//!      契约：**判不出来就返回 false**（没有过期信息 / 账号不存在），
//!      宁可漏刷也不要凭猜测去打上游。
//!   6. `is_stateful` + `forward_conversation`（W4a 加钩子，W5-T-d4 接上消费方，
//!      架构文档 §4.2.1）：本 provider 的上游是不是**多步会话协议**
//!      （CatPaw 是：round → turn → 工具循环 → completed）。默认 false；
//!      当前唯一的 true 是 `catpaw::CatPawAdapter`。`forward_conversation` 是
//!      会话式转发的入口（默认 503，只有有状态 provider 覆写），编排层的
//!      `provider_loop` 按 `is_stateful` 分流到它 —— 分流落地后本文件不再需要
//!      任何死代码抑制。
//!   7. `supports_web_login` + `build_login_url` + `exchange_login_code`
//!      （「网页登录」）：拉起官方登录页 → 用户完成登录 → 回调里带回一次性 code
//!      → 网关用 code 换凭证并**落进账号库**。
//!      workbuddy **不走这三个钩子**：它的 state/authUrl 来自上游 `auth/state`，
//!      由 `core::login` 的后台任务轮询 `auth/token` 取回凭证，是另一套协议
//!      （见 `api::session::login_start`），既有的 `supports_web_login` 对它
//!      保持默认 false，以免路由层以为它能用这套回调链路。
//!      为什么整条链都放进 trait 而不是在 `api::session` 里写 provider 分支：
//!      「授权地址怎么拼、回调长什么样、用哪个接口换凭证、响应里哪几个字段是
//!      凭证、落账号走哪条添加路径」全是 provider 知识，与 `sse_model_rewrite`
//!      同一性质；路由层只该回答「这家支不支持」，不该认识小浣熊的字段名。
//!   8. `supports_usage` + `query_usage`（余额 / 积分查询）：「这家有没有余额概念」
//!      「这个账号的余额怎么查」是 provider 知识（四家的接口地址、鉴权头、
//!      凭证来源全不相同：workbuddy 走腾讯计费接口、小浣熊要 Bearer JWT、
//!      CatPaw 要网页会话 cookie `token2`、AutoClaw 走带签名的 userapi 域），
//!      所以由适配器回答，`api::accounts` 只负责并发调度与单账号失败的收敛。
//!      改造前这条能力**只有 workbuddy 有**：`resolve_batch_targets` 显式按
//!      provider 过滤目标集合，前端也只在 workbuddy 卡片上渲染「积分」按钮。
//!      统一形状的契约写在 [`ProviderAdapter::query_usage`] 的文档里。
//!   9. `supports_model_refresh` + `refresh_models(force)`（「刷新模型清单」）：
//!      「这家有没有远程目录可拉」与「这次要不要绕过缓存」是两件事，拆成两个
//!      入口（前者恒定能力，后者单次语义）。默认 false，四家里 WorkBuddy /
//!      小浣熊覆写成 true，CatPaw / AutoClaw 保持默认 —— 后两家的清单是
//!      **上游没有目录接口**的静态表（美团 CatPaw 的固定模型 id、AutoClaw 的
//!      静态路由表），点了只会白打一次网络。`refresh_models` 的返回类型也随之
//!      从 `()` 变成 [`ModelRefreshOutcome`]：自动路径不看它（照旧只打日志），
//!      手动路径靠它逐家如实汇报，见 [`refresh_implemented_forced`]。
//!
//! ── 未注册的 provider 怎么办 ────────────────────────────────
//! 四家 provider 在 [`adapter_for`] 里各自接上真身，那个 match 是穷举的：
//! 加新 kind 时编译器强制在这里给出分支，「注册了 provider 却忘了接线」的
//! 情形在编译期就被拦住。过渡期曾用它返回一个占位适配器
//! （`pending::PendingAdapter`，W4a–W5 期间 CatPaw / AutoClaw 借它顶着，
//! 两家分别在 W5-T-d4 / W4b-T-c2 换成真身；W6 收尾时该文件已随「四家全部
//! 接上真身」而删除）。空清单 + 503 的语义保留在 `catalog` / `accounts` 的
//! 校验里：`/v1/models` 不会广告清单为空的那家。

use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

use super::qoder;
use super::raccoon;
use super::{kind_id, meta, ProviderKind};

/// 转发前对「账号 + 请求体」的完整构造计划（架构文档 §4.2 的 ChatRequestPlan）。
///
/// ── 与 `upstream::request::TransportRequest` 的区别（重要）───
/// 那个是**传输层**形态：已序列化好的 `payload: String` + 出网代理，
/// 直接交给 `send_chat_request` 发出去。本形态是 **provider 视角**的：
/// body 还是 `Value`（不同家的改写规则不同，比如 workbuddy 要注入首条
/// system 消息），且**不含出网代理** —— 代理是账号级的、与 provider 无关，
/// 由编排层从账号会话里解析后自己带上。
pub struct ChatRequestPlan {
    /// 上游完整 URL（含路径）
    pub url: String,
    /// 请求头（不含出网代理相关；Authorization 在此）
    pub headers: Vec<(String, String)>,
    /// 已按 provider 规则改写过的请求体
    pub body: Value,
}

/// 上游错误分类（架构文档 §4.2；三个动作的语义见模块头）。
///
/// ── 文案口径：`message` 是**客户端可见的最终文案**（含前缀）────
/// 改造前客户端的错误体是 `上游返回 {status}: {上游原文}{提示}`，其中提示只在
/// workbuddy 的 11128（敏感词拦截）时追加。三者合成为一条文案的规则是
/// provider 知识（提示的措辞是 workbuddy 专属的），因此在适配器里拼好、
/// 由编排层原样使用 —— 既有用户的错误文案因此**逐字不变**。
#[derive(Clone, Debug)]
pub enum UpstreamErrorClass {
    /// 限额/风控 → 标记账号冷却并换下一账号（workbuddy 的 429 / code 6004）
    QuotaLimited {
        /// 上游给出的恢复时间戳（毫秒）；解析不出时 None（调用方给兜底冷却）
        reset_at: Option<i64>,
        /// 客户端可见的完整文案（`上游返回 {status}: {上游原文}`）
        message: String,
        /// 上游业务码（进 GatewayError 的 `upstream_code` 与限额事件）
        upstream_code: Option<i64>,
        /// HTTP 状态码
        status: u16,
    },
    /// token 失效 → 刷新凭证后用同一账号重试一次
    TokenExpired {
        /// 客户端可见的完整文案（`上游返回 401: {上游原文}`）
        message: String,
    },
    /// 透传给客户端的错误
    Fatal {
        status: u16,
        /// 客户端可见的完整文案（含 11128 之类的 provider 提示）
        message: String,
        /// 上游业务码（原样进 GatewayError 的 `upstream_code`）
        upstream_code: Option<i64>,
    },
}

/// 一次「可退避重试」的建议（见模块头扩展 1）。
///
/// 适配器把「要不要退避重试」连同**措辞**一起给出，编排层只负责
/// 打日志、睡这一觉、再发一次 —— 于是 11128/「敏感词」这类 provider 专属
/// 知识不会漏进 `upstream/`。
#[derive(Clone, Debug)]
pub struct RetryAdvice {
    /// 退避时长（毫秒）
    pub delay_ms: u64,
    /// 完整日志文案（provider 专属措辞，编排层原样打）
    pub log_message: String,
}

/// 单个提供商的转发适配能力（架构文档 §4.2）。
///
/// 实现者必须是**无状态或内部自带共享句柄**的：`adapter_for` 返回的
/// `&'static` 引用会被并发调用，而 trait 没有 `&mut self` 方法。
///
/// `Send + Sync` 是必需的：适配器会被转发任务在 await 点之间持有，
/// 而转发跑在 tokio 的多线程运行时上。
///
/// ── 为什么异步方法写成 `-> Pin<Box<dyn Future>>` ─────────────
/// trait 里不能写 `async fn`（Rust 1.77 的 MSRV 不支持原生 async fn in trait，
/// 而 `async_trait` crate 会给每个方法加一层 Box 分配）。手写 Box 的代价
/// 只在这几个签名上，收益是 trait 保持 object-safe 且不引入新依赖。
pub trait ProviderAdapter: Send + Sync {
    /// 本适配器对应的 provider
    fn kind(&self) -> ProviderKind;

    /// 该 provider 的模型清单（供聚合目录 / 路由判定）。
    ///
    /// 返回的是**上游原始形态**的记录数组（字段名与 `/v3/config` 或
    /// 小浣熊 `/model_catalog` 一致），聚合层用 `models::list_item` 做字段映射。
    fn list_models(&self) -> Vec<Value>;

    /// 用账号凭证构造上游请求（URL / 头 / 体）。
    ///
    /// ── `account` 传什么（实现与调用的约定）────────────────────
    /// 传的是**该账号的会话形态**（`store.get_session_by_id` /
    /// `auth.get_current_session` 的返回值：含 `auth.accessToken`、
    /// `endpoint`、`edition`、`account.uid`、`proxy`），而不是 accounts.json 的
    /// 公开形态 —— 公开形态按设计**不含** accessToken（只有 tokenTail），
    /// 拿它拼不出 Authorization。契约里写「account 为 accounts.json 的账号对象」
    /// 是就「这是谁的凭证」而言；具体形态由编排层保证（见 §4.2 末段的
    /// 「允许为贴合现有代码微调参数」）。
    ///
    /// `body` 是客户端请求体（未经改写）；`client_headers` 是客户端入站头
    /// （本期实现不读它，保留参数是为了将来「透传客户端 UA / 自定义头」
    /// 的 provider 不必改契约）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError>;

    /// 判定上游错误类型（status + 已解析的错误体）。
    ///
    /// `error_body` 是**已归一化**的错误对象：至少含 `code`（上游业务码，
    /// 可能为 null）与 `message`（上游文案，取自 message/msg/error.message，
    /// 非 JSON 响应则是截断后的原文）。归一化由传输层做（
    /// `upstream::request::read_upstream_error`），因为「怎么读一个 HTTP 错误体」
    /// 是协议层的事、与哪一家无关。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass;

    /// 取可用 access token（含临期主动刷新；刷新结果回写 store）。
    ///
    /// `account_id` 为空串表示「没有指定账号」：用默认登录态
    /// （环境变量凭证或存储里派生的当前账号）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    >;

    /// 刷新模型目录。
    ///
    /// ── `force` 是干什么的（本方法唯一的行为开关）──────────────
    ///   - `force = false`：**自动路径**的语义（`GET /v1/models` 的后台刷新、
    ///     启动刷新）。有缓存机制的家照常早退 —— 小浣熊的 10 分钟 TTL 就是为
    ///     自动路径设的：客户端每次拉模型列表都真打一次上游既没必要也招风控。
    ///   - `force = true`：**用户手动点「刷新模型清单」**的语义，跳过缓存/TTL
    ///     早退、真打一次上游。**这个参数不是可有可无的优化**：用户按下按钮的
    ///     全部预期就是「现在、真的去拉一次」，复用自动路径的实现会让小浣熊在
    ///     TTL 窗口内什么都不做就返回 —— 界面显示「刷新成功」而清单一个字没变，
    ///     与功能坏掉无法区分。缓存该不该绕过只由**谁发起**决定，因此把判断权
    ///     交给调用方（本参数），而不是让实现在内部猜。
    ///
    /// ── 失败与返回 ──────────────────────────────────────────────
    /// 刷新失败**不返回错误**：目录刷新是维护动作，失败时保留现有清单
    /// （与改造前 `refresh_with_current_account` 的取向一致 —— 只打日志）。
    /// 返回值 [`ModelRefreshOutcome`] 是给**手动路径**如实汇报用的
    /// （自动路径不看它）：失败要在结果里说清原因，否则用户点了刷新只能看到
    /// 「已刷新」，而清单其实没动。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>,
    >;

    /// 本 provider 是否有**已接入的推理转发能力**（五家现在都是 true）。
    ///
    /// ── 这条声明曾经区分过什么（历史，别误会成现在还有 false）─────
    /// Qoder 在接入推理协议之前返回 false（它当时只有账号管理能力），
    /// 判据的用途正是下面这条「与 `list_models` 的分工」。那家接上转发后本方法
    /// 在生产代码里已无 false 分支；保留这个入口是因为它编码的**语义**仍然成立 ——
    /// 「清单这次是空的」与「这家根本没有转发能力」是两件事，将来再有新 provider
    /// 的过渡期（先上账号管理、后接转发）仍需要它。
    ///
    /// ── 与 `list_models` 的分工（为什么不看清单是否为空）──────────
    /// workbuddy 与 raccoon 的清单来自远程目录，拉取失败或没登录时**本来就可能
    /// 为空**，但它们能转发。拿清单当判据会把这两家在那种时刻误判成「不能转发」。
    /// 因此本方法是一个**恒定能力声明**（编译期常量），与 `supports_usage` /
    /// `supports_refresh` 同一性质；也正因为它是常量，调用点（账号存储派生全局
    /// 队首）可以逐账号调用而不必付 `list_models` 的克隆开销。
    ///
    /// 消费方：`account_store::pick_current`（全局队首 = `/api/session` 的
    /// `currentAccountId`、退出登录的删除目标、界面 ★）。没有转发能力却排进队首，
    /// 会让顶栏把它显示成「当前登录态」、并让「退出登录」把它删掉。
    fn supports_chat(&self) -> bool {
        true
    }

    /// 本 provider 是否有**可拉取的远程模型目录**（模块头扩展 9）。
    ///
    /// 默认 false = 这家没有远程目录接口，清单是本地的静态表。判据与
    /// `supports_refresh` 同一口径：**上游到底有没有那个接口**，不是「我们想
    /// 不想实现」。当前四家：WorkBuddy 是 `GET /v3/config`、小浣熊是
    /// `GET {llmBase}/model_catalog`（都为 true）；CatPaw（固定模型 id
    /// `kimi-k3` / `glm-5.3-flash`）与 AutoClaw（静态路由表 + `zai_auto` 回退）
    /// 的上游都没有目录接口，保持默认 false。
    ///
    /// 为什么要有这个查询（而不靠「反正刷新是空操作」）：这是**用户可见的
    /// 按钮**，界面对不支持的家必须如实说明（「这两家用的是固定清单」）。
    /// 声称支持却拿不到新清单，用户只会以为是 bug；遍历时对静态表打一次必然
    /// 白跑的网络请求更是纯粹的浪费。与 `supports_usage` 同一取舍。
    fn supports_model_refresh(&self) -> bool {
        false
    }

    /// token 被上游拒绝（401）后的**强制**刷新（模块头扩展 2）。
    ///
    /// 默认直接转调 `ensure_access_token`：对「401 即临期」的 provider 够用。
    fn refresh_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        self.ensure_access_token(store, account_id)
    }

    /// 「可退避重试的错误」的建议（模块头扩展 1）；None = 不重试。
    ///
    /// `attempt` 是**已经重试过**的次数（0 表示首次失败、还没重试过）。
    fn retry_advice(&self, _error_body: &Value, _attempt: usize) -> Option<RetryAdvice> {
        None
    }

    /// 本 provider 是否支持**主动**刷新凭证（模块头扩展 5）。
    ///
    /// 默认 false = 这家没有续期手段（没有 refreshToken、上游也没有刷新接口）。
    /// 判断依据是「上游到底有没有那个接口」，不是「我们想不想实现」：
    /// 声称支持却刷新不了，会让维护任务每次都白跑一趟并往日志里灌失败记录。
    ///
    /// 与 `refresh_access_token` 的关系：后者是「怎么刷」（401 之后的强制续期），
    /// 本方法是「能不能刷」（恒定能力）。维护任务先问这一个再动手 ——
    /// 对必然失败的家调用刷新只会制造噪音。
    fn supports_refresh(&self) -> bool {
        false
    }

    /// 该账号的凭证此刻是否**已过期或临期**（需要刷新）（模块头扩展 5）。
    ///
    /// 判据是各家自己的知识：workbuddy 看会话里的 `expiresAt`、小浣熊看 JWT 的
    /// `exp`、AutoClaw 看凭证里的 `expiresAt`；各自的**临期窗口也不同**
    /// （5 分钟 / 5 分钟 / 2 分钟），所以不能在这里写一个统一的阈值。
    ///
    /// ── 为什么「判不出来」一律返回 false ────────────────────────
    /// 没有过期时间（opaque token、JWT 里没有 `exp`）或账号不存在时返回 false：
    /// 无从判断就说「需要刷新」会让维护任务每轮都去打一次上游，而那种请求
    /// 要么白刷（token 本来还好），要么稳定失败（没有 refreshToken）。
    /// 这类凭证的续期只能等 401 时的那条懒刷新链路。
    ///
    /// **实现不得在内部取账号锁后跨 await**：本方法是同步的，取快照即返回；
    /// 真正的网络动作由 [`Self::refresh_access_token`] 在锁外做。
    fn credentials_expiring(&self, _store: &AccountStore, _account_id: &str) -> bool {
        false
    }

    /// 本 provider 是否允许「一个账号都没有时用默认登录态转发」
    /// （模块头扩展 3；workbuddy 的 `WORKBUDDY_TOKEN` 旁路）。
    fn allows_anonymous_default_session(&self) -> bool {
        false
    }

    /// 本 provider 的**环境变量凭证现在是否真的存在**（模块头扩展 3 的配套）。
    ///
    /// 与 `allows_anonymous_default_session` 的分工：那个回答「这家支持这条旁路
    /// 吗」（恒定能力），这个回答「此刻环境变量里有没有那串凭证」（运行时事实）。
    /// 聚合目录要用后者决定「这家现在算不算有可用登录态」——只认前者会让
    /// 一个没配环境变量的机器把 `/v1/models` 广告成「小浣熊可用」。
    ///
    /// 默认 false（不认环境变量）。实现里只做「存在且非空」的判定，不发网络。
    fn env_credentials_present(&self) -> bool {
        false
    }

    /// 本 provider 是否有「默认模型」概念（模块头扩展 4）。
    ///
    /// 语义（架构文档 §4.4 末句）：客户端**未指定 `model`** 时，网关仍取
    /// config.json 的 `defaultModel`（那是 workbuddy 语义）注入 —— 但只在
    /// 「这个默认值命中的 provider 里有家认这个概念」时才注入；
    /// 都不认就按「未指定」处理，让上游用它自己的默认模型。
    /// 否则会出现「注入了一个 A 家的模型名、请求却被路由到 B 家」。
    fn supports_default_model(&self) -> bool {
        false
    }

    /// SSE 下发帧要不要把 `model` 改写成客户端请求的名字（模块头扩展 4）。
    ///
    /// 默认 false（workbuddy 上游原样回显客户端给的模型名）。小浣熊上游会回自己
    /// 的内部名，因此它的适配器覆盖成 true —— 见 `upstream::sse` 的
    /// `model_rewrite` 参数与 `raccoon::mod` 的说明。
    fn sse_model_rewrite(&self) -> bool {
        false
    }

    /// 本 provider 的上游是否为**有状态（conversation 协议）**（模块头扩展 6，
    /// 架构文档 §4.2.1）。
    ///
    /// 默认 false（一次 HTTP 请求 = 一次对话）。CatPaw 覆写成 true：它的上游是
    /// round → event(running) → turn(SSE) → 工具循环 → event(completed) 的多步
    /// 会话（§9），单请求构造容纳不了。编排层据此把它分流到 adapter 自带的
    /// **会话式转发入口** [`Self::forward_conversation`]；产出与无状态路径同一种
    /// `ForwardOutcome`，于是 `chat.rs` 的其余链路（脱敏、记账、错误写出）零改动。
    ///
    /// 唯一的 true 是 `catpaw::CatPawAdapter`（W5-T-d4 接线后）。
    fn is_stateful(&self) -> bool {
        false
    }

    /// **会话式转发入口**（模块头扩展 6 的配套；架构文档 §4.2.1）。
    ///
    /// 只有有状态 provider 覆写它（当前是 CatPaw）。默认实现返回 503：走到这里
    /// 说明编排层把一家无状态 provider 当成了有状态的 —— 那是**内部契约错误**，
    /// 报错比让 `build_chat_request` 与 `forward_conversation` 各发一半安全。
    ///
    /// ── 与 `build_chat_request` 的分工 ──────────────────────────
    /// 无状态路径的编排是「构造请求 → 发 → 读错误 → 按分类换账号」；
    /// 有状态路径把这四步全收进适配器（CatPaw 是 round/turn/工具循环 + 注册表
    /// 状态机），编排层只保留它**必然共用**的那部分：账号选路循环、限额冷却、
    /// provider 轮询、telemetry 记账（见 `upstream::provider_loop` 的分流）。
    ///
    /// ── 参数语义（各 provider 通用，不是 CatPaw 专属）───────────
    ///   - `store`：账号存储（取凭证；**不持锁穿越 await** 由实现保证）；
    ///   - `account_id`：本次承载的账号 id（空串 = 没指定账号，用默认登录态）；
    ///   - `body`：客户端请求体（**原始形态**，归一化是实现内部的事）；
    ///   - `client_headers`：客户端入站头（会话类头如 `x-session-id` 在这里取）；
    ///   - `proxy`：账号级出网代理（已由编排层解析好，实现直接用于出网）；
    ///   - `stream`：客户端是否要流式（决定 `ForwardOutcome` 的形态）；
    ///   - `telemetry`：usage / 尝试次数的旁路槽（实现按需写入，可忽略）。
    fn forward_conversation<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
        _body: &'a Value,
        _client_headers: &'a HeaderMap,
        _proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        _stream: bool,
        _telemetry: &'a std::sync::Arc<crate::server::core::upstream::usage::RequestTelemetry>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::server::core::upstream::ForwardOutcome, GatewayError>,
                > + Send
                + 'a,
        >,
    > {
        let kind = self.kind();
        Box::pin(async move {
            Err(GatewayError::with_status(
                503,
                format!(
                    "该提供商不支持会话式转发（{}）：这是内部契约错误，请检查 is_stateful 的分流",
                    super::kind_id(kind)
                ),
            ))
        })
    }
    /// 本 provider 是否支持「拉起官方登录页、由网关换取凭证」这种**网页登录**
    /// （模块头扩展 7）。
    ///
    /// 默认 false。当前**只有小浣熊**覆写成 true：它的官方登录页在登录成功后会
    /// 跳转到一个固定形态的自定义协议回调（`office-raccoon://auth/callback?code=…`），
    /// 网关拿到那个一次性 code 就能换到独立凭证。CatPaw / AutoClaw 没有这样的
    /// 协议（后者的桌面端登录态只能从本机 auth.json 解密读出），因此保持默认值。
    ///
    /// workbuddy 也**刻意保持**默认 false：它的「网页登录」是上游 `auth/state`
    /// 那套无头流程（`core::login`），与这里的回调换码是两条链路。把它标成 true
    /// 会让路由层按回调链去调 `build_login_url`→None，把一个能用的功能变成 400。
    fn supports_web_login(&self) -> bool {
        false
    }

    /// 构造网页登录的授权地址，返回 `(auth_url, state)`（模块头扩展 7）。
    ///
    /// `state` 由实现生成与保管（不落盘）：它既是 CSRF 口径的一次性随机串，也是
    /// 回调进来时「这次授权确实由本进程发起」的唯一凭据，因此**必须**用不可预测
    /// 的随机源，且回调侧要逐字比对。
    ///
    /// 默认 None = 本家不支持（与 [`Self::supports_web_login`] 的默认值一致）。
    fn build_login_url(&self) -> Option<(String, String)> {
        None
    }

    /// 用回调里的一次性 `code` 换取凭证并**存入账号库**，成功返回账号 id
    /// （模块头扩展 7）。
    ///
    /// `state` 必须与 [`Self::build_login_url`] 返回的那个逐字一致 —— 实现要在这里
    /// 再校验一次（深度防御：调用链上游已校验过，但把校验放在「只有实现知道
    /// 该长什么样」的地方最可靠）。
    ///
    /// 落账号必须走本家**既有**的添加路径（小浣熊是 `add_raccoon_account`），
    /// 不另写一份落盘逻辑 —— 否则「手动添加」与「网页登录」两条路会在
    /// 校验、id 生成、优先级分配上慢慢分叉。
    ///
    /// 默认返回 501：走到这里说明路由层把一家不支持网页登录的 provider 送进来了，
    /// 那是内部契约错误，报错比静默什么都不做好。
    fn exchange_login_code<'a>(
        &'a self,
        _store: &'a AccountStore,
        _code: &'a str,
        _state: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>,
    > {
        let kind = self.kind();
        Box::pin(async move {
            Err(GatewayError::with_status(
                501,
                format!(
                    "该提供商不支持网页登录（{}）：这是内部契约错误，请检查 supports_web_login 的分流",
                    super::kind_id(kind)
                ),
            ))
        })
    }

    /// 本 provider 是否支持**余额 / 积分查询**（模块头扩展 8）。
    ///
    /// 默认 false。四家里四家都支持，但语义各不相同：
    ///   - workbuddy：腾讯计费接口的「积分简报」（`core::billing`，改造前唯一有的一家）；
    ///   - 小浣熊：官方积分钱包 + 订阅权益（`raccoon/balance.rs`）；
    ///   - CatPaw：美团 credit 域的额度接口，**需要额外配置网页会话凭证 token2**
    ///     （`catpaw/balance.rs`，未配置时返回可识别的「未配置」而不是失败）；
    ///   - AutoClaw：资产钱包 + 订阅信息（`autoclaw/balance.rs`）。
    ///
    /// 为什么这个方法而不是「能查就查」：前端按它决定卡片上要不要渲染「积分」按钮、
    /// 批量查询要不要把这个账号算进去。一家不支持却给了入口，用户点下去只会得到
    /// 一条必然失败的记录 —— 与 `supports_refresh` 同一取舍（如实回答恒定能力）。
    fn supports_usage(&self) -> bool {
        false
    }

    /// 查询某账号的余额 / 积分，返回**归一化后的 JSON**（模块头扩展 8）。
    ///
    /// ── 形状契约（`api::accounts` 的 `{results:[{id,name,usage,error}]}` 里
    ///    那个 `usage` 字段）────────────────────────────────────
    /// 两家新移植的实现（raccoon / autoclaw）遵守下面这套统一形状，
    /// **workbuddy 例外**：它的 `query_credits_summary` 是改造前就有的既有契约
    /// （`{kind, unlimited, totalLeft, planLeft, bonusLeft}`），前端积分面板
    /// 一直按它渲染，因此本方法对 workbuddy 直接透出既有形状、不做二次包装 ——
    /// 强行归一化会让那个面板的显示退化（这是明令禁止的）。
    /// ```jsonc
    /// {
    ///   "available": 1234.5,         // 可用总量（数字；判不出给 null）
    ///   "unit": "积分",               // 展示单位（前端拼文案用）
    ///   "wallets": [                  // 明细，可为空数组
    ///     { "type": "daily_points", "displayName": "每日积分", "balance": 800 }
    ///   ],
    ///   "subscription": {             // 可空
    ///     "planName": "…", "status": "…", "expireAt": 1789…,
    ///     "remainQuota": 100, "totalQuota": 1000
    ///   },
    ///   "raw": { }                    // 各家原始响应（排障用；前端默认不展示）
    /// }
    /// ```
    ///
    /// ── 契约（调用方按这几条写）────────────────────────────────
    ///   1. **不抛 HTTP 错误的语义**：实现返回 `Err` 表示「这次查询失败」，
    ///      由调用方把 message 放进结果的 `error` 字段（批量查询不因单账号失败中断）；
    ///   2. **超时由实现自己设**：余额接口是外部 HTTP，`egress` 的默认
    ///      read_timeout 是 600 秒（给 SSE 长连接用的），不设总超时会让一个挂住的
    ///      余额查询把前端转圈转到天荒地老（源项目给 15~20 秒）；
    ///   3. **401 由实现原样透出**（`GatewayError::with_status(401, …)`）：
    ///      调用方据此走「刷新后重试一次」的既有处置；
    ///   4. 未配置查询凭证这类**用户可修复的前置条件缺失**，用
    ///      [`usage_not_configured`] 那条文案报 400（带
    ///      [`USAGE_NOT_CONFIGURED_CODE`] 标记）—— 前端把它显示成中性的
    ///      「未配置查询」而不是红色失败。
    fn query_usage<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>,
    > {
        let kind = self.kind();
        Box::pin(async move {
            Err(GatewayError::with_status(
                501,
                format!(
                    "该提供商不支持余额查询（{}）：这是内部契约错误，请检查 supports_usage 的分流",
                    super::kind_id(kind)
                ),
            ))
        })
    }
}

/// 「该账号没有配置余额查询凭证」的**机器可识别标记**（`GatewayError::code`）。
///
/// ── 为什么要一个 code 而不是让前端匹配文案 ───────────────────
/// 前端要把这种情况显示成中性的「未配置查询」，而其余失败显示成红色错误。
/// 按文案匹配会在任何一次措辞调整后静默失效（变成一片红），按状态码又会与
/// 真实的 400 参数错误混在一起 —— 一个稳定的字符串标记是这里唯一可靠的判据。
pub const USAGE_NOT_CONFIGURED_CODE: &str = "usage_not_configured";

/// 「该账号没有配置余额查询凭证」的统一文案（`GatewayError::code` 同为
/// [`USAGE_NOT_CONFIGURED_CODE`]）。
///
/// ── 为什么这条要四家共用一份，而不是各家自己拼 ─────────────────
/// 前端按 `code`（不是文案）判定「这是未配置、不是失败」，但**文案仍会显示
/// 在面板上**；各家各写一句会让同一个「去设置里补一下凭证」的动作在四家账号上
/// 呈现四种措辞。CatPaw 是当前唯一会真正走到这里的一家（它的余额接口要的是
/// 网页会话凭证 `token2`，与转发用的 `X-Passport-Token` 不是同一个东西）。
///
/// `field_hint` 说明去哪儿补（「账号设置里的余额查询凭证」），拼进 message；
/// 状态码用 **400**：这是前置条件缺失、用户自己能修，不是服务端故障 ——
/// 前端据此把它显示成中性提示而不是红色错误。
pub fn usage_not_configured(provider_label: &str, field_hint: &str) -> GatewayError {
    GatewayError::with_status(
        400,
        format!(
            "该账号没有余额查询凭证（{provider_label} 的 {field_hint}），\
             请在账号设置里配置后再查询"
        ),
    )
    .with_code(USAGE_NOT_CONFIGURED_CODE)
}

/// 注册表：provider → 适配器实现。
///
/// **四家都已实现**：各自返回自己的静态实例。这个 match 是穷举的：加新 kind 时
/// 编译器会强制在这里给出一个分支，于是「注册了 provider 却忘了接线」这种事在
/// 编译期就被拦住。
///
/// 过渡期这里对未实现的 provider 返回过 `pending::PendingAdapter` 的占位实例
/// （空清单 + 503 错误）；随着 CatPaw（W5-T-d4）与 AutoClaw（W4b-T-c2）先后
/// 接上真身，占位实现失去全部调用点，W6 收尾时连同 `pending.rs` 一起删除 ——
/// 接线真身时只改这里的一行，其余调用点一行都不用动。
pub fn adapter_for(kind: ProviderKind) -> &'static dyn ProviderAdapter {
    match kind {
        ProviderKind::WorkBuddy => &super::workbuddy::WORKBUDDY_ADAPTER,
        ProviderKind::Raccoon => &super::raccoon::RACCOON_ADAPTER,
        ProviderKind::CatPaw => &super::catpaw::adapter::CATPAW_ADAPTER,
        ProviderKind::AutoClaw => &super::autoclaw::adapter::AUTOCLAW_ADAPTER,
        ProviderKind::Qoder => &super::qoder::QODER_ADAPTER,
    }
}

/// 已**完整实现**的适配器对应的 kind 列表（顺序 = 注册表顺序）。
///
/// ── 口径（W4a 明确，两种读法会得到不同结果，必须写清）──────────
/// 这是「已完整实现」而不是「已注册」：AutoClaw 曾在 W4a–W4b 之间处于「身份与
/// 注册表项都在、但适配器是占位」的中间态，那时它**不在本列表里**；
/// W4b-T-c2 接上真身（`autoclaw::adapter::AUTOCLAW_ADAPTER`）后列入本表 ——
/// 与 CatPaw 在 W5-T-d4 走过的路径相同。Qoder 也走过同一条路：接入推理转发
/// 之前它只有账号管理能力，本波次接上真身后列入。**五家现在全部在列表里**，
/// 与 `PROVIDERS` 的 id 集合一一对应（过渡期的占位实现已在 W6 随
/// `pending.rs` 删除）。
///
/// 为什么这份列表必须排除占位实现（当时的口径）：它的消费方是后台目录刷新
/// （`refresh_implemented` ← `api::chat::spawn_catalog_refresh` 与
/// `ServerState::bootstrap`）。对占位实例调 `refresh_models` 是空操作 ——
/// 列进去不会出错，但会让「implemented」这个词失去信息量：将来
/// 「这家的清单为什么刷新不了」之类的排障会从这份列表读起，
/// 一份混着占位实现的名字列表只会误导。
///
/// 注意这**不是** `/v1/models` 的「可用 provider」判据：那个走
/// `catalog::active_manifests`（清单非空 + 有可用登录态）—— 两处的口径各自
/// 成立、互不依赖。
pub fn implemented_kinds() -> Vec<ProviderKind> {
    vec![
        ProviderKind::WorkBuddy,
        ProviderKind::Raccoon,
        ProviderKind::CatPaw,
        ProviderKind::AutoClaw,
        ProviderKind::Qoder,
    ]
}

/// 一次模型目录刷新的结果（模块头扩展 9）。
///
/// ── 为什么不用 `core::models::RefreshOutcome` ────────────────────
/// 那个类型是 **workbuddy 单家**的既有契约（`{refreshed, count, source, reason}`，
/// 与 Node 版逐字对应），拴在 `ModelCatalog` 上。适配器层还要表达另外三家的事实
/// （小浣熊的「TTL 跳过」、静态表的「没有远程目录」），拿它覆盖四家会逼着三家填
/// 无意义的 `source`/`reason`，还要动 `core::models` 的既有类型。本类型是**最小
/// 适配器层形态**：workbuddy 在实现里把自己的 `RefreshOutcome` 搬运过来即可。
///
/// ── 三档语义（与 `credential_maintenance` 的状态常量同一口径）──
///   - `refreshed = true`：真的拿到了新清单（`count` 是落地后的条目数）。
///   - 否则 `message = None`：**没刷**（TTL 未到 / 上游没给可用清单 / 缺少登录态）。
///     「没刷」不等于「失败」：自动路径的意图本来就是「有缓存就用缓存」。
///   - 否则 `message = Some(..)`：**尝试了但失败**（状况见文案）。清单按既有
///     行为保留（不清空），但用户该看到为什么。
///
/// `count` 只在 `refreshed = true` 时有意义（其余情形为 0）。
#[derive(Clone, Debug, Default)]
pub struct ModelRefreshOutcome {
    /// 是否真的落地了一份新清单
    pub refreshed: bool,
    /// 落地后的目录条目数（`refreshed = true` 时有效）
    pub count: usize,
    /// 失败原因（`None` = 没失败：或成功，或本次按缓存/TTL 跳过）
    pub message: Option<String>,
}

impl ModelRefreshOutcome {
    /// 真刷新成功（`count` 是落地后的条目数）
    pub fn refreshed(count: usize) -> Self {
        Self { refreshed: true, count, message: None }
    }

    /// 没有刷（按缓存/TTL 跳过、上游没给可用清单、没有可用的家等）——
    /// 不是失败，只是这次没有新东西
    pub fn unchanged() -> Self {
        Self::default()
    }

    /// 尝试刷新但失败（`reason` 是会显示给用户的原因）
    pub fn failed(reason: impl Into<String>) -> Self {
        Self { refreshed: false, count: 0, message: Some(reason.into()) }
    }
}

/// 对**当前缓存清单**里的小浣熊模型补一次默认规则种子
/// （`raccoon-<hex>` 内部模型默认禁用 / `sn-` 前缀默认加去前缀映射，
/// 见 `model_rules::seed_raccoon_defaults`）。
///
/// 为什么不在 `raccoon::models::refresh` 一处就够了：刷新可能因 TTL 早退、
/// 上游失败而不落地新清单 —— 那条路径上没有种子可挂。启动后 / 首次请求前的
/// 缓存清单（升级用户的管理页第一次打开）也要有同样的默认值，所以在编排入口
/// 先按「现在手上的清单」补一次。幂等：种过的 id 不会再动。
fn seed_current_raccoon_defaults() {
    let ids: Vec<String> = raccoon::models::list()
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if let Some(summary) = crate::server::core::model_rules::seed_raccoon_defaults(&ids) {
        crate::server::logging::log("[Models]", &summary);
    }
}

/// 对**当前缓存清单**里的 WorkBuddy 模型补一次默认规则种子（默认只启用白名单
/// 内的模型，见 `model_rules::seed_workbuddy_defaults`）。
///
/// 与 `seed_current_raccoon_defaults` 同理：刷新可能因失败 / 无登录态而不落地
/// 新清单 —— 那条路径上没有种子可挂，启动后手里的这份清单（内置或旧缓存）
/// 也要有同样的默认值。幂等：种过的 id 不会再动。
fn seed_current_workbuddy_defaults() {
    let ids: Vec<String> = crate::server::core::models::global_catalog()
        .list()
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if let Some(summary) = crate::server::core::model_rules::seed_workbuddy_defaults(&ids) {
        crate::server::logging::log("[Models]", &summary);
    }
}

/// 对**当前缓存清单**里的 Qoder 模型补一次默认规则种子（默认只启用白名单内的
/// 模型，见 `model_rules::seed_qoder_defaults`）。
///
/// 委托给 `qoder::models::seed_default_rules` —— 那边取的是两个地区的**并集**，
/// 与刷新落地时种的是同一份口径。要这一手补种的理由与 WorkBuddy 相同：Qoder
/// 在没有账号 / 远程刷新失败时手里只剩静态兜底清单，而**升级用户**的并集正是
/// 那份兜底 —— 不补种的话，他们打开管理页看到的仍是旧的全开状态。
fn seed_current_qoder_defaults() {
    qoder::models::seed_default_rules();
}

/// 让所有**已实现**的 provider 各刷新一次模型目录（后台任务入口）。
///
/// 调用点：`api::chat::spawn_catalog_refresh`（GET /v1/models 的异步刷新）
/// 与 `ServerState::bootstrap`（启动刷新）。逐个 await（而不是并发）：
/// provider 数量是个位数、刷新是低频后台动作，串行更好排查（日志顺序稳定）。
/// 失败不会中断循环 —— 各适配器的 `refresh_models` 已把失败收敛成
/// 「保留现有清单 + 打日志」。
///
/// **传 `force = false`**：这是**自动**路径，各家照用自己的缓存/TTL（小浣熊的
/// 10 分钟 TTL 就是为它设的）。手动按钮走 [`refresh_implemented_forced`]。
pub async fn refresh_implemented(store: &AccountStore) {
    seed_current_raccoon_defaults();
    seed_current_workbuddy_defaults();
    seed_current_qoder_defaults();
    for kind in implemented_kinds() {
        let adapter = adapter_for(kind);
        // 注册表与适配器自报的 kind 必须一致（不一致说明 `adapter_for`
        // 的 match 分支接错了）；只在 debug 断言，release 不 panic（panic=abort）
        debug_assert_eq!(adapter.kind(), kind);
        // 自动路径不看结果：各适配器内部已经把「成功 / 没刷 / 失败」都打进了日志
        // （`refresh_models` 的契约就是失败不返回错误），这里再处理一遍只会重复
        adapter.refresh_models(store, false).await;
    }
}

/// **手动**刷新模型清单：只刷「支持刷新」的家，且强制绕过缓存。
///
/// 逐家结果：`[{ provider, providerLabel, status, count?, fixed?, message? }, ...]`，
/// 顺序 = 注册表顺序，**每家都有一条**（不支持的家也在里面，status = `skipped`
/// 并说明原因）—— 界面的汇总（成功 N / 失败 M / 跳过 K）与逐条明细因此能对上
/// 总数（与 `credential_maintenance::refresh_expiring_accounts` 同一取舍）。
///
/// ── 结果字段（前后端契约）──────────────────────────────────────
///   - `status`：`"refreshed" | "skipped" | "failed"`（三档语义见下）；
///   - `count`：仅在 `refreshed` 时出现，落地后的条目数；
///   - `fixed`：仅在「这家不支持刷新」时出现且为 true —— **机器可识别的标记**，
///     前端据此把「能力边界」（固定清单，永远刷不出东西）与「本次没取到新内容」
///     分开说；按 `message` 文案匹配会在措辞调整后静默失效
///     （与 `USAGE_NOT_CONFIGURED_CODE` 同一取舍）；
///   - `message`：`skipped` / `failed` 时给用户看的原因。
///
/// ── 与 `refresh_implemented` 的三处差异（都是有意的）───────────
///   1. **`force = true`**：用户按下按钮的全部预期是「现在真的去拉一次」。
///      小浣熊的 TTL 早退会让「点了没反应、清单没变」，与功能坏掉无法区分。
///      缓存只该为后台自动路径服务，用户显式要求时一律绕过。
///   2. **只刷支持的家**：静态清单的家不去打那次必然白跑的网络请求。
///   3. **返回逐家结果**：自动路径失败只写日志（没有人在等它）；手动路径必须
///      把「哪家刷到了几个、哪家为什么没刷」交给界面 —— 一句笼统的「已刷新」
///      会让「其实失败了」和「其实跳过了」都显示成成功。
///
/// ── `skipped` 与 `failed` 的区别（界面的文案完全依赖这个区分）──
///   - `skipped`：**这次没有可刷的东西，且不是错误**。两种来源：这家没有
///     远程目录（固定清单，带 `fixed: true`）、或这次刷新没有落地新内容
///     （上游返回的清单为空 / 没有可用登录态）。
///   - `failed`：**尝试了，但上游或本地出错了**（HTTP 非 2xx、网络错误、
///     解析失败）。`message` 里带原因，用户可能要排查（网络 / 凭证）。
///   把 `failed` 报成 `skipped` 会让真实故障静默；把 `skipped` 报成 `failed`
///   会让「CatPaw 用的是固定清单」这种正常事实变成一条红色错误。
///
/// 失败不抛错、逐家串行：一家的失败不影响其余家（`refresh_models` 契约本身就
/// 失败不返回错误），串行的理由与自动路径相同（provider 个位数、日志顺序稳定）。
pub async fn refresh_implemented_forced(store: &AccountStore) -> Vec<Value> {
    let mut results: Vec<Value> = Vec::new();
    // 手动刷新前对缓存清单补一次种子（刷新成功落地新清单后 raccoon / workbuddy
    // 内部还会再种一次）
    seed_current_raccoon_defaults();
    seed_current_workbuddy_defaults();
    seed_current_qoder_defaults();
    for kind in implemented_kinds() {
        let adapter = adapter_for(kind);
        debug_assert_eq!(adapter.kind(), kind);
        let provider_id = kind_id(kind);
        let provider_label = meta(kind).label;
        if !adapter.supports_model_refresh() {
            // 不支持的家**不进网络**：静态清单刷十次还是同一份，
            // 打上游只是白跑（CatPaw 的模型 id 是固定的，上游没有目录接口）。
            // `fixed: true` 是给前端的机器可识别标记（理由见上方「结果字段」）
            results.push(json!({
                "provider": provider_id,
                "providerLabel": provider_label,
                "status": "skipped",
                "fixed": true,
                "message": "该提供商使用固定模型清单（上游没有远程目录接口）",
            }));
            continue;
        }
        let outcome = adapter.refresh_models(store, true).await;
        if outcome.refreshed {
            results.push(json!({
                "provider": provider_id,
                "providerLabel": provider_label,
                "status": "refreshed",
                "count": outcome.count,
            }));
        } else if let Some(message) = outcome.message {
            results.push(json!({
                "provider": provider_id,
                "providerLabel": provider_label,
                "status": "failed",
                "message": message,
            }));
        } else {
            results.push(json!({
                "provider": provider_id,
                "providerLabel": provider_label,
                "status": "skipped",
                "message": "本次刷新没有取到新清单（上游未返回可用的模型列表）",
            }));
        }
    }
    results
}

// ─── 占位适配器（W2b-T3 起三次演化，W6 删除）─────────────────
//
// W2b-T3 这里曾有一个 `PlaceholderAdapter`（构造请求返回 501）。
// W4a 之后「未实现的已注册 provider」成了真实情形（四家里两家是占位），它被
// 参数化成 `pending::PendingAdapter` 并**真正接进** `adapter_for` 的
// CatPaw / AutoClaw 分支（状态码也从 501 改成语义更准的 503，
// 并补上 `is_stateful` 的如实声明）。
// 两家先后接上真身（W5-T-d4 / W4b-T-c2）后，占位实现不再有任何生产调用点，
// W6 收尾时连文件一起删除 —— 现在加 provider 的过渡方案是：先在
// `adapter_for` 里给一个最小实现，而不是重新引入一层占位结构。
