use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{State, connect_info::ConnectInfo},
    http::{
        HeaderMap, Request, StatusCode,
        header::{CONTENT_TYPE, USER_AGENT},
    },
    response::IntoResponse,
};
use base64::{Engine, engine::general_purpose};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{fs, process::Command};
use tracing::{error, info, warn};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    config::Config,
    paperless::{PaperlessClient, PaperlessUpdateResult},
    signature::{SignatureInfo, actionable_signatures, parse_pdfsig_output},
};

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Config,
    pub paperless: Option<PaperlessClient>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookResponse {
    pub request_id: String,
    pub status: &'static str,
    pub pdf_detected: bool,
    pub pdfsig_available: bool,
    pub signatures_found: bool,
    pub document_id: Option<i64>,
    pub paperless_updated: bool,
    pub message: String,
}

#[derive(Debug)]
struct ExtractedPdf {
    bytes: Bytes,
    source: String,
    file_name: Option<String>,
}

#[derive(Debug)]
struct ParsedPayload {
    pdf: Option<ExtractedPdf>,
    fields: BTreeMap<String, String>,
    json: Option<Value>,
    parameters: RequestParametersDebug,
}

#[derive(Debug)]
struct ParsedMultipartPayload {
    pdf: Option<ExtractedPdf>,
    fields: Vec<ParameterValue>,
    parts: Vec<MultipartPartDebug>,
}

#[derive(Debug, Serialize)]
struct DocumentContext {
    document_id: Option<i64>,
    document_url: Option<String>,
    fields: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct DebugDump {
    request_id: String,
    received_at: DateTime<Utc>,
    #[serde(rename = "ASK")]
    ask: AskDebug,
    #[serde(rename = "RESPONSE")]
    response: ResponseDebug,
    context: DocumentContext,
    pdf: PdfDebug,
    pdfsig: Option<PdfsigDebug>,
    paperless: PaperlessDebug,
}

#[derive(Debug, Serialize)]
struct AskDebug {
    remote_addr: String,
    method: String,
    uri: String,
    path: String,
    query_string: Option<String>,
    content_type: Option<String>,
    headers: BTreeMap<String, String>,
    body: DebugBody,
    parameters: RequestParametersDebug,
}

#[derive(Debug, Serialize)]
struct ResponseDebug {
    status_code: u16,
    body: WebhookResponse,
}

#[derive(Debug, Serialize)]
struct DebugBody {
    length: usize,
    sha256: String,
    is_utf8: bool,
    text: Option<String>,
    json: Option<Value>,
    base64: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct RequestParametersDebug {
    query: ParameterGroup,
    json: JsonParametersDebug,
    form_urlencoded: ParameterGroup,
    multipart: MultipartParametersDebug,
    header_context: ParameterGroup,
    combined: BTreeMap<String, Vec<ParameterOccurrence>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ParameterGroup {
    count: usize,
    values: Vec<ParameterValue>,
}

#[derive(Debug, Clone, Serialize)]
struct ParameterValue {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Serialize)]
struct ParameterOccurrence {
    source: String,
    name: String,
    value: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct JsonParametersDebug {
    detected: bool,
    parse_error: Option<String>,
    root: Option<Value>,
    flattened: Vec<ParameterValue>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct MultipartParametersDebug {
    detected: bool,
    boundary: Option<String>,
    parse_error: Option<String>,
    parts: Vec<MultipartPartDebug>,
}

#[derive(Debug, Clone, Serialize)]
struct MultipartPartDebug {
    name: Option<String>,
    file_name: Option<String>,
    content_type: Option<String>,
    size: usize,
    sha256: String,
    is_pdf: bool,
    text: Option<String>,
    text_truncated: bool,
    base64: Option<String>,
    base64_truncated: bool,
}

#[derive(Debug, Serialize)]
struct PdfDebug {
    detected: bool,
    source: Option<String>,
    file_name: Option<String>,
    temp_path: Option<String>,
    bytes: Option<usize>,
    download_error: Option<String>,
    removed_temp_file: bool,
    remove_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct PdfsigDebug {
    available: bool,
    command: Option<String>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    error: Option<String>,
    contains_signatures: bool,
    actionable_signatures_count: usize,
    ignored_signatures_count: usize,
    signatures: Vec<SignatureInfo>,
}

#[derive(Debug, Serialize)]
struct PaperlessDebug {
    attempted: bool,
    document_id: Option<i64>,
    result: Option<PaperlessUpdateResult>,
    skipped_reason: Option<String>,
    error: Option<String>,
}

pub async fn health() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

pub async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> impl IntoResponse {
    match process_webhook(state, remote_addr, req).await {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(err) => {
            error!(error = %err, "webhook processing failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WebhookResponse {
                    request_id: "unavailable".to_owned(),
                    status: "error",
                    pdf_detected: false,
                    pdfsig_available: false,
                    signatures_found: false,
                    document_id: None,
                    paperless_updated: false,
                    message: err.to_string(),
                }),
            )
        }
    }
}

async fn process_webhook(
    state: Arc<AppState>,
    remote_addr: SocketAddr,
    req: Request<Body>,
) -> anyhow::Result<WebhookResponse> {
    let request_id = request_id();
    let received_at = Utc::now();
    let (parts, body) = req.into_parts();
    let headers = parts.headers;
    let method = parts.method.to_string();
    let uri = parts.uri.to_string();
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    info!(
        request_id,
        remote_addr = %remote_addr,
        method = %method,
        uri = %uri,
        user_agent = %user_agent,
        "webhook request received"
    );

    let body = to_bytes(body, state.config.max_body_bytes).await?;
    let body_debug = debug_body(&body);
    let parsed_payload = parse_payload(&headers, &uri, body.clone()).await;
    let context = document_context(&headers, &uri, &parsed_payload);
    let parameters = parsed_payload.parameters.clone();
    let mut pdf = parsed_payload.pdf;
    let mut download_error = None;

    if pdf.is_none() {
        if let (Some(document_id), Some(paperless)) = (context.document_id, &state.paperless) {
            match paperless.download_document(document_id).await {
                Ok(bytes) => {
                    info!(
                        request_id,
                        document_id,
                        bytes = bytes.len(),
                        "downloaded PDF from Paperless"
                    );
                    pdf = Some(ExtractedPdf {
                        bytes,
                        source: format!("Paperless document {document_id} download"),
                        file_name: Some(format!("paperless-{document_id}.pdf")),
                    });
                }
                Err(err) => {
                    let message = err.to_string();
                    warn!(
                        request_id,
                        document_id,
                        error = %message,
                        "failed to download PDF from Paperless"
                    );
                    download_error = Some(message);
                }
            }
        }
    }

    let mut pdf_debug = PdfDebug {
        detected: pdf.is_some(),
        source: pdf.as_ref().map(|pdf| pdf.source.clone()),
        file_name: pdf.as_ref().and_then(|pdf| pdf.file_name.clone()),
        temp_path: None,
        bytes: pdf.as_ref().map(|pdf| pdf.bytes.len()),
        download_error,
        removed_temp_file: false,
        remove_error: None,
    };

    let mut pdfsig = None;
    if let Some(extracted_pdf) = pdf.take() {
        let temp_path = state.config.temp_dir.join(format!("{request_id}.pdf"));
        pdf_debug.temp_path = Some(temp_path.display().to_string());

        fs::write(&temp_path, &extracted_pdf.bytes).await?;
        info!(
            request_id,
            temp_path = %temp_path.display(),
            bytes = extracted_pdf.bytes.len(),
            source = %extracted_pdf.source,
            "temporary PDF saved"
        );

        pdfsig = Some(run_pdfsig(&state.config, &request_id, &temp_path).await);

        match fs::remove_file(&temp_path).await {
            Ok(()) => {
                pdf_debug.removed_temp_file = true;
                info!(request_id, temp_path = %temp_path.display(), "temporary PDF removed");
            }
            Err(err) => {
                let message = err.to_string();
                pdf_debug.remove_error = Some(message.clone());
                warn!(
                    request_id,
                    temp_path = %temp_path.display(),
                    error = %message,
                    "failed to remove temporary PDF"
                );
            }
        }
    } else {
        info!(request_id, "no PDF found in webhook payload");
    }

    let paperless =
        update_paperless_if_signed(&state, &request_id, context.document_id, pdfsig.as_ref()).await;

    let pdfsig_available = pdfsig
        .as_ref()
        .map(|pdfsig| pdfsig.available)
        .unwrap_or(false);
    let signatures_found = pdfsig
        .as_ref()
        .map(|pdfsig| pdfsig.contains_signatures)
        .unwrap_or(false);
    let pdf_detected = pdf_debug.detected;
    let paperless_updated = paperless.result.is_some();
    let document_id = context.document_id;
    let message = if !pdf_detected {
        "payload saved; no PDF detected".to_owned()
    } else if signatures_found && paperless_updated {
        "payload saved; PDF signature found and Paperless updated".to_owned()
    } else if signatures_found {
        "payload saved; PDF signature found but Paperless was not updated".to_owned()
    } else if pdfsig_available {
        "payload saved; PDF checked with pdfsig; no signatures found".to_owned()
    } else {
        "payload saved; PDF detected but pdfsig is unavailable or failed".to_owned()
    };

    let response = WebhookResponse {
        request_id,
        status: "ok",
        pdf_detected,
        pdfsig_available,
        signatures_found,
        document_id,
        paperless_updated,
        message,
    };
    let (path, query_string) = uri_parts(&uri);
    let dump = DebugDump {
        request_id: response.request_id.clone(),
        received_at,
        ask: AskDebug {
            remote_addr: remote_addr.to_string(),
            method,
            uri,
            path,
            query_string,
            content_type: content_type_value(&headers),
            headers: headers_to_map(&headers),
            body: body_debug,
            parameters,
        },
        response: ResponseDebug {
            status_code: StatusCode::OK.as_u16(),
            body: response.clone(),
        },
        context,
        pdf: pdf_debug,
        pdfsig,
        paperless,
    };

    write_debug_dump(&state.config.debug_dir, &response.request_id, &dump).await?;

    Ok(response)
}

async fn write_debug_dump(
    debug_dir: &Path,
    request_id: &str,
    dump: &DebugDump,
) -> anyhow::Result<()> {
    fs::create_dir_all(debug_dir).await?;
    let path = debug_dir.join(format!("{request_id}.json"));
    let json = serde_json::to_vec_pretty(dump)?;
    fs::write(&path, json).await?;
    info!(
        request_id,
        debug_path = %path.display(),
        "debug dump saved"
    );
    Ok(())
}

fn debug_body(body: &Bytes) -> DebugBody {
    DebugBody {
        length: body.len(),
        sha256: sha256_hex(body),
        is_utf8: std::str::from_utf8(body).is_ok(),
        text: String::from_utf8(body.to_vec()).ok(),
        json: serde_json::from_slice::<Value>(body).ok(),
        base64: general_purpose::STANDARD.encode(body),
    }
}

fn uri_parts(uri: &str) -> (String, Option<String>) {
    match uri.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (uri.to_owned(), None),
    }
}

fn content_type_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn parse_payload(headers: &HeaderMap, uri: &str, body: Bytes) -> ParsedPayload {
    let mut fields = BTreeMap::new();
    let mut parameters = RequestParametersDebug::default();

    let query_values = query_parameters(uri);
    add_parameter_values(&mut fields, &mut parameters, "query", &query_values);
    parameters.query = parameter_group(query_values);

    let header_values = header_context_parameters(headers);
    add_parameter_values(
        &mut fields,
        &mut parameters,
        "header_context",
        &header_values,
    );
    parameters.header_context = parameter_group(header_values);

    if is_pdf(&body) {
        return ParsedPayload {
            pdf: Some(ExtractedPdf {
                bytes: body,
                source: "raw body".to_owned(),
                file_name: Some("webhook.pdf".to_owned()),
            }),
            fields,
            json: None,
            parameters,
        };
    }

    let mut pdf = None;
    let mut json = None;
    let content_type = content_type(headers);
    let content_type_lower = content_type.to_ascii_lowercase();

    if content_type_lower.starts_with("multipart/form-data") {
        parameters.multipart.detected = true;
        if let Some(boundary) = multipart_boundary(&content_type) {
            parameters.multipart.boundary = Some(boundary.clone());
            match parse_multipart_payload(body.clone(), &boundary).await {
                Ok(payload) => {
                    add_parameter_values(
                        &mut fields,
                        &mut parameters,
                        "multipart",
                        &payload.fields,
                    );
                    parameters.multipart.parts = payload.parts;
                    pdf = payload.pdf;
                }
                Err(err) => {
                    let message = err.to_string();
                    parameters.multipart.parse_error = Some(message.clone());
                    warn!(error = %message, "failed to parse multipart webhook payload");
                }
            }
        } else {
            parameters.multipart.parse_error =
                Some("missing multipart boundary in Content-Type".to_owned());
        }
    }

    if content_type_lower.starts_with("application/x-www-form-urlencoded")
        || should_parse_body_as_form_urlencoded(&content_type_lower, &body)
    {
        let form_values = form_urlencoded_parameters(&body);
        add_parameter_values(
            &mut fields,
            &mut parameters,
            "form_urlencoded",
            &form_values,
        );
        parameters.form_urlencoded = parameter_group(form_values);
    }

    if pdf.is_none() && (content_type_lower.contains("json") || looks_like_json(&body)) {
        parameters.json.detected = true;
        match serde_json::from_slice::<Value>(&body) {
            Ok(value) => {
                let json_values = json_parameters(&value);
                add_parameter_values(&mut fields, &mut parameters, "json", &json_values);
                parameters.json.root = Some(value.clone());
                parameters.json.flattened = json_values;
                pdf = find_pdf_in_json(&value, "$");
                json = Some(value);
            }
            Err(err) => {
                let message = err.to_string();
                parameters.json.parse_error = Some(message.clone());
                warn!(error = %message, "failed to parse JSON webhook payload");
            }
        }
    }

    ParsedPayload {
        pdf,
        fields,
        json,
        parameters,
    }
}

async fn parse_multipart_payload(
    body: Bytes,
    boundary: &str,
) -> anyhow::Result<ParsedMultipartPayload> {
    let stream = stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
    let mut multipart = multer::Multipart::new(stream, boundary.to_owned());
    let mut pdf = None;
    let mut fields = Vec::new();
    let mut parts = Vec::new();

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().map(str::to_owned);
        let file_name = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(ToString::to_string);
        let bytes = field.bytes().await?;
        let size = bytes.len();
        let sha256 = sha256_hex(&bytes);

        let file_name_is_pdf = file_name
            .as_deref()
            .map(|value| value.to_ascii_lowercase().ends_with(".pdf"))
            .unwrap_or(false);
        let content_type_is_pdf = content_type
            .as_deref()
            .map(|value| value.eq_ignore_ascii_case("application/pdf"))
            .unwrap_or(false);
        let is_pdf_field = is_pdf(&bytes) || file_name_is_pdf || content_type_is_pdf;
        parts.push(multipart_part_debug(
            name.clone(),
            file_name.clone(),
            content_type,
            &bytes,
            size,
            sha256,
            is_pdf_field,
        ));

        if is_pdf_field && pdf.is_none() {
            pdf = Some(ExtractedPdf {
                bytes,
                source: format!(
                    "multipart field {}",
                    name.clone().unwrap_or_else(|| "<unnamed>".to_owned())
                ),
                file_name,
            });
            continue;
        }

        if let Some(name) = name {
            fields.push(ParameterValue {
                name,
                value: field_value(bytes),
            });
        }
    }

    Ok(ParsedMultipartPayload { pdf, fields, parts })
}

fn document_context(
    headers: &HeaderMap,
    uri: &str,
    parsed_payload: &ParsedPayload,
) -> DocumentContext {
    let mut fields = parsed_payload.fields.clone();
    for (key, value) in query_fields(uri) {
        fields.insert(key, value);
    }
    for (key, value) in header_context_fields(headers) {
        fields.insert(key, value);
    }

    let mut document_url = find_document_url_in_fields(&fields);
    if document_url.is_none() {
        if let Some(json) = &parsed_payload.json {
            document_url = find_document_url_in_json(json);
        }
    }

    let mut document_id = find_document_id_in_fields(&fields);
    if document_id.is_none() {
        if let Some(json) = &parsed_payload.json {
            document_id = find_document_id_in_json(json);
        }
    }
    if document_id.is_none() {
        document_id = document_url.as_deref().and_then(document_id_from_url);
    }

    DocumentContext {
        document_id,
        document_url,
        fields,
    }
}

fn query_fields(uri: &str) -> BTreeMap<String, String> {
    parameters_to_map(query_parameters(uri))
}

fn query_parameters(uri: &str) -> Vec<ParameterValue> {
    let Some((_, query)) = uri.split_once('?') else {
        return Vec::new();
    };

    form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| ParameterValue {
            name: name.into_owned(),
            value: value.into_owned(),
        })
        .collect()
}

fn header_context_fields(headers: &HeaderMap) -> BTreeMap<String, String> {
    parameters_to_map(header_context_parameters(headers))
}

fn header_context_parameters(headers: &HeaderMap) -> Vec<ParameterValue> {
    let mut values = Vec::new();
    for name in [
        "x-paperless-document-id",
        "x-document-id",
        "x-paperless-document-url",
        "x-document-url",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            values.push(ParameterValue {
                name: name.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    values
}

fn form_urlencoded_parameters(body: &[u8]) -> Vec<ParameterValue> {
    form_urlencoded::parse(body)
        .map(|(name, value)| ParameterValue {
            name: name.into_owned(),
            value: value.into_owned(),
        })
        .collect()
}

fn should_parse_body_as_form_urlencoded(content_type_lower: &str, body: &[u8]) -> bool {
    if !content_type_lower.trim().is_empty() {
        return false;
    }

    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    let text = text.trim();

    !text.is_empty()
        && text.contains('=')
        && !looks_like_json(body)
        && form_urlencoded_parameters(body)
            .iter()
            .any(|value| !value.name.trim().is_empty())
}

fn parameters_to_map(values: Vec<ParameterValue>) -> BTreeMap<String, String> {
    values
        .into_iter()
        .map(|value| (value.name, value.value))
        .collect()
}

fn parameter_group(values: Vec<ParameterValue>) -> ParameterGroup {
    ParameterGroup {
        count: values.len(),
        values,
    }
}

fn add_parameter_values(
    fields: &mut BTreeMap<String, String>,
    parameters: &mut RequestParametersDebug,
    source: &str,
    values: &[ParameterValue],
) {
    for value in values {
        fields.insert(value.name.clone(), value.value.clone());
        parameters
            .combined
            .entry(value.name.clone())
            .or_default()
            .push(ParameterOccurrence {
                source: source.to_owned(),
                name: value.name.clone(),
                value: value.value.clone(),
            });
    }
}

fn json_parameters(value: &Value) -> Vec<ParameterValue> {
    let mut values = Vec::new();
    collect_json_parameters(value, "$", &mut values);
    values
}

fn field_value(bytes: Bytes) -> String {
    if bytes.len() > 65_536 {
        return format!("<{} bytes>", bytes.len());
    }

    String::from_utf8(bytes.to_vec()).unwrap_or_else(|err| {
        format!(
            "<{} bytes non-utf8 field: {}>",
            err.as_bytes().len(),
            err.utf8_error()
        )
    })
}

fn collect_json_parameters(value: &Value, path: &str, values: &mut Vec<ParameterValue>) {
    match value {
        Value::String(text) => {
            values.push(ParameterValue {
                name: path.to_owned(),
                value: text.clone(),
            });
        }
        Value::Number(number) => {
            values.push(ParameterValue {
                name: path.to_owned(),
                value: number.to_string(),
            });
        }
        Value::Bool(value) => {
            values.push(ParameterValue {
                name: path.to_owned(),
                value: value.to_string(),
            });
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                collect_json_parameters(value, &format!("{path}[{index}]"), values);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                collect_json_parameters(value, &format!("{path}.{key}"), values);
            }
        }
        Value::Null => {}
    }
}

fn multipart_part_debug(
    name: Option<String>,
    file_name: Option<String>,
    content_type: Option<String>,
    bytes: &Bytes,
    size: usize,
    sha256: String,
    is_pdf: bool,
) -> MultipartPartDebug {
    let (text, text_truncated) = debug_text(bytes);
    let (base64, base64_truncated) = debug_base64(bytes);

    MultipartPartDebug {
        name,
        file_name,
        content_type,
        size,
        sha256,
        is_pdf,
        text,
        text_truncated,
        base64,
        base64_truncated,
    }
}

fn debug_text(bytes: &[u8]) -> (Option<String>, bool) {
    const DEBUG_TEXT_LIMIT: usize = 65_536;

    match std::str::from_utf8(bytes) {
        Ok(text) if text.chars().count() <= DEBUG_TEXT_LIMIT => (Some(text.to_owned()), false),
        Ok(text) => (Some(text.chars().take(DEBUG_TEXT_LIMIT).collect()), true),
        Err(_) => (None, false),
    }
}

fn debug_base64(bytes: &[u8]) -> (Option<String>, bool) {
    const DEBUG_FIELD_BASE64_LIMIT: usize = 1_048_576;

    if bytes.len() > DEBUG_FIELD_BASE64_LIMIT {
        (None, true)
    } else {
        (Some(general_purpose::STANDARD.encode(bytes)), false)
    }
}

fn find_document_id_in_fields(fields: &BTreeMap<String, String>) -> Option<i64> {
    fields.iter().find_map(|(key, value)| {
        if is_document_id_key(key) {
            parse_document_id(value).or_else(|| document_id_from_url(value))
        } else {
            None
        }
    })
}

fn find_document_id_in_json(value: &Value) -> Option<i64> {
    match value {
        Value::Object(values) => values.iter().find_map(|(key, value)| {
            if is_document_id_key(key) {
                parse_document_id_value(value)
            } else {
                find_document_id_in_json(value)
            }
        }),
        Value::Array(values) => values.iter().find_map(find_document_id_in_json),
        _ => None,
    }
}

fn find_document_url_in_fields(fields: &BTreeMap<String, String>) -> Option<String> {
    fields.iter().find_map(|(key, value)| {
        if is_document_url_key(key) {
            Some(value.to_owned())
        } else {
            None
        }
    })
}

fn find_document_url_in_json(value: &Value) -> Option<String> {
    match value {
        Value::Object(values) => values.iter().find_map(|(key, value)| {
            if is_document_url_key(key) {
                value.as_str().map(str::to_owned)
            } else {
                find_document_url_in_json(value)
            }
        }),
        Value::Array(values) => values.iter().find_map(find_document_url_in_json),
        _ => None,
    }
}

fn is_document_id_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    matches!(
        normalized.as_str(),
        "id" | "documentid"
            | "docid"
            | "paperlessdocumentid"
            | "xpaperlessdocumentid"
            | "xdocumentid"
            | "documentpk"
    )
}

fn is_document_url_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    normalized == "documentdownloadurl"
        || normalized == "downloadurl"
        || normalized == "documenturl"
        || normalized == "paperlessdocumenturl"
        || normalized == "docurl"
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn parse_document_id_value(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => parse_document_id(text).or_else(|| document_id_from_url(text)),
        _ => None,
    }
}

fn parse_document_id(value: &str) -> Option<i64> {
    value.trim().parse().ok()
}

fn document_id_from_url(value: &str) -> Option<i64> {
    let parts = value
        .split(['/', '?', '#'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    parts.windows(2).find_map(|window| {
        if window[0].eq_ignore_ascii_case("documents") {
            window[1].parse().ok()
        } else {
            None
        }
    })
}

async fn update_paperless_if_signed(
    state: &AppState,
    request_id: &str,
    document_id: Option<i64>,
    pdfsig: Option<&PdfsigDebug>,
) -> PaperlessDebug {
    let signatures = pdfsig
        .map(|pdfsig| actionable_signatures(&pdfsig.signatures))
        .unwrap_or_default();

    if signatures.is_empty() {
        return PaperlessDebug {
            attempted: false,
            document_id,
            result: None,
            skipped_reason: Some("no signatures detected".to_owned()),
            error: None,
        };
    }

    let Some(document_id) = document_id else {
        warn!(
            request_id,
            "PDF signatures found, but no Paperless document id was provided"
        );
        return PaperlessDebug {
            attempted: false,
            document_id: None,
            result: None,
            skipped_reason: Some("missing Paperless document id".to_owned()),
            error: None,
        };
    };

    let Some(paperless) = &state.paperless else {
        warn!(
            request_id,
            document_id, "PDF signatures found, but Paperless API is not configured"
        );
        return PaperlessDebug {
            attempted: false,
            document_id: Some(document_id),
            result: None,
            skipped_reason: Some("Paperless API is not configured".to_owned()),
            error: None,
        };
    };

    match paperless
        .update_signed_document(document_id, &signatures)
        .await
    {
        Ok(result) => {
            info!(
                request_id,
                document_id,
                tag_id = result.tag_id,
                tag_created = result.tag_created,
                "Paperless document updated after signature detection"
            );
            PaperlessDebug {
                attempted: true,
                document_id: Some(document_id),
                result: Some(result),
                skipped_reason: None,
                error: None,
            }
        }
        Err(err) => {
            let message = err.to_string();
            error!(
                request_id,
                document_id,
                error = %message,
                "failed to update Paperless document"
            );
            PaperlessDebug {
                attempted: true,
                document_id: Some(document_id),
                result: None,
                skipped_reason: None,
                error: Some(message),
            }
        }
    }
}

fn find_pdf_in_json(value: &Value, path: &str) -> Option<ExtractedPdf> {
    match value {
        Value::String(text) => try_decode_json_pdf(path, text),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .find_map(|(index, value)| find_pdf_in_json(value, &format!("{path}[{index}]"))),
        Value::Object(values) => values
            .iter()
            .find_map(|(key, value)| find_pdf_in_json(value, &format!("{path}.{key}"))),
        _ => None,
    }
}

fn try_decode_json_pdf(path: &str, text: &str) -> Option<ExtractedPdf> {
    if !should_try_base64(path, text) {
        return None;
    }

    let payload = if let Some((prefix, data)) = text.split_once(',') {
        if prefix.to_ascii_lowercase().contains("base64") {
            data
        } else {
            text
        }
    } else {
        text
    };
    let compact = payload.split_whitespace().collect::<String>();

    let decoded = general_purpose::STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| general_purpose::URL_SAFE.decode(compact.as_bytes()))
        .ok()?;

    if !is_pdf(&decoded) {
        return None;
    }

    Some(ExtractedPdf {
        bytes: Bytes::from(decoded),
        source: format!("JSON field {path}"),
        file_name: Some("webhook-json.pdf".to_owned()),
    })
}

fn should_try_base64(path: &str, text: &str) -> bool {
    if text.starts_with("data:application/pdf;base64,") || text.starts_with("JVBER") {
        return true;
    }

    if text.len() < 64 {
        return false;
    }

    let path = path.to_ascii_lowercase();
    ["pdf", "file", "document", "content", "data", "base64"]
        .iter()
        .any(|needle| path.contains(needle))
}

async fn run_pdfsig(config: &Config, request_id: &str, pdf_path: &Path) -> PdfsigDebug {
    let command_path = match resolve_pdfsig(config) {
        Some(path) => path,
        None => {
            warn!(
                request_id,
                "pdfsig command not found; skipping PDF signature check"
            );
            return PdfsigDebug {
                available: false,
                command: None,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("pdfsig command not found".to_owned()),
                contains_signatures: false,
                actionable_signatures_count: 0,
                ignored_signatures_count: 0,
                signatures: Vec::new(),
            };
        }
    };

    info!(
        request_id,
        command = %command_path.display(),
        pdf_path = %pdf_path.display(),
        "running pdfsig"
    );

    match Command::new(&command_path).arg(pdf_path).output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let signatures = parse_pdfsig_output(&stdout);
            let actionable_signatures_count = actionable_signatures(&signatures).len();
            let ignored_signatures_count = signatures
                .iter()
                .filter(|signature| signature.ignored)
                .count();
            PdfsigDebug {
                available: true,
                command: Some(command_path.display().to_string()),
                exit_code: output.status.code(),
                stdout,
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                error: None,
                contains_signatures: actionable_signatures_count > 0,
                actionable_signatures_count,
                ignored_signatures_count,
                signatures,
            }
        }
        Err(err) => {
            let message = err.to_string();
            warn!(
                request_id,
                command = %command_path.display(),
                error = %message,
                "failed to run pdfsig"
            );
            PdfsigDebug {
                available: false,
                command: Some(command_path.display().to_string()),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(message),
                contains_signatures: false,
                actionable_signatures_count: 0,
                ignored_signatures_count: 0,
                signatures: Vec::new(),
            }
        }
    }
}

fn resolve_pdfsig(config: &Config) -> Option<PathBuf> {
    if let Some(path) = &config.pdfsig_path {
        if path.exists() {
            return Some(path.clone());
        }
        warn!(path = %path.display(), "configured PDFSIG_PATH does not exist");
    }

    which::which("pdfsig").ok()
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if name.trim().eq_ignore_ascii_case("boundary") {
            Some(value.trim().trim_matches('"').to_owned())
        } else {
            None
        }
    })
}

fn looks_like_json(body: &[u8]) -> bool {
    body.iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .map(|byte| byte == b'{' || byte == b'[')
        .unwrap_or(false)
}

fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF")
}

fn headers_to_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = if should_redact_header(name.as_str()) {
                "<redacted>".to_owned()
            } else {
                value
                    .to_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|_| general_purpose::STANDARD.encode(value.as_bytes()))
            };
            (name.as_str().to_owned(), value)
        })
        .collect()
}

fn should_redact_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "set-cookie" | "x-api-key"
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn request_id() -> String {
    format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        Uuid::new_v4()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pdf_magic_header() {
        assert!(is_pdf(b"%PDF-1.7\n"));
        assert!(!is_pdf(b"not a pdf"));
    }

    #[test]
    fn extracts_pdf_from_json_data_url() {
        let encoded = general_purpose::STANDARD.encode(b"%PDF-1.7\nfake");
        let value = serde_json::json!({
            "document": {
                "pdf": format!("data:application/pdf;base64,{encoded}")
            }
        });

        let pdf = find_pdf_in_json(&value, "$").expect("pdf should be decoded");
        assert_eq!(pdf.source, "JSON field $.document.pdf");
        assert_eq!(pdf.bytes.as_ref(), b"%PDF-1.7\nfake");
    }

    #[test]
    fn normalizes_multipart_boundary() {
        assert_eq!(
            multipart_boundary("multipart/form-data; Boundary=\"abc123\""),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn extracts_document_id_from_paperless_url() {
        assert_eq!(
            document_id_from_url("https://paperless.example/api/documents/123/download/"),
            Some(123)
        );
    }

    #[test]
    fn detects_document_id_keys() {
        assert!(is_document_id_key("DOCUMENT_ID"));
        assert!(is_document_id_key("$.document_id"));
        assert!(is_document_id_key("x-paperless-document-id"));
    }

    #[test]
    fn detects_form_urlencoded_body_without_content_type() {
        let body = b"document_id=6&doc_url=https://paperless.infolab.com.pl/documents/6/";
        assert!(should_parse_body_as_form_urlencoded("", body));

        let values = form_urlencoded_parameters(body);
        assert_eq!(values[0].name, "document_id");
        assert_eq!(values[0].value, "6");
        assert_eq!(values[1].name, "doc_url");
        assert_eq!(
            values[1].value,
            "https://paperless.infolab.com.pl/documents/6/"
        );
    }
}
