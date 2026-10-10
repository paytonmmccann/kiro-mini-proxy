// 本服务由主项目精简复制而来，部分通用模块保留了暂未使用的能力
// （如凭据文件加载 CredentialsConfig、端点配额判断、配置回写 save 等），
// 允许 dead_code 以保持这些模块与主项目一致、便于后续同步与扩展。
#![allow(dead_code)]

mod anthropic;
mod common;
mod http_client;
mod kiro;
mod model;
pub mod token;

use clap::Parser;
use kiro::provider::StatelessProvider;
use model::arg::Args;
use model::config::Config;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 获取服务自身 API Key（用于客户端认证）
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });

    // 构建全局默认代理配置（可被凭据级 header 覆盖）
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });
    if let Some(url) = config.proxy_url.as_ref() {
        tracing::info!("已配置全局默认 HTTP 代理: {}", url);
    }

    // 创建无状态 Provider（不加载凭据文件、不建凭据池）
    let provider =
        StatelessProvider::new(config.clone(), proxy_config.clone()).unwrap_or_else(|e| {
            tracing::error!("创建 StatelessProvider 失败: {}", e);
            std::process::exit(1);
        });

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    // 构建 Anthropic API 路由
    let app = anthropic::create_router(&api_key, provider, config.extract_thinking);

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动无状态 Anthropic API 端点: {}", addr);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    tracing::info!("  POST /cc/v1/messages");
    tracing::info!("凭证通过 header 传入: x-kiro-credentials (base64 编码的 credentials JSON)");
    tracing::info!(
        "可选覆盖 header: x-kiro-machine-id / x-kiro-proxy-url / x-kiro-region / x-kiro-auth-region / x-kiro-api-region"
    );

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
