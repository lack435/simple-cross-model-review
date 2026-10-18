//! The lifecycle event hook (issue #152): an opt-in program (`--on-event`) run with a JSON envelope
//! on stdin for every review and consult turn that starts, completes, fails or is cancelled, so a
//! tool outside the calling MCP client -- a dashboard aggregating several machines -- can follow
//! every review, whichever client started it.
//!
//! It is the general sibling of `--on-converged` ([`crate::hook`]) and reuses its contained runner,
//! with one difference that matters: this hook is **fire and forget**. Events go onto a bounded
//! queue drained by one worker thread, which runs the hook once per event, in order, each under its
//! timeout. Emitting never blocks a review, and the server never waits on the hook or reads its
//! output: a hook that fails, hangs or exits non-zero is logged to stderr and otherwise ignored. A
//! full queue drops the event with a stderr line rather than wait.
//!
//! It is trusted code, as `--on-converged` is: it runs as the server's user, unsandboxed. It runs in
//! the state directory rather than the repository, but nothing stops a hook from touching the tree
//! a review is reading -- the guarantee is about the server's behaviour, not the hook's.
//!
//! The envelope is built only from server state. Finding titles and details, which can quote source
//! code, are included only with `--on-event-include-findings`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::errors::Failure;
use crate::hook::HookConfig;
use crate::registry::{JobKind, Outcome};

/// The envelope's own schema version.
pub const SCHEMA_VERSION: u32 = 1;
/// Default `--on-event-timeout-seconds`: shorter than the converged hook's, because events are
/// sequential and one slow event delays the next.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
/// Events waiting for the hook. A burst larger than this means the hook is far slower than the
/// reviews it describes; dropping is better than holding memory for a consumer that is not keeping up.
const QUEUE_SLOTS: usize = 256;

/// The configured event hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventConfig {
    pub hook: HookConfig,
    pub include_findings: bool,
}

/// The reviewer entry an event describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub cli: String,
    pub model: String,
    pub effort: String,
    /// The `--reviewer` chain index; `0` is the primary.
    pub chain_index: usize,
    /// Whether the turn fell through to this entry from the one it started on.
    pub fallback: bool,
}

/// What every event of one turn shares, fixed when the turn is registered.
#[derive(Clone, Debug)]
pub struct Turn {
    pub kind: JobKind,
    pub review_id: String,
    pub session: String,
    pub turn: u32,
    pub resumed: bool,
    /// The root this turn reviews: the server's working root, or the worktree the call named.
    pub root: PathBuf,
    /// The `level` the call asked for, if any.
    pub level: Option<String>,
    pub started_at: SystemTime,
    /// The entry the turn starts on.
    pub entry: Entry,
}

/// The queue end of the hook's worker. Cheap to share: emitting is a `try_send`.
pub struct EventSink {
    tx: SyncSender<String>,
    include_findings: bool,
    /// Events queued or running, so shutdown can wait for them (see [`drain`](Self::drain)).
    pending: Arc<AtomicUsize>,
    /// The status log every event is appended to, synchronously at emission (see [`send`]).
    /// `None` only in tests.
    ///
    /// [`send`]: Self::send
    log: Option<PathBuf>,
    /// Which turns are live, and whether shutdown has swept them. One lock, held across each
    /// terminal emission, so a turn's terminal event is claimed exactly once: either by the turn or
    /// by the shutdown sweep, never both and never neither (review f1 on #152).
    state: Mutex<Live>,
}

#[derive(Default)]
struct Live {
    turns: HashMap<String, Turn>,
    shut: bool,
}

/// The `error_code` of a turn the server's exit ended: the process does not wait for running
/// reviews, so they get no terminal event of their own.
pub const SERVER_EXITED: &str = "SERVER_EXITED";

impl EventSink {
    /// Start the worker that runs the hook in `cwd` -- the state directory, deliberately not a
    /// repository: an event consumer has no business in the reviewed tree. `None` if the thread
    /// cannot be spawned; the server then runs without events, as a failing hook would leave it.
    ///
    /// Each event is also appended to the state directory's status log ([`status_log_path`]),
    /// without finding text, for `--status --json`.
    pub fn spawn(cfg: EventConfig, cwd: PathBuf) -> Option<Self> {
        let log = status_log_path(&cwd);
        let mut sink = Self::spawn_with(cfg.include_findings, QUEUE_SLOTS, move |payload| {
            let dir = if cwd.is_dir() {
                cwd.clone()
            } else {
                std::env::temp_dir()
            };
            let report = crate::hook::run_process_with_flags(
                &cfg.hook,
                &dir,
                payload,
                crate::winjob::JobObject::new,
                crate::hook::CREATE_NO_WINDOW,
            );
            if report.is_failure() || report.status == crate::hook::HookStatus::Skipped {
                eprintln!("cross-review: --on-event hook {}", report.summary());
            }
        })?;
        sink.log = Some(log);
        Some(sink)
    }

    /// [`spawn`](Self::spawn) with the runner injected: a seam for tests, not a setting.
    fn spawn_with(
        include_findings: bool,
        slots: usize,
        run: impl Fn(&str) + Send + 'static,
    ) -> Option<Self> {
        let (tx, rx) = mpsc::sync_channel::<String>(slots);
        let pending = Arc::new(AtomicUsize::new(0));
        let worker_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("event-hook".into())
            .spawn(move || {
                for payload in rx {
                    run(&payload);
                    worker_pending.fetch_sub(1, Ordering::AcqRel);
                }
            })
            .ok()?;
        Some(Self {
            tx,
            include_findings,
            pending,
            log: None,
            state: Mutex::new(Live::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Live> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one event: appended to the status log here, on the emitting thread and under the
    /// state lock, then queued for the hook. Synchronous on purpose (review f3 on the status log):
    /// a turn's terminal event is emitted before its session lease is released, so the next turn
    /// -- in this process or another -- can only append after it, and file order is lifecycle
    /// order per session. Only the hook, which may be slow, runs asynchronously.
    fn send(&self, payload: Value) {
        if let Some(log) = &self.log {
            if let Err(e) = append_status(log, &payload) {
                eprintln!(
                    "cross-review: could not append to the status log {}: {e}",
                    log.display()
                );
            }
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        if let Err(e) = self.tx.try_send(payload.to_string()) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            if let TrySendError::Full(_) = e {
                eprintln!("cross-review: --on-event hook is not keeping up; an event was dropped")
            }
        }
    }

    /// At shutdown: report every turn still live as `turn_failed` with [`SERVER_EXITED`] -- the
    /// process does not wait for running reviews, so they would otherwise look running forever to a
    /// consumer -- then wait up to `max` for queued events to be delivered. After the sweep no
    /// event is emitted for a swept turn, so none is reported twice. Bounded: a hook that hangs
    /// costs the exit at most `max`, never more.
    pub fn drain(&self, max: Duration) {
        {
            let mut state = self.lock();
            state.shut = true;
            for (_, turn) in state.turns.drain() {
                let mut value = envelope(&turn, "turn_failed", &turn.entry, Some(now()));
                value
                    .as_object_mut()
                    .expect("envelope is an object")
                    .insert("error_code".into(), json!(SERVER_EXITED));
                self.send(value);
            }
        }
        let deadline = Instant::now() + max;
        while self.pending.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The turn was registered and its worker is about to start.
    pub fn started(&self, turn: &Turn) {
        let mut state = self.lock();
        if state.shut {
            return;
        }
        state.turns.insert(turn.review_id.clone(), turn.clone());
        self.send(envelope(turn, "turn_started", &turn.entry, None));
    }

    /// The turn reached a terminal state with this outcome, served by `entry`.
    pub fn finished(&self, turn: &Turn, entry: &Entry, outcome: &Outcome) {
        let mut value = envelope(
            turn,
            event_for(outcome.failure.as_ref()),
            entry,
            Some(now()),
        );
        let object = value.as_object_mut().expect("envelope is an object");
        object.insert("denial_count".into(), json!(outcome.denial_count));
        match &outcome.failure {
            Some(failure) => {
                object.insert("warnings".into(), json!(outcome.warnings));
                object.insert("error_code".into(), json!(failure.code));
            }
            None => self.completed_fields(object, outcome),
        }
        self.terminal(turn, value);
    }

    /// The turn ended before a worker could produce an outcome (spawn failure, early cancel, panic).
    pub fn failed(&self, turn: &Turn, failure: &Failure) {
        let mut value = envelope(turn, event_for(Some(failure)), &turn.entry, Some(now()));
        value
            .as_object_mut()
            .expect("envelope is an object")
            .insert("error_code".into(), json!(failure.code));
        self.terminal(turn, value);
    }

    /// Claim and send a turn's terminal event, under the lock the shutdown sweep takes.
    fn terminal(&self, turn: &Turn, value: Value) {
        let mut state = self.lock();
        let was_live = state.turns.remove(&turn.review_id).is_some();
        if state.shut && !was_live {
            return;
        }
        self.send(value);
    }

    fn completed_fields(&self, object: &mut Map<String, Value>, outcome: &Outcome) {
        // A consult has no findings envelope: the review fields stay null.
        let Some(env) = &outcome.envelope else {
            object.insert("warnings".into(), json!(outcome.warnings));
            return;
        };
        let mut by_severity = json!({"critical": 0, "major": 0, "minor": 0});
        for f in env.findings.iter().filter(|f| f.status.is_open()) {
            if let Some(key) = serde_json::to_value(f.severity)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
            {
                if let Some(n) = by_severity.get_mut(&key) {
                    *n = json!(n.as_u64().unwrap_or(0) + 1);
                }
            }
        }
        // The same union the completed result carries (review f3 on #152).
        object.insert(
            "warnings".into(),
            json!(crate::findings::warnings_with(env, &outcome.warnings)),
        );
        object.insert("outcome".into(), json!(env.outcome));
        object.insert("verdict".into(), json!(env.verdict));
        object.insert("structured".into(), json!(env.structured));
        object.insert("open_count".into(), json!(env.open_count));
        object.insert("total_count".into(), json!(env.total_count));
        object.insert("open_by_severity".into(), by_severity);
        if self.include_findings {
            object.insert("findings".into(), json!(env.findings));
        }
    }
}

/// The status log in `state_dir`: one JSON envelope per line, named for the machine like the usage
/// log, so logs gathered from several machines do not collide.
pub fn status_log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(format!("status-{}.jsonl", crate::metrics::host_tag()))
}

/// Append one envelope to the status log, with finding text removed whatever the hook is sent.
/// Under the same cross-process lock pattern the usage log uses, so servers sharing a state
/// directory never interleave a line.
fn append_status(log: &Path, payload: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut value = payload.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("findings".into(), Value::Null);
    }
    let mut line = value.to_string();
    line.push('\n');
    let _lock = crate::session::ExclusiveLock::acquire(
        &log.with_extension("jsonl.lock"),
        Duration::from_secs(2),
    )?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?
        .write_all(line.as_bytes())
}

/// `cross-review --status --json`: the latest envelope of each session, as a JSON array sorted by
/// session name. A session's latest status-log record wins; a saved session with none (it predates
/// `--on-event`, or the hook was not configured) gets a minimal envelope from its session record,
/// with `event` null. Read-only: nothing is created or written.
pub fn status_report(state_dir: &Path) -> String {
    let mut latest: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(status_log_path(state_dir)) {
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(session) = value.get("session").and_then(Value::as_str) {
                latest.insert(session.to_string(), value);
            }
        }
    }
    for (name, record) in crate::session::SessionStore::new(state_dir).list() {
        latest
            .entry(name.clone())
            .or_insert_with(|| record_envelope(&name, &record));
    }
    let all: Vec<Value> = latest.into_values().collect();
    serde_json::to_string_pretty(&all).unwrap_or_else(|_| "[]".into())
}

/// The envelope shape for a session known only from its record: what the record holds is filled in
/// (the last turn number, reviewer, kind, root, and when it was last updated), the rest is null.
fn record_envelope(name: &str, record: &crate::session::SessionRecord) -> Value {
    let turn = Turn {
        kind: if record.kind.as_deref() == Some(crate::session::KIND_CONSULT) {
            JobKind::Consult
        } else {
            JobKind::Review
        },
        review_id: String::new(),
        session: name.to_string(),
        turn: record.turns,
        resumed: record.turns > 1,
        root: PathBuf::from(&record.cwd),
        level: None,
        started_at: UNIX_EPOCH + Duration::from_secs(record.updated_unix),
        entry: Entry {
            cli: record.reviewer.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            chain_index: 0,
            fallback: false,
        },
    };
    let mut value = envelope(
        &turn,
        "",
        &turn.entry,
        Some(UNIX_EPOCH + Duration::from_secs(record.updated_unix)),
    );
    if let Some(object) = value.as_object_mut() {
        object.insert("event".into(), Value::Null);
        object.insert("review_id".into(), Value::Null);
        object.insert("started_at".into(), Value::Null);
        object.insert("resumed".into(), Value::Null);
        object.insert("reviewer".into(), {
            let mut r = object["reviewer"].clone();
            r["chain_index"] = Value::Null;
            r["fallback"] = Value::Null;
            r
        });
    }
    value
}

fn event_for(failure: Option<&Failure>) -> &'static str {
    match failure {
        None => "turn_completed",
        Some(f) if f.code == crate::errors::cancelled().code => "turn_cancelled",
        Some(_) => "turn_failed",
    }
}

/// The fields every event carries. Fields an event does not apply to are present and `null`, so a
/// consumer can rely on the shape.
fn envelope(turn: &Turn, event: &str, entry: &Entry, finished: Option<SystemTime>) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "event": event,
        "kind": match turn.kind {
            JobKind::Review => "review",
            JobKind::Consult => "consult",
        },
        "review_id": turn.review_id,
        "session": turn.session,
        "turn": turn.turn,
        "resumed": turn.resumed,
        "working_root": turn.root.to_string_lossy(),
        "level": turn.level,
        "reviewer": {
            "cli": entry.cli,
            "model": entry.model,
            "effort": entry.effort,
            "chain_index": entry.chain_index,
            "fallback": entry.fallback,
        },
        "started_at": rfc3339(turn.started_at),
        "finished_at": finished.map(rfc3339),
        "outcome": null,
        "verdict": null,
        "structured": null,
        "open_count": null,
        "total_count": null,
        "open_by_severity": null,
        "warnings": [],
        "denial_count": null,
        "error_code": null,
        // Only ever an array with `--on-event-include-findings`, on a completed review.
        "findings": null,
        "host": std::env::var("COMPUTERNAME").ok(),
        "server_version": env!("CARGO_PKG_VERSION"),
    })
}

fn now() -> SystemTime {
    SystemTime::now()
}

/// `t` as an RFC 3339 UTC timestamp with millisecond precision, e.g. `2026-10-17T21:04:05.123Z`.
/// Hand-rolled because the crate keeps `serde` as its only dependency.
pub fn rfc3339(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn turn() -> Turn {
        Turn {
            kind: JobKind::Review,
            review_id: "rv-1-1".into(),
            session: "s".into(),
            turn: 2,
            resumed: true,
            root: PathBuf::from(r"C:\repo"),
            level: Some("fast".into()),
            started_at: UNIX_EPOCH + Duration::from_millis(1_760_735_045_123),
            entry: Entry {
                cli: "codex".into(),
                model: "m".into(),
                effort: "high".into(),
                chain_index: 0,
                fallback: false,
            },
        }
    }

    type Seen = Arc<Mutex<Vec<Value>>>;

    fn recording(include_findings: bool) -> (EventSink, Seen) {
        let seen: Seen = Arc::default();
        let sink_seen = Arc::clone(&seen);
        let sink = EventSink::spawn_with(include_findings, 4, move |payload| {
            sink_seen
                .lock()
                .unwrap()
                .push(serde_json::from_str(payload).unwrap());
        })
        .unwrap();
        (sink, seen)
    }

    fn wait_for(seen: &Seen, n: usize) -> Vec<Value> {
        for _ in 0..200 {
            if seen.lock().unwrap().len() >= n {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        seen.lock().unwrap().clone()
    }

    /// The status log keeps the latest envelope per session without finding text, and
    /// `--status --json` reports it, filling sessions with no record from the session store.
    #[test]
    fn the_status_report_shows_each_sessions_latest_event() {
        let dir = crate::testutil::temp_dir("events-status");
        let log = status_log_path(dir.as_path());
        let mut t = turn();
        let started = envelope(&t, "turn_started", &t.entry, None);
        append_status(&log, &started).unwrap();
        let mut done = envelope(&t, "turn_completed", &t.entry, Some(now()));
        done["findings"] = json!([{"title": "secret source"}]);
        append_status(&log, &done).unwrap();
        t.session = "other".into();
        append_status(&log, &envelope(&t, "turn_started", &t.entry, None)).unwrap();
        std::fs::write(
            log.with_file_name("ignored.jsonl"),
            "not part of the report\n",
        )
        .unwrap();

        let report: Vec<Value> = serde_json::from_str(&status_report(dir.as_path())).unwrap();
        assert_eq!(report.len(), 2);
        assert_eq!(report[0]["session"], "other");
        assert_eq!(report[0]["event"], "turn_started");
        assert_eq!(report[1]["session"], "s");
        assert_eq!(report[1]["event"], "turn_completed");
        assert!(
            report[1]["findings"].is_null(),
            "finding text never reaches the log"
        );
        // A missing state directory is an empty report, not an error.
        assert_eq!(status_report(&dir.as_path().join("absent")), "[]");
    }

    /// A saved session with no recorded event is still listed, from its session record.
    #[test]
    fn a_session_without_events_gets_a_minimal_record() {
        let dir = crate::testutil::temp_dir("events-status-record");
        let store = crate::session::SessionStore::new(dir.as_path());
        crate::session::tests::record(&store, "only-saved", "thread-1");
        let report: Vec<Value> = serde_json::from_str(&status_report(dir.as_path())).unwrap();
        assert_eq!(report.len(), 1);
        let r = &report[0];
        assert_eq!(r["session"], "only-saved");
        assert!(r["event"].is_null() && r["review_id"].is_null());
        assert_eq!(r["turn"], 1);
        assert_eq!(r["kind"], "review");
        assert_eq!(r["reviewer"]["cli"], "codex");
        assert_eq!(r["working_root"], r"C:\repo");
        assert!(r["finished_at"].is_string());
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_millis(1_760_735_045_123)),
            "2025-10-17T21:04:05.123Z"
        );
        // A leap day.
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn events_arrive_in_order_with_a_stable_shape() {
        let (sink, seen) = recording(false);
        let t = turn();
        sink.started(&t);
        sink.failed(&t, &crate::errors::cancelled());
        sink.failed(&t, &crate::errors::worker_panicked("rv-1-1"));
        let events = wait_for(&seen, 3);
        let names: Vec<_> = events
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["turn_started", "turn_cancelled", "turn_failed"]);
        let first = &events[0];
        assert_eq!(first["schema_version"], 1);
        assert_eq!(first["kind"], "review");
        assert_eq!(first["turn"], 2);
        assert_eq!(first["level"], "fast");
        assert_eq!(first["reviewer"]["cli"], "codex");
        assert_eq!(first["started_at"], "2025-10-17T21:04:05.123Z");
        assert!(first["finished_at"].is_null() && first["outcome"].is_null());
        // Every event carries the same keys.
        let keys = |v: &Value| {
            let mut k: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert!(events.iter().all(|e| keys(e) == keys(first)));
        assert_eq!(
            events[2]["error_code"],
            crate::errors::worker_panicked("rv-1-1").code
        );
        assert!(events[2]["finished_at"].is_string());
    }

    /// A turn the server's exit ends is reported, once; one that already ended is not.
    #[test]
    fn shutdown_reports_the_turns_it_ends() {
        let (sink, seen) = recording(false);
        let ended = turn();
        let mut running = turn();
        running.review_id = "rv-1-2".into();
        sink.started(&ended);
        sink.started(&running);
        sink.failed(&ended, &crate::errors::cancelled());
        sink.drain(Duration::from_secs(5));
        let events = wait_for(&seen, 4);
        assert_eq!(events.len(), 4);
        let last = &events[3];
        assert_eq!(last["event"], "turn_failed");
        assert_eq!(last["review_id"], "rv-1-2");
        assert_eq!(last["error_code"], SERVER_EXITED);
    }

    /// A turn that finishes after the shutdown sweep reported it is not reported again.
    #[test]
    fn a_turn_finishing_after_the_sweep_is_reported_once() {
        let (sink, seen) = recording(false);
        let t = turn();
        sink.started(&t);
        sink.drain(Duration::from_secs(5));
        sink.failed(&t, &crate::errors::cancelled());
        sink.started(&t);
        sink.drain(Duration::from_secs(5));
        let events = wait_for(&seen, 2);
        let names: Vec<_> = events
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["turn_started", "turn_failed"]);
        assert_eq!(events[1]["error_code"], SERVER_EXITED);
    }

    #[test]
    fn a_hook_that_does_not_keep_up_drops_events_instead_of_blocking() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let worker_gate = Arc::clone(&gate);
        let sink = EventSink::spawn_with(false, 1, move |_| {
            let _wait = worker_gate.lock().unwrap();
        })
        .unwrap();
        let t = turn();
        let begun = std::time::Instant::now();
        for _ in 0..50 {
            sink.started(&t);
        }
        assert!(
            begun.elapsed() < Duration::from_secs(1),
            "emitting must not block"
        );
        // The bounded drain gives up rather than wait on a hook that is stuck.
        let draining = std::time::Instant::now();
        sink.drain(Duration::from_millis(100));
        assert!(draining.elapsed() < Duration::from_secs(1));
        drop(held);
        sink.drain(Duration::from_secs(5));
        assert_eq!(sink.pending.load(Ordering::Acquire), 0);
    }
}
