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

/// The in-setup marker beside the lock: a small record that a setup is (or was) in progress.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProvisionalMarker {
    holder_pid: u32,
    /// A per-run random nonce, so a recycled pid cannot be mistaken for the same run.
    holder_nonce: String,
    /// Unix seconds after which this marker is considered abandoned (secondary to the lock).
    expiry_unix: u64,
}

/// A live setup: holds the per-home exclusive lock and owns the provisional marker for its lifetime.
/// Dropping it without [`commit`](Self::commit) clears the marker; the lock releases with it.
pub struct SetupSession {
    _lock: ExclusiveLock,
    marker_path: PathBuf,
    holder_nonce: String,
    committed: bool,
}

impl SetupSession {
    /// Acquire the exclusive setup lock for `effective_home` under `base`, clear any in-progress marker
    /// a crashed prior run left behind, and write a fresh one.
    ///
    /// Fails with [`io::ErrorKind::WouldBlock`] if another live setup for the same home holds the lock.
    pub fn begin(base: &Path, effective_home: &Path) -> io::Result<Self> {
        Self::begin_with_wait(base, effective_home, SETUP_LOCK_WAIT)
    }

    /// [`begin`](Self::begin) with an explicit lock-wait, so tests do not wait the full production
    /// timeout to observe contention.
    fn begin_with_wait(base: &Path, effective_home: &Path, wait: Duration) -> io::Result<Self> {
        // The lock and marker live in the store's secured `auth` directory, so another user cannot
        // create or tamper with them. Ensure it exists and is locked down first.
        std::fs::create_dir_all(base)?;
        let auth = base.join("auth");
        let _dir = crate::winsec::create_secured_dir(&auth)?;

        let key = home_key(effective_home);
        let lock_path = auth.join(format!("setup-{key}.lock"));
        let marker_path = auth.join(format!("setup-{key}.marker"));

        // Acquire the exclusive lock. Holding it means no other *live* setup of this home is running.
        let lock = ExclusiveLock::acquire(&lock_path, wait)?;

        // We now hold the lock, so any marker present is from a prior run that died or exited without
        // clearing it (a live run would still hold the lock we just took). Clear it before starting.
        let _ = remove_if_present(&marker_path);

        let holder_nonce = crate::digest::random_hex_token(16)
            .ok_or_else(|| io::Error::other("could not generate a setup nonce"))?;
        let session = Self {
            _lock: lock,
            marker_path,
            holder_nonce,
            committed: false,
        };
        session.write_marker()?;
        Ok(session)
    }

    /// Commit the setup: the run succeeded, so the marker is removed. After this, dropping the session
    /// does nothing.
    pub fn commit(mut self) -> io::Result<()> {
        remove_if_present(&self.marker_path)?;
        self.committed = true;
        Ok(())
    }

    fn write_marker(&self) -> io::Result<()> {
        let marker = ProvisionalMarker {
            holder_pid: std::process::id(),
            holder_nonce: self.holder_nonce.clone(),
            expiry_unix: now_unix().saturating_add(MARKER_TTL_SECS),
        };
        let json = serde_json::to_vec_pretty(&marker)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        atomic_write(&self.marker_path, &json)
    }
}

impl Drop for SetupSession {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Uncommitted: clear the in-progress marker (best-effort). The lock releases as `_lock` drops.
        let _ = remove_if_present(&self.marker_path);
    }
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

/// A stable key for a home path, so the lock/marker file names are the same for the *same physical
/// directory* regardless of spelling. The path is canonicalized first (resolving `.`/`..`, symlinks,
/// and 8.3/short-name and case aliases via the OS), so equivalent spellings such as `C:\x\work` and
/// `C:\x\.\work` map to one key and cannot run concurrent setups of one home ([f5]). When the path
/// cannot be canonicalized (it does not exist yet), the normalized string is used as a best effort.
/// Collision-resistant (SHA-256), truncated for a tidy file name.
fn home_key(effective_home: &Path) -> String {
    let resolved =
        std::fs::canonicalize(effective_home).unwrap_or_else(|_| effective_home.to_path_buf());
    let normalized = resolved.to_string_lossy().to_lowercase().replace('/', "\\");
    let normalized = normalized.trim_end_matches('\\');
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
