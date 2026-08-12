//! Cross-process serialization and crash-recovery for reviewer-profile setup.
//!
//! Setting up a profile (authorizing it, provisioning its home, or re-logging it in) mutates
//! per-machine state that concurrent setups — in one server or across separate server processes —
//! must not race on. So each setup runs inside a [`SetupSession`] holding an **OS-level, cross-process
//! exclusive lock keyed by the effective home** ([`crate::session::ExclusiveLock`], released even if
//! the process dies), and records a small **provisional marker** that a setup is (or was) in progress.
//! If a prior setup died mid-flight, the next one to take the lock finds and clears that marker.
//!
//! **Scope note:** authorize-only setup creates nothing, so this session only needs the lock and the
//! in-progress marker. The **ownership-scoped rollback of created directories** the plan requires
//! (`[f2]`/`[f23]`) is deferred to the provisioning / re-login part of #15 and will be built together
//! with the directory-creation mechanism it must be entangled with — recording ownership *before*
//! creating, refusing a pre-existing directory, and retaining the marker until cleanup actually
//! succeeds — rather than as a standalone record-a-path API that could be pointed at any directory.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::{ApprovalDetails, ApprovalOutcome, ApprovalRow, ApprovalServer};
use crate::cancel::RequestCancel;
use crate::config::{Config, ReviewerKind, ReviewerSpec, UsageMinimum};
use crate::errors::{self, Failure};
use crate::profile::ProfileSelector;
use crate::reviewer::AuthMethod;
use crate::session::{now_unix, ExclusiveLock};

/// How long the approval page waits for the human before giving up. The MCP call blocks for this
/// long, well within the client tool timeout.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long to wait for another process's setup of the same profile before giving up.
const SETUP_LOCK_WAIT: Duration = Duration::from_secs(10);

/// How long a provisional marker is considered live. The exclusive lock is the primary liveness
/// signal (it releases when its holder dies), so this is a secondary bound: a marker older than this
/// is treated as abandoned and reclaimed even if the lock discipline were somehow bypassed.
const MARKER_TTL_SECS: u64 = 30 * 60;

/// The operation a setup journal records. `AuthorizeOnly` creates nothing, so it needs no recovery
/// beyond clearing the marker; the two provisioning operations are recovered by the presence-driven
/// state machine in [`recover`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Operation {
    #[default]
    AuthorizeOnly,
    FirstProvision,
    Relogin,
}

/// The ownership-nonce marker file written inside a nonce-named staging dir at creation and carried
/// into `home` by the rename. Recovery only ever deletes/quarantines a directory whose marker matches
/// the journal nonce, so it can never touch a home this run did not create (f13/f-a2).
const OWNED_MARKER: &str = ".cross-review-owned";

/// The write-ahead journal beside the setup lock: the in-progress marker extended with everything
/// recovery needs to reach a consistent state after a crash at any point (f2/f8/f-a3/f-b1..b5). Written
/// **before** each filesystem mutation. Older/authorize-only markers deserialize via the field
/// defaults (operation `AuthorizeOnly`, empty paths), so they are simply cleared.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Journal {
    holder_pid: u32,
    /// A per-run random nonce: the ownership proof for staging/rejected dirs, and a guard against a
    /// recycled pid being mistaken for the same run.
    holder_nonce: String,
    /// Unix seconds after which this marker is considered abandoned (secondary to the lock).
    expiry_unix: u64,
    #[serde(default)]
    operation: Operation,
    /// The final home path this setup targets.
    #[serde(default)]
    home: String,
    /// The nonce-named staging dir this run exclusively created (once created).
    #[serde(default)]
    staging_path: Option<String>,
    /// The nonce-named `.old` the existing home was moved aside to (re-login).
    #[serde(default)]
    old_path: Option<String>,
    /// The nonce-named quarantine path an uncommitted home was moved to.
    #[serde(default)]
    rejected_path: Option<String>,
    /// The `(volume, file-index)` identity of the home captured before `home → .old`, so recovery can
    /// prove `.old` is the object it moved aside, not a replacement (f-a3).
    #[serde(default)]
    old_file_id: Option<(u32, u64)>,
    /// The exact allowlist entry this run commits, so recovery can certify whether the commit landed
    /// (f-b4).
    #[serde(default)]
    expected_entry: Option<crate::allowlist::AllowEntry>,
    /// Advisory phase hint; recovery is driven by path presence, not this.
    #[serde(default)]
    phase: String,
}

/// What recovery decided about the found journal: clear it (a consistent state was reached), or retain
/// it because the state is ambiguous / unprovable and needs a human — in which case [`begin`] refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    Clear,
    Retain,
}

/// A live setup: holds the per-home exclusive lock and owns the journal/marker for its lifetime.
/// Dropping it without [`commit`](Self::commit) clears the marker; the lock releases with it.
pub struct SetupSession {
    _lock: ExclusiveLock,
    base: PathBuf,
    marker_path: PathBuf,
    holder_nonce: String,
    committed: bool,
}

impl SetupSession {
    /// Acquire the exclusive setup lock for `effective_home` under `base`, **run recovery** on any
    /// journal a crashed prior run left behind, then write a fresh in-progress marker.
    ///
    /// Recovery runs here, **before** the caller classifies the operation, so an interrupted swap
    /// (which can leave `home` absent) is repaired before first-provision/re-login is chosen (f-r2.4).
    /// Fails with [`io::ErrorKind::WouldBlock`] if another live setup holds the lock, or with a plain
    /// error if a prior run left the profile in a state needing manual review (recovery retained it).
    pub fn begin(base: &Path, effective_home: &Path) -> io::Result<Self> {
        Self::begin_with_wait(base, effective_home, SETUP_LOCK_WAIT)
    }

    fn begin_with_wait(base: &Path, effective_home: &Path, wait: Duration) -> io::Result<Self> {
        // The lock and journal live in the store's secured `auth` directory, so another user cannot
        // create or tamper with them. Ensure it exists and is locked down first.
        std::fs::create_dir_all(base)?;
        let auth = base.join("auth");
        let _dir = crate::winsec::create_secured_dir(&auth)?;

        let key = home_key(effective_home);
        let lock_path = auth.join(format!("setup-{key}.lock"));
        let marker_path = auth.join(format!("setup-{key}.marker"));

        // Acquire the exclusive lock. Holding it means no other *live* setup of this home is running.
        let lock = ExclusiveLock::acquire(&lock_path, wait)?;

        // Any journal present is from a prior run that died or exited without clearing it (a live run
        // would still hold the lock we just took). Replay it to a consistent state before starting.
        if let Some(journal) = read_journal(&marker_path) {
            match recover(base, &marker_path, &journal)? {
                Recovery::Clear => {
                    let _ = remove_if_present(&marker_path);
                }
                Recovery::Retain => {
                    return Err(io::Error::other(format!(
                        "a previous setup of {} left it in a state needing manual review; inspect \
                         the profile directory and any '.rejected-*' sibling, then remove {} to \
                         retry",
                        effective_home.display(),
                        marker_path.display(),
                    )));
                }
            }
        }

        let holder_nonce = crate::digest::random_hex_token(16)
            .ok_or_else(|| io::Error::other("could not generate a setup nonce"))?;
        let session = Self {
            _lock: lock,
            base: base.to_path_buf(),
            marker_path,
            holder_nonce,
            committed: false,
        };
        // The initial in-progress journal: an authorize-only placeholder (creates nothing). The
        // provisioning flow overwrites it with FirstProvision/Relogin journals as it advances.
        session.write_journal(&session.blank_journal(effective_home, Operation::AuthorizeOnly))?;
        Ok(session)
    }

    /// The per-run nonce (the ownership proof written into staging dirs).
    #[allow(dead_code)] // consumed by the provisioning flow (next slice of #15 part 3b).
    fn nonce(&self) -> &str {
        &self.holder_nonce
    }

    fn blank_journal(&self, home: &Path, operation: Operation) -> Journal {
        Journal {
            holder_pid: std::process::id(),
            holder_nonce: self.holder_nonce.clone(),
            expiry_unix: now_unix().saturating_add(MARKER_TTL_SECS),
            operation,
            home: home.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    /// Write the journal (write-ahead): atomically replace the marker file and flush it plus its
    /// directory to stable storage, so the recorded intent is durable before the mutation it guards
    /// (f-b3). Called before each filesystem step of the provisioning flow.
    fn write_journal(&self, journal: &Journal) -> io::Result<()> {
        write_journal_at(&self.marker_path, journal)
    }

    /// Commit the setup: the run succeeded, so the marker is removed. After this, dropping the session
    /// does nothing.
    pub fn commit(mut self) -> io::Result<()> {
        remove_if_present(&self.marker_path)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for SetupSession {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Uncommitted: clear the in-progress marker (best-effort). The lock releases as `_lock` drops.
        // A crash (rather than a clean drop) leaves the journal for the next `begin` to recover.
        let _ = remove_if_present(&self.marker_path);
    }
}

fn read_journal(marker_path: &Path) -> Option<Journal> {
    let bytes = std::fs::read(marker_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_journal_at(marker_path: &Path, journal: &Journal) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(journal)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    atomic_write(marker_path, &json)?;
    // Durability: flush the journal file and its directory so the intent survives a power loss (f-b3).
    let _ = crate::winsec::flush_file(marker_path);
    if let Some(dir) = marker_path.parent() {
        let _ = crate::winsec::flush_dir(dir);
    }
    Ok(())
}

fn write_owned_marker(dir: &Path, nonce: &str) -> io::Result<()> {
    std::fs::write(dir.join(OWNED_MARKER), nonce.as_bytes())
}

fn owned_marker_matches(dir: &Path, nonce: &str) -> bool {
    std::fs::read(dir.join(OWNED_MARKER))
        .map(|b| b == nonce.as_bytes())
        .unwrap_or(false)
}

/// Remove a directory tree and flush its parent so the unlink is durable (f-b5). Best-effort flush.
fn remove_dir_all_durable(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        let _ = crate::winsec::flush_dir(parent);
    }
    Ok(())
}

/// Restore `.old` back onto `home`, but only after proving `.old` is the object we moved aside (its
/// recorded `(volume, file-index)` identity, opened no-follow — f-a3). Returns `false` (fail closed,
/// leaving everything untouched) on a missing/mismatched id or a reparse point.
fn restore_old(old: &Path, home: &Path, journal: &Journal) -> io::Result<bool> {
    match journal.old_file_id {
        Some(expected) => match crate::winsec::dir_identity_no_follow(old) {
            Ok(id) if id == expected => {}
            _ => return Ok(false),
        },
        None => return Ok(false),
    }
    // No-replace: if `home` somehow already exists, do not clobber it — fail closed.
    match crate::winsec::rename_no_replace(old, home) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    }
    if let Some(parent) = home.parent() {
        let _ = crate::winsec::flush_dir(parent);
    }
    Ok(true)
}

/// Remove a staging dir only if it carries this run's ownership marker (f13). `false` = not provably
/// ours, left untouched.
fn remove_owned_staging(staging: &Path, journal: &Journal) -> io::Result<bool> {
    if !owned_marker_matches(staging, &journal.holder_nonce) {
        return Ok(false);
    }
    remove_dir_all_durable(staging)?;
    Ok(true)
}

/// Quarantine an uncommitted `home` this run created: WAL the rejected path, then rename it aside
/// (no-replace) so it is never deleted, only set aside for the human (f17). Only touches a
/// marker-verified home. `false` = not provably ours.
fn quarantine_home(marker_path: &Path, home: &Path, journal: &mut Journal) -> io::Result<bool> {
    if !owned_marker_matches(home, &journal.holder_nonce) {
        return Ok(false);
    }
    let rejected = sibling_with_suffix(home, &format!("rejected-{}", journal.holder_nonce));
    // Write-ahead: record the rejected path before the rename, so a mid-quarantine crash is
    // recoverable rather than stranding an untracked dir (f17).
    journal.rejected_path = Some(rejected.to_string_lossy().into_owned());
    write_journal_at(marker_path, journal)?;
    match crate::winsec::rename_no_replace(home, &rejected) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    }
    if let Some(parent) = home.parent() {
        let _ = crate::winsec::flush_dir(parent);
    }
    Ok(true)
}

/// Whether this run's exact authorization is durably present in the store (f-b4): the certification
/// that a re-login/first-provision commit actually landed before a crash.
fn commit_certified(base: &Path, journal: &Journal) -> io::Result<bool> {
    match &journal.expected_entry {
        Some(entry) => crate::allowlist::AllowlistStore::at(base).is_authorized(entry),
        None => Ok(false),
    }
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match path.parent() {
        Some(parent) => parent.join(format!("{name}.{suffix}")),
        None => PathBuf::from(format!("{name}.{suffix}")),
    }
}

/// Replay a crashed run's journal to a consistent state, driven by the **actual presence** of the
/// journalled nonce-named paths (the `phase` field is only advisory). Runs under the per-home exclusive
/// lock. See `docs/reviewer-account-profiles-3b-plan.md` "Crash-safety & recovery" for the full table.
fn recover(base: &Path, marker_path: &Path, journal: &Journal) -> io::Result<Recovery> {
    if journal.operation == Operation::AuthorizeOnly {
        return Ok(Recovery::Clear); // created nothing
    }
    let mut journal = journal.clone();
    let home = PathBuf::from(&journal.home);
    let staging = journal.staging_path.clone().map(PathBuf::from);
    let old = journal.old_path.clone().map(PathBuf::from);
    let rejected = journal.rejected_path.clone().map(PathBuf::from);

    // Phase 0: a journal-owned rejected dir is handled first (f20). Complete any pending `.old → home`
    // restore, then keep the rejected dir (marker-tagged) and retain the journal for the human.
    if let Some(rej) = rejected {
        if rej.exists() {
            if !owned_marker_matches(&rej, &journal.holder_nonce) {
                return Ok(Recovery::Retain); // not provably ours -> fail closed
            }
            if let Some(old) = &old {
                if old.exists() && !home.exists() && !restore_old(old, &home, &journal)? {
                    return Ok(Recovery::Retain);
                }
            }
            return Ok(Recovery::Retain);
        }
        // named but absent: already resolved; fall through.
    }

    let h = home.exists();
    let o = old.as_ref().map(|p| p.exists()).unwrap_or(false);
    let s = staging.as_ref().map(|p| p.exists()).unwrap_or(false);
    let relogin = journal.operation == Operation::Relogin;

    let outcome = match (h, o, s) {
        // Staging created, not swapped in: remove our staging, home untouched.
        (_, false, true) => {
            let staging = staging.as_ref().expect("s implies staging path");
            if !remove_owned_staging(staging, &journal)? {
                return Ok(Recovery::Retain);
            }
            if !h && relogin {
                return Ok(Recovery::Retain); // re-login home lost with nothing to restore
            }
            Recovery::Clear
        }
        // Crashed between home->.old and staging->home: restore .old, remove staging.
        (false, true, true) => {
            let old = old.as_ref().unwrap();
            let staging = staging.as_ref().unwrap();
            if !restore_old(old, &home, &journal)? {
                return Ok(Recovery::Retain);
            }
            let _ = remove_owned_staging(staging, &journal)?;
            Recovery::Clear
        }
        // Home moved to .old, staging gone (failed cleanup): restore .old.
        (false, true, false) => {
            let old = old.as_ref().unwrap();
            if !restore_old(old, &home, &journal)? {
                return Ok(Recovery::Retain);
            }
            Recovery::Clear
        }
        // Re-login crashed after staging->home, before/at authorize: consult the store.
        (true, true, false) => {
            let old = old.as_ref().unwrap();
            if commit_certified(base, &journal)? {
                remove_dir_all_durable(old)?; // committed: drop the old, keep the new home
                Recovery::Clear
            } else {
                // Not committed: roll back. Quarantine the uncommitted new home, restore .old.
                if !quarantine_home(marker_path, &home, &mut journal)? {
                    return Ok(Recovery::Retain);
                }
                if !restore_old(old, &home, &journal)? {
                    return Ok(Recovery::Retain);
                }
                Recovery::Retain // a rejected dir remains for the human
            }
        }
        // Complete, or an uncommitted first-provision/re-login home, or nothing of ours.
        (true, false, false) => {
            if commit_certified(base, &journal)? {
                Recovery::Clear // committed: keep the authorized home
            } else if owned_marker_matches(&home, &journal.holder_nonce) {
                // Our uncommitted home (first-provision, or re-login after .old was already dropped):
                // quarantine it. No .old to restore.
                if !quarantine_home(marker_path, &home, &mut journal)? {
                    return Ok(Recovery::Retain);
                }
                Recovery::Retain
            } else {
                Recovery::Clear // home is not ours and not authorized by us
            }
        }
        // Home present + our staging, no .old: remove staging, keep home.
        (true, false, true) => {
            let staging = staging.as_ref().unwrap();
            let _ = remove_owned_staging(staging, &journal)?;
            Recovery::Clear
        }
        // All absent.
        (false, false, false) => {
            if relogin {
                Recovery::Retain // home lost, nothing to restore
            } else {
                Recovery::Clear // first-provision never got anywhere
            }
        }
        // Our .old + our staging + a present home: an external re-creation / impossible interleave.
        (true, true, true) => Recovery::Retain,
    };
    Ok(outcome)
}

/// Run the profile-setup tool. Currently implements the **authorize-only** operation: authorize this
/// repository (its immutable launch root) to run reviews under an **already-provisioned, logged-in**
/// profile. It never touches the credential home or the login — its only prerequisites are a
/// read-only identity probe and explicit human approval, after which it commits the allowlist entry.
///
/// Ordered state machine (`[f18]`): parse + validate → probe (reject a non-subscription home before
/// bothering the human) → **human approval** on the loopback page → re-confirm the account is
/// unchanged → **commit** the allowlist entry. All of it runs under the per-home [`SetupSession`] lock,
/// so concurrent setups of the same profile serialize. First-provision and staged re-login (which need
/// a vendor login) are a later part of #15; until then a home that does not yet exist is refused with
/// guidance rather than provisioned.
pub fn run_setup(cfg: &Config, args: &Value, request: &RequestCancel) -> Result<String, Failure> {
    let reviewer = parse_reviewer(args)?;
    let selector = parse_selector(args, reviewer)?;

    let base = crate::profile::profile_base().ok_or_else(|| {
        errors::bad_request(
            "No profile base is configured: set CROSS_REVIEW_HOME (or ensure LOCALAPPDATA is set) so \
             there is a fixed, protected location to record the authorization.",
        )
    })?;
    let home = crate::profile::resolve_home(&selector, reviewer, Some(&base))
        .map_err(errors::bad_request)?
        .expect("a non-ambient selector resolves to a home");

    // Authorize-only requires an existing, logged-in home. Provisioning (vendor login) is a later part.
    if !home.is_dir() {
        return Err(setup_failure(format!(
            "The profile home {} does not exist yet. Authorize-only setup can only authorize an \
             already-provisioned, signed-in profile; provisioning a new home (running the vendor \
             login) is not available yet. Sign the profile in first (point the vendor CLI's config \
             home at {}), then run setup again.",
            home.display(),
            home.display(),
        )));
    }

    // Serialize with any other setup of this same profile, and clean up after a crashed prior run.
    let session = SetupSession::begin(&base, &home).map_err(|e| {
        if e.kind() == io::ErrorKind::WouldBlock {
            setup_failure(
                "Another setup for this profile is already in progress. Finish or cancel it, then \
                 try again.",
            )
        } else {
            setup_failure(format!("Could not start setup: {e}"))
        }
    })?;

    let spec = ReviewerSpec {
        reviewer,
        model: reviewer.default_model().to_string(),
        effort: reviewer.default_effort().to_string(),
        bin: None,
        usage_minimum: UsageMinimum::None,
        profile: selector.clone(),
    };
    let bin = crate::reviewer::resolve_bin(&spec)?;
    let adapter = crate::reviewer::for_kind(reviewer);

    // Probe the home's identity before involving the human, so a non-subscription or unreadable home
    // is refused up front. Runs read-only under the controlled environment (no login).
    let identity = adapter.resolve_home_identity(&bin, cfg, &home, request.cancel_flag())?;
    if identity.method != AuthMethod::Subscription {
        return Err(setup_failure(
            "The profile home is not signed in with a subscription account (it looks like an API key \
             or an unrecognised method). Only a subscription sign-in can back a reviewer profile.",
        ));
    }

    // Human approval on the loopback page. The account being authorized is shown so the human can see
    // exactly what they are granting.
    let details = ApprovalDetails {
        title: "Authorize a reviewer profile for this repository".to_string(),
        rows: vec![
            row(
                "Operation",
                "Authorize this repository to use an existing profile",
            ),
            row("Reviewer", reviewer.as_str()),
            row("Profile", &selector.label()),
            row("Profile home", &home.to_string_lossy()),
            row("Launch root", &cfg.launch_root.to_string_lossy()),
            row("Account", &identity.account),
        ],
        caution: Some(
            "Approving lets any review launched from this repository run under this profile's \
             account. Only approve if you started this setup."
                .to_string(),
        ),
    };
    let server = ApprovalServer::start(&details, APPROVAL_TIMEOUT)
        .map_err(|e| setup_failure(format!("Could not start the local approval page: {e}")))?;
    let _ = crate::approval::open_in_browser(server.url());
    eprintln!(
        "cross-review: open this URL in a browser and click Approve to authorize the profile:\n  {}",
        server.url()
    );

    // Wait for the human, honouring a cancelled tool call.
    let outcome = loop {
        if let Some(outcome) = server.poll() {
            break outcome;
        }
        if request.is_cancelled() {
            server.cancel();
            break ApprovalOutcome::Cancelled;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    match outcome {
        ApprovalOutcome::Approved => {}
        ApprovalOutcome::Cancelled => return Err(errors::cancelled()),
        ApprovalOutcome::TimedOut => return Err(setup_failure(
            "The approval page timed out with no response, so nothing was authorized. Run setup \
                 again and click Approve.",
        )),
    }

    // Re-confirm the account did not change while the human was deciding: they approved a *specific*
    // account, so a mid-approval re-login must not silently authorize a different one.
    let confirmed = adapter.resolve_home_identity(&bin, cfg, &home, request.cancel_flag())?;
    if confirmed != identity {
        return Err(setup_failure(
            "The profile home's account or sign-in method changed during approval, so nothing was \
             authorized. Run setup again.",
        ));
    }

    // A tool call cancelled between approval and the commit must not silently persist an
    // authorization the caller has walked away from ([f1]). An early check avoids even waiting on the
    // store lock for an already-cancelled call; the *authoritative* check is inside `authorize`, at the
    // atomic rename boundary (passing the cancel flag), so a cancellation during the lock wait or the
    // ACL I/O still aborts before anything is published.
    if request.is_cancelled() {
        return Err(errors::cancelled());
    }

    // Commit the allowlist entry, then clear the setup marker.
    let store = crate::allowlist::AllowlistStore::at(&base);
    let entry = crate::allowlist::AllowEntry {
        launch_root: cfg.launch_root.to_string_lossy().into_owned(),
        effective_home: home.to_string_lossy().into_owned(),
        reviewer_family: reviewer.as_str().to_string(),
        account_fingerprint: confirmed.account.clone(),
    };
    let newly = store
        .authorize(entry, Some(request.cancel_flag()))
        .map_err(|e| {
            if e.kind() == io::ErrorKind::Interrupted {
                errors::cancelled()
            } else {
                setup_failure(format!("Could not record the authorization: {e}"))
            }
        })?;
    session.commit().map_err(|e| {
        setup_failure(format!(
            "Authorized, but the setup marker could not be cleared: {e}"
        ))
    })?;

    Ok(format!(
        "{} to use the {} profile {} (account {}) for reviews launched from {}.",
        if newly {
            "Authorized this repository"
        } else {
            "This repository was already authorized"
        },
        reviewer.as_str(),
        selector.label(),
        confirmed.account,
        cfg.launch_root.display(),
    ))
}

fn row(label: &str, value: &str) -> ApprovalRow {
    ApprovalRow {
        label: label.to_string(),
        value: value.to_string(),
    }
}

/// A setup failure carries the stop-and-tell-the-user contract, like the review failures.
fn setup_failure(message: impl Into<String>) -> Failure {
    let message = message.into();
    Failure::new("PROFILE_SETUP_FAILED", message.clone(), message)
}

/// A string-typed optional argument, distinguishing **absent** (`Ok(None)`) from **present but
/// wrong-typed** (`Err`). A wrong-typed field must be a hard error, not silently treated as absent —
/// otherwise e.g. a numeric `profile` beside a valid `home` would be accepted as a home-only request,
/// contrary to the schema ([f6]).
fn string_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, Failure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(errors::bad_request(format!(
            "The \"{key}\" argument must be a string."
        ))),
    }
}

fn parse_reviewer(args: &Value) -> Result<ReviewerKind, Failure> {
    match string_arg(args, "reviewer")? {
        Some("codex") => Ok(ReviewerKind::Codex),
        Some("claude") => Ok(ReviewerKind::Claude),
        _ => Err(errors::bad_request(
            "Setup requires \"reviewer\" to be \"codex\" or \"claude\".",
        )),
    }
}

fn parse_selector(args: &Value, _reviewer: ReviewerKind) -> Result<ProfileSelector, Failure> {
    let name = string_arg(args, "profile")?;
    let home = string_arg(args, "home")?;
    match (name, home) {
        (Some(_), Some(_)) => Err(errors::bad_request(
            "Pass either \"profile\" (a named profile) or \"home\" (an explicit config-home path), not \
             both.",
        )),
        (Some(name), None) => {
            crate::profile::validate_profile_name(name).map_err(errors::bad_request)?;
            Ok(ProfileSelector::Named(name.to_string()))
        }
        (None, Some(home)) => {
            let path = PathBuf::from(home);
            if !path.is_absolute() {
                return Err(errors::bad_request("\"home\" must be an absolute path."));
            }
            Ok(ProfileSelector::ExplicitHome(path))
        }
        (None, None) => Err(errors::bad_request(
            "Setup requires \"profile\" (a named profile) or \"home\" (an explicit config-home path).",
        )),
    }
}

/// A stable key for a home path, so the lock/marker/journal file names are the same for the *same
/// physical directory* regardless of spelling **and regardless of whether the leaf exists yet**.
///
/// Canonicalizing the full path only works once it exists; during the `home → .old` swap window, and
/// on a fresh first-provision where even `profiles/{reviewer}` is not created yet, it does not. So the
/// key is built from the **longest existing ancestor** (canonicalized, resolving `.`/`..`, symlinks,
/// and 8.3/short-name and case aliases) plus the remaining components **lower-cased** — Windows
/// directories are case-insensitive, so `…\Work` and `…\work` are one directory and must map to one
/// key (f-a1/f-c2). Two equivalent spellings therefore collapse to one lock/journal, so they cannot run
/// concurrent setups of one home or bypass each other's recovery ([f5]). Collision-resistant
/// (SHA-256), truncated for a tidy file name.
fn home_key(effective_home: &Path) -> String {
    // Walk up to the longest existing ancestor, collecting the non-existent tail (leaf-first).
    let mut existing: &Path = effective_home;
    let mut remainder: Vec<String> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                remainder.push(name.to_string_lossy().to_lowercase());
                existing = parent;
            }
            // No parent (a root/prefix) or no file name: stop here and use what we have.
            _ => break,
        }
    }
    let base = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    let mut normalized = base.to_string_lossy().to_lowercase().replace('/', "\\");
    while normalized.ends_with('\\') {
        normalized.pop();
    }
    for comp in remainder.iter().rev() {
        normalized.push('\\');
        normalized.push_str(comp);
    }
    match crate::digest::Fingerprint::of(normalized.as_bytes()) {
        Some(fp) => fp.sha256[..24].to_string(),
        // The digest is only unavailable if the OS CNG call fails, which is essentially never; fall
        // back to a length-tagged sanitised form so two different homes still differ.
        None => format!(
            "x{}-{}",
            normalized.len(),
            normalized
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(16)
                .collect::<String>()
        ),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn temp_dir() -> TempDir {
        crate::testutil::temp_dir("cross-review-setup-tests")
    }

    #[test]
    fn home_key_is_stable_across_case_and_a_missing_leaf() {
        let base = temp_dir();
        // The reviewer/profiles ancestor exists; the leaf does not (a fresh first-provision).
        let ancestor = base.join("profiles").join("codex");
        std::fs::create_dir_all(&ancestor).expect("mkdir ancestor");
        // Two case spellings of the same not-yet-created leaf map to one key (f-c2)...
        let upper = ancestor.join("Work");
        let lower = ancestor.join("work");
        assert_eq!(
            home_key(&upper),
            home_key(&lower),
            "case-only spellings of one home must share a key"
        );
        // ...and even with NO existing parent at all, a stable key is produced (f-a1) and distinct
        // homes still differ.
        let missing_a = base.join("nope").join("a");
        let missing_b = base.join("nope").join("b");
        assert_ne!(home_key(&missing_a), home_key(&missing_b));
        // A different profile name is a different key.
        assert_ne!(home_key(&lower), home_key(&ancestor.join("personal")));
    }

    // --- recovery state machine -------------------------------------------------------------------

    /// Build a provisioning journal with the given nonce and paths, capturing `old`'s object identity
    /// when present (as the real flow does before moving a home aside).
    fn journal(
        op: Operation,
        home: &Path,
        staging: Option<&Path>,
        old: Option<&Path>,
        nonce: &str,
    ) -> Journal {
        Journal {
            holder_pid: 1,
            holder_nonce: nonce.to_string(),
            expiry_unix: 0,
            operation: op,
            home: home.to_string_lossy().into_owned(),
            staging_path: staging.map(|p| p.to_string_lossy().into_owned()),
            old_path: old.map(|p| p.to_string_lossy().into_owned()),
            rejected_path: None,
            old_file_id: old.and_then(|p| crate::winsec::dir_identity_no_follow(p).ok()),
            expected_entry: None,
            phase: String::new(),
        }
    }

    fn owned_dir(path: &Path, nonce: &str) {
        std::fs::create_dir_all(path).unwrap();
        write_owned_marker(path, nonce).unwrap();
    }

    #[test]
    fn recovery_authorize_only_clears() {
        let base = temp_dir();
        let mp = base.join("m.marker");
        let j = journal(
            Operation::AuthorizeOnly,
            &base.join("home"),
            None,
            None,
            "n1",
        );
        assert_eq!(recover(&base, &mp, &j).unwrap(), Recovery::Clear);
    }

    #[test]
    fn recovery_first_provision_removes_orphan_staging() {
        // H=0,O=0,S=1: staging created, never swapped in -> removed; nothing else touched.
        let base = temp_dir();
        let home = base.join("home");
        let staging = base.join("home.staging-n1");
        owned_dir(&staging, "n1");
        let j = journal(Operation::FirstProvision, &home, Some(&staging), None, "n1");
        assert_eq!(
            recover(&base, &base.join("m.marker"), &j).unwrap(),
            Recovery::Clear
        );
        assert!(!staging.exists(), "orphan staging removed");
        assert!(!home.exists(), "home was never created");
    }

    #[test]
    fn recovery_relogin_restores_old_when_swap_interrupted() {
        // H=0,O=1,S=1: crashed between home->.old and staging->home -> restore .old, drop staging.
        let base = temp_dir();
        let home = base.join("home");
        let old = base.join("home.old-n2");
        let staging = base.join("home.staging-n2");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("auth.json"), b"OLD").unwrap();
        owned_dir(&staging, "n2");
        let j = journal(Operation::Relogin, &home, Some(&staging), Some(&old), "n2");
        assert_eq!(
            recover(&base, &base.join("m.marker"), &j).unwrap(),
            Recovery::Clear
        );
        assert!(
            home.exists() && !old.exists(),
            "the valid old home was restored"
        );
        assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), b"OLD");
        assert!(!staging.exists(), "staging dropped");
    }

    #[test]
    fn recovery_relogin_rolls_back_an_uncommitted_swap() {
        // H=1,O=1,S=0, not in the store: the new home is uncommitted -> quarantine it and restore .old,
        // so the store-authorized credentials in .old are never destroyed (f9/f13).
        let base = temp_dir();
        let home = base.join("home");
        let old = base.join("home.old-n3");
        // The "new" home in place is this run's (marker-owned); .old is the original valid home.
        owned_dir(&home, "n3");
        std::fs::write(home.join("auth.json"), b"NEW").unwrap();
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("auth.json"), b"OLD").unwrap();
        let mp = base.join("m.marker");
        let j = journal(Operation::Relogin, &home, None, Some(&old), "n3");
        assert_eq!(recover(&base, &mp, &j).unwrap(), Recovery::Retain);
        // The valid old home is back in place; the uncommitted new home is quarantined, not deleted.
        assert!(home.exists());
        assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), b"OLD");
        let rejected = base.join("home.rejected-n3");
        assert!(rejected.exists(), "the uncommitted home was quarantined");
        assert_eq!(std::fs::read(rejected.join("auth.json")).unwrap(), b"NEW");
    }

    #[test]
    fn recovery_refuses_when_a_relogin_home_is_lost() {
        // H=0,O=0,S=0 for re-login: the home is gone with nothing to restore -> fail closed (Retain).
        let base = temp_dir();
        let home = base.join("home");
        let j = journal(Operation::Relogin, &home, None, None, "n4");
        assert_eq!(
            recover(&base, &base.join("m.marker"), &j).unwrap(),
            Recovery::Retain
        );
    }

    #[test]
    fn recovery_refuses_to_touch_a_home_it_does_not_own() {
        // H=1,O=1,S=0 uncommitted, but `home` lacks our ownership marker (a replacement): refuse.
        let base = temp_dir();
        let home = base.join("home");
        let old = base.join("home.old-n5");
        std::fs::create_dir_all(&home).unwrap(); // NOT owned-marked
        std::fs::create_dir_all(&old).unwrap();
        let j = journal(Operation::Relogin, &home, None, Some(&old), "n5");
        assert_eq!(
            recover(&base, &base.join("m.marker"), &j).unwrap(),
            Recovery::Retain
        );
        assert!(home.exists() && old.exists(), "nothing was moved");
    }

    #[test]
    fn parse_reviewer_accepts_the_two_families_and_rejects_others() {
        use serde_json::json;
        assert_eq!(
            parse_reviewer(&json!({"reviewer": "codex"})).unwrap(),
            ReviewerKind::Codex
        );
        assert_eq!(
            parse_reviewer(&json!({"reviewer": "claude"})).unwrap(),
            ReviewerKind::Claude
        );
        assert_eq!(
            parse_reviewer(&json!({"reviewer": "gemini"}))
                .unwrap_err()
                .code,
            "BAD_REQUEST"
        );
        assert_eq!(parse_reviewer(&json!({})).unwrap_err().code, "BAD_REQUEST");
    }

    #[test]
    fn parse_selector_validates_name_home_and_their_exclusivity() {
        use serde_json::json;
        fn sel(v: Value) -> Result<ProfileSelector, Failure> {
            parse_selector(&v, ReviewerKind::Codex)
        }
        // A valid named profile and a valid absolute explicit home.
        assert_eq!(
            sel(json!({"profile": "work"})).unwrap(),
            ProfileSelector::Named("work".to_string())
        );
        assert_eq!(
            sel(json!({"home": r"C:\homes\work"})).unwrap(),
            ProfileSelector::ExplicitHome(PathBuf::from(r"C:\homes\work"))
        );
        // Both, neither, an unsafe name, and a relative home are all rejected.
        assert_eq!(
            sel(json!({"profile": "work", "home": r"C:\x"}))
                .unwrap_err()
                .code,
            "BAD_REQUEST"
        );
        assert_eq!(sel(json!({})).unwrap_err().code, "BAD_REQUEST");
        assert_eq!(
            sel(json!({"profile": "a/b"})).unwrap_err().code,
            "BAD_REQUEST"
        );
        assert_eq!(
            sel(json!({"home": r"relative\path"})).unwrap_err().code,
            "BAD_REQUEST"
        );
    }

    #[test]
    fn a_second_setup_of_the_same_home_is_refused_while_the_first_holds_the_lock() {
        let base = temp_dir();
        let home = base.join("profiles").join("codex").join("work");
        let short = Duration::from_millis(150);
        let first = SetupSession::begin_with_wait(&base, &home, short).expect("first setup");
        // A concurrent setup of the *same* home cannot take the lock. (`SetupSession` is not `Debug`,
        // so match rather than `expect_err`; the short wait keeps the test fast.)
        match SetupSession::begin_with_wait(&base, &home, short) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
            Ok(_) => panic!("a second setup of the same home must be refused"),
        }
        // A different home is independent.
        let other = base.join("profiles").join("codex").join("personal");
        let _second = SetupSession::begin(&base, &other).expect("different home ok");
        drop(first);
        // Once the first releases, the same home can be set up again.
        let _reacquired = SetupSession::begin(&base, &home).expect("reacquire after release");
    }

    fn marker_path(base: &Path, home: &Path) -> PathBuf {
        base.join("auth")
            .join(format!("setup-{}.marker", home_key(home)))
    }

    #[test]
    fn a_live_setup_has_a_marker_and_commit_or_drop_clears_it() {
        let base = temp_dir();
        let home = base.join("profiles").join("codex").join("work");
        let marker = marker_path(&base, &home);
        {
            let session = SetupSession::begin(&base, &home).expect("begin");
            assert!(
                marker.exists(),
                "a live setup records an in-progress marker"
            );
            session.commit().expect("commit");
        }
        assert!(!marker.exists(), "commit clears the marker");
        // A dropped (uncommitted) session also clears its marker.
        {
            let _session = SetupSession::begin(&base, &home).expect("begin");
            assert!(marker.exists());
        }
        assert!(!marker.exists(), "drop clears the marker");
    }

    #[test]
    fn a_stale_marker_from_a_crashed_run_is_cleared_on_the_next_begin() {
        let base = temp_dir();
        let home = base.join("profiles").join("codex").join("work");
        let marker = marker_path(&base, &home);
        // Create the auth dir (via a throwaway committed session), then plant a stale marker as a
        // crashed run would leave (no live lock holder).
        SetupSession::begin(&base, &home).unwrap().commit().unwrap();
        std::fs::write(
            &marker,
            br#"{"holder_pid":999999,"holder_nonce":"x","expiry_unix":0}"#,
        )
        .unwrap();
        // A fresh setup takes the lock and replaces the stale marker with its own.
        let session = SetupSession::begin(&base, &home).expect("begin over stale marker");
        assert!(marker.exists(), "the fresh run wrote its own marker");
        session.commit().unwrap();
        assert!(!marker.exists());
    }

    #[test]
    fn a_wrong_typed_optional_argument_is_rejected_not_ignored() {
        use serde_json::json;
        // [f6]: a numeric `profile` beside a valid `home` must be a hard error, not silently treated
        // as a home-only request.
        assert_eq!(
            parse_selector(&json!({"profile": 5, "home": r"C:\x"}), ReviewerKind::Codex)
                .unwrap_err()
                .code,
            "BAD_REQUEST"
        );
        // A non-string reviewer is likewise rejected rather than read as absent.
        assert_eq!(
            parse_reviewer(&json!({"reviewer": true})).unwrap_err().code,
            "BAD_REQUEST"
        );
    }
}
