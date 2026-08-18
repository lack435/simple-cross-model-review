//! Named review sessions.
//!
//! The calling agent only ever deals in names it chooses ("default", "auth-refactor").
//! We keep the mapping from that name to the reviewer CLI's own opaque session id on
//! disk, so a review session survives an MCP server restart.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::vcs::baseline::{CaptureIdentity, PerforceBaseline};

/// Distinguishes temp files written by concurrent writers in this process.
static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// The three states a turn's in-progress marker can be read in. `Present` and `Unreadable` both
/// disable the next elision (fail-closed), but the resume disposition reports them as different
/// reasons -- a confirmed leftover marker versus a marker whose state could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker: the previous turn cleared it, so its baseline is trustworthy.
    Absent,
    /// A marker is present: the previous turn crashed or failed to persist.
    Present,
    /// The marker state could not be read (an I/O error). Fail-closed to "do not elide".
    Unreadable,
}

/// How long to wait for another process to release the session file. Short: this only
/// guards a read-modify-write of a small JSON file.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// A reviewer entry's binary *as configured*, stored so a resume can match the entry that
/// created a session without resolving (and thus preflighting) any other entry.
///
/// The tag is what makes a **new** PATH-backed entry (`PathSearch`) distinguishable from a
/// **legacy** record that predates the field (`SessionRecord::raw_bin == None`): a bare
/// `Option<PathBuf>` would collapse both to `None` and misapply the legacy exact-one rule. See
/// `docs/reviewer-fallback-chain.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawBin {
    /// No `--bin` was given: the CLI is resolved from PATH.
    PathSearch,
    /// An explicit `--bin <path>` was configured.
    Explicit(String),
}

impl RawBin {
    /// Whether two configured bins name the same install for resume/duplicate purposes.
    ///
    /// The tags must agree (`PathSearch` never matches an `Explicit`), and two `Explicit`
    /// payloads are compared as paths (Windows-case- and separator-insensitive) via
    /// `pathcmp`, not as raw strings -- so a case- or separator-only difference in a `--bin`
    /// path is the same bin. Distinct from the derived `PartialEq`, which stays byte-exact for
    /// serialization and any caller that wants literal equality.
    pub fn identity_matches(&self, other: &RawBin) -> bool {
        match (self, other) {
            (RawBin::PathSearch, RawBin::PathSearch) => true,
            (RawBin::Explicit(a), RawBin::Explicit(b)) => crate::pathcmp::identity_eq_str(a, b),
            _ => false,
        }
    }
}

/// The account-profile selector a session was created under, persisted so a resume cannot cross an
/// account. The session-side mirror of `config::ProfileSelector` (kept here, like [`RawBin`], because
/// `config` imports `session` and not the reverse); the conversion is done at the call site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileSelectorId {
    Ambient,
    Named(String),
    ExplicitHome(String),
}

/// The account identity a session was created under.
///
/// Presence of this field distinguishes a **new** session (which always records `Some`, even for
/// ambient) from a **legacy** record predating the field (`SessionRecord::profile_identity == None`),
/// exactly as [`RawBin`] distinguishes a new PATH entry from a legacy one — a legacy record has no
/// captured account and so is fail-closed non-resumable. The `selector`/`effective_home` pin *which*
/// account home this ran under; `account_fingerprint` pins *which account* was in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIdentity {
    pub selector: ProfileSelectorId,
    /// The canonical resolved config home (a string path). `None` only for `Ambient`, which has no
    /// profile home. Part of identity so the same `Named(name)` under a different base is a different
    /// identity, not just a different account (f7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_home: Option<String>,
    /// The account fingerprint captured at record time (Codex `tokens.account_id`, Claude
    /// account/org uuid). `None` when it could not be read — which, by the uniform fail-closed
    /// contract, makes the session non-resumable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_fingerprint: Option<String>,
}

impl ProfileIdentity {
    /// Whether a resume from `self` (stored) to `current` (freshly resolved/read) is allowed.
    ///
    /// The uniform fail-closed contract (f14): the selector and effective home must match, and a
    /// captured account fingerprint must be **present on both sides and equal**. A missing fingerprint
    /// on either side refuses — an unbindable session (none captured at record time) or a current
    /// account that cannot be read now is not resumed under an assumed identity.
    pub fn resume_matches(&self, current: &ProfileIdentity) -> bool {
        self.selector == current.selector
            && self.effective_home == current.effective_home
            && matches!(
                (&self.account_fingerprint, &current.account_fingerprint),
                (Some(a), Some(b)) if a == b
            )
    }
}

/// A session created by `cross_model_review`. The default kind, and the value a legacy record
/// (written before the `kind` field existed) is read as.
pub const KIND_REVIEW: &str = "review";
/// A session created by `cross_model_consult`. Its record carries no findings ledger and is never
/// resumed by a review, nor a review session by a consult. See `docs/cross-model-consult-plan.md`.
pub const KIND_CONSULT: &str = "consult";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    pub reviewer: String,
    /// The reviewer CLI's session identifier: a Claude Code session uuid, or a Codex
    /// thread id. Both are stable across resumes (verified).
    pub cli_session_id: String,
    pub model: String,
    pub effort: String,
    pub cwd: String,
    /// Which start path created this session: [`KIND_REVIEW`] or [`KIND_CONSULT`]. A resume must
    /// match kind — a review cannot resume a consult conversation, nor a consult a review — because
    /// the two are shaped for different protocols (a review has a findings ledger and convergence; a
    /// consult has neither). `None` on a record written before this field existed, read as
    /// [`KIND_REVIEW`], the only kind that then existed. See `docs/cross-model-consult-plan.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub turns: u32,
    pub created_unix: u64,
    pub updated_unix: u64,
    /// The last cumulative usage this session's reviewer reported, when it reports
    /// cumulatively at all.
    ///
    /// Codex reports the whole thread's running total on every turn, and the per-turn
    /// figure is nowhere in its event stream -- so the only way to get one is to subtract
    /// the previous total, which means remembering it. Absent for Claude, which reports
    /// per turn already, and absent on a session recorded before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<crate::metrics::Usage>,
    /// The Perforce changelist set this session is bound to, canonicalised (sorted, deduped).
    ///
    /// A review session follows one changelist set: a resume that names a different set is
    /// refused rather than silently continuing against work the reviewer never saw. `None` is
    /// a git session (no binding) or a session recorded before this field existed, both of
    /// which the resume check treats as unbound. The pending changelists' *contents* may move
    /// between turns -- that is expected in a re-review and is handled by re-capturing, not by
    /// this field, which is identity only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<u64>>,
    /// The git commit (`HEAD`) this session's last turn captured, when the backend is git and
    /// HEAD could be resolved.
    ///
    /// The next turn reviews only what changed since it (`<head_sha>..HEAD`) instead of the
    /// whole configured range, so the reviewer -- which already holds the earlier full diff in
    /// its resumed conversation -- is not re-sent a near-duplicate every turn. Advanced to the
    /// latest captured HEAD on every turn, unlike `changes`, which is an invariant binding.
    /// `None` for a Perforce session, a session recorded before this field existed, or a turn
    /// where HEAD could not be resolved (no commits yet, detached, git unavailable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// The resolved effective base of the range `head_sha` was captured under. The next turn
    /// deltas against `head_sha` only when its own range resolves to this same base; otherwise
    /// the base ref moved (or the mode changed) and a full capture is taken instead. Paired
    /// with `head_sha`: the two advance together on a complete capture and are retained
    /// together otherwise, so a `head` never sits beside a `base` from a different turn. `None`
    /// for Perforce or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Which capture backend this session belongs to (`"git"` or `"perforce"`), so a resume
    /// cannot cross backends: a git record must never satisfy the Perforce-only binding logic,
    /// nor vice versa. `None` for a record written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Whether the last Perforce turn was capturing shelved content. Part of the resume
    /// binding: a session that showed a shelf and then omits it (or the reverse) must not
    /// silently leave stale evidence in scope, so a change here refuses the incremental resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_shelved: Option<bool>,
    /// The resolved Perforce capture identity (server, client, charset, client-spec digest) the
    /// last turn ran under. A resume whose identity differs re-captures in full. `None` for git
    /// or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_identity: Option<CaptureIdentity>,
    /// The Perforce resume-delta baseline the next turn collapses against: the last *persisted*
    /// turn's per-file inventory (`Full`) or `Disabled` when that turn was incomplete. Written
    /// explicitly every Perforce turn -- never inherited from a prior turn -- so a stale
    /// inventory can never be eluded against. `None` for git or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perforce_baseline: Option<PerforceBaseline>,
    /// The consult *capture contract*: whether this consult session was created with
    /// `include_change: true`. Bound for the session's life — a resume whose effective
    /// `include_change` differs is refused (`resume_block`), so a conversation built tree-only is
    /// never continued against a captured diff, nor the reverse. `None` on a review record (the
    /// contract is consult-only) or a record predating this field; both read as tree-only
    /// (effective `false`). See `docs/cross-model-consult-include-change-impl.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_change: Option<bool>,
    /// **Legacy migration marker only** (retire-capture-modes / f5). Git no longer has a capture
    /// mode — `DiffMode`, `cfg.diff` and `--diff` are gone, and the change is derived live through
    /// `repository_diff` — so a *current* record always writes `None` here. It is retained solely to
    /// detect an *old* record written under the retired static-capture contract: a git consult record
    /// carrying `Some(diff_mode)` has change semantics this server can no longer match, so it is
    /// refused on resume (`resume_block`) and the caller rebaselines. `None` for every current record
    /// (review or consult) and a record predating the field; Perforce is bound by its changelist set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_mode: Option<String>,
    /// The reviewer entry's binary *as configured* (raw), used to match the chain entry that
    /// created this session on resume without resolving any other entry. `None` on a record
    /// written before the reviewer chain existed — matched leniently (exactly-one) then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_bin: Option<RawBin>,
    /// The path the entry's binary *resolved to* when the session was created. Verified against a
    /// fresh (uncached) resolution on resume: a mismatch means PATH now points at a different
    /// executable/account, and the resume is refused rather than continued through the wrong
    /// binary. `None` on a legacy record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_bin: Option<String>,
    /// The structured findings ledger for this session, stored as a raw JSON value rather than a
    /// typed field on purpose: a single unreadable or incompatible ledger degrades only *this*
    /// session (to [`LedgerLoad::Invalid`]) instead of failing the whole store parse, which is the
    /// difference between a bad record and a corrupt store. `None` for a session with no ledger
    /// yet, or a record written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings_ledger: Option<serde_json::Value>,
    /// A sticky terminal escalation state: `"ledger_too_large"` (the session outgrew a single review
    /// conversation) or `"session_stagnant"` (it went `--stagnant-session-turns` turns without
    /// raising or resolving a finding while findings were still open). Once set, a resume is refused
    /// until the session is restarted `fresh`.
    ///
    /// Written from the turn's *selected* non-convergence reason, and only for the reasons
    /// `NonConvergenceReason::sticky_terminal` names — `ledger_unavailable` and `turn_not_durable`
    /// are grave but are recorded as ledger coverage, so neither lands here.
    ///
    /// `None` for a healthy session or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    /// Which working-directory mode the Claude reviewer ran in when this turn was recorded
    /// (`crate::reviewer::CWD_MODE_PROJECT` / `CWD_MODE_NEUTRAL`). A resume whose current mode
    /// differs cannot reuse the conversation -- it lives under the other process cwd -- so the
    /// resume is rebound to a fresh turn instead of failing. `None` on a record written before
    /// this field existed, which is treated as "project" (the only mode that existed then). See
    /// `docs/resume-cache-cwd-invalidation.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_cwd_mode: Option<String>,
    /// The account identity this session was created under (selector, effective home, account
    /// fingerprint). A resume whose freshly-resolved identity does not [`ProfileIdentity::resume_matches`]
    /// the stored one is refused, so a session cannot resume under a different account or profile.
    /// `None` on a record written before this field existed (legacy): fail-closed non-resumable, since
    /// its account was never captured and cannot be verified. See `docs/reviewer-account-profiles.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_identity: Option<ProfileIdentity>,
}

/// The tri-state result of loading a session's findings ledger. A single record whose ledger is
/// unreadable or at an incompatible version is `Invalid` — that session is refused a resume — while
/// the store and other sessions stay usable (the difference from a corrupt *store*, [`StoreState`]).
#[derive(Clone, Debug)]
pub enum LedgerLoad {
    /// No ledger has been attached to this session yet.
    Absent,
    /// A readable ledger at a compatible version.
    Valid(crate::findings::Ledger),
    /// A ledger value is present but could not be read as a compatible ledger.
    Invalid,
}

impl SessionRecord {
    /// This session's kind, resolving a legacy `None` to [`KIND_REVIEW`]. The cross-kind resume
    /// refusal compares against this, never the raw `Option`, so a pre-`kind` record reads as a
    /// review rather than as "no kind".
    pub fn kind(&self) -> &str {
        self.kind.as_deref().unwrap_or(KIND_REVIEW)
    }

    /// Load this session's findings ledger, tri-state. `Invalid` is a durable poison: a resume of
    /// such a session is refused before the model call (an unreadable ledger cannot be injected).
    pub fn ledger_load(&self) -> LedgerLoad {
        match &self.findings_ledger {
            None => LedgerLoad::Absent,
            Some(v) => match serde_json::from_value::<crate::findings::Ledger>(v.clone()) {
                // Beyond deserializing at a compatible version, the ledger must be structurally
                // sound — unique ids and a `next_seq` strictly greater than every existing id — or
                // reconciliation could mint a colliding id. Fail-closed: anything else is `Invalid`.
                Ok(l)
                    if l.schema_version == crate::findings::LEDGER_SCHEMA_VERSION
                        && l.is_structurally_valid() =>
                {
                    LedgerLoad::Valid(l)
                }
                _ => LedgerLoad::Invalid,
            },
        }
    }
}

/// Whether the session *store* file parses. Distinct from a single bad ledger record: a corrupt
/// store means every session is inaccessible, so all reviews are refused before the model call
/// rather than silently starting a clean, convergeable conversation over unreadable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreState {
    /// No store file yet — genuinely empty, the normal first-run state.
    Absent,
    /// The store parses.
    Valid,
    /// The store file is present but did not parse. Refuse resume *and* fresh.
    Corrupt,
}

/// What a completed turn contributes to its session's record.
///
/// A struct rather than a parameter list: six of these in a row, five of them `&str`,
/// is a call site where two arguments can be transposed without the compiler noticing.
pub struct TurnFacts<'a> {
    pub reviewer: &'a str,
    /// The reviewer CLI's own session identifier for this conversation.
    pub cli_session_id: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub cwd: &'a str,
    /// Which start path produced this turn: [`KIND_REVIEW`] or [`KIND_CONSULT`]. Invariant across a
    /// session (a cross-kind resume is refused before a turn runs), so it is stored directly.
    pub kind: &'a str,
    /// The running total this turn reported, for reviewers that report cumulatively.
    pub cumulative_usage: Option<crate::metrics::Usage>,
    /// The canonical Perforce changelist set this session is bound to, or `None` for git.
    pub changes: Option<Vec<u64>>,
    /// The git HEAD this turn captured, so the next turn can review only what changed since
    /// it, and the resolved effective base of the range it was captured under. Both `None`
    /// together for Perforce, an unresolved HEAD, or a truncated capture; a resume only deltas
    /// when it has both.
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    /// The capture backend (`"git"`/`"perforce"`), the shelved-capture flag, the resolved
    /// capture identity, and the Perforce delta baseline this turn produced. All `None` for a
    /// git turn; a Perforce turn always supplies `backend`, `include_shelved`, `identity` and a
    /// `perforce_baseline` (`Full` or `Disabled`).
    pub backend: Option<&'a str>,
    pub include_shelved: Option<bool>,
    pub capture_identity: Option<CaptureIdentity>,
    pub perforce_baseline: Option<PerforceBaseline>,
    /// The consult capture contract this turn ran under: whether the consult includes the change
    /// (`include_change`), and the *configured* git `DiffMode` as its canonical string. Both `None`
    /// for a review turn (the contract is consult-only) and a Perforce consult carries `diff_mode`
    /// `None` (bound by its changelist set instead). Invariants for the session's life, so they
    /// persist-then-inherit like `changes`. See `docs/cross-model-consult-include-change-impl.md`.
    pub include_change: Option<bool>,
    pub diff_mode: Option<String>,
    /// The active reviewer entry's binary as configured, and the path it resolved to, so a resume
    /// can match this entry and detect PATH drift.
    pub raw_bin: RawBin,
    pub resolved_bin: String,
    /// The reconciled findings ledger this turn produced, already serialized. Stored directly (the
    /// new ledger already includes the prior findings) — never inherited from a prior turn. `None`
    /// for a turn that produced no ledger.
    pub findings_ledger: Option<serde_json::Value>,
    /// The sticky terminal state this turn resolves to, or `None`. Set on the record so a later
    /// resume is refused. Derived from the turn's selected non-convergence reason, so it is exactly
    /// the reasons `NonConvergenceReason::sticky_terminal` names: `"ledger_too_large"` and
    /// `"session_stagnant"`.
    pub terminal_reason: Option<String>,
    /// The working-directory mode the reviewer ran in this turn (`CWD_MODE_PROJECT` /
    /// `CWD_MODE_NEUTRAL`), stored so a later resume can detect a mode change it cannot survive.
    pub reviewer_cwd_mode: &'a str,
    /// The account identity this turn ran under (selector, resolved home, account fingerprint),
    /// recorded so a later resume can refuse an account/profile change. Always `Some` on the live
    /// review path; `None` only leaves the record at the fail-closed legacy state (used by tests that
    /// do not exercise profiles).
    pub profile_identity: Option<ProfileIdentity>,
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    sessions: BTreeMap<String, SessionRecord>,
}

pub struct SessionStore {
    path: PathBuf,
    /// Serialises our own writes. Cross-process safety comes from re-reading the file
    /// inside the lock before every mutation, plus an atomic rename on write.
    lock: Mutex<()>,
}

impl SessionStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("sessions.json"),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A sibling of the state file rather than the state file itself: locking the file we
    /// are about to atomically replace would fight with the replace.
    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    /// Fetch a record, tolerant of a corrupt store (reads it as empty) — the single-record parallel
    /// to [`list`](Self::list). The review path deliberately does **not** use this: it goes through
    /// the fail-closed [`get_checked`](Self::get_checked) so a transient read error cannot masquerade
    /// as "no record" and overwrite a real one. Retained as part of the documented accessor set and
    /// exercised by the tests below; it currently has no production caller.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<SessionRecord> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read().sessions.get(name).cloned()
    }

    /// Like [`get`](Self::get), but fail-closed: a corrupt/unreadable store is an `Err`, not a
    /// silent `Ok(None)`. The review path uses this so a transient read error after the preflight
    /// `store_state` check cannot make the worker behave as if the session were absent (and then
    /// overwrite the real record on `record_turn`). `Ok(None)` means genuinely absent.
    pub fn get_checked(&self, name: &str) -> io::Result<Option<SessionRecord>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read_or_corrupt()?.sessions.get(name).cloned())
    }

    pub fn list(&self) -> Vec<(String, SessionRecord)> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read().sessions.into_iter().collect()
    }

    /// Record the result of a completed turn, creating the session on first use.
    pub fn record_turn(&self, name: &str, turn: TurnFacts) -> io::Result<SessionRecord> {
        let TurnFacts {
            reviewer,
            cli_session_id,
            model,
            effort,
            cwd,
            kind,
            cumulative_usage,
            changes,
            head_sha,
            base_sha,
            backend,
            include_shelved,
            capture_identity,
            perforce_baseline,
            include_change,
            diff_mode,
            raw_bin,
            resolved_bin,
            findings_ledger,
            terminal_reason,
            reviewer_cwd_mode,
            profile_identity,
        } = turn;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // Held across the read and the write: this is a read-modify-write, so another
        // process reading between the two would write back a snapshot missing this turn.
        // A failure to lock is returned, not ignored, and reaches the caller as a warning.
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
        let mut store = self.read_or_corrupt()?;
        let now = now_unix();

        // The baseline is a (head, base) pair the next turn deltas from, so it advances as a
        // unit: a turn that produced a complete pair replaces it; any incomplete one (a
        // truncated capture, an unresolved HEAD, a Perforce turn) retains the prior pair intact
        // rather than storing a half of one. The prior pair is only inherited from the *same*
        // conversation -- a fresh or rebound reviewer session (a different `cli_session_id`
        // under this name) never saw the old diff, so an incomplete first turn there must store
        // nothing rather than a baseline the new reviewer cannot resume against.
        let prior = store
            .sessions
            .get(name)
            .filter(|p| p.cli_session_id == cli_session_id);
        let (head_sha, base_sha) = match (&head_sha, &base_sha) {
            (Some(_), Some(_)) => (head_sha, base_sha),
            _ => (
                prior.and_then(|p| p.head_sha.clone()),
                prior.and_then(|p| p.base_sha.clone()),
            ),
        };

        // Profile identity: exactly this turn's, never a carry-forward. A turn that could not read
        // the account fingerprint records `None`, which makes the session non-resumable -- the
        // uniform fail-closed contract. Carrying a prior turn's fingerprint forward was removed
        // (code-review f3): it could record a fingerprint the *current* turn did not verify, masking
        // an account switch that a partial credential-file read left unseen. Tolerating a transient
        // read failure instead belongs to the Phase 3 start-vs-final identity guard, which retains
        // the account asserted at spawn and verifies the final one equals it before delivery.

        let record = match store.sessions.get(name) {
            // Same underlying session: another turn on it. The changelist binding is
            // invariant across turns (a mismatched resume is refused before we get here), so
            // carrying this turn's value forward keeps it while tolerating a `None` from a
            // git turn without erasing an existing binding.
            Some(existing) if existing.cli_session_id == cli_session_id => SessionRecord {
                turns: existing.turns.saturating_add(1),
                created_unix: existing.created_unix,
                updated_unix: now,
                reviewer: reviewer.to_string(),
                cli_session_id: cli_session_id.to_string(),
                model: model.to_string(),
                effort: effort.to_string(),
                cwd: cwd.to_string(),
                // Invariant across a session: a cross-kind resume is refused before a turn runs, so
                // this turn's kind equals the existing record's. Stored from this turn either way.
                kind: Some(kind.to_string()),
                cumulative_usage,
                changes: changes.or(existing.changes.clone()),
                // The (head, base) baseline was already resolved above as a unit -- this turn's
                // complete pair, or the prior one retained -- so it is stored as-is here.
                head_sha,
                base_sha,
                // Backend, shelved flag and capture identity are the binding: stable across a
                // session, so carry this turn's value while tolerating a `None` from a turn that
                // did not supply one rather than erasing what is bound.
                backend: backend.map(str::to_string).or(existing.backend.clone()),
                include_shelved: include_shelved.or(existing.include_shelved),
                capture_identity: capture_identity.or(existing.capture_identity.clone()),
                // The delta baseline, unlike the binding, is *this* turn's alone: it reflects
                // exactly what the reviewer was last shown, so it is stored directly and never
                // inherited -- a stale inventory must never be eluded against.
                perforce_baseline,
                // The consult capture contract is a session-life invariant like the changelist
                // binding, so carry this turn's value while tolerating a `None` (a review turn, or a
                // Perforce consult's `diff_mode`) rather than erasing what is bound.
                include_change: include_change.or(existing.include_change),
                diff_mode: diff_mode.or(existing.diff_mode.clone()),
                raw_bin: Some(raw_bin),
                resolved_bin: Some(resolved_bin),
                // The findings ledger and terminal state are this turn's alone: the new ledger
                // already includes the prior findings, and the terminal state is recomputed each
                // turn (sticky terminal states are re-supplied by the caller from the prior record).
                findings_ledger,
                terminal_reason,
                reviewer_cwd_mode: Some(reviewer_cwd_mode.to_string()),
                profile_identity: profile_identity.clone(),
            },
            // New session, or the name was rebound to a fresh reviewer session.
            _ => SessionRecord {
                reviewer: reviewer.to_string(),
                cli_session_id: cli_session_id.to_string(),
                model: model.to_string(),
                effort: effort.to_string(),
                cwd: cwd.to_string(),
                kind: Some(kind.to_string()),
                turns: 1,
                created_unix: now,
                updated_unix: now,
                cumulative_usage,
                changes,
                head_sha,
                base_sha,
                backend: backend.map(str::to_string),
                include_shelved,
                capture_identity,
                perforce_baseline,
                include_change,
                diff_mode,
                raw_bin: Some(raw_bin),
                resolved_bin: Some(resolved_bin),
                findings_ledger,
                terminal_reason,
                reviewer_cwd_mode: Some(reviewer_cwd_mode.to_string()),
                profile_identity: profile_identity.clone(),
            },
        };

        store.sessions.insert(name.to_string(), record.clone());
        self.write(&store)?;
        Ok(record)
    }

    /// The path of a session's in-progress marker with the given extension. A sibling of the state
    /// file, keyed by name the same way the session lock is, so it survives a crash independently of
    /// the JSON. Two distinct markers ride this: `.pending` (the Perforce resume-delta baseline) and
    /// `.findings-pending` (the findings write-ahead) — kept separate so the findings resume-refusal
    /// does not preempt the Perforce full-capture fallback, which are different responses to a
    /// crashed turn.
    fn marker_path(&self, name: &str, ext: &str) -> PathBuf {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(48)
            .collect();
        dir.join(format!(
            "session-{safe}-{:016x}.{ext}",
            crate::config::fnv1a64(name)
        ))
    }

    fn pending_marker(&self, name: &str) -> PathBuf {
        self.marker_path(name, "pending")
    }

    /// Mark a Perforce turn as in progress. The marker is written *before* the turn does anything
    /// that could deliver a review without persisting its baseline, and cleared only once the turn
    /// is durably recorded. A crash, panic, or write failure in between therefore leaves it set,
    /// and [`marker_state`](Self::marker_state) tells the next resume to fall back to a full
    /// capture rather than collapse against a baseline that never advanced.
    ///
    /// Returns an error if the marker could not be written: the caller must then refuse to produce
    /// a resumable baseline this turn, since a later crash could not be detected.
    pub fn mark_pending(&self, name: &str) -> io::Result<()> {
        let path = self.pending_marker(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, b"")
    }

    /// Clear the in-progress marker after a turn is durably recorded. `Ok(())` on a successful
    /// delete or an already-absent marker; `Err` if the delete failed and the marker may survive
    /// (which would wrongly disable future elision, so the caller surfaces it).
    pub fn clear_pending(&self, name: &str) -> io::Result<()> {
        match std::fs::remove_file(self.pending_marker(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The three states of the previous turn's in-progress marker. Kept distinct because the
    /// resume disposition must tell a *confirmed* leftover marker (`Present` -- the previous turn
    /// crashed or failed to persist) from a marker state that could not be *read* (`Unreadable`):
    /// both disable elision, but they are different facts to report to the caller, and folding
    /// the I/O error into "present" would be a claim the read cannot support.
    pub fn marker_state(&self, name: &str) -> MarkerState {
        match self.pending_marker(name).try_exists() {
            Ok(true) => MarkerState::Present,
            Ok(false) => MarkerState::Absent,
            Err(_) => MarkerState::Unreadable,
        }
    }

    /// The findings write-ahead marker, distinct from the Perforce `.pending` baseline marker. It is
    /// written before *every* reviewer turn and cleared only once the turn is durably recorded, so a
    /// crash or persist failure leaves it set for the next resume to refuse — the ledger is then
    /// stale relative to the reviewer's advanced conversation, and resuming could mint colliding ids.
    /// Kept separate so refusing a findings resume never preempts the Perforce full-capture fallback.
    pub fn mark_findings_pending(&self, name: &str) -> io::Result<()> {
        let path = self.marker_path(name, "findings-pending");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, b"")
    }

    /// Clear the findings write-ahead marker after a turn is durably recorded. `Ok(())` on a
    /// successful delete or an already-absent marker; `Err` if the delete failed (a surviving marker
    /// over-refuses the next resume toward `fresh`, the safe direction, so the caller surfaces it).
    pub fn clear_findings_pending(&self, name: &str) -> io::Result<()> {
        match std::fs::remove_file(self.marker_path(name, "findings-pending")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether the findings write-ahead marker is present. Fail-closed: an I/O error reading it
    /// counts as `Unreadable` (treated as set), so an unreadable marker refuses the resume rather
    /// than risking a resume on a possibly-stale ledger.
    pub fn findings_marker_state(&self, name: &str) -> MarkerState {
        match self.marker_path(name, "findings-pending").try_exists() {
            Ok(true) => MarkerState::Present,
            Ok(false) => MarkerState::Absent,
            Err(_) => MarkerState::Unreadable,
        }
    }

    /// Whether the store file parses, tri-state. Used to gate *every* review — resume and fresh
    /// alike — when the store is `Corrupt`: a corrupt store is caught before the model call and is a
    /// resume refusal, never a silent clean start over unreadable state (which could converge on
    /// untracked history). A missing file is `Absent` (the normal first-run state), not corrupt.
    pub fn store_state(&self) -> StoreState {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::read_to_string(&self.path) {
            // Only a genuinely missing file is `Absent`. Any other read error (a permission or I/O
            // failure) is fail-closed to `Corrupt` — better to refuse than to proceed as if empty
            // and later overwrite whatever is really there.
            Err(e) if e.kind() == io::ErrorKind::NotFound => StoreState::Absent,
            Err(_) => StoreState::Corrupt,
            // An existing but empty/whitespace-only file is anomalous: `write` only ever produces a
            // fully-serialized store via temp+rename, so a zero-byte `sessions.json` is corruption
            // (e.g. an external truncation), not the normal first-run state — treat it as corrupt.
            Ok(text) if text.trim().is_empty() => StoreState::Corrupt,
            Ok(text) => match serde_json::from_str::<StoreFile>(&text) {
                Ok(_) => StoreState::Valid,
                Err(_) => StoreState::Corrupt,
            },
        }
    }

    /// Set a session's sticky `terminal_reason` without advancing a turn — used by the
    /// before-reviewer over-budget path, which runs no reviewer and records no turn but must make
    /// the terminal state durable so the next resume is refused. Idempotent: writing the same
    /// reason again is a no-op replace. Returns whether the named session existed.
    pub fn set_terminal_reason(&self, name: &str, reason: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
        let mut store = self.read_or_corrupt()?;
        match store.sessions.get_mut(name) {
            Some(record) => {
                record.terminal_reason = Some(reason.to_string());
                self.write(&store)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Remove a session mapping. One of the four documented store accessors
    /// (`get`/`record_turn`/`forget`/`list`) that must be fail-closed on a corrupt store — it reads
    /// through [`read_or_corrupt`](Self::read_or_corrupt), so it errors rather than silently treating
    /// an unreadable store as empty. It currently has no production caller: failed-turn recovery
    /// relies on the durable write-ahead markers (which refuse a resume) and preserves the prior
    /// record and its findings ledger for a human-directed rebaseline, rather than deleting the
    /// mapping. Retained as part of that accessor contract and exercised by the tests below.
    #[allow(dead_code)]
    pub fn forget(&self, name: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
        let mut store = self.read_or_corrupt()?;
        let removed = store.sessions.remove(name).is_some();
        if removed {
            self.write(&store)?;
        }
        Ok(removed)
    }

    /// A missing or corrupt store is treated as empty for *read* accessors (`get`/`list`): losing
    /// the ability to resume is recoverable, and the review path is gated on [`store_state`] before
    /// anything is spawned. **Mutators do not use this** — see [`read_or_corrupt`](Self::read_or_corrupt).
    fn read(&self) -> StoreFile {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!(
                    "cross-review: warning: {} is not readable as session state ({e}); \
                     starting from an empty set of sessions",
                    self.path.display()
                );
                StoreFile::default()
            }),
            Err(_) => StoreFile::default(),
        }
    }

    /// The store for a *mutating* read-modify-write, tri-state. An absent file is `Ok(empty)` (the
    /// normal first-run state), a valid file is `Ok(parsed)`, and a **present-but-unparseable file is
    /// `Err`** — so `record_turn`/`forget`/`set_terminal_reason` refuse rather than overwrite a
    /// corrupt store, which would destroy the evidence and could create a convergeable session over
    /// history it cannot see. Recovery is an explicit operator action.
    fn read_or_corrupt(&self) -> io::Result<StoreFile> {
        match std::fs::read_to_string(&self.path) {
            // Only a genuinely missing file is an empty store. Any other read error, or an existing
            // empty/whitespace-only file (never produced by `write`), is fail-closed to an error, so
            // a mutator refuses rather than overwriting whatever is really there.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(e) => Err(e),
            Ok(text) if text.trim().is_empty() => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is present but empty; refusing to overwrite it. Move it aside or point \
                     --state-dir at a clean directory.",
                    self.path.display()
                ),
            )),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is corrupt (it did not parse: {e}); refusing to overwrite it. Move it \
                         aside or point --state-dir at a clean directory.",
                        self.path.display()
                    ),
                )
            }),
        }
    }

    fn write(&self, store: &StoreFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(store)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Write-then-rename so a crash mid-write cannot truncate existing state. The
        // temp name carries our pid and a counter: a shared name would let two processes
        // clobber each other's half-written file and rename the wrong bytes into place.
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, json)?;

        // std::fs::rename is MoveFileExW with MOVEFILE_REPLACE_EXISTING, which replaces
        // atomically. It can still lose to a transient sharing violation from a scanner
        // or a concurrent reader, so retry briefly. Never unlink the live file first:
        // that trades a retry for a window where the state does not exist at all.
        let mut last = None;
        for attempt in 0..10 {
            match std::fs::rename(&tmp, &self.path) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(20 * (attempt + 1)));
                }
            }
        }
        std::fs::remove_file(&tmp).ok();
        Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
    }
}

// Cross-process locks, backed by the OS rather than by bookkeeping.
//
// The in-process mutex cannot help across processes: two `cross-review` servers pointed at the same
// project share a state directory, and every mutation is a read-modify-write, so without a lock each
// can write back a snapshot taken before the other's change and silently drop it.
//
// Exclusion comes from a byte-range lock (`LockFileEx`) on a fixed range of the lock file, not from
// the file open: every holder opens the file *with* sharing, so their handles coexist, and the
// `LockFileEx` range provides the mutual exclusion. This lets an exclusive holder (setup) and one or
// more shared holders (the review path) contend on the same lock path — an exclusive lock blocks while
// any shared lock is held and vice-versa, while shared locks coexist ([f5]/f9). Two properties still
// fall out of letting Windows own it: the lock releases when the handle closes, *including* when the
// process dies (no stale lock to reason about), and nothing ever deletes the lock file (no window in
// which one process removes a lock another just acquired).
//
// An earlier version opened the file with share-mode zero and tracked liveness itself (stealing any
// lock older than 60 seconds, writing anyway on timeout). Both were wrong. A process merely paused
// could have its lock stolen, and on drop it would then delete the *new* owner's lock; writing anyway
// reinstated the lost-update race the lock existed to prevent.
//
// LockFileEx / the OVERLAPPED it requires. Closing the handle releases any locks it holds, so no
// explicit UnlockFileEx is needed on drop.
const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: *mut std::ffi::c_void,
}

extern "system" {
    fn LockFileEx(
        file: *mut std::ffi::c_void,
        flags: u32,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

/// Take a byte-range lock (`exclusive` or shared) on `path`, retrying until `wait` elapses. The whole
/// file is opened with read/write sharing so holders' handles coexist; a single fixed byte is locked
/// so all holders contend on it.
fn acquire_range_lock(path: &Path, exclusive: bool, wait: Duration) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0x1 | 0x2 | 0x4) // FILE_SHARE_READ | WRITE | DELETE
        .open(path)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot open lock file {}: {e}", path.display()),
            )
        })?;
    let mut flags = LOCKFILE_FAIL_IMMEDIATELY;
    if exclusive {
        flags |= LOCKFILE_EXCLUSIVE_LOCK;
    }
    let deadline = Instant::now() + wait;
    loop {
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        };
        // SAFETY: `file` is a valid open handle held for the call; `overlapped` outlives it. We lock a
        // single byte at offset 0, the range every holder contends on.
        let ok = unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) };
        if ok != 0 {
            return Ok(file);
        }
        let e = io::Error::last_os_error();
        // A lock/sharing violation means someone holds a conflicting lock; anything else is a real
        // error (denied ACL, bad path) that retrying would only stall on.
        if !matches!(
            e.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
        ) {
            return Err(io::Error::new(
                e.kind(),
                format!("cannot lock {}: {e}", path.display()),
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another cross-review process is holding {} ({e})",
                    path.display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// An exclusive hold on a lock path: blocks while any shared or exclusive holder is present.
pub struct ExclusiveLock {
    // Held purely for its side effect: dropping it (closing the handle) releases the lock.
    _file: File,
}

impl ExclusiveLock {
    /// Take the lock exclusively, retrying until `wait` elapses. Failure is returned rather than
    /// ignored, so callers surface it instead of writing unprotected.
    pub fn acquire(path: &Path, wait: Duration) -> io::Result<Self> {
        Ok(Self {
            _file: acquire_range_lock(path, true, wait)?,
        })
    }
}

/// A shared (reader) hold on a lock path: coexists with other shared holders, blocks only while an
/// exclusive holder is present. The review path takes this across a whole attempt so a setup swap
/// (which takes the exclusive side) cannot rename the home out from under a live review ([f5]).
#[allow(dead_code)] // caller lands with the review-path lock wiring (#15 part 3b).
pub struct SharedLock {
    _file: File,
}

impl SharedLock {
    #[allow(dead_code)] // caller lands with the review-path lock wiring (#15 part 3b).
    pub fn acquire(path: &Path, wait: Duration) -> io::Result<Self> {
        Ok(Self {
            _file: acquire_range_lock(path, false, wait)?,
        })
    }
}

/// Lock path for a named review session, used to stop two server processes from
/// resuming the same reviewer conversation at once.
pub fn session_lock_path(state_dir: &Path, session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    // Sanitising is lossy: `a/b` and `a?b` both flatten to `a_b`, and anything past the
    // truncation point disappears. A hash of the whole original name keeps distinct
    // sessions on distinct lock files, so they cannot spuriously report SESSION_BUSY.
    state_dir.join(format!(
        "session-{safe}-{:016x}.lock",
        crate::config::fnv1a64(session)
    ))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A fresh directory per test so they can run in parallel. See `crate::testutil` for
    /// why it is both cleared on the way in and removed on the way out.
    fn temp_dir() -> TempDir {
        crate::testutil::temp_dir("cross-review-session-tests")
    }

    fn record(store: &SessionStore, name: &str, cli_id: &str) -> SessionRecord {
        store
            .record_turn(
                name,
                TurnFacts {
                    reviewer: "codex",
                    cli_session_id: cli_id,
                    model: "gpt-5.6-luna",
                    effort: "max",
                    cwd: "C:\\repo",
                    kind: KIND_REVIEW,
                    cumulative_usage: None,
                    changes: None,
                    head_sha: None,
                    base_sha: None,
                    backend: None,
                    include_shelved: None,
                    capture_identity: None,
                    perforce_baseline: None,
                    include_change: None,
                    diff_mode: None,
                    raw_bin: RawBin::PathSearch,
                    resolved_bin: String::new(),
                    findings_ledger: None,
                    terminal_reason: None,
                    reviewer_cwd_mode: "project",
                    profile_identity: None,
                },
            )
            .expect("record turn")
    }

    fn ident(sel: ProfileSelectorId, home: Option<&str>, fp: Option<&str>) -> ProfileIdentity {
        ProfileIdentity {
            selector: sel,
            effective_home: home.map(String::from),
            account_fingerprint: fp.map(String::from),
        }
    }

    #[test]
    fn resume_matches_requires_same_identity_and_a_matching_fingerprint() {
        let amb = |fp| ident(ProfileSelectorId::Ambient, None, fp);
        // Same account resumes.
        assert!(amb(Some("acct-1")).resume_matches(&amb(Some("acct-1"))));
        // A different account refuses.
        assert!(!amb(Some("acct-1")).resume_matches(&amb(Some("acct-2"))));
        // A missing fingerprint on either side refuses -- the uniform fail-closed contract.
        assert!(!amb(None).resume_matches(&amb(Some("acct-1"))));
        assert!(!amb(Some("acct-1")).resume_matches(&amb(None)));
        assert!(!amb(None).resume_matches(&amb(None)));
        // A different selector refuses even with a matching fingerprint.
        let named = ident(
            ProfileSelectorId::Named("work".into()),
            Some(r"C:\h\work"),
            Some("acct-1"),
        );
        assert!(!amb(Some("acct-1")).resume_matches(&named));
        // Same named account but a different effective home refuses (f7): the same name under a
        // different base is a different identity, not the same account in the same place.
        let named_other_home = ident(
            ProfileSelectorId::Named("work".into()),
            Some(r"D:\h\work"),
            Some("acct-1"),
        );
        assert!(!named.resume_matches(&named_other_home));
    }

    #[test]
    fn profile_identity_round_trips_through_serde() {
        for id in [
            ident(ProfileSelectorId::Ambient, None, Some("a")),
            ident(
                ProfileSelectorId::Named("work".into()),
                Some(r"C:\h\work"),
                Some("a"),
            ),
            ident(
                ProfileSelectorId::ExplicitHome(r"C:\x".into()),
                Some(r"C:\x"),
                None,
            ),
        ] {
            let json = serde_json::to_string(&id).expect("serialize");
            let back: ProfileIdentity = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(id, back);
        }
    }

    fn ambient_facts(cli: &'static str, fp: Option<&'static str>) -> TurnFacts<'static> {
        TurnFacts {
            reviewer: "codex",
            cli_session_id: cli,
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: r"C:\repo",
            kind: KIND_REVIEW,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: Some(ident(ProfileSelectorId::Ambient, None, fp)),
        }
    }

    #[test]
    fn a_turn_records_exactly_its_own_fingerprint_never_a_carry_forward() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        // Turn 1 captured the account.
        store
            .record_turn("s", ambient_facts("thread-1", Some("acct-1")))
            .unwrap();
        // Turn 2 on the same conversation could not read it: it records `None` (non-resumable), not
        // the prior fingerprint. A fingerprint the current turn did not verify must never be
        // recorded -- it could mask an account switch (code-review f3).
        let rec = store
            .record_turn("s", ambient_facts("thread-1", None))
            .unwrap();
        assert!(rec.profile_identity.unwrap().account_fingerprint.is_none());
    }

    #[test]
    fn a_fresh_session_without_a_capturable_fingerprint_stays_unbound() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        // No prior turn to carry forward from: an unbindable new session records `None`, which the
        // resume check treats as non-resumable (f14).
        let rec = store
            .record_turn("s", ambient_facts("thread-1", None))
            .unwrap();
        assert!(rec.profile_identity.unwrap().account_fingerprint.is_none());
    }

    #[test]
    fn unknown_session_is_none() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        assert!(store.get("default").is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn store_state_distinguishes_absent_valid_and_corrupt() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        // No file yet -> Absent (the normal first-run state, not corrupt).
        assert_eq!(store.store_state(), StoreState::Absent);
        // After a turn the store parses -> Valid.
        record(&store, "s", "thread-1");
        assert_eq!(store.store_state(), StoreState::Valid);
        // Corrupt the file -> Corrupt, so all reviews are refused before the model call.
        std::fs::write(store.path(), b"{ not valid json").expect("write garbage");
        assert_eq!(store.store_state(), StoreState::Corrupt);
    }

    #[test]
    fn ledger_load_is_tri_state_per_record() {
        use crate::findings::{Budget, LedgerCoverage};
        let dir = temp_dir();
        let store = SessionStore::new(&dir);

        // Absent: a record with no ledger.
        let rec = record(&store, "s", "thread-1");
        assert!(matches!(rec.ledger_load(), LedgerLoad::Absent));

        // Valid: attach a real ledger produced by the pure module.
        let ev = crate::findings::evaluate_turn(
            "s",
            1,
            "rv-1-1",
            "<<<CROSS_REVIEW_FINDINGS_IN:rv-1-1>>>\n{\"verdict\":\"approve\"}\n<<<CROSS_REVIEW_FINDINGS_IN_END:rv-1-1>>>",
            None,
            Budget::default(),
        );
        let mut rec = rec;
        rec.findings_ledger = Some(serde_json::to_value(&ev.ledger).expect("serialize ledger"));
        match rec.ledger_load() {
            LedgerLoad::Valid(l) => assert_eq!(l.coverage, LedgerCoverage::WholeConversation),
            other => panic!("expected valid ledger, got {other:?}"),
        }

        // Invalid: a ledger value that is not a compatible ledger.
        rec.findings_ledger = Some(serde_json::json!({"schema_version": 999, "nonsense": true}));
        assert!(matches!(rec.ledger_load(), LedgerLoad::Invalid));
    }

    #[test]
    fn a_ledger_holding_the_deleted_regressed_status_refuses_the_resume() {
        // Terminal resolution deleted `Status::Regressed`, and this is the one place that break is
        // observable: a ledger written by an older build with a regressed finding fails typed
        // deserialization and lands on `Invalid`, which refuses the resume before any model call.
        //
        // Deliberate, and no migration is owed -- but it costs more than a re-run, because the
        // ledger holds the stable ids, the immutable finding content and every disposition. A
        // refused resume means a human carries the still-open findings into a fresh session by
        // hand. Asserted rather than discovered.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let mut rec = record(&store, "s", "thread-1");
        rec.findings_ledger = Some(serde_json::json!({
            "schema_version": crate::findings::LEDGER_SCHEMA_VERSION,
            "coverage": "whole_conversation",
            "next_seq": 2,
            "findings": [{
                "id": "f1",
                "severity": "major",
                "status": "regressed",
                "title": "t",
                "detail": "d",
                "first_seen_turn": 1,
                "last_status_change_turn": 2
            }]
        }));
        assert!(matches!(rec.ledger_load(), LedgerLoad::Invalid));

        // The same ledger with a status that still exists loads, and the two fields added alongside
        // the deletion are absent rather than fatal -- so an ordinary older ledger resumes.
        rec.findings_ledger = Some(serde_json::json!({
            "schema_version": crate::findings::LEDGER_SCHEMA_VERSION,
            "coverage": "whole_conversation",
            "next_seq": 2,
            "findings": [{
                "id": "f1",
                "severity": "major",
                "status": "open",
                "title": "t",
                "detail": "d",
                "first_seen_turn": 1,
                "last_status_change_turn": 2
            }]
        }));
        match rec.ledger_load() {
            LedgerLoad::Valid(l) => {
                assert_eq!(l.findings[0].last_verified_turn, None);
                assert_eq!(l.findings[0].regression_of, None);
            }
            other => panic!("expected a valid legacy ledger, got {other:?}"),
        }
    }

    #[test]
    fn first_turn_creates_the_session() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let rec = record(&store, "default", "thread-1");
        assert_eq!(rec.turns, 1);
        assert_eq!(rec.cli_session_id, "thread-1");
        assert_eq!(store.get("default").unwrap().turns, 1);
    }

    #[test]
    fn reviewer_cwd_mode_is_recorded_and_a_legacy_record_reads_as_none() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let rec = record(&store, "default", "thread-1");
        assert_eq!(rec.reviewer_cwd_mode.as_deref(), Some("project"));
        // A record written before the field existed omits it and must deserialise as `None`,
        // which the resume-migration check treats as "project" (the only mode that existed then).
        let json = serde_json::to_string(&rec).expect("serialize");
        let legacy = json.replace(",\"reviewer_cwd_mode\":\"project\"", "");
        assert_ne!(json, legacy, "the field must have been present to remove");
        let back: SessionRecord = serde_json::from_str(&legacy).expect("legacy deserialize");
        assert_eq!(back.reviewer_cwd_mode, None);
    }

    #[test]
    fn turns_accumulate_on_the_same_reviewer_session() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "default", "thread-1");
        let rec = record(&store, "default", "thread-1");
        assert_eq!(rec.turns, 3);
        // Creation time is preserved across turns.
        assert_eq!(rec.created_unix, store.get("default").unwrap().created_unix);
    }

    #[test]
    fn a_recorded_baseline_survives_and_advances_across_turns() {
        // The cumulative baseline a turn stores is what the next turn reads back to
        // difference against. A turn that kept the mapping but never advanced this -- the
        // old id-less-resume path -- left the next turn subtracting against a stale total.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let turn_one = crate::metrics::Usage {
            input_tokens: Some(190_000),
            ..Default::default()
        };
        let turn_two = crate::metrics::Usage {
            input_tokens: Some(250_000),
            ..Default::default()
        };
        let facts = |usage| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            kind: KIND_REVIEW,
            cumulative_usage: Some(usage),
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        store
            .record_turn("default", facts(turn_one))
            .expect("turn 1");
        assert_eq!(
            store.get("default").unwrap().cumulative_usage,
            Some(turn_one)
        );
        store
            .record_turn("default", facts(turn_two))
            .expect("turn 2");
        let rec = store.get("default").unwrap();
        assert_eq!(rec.turns, 2);
        assert_eq!(
            rec.cumulative_usage,
            Some(turn_two),
            "the baseline advances to the latest running total"
        );
    }

    #[test]
    fn the_baseline_pair_advances_together_and_survives_an_incomplete_turn_intact() {
        // The next resume deltas from a (head, base) pair, so it advances as a unit: a complete
        // turn replaces it, an incomplete one (a truncated capture, an unresolved HEAD, a
        // Perforce turn -- all arriving as a `None` half) retains the prior pair rather than
        // erasing it or storing a head beside a base from a different turn.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |head: Option<&str>, base: Option<&str>| TurnFacts {
            reviewer: "claude",
            cli_session_id: "sid-1",
            model: "claude-opus-4-8",
            effort: "medium",
            cwd: "C:\\repo",
            kind: KIND_REVIEW,
            cumulative_usage: None,
            changes: None,
            head_sha: head.map(str::to_string),
            base_sha: base.map(str::to_string),
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        store
            .record_turn("g", facts(Some("aaa1"), Some("base0")))
            .expect("turn 1");
        let rec = store.get("g").unwrap();
        assert_eq!(rec.head_sha.as_deref(), Some("aaa1"));
        assert_eq!(rec.base_sha.as_deref(), Some("base0"));
        // A later complete turn advances both halves together.
        store
            .record_turn("g", facts(Some("bbb2"), Some("base0")))
            .expect("turn 2");
        let rec = store.get("g").unwrap();
        assert_eq!(rec.head_sha.as_deref(), Some("bbb2"));
        assert_eq!(rec.base_sha.as_deref(), Some("base0"));
        // An incomplete turn (here a resolved HEAD but no base -- a partial pair) must not
        // store half a baseline: the prior complete pair is retained intact.
        store
            .record_turn("g", facts(Some("ccc3"), None))
            .expect("turn 3");
        let rec = store.get("g").unwrap();
        assert_eq!(
            rec.head_sha.as_deref(),
            Some("bbb2"),
            "head retained, not ccc3"
        );
        assert_eq!(rec.base_sha.as_deref(), Some("base0"));
    }

    #[test]
    fn a_rebound_session_does_not_inherit_the_prior_conversations_baseline() {
        // A fresh review under an existing name is a different conversation. An incomplete
        // first turn there -- a truncated capture, say -- must not resume against the old
        // reviewer's baseline, which the new reviewer never saw. The prior pair is inherited
        // only within the same `cli_session_id`.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |cli: &'static str, head: Option<&str>, base: Option<&str>| TurnFacts {
            reviewer: "claude",
            cli_session_id: cli,
            model: "claude-opus-4-8",
            effort: "medium",
            cwd: "C:\\repo",
            kind: KIND_REVIEW,
            cumulative_usage: None,
            changes: None,
            head_sha: head.map(str::to_string),
            base_sha: base.map(str::to_string),
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        // The old conversation establishes a complete baseline.
        store
            .record_turn("s", facts("old", Some("h1"), Some("b1")))
            .expect("old turn");
        // Rebound to a new conversation whose first turn is incomplete (e.g. truncated).
        let rec = store
            .record_turn("s", facts("new", Some("h2"), None))
            .expect("rebound turn");
        assert_eq!(rec.turns, 1, "a rebound restarts the turn count");
        assert_eq!(rec.cli_session_id, "new");
        assert!(
            rec.head_sha.is_none() && rec.base_sha.is_none(),
            "an incomplete first turn of a new conversation must not inherit the old baseline"
        );
    }

    #[test]
    fn a_changelist_binding_persists_and_survives_a_none_turn() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |changes| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            kind: KIND_REVIEW,
            cumulative_usage: None,
            changes,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        store
            .record_turn("p4", facts(Some(vec![43650, 43651])))
            .expect("turn 1");
        assert_eq!(store.get("p4").unwrap().changes, Some(vec![43650, 43651]));
        // A later turn that carried no binding must not erase the one already stored.
        store.record_turn("p4", facts(None)).expect("turn 2");
        assert_eq!(store.get("p4").unwrap().changes, Some(vec![43650, 43651]));
    }

    #[test]
    fn the_consult_capture_contract_persists_and_survives_a_none_turn() {
        // include_change and the configured diff mode are session-life invariants like the
        // changelist binding: written every turn, but a turn that supplies `None` must inherit
        // rather than erase, so the contract the resume gate compares against stays intact.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |include_change: Option<bool>, diff_mode: Option<&str>| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            kind: KIND_CONSULT,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: Some("git"),
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change,
            diff_mode: diff_mode.map(str::to_string),
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        store
            .record_turn("c", facts(Some(true), Some("main...HEAD")))
            .expect("turn 1");
        let rec = store.get("c").unwrap();
        assert_eq!(rec.include_change, Some(true));
        assert_eq!(rec.diff_mode.as_deref(), Some("main...HEAD"));
        // A follow-up that supplies neither must not erase the bound contract.
        store.record_turn("c", facts(None, None)).expect("turn 2");
        let rec = store.get("c").unwrap();
        assert_eq!(rec.include_change, Some(true));
        assert_eq!(rec.diff_mode.as_deref(), Some("main...HEAD"));
    }

    #[test]
    fn a_tree_only_perforce_consult_persists_no_capture_state() {
        // The worker gates the Perforce capture fields on should_capture_change(), so a tree-only
        // consult hands `record_turn` `None` for `changes`/`include_shelved`/`perforce_baseline`
        // even though the backend is Perforce. The stored record must then carry no capture state, or
        // its own next tree-only resume would trip the f5 capture-state refusal in `resume_block`
        // (issue #105). A change-capturing consult, by contrast, persists them like a review.
        use crate::vcs::baseline::PerforceBaseline;
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |changes, include_shelved, baseline: Option<PerforceBaseline>| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            kind: KIND_CONSULT,
            cumulative_usage: None,
            changes,
            head_sha: None,
            base_sha: None,
            backend: Some("perforce"),
            include_shelved,
            capture_identity: None,
            perforce_baseline: baseline,
            include_change: Some(include_shelved.is_some()),
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        // Tree-only: the worker supplies no capture fields, so none are stored.
        store
            .record_turn("tree", facts(None, None, None))
            .expect("tree-only turn");
        let rec = store.get("tree").unwrap();
        assert_eq!(rec.include_change, Some(false));
        assert!(
            rec.changes.is_none(),
            "tree-only must persist no changelist set"
        );
        assert!(
            rec.include_shelved.is_none(),
            "tree-only must persist no shelved flag"
        );
        assert!(
            rec.perforce_baseline.is_none(),
            "tree-only must persist no baseline"
        );
        // Change-capturing: the binding is stored, like a review.
        store
            .record_turn(
                "cap",
                facts(
                    Some(vec![43650]),
                    Some(false),
                    Some(PerforceBaseline::Disabled),
                ),
            )
            .expect("change-capturing turn");
        let rec = store.get("cap").unwrap();
        assert_eq!(rec.include_change, Some(true));
        assert_eq!(rec.changes, Some(vec![43650]));
        assert_eq!(rec.include_shelved, Some(false));
    }

    #[test]
    fn the_perforce_baseline_is_this_turns_value_and_is_never_inherited() {
        // Unlike the (head, base) pair, the delta baseline reflects exactly what the reviewer
        // was last shown, so every turn overwrites it: a `Full` inventory on one turn must not
        // survive a later `Disabled` turn, or the next turn would elide against a stale set.
        use crate::vcs::baseline::{
            Basis, InventoryEntry, PerforceBaseline, UnitKind, INVENTORY_SCHEMA,
        };
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |baseline: Option<PerforceBaseline>| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            kind: KIND_REVIEW,
            cumulative_usage: None,
            changes: Some(vec![42]),
            head_sha: None,
            base_sha: None,
            backend: Some("perforce"),
            include_shelved: Some(false),
            capture_identity: None,
            perforce_baseline: baseline,
            include_change: None,
            diff_mode: None,
            raw_bin: RawBin::PathSearch,
            resolved_bin: String::new(),
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: "project",
            profile_identity: None,
        };
        let full = PerforceBaseline::Full {
            schema: INVENTORY_SCHEMA,
            entries: vec![InventoryEntry {
                change: 42,
                basis: Basis::Workspace,
                kind: UnitKind::TextDiff,
                depot: "//depot/a".into(),
                comparator: "3".into(),
                fingerprint: crate::digest::Fingerprint::of(b"x"),
            }],
        };
        store
            .record_turn("p4", facts(Some(full.clone())))
            .expect("turn 1");
        assert_eq!(store.get("p4").unwrap().perforce_baseline, Some(full));
        // A later incomplete turn records `Disabled` -- the prior `Full` must not linger.
        store
            .record_turn("p4", facts(Some(PerforceBaseline::Disabled)))
            .expect("turn 2");
        assert_eq!(
            store.get("p4").unwrap().perforce_baseline,
            Some(PerforceBaseline::Disabled),
            "the baseline is overwritten every turn, never inherited"
        );
        // The binding (backend, include_shelved) persists across turns.
        let rec = store.get("p4").unwrap();
        assert_eq!(rec.backend.as_deref(), Some("perforce"));
        assert_eq!(rec.include_shelved, Some(false));
    }

    #[test]
    fn a_pending_marker_survives_and_is_cleared_only_deliberately() {
        // The marker is the durable poison: set before a turn, cleared only when it is durably
        // recorded, so a crash/failure in between leaves it set for the next resume to see.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        assert_eq!(
            store.marker_state("p4"),
            MarkerState::Absent,
            "nothing pending initially"
        );
        store.mark_pending("p4").expect("mark");
        assert_eq!(
            store.marker_state("p4"),
            MarkerState::Present,
            "marked pending"
        );
        // It survives a fresh store over the same directory (i.e. an MCP server restart).
        assert_eq!(
            SessionStore::new(&dir).marker_state("p4"),
            MarkerState::Present
        );
        store.clear_pending("p4").expect("clear");
        assert_eq!(store.marker_state("p4"), MarkerState::Absent, "cleared");
        // Clearing an already-absent marker is not an error.
        store.clear_pending("p4").expect("clear absent is ok");
        // Distinct session names have distinct markers.
        store.mark_pending("a").expect("mark a");
        assert_eq!(store.marker_state("a"), MarkerState::Present);
        assert_eq!(store.marker_state("b"), MarkerState::Absent);
    }

    #[test]
    fn the_findings_marker_is_independent_of_the_perforce_pending_marker() {
        // The findings write-ahead is a *separate* sidecar from the Perforce `.pending` baseline
        // marker on purpose: a set `.pending` means "take a full re-capture and continue", while a
        // set `.findings-pending` means "refuse the resume". Sharing one file would entangle those
        // opposite recoveries, so the two must be fully independent — setting or clearing one must
        // never move the other, and each must survive a restart on its own.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        assert_eq!(store.marker_state("s"), MarkerState::Absent);
        assert_eq!(store.findings_marker_state("s"), MarkerState::Absent);

        // Setting the findings marker leaves the Perforce marker untouched.
        store.mark_findings_pending("s").expect("mark findings");
        assert_eq!(store.findings_marker_state("s"), MarkerState::Present);
        assert_eq!(
            store.marker_state("s"),
            MarkerState::Absent,
            "a findings marker must not set the Perforce pending marker"
        );

        // Setting the Perforce marker too: both are now present and distinct.
        store.mark_pending("s").expect("mark pending");
        assert_eq!(store.marker_state("s"), MarkerState::Present);
        assert_eq!(store.findings_marker_state("s"), MarkerState::Present);

        // Both survive a restart (a fresh store over the same directory).
        let restarted = SessionStore::new(&dir);
        assert_eq!(restarted.marker_state("s"), MarkerState::Present);
        assert_eq!(restarted.findings_marker_state("s"), MarkerState::Present);

        // Clearing one leaves the other set.
        store.clear_findings_pending("s").expect("clear findings");
        assert_eq!(store.findings_marker_state("s"), MarkerState::Absent);
        assert_eq!(
            store.marker_state("s"),
            MarkerState::Present,
            "clearing the findings marker must not clear the Perforce pending marker"
        );
    }

    #[test]
    fn rebinding_a_name_to_a_new_reviewer_session_restarts_the_count() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "default", "thread-1");
        // A fresh review under the same name is turn 1 again, not turn 3.
        let rec = record(&store, "default", "thread-2");
        assert_eq!(rec.turns, 1);
        assert_eq!(rec.cli_session_id, "thread-2");
    }

    #[test]
    fn sessions_are_independent() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "auth-work", "thread-2");
        record(&store, "auth-work", "thread-2");
        assert_eq!(store.get("default").unwrap().turns, 1);
        assert_eq!(store.get("auth-work").unwrap().turns, 2);
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn state_survives_a_new_store_over_the_same_directory() {
        // This is what makes a review resumable after the MCP server restarts.
        let dir = temp_dir();
        record(&SessionStore::new(&dir), "default", "thread-1");
        let reopened = SessionStore::new(&dir);
        let rec = reopened.get("default").expect("session persisted");
        assert_eq!(rec.cli_session_id, "thread-1");
        assert_eq!(rec.turns, 1);
    }

    #[test]
    fn forget_removes_only_the_named_session() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "other", "thread-2");
        assert!(store.forget("default").expect("forget"));
        assert!(store.get("default").is_none());
        assert!(store.get("other").is_some());
        // Forgetting something absent is not an error.
        assert!(!store.forget("default").expect("forget again"));
    }

    #[test]
    fn a_corrupt_store_reads_empty_but_is_never_overwritten() {
        // Read accessors stay best-effort empty (the review path is gated on `store_state` before
        // anything is spawned), but a mutator must NOT overwrite a corrupt store: that would destroy
        // the evidence and could create a convergeable session over history it cannot see. Recovery
        // is an explicit operator action, so `record_turn` refuses instead.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        std::fs::write(store.path(), "{ this is not json").expect("write garbage");
        assert_eq!(store.store_state(), StoreState::Corrupt);
        assert!(store.get("default").is_none());
        // record_turn refuses rather than clobbering the corrupt file.
        let err = store
            .record_turn(
                "default",
                TurnFacts {
                    reviewer: "codex",
                    cli_session_id: "thread-1",
                    model: "m",
                    effort: "max",
                    cwd: "C:\\repo",
                    kind: KIND_REVIEW,
                    cumulative_usage: None,
                    changes: None,
                    head_sha: None,
                    base_sha: None,
                    backend: None,
                    include_shelved: None,
                    capture_identity: None,
                    perforce_baseline: None,
                    include_change: None,
                    diff_mode: None,
                    raw_bin: RawBin::PathSearch,
                    resolved_bin: String::new(),
                    findings_ledger: None,
                    terminal_reason: None,
                    reviewer_cwd_mode: "project",
                    profile_identity: None,
                },
            )
            .expect_err("record_turn must refuse a corrupt store");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // The corrupt file is left untouched for recovery.
        assert_eq!(store.store_state(), StoreState::Corrupt);
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        // The guard has to outlive the path derived from it: bound to a temporary, it
        // would drop at the end of this statement and take the directory with it.
        let root = temp_dir();
        let dir = root.join("nested").join("deeper");
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        assert!(store.path().is_file());
    }

    #[test]
    fn an_exclusive_lock_excludes_a_second_holder() {
        let dir = temp_dir();
        let path = dir.join("thing.lock");

        let held = ExclusiveLock::acquire(&path, Duration::from_millis(50)).expect("first acquire");
        // Windows enforces this through the byte-range lock, so the second exclusive acquire fails
        // while the first handle is alive. No staleness heuristic is involved.
        let blocked = ExclusiveLock::acquire(&path, Duration::from_millis(100));
        assert!(blocked.is_err(), "a second holder must be refused");

        drop(held);
        // Releasing the handle releases the lock; the file itself is never deleted, so
        // there is no window where one process removes another's lock.
        ExclusiveLock::acquire(&path, Duration::from_millis(500)).expect("acquire after release");
        assert!(path.exists());
    }

    #[test]
    fn shared_locks_coexist_but_exclude_the_exclusive_side() {
        // [f5]/f9: many shared readers coexist on one path, while an exclusive holder blocks (and is
        // blocked by) any shared holder. This is the review-vs-setup coupling that keeps a swap from
        // renaming a home out from under a live review.
        let dir = temp_dir();
        let path = dir.join("home.lock");

        let r1 = SharedLock::acquire(&path, Duration::from_millis(50)).expect("first shared");
        let r2 =
            SharedLock::acquire(&path, Duration::from_millis(50)).expect("second shared coexists");

        // Setup's exclusive acquire is blocked while any shared reader is held.
        assert!(
            ExclusiveLock::acquire(&path, Duration::from_millis(100)).is_err(),
            "an exclusive lock must be blocked while shared readers are held"
        );

        drop(r1);
        drop(r2);
        // With no shared readers, setup takes the exclusive side.
        let ex = ExclusiveLock::acquire(&path, Duration::from_millis(500))
            .expect("exclusive after readers");
        // And while setup holds it exclusively, a review's shared acquire is blocked.
        assert!(
            SharedLock::acquire(&path, Duration::from_millis(100)).is_err(),
            "a shared lock must be blocked while the exclusive side is held"
        );
        drop(ex);
        SharedLock::acquire(&path, Duration::from_millis(500))
            .expect("shared after exclusive release");
    }

    #[test]
    fn recording_a_turn_holds_and_releases_the_lock() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        // If the lock had leaked, this second write would time out.
        record(&store, "default", "thread-1");
        assert_eq!(store.get("default").unwrap().turns, 2);
    }

    #[test]
    fn a_held_session_lock_blocks_a_second_claim() {
        let dir = temp_dir();
        let path = session_lock_path(&dir, "auth-work");
        let _held = ExclusiveLock::acquire(&path, Duration::from_millis(50)).expect("acquire");
        assert!(ExclusiveLock::acquire(&path, Duration::from_millis(50)).is_err());
    }

    #[test]
    fn session_lock_paths_are_distinct_and_filename_safe() {
        let dir = temp_dir();
        let a = session_lock_path(&dir, "auth-work");
        let b = session_lock_path(&dir, "other");
        assert_ne!(a, b);

        // Names come from the calling agent, so path separators must not escape.
        let nasty = session_lock_path(&dir, "../../etc/passwd");
        let name = nasty.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains('\\'), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert_eq!(nasty.parent(), Some(dir.as_path()));
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        // Enumerate the directory rather than probing one guessed name. The previous
        // version asserted `sessions.json.tmp` did not exist, but temp files are named
        // `sessions.<pid>.<seq>.tmp`, so it was asserting the absence of a path the code
        // never creates -- it would have passed while every temp file leaked.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read state dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write should rename, not leave temp files: {leftovers:?}"
        );
    }
}
