//! Companion repositories (issue #167): a second git repository a review may declare, whose change
//! is served through its own evidence server and certified exactly like the working root's.
//!
//! A companion is configured at startup (`--companion <name>=<path>`) and never named by path per
//! call, so widening what a reviewer can read stays an operator decision. Each one is held to the
//! same bar as the working root and is kept **disjoint** from every directory the server or the
//! reviewer writes to: the state directory and the temp directory, in both directions. Every write
//! location is one of those two or derived under one of them (the sterile, neutral and capability
//! directories, `Config::tmp_dir()`, the session and usage files), so a companion disjoint from both
//! bases cannot contain any of them, and the per-directory "outside the working root" checks need no
//! companion awareness. See `docs/companion-repositories-plan.md`.
//!
//! A companion is normally a sibling of the working root. It may also sit **inside** it (issue
//! #170: a vendored engine kept as its own repository and listed in the primary's `.gitignore`), but
//! only while the primary ignores it. An ignored directory contributes nothing to the primary's diff,
//! untracked files included, and the working root must also track nothing under it (no force-added
//! file, no gitlink), so no file is ever certified by two servers. That is checked at startup and
//! again every turn, so removing the ignore rule later refuses the companion rather than letting its
//! files reach both diffs. Certification rests only on each server's canonical diff; *reading* is
//! another matter: like any ignored path, a nested companion stays readable by name through the
//! working root's own evidence tools and read scope, exactly as it was before it was a companion.

use std::path::{Path, PathBuf};

/// The longest companion name. Short so every generated tool name
/// (`mcp__cross_review_evidence_<name>__repository_revision`) stays well inside client limits.
pub const MAX_NAME_LEN: usize = 16;

/// One configured companion: its name and its canonical root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Companion {
    pub name: String,
    pub root: PathBuf,
}

impl Companion {
    /// The MCP server name the reviewer sees this companion's evidence tools under.
    pub fn server_name(&self) -> String {
        format!("{}_{}", crate::evidence::SERVER_NAME, self.name)
    }

    /// This companion's evidence nonce for one review. Distinct from the review id and from every
    /// other companion's, so the cursor tokens, diff op ids and file names each server mints (all
    /// derived from its nonce) never collide across servers.
    pub fn nonce(&self, review_id: &str) -> String {
        format!("{review_id}-c-{}", self.name)
    }
}

/// `^[a-z][a-z0-9-]{0,15}$`. No `_`, so the server name never contains the `__` a client splits
/// `mcp__<server>__<tool>` on.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "--companion name '{name}' must be 1-{MAX_NAME_LEN} characters: a lowercase letter, then \
             lowercase letters, digits or '-'"
        ))
    }
}

/// Split one `--companion` value at its first `=` into `(name, path)`.
pub fn parse_flag(value: &str) -> Result<(String, String), String> {
    match value.split_once('=') {
        Some((name, path)) if !path.trim().is_empty() => {
            validate_name(name)?;
            Ok((name.to_string(), path.to_string()))
        }
        _ => Err(format!("--companion expects <name>=<path>, got '{value}'")),
    }
}

/// Whether two physical paths overlap: equal, or either inside the other. Case-folded through
/// `is_within`, which refuses more rather than less.
fn overlaps(a: &Path, b: &Path) -> bool {
    crate::reviewer::is_within(a, b) || crate::reviewer::is_within(b, a)
}

/// Refuse `root` unless it is disjoint from `dir`, judged on physical paths (junctions followed).
/// An unresolvable `dir` reads as overlapping, which fails closed.
fn require_disjoint(name: &str, root: &Path, dir: &Path, what: &str) -> Result<(), String> {
    match crate::worktree::physical(dir) {
        Some(physical) if !overlaps(root, &physical) => Ok(()),
        Some(physical) => Err(format!(
            "companion '{name}' ({}) overlaps the {what} ({}); a companion must be disjoint from it \
             in both directions, because the server and the reviewer write there",
            root.display(),
            physical.display()
        )),
        None => Err(format!(
            "companion '{name}': the {what} ({}) cannot be resolved, so it cannot be shown to be \
             disjoint from the companion",
            dir.display()
        )),
    }
}

/// The checks that hold for a companion at any moment: no reparse point anywhere along it, a git
/// toplevel, and disjoint from the state and temp directories. Shared by startup and the per-turn
/// recheck, which passes that turn's temp directory.
fn check_root(name: &str, root: &Path, state_dir: &Path, temp_dir: &Path) -> Result<(), String> {
    crate::winsec::reject_reparse_on_ancestors(root)
        .map_err(|e| format!("companion '{name}' ({}): {e}", root.display()))?;
    if !root.join(".git").try_exists().unwrap_or(false) {
        return Err(format!(
            "companion '{name}' ({}) is not the top level of a git repository",
            root.display()
        ));
    }
    require_disjoint(name, root, state_dir, "state directory")?;
    require_disjoint(name, root, temp_dir, "temp directory")
}

/// How a companion relates to the working root. Anything but `Nested` or `Separate` is refused.
fn check_against_root(name: &str, root: &Path, cwd: &Path) -> Result<(), String> {
    if crate::reviewer::is_within(cwd, root) {
        return Err(format!(
            "companion '{name}' ({}) is or contains the working root ({}); a companion must be a \
             separate repository",
            root.display(),
            cwd.display()
        ));
    }
    if !crate::reviewer::is_within(root, cwd) {
        return Ok(());
    }
    // Nested (issue #170): allowed only while the primary ignores it, so its files can never also
    // appear in the primary's diff.
    let rel = root
        .strip_prefix(cwd)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            format!(
                "companion '{name}' ({}) is inside the working root ({}) but its path relative to \
                 it cannot be determined",
                root.display(),
                cwd.display()
            )
        })?;
    let limits = crate::evidence::Limits::default();
    // Ignored is not enough on its own: a file force-added under an ignored path, or a gitlink, is
    // still in the working root's index and so in its diff (review f2).
    match crate::evidence::tracks_under(cwd, &rel, &limits) {
        Ok(false) => {}
        Ok(true) => {
            return Err(format!(
                "companion '{name}' ({}) is inside the working root ({}), which tracks files (or a \
                 submodule) under `{rel}`; those would be reviewed as part of both repositories. \
                 Untrack them in the working root (`git rm -r --cached {rel}`), or move the \
                 companion outside it",
                root.display(),
                cwd.display()
            ))
        }
        Err(e) => {
            return Err(format!(
                "companion '{name}' ({}) is inside the working root, and whether the working root \
                 tracks anything under it could not be checked: {}",
                root.display(),
                e.message
            ))
        }
    }
    match crate::evidence::ignores_untracked(cwd, &rel, &limits) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "companion '{name}' ({}) is inside the working root ({}) but the working root does not \
             ignore it; add `{rel}/` to its .gitignore, or move the companion outside it",
            root.display(),
            cwd.display()
        )),
        Err(e) => Err(format!(
            "companion '{name}' ({}) is inside the working root, and whether the working root \
             ignores it could not be checked: {}",
            root.display(),
            e.message
        )),
    }
}

/// Resolve every configured companion at startup. A relative path joins `cwd`. Refuses a Perforce
/// working root, duplicate names, a path that does not resolve, a reparse point anywhere along it,
/// a directory that is not a git toplevel, one that is or contains the working root, one inside it
/// that the working root does not ignore (issue #170), and any overlap with another companion, the
/// state directory or `temp_dir`.
pub fn resolve_all(
    raw: &[(String, String)],
    cwd: &Path,
    vcs: crate::config::Vcs,
    state_dir: &Path,
    temp_dir: &Path,
) -> Result<Vec<Companion>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if vcs != crate::config::Vcs::Git {
        return Err(
            "--companion needs a git working root; Perforce companions are not supported".into(),
        );
    }
    let mut out: Vec<Companion> = Vec::new();
    for (name, path) in raw {
        if out.iter().any(|c| &c.name == name) {
            return Err(format!("--companion name '{name}' is given more than once"));
        }
        let given = Path::new(path);
        let joined = if given.is_absolute() {
            given.to_path_buf()
        } else {
            cwd.join(given)
        };
        // Checked on the path as given, before canonicalizing, so a junction along it is caught
        // rather than resolved through.
        crate::winsec::reject_reparse_on_ancestors(&joined)
            .map_err(|e| format!("--companion '{name}' ({}): {e}", joined.display()))?;
        let resolved = std::fs::canonicalize(&joined).map_err(|e| {
            format!(
                "--companion '{name}' path {} cannot be resolved: {e}",
                joined.display()
            )
        })?;
        let root = crate::config::normalize_dir(resolved);
        check_root(name, &root, state_dir, temp_dir).map_err(|e| format!("--{e}"))?;
        check_against_root(name, &root, cwd).map_err(|e| format!("--{e}"))?;
        if let Some(other) = out.iter().find(|c| overlaps(&root, &c.root)) {
            return Err(format!(
                "--companion '{name}' ({}) overlaps companion '{}' ({})",
                root.display(),
                other.name,
                other.root.display()
            ));
        }
        out.push(Companion {
            name: name.clone(),
            root,
        });
    }
    Ok(out)
}

/// Re-check, just before a turn uses it, that a companion still resolves to its configured root
/// and still passes every startup check against this turn's temp directory, including, for one
/// nested in the server's working root `server_root`, that the working root still ignores it. Cheap,
/// not a lock.
pub fn recheck(
    c: &Companion,
    server_root: &Path,
    state_dir: &Path,
    temp_dir: &Path,
) -> Result<(), String> {
    let again = std::fs::canonicalize(&c.root)
        .map(crate::config::normalize_dir)
        .map_err(|e| format!("companion '{}' ({}): {e}", c.name, c.root.display()))?;
    if again != c.root {
        return Err(format!(
            "companion '{}' ({}) now resolves to {}; refusing to review a moved repository",
            c.name,
            c.root.display(),
            again.display()
        ));
    }
    check_root(&c.name, &c.root, state_dir, temp_dir)?;
    check_against_root(&c.name, &c.root, server_root)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Vcs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A one-commit git repository at `dir`.
    pub(crate) fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        git(dir, &["add", "f.txt"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// A scratch layout: `primary` and `assets` repositories plus separate state and "temp"
    /// directories. The real `%TEMP%` holds the whole scratch tree, so tests pass their own temp
    /// stand-in; production passes `std::env::temp_dir()`.
    struct Layout {
        _dir: crate::testutil::TempDir,
        base: PathBuf,
        primary: PathBuf,
        assets: PathBuf,
        state: PathBuf,
        temp: PathBuf,
    }

    fn layout(tag: &str) -> Layout {
        let dir = crate::testutil::temp_dir(tag);
        let base = crate::config::normalize_dir(dir.as_path().to_path_buf());
        let primary = base.join("primary");
        let assets = base.join("assets");
        init_repo(&primary);
        init_repo(&assets);
        let state = base.join("state");
        let temp = base.join("tmp");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        Layout {
            _dir: dir,
            base,
            primary,
            assets,
            state,
            temp,
        }
    }

    fn one(name: &str, path: &Path) -> Vec<(String, String)> {
        vec![(name.to_string(), path.to_string_lossy().into_owned())]
    }

    #[test]
    fn companions_need_reviewer_isolation() {
        let l = layout("companion-isolation");
        let err = crate::config::Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--level".into(),
            "standard:gpt-5.6-luna:max".into(),
            "--cwd".into(),
            l.primary.to_string_lossy().into_owned(),
            "--state-dir".into(),
            l.state.to_string_lossy().into_owned(),
            "--allow-reviewer-config".into(),
            "--companion".into(),
            format!("assets={}", l.assets.display()),
        ])
        .unwrap_err();
        assert!(err.contains("--allow-reviewer-config"), "{err}");
    }

    #[test]
    fn names_follow_the_grammar() {
        for good in ["a", "assets", "art-2", "abcdefghijklmnop"] {
            validate_name(good).unwrap();
        }
        for bad in [
            "",
            "Assets",
            "1st",
            "-a",
            "a_b",
            "a__b",
            "abcdefghijklmnopq",
            "a.b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_flag_splits_at_the_first_equals() {
        assert_eq!(
            parse_flag("assets=C:\\a=b").unwrap(),
            ("assets".to_string(), "C:\\a=b".to_string())
        );
        assert!(parse_flag("assets").is_err());
        assert!(parse_flag("assets=").is_err());
        assert!(parse_flag("Bad=C:\\x").is_err());
    }

    #[test]
    fn server_name_and_nonce_are_namespaced_per_companion() {
        let c = Companion {
            name: "assets".into(),
            root: PathBuf::from(r"C:\x"),
        };
        assert_eq!(c.server_name(), "cross_review_evidence_assets");
        assert_eq!(c.nonce("rv-1-2"), "rv-1-2-c-assets");
    }

    #[test]
    fn a_separate_repository_resolves_absolute_or_relative() {
        let l = layout("companion-ok");
        let abs = resolve_all(
            &one("assets", &l.assets),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp,
        )
        .unwrap();
        assert_eq!(abs[0].root, l.assets);
        let rel = vec![("assets".to_string(), r"..\assets".to_string())];
        let rel = resolve_all(&rel, &l.primary, Vcs::Git, &l.state, &l.temp).unwrap();
        assert_eq!(rel[0].root, l.assets);
        recheck(&rel[0], &l.primary, &l.state, &l.temp).unwrap();
    }

    #[test]
    fn perforce_duplicates_and_non_repositories_are_refused() {
        let l = layout("companion-bad");
        let c = one("assets", &l.assets);
        assert!(resolve_all(&c, &l.primary, Vcs::Perforce, &l.state, &l.temp).is_err());
        let dup = vec![c[0].clone(), c[0].clone()];
        assert!(resolve_all(&dup, &l.primary, Vcs::Git, &l.state, &l.temp)
            .unwrap_err()
            .contains("more than once"));
        let plain = l.base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(resolve_all(
            &one("plain", &plain),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp
        )
        .unwrap_err()
        .contains("not the top level"));
        let missing = l.base.join("missing");
        assert!(resolve_all(&one("m", &missing), &l.primary, Vcs::Git, &l.state, &l.temp).is_err());
    }

    #[test]
    fn overlap_with_the_working_root_or_another_companion_is_refused() {
        let l = layout("companion-overlap");
        // Nested inside the working root.
        let nested = l.primary.join("sub");
        init_repo(&nested);
        assert!(resolve_all(
            &one("sub", &nested),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp
        )
        .unwrap_err()
        .contains("working root"));
        // The working root itself.
        assert!(resolve_all(
            &one("p", &l.primary),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp
        )
        .is_err());
        // Two companions, one inside the other.
        let inner = l.assets.join("inner");
        init_repo(&inner);
        let both = vec![
            one("assets", &l.assets)[0].clone(),
            one("inner", &inner)[0].clone(),
        ];
        assert!(resolve_all(&both, &l.primary, Vcs::Git, &l.state, &l.temp)
            .unwrap_err()
            .contains("overlaps companion"));
    }

    #[test]
    fn overlap_with_the_state_or_temp_directory_is_refused_in_both_directions() {
        let l = layout("companion-writes");
        // The state directory inside the companion.
        let state_inside = l.assets.join("state");
        std::fs::create_dir_all(&state_inside).unwrap();
        assert!(resolve_all(
            &one("assets", &l.assets),
            &l.primary,
            Vcs::Git,
            &state_inside,
            &l.temp
        )
        .unwrap_err()
        .contains("state directory"));
        // A companion at <state_dir>/tmp: the state dir is outside it, yet the reviewer writes there.
        let at_tmp = l.state.join("tmp");
        init_repo(&at_tmp);
        assert!(
            resolve_all(&one("t", &at_tmp), &l.primary, Vcs::Git, &l.state, &l.temp)
                .unwrap_err()
                .contains("state directory")
        );
        // The temp directory, both ways.
        let temp_inside = l.assets.join("tmp");
        std::fs::create_dir_all(&temp_inside).unwrap();
        assert!(resolve_all(
            &one("assets", &l.assets),
            &l.primary,
            Vcs::Git,
            &l.state,
            &temp_inside
        )
        .unwrap_err()
        .contains("temp directory"));
        let under_temp = l.temp.join("repo");
        init_repo(&under_temp);
        assert!(resolve_all(
            &one("u", &under_temp),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp
        )
        .unwrap_err()
        .contains("temp directory"));
    }

    #[test]
    fn a_junction_is_refused_at_startup_and_a_physical_overlap_through_one_is_caught() {
        let l = layout("companion-junction");
        // The companion path itself runs through a junction.
        let link = l.base.join("link");
        if !crate::testutil::make_junction(&link, &l.assets) {
            eprintln!("skipping: could not create a junction");
            return;
        }
        assert!(resolve_all(
            &one("assets", &link),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp
        )
        .unwrap_err()
        .contains("reparse point"));
        // A state directory that is a junction into the companion overlaps it physically.
        let state_link = l.base.join("state-link");
        assert!(crate::testutil::make_junction(&state_link, &l.assets));
        assert!(resolve_all(
            &one("assets", &l.assets),
            &l.primary,
            Vcs::Git,
            &state_link,
            &l.temp
        )
        .unwrap_err()
        .contains("state directory"));
    }

    /// Issue #170: a companion nested in the working root is accepted only while the working root
    /// ignores it, directly or through an ignored parent, and the per-turn recheck refuses it once
    /// the ignore rule is gone. A plain (non-repository) subdirectory is refused either way.
    #[test]
    fn a_nested_companion_is_accepted_only_while_the_working_root_ignores_it() {
        let l = layout("companion-nested");
        let engine = l.primary.join("engine");
        init_repo(&engine);
        let resolve = |name: &str, path: &Path| {
            resolve_all(&one(name, path), &l.primary, Vcs::Git, &l.state, &l.temp)
        };

        // Not ignored: refused, saying how to fix it.
        let err = resolve("engine", &engine).unwrap_err();
        assert!(
            err.contains("does not ignore it") && err.contains("engine/"),
            "{err}"
        );

        // Ignored directly: accepted, and the recheck passes.
        std::fs::write(l.primary.join(".gitignore"), "engine/\n").unwrap();
        let c = resolve("engine", &engine).expect("ignored nested companion");
        recheck(&c[0], &l.primary, &l.state, &l.temp).expect("still ignored");

        // The ignore rule removed after startup: the recheck refuses it.
        std::fs::write(l.primary.join(".gitignore"), "").unwrap();
        let err = recheck(&c[0], &l.primary, &l.state, &l.temp).unwrap_err();
        assert!(err.contains("does not ignore it"), "{err}");

        // Ignored through an ignored parent directory: accepted.
        let deep = l.primary.join("vendor").join("sdk");
        init_repo(&deep);
        std::fs::write(l.primary.join(".gitignore"), "vendor/\n").unwrap();
        resolve("sdk", &deep).expect("under an ignored parent");

        // A gitlink the working root force-added at the ignored path is in its diff: refused, both
        // at startup and by the recheck of an already-accepted companion. (Git will not add a file
        // *inside* a nested repository; a gitlink, or a file tracked before the nested repository
        // existed, is how the working root comes to track something there.)
        git(&l.primary, &["add", "-f", "vendor/sdk"]);
        let err = resolve("sdk", &deep).unwrap_err();
        assert!(err.contains("tracks files"), "{err}");
        git(&l.primary, &["rm", "-q", "-f", "--cached", "vendor/sdk"]);
        let sdk = resolve("sdk", &deep).expect("untracked again");
        git(&l.primary, &["add", "-f", "vendor/sdk"]);
        assert!(recheck(&sdk[0], &l.primary, &l.state, &l.temp)
            .unwrap_err()
            .contains("tracks files"));
        git(&l.primary, &["rm", "-q", "-f", "--cached", "vendor/sdk"]);

        // A plain subdirectory, even an ignored one, is not a repository.
        let plain = l.primary.join("vendor").join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(resolve("plain", &plain)
            .unwrap_err()
            .contains("not the top level"));
    }

    /// A companion that contains the working root is refused, ignored or not (issue #170 keeps the
    /// other direction closed).
    #[test]
    fn a_companion_containing_the_working_root_is_refused() {
        let l = layout("companion-contains");
        let inner = l.assets.join("inner-primary");
        init_repo(&inner);
        let err = resolve_all(
            &one("assets", &l.assets),
            &inner,
            Vcs::Git,
            &l.state,
            &l.temp,
        )
        .unwrap_err();
        assert!(err.contains("contains the working root"), "{err}");
    }

    #[test]
    fn recheck_refuses_a_moved_companion_and_a_temp_dir_that_now_overlaps() {
        let l = layout("companion-recheck");
        let c = resolve_all(
            &one("assets", &l.assets),
            &l.primary,
            Vcs::Git,
            &l.state,
            &l.temp,
        )
        .unwrap()
        .remove(0);
        // This turn's temp directory sits inside the companion.
        let temp_now = l.assets.join("t");
        std::fs::create_dir_all(&temp_now).unwrap();
        assert!(recheck(&c, &l.primary, &l.state, &temp_now).is_err());
        // The repository is gone.
        std::fs::remove_dir_all(&l.assets).unwrap();
        assert!(recheck(&c, &l.primary, &l.state, &l.temp).is_err());
    }
}
