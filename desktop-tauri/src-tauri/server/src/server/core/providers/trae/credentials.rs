//! Trae 凭据（`auth.Auth` 的 Rust 对应物）。
//!
//! ── 哪些是长期的、哪些是短期的 ────────────────────────────
//! `accessToken` 是 **Cloud-IDE-JWT**，约 14 天到期；`refreshToken` 长期但
//! **每次 ExchangeToken 都轮换**（一次性）。所以"能不能续"取决于 refreshToken
//! 在不在，而"要不要续"取决于 accessToken 的到期时刻 —— 到期时刻优先信
//! JWT 自己的 `exp`，落盘字段只作兜底（参考实现两处都读，见 `expires_at_ms`）。
//!
//! 另外三个字段是**设备指纹**：`machineId` / `deviceId` 与登录时上传的公钥
//! 一起被服务端绑定（`CheckLogin` 会回 `BoundDeviceID` / `DeviceBindStatus`）。
//! 它们必须与凭据同生共死：换掉它们等于换了一台设备，社区明确把
//! "每次随机 deviceId" 标成风控隐患。
//!
//! `variant` 决定 ClientID / 平台码 / 目录血统，缺省按 `solo`
//! （本仓只服务 SOLO 通道，见 `mod.rs` 模块头）。

use serde_json::Value;

pub const DEFAULT_VARIANT: &str = "solo";

/// 一条 Trae 凭据。字段名与落盘形态一致（驼峰），因为账号存储就是按
/// 参考实现的 auth 文件形状读写（嵌套 `{type, provider, auth{…}, account{…}}`
/// 与平铺两种都认，见 `from_payload`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    /// accessToken 的到期时刻（Unix 秒或毫秒都接受，读的时候归一，见 `expires_at_ms`）。
    pub expires_at: i64,
    pub domain: String,
    /// OAuth/积分那条链的 host（`ExchangeToken` 的第一候选）。
    pub api_host: String,
    pub machine_id: String,
    pub device_id: String,
    pub variant: String,
    pub uid: String,
    pub enterprise_id: String,
    pub nickname: String,
    /// 登录时那套设备密钥的**公钥**（SPKI PEM，随 ExchangeToken 上传过的那份）。
    ///
    /// 落盘它不是为了再用它发请求，而是为了让"这台设备绑定过哪个账号"这件事
    /// 在凭据里可读回来 —— 服务端按 DevicePublicKey + DeviceID/MachineID 做绑定，
    /// 出问题时（20405 那族）第一件事就是要能看见当时上传的是哪把公钥。
    pub device_public_key: String,
    /// 登录时那套设备密钥的私钥（PKCS#8 PEM）。
    ///
    /// 参考实现**只写不读**（全目录 grep `devicePrivateKey` 只有写入与
    /// "重建时不许丢"的夹具测试）。这里同样只存不用，但必须存：
    /// 写回凭据时把它抹掉，就等于把这台设备的绑定关系弄断了。
    pub device_private_key: String,
    /// 这张 refreshToken **当初是在哪个 OAuth 应用里 mint 的**（上游在
    /// `ExchangeToken` 响应里回显 `Result.ClientID`，CPA 的 auth 文件里同名
    /// 字段叫 `authClientId`）。
    ///
    /// ── 为什么它必须落盘，而不是按 variant 现推 ──────────────
    /// `variant` 是**转发面**的谱系（本家只服务 SOLO 通道），`ClientID` 是
    /// **凭据的归属**。两者可以不一致：生产实测（2026-10-02，NAS 三条从 CPA
    /// 迁来的账号）库里 `variant=solo`，而 CPA 盘上的 `authClientId` 与当年
    /// 换证响应的 `Result.ClientID` 都是非 solo 的 `ono9krqynydwx5`。按
    /// variant 现推就会拿 solo 的 `en1oxy7wnw8j9n` 去续期，上游回
    /// `400 10101 "refresh token is not matched to the client"` —— 三条账号
    /// 从第一次进入续期窗口起就续不上（假令牌的对照回的是
    /// `401 20101 "refresh token is invalid"`，两种失败不同形，见 `refresh`）。
    /// 空串 = 不知道（老数据 / 手工粘贴没带），那时才回退到按 variant 推。
    pub auth_client_id: String,
}

/// 国际版谱系（`intl` / `solo-intl`）。
///
/// 本家**只接了国内 SOLO 那一条通道**：`trae-api-cn.mchost.guru` 上的
/// `llm_utils_chat` + `solo_work_lite`，目录与积分两条链也都是 CN 域名。
/// 国际版走的是另一套两步协议（`core-normal.trae.ai/api/remote/v1/chat_sessions`
/// → `GET …/{id}/events`，`Origin`/`Referer` 必须是 `work.trae.ai`，流还是
/// 累积式要还原增量），没实现之前这类凭据**不能落账号** —— 收下它只会在
/// 第一次转发时收到一句来自 CN host 的 401，用户完全看不出是谱系不对。
/// 判据本身是大小写无关的（`Credential::variant()` 归一后才走到闸门，
/// 但这条函数也可能被别处直接拿去问原始值，所以不依赖调用前先归一）。
///
/// 反向验证：本家两处闸门（落账号与转发）都是拿**归一后**的值问它，
/// 因此"未知值兜到 solo"这条老规则不会被误伤成"Intl 被放行"。
pub fn is_intl_variant(variant: &str) -> bool {
    matches!(variant.trim().to_lowercase().as_str(), "intl" | "solo-intl")
}

/// 拒绝 Intl 谱系时给的那句理由（三处入口共用一份文案，别说三种话）。
pub const INTL_UNSUPPORTED: &str = "Trae 国际版（intl / solo-intl）暂未接入：本家只支持国内 SOLO 通道，国际版是另一套两步协议（chat_sessions → events）。请在 trae.cn 登录后再添加，或等 Intl 通道接入；国内 `cn` 与 `solo` 两个谱系在转发面等价（都发 solo_work_lite），不在此列。";

impl Credential {
    pub fn valid(&self) -> bool {
        !self.access_token.trim().is_empty()
    }

    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.trim().is_empty()
    }

    /// 上游 `function` 与 ClientID 都按 variant 选，未知值兜到 solo。
    pub fn variant(&self) -> &str {
        match self.variant.as_str() {
            "cn" | "solo" | "intl" | "solo-intl" => &self.variant,
            _ => DEFAULT_VARIANT,
        }
    }

    /// 到期时刻统一成**毫秒**。
    ///
    /// 上游与各家客户端在秒与毫秒之间来回漂过（参考实现为此留了
    /// `normalizeExpiresAt`），所以按数量级判：小于 1e11 的当秒用
    /// （1e11 秒 ≈ 公元 5138 年，真毫秒值不可能小于它）。
    pub fn expires_at_ms(&self) -> i64 {
        if self.expires_at <= 0 {
            return 0;
        }
        if self.expires_at < 100_000_000_000 {
            self.expires_at * 1000
        } else {
            self.expires_at
        }
    }

    /// JWT 里的 `exp`（毫秒），解不出来返回 None。
    ///
    /// 只看第二段、不验签：这不是安全判定，只是"还有多久要续"的读数来源。
    /// 验签要公钥，而我们对上游的令牌格式没有信任需求 —— 判错过一次
    /// 也只是早续或晚续一次。
    pub fn jwt_expires_at_ms(&self) -> Option<i64> {
        // saturating：release 是 panic=abort 且整数溢出在 release 下**静默回绕**，
        // 一个畸形的大 `exp`（上游哪天塞 1e16）会翻成负数到期时刻，
        // 于是"刚签发"被读成"早该续期"、每轮都白刷一次。宁可顶到 i64::MAX。
        jwt_claim(&self.access_token, "exp").map(|seconds| seconds.saturating_mul(1000))
    }

    /// JWT 里的 `iat`（秒）。参考实现额外用它在签发龄超 15 天时强制轮换
    /// （服务端有吊销旧凭据的风险），所以这个读数不是装饰。
    pub fn jwt_issued_at_seconds(&self) -> Option<i64> {
        jwt_claim(&self.access_token, "iat")
    }

    /// 有效到期时刻：JWT 的 `exp` 优先，落盘字段兜底。
    pub fn effective_expiry_ms(&self) -> i64 {
        self.jwt_expires_at_ms().unwrap_or_else(|| self.expires_at_ms())
    }

    /// 是否需要续期：临期（`lead` 之内）**或**签发龄超过 `max_age`。
    ///
    /// 两个条件都要：只看临期的话，一个刚签发但服务端已吊销的令牌会被
    /// 当成"还早"，等到真用它时才 401（那时 refreshToken 可能也已经换过一轮）。
    pub fn needs_refresh(&self, lead_ms: i64, max_issue_age_ms: i64, now_ms: i64) -> bool {
        let expiry = self.effective_expiry_ms();
        if expiry > 0 && expiry - now_ms <= lead_ms {
            return true;
        }
        if max_issue_age_ms <= 0 {
            return false;
        }
        self.jwt_issued_at_seconds()
            .map(|issued| now_ms.saturating_sub(issued * 1000) >= max_issue_age_ms)
            .unwrap_or(false)
    }

    /// 从账号记录里读凭据：嵌套（CPA auth 文件形状）与平铺都认。
    ///
    /// 只认驼峰键（与参考实现落盘一致）。看到 `access_key_id` 那一类
    /// 蛇形键**直接报错**而不是当空凭据收下 —— 那是别家的形状，
    /// 静默接受会让一个明显有数据的账号显示成"缺凭据"。
    pub fn from_payload(payload: &Value) -> Result<Self, String> {
        let auth = payload.get("auth").filter(|value| value.is_object()).unwrap_or(payload);
        let account = payload.get("account").filter(|value| value.is_object());
        let text = text_of;
        let number = |object: &Value, key: &str| -> i64 {
            object
                .get(key)
                .and_then(Value::as_f64)
                .map(|value| value as i64)
                .unwrap_or(0)
        };
        // OAuth 归属（哪把 ClientID mint 的这张 refreshToken）。四种写法都认：
        // 本家与 CPA 落盘是 `authClientId`，上游响应回显是 `ClientID`，
        // 手工粘贴可能给小写 `clientId`；嵌套 `auth{}` 与平铺 payload 两个位置都找。
        let auth_client_id = [
            text(auth, "authClientId"),
            text(auth, "ClientID"),
            text(auth, "clientId"),
            text(payload, "authClientId"),
        ]
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default();
        let credential = Self {
            access_token: text(auth, "accessToken"),
            refresh_token: text(auth, "refreshToken"),
            expires_at: number(auth, "expiresAt"),
            domain: text(auth, "domain"),
            api_host: text(auth, "apiHost"),
            machine_id: text(auth, "machineId"),
            device_id: text(auth, "deviceId"),
            variant: text(auth, "variant"),
            uid: text_either(account, auth, "uid"),
            enterprise_id: text_either(account, auth, "enterpriseId"),
            nickname: text_either(account, auth, "nickname"),
            device_public_key: text(auth, "devicePublicKey"),
            device_private_key: text(auth, "devicePrivateKey"),
            auth_client_id,
        };
        if credential.access_token.trim().is_empty() && credential.refresh_token.trim().is_empty() {
            return Err("Trae 凭据里既没有 accessToken 也没有 refreshToken".to_string());
        }
        Ok(credential)
    }

    /// 写回账号记录用的那部分字段（**只并这些键**，不重建整条记录）。
    ///
    /// 参考实现里出过事故的那条：面板签到前的预刷新走了"重建"路径，
    /// refreshToken 保住了但设备密钥与一堆对拍扩展字段被抹掉。
    /// 所以这里给的是"字段补丁"，落盘侧只做覆盖写。
    pub fn patch_fields(&self) -> Vec<(&'static str, Value)> {
        let mut fields = vec![
            ("accessToken", json_or_empty(&self.access_token)),
            ("refreshToken", json_or_empty(&self.refresh_token)),
            // 落盘**统一成毫秒**。本家收到的值在秒与毫秒之间漂过：CPA 的 auth
            // 文件与手工粘贴是秒（`expiresAt: 1791009732`），上游刷新响应是毫秒。
            // 存进去是什么单位，决定了面板怎么读它 —— 有效期那一列与别家共用
            // 同一套按毫秒的读法，存秒的结果是"1970 年到期"，账号一进面板就红着
            // 显示「已过期」。读取侧本来就有 `expires_at_ms()` 归一（两种都认），
            // 所以这里写毫秒不会把单位读反，只是让**落盘形状与别家一致**。
            ("expiresAt", Value::from(self.expires_at_ms())),
            ("domain", json_or_empty(&self.domain)),
            ("apiHost", json_or_empty(&self.api_host)),
            ("machineId", json_or_empty(&self.machine_id)),
            ("deviceId", json_or_empty(&self.device_id)),
            ("variant", json_or_empty(self.variant())),
        ];
        if !self.device_public_key.is_empty() {
            fields.push(("devicePublicKey", Value::String(self.device_public_key.clone())));
        }
        if !self.device_private_key.is_empty() {
            fields.push(("devicePrivateKey", Value::String(self.device_private_key.clone())));
        }
        // 只在知道的时候写：空串落盘会把 CPA 那边迁来的 `authClientId` 抹掉，
        // 而那个值一旦没了，续期就又只能按 variant 猜（猜错就是今天的 10101）
        if !self.auth_client_id.is_empty() {
            fields.push(("authClientId", Value::String(self.auth_client_id.clone())));
        }
        fields
    }
}

fn json_or_empty(value: &str) -> Value {
    Value::String(value.to_string())
}

/// `account` 段优先，取不到（或为空）再回落到 `auth` 段。
///
/// 参考实现把身份放在 `account{uid,nickname,enterpriseId}`，而手工粘贴的
/// 凭据常常是平铺的 —— 两个位置都读一次，才不会让"明明填了昵称却显示空"。
fn text_either(account: Option<&Value>, auth: &Value, key: &str) -> String {
    let from_account = account.map(|object| text_of(object, key)).unwrap_or_default();
    if from_account.is_empty() { text_of(auth, key) } else { from_account }
}

fn text_of(object: &Value, key: &str) -> String {
    object
        .get(key)
        .map(|value| match value {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// 取 JWT 的一个数字声明（不验签）。
fn jwt_claim(token: &str, claim: &str) -> Option<i64> {
    let mut parts = token.trim().split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload.trim_end_matches('='),
    )
    .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value.get(claim)?.as_f64().map(|number| number as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 造一个能解出 exp/iat 的假 JWT（第二段是 base64url 的 JSON，签名段随便填）。
    fn fake_jwt(exp_seconds: i64, iat_seconds: i64) -> String {
        use base64::Engine;
        let encode = |object: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(object.to_string().as_bytes())
        };
        format!(
            "{}.{}.sig",
            encode(&json!({"alg": "HS256"})),
            encode(&json!({"exp": exp_seconds, "iat": iat_seconds}))
        )
    }

    #[test]
    fn nested_and_flat_payloads_both_parse() {
        let nested = json!({"type":"trae","provider":"trae","auth":{"accessToken":"a","refreshToken":"r","expiresAt":1_800_000_000_000i64,"machineId":"m","deviceId":"d","variant":"solo"},"account":{"uid":"u1","nickname":"小明"}});
        let credential = Credential::from_payload(&nested).expect("嵌套形状要能读");
        assert_eq!("a", credential.access_token);
        assert_eq!("u1", credential.uid);
        assert_eq!("小明", credential.nickname);
        let flat = json!({"accessToken":"a","refreshToken":"r","uid":"u1"});
        assert_eq!("u1", Credential::from_payload(&flat).expect("平铺也要能读").uid);
    }

    #[test]
    fn a_payload_without_any_token_is_rejected_not_empty_accepted() {
        // 静默收下空凭据 = 账号"看着有数据却提示缺凭据"，最难查的那种。
        let error = Credential::from_payload(&json!({"uid":"u1"})).expect_err("两个令牌都没有要报错");
        assert!(error.contains("accessToken"), "文案要指出缺什么：{error}");
    }

    #[test]
    fn seconds_and_milliseconds_are_told_apart_by_magnitude() {
        assert_eq!(1_800_000_000_000, Credential { expires_at: 1_800_000_000, ..Default::default() }.expires_at_ms());
        assert_eq!(1_800_000_000_000, Credential { expires_at: 1_800_000_000_000, ..Default::default() }.expires_at_ms());
        assert_eq!(0, Credential::default().expires_at_ms(), "没给到期时刻就是 0，不是 1970");
    }

    #[test]
    fn jwt_claims_drive_the_expiry_and_the_age_rule() {
        let now_seconds = 1_780_000_000;
        let credential = Credential {
            access_token: fake_jwt(now_seconds + 3600, now_seconds - 20 * 86400),
            // 落盘字段故意给一个更晚的到期时刻：JWT 说话必须算数
            expires_at: (now_seconds + 90 * 86400) * 1000,
            ..Default::default()
        };
        assert_eq!((now_seconds + 3600) * 1000, credential.effective_expiry_ms());
        // 提前 24h 的临期窗口：还剩 1 小时 → 要续
        assert!(credential.needs_refresh(24 * 3600 * 1000, 15 * 86400 * 1000, now_seconds * 1000));
        // 一个刚签发、到期也远的令牌：两条规则都不该触发。
        let fresh = Credential { access_token: fake_jwt(now_seconds + 90 * 86400, now_seconds - 60), ..Default::default() };
        assert!(!fresh.needs_refresh(24 * 3600 * 1000, 0, now_seconds * 1000), "关掉签发龄规则时不该续");
        assert!(!fresh.needs_refresh(24 * 3600 * 1000, 15 * 86400 * 1000, now_seconds * 1000), "刚签发的令牌两条规则都不沾");
        // 签发龄超 15 天：临期还远，但服务端有吊销风险，要靠年龄触发。
        let aged = Credential { access_token: fake_jwt(now_seconds + 90 * 86400, now_seconds - 20 * 86400), ..Default::default() };
        assert!(aged.needs_refresh(24 * 3600 * 1000, 15 * 86400 * 1000, now_seconds * 1000), "签发龄超 15 天要续（服务端有吊销风险）");
        assert!(!aged.needs_refresh(24 * 3600 * 1000, 0, now_seconds * 1000), "max_age=0 是关掉这条规则，不是立刻续");
    }

    #[test]
    fn a_broken_token_still_leaves_the_stored_expiry() {
        for token in ["", "not-a-jwt", "a.b", "a.!!!.c"] {
            let credential = Credential { access_token: token.to_string(), expires_at: 1_800_000_000, ..Default::default() };
            assert_eq!(1_800_000_000_000, credential.effective_expiry_ms(), "{token:?} 解不出来时要退回落盘值");
        }
    }

    #[test]
    fn an_unknown_variant_falls_back_to_solo() {
        assert_eq!("solo", Credential { variant: String::new(), ..Default::default() }.variant());
        assert_eq!("solo", Credential { variant: "CN-unknown".into(), ..Default::default() }.variant());
        assert_eq!("cn", Credential { variant: "cn".into(), ..Default::default() }.variant());
    }

    #[test]
    fn the_patch_keeps_device_key_material_and_writes_nothing_extra() {
        let credential = Credential {
            access_token: "a".into(),
            refresh_token: "r".into(),
            device_public_key: "-----BEGIN PUBLIC KEY-----".into(),
            device_private_key: "-----BEGIN PRIVATE KEY-----".into(),
            ..Default::default()
        };
        let fields = credential.patch_fields();
        let keys: Vec<&str> = fields.iter().map(|(key, _)| *key).collect();
        assert!(keys.contains(&"devicePrivateKey"), "私钥不能从写回里消失");
        assert!(keys.contains(&"devicePublicKey"), "公钥也不能（它与服务端那台设备绑定同源）");
        // 手工粘贴的凭据没有密钥对 —— 这时这两把键**不该**出现在写回里
        // （把一个空串写进已有字段，等于把设备上那把键覆盖成空）。
        let bare = Credential { access_token: "a".into(), ..Default::default() }.patch_fields();
        let bare_keys: Vec<&str> = bare.iter().map(|(key, _)| *key).collect();
        assert!(!bare_keys.contains(&"devicePrivateKey") && !bare_keys.contains(&"devicePublicKey"), "{bare_keys:?}");
        assert!(!keys.contains(&"uid"), "uid 属于 account 段，不该被 auth 补丁覆盖");
        assert_eq!(
            Some(&Value::String("-----BEGIN PRIVATE KEY-----".into())),
            fields.iter().find(|(key, _)| *key == "devicePrivateKey").map(|(_, value)| value),
        );
    }

    /// OAuth 归属要能**读进来也能写回去**：今天这三条账号的病根就是迁移把它丢了
    /// （CPA 的 auth 文件里有 `authClientId`，落进本家库里后没人读，于是续期只能
    /// 按 variant 猜，猜出来的是 solo 那把、上游认的是 IDE 那把）。
    #[test]
    fn the_attribution_survives_the_payload_round_trip() {
        let nested = serde_json::json!({
            "type": "trae", "provider": "trae",
            "auth": {"accessToken": "a", "refreshToken": "r", "variant": "solo", "authClientId": "ono9krqynydwx5"},
            "account": {"uid": "731612159154631"},
        });
        let credential = Credential::from_payload(&nested).expect("嵌套形状该收得下");
        assert_eq!("ono9krqynydwx5", credential.auth_client_id, "CPA 迁来的 authClientId 必须读得回来");
        let fields = credential.patch_fields();
        assert!(
            fields.iter().any(|(key, value)| *key == "authClientId" && value == "ono9krqynydwx5"),
            "写回里也得带上：{fields:?}"
        );
        // 上游响应里那把叫 `ClientID`，手工粘贴可能给小写 —— 都算同一个字段
        for key in ["ClientID", "clientId"] {
            let payload = serde_json::json!({"auth": {"accessToken": "a", "refreshToken": "r", key: "ono9krqynydwx5"}});
            let parsed = Credential::from_payload(&payload).expect("两种写法都该认");
            assert_eq!("ono9krqynydwx5", parsed.auth_client_id, "{key}");
        }
        // 不知道的时候**不写这一格**：把空串落盘会抹掉凭据里已有的归属，
        // 下一次续期就又要从头猜（与 devicePrivateKey 同一规矩）
        let bare = Credential { access_token: "a".into(), ..Default::default() }.patch_fields();
        assert!(!bare.iter().any(|(key, _)| *key == "authClientId"), "{bare:?}");
    }

    /// 落盘的 `expiresAt` 必须是**毫秒**，不管进来的是秒还是毫秒。
    ///
    /// 这条锁的是面板那一列的读数：`1791009732`（秒）当毫秒读就是 1970-01-21，
    /// 账号一进列表就红着显示「已过期」，而它的令牌其实还有几天。
    #[test]
    fn the_patch_writes_the_expiry_in_milliseconds_whatever_comes_in() {
        let field = |credential: &Credential, want| {
            credential
                .patch_fields()
                .into_iter()
                .find(|(key, _)| *key == want)
                .map(|(_, value)| value)
                .unwrap()
        };
        let seconds = Credential { access_token: "a".into(), expires_at: 1_791_009_732, ..Default::default() };
        let millis = Credential { access_token: "a".into(), expires_at: 1_791_009_732_000, ..Default::default() };
        assert_eq!(Value::from(1_791_009_732_000i64), field(&seconds, "expiresAt"), "进来是秒也要写成毫秒");
        assert_eq!(Value::from(1_791_009_732_000i64), field(&millis, "expiresAt"), "进来已经是毫秒就别动它");
        // 没给到期时刻仍是 0（"未知"），不能被归一化捏成一个 1970 年的时间戳
        assert_eq!(Value::from(0i64), field(&Credential::default(), "expiresAt"));
    }
}
