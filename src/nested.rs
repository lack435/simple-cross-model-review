//! Nested repositories the reviewer cannot see (issue #217).
//!
//! A git repository nested inside the review root contributes nothing to the root's canonical diff:
//! git never descends into it, and in the #170 layout the root ignores it as well. Unless it is a
//! declared companion, its change is never served, so the reviewer cannot read its diff or certify
//! it. This module finds such repositories that have changes and words a warning naming each one
//! and the fix, so the caller hears about it at review start and on every result.
//!
//! Advisory only: nothing here refuses a review or blocks convergence, and any failure becomes a
//! warning, never an error. The scan is deliberately shallow and capped (see the plan,
//! `docs/nested-repository-visibility-plan.md`): one `ls-files` call, then one level of `read_dir`
//! below each collapsed directory. A repository deeper than that is missed without a warning.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::companion::Companion;

/// At most this many collapsed directories are taken from `ls-files`.
const MAX_LISTED_DIRS: usize = 4096;
/// At most this many directories are inspected for `.git` (listed ones plus their children).
const MAX_INSPECTED_DIRS: usize = 512;
/// At most this many entries are read from any one directory during the one-level descent.
const MAX_CHILDREN_PER_DIR: usize = 256;
/// At most this many nested repositories are examined.
const MAX_CANDIDATES: usize = 32;
/// The whole scan's time budget. It runs before the start response is returned, so it is checked
/// before every git call and every candidate; a call already running can overrun it by at most
/// [`GIT_OP_TIMEOUT_MS`].
const SCAN_BUDGET: Duration = Duration::from_secs(5);
/// Each git call's own ceiling during the scan, well under the evidence service's default.
const GIT_OP_TIMEOUT_MS: u64 = 3_000;

/// Whether a nested repository's change could be determined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// It has a change: `modified` working-tree entries, and/or a branch that has moved past its
    /// fork point (`ahead`; `None` when no fork point resolved).
    Changed {
        modified: usize,
        ahead: Option<bool>,
    },
    /// Its state could not be determined, for the stated reason.
    Unknown(String),
}

/// How a nested repository relates to the review's companion configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Ignored by the review root and not a configured companion.
    Ignored,
    /// Not ignored by the review root, so it cannot be a companion as things stand.
    NotIgnored,
    /// A configured companion (matched by root, never by name) that this review did not declare.
    ConfiguredUndeclared(String),
}

/// One nested repository whose change this review does not serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unserved {
    /// Its path relative to the review root, `/`-separated, with a trailing `/`.
    pub rel: String,
    pub kind: Kind,
    pub state: State,
}

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detection {
    pub unserved: Vec<Unserved>,
    /// Why the scan stopped before covering everything it set out to, if it did.
    pub incomplete: Option<String>,
}

impl Detection {
    /// The caller-facing warnings: one per unserved repository, then one if the scan was incomplete.
    pub fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self.unserved.iter().map(warning).collect();
        if let Some(why) = &self.incomplete {
            out.push(format!(
                "Detection of nested git repositories stopped early ({why}); a nested repository \
                 whose change this review cannot see may not be listed."
            ));
        }
        out
    }

    /// The relative paths to name in the reviewer prompt.
    pub fn paths(&self) -> Vec<String> {
        self.unserved.iter().map(|u| u.rel.clone()).collect()
    }
}

fn describe(state: &State) -> String {
    match state {
        State::Changed { modified, ahead } => {
            let mut parts = Vec::new();
            if *ahead == Some(true) {
                parts.push("commits beyond its fork point".to_string());
            }
            if *modified > 0 {
                parts.push(format!(
                    "{modified} uncommitted {}",
                    if *modified == 1 { "change" } else { "changes" }
                ));
            }
            format!("has changes ({})", parts.join("; "))
        }
        State::Unknown(why) => format!("has changes that could not be determined: {why}"),
    }
}

fn warning(u: &Unserved) -> String {
    let rel = &u.rel;
    let name = rel.trim_end_matches('/');
    let head = format!(
        "`{rel}` is a separate git repository inside the reviewed root and {}. This review cannot \
         see or certify its change.",
        describe(&u.state)
    );
    let fix = match &u.kind {
        Kind::Ignored => format!(
            "If the change under review includes it, configure it as a companion repository \
             (`--companion <name>={name}`) and re-review with `companions: [\"<name>\"]`."
        ),
        Kind::ConfiguredUndeclared(cname) => format!(
            "It is configured as companion `{cname}`; if the change under review includes it, \
             re-review with `companions: [\"{cname}\"]`."
        ),
        Kind::NotIgnored => format!(
            "The reviewed root does not ignore it, so it cannot be a companion; if the change under \
             review includes it, add `{rel}` to the root's .gitignore and configure it as a \
             companion repository."
        ),
    };
    format!("{head} {fix}")
}

/// Scan `root` (the review root) for nested git repositories with changes that are not among the
/// declared companions. `configured` are the server's companions, matched to a found repository by
/// physical root; `declared` are the ones this review serves, which are skipped.
pub fn detect(root: &Path, configured: &[Companion], declared: &[Companion]) -> Detection {
    detect_within(root, configured, declared, Instant::now() + SCAN_BUDGET)
}

/// [`detect`] against an explicit deadline, so a test can exercise an exhausted budget.
fn detect_within(
    root: &Path,
    configured: &[Companion],
    declared: &[Companion],
    deadline: Instant,
) -> Detection {
    let limits = crate::evidence::Limits {
        operation_timeout_ms: GIT_OP_TIMEOUT_MS,
        ..crate::evidence::Limits::default()
    };
    let mut detection = Detection::default();
    let candidates = match enumerate(root, &limits, &mut detection.incomplete) {
        Some(c) => c,
        None => return detection,
    };
    // The review root's own linked worktrees (`.claude/worktrees/*`, say) are the same repository,
    // not a nested one.
    let worktrees: Vec<PathBuf> = crate::evidence::list_worktrees(root, &limits)
        .map(|out| {
            out.lines()
                .filter_map(|l| l.strip_prefix("worktree "))
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let same =
        |a: &Path, b: &Path| crate::reviewer::is_within(a, b) && crate::reviewer::is_within(b, a);
    for rel in candidates {
        if Instant::now() >= deadline {
            detection.incomplete.get_or_insert_with(|| {
                format!("it ran out of its {}s time budget", SCAN_BUDGET.as_secs())
            });
            break;
        }
        let abs = root.join(rel.trim_end_matches('/'));
        let physical = std::fs::canonicalize(&abs)
            .map(crate::config::normalize_dir)
            .unwrap_or_else(|_| abs.clone());
        if worktrees
            .iter()
            .any(|w| same(w, &physical) || same(w, &abs))
        {
            continue;
        }
        if declared.iter().any(|c| same(&c.root, &physical)) {
            continue;
        }
        let Some(state) = state_of(&abs, &limits, deadline) else {
            continue;
        };
        let kind = if let Some(c) = configured.iter().find(|c| same(&c.root, &physical)) {
            Kind::ConfiguredUndeclared(c.name.clone())
        } else {
            match crate::evidence::ignores_untracked(root, rel.trim_end_matches('/'), &limits) {
                Ok(false) => Kind::NotIgnored,
                // An unanswerable ignore check reads as ignored: the remediation that follows
                // (configure it as a companion) is the right first step either way, and the
                // companion's own startup check will say if the ignore rule is missing.
                _ => Kind::Ignored,
            }
        };
        detection.unserved.push(Unserved { rel, kind, state });
    }
    detection
}

/// Whether `dir` is the top level of a git repository: the same test as a companion's `check_root`.
fn is_repository(dir: &Path) -> bool {
    dir.join(".git").try_exists().unwrap_or(false)
}

/// A real directory, not a junction or symlink: the descent never follows a reparse point.
fn is_plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

/// The nested repositories under `root`, as root-relative `dir/` paths. `None` (with `incomplete`
/// set) when the listing itself failed. Each cap that binds sets `incomplete` and stops that part of
/// the scan.
fn enumerate(
    root: &Path,
    limits: &crate::evidence::Limits,
    incomplete: &mut Option<String>,
) -> Option<Vec<String>> {
    let mut listed = match crate::evidence::untracked_directories(root, limits) {
        Ok(dirs) => dirs,
        Err(e) => {
            *incomplete = Some(format!(
                "listing untracked directories failed: {}",
                e.message
            ));
            return None;
        }
    };
    if listed.len() > MAX_LISTED_DIRS {
        listed.truncate(MAX_LISTED_DIRS);
        *incomplete = Some(format!("more than {MAX_LISTED_DIRS} untracked directories"));
    }
    let mut found: Vec<String> = Vec::new();
    let mut inspected = 0usize;
    // The first cap to bind is the one reported.
    let stop = |why: String, incomplete: &mut Option<String>| {
        incomplete.get_or_insert(why);
    };
    'outer: for rel in listed {
        let dir = root.join(rel.trim_end_matches('/'));
        inspected += 1;
        if inspected > MAX_INSPECTED_DIRS {
            stop(
                format!("more than {MAX_INSPECTED_DIRS} directories inspected"),
                incomplete,
            );
            break;
        }
        if is_repository(&dir) {
            found.push(rel.clone());
        }
        // One level down, whether or not `dir` is itself a repository (plan r2 f8).
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                stop(format!("`{rel}` could not be read: {e}"), incomplete);
                continue;
            }
        };
        // Counted before an entry's error is looked at, so unreadable entries use up the cap too.
        for (n, entry) in entries.enumerate() {
            if n >= MAX_CHILDREN_PER_DIR {
                stop(
                    format!("more than {MAX_CHILDREN_PER_DIR} entries in `{rel}`"),
                    incomplete,
                );
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    stop(
                        format!("an entry of `{rel}` could not be read: {e}"),
                        incomplete,
                    );
                    continue;
                }
            };
            let child = entry.path();
            if entry.file_name() == ".git" || !is_plain_dir(&child) {
                continue;
            }
            inspected += 1;
            if inspected > MAX_INSPECTED_DIRS {
                stop(
                    format!("more than {MAX_INSPECTED_DIRS} directories inspected"),
                    incomplete,
                );
                break 'outer;
            }
            if is_repository(&child) {
                found.push(format!("{rel}{}/", entry.file_name().to_string_lossy()));
            }
        }
        if found.len() > MAX_CANDIDATES {
            found.truncate(MAX_CANDIDATES);
            stop(
                format!("more than {MAX_CANDIDATES} nested repositories"),
                incomplete,
            );
            break;
        }
    }
    Some(found)
}

/// The state of the nested repository at `dir`, or `None` when it is clean and on its fork point.
///
/// Only the **default branch** (`branch_base`, the same base a review's canonical diff uses) can
/// prove a repository clean. `@{upstream}` is a fallback that can only prove it *changed*: under the
/// push-to-same-name workflow the upstream is the branch's own remote copy, so a branch level with it
/// may still carry committed work past its real base (the reason `branch_base` never uses it). A clean
/// working tree level with its upstream is therefore *unknown*, not clean.
fn state_of(dir: &Path, limits: &crate::evidence::Limits, deadline: Instant) -> Option<State> {
    let out_of_time = || {
        (Instant::now() >= deadline).then(|| {
            State::Unknown(format!(
                "the {}s detection time budget ran out before it was examined",
                SCAN_BUDGET.as_secs()
            ))
        })
    };
    let failed = |what: &str, e: crate::evidence::EvidenceError| {
        Some(State::Unknown(format!("{what} failed: {}", e.message)))
    };
    // A `.git` git cannot use makes it discover the enclosing repository instead, and every answer
    // below would then describe the review root rather than `dir`.
    if let Some(late) = out_of_time() {
        return Some(late);
    }
    match crate::evidence::show_toplevel(dir, limits) {
        Ok(top)
            if crate::reviewer::is_within(&top, dir) && crate::reviewer::is_within(dir, &top) => {}
        Ok(top) => {
            return Some(State::Unknown(format!(
                "git does not recognise it as a repository (it resolves to {})",
                top.display()
            )))
        }
        Err(e) => return failed("`git rev-parse --show-toplevel`", e),
    }
    if let Some(late) = out_of_time() {
        return Some(late);
    }
    let modified = match crate::evidence::status_entry_count(dir, limits) {
        Ok(n) => n,
        Err(e) => return failed("`git status`", e),
    };
    if let Some(late) = out_of_time() {
        return Some(late);
    }
    let head = match crate::evidence::resolve_head(dir, limits) {
        Ok(head) => head,
        Err(e) => return failed("resolving its HEAD", e),
    };
    if let Some(late) = out_of_time() {
        return Some(late);
    }
    // `Some(ahead)` only when the answer can be trusted in that direction (see above).
    let ahead = match crate::evidence::branch_base(dir, limits) {
        Ok((base, _)) => head.as_ref().map(|head| head != &base),
        Err(e) if e.code == "branch_base_unresolved" => {
            if let Some(late) = out_of_time() {
                return Some(late);
            }
            match crate::evidence::upstream_fork_point(dir, limits) {
                Ok(Some(base)) if head.as_ref().is_some_and(|head| head != &base) => Some(true),
                Ok(_) => None,
                Err(e) => return failed("resolving its upstream", e),
            }
        }
        Err(e) => return failed("resolving its fork point", e),
    };
    match (modified, ahead) {
        (0, Some(false)) => None,
        (0, None) => Some(State::Unknown(
            "no default branch (`refs/remotes/origin/HEAD`, `main` or `master`) resolved to \
             compare its branch against"
                .to_string(),
        )),
        (modified, ahead) => Some(State::Changed { modified, ahead }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A one-commit repository at `dir` whose `refs/remotes/origin/main` is that commit, so its fork
    /// point resolves and it starts out clean and on base.
    fn repo_on_base(dir: &Path) {
        crate::companion::tests::init_repo(dir);
        git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    fn commit(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", file]);
    }

    struct Layout {
        _dir: crate::testutil::TempDir,
        primary: PathBuf,
        engine: PathBuf,
    }

    /// `primary` with `engine/` nested and ignored, both clean and on base.
    fn layout(tag: &str) -> Layout {
        let dir = crate::testutil::temp_dir(tag);
        let base = crate::config::normalize_dir(dir.as_path().to_path_buf());
        let primary = base.join("primary");
        repo_on_base(&primary);
        std::fs::write(primary.join(".gitignore"), "engine/\nvendor/\n").unwrap();
        git(&primary, &["add", ".gitignore"]);
        git(&primary, &["commit", "-q", "-m", "ignore"]);
        let engine = primary.join("engine");
        repo_on_base(&engine);
        Layout {
            _dir: dir,
            primary,
            engine,
        }
    }

    #[test]
    fn a_clean_nested_repository_on_base_is_not_reported() {
        let l = layout("nested-clean");
        let d = detect(&l.primary, &[], &[]);
        assert_eq!(d, Detection::default());
        assert!(d.warnings().is_empty());
    }

    #[test]
    fn uncommitted_and_committed_changes_are_reported_with_the_fix() {
        let l = layout("nested-changed");
        std::fs::write(l.engine.join("f.txt"), "edited").unwrap();
        let d = detect(&l.primary, &[], &[]);
        assert_eq!(
            d.unserved,
            vec![Unserved {
                rel: "engine/".into(),
                kind: Kind::Ignored,
                state: State::Changed {
                    modified: 1,
                    ahead: Some(false)
                },
            }]
        );
        let w = d.warnings();
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("`engine/`") && w[0].contains("--companion <name>=engine"),
            "{}",
            w[0]
        );

        // The issue's case: committed on a branch, working tree clean.
        git(&l.engine, &["checkout", "-q", "--", "f.txt"]);
        commit(&l.engine, "fix.cpp");
        let d = detect(&l.primary, &[], &[]);
        assert_eq!(
            d.unserved[0].state,
            State::Changed {
                modified: 0,
                ahead: Some(true)
            }
        );
        assert!(d.warnings()[0].contains("commits beyond its fork point"));
    }

    #[test]
    fn upstream_is_the_fallback_base_and_no_base_is_unknown() {
        let l = layout("nested-upstream");
        git(&l.engine, &["update-ref", "-d", "refs/remotes/origin/main"]);
        // No origin default branch and no upstream: clean, but unknown.
        let d = detect(&l.primary, &[], &[]);
        assert!(
            matches!(&d.unserved[0].state, State::Unknown(why) if why.contains("no default branch")),
            "{d:?}"
        );
        // An upstream on another remote name, at HEAD: still unknown, because an upstream can be the
        // branch's own remote copy and so cannot prove it has no committed work (review f1).
        git(
            &l.engine,
            &["remote", "add", "fork", "https://example.invalid/fork.git"],
        );
        git(&l.engine, &["update-ref", "refs/remotes/fork/main", "HEAD"]);
        git(&l.engine, &["branch", "-q", "--set-upstream-to=fork/main"]);
        assert!(matches!(
            &detect(&l.primary, &[], &[]).unserved[0].state,
            State::Unknown(_)
        ));
        // A commit past the upstream: changed.
        commit(&l.engine, "fix.cpp");
        assert_eq!(
            detect(&l.primary, &[], &[]).unserved[0].state,
            State::Changed {
                modified: 0,
                ahead: Some(true)
            }
        );
    }

    #[test]
    fn declared_companions_are_skipped_and_configured_ones_are_matched_by_root() {
        let l = layout("nested-configured");
        commit(&l.engine, "fix.cpp");
        let engine = Companion {
            name: "eng".into(),
            root: l.engine.clone(),
        };
        assert!(detect(
            &l.primary,
            std::slice::from_ref(&engine),
            std::slice::from_ref(&engine)
        )
        .unserved
        .is_empty());
        let d = detect(&l.primary, std::slice::from_ref(&engine), &[]);
        assert_eq!(d.unserved[0].kind, Kind::ConfiguredUndeclared("eng".into()));
        assert!(d.warnings()[0].contains("companions: [\"eng\"]"));
        // A configured companion that shares the basename but lives elsewhere is not suggested.
        let elsewhere = Companion {
            name: "engine".into(),
            root: l.primary.parent().unwrap().join("engine"),
        };
        let d = detect(&l.primary, &[elsewhere], &[]);
        assert_eq!(d.unserved[0].kind, Kind::Ignored);
        assert!(!d.warnings()[0].contains("companions: [\"engine\"]"));
    }

    #[test]
    fn a_nested_repository_the_root_does_not_ignore_says_so() {
        let l = layout("nested-not-ignored");
        let tools = l.primary.join("tools");
        repo_on_base(&tools);
        commit(&tools, "t.txt");
        let d = detect(&l.primary, &[], &[]);
        let u = d
            .unserved
            .iter()
            .find(|u| u.rel == "tools/")
            .expect("found");
        assert_eq!(u.kind, Kind::NotIgnored);
        assert!(d
            .warnings()
            .iter()
            .any(|w| w.contains("does not ignore it")));
    }

    #[test]
    fn one_level_below_an_ignored_directory_or_a_nested_repository_is_found() {
        let l = layout("nested-descent");
        // Under an ignored plain directory.
        let sdk = l.primary.join("vendor").join("sdk");
        repo_on_base(&sdk);
        commit(&sdk, "s.txt");
        // Under a nested repository.
        let inner = l.engine.join("third_party");
        repo_on_base(&inner);
        // Locally excluded, so `engine/` itself stays clean.
        std::fs::write(
            l.engine.join(".git").join("info").join("exclude"),
            "third_party/\n",
        )
        .unwrap();
        commit(&inner, "i.txt");
        let d = detect(&l.primary, &[], &[]);
        let rels: Vec<&str> = d.unserved.iter().map(|u| u.rel.as_str()).collect();
        assert!(rels.contains(&"vendor/sdk/"), "{rels:?}");
        assert!(rels.contains(&"engine/third_party/"), "{rels:?}");
        // `engine/` itself is clean and on base, so only its child is reported.
        assert!(!rels.contains(&"engine/"), "{rels:?}");
    }

    #[test]
    fn the_roots_own_linked_worktrees_are_not_nested_repositories() {
        let l = layout("nested-worktree");
        std::fs::write(l.primary.join(".gitignore"), "engine/\nwt/\n").unwrap();
        git(
            &l.primary,
            &["worktree", "add", "-q", "-b", "side", "wt/side"],
        );
        commit(&l.primary.join("wt").join("side"), "w.txt");
        let d = detect(&l.primary, &[], &[]);
        assert!(
            d.unserved.iter().all(|u| !u.rel.starts_with("wt/")),
            "{d:?}"
        );
    }

    #[test]
    fn a_failed_listing_is_an_incomplete_warning_not_an_error() {
        let dir = crate::testutil::temp_dir("nested-not-a-repo");
        let d = detect(dir.as_path(), &[], &[]);
        assert!(d.unserved.is_empty());
        assert!(d.incomplete.is_some());
        assert!(d.warnings()[0].contains("stopped early"));
    }

    #[test]
    fn a_failing_git_call_for_a_candidate_is_unknown_with_its_cause() {
        let l = layout("nested-broken");
        // A `.git` directory git cannot use.
        let broken = l.primary.join("vendor").join("broken");
        std::fs::create_dir_all(broken.join(".git")).unwrap();
        std::fs::write(broken.join("a.txt"), "a").unwrap();
        let d = detect(&l.primary, &[], &[]);
        let u = d
            .unserved
            .iter()
            .find(|u| u.rel == "vendor/broken/")
            .expect("found");
        assert!(
            matches!(&u.state, State::Unknown(why) if why.contains("does not recognise")),
            "{u:?}"
        );
    }

    /// Review f2: the scan runs before the start response, so an exhausted time budget stops it with
    /// the incomplete warning rather than holding the response.
    #[test]
    fn an_exhausted_time_budget_stops_the_scan_with_a_warning() {
        let l = layout("nested-deadline");
        commit(&l.engine, "fix.cpp");
        let d = detect_within(&l.primary, &[], &[], Instant::now());
        assert!(d.unserved.is_empty(), "{d:?}");
        assert!(
            d.incomplete
                .as_deref()
                .is_some_and(|w| w.contains("time budget")),
            "{d:?}"
        );
    }

    #[test]
    fn the_children_cap_marks_detection_incomplete() {
        let l = layout("nested-cap");
        let vendor = l.primary.join("vendor");
        for i in 0..=MAX_CHILDREN_PER_DIR {
            std::fs::create_dir_all(vendor.join(format!("d{i}"))).unwrap();
            std::fs::write(vendor.join(format!("d{i}")).join("x"), "x").unwrap();
        }
        let d = detect(&l.primary, &[], &[]);
        assert!(
            d.incomplete
                .as_deref()
                .is_some_and(|w| w.contains("entries in `vendor/`")),
            "{d:?}"
        );
    }
}
