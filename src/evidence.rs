//! Read-only repository evidence service for isolated Codex reviewers.
//!
//! The hidden server mode constructs no [`crate::tools::App`] and exposes no review or
//! process-execution tool. Repository-derived model input is data to this module, never an
//! argv fragment or shell command.

mod core;
mod git;

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SERVER_FLAG: &str = "--evidence-server";
pub const SERVER_NAME: &str = "cross_review_evidence";
pub const SCHEMA_VERSION: u32 = 1;
const PROTOCOL: &str = "2025-06-18";
const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BUNDLE_FILES: usize = 256;
const STALE_BUNDLE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub const TOOLS: [&str; 7] = [
    "repository_scope",
    "repository_list",
    "repository_search",
    "repository_read",
    "repository_change",
    "repository_history",
    "repository_revision",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsKind {
    Git,
    Perforce,
}

impl From<crate::config::Vcs> for VcsKind {
    fn from(value: crate::config::Vcs) -> Self {
        match value {
            crate::config::Vcs::Git => Self::Git,
            crate::config::Vcs::Perforce => Self::Perforce,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_request_bytes: u32,
    pub max_response_bytes: u32,
    pub max_total_bytes: u64,
    pub max_calls: u32,
    pub max_path_bytes: u32,
    pub max_query_bytes: u32,
    pub max_file_bytes: u32,
    pub max_line_bytes: u32,
    pub max_files: u32,
    pub max_entries: u32,
    pub default_entries: u32,
    pub max_matches: u32,
    pub default_matches: u32,
    pub max_lines: u32,
    pub default_lines: u32,
    pub max_change_bytes: u32,
    pub default_change_bytes: u32,
    pub max_history: u32,
    pub default_history: u32,
    pub operation_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_bytes: 64 * 1024,
            max_response_bytes: 256 * 1024,
            max_total_bytes: 2 * 1024 * 1024,
            max_calls: 96,
            max_path_bytes: 4096,
            max_query_bytes: 4096,
            max_file_bytes: 1024 * 1024,
            max_line_bytes: 16 * 1024,
            max_files: 20_000,
            max_entries: 1000,
            default_entries: 200,
            max_matches: 1000,
            default_matches: 100,
            max_lines: 2000,
            default_lines: 300,
            max_change_bytes: 192 * 1024,
            default_change_bytes: 48 * 1024,
            max_history: 200,
            default_history: 40,
            operation_timeout_ms: 15_000,
        }
    }
}

impl Limits {
    fn validate(&self) -> Result<(), EvidenceError> {
        let positive = [
            self.max_request_bytes as u64,
            self.max_response_bytes as u64,
            self.max_total_bytes,
            self.max_calls as u64,
            self.max_path_bytes as u64,
            self.max_query_bytes as u64,
            self.max_file_bytes as u64,
            self.max_line_bytes as u64,
            self.max_files as u64,
            self.max_entries as u64,
            self.max_matches as u64,
            self.max_lines as u64,
            self.max_change_bytes as u64,
            self.max_history as u64,
            self.operation_timeout_ms,
        ];
        if positive.contains(&0)
            || self.default_entries > self.max_entries
            || self.default_matches > self.max_matches
            || self.default_lines > self.max_lines
            || self.default_change_bytes > self.max_change_bytes
            || self.default_history > self.max_history
            || self.max_response_bytes as u64 > self.max_total_bytes
            || self.max_request_bytes > 1024 * 1024
            || self.max_response_bytes > 1024 * 1024
        {
            return Err(EvidenceError::new(
                "invalid_bundle",
                "evidence limits are inconsistent",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub schema_version: u32,
    pub nonce: String,
    pub root: String,
    pub vcs: VcsKind,
    pub change_label: String,
    pub status_summary: String,
    pub change: Option<String>,
    pub limits: Limits,
    pub initial_stamp: String,
}

impl Bundle {
    pub fn create(
        root: &Path,
        vcs: crate::config::Vcs,
        nonce: &str,
        change_label: String,
        status_summary: String,
        change: Option<String>,
    ) -> Result<Self, EvidenceError> {
        validate_nonce(nonce)?;
        let canonical = fs::canonicalize(root).map_err(|e| {
            EvidenceError::new("invalid_root", format!("cannot canonicalize root: {e}"))
        })?;
        let limits = Limits::default();
        let initial_stamp = core::initial_stamp(&canonical, &limits)?;
        let bundle = Self {
            schema_version: SCHEMA_VERSION,
            nonce: nonce.to_string(),
            root: canonical.to_string_lossy().to_string(),
            vcs: vcs.into(),
            change_label,
            status_summary,
            change,
            limits,
            initial_stamp,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    pub fn validate(&self) -> Result<(), EvidenceError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(EvidenceError::new(
                "schema_mismatch",
                format!("bundle schema {} is unsupported", self.schema_version),
            ));
        }
        validate_nonce(&self.nonce)?;
        if self.root.is_empty()
            || self.change_label.len() > 4096
            || self.status_summary.len() > 16 * 1024
        {
            return Err(EvidenceError::new(
                "invalid_bundle",
                "bundle strings are missing or too long",
            ));
        }
        if self.change.as_ref().is_some_and(|c| c.len() > 1024 * 1024) {
            return Err(EvidenceError::new(
                "invalid_bundle",
                "captured change exceeds bundle cap",
            ));
        }
        if self.initial_stamp.len() != 64
            || !self.initial_stamp.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(EvidenceError::new(
                "invalid_bundle",
                "initial drift stamp is invalid",
            ));
        }
        self.limits.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceError {
    pub code: &'static str,
    pub message: String,
}

impl EvidenceError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EvidenceError {}

pub struct BundleFile {
    pub path: PathBuf,
}

impl Drop for BundleFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn write_bundle(
    cfg: &crate::config::Config,
    bundle: &Bundle,
) -> Result<BundleFile, EvidenceError> {
    let dir = capability_dir(cfg)?;
    let path = dir.join(format!("{}-evidence.json", bundle.nonce));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            EvidenceError::new(
                "bundle_write_failed",
                format!("cannot create capability bundle: {e}"),
            )
        })?;
    serde_json::to_writer(&mut file, bundle).map_err(|e| {
        EvidenceError::new(
            "bundle_write_failed",
            format!("cannot encode capability bundle: {e}"),
        )
    })?;
    file.flush()
        .map_err(|e| EvidenceError::new("bundle_write_failed", e.to_string()))?;
    Ok(BundleFile { path })
}

fn capability_dir(cfg: &crate::config::Config) -> Result<PathBuf, EvidenceError> {
    let candidates = [cfg.state_dir.clone(), std::env::temp_dir()];
    let mut last = None;
    for base in candidates {
        if let Err(e) = fs::create_dir_all(&base) {
            last = Some(e);
            continue;
        }
        let Ok(base) = fs::canonicalize(&base) else {
            continue;
        };
        if crate::reviewer::is_within(&base, &cfg.cwd)
            || !crate::reviewer::verified_non_git_dir(&base)
        {
            continue;
        }
        let dir = base.join("cross-review-evidence-bundles");
        if let Err(e) = fs::create_dir_all(&dir) {
            last = Some(e);
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&dir) else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(&canonical) else {
            continue;
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                last = Some(io::Error::other("capability directory is a reparse point"));
                continue;
            }
        }
        if meta.is_dir()
            && !crate::reviewer::is_within(&canonical, &cfg.cwd)
            && crate::reviewer::verified_non_git_dir(&canonical)
        {
            cleanup_capability_dir(&canonical)?;
            return Ok(canonical);
        }
    }
    Err(EvidenceError::new(
        "bundle_write_failed",
        last.map(|e| e.to_string())
            .unwrap_or_else(|| "no safe capability directory is available".to_string()),
    ))
}

fn cleanup_capability_dir(dir: &Path) -> Result<(), EvidenceError> {
    let now = std::time::SystemTime::now();
    let mut retained = 0usize;
    for entry in
        fs::read_dir(dir).map_err(|e| EvidenceError::new("bundle_write_failed", e.to_string()))?
    {
        let entry = entry.map_err(|e| EvidenceError::new("bundle_write_failed", e.to_string()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with("-evidence.json") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|e| EvidenceError::new("bundle_write_failed", e.to_string()))?;
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_BUNDLE_AGE);
        if stale && metadata.is_file() {
            let _ = fs::remove_file(entry.path());
        } else {
            retained = retained.saturating_add(1);
        }
    }
    if retained >= MAX_BUNDLE_FILES {
        return Err(EvidenceError::new(
            "bundle_write_failed",
            "capability directory reached its bounded live-file limit",
        ));
    }
    Ok(())
}

fn read_bundle(path: &Path, expected_nonce: &str) -> Result<Bundle, EvidenceError> {
    validate_nonce(expected_nonce)?;
    let meta = fs::symlink_metadata(path).map_err(|e| {
        EvidenceError::new("bundle_unavailable", format!("cannot inspect bundle: {e}"))
    })?;
    if !meta.is_file() || meta.len() > MAX_BUNDLE_BYTES {
        return Err(EvidenceError::new(
            "invalid_bundle",
            "bundle is not a bounded regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(EvidenceError::new(
                "invalid_bundle",
                "bundle cannot be a reparse point",
            ));
        }
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| EvidenceError::new("bundle_unavailable", e.to_string()))?
        .take(MAX_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| EvidenceError::new("bundle_unavailable", e.to_string()))?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(EvidenceError::new(
            "invalid_bundle",
            "bundle grew beyond its cap",
        ));
    }
    let bundle: Bundle = serde_json::from_slice(&bytes)
        .map_err(|e| EvidenceError::new("invalid_bundle", format!("cannot decode bundle: {e}")))?;
    bundle.validate()?;
    if bundle.nonce != expected_nonce {
        return Err(EvidenceError::new(
            "nonce_mismatch",
            "bundle nonce does not match invocation",
        ));
    }
    Ok(bundle)
}

pub fn serve_stdio(path: &Path, expected_nonce: &str) -> Result<(), EvidenceError> {
    let bundle = read_bundle(path, expected_nonce)?;
    let max_request = bundle.limits.max_request_bytes as usize;
    let max_response = bundle.limits.max_response_bytes as usize;
    let cancel = Arc::new(AtomicBool::new(false));
    let cancellations = Arc::new(RequestCancellations::default());
    let core = core::Core::new_with_cancel(bundle, Arc::clone(&cancel))?;
    let (sender, receiver) = mpsc::sync_channel::<Result<Vec<u8>, EvidenceError>>(8);
    let reader_cancel = Arc::clone(&cancel);
    let reader_cancellations = Arc::clone(&cancellations);
    let reader = std::thread::Builder::new()
        .name("evidence-stdin".to_string())
        .spawn(move || read_requests(max_request, sender, reader_cancel, reader_cancellations))
        .map_err(|e| EvidenceError::new("protocol_read_failed", e.to_string()))?;
    let stdout = io::stdout();
    let result = serve_requests(
        receiver,
        BufWriter::new(stdout.lock()),
        core,
        max_response,
        cancel,
        cancellations,
    );
    if result.is_ok() {
        // A successful loop ends only after stdin EOF closed the channel, so the reader is done.
        let _ = reader.join();
    }
    result
}

#[derive(Default)]
struct RequestCancellations {
    active: Mutex<Option<String>>,
    pending: Mutex<HashSet<String>>,
    transport_closed: AtomicBool,
}

impl RequestCancellations {
    fn notify(&self, id: String, cancel: &AtomicBool) {
        if id.len() > 256 {
            return;
        }
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.as_deref() == Some(id.as_str()) {
            cancel.store(true, Ordering::Release);
        } else {
            drop(active);
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() < 256 {
                pending.insert(id);
            }
        }
    }

    fn begin(&self, id: String, cancel: &AtomicBool) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        cancel.store(
            pending || self.transport_closed.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    fn finish(&self, cancel: &AtomicBool) -> bool {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let transport_closed = self.transport_closed.load(Ordering::Acquire);
        let request_cancelled = cancel.load(Ordering::Acquire) && !transport_closed;
        cancel.store(transport_closed, Ordering::Release);
        request_cancelled
    }

    fn close_transport(&self, cancel: &AtomicBool) {
        self.transport_closed.store(true, Ordering::Release);
        cancel.store(true, Ordering::Release);
    }
}

fn read_requests(
    max_request: usize,
    sender: mpsc::SyncSender<Result<Vec<u8>, EvidenceError>>,
    cancel: Arc<AtomicBool>,
    cancellations: Arc<RequestCancellations>,
) {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    loop {
        let mut bytes = Vec::new();
        let result = reader
            .by_ref()
            .take(max_request as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|e| EvidenceError::new("protocol_read_failed", e.to_string()));
        match result {
            Ok(0) => break,
            Ok(_) if bytes.len() > max_request || !bytes.ends_with(b"\n") => {
                let _ = sender.send(Err(EvidenceError::new(
                    "request_too_large",
                    "MCP request exceeded its byte cap",
                )));
                break;
            }
            Ok(_) => {
                let parsed = serde_json::from_slice::<Value>(&bytes).ok();
                if parsed
                    .as_ref()
                    .and_then(|v| v.get("method"))
                    .and_then(Value::as_str)
                    == Some("notifications/cancelled")
                {
                    if let Some(id) = parsed.as_ref().and_then(|v| v.pointer("/params/requestId")) {
                        cancellations.notify(id.to_string(), &cancel);
                    }
                    continue;
                }
                if sender.send(Ok(bytes)).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = sender.send(Err(e));
                break;
            }
        }
    }
    // EOF is the authoritative signal that Codex (or its MCP transport) no longer owns this
    // capability. This runs concurrently with provider work, so reviewer::run observes it and
    // closes the provider's kill-on-close job without waiting for the operation deadline.
    cancellations.close_transport(&cancel);
}

fn serve_requests<W: Write>(
    receiver: mpsc::Receiver<Result<Vec<u8>, EvidenceError>>,
    mut writer: W,
    mut core: core::Core,
    max_response: usize,
    cancel: Arc<AtomicBool>,
    cancellations: Arc<RequestCancellations>,
) -> Result<(), EvidenceError> {
    while let Ok(message) = receiver.recv() {
        let bytes = message?;
        let request: Value = serde_json::from_slice(&bytes)
            .map_err(|e| EvidenceError::new("invalid_jsonrpc", format!("invalid JSON-RPC: {e}")))?;
        let request_id = request.get("id").map(Value::to_string);
        if let Some(id) = &request_id {
            cancellations.begin(id.clone(), &cancel);
        }
        let Some(response) = handle(&request, &mut core) else {
            if request_id.is_some() {
                cancellations.finish(&cancel);
            }
            continue;
        };
        if request_id.is_some() && cancellations.finish(&cancel) {
            // MCP cancellation abandons this request only. Suppress its response, then clear the
            // request-scoped flag so later calls remain usable on the same transport.
            continue;
        }
        let mut encoded = serde_json::to_vec(&response)
            .map_err(|e| EvidenceError::new("protocol_write_failed", e.to_string()))?;
        if encoded.len() > max_response {
            let replacement = if request.get("method").and_then(Value::as_str) == Some("tools/call")
            {
                tool_error_response(
                    request.get("id").cloned().unwrap_or(Value::Null),
                    "response_too_large",
                    "evidence response exceeded its byte cap; retry with a smaller page limit",
                )
            } else {
                json!({
                    "jsonrpc":"2.0",
                    "id":request.get("id").cloned().unwrap_or(Value::Null),
                    "error":{"code":-32603,"message":"MCP response exceeded its byte cap"}
                })
            };
            encoded = serde_json::to_vec(&replacement)
                .map_err(|e| EvidenceError::new("protocol_write_failed", e.to_string()))?;
        }
        writer
            .write_all(&encoded)
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush())
            .map_err(|e| EvidenceError::new("protocol_write_failed", e.to_string()))?;
    }
    Ok(())
}

fn handle(request: &Value, core: &mut core::Core) -> Option<Value> {
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name":"cross-review-evidence","version":env!("CARGO_PKG_VERSION")},
            "instructions":"Read-only, path-confined repository evidence. Start with repository_scope when repository context is needed."
        }),
        "tools/list" => json!({"tools": tool_definitions()}),
        "tools/call" => {
            let params = request.get("params").and_then(Value::as_object);
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let empty = json!({});
            let arguments = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
            match core.call(name, arguments) {
                Ok(structured) => json!({
                    "content":[{"type":"text","text":"Repository evidence returned in structuredContent."}],
                    "structuredContent":structured,
                    "isError":false
                }),
                Err(error) => tool_error_result(error.code, &error.message),
            }
        }
        "ping" => json!({}),
        other => {
            return Some(json!({
                "jsonrpc":"2.0","id":id,
                "error":{"code":-32601,"message":format!("method not found: {other}")}
            }))
        }
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn tool_error_result(code: &str, message: &str) -> Value {
    json!({
        "content":[{"type":"text","text":format!("{code}: {message}")}],
        "structuredContent":{"error":{"code":code,"message":message}},
        "isError":true
    })
}

fn tool_error_response(id: Value, code: &str, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":tool_error_result(code, message)})
}

pub fn handshake(exe: &Path, bundle_file: &Path, nonce: &str) -> Result<(), EvidenceError> {
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"cross-review-parent","version":env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ];
    let mut input = String::new();
    for request in requests {
        input.push_str(&request.to_string());
        input.push('\n');
    }
    let mut command = Command::new(exe);
    command.arg(SERVER_FLAG).arg(bundle_file).arg(nonce);
    let cancel = AtomicBool::new(false);
    let output =
        crate::reviewer::run(command, &input, Duration::from_secs(10), &cancel).map_err(|e| {
            EvidenceError::new(
                "handshake_failed",
                format!("cannot start evidence server: {e}"),
            )
        })?;
    if !output.success {
        return Err(EvidenceError::new("handshake_failed", output.diagnostics()));
    }
    let responses: Vec<Value> = output
        .stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).map_err(|e| {
                EvidenceError::new("handshake_failed", format!("invalid handshake JSON: {e}"))
            })
        })
        .collect::<Result<_, _>>()?;
    let init = responses
        .iter()
        .find(|v| v.get("id") == Some(&json!(1)))
        .ok_or_else(|| EvidenceError::new("handshake_failed", "initialize response missing"))?;
    if init["result"]["serverInfo"]["name"] != "cross-review-evidence" {
        return Err(EvidenceError::new(
            "handshake_failed",
            "unexpected evidence server identity",
        ));
    }
    let listed = responses
        .iter()
        .find(|v| v.get("id") == Some(&json!(2)))
        .and_then(|v| v["result"]["tools"].as_array())
        .ok_or_else(|| EvidenceError::new("handshake_failed", "tools/list response missing"))?;
    let names: Vec<&str> = listed.iter().filter_map(|v| v["name"].as_str()).collect();
    if names != TOOLS
        || listed.iter().any(|v| {
            v["inputSchema"]["additionalProperties"] != false
                || v["outputSchema"]["additionalProperties"] != false
        })
    {
        return Err(EvidenceError::new(
            "handshake_failed",
            "evidence tool surface does not match the allow-list",
        ));
    }
    Ok(())
}

/// Full no-model readiness check used by `cross_model_review_status`.
pub fn readiness(cfg: &crate::config::Config) -> Result<(), EvidenceError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = format!("status-{}-{nanos}", std::process::id());
    let sterile = if cfg.isolate_reviewer {
        Some(
            crate::reviewer::codex_sterile_dir(cfg, "cross-review-status")
                .map_err(|e| EvidenceError::new("sterile_root_unavailable", e.to_string()))?,
        )
    } else {
        None
    };
    let bundle = Bundle::create(
        &cfg.cwd,
        cfg.vcs,
        &nonce,
        "status preflight (no selected change)".to_string(),
        "no-model evidence readiness check".to_string(),
        None,
    )?;
    let file = write_bundle(cfg, &bundle)?;
    let exe = std::env::current_exe()
        .map_err(|e| EvidenceError::new("handshake_failed", e.to_string()))?;
    let result = handshake(&exe, &file.path, &nonce);
    drop(file);
    drop(sterile);
    result
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "repository_scope",
            "Return repository identity, selected change, limits, and drift state.",
            &[],
            &[],
        ),
        tool(
            "repository_list",
            "List one repository-relative directory without following links.",
            &[
                ("path", "string"),
                ("cursor", "string"),
                ("limit", "integer"),
            ],
            &[],
        ),
        tool(
            "repository_search",
            "Search UTF-8 files for a literal string.",
            &[
                ("query", "string"),
                ("path", "string"),
                ("cursor", "string"),
                ("limit", "integer"),
            ],
            &[],
        ),
        tool(
            "repository_read",
            "Read numbered UTF-8 lines from one repository-relative file.",
            &[
                ("path", "string"),
                ("start_line", "integer"),
                ("line_count", "integer"),
            ],
            &["path"],
        ),
        tool(
            "repository_change",
            "Read the immutable selected change captured before this turn.",
            &[("cursor", "string"), ("limit_bytes", "integer")],
            &[],
        ),
        tool(
            "repository_history",
            "Read bounded Git commit history; unsupported for Perforce.",
            &[
                ("path", "string"),
                ("before", "string"),
                ("cursor", "string"),
                ("limit", "integer"),
            ],
            &[],
        ),
        tool(
            "repository_revision",
            "Read a bounded full Git revision; unsupported for Perforce.",
            &[
                ("id", "string"),
                ("path", "string"),
                ("cursor", "string"),
                ("limit_bytes", "integer"),
            ],
            &[],
        ),
    ]
}

fn tool(name: &str, description: &str, properties: &[(&str, &str)], required: &[&str]) -> Value {
    let mut props = serde_json::Map::new();
    for (key, kind) in properties {
        let mut schema = json!({"type":kind});
        if *kind == "integer" {
            schema["minimum"] = json!(1);
        }
        props.insert((*key).to_string(), schema);
    }
    let mut input_schema = json!({
        "type":"object","properties":props,"required":required,"additionalProperties":false
    });
    if name == "repository_search" {
        input_schema["oneOf"] = json!([{"required":["query"]},{"required":["cursor"]}]);
    } else if name == "repository_revision" {
        input_schema["oneOf"] = json!([{"required":["id"]},{"required":["cursor"]}]);
    }
    json!({
        "name":name,
        "description":description,
        "inputSchema":input_schema,
        "outputSchema":output_schema(name),
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}
    })
}

fn output_schema(name: &str) -> Value {
    let page = |field: &str, item: Value| {
        json!({
            "type":"object",
            "properties":{
                field:{"type":"array","items":item},
                "complete":{"type":"boolean"},
                "truncated":{"type":"boolean"},
                "cursor":{"type":["string","null"]}
            },
            "required":[field,"complete","truncated","cursor"],
            "additionalProperties":false
        })
    };
    let text_page = |field: &str| {
        json!({
            "type":"object",
            "properties":{
                field:{"type":"string"},
                "end_byte":{"type":"integer"},
                "complete":{"type":"boolean"},
                "truncated":{"type":"boolean"},
                "cursor":{"type":["string","null"]}
            },
            "required":[field,"end_byte","complete","truncated","cursor"],
            "additionalProperties":false
        })
    };
    match name {
        "repository_scope" => json!({
            "type":"object",
            "properties":{
                "schema_version":{"type":"integer"},"nonce":{"type":"string"},
                "root":{"type":"string"},"vcs":{"type":"string"},
                "change_label":{"type":"string"},"status_summary":{"type":"string"},
                "limits":limits_schema(),"initial_stamp":{"type":"string"},
                "excluded_directory_names":{"type":"array","items":{"type":"string"}},
                "current_stamp":{"type":"string"},"drifted":{"type":"boolean"},
                "complete":{"type":"boolean"},"truncated":{"type":"boolean"},
                "cursor":{"type":"null"}
            },
            "required":["schema_version","nonce","root","vcs","change_label","status_summary","limits","excluded_directory_names","initial_stamp","current_stamp","drifted","complete","truncated","cursor"],
            "additionalProperties":false
        }),
        "repository_list" => page(
            "entries",
            json!({
                "type":"object","properties":{"path":{"type":"string"},"type":{"type":"string"},"bytes":{"type":"integer"}},
                "required":["path","type","bytes"],"additionalProperties":false
            }),
        ),
        "repository_search" => page(
            "matches",
            json!({
                "type":"object","properties":{"path":{"type":"string"},"line":{"type":"integer"},"excerpt":{"type":"string"}},
                "required":["path","line","excerpt"],"additionalProperties":false
            }),
        ),
        "repository_read" => json!({
            "type":"object","properties":{
                "path":{"type":"string"},"bytes":{"type":"integer"},"sha256":{"type":"string"},
                "total_lines":{"type":"integer"},
                "lines":{"type":"array","items":{"type":"object","properties":{"line":{"type":"integer"},"text":{"type":"string"}},"required":["line","text"],"additionalProperties":false}},
                "complete":{"type":"boolean"},"truncated":{"type":"boolean"},"cursor":{"type":"null"},"drifted":{"type":"boolean"}
            },
            "required":["path","bytes","sha256","total_lines","lines","complete","truncated","cursor","drifted"],
            "additionalProperties":false
        }),
        "repository_change" => json!({
            "type":"object","properties":{"label":{"type":"string"},"content":{"type":"string"},"bytes":{"type":"integer"},"complete":{"type":"boolean"},"truncated":{"type":"boolean"},"cursor":{"type":["string","null"]}},
            "required":["label","content","bytes","complete","truncated","cursor"],"additionalProperties":false
        }),
        "repository_history" => page(
            "commits",
            json!({
                "type":"object","properties":{"id":{"type":"string"},"authored":{"type":"string"},"author":{"type":"string"},"subject":{"type":"string"}},
                "required":["id","authored","author","subject"],"additionalProperties":false
            }),
        ),
        "repository_revision" => text_page("content"),
        _ => json!({"type":"object","properties":{},"additionalProperties":false}),
    }
}

fn limits_schema() -> Value {
    let names = [
        "max_request_bytes",
        "max_response_bytes",
        "max_total_bytes",
        "max_calls",
        "max_path_bytes",
        "max_query_bytes",
        "max_file_bytes",
        "max_line_bytes",
        "max_files",
        "max_entries",
        "default_entries",
        "max_matches",
        "default_matches",
        "max_lines",
        "default_lines",
        "max_change_bytes",
        "default_change_bytes",
        "max_history",
        "default_history",
        "operation_timeout_ms",
    ];
    let properties: serde_json::Map<String, Value> = names
        .iter()
        .map(|name| ((*name).to_string(), json!({"type":"integer","minimum":1})))
        .collect();
    json!({
        "type":"object","properties":properties,"required":names,
        "additionalProperties":false
    })
}

fn validate_nonce(nonce: &str) -> Result<(), EvidenceError> {
    if nonce.is_empty()
        || nonce.len() > 128
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(EvidenceError::new(
            "invalid_nonce",
            "nonce must be 1..=128 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn bundle(root: &Path) -> Bundle {
        Bundle::create(
            root,
            crate::config::Vcs::Git,
            "nonce-1",
            "working tree".into(),
            "captured".into(),
            Some("diff".into()),
        )
        .unwrap()
    }

    #[test]
    fn dispatcher_exposes_only_closed_read_only_evidence_tools() {
        let dir = temp_dir("evidence-mcp");
        let mut core = core::Core::new(bundle(dir.as_path())).unwrap();
        let listed = handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            &mut core,
        )
        .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len());
        assert!(tools
            .iter()
            .all(|t| t["inputSchema"]["additionalProperties"] == false));
        assert!(tools
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true));
        assert!(tools.iter().all(|t| !t["name"]
            .as_str()
            .unwrap()
            .starts_with("cross_model_review")));
    }

    #[test]
    fn scope_accepts_omitted_arguments_and_unknown_tools_fail_in_band() {
        let dir = temp_dir("evidence-call");
        let mut core = core::Core::new(bundle(dir.as_path())).unwrap();
        let scope = handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"repository_scope"}}), &mut core).unwrap();
        assert_eq!(scope["result"]["isError"], false);
        let unknown = handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cross_model_review","arguments":{}}}), &mut core).unwrap();
        assert_eq!(unknown["result"]["isError"], true);
    }

    #[test]
    fn bundle_nonce_is_checked_and_unknown_fields_are_rejected() {
        let dir = temp_dir("evidence-bundle");
        let mut value = serde_json::to_value(bundle(dir.as_path())).unwrap();
        value["surprise"] = json!(true);
        assert!(serde_json::from_value::<Bundle>(value).is_err());
        assert!(validate_nonce("../bad").is_err());
    }

    #[test]
    fn cancellation_is_request_scoped_and_transport_close_is_sticky() {
        let state = RequestCancellations::default();
        let cancel = AtomicBool::new(false);

        state.begin("1".into(), &cancel);
        state.notify("2".into(), &cancel);
        assert!(!cancel.load(Ordering::Acquire));
        assert!(!state.finish(&cancel));

        state.begin("2".into(), &cancel);
        assert!(cancel.load(Ordering::Acquire));
        assert!(state.finish(&cancel));
        assert!(!cancel.load(Ordering::Acquire));

        state.begin("3".into(), &cancel);
        state.notify("3".into(), &cancel);
        assert!(cancel.load(Ordering::Acquire));
        assert!(state.finish(&cancel));
        assert!(!cancel.load(Ordering::Acquire));

        state.close_transport(&cancel);
        assert!(!state.finish(&cancel));
        assert!(cancel.load(Ordering::Acquire));
    }

    #[test]
    fn maximum_change_page_fits_the_mcp_envelope_without_payload_duplication() {
        let dir = temp_dir("evidence-max-change");
        let mut bundle = bundle(dir.as_path());
        bundle.change = Some("x".repeat(bundle.limits.max_change_bytes as usize));
        let max_response = bundle.limits.max_response_bytes as usize;
        let max_change = bundle.limits.max_change_bytes;
        let mut core = core::Core::new(bundle).unwrap();
        let response = handle(
            &json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"repository_change","arguments":{"limit_bytes":max_change}}
            }),
            &mut core,
        )
        .unwrap();
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() <= max_response,
            "{} > {max_response}",
            encoded.len()
        );
        assert_eq!(
            response["result"]["content"][0]["text"],
            "Repository evidence returned in structuredContent."
        );
        assert_eq!(
            response["result"]["structuredContent"]["content"]
                .as_str()
                .unwrap()
                .len(),
            max_change as usize
        );
    }
}
