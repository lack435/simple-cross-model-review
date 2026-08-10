//! The four tools, and the state they share.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::cancel::RequestCancel;
use crate::config::{Config, ReviewerSpec, UsageMinimum};
use crate::errors::{self, Failure};
use crate::metrics::{self, MetricsLog};
use crate::prompt::{self, PromptParts, DEFAULT_PREAMBLE};
use crate::registry::{
    IdState, Outcome, Phase, Registry, Snapshot, StartRefused, Status, MAX_TERMINAL_PER_SESSION,
    MAX_TERMINAL_TOTAL,
};
use crate::reviewer::{self, Headroom, Reviewer};
use crate::session::{self, now_unix, ExclusiveLock, SessionStore};
use crate::usage::HeadroomStore;
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

/// Per-chain-entry preflight cache, keyed by entry index. Shared (via `Arc`) between `App` and the
/// worker's fall-through walk, so each entry is resolved and auth-checked at most once across the
/// selected-entry check, `status`, and the walk.
type PreflightCache = Arc<Mutex<std::collections::HashMap<usize, Preflight>>>;

/// Resolve and auth-check a chain entry, returning a cached result when present and caching a
/// fresh one otherwise. Successful preflights are cached for the process lifetime; failures are
/// not, so a user who runs `codex login` can retry without restarting the agent session. The
/// adapter is selected per entry (`for_kind`), so a mixed-family chain preflights each entry with
/// its own CLI.
fn ensure_entry_ready(
    cache: &Mutex<std::collections::HashMap<usize, Preflight>>,
    cfg: &Config,
    index: usize,
    cancel: &AtomicBool,
) -> Result<Preflight, Failure> {
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&index)
        .cloned()
    {
        return Ok(cached);
    }
    let spec = &cfg.reviewers[index];
    let bin = reviewer::resolve_bin(spec)?;
    let auth = reviewer::for_kind(spec.reviewer).auth_check(&bin, cfg, cancel)?;
    let ready = Preflight { bin, auth };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(index, ready.clone());
    Ok(ready)
}

/// The usage-headroom store key for a chain entry, or `None` when the entry cannot be keyed and
/// so is never gated: the chain is not armed, the entry has no minimum, its binary cannot be
/// resolved, or its account fingerprint cannot be read. Reads only cheap local sources — a
/// PATH resolve and a local account file — so it neither auth-checks nor spawns; it is safe to
/// call during selection, before any preflight. The account fingerprint is read *here*, once,
/// so the gate decision and the later store write for this entry share one reading (no
/// read-time/observe-time TOCTOU). See `docs/usage-remaining-gate.md`.
fn usage_headroom_key(cfg: &Config, spec: &ReviewerSpec) -> Option<String> {
    if !cfg.chain_gates_on_usage() {
        return None;
    }
    let bin = reviewer::resolve_bin(spec).ok()?;
    let account = reviewer::for_kind(spec.reviewer).account_fingerprint(cfg, spec)?;
    Some(crate::usage::entry_key(
        spec.reviewer.as_str(),
        &bin,
        &account,
    ))
}

/// The terminal `REVIEWERS_EXHAUSTED` failure, worded for the actual cause (round-6 finding f7):
/// pure rate-limited (today's exact detail, so the single-reviewer path is unchanged), pure
/// proactive-gate, or a mix. `rate` holds `describe_with_bin` strings of rate-limited entries;
/// `gated` holds already-reasoned describe strings of gated entries. See
/// `docs/usage-remaining-gate.md`.
fn exhaustion_failure(rate: &[String], gated: &[String]) -> Failure {
    if gated.is_empty() {
        errors::reviewers_exhausted(format!(
            "every configured reviewer reported a rate/usage limit, in order: {}",
            rate.join("; ")
        ))
    } else if rate.is_empty() {
        errors::reviewers_exhausted_gated(format!(
            "every configured reviewer was skipped for low usage remaining, in order: {}",
            gated.join("; ")
        ))
    } else {
        let mut parts: Vec<String> = rate.iter().map(|d| format!("{d} (rate-limited)")).collect();
        parts.extend(gated.iter().cloned());
        errors::reviewers_exhausted_mixed(format!(
            "every configured reviewer was exhausted (rate limit or usage minimum): {}",
            parts.join("; ")
        ))
    }
}

/// The result of the fresh-review proactive gate selection: the entry to start on, the
/// non-billed skips recorded before it, and that entry's usage-store key (one fingerprint
/// reading, carried so the later store write matches the gate decision). See
/// `docs/usage-remaining-gate.md`.
struct FreshSelection {
    start_index: usize,
    pre_start_skips: Vec<metrics::Attempt>,
    pre_start_gated_descs: Vec<String>,
    start_usage_key: Option<String>,
}

/// Build the non-billed metrics `Attempt` that records a gated skip (no spawn, no usage). Kept
/// as one helper so the pre-start selection and the in-walk fallback gate record it identically.
/// `resolved_bin` is the binary selection already resolved for the store key, so two
/// installations of the same reviewer stay distinguishable in the log (round-1-impl finding f8).
fn gated_skip_attempt(spec: &ReviewerSpec, resolved_bin: Option<PathBuf>) -> metrics::Attempt {
    metrics::Attempt {
        reviewer: spec.reviewer.as_str().to_string(),
        model: spec.model.clone(),
        effort: spec.effort.clone(),
        resolved_bin: resolved_bin.map(|p| p.to_string_lossy().into_owned()),
        failure_code: "USAGE_BELOW_MINIMUM".to_string(),
        wall_secs: 0,
        prompt_bytes: 0,
        billed: false,
    }
}

pub struct App {
    cfg: Arc<Config>,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    metrics: Arc<MetricsLog>,
    /// Persisted last-observed usage headroom per reviewer entry, for the proactive gate. Shared
    /// with the worker so an attempt can record its observation. See `docs/usage-remaining-gate.md`.
    usage: Arc<HeadroomStore>,
    preflight: PreflightCache,
    /// Set when the reviewer chain is semantically invalid (`Config::validate_chain`). While
    /// present, every review is refused in-band with this `INVALID_REVIEWER_CHAIN` failure
    /// rather than the server exiting — see `docs/reviewer-fallback-chain.md`. Checked before
    /// the session lease and before any reviewer preflight, so an invalid chain touches nothing.
    chain_error: Option<Failure>,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let sessions = SessionStore::new(&cfg.state_dir);
        let metrics = MetricsLog::new(&cfg.state_dir, cfg.metrics);
        let usage = HeadroomStore::new(&cfg.state_dir);
        // The chain's semantics are validated once, here. On failure the server still starts,
        // but every review is refused in-band with this failure (see `chain_error`).
        let chain_error = Config::validate_chain(&cfg.reviewers)
            .err()
            .map(errors::invalid_reviewer_chain);
        // Read before `cfg` moves into the Arc below.
        let cfg_max_concurrent = cfg.max_concurrent_reviews;
        Self {
            cfg: Arc::new(cfg),
            registry: Arc::new(Registry::with_max_concurrent(cfg_max_concurrent)),
            sessions: Arc::new(sessions),
            metrics: Arc::new(metrics),
            usage: Arc::new(usage),
            preflight: Arc::new(Mutex::new(std::collections::HashMap::new())),
            chain_error,
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

    /// Wake parked `cross_model_review_result` waiters so a poll whose request was just cancelled
    /// returns promptly instead of parking out its budget. The review is left running; only the
    /// wait ends. Called from `handle_cancellation` on the detach path, after the request's
    /// cancelled flag is set.
    pub fn wake_waiters(&self) {
        self.registry.wake();
    }

    /// Register reviews without a reviewer CLI, so the cancellation paths can be tested
    /// without spending a model call.
    #[cfg(test)]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Resolve and auth-check a specific chain entry, caching the result by index.
    ///
    /// Delegates to the shared [`ensure_entry_ready`], which the worker's fall-through walk uses
    /// too (via the same `Arc`-shared cache), so each entry is preflighted at most once across the
    /// selected-entry check here, `status`, and the walk.
    fn ensure_ready_for(&self, index: usize, cancel: &AtomicBool) -> Result<Preflight, Failure> {
        ensure_entry_ready(&self.preflight, &self.cfg, index, cancel)
    }

    /// Proactive gate for a **fresh review**: walk the chain from the primary, skipping any entry
    /// whose last-observed headroom is known-and-below its minimum, and return the first entry
    /// that clears along with the skips recorded before it. `Err(REVIEWERS_EXHAUSTED)` when every
    /// entry is gated out. Resolves bins and reads local account files only — no auth preflight,
    /// no spawn, no capture — so it is safe to run before the session lease. See
    /// `docs/usage-remaining-gate.md`.
    fn gate_fresh_selection(&self, now: u64) -> Result<FreshSelection, Failure> {
        let mut pre_start_skips: Vec<metrics::Attempt> = Vec::new();
        let mut pre_start_gated_descs: Vec<String> = Vec::new();
        for (i, spec) in self.cfg.reviewers.iter().enumerate() {
            // The key resolves the bin (a cheap PATH scan, no auth) and reads the account
            // fingerprint from a local file; `None` when the chain is unarmed or identity cannot
            // be established, in which case the entry is never gated (fail-open).
            let key = usage_headroom_key(&self.cfg, spec);
            if spec.usage_minimum.is_gating() {
                if let Some(k) = &key {
                    if !self.usage.get(k, now).clears(&spec.usage_minimum) {
                        pre_start_skips
                            .push(gated_skip_attempt(spec, reviewer::resolve_bin(spec).ok()));
                        pre_start_gated_descs
                            .push(format!("{} (usage below minimum)", spec.describe()));
                        continue;
                    }
                }
            }
            return Ok(FreshSelection {
                start_index: i,
                pre_start_skips,
                pre_start_gated_descs,
                start_usage_key: key,
            });
        }
        Err(errors::reviewers_exhausted_gated(format!(
            "every configured reviewer was skipped for low usage remaining, in order: {}",
            pre_start_gated_descs.join("; ")
        )))
    }

    // -----------------------------------------------------------------------
    // cross_model_review
    // -----------------------------------------------------------------------

    pub fn start_review(&self, args: &Value, request: &RequestCancel) -> Result<String, Failure> {
        // An invalid reviewer chain refuses every review, before the session lease and before any
        // reviewer preflight, so nothing is resolved or billed against a chain known to be broken.
        if let Some(err) = &self.chain_error {
            return Err(err.clone());
        }

        let instructions = string_arg(args, "instructions")
            .ok_or_else(|| errors::bad_request("'instructions' is required and must be a non-empty string describing what to review."))?;

        let session = string_arg(args, "session").unwrap_or_else(|| DEFAULT_SESSION.to_string());
        // A session name is rendered into the human response and keys on-disk marker files, so it
        // must be a single line of printable text. Rejecting control characters (newlines above all)
        // is defence-in-depth: it stops a name from breaking the response layout, and — together
        // with the marker-line strip applied to the whole rendered body — from smuggling a forged
        // envelope block into the text channel.
        if session.chars().any(|c| c.is_control()) {
            return Err(errors::bad_request(
                "'session' must be a single line of text: control characters (including newlines) \
                 are not allowed in a session name.",
            ));
        }
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

        // Fresh-review usage gate, before the session lease and store read: a `fresh: true` call
        // is a fresh review by definition (no prior), so if the whole chain is gated out we refuse
        // immediately — touching no lease, store, marker, preflight, or capture (round-1-impl
        // finding f5). A non-fresh call might be a resume (never gated), so its fresh-review gate
        // (for a genuinely new session) waits until the store read below confirms there is no
        // prior; reading before the lease would be the stale-read race the lease exists to prevent.
        let mut pre_lease_fresh_sel = if fresh {
            Some(self.gate_fresh_selection(now_unix())?)
        } else {
            None
        };

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

        // A corrupt session store is refused before anything is spawned or billed — resume *and*
        // fresh alike. An unreadable store cannot be safely read or written, so a `fresh` call
        // would clobber unreadable state (or merge into it) and a resume cannot find its record;
        // either way a silent clean start could converge on history it cannot see. Recovery is an
        // explicit operator action (move the corrupt store aside, or point --state-dir elsewhere),
        // never something the server does silently. A missing store is `Absent`, the normal
        // first-run state, and passes.
        if self.sessions.store_state() == crate::session::StoreState::Corrupt {
            return Err(errors::store_corrupt(
                &session,
                &self.sessions.path().display().to_string(),
            ));
        }

        // Decide whether this is a new review or another turn on an existing one. `get_checked` is
        // fail-closed: a transient read error is refused here rather than silently read as "no
        // record" (which would then overwrite the real record on `record_turn`).
        let prior = if fresh {
            None
        } else {
            self.sessions.get_checked(&session).map_err(|e| {
                errors::session_not_resumable(
                    &session,
                    format!(
                        "the session store could not be read ({e}); refusing rather than risk \
                         overwriting it. Retry, or point --state-dir at a readable directory."
                    ),
                )
            })?
        };

        // The findings write-ahead marker is checked for *every* non-fresh call — with or without a
        // stored record. A set marker means the previous turn did not durably record its ledger, so
        // resuming (or continuing under this name) would lose that turn and could converge on
        // untracked history. Crucially this fires even when no record exists: a fresh turn 1 that
        // marked, then failed to record, leaves the marker but no record, and the next non-fresh
        // call must not silently start a clean, convergeable turn 1 over it. Fail-closed: an
        // unreadable marker refuses too. `fresh: true` (which sets `prior = None` by choice) is the
        // explicit escape hatch and skips this. See also the Perforce note below.
        if !fresh
            && matches!(
                self.sessions.findings_marker_state(&session),
                crate::session::MarkerState::Present | crate::session::MarkerState::Unreadable
            )
        {
            return Err(resume_refusal(
                &session,
                "the previous turn did not finish durably (its findings write-ahead marker is \
                 still set), so its findings ledger may be stale or the turn was lost. Start a \
                 fresh review (fresh: true) carrying any still-open findings into the new \
                 instructions."
                    .to_string(),
                prior.as_ref(),
            ));
        }

        // A stored session that exists but must not be resumed is refused here, while the
        // lease is held and before anything is spawned or billed, so the caller explicitly
        // chooses to start fresh (fresh=true, or a new session name) rather than being
        // silently handed a review with no memory of the work it asked to continue.
        //
        // Perforce contract note: a set findings marker refuses the resume here, which pre-empts the
        // Perforce full-capture fallback in `Job::run` — but *only* for failures where the reviewer
        // may have advanced its conversation, so the ledger really could be stale and a full diff
        // recapture would not repair it. The two cases where the ledger is provably *not* stale keep
        // the fallback reachable, because the marker is not left set: a crash *before* the findings
        // marker is written (during capture), and a reviewer failure that started no child process
        // (`invocation` build error or `Command::spawn` failure), which `attempt` detects and clears
        // the marker for on any non-fresh call (its marker was confirmed absent on entry here). So
        // the refusal covers exactly the stale-ledger case, and the Perforce full-recapture path
        // survives for the not-stale ones.
        if let Some(record) = &prior {
            if let Some(reason) = resume_block(&self.cfg, record, &changes_canonical, now_unix()) {
                return Err(resume_refusal(&session, reason, Some(record)));
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

        // Select the entry to start on, then preflight *only* that entry: for a fresh review the
        // primary (index 0, then the walk); for a resume the entry that created the session
        // (matched above by `resume_block`, so `resume_entry_index` is `Some`). This is what keeps
        // a resume of a fell-back session from dying on the primary's CLI. Preflight runs here,
        // under the lease and after selection, never unconditionally against the primary.
        //
        // A **resume** is never gated — it runs its bound entry, and if genuinely out takes the
        // reactive RATE_LIMITED path with its `fresh: true` remediation — but its usage key is
        // still computed so the resumed turn records its own headroom observation (round-1-impl
        // finding f4). A **fresh review** was already gate-selected: a `fresh: true` call before
        // the lease, and a non-fresh call on a genuinely new session here (after the store read
        // confirmed no prior). See `docs/usage-remaining-gate.md`.
        let FreshSelection {
            start_index,
            pre_start_skips,
            pre_start_gated_descs,
            start_usage_key,
        } = match &prior {
            Some(record) => {
                let idx = self.cfg.resume_entry_index(record).unwrap_or(0);
                FreshSelection {
                    start_index: idx,
                    pre_start_skips: Vec::new(),
                    pre_start_gated_descs: Vec::new(),
                    start_usage_key: usage_headroom_key(&self.cfg, &self.cfg.reviewers[idx]),
                }
            }
            None => match pre_lease_fresh_sel.take() {
                Some(sel) => sel,
                None => self.gate_fresh_selection(now_unix())?,
            },
        };
        // A `notifications/cancelled` that arrived before the review is even registered still
        // stops setup here; the flag below then interrupts the (bounded) auth check itself.
        if request.is_cancelled() {
            return Err(errors::cancelled());
        }
        // The review has no registry cancel yet (it is not registered until `try_start`), so the
        // selected entry's auth check observes the request's own cancel mirror instead.
        let cancel = request.cancel_flag();
        let ready = match &prior {
            // Resume: resolve the selected entry *uncached* and validate its binary against what
            // the session was created with **before** auth-checking or caching it -- so a PATH
            // change cannot hide behind a stale cached entry, and a rejected resume never leaves a
            // rejected binary in the cache for a later fresh review to reuse.
            Some(record) => {
                let spec = &self.cfg.reviewers[start_index];
                let bin = reviewer::resolve_bin(spec)?;
                if let Some(stored) = &record.resolved_bin {
                    if bin.to_string_lossy() != *stored {
                        return Err(resume_refusal(
                            &session,
                            format!(
                                "it was created with the reviewer binary at '{stored}', but that \
                                 entry now resolves to '{}'; PATH or the install changed, so the \
                                 conversation cannot be resumed through a different executable.",
                                bin.display()
                            ),
                            Some(record),
                        ));
                    }
                }
                // Identity confirmed: now auth-check and cache the validated binary.
                let auth = reviewer::for_kind(spec.reviewer).auth_check(&bin, &self.cfg, cancel)?;
                let ready = Preflight { bin, auth };
                self.preflight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(start_index, ready.clone());
                ready
            }
            None => self.ensure_ready_for(start_index, cancel)?,
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
                StartRefused::TooManyRunning { limit } => errors::too_many_running(limit),
                StartRefused::ShuttingDown => errors::server_shutting_down(),
            })?;

        // Bind the review to the request that created it before the worker starts, so a
        // `notifications/cancelled` arriving mid-setup stops the reviewer instead of
        // finding nothing to stop. `attach_owned`: this request *owns* the review, because its
        // review_id has not been delivered, so a cancellation kills it. If it already arrived,
        // never spawn the CLI at all -- but still record a terminal state, or the session stays
        // claimed by a review that no worker will ever finish.
        if request.attach_owned(&id) {
            self.registry
                .finish(&id, Outcome::failed(errors::cancelled()));
            return Err(errors::cancelled());
        }

        let job = Job {
            cfg: Arc::clone(&self.cfg),
            reviewer: Arc::from(reviewer::for_kind(self.cfg.reviewers[start_index].reviewer)),
            spec: self.cfg.reviewers[start_index].clone(),
            start_index,
            preflight: Arc::clone(&self.preflight),
            registry: Arc::clone(&self.registry),
            sessions: Arc::clone(&self.sessions),
            metrics: Arc::clone(&self.metrics),
            usage: Arc::clone(&self.usage),
            pre_start_skips,
            pre_start_gated_descs,
            start_usage_key,
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
            // Resolve the prior findings state here, from the record validated under the lease.
            // `Some` exactly when resuming (`prior.is_some()`). A `Valid` ledger carries its real
            // coverage and findings; an `Absent` one is a legacy (pre-feature) resume treated as an
            // already-broken `legacy_uncovered` prior so it stays non-convergent rather than posing
            // as a fresh turn 1. `Invalid` cannot occur — `resume_block` already refused it above —
            // but is mapped to the same legacy fallback defensively.
            prior_findings: prior.as_ref().map(|record| match record.ledger_load() {
                session::LedgerLoad::Valid(l) => crate::findings::PriorState {
                    coverage: l.coverage,
                    next_seq: l.next_seq,
                    findings: l.findings,
                },
                session::LedgerLoad::Absent | session::LedgerLoad::Invalid => {
                    crate::findings::PriorState {
                        coverage: crate::findings::LedgerCoverage::LegacyUncovered,
                        next_seq: 1,
                        findings: Vec::new(),
                    }
                }
            }),
            // Non-fresh calls passed the findings gate, so their marker was absent on entry; fresh
            // calls skipped it. Used to decide whether a pre-launch failure may clear the marker.
            findings_marker_absent_on_entry: !fresh,
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
                    self.cfg.reviewers[start_index].reviewer.as_str(),
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
        out.push_str(&format!(
            "budget:    {}\n\n",
            fmt_elapsed(Duration::from_secs(self.cfg.max_wait_secs()))
        ));
        out.push_str(&format!(
            "Collect it with cross_model_review_result using review_id \"{id}\". That call blocks \
             until the review is done -- omit wait_seconds to wait to completion in one call -- and \
             reports progress while it is open when the MCP client supports progress notifications. \
             If the wait_seconds budget elapses first it returns status=running; if your client's \
             own tool timeout is shorter and fires first you get a client-side timeout instead of a \
             result. Either way the review keeps running -- abandoning a collect does not cancel it \
             -- so just call cross_model_review_result again with the same review_id. Use \
             cross_model_review_cancel to actually stop the reviewer.\n\n\
             In this project's usage, reviews commonly take at least five minutes, and complex \
             changes can take 20 minutes or longer. A running status during that window is normal \
             and is not a reason to start over or cancel the review.\n"
        ));
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // cross_model_review_result
    // -----------------------------------------------------------------------

    /// Resolve the target review, attach as a waiter, and wait up to the budget — returning the
    /// single final snapshot (and how long we waited). Both the text and `structuredContent`
    /// channels render from this *one* snapshot, so they can never describe different reviews or
    /// different states of the same review.
    fn collect_snapshot(
        &self,
        args: &Value,
        request: &RequestCancel,
    ) -> Result<(Snapshot, u64), Failure> {
        let review_id = string_arg(args, "review_id");
        let session = string_arg(args, "session");

        // An omitted wait_seconds blocks to completion: the default is the full cap, so the
        // ergonomic no-argument collect is the one blocking call the caller wants. A caller that
        // only wants a snapshot passes wait_seconds=0. The cap tracks the review budget rather than
        // a fixed 300, so a single call can cover a whole 20-minute review; see
        // docs/single-blocking-collect.md.
        let max_wait = self.cfg.max_wait_secs();
        let wait = args
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(max_wait)
            .min(max_wait);

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

        // Abandoning this call detaches the wait; it does NOT stop the review. `attach_wait`
        // binds this request as a waiter, not an owner, so a `notifications/cancelled` leaves the
        // reviewer running and the result collectible by review_id -- only the poll stops. This is
        // the whole point of the change: a client tool-timeout (which the server cannot tell from a
        // real cancellation) no longer destroys a review that was still coming, which is what lets
        // the wait cap track the review budget. Explicit destruction goes through
        // cross_model_review_cancel. See docs/single-blocking-collect.md.
        //
        // The pre-attach race is handled without killing either: if the cancellation already
        // arrived, return CANCELLED but leave the review alone. Mechanically calling
        // `registry.cancel` here would reintroduce the destroy-on-race behaviour on this narrow
        // window, so it deliberately does not.
        if request.attach_wait(&id) {
            return Err(errors::cancelled());
        }

        // Ending this wait when the cancellation lands mid-poll is `registry.wake()`'s job (driven
        // from `handle_cancellation`): the waiter re-checks its own cancelled flag and returns,
        // leaving the review running. The closure below is that check.
        //
        // `wait` can return None for an id that was Known a moment ago: a concurrent
        // finish elsewhere sweeps the caps, and this id can be what it drops. Re-checking
        // costs one lock and keeps the distinction the lookup above just drew, instead of
        // collapsing it into an opaque "no longer tracked".
        let snapshot = match self
            .registry
            .wait(&id, Duration::from_secs(wait), &|| request.is_cancelled())
        {
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

        Ok((snapshot, wait))
    }

    /// Collect a review and render just the text channel. Production goes through
    /// [`review_result_both`](Self::review_result_both) (which also produces `structuredContent`
    /// from the same snapshot); this text-only entry point is retained for tests that assert on the
    /// rendered text.
    #[cfg(test)]
    pub fn review_result(&self, args: &Value, request: &RequestCancel) -> Result<String, Failure> {
        let (snapshot, wait) = self.collect_snapshot(args, request)?;
        self.render_snapshot(&snapshot, wait)
    }

    /// Collect a review once and render *both* channels from the same snapshot: the text, plus the
    /// `structuredContent` envelope value when there is one (a `Failed` result carries neither — it
    /// is returned as an error). Used by the MCP dispatch so the two channels always agree.
    pub fn review_result_both(
        &self,
        args: &Value,
        request: &RequestCancel,
    ) -> Result<(String, Option<Value>), Failure> {
        let (snapshot, wait) = self.collect_snapshot(args, request)?;
        let text = self.render_snapshot(&snapshot, wait)?;
        Ok((text, self.snapshot_structured_content(&snapshot)))
    }

    /// Render the text channel for a collected snapshot. `Failed` becomes the tool error.
    fn render_snapshot(&self, snapshot: &Snapshot, wait: u64) -> Result<String, Failure> {
        match snapshot.status {
            Status::Running => Ok(self.render_running(snapshot, wait)),
            Status::Completed => Ok(self.render_completed(snapshot)),
            Status::Failed => {
                // Preserve the active-entry attribution the completed path renders: a failed
                // review names the entry that ran, not just the reviewer family in the message.
                let failure = snapshot
                    .failure
                    .clone()
                    .unwrap_or_else(|| {
                        errors::empty_review(self.cfg.primary().reviewer.as_str(), "")
                    })
                    .with_active_note(snapshot.active.as_deref());
                Err(failure)
            }
        }
    }

    /// The `structuredContent` value for a collected snapshot: the completed envelope's value, or
    /// the reduced running variant, or `None` for a failed result (which is an error, not a
    /// structured payload). The text channel always carries the same envelope in its `_OUT` block.
    fn snapshot_structured_content(&self, snapshot: &Snapshot) -> Option<Value> {
        match snapshot.status {
            Status::Completed => snapshot.envelope.as_ref().map(|e| e.to_structured_value()),
            Status::Running => Some(crate::findings::running_structured_value(
                &snapshot.session,
                snapshot.turn,
                running_progress_of(snapshot),
            )),
            Status::Failed => None,
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
        let body = format!(
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
            snapshot
                .active
                .clone()
                .unwrap_or_else(|| self.cfg.describe_reviewer()),
            snapshot.elapsed.as_secs(),
            self.cfg.max_wait_secs(),
            render_progress(
                snapshot,
                Duration::from_secs(self.cfg.max_wait_secs()),
                !snapshot.shutting_down,
            ),
        );
        // Strip any sentinel marker line from the body (e.g. injected via the session name) before
        // appending the one canonical running `_OUT` block, so a text-only client parses exactly one
        // nonce-bearing block whether the review is running or done. The running variant carries no
        // convergence/findings group — there is nothing to decide yet — but does carry the same
        // progress/liveness the text line shows.
        let out_block = crate::findings::running_out_block(
            &snapshot.id,
            &snapshot.session,
            snapshot.turn,
            running_progress_of(snapshot),
        );
        format!(
            "{}\n{out_block}\n",
            crate::findings::strip_marker_lines(&body)
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
        (snapshot.status == Status::Running).then(|| {
            render_progress(
                &snapshot,
                Duration::from_secs(self.cfg.max_wait_secs()),
                !snapshot.shutting_down,
            )
        })
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
            snapshot
                .active
                .clone()
                .unwrap_or_else(|| self.cfg.describe_reviewer()),
            snapshot.elapsed.as_secs(),
        ));

        // Stated on every completed review rather than kept in the log alone. A review
        // turn is many model calls over a conversation that grows with each turn, and an
        // agent that cannot see what a turn cost has no way to notice that its tenth
        // follow-up costs several times what its first did.
        if !snapshot.usage.is_empty() {
            out.push_str(&format!("usage:     {}\n\n", snapshot.usage.summary()));
        }

        // What the server captured and sent this turn: the resolved command/range, a size
        // summary, whether the diff hit the byte cap, and whether the capture was otherwise
        // complete. Present whenever a change was sent -- on a fresh turn as well as a resume, so
        // it sits above the resume-only `disposition:` line. A caller can confirm the reviewer saw
        // the intended change from this alone, without re-running git or p4. When the capture was
        // partial, this line points at the WARNING lines rendered just below.
        if let Some(summary) = &snapshot.capture_summary {
            out.push_str(&format!("captured:  {}\n\n", summary.summary()));
        }

        // Only on a resumed turn that sent a change; a fresh or no-change turn carries `None`.
        // This is informational -- it says what the server *sent* this turn (a delta, or the whole
        // change and why). A fall-back that the caller was configured for also raises a WARNING
        // below; a clean delta or a by-design full capture does not.
        if let Some(disposition) = &snapshot.disposition {
            out.push_str(&format!("disposition: {}\n\n", disposition.summary()));
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

        // Neutralise any sentinel marker line anywhere in the assembled body — from the session
        // name, a warning, a denied-command string, or prose bearing a stale/foreign nonce — before
        // appending the one canonical `_OUT` block. This is what guarantees a text-only client sees
        // exactly one parseable envelope block and it is the server's, not attacker-controlled.
        let mut out = crate::findings::strip_marker_lines(&out);

        // The machine-readable envelope on the text channel too, so a client that reads only
        // `content[].text` still gets the structured verdict without a second round trip. Exactly
        // one server `_OUT` block, bearing this review's nonce, appended after the strip above.
        if let Some(envelope) = &snapshot.envelope {
            out.push('\n');
            out.push_str(&envelope.to_out_block(&snapshot.id));
            out.push('\n');
        }
        out
    }

    /// One `status`/`--doctor` line describing an entry's proactive usage gate: its configured
    /// minimum and its last-observed headroom. `None` when the chain is not armed (the gate is
    /// inert, so there is nothing to show). Reads only the store — no additional CLI call beyond
    /// the auth check `status` already performs. See `docs/usage-remaining-gate.md`.
    fn usage_gate_status_line(&self, spec: &ReviewerSpec) -> Option<String> {
        if !self.cfg.chain_gates_on_usage() {
            return None;
        }
        let level_name = |l: crate::reviewer::HeadroomLevel| match l {
            crate::reviewer::HeadroomLevel::Ample => "ample",
            crate::reviewer::HeadroomLevel::Warning => "warning",
            crate::reviewer::HeadroomLevel::Exhausted => "exhausted",
        };
        let min = match spec.usage_minimum {
            UsageMinimum::None => "no gate".to_string(),
            UsageMinimum::Remaining(p) => format!("skip below {p}% remaining"),
            UsageMinimum::Status(l) => format!("skip below '{}'", level_name(l)),
        };
        let now = now_unix();
        let observed = match usage_headroom_key(&self.cfg, spec) {
            None => "unknown (identity unavailable)".to_string(),
            Some(key) => match self.usage.observation(&key, now) {
                None => "none recorded yet".to_string(),
                Some(o) => {
                    let value = match o.headroom {
                        Headroom::Unknown => "unknown".to_string(),
                        Headroom::Fraction { remaining_pct, .. } => {
                            format!("{remaining_pct:.0}% remaining")
                        }
                        Headroom::Level { level, .. } => level_name(level).to_string(),
                    };
                    let age = fmt_elapsed(Duration::from_secs(now.saturating_sub(o.observed_at)));
                    let resets = match o.resets_at {
                        Some(r) if r > now => {
                            format!(", resets in {}", fmt_elapsed(Duration::from_secs(r - now)))
                        }
                        Some(_) => ", window reset".to_string(),
                        None => String::new(),
                    };
                    // `actionable` is what the gate would actually use: a past-reset or
                    // TTL-aged observation reads as no-longer-gating even though the value is
                    // still shown (round-1-impl finding f9).
                    let status = if o.actionable {
                        "actionable"
                    } else {
                        "aged out - not gating"
                    };
                    format!("{value} ({age} ago{resets}; {status})")
                }
            },
        };
        Some(format!("usage gate:    {min}; last observed: {observed}\n"))
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

        // A semantically invalid chain is reported here first: the reviewer CLIs may be fine, but
        // no review can run until the configuration is fixed.
        if let Some(err) = &self.chain_error {
            out.push_str("ready:         NO\n");
            out.push_str(&format!("problem:       {} - {}\n", err.code, err.summary));
            if let Some(detail) = &err.detail {
                out.push_str(&format!("chain:         {detail}\n"));
            }
            out.push_str(&format!("working root:  {}\n", self.cfg.cwd.display()));
            return out;
        }

        // Preflight every entry in the chain, not just the primary: a fallback is only useful if
        // its CLI is installed and signed in, and a human running --doctor wants to see that now
        // rather than discover it mid-walk. Each entry uses its own adapter (`for_kind`).
        let multi = self.cfg.reviewers.len() > 1;
        let mut all_ready = true;
        for (i, spec) in self.cfg.reviewers.iter().enumerate() {
            if multi {
                let role = if i == 0 { "primary" } else { "fallback" };
                out.push_str(&format!("\nentry {i} ({role}): {}\n", spec.describe()));
            }
            match self.ensure_ready_for(i, &AtomicBool::new(false)) {
                Ok(ready) => {
                    out.push_str(&format!("cli:           {}\n", ready.bin.display()));
                    out.push_str(&format!(
                        "auth:          {}\n",
                        ready.auth.replace('\n', " ")
                    ));
                    out.push_str("ready:         yes\n");
                }
                Err(failure) => {
                    all_ready = false;
                    out.push_str("ready:         NO\n");
                    out.push_str(&format!(
                        "problem:       {} - {}\n",
                        failure.code, failure.summary
                    ));
                }
            }
            // The proactive usage gate, when the chain is armed: this entry's configured minimum
            // and its last-observed headroom (a store read, no CLI call beyond the auth check
            // above). Claude's signal is categorical, Codex's numeric -- shown as reported.
            if let Some(line) = self.usage_gate_status_line(spec) {
                out.push_str(&line);
            }
        }
        if multi {
            out.push_str(&format!(
                "\nchain ready:   {}\n",
                if all_ready {
                    "yes"
                } else {
                    "NO (see entries above)"
                }
            ));
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

        out.push_str("\nsaved review sessions:\n");
        // `list()` is tolerant (it reads a corrupt store as empty), so a bare "(none yet)" would
        // misreport an unreadable store as a clean first run. Check the store state explicitly so
        // this human-facing report distinguishes "no sessions" from "cannot read sessions".
        if self.sessions.store_state() == crate::session::StoreState::Corrupt {
            out.push_str(&format!(
                "  (cannot read: the session store did not parse -- move {} aside or point \
                 --state-dir at a clean directory)\n",
                self.sessions.path().display()
            ));
            return out;
        }
        let sessions = self.sessions.list();
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
    /// The active chain entry this job runs. Its adapter is `reviewer` and its resolved binary
    /// is `bin`. Every identity-bearing call on the run path reads this, never `cfg.primary()`,
    /// so a fallback names the reviewer that actually ran. The walk re-sets it per entry.
    spec: ReviewerSpec,
    /// The chain index the walk starts on: 0 for a fresh review (it then walks the chain), or the
    /// resume-matched entry (which runs alone, no fall-through). Its bin is already in `self.bin`,
    /// preflighted in `start_review`, so the walk does not re-resolve it.
    start_index: usize,
    /// The `App`'s per-entry preflight cache, shared so the walk's fallback preflights reuse (and
    /// populate) the same cache the selected-entry check and `status` use.
    preflight: PreflightCache,
    registry: Arc<Registry>,
    sessions: Arc<SessionStore>,
    metrics: Arc<MetricsLog>,
    /// The shared usage-headroom store: an attempt records its observation here, keyed by the
    /// active entry's `active_usage_key`. See `docs/usage-remaining-gate.md`.
    usage: Arc<HeadroomStore>,
    /// Non-billed `Attempt`s for entries the proactive gate skipped during pre-start selection,
    /// seeded into the walk's metrics history so a pre-start skip is still recorded on the turn.
    pre_start_skips: Vec<metrics::Attempt>,
    /// Describe strings of the pre-start-gated entries, prepended to a terminal
    /// `REVIEWERS_EXHAUSTED` detail so an exhaustion names every entry and its reason.
    pre_start_gated_descs: Vec<String>,
    /// The store key for the start entry, computed at selection from one fingerprint reading, so
    /// this entry's observation is written under the same identity the gate read (no TOCTOU).
    /// `None` when unarmed or identity could not be established.
    start_usage_key: Option<String>,
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
    /// The prior findings state to evaluate this turn against, resolved from the *already validated*
    /// session record in `start_review` (read under the lease, `Invalid` ledgers already refused by
    /// `resume_block`). Carried in rather than re-read in the worker: a second, fail-open
    /// `SessionStore::get` there could turn a transient read error into an empty `legacy_uncovered`
    /// ledger and then mint fresh ids over a real one. `Some` exactly on a resume; `None` on a fresh
    /// review or a new session.
    prior_findings: Option<crate::findings::PriorState>,
    /// Whether the findings write-ahead marker was confirmed *absent* when this call entered
    /// `start_review` — true for every non-`fresh` call, because the findings gate refuses a
    /// non-fresh call whose marker is set. When true, this turn's `mark_findings_pending` is the only
    /// thing that set the marker, so a pre-launch failure (no child started) may safely clear it. A
    /// `fresh` call is false: it bypasses the gate and may be sitting on a marker an earlier failed
    /// turn set, which it must not drop. Note this is *not* the same as `resume_id.is_some()`: a
    /// non-fresh turn 1 with no record still passed the gate and can clear.
    findings_marker_absent_on_entry: bool,
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

/// Combine the backend's disposition with the framing only the tools layer can supply.
///
/// Free of `Job` so the gate/precedence logic is unit-testable. See [`Job::resolve_disposition`]
/// for the contract and `docs/incremental-resume-disposition.md` for the decision order.
fn assemble_disposition(
    vcs: crate::config::Vcs,
    resume_incremental_diff: bool,
    resumed: bool,
    change_present: bool,
    backend: Option<vcs::Disposition>,
    marker_state: Option<crate::session::MarkerState>,
    pending_marked: bool,
) -> Option<vcs::Disposition> {
    use crate::session::MarkerState;
    use crate::vcs::disposition::{FellBack, FullByDesign};

    // G0: no disposition on a fresh turn or a turn that sent no change.
    if !resumed || !change_present {
        return None;
    }
    if let Some(d) = backend {
        return Some(d);
    }
    // The backend saw no resume baseline. G1: a disabled feature is by-design and wins over the
    // fall-back reasons below, so it never warns -- even when a marker also failed.
    if !resume_incremental_diff {
        return Some(vcs::Disposition::FullByDesign(FullByDesign::Disabled));
    }
    let reason = match vcs {
        crate::config::Vcs::Git => FellBack::NoCompleteBaselineRetained,
        crate::config::Vcs::Perforce => {
            if !pending_marked {
                FellBack::MarkerUnwritable
            } else {
                match marker_state {
                    Some(MarkerState::Present) => FellBack::PriorTurnPending,
                    Some(MarkerState::Unreadable) => FellBack::MarkerStateUnreadable,
                    // Markers are fine, so `resume = None` means there was no prior baseline.
                    _ => FellBack::PriorBaselineMissing,
                }
            }
        }
    };
    Some(vcs::Disposition::FellBackToFull(reason))
}

impl Job {
    /// Combine the backend's disposition with the framing only this layer can supply.
    ///
    /// A disposition exists only on a **resumed** turn that **sent a change** -- the two gates the
    /// backend cannot see (it receives only `Option<Resume>`, which conflates a fresh turn with a
    /// resumed one whose session held no baseline). When the backend already named the reason
    /// (`Some`), use it. When it did not (`None`), name it here in the plan's precedence:
    ///
    /// - **G1** first: a disabled feature is `FullByDesign { Disabled }`, above every fall-back --
    ///   even a marker failure -- so a disabled run never produces a warning.
    /// - git: a resumed turn with no complete baseline is `NoCompleteBaselineRetained`.
    /// - Perforce: the marker/missing reasons `tools.rs` forced `resume = None` for --
    ///   `MarkerUnwritable` (this turn's marker unwritable) > `PriorTurnPending` (prior marker
    ///   present) > `MarkerStateUnreadable` (prior marker unreadable) > `PriorBaselineMissing`
    ///   (no prior baseline at all).
    fn resolve_disposition(
        &self,
        resumed: bool,
        change_present: bool,
        backend: Option<vcs::Disposition>,
        marker_state: Option<crate::session::MarkerState>,
        pending_marked: bool,
    ) -> Option<vcs::Disposition> {
        assemble_disposition(
            self.cfg.vcs,
            self.cfg.resume_incremental_diff,
            resumed,
            change_present,
            backend,
            marker_state,
            pending_marked,
        )
    }

    fn run(mut self, resume_id: Option<String>) {
        let started = std::time::Instant::now();
        let mut guard = FinishGuard {
            registry: &self.registry,
            id: &self.id,
            armed: true,
        };

        // Captured once, before the attempt runs, so the reviewer's prompt and the usage
        // metrics below describe the same diff.
        // Publish the selected entry before capture, so a snapshot taken during the Capturing
        // phase names the reviewer that will run rather than a default. `self.bin` already holds
        // the start entry's resolved path (preflighted in `start_review`), so the identity names
        // the executable that will actually run, not merely the provider configuration.
        self.registry.set_active(
            &self.id,
            self.cfg.reviewers[self.start_index].describe_with_bin(&self.bin),
        );
        if self.cfg.chain_needs_capture() {
            self.registry.set_phase(&self.id, Phase::Capturing);
        }
        // Durable poison check: if the *previous* Perforce turn left an uncleared "in progress"
        // marker -- it crashed, panicked, or failed to persist its baseline -- the persisted
        // baseline may be stale relative to what the reviewer last saw, so do not collapse against
        // it. Read this before marking the current turn pending. The read is three-valued
        // (`Present`/`Absent`/`Unreadable`) so the disposition can tell a confirmed leftover
        // marker (`PriorTurnPending`) from a marker whose state could not be read
        // (`MarkerStateUnreadable`); `prior_pending` folds both non-`Absent` states fail-closed,
        // exactly as the previous `is_pending` did.
        let marker_state = (self.cfg.vcs == crate::config::Vcs::Perforce)
            .then(|| self.sessions.marker_state(&self.session));
        let prior_pending = matches!(
            marker_state,
            Some(crate::session::MarkerState::Present | crate::session::MarkerState::Unreadable)
        );
        // Mark this Perforce turn in progress; cleared only once it is durably recorded, so a
        // failure anywhere below leaves the marker set for the next resume to see. If the marker
        // cannot be written, a later crash could not be detected, so this turn must not produce a
        // resumable baseline at all (forced to `Disabled` below). This is the *Perforce* baseline
        // marker; the findings write-ahead is a separate marker written in `attempt`.
        let pending_marked = self.cfg.vcs == crate::config::Vcs::Perforce
            && self.sessions.mark_pending(&self.session).is_ok();

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
            // Force a full capture (no elision this turn) when either the prior turn did not
            // cleanly persist (`prior_pending`) or this turn's in-progress marker could not be
            // written (`!pending_marked`) -- in the latter case a later crash would be undetectable,
            // so eliding against the prior baseline now is not safe either.
            crate::config::Vcs::Perforce if prior_pending || !pending_marked => None,
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
        // If the in-progress marker could not be written, this turn cannot be made safely
        // resumable (a later crash would be undetectable), so refuse to persist a `Full` baseline
        // regardless of what the capture produced.
        let perforce_baseline = if self.cfg.vcs == crate::config::Vcs::Perforce && !pending_marked {
            Some(crate::vcs::baseline::PerforceBaseline::Disabled)
        } else {
            capture.perforce_baseline.clone()
        };
        let mut capture_warnings = capture.warnings;

        // The caller-facing resume disposition. The backend fills the decisions it can see; here
        // we supply the framing it cannot: a disposition exists only on a resumed turn that sent a
        // change, and a resumed turn whose backend saw no baseline is named by *this* layer, which
        // alone knows whether it resumed and why the baseline was withheld.
        let disposition = self.resolve_disposition(
            resume_id.is_some(),
            capture.change.is_some(),
            capture.disposition.clone(),
            marker_state,
            pending_marked,
        );

        // What the server captured and sent this turn, for the `captured:` response line. Present
        // exactly when a change was sent (`capture.change.is_some()`) -- no resume gate, so it is
        // there on a fresh turn too. Its metrics tag is taken from the local now, before the value
        // moves onto a *successful* outcome, so a failed reviewer attempt that still captured and
        // sent a change is logged with what it sent -- the same split as the disposition tag.
        let mut capture_summary = capture.change.as_ref().map(|c| c.summary.clone());
        let captured_tag = capture_summary.as_ref().map(vcs::CaptureSummary::tag);

        // A failed `mark_pending` means the *next* turn cannot safely elide. When the feature is
        // enabled that is worth an ordinary persistence warning on its own footing, independent of
        // the disposition (which G0 suppresses on a no-change resume): the durability guarantee is
        // gone. When the feature is off there is no future elision to protect, so it is immaterial.
        if self.cfg.vcs == crate::config::Vcs::Perforce
            && !pending_marked
            && self.cfg.resume_incremental_diff
        {
            capture_warnings.push(
                "The next incremental re-review could not be protected: this turn could not \
                 record a durable in-progress marker, so the next review of this session will \
                 re-send the whole change rather than only what changed."
                    .to_string(),
            );
        }

        // A delta that was expected and fell back to a full re-capture is a cost surprise the
        // caller should hear about -- the one disposition that also earns a warning. `FullByDesign`
        // and `Incremental` do not; a fresh or no-change turn has no disposition at all.
        if let Some(d) = &disposition {
            if d.warns() {
                capture_warnings.push(format!(
                    "Incremental re-review did not happen: {}.",
                    d.summary()
                ));
            }
        }
        // The disposition describes what the capture *sent* (a delta, or the whole change and
        // why), which is true whether or not the reviewer attempt below then succeeds. Take the
        // metrics tag from the local now, before the disposition moves onto a *successful* outcome
        // -- otherwise a failed reviewer attempt, which still sent this prompt, would record no
        // disposition even though a change was captured.
        let disposition_tag = disposition.as_ref().map(vcs::Disposition::tag);

        // What this attempt did, for the usage record.
        let mut facts = AttemptFacts::new(self.turn, resume_id.is_some(), self.gap_secs);

        // The fall-through walk. A **fresh** review walks the reviewer chain, advancing to the
        // next entry only on RATE_LIMITED; a **resume** runs exactly its bound entry (selected by
        // identity in `start_review`), never falling through, because the reviewer's memory lives
        // on one specific reviewer. The captured change is reused across attempts -- it was
        // gathered once above. See docs/reviewer-fallback-chain.md.
        let chain = self.cfg.reviewers.clone();
        let n = chain.len();
        // A fresh review walks from the selected start to the end; a resume runs its single bound
        // entry alone.
        let walk: Vec<usize> = if resume_id.is_none() {
            (self.start_index..n).collect()
        } else {
            vec![self.start_index]
        };

        let mut disposition = disposition;
        // Describe strings of every rate-limited entry, for the REVIEWERS_EXHAUSTED detail.
        let mut rate_limited_attempts: Vec<String> = Vec::new();
        // Describe strings of every entry the proactive gate skipped (pre-start + in-walk), for a
        // terminal REVIEWERS_EXHAUSTED detail. Seeded from the pre-start selection so an exhaustion
        // names entries skipped before the walk too. See `docs/usage-remaining-gate.md`.
        let mut gated_descs: Vec<String> = std::mem::take(&mut self.pre_start_gated_descs);
        // The earlier attempts, for the metrics record's `attempts` history (the terminal attempt
        // is the record itself, so it is not repeated here). Seeded with the pre-start gated skips
        // (non-billed), so a skip selected before the walk is still recorded on the turn.
        let mut metrics_attempts: Vec<metrics::Attempt> = std::mem::take(&mut self.pre_start_skips);
        // False once a fallback entry's binary could not be resolved: its path is unverified, so
        // the record must not attribute the previous entry's binary to it.
        let mut active_bin_resolved = true;
        let mut outcome: Option<Outcome> = None;

        for (pos, &i) in walk.iter().enumerate() {
            let entry = chain[i].clone();
            // The active entry's usage-store key: the start entry's was computed at selection (one
            // fingerprint reading, carried in), a fallback's is computed here at its own launch.
            // `None` when unarmed or identity could not be established.
            let active_usage_key = if i == self.start_index {
                self.start_usage_key.clone()
            } else {
                usage_headroom_key(&self.cfg, &entry)
            };
            // Proactive gate for a *fallback* entry, checked as the first thing in the iteration —
            // before `set_active` (so a skipped entry is never published as the active reviewer)
            // and before its lazy preflight (so a skip resolves nothing and spawns nothing). The
            // start entry was already gate-selected in `start_review`, so it is not re-gated here.
            // See `docs/usage-remaining-gate.md`.
            if i != self.start_index && entry.usage_minimum.is_gating() {
                let gated = active_usage_key
                    .as_ref()
                    .is_some_and(|k| !self.usage.get(k, now_unix()).clears(&entry.usage_minimum));
                if gated {
                    metrics_attempts.push(gated_skip_attempt(
                        &entry,
                        reviewer::resolve_bin(&entry).ok(),
                    ));
                    gated_descs.push(format!("{} (usage below minimum)", entry.describe()));
                    if pos == walk.len() - 1 {
                        // The last entry was gated: the chain is exhausted. Set the terminal
                        // outcome explicitly rather than falling through to WORKER_PANICKED.
                        outcome = Some(Outcome::failed(exhaustion_failure(
                            &rate_limited_attempts,
                            &gated_descs,
                        )));
                        break;
                    }
                    continue;
                }
            }
            // Each iteration starts a fresh attempt: reset the phase to Launching. Set per
            // iteration rather than once before the loop so a fallback taken after a rate-limited
            // attempt (which ended in Finalizing) is not still reported as Finalizing while it
            // preflights and runs.
            self.registry.set_phase(&self.id, Phase::Launching);
            // Publish the active entry: every identity-bearing read on the run path follows it,
            // and a running poll now names this entry. Its resolved binary is not known yet for a
            // fallback (it is preflighted just below), so name the entry by config here; the
            // resolved-bin identity is republished once the binary is verified.
            self.reviewer = Arc::from(reviewer::for_kind(entry.reviewer));
            self.spec = entry.clone();
            self.registry.set_active(&self.id, entry.describe());
            // The selected entry was preflighted in `start_review` (its bin is in `self.bin`). A
            // *fallback* entry is resolved and auth-checked here, lazily -- a fallback whose CLI
            // is missing or unauthenticated surfaces that failure (it is not RATE_LIMITED, so it
            // stops the walk) rather than troubling a healthy primary.
            if i != self.start_index {
                // Preflight the fallback through the shared cache (interruptible via the review's
                // cancel), so a fallback already checked by `status` is not re-resolved.
                match ensure_entry_ready(&self.preflight, &self.cfg, i, &self.cancel) {
                    Ok(ready) => self.bin = ready.bin,
                    Err(f) => {
                        // Preflight failed (resolution or auth): `self.bin` may still hold the
                        // previous entry's path, so this attempt has no verified binary to
                        // attribute in the record.
                        active_bin_resolved = false;
                        outcome = Some(Outcome::failed(f));
                        break;
                    }
                }
            }
            // The entry's binary is now resolved (the start entry's in `start_review`, a fallback's
            // just above), so republish the active identity with the resolved path -- a running
            // poll from here on names the executable that actually runs.
            self.registry
                .set_active(&self.id, self.spec.describe_with_bin(&self.bin));

            facts.prompt_bytes = 0;
            let attempt_started = std::time::Instant::now();
            match self.attempt(
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
                active_usage_key.as_deref(),
            ) {
                Ok(mut o) => {
                    // The disposition and capture summary ride on the successful outcome so the
                    // response can render them. A failed turn keeps `None` (`Outcome::failed`): it
                    // sent no reviewable change.
                    o.disposition = disposition.take();
                    o.capture_summary = capture_summary.take();
                    outcome = Some(o);
                    break;
                }
                Err(failure) => {
                    // A resume target the reviewer no longer has is a dead mapping. Do *not*
                    // `forget` it: reaching the reviewer means `mark_findings_pending` already
                    // succeeded (a failed mark aborts earlier), so the findings write-ahead marker
                    // is set and the next non-fresh call is refused at the findings gate before any
                    // resume is billed -- the "not billed for the same doomed resume" goal is met
                    // without deleting the record. Forgetting would only erase the prior findings
                    // ledger the rebaseline handoff needs, exactly like the failed-persistence arms
                    // in `attempt`. Reported rather than silently retried into a fresh conversation
                    // -- the caller decides whether to start over (fresh=true).
                    if failure.code == "SESSION_NOT_FOUND" && resume_id.is_some() {
                        eprintln!(
                            "cross-review: session '{}' could not be resumed (the reviewer no \
                             longer has it); reporting SESSION_NOT_FOUND rather than silently \
                             starting a fresh session",
                            self.session
                        );
                    }

                    // Only a rate/usage limit on a fresh multi-entry walk falls through; every
                    // other failure surfaces at once, and a single-entry chain returns the plain
                    // RATE_LIMITED it always did.
                    let is_last = pos == walk.len() - 1;
                    if resume_id.is_none() && n > 1 && failure.code == "RATE_LIMITED" {
                        // This entry ran to a RATE_LIMITED reply, so `self.bin` is its verified
                        // resolved path: name the executable in the exhausted list, not just the
                        // provider config.
                        rate_limited_attempts.push(self.spec.describe_with_bin(&self.bin));
                        if is_last {
                            // Cause-worded: pure rate-limited keeps today's exact detail; a chain
                            // that also gated some entries reports the mix (round-6 finding f7).
                            outcome = Some(Outcome::failed(exhaustion_failure(
                                &rate_limited_attempts,
                                &gated_descs,
                            )));
                            break;
                        }
                        // An earlier rate-limited attempt: record it for the metrics history
                        // (usage unknown -- a refusal exposes none) and advance to the next entry.
                        metrics_attempts.push(metrics::Attempt {
                            reviewer: entry.reviewer.as_str().to_string(),
                            model: entry.model.clone(),
                            effort: entry.effort.clone(),
                            resolved_bin: Some(self.bin.to_string_lossy().into_owned()),
                            failure_code: "RATE_LIMITED".to_string(),
                            wall_secs: attempt_started.elapsed().as_secs(),
                            prompt_bytes: facts.prompt_bytes,
                            // A rate-limited attempt spent a model call whose usage the CLI did
                            // not report back, so it is billed and taints completeness.
                            billed: true,
                        });
                        continue;
                    }
                    // A resume runs its bound entry only and never falls through, so a rate limit
                    // here is terminal -- but the remediation points at `fresh: true`, which does
                    // restart chain selection (at the cost of the reviewer's memory).
                    if resume_id.is_some() && failure.code == "RATE_LIMITED" {
                        outcome = Some(Outcome::failed(errors::rate_limited_on_resume(
                            self.spec.reviewer.as_str(),
                        )));
                        break;
                    }
                    outcome = Some(Outcome::failed(failure));
                    break;
                }
            }
        }

        // The walk always assigns `outcome` (every branch sets it or the loop covers every
        // index), so this fallback is a safety net rather than an expected path.
        let mut outcome =
            outcome.unwrap_or_else(|| Outcome::failed(errors::worker_panicked(&self.id)));
        // Attribute the terminal outcome to the entry that produced it (`self.spec` is the last
        // entry the walk touched), so the completed response names the reviewer that actually ran.
        // Name the resolved executable when it was verified; a fallback whose preflight failed left
        // `self.bin` holding the previous entry's path, so that case falls back to the config form.
        outcome.active = Some(if active_bin_resolved {
            self.spec.describe_with_bin(&self.bin)
        } else {
            self.spec.describe()
        });
        // Everything telemetry needs, taken before the outcome moves into the registry. The
        // disposition tag was captured above (from the local, so a failed attempt still records
        // what it sent).
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

        // The terminal entry's resolved binary, but only when it was actually resolved: a
        // fallback whose resolution failed leaves `self.bin` holding the previous entry's path,
        // which must not be attributed to it.
        let resolved_bin = active_bin_resolved.then(|| self.bin.to_string_lossy().into_owned());

        self.record_usage(
            usage,
            failure_code,
            disposition_tag,
            captured_tag,
            capture.change.as_ref(),
            started,
            facts,
            metrics_attempts,
            resolved_bin,
        );
    }

    /// Append this turn to the usage log, and say the same thing on stderr.
    ///
    /// Both, because they answer different questions. The log is what you aggregate
    /// across sessions and machines after the fact; the stderr line is what a user
    /// watching a review happen can see without going and finding the file.
    ///
    /// Best effort by construction: it runs after the review has been handed over, so
    /// nothing it does can affect the result.
    #[allow(clippy::too_many_arguments)]
    fn record_usage(
        &self,
        usage: crate::metrics::Usage,
        failure_code: Option<String>,
        disposition: Option<String>,
        captured: Option<String>,
        change: Option<&vcs::CapturedChange>,
        started: std::time::Instant,
        facts: AttemptFacts,
        attempts: Vec<metrics::Attempt>,
        resolved_bin: Option<String>,
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
            self.spec.reviewer.as_str(),
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
            // A record carrying fall-through attempts is stamped v2, so an old reader skips it
            // rather than reading the terminal usage as a complete accounting of the turn.
            v: metrics::record_version_for(!attempts.is_empty()),
            ts_unix: now_unix(),
            review_id: self.id.clone(),
            session: self.session.clone(),
            turn: facts.turn,
            resumed: facts.resumed,
            gap_secs: facts.gap_secs,
            reviewer: self.spec.reviewer.as_str().to_string(),
            model: self.spec.model.clone(),
            effort: self.spec.effort.clone(),
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
            disposition,
            resolved_bin,
            attempts,
            captured,
        });
    }

    /// Clear this turn's findings write-ahead marker after a failure that provably started no child
    /// process, so no reviewer conversation could have advanced and the ledger is not stale. Guarded
    /// by `findings_marker_absent_on_entry`: only a call whose marker was confirmed absent at entry
    /// (every non-`fresh` call — a resume *or* a non-fresh turn 1 with no record) may clear, because
    /// then this turn's `mark_findings_pending` is the only thing that set it. Undoing it lets the
    /// next call proceed instead of being wrongly refused, keeping the Perforce `.pending`
    /// full-recapture path reachable. A `fresh` call does not clear: it bypassed the gate and may be
    /// sitting on a marker an earlier failed turn set, which it must not silently drop. A failed
    /// clear only over-refuses the next call (the safe direction), so it is warned, not fatal.
    fn clear_findings_marker_after_pre_launch_failure(&self) {
        if !self.findings_marker_absent_on_entry {
            return;
        }
        if let Err(e) = self.sessions.clear_findings_pending(&self.session) {
            eprintln!(
                "cross-review: warning: could not clear the findings write-ahead marker after a \
                 pre-launch failure for session '{}': {e}; the next call may be refused",
                self.session
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &self,
        resume_id: Option<&str>,
        turn: u32,
        baseline: Option<crate::metrics::Usage>,
        captured: CaptureOutputs<'_>,
        capture_warnings: &[String],
        prompt_bytes: &mut usize,
        // The store key for this attempt's entry (from the walk), or `None` when unarmed /
        // identity unavailable. When `Some`, the observed headroom is recorded under it on both
        // the success and failure paths. See `docs/usage-remaining-gate.md`.
        usage_key: Option<&str>,
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
        // Rendered for the *active* entry, so a mixed-family fallback is told the truth about its
        // own shell/self-serve ability rather than the primary's.
        let capabilities = self
            .cfg
            .reviewer_capabilities_of(self.spec.reviewer, change.is_some());
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
        // This session's prior findings ledger, used both for the resumed prompt's digest and for
        // reconciliation after the reviewer answers. Resolved once in `start_review` from the record
        // validated under the lease and carried in on the job — NOT re-read here: a second, fail-open
        // `SessionStore::get` could turn a transient read error into an empty ledger and then mint
        // fresh ids over a real one, or report an empty ledger as trusted on a not-durable turn. An
        // `invalid` ledger or a set `terminal_reason` was already refused at the resume gate, so it
        // will not appear here; a legacy (pre-feature) resume arrives as `legacy_uncovered`.
        let prior_state: Option<crate::findings::PriorState> = self.prior_findings.clone();
        // Before-call bounded-growth check, done *before* the findings write-ahead marker so this
        // path involves no marker at all: if the prior ledger is already over budget on entry (a
        // budget lowered between runs, or an older ledger under a tighter cap), do not invoke the
        // reviewer — nothing is billed and no turn is advanced. Persist the sticky terminal state so
        // the next resume is refused, and return a completed `ledger_too_large` envelope for the
        // human to rebaseline from.
        if let Some(prior) = &prior_state {
            if crate::findings::prior_over_budget(prior, crate::findings::Budget::default()) {
                let _ = self
                    .sessions
                    .set_terminal_reason(&self.session, "ledger_too_large");
                // No reviewer ran, so no marker should be left implying an in-flight turn. The
                // Perforce `.pending` marker was set by `run` before this check; clear it so the
                // terminal state (not a stale marker) is what the next call sees.
                if self.cfg.vcs == crate::config::Vcs::Perforce {
                    let _ = self.sessions.clear_pending(&self.session);
                }
                let envelope =
                    crate::findings::over_budget_on_entry_envelope(&self.session, turn, prior);
                return Ok(Outcome {
                    review: Some(envelope.warnings.first().cloned().unwrap_or_default()),
                    failure: None,
                    denials: Vec::new(),
                    denial_count: 0,
                    denial_count_is_floor: false,
                    warnings: Vec::new(),
                    disposition: None,
                    capture_summary: None,
                    resumable: false,
                    usage: crate::metrics::Usage::default(),
                    // No reviewer ran; `run` attributes the terminal outcome to the entry the walk
                    // settled on. This over-budget short-circuit sits inside that walk, so leave it
                    // to `run` to name the active entry.
                    active: None,
                    envelope: Some(envelope),
                });
            }
        }

        // Findings write-ahead: mark before the reviewer runs, cleared only once the turn is durably
        // recorded (the `record_turn` Ok arm). If the mark cannot be written, a crash could not be
        // detected and the ledger could go stale relative to the reviewer's advanced conversation,
        // so abort before the model call — on *every* turn, so a `fresh: true` that overwrites an
        // existing record cannot advance the conversation marker-less and then strand the old record
        // resumable. A genuinely-new session has nothing to strand, but aborting there too is
        // harmless (just retry), so the rule is simply "mark, or abort".
        if self.sessions.mark_findings_pending(&self.session).is_err() {
            return Err(errors::session_not_resumable(
                &self.session,
                "the findings write-ahead marker could not be written, so a crash could not be \
                 detected and the ledger could go stale; the turn was not started. Retry, or start \
                 a fresh review."
                    .to_string(),
            ));
        }

        let prior_findings_digest: Option<String> = prior_state
            .as_ref()
            .map(|p| crate::findings::render_digest(&p.findings));
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
            // The nonce is this review's id (`rv-<pid>-<counter>`), unique per turn — a static
            // repository lookalike cannot know it. The prior-findings digest is built from the
            // loaded ledger in the worker wiring (task: tools.rs worker); `None` renders the
            // first-turn form.
            nonce: Some(&self.id),
            prior_findings_digest: prior_findings_digest.as_deref(),
        });
        // Reported back through the out-parameter so it survives the error paths below:
        // a failed turn still sent a prompt, and its size is part of explaining the cost.
        *prompt_bytes = text.len();

        let invocation = match self
            .reviewer
            .invocation(&self.cfg, &self.spec, &self.bin, resume_id, &self.id)
        {
            Ok(inv) => inv,
            Err(e) => {
                // Building the invocation failed (e.g. a temp last-message file could not be
                // created): no child process was ever started, so the reviewer conversation could
                // not have advanced and this session's findings ledger is not stale. Undo this turn's
                // findings marker (for a non-fresh call, whose marker was absent on entry) so the
                // next call is not refused — and, for Perforce, so Job::run's `.pending`
                // full-recapture fallback stays reachable.
                self.clear_findings_marker_after_pre_launch_failure();
                return Err(errors::spawn_failed(
                    self.spec.reviewer.as_str(),
                    &self.bin.display().to_string(),
                    e.to_string(),
                ));
            }
        };

        let last_message_file = invocation.last_message_file.clone();

        let run = match reviewer::run_observed(
            invocation.command,
            &text,
            self.cfg.timeout,
            &self.cancel,
            // The armed Claude path raises the stdout cap and terminates a runaway stream at the
            // byte/line bound; every other path keeps the retain-and-drain default. See
            // docs/usage-remaining-gate.md.
            self.reviewer.output_limits(&self.cfg),
            |activity| {
                self.registry
                    .report_activity(&self.id, activity.output_bytes);
            },
        ) {
            Ok(out) => Ok(out),
            Err(e) => {
                // A `Spawn` failure means the child never started — same reasoning as the invocation
                // failure above, so clear the (non-fresh) findings marker and keep the Perforce
                // full-recapture path reachable. An `Observe` failure happened after the child was
                // already running, so the conversation may have advanced: keep the marker set and
                // let the findings gate refuse the next call.
                if e.child_never_started() {
                    self.clear_findings_marker_after_pre_launch_failure();
                }
                Err(errors::spawn_failed(
                    self.spec.reviewer.as_str(),
                    &self.bin.display().to_string(),
                    e.to_string(),
                ))
            }
        };
        self.registry.set_phase(&self.id, Phase::Finalizing);

        let result = match run {
            Ok(out) => {
                // Observe usage headroom from the raw outcome BEFORE it is turned into a Parsed or
                // a Failure, so a rate-limited turn is observed exactly like a successful one
                // (round-1 finding f1). Read only when the entry is armed and keyable; store only
                // a real reading. Best-effort — the store never fails a review.
                if let Some(key) = usage_key {
                    let headroom = self.reviewer.observe_headroom(&self.cfg, &self.spec, &out);
                    if headroom != Headroom::Unknown {
                        self.usage.record(key, headroom, now_unix());
                    }
                }
                if out.cancelled || out.timed_out {
                    Err(reviewer::failure_for(&self.cfg, &self.spec, &out))
                } else {
                    self.reviewer
                        .parse(&self.cfg, &self.spec, &out, last_message_file.as_deref())
                }
            }
            Err(failure) => Err(failure),
        };

        if let Some(path) = &last_message_file {
            std::fs::remove_file(path).ok();
        }

        let mut parsed = result?;

        // Evaluate the reviewer's machine block against the prior ledger: extract, reconcile, and
        // build the completed envelope (pure — see `findings::evaluate_turn`). The nonce is this
        // review's id, matching what the prompt told the reviewer to emit. Then strip the reviewer's
        // raw block (and any lookalike output marker) from the prose we render and store, so the
        // human review keeps its narrative but not the transport block.
        // Keep a copy of the pre-turn state for the not-durable envelope, which must report the
        // pre-turn on-disk coverage and preserve the prior findings (evaluate_turn consumes it).
        let prior_snapshot = prior_state.clone();
        let turn_eval = crate::findings::evaluate_turn(
            &self.session,
            turn,
            &self.id,
            &parsed.text,
            prior_state,
            crate::findings::Budget::default(),
        );
        let findings_ledger_to_persist: Option<serde_json::Value> =
            Some(serde_json::to_value(&turn_eval.ledger).unwrap_or(serde_json::Value::Null));
        let terminal_reason_to_persist: Option<String> = turn_eval
            .over_budget
            .then(|| "ledger_too_large".to_string());
        // Remove the reviewer's own machine block (exact nonce) from the prose we store, so the
        // human review keeps its narrative but not the transport block. Any *other* stray marker
        // line (a wrong-nonce block, or one injected via another field) is neutralised at render
        // time by `strip_marker_lines` over the whole result body, right before the canonical
        // `_OUT` block is appended.
        parsed.text = crate::findings::strip_reviewer_block(&parsed.text, &self.id);

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
        // A resume whose reviewer answered under a *different* nonempty session id is a conversation
        // identity change: the id that answered is not the one we resumed, so this turn's reviewer
        // never held the session's earlier turn-by-turn history -- only the injected findings digest
        // (a summary, not the full prior prose/diffs). Recording the resumed ledger -- still stamped
        // `whole_conversation` -- under that new id would let a re-keyed, effectively first-turn
        // conversation converge as though it had reviewed the whole thread. Fail closed: do not
        // record. The findings write-ahead marker stays set (it is only cleared in the durable Ok
        // arm), so the next non-fresh call is refused at the findings gate and the caller rebaselines
        // fresh; the prior ledger is preserved on disk for that handoff. The review prose below is
        // unaffected. A fresh turn (`resume_id` is `None`) and a resume that echoes the same id or no
        // id at all are all unaffected.
        let resumed_id_mismatch =
            resumed_session_id_mismatch(resume_id, parsed.session_id.as_deref());
        // Whether the findings write-ahead marker was successfully cleared after a durable record.
        // A durable turn whose marker clear *failed* leaves the marker set, so the next non-fresh
        // call will be refused at the findings gate -- such a turn must not advertise itself as
        // resumable (finding: resumable ignored a failed marker clear). Set only in the record_turn
        // Ok arm below; stays false on every non-durable path.
        let mut findings_marker_cleared = false;
        // Whether this turn was durably recorded. Distinct from `resumable` below: a turn can be
        // durable yet non-resumable (an over-budget turn persists a terminal state that refuses the
        // next resume, or a failed marker clear that refuses it).
        let durable = if resumed_id_mismatch {
            warnings.push(format!(
                "The reviewer answered under a different session id ('{}') than the one this resume \
                 targeted ('{}'), so its conversation did not contain this session's earlier turns \
                 -- only the findings digest. This turn was not recorded and session '{}' cannot be \
                 resumed: the next call is refused a resume (the write-ahead marker is still set). \
                 Any prior findings are preserved on disk -- start a fresh review (fresh: true) \
                 carrying the still-open findings into the new instructions. The review below is \
                 still valid.",
                parsed.session_id.as_deref().unwrap_or_default(),
                resume_id.unwrap_or_default(),
                self.session
            ));
            eprintln!(
                "cross-review: warning: reviewer reported session id '{}' on a resume of '{}' \
                 (expected '{}'); not recording, leaving the session non-resumable",
                parsed.session_id.as_deref().unwrap_or_default(),
                self.session,
                resume_id.unwrap_or_default()
            );
            false
        } else {
            match record_under {
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
                            reviewer: self.spec.reviewer.as_str(),
                            cli_session_id: session_id,
                            model: &self.spec.model,
                            effort: &self.spec.effort,
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
                            // The active entry's identity, so a resume can match this exact entry and
                            // detect PATH drift.
                            raw_bin: self.spec.raw_bin(),
                            resolved_bin: self.bin.to_string_lossy().into_owned(),
                            // Filled by the findings-envelope worker wiring (see `attempt`); the
                            // reconciled ledger and any terminal state this turn produced.
                            findings_ledger: findings_ledger_to_persist.clone(),
                            terminal_reason: terminal_reason_to_persist.clone(),
                        },
                    ) {
                        Ok(_) => {
                            // Durably recorded: clear the Perforce in-progress marker so the next resume
                            // trusts this turn's baseline. Only reached after `record_turn` returned
                            // `Ok`. If the delete fails the marker may survive and wrongly disable the
                            // *next* incremental review, so say so rather than letting it silently
                            // persist.
                            if self.cfg.vcs == crate::config::Vcs::Perforce {
                                if let Err(e) = self.sessions.clear_pending(&self.session) {
                                    warnings.push(format!(
                                    "This turn was saved, but the session's in-progress marker \
                                     could not be cleared ({e}); the next review of session '{}' \
                                     may re-send the whole change instead of only what changed.",
                                    self.session
                                ));
                                }
                            }
                            // Clear the findings write-ahead marker too (all backends). A failed delete
                            // over-refuses the next resume toward `fresh` — the safe direction — so the
                            // turn is recorded (durable) but reported non-resumable (`resumable` folds
                            // in `findings_marker_cleared` below), matching what the next call will do.
                            match self.sessions.clear_findings_pending(&self.session) {
                            Ok(()) => findings_marker_cleared = true,
                            Err(e) => warnings.push(format!(
                                "This turn was saved, but the findings write-ahead marker could not \
                                 be cleared ({e}); the next review of session '{}' will be refused a \
                                 resume and must be restarted fresh.",
                                self.session
                            )),
                        }
                            true
                        }
                        Err(e) => {
                            // The review itself succeeded; losing resumability is worth a warning but
                            // not worth discarding the review.
                            //
                            // Do *not* `forget` the record here. The prior ledger must stay intact on
                            // disk so a human-directed rebaseline can carry its still-open findings
                            // forward -- that is the `turn_not_durable` recovery contract (design
                            // Decision 5). Resume is already blocked without deleting anything: the
                            // findings write-ahead marker is still set (this Err arm never reached the
                            // Ok arm that clears it), so the next non-fresh call is refused at the
                            // findings gate regardless of backend. For Perforce the `.pending` marker
                            // is set too, so even a crash before this point cannot collapse a later
                            // resume against the stale baseline. The old destructive poisoning (drop
                            // the mapping) predated the write-ahead markers and would now erase the
                            // preserved findings for no added safety.
                            warnings.push(format!(
                            "This turn could not be saved to disk ({e}), so session '{}' cannot be \
                             resumed: the next call is refused a resume because the write-ahead \
                             marker is still set. Any prior findings are preserved on disk -- \
                             recover by starting a fresh review (fresh: true) carrying the \
                             still-open findings into the new instructions. The review below is \
                             unaffected.",
                            self.session
                        ));
                            eprintln!("cross-review: warning: could not save session state: {e}");
                            false
                        }
                    }
                }
                None => {
                    // No session id means this turn cannot be recorded. Do *not* `forget` the prior
                    // record: like the failed-persistence arm above, it must stay intact on disk so a
                    // rebaseline can carry its findings forward. Resume is already blocked -- the
                    // findings write-ahead marker was set before the reviewer ran and is never cleared
                    // without a durable record, so the next non-fresh call is refused at the findings
                    // gate for every backend. No destructive poisoning of the mapping is needed.
                    warnings.push(format!(
                    "The reviewer did not report a session id, so this turn could not be recorded \
                     and session '{}' cannot be resumed: the next call is refused a resume because \
                     the write-ahead marker is still set. Any prior findings are preserved on disk \
                     -- start a fresh review (fresh: true) carrying the still-open findings into \
                     the new instructions. The review below is still valid.",
                    self.session
                ));
                    eprintln!(
                    "cross-review: warning: the reviewer did not report a session id, so review \
                     session '{}' cannot be resumed",
                    self.session
                );
                    false
                }
            }
        };

        // The envelope reported to the caller. When the turn was durably recorded, it is the
        // evaluated envelope (its reason reflects coverage/over-budget). When it was not
        // (`record_turn` failed, or the reviewer reported no id to record under), the durable
        // outcome is `turn_not_durable` with the pre-turn on-disk coverage — the caller escalates
        // and rebaselines rather than resuming on a ledger that disagrees with the reviewer.
        let envelope = if durable {
            turn_eval.envelope
        } else {
            crate::findings::not_durable_envelope(&self.session, turn, prior_snapshot.as_ref())
        };
        // A durable but over-budget turn has persisted a sticky `terminal_reason`, so the next
        // resume will be refused: do not invite one. A durable turn whose findings marker could not
        // be cleared will likewise be refused at the findings gate. Only a durable, in-budget turn
        // whose marker was cleared is genuinely resumable.
        let resumable = durable && !turn_eval.over_budget && findings_marker_cleared;

        Ok(Outcome {
            review: Some(parsed.text),
            failure: None,
            denials: parsed.denials,
            denial_count: parsed.denial_count,
            denial_count_is_floor: parsed.denial_count_is_floor,
            warnings,
            // Filled in by `run` after `attempt` returns: the disposition is assembled from the
            // capture plus the fresh-vs-resumed framing only `run` holds, and `active` from the
            // entry the walk settled on.
            disposition: None,
            // Likewise filled in by `run`, which holds the capture. Rides the successful outcome
            // for the response; a failed attempt renders as an error and shows no `captured:` line.
            capture_summary: None,
            resumable,
            usage: parsed.usage,
            active: None,
            envelope: Some(envelope),
        })
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Whether a resumed turn's reviewer answered under a *different* conversation than the one we
/// resumed, which must fail closed (the ledger is not durably recorded — see the call site).
///
/// True only when this was a resume (`resume_id` is `Some`) **and** the reviewer reported a
/// nonempty session id that differs from it. A fresh turn (`resume_id` is `None`), a resume that
/// echoes the same id, and a resume that reports no id at all (handled as "still the same thread")
/// are all *not* mismatches.
fn resumed_session_id_mismatch(resume_id: Option<&str>, reported: Option<&str>) -> bool {
    matches!(
        (resume_id, reported),
        (Some(resumed), Some(reported)) if resumed != reported
    )
}

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
    // A stored session id that normalizes to absent (empty or whitespace-only) is not a real
    // resume handle: `--resume ""` would try to continue a conversation no reviewer holds. Newly
    // reported ids are normalized away at the adapter boundary (`reviewer::normalize_session_id`),
    // but a record persisted before that guard (or by any other path) could still carry a blank id,
    // so refuse it here before the model call rather than passing it through. Recovery is a fresh
    // rebaseline.
    if record.cli_session_id.trim().is_empty() {
        return Some(
            "its stored reviewer session id is blank, so there is no conversation to resume. Start \
             a fresh review (fresh: true) carrying any still-open findings into the new \
             instructions."
                .to_string(),
        );
    }
    // Identity first: a session belongs to the reviewer, model and working root that
    // created it. A configuration that now points elsewhere cannot resume that conversation
    // at all, so the honest answer is to start over -- explicitly, so the caller learns the
    // earlier context is gone rather than inheriting it silently.
    // A session belongs to the reviewer *entry* that created it, which may be a fallback rather
    // than the primary. It resumes only if some configured entry still matches its full identity
    // (reviewer, model, effort, raw bin); a chain edited out from under it cannot resume that
    // conversation, so the honest answer is to start over -- explicitly.
    if cfg.resume_entry_index(record).is_none() {
        return Some(format!(
            "no reviewer in the configured chain matches this session's reviewer '{}', model \
             '{}' and effort '{}'; the chain was changed since it was created, and a conversation \
             cannot move between reviewers.",
            record.reviewer, record.model, record.effort
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

    // A sticky terminal escalation — currently `ledger_too_large` — is not resumable: the session
    // outgrew a single review conversation, so continuing it would only grow the ledger further.
    // The caller must rebaseline into a fresh session carrying the still-open findings.
    if let Some(reason) = &record.terminal_reason {
        return Some(format!(
            "it reached a terminal state ({reason}): the findings ledger outgrew a single review \
             conversation. Start a fresh review (fresh: true) carrying the still-open findings into \
             the new instructions."
        ));
    }

    // An unreadable or incompatible findings ledger cannot be injected into the resumed prompt, so
    // the reviewer would lose the grounded findings — refuse before the model call. This is the
    // per-record `invalid` coverage state; recovery is a fresh rebaseline.
    if matches!(record.ledger_load(), session::LedgerLoad::Invalid) {
        return Some(
            "its findings ledger is unreadable or at an incompatible version, so it cannot be \
             resumed with its findings intact. Start a fresh review (fresh: true) to rebaseline."
                .to_string(),
        );
    }

    None
}

/// Build the `SESSION_NOT_RESUMABLE` failure for a pre-model resume refusal, tagging the
/// unreadable-ledger case so a caller can tell it from a policy refusal.
///
/// The failure contract distinguishes an *unreadable ledger* (`ledger_unavailable`) from a whole-
/// store parse failure (`state_corrupt`, refused earlier with its own error) and from a plain
/// policy refusal (turns/idle/mismatch/PATH-drift/stale-marker — no detail). The `ledger_unavailable`
/// tag is attached exactly when the record's stored ledger fails to load, which is precisely the
/// state that needs a fresh rebaseline rather than a retry — so **every** existing-record refusal
/// on the pre-model path routes through here (the findings-marker gate, the `resume_block` policy
/// gate, and the resolved-binary identity gate), not just one of them, so a record whose ledger is
/// unreadable is tagged whichever gate happens to fire first. `record` is `None` for the one refusal
/// that can fire with no stored record (a leftover findings marker on a name with no record); it
/// then carries no detail.
fn resume_refusal(
    session: &str,
    reason: String,
    record: Option<&session::SessionRecord>,
) -> Failure {
    let mut failure = errors::session_not_resumable(session, reason);
    if matches!(
        record.map(|r| r.ledger_load()),
        Some(session::LedgerLoad::Invalid)
    ) {
        failure.detail = Some("ledger_unavailable".to_string());
    }
    failure
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

/// The progress/liveness fields for the structured running variant, taken from a snapshot — the
/// same signals the text progress line shows, so the structured channel is not strictly poorer.
fn running_progress_of(snapshot: &Snapshot) -> crate::findings::RunningProgress<'static> {
    crate::findings::RunningProgress {
        elapsed_seconds: snapshot.elapsed.as_secs(),
        phase: snapshot.phase.as_str(),
        phase_elapsed_seconds: snapshot.phase_elapsed.as_secs(),
        activity_age_seconds: snapshot.activity_age.as_secs(),
        output_bytes: snapshot.output_bytes as u64,
    }
}

fn render_progress(snapshot: &Snapshot, review_budget: Duration, reassure: bool) -> String {
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
             minutes or longer. This review's configured budget is {}.",
            fmt_elapsed(review_budget)
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

    #[test]
    fn exhaustion_failure_words_itself_by_cause() {
        // Pure rate-limited keeps today's exact single-reviewer detail (byte-for-byte contract).
        let rate_only = exhaustion_failure(&["a".into(), "b".into()], &[]);
        assert_eq!(rate_only.code, "REVIEWERS_EXHAUSTED");
        assert!(
            rate_only
                .detail
                .as_deref()
                .unwrap()
                .contains("rate/usage limit, in order: a; b"),
            "{rate_only:?}"
        );
        assert!(rate_only.summary.contains("rate or usage limit"));

        // All-gated names the usage minimum, not a rate limit.
        let gated_only = exhaustion_failure(&[], &["x (usage below minimum)".into()]);
        assert!(
            gated_only.summary.contains("below its configured minimum"),
            "{gated_only:?}"
        );

        // Mixed names both causes.
        let mixed = exhaustion_failure(&["a".into()], &["x (usage below minimum)".into()]);
        assert!(
            mixed.summary.contains("rate-limited or skipped"),
            "{mixed:?}"
        );
        let d = mixed.detail.as_deref().unwrap();
        assert!(
            d.contains("a (rate-limited)") && d.contains("usage below minimum"),
            "{d}"
        );
    }

    #[test]
    fn a_gated_skip_attempt_is_non_billed() {
        let spec = Config::from_args(&["--reviewer".into(), "codex".into()])
            .unwrap()
            .primary()
            .clone();
        let a = gated_skip_attempt(&spec, Some(PathBuf::from("C:/x/codex.exe")));
        assert_eq!(a.failure_code, "USAGE_BELOW_MINIMUM");
        assert!(!a.billed);
        assert_eq!(a.resolved_bin.as_deref(), Some("C:/x/codex.exe"));
    }

    #[test]
    fn resumed_session_id_mismatch_only_fires_on_a_resume_with_a_different_reported_id() {
        // A resume whose reviewer answered under a different nonempty id is the one fail-closed case.
        assert!(resumed_session_id_mismatch(Some("A"), Some("B")));
        // Same id echoed back: the normal resume, not a mismatch.
        assert!(!resumed_session_id_mismatch(Some("A"), Some("A")));
        // Resume that reported no id: handled elsewhere as "still the same thread", not a mismatch.
        assert!(!resumed_session_id_mismatch(Some("A"), None));
        // A fresh turn (no resume target) with any reported id is never a mismatch.
        assert!(!resumed_session_id_mismatch(None, Some("B")));
        assert!(!resumed_session_id_mismatch(None, None));
    }

    // -----------------------------------------------------------------------
    // assemble_disposition: the gates and framing the tools layer supplies over the backend.
    // -----------------------------------------------------------------------
    mod disposition {
        use super::assemble_disposition;
        use crate::config::Vcs;
        use crate::session::MarkerState;
        use crate::vcs::disposition::{Disposition, FellBack, FullByDesign};

        /// A convenience: git, feature on, resumed, change present, backend `None`, markers moot.
        fn git_no_backing() -> Option<Disposition> {
            assemble_disposition(Vcs::Git, true, true, true, None, None, false)
        }

        #[test]
        fn g0_suppresses_a_fresh_or_no_change_turn() {
            // Not resumed: no disposition even with a backend reason present.
            assert!(assemble_disposition(
                Vcs::Git,
                true,
                false,
                true,
                Some(Disposition::FellBackToFull(FellBack::BaseMoved)),
                None,
                true,
            )
            .is_none());
            // Resumed but no change sent: also none.
            assert!(assemble_disposition(
                Vcs::Git,
                true,
                true,
                false,
                Some(Disposition::FellBackToFull(FellBack::BaseMoved)),
                None,
                true,
            )
            .is_none());
        }

        #[test]
        fn a_backend_reason_passes_through_unchanged() {
            let d = assemble_disposition(
                Vcs::Git,
                true,
                true,
                true,
                Some(Disposition::FellBackToFull(FellBack::BranchRewritten)),
                None,
                true,
            );
            assert_eq!(
                d,
                Some(Disposition::FellBackToFull(FellBack::BranchRewritten))
            );
        }

        #[test]
        fn a_resumed_git_turn_with_no_baseline_is_no_complete_baseline_retained() {
            assert_eq!(
                git_no_backing(),
                Some(Disposition::FellBackToFull(
                    FellBack::NoCompleteBaselineRetained
                ))
            );
        }

        #[test]
        fn g1_disabled_wins_over_the_perforce_marker_reasons() {
            // Disabled AND the marker could not be written: G1 is a gate, so it is FullByDesign,
            // never a MarkerUnwritable warning. This is the round-7 contradiction, resolved.
            let d = assemble_disposition(
                Vcs::Perforce,
                false, // disabled
                true,
                true,
                None,
                Some(MarkerState::Present),
                false, // marker unwritable
            );
            assert_eq!(d, Some(Disposition::FullByDesign(FullByDesign::Disabled)));
            assert!(!d.unwrap().warns());
        }

        #[test]
        fn perforce_marker_precedence() {
            let p = |marker: Option<MarkerState>, pending_marked: bool| {
                assemble_disposition(
                    Vcs::Perforce,
                    true,
                    true,
                    true,
                    None,
                    marker,
                    pending_marked,
                )
            };
            // MarkerUnwritable wins even when a prior marker is also present.
            assert_eq!(
                p(Some(MarkerState::Present), false),
                Some(Disposition::FellBackToFull(FellBack::MarkerUnwritable))
            );
            // Then a confirmed-present prior marker.
            assert_eq!(
                p(Some(MarkerState::Present), true),
                Some(Disposition::FellBackToFull(FellBack::PriorTurnPending))
            );
            // Then an unreadable marker state -- not a false PriorTurnPending.
            assert_eq!(
                p(Some(MarkerState::Unreadable), true),
                Some(Disposition::FellBackToFull(FellBack::MarkerStateUnreadable))
            );
            // Markers fine: `resume = None` reaching here means no prior baseline existed.
            assert_eq!(
                p(Some(MarkerState::Absent), true),
                Some(Disposition::FellBackToFull(FellBack::PriorBaselineMissing))
            );
        }
    }

    /// A session record identical to what `cfg` would create, aged and lengthened to order.
    fn record_matching(
        cfg: &Config,
        turns: u32,
        updated_unix: u64,
    ) -> crate::session::SessionRecord {
        crate::session::SessionRecord {
            reviewer: cfg.primary().reviewer.as_str().to_string(),
            cli_session_id: "sid-1".to_string(),
            model: cfg.primary().model.clone(),
            effort: cfg.primary().effort.clone(),
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
            // Match what `cfg`'s primary entry would record, so the resume identity check binds.
            raw_bin: Some(cfg.primary().raw_bin()),
            resolved_bin: None,
            findings_ledger: None,
            terminal_reason: None,
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
    fn a_terminal_ledger_too_large_session_is_refused_resume() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        let mut rec = record_matching(&cfg, 1, now);
        rec.terminal_reason = Some("ledger_too_large".to_string());
        let reason = resume_block(&cfg, &rec, &[], now).expect("terminal is refused");
        assert!(reason.contains("terminal state"));
        assert!(reason.contains("ledger_too_large"));
    }

    #[test]
    fn a_blank_stored_session_id_is_refused_resume() {
        // A record persisted with a blank/whitespace cli_session_id is not a resumable handle:
        // resume_block refuses it before the model call rather than attempting `--resume ""`.
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        for blank in ["", "   ", "\t"] {
            let mut rec = record_matching(&cfg, 1, now);
            rec.cli_session_id = blank.to_string();
            let reason =
                resume_block(&cfg, &rec, &[], now).expect("a blank session id is refused resume");
            assert!(reason.contains("blank"), "{reason}");
        }
        // A real id still resumes (no blank-id refusal).
        let ok = record_matching(&cfg, 1, now);
        assert!(!ok.cli_session_id.trim().is_empty());
        assert!(resume_block(&cfg, &ok, &[], now).is_none());
    }

    #[test]
    fn an_invalid_findings_ledger_is_refused_resume() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        let mut rec = record_matching(&cfg, 1, now);
        // A ledger value that is not a compatible ledger -> LedgerLoad::Invalid -> refused.
        rec.findings_ledger = Some(serde_json::json!({"schema_version": 999}));
        let reason = resume_block(&cfg, &rec, &[], now).expect("invalid ledger is refused");
        assert!(reason.contains("unreadable or at an incompatible version"));
        // The refusal is tagged machine-readably as `ledger_unavailable`, so a caller can tell an
        // unreadable-ledger refusal from a policy one (turns/idle/mismatch, which carry no detail).
        // The tag rides whichever gate fires, so a record with an unreadable ledger is tagged even
        // when the *shown* reason is a different policy refusal — assert that compound case too.
        let failure = resume_refusal("default", reason, Some(&rec));
        assert_eq!(failure.code, "SESSION_NOT_RESUMABLE");
        assert_eq!(failure.detail.as_deref(), Some("ledger_unavailable"));

        // Compound: an unreadable ledger AND over the turn limit -> the turns message is shown, but
        // the ledger_unavailable tag still rides (the recovery, a fresh rebaseline, is identical).
        let mut invalid_and_stale = record_matching(&cfg, cfg.resume_max_turns + 1, now);
        invalid_and_stale.findings_ledger = Some(serde_json::json!({"schema_version": 999}));
        let compound_reason =
            resume_block(&cfg, &invalid_and_stale, &[], now).expect("too many turns is refused");
        assert!(compound_reason.contains("turn"));
        let compound = resume_refusal("default", compound_reason, Some(&invalid_and_stale));
        assert_eq!(compound.detail.as_deref(), Some("ledger_unavailable"));

        // A policy refusal on a record whose ledger is fine carries no such detail; nor does a
        // refusal with no record at all (a leftover findings marker on a name with no record).
        let mut healthy = record_matching(&cfg, cfg.resume_max_turns + 1, now);
        healthy.findings_ledger = None;
        let policy_reason =
            resume_block(&cfg, &healthy, &[], now).expect("too many turns is refused");
        assert_eq!(
            resume_refusal("default", policy_reason, Some(&healthy)).detail,
            None
        );
        assert_eq!(
            resume_refusal("default", "no record".to_string(), None).detail,
            None
        );
    }

    #[test]
    fn a_valid_findings_ledger_still_resumes() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let now = 1_000_000;
        let ev = crate::findings::evaluate_turn(
            "s",
            1,
            "rv-1-1",
            "<<<CROSS_REVIEW_FINDINGS_IN:rv-1-1>>>\n{\"verdict\":\"approve\"}\n<<<CROSS_REVIEW_FINDINGS_IN_END:rv-1-1>>>",
            None,
            crate::findings::Budget::default(),
        );
        let mut rec = record_matching(&cfg, 1, now);
        rec.findings_ledger = Some(serde_json::to_value(&ev.ledger).expect("serialize"));
        assert!(resume_block(&cfg, &rec, &[], now).is_none());
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

    /// A completed git capture summary for the render tests.
    fn git_summary() -> crate::vcs::CaptureSummary {
        crate::vcs::CaptureSummary::Git {
            range: "git diff a1b2c3d4e5f6..0f1e2d3c4b5a".into(),
            files: 12,
            insertions: 487,
            deletions: 89,
            untracked_files: 0,
            untracked_files_floor: false,
            diff_truncated: false,
            diff_incomplete: false,
            complete: true,
        }
    }

    fn completed_outcome(capture_summary: Option<crate::vcs::CaptureSummary>) -> Outcome {
        Outcome {
            review: Some("APPROVE".into()),
            failure: None,
            denials: Vec::new(),
            denial_count: 0,
            denial_count_is_floor: false,
            warnings: Vec::new(),
            disposition: Some(crate::vcs::Disposition::Incremental(
                crate::vcs::disposition::Incremental::GitRange {
                    prior: "aaaaaaaaaaaa".into(),
                    head: "bbbbbbbbbbbb".into(),
                    commits: Some(1),
                },
            )),
            capture_summary,
            resumable: true,
            usage: crate::metrics::Usage::default(),
            active: None,
            envelope: None,
        }
    }

    fn render_completed_for(capture_summary: Option<crate::vcs::CaptureSummary>) -> String {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        let (id, _c) = app.registry().try_start("default", 2, true).expect("start");
        app.registry()
            .finish(&id, completed_outcome(capture_summary));
        app.review_result(
            &json!({"review_id": id, "wait_seconds": 0}),
            &RequestCancel::new(),
        )
        .expect("completed render")
    }

    #[test]
    fn the_captured_line_is_rendered_and_precedes_the_disposition_line() {
        let out = render_completed_for(Some(git_summary()));
        assert!(
            out.contains(
                "captured:  git diff a1b2c3d4e5f6..0f1e2d3c4b5a — 12 files, +487/-89, 0 untracked \
                 — diff within budget — complete"
            ),
            "{out}"
        );
        let captured = out.find("captured:").expect("captured line");
        let disposition = out.find("disposition:").expect("disposition line");
        assert!(
            captured < disposition,
            "captured must precede disposition:\n{out}"
        );
    }

    #[test]
    fn no_captured_line_when_the_turn_sent_no_change() {
        let out = render_completed_for(None);
        assert!(!out.contains("captured:"), "{out}");
        // The rest of the response is unaffected -- the disposition still renders.
        assert!(out.contains("disposition:"), "{out}");
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
    fn a_cancelled_result_poll_detaches_without_stopping_the_review() {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let app = App::new(cfg);
        // Registered directly: starting one for real would need a reviewer CLI.
        let (id, cancel) = app.registry.try_start("default", 1, false).expect("start");

        let request = RequestCancel::new();
        // Cancelled before the poll named its review: nothing is bound yet, so there is nothing
        // even to detach here -- the poll itself will notice on attach_wait.
        assert_eq!(request.cancel(), crate::cancel::CancelAction::Nothing);
        // The poll returns CANCELLED (the response is suppressed), but it must NOT stop the
        // reviewer: the caller holds the review_id and can still collect it.
        let err = app
            .review_result(&json!({"review_id": id, "wait_seconds": 300}), &request)
            .unwrap_err();
        assert_eq!(err.code, "CANCELLED");
        assert!(
            !cancel.load(std::sync::atomic::Ordering::SeqCst),
            "abandoning a poll must not cancel the review"
        );
        assert_eq!(
            app.registry.snapshot(&id).expect("still tracked").status,
            Status::Running,
            "the review must still be running and collectible after a poll cancellation"
        );
    }

    #[test]
    fn a_live_result_call_detaches_rather_than_owns_its_review() {
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
        // Bound to the request as a *waiter*, so a cancellation arriving now detaches the poll
        // rather than killing the review.
        assert_eq!(request.cancel(), crate::cancel::CancelAction::Detach);
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
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
    fn wait_seconds_is_capped_at_the_configured_budget() {
        // The cap now tracks the review budget rather than a fixed 300, so a single blocking call
        // can cover a whole review. An over-large request is still clamped so an agent cannot pin
        // the server open with wait_seconds=99999.
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let max = cfg.max_wait_secs();
        assert!(max > 300, "the cap should now exceed the old fixed 300");

        let clamp = |v: u64| v.min(max);
        assert_eq!(
            clamp(99_999),
            max,
            "an over-large wait is clamped to the cap"
        );

        // A non-default --timeout-seconds moves the cap with it, so a 1-hour budget can be waited
        // out in one call.
        let cfg2 = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--timeout-seconds".into(),
            "3600".into(),
        ])
        .expect("config");
        assert!(
            cfg2.max_wait_secs() > cfg.max_wait_secs(),
            "the cap must follow --timeout-seconds"
        );
    }

    #[test]
    fn an_omitted_wait_seconds_blocks_to_completion() {
        // The default is the full cap, so the no-argument collect is one blocking call.
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        let max = cfg.max_wait_secs();
        let args = json!({});
        let wait = args
            .get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(max)
            .min(max);
        assert_eq!(wait, max);
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
