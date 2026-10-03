//! The converged-review hook (issue #142; design in `docs/converged-hook-plan.md`).
//!
//! When a git review converges on **exactly** a committed change, the server runs a
//! repository-configured program (`--on-converged`) with a JSON payload on its stdin, and the
//! repository does whatever it wants with it — typically post a commit status that a branch ruleset
//! requires. The server never talks to a forge and holds no forge credentials.
//!
//! The part that stays in the server is the **binding**: the hook fires for commit `H` only when the
//! canonical diff the reviewer was served is byte-identical to the `base..H` committed change,
//! composed by the same code ([`crate::evidence::committed_change_digest`]). A naive hook therefore
//! cannot post `success` for code the reviewer never saw. Every divergence skips; nothing here can
//! turn a non-match into a fire.
//!
//! The hook runs as the server's user, unsandboxed — it is the repository owner's code, with the same
//! trust as the MCP entry's own `command`. It does **not** touch the reviewer's isolation: the reviewer
//! cannot influence whether it runs, which commit it names, or any byte of its payload.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::evidence::{AutoCrlf, Limits};
use crate::winjob::JobObject;

/// Default `--on-converged-timeout-seconds`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Upper bound on `--on-converged-timeout-seconds`. The hook runs before the review result is
/// published (and, for an attest, under the session lease), so an unbounded hook would hold both.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// The payload's own schema version, independent of the result envelope's.
pub const PAYLOAD_SCHEMA_VERSION: u32 = 1;
/// How much of the hook's combined output the result keeps.
const OUTPUT_TAIL_BYTES: usize = 2048;
/// How often the runner checks whether the hook (and its descendants) have finished.
const POLL: Duration = Duration::from_millis(25);
/// How long to wait for the job to empty after terminating it.
const TERMINATE_QUIESCE: Duration = Duration::from_secs(2);
/// How long to wait for the output readers once every process in the job is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Skip and failure reasons, as the `reason` field reports them.
pub mod reason {
    pub const NOT_GIT: &str = "not_git";
    pub const NOT_CONVERGED: &str = "not_converged";
    pub const CANCELLED: &str = "cancelled";
    pub const NO_CANONICAL_DIFF: &str = "no_canonical_diff";
    pub const SESSION_NOT_RECORDED: &str = "session_not_recorded";
    pub const HEAD_MISMATCH: &str = "head_does_not_match_review";
    pub const BINDING_CHECK_FAILED: &str = "binding_check_failed";
    pub const NO_CONTAINMENT: &str = "no_containment";
    pub const SPAWN_FAILED: &str = "spawn_failed";
    pub const NOT_QUIESCED: &str = "not_quiesced";
    pub const WAIT_FAILED: &str = "wait_failed";
    pub const NOT_LATEST_TURN: &str = "not_latest_turn";
}

/// The configured hook: a program, its arguments (each passed as its own argument, never a command
/// line we split), and a timeout covering the program and everything it spawns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub timeout: Duration,
}

/// The served canonical change a converged turn approved — what the binding check compares against.
/// Taken from the turn's serve-record aggregate while the record still exists, plus the exact
/// `core.autocrlf` the turn's evidence bundle was built with (plan f2), never a fresh lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedChange {
    pub digest: String,
    pub base: String,
    pub base_source: String,
    pub autocrlf: Option<AutoCrlf>,
}

/// Everything needed to fire — or, through `cross_model_review_attest`, re-fire — the hook for one
/// converged turn. Held in memory on the registry record only; never persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attestable {
    pub served: ServedChange,
    pub review_id: String,
    pub session: String,
    pub turn: u32,
    /// The durable session identity this turn left behind, read under the review's own lease right
    /// after the turn was recorded. An attest requires the record still carries both (plan f7).
    pub cli_session_id: String,
    pub turns: u32,
    /// The result's human-facing `reviewer` string.
    pub reviewer: String,
    pub reviewer_kind: String,
    pub model: String,
    pub effort: String,
    /// The configured `--reviewer` chain index of the entry that ran; `0` is the primary.
    pub chain_index: usize,
}

/// What fired the hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The end of a converged review turn.
    Review,
    /// A `cross_model_review_attest` call.
    Attest,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Review => "review",
            Trigger::Attest => "attest",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStatus {
    Succeeded,
    Failed,
    TimedOut,
    Skipped,
}

/// What happened to the hook, as the result's `hook` field reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HookReport {
    pub status: HookStatus,
    pub reason: Option<String>,
    pub captured_head: Option<String>,
    pub exit_code: Option<i32>,
    pub output: Option<String>,
}

impl HookReport {
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::with(HookStatus::Skipped, Some(reason.into()))
    }

    fn failed(reason: impl Into<String>) -> Self {
        Self::with(HookStatus::Failed, Some(reason.into()))
    }

    fn with(status: HookStatus, reason: Option<String>) -> Self {
        Self {
            status,
            reason,
            captured_head: None,
            exit_code: None,
            output: None,
        }
    }

    /// Whether the hook ran (or tried to) and did not succeed: worth a run warning.
    pub fn is_failure(&self) -> bool {
        matches!(self.status, HookStatus::Failed | HookStatus::TimedOut)
    }

    /// The one-line text-body form, e.g. `succeeded (head 1a2b3c4d)`.
    pub fn summary(&self) -> String {
        let status = match self.status {
            HookStatus::Succeeded => "succeeded",
            HookStatus::Failed => "failed",
            HookStatus::TimedOut => "timed_out",
            HookStatus::Skipped => "skipped",
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(reason) = &self.reason {
            parts.push(match reason.as_str() {
                reason::HEAD_MISMATCH => format!(
                    "{reason}: the reviewed change is not exactly HEAD's committed change; commit \
                     everything, including untracked files, then re-review"
                ),
                _ => reason.clone(),
            });
        }
        if let Some(code) = self.exit_code {
            parts.push(format!("exit {code}"));
        }
        if let Some(head) = &self.captured_head {
            parts.push(format!("head {}", &head[..head.len().min(12)]));
        }
        if parts.is_empty() {
            status.to_string()
        } else {
            format!("{status} ({})", parts.join(", "))
        }
    }
}

/// Runs the hook program. A trait so the gating and attest logic are testable with a recording fake
/// (the issue's "fake status sink"); production uses [`ProcessRunner`].
pub trait HookRunner: Send + Sync {
    /// Run the hook with `payload` on stdin. The returned report's `captured_head` is filled in by
    /// [`fire`], not here.
    fn run(&self, payload: &str) -> HookReport;
}

/// The production runner: a contained child process in the working root.
pub struct ProcessRunner {
    cfg: HookConfig,
    root: PathBuf,
}

impl ProcessRunner {
    pub fn new(cfg: HookConfig, root: PathBuf) -> Self {
        Self { cfg, root }
    }
}

impl HookRunner for ProcessRunner {
    fn run(&self, payload: &str) -> HookReport {
        run_process(&self.cfg, &self.root, payload, JobObject::new)
    }
}

/// The binding check: `Ok(H)` when the served canonical diff is byte-identical to `base..H` for the
/// current `HEAD`, else the skip report saying why. Both sides are immutable (a recorded digest and
/// two commits), so there is no time-of-check window; any git failure skips.
pub fn check_binding(root: &Path, served: &ServedChange) -> Result<String, HookReport> {
    let limits = Limits::default();
    let failed =
        |detail: String| HookReport::skipped(format!("{}: {detail}", reason::BINDING_CHECK_FAILED));
    let head = match crate::evidence::resolve_head(root, &limits) {
        Ok(Some(head)) => head,
        Ok(None) => return Err(failed("HEAD does not resolve to a commit".into())),
        Err(e) => return Err(failed(e.message)),
    };
    match crate::evidence::committed_change_digest(
        root,
        &served.base,
        &served.base_source,
        &head,
        served.autocrlf,
        &limits,
    ) {
        Ok(digest) if digest == served.digest => Ok(head),
        Ok(_) => Err(HookReport::skipped(reason::HEAD_MISMATCH)),
        Err(e) => Err(failed(e.message)),
    }
}

/// The JSON object the hook reads on stdin. Every field is server-derived or caller-supplied
/// (`session`); nothing the reviewer wrote reaches it.
pub fn payload(att: &Attestable, head: &str, trigger: Trigger, root: &Path) -> String {
    json!({
        "schema_version": PAYLOAD_SCHEMA_VERSION,
        "event": "converged",
        // Always "converged" / true today: the hook only fires then. Kept so the payload is
        // self-describing; `tree_clean` means precisely "the reviewed change was exactly
        // captured_head's committed change", which the binding check proved.
        "outcome": "converged",
        "trigger": trigger.as_str(),
        "session": att.session,
        "turn": att.turn,
        "review_id": att.review_id,
        "reviewer": att.reviewer,
        "reviewer_kind": att.reviewer_kind,
        "model": att.model,
        "effort": att.effort,
        "chain_index": att.chain_index,
        "captured_head": head,
        "tree_clean": true,
        "base": att.served.base,
        "base_source": att.served.base_source,
        "diff_sha256": att.served.digest,
        "repo_root": root.to_string_lossy(),
        "server_version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

/// Check the binding, then run the hook for the matching commit.
pub fn fire(
    runner: &dyn HookRunner,
    root: &Path,
    att: &Attestable,
    trigger: Trigger,
) -> HookReport {
    let head = match check_binding(root, &att.served) {
        Ok(head) => head,
        Err(report) => return report,
    };
    let mut report = runner.run(&payload(att, &head, trigger, root));
    report.captured_head = Some(head);
    report
}

/// The turn-end decision. `None` when no hook is configured (the result's `hook` is then `null`);
/// otherwise the report — a skip naming why the hook did not run, or what running it did.
/// `attestable` is `Err(reason)` when a converged git turn could not be bound (no complete canonical
/// diff, or no durable session record).
pub fn on_turn_end(
    runner: Option<&dyn HookRunner>,
    is_git: bool,
    outcome: Option<crate::findings::Outcome>,
    cancelled: bool,
    attestable: Result<&Attestable, &str>,
    root: &Path,
) -> Option<HookReport> {
    let runner = runner?;
    if !is_git {
        return Some(HookReport::skipped(reason::NOT_GIT));
    }
    if outcome != Some(crate::findings::Outcome::Converged) {
        return Some(HookReport::skipped(reason::NOT_CONVERGED));
    }
    if cancelled {
        return Some(HookReport::skipped(reason::CANCELLED));
    }
    Some(match attestable {
        Ok(att) => fire(runner, root, att, Trigger::Review),
        Err(why) => HookReport::skipped(why),
    })
}

/// A pipe reader keeping only the last `cap` bytes, so a chatty hook costs bounded memory.
fn drain_tail(
    mut pipe: impl Read + Send + 'static,
    cap: usize,
) -> (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let sink = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut b = sink.lock().unwrap_or_else(|e| e.into_inner());
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > cap * 2 {
                        let excess = b.len() - cap;
                        b.drain(..excess);
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });
    (buf, done_rx)
}

fn tail_text(buf: &Arc<Mutex<Vec<u8>>>, cap: usize) -> String {
    let b = buf.lock().unwrap_or_else(|e| e.into_inner());
    let start = b.len().saturating_sub(cap);
    String::from_utf8_lossy(&b[start..]).into_owned()
}

/// Wait until the job has no live process, or `deadline` passes. `false` when it did not empty, or
/// could not be queried (treated as not quiesced: fail closed).
fn quiesce(job: &JobObject, deadline: Instant) -> bool {
    loop {
        match job.active_processes() {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Run the hook contained, with a bounded wait, and report what happened.
///
/// - **Containment is required** (plan f3): no job object means the hook is never launched, and
///   `spawn_in_job` creates the child suspended and kills it rather than resume it uncontained.
/// - stdin carries the payload on its own thread; stdout and stderr are piped (never inherited — the
///   server's stdout is MCP protocol traffic) and drained concurrently with a bounded tail (f4).
/// - **Every exit is followed by quiescence** (f5): `succeeded` needs exit 0 *and* an empty job, so a
///   helper still posting the status cannot be reported as done. The timeout covers the whole tree.
fn run_process(
    cfg: &HookConfig,
    root: &Path,
    payload: &str,
    make_job: impl FnOnce() -> Option<JobObject>,
) -> HookReport {
    let Some(job) = make_job() else {
        return HookReport::failed(reason::NO_CONTAINMENT);
    };
    let mut command = Command::new(&cfg.program);
    command
        .args(&cfg.args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = match job.spawn_in_job(&mut command) {
        Ok(child) => child,
        Err(e) => return HookReport::failed(format!("{}: {e}", reason::SPAWN_FAILED)),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let data = payload.as_bytes().to_vec();
        // Never joined: a hook that exits without reading stdin closes the pipe and ends the write.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
            let _ = stdin.flush();
        });
    }
    let stdout = child
        .stdout
        .take()
        .map(|p| drain_tail(p, OUTPUT_TAIL_BYTES));
    let stderr = child
        .stderr
        .take()
        .map(|p| drain_tail(p, OUTPUT_TAIL_BYTES));

    let deadline = started + cfg.timeout;
    let mut exit: Option<ExitStatus> = None;
    let mut wait_failed = false;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = Some(status);
                break;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => {
                wait_failed = true;
                break;
            }
        }
    }

    let mut report = match exit {
        Some(status) => {
            let code = status.code();
            if quiesce(&job, deadline) {
                let mut r = if status.success() {
                    HookReport::with(HookStatus::Succeeded, None)
                } else {
                    HookReport::with(HookStatus::Failed, None)
                };
                r.exit_code = code;
                r
            } else {
                job.terminate();
                quiesce(&job, Instant::now() + TERMINATE_QUIESCE);
                let mut r = HookReport::failed(reason::NOT_QUIESCED);
                r.exit_code = code;
                r
            }
        }
        None => {
            job.terminate();
            let _ = child.kill();
            let quiet = quiesce(&job, Instant::now() + TERMINATE_QUIESCE);
            let _ = child.wait();
            if wait_failed {
                HookReport::failed(reason::WAIT_FAILED)
            } else {
                HookReport::with(
                    HookStatus::TimedOut,
                    (!quiet).then(|| reason::NOT_QUIESCED.to_string()),
                )
            }
        }
    };

    // Every process is gone (or terminated), so the pipes close and the readers finish; the grace
    // only covers a reader still copying its last chunk.
    let grace = Instant::now() + DRAIN_GRACE;
    let mut collected = String::new();
    for (label, drained) in [("stdout", stdout), ("stderr", stderr)] {
        if let Some((buf, done)) = drained {
            let _ = done.recv_timeout(grace.saturating_duration_since(Instant::now()));
            let text = tail_text(&buf, OUTPUT_TAIL_BYTES);
            if !text.trim().is_empty() {
                eprintln!("cross-review: hook {label}: {}", text.trim_end());
                collected.push_str(&text);
            }
        }
    }
    if !collected.trim().is_empty() {
        let start = collected.len().saturating_sub(OUTPUT_TAIL_BYTES);
        let start = (start..collected.len())
            .find(|&i| collected.is_char_boundary(i))
            .unwrap_or(collected.len());
        report.output = Some(collected[start..].to_string());
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Outcome;

    /// Records every payload it is handed and answers with a fixed report.
    pub(crate) struct Recording {
        pub calls: Mutex<Vec<String>>,
    }

    impl Recording {
        pub fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl HookRunner for Recording {
        fn run(&self, payload: &str) -> HookReport {
            self.calls.lock().unwrap().push(payload.to_string());
            let mut r = HookReport::with(HookStatus::Succeeded, None);
            r.exit_code = Some(0);
            r
        }
    }

    fn attestable(served: ServedChange) -> Attestable {
        Attestable {
            served,
            review_id: "rv-1-1".into(),
            session: "feat/x".into(),
            turn: 2,
            cli_session_id: "thread-1".into(),
            turns: 2,
            reviewer: "OpenAI Codex (codex, model=gpt-5.6-luna, effort=xhigh)".into(),
            reviewer_kind: "codex".into(),
            model: "gpt-5.6-luna".into(),
            effort: "xhigh".into(),
            chain_index: 0,
        }
    }

    fn dummy_served() -> ServedChange {
        ServedChange {
            digest: "00".repeat(32),
            base: "a".repeat(40),
            base_source: "merge-base(HEAD, refs/remotes/origin/HEAD)".into(),
            autocrlf: None,
        }
    }

    fn cmd_exe() -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32").join("cmd.exe")
    }

    // ---- gating ----

    #[test]
    fn no_runner_means_no_hook_field() {
        let att = attestable(dummy_served());
        let r = on_turn_end(
            None,
            true,
            Some(Outcome::Converged),
            false,
            Ok(&att),
            Path::new("."),
        );
        assert!(r.is_none());
    }

    #[test]
    fn every_ungated_combination_skips_without_invoking_the_runner() {
        let rec = Recording::new();
        let att = attestable(dummy_served());
        let root = Path::new(".");
        // (is_git, outcome, cancelled, attestable, expected skip reason)
        type Case<'a> = (
            bool,
            Option<Outcome>,
            bool,
            Result<&'a Attestable, &'a str>,
            &'a str,
        );
        let cases: Vec<Case> = vec![
            (
                false,
                Some(Outcome::Converged),
                false,
                Ok(&att),
                reason::NOT_GIT,
            ),
            (
                true,
                Some(Outcome::ChangesRequested),
                false,
                Ok(&att),
                reason::NOT_CONVERGED,
            ),
            (
                true,
                Some(Outcome::Escalate),
                false,
                Ok(&att),
                reason::NOT_CONVERGED,
            ),
            (
                true,
                Some(Outcome::Rebaseline),
                false,
                Ok(&att),
                reason::NOT_CONVERGED,
            ),
            (true, None, false, Ok(&att), reason::NOT_CONVERGED),
            (
                true,
                Some(Outcome::Converged),
                true,
                Ok(&att),
                reason::CANCELLED,
            ),
            (
                true,
                Some(Outcome::Converged),
                false,
                Err(reason::NO_CANONICAL_DIFF),
                reason::NO_CANONICAL_DIFF,
            ),
        ];
        for (git, outcome, cancelled, att, want) in cases {
            let r = on_turn_end(Some(&rec), git, outcome, cancelled, att, root).unwrap();
            assert_eq!(r.status, HookStatus::Skipped);
            assert_eq!(r.reason.as_deref(), Some(want));
        }
        assert!(rec.calls().is_empty());
    }

    #[test]
    fn a_failed_binding_check_skips_without_invoking_the_runner() {
        // Not a git repository (a fresh temp dir): HEAD cannot resolve, so the check fails closed.
        let dir = crate::testutil::temp_dir("hook-nogit");
        let rec = Recording::new();
        let att = attestable(dummy_served());
        let r = on_turn_end(
            Some(&rec),
            true,
            Some(Outcome::Converged),
            false,
            Ok(&att),
            dir.as_path(),
        )
        .unwrap();
        assert_eq!(r.status, HookStatus::Skipped);
        assert!(r
            .reason
            .as_deref()
            .unwrap()
            .starts_with(reason::BINDING_CHECK_FAILED));
        assert!(rec.calls().is_empty());
    }

    #[test]
    fn the_payload_carries_the_requested_shape_and_no_reviewer_prose() {
        let att = attestable(dummy_served());
        let head = "b".repeat(40);
        let v: serde_json::Value =
            serde_json::from_str(&payload(&att, &head, Trigger::Attest, Path::new("C:\\r")))
                .unwrap();
        assert_eq!(v["outcome"], "converged");
        assert_eq!(v["event"], "converged");
        assert_eq!(v["trigger"], "attest");
        assert_eq!(v["tree_clean"], true);
        assert_eq!(v["captured_head"], head);
        assert_eq!(v["session"], "feat/x");
        assert_eq!(v["review_id"], "rv-1-1");
        assert_eq!(v["chain_index"], 0);
        assert_eq!(v["reviewer_kind"], "codex");
        assert_eq!(v["model"], "gpt-5.6-luna");
        assert_eq!(v["base"], "a".repeat(40));
        assert_eq!(v["diff_sha256"], "00".repeat(32));
        assert_eq!(v["repo_root"], "C:\\r");
        assert_eq!(v["schema_version"], PAYLOAD_SCHEMA_VERSION);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        for forbidden in ["findings", "review_prose", "verdict", "warnings"] {
            assert!(!keys.iter().any(|k| *k == forbidden), "{forbidden}");
        }
    }

    #[test]
    fn summaries_read_as_one_line() {
        let mut ok = HookReport::with(HookStatus::Succeeded, None);
        ok.exit_code = Some(0);
        ok.captured_head = Some("1a2b3c4d5e6f7a8b9c0d".into());
        assert_eq!(ok.summary(), "succeeded (exit 0, head 1a2b3c4d5e6f)");
        let skip = HookReport::skipped(reason::HEAD_MISMATCH);
        assert!(skip
            .summary()
            .starts_with("skipped (head_does_not_match_review: "));
        assert_eq!(
            HookReport::with(HookStatus::TimedOut, None).summary(),
            "timed_out"
        );
        assert!(HookReport::failed(reason::NO_CONTAINMENT).is_failure());
        assert!(!HookReport::skipped(reason::NOT_GIT).is_failure());
    }

    // ---- the process runner, against real cmd.exe stand-ins ----
    //
    // Each hook body is written to a `.cmd` file and run as `cmd /d /c <file>`, rather than passed
    // inline: Rust quotes an argument's embedded `"` as `\"`, which cmd does not understand, so any
    // inline script with a quoted path or a nested command breaks on quoting rather than on the
    // behaviour under test.

    /// Write `name` into `dir` with `body`, and return a hook config that runs it.
    fn script(dir: &Path, name: &str, body: &str, timeout: Duration) -> HookConfig {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("@echo off\r\n{}\r\n", body.replace('\n', "\r\n")),
        )
        .unwrap();
        HookConfig {
            program: cmd_exe(),
            args: vec![
                "/d".into(),
                "/c".into(),
                path.to_string_lossy().into_owned(),
            ],
            timeout,
        }
    }

    #[test]
    fn the_payload_arrives_on_stdin_and_stdout_is_captured_not_inherited() {
        let dir = crate::testutil::temp_dir("hook-stdin");
        let out = dir.as_path().join("payload.json");
        // `findstr "^"` copies stdin through; the echo goes to the hook's stdout, which must land
        // in the report rather than on this process's stdout (the MCP channel in production).
        let cfg = script(
            dir.as_path(),
            "hook.cmd",
            &format!(
                "findstr \"^\" > \"{}\"\necho hello-from-hook",
                out.display()
            ),
            Duration::from_secs(20),
        );
        let r = run_process(&cfg, dir.as_path(), "{\"k\":1}", JobObject::new);
        assert_eq!(r.status, HookStatus::Succeeded, "{r:?}");
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "{\"k\":1}");
        assert!(r.output.as_deref().unwrap().contains("hello-from-hook"));
    }

    #[test]
    fn a_nonzero_exit_is_failed_with_its_code() {
        let dir = crate::testutil::temp_dir("hook-exit");
        let cfg = script(
            dir.as_path(),
            "hook.cmd",
            "exit /b 3",
            Duration::from_secs(20),
        );
        let r = run_process(&cfg, dir.as_path(), "{}", JobObject::new);
        assert_eq!(r.status, HookStatus::Failed);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.is_failure());
    }

    #[test]
    fn no_containment_never_launches_the_program() {
        let dir = crate::testutil::temp_dir("hook-nojob");
        let marker = dir.as_path().join("ran.txt");
        let cfg = script(
            dir.as_path(),
            "hook.cmd",
            &format!("echo x > \"{}\"", marker.display()),
            Duration::from_secs(20),
        );
        let r = run_process(&cfg, dir.as_path(), "{}", || None);
        assert_eq!(r.status, HookStatus::Failed);
        assert_eq!(r.reason.as_deref(), Some(reason::NO_CONTAINMENT));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists());
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure() {
        let dir = crate::testutil::temp_dir("hook-missing");
        let cfg = HookConfig {
            program: dir.as_path().join("does-not-exist.exe"),
            args: vec![],
            timeout: Duration::from_secs(5),
        };
        let r = run_process(&cfg, dir.as_path(), "{}", JobObject::new);
        assert_eq!(r.status, HookStatus::Failed);
        assert!(r.reason.unwrap().starts_with(reason::SPAWN_FAILED));
    }

    /// A hook whose direct process starts `helper.cmd` in the background and exits at once. The
    /// helper sleeps about `secs` seconds (ping), then writes `marker`.
    fn backgrounded(dir: &Path, marker: &Path, secs: u32, timeout: Duration) -> HookConfig {
        script(
            dir,
            "helper.cmd",
            &format!(
                "ping -n {} 127.0.0.1 >nul\necho done > \"{}\"",
                secs + 1,
                marker.display()
            ),
            timeout,
        );
        let helper = dir.join("helper.cmd");
        script(
            dir,
            "hook.cmd",
            &format!(
                "start \"\" /b cmd /d /c \"{}\"\nexit /b 0",
                helper.display()
            ),
            timeout,
        )
    }

    /// f5: the direct process exits 0 while a helper it started is still working. The runner waits
    /// for the helper, so its side effect exists before `succeeded` is reported.
    #[test]
    fn success_waits_for_descendants_to_finish() {
        let dir = crate::testutil::temp_dir("hook-quiesce");
        let marker = dir.as_path().join("posted.txt");
        let cfg = backgrounded(dir.as_path(), &marker, 2, Duration::from_secs(30));
        let r = run_process(&cfg, dir.as_path(), "{}", JobObject::new);
        assert_eq!(r.status, HookStatus::Succeeded, "{r:?}");
        assert!(
            marker.exists(),
            "succeeded was reported before the helper finished"
        );
    }

    /// f5, the other side: a helper that outlives the timeout makes the hook fail `not_quiesced`,
    /// and it is killed rather than left to act later.
    #[test]
    fn a_lingering_descendant_is_not_success_and_is_killed() {
        let dir = crate::testutil::temp_dir("hook-linger");
        let marker = dir.as_path().join("late.txt");
        let cfg = backgrounded(dir.as_path(), &marker, 5, Duration::from_secs(2));
        let r = run_process(&cfg, dir.as_path(), "{}", JobObject::new);
        assert_eq!(r.status, HookStatus::Failed, "{r:?}");
        assert_eq!(r.reason.as_deref(), Some(reason::NOT_QUIESCED));
        assert_eq!(r.exit_code, Some(0));
        std::thread::sleep(Duration::from_secs(6));
        assert!(
            !marker.exists(),
            "the lingering helper survived termination"
        );
    }

    #[test]
    fn a_hook_that_outlives_its_timeout_is_timed_out_and_killed() {
        let dir = crate::testutil::temp_dir("hook-timeout");
        let marker = dir.as_path().join("late.txt");
        let cfg = script(
            dir.as_path(),
            "hook.cmd",
            &format!(
                "ping -n 6 127.0.0.1 >nul\necho late > \"{}\"",
                marker.display()
            ),
            Duration::from_secs(1),
        );
        let started = Instant::now();
        let r = run_process(&cfg, dir.as_path(), "{}", JobObject::new);
        assert_eq!(r.status, HookStatus::TimedOut, "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_secs(6));
        assert!(!marker.exists());
    }

    /// f4: several MiB on both streams completes without deadlock, and only a bounded tail is kept.
    #[test]
    fn a_chatty_hook_does_not_deadlock_and_its_output_is_bounded() {
        let dir = crate::testutil::temp_dir("hook-chatty");
        let line = "0123456789".repeat(10);
        // ~3 MiB to each stream: well past any pipe buffer.
        let cfg = script(
            dir.as_path(),
            "hook.cmd",
            &format!("for /l %%i in (1,1,30000) do (echo {line}\necho err{line} 1>&2\n)"),
            Duration::from_secs(120),
        );
        let r = run_process(&cfg, dir.as_path(), "{}", JobObject::new);
        assert_eq!(r.status, HookStatus::Succeeded, "{:?}", r.reason);
        let output = r.output.unwrap();
        assert!(output.len() <= OUTPUT_TAIL_BYTES);
        assert!(output.contains("err0123"));
    }
}
