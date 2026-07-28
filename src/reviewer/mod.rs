//! Driving a reviewer CLI as a child process.
//!
//! Shared here: locating the executable, running it with a timeout without
//! deadlocking on pipes, and the trait the two adapters implement.

pub mod claude;
pub mod codex;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::{Config, ReviewerKind};
use crate::errors::{self, Failure};

/// What the reviewer produced.
#[derive(Debug)]
pub struct Parsed {
    pub text: String,
    /// The CLI's own session id, needed to resume this review later.
    pub session_id: Option<String>,
    /// Tool calls the reviewer was not permitted to make.
    pub denials: Vec<String>,
}

pub struct Invocation {
    pub command: Command,
    /// A file the CLI writes its final message to, when it supports that.
    pub last_message_file: Option<PathBuf>,
}

pub trait Reviewer: Send + Sync {
    /// Cheap check that the CLI exists and is signed in. Runs before we spend a
    /// model call, so an unconfigured machine fails fast and legibly.
    fn auth_check(&self, bin: &Path) -> Result<String, Failure>;

    fn invocation(
        &self,
        cfg: &Config,
        bin: &Path,
        resume: Option<&str>,
        tmp_id: &str,
    ) -> std::io::Result<Invocation>;

    /// `last_message_file` is whatever `invocation` asked the CLI to write its final
    /// message to. It is passed separately because the `Command` is consumed by `run`.
    fn parse(
        &self,
        cfg: &Config,
        out: &RunOutcome,
        last_message_file: Option<&Path>,
    ) -> Result<Parsed, Failure>;
}

pub fn for_kind(kind: ReviewerKind) -> Box<dyn Reviewer> {
    match kind {
        ReviewerKind::Claude => Box::new(claude::ClaudeReviewer),
        ReviewerKind::Codex => Box::new(codex::CodexReviewer),
    }
}

// ---------------------------------------------------------------------------
// Executable resolution
// ---------------------------------------------------------------------------

/// Locate the reviewer CLI. An explicit `--bin` always wins; otherwise we search
/// PATH with the Windows executable extensions and then a few well-known install
/// locations, because npm-global and native installs land in different places.
pub fn resolve_bin(cfg: &Config) -> Result<PathBuf, Failure> {
    let mut tried: Vec<String> = Vec::new();

    if let Some(explicit) = &cfg.bin {
        if explicit.is_file() {
            return Ok(explicit.clone());
        }
        tried.push(format!("{} (from --bin)", explicit.display()));
        return Err(errors::cli_not_found(cfg.reviewer.as_str(), &tried));
    }

    let exts = path_exts();
    let stems = cfg.reviewer.bin_stems();

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for stem in stems {
                for ext in &exts {
                    let candidate = dir.join(format!("{stem}{ext}"));
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                }
            }
        }
        tried.push(format!(
            "PATH entries, for {} with extensions [{}]",
            stems.join(", "),
            exts.iter()
                .map(|e| if e.is_empty() { "(none)" } else { e.as_str() })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        tried.push("PATH is not set".to_string());
    }

    for candidate in fallback_locations(cfg.reviewer) {
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }

    Err(errors::cli_not_found(cfg.reviewer.as_str(), &tried))
}

fn path_exts() -> Vec<String> {
    let mut exts: Vec<String> = std::env::var("PATHEXT")
        .map(|v| {
            v.split(';')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if exts.is_empty() {
        exts = vec![".exe".into(), ".cmd".into(), ".bat".into()];
    }
    // Prefer a real executable over a shim: launching a .cmd goes through cmd.exe,
    // which adds a layer of argument escaping we would rather not depend on.
    exts.sort_by_key(|e| match e.as_str() {
        ".exe" => 0,
        ".com" => 1,
        ".cmd" => 2,
        ".bat" => 3,
        _ => 4,
    });
    exts.push(String::new());
    exts
}

fn fallback_locations(kind: ReviewerKind) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);

    match kind {
        ReviewerKind::Claude => {
            if let Some(h) = &home {
                out.push(h.join(".local").join("bin").join("claude.exe"));
                out.push(h.join(".claude").join("local").join("claude.exe"));
            }
            if let Some(a) = &appdata {
                out.push(a.join("npm").join("claude.cmd"));
            }
        }
        ReviewerKind::Codex => {
            if let Some(h) = &home {
                out.push(h.join(".local").join("bin").join("codex.exe"));
                out.push(h.join(".codex").join(".sandbox-bin").join("codex.exe"));
            }
            if let Some(a) = &appdata {
                out.push(a.join("npm").join("codex.cmd"));
            }
            if let Some(l) = &local {
                out.push(l.join("Programs").join("codex").join("codex.exe"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Running a child process
// ---------------------------------------------------------------------------

pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl RunOutcome {
    /// stderr first: that is where CLIs put the reason they failed.
    pub fn diagnostics(&self) -> String {
        let mut out = String::new();
        if !self.stderr.trim().is_empty() {
            out.push_str(self.stderr.trim());
        }
        if !self.stdout.trim().is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(self.stdout.trim());
        }
        out
    }
}

/// Run a child, feeding `stdin_data` in and collecting both output streams.
///
/// stdin is written on its own thread and both output streams are drained on their
/// own threads. Doing any of that on the calling thread risks a deadlock: a large
/// prompt can fill the child's stdin buffer while the child is blocked writing
/// output that nobody is reading.
pub fn run(
    mut command: Command,
    stdin_data: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> std::io::Result<RunOutcome> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child: Child = command.spawn()?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let payload = stdin_data.to_owned();
    let writer = std::thread::spawn(move || {
        // A reviewer that exits early closes the pipe; that is not our error to report.
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.flush();
        drop(stdin);
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stdout_reader = std::thread::spawn(move || read_to_string_lossy(&mut stdout_pipe));

    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stderr_reader = std::thread::spawn(move || read_to_string_lossy(&mut stderr_pipe));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;

    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None => {
                if cancel.load(Ordering::SeqCst) {
                    cancelled = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    let _ = writer.join();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    Ok(RunOutcome {
        stdout,
        stderr,
        exit: status.and_then(|s| s.code()),
        success: status.map(|s| s.success()).unwrap_or(false) && !timed_out && !cancelled,
        timed_out,
        cancelled,
    })
}

/// CLI output is normally UTF-8, but a stray non-UTF-8 byte in a diagnostic must not
/// lose us the whole message.
fn read_to_string_lossy(reader: &mut impl std::io::Read) -> String {
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(reader, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Turn a non-success run into the right `Failure`.
pub fn failure_for(cfg: &Config, out: &RunOutcome) -> Failure {
    let reviewer = cfg.reviewer.as_str();
    if out.cancelled {
        return errors::cancelled();
    }
    if out.timed_out {
        return errors::timed_out(reviewer, cfg.timeout.as_secs(), out.diagnostics());
    }
    errors::classify(
        reviewer,
        &cfg.model,
        &cfg.effort,
        out.exit,
        &out.diagnostics(),
    )
}

/// Path for a CLI's final-message file. Cleaned up by the caller.
pub fn tmp_file(cfg: &Config, tmp_id: &str, name: &str) -> std::io::Result<PathBuf> {
    let dir = cfg.tmp_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{tmp_id}-{name}")))
}
