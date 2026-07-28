//! The four tools, and the state they share.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::config::{Config, MAX_WAIT_SECS};
use crate::errors::{self, Failure};
use crate::prompt::{self, PromptParts, DEFAULT_PREAMBLE};
use crate::registry::{Outcome, Registry, Snapshot, Status};
use crate::reviewer::{self, Reviewer};
use crate::session::{self, now_unix, ExclusiveLock, SessionStore};

/// How long to wait for another server process to release a named session.
const SESSION_LEASE_WAIT: Duration = Duration::from_secs(3);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_SESSION: &str = "default";

#[derive(Clone)]
struct Preflight {
    bin: PathBuf,
    auth: String,
}

pub struct App {
    cfg: Arc<Config>,
    reviewer: Arc<dyn Reviewer>,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    /// Successful preflights are cached for the process lifetime; failures are not, so
    /// a user who runs `codex login` can retry without restarting the agent session.
    preflight: Mutex<Option<Preflight>>,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let sessions = SessionStore::new(&cfg.state_dir);
        let reviewer = reviewer::for_kind(cfg.reviewer);
        Self {
            cfg: Arc::new(cfg),
            reviewer: Arc::from(reviewer),
            registry: Arc::new(Registry::new()),
            sessions: Arc::new(sessions),
            preflight: Mutex::new(None),
        }
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    fn ensure_ready(&self) -> Result<Preflight, Failure> {
        if let Some(cached) = self
            .preflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(cached);
        }
        let bin = reviewer::resolve_bin(&self.cfg)?;
        let auth = self.reviewer.auth_check(&bin)?;
        let ready = Preflight { bin, auth };
        *self.preflight.lock().unwrap_or_else(|e| e.into_inner()) = Some(ready.clone());
        Ok(ready)
    }

    // -----------------------------------------------------------------------
    // cross_model_review
    // -----------------------------------------------------------------------

    pub fn start_review(&self, args: &Value) -> Result<String, Failure> {
        let instructions = string_arg(args, "instructions")
            .ok_or_else(|| errors::bad_request("'instructions' is required and must be a non-empty string describing what to review."))?;

        let session = string_arg(args, "session").unwrap_or_else(|| DEFAULT_SESSION.to_string());
        let fresh = args.get("fresh").and_then(Value::as_bool).unwrap_or(false);
        let context_paths = string_array_arg(args, "context_paths");

        let ready = self.ensure_ready()?;

        // The lease comes first, before the session record is read. Reading first would
        // be a stale-read race: another server process can finish a `fresh` review, or
        // rebind an expired session to a new id, while we are still waiting for the
        // lease -- and we would then resume the old id and overwrite the newer mapping.
        // Holding the lease across the read makes the state we act on the current state.
        let lease = ExclusiveLock::acquire(
            &session::session_lock_path(&self.cfg.state_dir, &session),
            SESSION_LEASE_WAIT,
        )
        .map_err(|e| errors::session_leased(&session, e.to_string()))?;

        // Decide whether this is a new review or another turn on an existing one.
        let prior = if fresh {
            None
        } else {
            self.sessions.get(&session)
        };
        let prior = prior.filter(|record| {
            // A session belongs to the reviewer that created it. If the project's
            // configuration now points at a different CLI or model, start over rather
            // than trying to resume someone else's conversation.
            record.reviewer == self.cfg.reviewer.as_str() && record.model == self.cfg.model
        });

        let (resume_id, turn, resumed) = match &prior {
            Some(record) => (
                Some(record.cli_session_id.clone()),
                record.turns.saturating_add(1),
                true,
            ),
            None => (None, 1, false),
        };

        // Claiming the session and registering the review are one atomic step, so two
        // concurrent calls cannot both start a review against the same conversation.
        let (id, cancel) = self
            .registry
            .try_start(&session, turn, resumed)
            .map_err(|existing| errors::session_busy(&session, &existing))?;

        let job = Job {
            cfg: Arc::clone(&self.cfg),
            reviewer: Arc::clone(&self.reviewer),
            registry: Arc::clone(&self.registry),
            sessions: Arc::clone(&self.sessions),
            bin: ready.bin,
            id: id.clone(),
            session: session.clone(),
            instructions,
            context_paths,
            turn,
            cancel,
            _lease: Some(lease),
        };

        let spawned = std::thread::Builder::new()
            .name(format!("review-{id}"))
            .spawn(move || job.run(resume_id));

        if let Err(e) = spawned {
            self.registry.finish(
                &id,
                Outcome::failed(errors::spawn_failed(
                    self.cfg.reviewer.as_str(),
                    "worker thread",
                    e.to_string(),
                )),
            );
        }

        let mut out = String::new();
        out.push_str("Review started. It runs in the background.\n\n");
        out.push_str(&format!("review_id: {id}\n"));
        out.push_str(&format!(
            "session:   {session} ({})\n",
            if resumed {
                format!("resumed, turn {turn}")
            } else {
                "new".to_string()
            }
        ));
        out.push_str(&format!("reviewer:  {}\n\n", self.cfg.describe_reviewer()));
        out.push_str(&format!(
            "Collect it with cross_model_review_result using review_id \"{id}\". That call waits \
             for the review rather than returning immediately, so one call with wait_seconds \
             set to 120 or more is usually enough. If it comes back with status=running, call it \
             again.\n\n\
             Reviews of substantial work commonly take one to several minutes.\n"
        ));
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // cross_model_review_result
    // -----------------------------------------------------------------------

    pub fn review_result(&self, args: &Value) -> Result<String, Failure> {
        let review_id = string_arg(args, "review_id");
        let session = string_arg(args, "session");

        let wait = args
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(crate::config::DEFAULT_WAIT_SECS)
            .min(MAX_WAIT_SECS);

        let id = match (&review_id, &session) {
            (Some(id), _) => {
                if !self.registry.exists(id) {
                    return Err(errors::bad_request(format!(
                        "No review with review_id '{id}' exists in this server process. Review \
                         ids do not survive a restart of the MCP server; start a new review \
                         instead."
                    )));
                }
                id.clone()
            }
            (None, Some(name)) => self.registry.latest_for_session(name).ok_or_else(|| {
                errors::bad_request(format!(
                    "No review has been started for session '{name}' in this server process. \
                     Call cross_model_review first."
                ))
            })?,
            (None, None) => {
                return Err(errors::bad_request(
                    "Provide either 'review_id' (preferred) or 'session'.",
                ))
            }
        };

        let snapshot = self
            .registry
            .wait(&id, Duration::from_secs(wait))
            .ok_or_else(|| errors::bad_request(format!("Review '{id}' is no longer tracked.")))?;

        match snapshot.status {
            Status::Running => Ok(self.render_running(&snapshot, wait)),
            Status::Completed => Ok(self.render_completed(&snapshot)),
            Status::Failed => Err(snapshot
                .failure
                .clone()
                .unwrap_or_else(|| errors::empty_review(self.cfg.reviewer.as_str(), ""))),
        }
    }

    fn render_running(&self, snapshot: &Snapshot, waited: u64) -> String {
        format!(
            "status:    {}\n\
             review_id: {}\n\
             session:   {} (turn {})\n\
             reviewer:  {}\n\
             elapsed:   {}s of a {}s budget\n\n\
             The reviewer is still working. This call waited {waited}s. Call \
             cross_model_review_result again with the same review_id to keep waiting.\n\n\
             Do not start a second review for this session, and do not proceed as though the \
             review had come back.\n",
            snapshot.status.as_str(),
            snapshot.id,
            snapshot.session,
            snapshot.turn,
            self.cfg.describe_reviewer(),
            snapshot.elapsed.as_secs(),
            self.cfg.timeout.as_secs(),
        )
    }

    fn render_completed(&self, snapshot: &Snapshot) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "status:    {}\n\
             review_id: {}\n\
             session:   {} ({})\n\
             reviewer:  {}\n\
             elapsed:   {}s\n\n",
            snapshot.status.as_str(),
            snapshot.id,
            snapshot.session,
            if snapshot.resumed {
                format!("turn {}, continuing an earlier review", snapshot.turn)
            } else {
                "turn 1, new review".to_string()
            },
            self.cfg.describe_reviewer(),
            snapshot.elapsed.as_secs(),
        ));

        for warning in &snapshot.warnings {
            out.push_str(&format!("WARNING: {warning}\n\n"));
        }

        if !snapshot.denials.is_empty() {
            out.push_str(&format!(
                "Note: the reviewer tried {} command(s) it was not permitted to run, so parts of \
                 its analysis may rest on less evidence than usual:\n",
                snapshot.denials.len()
            ));
            for denial in snapshot.denials.iter().take(10) {
                out.push_str(&format!("  - {denial}\n"));
            }
            out.push('\n');
        }

        out.push_str("--- BEGIN REVIEW ---\n");
        out.push_str(snapshot.review.as_deref().unwrap_or("(no review text)"));
        out.push_str("\n--- END REVIEW ---\n\n");

        out.push_str(
            "This is a second opinion from a different model, not a verdict you must obey. Act on \
             the findings you agree with. Where you think a finding is wrong, say so and explain \
             why rather than changing code you believe is correct.\n\n",
        );

        // Only promise continuity when it actually exists.
        if snapshot.resumable {
            out.push_str(&format!(
                "After you have addressed the feedback, call cross_model_review again with session \
                 \"{}\" to have the same reviewer re-check the work with its earlier findings still \
                 in context.\n",
                snapshot.session
            ));
        } else {
            out.push_str(&format!(
                "Note: this review was not saved as a resumable session, so calling \
                 cross_model_review with session \"{}\" again will start a fresh review that does \
                 not remember these findings. Include whatever context still matters in the new \
                 instructions.\n",
                snapshot.session
            ));
        }
        out
    }

    // -----------------------------------------------------------------------
    // cross_model_review_status
    // -----------------------------------------------------------------------

    pub fn status(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("cross-review {VERSION}\n\n"));
        out.push_str(&format!(
            "reviewer:      {}\n",
            self.cfg.describe_reviewer()
        ));

        match self.ensure_ready() {
            Ok(ready) => {
                out.push_str(&format!("cli:           {}\n", ready.bin.display()));
                out.push_str(&format!(
                    "auth:          {}\n",
                    ready.auth.replace('\n', " ")
                ));
                out.push_str("ready:         yes\n");
            }
            Err(failure) => {
                out.push_str("ready:         NO\n");
                out.push_str(&format!(
                    "problem:       {} - {}\n",
                    failure.code, failure.summary
                ));
            }
        }

        out.push_str(&format!("working root:  {}\n", self.cfg.cwd.display()));
        out.push_str(&format!("turn timeout:  {}s\n", self.cfg.timeout.as_secs()));
        out.push_str(&format!(
            "mcp isolation: {}\n",
            if self.cfg.isolate_mcp {
                "on (reviewer loads no MCP servers)"
            } else {
                "off"
            }
        ));
        out.push_str(&format!(
            "session state: {}\n",
            self.sessions.path().display()
        ));

        let sessions = self.sessions.list();
        out.push_str("\nsaved review sessions:\n");
        if sessions.is_empty() {
            out.push_str("  (none yet)\n");
        } else {
            let now = now_unix();
            for (name, record) in sessions {
                out.push_str(&format!(
                    "  {name}: {} turn(s), reviewer={} model={}, last used {}\n",
                    record.turns,
                    record.reviewer,
                    record.model,
                    fmt_age(now.saturating_sub(record.updated_unix)),
                ));
            }
        }

        out
    }

    // -----------------------------------------------------------------------
    // cross_model_review_cancel
    // -----------------------------------------------------------------------

    pub fn cancel(&self, args: &Value) -> Result<String, Failure> {
        let id = string_arg(args, "review_id")
            .ok_or_else(|| errors::bad_request("'review_id' is required."))?;
        if !self.registry.exists(&id) {
            return Err(errors::bad_request(format!(
                "No review with review_id '{id}' exists in this server process."
            )));
        }
        if self.registry.cancel(&id) {
            // Give the worker a moment to reap the child so the report is accurate.
            std::thread::sleep(Duration::from_millis(300));
            Ok(format!("Review '{id}' was cancelled. The reviewer process has been stopped and there is no review feedback.\n"))
        } else {
            Ok(format!("Review '{id}' had already finished; nothing to cancel. Collect it with cross_model_review_result.\n"))
        }
    }
}

// ---------------------------------------------------------------------------
// The background worker
// ---------------------------------------------------------------------------

struct Job {
    cfg: Arc<Config>,
    reviewer: Arc<dyn Reviewer>,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    bin: PathBuf,
    id: String,
    session: String,
    instructions: String,
    context_paths: Vec<String>,
    turn: u32,
    cancel: Arc<AtomicBool>,
    /// Cross-process claim on the named session. Never read: it exists so that dropping
    /// the job releases the session for other server processes.
    _lease: Option<ExclusiveLock>,
}

/// Records a failure if the worker thread unwinds before finishing.
///
/// Without this, a panic would leave the review `Running` for the life of the server:
/// the lease is released when the `Job` drops, but the registry entry never reaches a
/// terminal state, so every poll waits out its timeout and the session stays claimed.
struct FinishGuard<'a> {
    registry: &'a Registry,
    id: &'a str,
    armed: bool,
}

impl Drop for FinishGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.registry
                .finish(self.id, Outcome::failed(errors::worker_panicked(self.id)));
        }
    }
}

impl Job {
    fn run(self, resume_id: Option<String>) {
        let mut guard = FinishGuard {
            registry: &self.registry,
            id: &self.id,
            armed: true,
        };

        let outcome = match self.attempt(resume_id.as_deref(), self.turn) {
            Ok(outcome) => outcome,
            Err(failure) => {
                // A resume target that has expired is recoverable: drop the stale
                // mapping and review the work in a brand new session instead of
                // handing the caller a dead end.
                if failure.code == "SESSION_NOT_FOUND" && resume_id.is_some() {
                    self.sessions.forget(&self.session).ok();
                    eprintln!(
                        "cross-review: session '{}' could not be resumed; starting a fresh \
                         reviewer session",
                        self.session
                    );
                    match self.attempt(None, 1) {
                        Ok(mut outcome) => {
                            if let Some(review) = outcome.review.take() {
                                outcome.review = Some(format!(
                                    "NOTE: the previous review session had expired, so this is a \
                                     fresh review with no memory of earlier turns.\n\n{review}"
                                ));
                            }
                            outcome
                        }
                        Err(failure) => Outcome::failed(failure),
                    }
                } else {
                    Outcome::failed(failure)
                }
            }
        };
        self.registry.finish(&self.id, outcome);
        // Disarmed only once a terminal state is recorded, so the guard covers every
        // path that could unwind before this point.
        guard.armed = false;
    }

    fn attempt(&self, resume_id: Option<&str>, turn: u32) -> Result<Outcome, Failure> {
        let preamble = if self.cfg.no_preamble {
            None
        } else {
            Some(self.cfg.preamble.as_deref().unwrap_or(DEFAULT_PREAMBLE))
        };

        let text = prompt::build(&PromptParts {
            instructions: &self.instructions,
            context_paths: &self.context_paths,
            cwd: &self.cfg.cwd,
            turn,
            resumed: resume_id.is_some(),
            preamble,
        });

        let invocation = self
            .reviewer
            .invocation(&self.cfg, &self.bin, resume_id, &self.id)
            .map_err(|e| {
                errors::spawn_failed(
                    self.cfg.reviewer.as_str(),
                    &self.bin.display().to_string(),
                    e.to_string(),
                )
            })?;

        let last_message_file = invocation.last_message_file.clone();

        let run =
            reviewer::run(invocation.command, &text, self.cfg.timeout, &self.cancel).map_err(|e| {
                errors::spawn_failed(
                    self.cfg.reviewer.as_str(),
                    &self.bin.display().to_string(),
                    e.to_string(),
                )
            });

        let result = match run {
            Ok(out) => {
                if out.cancelled || out.timed_out {
                    Err(reviewer::failure_for(&self.cfg, &out))
                } else {
                    self.reviewer
                        .parse(&self.cfg, &out, last_message_file.as_deref())
                }
            }
            Err(failure) => Err(failure),
        };

        if let Some(path) = &last_message_file {
            std::fs::remove_file(path).ok();
        }

        let parsed = result?;

        // Only record the session once we have a real review in hand, so a failed
        // turn never leaves a session pointing at a conversation that went nowhere.
        // Resumability is tracked rather than assumed: the completed response invites a
        // follow-up on this session, so when that would not work the caller must be told.
        let mut warnings = Vec::new();
        let resumable = match &parsed.session_id {
            Some(session_id) => {
                match self.sessions.record_turn(
                    &self.session,
                    self.cfg.reviewer.as_str(),
                    session_id,
                    &self.cfg.model,
                    &self.cfg.effort,
                    &self.cfg.cwd.to_string_lossy(),
                ) {
                    Ok(_) => true,
                    Err(e) => {
                        // The review itself succeeded; losing resumability is worth a
                        // warning but not worth discarding the review.
                        eprintln!("cross-review: warning: could not save session state: {e}");
                        warnings.push(format!(
                            "This turn could not be saved to disk ({e}), so session '{}' may not \
                             resume correctly. The review below is unaffected.",
                            self.session
                        ));
                        false
                    }
                }
            }
            // On a resumed turn the stored mapping is still valid even if this turn did
            // not report an id, so the session remains resumable. Only a *new* review
            // with no id leaves nothing to resume.
            None if resume_id.is_some() => {
                eprintln!(
                    "cross-review: warning: the reviewer reported no session id on a resumed \
                     turn; keeping the existing mapping for session '{}'",
                    self.session
                );
                true
            }
            None => {
                eprintln!(
                    "cross-review: warning: the reviewer did not report a session id, so review \
                     session '{}' cannot be resumed",
                    self.session
                );
                warnings.push(format!(
                    "The reviewer did not report a session id, so session '{}' cannot be resumed. \
                     The review below is still valid, but a follow-up call with this session name \
                     will start a fresh review with no memory of it.",
                    self.session
                ));
                false
            }
        };

        Ok(Outcome {
            review: Some(parsed.text),
            failure: None,
            denials: parsed.denials,
            warnings,
            resumable,
        })
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_array_arg(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        // A single string where an array was expected is an easy mistake to make and
        // an easy one to absorb.
        Some(Value::String(single)) if !single.trim().is_empty() => {
            vec![single.trim().to_string()]
        }
        _ => Vec::new(),
    }
}

fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_arg_treats_blank_as_absent() {
        let args = json!({"instructions": "   ", "session": "work"});
        assert_eq!(string_arg(&args, "instructions"), None);
        assert_eq!(string_arg(&args, "session").as_deref(), Some("work"));
        assert_eq!(string_arg(&args, "missing"), None);
    }

    #[test]
    fn string_arg_trims() {
        let args = json!({"session": "  work  "});
        assert_eq!(string_arg(&args, "session").as_deref(), Some("work"));
    }

    #[test]
    fn context_paths_accepts_array_and_tolerates_a_bare_string() {
        assert_eq!(
            string_array_arg(
                &json!({"context_paths": ["a.rs", "  b.rs  ", ""]}),
                "context_paths"
            ),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
        assert_eq!(
            string_array_arg(&json!({"context_paths": "solo.rs"}), "context_paths"),
            vec!["solo.rs".to_string()]
        );
        assert!(string_array_arg(&json!({}), "context_paths").is_empty());
    }

    #[test]
    fn missing_instructions_is_a_request_error_not_a_stop_everything_failure() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let err = app.start_review(&json!({"session": "x"})).unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.is_agent_correctable());
        // The blunt stop-and-escalate wrapper is reserved for setup failures.
        assert!(!err.render_for_agent().contains("ACTION REQUIRED"));
    }

    #[test]
    fn result_without_an_identifier_is_rejected() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let err = app.review_result(&json!({})).unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
    }

    #[test]
    fn unknown_review_id_is_rejected_clearly() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let err = app
            .review_result(&json!({"review_id": "rv-nope-1"}))
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.summary.contains("rv-nope-1"));
    }

    #[test]
    fn wait_seconds_is_capped() {
        // Guards against an agent pinning the server open with wait_seconds=99999.
        let requested = json!({"wait_seconds": 99_999u64});
        let wait = requested
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(crate::config::DEFAULT_WAIT_SECS)
            .min(MAX_WAIT_SECS);
        assert_eq!(wait, MAX_WAIT_SECS);
    }

    #[test]
    fn age_formatting_reads_naturally() {
        assert_eq!(fmt_age(5), "5s ago");
        assert_eq!(fmt_age(90), "1m ago");
        assert_eq!(fmt_age(7200), "2h ago");
        assert_eq!(fmt_age(200_000), "2d ago");
    }
}
