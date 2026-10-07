//! **本地就绪闸门**：一条通道在「还没出本机就注定发不出去」时，让选路绕开它。
//!
//! ── 要解决的问题 ──────────────────────────────────────────
//! ZCode 的活动套餐通道（`start-plan`）需要**每条请求**附一枚由桌面端 WebView
//! 静默铸造的人机验证令牌（见 [`super::zcode::captcha`]）。headless / Docker
//! 部署没有 WebView，那座令牌池**恒空** —— 于是每一次构造请求都在本地失败。
//!
//! 改造前这条失败会**终止整份请求**：`provider_loop` 的构造期失败分支直接
//! `return Err`，换号顺延（动作 1 / 动作 3）根本没有机会跑。生产上留下的形状是
//! `attempts=1`、尝试明细里只有这一家、状态 503 —— 而同一份请求名在 catpaw /
//! codearts / autoclaw 上都是能答的。用户看到的不是「这家暂时不可用」，是
//! 「网关报错了」。
//!
//! ── 为什么这是 provider 级事实，不是账号级、更不是请求级 ──────
//!   · **与账号无关**：令牌池是全局一份、不绑账号（`captcha.rs` 模块头的第三条
//!     硬性质），所以同一家另一个账号不会更好；
//!   · **与请求无关**：判定发生在消息内容、模型名、协议之前，同一份请求换个
//!     账号发出去的还是同一个缺令牌的请求；
//!   · **稳定**：只取决于这台机器上有没有 WebView 在铸造，而那是部署形态，
//!     不是抖动。
//! 三条合起来意味着：**可以预判**。预判到了就不该把请求送进去撞墙，而该直接
//! 交给下一个能承载的家。
//!
//! ── 三件事各自落在哪一层 ──────────────────────────────────
//!   1. **判别**（适配器侧）：构造期失败给出专用状态码
//!      [`LOCAL_PRECONDITION_STATUS`]，与「上游回了 503」区分开。没有这个码，
//!      编排层只能靠**错误文案**判断该不该顺延，而文案是给人读的，改一个标点
//!      就会把顺延打成死路；
//!   2. **记账**（本模块）：适配器在失败当场 [`hold`] 一道 provider 级闸门，
//!      寿命 [`GATE_TTL_MS`]；令牌入库时 [`release`] 提前撤掉；
//!   3. **绕行**（选路与编排两侧）：
//!      · `upstream::rotate::accounts_in_providers` 把闸门挡住的账号摘出候选池
//!        —— 于是后续请求**根本不会被送进这一家**，一次本地失败都不再产生；
//!      · `upstream::provider_loop` 的构造期失败分支见 556 就顺延（与动作 3
//!        同一条路，受「切换账号重试次数」约束）—— 这是**第一枪**：闸门还没
//!        建立时，那份请求仍会送到这一家，此时必须顺延而不是终止。
//!
//! ── 为什么状态码是 556 ────────────────────────────────────
//! RFC 9110 §7.1 明确 5xx 允许服务端使用未列举的码，客户端按通用服务端错误
//! 处理，因此新增一个码不违反任何契约。选 556 而不是继续用 503 的**唯一**理由是
//! 取证：请求表里 `status=503` 现在至少有三层含义（上游真回了 503、这一家没有
//! 账号、以及这里的「本地发不出去」），排查时要把「从未发出去的请求」从前两类里
//! 择出来，只能逐条读文案。有了专属码，一句 SQL 就能分组，正对照也才做得动
//! （见 `deploy/nas` 的实测记录）。
//!
//! **它照样会到达客户端**：全部候选家都被挡住（或只有这一家）时，错误原样返回，
//! 文案不变 —— 那句话是唯一可执行的出路（把「使用套餐」切回编码套餐），不该为了
//! 状态码好看而吞掉。
//!
//! ── 闸门为什么会自己过期，而不是等令牌来了才解 ────────────
//! [`release`] 只在令牌入库时被调用。如果闸门只能靠入库解除，那么「铸造链换了
//! 别的出口」「界面推送走的是另一条路由」这类情况会让这一家被永久误挡 —— 而
//! 误挡的代价是**这一家再也不接请求**，比原来的问题更糟。所以闸门带寿命：到期
//! 自动放行，让下一次构造重新探一次本地条件。代价是有界且极小：headless 上每
//! [`GATE_TTL_MS`] 多一次本地失败（**没有任何上游往返**，`take()` 之前就返回），
//! 换来的是不需要任何配置项、也不需要谁去手工复位。
//!
//! [`hold`] 在闸门已成立时**不续期**：续期的话，繁忙的网关会每隔几秒把时间戳
//! 顶新一次，闸门永远到不了期 —— 那等于把上面的自愈能力抹掉。
//!
//! ── 锁中毒怎么办 ──────────────────────────────────────────
//! 退化成「无闸门」（放行）。闸门是优化，不是安全边界：误挡一家会让请求落到
//! 别家、或让这一家彻底没流量，而误放只会多一次注定失败的本地构造 —— 代价
//! 相差一个量级。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::logging;

/// 「这一家在**本机**就发不出去」的专用状态码（见模块头第 1 件事）。
///
/// 编排层按它决定「顺延而不是终止」，适配器用它标记「从未发生的请求」。
/// 两处都只认这个数字，不认文案。
pub const LOCAL_PRECONDITION_STATUS: u16 = 556;

/// 闸门寿命（毫秒）。
///
/// 与令牌 TTL 同量级（[`super::zcode::captcha::TOKEN_TTL_MS`]）：比它短没有意义
/// （令牌本身还没过期，闸门先放了行 = 白挡），比它长几倍则「本地条件已恢复」要
/// 等太久才被重新探到。
pub const GATE_TTL_MS: i64 = 120_000;

/// 一道闸门的登记项。
#[derive(Clone, Debug)]
struct Gate {
    /// 给人读的那句话（面板与终端日志用；**不参与任何判定**）
    reason: String,
    /// 闸门**成立**的时刻（首次 `hold`）。已成立时再 hold 不刷新，理由见模块头。
    held_at: i64,
}

#[derive(Default)]
struct Registry {
    gates: HashMap<String, Gate>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// 登记一道闸门：这一家此刻在本机发不出去，后续请求请绕开。
///
/// 由适配器在**构造期失败当场**调用（判别点就是失败点，两处不可能对不上文案）。
/// 时钟取当前时刻；要注入时钟的用 [`hold_at`]。
///
/// 返回 `true` = 本次调用让闸门**从放行变成挡住**（调用方可以据此打一行日志，
/// 逐请求打会刷屏；`false` = 本来就挡着）。
pub fn hold(provider_id: &str, reason: &str) -> bool {
    hold_at(provider_id, reason, logging::now_ms())
}

/// [`hold`] 的可注入时钟版本。
///
/// 写侧也要能注入时钟 —— 否则「不续期」那条判据根本测不出来：真实时钟下
/// 两次 `hold` 只差几微秒，无论有没有续期，「TTL 之后仍然放行」的断言都同样绿
/// （实测过：把续期改回去，用例照样全绿）。用例要的是**能看出差别**的时钟。
pub fn hold_at(provider_id: &str, reason: &str, now: i64) -> bool {
    let Some(provider_id) = non_empty(provider_id) else {
        return false;
    };
    let Ok(mut table) = registry().lock() else {
        return false;
    };
    match table.gates.get_mut(&provider_id) {
        Some(existing) => {
            // **不刷新 `held_at`**：已成立的闸门到期时刻由首次成立决定
            existing.reason = reason.to_string();
            false
        }
        None => {
            table.gates.insert(
                provider_id,
                Gate {
                    reason: reason.to_string(),
                    held_at: now,
                },
            );
            true
        }
    }
}

/// 撤掉闸门（令牌入库、或本地条件确认恢复时调用）。
///
/// 撤掉时顺手把这一家**当前还在闸门外**的事实打进终端日志：铸造链是异步的，
/// 界面推一个令牌进来，值得有一行「什么时候重新开门」的落点，否则排查「明明
/// 有令牌了为什么还绕着走」只能等到期。
///
/// 返回 `true` = 确实撤掉了一道闸门。
pub fn release(provider_id: &str) -> bool {
    let Some(provider_id) = non_empty(provider_id) else {
        return false;
    };
    let Ok(mut table) = registry().lock() else {
        return false;
    };
    let Some(gate) = table.gates.remove(&provider_id) else {
        return false;
    };
    logging::console_line(
        "[Routing]",
        &format!(
            "✅ {provider_id} 的本地闸门已撤（挡了 {}s，原因：{}），该家重新进入候选池",
            ((logging::now_ms() - gate.held_at) / 1000).max(0),
            gate.reason,
        ),
    );
    true
}

/// 这一家当前是否被闸门挡住。
///
/// 判定顺带**清掉已过期的登记项**：过期即放行，不留一堆到期的僵尸项让
/// [`snapshot`] 报出「挡着」的家。
pub fn gated(provider_id: &str) -> bool {
    gated_at(provider_id, logging::now_ms())
}

/// [`gated`] 的可注入时钟版本（测试按它验证 TTL，而不是睡两分钟）。
fn gated_at(provider_id: &str, now: i64) -> bool {
    let Some(provider_id) = non_empty(provider_id) else {
        return false;
    };
    let Ok(mut table) = registry().lock() else {
        return false;
    };
    match table.gates.get(&provider_id) {
        Some(gate) if now - gate.held_at < GATE_TTL_MS => true,
        Some(_) => {
            table.gates.remove(&provider_id);
            false
        }
        None => false,
    }
}

/// 当前挡着的闸门（面板与排障用；已过期的不在内）。
///
/// 每项形如 `{provider, reason, heldForMs}`。**只读**，不参与判定，因此锁中毒
/// 时给空表（读不出状态比读出「没有闸门」更糟）。
pub fn snapshot() -> Vec<Value> {
    let now = logging::now_ms();
    let Ok(table) = registry().lock() else {
        return Vec::new();
    };
    let mut items: Vec<Value> = table
        .gates
        .iter()
        .filter(|(_, gate)| now - gate.held_at < GATE_TTL_MS)
        .map(|(provider, gate)| {
            json!({
                "provider": provider,
                "reason": gate.reason,
                "heldForMs": now - gate.held_at,
            })
        })
        .collect();
    // HashMap 的迭代顺序不稳定；报表与断言都要一个确定的次序
    items.sort_by(|a, b| {
        a.get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                b.get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    items
}

/// 测试用：清空整张表。
///
/// 闸门是**进程级**状态，同一份 cargo test 二进制里所有用例共享它。没有这个
/// 复位点，用例之间会互相污染（前一条 hold 的家会让后一条的候选池少一项），
/// 那种红法看起来像被测代码坏了。
///
/// 复位**不等于**隔离：并发线程仍然能插进来，所以每个用例都要先拿
/// [`lock_for_tests`]。
#[cfg(test)]
pub fn clear_for_tests() {
    let Ok(mut table) = registry().lock() else {
        return;
    };
    table.gates.clear();
}

/// 测试用：串行化所有碰这张全局表的用例（含别的模块里那些）。
///
/// 与面板探针那边的 `PROBE_SLOT_LOCK` 同一手法 —— 全局状态 + 多线程测试执行器
/// 只能这样收：闸门表按 provider 记，而用例之间共用同一个 provider id（`"zcode"`
/// 是被测对象的真名，换一个假名就测不到 `plan.rs` 与选路两侧的同一条判据）。
#[cfg(test)]
pub fn lock_for_tests() -> std::sync::MutexGuard<'static, ()> {
    static SLOT: Mutex<()> = Mutex::new(());
    // 中毒继续用同一把锁：某个用例 panic 过已经被执行器记成失败，这里要做的
    // 只是让后面的用例照样跑完，而不是把「锁坏了」再报一次
    SLOT.lock().unwrap_or_else(|error| error.into_inner())
}

/// provider id 的最小校验：空串一律当作「不认识这家」。
///
/// 为什么值得单独一处：调用方给的是 `region.provider_id()` 与
/// `rotate::provider_of(&account)`，两者都可能是空串（账号记录缺 `provider` 键
/// 时 `provider_of` 回落到 `""`）。空串一旦进表，`gated("")` 就会把**所有**缺
/// provider 的账号一起挡掉 —— 那是候选池整体塌方，而不是一家的绕行。
fn non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod local_gates {
    //! 本地就绪闸门：成立 / 绕行 / 过期 / 撤销四条判据。
    //!
    //! 时钟一律走 [`gated_at`] 的注入版本 —— TTL 是 120 秒，用睡眠验证的写法
    //! 要么睡两分钟，要么睡一个远小于 TTL 的数然后**什么都没验证**。
    //!
    //! 每个用例先拿 [`lock_for_tests`]：这张表是进程级的，并发的用例互相能把
    //! 对方的前提改掉（那种红法与绿法都不可信）。

    use super::*;

    #[test]
    fn a_held_gate_gates_only_that_provider() {
        let _slot = lock_for_tests();
        clear_for_tests();
        let now = logging::now_ms();
        assert!(hold("zcode", "活动套餐令牌池为空"));
        assert!(gated_at("zcode", now), "hold 之后当场就该绕开");
        assert!(!gated_at("catpaw", now), "另一家没道理被牵连");
        // 同一家的国际版是**另一个 provider id**：闸门按 id 记，不认「同一款产品」
        assert!(!gated_at("zcode-intl", now));
    }

    #[test]
    fn an_empty_provider_id_gates_nothing() {
        let _slot = lock_for_tests();
        clear_for_tests();
        // 缺 `provider` 键的账号，`provider_of` 回落到空串；空串进表会把
        // 所有缺键的账号一起挡掉（见 `non_empty` 的说明）
        assert!(!hold("", "不该登记"), "空 provider 不登记");
        assert!(!gated(""), "空 provider 不绕行");
        assert!(snapshot().is_empty(), "表里不该多出东西");
    }

    #[test]
    fn a_gate_expires_rather_than_waiting_for_a_release() {
        let _slot = lock_for_tests();
        clear_for_tests();
        assert!(hold("zcode", "令牌池为空"));
        // 全程用注入时钟做 TTL 判定：真实时钟下这条只能靠睡眠验证，而睡眠要么
        // 睡满两分钟、要么睡不到 TTL（后者等于什么都没测）。
        let base = logging::now_ms();
        assert!(
            gated_at("zcode", base + GATE_TTL_MS - 1),
            "寿命内仍然挡着 —— 比令牌 TTL 还短就放行是白挡"
        );
        assert!(
            !gated_at("zcode", base + GATE_TTL_MS),
            "到点自动放行：下一次构造重新探一次本地条件"
        );
        assert!(
            snapshot().is_empty(),
            "过期的登记项跟着清掉，别报出一个不存在的闸门"
        );
        // 过期后再登记 = 一道**新**闸门（从它自己成立的那一刻起算），
        // 而不是老闸门的延续
        assert!(
            hold_at("zcode", "令牌池仍然为空", base + GATE_TTL_MS + 1),
            "过期后应当能重新成立"
        );
        assert!(
            gated_at("zcode", base + GATE_TTL_MS + 1),
            "新闸门从它自己成立的那一刻起算"
        );
        // 到期时刻 = 新登记时刻 + TTL（老闸门的 `base` 已经与此无关）
        assert!(
            !gated_at("zcode", base + 2 * GATE_TTL_MS + 1),
            "新闸门同样只活一个 TTL"
        );
    }

    #[test]
    fn reholding_a_live_gate_does_not_extend_it() {
        let _slot = lock_for_tests();
        clear_for_tests();
        let base = logging::now_ms();
        assert!(hold_at("zcode", "令牌池为空", base));
        // 繁忙的网关每隔几秒就会再登记一次。若登记会续期，闸门永远到不了期，
        // 上面那条自愈能力就等于没有 —— 这条用例钉住「不续期」。
        //
        // 第二颗时钟**必须**离第一颗足够远：只差几微秒时，续期与不续期在下面那条
        // 断言上看不出差别（把实现改回续期看过，用例照样全绿 —— 那是空跑）。
        assert!(
            !hold_at("zcode", "令牌池还是为空", base + GATE_TTL_MS / 2),
            "第二次登记不改变开合状态"
        );
        assert!(
            gated_at("zcode", base + GATE_TTL_MS - 1),
            "第二次登记之后仍在寿命内"
        );
        assert!(
            !gated_at("zcode", base + GATE_TTL_MS),
            "到期时刻由首次登记决定 —— 续期的话这里还挡着，那是「永远挡着」的开始"
        );
    }

    #[test]
    fn releasing_opens_the_gate_immediately() {
        let _slot = lock_for_tests();
        clear_for_tests();
        assert!(hold("zcode", "令牌池为空"));
        assert!(release("zcode"), "撤掉一道真实存在的闸门返回 true");
        assert!(!gated("zcode"), "有令牌了就不该再绕");
        assert!(!release("zcode"), "撤一道不存在的闸门返回 false");
        assert!(snapshot().is_empty());
    }
}
