use std::{
    collections::HashMap,
    convert::Infallible,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{
        Html, IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use chrono::{DateTime, Utc};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs,
    process::Command,
    sync::{Mutex, broadcast},
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    signature::{actionable_signatures, parse_pdfsig_output},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    NotSigned,
    All,
}

#[derive(Debug, Clone, Serialize)]
pub struct InventoryCounts {
    pub pdf_total: usize,
    pub signed_pdf: usize,
    pub unsigned_pdf: usize,
    pub non_pdf: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningScan {
    pub mode: ScanMode,
    pub phase: String,
    pub total: Option<usize>,
    pub processed: usize,
    pub signed_found: usize,
    pub newly_signed: usize,
    pub failed: usize,
    pub current_document_id: Option<i64>,
    pub current_document_title: Option<String>,
    pub started_at: DateTime<Utc>,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub mode: ScanMode,
    pub scanned: usize,
    pub signed_found: usize,
    pub newly_signed: usize,
    pub failed: usize,
    pub finished_at: DateTime<Utc>,
    pub canceled: bool,
    pub fatal_error: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanSnapshot {
    pub configured: bool,
    pub counts: InventoryCounts,
    pub running: Option<RunningScan>,
    pub last_result: Option<ScanResult>,
    pub last_error: Option<String>,
    pub refreshed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanStartResponse {
    pub status: &'static str,
    pub snapshot: ScanSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanCancelResponse {
    pub status: &'static str,
    pub snapshot: ScanSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanStateResponse {
    pub snapshot: ScanSnapshot,
}

#[derive(Debug, Deserialize)]
pub struct ScanStateQuery {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Deserialize)]
pub struct ScanStartRequest {
    pub mode: ScanMode,
}

#[derive(Debug)]
pub enum ScanStartError {
    AlreadyRunning(ScanSnapshot),
    PaperlessUnavailable(ScanSnapshot),
    NotConfigured(ScanSnapshot),
}

#[derive(Debug)]
pub enum ScanCancelError {
    NotRunning(ScanSnapshot),
}

#[derive(Debug)]
struct SharedState {
    counts: InventoryCounts,
    running: Option<RunningScan>,
    last_result: Option<ScanResult>,
    last_error: Option<String>,
    refreshed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct InventoryDocument {
    id: i64,
    title: Option<String>,
    mime_type: Option<String>,
    signed: bool,
}

#[derive(Debug)]
pub struct ScanCoordinator {
    state: Mutex<SharedState>,
    updates: broadcast::Sender<()>,
}

impl ScanCoordinator {
    pub fn new() -> Self {
        let (updates, _) = broadcast::channel(32);
        Self {
            state: Mutex::new(SharedState {
                counts: InventoryCounts {
                    pdf_total: 0,
                    signed_pdf: 0,
                    unsigned_pdf: 0,
                    non_pdf: 0,
                },
                running: None,
                last_result: None,
                last_error: None,
                refreshed_at: None,
            }),
            updates,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.updates.subscribe()
    }

    pub async fn snapshot(&self, configured: bool) -> ScanSnapshot {
        let state = self.state.lock().await;
        ScanSnapshot {
            configured,
            counts: state.counts.clone(),
            running: state.running.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            refreshed_at: state.refreshed_at,
        }
    }

    pub async fn refresh_inventory(
        &self,
        app_state: &Arc<crate::webhook::AppState>,
    ) -> anyhow::Result<ScanSnapshot> {
        let Some(paperless) = &app_state.paperless else {
            let mut state = self.state.lock().await;
            state.last_error = Some("Paperless API is not configured".to_owned());
            return Ok(ScanSnapshot {
                configured: false,
                counts: state.counts.clone(),
                running: state.running.clone(),
                last_result: state.last_result.clone(),
                last_error: state.last_error.clone(),
                refreshed_at: state.refreshed_at,
            });
        };

        let (documents, tags) =
            tokio::try_join!(paperless.list_documents_raw(), paperless.list_tags())?;

        let tag_lookup = tags
            .into_iter()
            .map(|tag| (tag.id, tag.name))
            .collect::<HashMap<_, _>>();
        let tag_name = app_state
            .config
            .paperless
            .as_ref()
            .map(|config| config.tag_name.clone())
            .unwrap_or_default();

        let counts = compute_inventory_counts(&documents, &tag_lookup, &tag_name);

        let mut state = self.state.lock().await;
        state.counts = counts;
        state.refreshed_at = Some(Utc::now());
        state.last_error = None;

        let snapshot = ScanSnapshot {
            configured: true,
            counts: state.counts.clone(),
            running: state.running.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            refreshed_at: state.refreshed_at,
        };
        drop(state);
        self.publish(&snapshot);

        Ok(snapshot)
    }

    pub async fn start_scan(
        self: Arc<Self>,
        app_state: Arc<crate::webhook::AppState>,
        mode: ScanMode,
    ) -> Result<ScanSnapshot, ScanStartError> {
        if app_state.paperless.is_none() {
            return Err(ScanStartError::PaperlessUnavailable(
                self.snapshot(false).await,
            ));
        }
        if app_state.config.paperless.is_none() {
            return Err(ScanStartError::NotConfigured(self.snapshot(false).await));
        }

        let mut state = self.state.lock().await;
        if state.running.is_some() {
            return Err(ScanStartError::AlreadyRunning(ScanSnapshot {
                configured: true,
                counts: state.counts.clone(),
                running: state.running.clone(),
                last_result: state.last_result.clone(),
                last_error: state.last_error.clone(),
                refreshed_at: state.refreshed_at,
            }));
        }

        let running = RunningScan {
            mode,
            phase: "Preparing inventory".to_owned(),
            total: None,
            processed: 0,
            signed_found: 0,
            newly_signed: 0,
            failed: 0,
            current_document_id: None,
            current_document_title: None,
            started_at: Utc::now(),
            cancel_requested: false,
        };
        state.running = Some(running);
        state.last_result = None;
        state.last_error = None;

        let snapshot = ScanSnapshot {
            configured: true,
            counts: state.counts.clone(),
            running: state.running.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            refreshed_at: state.refreshed_at,
        };
        drop(state);

        self.publish(&snapshot);
        tokio::spawn(run_scan_task(app_state, self.clone(), mode));

        Ok(snapshot)
    }

    pub async fn cancel_scan(&self, configured: bool) -> Result<ScanSnapshot, ScanCancelError> {
        let mut state = self.state.lock().await;
        let Some(running) = state.running.as_mut() else {
            return Err(ScanCancelError::NotRunning(ScanSnapshot {
                configured,
                counts: state.counts.clone(),
                running: state.running.clone(),
                last_result: state.last_result.clone(),
                last_error: state.last_error.clone(),
                refreshed_at: state.refreshed_at,
            }));
        };

        running.cancel_requested = true;
        let snapshot = ScanSnapshot {
            configured,
            counts: state.counts.clone(),
            running: state.running.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            refreshed_at: state.refreshed_at,
        };
        drop(state);

        self.publish(&snapshot);
        Ok(snapshot)
    }

    pub async fn is_cancel_requested(&self) -> bool {
        let state = self.state.lock().await;
        state
            .running
            .as_ref()
            .map(|running| running.cancel_requested)
            .unwrap_or(false)
    }

    pub async fn update_running<F>(&self, configured: bool, update: F)
    where
        F: FnOnce(&mut RunningScan),
    {
        let mut state = self.state.lock().await;
        if let Some(running) = state.running.as_mut() {
            update(running);
        }
        let snapshot = ScanSnapshot {
            configured,
            counts: state.counts.clone(),
            running: state.running.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            refreshed_at: state.refreshed_at,
        };
        drop(state);

        self.publish(&snapshot);
    }

    pub async fn finish(&self, configured: bool, result: ScanResult) {
        let mut state = self.state.lock().await;
        state.running = None;
        state.last_error = result.fatal_error.clone();
        state.last_result = Some(result);
        let snapshot = ScanSnapshot {
            configured,
            counts: state.counts.clone(),
            running: state.running.clone(),
            last_result: state.last_result.clone(),
            last_error: state.last_error.clone(),
            refreshed_at: state.refreshed_at,
        };
        drop(state);

        self.publish(&snapshot);
    }

    fn publish(&self, _snapshot: &ScanSnapshot) {
        let _ = self.updates.send(());
    }
}

pub async fn page() -> impl IntoResponse {
    Html(SCAN_PAGE)
}

pub async fn state(
    State(app_state): State<Arc<crate::webhook::AppState>>,
    Query(query): Query<ScanStateQuery>,
) -> impl IntoResponse {
    let snapshot = if query.refresh {
        match app_state.scan.refresh_inventory(&app_state).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                error!(error = %err, "failed to refresh scan inventory");
                let mut snapshot = app_state.scan.snapshot(app_state.paperless.is_some()).await;
                snapshot.last_error = Some(err.to_string());
                snapshot
            }
        }
    } else {
        app_state.scan.snapshot(app_state.paperless.is_some()).await
    };

    Json(ScanStateResponse { snapshot })
}

pub async fn start(
    State(app_state): State<Arc<crate::webhook::AppState>>,
    Json(request): Json<ScanStartRequest>,
) -> impl IntoResponse {
    match app_state
        .scan
        .clone()
        .start_scan(app_state.clone(), request.mode)
        .await
    {
        Ok(snapshot) => (
            StatusCode::ACCEPTED,
            Json(ScanStartResponse {
                status: "started",
                snapshot,
            }),
        ),
        Err(ScanStartError::AlreadyRunning(snapshot)) => (
            StatusCode::CONFLICT,
            Json(ScanStartResponse {
                status: "running",
                snapshot,
            }),
        ),
        Err(ScanStartError::PaperlessUnavailable(snapshot)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ScanStartResponse {
                status: "paperless_unavailable",
                snapshot,
            }),
        ),
        Err(ScanStartError::NotConfigured(snapshot)) => (
            StatusCode::BAD_REQUEST,
            Json(ScanStartResponse {
                status: "paperless_not_configured",
                snapshot,
            }),
        ),
    }
}

pub async fn cancel(State(app_state): State<Arc<crate::webhook::AppState>>) -> impl IntoResponse {
    match app_state
        .scan
        .cancel_scan(app_state.paperless.is_some())
        .await
    {
        Ok(snapshot) => (
            StatusCode::OK,
            Json(ScanCancelResponse {
                status: "cancel_requested",
                snapshot,
            }),
        ),
        Err(ScanCancelError::NotRunning(snapshot)) => (
            StatusCode::CONFLICT,
            Json(ScanCancelResponse {
                status: "not_running",
                snapshot,
            }),
        ),
    }
}

pub async fn events(
    State(app_state): State<Arc<crate::webhook::AppState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let configured = app_state.paperless.is_some();
    let initial_snapshot = app_state.scan.snapshot(configured).await;
    let updates = app_state.scan.subscribe();
    let app_state = app_state.clone();

    let stream = stream::unfold(
        (Some(initial_snapshot), updates, app_state),
        move |(initial, mut updates, app_state)| async move {
            if let Some(snapshot) = initial {
                let event = event_from_snapshot(snapshot);
                return Some((Ok(event), (None, updates, app_state)));
            }

            loop {
                match updates.recv().await {
                    Ok(()) => {
                        let snapshot = app_state.scan.snapshot(configured).await;
                        let event = event_from_snapshot(snapshot);
                        return Some((Ok(event), (None, updates, app_state)));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default().interval(std::time::Duration::from_secs(15)))
}

fn event_from_snapshot(snapshot: ScanSnapshot) -> Event {
    Event::default()
        .event("snapshot")
        .json_data(snapshot)
        .unwrap_or_else(|err| Event::default().event("error").data(err.to_string()))
}

async fn run_scan_task(
    app_state: Arc<crate::webhook::AppState>,
    scan: Arc<ScanCoordinator>,
    mode: ScanMode,
) {
    let result = match perform_scan(app_state.clone(), scan.clone(), mode).await {
        Ok(result) => result,
        Err(err) => {
            error!(error = %err, "scan task failed");
            ScanResult {
                mode,
                scanned: 0,
                signed_found: 0,
                newly_signed: 0,
                failed: 0,
                finished_at: Utc::now(),
                canceled: false,
                fatal_error: Some(err.to_string()),
                message: err.to_string(),
            }
        }
    };

    let configured = app_state.paperless.is_some();
    scan.finish(configured, result).await;
}

async fn perform_scan(
    app_state: Arc<crate::webhook::AppState>,
    scan: Arc<ScanCoordinator>,
    mode: ScanMode,
) -> anyhow::Result<ScanResult> {
    let paperless = app_state
        .paperless
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Paperless API is not configured"))?;
    let tag_name = app_state
        .config
        .paperless
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Paperless configuration is missing"))?
        .tag_name
        .clone();

    let (documents, tags) =
        tokio::try_join!(paperless.list_documents_raw(), paperless.list_tags())?;
    let tag_lookup = tags
        .into_iter()
        .map(|tag| (tag.id, tag.name))
        .collect::<HashMap<_, _>>();

    let inventory = build_inventory(&documents, &tag_lookup, &tag_name);
    scan.update_running(app_state.paperless.is_some(), |running| {
        running.phase = "Scanning documents".to_owned();
        running.total = Some(match mode {
            ScanMode::NotSigned => inventory
                .iter()
                .filter(|doc| is_pdf(&doc.mime_type) && !doc.signed)
                .count(),
            ScanMode::All => inventory
                .iter()
                .filter(|doc| is_pdf(&doc.mime_type))
                .count(),
        });
    })
    .await;

    let queue = inventory
        .into_iter()
        .filter(|doc| match mode {
            ScanMode::NotSigned => !doc.signed && is_pdf(&doc.mime_type),
            ScanMode::All => is_pdf(&doc.mime_type),
        })
        .collect::<Vec<_>>();

    let total = queue.len();
    let mut scanned = 0usize;
    let mut signed_found = 0usize;
    let mut newly_signed = 0usize;
    let mut failed = 0usize;

    for document in queue {
        if scan.is_cancel_requested().await {
            break;
        }

        scan.update_running(app_state.paperless.is_some(), |running| {
            running.phase = "Downloading document".to_owned();
            running.current_document_id = Some(document.id);
            running.current_document_title = document.title.clone();
        })
        .await;

        match scan_document(&app_state, &document).await {
            Ok(outcome) => {
                scanned += 1;
                if outcome.signatures_found {
                    signed_found += 1;
                }
                if outcome.newly_signed {
                    newly_signed += 1;
                }
                scan.update_running(app_state.paperless.is_some(), |running| {
                    running.processed = scanned;
                    running.signed_found = signed_found;
                    running.newly_signed = newly_signed;
                    running.failed = failed;
                    running.phase = if scanned == total {
                        "Finalizing".to_owned()
                    } else {
                        "Scanning documents".to_owned()
                    };
                })
                .await;
            }
            Err(err) => {
                failed += 1;
                warn!(
                    document_id = document.id,
                    error = %err,
                    "failed to scan Paperless document"
                );
                scan.update_running(app_state.paperless.is_some(), |running| {
                    running.processed = scanned + failed;
                    running.signed_found = signed_found;
                    running.newly_signed = newly_signed;
                    running.failed = failed;
                    running.phase = "Scanning documents".to_owned();
                })
                .await;
            }
        }
    }

    if let Err(err) = scan.refresh_inventory(&app_state).await {
        warn!(error = %err, "failed to refresh inventory after scan");
    }

    let canceled = scan.is_cancel_requested().await;
    let message = if canceled {
        format!("Scan canceled after {scanned} documents; {newly_signed} new signed documents")
    } else if failed > 0 {
        format!("Scan completed with {newly_signed} new signed documents and {failed} failures")
    } else {
        format!("Scan completed with {newly_signed} new signed documents")
    };

    info!(
        mode = ?mode,
        scanned,
        signed_found,
        newly_signed,
        failed,
        canceled,
        "scan completed"
    );

    Ok(ScanResult {
        mode,
        scanned,
        signed_found,
        newly_signed,
        failed,
        finished_at: Utc::now(),
        canceled,
        fatal_error: None,
        message,
    })
}

async fn scan_document(
    app_state: &Arc<crate::webhook::AppState>,
    document: &InventoryDocument,
) -> anyhow::Result<ScanOutcome> {
    let Some(paperless) = &app_state.paperless else {
        return Err(anyhow::anyhow!("Paperless API is not configured"));
    };

    let temp_path = app_state
        .config
        .temp_dir
        .join(format!("scan-{}.pdf", Uuid::new_v4()));
    let bytes = paperless.download_document(document.id).await?;
    fs::write(&temp_path, &bytes).await?;

    let signatures = match run_pdfsig_signatures(&app_state.config, &temp_path).await {
        Ok(value) => value,
        Err(err) => {
            let _ = fs::remove_file(&temp_path).await;
            return Err(err);
        }
    };
    let signatures_found = !signatures.is_empty();

    let mut newly_signed = false;
    if signatures_found && !document.signed {
        paperless
            .update_signed_document(document.id, &signatures)
            .await?;
        newly_signed = true;
    }

    match fs::remove_file(&temp_path).await {
        Ok(()) => {}
        Err(err) => warn!(
            document_id = document.id,
            error = %err,
            "failed to remove temporary scan PDF"
        ),
    }

    Ok(ScanOutcome {
        signatures_found,
        newly_signed,
    })
}

#[derive(Debug)]
struct ScanOutcome {
    signatures_found: bool,
    newly_signed: bool,
}

async fn run_pdfsig_signatures(
    config: &Config,
    pdf_path: &Path,
) -> anyhow::Result<Vec<crate::signature::SignatureInfo>> {
    let command_path = resolve_pdfsig(config)?;
    let output = Command::new(&command_path).arg(pdf_path).output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let signatures = parse_pdfsig_output(&stdout);
    Ok(actionable_signatures(&signatures))
}

fn resolve_pdfsig(config: &Config) -> anyhow::Result<PathBuf> {
    if let Some(path) = &config.pdfsig_path {
        if path.exists() {
            return Ok(path.clone());
        }
        warn!(path = %path.display(), "configured PDFSIG_PATH does not exist");
    }

    which::which("pdfsig").map_err(Into::into)
}

fn build_inventory(
    documents: &[Value],
    tag_lookup: &HashMap<i64, String>,
    tag_name: &str,
) -> Vec<InventoryDocument> {
    documents
        .iter()
        .filter_map(|document| parse_inventory_document(document, tag_lookup, tag_name))
        .collect()
}

fn compute_inventory_counts(
    documents: &[Value],
    tag_lookup: &HashMap<i64, String>,
    tag_name: &str,
) -> InventoryCounts {
    let mut counts = InventoryCounts {
        pdf_total: 0,
        signed_pdf: 0,
        unsigned_pdf: 0,
        non_pdf: 0,
    };

    for document in documents {
        if let Some(parsed) = parse_inventory_document(document, tag_lookup, tag_name) {
            if is_pdf(&parsed.mime_type) {
                counts.pdf_total += 1;
                if parsed.signed {
                    counts.signed_pdf += 1;
                } else {
                    counts.unsigned_pdf += 1;
                }
            } else {
                counts.non_pdf += 1;
            }
        }
    }

    counts
}

fn parse_inventory_document(
    document: &Value,
    tag_lookup: &HashMap<i64, String>,
    tag_name: &str,
) -> Option<InventoryDocument> {
    let id = extract_i64(document, &["id", "pk"])?;
    let title = extract_string(
        document,
        &["title", "name", "original_filename", "filename"],
    );
    let mime_type = extract_string(document, &["mime_type", "mimetype", "content_type"]);
    let signed = document_has_tag(document, tag_lookup, tag_name);

    Some(InventoryDocument {
        id,
        title,
        mime_type,
        signed,
    })
}

fn extract_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    })
}

fn extract_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        value.get(key).and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse().ok(),
            _ => None,
        })
    })
}

fn document_has_tag(document: &Value, tag_lookup: &HashMap<i64, String>, tag_name: &str) -> bool {
    collect_document_tags(document, tag_lookup)
        .into_iter()
        .any(|tag| tag.eq_ignore_ascii_case(tag_name))
}

fn collect_document_tags(document: &Value, tag_lookup: &HashMap<i64, String>) -> Vec<String> {
    let mut tags = Vec::new();

    if let Some(value) = document.get("tag_list") {
        tags.extend(collect_tag_values(value, tag_lookup));
    }
    if let Some(value) = document.get("tags") {
        tags.extend(collect_tag_values(value, tag_lookup));
    }
    if let Some(value) = document.get("tags_detail") {
        tags.extend(collect_tag_values(value, tag_lookup));
    }

    tags
}

fn collect_tag_values(value: &Value, tag_lookup: &HashMap<i64, String>) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| tag_value(item, tag_lookup))
            .collect(),
        Value::String(text) => text
            .split(',')
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .collect(),
        Value::Object(_) | Value::Number(_) => tag_value(value, tag_lookup).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn tag_value(value: &Value, tag_lookup: &HashMap<i64, String>) -> Option<String> {
    match value {
        Value::Number(number) => number.as_i64().and_then(|id| tag_lookup.get(&id).cloned()),
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object
            .get("name")
            .and_then(|value| value.as_str())
            .or_else(|| object.get("label").and_then(|value| value.as_str()))
            .or_else(|| object.get("title").and_then(|value| value.as_str()))
            .map(str::to_owned)
            .or_else(|| {
                object
                    .get("id")
                    .and_then(|value| value.as_i64())
                    .and_then(|id| tag_lookup.get(&id).cloned())
            }),
        _ => None,
    }
}

fn is_pdf(mime_type: &Option<String>) -> bool {
    mime_type
        .as_deref()
        .map(|value| {
            let lower = value.to_ascii_lowercase();
            lower.contains("pdf") || lower.ends_with(".pdf") || lower == "application/octet-stream"
        })
        .unwrap_or(false)
}

const SCAN_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>PDF Sign Check Scan</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #0b1020;
      --panel: rgba(15, 23, 42, 0.88);
      --panel-border: rgba(148, 163, 184, 0.18);
      --text: #e2e8f0;
      --muted: #94a3b8;
      --accent: #22c55e;
      --accent-2: #38bdf8;
      --danger: #fb7185;
      --warning: #fbbf24;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100vh;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background:
        radial-gradient(circle at top left, rgba(56, 189, 248, 0.18), transparent 28%),
        radial-gradient(circle at bottom right, rgba(34, 197, 94, 0.14), transparent 30%),
        linear-gradient(180deg, #050814 0%, #0b1020 60%, #0f172a 100%);
      color: var(--text);
      padding: 32px 20px 40px;
    }
    .wrap {
      max-width: 980px;
      margin: 0 auto;
    }
    .hero {
      display: grid;
      gap: 10px;
      margin-bottom: 22px;
    }
    .eyebrow {
      text-transform: uppercase;
      letter-spacing: 0.16em;
      font-size: 12px;
      color: var(--accent-2);
    }
    h1 {
      margin: 0;
      font-size: clamp(30px, 5vw, 54px);
      line-height: 1.02;
    }
    .subtitle {
      margin: 0;
      color: var(--muted);
      max-width: 64ch;
      line-height: 1.55;
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(12, 1fr);
      gap: 16px;
    }
    .card {
      grid-column: span 12;
      background: var(--panel);
      border: 1px solid var(--panel-border);
      border-radius: 20px;
      box-shadow: 0 24px 80px rgba(0, 0, 0, 0.28);
      backdrop-filter: blur(14px);
      padding: 18px;
    }
    @media (min-width: 820px) {
      .card.metrics { grid-column: span 4; }
      .card.main { grid-column: span 8; }
      .card.side { grid-column: span 4; }
    }
    .metric-label {
      color: var(--muted);
      font-size: 13px;
      margin-bottom: 10px;
    }
    .metric-value {
      font-size: 42px;
      line-height: 1;
      font-weight: 700;
      margin: 0 0 6px;
    }
    .metric-note {
      color: var(--muted);
      font-size: 14px;
    }
    .toolbar {
      display: flex;
      flex-wrap: wrap;
      gap: 12px;
      margin-top: 16px;
    }
    button {
      appearance: none;
      border: none;
      border-radius: 999px;
      padding: 12px 18px;
      font: inherit;
      font-weight: 650;
      cursor: pointer;
      transition: transform 120ms ease, opacity 120ms ease, background 120ms ease;
    }
    button:hover:not(:disabled) { transform: translateY(-1px); }
    button:disabled {
      cursor: not-allowed;
      opacity: 0.55;
    }
    .primary {
      color: #04111d;
      background: linear-gradient(135deg, #38bdf8 0%, #22c55e 100%);
    }
    .secondary {
      color: var(--text);
      background: rgba(148, 163, 184, 0.16);
      border: 1px solid rgba(148, 163, 184, 0.2);
    }
    .selected {
      outline: 2px solid rgba(56, 189, 248, 0.6);
      box-shadow: 0 0 0 3px rgba(56, 189, 248, 0.12);
    }
    .status {
      display: grid;
      gap: 10px;
    }
    .status-line {
      color: var(--text);
      font-weight: 600;
    }
    .status-detail {
      color: var(--muted);
      line-height: 1.5;
    }
    .progress-shell {
      margin-top: 16px;
      display: none;
      gap: 10px;
    }
    .progress-shell.visible { display: grid; }
    .progress-track {
      height: 14px;
      border-radius: 999px;
      background: rgba(148, 163, 184, 0.14);
      overflow: hidden;
      border: 1px solid rgba(148, 163, 184, 0.18);
    }
    .progress-bar {
      height: 100%;
      width: 0%;
      background: linear-gradient(90deg, var(--accent-2), var(--accent));
      transition: width 220ms ease;
    }
    .progress-meta {
      display: flex;
      justify-content: space-between;
      gap: 12px;
      color: var(--muted);
      font-size: 14px;
    }
    .result {
      border-top: 1px solid rgba(148, 163, 184, 0.16);
      margin-top: 16px;
      padding-top: 16px;
      color: var(--text);
      line-height: 1.55;
      white-space: pre-wrap;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      font-size: 12px;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      color: var(--muted);
    }
    .dot {
      width: 8px;
      height: 8px;
      border-radius: 50%;
      background: var(--accent-2);
      box-shadow: 0 0 18px rgba(56, 189, 248, 0.6);
    }
    .error { color: var(--danger); }
    .good { color: #86efac; }
    .warn { color: var(--warning); }
  </style>
</head>
<body>
  <main class="wrap">
    <section class="hero">
      <div class="eyebrow">Paperless PDF signature scan</div>
      <h1>Scan the Paperless database for signed PDFs.</h1>
      <p class="subtitle">
        The page keeps a shared server-side scan state, so every open tab sees the same progress, disabled controls, and completion summary.
      </p>
    </section>

    <section class="grid">
      <article class="card metrics">
        <div class="metric-label">Signed PDF documents</div>
        <div class="metric-value" id="signed-count">0</div>
        <div class="metric-note">Documents already marked with the configured signed tag.</div>
      </article>
      <article class="card metrics">
        <div class="metric-label">Unsigned PDF documents</div>
        <div class="metric-value" id="unsigned-count">0</div>
        <div class="metric-note">Documents that still need a signature scan.</div>
      </article>
      <article class="card metrics">
        <div class="metric-label">PDF total</div>
        <div class="metric-value" id="pdf-count">0</div>
        <div class="metric-note">Only PDF files are included in scan totals.</div>
      </article>

      <article class="card main">
        <div class="badge"><span class="dot"></span><span>Scan control</span></div>
        <div class="toolbar">
          <button id="scan-not-signed" class="primary" type="button">Scan not signed</button>
          <button id="scan-all" class="secondary" type="button">Scan all</button>
          <button id="scan-cancel" class="secondary" type="button">Cancel scan</button>
        </div>
        <div class="progress-shell" id="progress-shell">
          <div class="progress-track" aria-label="scan progress">
            <div class="progress-bar" id="progress-bar"></div>
          </div>
          <div class="progress-meta">
            <span id="progress-label">Idle</span>
            <span id="progress-count">0 / 0</span>
          </div>
        </div>
        <div class="result" id="result">Loading current Paperless state...</div>
      </article>

      <article class="card side status">
        <div class="badge"><span class="dot"></span><span>Live state</span></div>
        <div class="status-line" id="state-line">Waiting for state</div>
        <div class="status-detail" id="state-detail">The page polls the server every second so all tabs stay synchronized.</div>
      </article>
    </section>
  </main>

  <script type="module">
    const signedCount = document.getElementById("signed-count");
    const unsignedCount = document.getElementById("unsigned-count");
    const pdfCount = document.getElementById("pdf-count");
    const scanNotSigned = document.getElementById("scan-not-signed");
    const scanAll = document.getElementById("scan-all");
    const scanCancel = document.getElementById("scan-cancel");
    const progressShell = document.getElementById("progress-shell");
    const progressBar = document.getElementById("progress-bar");
    const progressLabel = document.getElementById("progress-label");
    const progressCount = document.getElementById("progress-count");
    const result = document.getElementById("result");
    const stateLine = document.getElementById("state-line");
    const stateDetail = document.getElementById("state-detail");

    const stateUrl = new URL("./scan/state", window.location.href);
    const startUrl = new URL("./scan/start", window.location.href);
    const cancelUrl = new URL("./scan/cancel", window.location.href);
    const eventsUrl = new URL("./scan/events", window.location.href);
    const lastModeKey = "pdf-sign-check:last-scan-mode";

    let eventSource = null;
    let lastMode = window.localStorage.getItem(lastModeKey) || "not_signed";

    function setButtonsDisabled(disabled) {
      scanNotSigned.disabled = disabled;
      scanAll.disabled = disabled;
      scanCancel.disabled = disabled;
    }

    function setSelectedMode(mode) {
      lastMode = mode;
      window.localStorage.setItem(lastModeKey, mode);
      scanNotSigned.classList.toggle("selected", mode === "not_signed");
      scanAll.classList.toggle("selected", mode === "all");
    }

    function render(snapshot) {
      const counts = snapshot.counts || { signed_pdf: 0, unsigned_pdf: 0, pdf_total: 0 };
      signedCount.textContent = counts.signed_pdf ?? 0;
      unsignedCount.textContent = counts.unsigned_pdf ?? 0;
      pdfCount.textContent = counts.pdf_total ?? 0;

      const running = snapshot.running;
      const lastResult = snapshot.last_result;
      const lastError = snapshot.last_error;
      const configured = snapshot.configured;

      if (!configured) {
        setButtonsDisabled(true);
        progressShell.classList.remove("visible");
        stateLine.textContent = "Paperless is not configured";
        stateDetail.textContent = "Set PAPERLESS_URL and credentials to enable scans.";
        result.textContent = "Paperless API configuration is missing.";
        return;
      }

      if (running) {
        setButtonsDisabled(true);
        scanCancel.disabled = false;
        progressShell.classList.add("visible");
        const total = running.total ?? 0;
        const processed = running.processed ?? 0;
        const percent = total > 0 ? Math.min(100, Math.round((processed / total) * 100)) : 0;
        progressBar.style.width = `${percent}%`;
        progressLabel.textContent = `${running.phase} (${running.mode.replaceAll("_", " ")})`;
        progressCount.textContent = total > 0 ? `${processed} / ${total}` : `${processed} / ?`;
        stateLine.textContent = running.cancel_requested ? "Cancel requested" : `Scan running: ${running.phase}`;
        stateDetail.textContent = `Processed ${processed} documents, ${running.newly_signed} newly signed, ${running.failed} failed.${running.cancel_requested ? " The scan will stop after the current document." : ""}`;
        result.textContent = `Scan in progress. Current document: ${running.current_document_id ?? "n/a"}${running.current_document_title ? " - " + running.current_document_title : ""}`;
        return;
      }

      setButtonsDisabled(false);
      scanCancel.disabled = true;
      progressShell.classList.remove("visible");
      progressBar.style.width = "0%";
      progressLabel.textContent = "Idle";
      progressCount.textContent = "0 / 0";

      if (lastResult) {
        if (lastResult.fatal_error) {
          stateLine.textContent = "Last scan failed";
          stateDetail.textContent = lastResult.fatal_error;
          result.innerHTML = `<span class="error">${escapeHtml(lastResult.fatal_error)}</span>`;
          return;
        }

        if (lastResult.canceled) {
          stateLine.textContent = "Last scan canceled";
          stateDetail.textContent = `${lastResult.scanned} documents processed before cancellation.`;
          result.innerHTML = `<span class="warn">${escapeHtml(lastResult.message)}</span>`;
          return;
        }

        const style = lastResult.failed > 0 ? "warn" : "good";
        stateLine.textContent = lastResult.failed > 0 ? "Last scan completed with failures" : "Last scan completed";
        stateDetail.textContent = `${lastResult.newly_signed} new signed documents, ${lastResult.failed} failures.`;
        result.innerHTML = `<span class="${style}">${escapeHtml(lastResult.message)}</span>`;
        return;
      }

      if (lastError) {
        stateLine.textContent = "Last scan error";
        stateDetail.textContent = lastError;
        result.innerHTML = `<span class="error">${escapeHtml(lastError)}</span>`;
        return;
      }

      stateLine.textContent = "Ready";
      stateDetail.textContent = "Choose a scan mode to start checking Paperless documents.";
      result.textContent = `No scan has been run yet. Last mode: ${lastMode.replaceAll("_", " ")}`;
    }

    async function loadState(refresh = false) {
      const url = new URL(stateUrl);
      if (refresh) {
        url.searchParams.set("refresh", "true");
      }
      const response = await fetch(url, { headers: { accept: "application/json" } });
      if (!response.ok) {
        throw new Error(`State request failed: ${response.status}`);
      }
      return response.json();
    }

    async function initialLoad() {
      try {
        const payload = await loadState(true);
        render(payload.snapshot);
      } catch (error) {
        stateLine.textContent = "State request failed";
        stateDetail.textContent = String(error);
        result.innerHTML = `<span class="error">${escapeHtml(String(error))}</span>`;
      }
    }

    async function startScan(mode) {
      try {
        setSelectedMode(mode);
        setButtonsDisabled(true);
        scanCancel.disabled = false;
        result.textContent = "Starting scan...";
        const response = await fetch(startUrl, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            accept: "application/json",
          },
          body: JSON.stringify({ mode }),
        });
        const payload = await response.json();
        render(payload.snapshot);
        if (!response.ok && response.status !== 202) {
          stateLine.textContent = "Scan request rejected";
          stateDetail.textContent = payload?.snapshot?.last_error ?? payload?.status ?? `HTTP ${response.status}`;
          result.innerHTML = `<span class="warn">Scan request was not accepted.</span>`;
        }
      } catch (error) {
        stateLine.textContent = "Scan request failed";
        stateDetail.textContent = String(error);
        result.innerHTML = `<span class="error">${escapeHtml(String(error))}</span>`;
      }
    }

    async function cancelScan() {
      try {
        const response = await fetch(cancelUrl, {
          method: "POST",
          headers: { accept: "application/json" },
        });
        const payload = await response.json();
        render(payload.snapshot);
        if (!response.ok && response.status !== 409) {
          stateLine.textContent = "Cancel request rejected";
          stateDetail.textContent = payload?.snapshot?.last_error ?? payload?.status ?? `HTTP ${response.status}`;
          result.innerHTML = `<span class="warn">Cancel request was not accepted.</span>`;
        }
      } catch (error) {
        stateLine.textContent = "Cancel request failed";
        stateDetail.textContent = String(error);
        result.innerHTML = `<span class="error">${escapeHtml(String(error))}</span>`;
      }
    }

    function connectStream() {
      if (eventSource) {
        eventSource.close();
      }

      eventSource = new EventSource(eventsUrl);
      eventSource.addEventListener("snapshot", (event) => {
        const payload = JSON.parse(event.data);
        render(payload);
      });
      eventSource.onerror = async () => {
        try {
          const payload = await loadState(false);
          render(payload.snapshot);
        } catch (error) {
          stateLine.textContent = "Live updates disconnected";
          stateDetail.textContent = String(error);
        }
      };
    }

    function escapeHtml(value) {
      return value
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll("\"", "&quot;")
        .replaceAll("'", "&#39;");
    }

    scanNotSigned.addEventListener("click", () => startScan("not_signed"));
    scanAll.addEventListener("click", () => startScan("all"));
    scanCancel.addEventListener("click", cancelScan);

    setSelectedMode(lastMode);
    await initialLoad();
    connectStream();
  </script>
</body>
</html>"#;
