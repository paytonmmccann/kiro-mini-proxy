//! 从 HTTP header 解析并构造 `KiroCredentials`（无状态透传核心）
//!
//! - `x-kiro-credentials`: base64(完整 credentials JSON)
//! - 独立覆盖 header（优先级高于 credentials JSON 内字段）:
//!   - `x-kiro-machine-id`       → 设备指纹
//!   - `x-kiro-proxy-url`        → 代理地址（特殊值 `direct` 表示不走代理）
//!   - `x-kiro-proxy-username` / `x-kiro-proxy-password`
//!   - `x-kiro-region` / `x-kiro-auth-region` / `x-kiro-api-region`
//!
//! 最终优先级：独立 header > credentials JSON 字段 > config.json 默认。

use axum::http::HeaderMap;
use base64::Engine;

use crate::kiro::model::credentials::KiroCredentials;

/// 凭证 header 名称
pub const HEADER_CREDENTIALS: &str = "x-kiro-credentials";
pub const HEADER_MACHINE_ID: &str = "x-kiro-machine-id";
pub const HEADER_PROXY_URL: &str = "x-kiro-proxy-url";
pub const HEADER_PROXY_USERNAME: &str = "x-kiro-proxy-username";
pub const HEADER_PROXY_PASSWORD: &str = "x-kiro-proxy-password";
pub const HEADER_REGION: &str = "x-kiro-region";
pub const HEADER_AUTH_REGION: &str = "x-kiro-auth-region";
pub const HEADER_API_REGION: &str = "x-kiro-api-region";

/// 从 header 解析凭证并应用独立 header 覆盖
pub fn credentials_from_headers(headers: &HeaderMap) -> anyhow::Result<KiroCredentials> {
    let raw = headers
        .get(HEADER_CREDENTIALS)
        .ok_or_else(|| anyhow::anyhow!("缺少 {} header（应为 base64 编码的 credentials JSON）", HEADER_CREDENTIALS))?
        .to_str()
        .map_err(|_| anyhow::anyhow!("{} header 含非法字符", HEADER_CREDENTIALS))?;

    let json_bytes = decode_base64(raw.trim())
        .map_err(|e| anyhow::anyhow!("{} base64 解码失败: {}", HEADER_CREDENTIALS, e))?;

    let mut cred: KiroCredentials = serde_json::from_slice(&json_bytes)
        .map_err(|e| anyhow::anyhow!("credentials JSON 解析失败: {}", e))?;

    // 规范化 authMethod（builder-id/iam → idc，apikey → api_key）
    cred.canonicalize_auth_method();

    apply_header_overrides(&mut cred, headers);

    Ok(cred)
}

/// base64 解码，兼容标准 / URL-safe / 无 padding 变体
fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};

    STANDARD
        .decode(input)
        .or_else(|_| URL_SAFE.decode(input))
        .map_err(|e| e.to_string())
}

/// 应用独立 header 覆盖（仅在 header 存在且非空时覆盖）
fn apply_header_overrides(cred: &mut KiroCredentials, headers: &HeaderMap) {
    if let Some(v) = header_str(headers, HEADER_MACHINE_ID) {
        cred.machine_id = Some(v);
    }
    if let Some(v) = header_str(headers, HEADER_PROXY_URL) {
        cred.proxy_url = Some(v);
    }
    if let Some(v) = header_str(headers, HEADER_PROXY_USERNAME) {
        cred.proxy_username = Some(v);
    }
    if let Some(v) = header_str(headers, HEADER_PROXY_PASSWORD) {
        cred.proxy_password = Some(v);
    }
    if let Some(v) = header_str(headers, HEADER_REGION) {
        cred.region = Some(v);
    }
    if let Some(v) = header_str(headers, HEADER_AUTH_REGION) {
        cred.auth_region = Some(v);
    }
    if let Some(v) = header_str(headers, HEADER_API_REGION) {
        cred.api_region = Some(v);
    }
}

/// 读取字符串型 header，去除首尾空白，空值视为未提供
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn encode(json: &str) -> String {
        use base64::engine::general_purpose::STANDARD;
        STANDARD.encode(json)
    }

    #[test]
    fn test_parse_minimal_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CREDENTIALS,
            HeaderValue::from_str(&encode(r#"{"refreshToken":"abc","authMethod":"social"}"#)).unwrap(),
        );
        let cred = credentials_from_headers(&headers).unwrap();
        assert_eq!(cred.refresh_token.as_deref(), Some("abc"));
        assert_eq!(cred.auth_method.as_deref(), Some("social"));
    }

    #[test]
    fn test_header_overrides_take_priority() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CREDENTIALS,
            HeaderValue::from_str(&encode(
                r#"{"refreshToken":"abc","machineId":"old","apiRegion":"us-east-1"}"#,
            ))
            .unwrap(),
        );
        headers.insert(HEADER_MACHINE_ID, HeaderValue::from_static("new-machine"));
        headers.insert(HEADER_API_REGION, HeaderValue::from_static("us-west-2"));
        headers.insert(HEADER_PROXY_URL, HeaderValue::from_static("direct"));

        let cred = credentials_from_headers(&headers).unwrap();
        assert_eq!(cred.machine_id.as_deref(), Some("new-machine"));
        assert_eq!(cred.api_region.as_deref(), Some("us-west-2"));
        assert_eq!(cred.proxy_url.as_deref(), Some("direct"));
    }

    #[test]
    fn test_missing_credentials_header() {
        let headers = HeaderMap::new();
        assert!(credentials_from_headers(&headers).is_err());
    }

    #[test]
    fn test_auth_method_canonicalized() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CREDENTIALS,
            HeaderValue::from_str(&encode(r#"{"refreshToken":"abc","authMethod":"builder-id"}"#)).unwrap(),
        );
        let cred = credentials_from_headers(&headers).unwrap();
        assert_eq!(cred.auth_method.as_deref(), Some("idc"));
    }

    #[test]
    fn test_expires_at_as_number() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CREDENTIALS,
            HeaderValue::from_str(&encode(r#"{"refreshToken":"abc","authMethod":"social","expiresAt":0}"#)).unwrap(),
        );
        let cred = credentials_from_headers(&headers).unwrap();
        assert_eq!(cred.expires_at.as_deref(), Some("0"));
    }

    #[test]
    fn test_expires_at_as_string() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CREDENTIALS,
            HeaderValue::from_str(&encode(r#"{"refreshToken":"abc","authMethod":"social","expiresAt":"2026-12-31T02:32:45.144Z"}"#)).unwrap(),
        );
        let cred = credentials_from_headers(&headers).unwrap();
        assert_eq!(cred.expires_at.as_deref(), Some("2026-12-31T02:32:45.144Z"));
    }
}
