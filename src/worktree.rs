//! The per-call review root (issue #146): review one of the served repository's own git worktrees
//! instead of the server's working root, without switching the main checkout's branch.
//!
//! The requested root is accepted only when it is **both** inside the server's working root and an
//! entry of `git worktree list` for that root. Nested-only is the boundary: every path-scoped read
//! rule, the evidence service and the "outside the working root" checks on the sterile and
//! capability directories then narrow to a subtree of what the server already exposes, and nothing
//! moves outward. Claude Code's `isolation: "worktree"` subagents put their worktrees under
//! `<repo>/.claude/worktrees/<name>`, which is the case this exists for.

use std::path::{Path, PathBuf};

/// One `git worktree list --porcelain` record, reduced to what the check needs.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    /// A bare repository has no working tree to review.
    bare: bool,
    /// Git knows the directory is gone or unusable; never review what it would prune.
    prunable: bool,
}

/// Records are separated by a blank line; each starts with `worktree <path>`. Unknown attribute
/// lines (`HEAD`, `branch`, `detached`, `locked`, …) are ignored: a detached or locked worktree that
/// still exists is a legitimate review root.
fn parse_porcelain(out: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(Entry {
                path: path.to_string(),
                bare: false,
                prunable: false,
            });
        } else if let Some(entry) = current.as_mut() {
            if line == "bare" {
                entry.bare = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                entry.prunable = true;
            }
        }
    }
    entries.extend(current);
    entries
}

/// Canonicalize and strip the verbatim prefix, as `--cwd` itself is normalized, so the result
/// compares and renders like the server root does. `None` when the path cannot be resolved.
fn canonical(path: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(path).ok()?;
    Some(crate::config::normalize_dir(resolved))
}

fn same(a: &Path, b: &Path) -> bool {
    crate::pathcmp::identity_eq_str(&a.to_string_lossy(), &b.to_string_lossy())
}

/// Resolve a caller-supplied `root` against the server's working root. `Ok` is the canonical
/// worktree path to review (the server root itself when `requested` names it); `Err` is a message
/// for the caller. A relative `requested` is taken relative to the server root.
pub fn resolve(server_root: &Path, requested: &str) -> Result<PathBuf, String> {
    let raw = Path::new(requested);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        server_root.join(raw)
    };
    let Some(root) = canonical(&joined) else {
        return Err(format!(
            "'root' {requested:?} does not exist or cannot be resolved."
        ));
    };
    if same(&root, server_root) {
        return Ok(server_root.to_path_buf());
    }
    if !crate::reviewer::is_within(&root, server_root) {
        return Err(format!(
            "'root' {} is outside this server's working root {}. Only a git worktree nested \
             inside the working root can be reviewed (for example <repo>\\.claude\\worktrees\\<name>).",
            root.display(),
            server_root.display()
        ));
    }
    let listing = crate::evidence::list_worktrees(server_root, &crate::evidence::Limits::default())
        .map_err(|e| {
            format!(
                "could not list this repository's git worktrees: {}",
                e.message
            )
        })?;
    verify_listed(&root, &listing)
}

/// Re-check, just before the root is used, that it still resolves to the same worktree the call
/// was validated against: a directory replaced by a junction in between would otherwise move the
/// review elsewhere. Cheap, not a lock; it closes the window to the time between this and use.
pub fn recheck(server_root: &Path, root: &Path) -> Result<(), String> {
    if same(root, server_root) {
        return Ok(());
    }
    match resolve(server_root, &root.to_string_lossy()) {
        Ok(again) if same(&again, root) => Ok(()),
        Ok(again) => Err(format!(
            "the review root {} now resolves to {}; refusing to review a moved worktree.",
            root.display(),
            again.display()
        )),
        Err(e) => Err(e),
    }
}

fn verify_listed(root: &Path, listing: &str) -> Result<PathBuf, String> {
    let entries = parse_porcelain(listing);
    for entry in &entries {
        let Some(path) = canonical(Path::new(&entry.path)) else {
            continue;
        };
        if !same(&path, root) {
            continue;
        }
        if entry.bare {
            return Err(format!(
                "'root' {} is a bare repository, which has no working tree to review.",
                root.display()
            ));
        }
        if entry.prunable {
            return Err(format!(
                "'root' {} is a git worktree git reports as prunable; repair or remove it first.",
                root.display()
            ));
        }
        return Ok(root.to_path_buf());
    }
    Err(format!(
        "'root' {} is not a git worktree of this repository (see `git worktree list`). Only the \
         working root or one of its own worktrees can be reviewed.",
        root.display()
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn porcelain_records_parse_with_their_flags() {
        let out = "worktree C:/dev/repo\nHEAD abc\nbranch refs/heads/main\n\n\
                   worktree C:/dev/repo/.claude/worktrees/a\nHEAD def\ndetached\nlocked\n\n\
                   worktree C:/dev/repo/.claude/worktrees/gone\nHEAD 123\nprunable gitdir file points to non-existent location\n\n\
                   worktree C:/dev/bare.git\nbare\n";
        let entries = parse_porcelain(out);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].path, "C:/dev/repo");
        assert!(!entries[0].bare && !entries[0].prunable);
        assert!(
            !entries[1].bare && !entries[1].prunable,
            "detached+locked is usable"
        );
        assert!(entries[2].prunable);
        assert!(entries[3].bare);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .output()
            .expect("git runs");
        assert!(
            status.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// A repository with one commit and a linked worktree nested at `.claude/worktrees/b`.
    pub(crate) fn repo_with_worktree(tag: &str) -> (crate::testutil::TempDir, PathBuf, PathBuf) {
        let dir = crate::testutil::temp_dir(tag);
        let repo = canonical(dir.as_path()).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        git(&repo, &["add", "f.txt"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                ".claude/worktrees/b",
                "-b",
                "feature-b",
            ],
        );
        let wt = repo.join(".claude").join("worktrees").join("b");
        (dir, repo, wt)
    }

    #[test]
    fn a_nested_worktree_is_accepted_absolute_or_relative() {
        let (_dir, repo, wt) = repo_with_worktree("wt-accept");
        assert!(same(&resolve(&repo, &wt.to_string_lossy()).unwrap(), &wt));
        assert!(same(&resolve(&repo, ".claude/worktrees/b").unwrap(), &wt));
        recheck(&repo, &wt).unwrap();
    }

    #[test]
    fn the_server_root_itself_is_a_no_op() {
        let (_dir, repo, _wt) = repo_with_worktree("wt-self");
        assert_eq!(resolve(&repo, &repo.to_string_lossy()).unwrap(), repo);
        assert_eq!(resolve(&repo, ".").unwrap(), repo);
    }

    #[test]
    fn a_plain_subdirectory_is_not_a_worktree() {
        let (_dir, repo, _wt) = repo_with_worktree("wt-subdir");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let err = resolve(&repo, "src").unwrap_err();
        assert!(err.contains("not a git worktree"), "{err}");
    }

    #[test]
    fn a_path_outside_the_server_root_is_refused() {
        let (_dir, repo, _wt) = repo_with_worktree("wt-outside");
        let outside = repo.parent().unwrap();
        let err = resolve(&repo, &outside.to_string_lossy()).unwrap_err();
        assert!(err.contains("outside this server's working root"), "{err}");
    }

    #[test]
    fn a_missing_path_is_refused() {
        let (_dir, repo, _wt) = repo_with_worktree("wt-missing");
        let err = resolve(&repo, ".claude/worktrees/nope").unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn an_unrelated_repository_nested_inside_is_refused() {
        let (_dir, repo, _wt) = repo_with_worktree("wt-foreign");
        let other = repo.join("vendor").join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q"]);
        let err = resolve(&repo, "vendor/other").unwrap_err();
        assert!(err.contains("not a git worktree"), "{err}");
    }
}
