//! 直播能力：浏览器采集屏幕/摄像头 → 分片推流 → 其他设备在浏览器里实时观看。
//!
//! 传输走「WebSocket + MediaSource」而不是 WebRTC：
//!   * 一个发送端对多个观众，服务端只做转发，不需要信令与 ICE；
//!   * 复用现有的 axum/WebSocket 栈，不引入额外依赖；
//!   * 代价是延迟约 1~3 秒（MediaRecorder 分片间隔决定），局域网内看直播足够。
//!
//! WebM 分片有两类：第 0 个分片含有初始化段（编码参数），之后是媒体数据。
//! 中途加入的观众必须先拿到初始化段，因此房间里单独保存 init，再配上最近
//! 一段分片（环缓冲），这样新观众不用等太久就能开始播放。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use bytes::Bytes;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::error::{ApiError, ApiResult};
use crate::model::{Event, FileMeta};
use crate::state::{now_secs, AppState};

/// 单个分片上限（MediaRecorder 的 timeslice 分片通常几十 KB ~ 几 MB）
const MAX_CHUNK_BYTES: usize = 16 * 1024 * 1024;
/// 给新观众保留的最近数据量，够快进到接近直播点
const TAIL_BUDGET_BYTES: usize = 24 * 1024 * 1024;
/// 广播通道容量（分片数）；观众消费不过来时会被标记 Lagged 并让其重连
const BROADCAST_CAPACITY: usize = 512;

/// 房间对外展示的信息
#[derive(Debug, Clone, Serialize)]
pub struct RoomInfo {
    pub id: String,
    pub title: String,
    pub mime: String,
    pub started_at: u64,
    pub viewers: usize,
    pub bytes: u64,
    pub chunks: u64,
    pub recording: bool,
}

#[derive(Debug, Deserialize)]
pub struct StartReq {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub mime: String,
    /// 是否在服务端同时录制，结束后自动进入文件列表
    #[serde(default)]
    pub record: bool,
}

#[derive(Debug, Serialize)]
pub struct StartResp {
    pub room_id: String,
    /// 主播密钥，推流和结束直播时需通过 x-live-key 头带上
    pub key: String,
}

#[derive(Debug, Serialize)]
pub struct PushResp {
    pub seq: u64,
    pub bytes: u64,
    pub viewers: usize,
}

#[derive(Clone)]
pub struct Chunk {
    pub seq: u64,
    pub data: Bytes,
}

#[derive(Clone)]
pub enum LiveMsg {
    Chunk(Chunk),
    Ended,
}

pub struct Room {
    pub id: String,
    pub title: String,
    /// 主播密钥：推流与结束直播时校验，防止别人冒名推流
    pub key: String,
    pub mime: String,
    pub started_at: u64,
    last_chunk_at: AtomicU64,
    seq: AtomicU64,
    pub sent_bytes: AtomicU64,
    pub chunks: AtomicU64,
    pub viewers: AtomicUsize,
    /// 本次直播是否开启了服务端录制
    pub recording_enabled: bool,
    sender: broadcast::Sender<LiveMsg>,
    init: Mutex<Option<Bytes>>,
    tail: Mutex<VecDeque<Chunk>>,
    tail_bytes: AtomicU64,
    recording: tokio::sync::Mutex<Option<tokio::io::BufWriter<tokio::fs::File>>>,
    record_path: std::path::PathBuf,
    finished: AtomicBool,
}

impl Room {
    pub fn new(
        id: String,
        title: String,
        key: String,
        mime: String,
        record_path: std::path::PathBuf,
        recording: Option<tokio::io::BufWriter<tokio::fs::File>>,
    ) -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        let recording_enabled = recording.is_some();
        Self {
            id,
            title,
            key,
            mime,
            started_at: now_secs(),
            last_chunk_at: AtomicU64::new(now_secs()),
            seq: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
            viewers: AtomicUsize::new(0),
            recording_enabled,
            sender,
            init: Mutex::new(None),
            tail: Mutex::new(VecDeque::new()),
            tail_bytes: AtomicU64::new(0),
            recording: tokio::sync::Mutex::new(recording),
            record_path,
            finished: AtomicBool::new(false),
        }
    }

    /// 追加一个分片：先落盘（若开启了录制），再广播给所有观众。
    pub async fn push(&self, data: Bytes) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.last_chunk_at.store(now_secs(), Ordering::Relaxed);
        self.sent_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.chunks.fetch_add(1, Ordering::Relaxed);

        // 第 0 个分片包含 WebM 初始化段，必须单独留存
        if seq == 0 {
            if let Ok(mut init) = self.init.lock() {
                *init = Some(data.clone());
            }
        }

        // 最近分片环缓冲：只保留有限的字节数
        if let Ok(mut tail) = self.tail.lock() {
            tail.push_back(Chunk {
                seq,
                data: data.clone(),
            });
            let mut bytes = self.tail_bytes.load(Ordering::Relaxed) + data.len() as u64;
            while bytes > TAIL_BUDGET_BYTES as u64 && tail.len() > 1 {
                if let Some(old) = tail.pop_front() {
                    bytes -= old.data.len() as u64;
                }
            }
            self.tail_bytes.store(bytes, Ordering::Relaxed);
        }

        {
            let mut guard = self.recording.lock().await;
            if let Some(file) = guard.as_mut() {
                if let Err(err) = file.write_all(&data).await {
                    tracing::warn!("写直播录像失败，已停止录制: {err}");
                    *guard = None;
                }
            }
        }

        let _ = self.sender.send(LiveMsg::Chunk(Chunk { seq, data }));
        seq
    }

    /// 观众加入时需要的快照：初始化分片 + 最近分片 + 已推送到的最后一个序号。
    ///
    /// 注意返回的是 `i64`：房间还没有任何分片时是 -1，不能像以前那样用
    /// `saturating_sub(1)` 得到 0，否则 seq=0 的初始化分片会被判定为
    /// 「快照里已经发过」而丢掉，观众将永远拿不到 WebM 初始化段。
    pub fn snapshot(&self) -> (Option<Bytes>, Vec<Chunk>, i64) {
        let pushed = self.seq.load(Ordering::SeqCst);
        let last_seq = pushed as i64 - 1;
        let init = self.init.lock().ok().and_then(|guard| guard.clone());
        let tail = self
            .tail
            .lock()
            .map(|guard| guard.iter().cloned().collect())
            .unwrap_or_default();
        (init, tail, last_seq)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LiveMsg> {
        self.sender.subscribe()
    }

    /// 结束直播：通知所有观众，之后 receiver 会收到 Ended
    pub fn end(&self) {
        if !self.finished.swap(true, Ordering::SeqCst) {
            let _ = self.sender.send(LiveMsg::Ended);
        }
    }

    pub fn idle_for(&self) -> Duration {
        Duration::from_secs(now_secs().saturating_sub(self.last_chunk_at.load(Ordering::Relaxed)))
    }

    pub fn info(&self) -> RoomInfo {
        RoomInfo {
            id: self.id.clone(),
            title: self.title.clone(),
            mime: self.mime.clone(),
            started_at: self.started_at,
            viewers: self.viewers.load(Ordering::Relaxed),
            bytes: self.sent_bytes.load(Ordering::Relaxed),
            chunks: self.chunks.load(Ordering::Relaxed),
            recording: self.recording_enabled,
        }
    }

    pub async fn take_recording(&self) -> Option<tokio::io::BufWriter<tokio::fs::File>> {
        let mut guard = self.recording.lock().await;
        if let Some(file) = guard.as_mut() {
            let _ = file.flush().await;
        }
        guard.take()
    }

    pub fn record_path(&self) -> &std::path::Path {
        &self.record_path
    }
}

/// 直播房间集合
#[derive(Default)]
pub struct LiveHub {
    rooms: DashMap<String, Arc<Room>>,
}

impl LiveHub {
    pub fn new() -> Self {
        Self {
            rooms: DashMap::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Room>> {
        self.rooms.get(id).map(|entry| entry.value().clone())
    }

    pub fn len(&self) -> usize {
        self.rooms.len()
    }

    pub fn list(&self) -> Vec<RoomInfo> {
        let mut list: Vec<RoomInfo> = self
            .rooms
            .iter()
            .map(|entry| entry.value().info())
            .collect();
        list.sort_by_key(|room| std::cmp::Reverse(room.started_at));
        list
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Room>> {
        self.rooms.remove(id).map(|(_, room)| room)
    }

    /// 创建房间；开启录制时同时准备好临时录像文件。
    pub async fn create(&self, state: &AppState, req: StartReq) -> ApiResult<StartResp> {
        let max_rooms = state.limits.max_rooms;
        if max_rooms > 0 && self.rooms.len() >= max_rooms {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!("同时进行的直播过多（上限 {max_rooms} 路），请稍后再试"),
            ));
        }

        let id = short_id();
        let key = uuid::Uuid::new_v4().simple().to_string();
        let title = sanitize_title(&req.title);
        let mime = normalize_mime(&req.mime);
        let record_path = state.tmp_dir.join(format!("live-{id}.webm"));

        let recording = if req.record {
            let file = tokio::fs::File::create(&record_path).await?;
            Some(tokio::io::BufWriter::new(file))
        } else {
            None
        };

        let room = Arc::new(Room::new(
            id.clone(),
            title.clone(),
            key.clone(),
            mime,
            record_path,
            recording,
        ));
        self.rooms.insert(id.clone(), room);
        state.broadcast(Event::LiveStarted {
            room_id: id.clone(),
            title,
        });
        tracing::info!("新建直播房间 {id}");
        Ok(StartResp { room_id: id, key })
    }

    /// 找出空闲超时的房间，用于自动结束（主播异常掉线）
    pub fn stale(&self, timeout: Duration) -> Vec<Arc<Room>> {
        self.rooms
            .iter()
            .filter(|entry| entry.value().idle_for() > timeout)
            .map(|entry| entry.value().clone())
            .collect()
    }
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn sanitize_title(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(60)
        .collect();
    if cleaned.is_empty() {
        "未命名直播".to_string()
    } else {
        cleaned
    }
}

/// 只接受 WebM 系列，避免把非浏览器能播放的格式转发给观众
fn normalize_mime(raw: &str) -> String {
    let mime = raw.split(';').next().unwrap_or("").trim().to_lowercase();
    if mime.is_empty() || !mime.starts_with("video/webm") && !mime.starts_with("audio/webm") {
        "video/webm".to_string()
    } else {
        raw.trim().to_string()
    }
}

// ------------------------------------------------------------------ HTTP 接口

pub async fn list_rooms(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<RoomInfo>>> {
    Ok(Json(state.live.list()))
}

pub async fn start(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartReq>,
) -> ApiResult<Json<StartResp>> {
    let resp = state.live.create(&state, req).await?;
    Ok(Json(resp))
}

/// 主播推送一个分片（请求体为裸字节）
pub async fn push_chunk(
    State(state): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> ApiResult<Json<PushResp>> {
    let room = state
        .live
        .get(&room_id)
        .ok_or_else(|| ApiError::not_found("直播不存在或已结束"))?;
    check_key(&room, &headers)?;

    let data = read_body_limited(body, MAX_CHUNK_BYTES).await?;
    if data.is_empty() {
        return Err(ApiError::bad_request("分片内容为空"));
    }

    let seq = room.push(data).await;
    Ok(Json(PushResp {
        seq,
        bytes: room.sent_bytes.load(Ordering::Relaxed),
        viewers: room.viewers.load(Ordering::Relaxed),
    }))
}

/// 结束直播；若开启了录制，录像会落成文件并出现在文件列表里
pub async fn stop(
    State(state): State<Arc<AppState>>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<StopResp>> {
    let room = state
        .live
        .get(&room_id)
        .ok_or_else(|| ApiError::not_found("直播不存在或已结束"))?;
    check_key(&room, &headers)?;
    state.live.remove(&room_id);
    room.end();

    let saved = finalize_recording(&state, &room).await;
    state.broadcast(Event::LiveEnded {
        room_id: room_id.clone(),
    });
    tracing::info!("直播 {room_id} 已结束");
    Ok(Json(StopResp { saved }))
}

#[derive(Debug, Serialize)]
pub struct StopResp {
    /// 录制成功后生成的录像文件（未开启录制或文件为空时为 null）
    pub saved: Option<FileMeta>,
}

fn check_key(room: &Room, headers: &HeaderMap) -> ApiResult<()> {
    let provided = headers
        .get("x-live-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if provided == room.key {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::FORBIDDEN, "直播间密钥不正确"))
    }
}

async fn read_body_limited(body: axum::body::Body, limit: usize) -> ApiResult<Bytes> {
    use futures_util::StreamExt;

    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(frame) = stream.next().await {
        let chunk = frame.map_err(|err| ApiError::bad_request(format!("读取请求体失败: {err}")))?;
        if buffer.len() + chunk.len() > limit {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("分片超过上限 {} 字节", limit),
            ));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

// ------------------------------------------------------------------ 观看端

/// 观众通过 WebSocket 接收「初始化分片 + 最近分片 + 后续实时分片」
pub async fn viewer(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(room_id): Path<String>,
) -> ApiResult<Response> {
    let room = state
        .live
        .get(&room_id)
        .ok_or_else(|| ApiError::not_found("直播不存在或已结束"))?;
    let max_viewers = state.limits.max_viewers;
    if max_viewers > 0 && room.viewers.load(Ordering::Relaxed) >= max_viewers {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("观看人数已达上限（{max_viewers} 人）"),
        ));
    }
    Ok(ws.on_upgrade(move |socket| run_viewer(socket, room)))
}

/// 观众断开时自动减计数
struct ViewerGuard(Arc<Room>);

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.viewers.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn run_viewer(mut socket: WebSocket, room: Arc<Room>) {
    room.viewers.fetch_add(1, Ordering::Relaxed);
    let _guard = ViewerGuard(room.clone());

    // 先订阅再取快照：两者之间产生的分片会出现在快照里，用 seq 去重
    let mut rx = room.subscribe();
    let (init, tail, snapshot_seq) = room.snapshot();

    let info = serde_json::json!({ "type": "info", "room": room.info() });
    if socket
        .send(Message::Text(info.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut backlog: Vec<Bytes> = Vec::with_capacity(tail.len() + 1);
    if let Some(init) = init {
        backlog.push(init);
    }
    for chunk in tail {
        if chunk.seq == 0 {
            continue; // 初始化分片已经单独发送
        }
        backlog.push(chunk.data);
    }
    for data in backlog {
        if socket.send(Message::Binary(data)).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            message = rx.recv() => match message {
                Ok(LiveMsg::Chunk(chunk)) => {
                    if chunk.seq as i64 <= snapshot_seq {
                        continue; // 快照里已经发过
                    }
                    if socket.send(Message::Binary(chunk.data)).await.is_err() {
                        break;
                    }
                }
                Ok(LiveMsg::Ended) => {
                    let ended = serde_json::json!({ "type": "ended" });
                    let _ = socket.send(Message::Text(ended.to_string().into())).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // 观众消费不过来：让它重连以重新取快照，避免越拖越久
                    tracing::debug!("观众落后 {skipped} 个分片，结束本次连接");
                    let lagged = serde_json::json!({ "type": "lagged" });
                    let _ = socket.send(Message::Text(lagged.to_string().into())).await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
}

// ------------------------------------------------------------------ 录像落盘

/// 把临时录像文件转成正式文件（移动 + 元数据 + 通知前端）
async fn finalize_recording(state: &Arc<AppState>, room: &Arc<Room>) -> Option<FileMeta> {
    let file = room.take_recording().await?;
    drop(file); // flush 已在 take_recording 里完成，这里关闭句柄以便改名

    let path = room.record_path().to_path_buf();
    let size = tokio::fs::metadata(&path).await.ok()?.len();
    if size == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return None;
    }

    let id = uuid::Uuid::new_v4();
    let name = format!("录屏-{}-{}.webm", room.title, format_timestamp(now_secs()));
    let stored = format!("{id}__{}", crate::api::sanitize_file_name(&name));
    let dest = state.files_dir.join(&stored);
    if let Err(err) = tokio::fs::rename(&path, &dest).await {
        tracing::warn!("移动录像文件失败: {err}");
        let _ = tokio::fs::remove_file(&path).await;
        return None;
    }

    let meta = FileMeta {
        id,
        name,
        stored,
        size,
        mime: "video/webm".to_string(),
        created_at: now_secs(),
    };
    if let Err(err) = state.persist_meta(&meta) {
        tracing::warn!("写入录像元数据失败: {err}");
    }
    state.files.insert(id, meta.clone());
    state.broadcast(Event::FileAdded { file: meta.clone() });
    tracing::info!("直播录像已保存: {} ({} 字节)", meta.name, meta.size);
    Some(meta)
}

/// 后台任务：主播异常掉线（长时间没有新分片）时自动结束直播并保存已有录像
pub fn spawn_janitor(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            for room in state.live.stale(state.limits.idle_timeout) {
                if state.live.remove(&room.id).is_none() {
                    continue;
                }
                tracing::info!("直播 {} 空闲超时，自动结束", room.id);
                room.end();
                let _ = finalize_recording(&state, &room).await;
                state.broadcast(Event::LiveEnded {
                    room_id: room.id.clone(),
                });
            }
        }
    });
}

/// 把 Unix 时间戳格式化成 `YYYYMMDD-HHMMSS`（UTC，避免引入时间库）
fn format_timestamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let seconds_of_day = unix_secs % 86_400;
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    );

    // Howard Hinnant 的 civil_from_days 算法
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}
