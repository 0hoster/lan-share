mod api;
mod assets;
mod config;
mod dashboard;
mod error;
mod live;
mod model;
mod net;
mod state;
mod tls;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(
    name = "lan-share",
    version,
    about = "局域网文件互传 + 直播录屏服务：启动后用浏览器互传文件、共享屏幕",
    long_about = None
)]
struct Cli {
    /// 监听端口（默认 8080）
    #[arg(short, long)]
    port: Option<u16>,

    /// 监听地址，默认 0.0.0.0（局域网内其他设备可访问）
    #[arg(long)]
    host: Option<String>,

    /// 文件保存目录（默认 lan-share-data）
    #[arg(short, long)]
    data_dir: Option<PathBuf>,

    /// 分片大小（MiB，默认 8），越大顺序写越高效，越小并行粒度越细
    #[arg(long)]
    chunk_mib: Option<u64>,

    /// 开启访问令牌（留空自动生成随机令牌）
    #[arg(long, num_args = 0..=1, default_missing_value = "auto")]
    token: Option<String>,

    /// 同时进行的直播上限，0 表示不限制（默认 8）
    #[arg(long)]
    live_max_rooms: Option<usize>,

    /// 单场直播的观众上限，0 表示不限制（默认 32）
    #[arg(long)]
    live_max_viewers: Option<usize>,

    /// 主播多久没有数据就自动结束直播（秒，默认 90）
    #[arg(long)]
    live_idle_secs: Option<u64>,

    /// 配置文件路径（默认 ./.env，不存在则忽略）
    #[arg(long, default_value = config::DEFAULT_ENV_FILE)]
    env_file: PathBuf,

    /// 不读取 .env 配置文件
    #[arg(long)]
    no_env_file: bool,

    /// 关闭终端实时统计面板
    #[arg(long)]
    no_dashboard: bool,

    /// 启用 HTTPS（手机浏览器调用摄像头必须；未指定端口时默认 8443）
    #[arg(long)]
    tls: bool,

    /// 使用自有证书（需与 --tls-key 一起给，缺省则自动生成自签证书）
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// 使用自有私钥
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// 不打印启动横幅
    #[arg(long)]
    quiet: bool,

    /// 打印更详细的日志
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 1) 先读 .env（已存在的环境变量优先，不会被覆盖）
    let mut env_entries = 0usize;
    let env_file = cli.env_file.clone();
    let env_loaded = !cli.no_env_file && env_file.exists();
    if env_loaded {
        env_entries = config::load_env_file(&env_file)
            .with_context(|| format!("加载配置文件失败: {}", env_file.display()))?;
    }
    let env = config::EnvConfig::from_env();

    // 2) 优先级：命令行 > .env / 环境变量 > 默认值
    let host = cli
        .host
        .or(env.host)
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let tls_cert = cli.tls_cert.clone().or(env.tls_cert.clone());
    let tls_key = cli.tls_key.clone().or(env.tls_key.clone());
    let use_tls = cli.tls || env.tls.unwrap_or(false) || tls_cert.is_some();
    // 启用 HTTPS 且未显式指定端口时用 8443，避免与 http 习惯端口混淆
    let port = cli
        .port
        .or(env.port)
        .unwrap_or(if use_tls { 8443 } else { 8080 });
    let data_dir = cli
        .data_dir
        .or(env.data_dir)
        .unwrap_or_else(|| PathBuf::from("lan-share-data"));
    let chunk_mib = cli.chunk_mib.or(env.chunk_mib).unwrap_or(8);
    let chunk_size = chunk_mib.clamp(1, 64) * 1024 * 1024;
    let limits = state::LiveLimits {
        max_rooms: cli.live_max_rooms.or(env.live_max_rooms).unwrap_or(8),
        max_viewers: cli.live_max_viewers.or(env.live_max_viewers).unwrap_or(32),
        idle_timeout: std::time::Duration::from_secs(
            cli.live_idle_secs.or(env.live_idle_secs).unwrap_or(90),
        ),
    };
    let token = match cli.token.or(env.token).as_deref() {
        Some("auto") | Some("") => {
            Some(uuid::Uuid::new_v4().simple().to_string()[..12].to_string())
        }
        Some(value) => Some(value.to_string()),
        None => None,
    };

    // 3) 日志：先初始化，稍后把实时面板挂上去
    let default_filter = env
        .log_filter
        .clone()
        .unwrap_or_else(|| "lan_share=info,warn".to_string());
    let filter = if cli.verbose {
        EnvFilter::new("lan_share=debug,tower_http=debug,info")
    } else {
        EnvFilter::new(default_filter)
    };
    let log_writer = dashboard::LogWriter::new();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(log_writer.clone())
        .init();

    // 4) 状态与服务
    let state = Arc::new(
        AppState::new(&data_dir, chunk_size, token.clone(), limits.clone())
            .with_context(|| format!("初始化数据目录失败: {}", data_dir.display()))?,
    );
    api::spawn_janitor(state.clone());
    live::spawn_janitor(state.clone());

    let dashboard_enabled =
        !cli.no_dashboard && env.dashboard.unwrap_or(true) && dashboard::stdout_is_terminal();
    let quiet = cli.quiet || env.banner == Some(false);
    let dash = dashboard::Dashboard::new(state.clone(), dashboard_enabled);
    log_writer.attach(dash.clone());

    let app = api::router(state.clone())
        .layer(CorsLayer::very_permissive())
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("监听地址不合法: {host}:{port}"))?;

    // HTTPS 证书在启动阶段就准备好，配置有问题立刻报错
    let tls_config = if use_tls {
        let files = tls::prepare(
            &data_dir,
            tls_cert.as_deref(),
            tls_key.as_deref(),
            std::slice::from_ref(&host),
        )?;
        let config = RustlsConfig::from_pem_file(&files.cert, &files.key)
            .await
            .with_context(|| format!("加载 TLS 证书失败: {}", files.cert.display()))?;
        Some((config, files))
    } else {
        None
    };

    let listener = match tls_config {
        Some(_) => None,
        None => Some(
            tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("端口绑定失败: {addr}，可能已被占用"))?,
        ),
    };
    let local_addr = match &listener {
        Some(listener) => listener.local_addr()?,
        None => addr,
    };

    if !quiet {
        print_banner(BannerInfo {
            addr: local_addr,
            tls: tls_config.is_some(),
            data_dir: &data_dir,
            chunk_size,
            token: token.as_deref(),
            files: state.files.len(),
            limits: &limits,
            env_info: env_loaded.then_some((env_file.as_path(), env_entries)),
            dashboard: dashboard_enabled,
        });
    }
    dash.spawn();

    match tls_config {
        Some((config, _files)) => {
            let handle = axum_server::Handle::new();
            let shutdown = handle.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                shutdown.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
            });
            axum_server::bind_rustls(local_addr, config)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .context("HTTPS 服务异常退出")?;
        }
        None => {
            let listener = listener.expect("http 监听器已创建");
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("HTTP 服务异常退出")?;
        }
    }

    tracing::info!("服务已停止");
    Ok(())
}

/// 启动横幅需要展示的信息（集中传参，避免函数签名过长）
struct BannerInfo<'a> {
    addr: SocketAddr,
    /// 是否以 HTTPS 提供服务
    tls: bool,
    data_dir: &'a std::path::Path,
    chunk_size: u64,
    token: Option<&'a str>,
    files: usize,
    limits: &'a state::LiveLimits,
    env_info: Option<(&'a std::path::Path, usize)>,
    dashboard: bool,
}

fn print_banner(info: BannerInfo<'_>) {
    let BannerInfo {
        addr,
        tls,
        data_dir,
        chunk_size,
        token,
        files,
        limits,
        env_info,
        dashboard,
    } = info;
    let token_suffix = token.map(|t| format!("?token={t}")).unwrap_or_default();
    let port = addr.port();
    let scheme = if tls { "https" } else { "http" };

    println!();
    println!("  \x1b[1mlan-share\x1b[0m · 局域网文件互传");
    println!("  ─────────────────────────────────────────────");
    println!("  本机访问    {scheme}://127.0.0.1:{port}/{token_suffix}");
    if let Some(ip) = net::primary_local_ip() {
        println!("  局域网访问  \x1b[36m{scheme}://{ip}:{port}/{token_suffix}\x1b[0m  ← 把这个地址发出去");
    } else {
        println!("  局域网访问  {scheme}://<本机IP>:{port}/{token_suffix}");
    }
    if tls {
        println!("  证书提示    自签证书：浏览器/手机会提示不安全，选择「继续访问」即可");
        println!("  手机摄像头  用上面的 https 地址 → 直播录屏 → 摄像头开播");
    } else {
        println!("  手机摄像头  手机浏览器要求 HTTPS，加 --tls 启动即可（自动生成自签证书）");
    }
    println!("  数据目录    {}", data_dir.display());
    println!("  分片大小    {} MiB", chunk_size / 1024 / 1024);
    println!(
        "  直播上限    {} 路 / 每路 {} 人 / 空闲 {} 秒自动结束",
        if limits.max_rooms == 0 {
            "不限".to_string()
        } else {
            limits.max_rooms.to_string()
        },
        if limits.max_viewers == 0 {
            "不限".to_string()
        } else {
            limits.max_viewers.to_string()
        },
        limits.idle_timeout.as_secs(),
    );
    println!("  已有文件    {files} 个");
    match env_info {
        Some((path, count)) => println!("  配置文件    {}（{} 项生效）", path.display(), count),
        None => println!("  配置文件    未使用（仅命令行与环境变量）"),
    }
    if let Some(token) = token {
        println!("  访问令牌    \x1b[33m{token}\x1b[0m（访问其他设备时需带上该令牌）");
    }
    if dashboard {
        println!("  实时面板    已开启（下方每秒刷新；--no-dashboard 可关闭）");
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
