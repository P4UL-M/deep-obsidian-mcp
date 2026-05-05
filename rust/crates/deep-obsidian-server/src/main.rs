use std::path::PathBuf;

use clap::Parser;
use deep_obsidian_config::normalize_service_config;
use deep_obsidian_types::{HttpConfigInput, ServiceConfigInput, TransportMode};
use tracing_subscriber::EnvFilter;

use deep_obsidian_server::bootstrap::run_http_service;

#[derive(Debug, Parser)]
#[command(author, version, about = "deep-obsidian-server prototype")]
struct Args {
    #[arg(long)]
    vault: PathBuf,
    #[arg(long)]
    index_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 4100)]
    port: u16,
    #[arg(long, default_value = "/mcp")]
    mcp_path: String,
    #[arg(long, default_value = "/healthz")]
    health_path: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let input = ServiceConfigInput {
        vault_path: Some(args.vault),
        index_dir: args.index_dir,
        transport: Some(TransportMode::Http),
        stdio_mode: None,
        http: Some(HttpConfigInput {
            host: Some(args.host),
            port: Some(args.port),
            mcp_path: Some(args.mcp_path),
            health_path: Some(args.health_path),
        }),
        auto_reindex: None,
        embedding: None,
        config_file_path: None,
    };

    let resolved = normalize_service_config(input)?;
    let mut bootstrap = run_http_service(resolved).await?;
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal?;
        }
        server_result = &mut bootstrap.server_handle => {
            match server_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error.into()),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}
