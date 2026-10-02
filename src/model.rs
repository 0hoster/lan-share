use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 已完成文件的元数据（持久化到 meta 目录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub id: Uuid,
    /// 用户看到的原始文件名
    pub name: String,
    /// 落盘后的存储文件名（位于 files 目录）
    pub stored: String,
    pub size: u64,
    pub mime: String,
    /// Unix 时间戳（秒）
    pub created_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct InitUploadReq {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct InitUploadResp {
    pub upload_id: Uuid,
    pub chunk_size: u64,
    pub chunk_count: u64,
    /// 已收到的分片下标（用于断点续传）
    pub received: Vec<u64>,
}

#[derive(Debug, Serialize)]
pub struct UploadStatus {
    pub upload_id: Uuid,
    pub name: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunk_count: u64,
    pub received: usize,
    pub received_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub files: usize,
    pub bytes: u64,
    pub uploads: usize,
    pub uptime_secs: u64,
    pub chunk_size: u64,
}

/// 通过 WebSocket 广播给所有浏览器的事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    FileRemoved {
        id: Uuid,
    },
    UploadStarted {
        upload_id: Uuid,
        name: String,
        size: u64,
    },
    UploadFinished {
        upload_id: Uuid,
        file: FileMeta,
    },
    UploadAborted {
        upload_id: Uuid,
    },
}
