//! 终端实时统计面板。
//!
//! 面板固定绘制在终端底部并每秒刷新；日志输出会先擦掉面板再打印，
//! 所以日志仍然按时间顺序可读，不会被面板顶掉。
//!
//! 只有 stdout 是终端、且未显式关闭时才启用（`--no-dashboard` 或
//! `LAN_SHARE_DASHBOARD=0`），重定向到文件时自动退化成纯日志。

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

use crate::state::AppState;

/// 采样一次速度用的上一次读数
struct SpeedSample {
    bytes: u64,
    at: Instant,
    speed: f64,
    /// 最近一次有进展的时刻，用来提示卡住的传输
    last_progress: Instant,
}

pub struct Dashboard {
    state: Arc<AppState>,
    enabled: AtomicBool,
    /// 当前已绘制的行数
    lines: AtomicUsize,
    /// 串行化"面板与日志"对 stdout 的写入
    output: Mutex<()>,
    speeds: Mutex<HashMap<Uuid, SpeedSample>>,
}

impl Dashboard {
    pub fn new(state: Arc<AppState>, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            state,
            enabled: AtomicBool::new(enabled),
            lines: AtomicUsize::new(0),
            output: Mutex::new(()),
            speeds: Mutex::new(HashMap::new()),
        })
    }

    /// 每秒刷新一次的面板循环
    pub fn spawn(self: &Arc<Self>) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let dash = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                dash.render();
            }
        });
    }

    /// 供日志使用：擦掉面板后写出日志正文
    pub fn write_log(&self, buf: &[u8]) -> std::io::Result<usize> {
        let _lock = self.output.lock().unwrap_or_else(|err| err.into_inner());
        let mut out = std::io::stdout();
        let drawn = self.lines.swap(0, Ordering::Relaxed);
        if drawn > 0 {
            write!(out, "\x1b[{drawn}F\x1b[J")?;
        }
        out.write_all(buf)?;
        out.flush()?;
        Ok(buf.len())
    }

    fn render(&self) {
        let text = self.compose();
        let _lock = self.output.lock().unwrap_or_else(|err| err.into_inner());
        let mut out = std::io::stdout();
        let drawn = self.lines.load(Ordering::Relaxed);
        if drawn > 0 {
            let _ = write!(out, "\x1b[{drawn}F\x1b[J");
        }
        let _ = write!(out, "{text}");
        let _ = out.flush();
        self.lines.store(text.lines().count(), Ordering::Relaxed);
    }

    fn compose(&self) -> String {
        let state = &self.state;
        let uptime = state.started.elapsed();
        let files = state.files.len();
        let bytes = state.total_bytes();
        let rooms = state.live.list();
        let viewers: usize = rooms.iter().map(|room| room.viewers).sum();

        let mut out = String::with_capacity(512);
        out.push_str(&format!(
            "\x1b[36m─── lan-share 运行中 {}\x1b[0m ── 文件 {} 个 / {} · 直播 {} 路 / {} 人观看\n",
            fmt_duration(uptime.as_secs()),
            files,
            fmt_bytes(bytes),
            rooms.len(),
            viewers,
        ));

        let mut session_ids: Vec<Uuid> = Vec::new();
        let mut rendered = 0usize;
        let mut pending = 0usize;
        let mut samples = self.speeds.lock().unwrap_or_else(|err| err.into_inner());
        for entry in state.sessions.iter() {
            let session = entry.value();
            session_ids.push(session.id);
            let received = session.received_bytes.load(Ordering::Relaxed);
            let sample = samples.entry(session.id).or_insert(SpeedSample {
                bytes: received,
                at: Instant::now(),
                speed: 0.0,
                last_progress: Instant::now(),
            });
            let elapsed = sample.at.elapsed().as_secs_f64();
            if elapsed >= 0.5 {
                sample.speed = received.saturating_sub(sample.bytes) as f64 / elapsed;
                if received > sample.bytes {
                    sample.last_progress = Instant::now();
                }
                sample.bytes = received;
                sample.at = Instant::now();
            }
            pending += 1;
            if rendered >= 5 {
                continue;
            }
            rendered += 1;
            let percent = if session.size == 0 {
                100.0
            } else {
                received as f64 / session.size as f64 * 100.0
            };
            const STALL_SECS: u64 = 15;
            let stalled = sample.last_progress.elapsed().as_secs();
            let tail = if stalled >= STALL_SECS {
                format!("\x1b[33m· 已 {stalled} 秒无进展\x1b[0m")
            } else {
                format!("{:>9}/s", fmt_bytes(sample.speed as u64))
            };
            out.push_str(&format!(
                "  {} {}  {:>3.0}%  {} / {}  {}  分段 {}/{}\n",
                progress_bar(percent),
                truncate(&session.name, 28),
                percent,
                fmt_bytes(received),
                fmt_bytes(session.size),
                tail,
                session.received_count(),
                session.chunk_count,
            ));
        }
        samples.retain(|id, _| session_ids.contains(id));
        drop(samples);

        if session_ids.is_empty() {
            out.push_str("  （当前没有进行中的上传）\n");
        } else if pending > rendered {
            out.push_str(&format!("  …还有 {} 个上传未显示\n", pending - rendered));
        }

        for room in rooms.iter().take(3) {
            out.push_str(&format!(
                "  \x1b[35m●\x1b[0m 直播 {} · {} 人观看 · {} · {} 个分片{}\n",
                truncate(&room.title, 24),
                room.viewers,
                fmt_bytes(room.bytes),
                room.chunks,
                if room.recording { " · 录像中" } else { "" },
            ));
        }

        out.push_str(&format!(
            "\x1b[36m───\x1b[0m 数据目录 {} · Ctrl+C 停止\n",
            state
                .files_dir
                .parent()
                .unwrap_or(&state.files_dir)
                .display(),
        ));
        out
    }
}

/// 包装 tracing 的写入器：先擦面板再写日志。
///
/// 面板需要 AppState 才能构造，而 AppState 创建过程中就会打日志，
/// 所以这里用一个"槽位"：日志系统先初始化，之后再挂上面板。
#[derive(Clone, Default)]
pub struct LogWriter {
    dashboard: Arc<Mutex<Option<Arc<Dashboard>>>>,
}

impl LogWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 状态准备好之后把面板挂上去
    pub fn attach(&self, dashboard: Arc<Dashboard>) {
        if let Ok(mut slot) = self.dashboard.lock() {
            *slot = Some(dashboard);
        }
    }
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }

    fn make_writer_for(&'a self, _meta: &tracing::Metadata<'_>) -> Self::Writer {
        self.clone()
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let dashboard = self
            .dashboard
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().cloned());
        match dashboard {
            Some(dashboard) => dashboard.write_log(buf),
            None => std::io::stdout().write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// stdout 是终端才启用面板
pub fn stdout_is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

fn progress_bar(percent: f64) -> String {
    const WIDTH: usize = 10;
    let filled = ((percent / 100.0) * WIDTH as f64)
        .round()
        .clamp(0.0, WIDTH as f64) as usize;
    let mut bar = String::with_capacity(WIDTH + 2);
    bar.push('[');
    for index in 0..WIDTH {
        bar.push(if index < filled { '█' } else { '░' });
    }
    bar.push(']');
    bar
}

fn truncate(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

pub fn fmt_bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub fn fmt_duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
