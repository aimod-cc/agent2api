//! Cline 的规则部分：默认映射种子 + 拆分迁移 + 池前缀工具。
//!
//! ── 为什么单独一个文件 ──────────────────────────────────────
//! 这几件事都只服务 **Cline / 它的两个额度池**，与 workbuddy / 小浣熊那几套
//! 种子（留在 `mod.rs`）没有共享逻辑；而且其中「拆分迁移」是一次性的历史包袱，
//! 与长期存在的种子机制混在一个文件里，读的人分不清哪一段可以随版本删掉。
//! 拆出来的另一个好处是 `mod.rs` 回到「规则机制本身」的体量（见项目约定：
//! 单文件尽量不超过 800 行）。
//!
//! ── 与 `mod.rs` 的关系 ──────────────────────────────────────
//! 这里用到的都是 `mod.rs` 提供的机制（`ModelRules` 的读写、`Mapping` /
//! `RuleEntry` 的形状、`save` / `alias_valid` 等），本身不定义新类型。
//! 因此它**不是**独立的规则层，只是那层的 Cline 专用件。

use super::*;

use crate::server::core::providers::cline::models::{friendly_alias, Pool};

/// Cline 清单的**默认映射种子**（按池调用，两家各跑一遍）。
///
/// ── 为什么要映射（而不是在转发时剥前缀）─────────────────────
/// Cline 的上游模型 id 形如 `cline-free/deepseek-v4.1-flash`（免费池）与
/// `cline-pass/glm-5.3`（订阅池），**前缀是它的计费通道选择器**，剥掉会 404。
/// 直接用完整 id 当对外模型名虽然能用，但客户端里写 `cline-free/deepseek-v4.1-flash`
/// 很难看、也容易与别家的 `deepseek-v4.1-flash` 混淆。因此给每个带前缀的模型
/// 自动配一条 `deepseek-v4.1-flash → cline-free/deepseek-v4.1-flash` 的别名，
/// 两种名字都能用。
///
/// ── 免费池的裸 id 也建映射（本次修复）───────────────────────
/// 免费组里有两条不带通道前缀的条目（`z-ai/glm-5.3-flash`、
/// `poolside/laguna-s-2.1:free`），它们同样该有能直接敲的短名字。剥什么由
/// [`friendly_alias`] 按池给出：免费池的裸 id 剥**厂商前缀**（那是承载方，
/// 与通道前缀同层含义），`recommended` 组的 `openai/gpt-6-astra` 则不剥
/// （那里厂商前缀是模型身份）。撞上别家原生 id（`glm-5.3-flash` 也是 CatPaw
/// 的模型名）**不回避**：同一对外名由多家承载正是主备路由的常态。
///
/// ── 三条纪律（与小浣熊种子同构）─────────────────────────────
///   1. **幂等**：处理过的 `(provider, id)` 记入 `seeded`，用户事后删掉这条自动
///      映射不会被下一次刷新改回来；
///   2. **不抢别名**：alias 已被别的映射占用 → 跳过；
///   3. **不与上游 id 撞名**：alias 与**本池**清单里任何上游 id 同名 → 跳过。
///      **本家会撞**：`deepseek-v4.1-flash` 两池都有，而**两池现在是两家
///      provider** —— `has_alias` 是「任何人占用都不动」，所以先跑种子的那家
///      拿到短名，另一家只记 seeded、不建映射。没拿到的那家由
///      `EXTRA_ALIASES` 点名补上（那条走三判重，允许跨 provider 同 alias）。
///
/// ── `provider` 参数（拆分后新增）────────────────────────────
/// 传 `"cline-free"` / `"cline-pass"`（= `cline::models::Pool::provider_id()`）。
/// **调用方只需传本池的 id 列表**（`cline::models::ids_of(pool)`）—— 混进另一个
/// 池的 id 会让这家的 seeded 里出现不属于它的键，下一轮那家自己跑时才会补上，
/// 白绕一圈。
///
/// ── 为什么「seeded 有变化但没建映射」也要落盘 ────────────────
/// 与 `seed_default_enabled` 同一个理由：不落盘的话下次启动会重种一遍，
/// 而重种可能再次尝试建映射 —— 用户刚删掉的别名会被悄悄加回来。
/// 种子是「只对首次出现生效」的承诺，那承诺必须落盘才算数。
///
/// 返回给日志的摘要；没有任何新动作时返回 None（不落盘）。
pub fn seed_cline_defaults(provider: &str, ids: &[String]) -> Option<String> {
    let mut rules = current();
    let seeded_before = rules.seeded.len();
    let mut mappings_added: Vec<String> = Vec::new();
    // 本家是哪个池：友好名的判据要用它（免费池的裸 id 要剥厂商前缀，见
    // `models::friendly_alias`）。查不出池（不该发生：调用方传的就是两个池的
    // provider id）时按订阅池处理 —— 那是最保守的一支（不剥任何前缀）。
    let pool = crate::server::core::providers::cline::models::Pool::from_provider_id(provider)
        .unwrap_or(Pool::Pass);
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        // 额外对外名先处理：**不受下面主 seeded 短路影响**（理由见 extra_alias_key）。
        // 这里补的正是「两池同名、去前缀先到先得最多只给一个池」的另一半 ——
        // `EXTRA_ALIASES` 点名的两条总是会拿到别名，与哪家先跑无关。
        // 返回的「动过 seeded」不需要单独接：下面的 len 判断把主种子与
        // 额外别名键的 seeded 变化一起覆盖了。
        seed_extra_aliases(&mut rules, provider, id, ids, &mut mappings_added);
        if rules.is_seeded(provider, id) {
            continue;
        }
        rules.seeded.push(format!("{provider}:{id}"));
        // 剥出友好名：带通道前缀的剥通道前缀，免费池的裸 id 剥厂商前缀
        // （`z-ai/glm-5.3-flash` → `glm-5.3-flash`）。上游本来就给的友好名
        // 与「不该剥」的条目（`openai/gpt-6-astra` 那类）返回 None，不建映射。
        let Some(alias) = friendly_alias(pool, id) else {
            continue;
        };
        if alias.is_empty() || !alias_valid(alias) {
            continue;
        }
        // 纪律 2 + 3：别名被占用或与任何上游 id 同名 → 只记 seeded。
        // `has_alias` 是「任何人占用都不动」—— 于是两池同名时先跑的池拿到别名；
        // 没拿到的池由 `EXTRA_ALIASES` 点名补上（不受此限制）。
        //
        // **纪律 2 只挡映射占用，不挡别家的原生 id**：`glm-5.3-flash` 同时是
        // CatPaw / AutoClaw 的原生模型名，那不妨碍这里给它建别名 ——
        // 同一对外名由多家承载正是本项目主备路由的常态（见 `model_rules` 模块头），
        // 候选链会把两家一起列上，谁先谁后按账号优先级走。
        if rules.has_alias(alias)
            || ids.iter().any(|other| other.eq_ignore_ascii_case(alias))
        {
            continue;
        }
        rules.mappings.retain(|m| {
            !(m.alias.eq_ignore_ascii_case(alias) && m.provider.as_deref() == Some(provider))
        });
        rules.mappings.push(Mapping {
            alias: alias.to_string(),
            target: id.to_string(),
            provider: Some(provider.to_string()),
            // 种子建的映射不绑思考等级（那是用户手动绑定的东西，见 mod.rs 模块头）
            reasoning: None,
        });
        mappings_added.push(format!("{alias} → {id}"));
    }
    if rules.seeded.len() == seeded_before {
        // 没有新模型：一个字节都不用落盘（与 `seed_default_enabled` 同一判据）。
        // 这一步是必需的：本函数在**每次目录刷新**后都会被调用，无条件 save 会让
        // 「用户什么都没变」的刷新也重写一次 config.json。
        return None;
    }
    save(&rules);
    if mappings_added.is_empty() {
        // 有新的种子标记、但都没建映射（无前缀 / 别名冲突）—— 落盘即可，日志不必吵
        return None;
    }
    Some(format!(
        "🧩 Cline 模型默认映射: [{}]",
        mappings_added.join(", ")
    ))
}

// ─── Cline 拆分为两家 provider 的存量键迁移 ──────────────────

/// 拆分前的 Cline provider id（**只出现在迁移代码里**，别处已无这个 id）
const LEGACY_CLINE_ID: &str = "cline";

/// 把存量配置里属于**旧 Cline 这一家**的规则键改写到拆分后的两家。
///
/// ── 为什么必须迁移（这是拆分里最容易出事的一步）──────────────
/// 规则键是 **(provider, 模型 id)**，provider id 一改，旧条目就再也匹配不上。
/// 不迁移的后果不是「少一条设置」，而是**静默失效**：
///   - `disabled` 里带 `provider: "cline"` 的条目 → 用户禁用的模型**全部
///     变回启用**，而且他再也找不到当初关它的那个开关（那一条已经匹配不上了）；
///   - `seeded` 里的 `cline:<id>` → 两家都认为「这批模型从没种过」，于是
///     重跑一遍种子。用户主动删掉的自动映射会被**悄悄加回来** —— 这正是
///     `seeded` 存在要防的事，键名一变就全废了。
///
/// ── 池怎么判定（逐条与本文件其余部分的判据同源）──────────────
/// 一律用 `cline::models::pool_of(id)` 按**模型 id 的前缀**判：
///   - 启停条目里的 id 是上游模型 id（管理页传的就是清单里的 id），直接判；
///   - 映射条目里的目标名同上；provider 缺失的旧版全局条目**只在 target 带池
///     前缀时才补 provider** —— `cline-free/…` / `cline-pass/…` 这两个 id 是
///     Cline 独有的，补上 provider 是纯收窄；而无前缀的 id（如 `glm-5.3-flash`）
///     可能同时被别家承载，把它收窄到某一家会是**行为改变**，不动它；
///   - `seeded` 键里的 id 可以带 `#alias:<别名>` 后缀（额外别名种子），
///     取 `#` 之前那段判池。
///
/// 幂等：迁移后再跑一遍不会命中任何一条（`cline:` 与 `cline-free:` /
/// `cline-pass:` 在等值比较下不同 —— `cline-free:` 的第 6 个字符是 `-`，
/// 不会 `starts_with("cline:")`）。因此可以安全地在每次启动时调用。
///
/// 没有任何改动时返回 None，**不落盘**（与各家的种子同一纪律：启动路径上
/// 只读不写的成本必须为零）。
pub fn migrate_cline_split() -> Option<String> {
    let mut rules = current();
    let mut touched = false;
    let mut summary: Vec<String> = Vec::new();

    // ① 启停列表：provider 改名
    for (label, list) in [("禁用", &mut rules.disabled), ("隐藏", &mut rules.hidden)] {
        let mut count = 0usize;
        for entry in list.iter_mut() {
            if entry.provider.as_deref() != Some(LEGACY_CLINE_ID) {
                continue;
            }
            let pool = crate::server::core::providers::cline::models::pool_of(&entry.id);
            entry.provider = Some(pool.provider_id().to_string());
            count += 1;
        }
        if count > 0 {
            touched = true;
            summary.push(format!("{label} {count} 条"));
        }
    }

    // ② 映射：带旧 provider 的改名；provider 缺失但 target 是 Cline 池前缀的补上
    let mut renamed = 0usize;
    let mut tagged = 0usize;
    for mapping in rules.mappings.iter_mut() {
        match mapping.provider.as_deref() {
            Some(LEGACY_CLINE_ID) => {
                let pool = cline_pool_of(&mapping.target).provider_id();
                mapping.provider = Some(pool.to_string());
                renamed += 1;
            }
            // 旧版全局条目：只在 target 确实带**池前缀**时收窄（见函数头）
            None if is_cline_channel(&mapping.target) => {
                let pool = cline_pool_of(&mapping.target).provider_id();
                mapping.provider = Some(pool.to_string());
                tagged += 1;
            }
            _ => {}
        }
    }
    if renamed > 0 || tagged > 0 {
        touched = true;
        summary.push(format!("映射 {renamed} 条改名 / {tagged} 条补归属"));
    }

    // ③ 种子键：`cline:<id>[#alias:<别名>]` → `<池 provider id>:<同一段>`
    let mut reseeded = 0usize;
    for key in rules.seeded.iter_mut() {
        let Some(rest) = key.strip_prefix("cline:") else {
            continue;
        };
        // id 与 `#alias:` 后缀分开，只用 id 判池
        let id_part = rest.split('#').next().unwrap_or(rest);
        let pool = cline_pool_of(id_part).provider_id();
        *key = format!("{pool}:{rest}");
        reseeded += 1;
    }
    if reseeded > 0 {
        touched = true;
        summary.push(format!("种子标记 {reseeded} 条"));
    }

    if !touched {
        return None;
    }
    save(&rules);
    Some(format!(
        "🔀 Cline 已拆为 Cline Free / Cline Pass 两家，存量规则已迁移（{}）",
        summary.join("；")
    ))
}

/// 模型 id → 它属于哪个池（**迁移专用**的薄封装）。
///
/// 池的判定只有一处事实来源（`cline::models::pool_of`，按 id 前缀），这里
/// 只是把那个模块的路径收短一点 —— 迁移代码里要判七八次池，写全路径会淹没
/// 逻辑本身。
fn cline_pool_of(model_id: &str) -> crate::server::core::providers::cline::models::Pool {
    crate::server::core::providers::cline::models::pool_of(model_id)
}

/// 这个 id 是不是 Cline 的**计费通道** id（带 `cline-free/` / `cline-pass/` 前缀）。
///
/// 迁移用它区分「Cline 独有的 id」与「可能被别家也承载的裸模型名」：
/// 前者可以安全地把旧版全局映射收窄到某一家，后者不能（见 `migrate_cline_split`）。
fn is_cline_channel(model_id: &str) -> bool {
    ["cline-free/", "cline-pass/"]
        .iter()
        .any(|prefix| model_id.starts_with(prefix))
}

