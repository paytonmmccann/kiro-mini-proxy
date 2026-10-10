//! 无状态 Kiro API Provider（带进程内 Token 缓存）
//!
//! 与主项目 `KiroProvider` 的区别：
//! - 不持有凭据池（MultiTokenManager），每次调用由调用方传入 `KiroCredentials`
//! - 不做多凭据故障转移、失败计数、自愈、持久化
//! - 对同一凭证的 accessToken 做进程内缓存，避免每次请求都刷新
//! - 仅对上游瞬态错误（网络错误 / 408 / 429 / 5xx）做同凭证指数退避重试
//! - 上游 401/403「bearer token 失效」时强制刷新并更新缓存后重试一次
//! - 上游明确状态码通过 `UpstreamError` 如实反馈给客户端
//!
//! 设备指纹、代理、region 均由传入的凭据（叠加 header 覆盖后）现场派生。

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{IdeEndpoint, KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::refresher::{refresh_token_with_expiry, resolve_access_token};
use crate::model::config::{Config, TlsBackend};

/// 单凭证下瞬态错误的最大尝试次数
const MAX_TRANSIENT_RETRIES: usize = 3;

/// 缓存 token 的兜底有效期（秒），当刷新响应未给出 expiresAt 时使用
const FALLBACK_TOKEN_TTL_SECS: u64 = 50 * 60;

/// 提前判定 token 过期的余量（分钟）
const EXPIRY_MARGIN_MINUTES: i64 = 5;

/// 上游返回的明确错误（携带状态码，用于向客户端如实反馈）
#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub body: String,
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "上游返回 {}: {}", self.status, self.body)
    }
}

impl std::error::Error for UpstreamError {}

/// 缓存的 Token 条目
struct CachedToken {
    access_token: String,
    /// 刷新得到的过期时间（RFC3339）
    expires_at: Option<String>,
    /// 写入缓存的时刻（兜底 TTL 用）
    cached_at: Instant,
}

impl CachedToken {
    /// 是否仍然有效（提前 EXPIRY_MARGIN_MINUTES 视为过期）
    fn is_valid(&self) -> bool {
        if let Some(ea) = &self.expires_at {
            if let Ok(t) = DateTime::parse_from_rfc3339(ea) {
                return t > Utc::now() + chrono::Duration::minutes(EXPIRY_MARGIN_MINUTES);
            }
        }
        // expiresAt 缺失或不可解析时，用兜底 TTL
        self.cached_at.elapsed() < Duration::from_secs(FALLBACK_TOKEN_TTL_SECS)
    }
}

/// 无状态 Kiro API Provider
pub struct StatelessProvider {
    /// 全局配置（提供 region / 版本 / TLS 等默认伪装参数）
    config: Config,
    /// 端点实现（第一版固定为 ide）
    endpoint: IdeEndpoint,
    /// 全局默认代理（来自 config），凭据级代理优先
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config，共享相同代理的调用复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// Token 缓存：key = 凭证指纹（sha256(refreshToken)），value = 最新 accessToken
    token_cache: Mutex<HashMap<String, CachedToken>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
}

impl StatelessProvider {
    /// 创建 StatelessProvider
    pub fn new(config: Config, global_proxy: Option<ProxyConfig>) -> anyhow::Result<Self> {
        let tls_backend = config.tls_backend;
        let initial_client = build_client(global_proxy.as_ref(), 720, tls_backend)?;
        let mut cache = HashMap::new();
        cache.insert(global_proxy.clone(), initial_client);

        Ok(Self {
            config,
            endpoint: IdeEndpoint::new(),
            global_proxy,
            client_cache: Mutex::new(cache),
            token_cache: Mutex::new(HashMap::new()),
            tls_backend,
        })
    }

    /// 获取全局配置引用
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 解析当前凭据的有效代理配置
    fn proxy_for(&self, credentials: &KiroCredentials) -> Option<ProxyConfig> {
        credentials.effective_proxy(self.global_proxy.as_ref())
    }

    /// 计算凭证的缓存指纹：基于 refreshToken（无则基于 accessToken）
    ///
    /// 用稳定标识做 key，避免 accessToken/expiresAt 变化导致缓存 miss。
    fn token_cache_key(credentials: &KiroCredentials) -> Option<String> {
        let seed = credentials
            .refresh_token
            .as_deref()
            .or(credentials.access_token.as_deref())?;
        let mut h = Sha256::new();
        h.update(b"kiro-mini-proxy/token-cache/");
        h.update(seed.as_bytes());
        Some(hex::encode(h.finalize()))
    }

    /// 读取仍然有效的缓存 token
    fn cache_get(&self, key: &str) -> Option<String> {
        let cache = self.token_cache.lock();
        cache
            .get(key)
            .filter(|e| e.is_valid())
            .map(|e| e.access_token.clone())
    }

    /// 刷新 token 并写入缓存，返回新 token
    async fn refresh_and_cache(
        &self,
        credentials: &KiroCredentials,
        proxy: Option<&ProxyConfig>,
        key: Option<&str>,
    ) -> anyhow::Result<String> {
        let (token, expires_at) =
            refresh_token_with_expiry(credentials, &self.config, proxy).await?;
        if let Some(k) = key {
            let mut cache = self.token_cache.lock();
            cache.insert(
                k.to_string(),
                CachedToken {
                    access_token: token.clone(),
                    expires_at,
                    cached_at: Instant::now(),
                },
            );
        }
        Ok(token)
    }

    /// 获取可用 token：
    /// - API Key 凭据直接用（不缓存、不刷新）
    /// - 否则缓存优先；miss/过期则用 refreshToken 刷新并缓存
    async fn acquire_token(
        &self,
        credentials: &KiroCredentials,
        proxy: Option<&ProxyConfig>,
    ) -> anyhow::Result<String> {
        if credentials.is_api_key_credential() {
            return credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"));
        }

        let key = Self::token_cache_key(credentials);

        // 命中有效缓存：直接用，跳过刷新
        if let Some(k) = &key {
            if let Some(tok) = self.cache_get(k) {
                tracing::debug!("Token 缓存命中 ({}…)", &k[..8.min(k.len())]);
                return Ok(tok);
            }
        }

        // 无 refreshToken：无法刷新，回退用传入凭证解析（可能直接用其 accessToken）
        if credentials.refresh_token.is_none() {
            return resolve_access_token(credentials, &self.config, proxy).await;
        }

        // 缓存 miss：刷新并缓存
        self.refresh_and_cache(credentials, proxy, key.as_deref()).await
    }

    /// 发送非流式 API 请求
    pub async fn call_api(
        &self,
        credentials: &KiroCredentials,
        request_body: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.call_api_inner(credentials, request_body, false).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        credentials: &KiroCredentials,
        request_body: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.call_api_inner(credentials, request_body, true).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    ///
    /// 不做重试：WebSearch 失败在上层会降级为无搜索结果。
    pub async fn call_mcp(
        &self,
        credentials: &KiroCredentials,
        request_body: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let proxy = self.proxy_for(credentials);
        let token = self.acquire_token(credentials, proxy.as_ref()).await?;
        let machine_id = machine_id::generate_from_credentials(credentials, &self.config);

        let rctx = RequestContext {
            credentials,
            token: &token,
            machine_id: &machine_id,
            config: &self.config,
        };

        let url = self.endpoint.mcp_url(&rctx);
        let body = self.endpoint.transform_mcp_body(request_body, &rctx);

        let base = self
            .client_for(credentials)?
            .post(&url)
            .body(body)
            .header("content-type", "application/json")
            .header("Connection", "close");
        let request = self.endpoint.decorate_mcp(base, &rctx);

        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        Err(UpstreamError {
            status: status.as_u16(),
            body: text,
        }
        .into())
    }

    /// 内部方法：带缓存 token + 瞬态重试 + bearer 失效强刷的 API 调用
    async fn call_api_inner(
        &self,
        credentials: &KiroCredentials,
        request_body: &str,
        is_stream: bool,
    ) -> anyhow::Result<reqwest::Response> {
        let api_type = if is_stream { "流式" } else { "非流式" };

        let proxy = self.proxy_for(credentials);
        let machine_id = machine_id::generate_from_credentials(credentials, &self.config);
        let cache_key = Self::token_cache_key(credentials);

        // 获取 token（缓存优先，miss 则刷新并缓存）
        let mut token = self.acquire_token(credentials, proxy.as_ref()).await?;

        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed = false;

        for attempt in 0..MAX_TRANSIENT_RETRIES {
            let rctx = RequestContext {
                credentials,
                token: &token,
                machine_id: &machine_id,
                config: &self.config,
            };

            let url = self.endpoint.api_url(&rctx);
            let body = self.endpoint.transform_api_body(request_body, &rctx);

            let base = self
                .client_for(credentials)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json")
                .header("Connection", "close");
            let request = self.endpoint.decorate_api(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    // 网络错误通常是链路瞬态问题，重试
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        MAX_TRANSIENT_RETRIES,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < MAX_TRANSIENT_RETRIES {
                        sleep(retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }

            let body_text = response.text().await.unwrap_or_default();

            // 401/403 且上游明确提示 bearer token 失效：强制刷新（更新缓存）后重试一次
            if matches!(status.as_u16(), 401 | 403)
                && self.endpoint.is_bearer_token_invalid(&body_text)
                && !force_refreshed
                && credentials.refresh_token.is_some()
            {
                force_refreshed = true;
                tracing::warn!("上游 bearer token 失效，强制刷新并更新缓存后重试");
                match self
                    .refresh_and_cache(credentials, proxy.as_ref(), cache_key.as_deref())
                    .await
                {
                    Ok(new_token) => {
                        token = new_token;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("强制刷新 Token 失败: {}", e);
                    }
                }
            }

            // 408/429/5xx：上游瞬态错误，同凭证重试
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    MAX_TRANSIENT_RETRIES,
                    status,
                    body_text
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body_text
                ));
                if attempt + 1 < MAX_TRANSIENT_RETRIES {
                    sleep(retry_delay(attempt)).await;
                }
                continue;
            }

            // 其余（含刷新后仍 401/403、400/402 等）：如实返回上游状态码
            return Err(UpstreamError {
                status: status.as_u16(),
                body: body_text,
            }
            .into());
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                MAX_TRANSIENT_RETRIES
            )
        }))
    }
}

/// 指数退避 + 少量抖动
fn retry_delay(attempt: usize) -> Duration {
    const BASE_MS: u64 = 200;
    const MAX_MS: u64 = 2_000;
    let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
    let backoff = exp.min(MAX_MS);
    let jitter_max = (backoff / 4).max(1);
    let jitter = fastrand::u64(0..=jitter_max);
    Duration::from_millis(backoff.saturating_add(jitter))
}
