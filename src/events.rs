//! The lifecycle event hook (issue #152): an opt-in program (`--on-event`) run with a JSON envelope
//! on stdin for every review and consult turn that starts, completes, fails or is cancelled, so a
//! tool outside the calling MCP client -- a dashboard aggregating several machines -- can follow
//! every review, whichever client started it.
//!
//! It is the general sibling of `--on-converged` ([`crate::hook`]) and reuses its contained runner,
//! with one difference that matters: this hook is **fire and forget**. Events go onto a bounded
//! queue drained by one worker thread, which runs the hook once per event, in order, each under its
//! timeout. Emitting never blocks a review, and nothing the hook does -- failing, hanging, exiting
//! non-zero -- reaches a review; it is logged to stderr and otherwise ignored. A full queue drops
//! the event with a stderr line rather than wait.
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
    tx: SyncSender<(PathBuf, String)>,
    include_findings: bool,
    /// Events queued or running, so shutdown can wait for them (see [`drain`](Self::drain)).
    pending: Arc<AtomicUsize>,
    /// Turns that have started and not yet ended, so a shutdown that kills them can still say so.
    live: Mutex<HashMap<String, Turn>>,
}

/// The `error_code` of a turn the server's exit ended: the process does not wait for running
/// reviews, so they get no terminal event of their own.
pub const SERVER_EXITED: &str = "SERVER_EXITED";

impl EventSink {
    /// Start the worker that runs the hook. `None` if the thread cannot be spawned; the server then
    /// runs without events, which is what a hook failure would mean anyway.
    pub fn spawn(cfg: EventConfig) -> Option<Self> {
        Self::spawn_with(cfg.include_findings, QUEUE_SLOTS, move |root, payload| {
            let report = crate::hook::run_process_with_flags(
                &cfg.hook,
                root,
                payload,
                crate::winjob::JobObject::new,
                crate::hook::CREATE_NO_WINDOW,
            );
            if report.is_failure() || report.status == crate::hook::HookStatus::Skipped {
                eprintln!("cross-review: --on-event hook {}", report.summary());
            }
        })
    }

    /// [`spawn`](Self::spawn) with the runner injected: a seam for tests, not a setting.
    fn spawn_with(
        include_findings: bool,
        slots: usize,
        run: impl Fn(&Path, &str) + Send + 'static,
    ) -> Option<Self> {
        let (tx, rx) = mpsc::sync_channel::<(PathBuf, String)>(slots);
        let pending = Arc::new(AtomicUsize::new(0));
        let worker_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("event-hook".into())
            .spawn(move || {
                for (root, payload) in rx {
                    run(&root, &payload);
                    worker_pending.fetch_sub(1, Ordering::AcqRel);
                }
            })
            .ok()?;
        Some(Self {
            tx,
            include_findings,
            pending,
            live: Mutex::new(HashMap::new()),
        })
    }

    fn send(&self, root: &Path, payload: Value) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        match self.tx.try_send((root.to_path_buf(), payload.to_string())) {
            Ok(()) => {}
            Err(e) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                if let TrySendError::Full(_) = e {
                    eprintln!(
                        "cross-review: --on-event hook is not keeping up; an event was dropped"
                    )
                }
            }
        }
    }

    /// At shutdown: report every turn still live as `turn_failed` with [`SERVER_EXITED`] -- the
    /// process does not wait for running reviews, so they would otherwise look running forever to a
    /// consumer -- then wait up to `max` for queued events to be delivered. Bounded: a hook that
    /// hangs costs the exit at most `max`, never more.
    pub fn drain(&self, max: Duration) {
        let orphans: Vec<Turn> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, t)| t)
            .collect();
        for turn in orphans {
            let mut value = envelope(&turn, "turn_failed", &turn.entry, Some(now()));
            value
                .as_object_mut()
                .expect("envelope is an object")
                .insert("error_code".into(), json!(SERVER_EXITED));
            self.send(&turn.root, value);
        }
        let deadline = Instant::now() + max;
        while self.pending.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The turn was registered and its worker started.
    pub fn started(&self, turn: &Turn) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(turn.review_id.clone(), turn.clone());
        self.send(
            &turn.root,
            envelope(turn, "turn_started", &turn.entry, None),
        );
    }

    /// The turn reached a terminal state with this outcome, served by `entry`.
    pub fn finished(&self, turn: &Turn, entry: &Entry, outcome: &Outcome) {
        self.ended(turn);
        let mut value = envelope(
            turn,
            event_for(outcome.failure.as_ref()),
            entry,
            Some(now()),
        );
        let object = value.as_object_mut().expect("envelope is an object");
        object.insert("warnings".into(), json!(outcome.warnings));
        object.insert("denial_count".into(), json!(outcome.denial_count));
        match &outcome.failure {
            Some(failure) => {
                object.insert("error_code".into(), json!(failure.code));
            }
            None => self.completed_fields(object, outcome),
        }
        self.send(&turn.root, value);
    }

    /// The turn ended before a worker could produce an outcome (spawn failure, early cancel, panic).
    pub fn failed(&self, turn: &Turn, failure: &Failure) {
        self.ended(turn);
        let mut value = envelope(turn, event_for(Some(failure)), &turn.entry, Some(now()));
        value
            .as_object_mut()
            .expect("envelope is an object")
            .insert("error_code".into(), json!(failure.code));
        self.send(&turn.root, value);
    }

    fn ended(&self, turn: &Turn) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&turn.review_id);
    }

    fn completed_fields(&self, object: &mut Map<String, Value>, outcome: &Outcome) {
        // A consult has no findings envelope: the review fields stay null.
        let Some(env) = &outcome.envelope else {
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
        let sink = EventSink::spawn_with(include_findings, 4, move |_, payload| {
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

    #[test]
    fn a_hook_that_does_not_keep_up_drops_events_instead_of_blocking() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let worker_gate = Arc::clone(&gate);
        let sink = EventSink::spawn_with(false, 1, move |_, _| {
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
