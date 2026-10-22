//! Streams a transfer between the Agent's socket and the browser's
//! request, holding one piece at a time.
//!
//! A download's response starts once the Agent has opened the file, so an
//! error before then (access denied, gone) is a normal error response. Its
//! body is the Agent's pieces as they arrive: hyper asks for the next only
//! when the browser has taken the last, so a slow browser slows the Agent
//! through TCP. The server works out the SHA-256 of what it relayed and
//! checks it, and the byte count, against what the Agent says it sent; a
//! mismatch, or the device going away, aborts the response, so the browser
//! marks the download failed rather than saving a bad file. Files can be
//! resumed with a `Range` request.
//!
//! An upload's body goes to the Agent piece by piece, each sent only when
//! the Agent's socket has taken the one before, so the browser waits on the
//! device's disk. The Agent says how much its partial file holds at the end.
use std::time::Duration;

use axum::{
    body::Body,
    extract::ws::{Message, WebSocket},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol_types::{
    AgentCommand, FILE_STREAM_CHUNK_BYTES, FileDownloadKind, FileStreamControl, FileStreamEvent,
    FileTransfer, FileTransferRequest, FileVersion,
};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    Download, Progress, ProgressState, TransferClaim, Upload, agent_error, device_offline,
    transfer_limit,
};
use crate::{
    audit::{self, Target},
    http::{ApiError, AppState},
};

/// A peer that takes this long to send or take a piece is gone. Long,
/// since a browser may pause a download.
const STALL: Duration = Duration::from_secs(10 * 60);
/// How long the Agent gets to save an upload's last pieces.
const FINISH_WAIT: Duration = Duration::from_secs(120);
/// At most this many paths go in an audit event.
const AUDITED_PATHS: usize = 100;

/// The part of a file a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    /// The last byte, inclusive.
    pub end: Option<u64>,
}

/// Reads a `Range` header for one range of `size` bytes: `bytes=N-`,
/// `bytes=N-M` or `bytes=-N`. Anything else asks for the whole file.
pub fn parse_range(value: &str, size: u64) -> Option<Result<Range, ()>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    let range = if first.is_empty() {
        let suffix = last.parse::<u64>().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        Range {
            start: size.saturating_sub(suffix),
            end: None,
        }
    } else {
        let start = first.parse::<u64>().ok()?;
        let end = if last.is_empty() {
            None
        } else {
            let end = last.parse::<u64>().ok()?;
            if end < start {
                return None;
            }
            Some(end.min(size.saturating_sub(1)))
        };
        Range { start, end }
    };
    if range.start >= size {
        return Some(Err(()));
    }
    Some(Ok(range))
}

/// A file version's entity tag.
fn entity_tag(size: u64, modified: Option<i64>) -> String {
    format!("\"{size}-{}\"", modified.unwrap_or_default())
}

/// The next JSON event on the Agent's socket; `None` when it closed.
async fn next_event(socket: &mut WebSocket) -> Option<FileStreamEvent> {
    loop {
        match tokio::time::timeout(STALL, socket.recv()).await {
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(&text).ok(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            _ => return None,
        }
    }
}

fn disconnected() -> ApiError {
    ApiError::conflict(
        "the device disconnected during the transfer; try again once it's back online",
    )
    .with_code("device_disconnected")
}

/// Sends the Agent `transfer` and waits for its socket.
async fn connect_agent(
    state: &AppState,
    claim: &mut TransferClaim,
    device_id: &str,
    run_as: meshrmm_protocol_types::RunAs,
    transfer: FileTransfer,
) -> Result<WebSocket, ApiError> {
    let command = AgentCommand::FileTransfer {
        transfer: FileTransferRequest {
            transfer_id: claim.id.clone(),
            run_as,
            transfer,
        },
    };
    if !state.agents.send(device_id, command) {
        return Err(device_offline());
    }
    let arrived = claim.agent.take().expect("a new claim waits for the Agent");
    let offline = claim.offline.clone();
    tokio::select! {
        arrived = tokio::time::timeout(state.files.timeouts().agent_connect, arrived) => match arrived {
            Ok(Ok(socket)) => Ok(socket),
            _ => Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "the device didn't start the transfer; try again",
            )
            .with_code("device_timeout")),
        },
        () = offline.cancelled() => Err(disconnected()),
    }
}

/// The `Content-Disposition` for saving as `name`: plain ASCII for old
/// clients, and the exact name in UTF-8.
fn disposition(name: &str) -> HeaderValue {
    let plain = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let encoded = name
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{plain}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// `GET /v1/file-downloads/{id}`'s response: the file from where `range`
/// asks, or the archive.
pub async fn download(
    state: &AppState,
    download: Download,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let file = download.info.kind == FileDownloadKind::File;
    let full_tag = entity_tag(download.info.size, download.info.modified);
    // A resumed download goes on only from the version it started with.
    let still_same = headers
        .get(header::IF_RANGE)
        .is_none_or(|value| value.as_bytes() == full_tag.as_bytes());
    let range = match headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if file && still_same => parse_range(value, download.info.size),
        _ => None,
    };
    let range = match range {
        Some(Ok(range)) => Some(range),
        Some(Err(())) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{}", download.info.size))
                        .expect("a valid header"),
                )],
            )
                .into_response());
        }
        None => None,
    };
    let max_bytes = transfer_limit(state).await?;
    let mut claim = state
        .files
        .reserve(&download.device_id, &download.user_id)?;
    let offset = range.map_or(0, |range| range.start);
    let length = range.and_then(|range| range.end.map(|end| end - range.start + 1));
    let transfer = FileTransfer::Download {
        paths: download.paths.clone(),
        kind: download.info.kind,
        offset,
        length,
        expect: range.map(|_| FileVersion {
            size: download.info.size,
            modified: download.info.modified,
        }),
        max_bytes,
    };
    state.files.set_progress(&download.id, |progress| {
        *progress = Progress {
            state: ProgressState::Sending,
            offset,
            ..Progress::default()
        }
    });
    let started = connect_agent(
        state,
        &mut claim,
        &download.device_id,
        download.run_as,
        transfer,
    )
    .await;
    let mut socket = match started {
        Ok(socket) => socket,
        Err(error) => {
            fail_progress(state, &download.id, error.message());
            return Err(error);
        }
    };
    let (size, modified) = match next_event(&mut socket).await {
        Some(FileStreamEvent::Begin { size, modified }) => (size, modified),
        Some(FileStreamEvent::Failed { error }) => {
            let error = agent_error(error);
            fail_progress(state, &download.id, error.message());
            return Err(error);
        }
        _ => {
            fail_progress(state, &download.id, "the device disconnected");
            return Err(disconnected());
        }
    };
    if file && size.is_none() {
        return Err(disconnected());
    }
    state
        .files
        .set_progress(&download.id, |progress| progress.size = size);

    let name = match download.info.kind {
        FileDownloadKind::File => download.info.name.clone(),
        FileDownloadKind::Archive => format!("{}.zip", download.info.name),
    };
    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_DISPOSITION, disposition(&name));
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    let mut status = StatusCode::OK;
    if file {
        let size = size.expect("checked above");
        response_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
        response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        let total = if range.is_some() {
            download.info.size
        } else {
            size
        };
        if let Ok(tag) = HeaderValue::from_str(&entity_tag(total, modified)) {
            response_headers.insert(header::ETAG, tag);
        }
        if range.is_some() {
            status = StatusCode::PARTIAL_CONTENT;
            let last = (offset + size).saturating_sub(1);
            response_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {offset}-{last}/{total}"))
                    .expect("a valid header"),
            );
        }
    } else {
        response_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/zip"),
        );
        response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    }
    let sending = Sending {
        state: state.clone(),
        socket,
        claim,
        download,
        offset,
        expected: size,
        hasher: Sha256::new(),
        bytes: 0,
        finished: false,
    };
    let body = Body::from_stream(futures_util::stream::unfold(sending, Sending::next));
    Ok((status, response_headers, body).into_response())
}

fn fail_progress(state: &AppState, id: &str, error: &str) {
    state.files.set_progress(id, |progress| {
        progress.state = ProgressState::Failed;
        progress.error = Some(error.to_owned());
    });
}

/// A download's body as it streams. Dropped before the end, as when the
/// browser goes away, it records the download as cancelled.
struct Sending {
    state: AppState,
    socket: WebSocket,
    claim: TransferClaim,
    download: Download,
    offset: u64,
    expected: Option<u64>,
    hasher: Sha256,
    bytes: u64,
    finished: bool,
}

type Piece = Result<Bytes, std::io::Error>;

impl Sending {
    async fn next(mut self) -> Option<(Piece, Self)> {
        if self.finished {
            return None;
        }
        loop {
            let offline = self.claim.offline.clone();
            let frame = tokio::select! {
                frame = tokio::time::timeout(STALL, self.socket.recv()) => frame,
                () = offline.cancelled() => return self.fail("the device went offline"),
            };
            match frame {
                Ok(Some(Ok(Message::Binary(bytes)))) => {
                    let total = self.bytes + bytes.len() as u64;
                    if self.expected.is_some_and(|expected| total > expected) {
                        return self.fail("the device sent more than the file holds");
                    }
                    self.hasher.update(&bytes);
                    self.bytes = total;
                    self.state
                        .files
                        .set_progress(&self.download.id, |progress| progress.bytes = total);
                    return Some((Ok(bytes), self));
                }
                Ok(Some(Ok(Message::Text(text)))) => {
                    return match serde_json::from_str::<FileStreamEvent>(&text) {
                        Ok(FileStreamEvent::Done { bytes, sha256 }) => {
                            let relayed = hex(&std::mem::take(&mut self.hasher).finalize());
                            if bytes != self.bytes
                                || self.expected.is_some_and(|expected| expected != self.bytes)
                            {
                                self.fail("the device sent a different amount than it said")
                            } else if sha256.as_deref() != Some(relayed.as_str()) {
                                self.fail("what arrived didn't match the device's SHA-256")
                            } else {
                                self.complete(relayed);
                                None
                            }
                        }
                        Ok(FileStreamEvent::Failed { error }) => {
                            self.fail(&error.sanitized().message)
                        }
                        _ => self.fail("the device sent something unexpected"),
                    };
                }
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
                _ => return self.fail("the device disconnected"),
            }
        }
    }

    /// Ends the body with an error, which aborts the response.
    fn fail(mut self, error: &str) -> Option<(Piece, Self)> {
        self.finished = true;
        fail_progress(&self.state, &self.download.id, error);
        self.record("failed", None, Some(error));
        Some((Err(std::io::Error::other(error.to_owned())), self))
    }

    fn complete(&mut self, sha256: String) {
        self.finished = true;
        let bytes = self.bytes;
        let digest = sha256.clone();
        self.state
            .files
            .set_progress(&self.download.id, |progress| {
                progress.state = ProgressState::Done;
                progress.bytes = bytes;
                progress.sha256 = Some(digest);
            });
        self.record("completed", Some(sha256), None);
    }

    fn record(&self, result: &str, sha256: Option<String>, error: Option<&str>) {
        let state = self.state.clone();
        let download = self.download.clone();
        let metadata = json!({
            "paths": download.paths.iter().take(AUDITED_PATHS).collect::<Vec<_>>(),
            "path_count": download.paths.len(),
            // Not `name`, which the audit log reads as the target's.
            "file_name": download.info.name,
            "kind": match download.info.kind {
                FileDownloadKind::File => "file",
                FileDownloadKind::Archive => "archive",
            },
            "run_as": download.run_as.as_str(),
            "offset": self.offset,
            "bytes": self.bytes,
            "size": download.info.size,
            "sha256": sha256,
            "result": result,
            "error": error,
        });
        tracing::info!(
            download_id = download.id,
            device_id = download.device_id,
            result,
            bytes = self.bytes,
            "a file download ended"
        );
        tokio::spawn(async move {
            record(
                &state,
                &download.actor,
                &download.device_id,
                "files.download",
                metadata,
            )
            .await;
        });
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        if !self.finished {
            self.finished = true;
            fail_progress(&self.state, &self.download.id, "the download was stopped");
            self.record("cancelled", None, Some("the download was stopped"));
        }
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Records `action` on the device; a failure only logs.
pub async fn record(
    state: &AppState,
    actor: &crate::audit::Actor,
    device_id: &str,
    action: &str,
    metadata: serde_json::Value,
) {
    if let Err(error) = audit::record(
        &mut &state.database,
        actor,
        action,
        Target::device(device_id),
        metadata,
    )
    .await
    {
        tracing::warn!(%error, device_id, action, "could not record a file operation in the audit log");
    }
}

/// `PUT /v1/file-uploads/{id}`: sends `body`, the upload's bytes from
/// `offset`, to the device. Returns how many bytes its partial file holds.
pub async fn upload(
    state: &AppState,
    upload: &Upload,
    offset: u64,
    body: Body,
) -> Result<u64, ApiError> {
    if offset > upload.size {
        return Err(ApiError::bad_request(
            "the offset is past the end of the upload",
        ));
    }
    let mut claim = state.files.reserve(&upload.device_id, &upload.user_id)?;
    let transfer = FileTransfer::Upload {
        upload_id: upload.id.clone(),
        path: upload.path.clone(),
        offset,
        size: upload.size,
    };
    let socket = connect_agent(
        state,
        &mut claim,
        &upload.device_id,
        upload.run_as,
        transfer,
    )
    .await?;
    let mut socket = socket;
    match next_event(&mut socket).await {
        Some(FileStreamEvent::Ready { received }) if received == offset => {}
        Some(FileStreamEvent::Failed { error }) => return Err(agent_error(error)),
        Some(FileStreamEvent::Ready { received }) => {
            return Err(ApiError::conflict(format!(
                "the device has {received} bytes of this upload, not {offset}"
            ))
            .with_code("offset_mismatch"));
        }
        _ => return Err(disconnected()),
    }
    let (mut agent_tx, mut agent_rx) = socket.split();
    let mut body = body.into_data_stream();
    let room = upload.size - offset;
    let pump = async {
        let mut sent = 0u64;
        while let Some(frame) = body.next().await {
            let Ok(bytes) = frame else {
                return Err(Pumped::BrowserLeft);
            };
            let mut start = 0;
            while start < bytes.len() {
                let end = (start + FILE_STREAM_CHUNK_BYTES).min(bytes.len());
                sent += (end - start) as u64;
                if sent > room {
                    return Err(Pumped::TooMuch);
                }
                let piece = Message::Binary(bytes.slice(start..end));
                if !matches!(
                    tokio::time::timeout(STALL, agent_tx.send(piece)).await,
                    Ok(Ok(()))
                ) {
                    return Err(Pumped::AgentLeft);
                }
                start = end;
            }
        }
        let end = serde_json::to_string(&FileStreamControl::End).expect("serializes");
        match tokio::time::timeout(STALL, agent_tx.send(Message::Text(end.into()))).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(Pumped::AgentLeft),
        }
    };
    // The Agent may fail partway, as when the disk fills.
    let early = async {
        loop {
            match agent_rx.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<FileStreamEvent>(&text).ok();
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    };
    let offline = claim.offline.clone();
    let pumped = tokio::select! {
        pumped = pump => pumped,
        event = early => {
            return match event {
                Some(FileStreamEvent::Failed { error }) => Err(agent_error(error)),
                _ => Err(disconnected()),
            };
        }
        () = offline.cancelled() => return Err(disconnected()),
    };
    match pumped {
        Ok(()) => {}
        Err(Pumped::BrowserLeft) => {
            return Err(
                ApiError::bad_request("the upload was interrupted; resume it")
                    .with_code("upload_interrupted"),
            );
        }
        Err(Pumped::TooMuch) => {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the request carries more bytes than the upload announced",
            )
            .with_code("too_large"));
        }
        Err(Pumped::AgentLeft) => {}
    }
    let finished = tokio::time::timeout(FINISH_WAIT, async {
        let offline = claim.offline.clone();
        loop {
            let frame = tokio::select! {
                frame = agent_rx.next() => frame,
                () = offline.cancelled() => return None,
            };
            match frame {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<FileStreamEvent>(&text).ok();
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    })
    .await;
    match finished {
        Ok(Some(FileStreamEvent::Done { bytes, .. })) => Ok(bytes),
        Ok(Some(FileStreamEvent::Failed { error })) => Err(agent_error(error)),
        _ => Err(disconnected()),
    }
}

/// Why sending an upload's body stopped.
enum Pumped {
    BrowserLeft,
    AgentLeft,
    TooMuch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_read_as_browsers_resume_downloads() {
        let range = |value| parse_range(value, 1000);
        assert_eq!(
            range("bytes=100-"),
            Some(Ok(Range {
                start: 100,
                end: None
            }))
        );
        assert_eq!(
            range("bytes=0-499"),
            Some(Ok(Range {
                start: 0,
                end: Some(499)
            }))
        );
        assert_eq!(
            range("bytes=900-5000"),
            Some(Ok(Range {
                start: 900,
                end: Some(999)
            }))
        );
        assert_eq!(
            range("bytes=-100"),
            Some(Ok(Range {
                start: 900,
                end: None
            }))
        );
        assert_eq!(range("bytes=1000-"), Some(Err(())));
        assert_eq!(range("bytes=0-1,5-6"), None);
        assert_eq!(range("items=0-1"), None);
        assert_eq!(range("bytes=5-1"), None);
    }

    #[test]
    fn names_are_offered_safely() {
        assert_eq!(
            disposition("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
        assert_eq!(
            disposition("naïve \"x\".txt"),
            "attachment; filename=\"na_ve _x_.txt\"; filename*=UTF-8''na%C3%AFve%20%22x%22.txt"
        );
        assert_eq!(entity_tag(5, Some(9)), "\"5-9\"");
    }
}
