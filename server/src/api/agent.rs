//! Routes the Agent calls with its own credential: its control connection,
//! its screen thumbnail, its system information snapshots, and the outcome
//! of toolbox runs and deliveries, update installs and application runs.
//!
//! Anything large goes here rather than on the control connection: one
//! message there holds up every other, and the server takes an Agent it has
//! heard no whole frame from for [`crate::agents::Timeouts::silence`] as gone,
//! so a message that takes longer than that to arrive cuts the connection
//! however steadily it moves. A body here may take as long as it needs while
//! it keeps moving ([`crate::agents::Timeouts::upload_stall`]).
//!
//! The Agent keeps each outcome until one of these routes answers it with a
//! success, and sends it again otherwise, so an outcome the server already
//! has is answered with a success and changes nothing. One for a run already
//! counted as lost (or interrupted) replaces that: the run did happen. A
//! client error tells the Agent to drop the outcome: the device has no such
//! run, or the report is malformed.
use std::time::Duration;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use meshrmm_protocol_types::{
    ApplicationRunResult, FileDeliveryReport, FileDeliveryStatus, MAX_SCRIPT_OUTPUT_BYTES,
    MAX_SYSTEM_INFO_REPORT_BYTES, ScriptRunReport, ScriptRunStatus, SystemInfoReceipt,
    SystemInfoReport, UpdateInstallResult, truncate_output,
};
use sea_query::{Expr, ExprTrait, Query};

use crate::{
    agents::{self, AuthenticatedAgent, parse_id},
    automation::jobs::{self, Changed, RunTable},
    db::tables::{FileDeliveries, ScriptRuns},
    http::{ApiError, AppState, client_ip::ClientIp},
    realtime::coordinator,
    storage::Durability,
    system_info,
    time::now_ms,
    toolbox::{self, DELIVERY_WINDOW_MS},
};

/// Matches the Agent's limit. A 640 by 400 JPEG is normally 20–80 KiB.
pub const MAX_THUMBNAIL_BYTES: usize = 512 * 1024;
/// An Agent's report: two output streams at their limit, escaped as JSON,
/// and more than an install or application run result can be.
pub const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
const JPEG_MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];
const MAX_RAN_AS_CHARS: usize = 256;
const MAX_REPORTED_ERROR_CHARS: usize = 2000;
const MAX_PATH_CHARS: usize = 1024;

/// Why `bytes` cannot be a thumbnail the Agent encoded.
fn thumbnail_problem(bytes: &[u8]) -> Option<ApiError> {
    if bytes.len() > MAX_THUMBNAIL_BYTES {
        Some(too_large("the thumbnail is larger than 512 KiB"))
    } else if !bytes.starts_with(&JPEG_MAGIC) {
        Some(ApiError::bad_request("the thumbnail must be a JPEG"))
    } else {
        None
    }
}

fn too_large(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, message)
}

/// Reads a request body of at most `limit` bytes, refusing a larger one as
/// soon as it says or sends more, and giving up on one that sends nothing for
/// `stall`: there is no limit in total, so a slow body that keeps moving
/// arrives. Agent routes read their body only after authenticating, so
/// nobody else can make the server buffer one.
pub(super) async fn read_body(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
    too_large_message: &'static str,
    stall: Duration,
) -> Result<Bytes, ApiError> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<usize>().ok());
    if declared.is_some_and(|length| length > limit) {
        return Err(too_large(too_large_message));
    }
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::with_capacity(declared.unwrap_or_default());
    loop {
        let Ok(chunk) = tokio::time::timeout(stall, stream.next()).await else {
            return Err(ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "the request body stopped arriving",
            ));
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|_| ApiError::bad_request("the request body was interrupted"))?;
        if bytes.len() + chunk.len() > limit {
            return Err(too_large(too_large_message));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.into())
}

/// An Agent's JSON report, read after authenticating.
async fn read_report<T: serde::de::DeserializeOwned>(
    state: &AppState,
    headers: &HeaderMap,
    body: Body,
) -> Result<T, ApiError> {
    let bytes = read_body(
        headers,
        body,
        MAX_REPORT_BYTES,
        "the report is too large",
        state.agent_timeouts.upload_stall,
    )
    .await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ApiError::bad_request(format!("invalid report: {error}")))
}

/// Text from the Agent as the database can store it: PostgreSQL text can't
/// hold NUL, which program output sometimes contains.
fn storable(text: &str) -> String {
    text.replace('\0', "\u{fffd}")
}

/// The largest message an Agent may send on its control connection, which
/// the server buffers whole before it reads it. The largest the Agent sends
/// are scans: an application scan lists at most 5,000 packages and an update
/// scan at most 5,000 updates, typically 100 to 300 bytes each (this takes
/// 5,000 at 800 bytes), then a page of a file listing, at most 1,000 entries,
/// and monitor results, at most 512 KiB of text a message. Snapshots, which
/// are larger, are posted over HTTPS ([`report_system_info`]).
pub const MAX_CONTROL_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// The control connection's read buffer, which every connected Agent holds
/// (tungstenite's default is 128 KiB). A metrics report, sent every few
/// seconds, fits in one read; a larger message, such as a system
/// information snapshot, grows the buffer to its size and arrives over
/// several reads, so this bounds each read, not the message.
const CONTROL_READ_BUFFER_BYTES: usize = 4 * 1024;

/// `GET /v1/agents/{id}/connect`: the Agent's control connection, which it
/// keeps open while it runs. See [`coordinator`].
pub async fn connect(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let agent = agents::authenticate(&state, &headers, &id).await?;
    Ok(upgrade
        .max_message_size(MAX_CONTROL_MESSAGE_BYTES)
        .read_buffer_size(CONTROL_READ_BUFFER_BYTES)
        .on_upgrade(move |socket| {
            coordinator::serve(state, agent.device_id, agent.deletion_requested, ip, socket)
        }))
}

/// `PUT /v1/agents/{id}/thumbnail`: replaces the device's screen thumbnail.
pub async fn put_thumbnail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let agent = agents::authenticate(&state, &headers, &id).await?;
    // A device being removed must not put back the image its deletion removed.
    if agent.deletion_requested {
        return Err(ApiError::conflict("the device is being removed"));
    }
    let bytes = read_body(
        &headers,
        body,
        MAX_THUMBNAIL_BYTES,
        "the thumbnail is larger than 512 KiB",
        state.agent_timeouts.upload_stall,
    )
    .await?;
    if let Some(problem) = thumbnail_problem(&bytes) {
        return Err(problem);
    }
    let path = state.storage.thumbnail(&agent.device_id);
    // Not flushed to disk: an image a crash loses comes back with the
    // Agent's first capture after it reconnects, which it sends even when
    // the screen looks the same, and a large fleet sends many a second.
    state
        .storage
        .write(path.clone(), bytes, Durability::Replaceable)
        .await
        .map_err(anyhow::Error::from)?;
    // The device may have been deleted while the image was written.
    if !agents::is_active(&mut &state.database, &agent.device_id).await? {
        state
            .storage
            .remove(&path)
            .await
            .map_err(anyhow::Error::from)?;
        return Err(ApiError::conflict("the device is being removed"));
    }
    // A tab showing the device loads the new image now.
    state.metrics.thumbnail_replaced(&agent.device_id);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/agents/{id}/system-info`: the device's new system information
/// snapshot, which replaces its last one. The answer says which application
/// list the server now holds, and whether it wants a new snapshot to send
/// the list it lacks. Taken whether or not the Agent's control connection is
/// up: the credential is what counts.
pub async fn report_system_info(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<SystemInfoReceipt>, ApiError> {
    let agent = agents::authenticate(&state, &headers, &id).await?;
    let removed = || ApiError::conflict("the device is being removed");
    if agent.deletion_requested {
        return Err(removed());
    }
    let bytes = read_body(
        &headers,
        body,
        MAX_SYSTEM_INFO_REPORT_BYTES,
        "the snapshot is larger than its limit",
        state.agent_timeouts.upload_stall,
    )
    .await?;
    let report: SystemInfoReport = serde_json::from_slice(&bytes)
        .map_err(|error| ApiError::bad_request(format!("invalid snapshot: {error}")))?;
    drop(bytes);
    let device_id = agent.device_id;
    let named = report.applications_unchanged.clone();
    let stored = system_info::store(
        &state.database,
        &device_id,
        ip,
        report.system,
        report.applications_unchanged,
    )
    .await?
    .ok_or_else(removed)?;
    tracing::info!(device_id, applications = ?stored, "stored the device's system information");
    // The device list shows what changed.
    state.presence.refresh(&device_id).await;
    Ok(Json(match stored {
        system_info::Stored::Listed { applications_hash } => SystemInfoReceipt {
            applications_hash: Some(applications_hash),
            collect_again: false,
        },
        // The list the snapshot named.
        system_info::Stored::Unchanged => SystemInfoReceipt {
            applications_hash: named,
            collect_again: false,
        },
        system_info::Stored::Unknown { applications_hash } => SystemInfoReceipt {
            applications_hash,
            collect_again: true,
        },
    }))
}

fn bounded(text: Option<String>, max_chars: usize) -> Option<String> {
    text.map(|text| storable(&text).chars().take(max_chars).collect())
}

/// Authenticates the Agent and checks the ID of the run or delivery it
/// reports on.
async fn reporting_agent(
    state: &AppState,
    headers: &HeaderMap,
    device_id: &str,
    id: &str,
    what: &str,
) -> Result<(AuthenticatedAgent, String), ApiError> {
    let agent = agents::authenticate(state, headers, device_id).await?;
    Ok((agent, parse_id(id, what)?))
}

/// `POST /v1/agents/{id}/script-runs/{run_id}/result`: how a run finished.
pub async fn report_script_run(
    State(state): State<AppState>,
    Path((device_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let (agent, run_id) = reporting_agent(&state, &headers, &device_id, &run_id, "run").await?;
    let report: ScriptRunReport = read_report(&state, &headers, body).await?;
    if !matches!(
        report.status,
        ScriptRunStatus::Completed | ScriptRunStatus::Failed | ScriptRunStatus::TimedOut
    ) {
        return Err(ApiError::bad_request("a run reports how it finished"));
    }
    let (stdout, stderr) = (storable(&report.stdout), storable(&report.stderr));
    let (stdout, stdout_cut) = truncate_output(&stdout, MAX_SCRIPT_OUTPUT_BYTES);
    let (stderr, stderr_cut) = truncate_output(&stderr, MAX_SCRIPT_OUTPUT_BYTES);
    let ran_as: String = storable(&report.ran_as)
        .chars()
        .take(MAX_RAN_AS_CHARS)
        .collect();
    let now = now_ms();
    // The run and its job device's outcome change together.
    let mut transaction = state.database.begin().await?;
    let updated = transaction
        .execute(
            &Query::update()
                .table(ScriptRuns::Table)
                .value(ScriptRuns::Status, report.status.as_str())
                .value(ScriptRuns::RanAs, ran_as)
                .value(ScriptRuns::ExitCode, report.exit_code.map(i64::from))
                .value(ScriptRuns::Stdout, stdout)
                .value(ScriptRuns::Stderr, stderr)
                .value(
                    ScriptRuns::OutputTruncated,
                    report.output_truncated || stdout_cut || stderr_cut,
                )
                .value(
                    ScriptRuns::Error,
                    bounded(report.error, MAX_REPORTED_ERROR_CHARS),
                )
                .value(ScriptRuns::CompletedAt, now)
                .and_where(Expr::col(ScriptRuns::Id).eq(run_id.as_str()))
                .and_where(Expr::col(ScriptRuns::DeviceId).eq(agent.device_id.as_str()))
                .and_where(Expr::col(ScriptRuns::Status).is_in([
                    ScriptRunStatus::Pending.as_str(),
                    ScriptRunStatus::Lost.as_str(),
                ]))
                .to_owned(),
        )
        .await?;
    if updated == 0 {
        drop(transaction);
        let known = Query::select()
            .column(ScriptRuns::Id)
            .from(ScriptRuns::Table)
            .and_where(Expr::col(ScriptRuns::Id).eq(run_id.as_str()))
            .and_where(Expr::col(ScriptRuns::DeviceId).eq(agent.device_id.as_str()))
            .to_owned();
        return reported_before(&state, known, "no run has this ID").await;
    }
    jobs::runs_changed(
        &mut transaction,
        RunTable::Script,
        Changed::Id(&run_id),
        now,
    )
    .await?;
    transaction.commit().await?;
    crate::alerts::script_run_finished(&state, &run_id).await;
    // A job's counts changed, and its device may take the next run.
    state.automation.device_changed(&agent.device_id);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/agents/{id}/file-deliveries/{delivery_id}/content`: the Agent
/// downloads a file it was asked to save, while the delivery is pending.
pub async fn delivery_content(
    State(state): State<AppState>,
    Path((device_id, delivery_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (agent, delivery_id) =
        reporting_agent(&state, &headers, &device_id, &delivery_id, "delivery").await?;
    let mut database = &state.database;
    let (file_id,): (String,) = database
        .fetch_optional(
            &Query::select()
                .column(FileDeliveries::FileId)
                .from(FileDeliveries::Table)
                .and_where(Expr::col(FileDeliveries::Id).eq(delivery_id.as_str()))
                .and_where(Expr::col(FileDeliveries::DeviceId).eq(agent.device_id.as_str()))
                .and_where(
                    Expr::col(FileDeliveries::Status).eq(FileDeliveryStatus::Pending.as_str()),
                )
                .and_where(Expr::col(FileDeliveries::CreatedAt).gt(now_ms() - DELIVERY_WINDOW_MS))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| ApiError::not_found("no pending delivery has this ID"))?;
    let gone = || ApiError::not_found("the file is no longer in the toolbox");
    if toolbox::file_by_id(&mut database, &file_id)
        .await?
        .is_none()
    {
        return Err(gone());
    }
    file_download(&state, &file_id).await?.ok_or_else(gone)
}

/// A stored library file as a download, or `None` if its content is gone.
pub async fn file_download(state: &AppState, file_id: &str) -> Result<Option<Response>, ApiError> {
    let file = match tokio::fs::File::open(state.storage.toolbox_file(file_id)).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow::Error::from(error).into()),
    };
    let length = file.metadata().await.map_err(anyhow::Error::from)?.len();
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    Ok(Some(
        (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                ),
                (header::CONTENT_LENGTH, HeaderValue::from(length)),
                (
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment"),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, no-store"),
                ),
            ],
            body,
        )
            .into_response(),
    ))
}

/// `POST /v1/agents/{id}/file-deliveries/{delivery_id}/result`: how a
/// delivery finished.
pub async fn report_file_delivery(
    State(state): State<AppState>,
    Path((device_id, delivery_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let (agent, delivery_id) =
        reporting_agent(&state, &headers, &device_id, &delivery_id, "delivery").await?;
    let report: FileDeliveryReport = read_report(&state, &headers, body).await?;
    if !matches!(
        report.status,
        FileDeliveryStatus::Delivered | FileDeliveryStatus::Failed
    ) {
        return Err(ApiError::bad_request("a delivery reports how it finished"));
    }
    let now = now_ms();
    let mut transaction = state.database.begin().await?;
    let updated = transaction
        .execute(
            &Query::update()
                .table(FileDeliveries::Table)
                .value(FileDeliveries::Status, report.status.as_str())
                .value(FileDeliveries::Path, bounded(report.path, MAX_PATH_CHARS))
                .value(
                    FileDeliveries::Error,
                    bounded(report.error, MAX_REPORTED_ERROR_CHARS),
                )
                .value(FileDeliveries::CompletedAt, now)
                .and_where(Expr::col(FileDeliveries::Id).eq(delivery_id.as_str()))
                .and_where(Expr::col(FileDeliveries::DeviceId).eq(agent.device_id.as_str()))
                // A delivery past its window is only shown as lost; it stays
                // pending here, so its late outcome is still taken.
                .and_where(
                    Expr::col(FileDeliveries::Status).eq(FileDeliveryStatus::Pending.as_str()),
                )
                .to_owned(),
        )
        .await?;
    if updated == 0 {
        drop(transaction);
        let known = Query::select()
            .column(FileDeliveries::Id)
            .from(FileDeliveries::Table)
            .and_where(Expr::col(FileDeliveries::Id).eq(delivery_id.as_str()))
            .and_where(Expr::col(FileDeliveries::DeviceId).eq(agent.device_id.as_str()))
            .to_owned();
        return reported_before(&state, known, "no delivery has this ID").await;
    }
    jobs::runs_changed(
        &mut transaction,
        RunTable::Delivery,
        Changed::Id(&delivery_id),
        now,
    )
    .await?;
    transaction.commit().await?;
    state.automation.device_changed(&agent.device_id);
    Ok(StatusCode::NO_CONTENT)
}

/// The answer to a toolbox outcome that changed nothing: a success if
/// `known` finds the device's run or delivery, which has finished already,
/// so the outcome was reported before, and `unknown` otherwise.
async fn reported_before(
    state: &AppState,
    known: sea_query::SelectStatement,
    unknown: &'static str,
) -> Result<StatusCode, ApiError> {
    let found: Option<(String,)> = state.database.fetch_optional(&known).await?;
    match found {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError::not_found(unknown)),
    }
}

/// `POST /v1/agents/{id}/update-runs/{run_id}/result`: how an install
/// finished.
pub async fn report_update_install(
    State(state): State<AppState>,
    Path((device_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let (agent, run_id) = reporting_agent(&state, &headers, &device_id, &run_id, "run").await?;
    let result: UpdateInstallResult = read_report(&state, &headers, body).await?;
    if crate::updates::install_reported(&state, &agent.device_id, &run_id, result).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no install has this ID"))
    }
}

/// `POST /v1/agents/{id}/application-runs/{run_id}/result`: how an
/// application run finished.
pub async fn report_application_run(
    State(state): State<AppState>,
    Path((device_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let (agent, run_id) = reporting_agent(&state, &headers, &device_id, &run_id, "run").await?;
    let result: ApplicationRunResult = read_report(&state, &headers, body).await?;
    if crate::applications::result_reported(&state, &agent.device_id, &run_id, result).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no application run has this ID"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALL: Duration = Duration::from_secs(60);

    /// A body whose chunks the test sends, and which ends when the sender
    /// is dropped.
    fn gated() -> (
        tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
        Body,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|chunk| (chunk, receiver))
        });
        (sender, Body::from_stream(stream))
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_body_that_keeps_moving_arrives_however_long_it_takes() {
        let (sender, body) = gated();
        let reading =
            tokio::spawn(
                async move { read_body(&HeaderMap::new(), body, 1024, "large", STALL).await },
            );
        // Ten times the stall limit in all, a chunk just inside it each time.
        for _ in 0..10 {
            tokio::time::sleep(STALL - Duration::from_secs(1)).await;
            sender
                .send(Ok(Bytes::from_static(b"0123456789")))
                .await
                .unwrap();
        }
        drop(sender);
        let bytes = reading.await.unwrap().unwrap();
        assert_eq!(bytes.len(), 100);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_stops_is_dropped_after_the_stall_limit() {
        let (sender, body) = gated();
        let started = tokio::time::Instant::now();
        let reading =
            tokio::spawn(
                async move { read_body(&HeaderMap::new(), body, 1024, "large", STALL).await },
            );
        sender
            .send(Ok(Bytes::from_static(b"0123456789")))
            .await
            .unwrap();
        // The sender stays open, sending nothing more.
        let error = reading.await.unwrap().unwrap_err();
        assert_eq!(error.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(started.elapsed(), STALL);
        drop(sender);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_past_its_limit_is_refused_before_it_ends() {
        let (sender, body) = gated();
        let reading =
            tokio::spawn(
                async move { read_body(&HeaderMap::new(), body, 25, "large", STALL).await },
            );
        for _ in 0..3 {
            sender
                .send(Ok(Bytes::from_static(b"0123456789")))
                .await
                .unwrap();
        }
        // Refused at once, with the body still open and more to come.
        let error = tokio::time::timeout(Duration::from_secs(1), reading)
            .await
            .expect("refused without waiting for the rest")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // One that says it is too large is refused before any of it is read.
        let (sender, body) = gated();
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(26));
        let error = read_body(&headers, body, 25, "large", STALL)
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
        drop(sender);
    }

    #[test]
    fn nul_is_replaced_so_postgresql_can_store_it() {
        assert_eq!(storable("a\0b"), "a\u{fffd}b");
        assert_eq!(storable("plain"), "plain");
    }

    #[test]
    fn thumbnails_must_be_small_jpegs() {
        assert!(thumbnail_problem(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]).is_none());
        let status = |bytes: &[u8]| thumbnail_problem(bytes).map(|problem| problem.status());
        assert_eq!(status(b"\x89PNG\r\n\x1a\n"), Some(StatusCode::BAD_REQUEST));
        assert_eq!(status(&[]), Some(StatusCode::BAD_REQUEST));
        let mut large = vec![0; MAX_THUMBNAIL_BYTES + 1];
        large[..3].copy_from_slice(&JPEG_MAGIC);
        assert_eq!(status(&large), Some(StatusCode::PAYLOAD_TOO_LARGE));
        large.truncate(MAX_THUMBNAIL_BYTES);
        assert_eq!(status(&large), None);
    }
}
