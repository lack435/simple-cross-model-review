use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{json, Value};

use super::{EvidenceError, Limits};

pub fn history(
    root: &Path,
    path: &str,
    before: &str,
    limits: &Limits,
    cancel: &AtomicBool,
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
    let output = run(root, &owned, limits, cancel)?;
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
    run(root, &args, limits, cancel)
}

fn run(
    root: &Path,
    args: &[String],
    limits: &Limits,
    cancel: &AtomicBool,
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
    let output = crate::reviewer::run(
        command,
        "",
        Duration::from_millis(limits.operation_timeout_ms),
        cancel,
    )
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
        let result = history(&root, "", "", &Limits::default(), &cancel);
        assert!(result.is_err());
    }
}
