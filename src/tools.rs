//! The four tools, and the state they share.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::cancel::RequestCancel;
use crate::config::{Config, MAX_WAIT_SECS};
use crate::errors::{self, Failure};
use crate::git;
use crate::prompt::{self, PromptParts, DEFAULT_PREAMBLE};
use crate::registry::{
    IdState, Outcome, Registry, Snapshot, Status, MAX_TERMINAL_PER_SESSION, MAX_TERMINAL_TOTAL,
};
use crate::reviewer::{self, Reviewer};
use crate::session::{self, now_unix, ExclusiveLock, SessionStore};

/// How long to wait for another server process to release a named session.
const SESSION_LEASE_WAIT: Duration = Duration::from_secs(3);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_SESSION: &str = "default";

/// Commit this binary was built from, when the build supplied one.
///
/// `CARGO_PKG_VERSION` alone cannot identify a build: it is pinned in Cargo.toml and
/// changes only on a release, so two binaries built months apart report the same string.
/// That made "is this binary current?" unanswerable, which mattered while the executable
/// was committed. Release builds set this; a local `cargo build` leaves it unset.
pub const BUILD: Option<&str> = option_env!("CROSS_REVIEW_BUILD");

/// Version plus provenance, for `--version` and the status tool.
pub fn version_line() -> String {
    match BUILD {
        Some(build) => format!("cross-review {VERSION} ({build})"),
        None => format!("cross-review {VERSION} (local build)"),
    }
}

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

    /// Release anything parked in a long poll: stdin has closed and the process is on its
    /// way out, so a waiter's remaining budget is time nobody is waiting for.
    pub fn begin_shutdown(&self) {
        self.registry.begin_shutdown();
    }

    /// Register reviews without a reviewer CLI, so the cancellation paths can be tested
    /// without spending a model call.
    #[cfg(test)]
    pub fn registry(&self) -> &Registry {
        &self.registry
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
        let auth = self.reviewer.auth_check(&bin, &self.cfg)?;
        let ready = Preflight { bin, auth };
        *self.preflight.lock().unwrap_or_else(|e| e.into_inner()) = Some(ready.clone());
        Ok(ready)
    }

    // -----------------------------------------------------------------------
    // cross_model_review
    // -----------------------------------------------------------------------

    pub fn start_review(&self, args: &Value, request: &RequestCancel) -> Result<String, Failure> {
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

        // Bind the review to the request that created it before the worker starts, so a
        // `notifications/cancelled` arriving mid-setup stops the reviewer instead of
        // finding nothing to stop. If it already arrived, never spawn the CLI at all --
        // but still record a terminal state, or the session stays claimed by a review
        // that no worker will ever finish.
        if request.attach_review(&id) {
            self.registry
                .finish(&id, Outcome::failed(errors::cancelled()));
            return Err(errors::cancelled());
        }

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

    pub fn review_result(&self, args: &Value, request: &RequestCancel) -> Result<String, Failure> {
        let review_id = string_arg(args, "review_id");
        let session = string_arg(args, "session");

        let wait = args
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(crate::config::DEFAULT_WAIT_SECS)
            .min(MAX_WAIT_SECS);

        let id = match (&review_id, &session) {
            (Some(id), _) => match self.registry.lookup(id) {
                IdState::Known => id.clone(),
                // Distinguished from "never existed" deliberately. Both end in "start a
                // new review", but a caller told its id was never issued has reason to
                // suspect it mangled the id, and will go looking for a bug that is not
                // there.
                IdState::Evicted => return Err(evicted_error(id)),
                IdState::Unknown => {
                    return Err(errors::bad_request(format!(
                        "No review with review_id '{id}' exists in this server process. Review \
                         ids do not survive a restart of the MCP server; start a new review \
                         instead."
                    )));
                }
            },
            // Deliberately does not claim which of the two it is. Telling them apart
            // would mean remembering every session name that ever had a review evicted,
            // which is unbounded in exactly the way the retention caps exist to prevent --
            // and the growth would be caller-controlled, since session names are. A
            // `review_id` still gets the strict distinction, because that can be derived
            // rather than stored; see `Registry::was_issued`. So this states both
            // possibilities rather than guessing at one, which is the honest shape of what
            // the server actually knows.
            (None, Some(name)) => self.registry.latest_for_session(name).ok_or_else(|| {
                errors::bad_request(format!(
                    "No review is currently retained for session '{name}'. Either none was \
                     started in this server process, or one finished and its result has since \
                     been discarded to bound memory. Either way it is not recoverable: start a \
                     new review. If you still hold the review_id, pass that instead — it can \
                     tell the two apart."
                ))
            })?,
            (None, None) => {
                return Err(errors::bad_request(
                    "Provide either 'review_id' (preferred) or 'session'.",
                ))
            }
        };

        // Abandoning this call cancels the review it is waiting on. That is a deliberate
        // trade, and it is the destructive direction: unlike the start call, the caller
        // does hold the review_id and could have come back for it -- SESSION_BUSY even
        // tells it to. It is made anyway because a review nobody is waiting on bills
        // against its whole timeout budget and holds the session lease for just as long,
        // and because the protocol cannot distinguish a caller that will return from one
        // that will not. The client-side tool timeout must therefore exceed MAX_WAIT_SECS,
        // which is why both example configurations pin it and say why.
        //
        // Binding the two here is also what ends this wait when the notification lands
        // mid-poll: cancelling drives the worker to a terminal state, which wakes the
        // condvar, so a suppressed response does not park a thread until shutdown.
        //
        // The binding is many requests to one review, so cancelling one of two concurrent
        // polls ends the other as well. Left alone: agents poll a review sequentially, and
        // the CANCELLED text the second one gets is accurate about what happened to it.
        if request.attach_review(&id) {
            self.registry.cancel(&id);
            return Err(errors::cancelled());
        }

        // `wait` can return None for an id that was Known a moment ago: a concurrent
        // finish elsewhere sweeps the caps, and this id can be what it drops. Re-checking
        // costs one lock and keeps the distinction the lookup above just drew, instead of
        // collapsing it into an opaque "no longer tracked".
        let snapshot = match self.registry.wait(&id, Duration::from_secs(wait)) {
            Some(snapshot) => snapshot,
            None if self.registry.lookup(&id) == IdState::Evicted => {
                return Err(evicted_error(&id));
            }
            None => {
                return Err(errors::bad_request(format!(
                    "Review '{id}' is no longer tracked."
                )))
            }
        };

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
        // Two different reasons to be looking at a running review, and inviting a retry is
        // only right for one of them. A wait ended by shutdown will get no second call:
        // the process is exiting and review ids do not survive it, so saying "call again"
        // would be advice the caller cannot act on.
        let next = if snapshot.shutting_down {
            "This server's stdin has closed, so it is shutting down and this wait ended \
             early. The review will not be delivered, and its review_id does not survive \
             the server process.\n\n\
             Do not proceed as though the review had come back. Start a new review once the \
             server is running again."
                .to_string()
        } else {
            format!(
                "The reviewer is still working. This call waited {waited}s. Call \
                 cross_model_review_result again with the same review_id to keep waiting.\n\n\
                 Do not start a second review for this session, and do not proceed as though \
                 the review had come back."
            )
        };
        format!(
            "status:    {}\n\
             review_id: {}\n\
             session:   {} (turn {})\n\
             reviewer:  {}\n\
             elapsed:   {}s of a {}s budget\n\n\
             {next}\n",
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
        out.push_str(&format!("{}\n\n", version_line()));
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
            "isolation:     {}\n",
            if self.cfg.isolate_reviewer {
                "on (reviewer loads no project hooks, settings, plugins or MCP servers)"
            } else {
                "OFF - the reviewer loads this project's configuration, including hooks"
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

    /// Stop a review by id, reporting whether it was still running. Used by the tool
    /// below and by the protocol layer when a client cancels the request that owns it.
    pub fn cancel_review(&self, id: &str) -> bool {
        self.registry.cancel(id)
    }

    pub fn cancel(&self, args: &Value) -> Result<String, Failure> {
        let id = string_arg(args, "review_id")
            .ok_or_else(|| errors::bad_request("'review_id' is required."))?;
        match self.registry.lookup(&id) {
            IdState::Known => {}
            // An evicted review is a finished one, so the honest answer is the same as
            // for any other finished review: there is nothing to stop. Reporting it as
            // an unknown id would suggest the caller got the id wrong.
            IdState::Evicted => {
                return Ok(format!(
                    "Review '{id}' finished earlier and its result has since been discarded, so \
                     there is nothing to cancel.\n"
                ));
            }
            IdState::Unknown => {
                return Err(errors::bad_request(format!(
                    "No review with review_id '{id}' exists in this server process."
                )));
            }
        }
        if self.cancel_review(&id) {
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

        // Captured once, before either attempt: the retry below re-runs the same review
        // in a new reviewer session, so re-running git for it would only spend time and
        // risk showing the two turns different trees.
        let capture = git::capture(&self.cfg, &self.cancel);
        let change = capture
            .change
            .as_ref()
            .map(|change| git::render(change, &self.cfg.cwd, self.cfg.reviewer_has_shell()));
        let capture_warnings = capture.warnings;

        let outcome = match self.attempt(
            resume_id.as_deref(),
            self.turn,
            change.as_deref(),
            &capture_warnings,
        ) {
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
                    match self.attempt(None, 1, change.as_deref(), &capture_warnings) {
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

    fn attempt(
        &self,
        resume_id: Option<&str>,
        turn: u32,
        change: Option<&str>,
        capture_warnings: &[String],
    ) -> Result<Outcome, Failure> {
        let preamble = if self.cfg.no_preamble {
            None
        } else {
            Some(self.cfg.preamble.as_deref().unwrap_or(DEFAULT_PREAMBLE))
        };

        // --no-preamble means "send my instructions with nothing added", so it has to
        // suppress the capability section too, not just the preamble. It does not
        // suppress the change: that is evidence the reviewer cannot fetch, not framing
        // we chose to add, and `--diff none` is the switch for turning it off.
        //
        // The capability text is told what was actually captured rather than what was
        // configured, so a diff that could not be produced is never announced.
        let capabilities = self.cfg.reviewer_capabilities(change.is_some());
        let capabilities = if self.cfg.no_preamble {
            None
        } else {
            Some(capabilities.as_str())
        };
        let text = prompt::build(&PromptParts {
            instructions: &self.instructions,
            context_paths: &self.context_paths,
            cwd: &self.cfg.cwd,
            turn,
            resumed: resume_id.is_some(),
            preamble,
            capabilities,
            change,
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

        let mut parsed = result?;

        // Only record the session once we have a real review in hand, so a failed
        // turn never leaves a session pointing at a conversation that went nowhere.
        // Resumability is tracked rather than assumed: the completed response invites a
        // follow-up on this session, so when that would not work the caller must be told.

        // Carried first, so a review made without the change under review says so before
        // anything else. These are not failures -- the review ran -- but a caller that
        // asked for a review of a diff and silently got a review of the tree is the one
        // way this tool can be wrong without anything appearing to go wrong.
        let mut warnings = capture_warnings.to_vec();
        // Then whatever the adapter noticed, so a run that hit the output cap but still
        // produced a usable review reports that rather than looking untroubled. Second
        // because it is about how the review was collected, not about what was reviewed.
        warnings.extend(std::mem::take(&mut parsed.warnings));
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

/// What to tell a caller holding the id of a review that has been evicted.
///
/// Names both caps, not just the per-session one. Either can be the reason, and a caller
/// that ran a single review in this session, told it keeps "only the 3 most recent per
/// session", can see that the explanation does not fit -- which undermines the message at
/// exactly the moment it is meant to be believed.
fn evicted_error(id: &str) -> Failure {
    errors::bad_request(format!(
        "Review '{id}' finished earlier and its result has since been discarded: this server \
         keeps the {MAX_TERMINAL_PER_SESSION} most recent finished reviews per session and \
         {MAX_TERMINAL_TOTAL} in total, so that a long agent session does not accumulate every \
         review it has ever run. The id was valid; the review is not recoverable. Start a new \
         review instead."
    ))
}

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
        let err = app
            .start_review(&json!({"session": "x"}), &RequestCancel::new())
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.is_agent_correctable());
        // The blunt stop-and-escalate wrapper is reserved for setup failures.
        assert!(!err.render_for_agent().contains("ACTION REQUIRED"));
    }

    #[test]
    fn result_without_an_identifier_is_rejected() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let err = app
            .review_result(&json!({}), &RequestCancel::new())
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
    }

    #[test]
    fn unknown_review_id_is_rejected_clearly() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let err = app
            .review_result(&json!({"review_id": "rv-nope-1"}), &RequestCancel::new())
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.summary.contains("rv-nope-1"));
    }

    /// Finish enough reviews on one session to push its oldest past the retention cap.
    fn app_with_an_evicted_review(session: &str) -> (App, String) {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let mut ids = Vec::new();
        for turn in 1..=MAX_TERMINAL_PER_SESSION as u32 + 1 {
            let (id, _c) = app
                .registry()
                .try_start(session, turn, turn > 1)
                .expect("start");
            app.registry()
                .finish(&id, Outcome::failed(errors::cancelled()));
            ids.push(id);
        }
        (app, ids.remove(0))
    }

    #[test]
    fn an_evicted_review_id_is_not_reported_as_one_that_never_existed() {
        // What the caller is told is the whole point of the retention change: this
        // message is what stops a calling agent silently proceeding, and "no such id"
        // would send it looking for a bug in how it stored the id instead.
        let (app, evicted) = app_with_an_evicted_review("default");
        let err = app
            .review_result(&json!({"review_id": evicted}), &RequestCancel::new())
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.summary.contains(&evicted), "{}", err.summary);
        assert!(err.summary.contains("discarded"), "{}", err.summary);
        assert!(!err.summary.contains("No review with"), "{}", err.summary);
        // Both caps are named, because either can be the reason and a caller that ran one
        // review in this session can see that the per-session cap does not explain it.
        assert!(
            err.summary.contains(&MAX_TERMINAL_PER_SESSION.to_string())
                && err.summary.contains(&MAX_TERMINAL_TOTAL.to_string()),
            "{}",
            err.summary
        );
    }

    #[test]
    fn a_session_with_no_retained_result_gets_one_clear_message_either_way() {
        // This wording replaced the retained-session distinction, which could not be kept
        // without unbounded caller-controlled growth. It is the one place in the change
        // where the caller is told less than before, so what it *is* told has to hold: the
        // two cases must be indistinguishable, and both must point at the identifier that
        // can still tell them apart.
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);

        // A session this process never saw.
        let never = app
            .review_result(&json!({"session": "never-used"}), &RequestCancel::new())
            .unwrap_err();

        // A session whose only review was evicted by the process-wide cap.
        for n in 0..=MAX_TERMINAL_TOTAL {
            let session = format!("session-{n}");
            let (id, _c) = app.registry().try_start(&session, 1, false).expect("start");
            app.registry()
                .finish(&id, Outcome::failed(errors::cancelled()));
        }
        let evicted = app
            .review_result(&json!({"session": "session-0"}), &RequestCancel::new())
            .unwrap_err();

        for (label, err) in [("never started", never), ("evicted", evicted)] {
            assert_eq!(err.code, "BAD_REQUEST", "{label}");
            assert!(
                err.summary.contains("currently retained"),
                "{label}: {}",
                err.summary
            );
            assert!(
                err.summary.contains("review_id"),
                "{label}: {}",
                err.summary
            );
            // It must not pick one of the two possibilities and assert it.
            assert!(err.summary.contains("Either"), "{label}: {}", err.summary);
        }
    }

    #[test]
    fn cancelling_an_evicted_review_says_there_is_nothing_to_stop() {
        // An evicted review is a finished one, so this is not an error at all -- and
        // reporting it as an unknown id would suggest the caller got the id wrong.
        let (app, evicted) = app_with_an_evicted_review("default");
        let message = app
            .cancel(&json!({"review_id": evicted}))
            .expect("not an error");
        assert!(message.contains("nothing to cancel"), "{message}");
    }

    #[test]
    fn a_cancelled_result_call_stops_the_review_it_was_waiting_on() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        // Registered directly: starting one for real would need a reviewer CLI.
        let (id, cancel) = app.registry.try_start("default", 1, false).expect("start");

        let request = RequestCancel::new();
        assert_eq!(request.cancel(), None);
        // The client cancelled before the poll got as far as naming its review, so the
        // poll itself must notice and stop it rather than wait out its budget.
        let err = app
            .review_result(&json!({"review_id": id, "wait_seconds": 300}), &request)
            .unwrap_err();
        assert_eq!(err.code, "CANCELLED");
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_live_result_call_leaves_its_review_alone() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let (id, cancel) = app.registry.try_start("default", 1, false).expect("start");

        let request = RequestCancel::new();
        let out = app
            .review_result(
                &json!({"review_id": id.clone(), "wait_seconds": 0}),
                &request,
            )
            .expect("still running");
        assert!(out.contains("status:    running"));
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
        // Bound to the request, so a cancellation arriving now knows what to stop.
        assert_eq!(request.cancel().as_deref(), Some(id.as_str()));
    }

    #[test]
    fn shutdown_ends_a_long_poll_and_says_why() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = Arc::new(App::new(cfg));
        let (id, _cancel) = app.registry.try_start("default", 1, false).expect("start");

        // A budget far longer than this test needs, so only the shutdown can end the poll
        // in time for the assertions below to hold.
        let poller = {
            let app = Arc::clone(&app);
            let args = json!({"review_id": id, "wait_seconds": 30});
            std::thread::spawn(move || app.review_result(&args, &RequestCancel::new()))
        };

        let started = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        app.begin_shutdown();

        let out = poller.join().expect("poller").expect("still running");
        // Timed as well as read: a snapshot taken at the deadline would carry the same
        // shutdown text, so only the elapsed time distinguishes a woken poll from one that
        // sat out its full budget.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown did not end the poll"
        );
        assert!(out.contains("status:    running"));
        // The caller must not be told to call again: nothing will be there to answer.
        assert!(out.contains("shutting down"));
        assert!(!out.contains("Call cross_model_review_result again"));
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
