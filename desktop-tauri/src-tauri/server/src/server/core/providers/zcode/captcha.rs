//! 活动套餐通道的**人机验证令牌池**（阿里云无痕验证 proof，由界面铸造、网关消费）。
//!
//! ── 为什么需要它（实测结论，2026-09-28）────────────────────────
//! `POST {zcode}/api/v1/zcode-plan/anthropic/v1/messages` 这道门有**两道锁**：
//!
//!   1. 少 `X-Aliyun-Captcha-Verify-Param` → `400 {"code":3007,"msg":"captcha verify failed"}`；
//!   2. 令牌对但请求体里没有官方身份提示词块 → `405 {"code":3012,...}`（见 `plan.rs`）。
//!
//! 而活动额度（限时体验套餐）**只**从这条路花得出去：同样一个账号，拿套餐 JWT 打
//! 开放平台的 `/api/anthropic` 或 `/api/coding/paas/v4` 一律回「无可用资源包」，
//! 打 `api.z.ai` 是 401（社区实测，与我们的探测一致）。也就是说「1 亿 token」
//! 要用起来，就必须**每条请求附一个当次铸的验证码令牌**。
//!
//! 令牌是阿里云验证码 V3 的「无痕验证」产物（`certifyId` + `securityToken` 的
//! JWT 形态，约 280 字符）。参考实现（`Acankao/zcode-api`）用 happy-dom 在进程内
//! 跑阿里云 SDK 铸令牌；Rust 侧没有 JS/DOM 引擎，铸不出来 —— **但我们的桌面端
//! 有**：Tauri 的 WebView 就是浏览器，`ui/aliyun-captcha.js` 早已为「领套餐」加载
//! 同一套 SDK（同一个 scene/prefix）。于是分工是：
//!
//! ```text
//!   WebView（ui/zcode-captcha-pool.js）
//!       └─ SDK.instance.startTracelessVerification()   ← 静默、无需用户操作
//!              └─ POST /api/zcode/captcha {tokens:[…]}  ← 铸一个推一个（本模块的收口）
//!   Rust 转发层（plan.rs）
//!       └─ take() → X-Aliyun-Captcha-Verify-Param / -Region
//! ```
//!
//! ── 令牌的几个硬性质（都来自实测/参考实现，别当成优化去掉）────
//!   · **一次一用**：同一个令牌发第二次必回 3007（我们实测）；失败的请求同样
//!     消耗掉它（上游在鉴权前先验验证码），所以「重试」必须再取一个；
//!   · **有寿命**：参考实现按 ~95 秒 TTL 维护池子，我们取 [`TOKEN_TTL_MS`]；
//!     过期的直接丢，不试 —— 试了也是 3007，还白费一次往返；
//!   · **与账号无关**：令牌是「这台设备 + 这个场景」的产物，不绑账号，全池共用
//!     （参考实现同样是一个池子服务所有账号）；
//!   · **有风控上限**：阿里云侧对铸造频率/IP 有风控（参考实现为此做了限速与
//!     熔断）。因此铸造节奏由界面控制（库存低于目标才补），本模块只做收口与
//!     消费，不主动向任何地方要令牌。
//!
//! ── 没有令牌时怎么办（这是本模块的**边界**）──────────────────
//! 如实失败，给出可执行的提示：headless / Docker 部署没有 WebView，铸不出令牌，
//! 活动套餐通道在那里**不可用** —— 那种部署请用编码套餐通道（账号设置里的
//! 「使用套餐」切回编码套餐）。不要伪造一个令牌、也不要静默降级：上游会用
//! 3007 把请求挡回来，用户只会看到一句看不懂的英文。
//!
//! 「不可用」现在会被**记住**：`plan.rs` 在取不到令牌的那一次登记一道本地闸门
//! （[`readiness`]），选路随后暂时绕开这一家、请求落到别的承载家；令牌一入池就
//! 由 [`push`] 撤掉闸门（[`ungate`]）。绕不开的是判据本身 —— 这条通道需要的是
//! 一个**本机铸不出**的东西，网关没有义务替它编一个。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic；锁中毒退化成空池。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::core::providers::readiness;
use crate::server::logging;

/// 请求头：阿里云验证码 proof（参考实现 `RETRY_HEADERS.PARAM` 逐字相同）
pub(super) const VERIFY_PARAM_HEADER: &str = "x-aliyun-captcha-verify-param";

/// 请求头：这个 proof 属于哪个阿里云站点（`cn` / `sgp` 等）
pub(super) const VERIFY_REGION_HEADER: &str = "x-aliyun-captcha-verify-region";

/// 令牌寿命（毫秒）。参考实现按 ~95 秒维护，这里留一点余量给「铸好到发出」的
/// 窗口 —— 超过它的一律丢弃（过期令牌上游一定拒，试它只是多一次 3007）。
pub const TOKEN_TTL_MS: i64 = 120_000;

/// 池子容量上限：超过就丢**最旧**的。
///
/// 界面按「库存低于目标才补」的节奏铸造，正常不会逼近这个上限；它是防御性的
/// （界面被改坏 / 重复推送时不至于把内存撑爆）。
const POOL_MAX: usize = 500;

/// 一条令牌
struct Entry {
    param: String,
    region: String,
    /// 铸出时刻（毫秒时间戳，本地时钟）
    at: i64,
}

/// 池子 + 计数（计数只为界面/排障展示，不参与判定）
#[derive(Default)]
struct Pool {
    entries: VecDeque<Entry>,
    /// 累计入库 / 消费 / 上游拒收（3007）/ 过期丢弃
    minted: u64,
    consumed: u64,
    rejected: u64,
    stale: u64,
    /// 最近一次上游回 3007 的时刻（0 = 没有过）—— 界面据此提高补货优先级
    last_challenge_at: i64,
    /// 最近一次入库时刻
    last_mint_at: i64,
}

fn pool() -> &'static Mutex<Pool> {
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Pool::default()))
}

/// 入池（界面铸好一个就推一个）。返回入库后的库存数。
///
/// 空串一律拒绝：`param` 是必填的 JWT 形态串，空值入池只会让下一次请求带着一个
/// 空头出去（上游当缺失处理，回 3007，白费一次往返）。
pub fn push(param: &str, region: &str) -> usize {
    let param = param.trim();
    if param.is_empty() {
        return ready();
    }
    let now = crate::server::logging::now_ms();
    // 池锁只在这个块里拿着：块末就放。下面撤闸门要走**另一把**锁（readiness 的
    // 注册表），两把锁嵌套就会有一条未来的死锁路径（谁先谁后全靠这里的写法）。
    let stock = {
        let Ok(mut pool) = pool().lock() else {
            return 0;
        };
        pool.entries.push_back(Entry {
            param: param.to_string(),
            region: region.trim().to_string(),
            at: now,
        });
        while pool.entries.len() > POOL_MAX {
            pool.entries.pop_front();
        }
        pool.minted = pool.minted.saturating_add(1);
        pool.last_mint_at = now;
        pool.entries.len()
    };
    ungate();
    stock
}

/// 令牌进池 → 撤掉 ZCode 两地的**本地闸门**。
///
/// 为什么在这儿撤而不是让闸门自己到期：闸门的存在意义是「headless 上这条通道
/// 恒不可用，别把请求送进去」，而一枚令牌入库就是那个前提**当场被推翻**的证据。
/// 等到期（[`readiness::GATE_TTL_MS`]）最坏会让这一家白躲两分钟 —— 对正在跑
/// WebView 铸造的桌面端来说，这就是「明明有令牌了还绕着走」。
///
/// 两地一起撤：池子是全局一份、不绑地区（见模块头的「与账号无关」），国内版铸
/// 出来的令牌国际版同样能用，所以国际版的闸门也没有理由继续立着。
fn ungate() {
    for region in super::region::Region::ALL {
        readiness::release(region.provider_id());
    }
}

/// 取一个令牌（FIFO：先铸先用，避免新令牌被旧令牌挤到过期）。
///
/// 顺手把过期条目丢掉并计数 —— 池子里躺着几十条过期令牌时，库存数会骗人
/// （界面看「还有 8 个」却连续 3007）。
pub fn take() -> Option<(String, String)> {
    let now = crate::server::logging::now_ms();
    let mut pool = pool().lock().ok()?;
    while let Some(entry) = pool.entries.pop_front() {
        if now.saturating_sub(entry.at) > TOKEN_TTL_MS {
            pool.stale = pool.stale.saturating_add(1);
            continue;
        }
        pool.consumed = pool.consumed.saturating_add(1);
        return Some((entry.param, entry.region));
    }
    None
}

/// 上游回了一次 3007（令牌被拒/过期/没用上）。
///
/// 由 `plan.rs` 的错误分类点调用：它同时是「池子里的令牌不可信了」的信号 ——
/// 界面看到这个计数就会立刻补货。
pub fn note_challenge() {
    let now = crate::server::logging::now_ms();
    if let Ok(mut pool) = pool().lock() {
        pool.rejected = pool.rejected.saturating_add(1);
        pool.last_challenge_at = now;
    }
}

/// 记一次「上游确实回了 3007」，并把「推理免码」的结论**翻回要码**。
///
/// 为什么 3007 必须能翻这个结论：它是这条链上**唯一**能证伪 `skip_model_request`
/// 的观测。免码上线后如果上游哪天把这一格改回 `false`（或直接不认这个开关），
/// 我们手上不会有任何配置侧的信号 —— 只有 3007 会说话。让它当场改回要码，
/// 免码路线的代价就被钉在「**最多一次** 3007 往返」上，而不是每次都撞。
pub fn note_challenge_and_require_token() {
    note_challenge();
    set_required(true);
}

// ── 「此刻到底要不要一枚令牌」这个结论长在哪 ───────────────────
//
// 判据来自上游自己的配置（`configs.captcha.skip_model_request`，见
// [`super::claim::CaptchaConfig`]），由 [`super::config_sync`] 每 5 分钟同步一次；
// 3007 会当场把它翻回「要码」（[`note_challenge_and_require_token`]）。
//
// 默认值 = **要码**。这不是保守主义：headless 部署上「要码」的后果只是这个模型名
// 由别家承载（见 `core::providers::readiness` 那条顺延路），而「免码」判错的后果是
// 每条请求都白撞一次 3007、逐次消耗账号行为分。两边代价不对称。

/// 结论本体（`true` = 每条模型请求都要一枚令牌）
static TOKEN_REQUIRED: AtomicBool = AtomicBool::new(true);
/// 上一次同步到配置的时刻（0 = 启动至今没同步成功过；面板与排障读它）
static LAST_SYNC_AT: AtomicI64 = AtomicI64::new(0);

/// `ZCODE_REQUIRE_CAPTCHA`：把它设成 `1` / `true` / `on` 就**无条件要码**，
/// 上游那格配置不再有影响。
///
/// 留这个口子有两个用途：上游哪天「配置说免码、实际照拒」时，部署方不必等我们
/// 改代码；以及对比排障时要能一行环境变量把两条路径切来切去。读一次就定死
/// （`OnceLock`）—— 进程活到一半改环境变量不会生效，那本来就不是它的用法。
fn env_forces_token() -> bool {
    static FORCED: OnceLock<bool> = OnceLock::new();
    *FORCED.get_or_init(|| {
        let raw = std::env::var("ZCODE_REQUIRE_CAPTCHA")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        matches!(raw.as_str(), "1" | "true" | "yes" | "on")
    })
}

/// 现在发这条模型请求**要不要**取一枚令牌。
pub fn required() -> bool {
    env_forces_token() || TOKEN_REQUIRED.load(Ordering::SeqCst)
}

/// 写入结论（配置同步与 3007 两处）。返回 `true` = 这次**改变了**结论。
///
/// 改变时打一行终端日志，并在改回「免码」时顺手撤掉本地闸门 —— 那道闸门存在的
/// 前提就是「本机铸不出令牌而这条通道需要令牌」，前提没了还挡着，就把一条已经
/// 能用的通道永久关在候选池外了。
pub fn set_required(required: bool) -> bool {
    if env_forces_token() && !required {
        // 环境变量说「照旧要码」：上游那格配置不再能翻这个结论，否则 `ZCODE_REQUIRE_CAPTCHA`
        // 会被五分钟一次的同步悄悄冲掉 —— 部署方按下的开关必须是最终结论
        return false;
    }
    let changed = TOKEN_REQUIRED.swap(required, Ordering::SeqCst) != required;
    if !changed {
        return false;
    }
    if required {
        logging::console_line(
            "[ZCode]",
            "🔒 活动套餐通道改回「每条请求都要一枚验证码令牌」（上游配置翻回、或它刚拒了一次免码请求）",
        );
    } else {
        logging::console_line(
            "[ZCode]",
            "🔓 上游配置说模型请求不必再附验证码令牌（skip_model_request）：\
             活动套餐通道改为免码直发，桌面端停止铸造令牌",
        );
        ungate();
    }
    true
}

/// 记下一次**成功**同步配置的时刻。
pub fn note_synced() {
    LAST_SYNC_AT.store(crate::server::logging::now_ms(), Ordering::SeqCst);
}

/// 上次同步配置的时刻（0 = 本次启动至今没同步成功过）
pub fn last_sync_at() -> i64 {
    LAST_SYNC_AT.load(Ordering::SeqCst)
}

/// 当前可用库存（不含过期条目）
pub fn ready() -> usize {
    let now = crate::server::logging::now_ms();
    let Ok(mut pool) = pool().lock() else {
        return 0;
    };
    while pool
        .entries
        .front()
        .is_some_and(|entry| now.saturating_sub(entry.at) > TOKEN_TTL_MS)
    {
        pool.entries.pop_front();
        pool.stale = pool.stale.saturating_add(1);
    }
    pool.entries.len()
}

/// 池子概况（管理接口 `GET /api/zcode/captcha` 的响应体）
pub fn stats() -> Value {
    let ready = ready();
    let now = crate::server::logging::now_ms();
    let Ok(pool) = pool().lock() else {
        return json!({ "ready": ready, "ttlMs": TOKEN_TTL_MS });
    };
    let oldest_age_ms = pool
        .entries
        .front()
        .map(|entry| now.saturating_sub(entry.at))
        .unwrap_or(0);
    json!({
        "ready": ready,
        // 「上游此刻要不要每条模型请求附一枚令牌」——界面据此决定要不要铸造
        // （false 时白铸就是在烧阿里云风控配额，见 `required` 的说明）
        "required": required(),
        "lastSyncAt": last_sync_at(),
        "ttlMs": TOKEN_TTL_MS,
        "oldestAgeMs": oldest_age_ms,
        "minted": pool.minted,
        "consumed": pool.consumed,
        "rejected": pool.rejected,
        "stale": pool.stale,
        "lastChallengeAt": pool.last_challenge_at,
        "lastMintAt": pool.last_mint_at,
    })
}
