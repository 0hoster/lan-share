use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::assets;
use crate::error::{ApiError, ApiResult};
use crate::live;
use crate::model::{Event, FileMeta, InitUploadReq, InitUploadResp, Stats, UploadStatus};
use crate::state::{now_secs, record_received, AppState, UploadSession};
use crate::ws;

/// 上传分片的缓冲队列长度（每块一个 body frame），控制内存占用。
const WRITE_QUEUE: usize = 16;
/// 下载流式读取的缓冲区大小。
const READ_BUFFER: usize = 256 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/files", get(list_files))
        .route("/files/{id}", delete(delete_file))
        .route("/files/{id}/download", get(download).head(head_download))
        .route("/upload", get(list_uploads))
        .route("/upload/init", post(init_upload))
        .route("/upload/{id}/chunk/{index}", put(upload_chunk))
        .route("/upload/{id}/complete", post(complete_upload))
        .route("/upload/{id}", delete(abort_upload))
        .route("/stats", get(stats))
        // ---- 直播 / 录屏 ----
        .route("/live/rooms", get(live::list_rooms))
        .route("/live/start", post(live::start))
        .route("/live/{id}/chunk", put(live::push_chunk))
        .route("/live/{id}/stop", post(live::stop))
        .route("/live/{id}/reset", post(live::reset))
        .route("/live/{id}/ws", get(live::viewer))
        // 分片上传：单个请求体最大约 64 MiB，足够覆盖默认分片大小
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 事件流同样可能泄露文件名，开启令牌时必须一起鉴权
    let events =
        Router::new()
            .route("/ws", get(ws::handler))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ));

    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/live.js", get(live_js))
        .route("/styles.css", get(styles_css))
        .route("/favicon.ico", get(favicon))
        .merge(events)
        .nest("/api", api)
        .with_state(state)
}

// ---------------------------------------------------------------- 静态资源

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        assets::INDEX_HTML,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        assets::APP_JS,
    )
}

async fn live_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        assets::LIVE_JS,
    )
}

async fn styles_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        assets::STYLES_CSS,
    )
}

async fn favicon() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------- 鉴权

/// 可选访问令牌：未配置时全部放行；配置后要求 `?token=` 或 `Authorization: Bearer`。
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(next.run(request).await);
    };

    let provided = header_token(request.headers()).or_else(|| query_token(request.uri().query()));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => Err(ApiError::unauthorized("访问令牌缺失或不正确")),
    }
}

fn header_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    Some(token.trim().to_string())
}

fn query_token(query: Option<&str>) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == "token" {
            Some(percent_decode(value))
        } else {
            None
        }
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------- 文件列表

async fn list_files(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<FileMeta>>> {
    let mut files: Vec<FileMeta> = state.files.iter().map(|f| f.value().clone()).collect();
    files.sort_by_key(|file| std::cmp::Reverse(file.created_at));
    Ok(Json(files))
}

async fn list_uploads(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<UploadStatus>>> {
    let mut uploads: Vec<UploadStatus> = state
        .sessions
        .iter()
        .map(|entry| {
            let s = entry.value();
            UploadStatus {
                upload_id: s.id,
                name: s.name.clone(),
                size: s.size,
                chunk_size: s.chunk_size,
                chunk_count: s.chunk_count,
                received: s.received_count(),
                received_bytes: s.received_bytes.load(Ordering::Relaxed),
            }
        })
        .collect();
    uploads.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(uploads))
}

async fn stats(State(state): State<Arc<AppState>>) -> ApiResult<Json<Stats>> {
    let mut active: Vec<UploadStatus> = state
        .sessions
        .iter()
        .map(|entry| {
            let session = entry.value();
            UploadStatus {
                upload_id: session.id,
                name: session.name.clone(),
                size: session.size,
                chunk_size: session.chunk_size,
                chunk_count: session.chunk_count,
                received: session.received_count(),
                received_bytes: session.received_bytes.load(Ordering::Relaxed),
            }
        })
        .collect();
    active.sort_by_key(|item| std::cmp::Reverse(item.received_bytes));

    Ok(Json(Stats {
        files: state.files.len(),
        bytes: state.total_bytes(),
        uploads: state.sessions.len(),
        live: state.live.len(),
        uptime_secs: state.started.elapsed().as_secs(),
        chunk_size: state.chunk_size,
        active_uploads: active,
    }))
}

// ---------------------------------------------------------------- 上传

/// 创建上传会话：预分配分片尺寸并返回需要上传的分片数量。
async fn init_upload(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InitUploadReq>,
) -> ApiResult<Json<InitUploadResp>> {
    let name = sanitize_display_name(&req.name);
    let id = Uuid::new_v4();
    let stored = format!("{id}__{}", sanitize_file_name(&name));
    let chunk_size = state.chunk_size;
    let chunk_count = AppState::next_chunk_count(req.size, chunk_size);
    let part_path = state.tmp_dir.join(format!("{id}.part"));

    // 预分配稀疏文件：减少写入时的碎片，顺序落盘更快。
    {
        let path = part_path.clone();
        let size = req.size;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)?;
            file.set_len(size)?;
            file.sync_all()?;
            Ok(())
        })
        .await
        .map_err(|e| ApiError::internal(format!("创建临时文件失败: {e}")))??;
    }

    let mut received = vec![false; chunk_count as usize];
    if req.size == 0 {
        received[0] = true;
    }

    let session = UploadSession {
        id,
        name: name.clone(),
        stored,
        size: req.size,
        chunk_size,
        chunk_count,
        received: std::sync::Mutex::new(received.clone()),
        received_bytes: std::sync::atomic::AtomicU64::new(0),
        created_at: std::time::SystemTime::now(),
        part_path,
    };
    state.sessions.insert(id, session);
    state.broadcast(Event::UploadStarted {
        upload_id: id,
        name,
        size: req.size,
    });
    tracing::info!("新建上传会话 {id}，{} 字节，{chunk_count} 个分片", req.size);

    Ok(Json(InitUploadResp {
        upload_id: id,
        chunk_size,
        chunk_count,
        received: received
            .iter()
            .enumerate()
            .filter(|(_, done)| **done)
            .map(|(idx, _)| idx as u64)
            .collect(),
    }))
}

/// 接收单个分片：请求体以流式方式写入临时文件的指定偏移，内存占用恒定。
async fn upload_chunk(
    State(state): State<Arc<AppState>>,
    Path((upload_id, index)): Path<(Uuid, u64)>,
    body: Body,
) -> ApiResult<Json<UploadStatus>> {
    let (part_path, chunk_size, expected_len, size) = {
        let session = state
            .sessions
            .get(&upload_id)
            .ok_or_else(|| ApiError::not_found("上传会话不存在或已过期"))?;
        if index >= session.chunk_count {
            return Err(ApiError::bad_request(format!(
                "分片下标越界: {index} >= {}",
                session.chunk_count
            )));
        }
        (
            session.part_path.clone(),
            session.chunk_size,
            session.chunk_len_for_index(index),
            session.size,
        )
    };
    let _ = size;
    let offset = index * chunk_size;

    let (tx, mut rx) = mpsc::channel::<Bytes>(WRITE_QUEUE);
    let writer = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        let mut file = std::fs::OpenOptions::new().write(true).open(&part_path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut written = 0u64;
        while let Some(buf) = rx.blocking_recv() {
            file.write_all(&buf)?;
            written += buf.len() as u64;
        }
        file.flush()?;
        Ok(written)
    });

    let mut stream = body.into_data_stream();
    let mut received_len = 0u64;
    let mut send_error: Option<ApiError> = None;
    while let Some(frame) = stream.next().await {
        let chunk = match frame {
            Ok(chunk) => chunk,
            Err(err) => {
                send_error = Some(ApiError::bad_request(format!("读取请求体失败: {err}")));
                break;
            }
        };
        received_len += chunk.len() as u64;
        if received_len > expected_len {
            send_error = Some(ApiError::bad_request(format!(
                "分片 {index} 数据超长: 期望 {expected_len} 字节"
            )));
            break;
        }
        if tx.send(chunk).await.is_err() {
            break;
        }
    }
    drop(tx);

    let written = writer
        .await
        .map_err(|e| ApiError::internal(format!("写入任务异常: {e}")))??;
    if let Some(err) = send_error {
        return Err(err);
    }
    if written != expected_len || received_len != expected_len {
        return Err(ApiError::bad_request(format!(
            "分片 {index} 长度不匹配: 收到 {received_len} 字节，期望 {expected_len} 字节"
        )));
    }

    // 标记分片完成（幂等：重复上传不会重复计数）
    {
        let session = state
            .sessions
            .get(&upload_id)
            .ok_or_else(|| ApiError::not_found("上传会话不存在或已过期"))?;
        let _ = record_received(&session, index);
    }

    let session = state
        .sessions
        .get(&upload_id)
        .ok_or_else(|| ApiError::not_found("上传会话不存在或已过期"))?;
    let status = UploadStatus {
        upload_id,
        name: session.name.clone(),
        size: session.size,
        chunk_size: session.chunk_size,
        chunk_count: session.chunk_count,
        received: session.received_count(),
        received_bytes: session.received_bytes.load(Ordering::Relaxed),
    };
    Ok(Json(status))
}

/// 合并分片：校验完整性后把临时文件移动到 files 目录并写入元数据。
async fn complete_upload(
    State(state): State<Arc<AppState>>,
    Path(upload_id): Path<Uuid>,
) -> ApiResult<Json<FileMeta>> {
    let (name, stored, size, part_path, received, chunk_count) = {
        let session = state
            .sessions
            .get(&upload_id)
            .ok_or_else(|| ApiError::not_found("上传会话不存在或已过期"))?;
        (
            session.name.clone(),
            session.stored.clone(),
            session.size,
            session.part_path.clone(),
            session.received_count(),
            session.chunk_count,
        )
    };

    if received as u64 != chunk_count {
        return Err(ApiError::conflict(format!(
            "分片未全部上传（{received}/{chunk_count}）"
        )));
    }

    let actual = tokio::fs::metadata(&part_path).await?.len();
    if actual != size {
        return Err(ApiError::conflict(format!(
            "文件长度校验失败: 实际 {actual} 字节，期望 {size} 字节"
        )));
    }

    let final_path = state.files_dir.join(&stored);
    tokio::fs::rename(&part_path, &final_path).await?;

    let mime = mime_guess::from_path(&name)
        .first_or_octet_stream()
        .to_string();
    let meta = FileMeta {
        id: upload_id,
        name: name.clone(),
        stored,
        size,
        mime,
        created_at: now_secs(),
    };

    {
        let state = state.clone();
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || state.persist_meta(&meta))
            .await
            .map_err(|e| ApiError::internal(format!("写入元数据失败: {e}")))??;
    }

    state.files.insert(upload_id, meta.clone());
    state.sessions.remove(&upload_id);
    state.broadcast(Event::UploadFinished {
        upload_id,
        file: meta.clone(),
    });
    tracing::info!("上传完成: {} ({} 字节)", meta.name, meta.size);
    Ok(Json(meta))
}

/// 取消上传并删除临时文件。
async fn abort_upload(
    State(state): State<Arc<AppState>>,
    Path(upload_id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (_, session) = state
        .sessions
        .remove(&upload_id)
        .ok_or_else(|| ApiError::not_found("上传会话不存在或已过期"))?;
    let _ = tokio::fs::remove_file(&session.part_path).await;
    state.broadcast(Event::UploadAborted { upload_id });
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- 下载

#[derive(Debug, Deserialize)]
struct DownloadQuery {
    /// 传 1 时以 inline 方式返回（便于浏览器直接预览音视频/图片）
    inline: Option<u8>,
}

async fn head_download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Query(_query): Query<DownloadQuery>,
) -> ApiResult<Response> {
    let meta = state
        .files
        .get(&id)
        .map(|f| f.value().clone())
        .ok_or_else(|| ApiError::not_found("文件不存在"))?;

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, meta.mime.as_str())
        .header(header::CONTENT_LENGTH, meta.size.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(&meta.name, true),
        )
        .body(Body::empty())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(response)
}

/// 支持 HTTP Range 的下载接口：浏览器可开多连接并行拉取不同区间。
async fn download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let meta = state
        .files
        .get(&id)
        .map(|f| f.value().clone())
        .ok_or_else(|| ApiError::not_found("文件不存在"))?;

    let path = state.files_dir.join(&meta.stored);
    let total = meta.size;
    let inline = query.inline == Some(1);

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|raw| parse_range(raw, total));

    let (status, start, end) = match range {
        None => (StatusCode::OK, 0u64, total.saturating_sub(1)),
        Some(Some((start, end))) => (StatusCode::PARTIAL_CONTENT, start, end),
        Some(None) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                .body(Body::empty())
                .map_err(|e| ApiError::internal(e.to_string()));
        }
    };

    let len = if total == 0 { 0 } else { end - start + 1 };
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let stream = ReaderStream::with_capacity(file.take(len), READ_BUFFER);

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, meta.mime.as_str())
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(&meta.name, !inline),
        );

    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }

    builder
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::internal(e.to_string()))
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (_, meta) = state
        .files
        .remove(&id)
        .ok_or_else(|| ApiError::not_found("文件不存在"))?;
    let _ = tokio::fs::remove_file(state.files_dir.join(&meta.stored)).await;
    state.remove_meta_file(&id);
    state.broadcast(Event::FileRemoved { id });
    tracing::info!("已删除文件: {}", meta.name);
    Ok(StatusCode::NO_CONTENT)
}

/// 解析单区间 `Range` 请求头；返回 `None` 表示不合法（416）。
fn parse_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    let spec = raw.trim().strip_prefix("bytes=")?.trim();
    // 只处理单区间请求，多区间回退为整体下载由调用方决定。
    if spec.contains(',') {
        return None;
    }
    let (start_raw, end_raw) = spec.split_once('-')?;
    if total == 0 {
        return None;
    }

    if start_raw.trim().is_empty() {
        let suffix: u64 = end_raw.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        return Some((start, total - 1));
    }

    let start: u64 = start_raw.trim().parse().ok()?;
    let end = if end_raw.trim().is_empty() {
        total - 1
    } else {
        end_raw.trim().parse::<u64>().ok()?
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end.min(total - 1)))
}

/// 生成 Content-Disposition，文件名使用 RFC 5987 编码以兼容中文。
fn content_disposition(name: &str, attachment: bool) -> HeaderValue {
    let kind = if attachment { "attachment" } else { "inline" };
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encode(name);
    let value = format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    HeaderValue::from_str(&value)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""))
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 去掉路径分隔符与控制字符，避免展示名异常或响应头注入。
pub fn sanitize_display_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let base = base.trim();
    let trimmed: String = base.chars().take(200).collect();
    if trimmed.is_empty() {
        "未命名文件".to_string()
    } else {
        trimmed
    }
}

/// 生成磁盘上的安全文件名（保留中文与常见字符）。
pub fn sanitize_file_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches([' ', '.'].as_slice());
    let limited: String = trimmed.chars().take(120).collect();
    if limited.is_empty() {
        "file".to_string()
    } else {
        limited
    }
}

/// 后台任务：定期清理长时间未完成的上传会话。
pub fn spawn_janitor(state: Arc<AppState>) {
    tokio::spawn(async move {
        let max_age = Duration::from_secs(60 * 60 * 6);
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let removed = state.purge_stale_sessions(max_age);
            if removed > 0 {
                tracing::info!("已清理 {removed} 个超时上传会话");
            }
        }
    });
}
