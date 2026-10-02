//! 配置来源与优先级：**命令行参数 > .env 文件 > 内置默认值**。
//!
//! .env 只做最基础的解析（KEY=VALUE、支持引号与 export），不引入依赖；
//! 已存在的环境变量优先于 .env 中的同名项，和常见 dotenv 行为一致。

use std::path::{Path, PathBuf};

use anyhow::Context;

pub const DEFAULT_ENV_FILE: &str = ".env";

/// 从 .env 与环境变量中读到的可选项；None 表示"未配置"
#[derive(Debug, Default, Clone)]
pub struct EnvConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub data_dir: Option<PathBuf>,
    pub chunk_mib: Option<u64>,
    pub token: Option<String>,
    pub log_filter: Option<String>,
    /// 是否显示终端实时面板
    pub dashboard: Option<bool>,
    /// 是否显示启动横幅
    pub banner: Option<bool>,
    pub live_max_rooms: Option<usize>,
    pub live_max_viewers: Option<usize>,
    pub live_idle_secs: Option<u64>,
    /// 是否启用 HTTPS（手机摄像头直播需要）
    pub tls: Option<bool>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}

/// 解析 .env 文件并注入进程环境（不覆盖已存在的变量），返回生效的条目数。
pub fn load_env_file(path: &Path) -> anyhow::Result<usize> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;

    let mut applied = 0usize;
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            tracing::warn!(
                "{}:{} 不是 KEY=VALUE 格式，已忽略",
                path.display(),
                index + 1
            );
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = unquote(value.trim());

        // 环境变量优先：只在未设置时注入
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
            applied += 1;
        }
    }
    Ok(applied)
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    // 行尾注释：`KEY=value # 说明`
    match value.find(" #") {
        Some(pos) => value[..pos].trim_end().to_string(),
        None => value.to_string(),
    }
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    let raw = env_string(key)?;
    match raw.parse::<T>() {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!("环境变量 {key}={raw} 无法解析，已忽略");
            None
        }
    }
}

fn env_bool(key: &str) -> Option<bool> {
    let raw = env_string(key)?;
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            tracing::warn!("环境变量 {key}={other} 不是布尔值，已忽略");
            None
        }
    }
}

impl EnvConfig {
    /// 从当前环境变量读取（.env 已在此之前注入）
    pub fn from_env() -> Self {
        Self {
            host: env_string("LAN_SHARE_HOST"),
            port: env_parse("LAN_SHARE_PORT"),
            data_dir: env_string("LAN_SHARE_DATA_DIR").map(PathBuf::from),
            chunk_mib: env_parse("LAN_SHARE_CHUNK_MIB"),
            token: env_string("LAN_SHARE_TOKEN"),
            log_filter: env_string("LAN_SHARE_LOG").or_else(|| env_string("RUST_LOG")),
            dashboard: env_bool("LAN_SHARE_DASHBOARD"),
            banner: env_bool("LAN_SHARE_BANNER"),
            live_max_rooms: env_parse("LAN_SHARE_LIVE_MAX_ROOMS"),
            live_max_viewers: env_parse("LAN_SHARE_LIVE_MAX_VIEWERS"),
            live_idle_secs: env_parse("LAN_SHARE_LIVE_IDLE_SECS"),
            tls: env_bool("LAN_SHARE_TLS"),
            tls_cert: env_string("LAN_SHARE_TLS_CERT").map(PathBuf::from),
            tls_key: env_string("LAN_SHARE_TLS_KEY").map(PathBuf::from),
        }
    }
}
