//! PKCE 随机串与本机标识。随机源失败时终止登录，不降级为可预测值。

use std::path::PathBuf;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use crate::server::errors::GatewayError;

static MACHINE_ID: Mutex<Option<String>> = Mutex::new(None);

pub fn random_secret() -> Result<String, GatewayError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| GatewayError::with_status(500, "无法生成安全的 Qoder 登录随机串"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn random_uuid() -> Result<String, GatewayError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| GatewayError::with_status(500, "无法生成 Qoder 机器标识"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

fn read_id(path: &std::path::Path) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then_some(value)
}

pub fn machine_id() -> Result<String, GatewayError> {
    let mut cached = MACHINE_ID.lock()
        .map_err(|_| GatewayError::with_status(500, "Qoder 机器标识锁不可用"))?;
    if let Some(value) = cached.as_ref() {
        return Ok(value.clone());
    }
    let directory = crate::server::config::config_dir();
    let path = directory.join("qoder-machine-id");
    if let Some(value) = read_id(&path) {
        *cached = Some(value.clone());
        return Ok(value);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    let existing = home.map(PathBuf::from).and_then(|home| {
        [
            home.join(".qoder-proxy/machine_id"),
            home.join(".qoder/.auth/machine_id"),
            home.join(".qoder/machine_id"),
        ].iter().find_map(|candidate| read_id(candidate))
    });
    let value = match existing {
        Some(value) => value,
        None => random_uuid()?,
    };
    std::fs::create_dir_all(&directory)
        .and_then(|_| std::fs::write(&path, &value))
        .map_err(|error| GatewayError::with_status(500, format!("保存 Qoder 机器标识失败: {error}")))?;
    *cached = Some(value.clone());
    Ok(value)
}
