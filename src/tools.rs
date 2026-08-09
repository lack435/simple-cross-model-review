//! The four tools, and the state they share.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::cancel::RequestCancel;
use crate::config::{Config, MAX_WAIT_SECS};
use crate::errors::{self, Failure};
use crate::metrics::{self, MetricsLog};
use crate::prompt::{self, PromptParts, DEFAULT_PREAMBLE};
use crate::registry::{
    IdState, Outcome, Phase, Registry, Snapshot, StartRefused, Status, MAX_TERMINAL_PER_SESSION,
    MAX_TERMINAL_TOTAL,
};
use crate::reviewer::{self, Reviewer};
use crate::session::{self, now_unix, ExclusiveLock, SessionStore};
use crate::vcs;

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
    metrics: Arc<MetricsLog>,
    /// Successful preflights are cached for the process lifetime; failures are not, so
    /// a user who runs `codex login` can retry without restarting the agent session.
    preflight: Mutex<Option<Preflight>>,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let sessions = SessionStore::new(&cfg.state_dir);
        let metrics = MetricsLog::new(&cfg.state_dir, cfg.metrics);
        let reviewer = reviewer::for_kind(cfg.reviewer);
        Self {
            cfg: Arc::new(cfg),
            reviewer: Arc::from(reviewer),
            registry: Arc::new(Registry::new()),
            sessions: Arc::new(sessions),
            metrics: Arc::new(metrics),
            preflight: Mutex::new(None),
        }
    }

    /// The recorded usage history, for the `--usage` report.
    pub fn usage_report(&self) -> String {
        let (summary, report) = self.metrics.summarise();
        metrics::render_summary(&summary, self.metrics.dir(), &report)
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

        // Perforce changelists are named per call, and validated here -- before the preflight
        // and the session lease -- so a malformed or backend-mismatched request costs nothing.
        // A git server rejects the Perforce-only inputs by presence (any non-null value)
        // rather than silently ignoring them, so a git caller gets the pointed "wrong backend"
        // message instead of a downstream parse error or a no-op. A JSON null is "unset", so
        // it is treated as absent, here and in the strict parse below.
        if self.cfg.vcs == crate::config::Vcs::Git {
            let present = |key: &str| args.get(key).is_some_and(|v| !v.is_null());
            if present("change") || present("include_shelved") {
                return Err(errors::bad_request(
                    "'change' and 'include_shelved' name Perforce inputs, but this working root \
                     is git. Omit them -- the git diff to review is configured on the server \
                     (see --diff).",
                ));
            }
        }

        let changes = parse_change_arg(args)?;
        // Parsed strictly rather than coerced: a non-boolean silently becoming `false` would
        // drop a caller's intent to review a shelf without any error.
        let include_shelved = match args.get("include_shelved") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(errors::bad_request(
                    "'include_shelved' must be true or false.",
                ))
            }
        };
        if self.cfg.vcs == crate::config::Vcs::Perforce && changes.is_empty() {
            return Err(errors::bad_request(
                "'change' is required for a Perforce review: name the changelist number(s) to \
                 review, for example \"43650\" or [\"43650\",\"43651\"].",
            ));
        }
        let changes_canonical = crate::changeset::canonical(&changes);

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

        // A stored session that exists but must not be resumed is refused here, while the
        // lease is held and before anything is spawned or billed, so the caller explicitly
        // chooses to start fresh (fresh=true, or a new session name) rather than being
        // silently handed a review with no memory of the work it asked to continue.
        if let Some(record) = &prior {
            if let Some(reason) = resume_block(&self.cfg, record, &changes_canonical, now_unix()) {
                return Err(errors::session_not_resumable(&session, reason));
            }
        }

        let (resume_id, turn, resumed) = match &prior {
            Some(record) => (
                Some(record.cli_session_id.clone()),
                record.turns.saturating_add(1),
                true,
            ),
            None => (None, 1, false),
        };

        // Claiming the session and registering the review are one atomic step, so two
        // concurrent calls cannot both start a review against the same conversation. It
        // can also refuse because the server is going away: this handler may have spent
        // the preflight or the lease wait crossing stdin's closure, and a review started
        // on the other side of that could never be collected.
        let (id, cancel) = self
            .registry
            .try_start(&session, turn, resumed)
            .map_err(|refused| match refused {
                StartRefused::Busy(existing) => errors::session_busy(&session, &existing),
                StartRefused::ShuttingDown => errors::server_shutting_down(),
            })?;

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
            metrics: Arc::clone(&self.metrics),
            bin: ready.bin,
            id: id.clone(),
            session: session.clone(),
            instructions,
            context_paths,
            changes: changes.clone(),
            include_shelved,
            turn,
            // Read here, while the lease is held and before this turn overwrites it, so
            // the recorded gap is the interval the reviewer's prompt cache actually saw.
            // Taking it after the review would measure zero.
            gap_secs: metrics::gap_since(prior.as_ref().map(|record| record.updated_unix)),
            prior_cumulative: prior.as_ref().and_then(|record| record.cumulative_usage),
            // Only carried on a genuine resume: `prior` is `None` for a fresh review
            // (fresh=true or a new session name), so the first turn always captures in full.
            prior_head: prior.as_ref().and_then(|record| record.head_sha.clone()),
            prior_base: prior.as_ref().and_then(|record| record.base_sha.clone()),
            prior_perforce_baseline: prior
                .as_ref()
                .and_then(|record| record.perforce_baseline.clone()),
            prior_capture_identity: prior
                .as_ref()
                .and_then(|record| record.capture_identity.clone()),
            prior_include_shelved: prior.as_ref().and_then(|record| record.include_shelved),
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
        out.push_str(&format!("reviewer:  {}\n", self.cfg.describe_reviewer()));
        if !changes.is_empty() {
            out.push_str(&format!(
                "changelists: {}{}\n",
                changes
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                if include_shelved {
                    " (including shelved snapshots)"
                } else {
                    ""
                }
            ));
        }
        out.push_str(&format!("budget:    {}\n\n", fmt_elapsed(self.cfg.timeout)));
        out.push_str(&format!(
            "Collect it with cross_model_review_result using review_id \"{id}\". That call waits \
             for the review and reports progress while it is open when the MCP client supports \
             progress notifications. Use wait_seconds=300; if it returns status=running, call it \
             again with the same review_id.\n\n\
             In this project's usage, reviews commonly take at least five minutes, and complex \
             changes can take 20 minutes or longer. A running status during that window is normal \
             and is not a reason to start over or cancel the review.\n"
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
        // condvar, so a suppressed response does not park a handler thread.
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
            "This server's stdin is no longer readable, so it is shutting down and this \
             wait will not be extended. The review will not be delivered, and its \
             review_id does not survive the server process.\n\n\
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
             progress:  {}\n\n\
             {next}\n",
            snapshot.status.as_str(),
            snapshot.id,
            snapshot.session,
            snapshot.turn,
            self.cfg.describe_reviewer(),
            snapshot.elapsed.as_secs(),
            self.cfg.timeout.as_secs(),
            render_progress(snapshot, self.cfg.timeout, !snapshot.shutting_down),
        )
    }

    /// A concise live snapshot for MCP `notifications/progress`.
    ///
    /// Invalid or already-finished identifiers return no message: the result call itself
    /// owns the authoritative error or terminal response, and a speculative notification
    /// must not race it with a contradictory claim.
    pub fn review_progress(&self, args: &Value) -> Option<String> {
        let id = string_arg(args, "review_id").or_else(|| {
            string_arg(args, "session").and_then(|name| self.registry.latest_for_session(&name))
        })?;
        let snapshot = self.registry.snapshot(&id)?;
        (snapshot.status == Status::Running)
            .then(|| render_progress(&snapshot, self.cfg.timeout, !snapshot.shutting_down))
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

        // Stated on every completed review rather than kept in the log alone. A review
        // turn is many model calls over a conversation that grows with each turn, and an
        // agent that cannot see what a turn cost has no way to notice that its tenth
        // follow-up costs several times what its first did.
        if !snapshot.usage.is_empty() {
            out.push_str(&format!("usage:     {}\n\n", snapshot.usage.summary()));
        }

        for warning in &snapshot.warnings {
            out.push_str(&format!("WARNING: {warning}\n\n"));
        }

        if snapshot.denial_count > 0 || !snapshot.denials.is_empty() {
            // Keep the exact total separate from the bounded examples shown below. The
            // fallback preserves honest output for snapshots created by older in-memory
            // callers that did not populate the count.
            let denial_count = snapshot.denial_count.max(snapshot.denials.len());
            // When the count was recovered from capped output, later refusals were dropped,
            // so it is a lower bound -- say so rather than presenting it as the exact total.
            let count_phrase = if snapshot.denial_count_is_floor {
                format!("at least {denial_count}")
            } else {
                denial_count.to_string()
            };
            out.push_str(&format!(
                "Note: the reviewer tried {count_phrase} command(s) it was not permitted to run, \
                 so parts of its analysis may rest on less evidence than usual:\n",
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
            "resume limits: {}, {} (a session past either is refused, not silently restarted)\n",
            match self.cfg.resume_max_turns {
                0 => "no turn cap".to_string(),
                n => format!("max {n} turns"),
            },
            if self.cfg.resume_max_idle.is_zero() {
                "no idle cap".to_string()
            } else {
                format!("idle up to {}", fmt_elapsed(self.cfg.resume_max_idle))
            },
        ));
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

        if self.metrics.enabled() {
            let (summary, report) = self.metrics.summarise();
            out.push('\n');
            out.push_str(&metrics::render_summary(
                &summary,
                self.metrics.dir(),
                &report,
            ));
        } else {
            out.push_str("usage log:     off (--no-metrics)\n");
        }

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
    metrics: Arc<MetricsLog>,
    bin: PathBuf,
    id: String,
    session: String,
    instructions: String,
    context_paths: Vec<String>,
    /// The Perforce changelists this review captures, in request order. Empty for git.
    changes: Vec<u64>,
    /// Whether to pull shelved content for a pending changelist with nothing open.
    include_shelved: bool,
    turn: u32,
    /// How long this session sat idle before this turn, when it had a previous one.
    gap_secs: Option<u64>,
    /// The cumulative usage this session's reviewer last reported, for adapters that
    /// report cumulatively. Subtracting it is the only way to recover a per-turn figure
    /// from Codex, whose event stream carries the thread total and nothing else.
    prior_cumulative: Option<crate::metrics::Usage>,
    /// The git HEAD the previous turn of this session captured, and the `--diff` spec it was
    /// captured under, for the incremental-resume delta. Both `None` on a fresh review, a
    /// Perforce session, or a prior turn that could not resolve HEAD; the delta needs both, so
    /// a resume only reviews the delta when each is present and the spec still matches.
    prior_head: Option<String>,
    prior_base: Option<String>,
    /// The previous Perforce turn's resume-delta baseline, capture identity and shelved-capture
    /// mode, for collapsing unchanged files. All `None` on a fresh review or a git session; the
    /// delta needs the baseline and the identity, and only collapses when the mode still matches.
    prior_perforce_baseline: Option<crate::vcs::baseline::PerforceBaseline>,
    prior_capture_identity: Option<crate::vcs::baseline::CaptureIdentity>,
    prior_include_shelved: Option<bool>,
    cancel: Arc<AtomicBool>,
    /// Cross-process claim on the named session. Never read: it exists so that dropping
    /// the job releases the session for other server processes.
    _lease: Option<ExclusiveLock>,
}

/// The capture's outputs that `attempt` threads onward: the rendered change goes into the
/// prompt, and the captured (HEAD, base) baseline goes into the session record so the next
/// resume can review only what changed since it. Bundled so they travel together rather than as
/// loose `Option<&str>` arguments that could be transposed.
struct CaptureOutputs<'a> {
    change: Option<&'a str>,
    head_sha: Option<&'a str>,
    base_sha: Option<&'a str>,
    /// The Perforce capture identity and resume-delta baseline this turn produced, recorded on
    /// the session so the next resume knows what capture it may collapse against. Both `None`
    /// for git.
    capture_identity: Option<&'a crate::vcs::baseline::CaptureIdentity>,
    perforce_baseline: Option<&'a crate::vcs::baseline::PerforceBaseline>,
}

/// What the attempt that produced the outcome actually did, gathered for the usage record.
///
/// `prompt_bytes` is filled in by `attempt` through an out-parameter, so it survives the
/// error paths there: a failed turn still sent a prompt, and its size is part of explaining
/// the cost.
#[derive(Clone, Copy)]
struct AttemptFacts {
    turn: u32,
    resumed: bool,
    gap_secs: Option<u64>,
    prompt_bytes: usize,
}

impl AttemptFacts {
    fn new(turn: u32, resumed: bool, gap_secs: Option<u64>) -> Self {
        Self {
            turn,
            resumed,
            gap_secs,
            prompt_bytes: 0,
        }
    }
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
        let started = std::time::Instant::now();
        let mut guard = FinishGuard {
            registry: &self.registry,
            id: &self.id,
            armed: true,
        };

        // Captured once, before the attempt runs, so the reviewer's prompt and the usage
        // metrics below describe the same diff.
        if self.cfg.supplies_change() {
            self.registry.set_phase(&self.id, Phase::Capturing);
        }
        // Durable poison check: if the *previous* Perforce turn left an uncleared "in progress"
        // marker -- it crashed, panicked, or failed to persist its baseline -- the persisted
        // baseline may be stale relative to what the reviewer last saw, so do not collapse against
        // it. Read this before marking the current turn pending.
        let prior_pending =
            self.cfg.vcs == crate::config::Vcs::Perforce && self.sessions.is_pending(&self.session);
        // Mark this Perforce turn in progress; cleared only once it is durably recorded, so a
        // failure anywhere below leaves the marker set for the next resume to see.
        if self.cfg.vcs == crate::config::Vcs::Perforce {
            self.sessions.mark_pending(&self.session);
        }

        // The prior turn's baseline for the incremental-resume delta, tagged by backend. Git
        // needs both HEAD and base; Perforce needs its stored inventory. The backend decides from
        // there whether the delta is safe (git: matching base and ancestry; Perforce: matching
        // identity, mode and per-file fingerprint). A git session never carries a Perforce
        // baseline and vice versa, so at most one arm is `Some`.
        let resume = match self.cfg.vcs {
            crate::config::Vcs::Git => {
                match (self.prior_head.as_deref(), self.prior_base.as_deref()) {
                    (Some(head), Some(base)) => {
                        Some(vcs::Resume::Git(vcs::GitResumeBaseline { head, base }))
                    }
                    _ => None,
                }
            }
            // `prior_pending` forces a full capture: the prior turn did not cleanly persist, so its
            // baseline cannot be trusted.
            crate::config::Vcs::Perforce if prior_pending => None,
            crate::config::Vcs::Perforce => self.prior_perforce_baseline.as_ref().map(|baseline| {
                vcs::Resume::Perforce(vcs::perforce::PerforceResume {
                    baseline,
                    identity: self.prior_capture_identity.as_ref(),
                    include_shelved: self.prior_include_shelved,
                })
            }),
        };
        let capture = vcs::capture(
            &self.cfg,
            &self.changes,
            self.include_shelved,
            resume,
            &self.cancel,
        );
        // The backend has already rendered the change into the prompt string; clone it out
        // so `capture.change` stays available for the usage metrics below.
        let change = capture.change.as_ref().map(|c| c.rendered.clone());
        // The (HEAD, base) baseline this turn captured, recorded on the session so the next
        // turn can delta against it. Git-only; both `None` for Perforce, an unresolved HEAD, or
        // a truncated capture.
        let head_sha = capture.head_sha.clone();
        let base_sha = capture.base_sha.clone();
        // The Perforce capture identity and resume-delta baseline this turn produced. Both
        // `None` for git; recorded on the session so the next resume can collapse against them.
        let capture_identity = capture.capture_identity.clone();
        let perforce_baseline = capture.perforce_baseline.clone();
        let capture_warnings = capture.warnings;
        self.registry.set_phase(&self.id, Phase::Launching);

        // What this attempt did, for the usage record.
        let mut facts = AttemptFacts::new(self.turn, resume_id.is_some(), self.gap_secs);

        let outcome = match self.attempt(
            resume_id.as_deref(),
            self.turn,
            self.prior_cumulative,
            CaptureOutputs {
                change: change.as_deref(),
                head_sha: head_sha.as_deref(),
                base_sha: base_sha.as_deref(),
                capture_identity: capture_identity.as_ref(),
                perforce_baseline: perforce_baseline.as_ref(),
            },
            &capture_warnings,
            &mut facts.prompt_bytes,
        ) {
            Ok(outcome) => outcome,
            Err(failure) => {
                // A resume target the reviewer no longer has is a dead mapping: drop it so a
                // follow-up call is not billed for the same doomed resume. The failure is
                // then reported rather than silently retried into a fresh conversation --
                // the caller is told the session expired and decides whether to start over
                // (fresh=true), the same explicit contract the pre-flight resume checks use.
                // Its remediation already says exactly that.
                if failure.code == "SESSION_NOT_FOUND" && resume_id.is_some() {
                    self.sessions.forget(&self.session).ok();
                    eprintln!(
                        "cross-review: session '{}' could not be resumed (the reviewer no \
                         longer has it); reporting SESSION_NOT_FOUND rather than silently \
                         starting a fresh session",
                        self.session
                    );
                }
                Outcome::failed(failure)
            }
        };
        // Everything telemetry needs, taken before the outcome moves into the registry.
        let usage = outcome.usage;
        let failure_code = outcome.failure.as_ref().map(|f| f.code.to_string());

        // Deliver the review first, and disarm before any accounting runs. Recording used
        // to happen here, while the guard was still armed: `eprintln!` panics if stderr
        // has closed, which would have replaced a completed, fully-parsed review with
        // WORKER_PANICKED and lost it outright. Telemetry must never be able to cost the
        // caller the review it is describing -- and it need not hold up the response for
        // a lock, either.
        self.registry.finish(&self.id, outcome);
        guard.armed = false;

        self.record_usage(usage, failure_code, capture.change.as_ref(), started, facts);
    }

    /// Append this turn to the usage log, and say the same thing on stderr.
    ///
    /// Both, because they answer different questions. The log is what you aggregate
    /// across sessions and machines after the fact; the stderr line is what a user
    /// watching a review happen can see without going and finding the file.
    ///
    /// Best effort by construction: it runs after the review has been handed over, so
    /// nothing it does can affect the result.
    fn record_usage(
        &self,
        usage: crate::metrics::Usage,
        failure_code: Option<String>,
        change: Option<&vcs::CapturedChange>,
        started: std::time::Instant,
        facts: AttemptFacts,
    ) {
        let status = if failure_code.is_some() {
            "failed"
        } else {
            "completed"
        };

        // The rendered diff is what actually went into the prompt, so that is what is
        // measured -- not the raw `git diff` output, which is a different size.
        let diff_bytes = change.map(|c| c.diff_bytes).unwrap_or(0);
        let diff_truncated = change.map(|c| c.diff_truncated).unwrap_or(false);

        eprintln!(
            "cross-review: {} turn {} of session '{}' {} in {}s{} -- {}",
            self.cfg.reviewer.as_str(),
            facts.turn,
            self.session,
            status,
            started.elapsed().as_secs(),
            match facts.gap_secs {
                Some(gap) => format!(
                    " after a {} idle gap",
                    fmt_elapsed(Duration::from_secs(gap))
                ),
                None => String::new(),
            },
            if usage.is_empty() {
                "no usage reported by the CLI".to_string()
            } else {
                usage.summary()
            },
        );

        self.metrics.record(&metrics::Record {
            v: metrics::RECORD_VERSION,
            ts_unix: now_unix(),
            review_id: self.id.clone(),
            session: self.session.clone(),
            turn: facts.turn,
            resumed: facts.resumed,
            gap_secs: facts.gap_secs,
            reviewer: self.cfg.reviewer.as_str().to_string(),
            model: self.cfg.model.clone(),
            effort: self.cfg.effort.clone(),
            prompt_bytes: facts.prompt_bytes,
            diff_bytes,
            diff_truncated,
            usage,
            wall_secs: started.elapsed().as_secs(),
            // Turns are no longer auto-retried after an expired session -- the failure is
            // surfaced instead -- so a freshly written record is never a retry. The field
            // stays in the schema so the reader can still report `retried: true` from
            // records written before that change.
            retried: false,
            status: status.to_string(),
            failure_code,
        });
    }

    fn attempt(
        &self,
        resume_id: Option<&str>,
        turn: u32,
        baseline: Option<crate::metrics::Usage>,
        captured: CaptureOutputs<'_>,
        capture_warnings: &[String],
        prompt_bytes: &mut usize,
    ) -> Result<Outcome, Failure> {
        let CaptureOutputs {
            change,
            head_sha,
            base_sha,
            capture_identity,
            perforce_baseline,
        } = captured;
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
        // On a resumed Perforce turn the captured change is a fresh snapshot of a changelist
        // whose pending contents can move between turns; say so, so the reviewer does not read
        // a legitimate change as a contradiction of its earlier findings.
        let resumed_capture_note = (resume_id.is_some()
            && self.cfg.vcs == crate::config::Vcs::Perforce
            && change.is_some())
        .then_some(
            "Note: the \"Change under review\" above is a freshly captured snapshot of the \
             changelist(s). A pending changelist's contents can move between turns, so it may \
             differ from what you reviewed on the previous turn -- review what is shown here, and \
             do not treat a difference from your earlier reading as a contradiction.",
        );
        let text = prompt::build(&PromptParts {
            instructions: &self.instructions,
            context_paths: &self.context_paths,
            cwd: &self.cfg.cwd,
            turn,
            resumed: resume_id.is_some(),
            preamble,
            capabilities,
            change,
            resumed_capture_note,
        });
        // Reported back through the out-parameter so it survives the error paths below:
        // a failed turn still sent a prompt, and its size is part of explaining the cost.
        *prompt_bytes = text.len();

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

        let run = reviewer::run_observed(
            invocation.command,
            &text,
            self.cfg.timeout,
            &self.cancel,
            |activity| {
                self.registry
                    .report_activity(&self.id, activity.output_bytes);
            },
        )
        .map_err(|e| {
            errors::spawn_failed(
                self.cfg.reviewer.as_str(),
                &self.bin.display().to_string(),
                e.to_string(),
            )
        });
        self.registry.set_phase(&self.id, Phase::Finalizing);

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

        // A cumulative reporter gives the thread's running total, so this turn's cost is
        // the difference from the last one. Without this the first turn looks right and
        // every later one is inflated by everything before it -- which is exactly what
        // happened, and it is invisible in a single figure.
        //
        // The baseline is passed in as a parameter rather than read from the job, so a fresh
        // (non-resumed) turn gets `None` and never differences its total against another
        // thread's, keeping this reconciliation a pure function of its inputs.
        // `baseline_to_persist` is what the next turn will subtract against: when a baseline
        // already exists it is always carried forward (an empty reading keeps the old one
        // rather than erasing it), and only in the no-baseline case is one persisted solely
        // when this reading actually carried a running total. The whole reconciliation is a
        // pure function so its every branch can be tested without a live reviewer -- see
        // `metrics::reconcile_cumulative`.
        let mut usage_warning: Option<String> = None;
        let baseline_to_persist = if let Some(total) =
            parsed.usage_is_cumulative.then_some(parsed.usage)
        {
            let reconciled = metrics::reconcile_cumulative(total, baseline, resume_id.is_some());
            parsed.usage = reconciled.per_turn;
            if reconciled.unknown {
                usage_warning = Some(
                    "Per-turn usage is unknown for this turn: this session predates usage \
                     tracking, so its running total cannot be split into a per-turn cost. \
                     Recording resumes once a later turn reports a running total to measure \
                     against."
                        .to_string(),
                );
            }
            reconciled.baseline
        } else {
            // Not a cumulative reporter (Claude reports per turn): nothing to difference and
            // no baseline to keep.
            None
        };

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
        if let Some(w) = usage_warning {
            warnings.push(w);
        }
        // A resumed turn that did not echo its id is still the same thread, so record it
        // under the id we resumed with. Skipping the record -- as this used to -- kept the
        // mapping but never advanced the baseline, so the next turn subtracted against a
        // stale total and double-counted this one.
        let record_under = parsed.session_id.as_deref().or(resume_id);
        let resumable = match record_under {
            Some(session_id) => {
                if parsed.session_id.is_none() {
                    eprintln!(
                        "cross-review: warning: the reviewer reported no session id on a resumed \
                         turn; recording under the resumed id for session '{}'",
                        self.session
                    );
                }
                match self.sessions.record_turn(
                    &self.session,
                    session::TurnFacts {
                        reviewer: self.cfg.reviewer.as_str(),
                        cli_session_id: session_id,
                        model: &self.cfg.model,
                        effort: &self.cfg.effort,
                        cwd: &self.cfg.cwd.to_string_lossy(),
                        cumulative_usage: baseline_to_persist,
                        // Bind the session to the changelist set (Perforce only), canonicalised
                        // so a re-review naming the same changelists in another order resumes.
                        changes: (self.cfg.vcs == crate::config::Vcs::Perforce)
                            .then(|| crate::changeset::canonical(&self.changes)),
                        // The (HEAD, base) baseline this turn captured, so the next resume
                        // reviews only what changed since it -- and only when its own range
                        // still resolves to the same base. Both come from the capture, which
                        // leaves them `None` (together) for Perforce, an unresolved HEAD, or a
                        // truncated diff; the record then advances or retains them as a pair.
                        head_sha: head_sha.map(str::to_string),
                        base_sha: base_sha.map(str::to_string),
                        // The Perforce resume-delta binding and baseline. `backend` and the
                        // shelved flag are known from config; the capture identity and per-file
                        // baseline come from the capture (both `None` for git).
                        backend: Some(self.cfg.vcs.backend_id()),
                        include_shelved: (self.cfg.vcs == crate::config::Vcs::Perforce)
                            .then_some(self.include_shelved),
                        capture_identity: capture_identity.cloned(),
                        perforce_baseline: perforce_baseline.cloned(),
                    },
                ) {
                    Ok(_) => {
                        // Durably recorded: clear the in-progress marker so the next resume trusts
                        // this turn's baseline. Only reached after `record_turn` returned `Ok`.
                        if self.cfg.vcs == crate::config::Vcs::Perforce {
                            self.sessions.clear_pending(&self.session);
                        }
                        true
                    }
                    Err(e) => {
                        // The review itself succeeded; losing resumability is worth a
                        // warning but not worth discarding the review.
                        //
                        // Fail-closed for Perforce: the write left the persisted record at the
                        // *prior* turn, so its stored inventory is now stale relative to what the
                        // reviewer just saw. A later resume that collapsed files against it would
                        // hide this turn's changes -- so poison the session by dropping the
                        // mapping, forcing the next call to start fresh. Do this *before* the
                        // diagnostics `eprintln!`, which can panic if stderr has closed (see the
                        // FinishGuard note above), so a lost stderr cannot skip the cleanup. Git's
                        // delta is ancestry-checked and has no equivalent hazard.
                        if self.cfg.vcs == crate::config::Vcs::Perforce {
                            match self.sessions.forget(&self.session) {
                                // Only `Ok(true)` -- a mapping was actually removed -- proves the
                                // session was reset. `Ok(false)` means nothing was removed, which
                                // includes the case where `read()` silently fell back to an empty
                                // store on a transient read error, leaving the stale mapping on
                                // disk. Treat that as uncertain, like an outright error.
                                Ok(true) => warnings.push(format!(
                                    "This turn could not be saved to disk ({e}). To stop a later \
                                     review from collapsing files against a stale baseline, session \
                                     '{}' has been reset -- a follow-up call will start a fresh \
                                     review. The review below is unaffected.",
                                    self.session
                                )),
                                // Poisoning could not be confirmed (the store was unwritable, or a
                                // read fell back to empty), so a stale mapping may survive. Say so
                                // plainly and tell the caller to force a fresh review.
                                other => {
                                    let detail = match other {
                                        Err(forget_err) => format!(" ({forget_err})"),
                                        _ => String::new(),
                                    };
                                    warnings.push(format!(
                                        "This turn could not be saved to disk ({e}), and session \
                                         '{}' could not be reliably reset{detail}. A follow-up \
                                         resume of this session could collapse files against a \
                                         stale baseline -- pass fresh: true to review it safely. \
                                         The review below is unaffected.",
                                        self.session
                                    ));
                                }
                            }
                        } else {
                            warnings.push(format!(
                                "This turn could not be saved to disk ({e}), so session '{}' may \
                                 not resume correctly. The review below is unaffected.",
                                self.session
                            ));
                        }
                        eprintln!("cross-review: warning: could not save session state: {e}");
                        false
                    }
                }
            }
            None => {
                // No session id means this turn cannot be recorded, so a prior Perforce mapping
                // (with its now-stale baseline) would otherwise survive and be resumed. Drop it,
                // before the panic-prone diagnostics, so the next call starts fresh.
                if self.cfg.vcs == crate::config::Vcs::Perforce {
                    self.sessions.forget(&self.session).ok();
                }
                warnings.push(format!(
                    "The reviewer did not report a session id, so session '{}' cannot be resumed. \
                     The review below is still valid, but a follow-up call with this session name \
                     will start a fresh review with no memory of it.",
                    self.session
                ));
                eprintln!(
                    "cross-review: warning: the reviewer did not report a session id, so review \
                     session '{}' cannot be resumed",
                    self.session
                );
                false
            }
        };

        Ok(Outcome {
            review: Some(parsed.text),
            failure: None,
            denials: parsed.denials,
            denial_count: parsed.denial_count,
            denial_count_is_floor: parsed.denial_count_is_floor,
            warnings,
            resumable,
            usage: parsed.usage,
        })
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Why a stored session must not be resumed, phrased for the calling agent, or `None` when
/// it may be. Checked while the session lease is held and before any reviewer is spawned,
/// so a refusal costs nothing. `fresh: true` never reaches here -- it looks up no prior --
/// so it is always the escape hatch.
///
/// Every branch refuses rather than silently starting a new session. A resume that quietly
/// became a fresh review is the one outcome this tool must not produce: the caller asked
/// for continuity and would act on the answer as though it had it.
fn resume_block(
    cfg: &Config,
    record: &session::SessionRecord,
    requested_changes: &[u64],
    now: u64,
) -> Option<String> {
    // Identity first: a session belongs to the reviewer, model and working root that
    // created it. A configuration that now points elsewhere cannot resume that conversation
    // at all, so the honest answer is to start over -- explicitly, so the caller learns the
    // earlier context is gone rather than inheriting it silently.
    if record.reviewer != cfg.reviewer.as_str() {
        return Some(format!(
            "it was created by reviewer '{}', but this server is now configured for '{}', and \
             a conversation cannot move between reviewers.",
            record.reviewer,
            cfg.reviewer.as_str()
        ));
    }
    if record.model != cfg.model {
        return Some(format!(
            "it was created with model '{}', but this server is now configured for '{}'.",
            record.model, cfg.model
        ));
    }
    let cwd = cfg.cwd.to_string_lossy();
    // Windows paths are case-insensitive, and the state directory is already keyed
    // case-insensitively, so a case-only difference here is the same root, not a new one.
    if !record.cwd.eq_ignore_ascii_case(&cwd) {
        return Some(format!(
            "it was created against working root '{}', but this server is now working in '{}'.",
            record.cwd, cwd
        ));
    }

    // A session belongs to the capture backend that created it. A git record must never satisfy
    // the Perforce binding logic (a git record carries no `changes`, which would otherwise read
    // as "unbound"), nor a Perforce record be resumed under git. `None` is a record written
    // before the backend field existed -- treated as unknown and allowed, since the other
    // identity checks still gate it.
    if let Some(backend) = &record.backend {
        if backend != cfg.vcs.backend_id() {
            return Some(format!(
                "it was created by the {} backend, but this server is now configured for {}, and \
                 a session cannot move between capture backends.",
                backend,
                cfg.vcs.backend_id()
            ));
        }
    }

    // A Perforce session follows one changelist set. A resume that names a different set is
    // continuing different work, so refuse rather than silently re-reviewing it with the old
    // findings in context. `requested_changes` is canonical (sorted, deduped) and so is the
    // stored binding, so the comparison is order-insensitive. `None` on the record is a git
    // session or one recorded before this binding existed -- treated as unbound.
    if cfg.vcs == crate::config::Vcs::Perforce {
        if let Some(bound) = &record.changes {
            if bound != requested_changes {
                return Some(format!(
                    "it is bound to changelist(s) {}, but this call names {}. A review session \
                     follows one changelist set; start a fresh review (fresh: true) to review a \
                     different set, or use a new session name.",
                    join_changes(bound),
                    join_changes(requested_changes),
                ));
            }
        }
    }

    // Then staleness. Turns before idle: when a session is both long and old, "it ran too
    // many turns" is the more specific thing to say.
    if cfg.resume_max_turns > 0 && record.turns >= cfg.resume_max_turns {
        return Some(format!(
            "it has already run {} turn(s), reaching the configured limit of {} \
             (--session-max-turns); each turn re-processes the whole conversation so far, so a \
             longer session grows expensive and prone to drift.",
            record.turns, cfg.resume_max_turns
        ));
    }
    if !cfg.resume_max_idle.is_zero() {
        let idle = now.saturating_sub(record.updated_unix);
        if idle > cfg.resume_max_idle.as_secs() {
            return Some(format!(
                "it was last used {} ago, past the configured {} resume window \
                 (--session-max-idle-seconds); by then the reviewer's prompt cache may no \
                 longer be warm and its context may have drifted, so resuming risks paying to \
                 re-read the whole conversation.",
                fmt_age(idle),
                fmt_elapsed(cfg.resume_max_idle)
            ));
        }
    }
    None
}

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

/// Parse the `change` request argument into validated changelist numbers.
///
/// Accepts the same comma-separated string form the removed `--change` flag took (`"43650"`
/// or `"43650,43651"`) and an array of strings/numbers, so a caller can pass either shape.
/// Both funnel through [`crate::changeset::parse_change_tokens`], so the dedupe, cap and
/// numeric validation are single-sourced. An absent argument yields an empty list; the
/// backend-specific requirement is enforced by the caller.
fn parse_change_arg(args: &Value) -> Result<Vec<u64>, Failure> {
    match args.get("change") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => crate::changeset::parse_changes(s).map_err(errors::bad_request),
        Some(Value::Array(items)) => {
            // An array element may be a JSON string or number, and a string element may itself
            // carry a comma; collect owned tokens, then split so the two forms are identical.
            let mut toks: Vec<String> = Vec::new();
            for item in items {
                match item {
                    Value::String(s) => toks.push(s.clone()),
                    Value::Number(n) if n.is_u64() => toks.push(n.to_string()),
                    _ => {
                        return Err(errors::bad_request(
                            "each entry in 'change' must be a changelist number (a string like \
                             \"43650\" or a non-negative integer).",
                        ))
                    }
                }
            }
            crate::changeset::parse_change_tokens(toks.iter().flat_map(|t| t.split(',')))
                .map_err(errors::bad_request)
        }
        Some(_) => Err(errors::bad_request(
            "'change' must be a changelist number string (\"43650\" or \"43650,43651\") or an \
             array of changelist numbers.",
        )),
    }
}

/// Render a changelist list for a message, or "none" when empty.
fn join_changes(changes: &[u64]) -> String {
    if changes.is_empty() {
        return "none".to_string();
    }
    changes
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ")
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

fn fmt_elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        ),
    }
}

fn render_progress(snapshot: &Snapshot, turn_budget: Duration, reassure: bool) -> String {
    let observed = if snapshot.phase == Phase::Reviewing {
        "reviewer process confirmed alive"
    } else {
        "worker phase updated"
    };
    let mut message = format!(
        "{} for {}; {} elapsed overall; {observed} {} ago",
        snapshot.phase.as_str(),
        fmt_elapsed(snapshot.phase_elapsed),
        fmt_elapsed(snapshot.elapsed),
        fmt_elapsed(snapshot.activity_age),
    );
    if snapshot.output_bytes > 0 {
        message.push_str(&format!(
            "; {} of reviewer output received",
            fmt_bytes(snapshot.output_bytes)
        ));
    } else if snapshot.phase == Phase::Reviewing {
        message.push_str("; no streamed output yet (some reviewers emit only on completion)");
    }
    message.push('.');
    if reassure {
        message.push_str(&format!(
            " In this project's usage, long reviews are normal and complex changes can take 20 \
             minutes or longer. This turn's configured budget is {}.",
            fmt_elapsed(turn_budget)
        ));
    }
    message
}

fn fmt_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A session record identical to what `cfg` would create, aged and lengthened to order.
    fn record_matching(
        cfg: &Config,
        turns: u32,
        updated_unix: u64,
    ) -> crate::session::SessionRecord {
        crate::session::SessionRecord {
            reviewer: cfg.reviewer.as_str().to_string(),
            cli_session_id: "sid-1".to_string(),
            model: cfg.model.clone(),
            effort: cfg.effort.clone(),
            cwd: cfg.cwd.to_string_lossy().to_string(),
            turns,
            created_unix: 0,
            updated_unix,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
        }
    }

    #[test]
    fn a_fresh_short_matching_session_resumes() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        let ok = record_matching(&cfg, cfg.resume_max_turns - 1, now - 10);
        assert!(resume_block(&cfg, &ok, &[], now).is_none());
    }

    #[test]
    fn reaching_the_turn_limit_refuses_resume() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        // One below the cap still resumes; the cap itself does not.
        assert!(resume_block(
            &cfg,
            &record_matching(&cfg, cfg.resume_max_turns - 1, now),
            &[],
            now
        )
        .is_none());
        let reason = resume_block(
            &cfg,
            &record_matching(&cfg, cfg.resume_max_turns, now),
            &[],
            now,
        )
        .expect("at the cap it is refused");
        assert!(reason.contains("turn"), "{reason}");
        assert!(reason.contains("--session-max-turns"), "{reason}");
    }

    #[test]
    fn passing_the_idle_window_refuses_resume() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        let idle = cfg.resume_max_idle.as_secs();
        // Exactly at the window still resumes; one second past it does not.
        assert!(resume_block(&cfg, &record_matching(&cfg, 1, now - idle), &[], now).is_none());
        let reason = resume_block(&cfg, &record_matching(&cfg, 1, now - idle - 1), &[], now)
            .expect("past the window it is refused");
        assert!(reason.contains("resume window"), "{reason}");
        assert!(reason.contains("--session-max-idle-seconds"), "{reason}");
    }

    #[test]
    fn identity_mismatch_refuses_rather_than_silently_starting_fresh() {
        // reviewer, model and working root each pin the session. A configuration that now
        // points elsewhere is told so explicitly rather than being handed a fresh review.
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;

        let mut wrong_reviewer = record_matching(&cfg, 1, now);
        wrong_reviewer.reviewer = "claude".to_string();
        assert!(resume_block(&cfg, &wrong_reviewer, &[], now)
            .expect("refused")
            .contains("reviewer"));

        let mut wrong_model = record_matching(&cfg, 1, now);
        wrong_model.model = "gpt-5.6-sol".to_string();
        assert!(resume_block(&cfg, &wrong_model, &[], now)
            .expect("refused")
            .contains("model"));

        let mut wrong_cwd = record_matching(&cfg, 1, now);
        wrong_cwd.cwd = "C:\\somewhere\\else".to_string();
        assert!(resume_block(&cfg, &wrong_cwd, &[], now)
            .expect("refused")
            .contains("working root"));

        // A case-only difference in the working root is the same root on Windows.
        let mut cased = record_matching(&cfg, 1, now);
        cased.cwd = cfg.cwd.to_string_lossy().to_uppercase();
        assert!(resume_block(&cfg, &cased, &[], now).is_none());
    }

    #[test]
    fn a_session_cannot_be_resumed_under_a_different_backend() {
        // A git record must never satisfy the Perforce binding logic (it carries no `changes`,
        // which would otherwise read as "unbound"), and vice versa.
        let git = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("git cfg");
        let p4 = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--vcs".into(),
            "perforce".into(),
        ])
        .expect("p4 cfg");
        let now = 1_000_000;

        let mut git_record = record_matching(&git, 1, now);
        git_record.backend = Some("git".into());
        assert!(resume_block(&p4, &git_record, &[42], now)
            .expect("refused")
            .contains("backend"));

        let mut p4_record = record_matching(&p4, 1, now);
        p4_record.backend = Some("perforce".into());
        assert!(resume_block(&git, &p4_record, &[], now)
            .expect("refused")
            .contains("backend"));
    }

    #[test]
    fn zero_disables_the_staleness_checks() {
        // A session that is both ancient and very long still resumes when both caps are off.
        let cfg = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--session-max-turns".into(),
            "0".into(),
            "--session-max-idle-seconds".into(),
            "0".into(),
        ])
        .expect("config");
        let now = 1_000_000;
        let ancient_and_long = record_matching(&cfg, 999, 0);
        assert!(resume_block(&cfg, &ancient_and_long, &[], now).is_none());
    }

    #[test]
    fn change_arg_accepts_string_comma_and_array_forms() {
        // All three shapes normalise to the same deduped list through the shared core.
        assert_eq!(
            parse_change_arg(&json!({"change": "43650"})).unwrap(),
            vec![43650]
        );
        assert_eq!(
            parse_change_arg(&json!({"change": "43650,43651"})).unwrap(),
            vec![43650, 43651]
        );
        assert_eq!(
            parse_change_arg(&json!({"change": ["43650", "43651"]})).unwrap(),
            vec![43650, 43651]
        );
        // JSON numbers are accepted, and an array element may itself carry a comma.
        assert_eq!(
            parse_change_arg(&json!({"change": [43650, "43651,43650"]})).unwrap(),
            vec![43650, 43651]
        );
        // Absent means empty; the backend requirement is enforced by the caller.
        assert!(parse_change_arg(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn change_arg_rejects_malformed_input_as_agent_correctable() {
        for bad in [
            json!({"change": "default"}),
            json!({"change": "-3"}),
            json!({"change": "0"}),
            json!({"change": ["43650", "nope"]}),
            json!({"change": [43650.5]}),
            json!({"change": 43650}), // a bare number, not a string or array
            json!({"change": true}),
        ] {
            let err = parse_change_arg(&bad).unwrap_err();
            assert_eq!(err.code, "BAD_REQUEST", "{bad}");
        }
    }

    #[test]
    fn a_perforce_review_requires_change_and_git_refuses_it() {
        // Perforce with no `change`: a request error, not a silent review of the tree.
        let p4 = App::new(
            Config::from_args(&[
                "--reviewer".into(),
                "codex".into(),
                "--vcs".into(),
                "perforce".into(),
            ])
            .expect("cfg"),
        );
        let err = p4
            .start_review(&json!({"instructions": "look"}), &RequestCancel::new())
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(
            err.summary.contains("'change' is required"),
            "{}",
            err.summary
        );

        // git handed a `change`: rejected with a message pointing back at git.
        let git = App::new(
            Config::from_args(&[
                "--reviewer".into(),
                "codex".into(),
                "--vcs".into(),
                "git".into(),
            ])
            .expect("cfg"),
        );
        let err = git
            .start_review(
                &json!({"instructions": "look", "change": "43650"}),
                &RequestCancel::new(),
            )
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(
            err.summary.contains("this working root is git"),
            "{}",
            err.summary
        );
    }

    #[test]
    fn include_shelved_is_parsed_strictly_and_rejected_under_git() {
        let p4 = App::new(
            Config::from_args(&[
                "--reviewer".into(),
                "codex".into(),
                "--vcs".into(),
                "perforce".into(),
            ])
            .expect("cfg"),
        );
        // A non-boolean include_shelved is a request error, not a silent false.
        let err = p4
            .start_review(
                &json!({"instructions": "x", "change": "43650", "include_shelved": "yes"}),
                &RequestCancel::new(),
            )
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.summary.contains("include_shelved"), "{}", err.summary);

        // Under git it is a Perforce-only input and is refused rather than silently ignored.
        let git = App::new(
            Config::from_args(&[
                "--reviewer".into(),
                "codex".into(),
                "--vcs".into(),
                "git".into(),
            ])
            .expect("cfg"),
        );
        let err = git
            .start_review(
                &json!({"instructions": "x", "include_shelved": true}),
                &RequestCancel::new(),
            )
            .unwrap_err();
        assert_eq!(err.code, "BAD_REQUEST");
        assert!(err.summary.contains("Perforce inputs"), "{}", err.summary);
    }

    #[test]
    fn a_perforce_session_is_bound_to_its_changelist_set() {
        // A record created under Perforce, bound to a changelist set.
        let cfg = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--vcs".into(),
            "perforce".into(),
        ])
        .expect("cfg");
        let now = 1_000_000;
        let mut record = record_matching(&cfg, 1, now);
        record.changes = Some(vec![43650, 43651]);

        // Same set (any order, canonicalised by the caller) resumes.
        assert!(resume_block(&cfg, &record, &[43650, 43651], now).is_none());
        // A different set is refused, naming both sets and the escape hatch.
        let reason = resume_block(&cfg, &record, &[43650], now).expect("refused");
        assert!(reason.contains("43650, 43651"), "{reason}");
        assert!(reason.contains("fresh: true"), "{reason}");

        // A record with no binding (legacy or git) is treated as unbound and resumes.
        let mut unbound = record.clone();
        unbound.changes = None;
        assert!(resume_block(&cfg, &unbound, &[99999], now).is_none());
    }

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
        assert!(out.contains("progress:  preparing the review"), "{out}");
        assert!(out.contains("configured budget"), "{out}");
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
        assert!(
            !out.contains("long reviews are normal"),
            "shutdown advice contradicted itself: {out}"
        );
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

    #[test]
    fn elapsed_formatting_handles_unit_boundaries() {
        assert_eq!(fmt_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(fmt_elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3599)), "59m 59s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3600)), "1h 00m 00s");
    }

    #[test]
    fn byte_formatting_handles_unit_boundaries() {
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1 KiB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MiB");
    }
}
