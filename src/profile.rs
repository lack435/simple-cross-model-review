//! Reviewer account profiles: which config home — and therefore which account — a reviewer child
//! runs under.
//!
//! A reviewer CLI resolves its account from its config home (`$CODEX_HOME`, `$CLAUDE_CONFIG_DIR`).
//! A *profile* points that home at a dedicated per-user directory so a review bills the intended
//! account regardless of what the user's desktop app is signed into. See
//! `docs/reviewer-account-profiles.md` (design) and `docs/reviewer-account-profiles-impl.md` (plan).
//!
//! This module owns the selector type, name validation, and home *resolution*. Authorization (the
//! allowlist), directory provisioning, ACLs, and the setup tool are later phases; Phase 1 resolves
//! and validates only, and every non-ambient *use* is refused upstream (deny-all) until then.

use std::path::{Path, PathBuf};

use crate::config::ReviewerKind;

/// Which config home a reviewer chain entry runs under. Part of the reviewer's identity, alongside
/// model/effort/bin, so a fallback entry can carry its own account and a resume cannot cross one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileSelector {
    /// No profile flag: inherit the ambient environment exactly as before this feature existed.
    Ambient,
    /// `--codex-profile` / `--claude-profile <name>`: a named profile under the profile root. The
    /// name is a validated safe name (see [`validate_profile_name`]).
    Named(String),
    /// `--codex-home` / `--claude-config-dir <abs>`: an explicit home directory. Local/trusted-only
    /// escape hatch; still requires authorization like a named profile (later phase).
    ExplicitHome(PathBuf),
}

impl ProfileSelector {
    /// A short, non-secret label for logs and identity strings. Never the resolved path for
    /// `Named` (that depends on the base), so two installs with the same name read the same here.
    pub fn label(&self) -> String {
        match self {
            ProfileSelector::Ambient => "ambient".to_string(),
            ProfileSelector::Named(n) => format!("profile:{n}"),
            ProfileSelector::ExplicitHome(p) => format!("home:{}", p.display()),
        }
    }

    /// Whether this selector points at a dedicated home (anything but `Ambient`). Non-ambient use is
    /// what requires authorization and a controlled child environment.
    pub fn is_ambient(&self) -> bool {
        matches!(self, ProfileSelector::Ambient)
    }

    /// Whether this (config-side) selector is the same as a persisted [`crate::session::ProfileSelectorId`]
    /// — used so a resume binds to the chain entry with the *same* profile, not merely the same
    /// reviewer/model/effort. An explicit home is compared as a path (case/separator-insensitive).
    pub fn matches_id(&self, id: &crate::session::ProfileSelectorId) -> bool {
        use crate::session::ProfileSelectorId as Id;
        match (self, id) {
            (ProfileSelector::Ambient, Id::Ambient) => true,
            (ProfileSelector::Named(a), Id::Named(b)) => a == b,
            (ProfileSelector::ExplicitHome(a), Id::ExplicitHome(b)) => {
                crate::pathcmp::identity_eq_str(&a.to_string_lossy(), b)
            }
            _ => false,
        }
    }
}

/// The maximum length of a profile name, in characters. Well under the 255-unit NTFS component cap,
/// and generous for a human-chosen label.
const MAX_PROFILE_NAME_LEN: usize = 64;

/// Windows reserved device basenames (case-insensitive), which alias a device regardless of any
/// extension (`NUL.txt` is still the null device). A profile directory named for one of these could
/// be redirected or misbehave, so they are refused.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a profile name: a strict *safe Windows path component*, so it cannot escape the profile
/// root, alias another profile, or name a device.
///
/// Grammar: 1..=[`MAX_PROFILE_NAME_LEN`] of `[A-Za-z0-9._-]`, not the traversal names `.`/`..`, not
/// ending in `.` (Win32 path APIs strip a trailing dot, so `work.` would alias `work` while the native
/// `NtCreateFile` path treats them as distinct — a cross-API aliasing hazard), and whose device
/// basename (the part before the first `.`) is not a reserved device (`CON`, `NUL`, `COM1`…). This
/// rejects path separators, drive prefixes (`C:`), rooted paths, and whitespace by construction.
/// Containment is *also* checked structurally at provisioning time; this is the first line.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a profile name must not be empty".to_string());
    }
    if name.len() > MAX_PROFILE_NAME_LEN {
        return Err(format!(
            "profile name '{name}' is too long (max {MAX_PROFILE_NAME_LEN} characters)"
        ));
    }
    if name == "." || name == ".." {
        return Err(format!(
            "'{name}' is not a valid profile name (reserved traversal name)"
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "invalid character {bad:?} in profile name '{name}' (allowed: letters, digits, '.', \
             '_', '-')"
        ));
    }
    if name.ends_with('.') {
        return Err(format!(
            "profile name '{name}' must not end in '.' (Windows would strip it, aliasing another \
             profile)"
        ));
    }
    let device_base = name.split('.').next().unwrap_or(name);
    if RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| device_base.eq_ignore_ascii_case(reserved))
    {
        return Err(format!(
            "profile name '{name}' is a reserved Windows device name"
        ));
    }
    Ok(())
}

// Test-only override of the profile base (issue #99). Thread-local so parallel tests never collide,
// and so a test that isolates its base cannot leak that base into another test on the same thread
// past the guard's drop. See `isolate_profile_base`.
#[cfg(test)]
thread_local! {
    static PROFILE_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// The base directory profiles live under: `%CROSS_REVIEW_HOME%` when set, else
/// `%LOCALAPPDATA%\cross-review`. Deliberately independent of `--state-dir`, which is user- and
/// repo-settable and must never determine a credential home. `None` when neither is set.
pub fn profile_base() -> Option<PathBuf> {
    // A test that isolates its base (issue #99) wins over the real machine environment, so
    // authorization is deterministic regardless of the developer's real store.
    #[cfg(test)]
    if let Some(base) = PROFILE_BASE_OVERRIDE.with(|c| c.borrow().clone()) {
        return Some(base);
    }
    if let Some(h) = std::env::var_os("CROSS_REVIEW_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("cross-review"))
}

/// Restores the previous [`profile_base`] override when dropped. Held for the duration of a test that
/// needs a deterministic (empty) store; see [`isolate_profile_base`].
#[cfg(test)]
#[must_use]
pub(crate) struct ProfileBaseGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for ProfileBaseGuard {
    fn drop(&mut self) {
        PROFILE_BASE_OVERRIDE.with(|c| *c.borrow_mut() = self.previous.take());
    }
}

/// Point [`profile_base`] (and therefore the allowlist store and every resolved profile home) at a
/// unique, non-existent directory for the current thread, so authorization is deterministic
/// regardless of the developer machine's real store (issue #99). Nothing is created on disk: an
/// absent store reads as "nothing authorized", which is exactly the empty state these tests assert
/// against. The returned guard restores the prior base on drop.
///
/// The candidate must be verified *absent*, not merely process-unique: the pid+counter names collide
/// across process runs (pids are reused), so a leftover directory from an earlier run — or anything
/// else that happens to sit at that path — could carry `auth`/`profiles` data and reintroduce the very
/// machine-dependence this exists to remove (cross-review f1). So we advance the nonce until the path
/// does not exist, and treat an *inconclusive* `try_exists` (a permission error) as "unusable" and
/// skip it too — only a definite `Ok(false)` is accepted.
///
/// The search is bounded: if `%TEMP%` itself is inaccessible or misconfigured, *every* `try_exists`
/// returns `Err` and an unbounded loop would spin until the counter wrapped — a hang, not a failure.
/// So after a small number of attempts we panic with the last error rather than skip forever
/// (cross-review f2). A working temp directory yields an absent candidate on the first or second try,
/// so the bound is never approached in practice.
#[cfg(test)]
pub(crate) fn isolate_profile_base() -> ProfileBaseGuard {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let tmp = std::env::temp_dir();
    let mut last_err = None;
    let mut base = None;
    for _ in 0..16 {
        let candidate = tmp.join(format!(
            "cross-review-test-base-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match candidate.try_exists() {
            Ok(false) => {
                base = Some(candidate);
                break;
            }
            Ok(true) => {}
            Err(e) => last_err = Some(e),
        }
    }
    let base = base.unwrap_or_else(|| {
        panic!(
            "isolate_profile_base: no absent candidate under {} after 16 attempts (last error: {:?})",
            tmp.display(),
            last_err
        )
    });
    let previous = PROFILE_BASE_OVERRIDE.with(|c| c.borrow_mut().replace(base));
    ProfileBaseGuard { previous }
}

/// The per-reviewer profile root under `base`: `{base}\profiles\{reviewer}`.
fn profile_root(base: &Path, reviewer: ReviewerKind) -> PathBuf {
    base.join("profiles").join(reviewer.as_str())
}

/// Resolve a selector to its effective config home, or `None` for `Ambient` (inherit).
///
/// **Not the review-path resolver.** Review code must obtain a home through
/// [`crate::config::Config::resolve_authorized_home`], which resolves *and* authorizes in one step;
/// this raw resolver performs no authorization and exists only for that function and the Phase 3
/// provisioning path to build on (code-review f5). `pub(crate)` keeps it off any external surface.
///
/// For `Named`, the path is `{base}\profiles\{reviewer}\{name}`, checked to lie under the profile
/// root (belt-and-braces with [`validate_profile_name`]). For `ExplicitHome`, the caller's path is
/// returned as-is (no containment: it is deliberately outside the root). Either way the resolved home
/// must be **absolute** — a relative home would bind differently under different child working
/// directories (code-review f4). Filesystem hardening — reparse-point rejection on the real
/// directory, handle-based creation, ACLs, and symlink canonicalization — belongs to provisioning
/// (a later phase) and is applied when the home is created/opened; Phase 1 never opens it because
/// non-ambient use is refused upstream.
pub(crate) fn resolve_home(
    selector: &ProfileSelector,
    reviewer: ReviewerKind,
    base: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let home = match selector {
        ProfileSelector::Ambient => return Ok(None),
        ProfileSelector::Named(name) => {
            validate_profile_name(name)?;
            let base = base.ok_or_else(|| {
                "cannot resolve a named profile: neither CROSS_REVIEW_HOME nor LOCALAPPDATA is set"
                    .to_string()
            })?;
            let root = profile_root(base, reviewer);
            let home = root.join(name);
            // Lexical containment: after a validated name the join cannot escape, but assert it so a
            // future looser grammar cannot silently open the door. Compared as normalized strings.
            if !crate::reviewer::is_within(&home, &root) {
                return Err(format!(
                    "resolved profile home {} is not under the profile root {}",
                    home.display(),
                    root.display()
                ));
            }
            home
        }
        ProfileSelector::ExplicitHome(path) => {
            if path.as_os_str().is_empty() {
                return Err(
                    "--codex-home / --claude-config-dir requires a non-empty path".to_string(),
                );
            }
            path.clone()
        }
    };
    // The effective home must be absolute so it names one directory regardless of any child's cwd.
    // Named homes are absolute only if their base is (CROSS_REVIEW_HOME could be set to anything);
    // explicit homes are checked at parse too, but this is the single guarantee both share.
    if !home.is_absolute() {
        return Err(format!(
            "the resolved profile home {} is not absolute; set CROSS_REVIEW_HOME (or the explicit \
             home) to an absolute path",
            home.display()
        ));
    }
    Ok(Some(home))
}

/// A provisioned profile home, held open across the vendor login so it cannot be swapped or deleted
/// while a credential is written into it ([f20]).
///
/// The `_handle` is deliberately kept private and alive: it is a no-follow, no-delete-share handle on
/// the verified directory, so dropping [`SecuredProfileDir`] is what releases the hold. `path` is the
/// directory's location, for handing to the vendor CLI as `CODEX_HOME` / `CLAUDE_CONFIG_DIR`.
#[allow(dead_code)] // constructed and held by the Phase 3 setup tool (#15).
pub struct SecuredProfileDir {
    pub path: PathBuf,
    /// The held directory handle. Kept both for its lifetime effect (the no-delete-share hold across
    /// the vendor login) and as the `RootDirectory` for the handle-relative credential operations
    /// below ([f20]/f-a5).
    handle: std::os::windows::io::OwnedHandle,
}

impl SecuredProfileDir {
    /// Prove a credential file the vendor just wrote is a direct child of this held directory and lock
    /// it to the current user, then verify ([f20], apply-then-verify — a fresh child does not inherit
    /// the directory's non-inheritable DACL). Fails closed.
    #[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
    pub fn secure_and_verify_credential(&self, leaf: &std::ffi::OsStr) -> std::io::Result<()> {
        crate::winsec::secure_and_verify_child_file(&self.handle, leaf)
    }

    /// Read a credential/account file **handle-relative** to this held directory (no by-path window),
    /// so the account fingerprint used for authorization is bound to the object we hold (f-a5).
    #[allow(dead_code)] // production caller lands with the setup confirmation probe (#15 part 3b).
    pub fn read_credential(&self, leaf: &std::ffi::OsStr) -> std::io::Result<Vec<u8>> {
        crate::winsec::read_child_file(&self.handle, leaf)
    }

    /// Release the held directory handle **before** a rename into place: the handle omits
    /// `FILE_SHARE_DELETE`, so a `staging → home` rename would fail while it is held (f-r3.4). Returns
    /// the plain path; the caller renames it, then re-opens and re-holds the final home.
    #[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

/// Provision a profile home directory securely and return it **held open**.
///
/// This is the provisioning-path resolver ([f10]) — it *creates and locks* a directory, so it is used
/// only by the human-gated setup flow (#15), never the review path (which reads an already-authorized
/// home via [`crate::config::Config::resolve_authorized_home`]). Creating a profile here never
/// authorizes any repo to use it; that is a separate allowlist entry.
///
/// - **`Named`**: descends **handle-relative** from the trusted base (`profiles` → `{reviewer}` →
///   `{name}`), creating and locking each level to the current user. Because every step is a
///   handle-relative open whose leaf is opened as a reparse point and refused if it is one, a junction
///   swapped in at any level fails the open rather than redirecting, and containment under the profile
///   root is *structural* — no path component is re-resolved ([f5]/[f15]/[f20]). The name is validated
///   first.
/// - **`ExplicitHome`**: the deliberately-outside-the-root escape hatch. Rejects a reparse point on
///   each original ancestor component, then creates and locks the leaf directory by handle. Local /
///   trusted-only.
/// - **`Ambient`**: has no profile home to provision — an error, never reached in the setup flow.
#[allow(dead_code)] // production caller lands with the setup tool (Phase 3 task #15).
pub fn secure_profile_dir(
    selector: &ProfileSelector,
    reviewer: ReviewerKind,
    base: Option<&Path>,
) -> std::io::Result<SecuredProfileDir> {
    use std::ffi::OsStr;
    use std::io;

    match selector {
        ProfileSelector::Ambient => {
            Err(io::Error::other("ambient has no profile home to provision"))
        }
        ProfileSelector::Named(name) => {
            validate_profile_name(name).map_err(io::Error::other)?;
            let base = base.ok_or_else(|| {
                io::Error::other(
                    "cannot provision a named profile: neither CROSS_REVIEW_HOME nor LOCALAPPDATA \
                     is set",
                )
            })?;
            if !base.is_absolute() {
                return Err(io::Error::other(format!(
                    "the profile base {} is not absolute; set CROSS_REVIEW_HOME to an absolute path",
                    base.display()
                )));
            }
            // The trusted anchor: `{base}` inherits the user-scoped %LOCALAPPDATA% ACL (created plain),
            // then is opened no-follow — rejecting `{base}` itself being a reparse point. Everything
            // below is created and locked by handle-relative descent, so it is checked structurally.
            std::fs::create_dir_all(base)?;
            let anchor = crate::winsec::open_dir_no_follow(base, false)?;
            let profiles =
                crate::winsec::create_secured_child_dir(&anchor, OsStr::new("profiles"))?;
            let family =
                crate::winsec::create_secured_child_dir(&profiles, OsStr::new(reviewer.as_str()))?;
            let leaf = crate::winsec::create_secured_child_dir(&family, OsStr::new(name))?;
            let path = profile_root(base, reviewer).join(name);
            Ok(SecuredProfileDir { path, handle: leaf })
        }
        ProfileSelector::ExplicitHome(p) => {
            use std::path::Component;
            if p.as_os_str().is_empty() || !p.is_absolute() {
                return Err(io::Error::other(
                    "--codex-home / --claude-config-dir requires a non-empty absolute path",
                ));
            }
            // The path must name a real *directory leaf*, not a root/prefix, and must contain no
            // `.`/`..` component: `create_secured_dir` reports an existing file as already-present and
            // a `..` resolves to a parent or the drive root, either of which would otherwise receive
            // the restrictive DACL — the wrong object (f1). Reject **both** `CurDir` and `ParentDir`.
            // For an ordinary absolute path `Path::components()` normalizes non-leading `.` away, so
            // the `CurDir` arm never fires there; but a **verbatim `\\?\` path disables that
            // normalization and preserves `.`**, so without the `CurDir` arm a form like
            // `\\?\C:\.\home` would slip through while `\\?\C:\home\.` was inconsistently caught by the
            // leaf check. Rejecting any `.`/`..` outright makes the contract hold uniformly (f4). The
            // directory attribute is also verified on the opened handle inside `create_secured_dir`.
            if p.components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            {
                return Err(io::Error::other(
                    "--codex-home / --claude-config-dir must not contain a '.' or '..' component",
                ));
            }
            if !matches!(p.components().next_back(), Some(Component::Normal(_))) {
                return Err(io::Error::other(
                    "--codex-home / --claude-config-dir must name a directory, not a drive root",
                ));
            }
            // Reject a reparse point on any existing original ancestor **before** creating anything:
            // creating the tail first could follow a junctioned ancestor and mutate the redirected
            // target before we ever error (f3). There is a documented residual ancestor TOCTOU for
            // this local/trusted-only escape hatch; the leaf itself is created and verified by handle.
            crate::winsec::reject_reparse_on_ancestors(p)?;
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let handle = crate::winsec::create_secured_dir(p)?;
            Ok(SecuredProfileDir {
                path: p.clone(),
                handle,
            })
        }
    }
}

/// Exclusively create a **nonce-named staging sibling** of the profile home and return it held open.
///
/// The provisioning flow never creates the final `home` directly; it logs into a staging dir named
/// `{leaf}.staging-{nonce}` beside it (created with `FILE_CREATE`, so a collision is refused, not
/// adopted — the ownership proof, f1/f-a1) and later renames it into place. Same handle-relative,
/// ACL-locked descent as [`secure_profile_dir`], only the leaf differs.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn secure_staging_dir(
    selector: &ProfileSelector,
    reviewer: ReviewerKind,
    base: Option<&Path>,
    nonce: &str,
) -> std::io::Result<SecuredProfileDir> {
    use std::ffi::OsStr;
    use std::io;

    match selector {
        ProfileSelector::Ambient => {
            Err(io::Error::other("ambient has no profile home to provision"))
        }
        ProfileSelector::Named(name) => {
            validate_profile_name(name).map_err(io::Error::other)?;
            let base = base.ok_or_else(|| {
                io::Error::other(
                    "cannot provision a named profile: no CROSS_REVIEW_HOME/LOCALAPPDATA",
                )
            })?;
            if !base.is_absolute() {
                return Err(io::Error::other("the profile base is not absolute"));
            }
            std::fs::create_dir_all(base)?;
            let anchor = crate::winsec::open_dir_no_follow(base, false)?;
            let profiles =
                crate::winsec::create_secured_child_dir(&anchor, OsStr::new("profiles"))?;
            let family =
                crate::winsec::create_secured_child_dir(&profiles, OsStr::new(reviewer.as_str()))?;
            let staging_leaf = format!("{name}.staging-{nonce}");
            let handle =
                crate::winsec::create_new_secured_child_dir(&family, OsStr::new(&staging_leaf))?;
            let path = profile_root(base, reviewer).join(&staging_leaf);
            Ok(SecuredProfileDir { path, handle })
        }
        ProfileSelector::ExplicitHome(p) => {
            use std::path::Component;
            if p.as_os_str().is_empty() || !p.is_absolute() {
                return Err(io::Error::other(
                    "explicit home requires a non-empty absolute path",
                ));
            }
            if p.components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            {
                return Err(io::Error::other(
                    "explicit home must not contain a '.' or '..' component",
                ));
            }
            let leaf = match p.components().next_back() {
                Some(Component::Normal(n)) => n.to_string_lossy().into_owned(),
                _ => {
                    return Err(io::Error::other(
                        "explicit home must name a directory, not a root",
                    ))
                }
            };
            crate::winsec::reject_reparse_on_ancestors(p)?;
            let parent = p
                .parent()
                .ok_or_else(|| io::Error::other("explicit home has no parent directory"))?;
            std::fs::create_dir_all(parent)?;
            let parent_handle = crate::winsec::open_dir_no_follow(parent, false)?;
            let staging_leaf = format!("{leaf}.staging-{nonce}");
            let handle = crate::winsec::create_new_secured_child_dir(
                &parent_handle,
                OsStr::new(&staging_leaf),
            )?;
            Ok(SecuredProfileDir {
                path: parent.join(&staging_leaf),
                handle,
            })
        }
    }
}

impl SecuredProfileDir {
    /// Open an **existing** secured home no-follow, verify its restrictive DACL, and hold it — the
    /// re-open after a `staging → home` rename, so the final identity re-probe and the allowlist commit
    /// run bound to the held object (f3/f-a5). Fails closed on a widened DACL or a reparse point.
    #[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
    pub fn open_existing(path: &Path) -> std::io::Result<SecuredProfileDir> {
        let handle = crate::winsec::open_dir_no_follow(path, false)?;
        crate::winsec::verify_restrictive_dacl(&handle)?;
        Ok(SecuredProfileDir {
            path: path.to_path_buf(),
            handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_accept_and_reject() {
        for ok in [
            "work",
            "personal",
            "acme-corp",
            "a.b_c-1",
            "A1",
            "con-fig",
            "nullish",
        ] {
            assert!(validate_profile_name(ok).is_ok(), "{ok} should be valid");
        }
        let too_long = "a".repeat(MAX_PROFILE_NAME_LEN + 1);
        for bad in [
            "", ".", "..", "...", "a/b", "a\\b", "..\\x", "C:x", "a b", "a:b", "a*b",
            // Trailing-dot aliases (Win32 strips the dot); reserved device names, with or without an
            // extension and any case; and an over-length name.
            "work.", "CON", "con", "nul.txt", "COM1", "LPT9", "aux", &too_long,
        ] {
            assert!(
                validate_profile_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn ambient_resolves_to_none() {
        assert_eq!(
            resolve_home(&ProfileSelector::Ambient, ReviewerKind::Codex, None).unwrap(),
            None
        );
    }

    #[test]
    fn named_resolves_under_the_profile_root() {
        let base = PathBuf::from(r"C:\base");
        let home = resolve_home(
            &ProfileSelector::Named("work".to_string()),
            ReviewerKind::Codex,
            Some(&base),
        )
        .expect("resolve")
        .expect("some");
        assert!(crate::reviewer::is_within(
            &home,
            &base.join("profiles").join("codex")
        ));
        assert!(home.ends_with("work"));
    }

    #[test]
    fn named_rejects_a_traversal_name_before_resolution() {
        let base = PathBuf::from(r"C:\base");
        let err = resolve_home(
            &ProfileSelector::Named("..".to_string()),
            ReviewerKind::Codex,
            Some(&base),
        )
        .unwrap_err();
        assert!(err.contains("traversal"), "{err}");
    }

    #[test]
    fn named_needs_a_base() {
        let err = resolve_home(
            &ProfileSelector::Named("work".to_string()),
            ReviewerKind::Claude,
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("CROSS_REVIEW_HOME") || err.contains("LOCALAPPDATA"),
            "{err}"
        );
    }

    #[test]
    fn explicit_home_is_returned_as_is() {
        let p = PathBuf::from(r"D:\somewhere\home");
        assert_eq!(
            resolve_home(
                &ProfileSelector::ExplicitHome(p.clone()),
                ReviewerKind::Claude,
                None
            )
            .unwrap(),
            Some(p)
        );
    }

    fn temp_dir() -> crate::testutil::TempDir {
        crate::testutil::temp_dir("cross-review-profile-tests")
    }

    /// Confirm a provisioned directory carries the restrictive DACL: open it no-follow and verify.
    fn assert_locked(path: &Path) {
        let handle = crate::winsec::open_dir_no_follow(path, false).expect("open provisioned dir");
        crate::winsec::verify_restrictive_dacl(&handle).expect("provisioned dir must be locked");
    }

    #[test]
    fn secure_profile_dir_named_creates_a_locked_dir_under_the_root() {
        let base = temp_dir();
        let secured = secure_profile_dir(
            &ProfileSelector::Named("work".to_string()),
            ReviewerKind::Codex,
            Some(base.as_path()),
        )
        .expect("provision");
        let root = base.join("profiles").join("codex");
        assert!(crate::reviewer::is_within(&secured.path, &root));
        assert!(secured.path.ends_with("work"));
        assert!(secured.path.is_dir());
        // The leaf and both intermediate directories are locked to the current user.
        assert_locked(&secured.path);
        assert_locked(&root);
        assert_locked(&base.join("profiles"));
    }

    #[test]
    fn secure_profile_dir_named_is_idempotent() {
        let base = temp_dir();
        let sel = ProfileSelector::Named("work".to_string());
        secure_profile_dir(&sel, ReviewerKind::Codex, Some(base.as_path())).expect("first");
        // A second provision of an already-secured profile re-verifies rather than failing.
        secure_profile_dir(&sel, ReviewerKind::Codex, Some(base.as_path())).expect("second");
    }

    #[test]
    fn secure_profile_dir_rejects_a_traversal_name() {
        let base = temp_dir();
        assert!(secure_profile_dir(
            &ProfileSelector::Named("..".to_string()),
            ReviewerKind::Codex,
            Some(base.as_path()),
        )
        .is_err());
    }

    #[test]
    fn secure_profile_dir_explicit_home_creates_a_locked_leaf() {
        let base = temp_dir();
        let home = base.join("explicit").join("home");
        let secured = secure_profile_dir(
            &ProfileSelector::ExplicitHome(home.clone()),
            ReviewerKind::Claude,
            None,
        )
        .expect("provision");
        assert_eq!(secured.path, home);
        assert!(home.is_dir());
        assert_locked(&home);
    }

    #[test]
    fn secure_profile_dir_explicit_home_rejects_a_relative_path() {
        assert!(secure_profile_dir(
            &ProfileSelector::ExplicitHome(PathBuf::from(r"rel\home")),
            ReviewerKind::Claude,
            None,
        )
        .is_err());
    }

    #[test]
    fn secure_profile_dir_explicit_home_rejects_a_file_target() {
        // An explicit home naming an existing *file* must not have the restrictive DACL applied to it
        // (or to a directory reached via a trailing `..`) — it must be a real directory leaf (f1).
        let base = temp_dir();
        let file = base.join("not-a-dir");
        std::fs::write(&file, b"x").expect("write file");
        assert!(secure_profile_dir(
            &ProfileSelector::ExplicitHome(file),
            ReviewerKind::Claude,
            None,
        )
        .is_err());
    }

    #[test]
    fn secure_profile_dir_explicit_home_rejects_a_dotdot_component() {
        let base = temp_dir();
        // `{base}\sub\..` resolves to `{base}` — locking that would be the wrong object (f1).
        let sneaky = base.join("sub").join("..");
        assert!(secure_profile_dir(
            &ProfileSelector::ExplicitHome(sneaky),
            ReviewerKind::Claude,
            None,
        )
        .is_err());
    }

    #[test]
    fn secure_profile_dir_explicit_home_rejects_a_verbatim_dot_component() {
        // A verbatim `\\?\` path disables `.` normalization, so a `.` survives as a CurDir component.
        // It must be refused (the reject check covers CurDir as well as ParentDir), before any
        // filesystem access — so this needs no real directory.
        let verbatim = PathBuf::from(r"\\?\C:\trusted\.\home");
        assert!(secure_profile_dir(
            &ProfileSelector::ExplicitHome(verbatim),
            ReviewerKind::Claude,
            None,
        )
        .is_err());
    }

    #[test]
    fn secure_profile_dir_explicit_home_rejects_a_junctioned_ancestor() {
        // A junction standing in for an ancestor of an explicit home must be refused before the leaf
        // is created — a canonicalize-then-check test would see through it. Junctions do not need
        // elevation, so this runs unprivileged.
        let base = temp_dir();
        let real = base.join("real");
        std::fs::create_dir_all(&real).expect("mkdir real");
        let link = base.join("link");
        assert!(
            crate::testutil::make_junction(&link, &real),
            "could not create a junction to exercise the reparse check"
        );
        let home = link.join("home");
        assert!(
            secure_profile_dir(
                &ProfileSelector::ExplicitHome(home),
                ReviewerKind::Claude,
                None,
            )
            .is_err(),
            "an explicit home under a junctioned ancestor must be refused"
        );
    }
}
