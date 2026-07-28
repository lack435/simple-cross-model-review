//! Configuration. Everything is supplied as CLI arguments on the MCP server entry,
//! so a project's `.mcp.json` / `config.toml` is the single source of truth and there
//! is no machine-level config file to drift out of sync.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewerKind {
    Claude,
    Codex,
}

impl ReviewerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Human label used in tool descriptions so the calling agent knows who is reviewing.
    pub fn vendor(self) -> &'static str {
        match self {
            Self::Claude => "Anthropic Claude Code",
            Self::Codex => "OpenAI Codex",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            // Pinned by full id on purpose: the `opus` alias resolves to an older
            // model (verified: `--model opus` reported claude-opus-4-8).
            Self::Claude => "claude-opus-5",
            Self::Codex => "gpt-5.6-terra",
        }
    }

    pub fn default_effort(self) -> &'static str {
        match self {
            Self::Claude => "high",
            Self::Codex => "xhigh",
        }
    }

    pub fn known_efforts(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["low", "medium", "high", "xhigh", "max"],
            Self::Codex => &["low", "medium", "high", "xhigh", "max", "ultra"],
        }
    }

    /// Executable stems to look for on PATH, most preferred first.
    pub fn bin_stems(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude"],
            Self::Codex => &["codex"],
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "anthropic" => Some(Self::Claude),
            "codex" | "chatgpt" | "openai" | "gpt" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// Tools the Claude reviewer is pre-approved to use.
///
/// Bash is deliberately absent. Claude's permission patterns match by command prefix,
/// and `Bash(git diff:*)` therefore permits *any* arguments to `git diff` -- including
/// `--output=<file>`, which writes. That was verified, not theorised: with
/// `Bash(git diff:*)` allow-listed, the reviewer ran
/// `git diff --output=PWNED_DIFF.txt HEAD~1 HEAD` and created a 354-byte file.
/// `git log`, `git show` and `git blame` accept `--output` too.
///
/// The problem generalises past git: read-oriented tools routinely have flags that
/// write files or execute programs (ripgrep's `--pre` runs an external command), so a
/// prefix allow-list cannot express "read-only". Shell redirection *is* caught by the
/// permission parser -- `git status --short > REDIR.txt` was denied -- but that only
/// closes the obvious hole, not this one.
///
/// So the Claude reviewer gets Read, Grep and Glob: Claude Code's own tools, which have
/// no write or execute capability at all. See `--allow-tools` to opt back into shell
/// access, and the README for why the Codex reviewer can safely keep it.
pub const DEFAULT_CLAUDE_ALLOWED_TOOLS: &str = "Read Grep Glob";

/// Built-in tools the Claude reviewer may use at all. Omitting Write, Edit and Bash
/// here removes them from the session entirely, so the model has nothing to attempt.
pub const DEFAULT_CLAUDE_TOOLS: &str = "Read,Grep,Glob";

pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_WAIT_SECS: u64 = 60;
pub const MAX_WAIT_SECS: u64 = 300;

#[derive(Clone, Debug)]
pub struct Config {
    pub reviewer: ReviewerKind,
    pub model: String,
    pub effort: String,
    /// Explicit path to the reviewer CLI. When absent we resolve it from PATH.
    pub bin: Option<PathBuf>,
    /// Working root handed to the reviewer. Defaults to the server's cwd, which is
    /// the project root when a harness launches us from `.mcp.json`.
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub state_dir: PathBuf,
    /// Codex sandbox policy.
    pub sandbox: String,
    /// Override for the Claude allow-list.
    pub allowed_tools: String,
    pub tools: String,
    pub preamble: Option<String>,
    pub no_preamble: bool,
    /// Stop the reviewer from loading MCP servers. Without this, a reviewer that has
    /// cross-review registered could call cross-review recursively. Costs the reviewer
    /// its user-level config: for Claude that is only MCP servers, for Codex it is the
    /// whole of config.toml (see the note in reviewer::codex).
    pub isolate_mcp: bool,
}

impl Config {
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let mut reviewer: Option<ReviewerKind> = None;
        let mut model: Option<String> = None;
        let mut effort: Option<String> = None;
        let mut bin: Option<PathBuf> = None;
        let mut cwd: Option<PathBuf> = None;
        let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
        let mut state_dir: Option<PathBuf> = None;
        let mut sandbox = "read-only".to_string();
        let mut allowed_tools: Option<String> = None;
        let mut tools: Option<String> = None;
        let mut preamble_file: Option<PathBuf> = None;
        let mut no_preamble = false;
        let mut isolate_mcp = true;

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            // Accept both `--flag value` and `--flag=value`.
            let (key, inline) = match arg.split_once('=') {
                Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
                _ => (arg, None),
            };
            let mut take = |name: &str| -> Result<String, String> {
                if let Some(v) = inline.clone() {
                    return Ok(v);
                }
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{name} requires a value"))
            };

            match key {
                "--reviewer" => {
                    let v = take("--reviewer")?;
                    reviewer = Some(ReviewerKind::parse(&v).ok_or_else(|| {
                        format!("unknown --reviewer '{v}' (expected 'claude' or 'codex')")
                    })?);
                }
                "--model" => model = Some(take("--model")?),
                "--effort" => effort = Some(take("--effort")?),
                "--bin" => bin = Some(PathBuf::from(take("--bin")?)),
                "--cwd" => cwd = Some(PathBuf::from(take("--cwd")?)),
                "--timeout-seconds" => {
                    let v = take("--timeout-seconds")?;
                    timeout_secs = v
                        .parse()
                        .map_err(|_| format!("--timeout-seconds must be an integer, got '{v}'"))?;
                    if timeout_secs == 0 {
                        return Err("--timeout-seconds must be greater than 0".into());
                    }
                }
                "--state-dir" => state_dir = Some(PathBuf::from(take("--state-dir")?)),
                "--sandbox" => sandbox = take("--sandbox")?,
                "--allow-tools" | "--allowed-tools" => allowed_tools = Some(take("--allow-tools")?),
                "--tools" => tools = Some(take("--tools")?),
                "--preamble-file" => preamble_file = Some(PathBuf::from(take("--preamble-file")?)),
                "--no-preamble" => no_preamble = true,
                "--allow-reviewer-mcp" => isolate_mcp = false,
                other => return Err(format!("unknown argument '{other}' (try --help)")),
            }
            i += 1;
        }

        let reviewer = reviewer.ok_or(
            "--reviewer is required (use '--reviewer codex' when the caller is Claude Code, \
             '--reviewer claude' when the caller is Codex)",
        )?;

        let cwd = match cwd {
            Some(p) => p,
            None => std::env::current_dir()
                .map_err(|e| format!("cannot determine current directory: {e}"))?,
        };
        let cwd = normalize_dir(cwd);

        let effort = effort.unwrap_or_else(|| reviewer.default_effort().to_string());
        if !reviewer.known_efforts().contains(&effort.as_str()) {
            // Not fatal: new effort levels appear over time and the CLI is the real
            // authority. A bad value surfaces as MODEL_UNAVAILABLE on first use.
            eprintln!(
                "cross-review: warning: effort '{}' is not one of the known levels for {} ({}). \
                 Passing it through anyway.",
                effort,
                reviewer.as_str(),
                reviewer.known_efforts().join(", ")
            );
        }

        let preamble = match preamble_file {
            Some(p) => Some(
                std::fs::read_to_string(&p)
                    .map_err(|e| format!("cannot read --preamble-file {}: {e}", p.display()))?,
            ),
            None => None,
        };

        Ok(Self {
            reviewer,
            model: model.unwrap_or_else(|| reviewer.default_model().to_string()),
            effort,
            bin,
            state_dir: state_dir.unwrap_or_else(|| default_state_dir(&cwd)),
            cwd,
            timeout: Duration::from_secs(timeout_secs),
            sandbox,
            allowed_tools: allowed_tools
                .unwrap_or_else(|| DEFAULT_CLAUDE_ALLOWED_TOOLS.to_string()),
            tools: tools.unwrap_or_else(|| DEFAULT_CLAUDE_TOOLS.to_string()),
            preamble,
            no_preamble,
            isolate_mcp,
        })
    }

    pub fn describe_reviewer(&self) -> String {
        format!(
            "{} ({}, model={}, effort={})",
            self.reviewer.vendor(),
            self.reviewer.as_str(),
            self.model,
            self.effort
        )
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.state_dir.join("tmp")
    }
}

/// Resolve a directory to an absolute path without the `\\?\` verbatim prefix that
/// `canonicalize` adds on Windows. That prefix is correct but leaks into the reviewer's
/// prompt and command line, where some tools mishandle it.
fn normalize_dir(dir: PathBuf) -> PathBuf {
    let resolved = dir.canonicalize().unwrap_or(dir);
    let text = resolved.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    resolved
}

/// Per-project state directory so two checkouts never share session ids.
fn default_state_dir(cwd: &Path) -> PathBuf {
    let leaf = cwd
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let leaf: String = leaf
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let key = format!(
        "{}-{:016x}",
        leaf,
        fnv1a64(&cwd.to_string_lossy().to_lowercase())
    );

    match std::env::var_os("LOCALAPPDATA") {
        Some(base) if !base.is_empty() => PathBuf::from(base).join("cross-review").join(key),
        // No LOCALAPPDATA (unusual, but do not fail): keep state beside the project.
        _ => cwd.join(".cross-review").join(key),
    }
}

fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub const USAGE: &str = r#"cross-review - an MCP server that sends work to a different model for review

USAGE:
  cross-review --reviewer <claude|codex> [OPTIONS]

The server speaks MCP over stdio. Register it in a project's .mcp.json (Claude Code)
or ~/.codex/config.toml (Codex); see examples/ in the repository.

REQUIRED:
  --reviewer <claude|codex>   Which CLI performs the review. Pick the model that is
                              NOT the calling agent.

OPTIONS:
  --model <id>                Reviewer model. Pin the full id, not an alias.
                              default: claude -> claude-opus-5, codex -> gpt-5.6-terra
  --effort <level>            Reasoning effort.
                              claude: low|medium|high|xhigh|max          (default high)
                              codex:  low|medium|high|xhigh|max|ultra    (default xhigh)
  --bin <path>                Path to the reviewer CLI. Default: resolved from PATH.
  --cwd <path>                Working root for the reviewer. Default: this process's cwd.
  --timeout-seconds <n>       Hard kill for a single review turn. Default: 900.
  --state-dir <path>          Where named sessions are recorded.
                              Default: %LOCALAPPDATA%\cross-review\<project>-<hash>
  --sandbox <mode>            Codex sandbox policy. Default: read-only.
  --tools <list>              Claude built-in tools. Default: Read,Grep,Glob,Bash
  --allow-tools <list>        Claude permission allow-list (read-only commands).
  --preamble-file <path>      Replace the built-in reviewer preamble.
  --no-preamble               Send the caller's instructions with no preamble at all.
  --allow-reviewer-mcp        Let the reviewer load its own MCP servers and user config.
                              Off by default: a reviewer that also has cross-review
                              registered would otherwise be able to recurse into it.

OTHER:
  --doctor                    Check the reviewer CLI and auth, then exit.
  --help, -h                  Show this help.
  --version, -V               Show the version.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reviewer_is_required() {
        let err = Config::from_args(&args(&["--model", "x"])).unwrap_err();
        assert!(err.contains("--reviewer is required"));
    }

    #[test]
    fn defaults_are_pinned_per_reviewer() {
        let claude = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert_eq!(claude.model, "claude-opus-5");
        assert_eq!(claude.effort, "high");

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(codex.model, "gpt-5.6-terra");
        assert_eq!(codex.effort, "xhigh");
    }

    #[test]
    fn model_and_effort_can_be_overridden() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-sol",
            "--effort",
            "ultra",
        ]))
        .expect("config");
        assert_eq!(cfg.model, "gpt-5.6-sol");
        assert_eq!(cfg.effort, "ultra");
    }

    #[test]
    fn equals_form_is_accepted() {
        // MCP config files vary in how they split args, so both forms must work.
        let cfg =
            Config::from_args(&args(&["--reviewer=claude", "--effort=xhigh"])).expect("config");
        assert_eq!(cfg.reviewer, ReviewerKind::Claude);
        assert_eq!(cfg.effort, "xhigh");
    }

    #[test]
    fn reviewer_aliases_resolve() {
        for alias in ["codex", "chatgpt", "openai", "gpt", "CODEX"] {
            let cfg = Config::from_args(&args(&["--reviewer", alias])).expect("config");
            assert_eq!(cfg.reviewer, ReviewerKind::Codex, "alias {alias}");
        }
        for alias in ["claude", "claude-code", "anthropic"] {
            let cfg = Config::from_args(&args(&["--reviewer", alias])).expect("config");
            assert_eq!(cfg.reviewer, ReviewerKind::Claude, "alias {alias}");
        }
    }

    #[test]
    fn unknown_reviewer_is_rejected_with_the_valid_options() {
        let err = Config::from_args(&args(&["--reviewer", "gemini"])).unwrap_err();
        assert!(err.contains("gemini"));
        assert!(err.contains("claude"));
        assert!(err.contains("codex"));
    }

    #[test]
    fn unknown_argument_is_rejected_rather_than_ignored() {
        let err = Config::from_args(&args(&["--reviewer", "codex", "--turbo"])).unwrap_err();
        assert!(err.contains("--turbo"));
    }

    #[test]
    fn missing_value_is_rejected() {
        let err = Config::from_args(&args(&["--reviewer", "codex", "--model"])).unwrap_err();
        assert!(err.contains("--model requires a value"));
    }

    #[test]
    fn bad_timeout_is_rejected() {
        let err = Config::from_args(&args(&["--reviewer", "codex", "--timeout-seconds", "soon"]))
            .unwrap_err();
        assert!(err.contains("must be an integer"));

        let err = Config::from_args(&args(&["--reviewer", "codex", "--timeout-seconds", "0"]))
            .unwrap_err();
        assert!(err.contains("greater than 0"));
    }

    #[test]
    fn timeout_default_and_override() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.timeout.as_secs(), DEFAULT_TIMEOUT_SECS);

        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--timeout-seconds", "60"]))
            .expect("config");
        assert_eq!(cfg.timeout.as_secs(), 60);
    }

    #[test]
    fn mcp_isolation_is_on_unless_opted_out() {
        assert!(
            Config::from_args(&args(&["--reviewer", "codex"]))
                .unwrap()
                .isolate_mcp
        );
        assert!(
            !Config::from_args(&args(&["--reviewer", "codex", "--allow-reviewer-mcp"]))
                .unwrap()
                .isolate_mcp
        );
    }

    #[test]
    fn write_tools_are_absent_from_the_default_tool_set() {
        let cfg = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert!(!cfg.tools.contains("Write"));
        assert!(!cfg.tools.contains("Edit"));
        assert!(!cfg.tools.contains("NotebookEdit"));
        assert!(cfg.tools.contains("Read"));
    }

    #[test]
    fn default_claude_policy_grants_no_shell_access() {
        // Regression guard for a verified breach: `Bash(git diff:*)` matches by prefix,
        // so it permitted `git diff --output=PWNED_DIFF.txt` and a file was written.
        // git log, show and blame accept --output as well, and the same class of hole
        // exists in non-git tools, so no Bash pattern belongs in the default policy.
        let cfg = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert!(
            !cfg.tools.contains("Bash"),
            "Bash must not be in the default tool set: {}",
            cfg.tools
        );
        assert!(
            !cfg.allowed_tools.contains("Bash"),
            "no Bash pattern may be pre-approved by default: {}",
            cfg.allowed_tools
        );
    }

    #[test]
    fn shell_access_can_still_be_opted_into() {
        // Escape hatch for users who accept the trade-off documented in the README.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ]))
        .expect("config");
        assert!(cfg.tools.contains("Bash"));
        assert!(cfg.allowed_tools.contains("Bash(git diff:*)"));
    }

    #[test]
    fn state_dir_differs_between_projects() {
        let a = default_state_dir(Path::new("C:\\dev\\project-a"));
        let b = default_state_dir(Path::new("C:\\dev\\project-b"));
        assert_ne!(a, b);
        // Readable leaf, so a human can find it.
        assert!(a.to_string_lossy().contains("project-a"));
    }

    #[test]
    fn state_dir_is_stable_for_the_same_project() {
        let a = default_state_dir(Path::new("C:\\dev\\thing"));
        let b = default_state_dir(Path::new("C:\\dev\\thing"));
        assert_eq!(a, b);
    }

    #[test]
    fn state_dir_ignores_path_case_as_windows_does() {
        let a = default_state_dir(Path::new("C:\\Dev\\Thing"));
        let b = default_state_dir(Path::new("c:\\dev\\thing"));
        // Same directory on Windows, so the same session state.
        assert_eq!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .split('-')
                .next_back(),
            b.file_name()
                .unwrap()
                .to_string_lossy()
                .split('-')
                .next_back()
        );
    }

    #[test]
    fn cwd_has_no_verbatim_prefix() {
        // \\?\C:\... is a valid path but confuses downstream tools and reads badly in
        // the prompt we hand the reviewer.
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert!(
            !cfg.cwd.to_string_lossy().starts_with(r"\\?\"),
            "cwd leaked a verbatim prefix: {}",
            cfg.cwd.display()
        );
        assert!(cfg.cwd.is_absolute());
    }

    #[test]
    fn normalize_dir_strips_verbatim_prefixes() {
        // Non-existent paths cannot be canonicalized, so they exercise the string path.
        assert_eq!(
            normalize_dir(PathBuf::from(r"\\?\C:\dev\thing")),
            PathBuf::from(r"C:\dev\thing")
        );
        assert_eq!(
            normalize_dir(PathBuf::from(r"\\?\UNC\server\share\thing")),
            PathBuf::from(r"\\server\share\thing")
        );
        // Ordinary paths are left alone.
        assert_eq!(
            normalize_dir(PathBuf::from(r"C:\dev\thing")),
            PathBuf::from(r"C:\dev\thing")
        );
    }

    #[test]
    fn unusual_effort_is_passed_through_not_rejected() {
        // The CLI is the authority on valid effort levels; new ones must not need a
        // rebuild of this tool.
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--effort", "hyper"]))
            .expect("unusual effort should warn, not fail");
        assert_eq!(cfg.effort, "hyper");
    }
}
