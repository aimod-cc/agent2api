//! AutoClaw 桌面端 Electron safeStorage 凭证解密（Agent2API 二期 T-c1）。
//!
//! 移植来源：`D:\APP\AutoClaw\autoclaw-local-proxy\crypto-helper.mjs`
//! （**唯一事实来源**；本文件逐步骤对照它实现，切分口径不得自行发挥）。
//!
//! ── 解密链（Windows 专属）─────────────────────────────────
//! AutoClaw 是 Electron 应用，把 token 用 `safeStorage` 加密后以 `enc:` 前缀
//! 写进 `%APPDATA%/AutoClaw/auth.json`。Windows 上 Electron 的 safeStorage 就是
//! Chromium 的 os_crypt 方案，共两步：
//!
//! **第一步：取 AES 密钥（DPAPI）**
//! `%APPDATA%/AutoClaw/Local State` 是 Chromium 的明文 JSON 配置文件，其中
//! `os_crypt.encrypted_key` 是 `base64(DPAPI(密钥))`。base64 解码后的字节流
//! 前 5 字节是 ASCII `DPAPI`（Chromium 的标记，源实现 PowerShell 里那句
//! `Select-Object -Skip 5` 就是跳过它），去掉之后剩下的才是真正的 DPAPI blob，
//! `CryptUnprotectData` 解出 **32 字节 AES-256 密钥**。
//!
//! 源实现走 PowerShell 子进程（`[ProtectedData]::Unprotect`，先试 pwsh.exe
//! 再退回 powershell.exe）；这里**直接调 Win32 的 CryptUnprotectData**：
//! 少一个子进程、少一次 PowerShell 启动，也不再依赖用户机器上装没装 pwsh。
//! 本项目的 Win32 直调约定与理由见 `Cargo.toml` 的 windows-sys 注释
//! （曾经因为 spawn powershell 被杀软启发式拦杀）。
//!
//! **第二步：解密字段（AES-256-GCM）**
//! auth.json 里 `token` / `refreshToken` 的值形如 `enc:<base64>`。base64 解码后
//! 的字节流（**去原实现核对过的确切切分**）：
//!
//! ```text
//! [0..3)     "v10"           ← os_crypt 版本前缀（3 字节 ASCII）
//! [3..15)    nonce           ← 12 字节
//! [15..n)    密文            ← AES-256-GCM ciphertext
//! [n..n+16)  tag             ← 16 字节认证标签，**附在密文尾部**
//! ```
//!
//! 注意源实现 `decryptEncValue` 的真实取法：它先 `payload = blob.slice(3)`，再
//! `nonce = payload.subarray(0, 12)`、`tag = payload.subarray(len - 16)`（取**尾部**
//! 而非紧跟 nonce 的下一段）、`cipherText = payload.subarray(12, len - 16)`。
//! 这正是 Electron 的「Postfix tag」拼法，与 aes-gcm 的默认
//! `TAG_POSITION = Postfix` 语义一致，所以这里把「密文 + tag」整段交给
//! `Aead::decrypt` 即可，**不需要**自己把 tag 切出来。
//! 长度守卫也照抄：`payload.len() <= 12 + 16` 直接判为异常（源实现的
//! `payload.length <= 12 + TAG_LENGTH`，注意是 `<=`，恰好等于 28 也算异常）。
//!
//! 明文是 token 本体（**不是** JSON）——本机实测两个字段解出来都带
//! `Bearer ` 前缀，去前缀由调用方负责（`credentials.rs` 的 `normalize_token`）。
//!
//! ── 为什么非 `enc:` 的值原样返回 ────────────────────────────
//! 源实现 `decryptEncValue` 对不以 `enc:` 开头的值直接返回（兼容明文字段）；
//! AutoClaw 桌面端历史版本/手工改过的 auth.json 可能是明文，照抄这个宽容行为，
//! 免得「明明是明文却报解密失败」。
//!
//! ── 平台约束与桩实现 ────────────────────────────────────────
//! DPAPI（`CryptUnprotectData`）只有 Windows 有：本项目只发 Windows 包，但代码
//! 必须能在非 Windows 上**编译通过**（CI / 交叉编译安全）。因此 win32 调用收在
//! `#[cfg(windows)]` 的 `dpapi` 子模块里，非 Windows 是一份返回中文错误的桩
//! （对齐源实现 `process.platform !== 'win32'` 时抛
//! 「DPAPI 解密仅支持 Windows 平台」的行为）。
//!
//! ── panic=abort：全程 Result ────────────────────────────────
//! release 是 panic=abort（见 Cargo.toml 的 profile），一条被截断的 base64 /
//! 一个换过用户的 Local State 都会让解密失败。本文件**不 unwrap / 不 expect /
//! 不索引越界**：所有切片都先做长度判断，AES-GCM 的 nonce 用 `try_from` 转换，
//! 每一步失败都返回带中文说明的错误。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;

/// os_crypt 密文的版本前缀（源实现 `blob.subarray(0, 3).toString('utf8')` 比对值）
const OS_CRYPT_VERSION: &str = "v10";

/// `enc:` 字段前缀（源实现 `value.startsWith('enc:')` / `value.slice(4)`）
const ENC_PREFIX: &str = "enc:";

/// DPAPI blob 前的 Chromium 标记长度（源实现 `Select-Object -Skip 5`）
const DPAPI_PREFIX_LEN: usize = 5;

/// nonce 长度（GCM 标准 96 bit）
const NONCE_LEN: usize = 12;

/// 认证标签长度（GCM 默认 128 bit）
const TAG_LEN: usize = 16;

/// AES-256 密钥长度（DPAPI 解出来必须正好这么长）
const AES_KEY_LEN: usize = 32;

/// base64 引擎：标准字母表，解码宽容度对齐 Node 的 `Buffer.from(x, 'base64')`。
///
/// Chromium 与 Electron 写的都是规范填充的标准 base64（源实现还用
/// `/^[A-Za-z0-9+/]+={0,2}$/` 做过一次形态校验），但 `Buffer.from` 本身允许
/// 填充缺失与非零尾随位。这里用 `Indifferent` + `allow_trailing_bits` 复刻那份
/// 宽容：紧一点只会让我们把手能解开的数据判成失败，没有安全收益
/// （数据来自本机文件，不是不可信输入；真正的完整性由 GCM 的 tag 保证）。
const OS_CRYPT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

// ─── AES 密钥（Local State + DPAPI）────────────────────────────

/// 进程级密钥缓存：`(Local State 路径, 32 字节 AES 密钥)`。
///
/// 与源实现同一策略（`cachedAesKey` + `cachedAesKeySource`）：DPAPI 调用不便宜，
/// 而密钥在一个登录会话里是稳定的，因此**按路径缓存、进程内复用**。
/// 缓存只有一格（源实现也是单槽）：AutoClaw 的用户数据目录只有一个。
fn aes_key_cache() -> &'static Mutex<Option<(String, Vec<u8>)>> {
    static CACHE: OnceLock<Mutex<Option<(String, Vec<u8>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 取 `Local State` 里的 os_crypt AES 密钥（DPAPI 解密，带进程级缓存）。
///
/// 调用链：读 JSON → `os_crypt.encrypted_key`（base64）→ 去 5 字节 `DPAPI` 前缀
/// → `CryptUnprotectData` → 校验长度 32。任一环节失败返回中文错误。
///
/// **同步函数**：没有 await 点，调用它的 async 链路不会因此持有非 Send 状态
/// （对照 `raccoon/credentials.rs` 模块头那条 Send 硬约束）。
pub fn os_crypt_aes_key(local_state_path: &Path) -> Result<Vec<u8>, String> {
    let cache_key = local_state_path.to_string_lossy().to_string();
    if let Ok(guard) = aes_key_cache().lock() {
        if let Some((cached_path, key)) = guard.as_ref() {
            if *cached_path == cache_key {
                return Ok(key.clone());
            }
        }
    }
    let key = load_os_crypt_aes_key(local_state_path)?;
    if let Ok(mut guard) = aes_key_cache().lock() {
        *guard = Some((cache_key, key.clone()));
    }
    Ok(key)
}

/// 读 Local State 并做一次 DPAPI 解密（不带缓存，供 `os_crypt_aes_key` 调用）
fn load_os_crypt_aes_key(local_state_path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(local_state_path).map_err(|error| {
        format!(
            "无法读取 AutoClaw Local State（{}）：{error}",
            local_state_path.display()
        )
    })?;
    let value = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|_| "AutoClaw Local State 无法解析（不是有效 JSON）".to_string())?;
    let encoded = value
        .get("os_crypt")
        .and_then(|os_crypt| os_crypt.get("encrypted_key"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    if encoded.is_empty() {
        return Err("AutoClaw Local State 里没有 os_crypt.encrypted_key".to_string());
    }
    // 形态校验照抄源实现（`/^[A-Za-z0-9+/]+={0,2}$/`）：明显不是 base64 的值
    // 不必进 DPAPI，先给一个更清楚的错误
    if !is_base64_like(encoded) {
        return Err("AutoClaw Local State 的 os_crypt.encrypted_key 不是合法 base64".to_string());
    }
    let blob = OS_CRYPT_BASE64
        .decode(encoded)
        .map_err(|_| "AutoClaw Local State 的 os_crypt.encrypted_key 解码失败".to_string())?;
    if blob.len() <= DPAPI_PREFIX_LEN {
        return Err("AutoClaw os_crypt 密钥 blob 长度异常（不足以去掉 DPAPI 前缀）".to_string());
    }
    if &blob[..DPAPI_PREFIX_LEN] != b"DPAPI" {
        return Err(
            "AutoClaw os_crypt 密钥前缀不是 DPAPI（这不是 Windows 平台的 Local State）"
                .to_string(),
        );
    }
    let key = dpapi::unprotect(&blob[DPAPI_PREFIX_LEN..])?;
    if key.len() != AES_KEY_LEN {
        return Err(format!(
            "AutoClaw os_crypt 密钥长度异常（期望 {AES_KEY_LEN} 字节，实际 {} 字节）",
            key.len()
        ));
    }
    Ok(key)
}

/// 源实现那条 base64 形态校验（`^[A-Za-z0-9+/]+={0,2}$`）：只判字符集与
/// 「填充只在尾部且最多两个」，不判长度是否为 4 的倍数（`Buffer.from` 也宽容）
fn is_base64_like(value: &str) -> bool {
    let mut padding = 0usize;
    let mut seen_padding = false;
    let mut body = 0usize;
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => {
                if seen_padding {
                    return false;
                }
                body += 1;
            }
            b'=' => {
                seen_padding = true;
                padding += 1;
                if padding > 2 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    body > 0
}

// ─── `enc:` 字段解密 ─────────────────────────────────────────

/// 解密 AutoClaw `enc:` 前缀的 safeStorage 字段，返回 UTF-8 明文。
///
/// 与源实现 `decryptEncValue(value, aesKey)` 逐条对齐：
///   - 不以 `enc:` 开头 → **原样返回**（兼容明文字段，见模块头）；
///   - 有 `enc:` 但没有可用密钥 → 报错（源实现同一文案）；
///   - base64 解码 → 校验 3 字节版本前缀是 `v10`（否则报出不支持的版本）；
///   - 长度必须 `> 12 + 16`（**严格大于**，`<=` 判异常）；
///   - nonce = 12 字节，tag = 尾部 16 字节，密文 = 中间段 → AES-256-GCM 解密；
///   - 解密失败统一给「AES-GCM 解密失败（密钥或数据不匹配）」。
///
/// 明文按 UTF-8 解析（源实现 `.toString('utf8')`）；非法 UTF-8 报错而不是
/// 替换字符 —— 替换字符会让调用方拿着一段看似正常的垃圾 token 去请求上游。
pub fn decrypt_enc_value(value: &str, aes_key: Option<&[u8]>) -> Result<String, String> {
    let Some(encoded) = value.strip_prefix(ENC_PREFIX) else {
        // 兼容明文字段：不是 enc: 就原样返回
        return Ok(value.to_string());
    };
    let Some(key) = aes_key else {
        return Err("没有可用的 os_crypt 密钥，无法解密 enc: 字段".to_string());
    };
    let blob = OS_CRYPT_BASE64
        .decode(encoded.trim())
        .map_err(|_| "AutoClaw 加密字段不是合法 base64".to_string())?;
    if blob.len() < 3 {
        return Err("AutoClaw 加密字段长度异常（不足版本前缀）".to_string());
    }
    let prefix = String::from_utf8_lossy(&blob[..3]).to_string();
    if prefix != OS_CRYPT_VERSION {
        return Err(format!("不支持的 os_crypt 密文版本: {prefix}"));
    }
    let payload = &blob[3..];
    if payload.len() <= NONCE_LEN + TAG_LEN {
        return Err("os_crypt 密文长度异常".to_string());
    }
    let nonce = &payload[..NONCE_LEN];
    // 密文 + 尾部 tag 整段交给 aes-gcm（Postfix tag，见模块头）
    let body = &payload[NONCE_LEN..];
    let plaintext = aes_gcm_decrypt(key, nonce, body)?;
    String::from_utf8(plaintext).map_err(|_| "解密结果不是合法 UTF-8 文本".to_string())
}

/// AES-256-GCM 解密（无 AAD，tag 附在密文尾部）。
///
/// 全程无 panic：密钥长度由 `new_from_slice` 校验，nonce 由 `try_from` 转换
/// （都是 Result），解密失败由 GCM 的 tag 校验给出 Error。
fn aes_gcm_decrypt(key: &[u8], nonce: &[u8], ciphertext_with_tag: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::consts::U12;
    use aes_gcm::aead::Aead as _;
    use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce};

    if key.len() != AES_KEY_LEN {
        return Err(format!(
            "os_crypt 密钥长度异常（期望 {AES_KEY_LEN} 字节，实际 {} 字节）",
            key.len()
        ));
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "os_crypt 密钥长度不受 AES-256 支持".to_string())?;
    let nonce = Nonce::<U12>::try_from(nonce)
        .map_err(|_| format!("os_crypt nonce 长度异常（期望 {NONCE_LEN} 字节）"))?;
    cipher
        .decrypt(&nonce, ciphertext_with_tag)
        .map_err(|_| "AES-GCM 解密失败（密钥或数据不匹配）".to_string())
}

// ─── JWT 声明解码 ────────────────────────────────────────────

/// 解析 JWT payload，返回 claims；解析失败返回 None。**不校验签名**。
///
/// 源实现 `decodeJwtClaims`（也在 crypto-helper.mjs 里，与解密同源，故一并放在
/// 本文件）：去 `Bearer ` 前缀 → 按 `.` 切段 → 第二段按 base64url 规则还原成
/// 标准 base64（`-`→`+`、`_`→`/`，并补 `=`）→ 解析 JSON → 根必须是对象。
///
/// 网关只用它读 `exp` / `user_id` / `device_id`：token 好不好用由上游说了算，
/// 验签没有意义。解析失败一律 None（panic=abort，畸形 token 不能带走进程）。
pub fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let raw = strip_bearer(token);
    let payload = raw.split('.').nth(1)?;
    if payload.is_empty() {
        return None;
    }
    let mut normalized: String = payload
        .chars()
        .map(|ch| match ch {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let remainder = normalized.len() % 4;
    if remainder != 0 {
        normalized.push_str(&"=".repeat(4 - remainder));
    }
    let bytes = OS_CRYPT_BASE64.decode(&normalized).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let claims = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    claims.is_object().then_some(claims)
}

/// 去 `Bearer ` 前缀（大小写不敏感，至少一个空白）。
///
/// 与源实现 `String(x).replace(/^Bearer\s+/i, '').trim()` 对齐：本机实测
/// auth.json 解出来的 token 明文**自带** `Bearer ` 前缀，所以这一步是必须的，
/// 否则拼出来的 Authorization 头会变成 `Bearer Bearer eyJ...`。
pub fn strip_bearer(value: &str) -> String {
    let trimmed = value.trim();
    // 按**字节**比对是安全的：`bearer` 全是 ASCII，命中时前 6 字节必然是
    // 完整字符边界；非 ASCII 开头的值不会命中这个分支
    let bytes = trimmed.as_bytes();
    if bytes.len() > b"bearer".len() && bytes[..b"bearer".len()].eq_ignore_ascii_case(b"bearer") {
        let rest = &trimmed[b"bearer".len()..];
        if rest.starts_with(char::is_whitespace) {
            return rest.trim_start().to_string();
        }
    }
    trimmed.to_string()
}

// ─── Windows DPAPI ──────────────────────────────────────────

/// DPAPI 的 `CryptUnprotectData` 包装（仅 Windows 编译）。
///
/// 用 `windows` crate（架构文档 §10.1 指定的依赖）：`windows-sys` 是扁平句柄 +
/// 手写 `GetLastError`，而这里要的是「一次调用 + 结构化错误 + 释放 DPAPI
/// 分配的内存」。`windows` 的绑定把 `BOOL` 返回值直接包成
/// `windows_core::Result<()>`（失败时自动 `Error::from_win32()`），
/// 输出的 `CRYPT_INTEGER_BLOB.pbData` 又是带 Drop 语义的 `HLOCAL`
/// （底层 `LocalFree`），不必手写释放，也**不会漏**。
/// 两者都是 Windows 专属依赖，只开 `Win32_Security_Cryptography` 一个叶子
/// feature（见 Cargo.toml）。
///
/// `dwFlags` 传 0，与源实现
/// `[ProtectedData]::Unprotect(blob, $null, 'CurrentUser')` 的语义一致
/// （.NET 内部同样是 flags=0；作用域由 `CurrentUser` 决定 = 不传
/// CRYPTPROTECT_LOCAL_MACHINE）。
#[cfg(windows)]
mod dpapi {
    use windows::core::Owned;
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    /// 解开一段 DPAPI blob（当前用户作用域）并返回明文字节
    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        if blob.is_empty() {
            return Err("DPAPI blob 为空".to_string());
        }
        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY：input 指向的切片在本次调用期间存活；output 是出参，由 DPAPI
        // 填充；其余四个可选参数按 API 约定传空。返回的 `Result` 由 windows crate
        // 从 `GetLastError()` 构造（错误码进 message），失败时 output 不被填充。
        unsafe { CryptUnprotectData(&input, None, None, None, None, 0, &mut output) }.map_err(
            |error| {
                format!(
                    "DPAPI 解密 os_crypt 密钥失败: {error}（Local State 属于另一个 Windows 用户时不通用）"
                )
            },
        )?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err("DPAPI 解密结果为空".to_string());
        }
        // 用 `Owned<HLOCAL>` 接管这块 DPAPI 分配的内存：它的 Drop 会调
        // `HLOCAL` 的 `Free::free()`（底层 LocalFree）。**不能**直接对 HLOCAL
        // 调 `drop()` —— 它是 Copy 类型，drop 一个副本什么也不会释放（内存泄漏）。
        // SAFETY：成功返回的 pbData 是调用方拥有的 LocalAlloc 内存，且这里是它
        // 唯一的接管点（下面按引用读，不复制句柄）。
        let owned = unsafe { Owned::new(HLOCAL(output.pbData as *mut _)) };
        // SAFETY：pbData 指向 cbData 个有效字节（DPAPI 成功返回的约定）；
        // `owned` 在本函数结束前存活，所以这块内存在拷贝期间一直有效。
        let plaintext =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        drop(owned);
        if plaintext.is_empty() {
            return Err("DPAPI 解密结果为空".to_string());
        }
        Ok(plaintext)
    }
}

/// 非 Windows 桩：DPAPI 是 Windows 独有的（`CryptUnprotectData` 只在 crypt32.dll）。
///
/// 文案对齐源实现：`process.platform !== 'win32'` 时
/// `CryptoHelperError('DPAPI 解密仅支持 Windows 平台')`。
/// 这里保留同一句话，让「换了平台跑」与「解密失败」在日志里能区分开。
/// 本模块的其余部分（base64 / AES-GCM / JWT）在非 Windows 上照常可用，
/// 所以只有这一个函数需要分平台 —— `enc:` 明文字段与 openclaw.json 那两条
/// 来源在非 Windows 上仍能工作。
#[cfg(not(windows))]
mod dpapi {
    pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI 解密仅支持 Windows 平台".to_string())
    }
}
