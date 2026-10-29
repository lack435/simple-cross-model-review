//! Perforce history, revision and diff for the evidence service (issue #183).
//!
//! Each tool is one fixed, read-only command: `p4 changes`, `p4 describe`, `p4 print`, `p4 diff` or
//! `p4 diff2`. Nothing the reviewer sends becomes a subcommand or a flag: a path is escaped into a
//! file spec under the working root, and a revision is a validated changelist number. Every command
//! runs as the server, user and client the parent's capture resolved ([`PerforceAccess`]), so a
//! reviewer is never served evidence from a different workspace than the change under review.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{EvidenceError, Limits, PerforceAccess};

/// Submitted changelists that touched `path` (the whole working root when empty), newest first, up
/// to `limits.max_history`. `before` lists only changelists older than that number. The second
/// value is whether the list is complete (fewer than the cap came back).
pub fn history(
    root: &Path,
    access: &PerforceAccess,
    path: &str,
    before: Option<u64>,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<(Vec<Value>, bool), EvidenceError> {
    let mut spec = file_spec(root, path)?;
    if let Some(before) = before {
        if before <= 1 {
            return Ok((Vec::new(), true));
        }
        spec.push_str(&format!("@{}", before - 1));
    }
    let max = limits.max_history.to_string();
    // `-t` adds the time; without `-l` each changelist is one line, its description cut to a
    // short subject (the full one is `repository_revision` on that changelist).
    let output = run(
        root,
        access,
        &["changes", "-s", "submitted", "-t", "-m", &max, &spec],
        limits,
        cancel,
        received_at,
    )?;
    let changes = parse_changes(&output);
    let complete = changes.len() < limits.max_history as usize;
    Ok((changes, complete))
}

/// One changelist: with no `path`, its description and diff (`p4 describe -du`, with `-S` for a
/// shelved changelist's shelved diffs); with a `path`, that file's content as of the changelist
/// (`p4 print`), which for a shelved changelist is its shelved revision.
pub fn revision(
    root: &Path,
    access: &PerforceAccess,
    change: u64,
    path: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let state = change_state(root, access, change, limits, cancel, received_at)?;
    if path.is_empty() {
        let number = change.to_string();
        let mut args = vec!["describe", "-du"];
        if state == (ChangeState::Pending { shelved: true }) {
            args.push("-S");
        }
        args.push(&number);
        return run(root, access, &args, limits, cancel, received_at);
    }
    let spec = file_spec(root, path)?;
    if spec.ends_with("\\...") {
        return Err(EvidenceError::new(
            "invalid_arguments",
            "on Perforce, repository_revision with a path reads one file's content; name a file, \
             or omit path to describe the whole changelist",
        ));
    }
    let spec = at_change(
        &spec,
        change,
        state,
        "read this workspace's file with repository_read, or the change under review with \
         repository_change",
    )?;
    run(
        root,
        access,
        &["print", "-q", &spec],
        limits,
        cancel,
        received_at,
    )
}

/// `path` as of changelist `base` against the live workspace (`head` `None`, `p4 diff`), or against
/// the same path as of changelist `head` (`p4 diff2`). Unified, and ignoring line endings for the
/// workspace side as the capture does. Either changelist may be submitted or shelved (see
/// [`at_change`]).
#[allow(clippy::too_many_arguments)]
pub fn diff(
    root: &Path,
    access: &PerforceAccess,
    path: &str,
    base: u64,
    head: Option<u64>,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let spec = file_spec(root, path)?;
    let base_state = change_state(root, access, base, limits, cancel, received_at)?;
    let left = at_change(
        &spec,
        base,
        base_state,
        "use a submitted or shelved changelist as 'base'",
    )?;
    match head {
        // `-f` diffs files that are not opened too: the question is how the workspace differs from
        // that changelist, not only what is checked out.
        None => run(
            root,
            access,
            &["diff", "-du", "-dl", "-f", &left],
            limits,
            cancel,
            received_at,
        ),
        Some(head) => {
            let head_state = change_state(root, access, head, limits, cancel, received_at)?;
            let right = at_change(
                &spec,
                head,
                head_state,
                "omit 'head' to diff 'base' against this workspace, or read the change under \
                 review with repository_change",
            )?;
            run(
                root,
                access,
                &["diff2", "-du", &left, &right],
                limits,
                cancel,
                received_at,
            )
        }
    }
}

/// What the server holds for a changelist number (issue #188).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeState {
    Submitted,
    Pending { shelved: bool },
}

/// Look a changelist up with `p4 describe -s` (`-m 1` keeps a large changelist's file list out).
fn change_state(
    root: &Path,
    access: &PerforceAccess,
    change: u64,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<ChangeState, EvidenceError> {
    let number = change.to_string();
    let text = run(
        root,
        access,
        &["-ztag", "describe", "-s", "-m", "1", &number],
        limits,
        cancel,
        received_at,
    )?;
    parse_change_state(&text).ok_or_else(|| {
        EvidenceError::new(
            "invalid_arguments",
            format!("changelist {change} was not found"),
        )
    })
}

/// The state in `p4 -ztag describe -s` output. The description comes before `status` and may span
/// lines, so only the last `status` line is p4's own; `shelved` is a field after it, present only
/// when the changelist has shelved files.
fn parse_change_state(text: &str) -> Option<ChangeState> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let at = lines.iter().rposition(|l| l.starts_with("... status "))?;
    let shelved = lines[at + 1..].contains(&"... shelved");
    match &lines[at]["... status ".len()..] {
        "submitted" => Some(ChangeState::Submitted),
        "pending" => Some(ChangeState::Pending { shelved }),
        "shelved" => Some(ChangeState::Pending { shelved: true }),
        _ => None,
    }
}

/// `spec` as of changelist `change`. A submitted changelist is a point in history (`@n`) and a
/// shelved one its shelved revision (`@=n`). `@n` on a pending changelist would name the last
/// submitted revision at or below it, silently hiding the pending edits, and one that is not
/// shelved has no revision on the server at all, so it is refused with `instead`.
fn at_change(
    spec: &str,
    change: u64,
    state: ChangeState,
    instead: &str,
) -> Result<String, EvidenceError> {
    match state {
        ChangeState::Submitted => Ok(format!("{spec}@{change}")),
        ChangeState::Pending { shelved: true } => Ok(format!("{spec}@={change}")),
        ChangeState::Pending { shelved: false } => Err(EvidenceError::new(
            "invalid_arguments",
            format!(
                "changelist {change} is pending and not shelved, so its edits exist only in its \
                 workspace; {instead}"
            ),
        )),
    }
}

/// A changelist number argument: decimal digits, greater than zero.
pub fn changelist(raw: &str, field: &str) -> Result<u64, EvidenceError> {
    raw.parse::<u64>()
        .ok()
        .filter(|&n| n > 0 && raw.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| {
            EvidenceError::new(
                "invalid_arguments",
                format!("on Perforce, {field} must be a changelist number"),
            )
        })
}

/// The local-syntax file spec for a working-root-relative `path`, relative to the root (the command
/// runs there): `...` for the whole root, `dir\...` for a directory, the escaped file otherwise.
/// Perforce's reserved characters are escaped so a file name can never act as a revision or a
/// wildcard; `...` cannot be escaped, so a path containing it is refused. Every spec starts with
/// `.\`, so a file name beginning with `-` can never be read as an option (`p4 print -o` writes).
fn file_spec(root: &Path, path: &str) -> Result<String, EvidenceError> {
    if path.is_empty() {
        return Ok(".\\...".to_string());
    }
    if path.contains("...") {
        return Err(EvidenceError::new(
            "invalid_arguments",
            "a Perforce path cannot contain `...`",
        ));
    }
    // As for every evidence path, no link or reparse point along it, so `p4 diff` cannot read a
    // workspace file outside the root. Only the part that exists is checked: a file may exist only
    // in history, and p4 maps a missing tail lexically through the client view.
    let mut current = root.to_path_buf();
    let mut is_dir = false;
    for component in Path::new(path).components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if super::core::metadata_is_reparse(&meta) => {
                return Err(EvidenceError::new(
                    "link_forbidden",
                    "links and reparse points are forbidden",
                ))
            }
            Ok(meta) => is_dir = meta.is_dir(),
            Err(_) => {
                is_dir = false;
                break;
            }
        }
    }
    let escaped = path
        .replace('%', "%25")
        .replace('@', "%40")
        .replace('#', "%23")
        .replace('*', "%2A")
        .replace('/', "\\");
    if is_dir {
        Ok(format!(".\\{}\\...", escaped.trim_end_matches('\\')))
    } else {
        Ok(format!(".\\{escaped}"))
    }
}

/// Parse `p4 changes -t` lines: `Change 12 on 2026/10/29 12:34:56 by user@client 'subject'`.
fn parse_changes(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_end().strip_prefix("Change ")?;
            let (id, rest) = rest.split_once(" on ")?;
            id.parse::<u64>().ok()?;
            let (when, rest) = rest.split_once(" by ")?;
            let (who, subject) = rest.split_once(' ').unwrap_or((rest, ""));
            let author = who.split('@').next().unwrap_or(who);
            let subject = subject.trim();
            let subject = subject.strip_prefix('\'').unwrap_or(subject);
            let subject = subject.strip_suffix('\'').unwrap_or(subject);
            Some(json!({"id": id, "authored": when, "author": author, "subject": subject}))
        })
        .collect()
}

/// Run one read-only `p4` command from the working root as the capture's client.
fn run(
    root: &Path,
    access: &PerforceAccess,
    args: &[&str],
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let bin = crate::reviewer::on_path("p4")
        .ok_or_else(|| EvidenceError::new("provider_unavailable", "p4 is not on PATH"))?;
    let mut command = Command::new(bin);
    // p4 takes its working directory from `PWD` when it is set, ahead of the process's own, so a
    // stale inherited `PWD` would resolve the relative file specs (and `P4CONFIG`) somewhere else
    // entirely. Pin it to the working root, without the `\\?\` prefix p4 does not understand.
    let dir = crate::config::normalize_dir(root.to_path_buf());
    // `-C utf8` and the global `-c <client>` exactly as the capture passes them, and the user the
    // capture resolved: this environment has no `USERNAME`, p4's default user on Windows.
    command
        .args(["-C", "utf8", "-c", &access.client, "-u", &access.user])
        .args(args)
        .current_dir(&dir);
    for (key, value) in &access.env {
        command.env(key, value);
    }
    command.env("PWD", &dir);
    command
        .env_remove("P4DIFF")
        .env_remove("P4MERGE")
        .env_remove("P4DIFFHTML")
        .env_remove("P4EDITOR");
    let budget = super::core::child_budget(received_at)
        .min(Duration::from_millis(limits.operation_timeout_ms));
    if budget.is_zero() {
        return Err(EvidenceError::new(
            "deadline_exceeded",
            "not enough of the evidence request budget remained to run a p4 command and still \
             answer inside the client's per-call ceiling",
        ));
    }
    let output = crate::reviewer::run(command, "", budget, cancel)
        .map_err(|e| EvidenceError::new("provider_failed", format!("could not run p4: {e}")))?;
    if output.cancelled {
        return Err(EvidenceError::new(
            "cancelled",
            "Perforce evidence operation was cancelled",
        ));
    }
    if output.timed_out {
        return Err(EvidenceError::new(
            "deadline_exceeded",
            "the p4 command exceeded the evidence request budget",
        ));
    }
    if !output.success {
        return Err(EvidenceError::new("provider_failed", output.diagnostics()));
    }
    if output.stdout_truncated || output.stdout_incomplete || output.stdout_lossy {
        return Err(EvidenceError::new(
            "limit_exceeded",
            "p4 output was truncated, incomplete, or not valid UTF-8 (a binary file cannot be \
             shown)",
        ));
    }
    Ok(output.stdout.replace("\r\n", "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_lines_parse_into_commit_records() {
        let text = "Change 42 on 2026/10/29 12:34:56 by alice@alice_ws 'Fix the thing'\r\n\
                    Change 7 on 2026/01/02 03:04:05 by bob@ws ''\n\
                    not a change line\n";
        let changes = parse_changes(&text.replace("\r\n", "\n"));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["id"], "42");
        assert_eq!(changes[0]["authored"], "2026/10/29 12:34:56");
        assert_eq!(changes[0]["author"], "alice");
        assert_eq!(changes[0]["subject"], "Fix the thing");
        assert_eq!(changes[1]["subject"], "");
    }

    #[test]
    fn change_state_comes_from_the_last_status_line() {
        let submitted = "... change 1\n... desc first\n... status submitted\n\
                         ... depotFile0 //depot/a.txt\n";
        assert_eq!(parse_change_state(submitted), Some(ChangeState::Submitted));
        let pending = "... change 2\r\n... desc  pending edit\r\n\r\n... status pending\r\n\
                       ... changeType public\r\n... depotFile0 //depot/a.txt\r\n";
        assert_eq!(
            parse_change_state(pending),
            Some(ChangeState::Pending { shelved: false })
        );
        let shelved = "... change 3\n... status pending\n... changeType public\n... shelved \n";
        assert_eq!(
            parse_change_state(shelved),
            Some(ChangeState::Pending { shelved: true })
        );
        // A description cannot pass itself off as the status or the shelved field.
        let spoofed = "... change 4\n... desc x\n... status submitted\n... shelved\n\
                       ... status pending\n... changeType public\n";
        assert_eq!(
            parse_change_state(spoofed),
            Some(ChangeState::Pending { shelved: false })
        );
        assert_eq!(parse_change_state(""), None);
        assert_eq!(parse_change_state("... status unknown\n"), None);
    }

    #[test]
    fn a_changelist_resolves_to_history_the_shelf_or_a_refusal() {
        assert_eq!(
            at_change(".\\a.txt", 5, ChangeState::Submitted, "x").unwrap(),
            ".\\a.txt@5"
        );
        assert_eq!(
            at_change(".\\a.txt", 5, ChangeState::Pending { shelved: true }, "x").unwrap(),
            ".\\a.txt@=5"
        );
        let err = at_change(
            ".\\a.txt",
            5,
            ChangeState::Pending { shelved: false },
            "omit it",
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
        assert!(err.message.contains("pending") && err.message.ends_with("omit it"));
    }

    #[test]
    fn changelist_numbers_are_positive_decimal_only() {
        assert_eq!(changelist("123", "id").unwrap(), 123);
        for bad in ["0", "-1", "+5", "12a", "", " 3", "@3", "#head"] {
            assert!(changelist(bad, "id").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn file_specs_escape_reserved_characters_and_widen_directories() {
        let dir = crate::testutil::temp_dir("evidence-p4-spec");
        let root = dir.as_path();
        std::fs::create_dir(root.join("src")).unwrap();
        assert_eq!(file_spec(root, "").unwrap(), ".\\...");
        assert_eq!(file_spec(root, "src").unwrap(), ".\\src\\...");
        assert_eq!(file_spec(root, "src/a.rs").unwrap(), ".\\src\\a.rs");
        assert_eq!(
            file_spec(root, "a@b#c*d%e.txt").unwrap(),
            ".\\a%40b%23c%2Ad%25e.txt"
        );
        // An option-shaped name stays a path: `p4 print -o` would write a file.
        assert_eq!(file_spec(root, "-o").unwrap(), ".\\-o");
        assert!(file_spec(root, "src/.../x").is_err());
        // A file that exists only in history is fine; a junction along the path is not.
        assert_eq!(file_spec(root, "gone/old.rs").unwrap(), ".\\gone\\old.rs");
        let outside = crate::testutil::temp_dir("evidence-p4-spec-outside");
        if crate::testutil::make_junction(&root.join("link"), outside.as_path()) {
            let err = file_spec(root, "link/a.rs").unwrap_err();
            assert_eq!(err.code, "link_forbidden");
        }
    }
}
