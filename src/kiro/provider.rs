//! 无状态 Kiro API Provider
//!
//! 与主项目 `KiroProvider` 的区别：
//! - 不持有凭据池（MultiTokenManager），每次调用由调用方传入 `KiroCredentials`
//! - 不做多凭据故障转移、失败计数、自愈、持久化
//! - 仅对上游瞬态错误（网络错误 / 408 / 429 / 5xx）做同凭证指数退避重试
//!
//! 设备指纹、代理、region 均由传入的凭据（叠加 header 覆盖后）现场派生。

use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;

use parking_lot::Mutex;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{IdeEndpoint, KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::refresher::{force_refresh_access_token, resolve_access_token};
use crate::model::config::{Config, TlsBackend};

/// 单凭证下瞬态错误的最大尝试次数
const MAX_TRANSIENT_RETRIES: usize = 3;

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
    /// TLS 后端配置
    tls_backend: TlsBackend,
}

impl StatelessProvider {
    /// 创建 StatelessProvider
    ///
    /// * `config` - 全局配置
    /// * `global_proxy` - 全局默认代理（凭据无自定义代理时的回退）
    pub fn new(config: Config, global_proxy: Option<ProxyConfig>) -> anyhow::Result<Self> {
        let tls_backend = config.tls_backend;
        // 预热：构建全局代理对应的 Client
        let initial_client = build_client(global_proxy.as_ref(), 720, tls_backend)?;
        let mut cache = HashMap::new();
        cache.insert(global_proxy.clone(), initial_client);

        Ok(Self {
            config,
            endpoint: IdeEndpoint::new(),
            global_proxy,
            client_cache: Mutex::new(cache),
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
        let token = resolve_access_token(credentials, &self.config, proxy.as_ref()).await?;
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
        anyhow::bail!("MCP 请求失败: {} {}", status, text)
    }

    /// 内部方法：带瞬态重试的 API 调用（单凭证）
    async fn call_api_inner(
        &self,
        credentials: &KiroCredentials,
        request_body: &str,
        is_stream: bool,
    ) -> anyhow::Result<reqwest::Response> {
        let api_type = if is_stream { "流式" } else { "非流式" };

        // Token 仅解析一次（未过期 accessToken 直接用，否则现场刷新）
        // 刷新失败多为凭据问题，直接返回，不重试
        let proxy = self.proxy_for(credentials);
        let mut token = resolve_access_token(credentials, &self.config, proxy.as_ref()).await?;
        let machine_id = machine_id::generate_from_credentials(credentials, &self.config);

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

            // 401/403 且上游明确提示 bearer token 失效：强制刷新一次并重试
            // （对齐主项目行为：本地 token 可能已被上游失效，需强刷兜底）
            if matches!(status.as_u16(), 401 | 403)
                && self.endpoint.is_bearer_token_invalid(&body_text)
                && !force_refreshed
            {
                force_refreshed = true;
                tracing::warn!("上游返回 bearer token 失效，强制刷新 Token 后重试");
                match force_refresh_access_token(credentials, &self.config, proxy.as_ref()).await {
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

            // 400/401/402/403 及其他 4xx：单凭证无从切换，直接返回错误
            anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body_text);
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
