//! AutoClaw 的**地区**（国内版 / 国际版）：两套域名与两条 provider 身份。
//!
//! ── 为什么地区要成为一等公民 ────────────────────────────────
//! AutoClaw 是同一套客户端代码的两个**构建**（`isOversea` 编译期常量：
//! 国内 `false` / 国际 `true`），它们共用同一套账号体系接口与同一套
//! `X-Auth-Sign` 客户端指纹（appId/appKey 两地逐字相同，实测确认），
//! 但**站点不同**：
//!
//! ```text
//!            userapi（账号 / 积分 / 签到）                       LLM 代理
//!   国内  https://autoglm-acceleration-api.zhipuai.cn  .../autoclaw-proxy/proxy/autoclaw
//!   国际  https://autoglm-api.autoglm.ai               .../autoclaw-proxy/proxy/autoclaw
//! ```
//!
//! 因此「这一家的域名」不能是模块级常量 —— 那是改造前只有一家时的写法。
//! 本文件是**地区 → 域名 / 身份 / 环境变量**的唯一事实来源，别处不要再写
//! `"https://autoglm-api.autoglm.ai"` 这类字面量。
//!
//! ── 为什么是两个 provider 而不是「一家的一个选项」（与 Cline 同一条思路）──
//! Cline 的两个额度池、AutoClaw 的两个地区，形态是同一类问题：**同一套协议、
//! 不同的通道**。把它们建成「一个 provider 上的 `region` 字段」的后果与 Cline
//! 那次一模一样 —— 地区成了**账号的属性**，界面上混在一起，而「哪个账号走哪个
//! 站点」在列表里看不出来；更要紧的是账号库里的记录无法按地区隔离，
//! 用户给两个地区各加一个账号时，界面与选路都会把它们当成同一家的两条记录。
//!
//! 现在按**两个 provider** 建模（`autoclaw` 国内版 / `autoclaw-intl` 国际版）：
//! 各自有独立的账号、清单、启停与映射，界面上各占一个分组、在添加弹窗里相邻。
//!
//! ── 国内版的 provider id 为什么**不改名** ────────────────────
//! 注册表里 `autoclaw` 这个 id 已经在用户的 accounts.json 里了（`provider` 字段
//! 是落盘契约）。把它改成 `autoclaw-cn` 会让全部存量账号在升级后变成「未知
//! provider」——`kind_from_id` 返回 None，账号从列表里静默消失。
//! 所以**只改展示名**（`label` 从 `AutoClaw` 变成 `AutoClaw 国内版`），id 保持
//! `autoclaw` 不变；新增的国际版取 `autoclaw-intl`。
//!
//! ── 桌面端登录态文件是两地**共用**的（一处真实的歧义，别装作没有）──
//! 两个构建的 Electron 应用名都是 `autoclaw`，userData 都落在
//! `%APPDATA%/AutoClaw`（Windows 大小写不敏感，`%APPDATA%/autoclaw` 是同一个
//! 目录 —— 实测 `stat` 出来的 inode 相同）。也就是说 **`auth.json` 里是哪一
//! 个地区的登录态，只取决于用户装的是哪个构建**，文件本身没有地区标记
//! （`{deviceId, updatedAt, userInfo, token, refreshToken}`，token 是 safeStorage
//! 密文）。
//!
//! 这带来一个无法在本机消解的歧义：**同一个 auth.json 不能同时是两地的登录态**。
//! 本模块的处置是**不猜**，并且**把选择权交给用户**：
//!   - 「导入桌面端登录态」**两个地区都给**。用户在哪一项下点导入，就得到哪一家
//!     的账号 —— 他自己知道装的是哪个客户端，本机不知道。
//!   - 猜错的后果是**可见的**：凭证与域名不匹配时上游直接 401，换一项重导即可。
//!   - 两地的桌面端账号用**不同的记录 id**（`autoclaw-desktop` /
//!     `autoclaw-intl-desktop`），因此两条记录可以并存，不会撞 id。
//!
//! ── 曾经的错误做法（别改回去）──────────────────────────────
//! 这里一度只给国内版开放导入（理由是「文件归国内版」），国际版直接 401。
//! 那条限制把最需要这条路的人挡住了：**国际版客户端的官方主登录方式是
//! Zai / Google OAuth**，而当时 OAuth 链路网关走不通（强制阿里云风控验证码，
//! 见 `login.rs` 的模块头），于是「从客户端导入」几乎是 OAuth 用户唯一实用的
//! 入口。本机这份 auth.json 正是一个 OAuth 账号（邮箱有值、手机号为空）。
//!
//! OAuth 那条链路**现已接上**（见 `oauth.rs`），因此导入不再是唯一入口 ——
//! 但导入本身**照旧两个地区都给**：它是一条独立可用的路径（不需要联网过一次
//! 验证码），没有任何理由因为「多了 OAuth」就把它收回去。
//!
//! ── 缓存键必须带地区（一处真 bug，已修）──────────────────────
//! 两地的 `auth.json` 是同一个文件、同一个 mtime，而凭证缓存按
//! 「来源 + 路径指纹 + mtime」作键。键里不带地区的话，先解析的那一家会把凭证
//! 连同**它自己的 region** 一起缓存进去，另一家读到后拿到「带着对方域名的
//! 凭证」—— 转发打到错误站点、稳定 401，且日志上完全看不出原因（缓存命中，
//! 没有任何解析痕迹）。现在键里带 `region.provider_id()`（见 `credentials.rs`）。
//!
//! ── 环境变量 ────────────────────────────────────────────────
//! 国内版沿用历史名（`AUTOCLAW_*`），国际版用 `AUTOCLAW_INTL_*` ——
//! 不能两地共用一个变量名：那会让「只想给国际版配代理」变成「两地一起改」。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use crate::server::core::providers::{kind_id, ProviderKind};

/// AutoClaw 的地区。
///
/// 顺序 = 注册表顺序（国内版在前）：`ALL` 的遍历顺序决定模型目录合并时
/// 同名模型先归谁家、以及界面上两家的先后。**国内版在前**是因为它是历史
/// 已有的一家（存量账号都归它），把国际版排在后面不会改变既有账号的归属。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Region {
    /// 国内版（智谱加速域；历史已有的那一家，provider id 仍是 `autoclaw`）
    #[default]
    Cn,
    /// 国际版（autoglm.ai；本次新增，provider id 是 `autoclaw-intl`）
    Intl,
}

impl Region {
    /// 两个地区（注册表顺序：国内版在前）
    pub const ALL: [Region; 2] = [Region::Cn, Region::Intl];

    /// 本地区对应哪个 provider kind（地区 → 身份的**唯一**映射）
    pub fn kind(self) -> ProviderKind {
        match self {
            Self::Cn => ProviderKind::AutoClaw,
            Self::Intl => ProviderKind::AutoClawIntl,
        }
    }

    /// 本地区的 provider id（`"autoclaw"` / `"autoclaw-intl"`）
    pub fn provider_id(self) -> &'static str {
        kind_id(self.kind())
    }

    /// provider id → 地区（`autoclaw` 系之外的 id 返回 None）
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|region| region.provider_id() == provider_id)
    }

    /// 这个 kind 是不是 AutoClaw 系（两家都算）—— 判据只在这里写一份
    pub fn from_kind(kind: ProviderKind) -> Option<Self> {
        Self::ALL.into_iter().find(|region| region.kind() == kind)
    }

    /// 展示名（界面上跟在 `AutoClaw` 后面的那一段）
    pub fn label(self) -> &'static str {
        match self {
            Self::Cn => "国内版",
            Self::Intl => "国际版",
        }
    }

    /// 上游 LLM 代理基址（**含**尾部的 `/autoclaw`）
    pub fn upstream_base_url(self) -> &'static str {
        match self {
            Self::Cn => "https://autoglm-acceleration-api.zhipuai.cn/autoclaw-proxy/proxy/autoclaw",
            Self::Intl => "https://autoglm-api.autoglm.ai/autoclaw-proxy/proxy/autoclaw",
        }
    }

    /// 用户中心（userapi）基址 —— 刷新 / 积分 / 签到 / 登录都在这个域
    pub fn userapi_base_url(self) -> &'static str {
        match self {
            Self::Cn => "https://autoglm-acceleration-api.zhipuai.cn",
            Self::Intl => "https://autoglm-api.autoglm.ai",
        }
    }

    /// 本地区的环境变量前缀。
    ///
    /// 国内版是 `AUTOCLAW_`（历史名，不能改：用户已有的脚本 / CI 配置依赖它），
    /// 国际版是 `AUTOCLAW_INTL_`。
    pub fn env_prefix(self) -> &'static str {
        match self {
            Self::Cn => "AUTOCLAW_",
            Self::Intl => "AUTOCLAW_INTL_",
        }
    }

    /// 读本地区的环境变量覆盖（空值视为未设置）。
    ///
    /// 返回 None 表示没有覆盖 —— 调用方回落到 [`Self::upstream_base_url`] /
    /// [`Self::userapi_base_url`]。
    pub fn env_override(self, name: &str) -> Option<String> {
        let key = format!("{}{name}", self.env_prefix());
        std::env::var(key)
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
    }

    /// 本地区账号记录 id 的**前缀**（id 生成用）。
    ///
    /// ── 为什么国际版要带前缀、国内版不带 ────────────────────────
    /// 账号 id 是 `accounts.json` 的主键，**在整份账号集合里唯一**。
    ///
    /// 国内版必须保持裸 `user-<userId>`：存量账号的 id 就长这样，换前缀会让它们
    /// 在升级后变成「找不到的记录」（`remove_account` / `patch_account` 都按裸 id
    /// 查找），用户看到的是账号凭空消失。
    ///
    /// 国际版**要**带前缀：两地的 userId 空间互不相通，但**同一个人在两地的
    /// userId 完全可能相同**（同一手机号在两套系统里各自注册，而服务端的 id
    /// 生成规则一旦同形就会撞）。撞了的话存储层现有的撞 id 保护会直接报错拒绝
    /// （见 `add_autoclaw_account` 的跨 provider 判定）—— 那虽然不会写坏数据，
    /// 却让「两地账号并存」变成不可能，而并存正是本功能的意义之一。
    /// 带上前缀之后两地的 id 天然不相交，同一个人的两个地区账号可以并存。
    ///
    /// Cline 的两个池用的是同一手法（`cline-free-usr-…` / `cline-pass-usr-…`），
    /// 那边是先拆池、不存在存量 id，所以两家都带前缀。
    pub fn account_id_prefix(self) -> &'static str {
        match self {
            Self::Cn => "user-",
            Self::Intl => "intl-user-",
        }
    }
}
