//! The per-machine profile authorization store.
//!
//! A profile *use* is authorized only by an entry in this store, which binds an immutable **launch
//! root** to the exact profile it may drive: `(launch_root) → (canonical effective_home +
//! reviewer_family + account_fingerprint)`. All four must match for a review to run under a profile,
//! so authorization survives neither a moved checkout (launch root differs), a different profile home
//! (home differs), the wrong reviewer (family differs), nor a silent re-login of the profile to
//! another account (fingerprint differs). This is the single contract `[f19]`; setup writes it,
//! [`crate::config::Config::resolve_authorized_home`] reads it.
//!
//! The store is the authorization boundary, so it carries its own security contract `[f22]`: its
//! directory and file are locked to the current user with the restrictive, inheritance-protected DACL
//! from [`crate::winsec`] (an attacker who could write the store could authorize themselves). Reads
//! verify that DACL and reject a reparse point; writes are atomic (temp + rename) under a
//! cross-process exclusive lock. Any ACL/verification/parse failure is treated as untrusted and fails
//! closed — every profile use is refused rather than risk honouring a tampered store.
//!
//! The location is fixed under `%CROSS_REVIEW_HOME%` / `%LOCALAPPDATA%\cross-review` (never
//! `--state-dir`, which a repo can point anywhere): a repo must not be able to choose where its own
//! authorization is recorded.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::session::ExclusiveLock;

/// Distinguishes temp files written by concurrent writers in this process (mirrors `session`).
static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// How long to wait for another process to release the store lock. Short: it only guards a
/// read-modify-write of a small JSON file, as in [`crate::session`].
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// One authorization: an immutable launch root bound to the exact profile it may drive. Serialized
/// as-is into the store; also the query key `resolve_authorized_home` matches against.
///
/// Paths are compared with `pathcmp` identity (Windows case- and separator-insensitive), family and
/// fingerprint byte-exact. The stored strings keep their original spelling for the human reading the
/// file; only comparison is normalized.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowEntry {
    /// The immutable directory the authorized process was launched from (`Config::launch_root`),
    /// canonicalized. Never `Config::cwd`, which `--cwd` can steer.
    pub launch_root: String,
    /// The canonical resolved config home the review runs under.
    pub effective_home: String,
    /// `"codex"` or `"claude"` — the reviewer family, so one family's authorization never satisfies
    /// the other's.
    pub reviewer_family: String,
    /// The account currently required in `effective_home` (Codex `tokens.account_id`, Claude
    /// account/org uuid). A re-login to a different account changes this, and the review is refused
    /// until reauthorized.
    pub account_fingerprint: String,
}

impl AllowEntry {
    /// Whether `self` (a stored entry) authorizes the profile use described by `query` (built from the
    /// live launch root and profile home). All four fields must agree.
    ///
    /// The two paths compare by **filesystem identity**, not a folded string: on a case-sensitive
    /// directory `C:\dev\Repo` and `C:\dev\repo` are *different* objects, so a folded comparison could
    /// let an entry for one authorize a launch from the other. [`identity_path_matches`] treats a mere
    /// case/separator difference as a match only when both spellings resolve to the same directory on
    /// disk, and fails closed when a stored path no longer resolves. Family and fingerprint are exact.
    fn authorizes(&self, query: &AllowEntry) -> bool {
        crate::pathcmp::identity_path_matches(Path::new(&query.launch_root), &self.launch_root)
            && crate::pathcmp::identity_path_matches(
                Path::new(&query.effective_home),
                &self.effective_home,
            )
            && self.reviewer_family == query.reviewer_family
            && self.account_fingerprint == query.account_fingerprint
    }
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    entries: Vec<AllowEntry>,
}

/// The authorization store rooted at a fixed base directory. Cheap to construct; does no I/O until a
/// query or write.
pub struct AllowlistStore {
    /// The secured directory holding the store file — `{base}\auth`. Its own protected DACL is what
    /// stops another user creating or replacing the store even if `{base}` were permissive.
    dir: PathBuf,
}

impl AllowlistStore {
    /// The store under a resolved base (`%CROSS_REVIEW_HOME%` / `%LOCALAPPDATA%\cross-review`).
    pub fn at(base: &Path) -> Self {
        Self {
            dir: base.join("auth"),
        }
    }

    /// The store for the current machine, or `None` when no base is resolvable (neither
    /// `CROSS_REVIEW_HOME` nor `LOCALAPPDATA` is set) — in which case nothing can be authorized.
    pub fn current() -> Option<Self> {
        crate::profile::profile_base().map(|base| Self::at(&base))
    }

    fn file(&self) -> PathBuf {
        self.dir.join("allowlist.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("allowlist.json.lock")
    }

    /// Whether `query` is authorized by a stored entry.
    ///
    /// Fail-closed: a genuinely absent store is `Ok(false)` (the normal first-run state — nothing is
    /// authorized yet), but any *present* store that cannot be securely read — a widened DACL, a
    /// reparse point standing in for the file, unparseable contents — is an `Err`, so a tampered or
    /// corrupt store refuses every profile rather than silently authorizing or silently denying as if
    /// empty. No directory is created on this read path.
    pub fn is_authorized(&self, query: &AllowEntry) -> io::Result<bool> {
        let file = self.file();
        match file.try_exists() {
            Ok(false) => return Ok(false),
            Ok(true) => {}
            Err(e) => return Err(e),
        }
        let bytes = crate::winsec::read_secured_file(&file)?;
        let store: StoreFile = serde_json::from_slice(&bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("authorization store {} is corrupt: {e}", file.display()),
            )
        })?;
        Ok(store.entries.iter().any(|e| e.authorizes(query)))
    }

    /// Add an authorization, if an identical one is not already present. Used by the Phase 3 setup
    /// tool (#15) once a profile's identity is confirmed under the human gate; there is no review-path
    /// caller. Serialized across processes by the store lock, written atomically with the restrictive
    /// DACL. Returns whether a new entry was written (`false` if it already existed).
    #[allow(dead_code)] // production caller lands with the setup tool (Phase 3 task #15).
    pub fn authorize(&self, entry: AllowEntry) -> io::Result<bool> {
        // Secure the directory first (create + ACL + verify), so the file we write and lock lives in a
        // directory only this user can write. `{base}` itself is created plain (it inherits the
        // user-scoped %LOCALAPPDATA% ACL); `{base}\auth` gets the protected DACL.
        if let Some(base) = self.dir.parent() {
            std::fs::create_dir_all(base)?;
        }
        let _dir = crate::winsec::create_secured_dir(&self.dir)?;

        let _lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
        let mut store = self.read_locked()?;
        if store.entries.contains(&entry) {
            return Ok(false);
        }
        store.entries.push(entry);
        self.write_locked(&store)?;
        Ok(true)
    }

    /// Read the store while holding the lock, tolerating a genuinely absent file (empty store) but
    /// failing closed on any present-but-unreadable one — the mutating parallel to [`is_authorized`].
    fn read_locked(&self) -> io::Result<StoreFile> {
        let file = self.file();
        match file.try_exists() {
            Ok(false) => Ok(StoreFile::default()),
            Ok(true) => {
                let bytes = crate::winsec::read_secured_file(&file)?;
                serde_json::from_slice(&bytes).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "authorization store {} is corrupt; refusing to overwrite it: {e}",
                            file.display()
                        ),
                    )
                })
            }
            Err(e) => Err(e),
        }
    }

    fn write_locked(&self, store: &StoreFile) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(store)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = self.dir.join(format!(
            "allowlist.json.{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        crate::winsec::write_secured_file(&self.file(), &tmp, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn temp_dir() -> TempDir {
        crate::testutil::temp_dir("cross-review-allowlist-tests")
    }

    fn entry(root: &str, home: &str, family: &str, fp: &str) -> AllowEntry {
        AllowEntry {
            launch_root: root.to_string(),
            effective_home: home.to_string(),
            reviewer_family: family.to_string(),
            account_fingerprint: fp.to_string(),
        }
    }

    #[test]
    fn absent_store_authorizes_nothing() {
        let dir = temp_dir();
        let store = AllowlistStore::at(&dir);
        let q = entry(r"C:\repo", r"C:\home\work", "codex", "acct-1");
        assert!(!store.is_authorized(&q).expect("query"));
    }

    #[test]
    fn an_authorized_tuple_matches_and_a_changed_field_does_not() {
        let dir = temp_dir();
        let store = AllowlistStore::at(&dir);
        let e = entry(r"C:\repo", r"C:\home\work", "codex", "acct-1");
        assert!(store.authorize(e.clone()).expect("authorize"));
        // The exact tuple is authorized.
        assert!(store.is_authorized(&e).expect("q1"));
        // A different account (re-login) is not.
        assert!(!store
            .is_authorized(&entry(r"C:\repo", r"C:\home\work", "codex", "acct-2"))
            .expect("q2"));
        // A different launch root is not.
        assert!(!store
            .is_authorized(&entry(r"C:\other", r"C:\home\work", "codex", "acct-1"))
            .expect("q3"));
        // A different reviewer family is not.
        assert!(!store
            .is_authorized(&entry(r"C:\repo", r"C:\home\work", "claude", "acct-1"))
            .expect("q4"));
        // A different home is not.
        assert!(!store
            .is_authorized(&entry(r"C:\repo", r"C:\home\personal", "codex", "acct-1"))
            .expect("q5"));
    }

    #[test]
    fn a_different_spelling_of_the_same_real_dir_matches_but_a_folded_nonexistent_one_does_not() {
        // Identity, not folded strings: a case/separator-only difference authorizes only when both
        // spellings resolve to the *same real directory*. Use real temp dirs so the on-disk identity
        // check (canonicalize) has something to resolve.
        let dir = temp_dir();
        let root = dir.join("launch");
        let home = dir.join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let store = AllowlistStore::at(&dir);
        store
            .authorize(entry(
                &root.to_string_lossy(),
                &home.to_string_lossy(),
                "codex",
                "acct-1",
            ))
            .expect("authorize");

        // The same real directories spelled with forward slashes still match (they resolve to the
        // same object on disk).
        let root_fwd = root.to_string_lossy().replace('\\', "/");
        let home_fwd = home.to_string_lossy().replace('\\', "/");
        assert!(store
            .is_authorized(&entry(&root_fwd, &home_fwd, "codex", "acct-1"))
            .expect("query same dir, different spelling"));

        // A *different* real directory does not match, even one that only differs in a trailing
        // component — identity is by object, not by folded string.
        let other = dir.join("launch2");
        std::fs::create_dir_all(&other).unwrap();
        assert!(!store
            .is_authorized(&entry(
                &other.to_string_lossy(),
                &home.to_string_lossy(),
                "codex",
                "acct-1"
            ))
            .expect("query different dir"));
    }

    #[test]
    fn authorize_is_idempotent() {
        let dir = temp_dir();
        let store = AllowlistStore::at(&dir);
        let e = entry(r"C:\repo", r"C:\home\work", "codex", "acct-1");
        assert!(store.authorize(e.clone()).expect("first"));
        // A second identical authorize writes nothing new.
        assert!(!store.authorize(e).expect("second"));
    }

    #[test]
    fn authorization_persists_across_store_handles() {
        let dir = temp_dir();
        let e = entry(r"C:\repo", r"C:\home\work", "claude", "org/acct");
        AllowlistStore::at(&dir)
            .authorize(e.clone())
            .expect("write");
        // A fresh handle (as a later process would open) reads the persisted, secured store.
        assert!(AllowlistStore::at(&dir).is_authorized(&e).expect("read"));
    }

    #[test]
    fn a_corrupt_store_fails_closed() {
        let dir = temp_dir();
        let store = AllowlistStore::at(&dir);
        // Seed a valid store so the secured directory and file exist, then corrupt the file in place
        // (preserving its DACL) so the failure is parse, not permissions.
        store
            .authorize(entry(r"C:\repo", r"C:\home\work", "codex", "acct-1"))
            .expect("seed");
        std::fs::write(store.file(), b"{ not json").expect("corrupt");
        let err = store
            .is_authorized(&entry(r"C:\repo", r"C:\home\work", "codex", "acct-1"))
            .expect_err("corrupt store must fail closed, not authorize or silently deny");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
