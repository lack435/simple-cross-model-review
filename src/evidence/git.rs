use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{EvidenceError, Limits};

/// Why a Git command produced no output, classified finely enough for the enumeration's fallback
/// rule (issue #86).
///
/// `run`'s flattened `provider_failed` cannot carry this: it is the answer for a timeout, a
/// non-zero exit, a spawn failure and a corrupt index alike, and the enumeration must treat those
/// differently. Reading "git did not answer" as "there is no git here" sends a repository with a
/// large ignored tree into the very filesystem walk this change exists to avoid.
#[derive(Debug)]
pub enum GitFailure {
    /// No `git` binary on PATH. Decided before any spawn, so it is the one condition that is
    /// *verified* rather than inferred from an exit code — and the only one for which the
    /// filesystem walk is the better answer.
    NoGit,
    /// Timed out, or refused before spawning because the request budget was already gone. Never
    /// retried: the budget a retry would spend is the budget that just ran out.
    OutOfTime(EvidenceError),
    /// The request was cancelled. Kept apart from `OutOfTime` because it is not a degraded
    /// observation to be salvaged into a partial answer — nobody is waiting for one.
    Cancelled(EvidenceError),
    /// Everything else — a non-zero exit, a spawn or observe failure, or output `run` refused as
    /// truncated, incomplete or undecodable.
    Failed(EvidenceError),
}

impl GitFailure {
    pub fn into_evidence(self) -> EvidenceError {
        match self {
            Self::NoGit => EvidenceError::new("provider_unavailable", "git is not on PATH"),
            Self::OutOfTime(e) | Self::Failed(e) | Self::Cancelled(e) => e,
        }
    }
}

/// The file set a scan will consider, and whether it is the whole of what was asked for.
///
/// `complete` rather than a bare `Vec` because several outcomes produce a short-but-useful list
/// that an `Err` would throw away: the `max_files` ceiling and a second command that failed while
/// the first succeeded. It is *not* how truncated output is reported — see `run_classified`.
#[derive(Debug)]
pub struct Enumeration {
    pub paths: Vec<String>,
    pub complete: bool,
}

/// Ask Git which files are reviewable, instead of walking a working root that may hold hundreds of
/// thousands of ignored files (issue #86).
///
/// Two fixed commands, in this order, unioned:
///
/// ```text
/// git … ls-files -z --others --exclude-standard      # 1: also the probe
/// git … ls-files -z --cached --recurse-submodules    # 2: retried once as plain --cached
/// ```
///
/// Two rather than one because `--cached --others` lists a submodule as the bare gitlink path and
/// never its contents, while the filesystem walk this replaces descends into it — and Git refuses
/// `--recurse-submodules` together with `--others`. Command 1 uses only flags as old as `ls-files`
/// itself, which is what makes it the probe: its `NoGit` is the sole route to the filesystem
/// fallback, and every other failure is an error rather than a licence to walk the tree.
///
/// What this does **not** honour, deliberately: a `core.excludesFile` set in the user's global Git
/// config, because this runner disables user and system config. Per-directory `.gitignore`,
/// `$GIT_DIR/info/exclude` and the default `~/.config/git/ignore` all still apply. Relaxing that
/// for one of the two call sites (capture time and service time) would give two different file
/// sets one `Git` stamp method and produce false drift, so both keep the isolated environment.
pub fn reviewable_paths(
    root: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<Enumeration, GitFailure> {
    enumerate(limits, |args| {
        run_classified(root, args, limits, cancel, received_at)
    })
}

/// `reviewable_paths` with the command runner injected: a seam, not a setting. Production always
/// passes `run_classified`; it exists so the aggregate rule and the parsing can be driven through
/// every outcome without a Git repository, including the spawn and budget failures that have no
/// exit code to fake.
fn enumerate(
    limits: &Limits,
    mut run: impl FnMut(&[&str]) -> Result<String, GitFailure>,
) -> Result<Enumeration, GitFailure> {
    let untracked = run(&["ls-files", "-z", "--others", "--exclude-standard"])?;
    let mut complete = true;
    let tracked = match run(&["ls-files", "-z", "--cached", "--recurse-submodules"]) {
        Ok(text) => Some(text),
        // A cancelled request is not a partial answer to salvage: nobody is waiting for one.
        Err(cancelled @ GitFailure::Cancelled(_)) => return Err(cancelled),
        // Out of time, or the binary vanished between the two calls: keep what command 1 gave us
        // rather than spending a retry on the budget that just ran out.
        Err(GitFailure::OutOfTime(_)) | Err(GitFailure::NoGit) => {
            complete = false;
            None
        }
        // A non-zero exit is where an unsupported `--recurse-submodules` lands (Git before 2.11).
        // One retry with a flag that predates it, so an old Git costs the submodule contents
        // rather than the entire tracked file set.
        Err(GitFailure::Failed(_)) => {
            complete = false;
            match run(&["ls-files", "-z", "--cached"]) {
                Ok(text) => Some(text),
                // `.ok()` here would have swallowed a cancellation and handed back a partial
                // enumeration as though it were an answer.
                Err(cancelled @ GitFailure::Cancelled(_)) => return Err(cancelled),
                Err(_) => None,
            }
        }
    };

    let mut paths = BTreeSet::new();
    for text in [Some(untracked), tracked].into_iter().flatten() {
        for entry in text.split('\0') {
            if !entry.is_empty() {
                paths.insert(entry.to_string());
            }
        }
    }
    let max = limits.max_files as usize;
    if paths.len() > max {
        complete = false;
    }
    Ok(Enumeration {
        paths: paths.into_iter().take(max).collect(),
        complete,
    })
}

pub fn history(
    root: &Path,
    path: &str,
    before: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<(Vec<Value>, bool), EvidenceError> {
    let mut owned = vec![
        "log".to_string(),
        "--no-decorate".to_string(),
        "--date=iso-strict".to_string(),
        "--pretty=format:%H%x1f%aI%x1f%an%x1f%s".to_string(),
        format!("-n{}", limits.max_history),
    ];
    if !before.is_empty() {
        owned.push("--skip=1".to_string());
        owned.push(before.to_string());
    }
    if !path.is_empty() {
        owned.push("--".into());
        owned.push(path.to_string());
    }
    let output = run(root, &owned, limits, cancel, received_at)?;
    let mut commits = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\x1f').collect();
        if fields.len() != 4 {
            continue;
        }
        commits.push(
            json!({"id":fields[0],"authored":fields[1],"author":fields[2],"subject":fields[3]}),
        );
    }
    let complete = commits.len() < limits.max_history as usize;
    Ok((commits, complete))
}

pub fn revision(
    root: &Path,
    id: &str,
    path: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let mut args = vec![
        "show".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--format=fuller".to_string(),
        "--stat".to_string(),
        "--patch".to_string(),
        id.to_string(),
    ];
    if !path.is_empty() {
        args.push("--".into());
        args.push(path.to_string());
    }
    run(root, &args, limits, cancel, received_at)
}

/// Diff the working tree (or a commit range) against a resolved base, live.
///
/// `spec` is the already-resolved endpoint list the handler built — `["<base_id>"]` for the
/// working tree against a commit, `["--cached", "<base_id>"]` for the index, `["<a>", "<b>"]` for a
/// commit-to-commit range. Every token is either a fixed flag this module wrote or a full object id
/// the handler validated with `valid_object_id`; no symbolic ref or raw model input reaches here, so
/// the closed `--diff`-style hardening is preserved. Untracked files are *not* part of this output —
/// `git diff` never lists them — and are composed in by the caller from `reviewable_paths`; this
/// function is the tracked half only.
pub fn diff(
    root: &Path,
    spec: &[&str],
    path: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let mut args = vec![
        "diff".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
    ];
    args.extend(spec.iter().map(|s| s.to_string()));
    if !path.is_empty() {
        args.push("--".into());
        args.push(path.to_string());
    }
    run(root, &args, limits, cancel, received_at)
}

/// The untracked, non-ignored files in the working tree (`git ls-files --others
/// --exclude-standard`), NUL-separated so a newline in a path cannot split an entry. `complete` is
/// false when the list hit `max_files`. These are what `git diff` never shows and the handler
/// composes into the working-tree diff (f2).
pub fn untracked_paths(
    root: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<(Vec<String>, bool), EvidenceError> {
    let args = vec![
        "ls-files".to_string(),
        "-z".to_string(),
        "--others".to_string(),
        "--exclude-standard".to_string(),
    ];
    let out = run(root, &args, limits, cancel, received_at)?;
    let mut paths: Vec<String> = out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let max = limits.max_files as usize;
    let complete = paths.len() <= max;
    paths.truncate(max);
    Ok((paths, complete))
}

/// `git diff --numstat` counts for a resolved spec: (files, insertions, deletions). A binary file
/// counts toward `files` but contributes no line counts (git renders it as `-\t-`). Feeds the
/// serve-record so the caller-facing `captured:` line reports what was served (mechanism 5).
pub fn numstat(
    root: &Path,
    spec: &[&str],
    path: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<(usize, usize, usize), EvidenceError> {
    let mut args = vec![
        "diff".to_string(),
        "--numstat".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
    ];
    args.extend(spec.iter().map(|s| s.to_string()));
    if !path.is_empty() {
        args.push("--".into());
        args.push(path.to_string());
    }
    let out = run(root, &args, limits, cancel, received_at)?;
    let (mut files, mut insertions, mut deletions) = (0usize, 0usize, 0usize);
    for line in out.lines() {
        let mut fields = line.split('\t');
        let added = fields.next().unwrap_or("");
        let deleted = fields.next().unwrap_or("");
        if fields.next().is_none() {
            continue; // not a numstat row (no path field)
        }
        files += 1;
        insertions += added.parse::<usize>().unwrap_or(0);
        deletions += deleted.parse::<usize>().unwrap_or(0);
    }
    Ok((files, insertions, deletions))
}

/// Resolve a revision to a full commit object id, or `None` if it does not name one.
///
/// Used to pin the endpoints of the canonical diff — the branch's upstream and the merge-base — to
/// fixed ids that cannot move mid-review, and to peel a ref to a commit. `rev` is trusted server
/// input (a configured/detected branch name or `@{upstream}`), never a model-supplied token, and it
/// is passed after `--end-of-options` so it can never be read as a flag.
pub fn resolve_commit(
    root: &Path,
    rev: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<Option<String>, EvidenceError> {
    let args = vec![
        "rev-parse".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        "--end-of-options".to_string(),
        format!("{rev}^{{commit}}"),
    ];
    // `--verify --quiet` exits non-zero with empty stdout when the rev does not resolve; `run`
    // surfaces that as a `provider_failed`, so a resolvable-but-absent ref and a genuine git error
    // are told apart by the empty-vs-populated result rather than by the exit code alone.
    match run(root, &args, limits, cancel, received_at) {
        Ok(out) => {
            let id = out.trim().to_string();
            Ok(if id.is_empty() { None } else { Some(id) })
        }
        Err(_) => Ok(None),
    }
}

/// The merge-base of two commits — the branch's fork point when called as `merge_base(HEAD,
/// upstream)`. `None` when the two share no history. Both arguments are full object ids the handler
/// already resolved and validated.
pub fn merge_base(
    root: &Path,
    a: &str,
    b: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<Option<String>, EvidenceError> {
    // No `--end-of-options`: `git merge-base` does not accept it, and would fail the whole
    // operation as "no merge base" (f2). It is unnecessary here anyway — both operands are full
    // hex object ids the caller validated, so neither can be read as an option or a ref.
    let args = vec!["merge-base".to_string(), a.to_string(), b.to_string()];
    match run(root, &args, limits, cancel, received_at) {
        Ok(out) => {
            let id = out.trim().to_string();
            Ok(if id.is_empty() { None } else { Some(id) })
        }
        // `merge-base` exits 1 with no output when there is no common ancestor; that is a `None`,
        // not an error, but any other failure (a bad object, a broken repo) still propagates.
        Err(_) => Ok(None),
    }
}

/// Run one bounded Git command, against the **request's** deadline rather than a fresh per-command
/// one.
///
/// `received_at` is the request's own receipt instant, not a duration snapshot, and the child's
/// timeout is computed from it at the last possible moment — after the PATH lookup and the command
/// setup, which are themselves filesystem work that a snapshot taken at the call site would have
/// silently spent (round-1 finding f2).
///
/// Two subtractions, both load-bearing:
///
/// - **The drain grace.** `reviewer::run` does not return when its timeout fires; it then collects
///   the child's pipes for up to `DRAIN_GRACE`. A budget that ignored that could answer a full ten
///   seconds after the deadline it claimed to honour — past the client ceiling, which is the whole
///   failure this change exists to prevent.
/// - **The configured per-operation timeout**, which still binds when it is the tighter of the two.
///
/// What is left is what the child may actually spend. If that is nothing, refuse rather than start
/// a process whose output nobody is waiting for.
fn run(
    root: &Path,
    args: &[String],
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_classified(root, &borrowed, limits, cancel, received_at).map_err(GitFailure::into_evidence)
}

/// `run`'s body, with the failure kept classified rather than flattened to one code. `run` is the
/// thin wrapper that discards the classification for callers which do not act on it.
fn run_classified(
    root: &Path,
    args: &[&str],
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, GitFailure> {
    let bin = crate::reviewer::on_path("git").ok_or(GitFailure::NoGit)?;
    let mut command = Command::new(bin);
    command
        .arg("--no-pager")
        .args(["-c", "core.fsmonitor="])
        .args(["-c", "core.hooksPath=NUL"])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "NUL")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "")
        .env("PAGER", "");
    // Computed here, immediately before the spawn, so the lookup and setup above are charged to the
    // request rather than to the child.
    let budget = super::core::child_budget(received_at)
        .min(Duration::from_millis(limits.operation_timeout_ms));
    if budget.is_zero() {
        return Err(GitFailure::OutOfTime(EvidenceError::new(
            "deadline_exceeded",
            "not enough of the evidence request budget remained to run a Git command and still              answer inside the client's per-call ceiling",
        )));
    }
    // A spawn or observe failure arrives as an `io::Error` with no exit code, no `timed_out` and
    // no `cancelled` to read, so it is classified here rather than inferred downstream.
    let output = crate::reviewer::run(command, "", budget, cancel).map_err(|e| {
        GitFailure::Failed(EvidenceError::new(
            "provider_failed",
            format!("could not run git: {e}"),
        ))
    })?;
    if output.cancelled {
        return Err(GitFailure::Cancelled(EvidenceError::new(
            "cancelled",
            "Git evidence operation was cancelled",
        )));
    }
    if output.timed_out {
        return Err(GitFailure::OutOfTime(EvidenceError::new(
            "deadline_exceeded",
            "the Git command exceeded the evidence request budget",
        )));
    }
    if !output.success {
        return Err(GitFailure::Failed(EvidenceError::new(
            "provider_failed",
            output.diagnostics(),
        )));
    }
    if output.stdout_truncated || output.stdout_incomplete || output.stdout_lossy {
        return Err(GitFailure::Failed(EvidenceError::new(
            "limit_exceeded",
            "git output was truncated, incomplete, or not valid UTF-8",
        )));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake runner: hands out one scripted outcome per call, in order.
    fn scripted(
        outcomes: Vec<Result<String, GitFailure>>,
    ) -> impl FnMut(&[&str]) -> Result<String, GitFailure> {
        let calls = RefCell::new(outcomes.into_iter());
        move |_args| {
            calls
                .borrow_mut()
                .next()
                .unwrap_or_else(|| panic!("the enumeration ran more commands than were scripted"))
        }
    }

    fn failed() -> GitFailure {
        GitFailure::Failed(EvidenceError::new("provider_failed", "exit 128"))
    }

    fn out_of_time() -> GitFailure {
        GitFailure::OutOfTime(EvidenceError::new("deadline_exceeded", "took too long"))
    }

    #[test]
    fn the_enumeration_unions_both_commands_and_drops_the_empty_tail() {
        let enumeration = enumerate(
            &Limits::default(),
            scripted(vec![
                Ok("untracked.txt\0".into()),
                Ok("a.txt\0sub/b.txt\0a.txt\0".into()),
            ]),
        )
        .unwrap();
        assert_eq!(enumeration.paths, ["a.txt", "sub/b.txt", "untracked.txt"]);
        assert!(enumeration.complete);
    }

    #[test]
    fn the_file_budget_truncates_the_enumeration_rather_than_failing_it() {
        let limits = Limits {
            max_files: 2,
            ..Default::default()
        };
        let enumeration = enumerate(
            &limits,
            scripted(vec![Ok(String::new()), Ok("a\0b\0c\0d\0".into())]),
        )
        .unwrap();
        assert_eq!(enumeration.paths, ["a", "b"]);
        assert!(
            !enumeration.complete,
            "a truncated list is not the whole tree"
        );
    }

    // The probe is the only command whose failure decides anything for the caller, so its three
    // outcomes must stay distinguishable all the way out of the enumeration (issue #86, f9).
    #[test]
    fn a_probe_failure_keeps_its_classification() {
        for (failure, expect_no_git) in [
            (GitFailure::NoGit, true),
            (failed(), false),
            (out_of_time(), false),
        ] {
            let error = enumerate(&Limits::default(), scripted(vec![Err(failure)])).unwrap_err();
            assert_eq!(matches!(error, GitFailure::NoGit), expect_no_git);
        }
    }

    // Git before 2.11 rejects `--recurse-submodules`. Without the retry that costs the whole
    // tracked file set; with it, only the submodule contents.
    #[test]
    fn a_rejected_recurse_flag_is_retried_as_plain_cached() {
        let enumeration = enumerate(
            &Limits::default(),
            scripted(vec![
                Ok("untracked.txt\0".into()),
                Err(failed()),
                Ok("a.txt\0".into()),
            ]),
        )
        .unwrap();
        assert_eq!(enumeration.paths, ["a.txt", "untracked.txt"]);
        assert!(
            !enumeration.complete,
            "the retry cannot see submodule contents, so the answer is not whole"
        );
    }

    #[test]
    fn a_second_command_that_ran_out_of_time_is_not_retried() {
        // Only two outcomes are scripted: a third call would panic, which is the assertion.
        let enumeration = enumerate(
            &Limits::default(),
            scripted(vec![Ok("untracked.txt\0".into()), Err(out_of_time())]),
        )
        .unwrap();
        assert_eq!(enumeration.paths, ["untracked.txt"]);
        assert!(!enumeration.complete);
    }

    // Cancellation is not a degraded answer to salvage. It has to survive both the second command
    // and its retry -- the retry used `.ok()`, which swallowed it.
    #[test]
    fn a_cancelled_command_is_never_salvaged_into_a_partial_enumeration() {
        let cancelled = || GitFailure::Cancelled(EvidenceError::new("cancelled", "stopped"));
        for script in [
            vec![Err(cancelled())],
            vec![Ok("untracked.txt\0".into()), Err(cancelled())],
            // The retry itself cancelled, after a `Failed` second command.
            vec![
                Ok("untracked.txt\0".into()),
                Err(failed()),
                Err(cancelled()),
            ],
        ] {
            let error = enumerate(&Limits::default(), scripted(script)).unwrap_err();
            assert!(
                matches!(error, GitFailure::Cancelled(_)),
                "cancellation was converted into an answer"
            );
        }
    }

    #[test]
    fn both_tracked_attempts_failing_keeps_the_untracked_half() {
        let enumeration = enumerate(
            &Limits::default(),
            scripted(vec![
                Ok("untracked.txt\0".into()),
                Err(failed()),
                Err(failed()),
            ]),
        )
        .unwrap();
        assert_eq!(enumeration.paths, ["untracked.txt"]);
        assert!(!enumeration.complete);
    }

    fn git(root: &Path, args: &[&str]) -> bool {
        let Some(bin) = crate::reviewer::on_path("git") else {
            return false;
        };
        Command::new(bin)
            .args(["-c", "user.email=test@example.invalid"])
            .args(["-c", "user.name=test"])
            .args(["-c", "protocol.file.allow=always"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn enumerated(root: &Path) -> Vec<String> {
        let cancel = AtomicBool::new(false);
        reviewable_paths(root, &Limits::default(), &cancel, Instant::now())
            .expect("enumeration failed")
            .paths
    }

    /// Set up a repository, or say why the test is being skipped. Real Git, no model, no network.
    fn repo(name: &str) -> Option<crate::testutil::TempDir> {
        if crate::reviewer::on_path("git").is_none() {
            eprintln!("skipping {name}: git is not on PATH");
            return None;
        }
        let dir = crate::testutil::temp_dir(name);
        if !git(dir.as_path(), &["init", "-q"]) {
            eprintln!("skipping {name}: git init failed");
            return None;
        }
        Some(dir)
    }

    // `--cached` alone would pass half of this. The untracked half is what proves the second
    // command is doing its job, and the ignored half is the point of the change.
    #[test]
    fn the_enumeration_covers_untracked_files_and_skips_ignored_ones() {
        let Some(dir) = repo("evidence-enumerate") else {
            return;
        };
        let root = dir.as_path();
        std::fs::write(root.join(".gitignore"), "ignored.txt\nvendored/\n").unwrap();
        std::fs::write(root.join("tracked.txt"), "tracked").unwrap();
        std::fs::write(root.join("untracked.txt"), "untracked").unwrap();
        std::fs::write(root.join("ignored.txt"), "ignored").unwrap();
        std::fs::create_dir(root.join("vendored")).unwrap();
        std::fs::write(root.join("vendored").join("huge.bin"), "huge").unwrap();
        assert!(git(root, &["add", "tracked.txt", ".gitignore"]));
        assert!(git(root, &["commit", "-qm", "init"]));

        let paths = enumerated(root);
        assert!(paths.contains(&"tracked.txt".to_string()));
        assert!(
            paths.contains(&"untracked.txt".to_string()),
            "an untracked, unignored file is reviewable content: {paths:?}"
        );
        assert!(!paths.contains(&"ignored.txt".to_string()));
        assert!(
            !paths.iter().any(|p| p.starts_with("vendored/")),
            "the ignored tree is what issue #86 is about: {paths:?}"
        );
    }

    // `$GIT_DIR/info/exclude` is one of the standard sources `--exclude-standard` reads, and it
    // still works under this runner's isolated config -- unlike a custom global `core.excludesFile`,
    // which does not, deliberately (see `reviewable_paths`).
    #[test]
    fn repository_local_excludes_are_honoured() {
        let Some(dir) = repo("evidence-exclude") else {
            return;
        };
        let root = dir.as_path();
        std::fs::write(root.join("hidden.txt"), "hidden").unwrap();
        assert!(enumerated(root).contains(&"hidden.txt".to_string()));
        std::fs::write(
            root.join(".git").join("info").join("exclude"),
            "hidden.txt\n",
        )
        .unwrap();
        assert!(!enumerated(root).contains(&"hidden.txt".to_string()));
    }

    // `ls-files --cached --others` reports a submodule as the bare gitlink and never its contents,
    // which would silently drop every submodule file from search and drift.
    #[test]
    fn submodule_contents_are_enumerated_and_the_gitlink_is_not() {
        let Some(outer) = repo("evidence-submodule") else {
            return;
        };
        let sub = outer.as_path().join("sub-origin");
        std::fs::create_dir(&sub).unwrap();
        if !git(&sub, &["init", "-q"]) {
            eprintln!("skipping: could not init the submodule origin");
            return;
        }
        std::fs::write(sub.join("inside.txt"), "inside").unwrap();
        assert!(git(&sub, &["add", "-A"]));
        assert!(git(&sub, &["commit", "-qm", "init"]));

        let super_root = outer.as_path().join("super");
        std::fs::create_dir(&super_root).unwrap();
        if !git(&super_root, &["init", "-q"])
            || !git(
                &super_root,
                &["submodule", "add", "-q", "../sub-origin", "sub"],
            )
        {
            eprintln!("skipping: could not add the submodule");
            return;
        }
        std::fs::write(super_root.join("top.txt"), "top").unwrap();
        assert!(git(&super_root, &["add", "-A"]));
        assert!(git(&super_root, &["commit", "-qm", "init"]));

        let paths = enumerated(&super_root);
        assert!(
            paths.contains(&"sub/inside.txt".to_string()),
            "submodule contents must be covered: {paths:?}"
        );
        assert!(
            !paths.contains(&"sub".to_string()),
            "the bare gitlink is not a file: {paths:?}"
        );
        assert!(paths.contains(&".gitmodules".to_string()));
    }

    #[test]
    fn provider_rejects_missing_git_root_without_a_shell() {
        let root = std::env::temp_dir();
        let cancel = AtomicBool::new(false);
        let result = history(&root, "", "", &Limits::default(), &cancel, Instant::now());
        assert!(result.is_err());
    }
}
