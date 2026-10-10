use clap::Parser;

/// 无状态 Anthropic <-> Kiro API 代理服务
///
/// 凭证由每个请求的 header 提供，服务端不维护凭据池。
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 配置文件路径
    #[arg(short, long)]
    pub config: Option<String>,
}
