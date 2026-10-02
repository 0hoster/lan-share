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
    pub live: usize,
    pub uptime_secs: u64,
    pub chunk_size: u64,
    /// 正在进行的传输明细（终端面板与网页端都用它）
    pub active_uploads: Vec<UploadStatus>,
}

/// 通过 WebSocket 广播给所有浏览器的事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// 新增文件（目前用于直播录像落盘）
    FileAdded {
        file: FileMeta,
    },
    FileRemoved {
        id: Uuid,
    },
    /// 有新的直播开始
    LiveStarted {
        room_id: String,
        title: String,
    },
    /// 直播结束（含主播手动结束与超时自动结束）
    LiveEnded {
        room_id: String,
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
