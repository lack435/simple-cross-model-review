//! Driving a reviewer CLI as a child process.
//!
//! Shared here: locating the executable, running it with a timeout without
//! deadlocking on pipes, and the trait the two adapters implement.

pub mod claude;
pub mod codex;

#[cfg(test)]
mod argv_tests;

use std::io::{Read, Write};
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

/// The stdout bounds [`run_observed`] enforces for one reviewer. The default keeps the historic
/// behaviour — retain up to `max_bytes`, but keep draining past it so the child never blocks on a
/// full pipe (`terminate_at_cap: false`). The **armed** Claude path sets `terminate_at_cap: true`
/// so a runaway stream is *killed* at the byte or line bound rather than held until the timeout
/// (round-1-impl finding f2). See `docs/usage-remaining-gate.md`.
#[derive(Clone, Copy, Debug)]
pub struct StdoutLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub terminate_at_cap: bool,
}

/// Which armed stdout bound a stream overran, so the failure can name it accurately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamCapKind {
    Bytes,
    Lines,
}

impl StdoutLimits {
    /// The historic default: cap retention at `MAX_OUTPUT_BYTES`, no line cap, keep draining past
    /// the cap (do not kill the child). This preserves e.g. Codex finishing its final-message file
    /// even when its stdout event stream exceeds the cap.
    pub fn default_retain() -> Self {
        Self {
            max_bytes: MAX_OUTPUT_BYTES,
            max_lines: usize::MAX,
            terminate_at_cap: false,
        }
    }
}

use crate::config::{Config, ReviewerKind, ReviewerSpec, UsageMinimum};
use crate::errors::{self, Failure};

/// A reviewer's usage-remaining headroom, observed from the CLI's own machine output (Codex's
/// rollout `token_count.rate_limits`, Claude's `rate_limit_event`) -- never the model's prose.
///
/// Three-state with an explicit `Unknown`: an absent, unparseable, unrecognised, stale, or
/// identity-mismatched signal is `Unknown`, and `Unknown` never gates (fail-open). See
/// `docs/usage-remaining-gate.md`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Per-turn capability owned by the parent and injected only into a Codex invocation.
/// Claude receives its captured change in the prompt and never starts this server.
pub struct EvidenceInvocation<'a> {
    pub executable: &'a Path,
    pub bundle_file: &'a Path,
    pub nonce: &'a str,
    pub sterile_dir: Option<&'a Path>,
}

/// Guard for a verified empty Codex working directory outside the reviewed repository.
/// The directory is removed after the turn and recreated at the same stable path on resume.
pub struct SterileDir {
    path: PathBuf,
}

impl SterileDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SterileDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir(&self.path) {
            eprintln!(
                "cross-review: warning: could not remove sterile Codex directory {}: {e}",
                self.path.display()
            );
        }
    }
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

/// Environment variables carried through to a *controlled* reviewer child, by name.
///
/// A non-ambient child starts from a cleared environment ([`apply_controlled_env`]) and receives
/// only these, each from the parent's current value, plus its config-home variable. Everything else —
/// every provider-auth variable (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
/// `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, …) included — is simply absent, so an
/// unknown inherited variable cannot override the profile's OAuth credentials. This list is the
/// contract; the pre-spawn identity assertion (a later phase) is the backstop that catches anything it
/// still misses. See `docs/reviewer-account-profiles-impl.md`.
pub const CONTROLLED_ENV_ALLOWLIST: &[&str] = &[
    "SystemRoot",
    "windir",
    "SystemDrive",
    "ComSpec",
    "PATH",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Give `cmd` a controlled environment: clear the inherited environment, carry only
/// [`CONTROLLED_ENV_ALLOWLIST`] (each from the parent's current value), then set `home_var` to
/// `home`. Used for a non-ambient reviewer child. Ambient children are never passed here — they
/// inherit the environment unchanged, exactly as before this feature existed.
pub fn apply_controlled_env(cmd: &mut Command, home_var: &str, home: &Path) {
    cmd.env_clear();
    for key in CONTROLLED_ENV_ALLOWLIST {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    cmd.env(home_var, home.as_os_str());
}

/// Which interactive shape a reviewer's vendor login takes (#15 part 3b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed by the setup provisioning flow (#15 part 3b).
pub enum LoginMode {
    /// The login opens a browser that redirects to a localhost callback the CLI hosts; it completes on
    /// its own with stdin closed and output discarded (Codex).
    BrowserCallback,
    /// The login shows a code the human must paste back into the CLI's stdin (Claude); needs the
    /// interactive code-entry page + [`run_login_code_paste`].
    CodePaste,
}

/// The outcome of a vendor login run — deliberately **carries no captured text** (redaction, f-r2.7):
/// the child's stdout/stderr may echo OAuth tokens or the authorize URL, so they are discarded, never
/// returned or logged. Only these control-flow signals are exposed, so a caller cannot accidentally
/// surface a secret in a `Failure` or diagnostic.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub struct LoginOutcome {
    pub success: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub exit: Option<i32>,
    /// The login job could not be proven quiesced (terminate failed or a member outlived the bounded
    /// wait). A member may still be writing the staging dir, so the caller must **not** delete it now —
    /// leave it for recovery, whose armed journal owns it (f8/f1-impl-round-3).
    pub uncontained: bool,
}

/// How long to wait for in-job login helpers to exit after the login process itself does, before the
/// caller verifies credentials / renames staging (f14). Generous: normal logins quiesce at once.
const LOGIN_QUIESCE_BOUND: Duration = Duration::from_secs(10);

/// Run a vendor login `command` to completion under strict containment and redaction (#15 part 3b).
///
/// - **Redaction:** stdin/stdout/stderr are all `null`, so nothing the child prints is captured; the
///   result type has no text field.
/// - **Isolation:** the child runs from `scratch_cwd`, an owned empty directory (never a repo-settable
///   state dir), with the controlled environment already applied by the adapter's `login_command`.
/// - **Containment (f10/f14/f-b1):** the child is created suspended and assigned to a
///   `KILL_ON_JOB_CLOSE`, no-breakaway job *before* it runs (creation-time association), then resumed.
///   On timeout or cancel the job is terminated and drained; on natural exit the runner waits for the
///   job to quiesce (no in-job helper still writing the staging dir) before returning. If the job
///   cannot be created/assigned, or does not quiesce, login **fails closed** (`success == false`). The
///   browser the vendor opens is out-of-job and so never gated on or killed. Dropping the job at the
///   end reaps any straggler — including if *this* process later crashes (f-b1).
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn run_login(
    mut command: Command,
    scratch_cwd: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> LoginOutcome {
    command
        .current_dir(scratch_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let fail = LoginOutcome {
        success: false,
        timed_out: false,
        cancelled: false,
        exit: None,
        uncontained: false,
    };

    // Fail closed if we cannot obtain a containment job at all.
    let Some(job) = crate::winjob::JobObject::new() else {
        return fail;
    };
    let mut child = match job.spawn_in_job(&mut command) {
        Ok(c) => c,
        Err(_) => return fail,
    };

    let deadline = Instant::now() + timeout;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => {
                let contained = terminate_and_settle(&job, &mut child);
                return LoginOutcome {
                    uncontained: !contained,
                    ..fail
                };
            }
        }
        if cancel.load(Ordering::SeqCst) {
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                cancelled: true,
                uncontained: !contained,
                ..fail
            };
        }
        if Instant::now() >= deadline {
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                timed_out: true,
                uncontained: !contained,
                ..fail
            };
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    // Natural exit: wait for in-job helpers to quiesce before the caller verifies/renames staging. A
    // lingering in-job writer that does not clear in time is reaped **and we then wait for the reaping
    // to settle** before returning; if it still cannot be proven quiesced the outcome is marked
    // **uncontained**, so the caller leaves staging for recovery rather than deleting it under a
    // possibly-still-writing member (f8/f1-impl-round-3).
    if !wait_quiescent(&job) {
        let contained = terminate_and_settle(&job, &mut child);
        return LoginOutcome {
            exit: exit_code,
            uncontained: !contained,
            ..fail
        };
    }
    LoginOutcome {
        success: exit_code == Some(0),
        timed_out: false,
        cancelled: false,
        exit: exit_code,
        uncontained: false,
    }
}

/// Abort a login job: terminate it, wait (bounded) for the direct child, then wait for the whole job
/// to go quiet, so the caller never cleans up staging while a member is still writing (f8). The child
/// wait is bounded rather than an unbounded `child.wait()` so a wedged process cannot hang the runner.
///
/// Returns **contained** only if `TerminateJobObject` was accepted **and** the job reached zero active
/// processes — a failed terminate (even if quiescence then coincidentally reads zero) is *not*
/// containment. The caller leaves staging for recovery whenever this is false.
#[allow(dead_code)] // reached via run_login, whose caller lands with the setup provisioning flow.
fn terminate_and_settle(job: &crate::winjob::JobObject, child: &mut Child) -> bool {
    let terminated = job.terminate();
    let deadline = Instant::now() + LOGIN_QUIESCE_BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    terminated && wait_quiescent(job)
}

/// Wait (bounded) for the job's live process count to reach zero. `true` if it quiesced; `false` on
/// timeout or an unqueryable job (treated as uncontained by the caller).
#[allow(dead_code)] // reached via run_login, whose caller lands with the setup provisioning flow.
fn wait_quiescent(job: &crate::winjob::JobObject) -> bool {
    let deadline = Instant::now() + LOGIN_QUIESCE_BOUND;
    loop {
        match job.active_processes() {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run a vendor login that uses a **code-paste** flow (Claude's `auth login`): the browser shows an
/// authorization code the human must paste back into the login's **stdin**, rather than redirecting to
/// a localhost callback (#15 part 3b). Same containment/redaction posture as [`run_login`], plus the
/// interactive plumbing:
///
/// - The child runs in the login job (creation-time association) with stdin/stdout/stderr **piped**.
/// - Background scanners drain stdout+stderr (so the child never blocks on a full pipe) and extract the
///   vendor's **authorization URL** — the *only* thing taken from that output; the raw stream is never
///   logged or returned (it carries the URL's `state`/PKCE, redaction f-r2.7).
/// - A loopback [`crate::codeentry::CodeEntryServer`] shows the human that URL and a field to paste the
///   code; the submitted code is written to the child's stdin and never logged.
/// - Cancellation, an overall timeout, quiescence, and `uncontained` are handled exactly as in
///   [`run_login`]; on any abort the job is terminated-and-settled and staging is left for recovery.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn run_login_code_paste(
    mut command: Command,
    scratch_cwd: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
) -> LoginOutcome {
    command
        .current_dir(scratch_cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let fail = LoginOutcome::default();

    let Some(job) = crate::winjob::JobObject::new() else {
        return fail;
    };
    let mut child = match job.spawn_in_job(&mut command) {
        Ok(c) => c,
        Err(_) => return fail,
    };
    let url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut scanners = Vec::new();
    if let Some(o) = child.stdout.take() {
        scanners.push(spawn_url_scanner(o, Arc::clone(&url)));
    }
    if let Some(e) = child.stderr.take() {
        scanners.push(spawn_url_scanner(e, Arc::clone(&url)));
    }
    let mut stdin = child.stdin.take();
    let deadline = Instant::now() + timeout;

    // Phase 1: wait for the authorization URL to appear in the child's output.
    let auth_url = loop {
        if let Some(u) = url.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            break u;
        }
        if cancel.load(Ordering::SeqCst) {
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                cancelled: true,
                uncontained: !contained,
                ..fail
            };
        }
        if Instant::now() >= deadline || matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            // Timed out, or the child exited before printing a URL — a failed login.
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                timed_out: Instant::now() >= deadline,
                uncontained: !contained,
                ..fail
            };
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    // Phase 2: show the URL + a code field on a loopback page and wait for the human's code.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let server = match crate::codeentry::CodeEntryServer::start(
        "Finish signing in your reviewer profile",
        &auth_url,
        remaining,
    ) {
        Ok(s) => s,
        Err(_) => {
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                uncontained: !contained,
                ..fail
            };
        }
    };
    let _ = crate::approval::open_in_browser(server.url());
    eprintln!(
        "cross-review: after signing in, paste the code on this local page to finish:\n  {}",
        server.url()
    );

    let code = loop {
        match server.poll() {
            Some(crate::codeentry::CodeOutcome::Submitted(c)) => break c,
            Some(_) => {
                // The page timed out or was cancelled.
                let contained = terminate_and_settle(&job, &mut child);
                return LoginOutcome {
                    timed_out: Instant::now() >= deadline,
                    uncontained: !contained,
                    ..fail
                };
            }
            None => {}
        }
        if cancel.load(Ordering::SeqCst) {
            server.cancel();
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                cancelled: true,
                uncontained: !contained,
                ..fail
            };
        }
        if Instant::now() >= deadline || matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            server.cancel();
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                timed_out: Instant::now() >= deadline,
                uncontained: !contained,
                ..fail
            };
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    // Feed the code to the child's stdin on a **detached thread** so a non-reading child cannot block
    // the runner on a full pipe (f2): the overall timeout below reaps a wedged child, which closes this
    // pipe and unblocks (or errors) the write. Dropping the writer's `ChildStdin` at the end of the
    // thread gives the child EOF after the code. The code string is moved in and never logged.
    if let Some(mut si) = stdin.take() {
        let code_line = format!("{code}\n");
        std::thread::spawn(move || {
            let _ = si.write_all(code_line.as_bytes());
            let _ = si.flush();
        });
    }
    drop(server);

    // Phase 3: wait (bounded) for the child to exchange the code, write credentials, and exit.
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => {
                let contained = terminate_and_settle(&job, &mut child);
                return LoginOutcome {
                    uncontained: !contained,
                    ..fail
                };
            }
        }
        if cancel.load(Ordering::SeqCst) {
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                cancelled: true,
                uncontained: !contained,
                ..fail
            };
        }
        if Instant::now() >= deadline {
            let contained = terminate_and_settle(&job, &mut child);
            return LoginOutcome {
                timed_out: true,
                uncontained: !contained,
                ..fail
            };
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    // The child has exited. Prove containment with the **bounded** quiescence/terminate path, then
    // return. The scanner threads are **not** joined unboundedly (f1): a descendant that inherited a
    // pipe could hold it open forever, so joining could wedge the runner (and setup). They exit on
    // their own once the pipes close — which job termination guarantees — so we detach them.
    let contained = wait_quiescent(&job) || terminate_and_settle(&job, &mut child);
    drop(scanners);
    LoginOutcome {
        success: contained && exit_code == Some(0),
        exit: exit_code,
        uncontained: !contained,
        ..fail
    }
}

/// Drain `reader` (a child stdout/stderr pipe) so the child never blocks on a full pipe, scanning for
/// the first `https://…` authorization URL and storing it in `url`. The buffered output is bounded and
/// **never logged or returned** — only the extracted URL is surfaced.
#[allow(dead_code)] // reached via run_login_code_paste, whose caller lands with the setup flow.
fn spawn_url_scanner<R: Read + Send + 'static>(
    mut reader: R,
    url: Arc<Mutex<Option<String>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // A **byte** accumulator, so bounding it trims at a byte offset (a `String::split_off` at an
        // arbitrary byte would panic on non-ASCII output and stop the drain, f3).
        let mut acc: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    acc.extend_from_slice(&chunk[..n]);
                    {
                        let mut slot = url.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_none() {
                            if let Some(found) = extract_https_url(&String::from_utf8_lossy(&acc)) {
                                *slot = Some(found);
                            }
                        }
                    }
                    // Keep the buffer bounded: retain a tail large enough to hold a split URL.
                    if acc.len() > 64 * 1024 {
                        acc.drain(..acc.len() - 8192);
                    }
                }
            }
        }
    })
}

/// Extract the first `https://…` URL from `text`, ending at whitespace/control. `None` until a
/// plausibly-complete one is present.
fn extract_https_url(text: &str) -> Option<String> {
    let start = text.find("https://")?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c.is_control() || c == '"' || c == '<')
        .unwrap_or(rest.len());
    // Only accept a URL that has clearly ended (a delimiter followed), so we do not grab a prefix that
    // is still being written; and require a minimum length.
    if end < rest.len() && end > "https://a.b/".len() {
        Some(rest[..end].to_string())
    } else {
        None
    }
}

/// The config home an adapter should *read* an account from for `spec`, or `None` when it must read
/// none. The single account-read seam, threaded through [`Config::resolve_authorized_home`]:
///
/// - `Ok(Some(home))` — an authorized profile: read that home.
/// - `Ok(None)` — ambient: read the adapter's own ambient home via `ambient()`, today's behaviour.
/// - `Err(_)` — a non-ambient profile that is unauthorized or unresolvable: `None`. Accounting reads
///   (`account_fingerprint`, headroom observation) then fail open (`Unknown`); the *review* path
///   never reaches an accounting read for such a spec, because `resolve_authorized_home` already
///   refused the entry upstream.
pub fn home_for_reads(
    cfg: &Config,
    spec: &ReviewerSpec,
    ambient: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    match cfg.resolve_authorized_home(spec) {
        Ok(Some(home)) => Some(home),
        Ok(None) => ambient(),
        Err(_) => None,
    }
}

/// Whether a profile home's authentication is the subscription OAuth — the only method a profile may
/// use — or something else (an API key, an unrecognised method) that must be refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    /// Codex ChatGPT (`auth_mode == "chatgpt"`) / Claude claude.ai first-party subscription.
    Subscription,
    /// An API key or an unrecognised method: never valid for a profile.
    Other,
}

/// The account and auth method a profile home resolves to, established **pre-spawn** from local
/// surfaces (the auth file, the CLI's own `auth status` machine output) — never the model's prose.
/// The fail-closed confirmation that the controlled environment routed the reviewer to the intended
/// subscription account. See `docs/reviewer-account-profiles-impl.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    /// The account fingerprint the home resolves to (the same value `account_fingerprint` reads).
    pub account: String,
    /// Whether that home's auth is the subscription OAuth.
    pub method: AuthMethod,
}

/// Assert a profile home's resolved identity is usable for `expected_account`: the method is the
/// subscription OAuth, **and** the account matches exactly.
///
/// Fail-closed — a non-subscription method or any account mismatch refuses, so a review never runs
/// under the wrong credential. `expected_account` is the account the routing already authorized: at
/// the auth-check it is the profile home's own fingerprint (a method + self-consistency check); once
/// the allowlist exists it is the *authorized* account from the allowlist entry, so a profile
/// silently re-logged to a different account is caught here.
pub fn assert_profile_identity(
    reviewer: &str,
    resolved: &ResolvedIdentity,
    expected_account: &str,
) -> Result<(), Failure> {
    if resolved.method != AuthMethod::Subscription {
        return Err(errors::profile_identity_mismatch(
            reviewer,
            "the profile home's authentication is not a subscription sign-in (it is an API key or \
             an unrecognised method); a profile must be a subscription account",
        ));
    }
    if resolved.account != expected_account {
        return Err(errors::profile_identity_mismatch(
            reviewer,
            "the account the profile home resolves to is not the account authorized for this \
             profile; it may have been re-logged to a different account",
        ));
    }
    Ok(())
}

pub fn is_within(path: &Path, root: &Path) -> bool {
    let path = normalize_windows_path(path);
    let root = normalize_windows_path(root);
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

fn normalize_windows_path(path: &Path) -> String {
    let value = path.to_string_lossy().to_lowercase().replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/unc/") {
        format!("//{rest}")
    } else if let Some(rest) = value.strip_prefix("//?/") {
        rest.to_string()
    } else {
        value
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn containment_normalizes_verbatim_drive_and_unc_paths() {
        assert!(is_within(
            Path::new(r"\\?\C:\dev\repo\sub"),
            Path::new(r"C:\dev\repo")
        ));
        assert!(is_within(
            Path::new(r"C:\dev\repo\sub"),
            Path::new(r"\\?\C:\dev\repo")
        ));
        assert!(is_within(
            Path::new(r"\\?\UNC\server\share\repo\sub"),
            Path::new(r"\\server\share\repo")
        ));
        assert!(!is_within(
            Path::new(r"\\?\C:\dev\repository"),
            Path::new(r"C:\dev\repo")
        ));
    }
}

/// Working-directory modes stamped on a session so a resume cannot cross the project/neutral or
/// pre-evidence/sterile-evidence boundary under an existing reviewer conversation.
pub const CWD_MODE_PROJECT: &str = "project";
pub const CWD_MODE_NEUTRAL: &str = "neutral";
pub const CWD_MODE_CODEX_EVIDENCE: &str = "codex-sterile-evidence-v1";

/// Decide whether the Claude reviewer should run this turn from a neutral (non-git) working
/// directory, and if so with which absolute read-scope rules. `Some((dir, rules))` means run
/// the child there with `rules`; `None` means keep `cfg.cwd` and the configured (relative)
/// rules. Every condition must hold, and each failing one fails *closed* to `None`.
///
/// The point is to keep the reviewer's prompt cache stable across turns: Claude Code derives a
/// per-invocation git context from its cwd, so when the parent agent commits between turns that
/// context changes and invalidates the replayed conversation. Running outside the repo removes
/// the git context at the source. See `docs/resume-cache-cwd-invalidation.md` for the full
/// investigation and why each gate exists.
pub fn claude_neutral_target(
    cfg: &Config,
    reviewer: ReviewerKind,
) -> Option<(PathBuf, Vec<String>)> {
    // Claude only: isolated Codex uses its own sterile cwd plus evidence-service path.
    if reviewer != ReviewerKind::Claude {
        return None;
    }
    // Git backend only. The cache churn we are fixing comes from Claude Code's git context, and
    // the neutral-cwd path listings assume git-shaped output. A Perforce review whose workspace
    // happens to sit inside a git repo (so `is_git_toplevel` would be true) emits working-root-
    // relative Perforce paths, not git `a/`/`b/` diffs, so it must stay in project-cwd mode.
    if cfg.vcs != crate::config::Vcs::Git {
        return None;
    }
    // Isolation must be on. `--allow-reviewer-config` opts into loading project/user config,
    // which needs the project as cwd.
    if !cfg.isolate_reviewer {
        return None;
    }
    // Shell-less only: this helper predates the separate Codex evidence path. A shell-enabled
    // Claude reviewer still expects to run git itself and `--diff auto` withholds the capture on
    // that basis, so it needs the project as cwd. Use the
    // active entry's predicate, not the primary's, so a Codex->shell-less-Claude fallback is
    // judged on Claude.
    if cfg.reviewer_has_shell_of(reviewer) {
        return None;
    }
    // Only the default read rules can be translated to absolute form safely; a caller-supplied
    // relative rule would lose access from a neutral cwd.
    if !cfg.allowed_tools_are_default() {
        return None;
    }
    // Only when `cfg.cwd` is the git top-level. Then the working root is the repository root, so
    // every captured path (git-status is repo-root-relative, diff/untracked are working-root-
    // relative) shares one origin -- the absolute root we hand the reviewer -- and there is no
    // sub-directory ambiguity to resolve. It is also the only case where a git context that our
    // move would remove actually exists here.
    if !is_git_toplevel(&cfg.cwd) {
        return None;
    }
    // The path must be representable as a safe absolute glob prefix.
    let rules = crate::config::absolute_scoped_rules(&cfg.cwd)?;
    // The neutral directory must be verified to have no `.git` ancestor, or we would just move
    // the problem into a different repository. Fail closed if it cannot be confirmed.
    let dir = neutral_dir(cfg);
    if !verified_non_git_dir(&dir) {
        return None;
    }
    Some((dir, rules))
}

/// The cwd/evidence mode for the reviewer that would run this turn, recorded on the session and
/// compared on resume.
pub fn reviewer_cwd_mode(cfg: &Config, reviewer: ReviewerKind) -> &'static str {
    if reviewer == ReviewerKind::Codex && cfg.isolate_reviewer {
        CWD_MODE_CODEX_EVIDENCE
    } else if claude_neutral_target(cfg, reviewer).is_some() {
        CWD_MODE_NEUTRAL
    } else {
        CWD_MODE_PROJECT
    }
}

/// Whether `dir` is a git top-level -- it has a `.git` entry directly in it (a directory for a
/// normal clone, a file for a worktree or submodule). Errors read as "no", which is the
/// fail-closed direction: we only *enable* the optimisation when this is true.
fn is_git_toplevel(dir: &Path) -> bool {
    dir.join(".git").try_exists().unwrap_or(false)
}

/// Whether `dir` can be confirmed to sit outside any git repository. Canonicalised first so a
/// `..` segment or a junction/symlink cannot present a path that is textually outside the repo
/// while physically inside it, then every ancestor is checked for a `.git` entry. Any error --
/// a failed canonicalisation, an unreadable ancestor -- returns `false` (fail closed): an
/// unverifiable directory is treated as unsafe.
pub fn verified_non_git_dir(dir: &Path) -> bool {
    let Ok(canon) = std::fs::canonicalize(dir) else {
        return false;
    };
    for ancestor in canon.ancestors() {
        match ancestor.join(".git").try_exists() {
            Ok(true) => return false,
            Ok(false) => {}
            Err(_) => return false,
        }
    }
    true
}

/// Create and verify the stable empty directory used as an isolated Codex process cwd.
pub fn codex_sterile_dir(cfg: &Config, session: &str) -> std::io::Result<SterileDir> {
    const MAX_STERILE_DIRS: usize = 256;
    const STALE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
    let fingerprint = crate::digest::Fingerprint::of(session.as_bytes())
        .ok_or_else(|| std::io::Error::other("SHA-256 unavailable for sterile directory name"))?;
    let name = &fingerprint.sha256[..24];
    // Use the process temp root unconditionally. A user-selected state directory can move or
    // become writable between turns; choosing between it and temp dynamically would let one
    // Codex session resume under a different cwd. One deterministic base makes that impossible.
    let candidates = [std::env::temp_dir()];
    let mut last = None;
    for base in candidates {
        if let Err(e) = std::fs::create_dir_all(&base) {
            last = Some(e);
            continue;
        }
        let Ok(base) = std::fs::canonicalize(&base) else {
            continue;
        };
        if is_within(&base, &cfg.cwd) || !verified_non_git_dir(&base) {
            continue;
        }
        // This is the deterministic base for this configuration. Once selected, any failure or
        // contamination below refuses the turn; falling through to another base would silently
        // change Codex's session cwd between fresh and resume.
        let parent = base.join("cross-review-codex-cwd");
        std::fs::create_dir_all(&parent)?;
        let now = std::time::SystemTime::now();
        let mut retained = 0usize;
        for entry in std::fs::read_dir(&parent)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= STALE_AGE);
            let empty_dir = metadata.is_dir()
                && std::fs::read_dir(entry.path())?
                    .next()
                    .transpose()?
                    .is_none();
            if stale && empty_dir {
                let _ = std::fs::remove_dir(entry.path());
            } else {
                retained = retained.saturating_add(1);
            }
        }
        if retained >= MAX_STERILE_DIRS {
            return Err(std::io::Error::other(
                "sterile Codex directory reached its bounded live-entry limit",
            ));
        }
        let path = parent.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let canonical = std::fs::canonicalize(&path)?;
        let metadata = std::fs::symlink_metadata(&canonical)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(std::io::Error::other(
                    "sterile directory is a reparse point",
                ));
            }
        }
        if !metadata.is_dir()
            || is_within(&canonical, &cfg.cwd)
            || !verified_non_git_dir(&canonical)
        {
            return Err(std::io::Error::other(
                "sterile directory is not a verified non-repository directory",
            ));
        }
        if std::fs::read_dir(&canonical)?.next().transpose()?.is_some() {
            return Err(std::io::Error::other("sterile directory is not empty"));
        }
        return Ok(SterileDir { path: canonical });
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no safe sterile directory is available")))
}

pub trait Reviewer: Send + Sync {
    /// Cheap check that the CLI exists and is signed in. Runs before we spend a
    /// model call, so an unconfigured machine fails fast and legibly.
    ///
    /// `cancel` interrupts the check: a fallback entry's preflight runs inside the review's walk,
    /// so a cancellation (or shutdown) must be able to stop a 30-second auth probe rather than
    /// wait it out. See `docs/reviewer-fallback-chain.md`.
    fn auth_check(
        &self,
        bin: &Path,
        cfg: &Config,
        spec: &ReviewerSpec,
        cancel: &AtomicBool,
    ) -> Result<String, Failure>;

    fn invocation(
        &self,
        cfg: &Config,
        spec: &ReviewerSpec,
        bin: &Path,
        resume: Option<&str>,
        tmp_id: &str,
        evidence: Option<&EvidenceInvocation<'_>>,
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

    /// The same account identifier as [`account_fingerprint`](Self::account_fingerprint), but read
    /// **directly from a given config home** rather than through the authorized-home seam.
    ///
    /// This is the account currently in `home`, used by
    /// [`Config::resolve_authorized_home`](crate::config::Config::resolve_authorized_home) itself to
    /// match the allowlist's fingerprint field. It cannot route through `account_fingerprint`, which
    /// resolves the home via the very authorization being decided — that would recurse. `None` when
    /// the home has no readable account (an unprovisioned or re-logging profile), which the caller
    /// treats as unauthorized. See `docs/reviewer-account-profiles-impl.md` (`[f19]`).
    fn fingerprint_at(&self, _home: &Path) -> Option<String> {
        None
    }

    /// The per-spawn profile identity + auth-method probe: resolve the account and method a profile
    /// `home` currently presents, from local surfaces (Codex `auth.json`; Claude `auth status` + the
    /// account file). Runs **before every non-ambient spawn** and by the setup confirmation (#15) —
    /// **never cached**, because a cached subscription assertion could miss a later same-account
    /// auth-method downgrade (`[f2/f3]`). The caller passes the result to [`assert_profile_identity`]
    /// against the authorized account. Default fails closed for an adapter with no probe.
    fn resolve_home_identity(
        &self,
        _bin: &Path,
        _cfg: &Config,
        _home: &Path,
        _cancel: &AtomicBool,
    ) -> Result<ResolvedIdentity, Failure> {
        Err(errors::profile_identity_mismatch(
            "reviewer",
            "no profile identity probe is implemented for this reviewer",
        ))
    }

    /// Build the vendor **login** command that signs a fresh subscription session into `home` (the
    /// staging dir), with the controlled environment applied (`env_clear` + allowlist + the home var).
    /// The caller adds the owned scratch cwd, stdio and containment. Never an api-key/token flow.
    /// Default fails closed for an adapter with no login. (#15 part 3b)
    #[allow(dead_code)] // caller lands with the setup provisioning flow (#15 part 3b).
    fn login_command(&self, _bin: &Path, _home: &Path) -> Result<Command, Failure> {
        Err(errors::bad_request(
            "no vendor login is implemented for this reviewer",
        ))
    }

    /// Which login flow this reviewer uses (#15 part 3b): a **browser callback** to a localhost server
    /// (Codex — stdin closed, output discarded) or a **code paste** where the human pastes the
    /// authorization code back into the login's stdin (Claude). The setup runner picks the matching
    /// login executor. Default browser-callback.
    #[allow(dead_code)] // caller lands with the setup provisioning flow (#15 part 3b).
    fn login_mode(&self) -> LoginMode {
        LoginMode::BrowserCallback
    }

    /// The credential files the vendor writes into a signed-in home, in the order to poll-for-arrival
    /// and re-secure ([f20]). The **first** entry is the account file the confirmation reads
    /// handle-relative (f-a5). Empty for an adapter with no login.
    #[allow(dead_code)] // caller lands with the setup provisioning flow (#15 part 3b).
    fn credential_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// Confirm the just-provisioned `home`'s identity for the **setup** flow: read the account
    /// fingerprint **handle-relative** through the held directory (f-a5) and establish the auth method
    /// from an isolated probe run from `scratch_cwd` (f-r2.6). Distinct from
    /// [`resolve_home_identity`](Self::resolve_home_identity), which reads by path on the review path.
    /// Default fails closed. (#15 part 3b)
    #[allow(dead_code)] // caller lands with the setup provisioning flow (#15 part 3b).
    fn confirm_setup_identity(
        &self,
        _bin: &Path,
        _cfg: &Config,
        _home: &crate::profile::SecuredProfileDir,
        _scratch_cwd: &Path,
        _cancel: &AtomicBool,
    ) -> Result<ResolvedIdentity, Failure> {
        Err(errors::profile_identity_mismatch(
            "reviewer",
            "no setup identity confirmation is implemented for this reviewer",
        ))
    }

    /// The stdout bounds the runner applies to this reviewer. Default is retain-and-drain at
    /// `MAX_OUTPUT_BYTES`; the armed Claude path raises the byte cap, adds a line cap, and asks
    /// the runner to *terminate* the child at either bound (because `stream-json` carries the
    /// review text more than once and a runaway stream must not hold the worker until timeout).
    /// This is the single truncation contract on raw bytes/lines read — overrun before the
    /// terminal result is `OUTPUT_TRUNCATED`. See `docs/usage-remaining-gate.md`.
    fn output_limits(&self, _cfg: &Config) -> StdoutLimits {
        StdoutLimits::default_retain()
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
            if let Some(abs) = absolutize(explicit) {
                return Ok(abs);
            }
            tried.push(format!(
                "{} (from --bin; exists but could not be resolved to an absolute path)",
                explicit.display()
            ));
            return Err(errors::cli_not_found(spec.reviewer.as_str(), &tried));
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
                        if let Some(abs) = absolutize(&candidate) {
                            return Ok(abs);
                        }
                        // Existed a moment ago but no longer resolves: keep searching rather than
                        // return a non-absolute path.
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
            if let Some(abs) = absolutize(&candidate) {
                return Ok(abs);
            }
        }
        tried.push(candidate.display().to_string());
    }

    Err(errors::cli_not_found(spec.reviewer.as_str(), &tried))
}

/// Absolute form of a resolved bin path, so its stored, compared, and spawned forms do not depend
/// on the process working directory -- a relative `--bin` or relative PATH entry would otherwise
/// resolve to a different executable after a cwd change, which `resolved_bin_matches` could not
/// detect from the stored string alone.
///
/// `std::path::absolute` is a *lexical* full-path (GetFullPathName semantics): it resolves every
/// Windows form to an absolute path -- including a *drive-relative* `C:foo.exe`, which
/// `current_dir().join` would mishandle -- **without touching the filesystem**. Not resolving
/// symlinks matters here: the reviewer CLI is often a stable shim pointing at a versioned release
/// dir (e.g. codex's `...\releases\<ver>\...`), so canonicalizing would make the resolved bin
/// change on every CLI update and needlessly refuse resumes for the same install. It also adds no
/// `\\?\` prefix. Returns `None` only if absolutization fails (empty path, or the cwd cannot be
/// read); callers then treat the entry as unresolved rather than keeping a non-absolute path --
/// there is deliberately no silent fallback. (Needs Rust 1.79, hence this crate's MSRV.)
fn absolutize(path: &Path) -> Option<PathBuf> {
    std::path::absolute(path).ok()
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
    /// Set when an **armed** stdout stream overran its byte or line bound and the reader
    /// terminated the child at that point (`StdoutLimits::terminate_at_cap`). Names which bound
    /// tripped so the armed parser can report an accurate `OUTPUT_TRUNCATED`. `None` on every
    /// non-armed run. See `docs/usage-remaining-gate.md`.
    pub stdout_cap_hit: Option<StreamCapKind>,
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
    // flatten it back to a plain `io::Error`. They keep the ordinary retain-and-drain limits.
    run_observed(
        command,
        stdin_data,
        timeout,
        cancel,
        StdoutLimits::default_retain(),
        |_| {},
    )
    .map_err(RunError::into_io)
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
    // Bounds on the retained stdout buffer, and whether to kill the child at the bound. The armed
    // Claude path raises the byte cap and terminates at it; every other path keeps the historic
    // retain-and-drain (see StdoutLimits). stderr always uses MAX_OUTPUT_BYTES retention.
    stdout_limits: StdoutLimits,
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
    let stdout_buf = drain(
        child.stdout.take().expect("stdout was piped"),
        stdout_limits,
    );
    // stderr is never the review, so it keeps the historic retain-and-drain (no early kill).
    let stderr_buf = drain(
        child.stderr.take().expect("stderr was piped"),
        StdoutLimits::default_retain(),
    );

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
                // The armed stdout reader stops and flags which bound it overran; kill the child
                // it left blocked on a full pipe.
                let over_cap = stdout_buf.cap_hit().is_some();
                let stop = if cancel.load(Ordering::SeqCst) {
                    cancelled = true;
                    true
                } else if now >= deadline {
                    timed_out = true;
                    true
                } else if over_cap {
                    // Armed stream ran past its byte or line bound: kill it now rather than hold
                    // the worker until the timeout (round-1-impl finding f2). `stdout_truncated`
                    // (byte cap) and the retained line count let the parser name which bound
                    // tripped and return OUTPUT_TRUNCATED. Not flagged as timed_out/cancelled.
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
    // Read the cap signal *after* `collect` has waited for the reader thread (round-2-impl
    // finding f10 propagation race): the reader sets `cap_hit` and then `done`, and `collect`
    // blocks on `done`, so sampling it before could miss an overrun the reader recorded a moment
    // later — letting a truncated stream through as accepted.
    let stdout_cap_hit = stdout_buf.cap_hit();

    Ok(RunOutcome {
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout_lossy: stdout.lossy,
        stdout_incomplete: stdout.incomplete,
        stdout_cap_hit,
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
    /// For an **armed** stream (`StdoutLimits::terminate_at_cap`), which bound the reader first
    /// overran — at which point it stops reading, so the child blocks and the poll loop kills it.
    /// `0` none, `1` bytes, `2` lines. `None` shape via [`Drain::cap_hit`].
    cap_hit: Arc<std::sync::atomic::AtomicU8>,
}

impl Drain {
    fn len(&self) -> usize {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn cap_hit(&self) -> Option<StreamCapKind> {
        match self.cap_hit.load(Ordering::SeqCst) {
            1 => Some(StreamCapKind::Bytes),
            2 => Some(StreamCapKind::Lines),
            _ => None,
        }
    }
}

/// Start draining a pipe into a shared buffer.
///
/// Reading incrementally rather than returning at EOF is what makes a partial transcript
/// recoverable: whatever arrived before the deadline is already in the buffer.
///
/// The buffer is capped at `limits.max_bytes`. For a **non-armed** stream
/// (`terminate_at_cap == false`) the reader keeps consuming the pipe past the cap and throws the
/// bytes away, because a reader that stopped would fill the pipe and block the child forever --
/// trading unbounded memory for a hung review, which is a worse bargain; reaching the cap is
/// recorded as `truncated`. For an **armed** stream the reader instead **stops at the first
/// raw-byte or raw-line overrun**, records which bound it was, and returns: the pipe then fills,
/// the child blocks, and the poll loop terminates it — so raw input is bounded to the cap plus at
/// most one chunk rather than read to completion (round-2-impl finding f10).
fn drain(mut pipe: impl std::io::Read + Send + 'static, limits: StdoutLimits) -> Drain {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let truncated = Arc::new(AtomicBool::new(false));
    let errored = Arc::new(AtomicBool::new(false));
    let cap_hit = Arc::new(std::sync::atomic::AtomicU8::new(0));

    let writer_buf = Arc::clone(&buffer);
    let writer_done = Arc::clone(&done);
    let writer_truncated = Arc::clone(&truncated);
    let writer_errored = Arc::clone(&errored);
    let writer_cap_hit = Arc::clone(&cap_hit);
    std::thread::spawn(move || {
        let mut raw_bytes: usize = 0;
        let mut raw_lines: usize = 0;
        loop {
            let mut chunk = [0u8; 8192];
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Err(_) => {
                    // A read error means the stream ended abnormally: record it so a consumer
                    // that needs a complete stream does not parse a partial prefix as whole.
                    writer_errored.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(n) => {
                    raw_bytes = raw_bytes.saturating_add(n);
                    let newlines = chunk[..n].iter().filter(|&&b| b == b'\n').count();
                    raw_lines = raw_lines.saturating_add(newlines);
                    {
                        let mut buffer = writer_buf.lock().unwrap_or_else(|e| e.into_inner());
                        let room = limits.max_bytes.saturating_sub(buffer.len());
                        if n > room {
                            writer_truncated.store(true, Ordering::SeqCst);
                        }
                        buffer.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    // Armed: stop at the first bound overrun (strictly exceeding the bound, so a
                    // stream exactly at the bound is allowed), recording which bound it was. The
                    // poll loop then kills the child. Bytes checked before lines only to pick one
                    // when a single chunk crosses both.
                    if limits.terminate_at_cap {
                        if raw_bytes > limits.max_bytes {
                            writer_cap_hit.store(1, Ordering::SeqCst);
                            break;
                        }
                        if raw_lines > limits.max_lines {
                            writer_cap_hit.store(2, Ordering::SeqCst);
                            break;
                        }
                    }
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
        cap_hit,
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
        assert_eq!(
            HeadroomLevel::from_status("allowed"),
            Some(HeadroomLevel::Ample)
        );
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
        let drain = drain(std::io::Cursor::new(bytes), StdoutLimits::default_retain());
        collect(&drain, Instant::now() + Duration::from_secs(5))
    }

    #[test]
    fn output_under_the_cap_is_kept_whole_and_not_flagged() {
        let collected = drained(b"a normal transcript".to_vec());
        assert_eq!(collected.text, "a normal transcript");
        assert!(!collected.truncated);
    }

    #[test]
    fn armed_cap_hit_is_visible_after_collect_even_on_natural_eof() {
        // Regression for the f10 propagation race: a finite stream (EOF, like a child that exits
        // right after overrunning) that exceeds the byte bound must have `cap_hit` observable
        // once `collect` has waited for the reader. `collect` blocks on the reader's `done`, and
        // the reader sets `cap_hit` before `done`, so reading it after `collect` is race-free.
        let limits = StdoutLimits {
            max_bytes: 16,
            max_lines: usize::MAX,
            terminate_at_cap: true,
        };
        let bytes_drain = drain(std::io::Cursor::new(vec![b'x'; 64]), limits);
        let _ = collect(&bytes_drain, Instant::now() + Duration::from_secs(5));
        assert_eq!(bytes_drain.cap_hit(), Some(StreamCapKind::Bytes));

        // The line bound likewise, and a stream exactly at neither bound is not flagged.
        let line_limits = StdoutLimits {
            max_bytes: usize::MAX,
            max_lines: 3,
            terminate_at_cap: true,
        };
        let d = drain(
            std::io::Cursor::new(b"a\nb\nc\nd\ne\n".to_vec()),
            line_limits,
        );
        let _ = collect(&d, Instant::now() + Duration::from_secs(5));
        assert_eq!(d.cap_hit(), Some(StreamCapKind::Lines));

        let ok = drain(
            std::io::Cursor::new(b"a\nb\n".to_vec()),
            StdoutLimits {
                max_bytes: 1024,
                max_lines: 10,
                terminate_at_cap: true,
            },
        );
        let _ = collect(&ok, Instant::now() + Duration::from_secs(5));
        assert_eq!(ok.cap_hit(), None);
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
            StdoutLimits::default_retain(),
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
        let drain = drain(
            Counting {
                remaining: source,
                taken: Arc::clone(&taken),
            },
            StdoutLimits::default_retain(),
        );
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
            stdout_cap_hit: None,
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
            stdout_cap_hit: None,
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
            stdout_cap_hit: None,
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

#[cfg(test)]
mod controlled_env_tests {
    use super::*;

    fn envs(cmd: &Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn controlled_env_sets_the_home_and_drops_provider_variables() {
        let mut cmd = Command::new("x");
        // Provider-auth variables explicitly present before the controlled env must be gone after —
        // `env_clear` wipes the whole map, including prior explicit sets, so nothing inherited can
        // override the profile OAuth.
        cmd.env("ANTHROPIC_API_KEY", "secret");
        cmd.env("OPENAI_API_KEY", "secret");
        cmd.env("CODEX_ACCESS_TOKEN", "secret");
        apply_controlled_env(&mut cmd, "CODEX_HOME", Path::new(r"C:\profiles\codex\work"));

        let envs = envs(&cmd);
        assert!(
            envs.iter()
                .any(|(k, v)| k == "CODEX_HOME" && v.as_deref() == Some(r"C:\profiles\codex\work")),
            "home var must be set to the profile home: {envs:?}"
        );
        // No provider-auth variable survives, by exact name or by shape.
        for (k, _) in &envs {
            assert!(
                !k.contains("API_KEY") && !k.contains("ACCESS_TOKEN") && !k.contains("OAUTH_TOKEN"),
                "provider-auth variable leaked into the controlled env: {k}"
            );
        }
        // Every remaining key is on the allowlist or is the home var itself — nothing else leaks.
        for (k, _) in &envs {
            assert!(
                k == "CODEX_HOME" || CONTROLLED_ENV_ALLOWLIST.contains(&k.as_str()),
                "unexpected key in controlled env: {k}"
            );
        }
    }

    #[test]
    fn assert_profile_identity_requires_subscription_and_matching_account() {
        let sub = ResolvedIdentity {
            account: "acct-1".to_string(),
            method: AuthMethod::Subscription,
        };
        assert!(assert_profile_identity("codex", &sub, "acct-1").is_ok());
        // A different account refuses.
        let e = assert_profile_identity("codex", &sub, "acct-2").unwrap_err();
        assert_eq!(e.code, "PROFILE_IDENTITY_MISMATCH");
        // A non-subscription method refuses even with a matching account.
        let apikey = ResolvedIdentity {
            account: "acct-1".to_string(),
            method: AuthMethod::Other,
        };
        let e = assert_profile_identity("codex", &apikey, "acct-1").unwrap_err();
        assert_eq!(e.code, "PROFILE_IDENTITY_MISMATCH");
    }

    #[test]
    fn the_allowlist_carries_no_provider_auth_variable() {
        for key in CONTROLLED_ENV_ALLOWLIST {
            assert!(
                !key.contains("API_KEY")
                    && !key.contains("ACCESS_TOKEN")
                    && !key.contains("OAUTH")
                    && !key.starts_with("ANTHROPIC")
                    && !key.starts_with("OPENAI")
                    && !key.starts_with("CODEX")
                    && !key.starts_with("CLAUDE"),
                "the controlled-env allowlist must not carry a provider/auth variable: {key}"
            );
        }
    }

    #[test]
    fn run_login_reports_success_and_failure_exit_codes() {
        let scratch = std::env::temp_dir();
        // A "login" that exits 0 succeeds and reports its code.
        let mut ok = Command::new("cmd.exe");
        ok.args(["/C", "exit 0"]);
        let out = run_login(
            ok,
            &scratch,
            Duration::from_secs(20),
            &AtomicBool::new(false),
        );
        assert!(out.success && out.exit == Some(0) && !out.timed_out && !out.cancelled);

        // A non-zero exit is a failed login, not a success.
        let mut bad = Command::new("cmd.exe");
        bad.args(["/C", "exit 3"]);
        let out = run_login(
            bad,
            &scratch,
            Duration::from_secs(20),
            &AtomicBool::new(false),
        );
        assert!(!out.success && out.exit == Some(3));
    }

    #[test]
    fn extract_https_url_finds_a_complete_url_only() {
        // A complete URL followed by a delimiter is extracted; a still-being-written prefix is not.
        let claude = "Opening browser to sign in…\nIf the browser didn't open, visit: \
                      https://claude.com/cai/oauth/authorize?code=true&state=abc\nPaste code here > ";
        assert_eq!(
            extract_https_url(claude).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&state=abc")
        );
        // No delimiter yet (URL may still be streaming) -> None.
        assert_eq!(
            extract_https_url("visit: https://claude.com/cai/oauth"),
            None
        );
        // No URL at all.
        assert_eq!(extract_https_url("Opening browser to sign in…\n"), None);
    }

    #[test]
    fn run_login_honours_cancellation() {
        // A pre-cancelled run terminates the child promptly and reports cancelled, never success.
        let scratch = std::env::temp_dir();
        let mut slow = Command::new("cmd.exe");
        // ping is a portable ~29s sleep; the runner must not wait it out once cancel is set. A generous
        // bound (well under the child's own runtime) proves the short-circuit without flaking under
        // heavy parallel test load, where terminate+quiesce can add a couple of seconds.
        slow.args(["/C", "ping -n 30 127.0.0.1"]);
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        let out = run_login(slow, &scratch, Duration::from_secs(120), &cancel);
        assert!(out.cancelled && !out.success);
        // A normally-terminable child is proven contained (terminate accepted + job quiesced), so the
        // caller may clean up staging rather than leaking it. (The `uncontained == true` path requires
        // an OS terminate failure / non-quiescence, which is impractical to force in a unit test.)
        assert!(
            !out.uncontained,
            "a killable login child must be reported contained"
        );
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "a cancelled login must not wait out the child"
        );
    }
}
