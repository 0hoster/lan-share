use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use dashmap::DashMap;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::live::LiveHub;
use crate::model::{Event, FileMeta};

/// 一次上传会话（对应一个分片上传中的文件）。
pub struct UploadSession {
    pub id: Uuid,
    /// 展示用的原始文件名
    pub name: String,
    /// 落盘文件名
    pub stored: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunk_count: u64,
    /// 每个分片是否已写入
    pub received: Mutex<Vec<bool>>,
    pub received_bytes: AtomicU64,
    pub created_at: SystemTime,
    pub part_path: PathBuf,
}

impl UploadSession {
    pub fn received_count(&self) -> usize {
        self.received
            .lock()
            .map(|chunks| chunks.iter().filter(|c| **c).count())
            .unwrap_or(0)
    }

    pub fn age(&self) -> std::time::Duration {
        self.created_at
            .elapsed()
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
    }

    /// 指定分片应当写入的字节数（最后一片可能更短，0 字节文件为 0）。
    pub fn chunk_len_for_index(&self, index: u64) -> u64 {
        let start = index * self.chunk_size;
        if start >= self.size {
            return 0;
        }
        (self.size - start).min(self.chunk_size)
    }
}

pub struct AppState {
    pub files_dir: PathBuf,
    pub meta_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub chunk_size: u64,
    pub sessions: DashMap<Uuid, UploadSession>,
    pub files: DashMap<Uuid, FileMeta>,
    pub events: broadcast::Sender<Event>,
    /// 正在进行的直播房间
    pub live: LiveHub,
    pub limits: LiveLimits,
    pub token: Option<String>,
    pub started: Instant,
}

/// 直播相关上限，可通过 CLI / .env 覆盖
#[derive(Debug, Clone)]
pub struct LiveLimits {
    pub max_rooms: usize,
    pub max_viewers: usize,
    pub idle_timeout: std::time::Duration,
}

impl Default for LiveLimits {
    fn default() -> Self {
        Self {
            max_rooms: 8,
            max_viewers: 32,
            idle_timeout: std::time::Duration::from_secs(90),
        }
    }
}

impl AppState {
    pub fn new(
        data_dir: &Path,
        chunk_size: u64,
        token: Option<String>,
        limits: LiveLimits,
    ) -> anyhow::Result<Self> {
        let files_dir = data_dir.join("files");
        let meta_dir = data_dir.join("meta");
        let tmp_dir = data_dir.join("tmp");

        for dir in [
            data_dir.to_path_buf(),
            files_dir.clone(),
            meta_dir.clone(),
            tmp_dir.clone(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("创建目录失败: {}", dir.display()))?;
        }

        let (events, _) = broadcast::channel(1024);
        let state = Self {
            files_dir,
            meta_dir,
            tmp_dir,
            chunk_size,
            sessions: DashMap::new(),
            files: DashMap::new(),
            events,
            live: LiveHub::new(),
            limits,
            token,
            started: Instant::now(),
        };
        state.load_meta()?;
        state.cleanup_tmp()?;
        Ok(state)
    }

    /// 启动时从 meta 目录恢复文件索引，并剔除已经丢失的数据文件。
    fn load_meta(&self) -> anyhow::Result<()> {
        let entries = match std::fs::read_dir(&self.meta_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };

        let mut loaded = 0usize;
        let mut dropped = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(err) => {
                    tracing::warn!("读取元数据失败 {}: {err}", path.display());
                    continue;
                }
            };
            match serde_json::from_str::<FileMeta>(&raw) {
                Ok(meta) => {
                    if !self.files_dir.join(&meta.stored).exists() {
                        tracing::warn!("数据文件缺失，清理元数据: {}", meta.stored);
                        let _ = std::fs::remove_file(&path);
                        dropped += 1;
                        continue;
                    }
                    self.files.insert(meta.id, meta);
                    loaded += 1;
                }
                Err(err) => {
                    tracing::warn!("元数据损坏 {}: {err}", path.display());
                    let _ = std::fs::remove_file(&path);
                    dropped += 1;
                }
            }
        }
        if loaded > 0 || dropped > 0 {
            tracing::info!("已恢复 {loaded} 个文件记录，清理 {dropped} 条无效记录");
        }
        Ok(())
    }

    /// 清理上次异常退出留下的临时分片文件。
    fn cleanup_tmp(&self) -> anyhow::Result<()> {
        let Ok(entries) = std::fs::read_dir(&self.tmp_dir) else {
            return Ok(());
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            if std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!("已清理 {removed} 个未完成的上传临时文件");
        }
        Ok(())
    }

    pub fn persist_meta(&self, meta: &FileMeta) -> anyhow::Result<()> {
        let path = self.meta_dir.join(format!("{}.json", meta.id));
        let tmp = self.meta_dir.join(format!("{}.json.tmp", meta.id));
        let raw = serde_json::to_vec_pretty(meta)?;
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn remove_meta_file(&self, id: &Uuid) {
        let path = self.meta_dir.join(format!("{id}.json"));
        if let Err(err) = std::fs::remove_file(&path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("删除元数据失败 {}: {err}", path.display());
            }
        }
    }

    pub fn broadcast(&self, event: Event) {
        // 没有订阅者时 send 会返回错误，这里直接忽略。
        let _ = self.events.send(event);
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.value().size).sum()
    }

    /// 定期清理超时未完成的上传会话。
    pub fn purge_stale_sessions(&self, max_age: std::time::Duration) -> usize {
        let stale: Vec<Uuid> = self
            .sessions
            .iter()
            .filter(|s| s.value().age() > max_age)
            .map(|s| *s.key())
            .collect();

        for id in &stale {
            if let Some((_, session)) = self.sessions.remove(id) {
                let _ = std::fs::remove_file(&session.part_path);
                self.broadcast(Event::UploadAborted { upload_id: *id });
            }
        }
        stale.len()
    }

    pub fn next_chunk_count(size: u64, chunk_size: u64) -> u64 {
        if size == 0 {
            return 1;
        }
        size.div_ceil(chunk_size)
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn record_received(session: &UploadSession, index: u64) -> Option<u64> {
    let mut chunks = session.received.lock().ok()?;
    let idx = index as usize;
    if idx >= chunks.len() || chunks[idx] {
        return None;
    }
    chunks[idx] = true;
    let added = session.chunk_len_for_index(index);
    session.received_bytes.fetch_add(added, Ordering::Relaxed);
    Some(added)
}
