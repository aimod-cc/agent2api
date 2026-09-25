//! ZCode 的模型清单（**静态表**，两地共用一份）。
//!
//! ── 为什么是静态表而不是远程目录 ────────────────────────────
//! 上游没有「列模型」的公开接口：编码套餐的可用模型由**客户端内置的目录**
//! 决定，参考实现（`Acankao/zcode-api` 的 `src/provider/models.ts`）同样把它
//! 硬编码成常量表，并在注释里写明「新模型发布或规格变化时更新这张表」。
//!
//! 因此本模块不提供 `refresh`：`refresh_models` 会如实回答「这家没有远程目录」
//! （`supports_model_refresh() = false`），而不是假装刷了一次。
//! 这与 raccoon / autoclaw 那两家「有远程目录」的情形不同，别照抄它们的结构。
//!
//! ── 两地清单为什么逐字相同 ──────────────────────────────────
//! 参考实现的两处注记都写着 zai 与 bigmodel 的目录条目**完全一致**
//! （`models_catalog.json` 的 `zai` / `bigmodel` 两段同值）。差异在**账号能
//! 用到哪些**（套餐档位决定），不在清单本身 —— 而「某账号能用哪些」是上游
//! 按套餐判的（用不到的模型会回 `502 exceed quota limit`），本机无法预先枚举。
//! 所以两地共用一份清单，把「用不到」留给上游如实报错，与参考实现一致。
//!
//! ── 字段形状 ────────────────────────────────────────────────
//! 条目用的是**聚合目录认的键**（`id` / `name` / `maxInputTokens` /
//! `maxOutputTokens` / `supportsImages` / `supportsReasoning`），
//! 由 `models::shape::list_item` 翻成 OpenAI 形态 —— 与 raccoon 那家
//! 「上游原始形态 + 出口翻译」的做法不同：本家没有上游原始形态可言
//! （清单就是本机常量），所以直接按聚合层的输入契约书写，少一层无谓映射。
//!
//! ── `glm-5.3-flash` 为什么要广告出来 ────────────────────────
//! 它是**体验套餐（周末套餐）**被领到之后实际可用的那一档 —— 参考实现为此
//! 专门加了一条注记：「start-plan gateway serves the -flash variant (used by
//! claimed trial plans like the weekend package); advertised so client-side
//! discovery lists it」。不广告它，用户领到套餐后在 `/v1/models` 里看不到
//! 能用的模型，会以为领取失败。

use serde_json::{json, Value};

use super::region::Region;

/// 一个模型的静态规格
struct Spec {
    /// 上游模型 id（**原样转发**，不做任何改名）
    id: &'static str,
    /// 展示名
    name: &'static str,
    /// 上下文窗口
    context: i64,
    /// 最大输出
    max_output: i64,
    /// 是否支持思考
    reasoning: bool,
    /// 是否支持图像输入
    vision: bool,
}

/// 编码套餐的模型表（规格同步自参考实现，注释里的数字来自其 `models.ts`）。
///
/// 顺序 = 展示顺序：能力由强到弱、同代相邻，便于管理页阅读。
const MODELS: &[Spec] = &[
    Spec { id: "glm-5.3", name: "GLM-5.3", context: 1_000_000, max_output: 128_000, reasoning: true, vision: false },
    Spec { id: "glm-5.3-flash", name: "GLM-5.3 Flash", context: 1_000_000, max_output: 128_000, reasoning: true, vision: false },
    Spec { id: "glm-5.2", name: "GLM-5.2", context: 1_000_000, max_output: 128_000, reasoning: true, vision: false },
    Spec { id: "glm-5.1", name: "GLM-5.1", context: 200_000, max_output: 64_000, reasoning: true, vision: false },
    Spec { id: "glm-5", name: "GLM-5", context: 200_000, max_output: 64_000, reasoning: true, vision: false },
    Spec { id: "glm-5-turbo", name: "GLM-5 Turbo", context: 200_000, max_output: 64_000, reasoning: true, vision: false },
    Spec { id: "glm-4.7", name: "GLM-4.7", context: 200_000, max_output: 131_072, reasoning: true, vision: false },
    Spec { id: "glm-4.6", name: "GLM-4.6", context: 200_000, max_output: 131_072, reasoning: true, vision: false },
    Spec { id: "glm-4.5-air", name: "GLM-4.5 Air", context: 131_072, max_output: 98_304, reasoning: true, vision: false },
    // 视觉两档：不支持思考，能力位与其它档不同（目录里要如实标出来，
    // 否则客户端会按「支持思考」去发 `thinking`，上游直接拒）
    Spec { id: "glm-4.6v", name: "GLM-4.6V", context: 131_072, max_output: 32_768, reasoning: false, vision: true },
    Spec { id: "glm-5v-turbo", name: "GLM-5V Turbo", context: 200_000, max_output: 131_072, reasoning: false, vision: true },
];

/// 本家的模型清单（聚合目录认的形态）。
///
/// `region` 当前**不影响**返回值（两地清单相同，见模块头），参数保留是为了
/// 让调用点写成 `models::list(region)` —— 将来若真出现地区差异（例如某地上了
/// 独家模型），改这一处即可，不必回头改所有调用点。
pub fn list(_region: Region) -> Vec<Value> {
    MODELS
        .iter()
        .map(|spec| {
            json!({
                "id": spec.id,
                "name": spec.name,
                "maxInputTokens": spec.context,
                "maxOutputTokens": spec.max_output,
                // 编码套餐的模型都支持工具调用（客户端本身就是靠它跑 agent 循环）
                "supportsToolCall": true,
                "supportsImages": spec.vision,
                "supportsReasoning": spec.reasoning,
            })
        })
        .collect()
}

/// 这个模型名是不是本家认识的（**广告视图**的判据，见 `adapter` 的 `list_models`）。
///
/// 大小写不敏感：上游模型 id 是小写，而客户端偶尔会带上原始大小写
/// （`GLM-5.3`），若按严格相等判定会让「列表里明明有」的名字点不动。
pub fn is_known(model: &str) -> bool {
    let needle = model.trim().to_ascii_lowercase();
    MODELS.iter().any(|spec| spec.id == needle)
}
