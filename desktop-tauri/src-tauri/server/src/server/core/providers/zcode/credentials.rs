//! ZCode 的凭证：OAuth 换回来的访问令牌 + 套餐 JWT + 设备标识。
//!
//! ── 这一家的凭证为什么是三个字段 ─────────────────────────────
//! 一次 OAuth 完成会同时拿到**两样东西**（参考实现 `OAuthFlowTokens`）：
//!
//!   - `access_token`：推理平面用的访问令牌（打 `open.bigmodel.cn` / `api.z.ai`）
//!   - `jwt`：ZCode 套餐令牌（打 `zcode.z.ai` 的领取接口用）
//!
//! 两者**不能互相替代**：推理走开放平台的编码套餐端点，领取走 ZCode 自己的
//! billing 接口，各认各的。参考实现把 jwt 单独存一个字段，本模块照做 ——
//! 只存 access_token 的话领取功能会直接不可用（反之亦然）。
//!
//! 第三个字段 `device_mid` 不是凭证，而是**活动期的网关要求**：
//! 领取接口在 0828 / 0918 两期活动里都要求一个 UUID 形态的 `X-Device-Mid`，
//! 缺它会被以 biz 3001「参数错误」拒掉（参考实现实测记录，见 `claim.rs` 模块头）。
//! 它必须**跨请求稳定**（风控会把同一个设备标识的多次请求关联起来），
//! 因此生成一次就随凭证落盘，而不是每次请求现编一个。
//!
//! ── 与「账号记录」的关系 ────────────────────────────────────
//! 落盘时三个字段都进账号记录的 `auth` 子对象（`accessToken` / `jwt` /
//! `deviceMid`）—— 与各家一致的形态，于是适配器那条
//! `account.get("auth").get("accessToken")` 的通用取法在本家同样成立。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use super::region::Region;

/// 一次 ZCode 登录（或粘贴凭证）得到的全部凭证
#[derive(Clone, Debug)]
pub struct ZcodeCredentials {
    /// 这份凭证属于哪个地区（推理平面与账号 id 前缀都由它决定）
    pub region: Region,
    /// 推理平面访问令牌（打 `{openai_base}/chat/completions`）
    pub access_token: String,
    /// ZCode 套餐令牌（打 `zcode.z.ai` 的领取接口；可能是空的 —— 见下）
    pub jwt: String,
    /// 上游用户 id（OAuth 响应里的 `user.user_id`；用于展示名与去重）
    pub user_id: String,
    /// 设备标识（UUID 形态；跨请求稳定，见模块头）
    pub device_mid: String,
}

impl ZcodeCredentials {
    /// 展示名（账号列表里那一列）。
    ///
    /// 上游不给昵称/邮箱，只有 user_id，所以就用它拼一个可读的名字；
    /// 有邮箱时上游会把它放进 user 对象，但参考实现没有稳定取到，不猜。
    pub fn display_name(&self) -> String {
        if self.user_id.trim().is_empty() {
            format!("ZCode {}", self.region.label())
        } else {
            format!("ZCode {} · {}", self.region.label(), self.user_id.trim())
        }
    }

    /// 账号记录 id（`accounts.json` 主键）。
    ///
    /// 前缀由地区给出（`zcode-user-` / `zcode-intl-user-`），于是两地的记录
    /// 天然不相交 —— 同一个人在两套系统里的 user_id 可能相同，
    /// 撞 id 会让存储层的保护拒绝写入，而「两地账号并存」正是两个 provider
    /// 建模的意义之一（与 AutoClaw 国际版同一处置，见 `region.rs`）。
    ///
    /// `user_id` 为空时回落到固定串：OAuth 理论上总会给 user_id，
    /// 但真为空时用固定串至少能落一条可用记录，而不是生成一个空 id
    /// （空 id 会让后续的 patch / remove 全部找不到它）。
    pub fn account_id(&self) -> String {
        let suffix = if self.user_id.trim().is_empty() {
            "unknown"
        } else {
            self.user_id.trim()
        };
        format!("{}{}", self.region.account_id_prefix(), suffix)
    }

    /// 这份凭证够不够用来**转发**（推理只需要访问令牌）
    pub fn can_forward(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    /// 这份凭证够不够用来**领取套餐**（领取只需要套餐 JWT）
    ///
    /// 与 [`Self::can_forward`] 分开：粘贴凭证的场景下用户可能只给了其中一样
    /// （例如只想测领取），此时另一项功能会如实报缺，而不是整体判为不可用。
    pub fn can_claim(&self) -> bool {
        !self.jwt.trim().is_empty()
    }
}

/// 从令牌里读出过期时刻（**毫秒**，供账号页的「有效期」列显示）。
///
/// ── 为什么必须有这个函数（一个跨模块的契约）──────────────────
/// 账号页的能力表（`ui/accounts-groups.js`）给本家登记的是
/// `expiry: 'expiresAt'`，并在注释里写明「这是给 `add_zcode_account` 的契约 ——
/// 落账号时要把访问令牌的过期时间写进这个键」。缺了它，那一列会**空着**，
/// 而用户无从知道是「没有过期时间」还是「读不到」。
///
/// ── 取值链为什么有两个来源 ──────────────────────────────────
/// 上游给的访问令牌**未必**是 JWT（可能是不可解析的不透明串），而套餐 JWT
/// 一定是。因此先试 `access_token`、再退回 `jwt` —— 两条都是「有就用」，
/// 都解不出时返回 None（那一列空着，比编一个假时间诚实）。
///
/// `exp` 是 unix **秒**（JWT 标准），这里换算成毫秒 —— 账号页各家的
/// `expiresAt` 一律是毫秒（与 workbuddy / qoder 同口径），混用会让时间
/// 显示成 1970 年。
pub fn expires_at_ms(credentials: &ZcodeCredentials) -> Option<f64> {
    jwt_expires_at_ms(&credentials.access_token)
        .or_else(|| jwt_expires_at_ms(&credentials.jwt))
}

/// 解一个 JWT 的 payload 段取 `exp`（秒 → 毫秒）。解不出返回 None。
///
/// 不校验签名：这里只读一个展示用的时间戳，而令牌是上游刚下发的。
/// 签名校验需要公钥，那是一条与本用途无关的链路。
fn jwt_expires_at_ms(token: &str) -> Option<f64> {
    let payload = token.trim().split('.').nth(1)?;
    // base64url（带不带 padding 都认）：JWT 的 payload 段按 RFC 7515 是
    // base64url 且**不带** padding，但有的实现会补上，因此两条都试。
    let normalized = payload.trim_end_matches('=');
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        normalized,
    )
    .or_else(|_| {
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, normalized)
    })
    .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = value.get("exp").and_then(Value::as_f64)?;
    // 上游若给的是毫秒（少数实现这么干），别再乘一次
    Some(if exp > 1e12 { exp } else { exp * 1000.0 })
}

/// 生成一个 UUID v4 形态的设备标识（`X-Device-Mid`）。
///
/// ── 为什么必须符合 UUID 形态而不是随便一串 ───────────────────
/// 上游把它当设备指纹解析（参考实现记为「UUID-format `X-Device-Mid`」），
/// 非 UUID 形态与缺失同效 —— 都是 biz 3001。因此这里按 RFC 4122 v4 拼：
/// 第 7 字节高 4 位固定 `0100`、第 9 字节高 2 位固定 `10`。
///
/// 随机源用 `getrandom`（与 `qoder::machine` 同一处依赖，不引入新 crate）。
/// 取不到随机数时返回 None 而不是 panic —— 调用方可以退回「不带该头」，
/// 那虽然会命中 3001，但比整个进程 abort 好（release 是 `panic=abort`）。
pub fn new_device_mid() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    // 版本位（v4）与变体位（RFC 4122）
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ))
}
