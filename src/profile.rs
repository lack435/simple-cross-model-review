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
}

/// Validate a profile name: a strict safe name, so it cannot escape the profile root when joined.
///
/// Grammar: one or more of `[A-Za-z0-9._-]`, and not the traversal names `.` or `..`. This rejects
/// path separators, drive prefixes (`C:`), rooted paths, whitespace, and anything else that could
/// turn `{base}\profiles\{reviewer}\{name}` into a path outside the root. Containment is *also*
/// checked at resolution time; this is the first line, not the only one.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a profile name must not be empty".to_string());
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
    Ok(())
}

/// The base directory profiles live under: `%CROSS_REVIEW_HOME%` when set, else
/// `%LOCALAPPDATA%\cross-review`. Deliberately independent of `--state-dir`, which is user- and
/// repo-settable and must never determine a credential home. `None` when neither is set.
pub fn profile_base() -> Option<PathBuf> {
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

/// The per-reviewer profile root under `base`: `{base}\profiles\{reviewer}`.
fn profile_root(base: &Path, reviewer: ReviewerKind) -> PathBuf {
    base.join("profiles").join(reviewer.as_str())
}

/// Resolve a selector to its effective config home, or `None` for `Ambient` (inherit).
///
/// For `Named`, the path is `{base}\profiles\{reviewer}\{name}` and is checked to lie under the
/// profile root (belt-and-braces with [`validate_profile_name`]). For `ExplicitHome`, the caller's
/// path is returned as-is (no containment: it is deliberately outside the root). Filesystem
/// hardening — reparse-point rejection on the real directory, handle-based creation, ACLs — belongs
/// to provisioning (a later phase) and is applied when the home is actually created/opened; Phase 1
/// never opens the home because non-ambient use is refused upstream.
pub fn resolve_home(
    selector: &ProfileSelector,
    reviewer: ReviewerKind,
    base: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    match selector {
        ProfileSelector::Ambient => Ok(None),
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
            Ok(Some(home))
        }
        ProfileSelector::ExplicitHome(path) => {
            if path.as_os_str().is_empty() {
                return Err(
                    "--codex-home / --claude-config-dir requires a non-empty path".to_string(),
                );
            }
            Ok(Some(path.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_accept_and_reject() {
        for ok in ["work", "personal", "acme-corp", "a.b_c-1", "A1"] {
            assert!(validate_profile_name(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "", ".", "..", "a/b", "a\\b", "..\\x", "C:x", "a b", "a:b", "a*b",
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
}
