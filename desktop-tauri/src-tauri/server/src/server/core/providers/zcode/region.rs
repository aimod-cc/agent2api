//! ZCode 的**地区**（国内版 / 国际版）：两套域名与两条 provider 身份。
//!
//! ── 这一家是什么 ────────────────────────────────────────────
//! ZCode 是智谱 / Z.AI 的编码代理客户端（`zcode.z.ai`），它的「编码套餐」
//! （Coding Plan / Start Plan）在客户端里可以用**订阅登录态**调用，而不必
//! 去开放平台申请 API Key。本家要复刻的正是这条登录态链路：
//!
//! ```text
//!              zcode 平面（登录 / 领取 / 客户端配置）      推理平面
//!   国内版  https://zcode.z.ai                       https://open.bigmodel.cn
//!   国际版  https://zcode.z.ai                       https://api.z.ai
//! ```
//!
//! ── 为什么 zcode 平面两地**相同**、推理平面两地不同 ─────────
//! 登录与「周末套餐」领取都发生在 ZCode 自己的服务端（`zcode.z.ai`），
//! 两地客户端用的是**同一个** `zcode.z.ai`（OAuth 的 `provider` 字段区分
//! `zai` / `bigmodel`，见 `oauth.rs`）。而真正跑推理的网关是各自开放平台的
//! 编码套餐端点：国内 `open.bigmodel.cn`、国际 `api.z.ai`。
//!
//! 也就是说「地区」在这一家**只影响推理平面与账号归属**，不影响 zcode 平面 ——
//! 这与 AutoClaw（两地各自一整套域名）不同，是照抄参考实现时最容易搞错的一处。
//!
//! ── 为什么是两个 provider 而不是「一家的一个选项」──────────
//! 与 Cline 的两个额度池、AutoClaw / Accio 的两个地区同一思路：做成「一个
//! provider 上的 `region` 字段」会让地区变成**账号的属性**，界面上混在一起、
//! 「哪个账号走哪个站点」看不出来，账号记录也无法按地区隔离。
//! 按两个 provider 建模之后各自有独立的账号、清单、启停与映射。
//!
//! ── 与 `preset-providers.js` 里既有两项的关系（别重复建设）──
//! 前端预设里已经有 `glm`（`api.z.ai/api/anthropic`）与 `glm-cn`
//! （`open.bigmodel.cn/api/coding/paas/v4`）两个**自定义提供商** —— 那两条路
//! 要用户自己提供 API Key，走的是开放平台计费。本家补的是**另一条通道**：
//! 用 ZCode 的订阅登录态（OAuth → JWT）转发，不需要 API Key。
//! 两者可以并存：想用 API Key 的继续用预设，想用订阅额度的用本家。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use crate::server::core::providers::{kind_id, ProviderKind};

/// ZCode 的地区。
///
/// 顺序 = 注册表顺序（国内版在前）：`ALL` 的遍历顺序决定模型目录合并时
/// 同名模型先归谁家、以及界面上两家的先后。国内版在前是因为国内网络环境
/// 下它是更常被添加的那一个（与 AutoClaw 的排序理由一致）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Region {
    /// 国内版（智谱开放平台；provider id 是 `zcode`）
    #[default]
    Cn,
    /// 国际版（Z.AI；provider id 是 `zcode-intl`）
    Intl,
}

impl Region {
    /// 两个地区（注册表顺序：国内版在前）
    pub const ALL: [Region; 2] = [Region::Cn, Region::Intl];

    /// 本地区对应哪个 provider kind（地区 → 身份的**唯一**映射）
    pub fn kind(self) -> ProviderKind {
        match self {
            Self::Cn => ProviderKind::Zcode,
            Self::Intl => ProviderKind::ZcodeIntl,
        }
    }

    /// 本地区的 provider id（`"zcode"` / `"zcode-intl"`）
    pub fn provider_id(self) -> &'static str {
        kind_id(self.kind())
    }

    /// provider id → 地区（`zcode` 系之外的 id 返回 None）
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|region| region.provider_id() == provider_id)
    }

    /// 这个 kind 是不是 ZCode 系（两家都算）—— 判据只在这里写一份
    pub fn from_kind(kind: ProviderKind) -> Option<Self> {
        Self::ALL.into_iter().find(|region| region.kind() == kind)
    }

    /// 展示名（界面上跟在 `ZCode` 后面的那一段）
    pub fn label(self) -> &'static str {
        match self {
            Self::Cn => "国内版",
            Self::Intl => "国际版",
        }
    }

    /// OAuth / 领取链路里的 `provider` 取值（参考实现的 `ProviderId`）。
    ///
    /// 它同时是 `/oauth/cli/init` 请求体里的 `provider`、`/oauth/cli/poll`
    /// 响应里那个分支键（`data.zai.access_token` / `data.bigmodel.access_token`）。
    /// **不是**我们的 provider id —— 这两个命名空间别混。
    pub fn upstream_provider(self) -> &'static str {
        match self {
            Self::Cn => "bigmodel",
            Self::Intl => "zai",
        }
    }

    /// zcode 平面基址（登录 / 领取 / 客户端配置；**两地相同**）。
    pub fn zcode_origin(self) -> &'static str {
        "https://zcode.z.ai"
    }

    /// 推理平面：OpenAI 兼容基址（**不含**尾斜杠）
    pub fn openai_base_url(self) -> &'static str {
        match self {
            Self::Cn => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::Intl => "https://api.z.ai/api/coding/paas/v4",
        }
    }

    /// 推理平面：Anthropic 兼容基址（**不含**尾斜杠）
    pub fn anthropic_base_url(self) -> &'static str {
        match self {
            Self::Cn => "https://open.bigmodel.cn/api/anthropic",
            Self::Intl => "https://api.z.ai/api/anthropic",
        }
    }

    /// 开放平台业务域（API Key 解析用；参考实现的 `bizHost`）
    pub fn biz_host(self) -> &'static str {
        match self {
            Self::Cn => "https://open.bigmodel.cn",
            Self::Intl => "https://api.z.ai",
        }
    }

    /// 本地区的环境变量前缀。
    ///
    /// 两家都带前缀（本家是新接入的，不存在存量裸名变量）：国内 `ZCODE_`、
    /// 国际 `ZCODE_INTL_` —— 不能两地共用一个名字，那会让「只想给国际版配
    /// 代理」变成「两地一起改」（与 AutoClaw 同一条理由）。
    pub fn env_prefix(self) -> &'static str {
        match self {
            Self::Cn => "ZCODE_",
            Self::Intl => "ZCODE_INTL_",
        }
    }

    /// 读本地区的环境变量覆盖（空值视为未设置）。
    ///
    /// 返回 None 表示没有覆盖 —— 调用方回落到上面那几个常量基址。
    pub fn env_override(self, name: &str) -> Option<String> {
        let key = format!("{}{name}", self.env_prefix());
        std::env::var(key)
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
    }

    /// 本地区账号记录 id 的**前缀**（id 生成用）。
    ///
    /// 两家都带前缀：本家没有存量账号（不存在 AutoClaw 国内版那种「裸
    /// `user-<id>` 是落盘契约」的约束），而带前缀能让两地的记录天然不相交 ——
    /// 同一个人在国内 / 国际两套系统里的 userId 完全可能相同（与 AutoClaw
    /// 国际版同一处境），撞了的话存储层的撞 id 保护会拒绝写入，
    /// 而「两地账号并存」正是两个 provider 建模的意义之一。
    pub fn account_id_prefix(self) -> &'static str {
        match self {
            Self::Cn => "zcode-user-",
            Self::Intl => "zcode-intl-user-",
        }
    }
}
