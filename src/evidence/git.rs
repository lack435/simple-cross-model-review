use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{EvidenceError, Limits};

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
    let bin = crate::reviewer::on_path("git")
        .ok_or_else(|| EvidenceError::new("provider_unavailable", "git is not on PATH"))?;
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
        return Err(EvidenceError::new(
            "deadline_exceeded",
            "not enough of the evidence request budget remained to run a Git command and still              answer inside the client's per-call ceiling",
        ));
    }
    let output = crate::reviewer::run(command, "", budget, cancel)
        .map_err(|e| EvidenceError::new("provider_failed", format!("could not run git: {e}")))?;
    if output.cancelled {
        return Err(EvidenceError::new(
            "cancelled",
            "Git evidence operation was cancelled",
        ));
    }
    if !output.success {
        return Err(EvidenceError::new("provider_failed", output.diagnostics()));
    }
    if output.stdout_truncated || output.stdout_incomplete || output.stdout_lossy {
        return Err(EvidenceError::new(
            "limit_exceeded",
            "git output was truncated, incomplete, or not valid UTF-8",
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_rejects_missing_git_root_without_a_shell() {
        let root = std::env::temp_dir();
        let cancel = AtomicBool::new(false);
        let result = history(&root, "", "", &Limits::default(), &cancel, Instant::now());
        assert!(result.is_err());
    }
}
