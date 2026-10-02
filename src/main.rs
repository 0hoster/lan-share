mod api;
mod assets;
mod error;
mod model;
mod net;
mod state;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(
    name = "lan-share",
    version,
    about = "局域网文件互传服务：启动后在浏览器中互相上传/下载文件",
    long_about = None
)]
struct Cli {
    /// 监听端口
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// 监听地址，默认 0.0.0.0（局域网内其他设备可访问）
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// 文件保存目录
    #[arg(short, long, default_value = "lan-share-data")]
    data_dir: PathBuf,

    /// 分片大小（MiB），越大顺序写越高效，越小并行粒度越细
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,

    /// 开启访问令牌（留空自动生成随机令牌）
    #[arg(long, num_args = 0..=1, default_missing_value = "auto")]
    token: Option<String>,

    /// 打印更详细的日志
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("lan_share=debug,tower_http=debug,info")
    } else {
        EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "lan_share=info,warn".to_string()),
        )
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let chunk_size = cli.chunk_mib.clamp(1, 64) * 1024 * 1024;
    let token = match cli.token.as_deref() {
        Some("auto") | Some("") => {
            Some(uuid::Uuid::new_v4().simple().to_string()[..12].to_string())
        }
        Some(value) => Some(value.to_string()),
        None => None,
    };

    let data_dir = cli.data_dir.clone();
    let state = Arc::new(
        AppState::new(&data_dir, chunk_size, token.clone())
            .with_context(|| format!("初始化数据目录失败: {}", data_dir.display()))?,
    );
    api::spawn_janitor(state.clone());

    let app = api::router(state.clone())
        .layer(CorsLayer::very_permissive())
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port)
        .parse()
        .with_context(|| format!("监听地址不合法: {}:{}", cli.host, cli.port))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("端口绑定失败: {addr}，可能已被占用"))?;
    let local_addr = listener.local_addr()?;

    print_banner(
        local_addr,
        &data_dir,
        chunk_size,
        token.as_deref(),
        state.files.len(),
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP 服务异常退出")?;

    tracing::info!("服务已停止");
    Ok(())
}

fn print_banner(
    addr: SocketAddr,
    data_dir: &std::path::Path,
    chunk_size: u64,
    token: Option<&str>,
    files: usize,
) {
    let token_suffix = token.map(|t| format!("?token={t}")).unwrap_or_default();
    let port = addr.port();

    println!();
    println!("  \x1b[1mlan-share\x1b[0m · 局域网文件互传");
    println!("  ─────────────────────────────────────────────");
    println!("  本机访问    http://127.0.0.1:{port}/{token_suffix}");
    if let Some(ip) = net::primary_local_ip() {
        println!(
            "  局域网访问  \x1b[36mhttp://{ip}:{port}/{token_suffix}"
        );
    } else {
        println!("  局域网访问  http://<本机IP>:{port}/{token_suffix}");
    }
    println!("  数据目录    {}", data_dir.display());
    println!("  分片大小    {} MiB", chunk_size / 1024 / 1024);
    println!("  已有文件    {files} 个");
    if let Some(token) = token {
        println!("  访问令牌    \x1b[33m{token}\x1b[0m（访问其他设备时需带上该令牌）");
    }
    println!("  按 Ctrl+C 停止服务");
    println!();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("无法监听 Ctrl+C 信号");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法监听 SIGTERM 信号")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!();
    tracing::info!("收到退出信号，正在关闭服务……");
}
