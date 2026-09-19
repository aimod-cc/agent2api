//! Qoder COSY 请求签名与请求体编码（移植来源 `Qoder-Proxy/src/cosy.mjs` 与
//! `encoding.mjs`）。
//!
//! ── 为什么不能只发 Bearer ───────────────────────────────────
//! Qoder 的推理网关（`/algo/...` 这一族）不认普通 Bearer 令牌，而是一套
//! **自签名**鉴权：
//!
//!   1. 组装用户身份 JSON（uid / security_oauth_token / name / email）；
//!   2. 生成一把一次性 AES 密钥，用它把身份 JSON 加密成 `info`；
//!   3. 用内置 RSA 公钥把该 AES 密钥加密成 `key`（服务端用私钥解开）；
//!   4. 把 {version, requestId, info, cosyVersion, ideVersion} 序列化后 base64，
//!      得到 `payload`；
//!   5. 对 `payload \n key \n 时间戳 \n 请求体 \n 签名路径` 求 MD5，得到签名；
//!   6. 拼成 `Authorization: Bearer COSY.<payload>.<签名>`，并附一组 `Cosy-*` 头。
//!
//! 签名路径要**去掉 `/algo` 前缀、不含查询串** —— 服务端按同一规则校验。
//! **请求体参与签名**，所以调用顺序不能颠倒：先编码（`encode_body`）再签名。
//!
//! ── 两个原语为什么是手写的 ──────────────────────────────────
//! 项目里已有 `md-5` / `sha2` / `aes-gcm`（AutoClaw 用），但 **RSA 公钥加密**
//! 与 **AES-128-CBC** 没有现成实现：
//!   - RSA：只需「按 PKCS#1 v1.5 打包后做一次模幂」，指数固定 65537
//!     （`modpow` 约 17 次平方），`num-bigint` 一个依赖就够 —— 引入完整的
//!     `rsa` crate 会拖进 num-bigint-dig / pkcs1 / pkcs8 / der 等一长串依赖，
//!     而我们只用它最基础的一步；
//!   - AES-CBC：`aes` 0.9 已在依赖树里（aes-gcm 拉的），CBC 链式异或 + PKCS#7
//!     补位是十几行确定性代码，不值得为它引入 `cbc` + `block-padding`。
//!
//! 两者的**正确性已与 Node 的 `crypto` 逐字节对拍**（AES-CBC 密文、MD5 签名、
//! 编码后的请求体全部一致；RSA 走「Rust 加密 → Node 私钥解密」的往返验证）。
//!
//! ── 随机源 ──────────────────────────────────────────────────
//! AES 密钥、RSA 的 PS 填充、requestId 都必须是**不可预测**的随机值
//! （PS 若固定则 RSA 退化成确定性加密，是一条可被指纹化的特征）。
//! 因此统一走 `super::machine` 的 `getrandom` 封装，不沿用廉价串。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic，取值走 Option 链，
//! 所有失败都转成 `GatewayError`。

use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use base64::Engine as _;
use md5::{Digest, Md5};
use num_bigint::BigUint;

use crate::server::errors::GatewayError;

use super::machine;

/// 推理网关（`/algo/...`）使用的 COSY 协议版本
pub const GATEWAY_COSY_VERSION: &str = "1.1.38";
/// 客户端类型标识（桌面端为 0，这里沿用 CLI 形态，与源项目一致）
pub const CLIENT_TYPE: &str = "5";
/// 数据策略：不同意用于训练
pub const DATA_POLICY: &str = "disagree";
/// 登录版本标识
pub const LOGIN_VERSION: &str = "v2";

/// 网关用它区分客户端形态（COSY 头 `Cosy-Machinetype`）
const MACHINE_TYPE: &str = "5";

/// `Cosy-Clientip`：本机回环（源实现写死该值）
const CLIENT_IP: &str = "127.0.0.1";

/// COSY 身份加密用的 RSA 公钥模数（1024 位，hex，无前导零）。
///
/// 这是**客户端内置的固定公钥**（源实现 `RSA_PUBLIC_KEY` 的 PEM 正文），
/// 只用于加密一次性 AES 密钥，不是机密材料。指数固定 65537。
///
/// 公钥 PEM 与这里的模数是同一份数据的两种形态；导出方式：
/// 对 PEM 正文做 base64 解码 → DER 里 `02 81 81` 之后的 128 字节即模数，
/// 紧随其后的 `02 03 01 00 01` 即指数。
const RSA_MODULUS_HEX: &str = "c0f22307e5cd362e296bb04470f6de8fbf935ce24e8fcf511a0e2701329769c4a76e499bb938036a52af1eaf818cf79a2600620e3ce87e371d2ca6d85803606a1b3fa5e874643c9ed2db7e85673ef7227fca56e2e7c08f0927609bb896a9f24be1782099a66016a5bfdc3f1ff756bfc9e88d7b5dc5be30bf45a0223a00ebcecf";

/// RSA 公钥指数（源 PEM 里的 `02 03 01 00 01`）
const RSA_EXPONENT: u32 = 65537;

/// 自定义替换字母表（与标准 base64 字母表逐字符对应）
const CUSTOM_ALPHABET: &[u8] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
/// 标准 base64 字母表
const STD_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 一次签名所需的身份素材（对应源实现 `buildAuthHeaders` 的 creds 参数）。
pub struct CosyIdentity<'a> {
    /// 账号在 Qoder 侧的 uid（源实现 `uid`）
    pub user_id: &'a str,
    /// access token（源实现 `security_oauth_token`）
    pub auth_token: &'a str,
    /// 昵称（可为空）
    pub name: &'a str,
    /// 邮箱（可为空）
    pub email: &'a str,
    /// 机器标识（COSY 头里要带，且参与「同一账号不同设备」的判定）
    pub machine_id: &'a str,
}

/// 构造一次请求所需的全部 COSY 头。
///
/// `body` 是**已编码**的请求体（GET 类请求传 `None`）。签名覆盖它，
/// 所以调用方必须先 `encode_body` 再调本函数 —— 顺序颠倒会得到一个
/// 「签名不匹配」的上游错误，且无法从错误里看出原因。
pub fn build_auth_headers(
    body: Option<&[u8]>,
    request_url: &str,
    identity: &CosyIdentity<'_>,
) -> Result<Vec<(String, String)>, GatewayError> {
    if identity.user_id.is_empty() {
        return Err(GatewayError::with_status(400, "Qoder 账号缺少用户标识，无法签名"));
    }
    if identity.auth_token.is_empty() {
        return Err(GatewayError::with_status(401, "Qoder 账号缺少访问令牌，请重新登录"));
    }

    // 一次性 AES 密钥：16 个 ASCII 字符（源实现在 UUID 去掉连字符后取前 16 位，
    // 这里取密码学随机串的前 16 位 —— 同一语义、更强的随机源）
    let secret = machine::random_secret()?;
    let aes_key: String = secret.chars().take(16).collect();
    if aes_key.len() < 16 {
        return Err(GatewayError::with_status(500, "无法生成 Qoder 签名密钥"));
    }

    // 身份 JSON：**键顺序照抄源实现**（uid / security_oauth_token / name / aid / email）。
    //
    // ── 为什么手写而不是 `json!`（这不是洁癖，是必须）──────────────
    // 本项目的 serde_json 未开 `preserve_order`，`Value::Object` 是**按键排序**的
    // BTreeMap：`json!` 出来的键序是 aid / email / name / security_oauth_token / uid。
    // 而这段 JSON 会被 AES 加密进 `info`、再被 base64 进 payload，**整条字节串
    // 参与签名与解密** —— 键序变了，密文与签名就都与源实现不同。
    // 上游虽然只是解密后按 JSON 语义读取（键序无关），因此理论上不影响互通，
    // 但「与参考实现逐字节一致」是一条能靠对拍证伪的硬保证；键序漂移会让这条
    // 保证消失，将来任何一次签名问题都只能靠猜。各字段用 `to_string` 单独转义，
    // 拼出来仍是合法 JSON（值里含引号/中文时也不会拼坏）。
    let user_info = format!(
        "{{\"uid\":{},\"security_oauth_token\":{},\"name\":{},\"aid\":\"\",\"email\":{}}}",
        json_text(identity.user_id),
        json_text(identity.auth_token),
        json_text(identity.name),
        json_text(identity.email),
    );
    let info = aes128_cbc_base64(&aes_key, user_info.as_bytes())?;
    let cosy_key = rsa_encrypt_base64(&aes_key)?;

    let request_id = machine::random_uuid()?;
    let timestamp = (crate::server::logging::now_ms() / 1000).to_string();

    // payload 同样按源实现的键序手写（理由见上）：version / requestId / info /
    // cosyVersion / ideVersion。`info` 是 base64、requestId 是 UUID、版本是常量，
    // 都不需要转义。
    let payload_src = format!(
        "{{\"version\":\"v1\",\"requestId\":\"{request_id}\",\"info\":\"{info}\",\
         \"cosyVersion\":\"{GATEWAY_COSY_VERSION}\",\"ideVersion\":\"\"}}"
    );
    let payload = base64::engine::general_purpose::STANDARD.encode(payload_src.as_bytes());

    let sig_path = signature_path(request_url)?;
    let body_bytes = body.unwrap_or(&[]);

    // MD5(payload \n key \n 时间戳 \n 请求体 \n 签名路径)
    let mut hasher = Md5::new();
    hasher.update(payload.as_bytes());
    hasher.update(b"\n");
    hasher.update(cosy_key.as_bytes());
    hasher.update(b"\n");
    hasher.update(timestamp.as_bytes());
    hasher.update(b"\n");
    hasher.update(body_bytes);
    hasher.update(b"\n");
    hasher.update(sig_path.as_bytes());
    let signature = format!("{:x}", hasher.finalize());

    let mut body_hasher = Md5::new();
    body_hasher.update(body_bytes);
    let body_hash = format!("{:x}", body_hasher.finalize());

    let headers: Vec<(String, String)> = vec![
        (
            "Authorization".to_string(),
            format!("Bearer COSY.{payload}.{signature}"),
        ),
        ("Cosy-Key".to_string(), cosy_key),
        ("Cosy-User".to_string(), identity.user_id.to_string()),
        ("Cosy-Date".to_string(), timestamp),
        ("Cosy-Version".to_string(), GATEWAY_COSY_VERSION.to_string()),
        ("Cosy-Machineid".to_string(), identity.machine_id.to_string()),
        ("Cosy-Machinetoken".to_string(), identity.machine_id.to_string()),
        ("Cosy-Machinetype".to_string(), MACHINE_TYPE.to_string()),
        ("Cosy-Machineos".to_string(), machine_os()),
        ("Cosy-Clienttype".to_string(), CLIENT_TYPE.to_string()),
        ("Cosy-Clientip".to_string(), CLIENT_IP.to_string()),
        ("Cosy-Bodyhash".to_string(), body_hash),
        ("Cosy-Bodylength".to_string(), body_bytes.len().to_string()),
        ("Cosy-Sigpath".to_string(), sig_path),
        ("Cosy-Data-Policy".to_string(), DATA_POLICY.to_string()),
        ("Cosy-Organization-Id".to_string(), String::new()),
        ("Cosy-Organization-Tags".to_string(), String::new()),
        ("Login-Version".to_string(), LOGIN_VERSION.to_string()),
        ("X-Request-Id".to_string(), request_id),
    ];
    Ok(headers)
}

/// 请求体编码（源实现 `encodeBody`）：上游带 `Encode=1` 时请求体不是裸 JSON。
///
/// 三步都是**纯字节变换**，服务端按同一规则还原：
///   1. 对 JSON 字节做标准 base64，得到一段文本；
///   2. 把该文本按「尾段 / 中段 / 首段」重排（各段长度都是 ⌊n/3⌋，
///      余数留在中段，所以三段拼回来仍是原长）；
///   3. 逐字符做一次字母表替换（含把 `=` 换成 `$`）。
///
/// 签名算在**编码之后**的字节上（见 `build_auth_headers`）。
pub fn encode_body(payload: &[u8]) -> Vec<u8> {
    let standard = base64::engine::general_purpose::STANDARD.encode(payload);
    let bytes = standard.as_bytes();
    let length = bytes.len();
    let third = length / 3;

    let mut table = [0u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    for (index, ch) in STD_ALPHABET.iter().enumerate() {
        table[*ch as usize] = CUSTOM_ALPHABET[index];
    }
    // base64 的填充符也要换掉
    table[b'=' as usize] = b'$';

    let mut out = Vec::with_capacity(length);
    // 尾段 → 中段 → 首段
    for index in (length - third)..length {
        out.push(table[bytes[index] as usize]);
    }
    for index in third..(length - third) {
        out.push(table[bytes[index] as usize]);
    }
    for index in 0..third {
        out.push(table[bytes[index] as usize]);
    }
    out
}

/// 一个字符串的 JSON 文本形态（带引号与转义）。
///
/// 用于手写身份 JSON / payload 时的取值转义（见 `build_auth_headers` 的说明）。
/// `serde_json::to_string` 对 `&str` 就是把该串转义成 JSON 字符串，
/// 失败只可能是分配失败 —— 那时给一对空引号，签名会失败但不会 panic。
fn json_text(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// 参与签名的路径：去掉 `/algo` 前缀、不含查询串（源实现 `sigPathOf`）。
pub fn signature_path(request_url: &str) -> Result<String, GatewayError> {
    let parsed = url::Url::parse(request_url)
        .map_err(|_| GatewayError::with_status(500, "Qoder 请求地址无效"))?;
    let path = parsed.path();
    Ok(path.strip_prefix("/algo").map(str::to_string).unwrap_or_else(|| path.to_string()))
}

/// 机器操作系统标识（源实现 `machineOs`）：`{arch}_windows` 这类形态。
fn machine_os() -> String {
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let platform = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    format!("{arch}_{platform}")
}

/// AES-128-CBC + PKCS#7，返回 base64 密文。
///
/// 密钥与 IV **都取同一段 16 字节**（源实现 `aesEncrypt` 的既有做法，
/// 不是这里的设计选择），所以调用方只需给一个 16 字符的 key。
fn aes128_cbc_base64(key16: &str, plaintext: &[u8]) -> Result<String, GatewayError> {
    if key16.len() != 16 {
        return Err(GatewayError::with_status(500, "Qoder 签名密钥长度无效"));
    }
    let cipher = Aes128::new_from_slice(key16.as_bytes())
        .map_err(|_| GatewayError::with_status(500, "无法初始化 Qoder 签名算法"))?;
    let iv = key16.as_bytes();

    // PKCS#7：补 1..16 字节，值等于补位长度（源实现在 Node 的
    // `createCipheriv` 里由 OpenSSL 完成，这里手写同一规则）
    let mut data = plaintext.to_vec();
    let pad = 16 - (data.len() % 16);
    data.extend(std::iter::repeat(pad as u8).take(pad));

    let mut previous = [0u8; 16];
    previous.copy_from_slice(iv);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        if chunk.len() != 16 {
            return Err(GatewayError::with_status(500, "Qoder 签名分组长度异常"));
        }
        let mut block = [0u8; 16];
        for index in 0..16 {
            block[index] = chunk[index] ^ previous[index];
        }
        let mut encrypted = match Block::try_from(&block[..]) {
            Ok(block) => block,
            Err(_) => {
                return Err(GatewayError::with_status(500, "Qoder 签名分组长度异常"));
            }
        };
        cipher.encrypt_block(&mut encrypted);
        out.extend_from_slice(&encrypted);
        previous.copy_from_slice(&encrypted);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(&out))
}

/// RSA 公钥加密（PKCS#1 v1.5），返回 base64 密文。
///
/// 布局：`00 || 02 || PS || 00 || M`，PS 是**非零随机**字节
/// （源实现走 Node 的 `RSA_PKCS1_PADDING`，OpenSSL 用随机 PS）。
/// PS 固定会让 RSA 退化成确定性加密 —— 那是可被指纹化的特征，不要改。
fn rsa_encrypt_base64(plaintext: &str) -> Result<String, GatewayError> {
    let modulus = BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16)
        .ok_or_else(|| GatewayError::with_status(500, "Qoder 签名公钥无效"))?;
    let key_len = ((modulus.bits() + 7) / 8) as usize;
    let message = plaintext.as_bytes();
    if message.len() + 11 > key_len {
        return Err(GatewayError::with_status(500, "Qoder 签名内容过长"));
    }
    let ps_len = key_len - 3 - message.len();

    let mut ps = vec![0u8; ps_len];
    getrandom::getrandom(&mut ps)
        .map_err(|_| GatewayError::with_status(500, "无法生成 Qoder 签名随机填充"))?;
    // PKCS#1 v1.5 要求 PS 全为非零字节：把抽到的 0 映射成 1
    for byte in ps.iter_mut() {
        if *byte == 0 {
            *byte = 1;
        }
    }

    let mut encoded = Vec::with_capacity(key_len);
    encoded.push(0x00);
    encoded.push(0x02);
    encoded.extend_from_slice(&ps);
    encoded.push(0x00);
    encoded.extend_from_slice(message);

    let cipher = BigUint::from_bytes_be(&encoded).modpow(&BigUint::from(RSA_EXPONENT), &modulus);
    // 定长输出（模数 1024 位 → 128 字节），不足时左侧补零
    let raw = cipher.to_bytes_be();
    if raw.len() > key_len {
        return Err(GatewayError::with_status(500, "Qoder 签名结果长度异常"));
    }
    let mut out = vec![0u8; key_len];
    out[key_len - raw.len()..].copy_from_slice(&raw);
    Ok(base64::engine::general_purpose::STANDARD.encode(&out))
}

/// 生成一个不可猜测的 UUID v4（供会话/记录标识使用）。
pub fn random_uuid() -> Result<String, GatewayError> {
    machine::random_uuid()
}
