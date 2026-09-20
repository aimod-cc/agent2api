//! Cline（官方 `api.cline.bot`）：账号管理 + 推理转发。
//!
//! ── 上游是什么（一句话）─────────────────────────────────────
//! Cline 官方账号体系是一个**标准 OpenAI 兼容上游**：LLM 走
//! `POST https://api.cline.bot/api/v1/chat/completions`，鉴权是
//! `Authorization: Bearer workos:<JWT>`，模型名带**计费通道前缀**
//! （`cline-pass/` 订阅池、`cline-free/` 免费池）。比 CatPaw 的自有
//! conversation 协议与 Qoder 的 COSY 自签名都简单。
//!
//! ── 子模块 ──────────────────────────────────────────────────
//! ```text
//!   credentials.rs 凭证格式（workos: 前缀）+ 桌面端 providers.json 读取
//!   login.rs       设备授权登录（WorkOS RFC 8628，四步含 /auth/register 换码）
//!   refresh.rs     续期（POST /auth/refresh + 单飞 + 比较再写）
//!   models.rs      模型目录（recommended-models + 两池 + 静态兜底 + 默认映射种子）
//!   balance.rs     余额 / 订阅（credit 余额归一 + 套餐）
//!   adapter.rs     ProviderAdapter 实现（无状态，OpenAI 兼容，**按池参数化**）
//! ```
//!
//! ── 三件必须记住的事（都是实测踩出来的）─────────────────────
//!   1. **产品面头 `X-CLIENT-TYPE: cline-sdk`**：不带它免费池模型一律 403
//!      （`only available via Cline product surfaces`）。见 `adapter.rs` 模块头。
//!   2. **路径只有一个 `v1`**：`/api/v1/chat/completions`，不是 `/api/v1/v1/...`。
//!   3. **响应形态不对称**：`stream:true` 是裸 chunk（可直传），非流式是
//!      `{"data":{...}}` 信封。网关总是以流式请求上游，所以主链路只走前者；
//!      管理接口（balance）要解信封。
//!
//! ── 凭证来源（两种，没有环境变量旁路）───────────────────────
//!   - **账号记录**：`POST /api/login`（设备授权）或手填 token 添加；
//!   - **桌面端实时登录态**：`~/.cline/data/settings/providers.json`，
//!     每次实时读取（不回写 —— 那是 Cline 客户端自己的文件，见
//!     `refresh::persist_refresh` 的说明）。
//!
//! ── 两个额度池 = 两家提供商（本模块的组织方式）───────────────
//! `cline-free/` 与 `cline-pass/` 在上游是**同一个账号上的两个额度池**，
//! 但本项目把它们当**两个 provider** 接入：`cline-free` 与 `cline-pass`
//! 各有独立的账号、清单、启停规则与映射，界面上各占一个分组。
//!
//! 实现仍是**一套**：`adapter::ClineAdapter` 持有一个 `models::Pool`，两个
//! 静态实例（`cline::CLINE_FREE_ADAPTER` / `CLINE_PASS_ADAPTER`）由
//! `adapter::adapter_for` 按 kind 给出。池 → provider 的互查在 `models::Pool`
//! （`kind` / `provider_id` / `from_provider_id`），别处不要再写 `"cline-free"`
//! 这类字面量。
//!
//! 拆分的理由（以及「一家的一个选项」那套旧建模为什么说不通）见
//! `models.rs` 的模块头。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块绝不 unwrap/expect/panic。

pub mod adapter;
pub mod balance;
pub mod credentials;
pub mod login;
pub mod models;
pub mod refresh;

pub use adapter::{CLINE_FREE_ADAPTER, CLINE_PASS_ADAPTER};
