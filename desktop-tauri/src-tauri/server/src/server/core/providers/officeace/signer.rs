//! OfficeAce（华为云果办 / OfficeClaw）的两套请求签名：**SDK-HMAC-SHA256** 与
//! **V11-HMAC-SHA256**。
//!
//! ── 为什么单独一份，不复用 `codearts::signer` ────────────────
//! 两家同属华为云、算法同名，但**规范化规则有三处实测差异**（依据是
//! `signer_vectors.json`：它由 `officeace2api` 的真实 `signV4`/`signV11` 产出，
//! 见该文件头的生成方式）。照抄 codearts 那份会在这三处算错、且上游只回 401：
//!
//!   1. **未保留集更严**：codearts（Go 侧 `unreserved`）把 `! ' ( ) *` 留在字面量里，
//!      而 OfficeAce 的 JS 签名器先 `encodeURIComponent`、再把这五个字符也转义
//!      （`quota.mjs` 的 `rfc3986`）。路径或查询里出现它们时两边签名不同。
//!   2. **查询串按 WHATWG 口径**：JS 走 `URL.searchParams`（键值先解码再重新编码、
//!      `+` 折成空格、按 `(键, 值)` 排序），不是 codearts 那份 Go `ParseQuery` 的
//!      「坏 `%` 丢一对」语义。
//!   3. **V11 的派生密钥按「小写十六进制字符串」参与 HMAC**，不是 32 字节原始 key。
//!      这一步两份 OfficeAce 实现（`officeace2api` 与 `opencode-officeace-auth`）
//!      写法一致，属于在这条链上必须复刻的既成事实（见本文件末的说明）。
//!
//! ── 算法 ────────────────────────────────────────────────────
//! ```text
//! canonicalRequest = METHOD \n canonicalURI \n canonicalQuery \n
//!                    canonicalHeaders \n signedHeaders \n payloadHash
//! SDK:  stringToSign = "SDK-HMAC-SHA256" \n X-Sdk-Date \n sha256hex(canonicalRequest)
//!       signature    = hmacSha256hex(secretKey, stringToSign)
//! V11:  scope        = <YYYYMMDD>/<region>/apic
//!       signingKey   = hmacSha256(hmacSha256(AK, SK), scope ++ 0x01)[..32] 的 **hex 串**
//!       stringToSign = "V11-HMAC-SHA256" \n X-Sdk-Date \n scope \n sha256hex(canonicalRequest)
//!       signature    = hmacSha256hex(signingKey_hex_string, stringToSign)
//! ```
//!
//! ── 与本仓 codearts 的那份的关系 ─────────────────────────────
//! 两处的 `canonicalURI` 都**强制补尾斜杠**、都按段解码再编码；差别只在未保留集
//! 与查询语义（上面 1、2）。`sha256hex` / `hmac` 这些与规则无关的部分两边一致。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const SDK_ALGORITHM: &str = "SDK-HMAC-SHA256";
const V11_ALGORITHM: &str = "V11-HMAC-SHA256";
/// APIG 的固定服务名（V11 的 CredentialScope 第三段）
const SCOPE_SERVICE: &str = "apic";
const HEADER_XSDK_DATE: &str = "X-Sdk-Date";
const HEADER_SECURITY_TOKEN: &str = "X-Security-Token";
const HEADER_PROJECT_ID: &str = "X-Project-ID";
const HEADER_AUTHORIZATION: &str = "Authorization";

/// `sha256("")`，无体请求固定签这个。
const EMPTY_BODY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 网关（模型面）默认区域。控制面是另一个 host，但区域同为 cn-southwest-2。
pub const DEFAULT_REGION: &str = "cn-southwest-2";

/// 一次登录/导入换来的凭据：临时 AK/SK/STS + 项目号。
///
/// `security_token` / `project_id` 为空时**完全不进**头集合（不是进空值），
/// 否则签名的头集合与上游收到的不一致 —— 这一条与 codearts 那份同一个口径。
#[derive(Clone, Debug, Default)]
pub struct Credential {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub security_token: String,
    pub project_id: String,
}

impl Credential {
    fn valid(&self) -> bool {
        !self.access_key_id.is_empty() && !self.secret_access_key.is_empty()
    }
}

/// OfficeAce 的未保留集：字母数字 + `- _ . ~`。
///
/// 与 codearts 那份的差别就在**这里**：`! ' ( ) *` 不再留在字面量（它们会被
/// 转义成 `%21 %27 %28 %29 %2A`）。
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// `encodeURIComponent` + 「把 `!'()*` 也转义」的百分号编码（`quota.mjs` 的 `rfc3986`）。
fn encode_component(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        if unreserved(*byte) {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// 把一个百分号序列解成一个字节（`%XX`），坏转义返回 None。
fn unhex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// JS `decodeURIComponent` 的近似：按 `%XX` 解字节；`+` 保持字面量。
///
/// 坏转义返回原串（JS 的 `decodeURIComponent` 会抛，但 `URL.pathname` 拿到的
/// 已经是合法转义，所以这条分支只是防御）。
fn decode_component(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (unhex(bytes[index + 1]), unhex(bytes[index + 2])) {
                out.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// 路径按段解码再编码，末尾**强制补斜杠**。
fn canonical_uri(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    let escaped = path
        .split('/')
        .map(|segment| encode_component(&decode_component(segment)))
        .collect::<Vec<_>>()
        .join("/");
    if escaped.ends_with('/') {
        escaped
    } else {
        format!("{escaped}/")
    }
}

/// 查询串按 WHATWG 口径：键值先按 `+`→空格、`%XX` 解码，再重新编码，按 `(键, 值)` 排序。
///
/// JS 那边是 `URL.searchParams`（内部已解码）+ `Array.prototype.sort` 的字符串序，
/// 这里用它等价的两趟排序。
fn canonical_query(raw_query: &str) -> String {
    if raw_query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = raw_query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = match part.split_once('=') {
                Some((key, value)) => (key, value),
                None => (part, ""),
            };
            (decode_form(key), decode_form(value))
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_component(key.as_bytes()),
                encode_component(value.as_bytes())
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// 表单分量的解码：`+` 折成空格，`%XX` 解字节（JS `URLSearchParams` 的语义）。
fn decode_form(input: &str) -> String {
    let bytes = decode_component(&input.replace('+', " "));
    String::from_utf8_lossy(&bytes).to_string()
}

/// 拆出 `scheme://host[:port]` 之后的 path 与 query（`#fragment` 丢掉，与 `new URL` 一致）。
fn split_path_query(url: &str) -> Result<(String, String), String> {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("地址缺少 scheme：{url}"))?;
    let rest = after_scheme;
    let without_fragment = rest.split('#').next().unwrap_or(rest);
    let (_, path_query) = match without_fragment.find('/') {
        Some(index) => without_fragment.split_at(index),
        None => (without_fragment, "/"),
    };
    let (path, query) = match path_query.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path_query, ""),
    };
    Ok((path.to_string(), query.to_string()))
}

/// `scheme://host[:port]`（`Host` 头要的值，不含 userinfo）。
fn authority_of(url: &str) -> Result<String, String> {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("地址缺少 scheme：{url}"))?;
    let host_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..host_end];
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    Ok(authority.to_string())
}

/// 头集合：大小写不敏感地设置（后到覆盖前值、位置不变），与 JS 的 `setHeader` 同口径。
#[derive(Default, Clone)]
struct Headers {
    items: Vec<(String, String)>,
}

impl Headers {
    fn set(&mut self, name: &str, value: &str) {
        let lower = name.to_ascii_lowercase();
        for (existing, slot) in self.items.iter_mut() {
            if existing.to_ascii_lowercase() == lower {
                *slot = value.to_string();
                return;
            }
        }
        self.items.push((name.to_string(), value.to_string()));
    }

    fn contains(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.items
            .iter()
            .any(|(existing, _)| existing.to_ascii_lowercase() == lower)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256_hex(key: &[u8], message: &str) -> String {
    hmac_sha256(key, message).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 签名算法选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    /// 标准华为云签名（AK 直接做密钥）。控制面的 `client-permission-validate` 用它。
    Sdk,
    /// APIG 区域作用域签名。额度 / 签到那几个 `/v1/subscription*` 用它。
    V11,
}

/// 签名所需的一切。
pub struct SignRequest<'a> {
    pub algorithm: Algorithm,
    pub method: &'a str,
    pub url: &'a str,
    /// 调用方已经要发的头（不含 `Authorization`）
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub credential: &'a Credential,
    pub region: &'a str,
    /// 固定 `X-Sdk-Date`（测试用；None 表示按当前 UTC 生成）
    pub date_override: Option<&'a str>,
}

/// 一次签名的全部中间量。除 `headers` 外目前只有测试读，端口出错时用来对账。
pub struct Material {
    pub headers: Vec<(String, String)>,
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signed_headers: String,
    pub signature: String,
}

/// `X-Sdk-Date`：UTC 的 `yyyyMMddTHHmmssZ`。
fn sdk_date_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    format_sdk_date(seconds)
}

fn format_sdk_date(seconds: u64) -> String {
    // 只做「秒 → UTC 日历」的换算，不引入 chrono（本仓签名层此前也只用手算日期）。
    let days = (seconds / 86_400) as i64;
    let secs_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Howard Hinnant 的 `civil_from_days`（epoch 起的天数 → 年月日）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn sign_material(request: &SignRequest<'_>) -> Result<Material, String> {
    if !request.credential.valid() {
        return Err("凭据不完整：access key 与 secret key 都得有".to_string());
    }
    let (path, raw_query) = split_path_query(request.url)?;
    let mut out = Headers::default();
    for (name, value) in request.headers {
        out.set(name, value);
    }
    if !request.credential.security_token.is_empty() {
        out.set(HEADER_SECURITY_TOKEN, &request.credential.security_token);
    }
    if !request.credential.project_id.is_empty() {
        out.set(HEADER_PROJECT_ID, &request.credential.project_id);
    }
    if let Some(date) = request.date_override {
        out.set(HEADER_XSDK_DATE, date);
    } else if !out.contains(HEADER_XSDK_DATE) {
        out.set(HEADER_XSDK_DATE, &sdk_date_now());
    }
    if !out.contains("host") {
        out.set("Host", &authority_of(request.url)?);
    }
    let date = out
        .items
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(HEADER_XSDK_DATE))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    // 规范头：名字转小写、值把内部连续空白折成一个空格（`normalizeHeaders` 的
    // `replace(/\s+/g, ' ')`，不是只 trim），按名字排序
    let mut canonical: Vec<(String, String)> = out
        .items
        .iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    canonical.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers = canonical
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let payload_hash = if request.body.is_empty() {
        EMPTY_BODY_SHA256.to_string()
    } else {
        sha256_hex(request.body)
    };

    let canonical_request = [
        request.method.to_ascii_uppercase(),
        canonical_uri(&path),
        canonical_query(&raw_query),
        canonical_headers.clone(),
        signed_headers.clone(),
        payload_hash,
    ]
    .join("\n");

    let (string_to_sign, signature, authorization) = match request.algorithm {
        Algorithm::Sdk => {
            let string_to_sign = [
                SDK_ALGORITHM,
                &date,
                &sha256_hex(canonical_request.as_bytes()),
            ]
            .join("\n");
            let signature =
                hmac_sha256_hex(request.credential.secret_access_key.as_bytes(), &string_to_sign);
            let authorization = format!(
                "{SDK_ALGORITHM} Access={}, SignedHeaders={signed_headers}, Signature={signature}",
                request.credential.access_key_id
            );
            (string_to_sign, signature, authorization)
        }
        Algorithm::V11 => {
            let scope = format!(
                "{}/{}/{}",
                &date[..date.len().min(8)],
                request.region,
                SCOPE_SERVICE
            );
            let string_to_sign = [
                V11_ALGORITHM,
                &date,
                &scope,
                &sha256_hex(canonical_request.as_bytes()),
            ]
            .join("\n");
            let step1 = hmac_sha256(
                request.credential.access_key_id.as_bytes(),
                &request.credential.secret_access_key,
            );
            let mut seed = scope.clone().into_bytes();
            seed.push(1);
            let derived = &hmac_sha256(&step1, &String::from_utf8_lossy(&seed))[..32];
            let signing_key_hex: String = derived.iter().map(|byte| format!("{byte:02x}")).collect();
            // ⚠️ 按**十六进制字符串**当密钥（不是那 32 个字节）—— 见文件头第 3 条
            let signature = hmac_sha256_hex(signing_key_hex.as_bytes(), &string_to_sign);
            let authorization = format!(
                "{V11_ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                request.credential.access_key_id
            );
            (string_to_sign, signature, authorization)
        }
    };

    out.set(HEADER_AUTHORIZATION, &authorization);
    Ok(Material {
        headers: out.items,
        canonical_request,
        string_to_sign,
        signed_headers,
        signature,
    })
}

/// 给请求签名，返回**应当发出去**的头集合（入参头 + 注入项 + `Authorization`）。
pub fn sign(request: &SignRequest<'_>) -> Result<Vec<(String, String)>, String> {
    Ok(sign_material(request)?.headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture() -> Value {
        let text = include_str!("signer_vectors.json");
        serde_json::from_str(text).expect("夹具是合法 JSON")
    }

    /// `Content-Type` 头名（只在测试里用来把夹具声明的 content_type 加进头集合）。
    const HEADER_CONTENT_TYPE: &str = "Content-Type";

    fn credential_of(fixture: &Value) -> Credential {
        let shape = &fixture["credential"];
        Credential {
            access_key_id: shape["accessKey"].as_str().unwrap().to_string(),
            secret_access_key: shape["secretKey"].as_str().unwrap().to_string(),
            security_token: shape["securityToken"].as_str().unwrap().to_string(),
            project_id: shape["projectId"].as_str().unwrap().to_string(),
        }
    }

    /// 逐用例把 Rust 实现算出的规范请求、待签串与签名，与 officeace2api 的
    /// 真实 `signV4`/`signV11` 产物逐字比对。
    #[test]
    fn matches_the_reference_implementation_byte_for_byte() {
        let fixture = fixture();
        let credential = credential_of(&fixture);
        let region = fixture["region"].as_str().unwrap();
        let mut checked = 0;
        for case in fixture["cases"].as_array().expect("cases 是数组") {
            let name = case["name"].as_str().unwrap();
            let algorithm = match case["algorithm"].as_str().unwrap() {
                "SDK" => Algorithm::Sdk,
                "V11" => Algorithm::V11,
                other => panic!("未知算法 {other}"),
            };
            let mut headers: Vec<(String, String)> = case["extra_headers"]
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(content_type) = case["content_type"].as_str() {
                headers.push((HEADER_CONTENT_TYPE.to_string(), content_type.to_string()));
            }
            let body = case["body"].as_str().unwrap();
            let material = sign_material(&SignRequest {
                algorithm,
                method: case["method"].as_str().unwrap(),
                url: case["url"].as_str().unwrap(),
                headers: &headers,
                body: body.as_bytes(),
                credential: &credential,
                region,
                date_override: Some(case["x_sdk_date"].as_str().unwrap()),
            })
            .unwrap_or_else(|error| panic!("{name}: {error}"));

            assert_eq!(
                case["canonical_request"].as_str().unwrap(),
                material.canonical_request,
                "{name}: 规范请求串不一致"
            );
            assert_eq!(
                case["string_to_sign"].as_str().unwrap(),
                material.string_to_sign,
                "{name}: 待签串不一致"
            );
            assert_eq!(
                case["signature"].as_str().unwrap(),
                material.signature,
                "{name}: 签名不一致"
            );
            let authorization = material
                .headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(HEADER_AUTHORIZATION))
                .map(|(_, value)| value.clone())
                .expect("签完必须带 Authorization");
            assert_eq!(
                case["authorization"].as_str().unwrap(),
                authorization,
                "{name}: Authorization 头不一致"
            );
            checked += 1;
        }
        assert_eq!(fixture["cases"].as_array().unwrap().len(), checked);
        assert!(checked >= 6, "夹具被削到只剩 {checked} 条");
    }

    /// 转义那一条的**反面**：按 codearts 的未保留集（`!'()*` 不转义）算，
    /// 签名必须与夹具不同 —— 否则说明这条用例区分不了两份实现，夹具就白加了。
    #[test]
    fn the_escaping_case_actually_distinguishes_the_two_escape_sets() {
        let fixture = fixture();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"].as_str().unwrap().contains("escaping"))
            .expect("夹具里要有转义敏感的用例");
        let url = case["url"].as_str().unwrap();
        // 用宽松的未保留集（codearts 口径）重算规范请求串的路径段
        let loose_keep = |byte: u8| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
        };
        let (path, _) = split_path_query(url).unwrap();
        let loose_path = path
            .split('/')
            .map(|segment| {
                let bytes = decode_component(segment);
                let mut out = String::new();
                for byte in bytes {
                    if loose_keep(byte) {
                        out.push(byte as char);
                    } else {
                        out.push_str(&format!("%{byte:02X}"));
                    }
                }
                out
            })
            .collect::<Vec<_>>()
            .join("/")
            + "/";
        let reference = case["canonical_request"].as_str().unwrap();
        assert!(
            !reference.contains(&loose_path),
            "夹具路径 {loose_path} 竟然与宽松口径相同，这条用例钉不住转义集"
        );
        assert!(
            reference.lines().nth(1).unwrap().contains("%21"),
            "夹具应当把 `!` 转义成 %21：{}",
            reference.lines().nth(1).unwrap()
        );
    }

    /// V11 的派生密钥取 hex 串当密钥；用原始 32 字节会算出另一个签名。
    /// 固定夹具里的那一条，证明我们复刻的是**hex 串**那一支。
    #[test]
    fn v11_uses_the_hex_string_as_the_hmac_key() {
        let fixture = fixture();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["algorithm"].as_str() == Some("V11"))
            .expect("夹具里要有 V11 用例");
        let signature = case["signature"].as_str().unwrap();
        // 用原始字节重算同一个待签串，签名必须不同（否则这条测试没有区分力）
        let credential = credential_of(&fixture);
        let region = fixture["region"].as_str().unwrap();
        let date = case["x_sdk_date"].as_str().unwrap();
        let scope = format!("{}/{region}/{SCOPE_SERVICE}", &date[..8]);
        let step1 = hmac_sha256(
            credential.access_key_id.as_bytes(),
            &credential.secret_access_key,
        );
        let mut seed = scope.into_bytes();
        seed.push(1);
        let derived = &hmac_sha256(&step1, &String::from_utf8_lossy(&seed))[..32];
        let bytes_variant = hmac_sha256_hex(derived, case["string_to_sign"].as_str().unwrap());
        assert_ne!(
            signature, bytes_variant,
            "两种密钥用法算出同一个签名 —— 这条用例没有区分力"
        );
    }

    /// 日期格式化与 JS 的 `toISOString().replace(...)` 同形。
    #[test]
    fn sdk_date_matches_the_js_shape() {
        // 2026-10-10T03:15:00Z（夹具里的固定 X-Sdk-Date）
        assert_eq!("20261010T031500Z", format_sdk_date(1_791_602_100));
        assert_eq!("19700101T000000Z", format_sdk_date(0));
        // 闰日：2024-02-29T00:00:00Z
        assert_eq!("20240229T000000Z", format_sdk_date(1_709_164_800));
    }
}
