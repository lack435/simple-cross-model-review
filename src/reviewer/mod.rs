//! Driving a reviewer CLI as a child process.
//!
//! Shared here: locating the executable, running it with a timeout without
//! deadlocking on pipes, and the trait the two adapters implement.

pub mod claude;
pub mod codex;

#[cfg(test)]
mod argv_tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to keep draining the output pipes after the child is gone. A descendant that
/// inherited a pipe handle can hold it open past its parent's exit, so output collection
/// is bounded rather than an unbounded join.
pub const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Ceiling on what we keep from one output pipe.
///
/// Collection was bounded in time but not in size: a reviewer emitting output
/// continuously was held in memory until it stopped or the deadline passed. Deliberately
/// far above anything observed -- real transcripts are kilobytes, and Codex's event stream
/// is the largest of them by a wide margin -- so reaching this means something has gone
/// wrong, and the point is that it fails legibly rather than eating the machine.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Total raw stdout bytes the *armed* Claude reader will read before declaring the stream
/// truncated. Larger than `MAX_OUTPUT_BYTES` because `--output-format stream-json` carries the
/// review text more than once (assistant events plus the terminal `result`), so a review whose
/// content fits the buffered path produces a stream several times its size. Practical sizing,
/// not a proven ceiling -- its job is to stop a runaway stream, not to decide ordinary
/// truncation. See `docs/usage-remaining-gate.md`.
pub const MAX_ARMED_STREAM_BYTES: usize = 4 * MAX_OUTPUT_BYTES;

/// Companion line/event cap for the armed reader: a stream may stay under the byte cap while
/// emitting a pathological number of tiny events. Bounds work independently of byte size.
pub const MAX_ARMED_STREAM_LINES: usize = 500_000;

use crate::config::{Config, ReviewerKind, ReviewerSpec, UsageMinimum};
use crate::errors::{self, Failure};

/// A reviewer's usage-remaining headroom, observed from the CLI's own machine output (Codex's
/// rollout `token_count.rate_limits`, Claude's `rate_limit_event`) -- never the model's prose.
///
/// Three-state with an explicit `Unknown`: an absent, unparseable, unrecognised, stale, or
/// identity-mismatched signal is `Unknown`, and `Unknown` never gates (fail-open). See
/// `docs/usage-remaining-gate.md`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Headroom {
    /// No usable signal -- fail-open, always clears any minimum.
    Unknown,
    /// Codex: remaining percentage of the *limiting* (lowest-remaining) window, with that
    /// window's own reset time.
    Fraction {
        remaining_pct: f64,
        resets_at: Option<u64>,
    },
    /// Claude: a categorical level, with the window's reset time.
    Level {
        level: HeadroomLevel,
        resets_at: Option<u64>,
    },
}

/// Claude's categorical usage status, normalized. Rank is explicit (`Exhausted < Warning <
/// Ample`) rather than derived from declaration order, so a comparison cannot silently invert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadroomLevel {
    Exhausted,
    Warning,
    Ample,
}

impl HeadroomLevel {
    /// Higher rank = more headroom. Load-bearing for `clears`; do not replace with a derived
    /// `Ord`, whose declaration-order default would rank `Ample` lowest and invert the decision.
    pub fn rank(self) -> u8 {
        match self {
            HeadroomLevel::Exhausted => 0,
            HeadroomLevel::Warning => 1,
            HeadroomLevel::Ample => 2,
        }
    }

    /// Map Claude's `status` string to a level; an unrecognised value yields `None` (the caller
    /// treats it as `Headroom::Unknown`, so an unlisted future value cannot mis-gate).
    pub fn from_status(status: &str) -> Option<Self> {
        match status {
            "allowed" => Some(HeadroomLevel::Ample),
            "allowed_warning" => Some(HeadroomLevel::Warning),
            "rejected" => Some(HeadroomLevel::Exhausted),
            _ => None,
        }
    }
}

impl Headroom {
    /// The reset time this observation is tied to, if any (the limiting window for a Codex
    /// `Fraction`). `None` for `Unknown` and for a signal that carried no reset.
    pub fn resets_at(&self) -> Option<u64> {
        match self {
            Headroom::Unknown => None,
            Headroom::Fraction { resets_at, .. } | Headroom::Level { resets_at, .. } => *resets_at,
        }
    }

    /// Does this observation clear the configured minimum? `Unknown` always clears (fail-open).
    /// Each shape is only ever compared against its own-shaped minimum -- the config grammar
    /// guarantees a Codex entry carries `Remaining(_)` and a Claude entry `Status(_)`; a
    /// mismatched pairing (which the grammar forbids) also clears, never gating on a signal it
    /// cannot interpret. See `docs/usage-remaining-gate.md`.
    pub fn clears(&self, min: &UsageMinimum) -> bool {
        match (self, min) {
            (Headroom::Unknown, _) | (_, UsageMinimum::None) => true,
            (Headroom::Fraction { remaining_pct, .. }, UsageMinimum::Remaining(pct)) => {
                *remaining_pct >= f64::from(*pct)
            }
            (Headroom::Level { level, .. }, UsageMinimum::Status(min_level)) => {
                level.rank() >= min_level.rank()
            }
            // Shape/minimum mismatch (forbidden by the grammar): fail-open.
            _ => true,
        }
    }
}

/// What the reviewer produced.
#[derive(Debug)]
pub struct Parsed {
    pub text: String,
    /// The CLI's own session id, needed to resume this review later.
    pub session_id: Option<String>,
    /// Tool calls the reviewer was not permitted to make.
    pub denials: Vec<String>,
    /// Total number of tool calls the CLI reported as denied. `denials` is a bounded list
    /// of examples for the caller, so this count must not be inferred from its length.
    pub denial_count: usize,
    /// Whether `denial_count` is a lower bound rather than the exact total. Set when the
    /// count was recovered from output that hit the collection cap, so later refusals were
    /// discarded: presenting the retained count as exact would understate it silently. The
    /// flag travels with `denial_count` so the render can say "at least N".
    pub denial_count_is_floor: bool,
    /// Problems that did not invalidate the review but that the caller must know about.
    pub warnings: Vec<String>,
    /// What the CLI reported about the tokens this turn consumed. Both CLIs report it and
    /// we used to discard it, which left the cost of a review invisible to the tool that
    /// caused it. Defaulted rather than optional: a CLI that stops reporting usage should
    /// degrade to unreported usage in the log -- every `Usage` field stays `None` and
    /// serialises as absent, never as an asserted zero -- not to a failed review.
    pub usage: crate::metrics::Usage,
    /// Whether `usage` counts the whole reviewer conversation rather than this turn.
    ///
    /// The two CLIs differ and the difference is invisible in the numbers, which is how
    /// a thread total came to be recorded as a turn's cost: Claude reports per turn,
    /// Codex reports the thread's running total on every `turn.completed`. Stated by the
    /// adapter rather than inferred by the caller, because inferring it is exactly what
    /// went wrong -- a cumulative figure looks like a plausible per-turn one right up
    /// until you compare two turns.
    pub usage_is_cumulative: bool,
}

/// Normalize a CLI-reported session id at the adapter boundary: trim surrounding whitespace and
/// treat an empty (or all-whitespace) value as *absent*, never as a usable id.
///
/// A session id is the key a later turn resumes by. An empty or whitespace id is not a real handle,
/// but the record path would otherwise persist it and advertise the turn resumable, so the next
/// resume would try to continue an id no reviewer holds. Collapsing it to `None` here makes such a
/// turn fall through to the not-durable path (the caller rebaselines fresh) instead. Both adapters
/// funnel their reported id through this, so neither can leak a blank id into a `Parsed`.
pub fn normalize_session_id(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub struct Invocation {
    pub command: Command,
    /// A file the CLI writes its final message to, when it supports that.
    pub last_message_file: Option<PathBuf>,
}

/// A directory to run non-review CLI invocations from, so they cannot pick up the
/// reviewed project's configuration.
///
/// The state directory is normally ours and well outside the project, but not always: it
/// is user-settable via `--state-dir`, and its own fallback puts it under the project when
/// `LOCALAPPDATA` is unset. A state directory inside the project would make this "neutral"
/// directory anything but, so that case is rejected in favour of the temp directory.
pub fn neutral_dir(cfg: &Config) -> PathBuf {
    if cfg.state_dir.is_dir() && !is_within(&cfg.state_dir, &cfg.cwd) {
        cfg.state_dir.clone()
    } else {
        std::env::temp_dir()
    }
}

/// Is `path` inside `root`? Compared case-insensitively, as Windows paths are.
///
/// Shared with the diff capture, which uses it as a security check rather than a
/// convenience, so there is deliberately one implementation and not two.
/// The user's home directory, for locating a CLI's local account/session files. Honours the
/// platform's usual variables (`USERPROFILE` on Windows, then `HOME`). `None` if neither is set.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

pub fn is_within(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy().to_lowercase().replace('\\', "/");
    let root = root.to_string_lossy().to_lowercase().replace('\\', "/");
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

pub trait Reviewer: Send + Sync {
    /// Cheap check that the CLI exists and is signed in. Runs before we spend a
    /// model call, so an unconfigured machine fails fast and legibly.
    ///
    /// `cancel` interrupts the check: a fallback entry's preflight runs inside the review's walk,
    /// so a cancellation (or shutdown) must be able to stop a 30-second auth probe rather than
    /// wait it out. See `docs/reviewer-fallback-chain.md`.
    fn auth_check(&self, bin: &Path, cfg: &Config, cancel: &AtomicBool) -> Result<String, Failure>;

    fn invocation(
        &self,
        cfg: &Config,
        spec: &ReviewerSpec,
        bin: &Path,
        resume: Option<&str>,
        tmp_id: &str,
    ) -> std::io::Result<Invocation>;

    /// `last_message_file` is whatever `invocation` asked the CLI to write its final
    /// message to. It is passed separately because the `Command` is consumed by `run`.
    ///
    /// `spec` is the *active* chain entry that ran, so model/effort in any classified failure
    /// name the reviewer that actually produced the output, not the primary.
    fn parse(
        &self,
        cfg: &Config,
        spec: &ReviewerSpec,
        out: &RunOutcome,
        last_message_file: Option<&Path>,
    ) -> Result<Parsed, Failure>;

    /// Observe this reviewer's usage-remaining headroom from its own machine output, read on
    /// **both** the success and failure paths (a rate-limited refusal is exactly the account the
    /// gate should skip next time). Reads only CLI-owned machine output — Codex's rollout
    /// `token_count.rate_limits`, Claude's `rate_limit_event` — never the model's prose. Called
    /// only when the chain is armed (`Config::chain_gates_on_usage`). Default `Unknown`
    /// (fail-open). See `docs/usage-remaining-gate.md`.
    fn observe_headroom(&self, _cfg: &Config, _spec: &ReviewerSpec, _out: &RunOutcome) -> Headroom {
        Headroom::Unknown
    }

    /// A cheap, *current*, local account identifier for this reviewer — the one the store keys a
    /// usage observation under, so a snapshot cannot cross an account switch. Read from the CLI's
    /// own local account file (Codex `$CODEX_HOME/auth.json`, Claude `~/.claude.json`), never via
    /// a CLI call and never a secret; `None` when it cannot be read (the gate then fails open).
    /// See `docs/usage-remaining-gate.md`.
    fn account_fingerprint(&self, _cfg: &Config, _spec: &ReviewerSpec) -> Option<String> {
        None
    }
}

pub fn for_kind(kind: ReviewerKind) -> Box<dyn Reviewer> {
    match kind {
        ReviewerKind::Claude => Box::new(claude::ClaudeReviewer),
        ReviewerKind::Codex => Box::new(codex::CodexReviewer),
    }
}

// ---------------------------------------------------------------------------
// Executable resolution
// ---------------------------------------------------------------------------

/// Locate the reviewer CLI. An explicit `--bin` always wins; otherwise we search
/// PATH with the Windows executable extensions and then a few well-known install
/// locations, because npm-global and native installs land in different places.
pub fn resolve_bin(spec: &ReviewerSpec) -> Result<PathBuf, Failure> {
    let mut tried: Vec<String> = Vec::new();

    if let Some(explicit) = &spec.bin {
        if explicit.is_file() {
            return Ok(explicit.clone());
        }
        tried.push(format!("{} (from --bin)", explicit.display()));
        return Err(errors::cli_not_found(spec.reviewer.as_str(), &tried));
    }

    let exts = path_exts();
    let stems = spec.reviewer.bin_stems();

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for stem in stems {
                for ext in &exts {
                    let candidate = dir.join(format!("{stem}{ext}"));
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                }
            }
        }
        tried.push(format!(
            "PATH entries, for {} with extensions [{}]",
            stems.join(", "),
            exts.iter()
                .map(|e| if e.is_empty() { "(none)" } else { e.as_str() })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        tried.push("PATH is not set".to_string());
    }

    for candidate in fallback_locations(spec.reviewer) {
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }

    Err(errors::cli_not_found(spec.reviewer.as_str(), &tried))
}

/// Locate `stem` on PATH, and nowhere else.
///
/// `Command::new("git")` will not do. Windows program resolution searches the *calling
/// executable's own directory* before PATH, and this binary is designed to be vendored
/// into the repository it reviews -- the README's own instruction is to copy
/// `cross-review.exe` into a project's `tools\`. A `tools\git.exe` committed to a hostile
/// repository would then run as the user, with no sandbox, no job-object policy and none
/// of the configuration isolation the rest of this tool is built on, which is precisely
/// backwards for a program whose job is to look at code you are unsure about.
///
/// Verified on Windows 11 with a stand-in `git.exe`: placed next to the calling
/// executable it was executed in preference to the real git on PATH; placed in the child
/// process's working directory it was not. So the application directory is the hazard,
/// and resolving against PATH ourselves is what removes it.
pub fn on_path(stem: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts = path_exts();
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &exts {
            let candidate = dir.join(format!("{stem}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn path_exts() -> Vec<String> {
    let mut exts: Vec<String> = std::env::var("PATHEXT")
        .map(|v| {
            v.split(';')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if exts.is_empty() {
        exts = vec![".exe".into(), ".cmd".into(), ".bat".into()];
    }
    // Prefer a real executable over a shim: launching a .cmd goes through cmd.exe,
    // which adds a layer of argument escaping we would rather not depend on.
    exts.sort_by_key(|e| match e.as_str() {
        ".exe" => 0,
        ".com" => 1,
        ".cmd" => 2,
        ".bat" => 3,
        _ => 4,
    });
    exts.push(String::new());
    exts
}

fn fallback_locations(kind: ReviewerKind) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    match kind {
        ReviewerKind::Claude => {
            if let Some(h) = &home {
                out.push(h.join(".local").join("bin").join("claude.exe"));
                out.push(h.join(".claude").join("local").join("claude.exe"));
            }
            if let Some(a) = &appdata {
                out.push(a.join("npm").join("claude.cmd"));
            }
        }
        ReviewerKind::Codex => {
            if let Some(h) = &home {
                out.push(h.join(".local").join("bin").join("codex.exe"));
                out.push(h.join(".codex").join(".sandbox-bin").join("codex.exe"));
            }
            if let Some(a) = &appdata {
                out.push(a.join("npm").join("codex.cmd"));
            }
            if let Some(l) = &local {
                out.push(l.join("Programs").join("codex").join("codex.exe"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Running a child process
// ---------------------------------------------------------------------------

pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    /// stdout hit the size cap. Kept apart from stderr because both adapters read the
    /// review itself from stdout, so only this one makes a review unrecoverable.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// stdout contained bytes that were not valid UTF-8 and were replaced during decoding,
    /// so `stdout` is not a faithful copy of what the child produced. The Perforce resume
    /// delta needs this: a fingerprint over lossily-decoded text cannot claim byte-identity
    /// with the underlying file, so any evidence captured from such output is non-elidable.
    pub stdout_lossy: bool,
    /// The stdout stream ended before it was fully drained -- a pipe read error, or the collect
    /// deadline expiring -- so `stdout` may be a partial prefix even though the process exited
    /// cleanly. The Perforce capture treats this like truncation: an incomplete list or diff must
    /// not be parsed as a whole and seed an elision baseline.
    pub stdout_incomplete: bool,
}

/// Why [`run_observed`] failed. The distinction is a durability boundary, not cosmetic: a `Spawn`
/// failure means no child process was ever created, so a *resumed* reviewer conversation could not
/// have advanced and the session's findings ledger is provably not stale. An `Observe` failure
/// happened after the child was already running, so the reviewer may have advanced its conversation
/// and the ledger must be treated as possibly stale.
#[derive(Debug)]
pub enum RunError {
    /// `Command::spawn` failed — the child never started.
    Spawn(std::io::Error),
    /// The child started but observing it to completion failed (e.g. `try_wait`).
    Observe(std::io::Error),
}

impl RunError {
    /// True only when no child process was ever created, so nothing the reviewer persists could
    /// have moved. Callers use this to decide whether a resumed session's findings write-ahead
    /// marker can be safely cleared after a failure.
    pub fn child_never_started(&self) -> bool {
        matches!(self, RunError::Spawn(_))
    }

    fn into_io(self) -> std::io::Error {
        match self {
            RunError::Spawn(e) | RunError::Observe(e) => e,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) | RunError::Observe(e) => write!(f, "{e}"),
        }
    }
}

/// Observable liveness from a reviewer child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Bytes retained from both output pipes so far. This stops growing at the collection
    /// cap even though the pipes continue to be drained.
    pub output_bytes: usize,
}

impl RunOutcome {
    /// Did either stream hit the cap? For "is the review recoverable", ask
    /// `stdout_truncated` instead -- a flooded stderr says nothing about the review.
    pub fn truncated(&self) -> bool {
        self.stdout_truncated || self.stderr_truncated
    }

    /// Whether stdout cannot be trusted as a complete, byte-faithful document: it hit the size
    /// cap, ended before it was fully drained, or did not decode cleanly. A consumer that parses
    /// stdout as authoritative evidence (the Perforce capture) must not trust it when this is set.
    pub fn stdout_untrustworthy(&self) -> bool {
        self.stdout_truncated || self.stdout_incomplete || self.stdout_lossy
    }

    /// stderr first: that is where CLIs put the reason they failed.
    pub fn diagnostics(&self) -> String {
        let mut out = String::new();
        // Stated first, because everything below it is now evidence of unknown
        // completeness and a reader who learns that afterwards has already drawn
        // conclusions from it.
        if self.truncated() {
            out.push_str(&format!(
                "[cross-review: the reviewer's output exceeded {} MiB and was truncated; what \
                 follows is incomplete]\n\n",
                MAX_OUTPUT_BYTES / (1024 * 1024)
            ));
        }
        if !self.stderr.trim().is_empty() {
            out.push_str(self.stderr.trim());
        }
        if !self.stdout.trim().is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(self.stdout.trim());
        }
        out
    }
}

/// Run a child, feeding `stdin_data` in and collecting both output streams.
///
/// stdin is written on its own thread and both output streams are drained on their
/// own threads. Doing any of that on the calling thread risks a deadlock: a large
/// prompt can fill the child's stdin buffer while the child is blocked writing
/// output that nobody is reading.
pub fn run(
    command: Command,
    stdin_data: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> std::io::Result<RunOutcome> {
    // The status/liveness probes that use this wrapper do not need the spawn/observe distinction, so
    // flatten it back to a plain `io::Error`.
    run_observed(command, stdin_data, timeout, cancel, |_| {}).map_err(RunError::into_io)
}

/// Run a reviewer child and periodically report that it was observed alive.
///
/// The callback runs only after `try_wait` says the child is still running. It therefore
/// gives the registry a real liveness signal rather than a timer attached to a worker
/// thread that might itself be stuck before process launch.
pub fn run_observed(
    mut command: Command,
    stdin_data: &str,
    timeout: Duration,
    cancel: &AtomicBool,
    mut on_activity: impl FnMut(Activity),
) -> Result<RunOutcome, RunError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child: Child = command.spawn().map_err(RunError::Spawn)?;

    // Assign before the reviewer does any real work, so anything it spawns is inside the
    // job and dies with it. Without this, a helper that outlives its parent keeps our
    // pipes open and leaks across reviews.
    let job = crate::winjob::JobObject::new();
    match &job {
        Some(job) => {
            if !job.assign(&child) {
                eprintln!(
                    "cross-review: warning: could not put the reviewer in a job object; \
                     processes it spawns may outlive it"
                );
            }
        }
        None => eprintln!(
            "cross-review: warning: could not create a job object; processes the reviewer \
             spawns may outlive it"
        ),
    }

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let payload = stdin_data.to_owned();
    // Deliberately never joined: if the child exits without reading the prompt, this
    // thread stays blocked in write_all until the pipe closes. It owns nothing we need.
    std::thread::spawn(move || {
        // A reviewer that exits early closes the pipe; that is not our error to report.
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.flush();
        drop(stdin);
    });

    // Readers append into a shared buffer rather than returning at EOF. If a straggler
    // holds a pipe open past the drain deadline we still get everything that arrived --
    // returning at EOF meant abandoning the channel discarded the whole transcript, and
    // for Claude, whose review is stdout-only, a completed review became EMPTY_REVIEW.
    let stdout_buf = drain(child.stdout.take().expect("stdout was piped"));
    let stderr_buf = drain(child.stderr.take().expect("stderr was piped"));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut next_activity = Instant::now();

    let status = loop {
        match child.try_wait().map_err(RunError::Observe)? {
            Some(status) => break Some(status),
            None => {
                let now = Instant::now();
                if now >= next_activity {
                    on_activity(Activity {
                        output_bytes: stdout_buf.len() + stderr_buf.len(),
                    });
                    next_activity = now + Duration::from_secs(5);
                }
                let stop = if cancel.load(Ordering::SeqCst) {
                    cancelled = true;
                    true
                } else if now >= deadline {
                    timed_out = true;
                    true
                } else {
                    false
                };

                if stop {
                    // Whole tree, not just the direct child: helpers holding our pipes
                    // would otherwise survive and stall output collection.
                    if let Some(job) = &job {
                        job.terminate();
                    }
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    // The direct child is gone, so nothing still in the job can contribute output we
    // would parse. Killing them now closes the pipes, which is what lets collection
    // finish immediately instead of waiting out the grace period. This also covers the
    // clean-exit-with-surviving-helper case, where there is no timeout to trigger it.
    if let Some(job) = &job {
        job.terminate();
    }

    let drain_by = Instant::now() + DRAIN_GRACE;
    let stdout = collect(&stdout_buf, drain_by);
    let stderr = collect(&stderr_buf, drain_by);

    Ok(RunOutcome {
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout_lossy: stdout.lossy,
        stdout_incomplete: stdout.incomplete,
        stdout: stdout.text,
        stderr: stderr.text,
        exit: status.and_then(|s| s.code()),
        success: status.map(|s| s.success()).unwrap_or(false) && !timed_out && !cancelled,
        timed_out,
        cancelled,
    })
}

/// Progress of one output pipe: the bytes so far, whether the reader reached EOF, and
/// whether it had to start discarding.
struct Drain {
    buffer: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
    truncated: Arc<AtomicBool>,
    /// The reader broke on a pipe read *error* rather than a clean EOF, so the output may be cut
    /// mid-stream. Distinct from `truncated` (the deliberate size cap): a consumer that needs a
    /// complete stream (the Perforce capture) must treat this as untrustworthy.
    errored: Arc<AtomicBool>,
}

impl Drain {
    fn len(&self) -> usize {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Start draining a pipe into a shared buffer.
///
/// Reading incrementally rather than returning at EOF is what makes a partial transcript
/// recoverable: whatever arrived before the deadline is already in the buffer.
///
/// The buffer is capped, but the *reading* is not. Once the cap is reached the reader
/// keeps consuming the pipe and throws the bytes away, because a reader that stopped
/// would fill the pipe and block the child forever -- trading unbounded memory for a
/// hung review, which is a worse bargain. Reaching the cap is recorded, so a transcript
/// that lost its middle is reported as truncated rather than as merely short.
fn drain(mut pipe: impl std::io::Read + Send + 'static) -> Drain {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let truncated = Arc::new(AtomicBool::new(false));
    let errored = Arc::new(AtomicBool::new(false));

    let writer_buf = Arc::clone(&buffer);
    let writer_done = Arc::clone(&done);
    let writer_truncated = Arc::clone(&truncated);
    let writer_errored = Arc::clone(&errored);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Err(_) => {
                    // A read error means the stream ended abnormally: record it so a consumer
                    // that needs a complete stream does not parse a partial prefix as whole.
                    writer_errored.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(n) => {
                    let mut buffer = writer_buf.lock().unwrap_or_else(|e| e.into_inner());
                    let room = MAX_OUTPUT_BYTES.saturating_sub(buffer.len());
                    if room == 0 {
                        writer_truncated.store(true, Ordering::SeqCst);
                        continue;
                    }
                    if n > room {
                        writer_truncated.store(true, Ordering::SeqCst);
                    }
                    buffer.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        writer_done.store(true, Ordering::SeqCst);
    });

    Drain {
        buffer,
        done,
        truncated,
        errored,
    }
}

/// One pipe's output, whether the cap threw any of it away, whether decoding was lossy, and
/// whether the stream ended before it was fully drained (a read error or the collect deadline).
struct Collected {
    text: String,
    truncated: bool,
    lossy: bool,
    incomplete: bool,
}

/// Take what a pipe has produced, waiting until EOF or `deadline`, whichever comes first.
///
/// CLI output is normally UTF-8, but a stray non-UTF-8 byte in a diagnostic must not lose
/// us the whole message, so decoding is lossy.
fn collect(drain: &Drain, deadline: Instant) -> Collected {
    while !drain.done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    // The stream is incomplete if the reader hit a pipe error, or if we gave up waiting before it
    // reached EOF -- either way the buffer may be a partial prefix, which a byte-fidelity consumer
    // (the Perforce capture) must not parse as a whole document.
    let incomplete = drain.errored.load(Ordering::SeqCst) || !drain.done.load(Ordering::SeqCst);
    let buffer = drain.buffer.lock().unwrap_or_else(|e| e.into_inner());
    // Whether decoding replaced any byte: valid UTF-8 decodes losslessly, anything else does
    // not. Recorded so a caller that needs byte-fidelity (the Perforce fingerprint) can tell.
    let lossy = std::str::from_utf8(&buffer).is_err();
    Collected {
        text: String::from_utf8_lossy(&buffer).into_owned(),
        truncated: drain.truncated.load(Ordering::SeqCst),
        lossy,
        incomplete,
    }
}

/// The failure to report when output was capped, if it was.
///
/// Checked ahead of `EMPTY_REVIEW` by both adapters. Truncation makes every downstream
/// diagnosis unreliable -- a JSON document cut in half parses as nothing at all -- so the
/// cap has to be named rather than left to surface as "the CLI wrote nothing", which is
/// the opposite of what happened and points the caller at a useless retry.
pub fn truncation_failure(spec: &ReviewerSpec, out: &RunOutcome) -> Option<Failure> {
    // Gated on stdout alone. A run whose stderr flooded but whose stdout is simply empty
    // failed for some other reason, and reporting it as truncation would tell the caller
    // not to retry when retrying is exactly right. `stdout_incomplete` (a partial prefix from a
    // pipe error or a drain-deadline expiry) is treated the same way: a review parsed from a
    // truncated transcript is as unreliable as one from a capped one, and the Codex JSONL
    // fallback would otherwise return an earlier agent message as the review.
    if out.stdout_truncated {
        Some(errors::output_truncated(
            spec.reviewer.as_str(),
            MAX_OUTPUT_BYTES / (1024 * 1024),
            out.diagnostics(),
        ))
    } else if out.stdout_incomplete {
        // A partial prefix that did not hit the size cap: a distinct diagnostic, since the
        // truncation message would wrongly claim the CLI exceeded the cap.
        Some(errors::output_incomplete(
            spec.reviewer.as_str(),
            out.diagnostics(),
        ))
    } else {
        None
    }
}

/// Turn a non-success run into the right `Failure`. `spec` is the *active* entry that ran,
/// not the primary, so a fallback's failure names the reviewer that actually produced it.
pub fn failure_for(cfg: &Config, spec: &ReviewerSpec, out: &RunOutcome) -> Failure {
    let reviewer = spec.reviewer.as_str();
    if out.cancelled {
        return errors::cancelled();
    }
    if out.timed_out {
        if spec.reviewer == ReviewerKind::Codex {
            let policy_denials = codex::policy_denial_count(&out.stderr);
            if policy_denials > 0 {
                return errors::timed_out_after_policy_denials(
                    reviewer,
                    cfg.timeout.as_secs(),
                    policy_denials,
                    // A capped stderr dropped later refusals, so the count is a floor.
                    out.stderr_truncated,
                    out.diagnostics(),
                );
            }
        }
        return errors::timed_out(reviewer, cfg.timeout.as_secs(), out.diagnostics());
    }
    errors::classify(
        reviewer,
        &spec.model,
        &spec.effort,
        out.exit,
        &out.diagnostics(),
        &out.diagnostics(),
    )
}

/// Path for a CLI's final-message file. Cleaned up by the caller.
pub fn tmp_file(cfg: &Config, tmp_id: &str, name: &str) -> std::io::Result<PathBuf> {
    let dir = cfg.tmp_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{tmp_id}-{name}")))
}

#[cfg(test)]
mod session_id_tests {
    use super::normalize_session_id;

    #[test]
    fn empty_or_whitespace_session_ids_normalize_to_absent() {
        // A real id is trimmed and kept.
        assert_eq!(
            normalize_session_id(Some("  abc-123  ".to_string())),
            Some("abc-123".to_string())
        );
        // Empty and all-whitespace ids are not usable handles: collapse to absent so the record
        // path never persists a blank id and advertises a doomed resume.
        assert_eq!(normalize_session_id(Some(String::new())), None);
        assert_eq!(normalize_session_id(Some("   ".to_string())), None);
        assert_eq!(normalize_session_id(Some("\t\n".to_string())), None);
        // Absent stays absent.
        assert_eq!(normalize_session_id(None), None);
    }
}

#[cfg(test)]
mod headroom_tests {
    use super::*;

    #[test]
    fn unknown_always_clears() {
        assert!(Headroom::Unknown.clears(&UsageMinimum::Remaining(90)));
        assert!(Headroom::Unknown.clears(&UsageMinimum::Status(HeadroomLevel::Ample)));
    }

    #[test]
    fn fraction_clears_at_or_above_the_minimum() {
        let h = Headroom::Fraction {
            remaining_pct: 17.0,
            resets_at: None,
        };
        assert!(h.clears(&UsageMinimum::Remaining(10)));
        assert!(h.clears(&UsageMinimum::Remaining(17)));
        assert!(!h.clears(&UsageMinimum::Remaining(18)));
        assert!(h.clears(&UsageMinimum::None));
    }

    #[test]
    fn level_ranks_ample_highest_not_by_declaration_order() {
        // The load-bearing check: an accidental derived Ord would rank Ample lowest.
        assert!(HeadroomLevel::Ample.rank() > HeadroomLevel::Warning.rank());
        assert!(HeadroomLevel::Warning.rank() > HeadroomLevel::Exhausted.rank());

        let ample = Headroom::Level {
            level: HeadroomLevel::Ample,
            resets_at: None,
        };
        let warning = Headroom::Level {
            level: HeadroomLevel::Warning,
            resets_at: None,
        };
        let rejected = Headroom::Level {
            level: HeadroomLevel::Exhausted,
            resets_at: None,
        };
        // min=ample: only ample clears.
        assert!(ample.clears(&UsageMinimum::Status(HeadroomLevel::Ample)));
        assert!(!warning.clears(&UsageMinimum::Status(HeadroomLevel::Ample)));
        assert!(!rejected.clears(&UsageMinimum::Status(HeadroomLevel::Ample)));
        // min=warning: ample and warning clear, rejected does not.
        assert!(ample.clears(&UsageMinimum::Status(HeadroomLevel::Warning)));
        assert!(warning.clears(&UsageMinimum::Status(HeadroomLevel::Warning)));
        assert!(!rejected.clears(&UsageMinimum::Status(HeadroomLevel::Warning)));
    }

    #[test]
    fn status_strings_map_and_unknown_is_none() {
        assert_eq!(HeadroomLevel::from_status("allowed"), Some(HeadroomLevel::Ample));
        assert_eq!(
            HeadroomLevel::from_status("allowed_warning"),
            Some(HeadroomLevel::Warning)
        );
        assert_eq!(
            HeadroomLevel::from_status("rejected"),
            Some(HeadroomLevel::Exhausted)
        );
        assert_eq!(HeadroomLevel::from_status("some_new_state"), None);
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    fn drained(bytes: Vec<u8>) -> Collected {
        let drain = drain(std::io::Cursor::new(bytes));
        collect(&drain, Instant::now() + Duration::from_secs(5))
    }

    #[test]
    fn output_under_the_cap_is_kept_whole_and_not_flagged() {
        let collected = drained(b"a normal transcript".to_vec());
        assert_eq!(collected.text, "a normal transcript");
        assert!(!collected.truncated);
    }

    #[test]
    fn a_failed_spawn_reports_child_never_started() {
        // A command that cannot launch never creates a child, so `run_observed` returns
        // `RunError::Spawn` and `child_never_started()` is true. That is the signal the worker uses
        // to safely clear a resumed session's findings marker after a pre-launch failure: with no
        // child, the reviewer conversation could not have advanced, so the ledger is not stale.
        let cmd = std::process::Command::new("cross-review-no-such-binary-9c3f1a2b");
        let result = run_observed(
            cmd,
            "",
            Duration::from_secs(5),
            &std::sync::atomic::AtomicBool::new(false),
            |_| {},
        );
        match result {
            Ok(_) => panic!("spawn of a missing binary must fail"),
            Err(e) => {
                assert!(e.child_never_started());
                assert!(matches!(e, RunError::Spawn(_)));
            }
        }
    }

    #[test]
    fn output_over_the_cap_is_bounded_and_flagged() {
        // Collection was bounded in time but not in size, so a reviewer emitting output
        // continuously was held in memory until it stopped.
        let collected = drained(vec![b'x'; MAX_OUTPUT_BYTES + 4096]);
        assert_eq!(collected.text.len(), MAX_OUTPUT_BYTES);
        assert!(collected.truncated);
    }

    #[test]
    fn valid_utf8_is_not_flagged_lossy_but_invalid_bytes_are() {
        // The Perforce fingerprint keys off this: text that decoded cleanly can be trusted as
        // byte-faithful, text that needed replacement cannot.
        assert!(!drained("clean utf-8 café".as_bytes().to_vec()).lossy);
        // A lone 0xFF is not valid UTF-8, so decoding replaces it.
        let lossy = drained(vec![b'o', b'k', 0xFF, b'!']);
        assert!(lossy.lossy);
        assert!(lossy.text.contains('\u{FFFD}'));
    }

    /// A reader that records how many bytes were actually taken from it.
    struct Counting {
        remaining: usize,
        taken: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl std::io::Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.remaining);
            self.remaining -= n;
            self.taken.fetch_add(n, Ordering::SeqCst);
            buf[..n].fill(b'x');
            Ok(n)
        }
    }

    #[test]
    fn the_whole_pipe_is_still_consumed_after_the_cap() {
        // The reader must keep reading and discard, not stop: a reader that stopped would
        // fill the pipe and block the child for ever, trading unbounded memory for a hung
        // review.
        //
        // Asserted by counting bytes taken from the source, not by checking `done`. `done`
        // is set whenever the read loop exits, so it is also set by a `break` at the cap --
        // which is precisely the regression this test exists to catch, and which it would
        // therefore have passed.
        let source = MAX_OUTPUT_BYTES * 2;
        let taken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drain = drain(Counting {
            remaining: source,
            taken: Arc::clone(&taken),
        });
        let collected = collect(&drain, Instant::now() + Duration::from_secs(10));

        assert!(collected.truncated);
        assert_eq!(
            collected.text.len(),
            MAX_OUTPUT_BYTES,
            "buffer exceeded the cap"
        );
        assert_eq!(
            taken.load(Ordering::SeqCst),
            source,
            "the reader stopped at the cap instead of draining the pipe"
        );
    }

    #[test]
    fn a_cap_hit_on_either_stream_is_stated_before_the_evidence() {
        // Everything after it is evidence of unknown completeness, and a reader who
        // learns that afterwards has already drawn conclusions from it.
        let out = RunOutcome {
            stdout: "partial".into(),
            stderr: "boom".into(),
            exit: Some(1),
            success: false,
            timed_out: false,
            cancelled: false,
            stdout_truncated: true,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
        };
        let diagnostics = out.diagnostics();
        assert!(diagnostics.starts_with("[cross-review:"), "{diagnostics}");
        assert!(diagnostics.contains("truncated"), "{diagnostics}");

        let intact = RunOutcome {
            stdout_truncated: false,
            stderr_truncated: false,
            ..out
        };
        assert!(!intact.diagnostics().contains("truncated"));
    }

    #[test]
    fn truncation_is_reported_under_its_own_code_not_as_an_empty_review() {
        // An empty review means the CLI wrote nothing and retrying is reasonable. A
        // truncated one means it wrote far too much, and retrying does the same again.
        let cfg =
            Config::from_args(&["--reviewer".to_string(), "claude".to_string()]).expect("config");
        let out = RunOutcome {
            stdout: "{\"result\": \"half a doc".into(),
            stderr: String::new(),
            exit: Some(0),
            success: true,
            timed_out: false,
            cancelled: false,
            stdout_truncated: true,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
        };
        let failure = truncation_failure(cfg.primary(), &out).expect("a truncation failure");
        assert_eq!(failure.code, "OUTPUT_TRUNCATED");

        let intact = RunOutcome {
            stdout_truncated: false,
            stderr_truncated: false,
            ..out
        };
        assert!(truncation_failure(cfg.primary(), &intact).is_none());
    }

    #[test]
    fn a_codex_timeout_with_policy_denials_explains_the_likely_stall() {
        let cfg =
            Config::from_args(&["--reviewer".to_string(), "codex".to_string()]).expect("config");
        let out = RunOutcome {
            stdout: String::new(),
            stderr: concat!(
                "ERROR codex_core::tools::router: error=`git grep foo` rejected: blocked by policy\n",
                "ERROR codex_core::tools::router: error=`git ls-files` rejected: blocked by policy\n",
            )
            .to_string(),
            exit: None,
            success: false,
            timed_out: true,
            cancelled: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
        };

        let failure = failure_for(&cfg, cfg.primary(), &out);
        assert_eq!(failure.code, "TIMEOUT");
        assert!(failure.summary.contains("refused 2 shell command(s)"));
        assert!(failure
            .remediation
            .contains("non-interactive command-policy refusals"));
        assert!(failure.detail.unwrap_or_default().contains("git grep foo"));

        // With a capped stderr the two counted refusals are only the ones that survived, so
        // the summary must report the count as a floor rather than as the exact total.
        let capped = RunOutcome {
            stderr_truncated: true,
            ..out
        };
        let failure = failure_for(&cfg, cfg.primary(), &capped);
        assert_eq!(failure.code, "TIMEOUT");
        assert!(
            failure
                .summary
                .contains("refused at least 2 shell command(s)"),
            "{}",
            failure.summary
        );
    }
}
