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
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SERVER_FLAG: &str = "--evidence-server";
pub const SERVER_NAME: &str = "cross_review_evidence";
/// Bumped to 2 by issue #86: `drifted` and the two stamps became nullable, and `repository_scope`
/// gained `drift_unavailable` and `scan_scope`. There is no migration to write — one binary writes
/// and reads the bundle within one process tree, and the file is deleted when the review ends — so
/// this is the reviewer-visible declaration that the contract changed, not a compatibility switch.
///
/// Bumped to 3 by the retire-capture-modes work: a live `repository_diff` tool was added, and
/// (once wired) the bundle stops carrying a pre-rendered `change`, so the reviewer-visible tool set
/// and delivery contract both changed.
pub const SCHEMA_VERSION: u32 = 3;
/// The MCP client-side per-`tools/call` ceiling we hand Codex (`tool_timeout_sec`). This is the
/// single source of truth for that ceiling: the reviewer config (`src/reviewer/codex.rs`) emits
/// it, and the evidence read watchdog (`src/evidence/core.rs`) derives its budget from it with a
/// compile-time margin, so the two can never drift into inversion (issue #61, finding f4).
pub const CODEX_TOOL_TIMEOUT_SECS: u64 = 30;
const PROTOCOL: &str = "2025-06-18";
const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BUNDLE_FILES: usize = 256;
const STALE_BUNDLE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub const TOOLS: [&str; 8] = [
    "repository_scope",
    "repository_list",
    "repository_search",
    "repository_read",
    "repository_change",
    "repository_history",
    "repository_revision",
    "repository_diff",
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

/// Cap on a stored unavailability reason, in characters. Comfortably under the 1,024-*byte* limit
/// `Drift::validate` enforces even for multi-byte text, so truncation at construction cannot be
/// undone by encoding.
const MAX_DRIFT_REASON_CHARS: usize = 240;

/// Which scan produced a drift stamp. Two stamps are comparable only when this matches: the Git
/// enumeration and the filesystem walk cover different file sets, so comparing one against the
/// other would report drift that never happened (issue #86).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StampMethod {
    Git,
    Filesystem,
}

impl StampMethod {
    /// What the reviewer is told the scan covered, in words, since "no matches" and "not scanned"
    /// are answers a model will otherwise conflate.
    pub fn scan_scope(self) -> &'static str {
        match self {
            Self::Git => {
                "files Git reports as tracked (including submodule contents) or untracked \
                          and not ignored"
            }
            Self::Filesystem => {
                "every file under the repository root except the excluded \
                                 directory names"
            }
        }
    }
}

/// A drift observation, or the reason there is not one.
///
/// `Option<String>` cannot express this: it conflates "not observed yet" with "observed and
/// unavailable", and carries no method to compare against. Both distinctions are load-bearing —
/// without the first the service re-runs an enumeration that already failed on every read, and
/// without the second a method change between capture and read time reads as drift.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Drift {
    Stamp { method: StampMethod, sha256: String },
    Unavailable { reason: String },
}

impl Drift {
    /// The reason is bounded here rather than at the call sites, because every one of them is a
    /// formatted error and one of them carries a child process's diagnostics. Unbounded, a
    /// sufficiently chatty Git failure would fail `validate` and take down the very
    /// `Bundle::create` that was degrading gracefully.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        let mut reason = reason.into();
        if reason.chars().count() > MAX_DRIFT_REASON_CHARS {
            reason = reason
                .chars()
                .take(MAX_DRIFT_REASON_CHARS)
                .collect::<String>()
                + "…";
        }
        Self::Unavailable { reason }
    }

    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::Stamp { sha256, .. } => Some(sha256),
            Self::Unavailable { .. } => None,
        }
    }

    fn validate(&self) -> Result<(), EvidenceError> {
        match self {
            Self::Stamp { sha256, .. } => {
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(EvidenceError::new(
                        "invalid_bundle",
                        "initial drift stamp is invalid",
                    ));
                }
                Ok(())
            }
            Self::Unavailable { reason } => {
                if reason.is_empty() || reason.len() > 1024 {
                    return Err(EvidenceError::new(
                        "invalid_bundle",
                        "drift unavailability reason is missing or too long",
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Compare a capture-time baseline against a live observation: `(drifted, reason)`, where a `None`
/// verdict always carries a reason. Total by construction — every way of not knowing produces
/// `None` rather than a default `false`, which is the failure mode that would matter.
pub fn compare_drift(baseline: &Drift, observed: &Drift) -> (Option<bool>, Option<String>) {
    match (baseline, observed) {
        (
            Drift::Stamp {
                method: a,
                sha256: before,
            },
            Drift::Stamp {
                method: b,
                sha256: now,
            },
        ) if a == b => (Some(before != now), None),
        (Drift::Stamp { .. }, Drift::Stamp { .. }) => (
            None,
            Some(
                "the drift baseline and this observation were produced by different scan methods, \
                 so they are not comparable"
                    .to_string(),
            ),
        ),
        (Drift::Unavailable { reason }, _) => (
            None,
            Some(format!("no drift baseline was captured: {reason}")),
        ),
        (_, Drift::Unavailable { reason }) => (
            None,
            Some(format!("the live tree could not be scanned: {reason}")),
        ),
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
    pub initial_stamp: Drift,
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
        // Deliberately not `?`: a stamp that cannot be computed is an advisory signal lost, not a
        // review to refuse. Before issue #86 a working root with more than `max_files` entries --
        // a vendored engine tree is enough -- failed here and took the whole review with it.
        let initial_stamp = core::initial_stamp(&canonical, &limits, vcs.into());
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
        self.initial_stamp.validate()?;
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

/// UTF-8 view of a path for JSON, failing closed rather than lossily mangling a non-UTF-8 path
/// into a command line the reviewer would then mis-spawn.
fn json_path(p: &Path) -> Result<&str, EvidenceError> {
    p.to_str().ok_or_else(|| {
        EvidenceError::new(
            "mcp_config_write_failed",
            format!(
                "path is not valid UTF-8 and cannot be encoded as JSON: {}",
                p.display()
            ),
        )
    })
}

/// Write the Claude `--mcp-config` JSON registering the evidence server (and, under
/// `--strict-mcp-config`, only it) for an in-scope Claude review. Returns a self-deleting handle,
/// stored alongside the bundle and removed with it. The file carries no secret: the command, the
/// bundle path, and the per-turn nonce (which is the review id, already in the bundle filename).
/// `env` is deliberately omitted so the evidence child inherits the reviewer's environment and can
/// still resolve `git` on `PATH`; it reads only the immutable bundle and the working tree.
pub fn write_claude_mcp_config(
    cfg: &crate::config::Config,
    executable: &Path,
    bundle_file: &Path,
    nonce: &str,
) -> Result<BundleFile, EvidenceError> {
    let mut servers = serde_json::Map::new();
    servers.insert(
        SERVER_NAME.to_string(),
        json!({
            "command": json_path(executable)?,
            "args": [SERVER_FLAG, json_path(bundle_file)?, nonce],
        }),
    );
    let config = json!({ "mcpServers": Value::Object(servers) });

    let dir = capability_dir(cfg)?;
    let path = dir.join(format!("{nonce}-claude-mcp.json"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            EvidenceError::new(
                "mcp_config_write_failed",
                format!("cannot create Claude MCP config: {e}"),
            )
        })?;
    serde_json::to_writer(&mut file, &config).map_err(|e| {
        EvidenceError::new(
            "mcp_config_write_failed",
            format!("cannot encode Claude MCP config: {e}"),
        )
    })?;
    file.flush()
        .map_err(|e| EvidenceError::new("mcp_config_write_failed", e.to_string()))?;
    Ok(BundleFile { path })
}

/// The nonce-bound serve-record file for one review (retire-capture-modes mechanism 3). Deterministic
/// from the capability dir and nonce, so the child (`serve_stdio`, deriving it from the bundle's own
/// directory) and the parent (which computes it here to read and to own its RAII cleanup) name the
/// same file without threading a path between processes.
pub fn serve_record_path(
    cfg: &crate::config::Config,
    nonce: &str,
) -> Result<PathBuf, EvidenceError> {
    Ok(capability_dir(cfg)?.join(format!("{nonce}-serverecord.jsonl")))
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
        // Reap both the bundle and the serve-record side-channel (f4): a serve-record file orphaned
        // by a crash must not linger to be misread by a later run that reused its id.
        if !name.ends_with("-evidence.json") && !name.ends_with("-serverecord.jsonl") {
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
    let mut core = core::Core::new_with_cancel(bundle, Arc::clone(&cancel))?;
    // The serve-record side-channel sits beside the bundle, under the same per-turn nonce, so the
    // parent (which created the bundle and knows both) can find and read it after the reviewer exits.
    if let Some(dir) = path.parent() {
        core.set_serve_record(dir.join(format!("{expected_nonce}-serverecord.jsonl")));
    }
    // A *bounded* dispatch channel: admission control that caps buffered requests, so a client that
    // pipelines faster than the serial dispatcher drains cannot grow the queue until the evidence
    // process runs out of memory (issue #61, code-review finding f8). When it is full the reader
    // blocks on `send`, which stops it draining stdin and backpressures the client through the OS
    // pipe — the explicit ingress contract.
    //
    // Receipt/budget contract (finding f6): each request's watchdog budget is measured from the
    // `Instant` stamped the moment its line is read off the wire (`read_requests`), and any wait in
    // this channel before dispatch is subtracted from the remaining budget (elapsed keeps growing).
    // The one residual is a request that sat in the OS pipe *un-read* while the reader was blocked
    // on a full queue: its pre-read wait is not in its budget. That requires the client to pipeline
    // enough concurrent requests to saturate this queue *and* stall the dispatcher — the real MCP
    // client issues evidence calls serially (request → response), so it never does. The residual is
    // the same unobservable class as raw transport latency (we cannot stamp a request before we
    // read it) and is covered by the budget margin, not a silent gap.
    let (sender, receiver) = mpsc::sync_channel::<(Instant, Result<Vec<u8>, EvidenceError>)>(8);
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
    sender: mpsc::SyncSender<(Instant, Result<Vec<u8>, EvidenceError>)>,
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
        // Stamp the receipt the moment the request is fully read off the wire, before it can wait
        // in the dispatch channel behind an in-flight request. The read watchdog measures its
        // budget from here so a queued read cannot receive a fresh budget after Codex's client-side
        // timer has been running (issue #61, code-review finding f1).
        let received = Instant::now();
        match result {
            Ok(0) => break,
            Ok(_) if bytes.len() > max_request || !bytes.ends_with(b"\n") => {
                let _ = sender.send((
                    received,
                    Err(EvidenceError::new(
                        "request_too_large",
                        "MCP request exceeded its byte cap",
                    )),
                ));
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
                if sender.send((received, Ok(bytes))).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = sender.send((received, Err(e)));
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
    receiver: mpsc::Receiver<(Instant, Result<Vec<u8>, EvidenceError>)>,
    mut writer: W,
    mut core: core::Core,
    max_response: usize,
    cancel: Arc<AtomicBool>,
    cancellations: Arc<RequestCancellations>,
) -> Result<(), EvidenceError> {
    while let Ok((received_at, message)) = receiver.recv() {
        let bytes = message?;
        let request: Value = serde_json::from_slice(&bytes)
            .map_err(|e| EvidenceError::new("invalid_jsonrpc", format!("invalid JSON-RPC: {e}")))?;
        let request_id = request.get("id").map(Value::to_string);
        if let Some(id) = &request_id {
            cancellations.begin(id.clone(), &cancel);
        }
        let Some(response) = handle(&request, &mut core, received_at) else {
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

fn handle(request: &Value, core: &mut core::Core, received_at: Instant) -> Option<Value> {
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
            match core.call_with_receipt(name, arguments, received_at) {
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

/// The internal session name for the status preflight's lease and sterile directory.
///
/// The leading NUL puts it in the control-character space that user session names are forbidden
/// from (`start_review` rejects any control character), so a real review can never pick a name that
/// collides with the status lease or its sterile directory. It is only ever hashed (into the sterile
/// dir name and the lock filename), never used as a path component, so the NUL reaches no filesystem
/// call.
const STATUS_SESSION: &str = "\u{0}cross-review-status";

/// Full no-model readiness check used by `cross_model_review_status`. Reports how drift tracking
/// came out as well as whether the service works: since issue #86 a tree too large to scan no
/// longer fails a review, so "ready, but drift is unknown here" is a state a caller can be in
/// without ever seeing it in a review, and a large repository must be distinguishable from a
/// broken service without paying for a model call to find out.
pub fn readiness(cfg: &crate::config::Config) -> Result<String, EvidenceError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = format!("status-{}-{nanos}", std::process::id());
    // The status preflight's sterile root uses a fixed name, so two concurrent status calls would
    // otherwise alias onto one directory and reap or drop it from under each other. Take the same
    // per-session lease the review path holds, keyed on the same `(state_dir, name)` pair the
    // sterile directory is named for (`STATUS_SESSION`), so a shared directory is always a
    // serialized one. Held for the whole preflight: declared before `sterile` so it outlives it.
    // Unlike a review this never resumes, but the lease costs nothing (a local file lock, no model
    // call) and keeps one rule.
    const STATUS_LEASE_WAIT: Duration = Duration::from_secs(3);
    let _status_lease = if cfg.isolate_reviewer {
        Some(
            crate::session::ExclusiveLock::acquire(
                &crate::session::session_lock_path(&cfg.state_dir, STATUS_SESSION),
                STATUS_LEASE_WAIT,
            )
            .map_err(|e| EvidenceError::new("sterile_root_unavailable", e.to_string()))?,
        )
    } else {
        None
    };
    let sterile = if cfg.isolate_reviewer {
        Some(
            crate::reviewer::codex_sterile_dir(cfg, STATUS_SESSION)
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
    result?;
    Ok(match &bundle.initial_stamp {
        Drift::Stamp { method, .. } => format!("on ({})", method.scan_scope()),
        Drift::Unavailable { reason } => format!("unavailable - {reason}"),
    })
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "repository_scope",
            "Return repository identity, selected change, limits, which files the scans cover, \
             and drift state. A null 'drifted' means drift could not be determined, not that \
             nothing changed; 'drift_unavailable' says why.",
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
            "Search UTF-8 files for a literal string. Searching a directory covers only the files \
             'scan_scope' describes, so files Git ignores are not searched; naming an ignored file \
             as 'path' searches it anyway. Check 'complete' before reading no matches as absence.",
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
        tool(
            "repository_diff",
            "Diff the live working tree against a base, on demand; unsupported for Perforce. 'base' \
             is the left side (default 'branch-base', the branch's fork point) and 'head' the right \
             (default 'worktree', the live working tree including untracked files). Each is a full \
             Git object id or one of the sentinels 'worktree', 'index', 'head', 'branch-base'. To \
             review the whole change, diff 'branch-base'..'worktree'; narrow with 'path' for focus.",
            &[
                ("base", "string"),
                ("head", "string"),
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
                "limits":limits_schema(),"initial_stamp":{"type":["string","null"]},
                "excluded_directory_names":{"type":"array","items":{"type":"string"}},
                "current_stamp":{"type":["string","null"]},"drifted":{"type":["boolean","null"]},
                "drift_unavailable":{"type":["string","null"]},"scan_scope":{"type":"string"},
                "complete":{"type":"boolean"},"truncated":{"type":"boolean"},
                "cursor":{"type":"null"}
            },
            "required":["schema_version","nonce","root","vcs","change_label","status_summary","limits","excluded_directory_names","initial_stamp","current_stamp","drifted","drift_unavailable","scan_scope","complete","truncated","cursor"],
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
                "complete":{"type":"boolean"},"truncated":{"type":"boolean"},"cursor":{"type":"null"},
                "drifted":{"type":["boolean","null"]},"drift_unavailable":{"type":["string","null"]}
            },
            "required":["path","bytes","sha256","total_lines","lines","complete","truncated","cursor","drifted","drift_unavailable"],
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
        "repository_diff" => text_page("diff"),
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

    #[test]
    fn status_session_name_is_unreachable_by_user_sessions() {
        // The status preflight's lease and sterile directory key on STATUS_SESSION. `start_review`
        // rejects any control character in a user-chosen session name, so a control character here
        // guarantees no user session can ever equal it -- and thus none can contend for the status
        // lease or alias onto its sterile directory. If this ever became control-free, a review
        // named "cross-review-status" would collide.
        assert!(
            STATUS_SESSION.chars().any(char::is_control),
            "STATUS_SESSION must live in the control-character namespace users are forbidden from"
        );
    }

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
            Instant::now(),
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
        let scope = handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"repository_scope"}}), &mut core, Instant::now()).unwrap();
        assert_eq!(scope["result"]["isError"], false);
        let unknown = handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cross_model_review","arguments":{}}}), &mut core, Instant::now()).unwrap();
        assert_eq!(unknown["result"]["isError"], true);
    }

    fn stamp(method: StampMethod, byte: char) -> Drift {
        Drift::Stamp {
            method,
            sha256: std::iter::repeat_n(byte, 64).collect(),
        }
    }

    // Every way of not knowing has to produce `None` with a reason. A default `false` here is the
    // one outcome that would matter: it tells a reviewer the tree held still when nobody looked.
    #[test]
    fn drift_is_only_reported_when_two_comparable_stamps_exist() {
        let git_a = stamp(StampMethod::Git, 'a');
        let git_b = stamp(StampMethod::Git, 'b');
        let walk_a = stamp(StampMethod::Filesystem, 'a');
        let missing = Drift::unavailable("the tree was too large to scan");

        assert_eq!(compare_drift(&git_a, &git_a), (Some(false), None));
        assert_eq!(compare_drift(&git_a, &git_b).0, Some(true));
        // Same digest, different scan: the file sets are not the same file set.
        let (verdict, reason) = compare_drift(&git_a, &walk_a);
        assert_eq!(verdict, None);
        assert!(reason.unwrap().contains("different scan methods"));
        for pair in [(&missing, &git_a), (&git_a, &missing)] {
            let (verdict, reason) = compare_drift(pair.0, pair.1);
            assert_eq!(verdict, None);
            assert!(reason.is_some_and(|r| r.contains("too large")));
        }
    }

    // The reasons are formatted errors, and one of them carries a child process's diagnostics.
    // Unbounded, a chatty Git failure would fail validation and take down the `Bundle::create` that
    // was degrading gracefully -- turning the fix for issue #86 back into the bug.
    #[test]
    fn an_unavailability_reason_cannot_grow_past_what_a_bundle_accepts() {
        let shouty = Drift::unavailable("é".repeat(10_000));
        let Drift::Unavailable { reason } = &shouty else {
            panic!("expected an unavailable drift");
        };
        assert!(reason.len() <= 1024, "{} bytes", reason.len());
        shouty.validate().unwrap();
        let short = Drift::unavailable("no git");
        assert_eq!(
            match &short {
                Drift::Unavailable { reason } => reason.as_str(),
                _ => unreachable!(),
            },
            "no git",
            "a reason under the cap is stored verbatim"
        );
    }

    #[test]
    fn a_bundle_carries_an_unavailable_stamp_and_still_validates() {
        let dir = temp_dir("evidence-unavailable-bundle");
        let mut bundle = bundle(dir.as_path());
        bundle.initial_stamp = Drift::unavailable("no reviewable file list");
        bundle.validate().unwrap();
        // Round-trips through the capability file the reviewer's service reads.
        let encoded = serde_json::to_value(&bundle).unwrap();
        let decoded: Bundle = serde_json::from_value(encoded).unwrap();
        assert!(matches!(decoded.initial_stamp, Drift::Unavailable { .. }));

        bundle.initial_stamp = Drift::Stamp {
            method: StampMethod::Git,
            sha256: "not-a-digest".into(),
        };
        assert!(bundle.validate().is_err());
        bundle.initial_stamp = Drift::unavailable("");
        assert!(bundle.validate().is_err());
    }

    // Codex validates `structuredContent` against the declared output schema, so a null drift
    // verdict has to be declared as well as produced.
    #[test]
    fn the_declared_schemas_admit_an_unknown_drift_verdict() {
        for (tool, fields) in [
            (
                "repository_scope",
                vec![
                    "drifted",
                    "initial_stamp",
                    "current_stamp",
                    "drift_unavailable",
                ],
            ),
            ("repository_read", vec!["drifted", "drift_unavailable"]),
        ] {
            let schema = output_schema(tool);
            let required = schema["required"].as_array().unwrap();
            for field in fields {
                let declared = &schema["properties"][field]["type"];
                assert!(
                    declared
                        .as_array()
                        .is_some_and(|t| t.contains(&json!("null"))),
                    "{tool}.{field} must admit null, got {declared}"
                );
                assert!(
                    required.iter().any(|r| r == field),
                    "{tool}.{field} missing"
                );
            }
            assert_eq!(schema["additionalProperties"], false);
        }
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
            Instant::now(),
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
