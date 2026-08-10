//! Tests for the reviewer command lines.
//!
//! Every security property this tool claims lives in `invocation()`: `--safe-mode`,
//! `--tools`, the scoped `--allowed-tools` rules, `--permission-mode dontAsk`,
//! `-s read-only`, `--ignore-user-config`. Those were previously covered only
//! indirectly -- tests asserted the `Config` fields, but nothing asserted the fields
//! reached the CLI, so deleting `cmd.arg("--safe-mode")` left the suite green.
//!
//! These tests read the constructed argv back with `Command::get_args`.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::reviewer::{claude::ClaudeReviewer, codex::CodexReviewer, Reviewer};

fn config(extra: &[&str]) -> Config {
    let mut args: Vec<String> = vec!["--reviewer".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    Config::from_args(&args).expect("config")
}

/// A config with an explicit working root (and optionally a state dir), so a test controls
/// whether the Claude reviewer's neutral-cwd switch is eligible rather than depending on where
/// the test process happens to run.
fn config_at(cwd: &Path, state: Option<&Path>, extra: &[&str]) -> Config {
    let mut args: Vec<String> = vec!["--reviewer".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.push("--cwd".into());
    args.push(cwd.to_string_lossy().into_owned());
    if let Some(state) = state {
        args.push("--state-dir".into());
        args.push(state.to_string_lossy().into_owned());
    }
    Config::from_args(&args).expect("config")
}

/// The full argv as strings, for presence checks.
fn argv(reviewer: &dyn Reviewer, cfg: &Config, resume: Option<&str>) -> Vec<String> {
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            resume,
            "tmp-1",
        )
        .expect("invocation");
    inv.command
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

fn program(reviewer: &dyn Reviewer, cfg: &Config) -> PathBuf {
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            None,
            "tmp-1",
        )
        .expect("invocation");
    PathBuf::from(inv.command.get_program())
}

fn cwd_of(reviewer: &dyn Reviewer, cfg: &Config) -> PathBuf {
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            None,
            "tmp-1",
        )
        .expect("invocation");
    inv.command
        .get_current_dir()
        .map(Path::to_path_buf)
        .expect("current_dir must be set")
}

/// Value that follows `flag`, when the flag is present.
fn value_after(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

/// All values between `flag` and the next `--flag`, for variadic options.
fn values_after(args: &[String], flag: &str) -> Vec<String> {
    let Some(i) = args.iter().position(|a| a == flag) else {
        return Vec::new();
    };
    args[i + 1..]
        .iter()
        .take_while(|a| !a.starts_with("--"))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------

#[test]
fn claude_runs_in_the_working_root_when_it_is_not_a_git_toplevel() {
    // A working root that is not a git top-level gives Claude Code no git context to churn the
    // prompt cache, so there is nothing a neutral cwd would gain: the reviewer runs in the
    // working root with the relative read rules, exactly as before. (Uses an explicit non-git
    // `--cwd` so the result does not depend on where the test process runs.)
    let dir = crate::testutil::temp_dir("cross-review-argv-project");
    let cfg = config_at(&dir, None, &["claude"]);
    assert_eq!(
        program(&ClaudeReviewer, &cfg),
        PathBuf::from("C:\\fake\\reviewer.exe")
    );
    assert_eq!(cwd_of(&ClaudeReviewer, &cfg), cfg.cwd);
}

#[test]
fn claude_argv_carries_the_pinned_model_and_effort() {
    let cfg = config(&["claude", "--model", "claude-opus-4-8", "--effort", "xhigh"]);
    let args = argv(&ClaudeReviewer, &cfg, None);
    assert_eq!(
        value_after(&args, "--model").as_deref(),
        Some("claude-opus-4-8")
    );
    assert_eq!(value_after(&args, "--effort").as_deref(), Some("xhigh"));
    assert!(
        args.iter().any(|a| a == "-p"),
        "must run non-interactively: {args:?}"
    );
    assert_eq!(
        value_after(&args, "--output-format").as_deref(),
        Some("json")
    );
}

#[test]
fn claude_argv_denies_writes_and_grants_no_shell() {
    let cfg = config(&["claude"]);
    for resume in [None, Some("sess-1")] {
        let args = argv(&ClaudeReviewer, &cfg, resume);

        // dontAsk denies anything outside the allow-list instead of prompting.
        assert_eq!(
            value_after(&args, "--permission-mode").as_deref(),
            Some("dontAsk")
        );
        assert_eq!(
            value_after(&args, "--tools").as_deref(),
            Some("Read,Grep,Glob")
        );

        let denied = value_after(&args, "--disallowed-tools").unwrap_or_default();
        assert!(denied.contains("Write"), "{denied}");
        assert!(denied.contains("Edit"), "{denied}");

        // The whole reason Bash was removed: a prefix allow-list cannot express
        // read-only, because `Bash(git diff:*)` permits `--output=<file>`.
        assert!(
            !args.iter().any(|a| a.contains("Bash")),
            "no argument may mention Bash by default: {args:?}"
        );
    }
}

#[test]
fn claude_argv_scopes_reads_relative_in_a_non_git_working_root() {
    // In the project-cwd mode (here forced by a non-git working root) the scope stays the
    // relative `./**`. One argv entry per rule: a project path containing a space or comma must
    // not be split into fragments, and a rule swallowed as a positional would silently become
    // part of the prompt instead of a permission.
    let dir = crate::testutil::temp_dir("cross-review-argv-relscope");
    let cfg = config_at(&dir, None, &["claude"]);
    let args = argv(&ClaudeReviewer, &cfg, None);
    let rules = values_after(&args, "--allowed-tools");
    assert_eq!(
        rules,
        vec!["Read(./**)", "Grep(./**)", "Glob(./**)"],
        "{args:?}"
    );
}

#[test]
fn claude_runs_neutral_with_absolute_read_scope_at_a_git_toplevel() {
    // The change this suite guards: at a git top-level, the default isolated shell-less reviewer
    // runs from a neutral directory so the parent agent's between-turn commits do not invalidate
    // its prompt cache, and the read scope moves from the relative `./**` to absolute rules
    // pinned to the working root. See docs/resume-cache-cwd-invalidation.md.
    let repo = crate::testutil::temp_dir("cross-review-argv-neutral-repo");
    // A `.git` entry (a file here, as a worktree/submodule has) makes this a git top-level.
    std::fs::write(repo.join(".git"), b"gitdir: elsewhere").expect("mark git toplevel");
    // An explicit non-git state dir is the neutral target, so the switch does not depend on the
    // ambient state directory existing.
    let state = crate::testutil::temp_dir("cross-review-argv-neutral-state");
    let cfg = config_at(&repo, Some(&state), &["claude"]);

    let cwd = cwd_of(&ClaudeReviewer, &cfg);
    assert_ne!(
        cwd, cfg.cwd,
        "the reviewer must run outside the working root"
    );
    assert!(
        !cwd.starts_with(&cfg.cwd),
        "neutral cwd {} must be outside the working root {}",
        cwd.display(),
        cfg.cwd.display()
    );

    let args = argv(&ClaudeReviewer, &cfg, None);
    let root = cfg.cwd.to_string_lossy().replace('\\', "/");
    assert_eq!(
        values_after(&args, "--allowed-tools"),
        vec![
            format!("Read({root}/**)"),
            format!("Grep({root}/**)"),
            format!("Glob({root}/**)"),
        ],
        "{args:?}"
    );
}

#[test]
fn claude_stays_in_the_working_root_when_a_gate_is_not_met() {
    // Each gate that must hold for the neutral switch, checked one at a time against an otherwise
    // eligible git top-level: dropping isolation, granting a shell, and overriding the read rules
    // each keep the reviewer in the working root (project mode).
    let repo = crate::testutil::temp_dir("cross-review-argv-gate-repo");
    std::fs::write(repo.join(".git"), b"gitdir: elsewhere").expect("mark git toplevel");
    let state = crate::testutil::temp_dir("cross-review-argv-gate-state");

    let cases: [&[&str]; 4] = [
        &["claude", "--allow-reviewer-config"],
        &[
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ],
        &["claude", "--allow-tools", "Read(./src/**)"],
        // Perforce backend: even at a git top-level (a coincidental .git) a Perforce review
        // stays in project-cwd mode -- its captured paths are not git-shaped.
        &["claude", "--vcs", "perforce"],
    ];
    for extra in cases {
        let cfg = config_at(&repo, Some(&state), extra);
        assert_eq!(
            cwd_of(&ClaudeReviewer, &cfg),
            cfg.cwd,
            "a gate is unmet, so the reviewer must stay in the working root: {extra:?}"
        );
    }
}

#[test]
fn claude_argv_isolates_project_configuration_by_default() {
    let cfg = config(&["claude"]);
    for resume in [None, Some("sess-1")] {
        let args = argv(&ClaudeReviewer, &cfg, resume);
        // Without this a committed .claude/settings.json hook executes a shell command
        // with no tool call, so no permission check (verified).
        assert!(args.iter().any(|a| a == "--safe-mode"), "{args:?}");
        assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
    }
}

#[test]
fn claude_argv_drops_isolation_only_when_asked() {
    let cfg = config(&["claude", "--allow-reviewer-config"]);
    let args = argv(&ClaudeReviewer, &cfg, None);
    assert!(!args.iter().any(|a| a == "--safe-mode"), "{args:?}");
    assert!(!args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
}

#[test]
fn claude_resumes_by_session_id_and_omits_it_otherwise() {
    let cfg = config(&["claude"]);

    let fresh = argv(&ClaudeReviewer, &cfg, None);
    assert!(!fresh.iter().any(|a| a == "--resume"), "{fresh:?}");

    let resumed = argv(
        &ClaudeReviewer,
        &cfg,
        Some("3d759777-4801-4e26-b6c5-4fbdb70adbbf"),
    );
    assert_eq!(
        value_after(&resumed, "--resume").as_deref(),
        Some("3d759777-4801-4e26-b6c5-4fbdb70adbbf")
    );
}

#[test]
fn claude_argv_honours_an_explicit_tool_override() {
    let cfg = config(&[
        "claude",
        "--tools",
        "Read,Grep,Glob,Bash",
        "--allow-tools",
        "Read Grep Glob Bash(git diff:*)",
    ]);
    let args = argv(&ClaudeReviewer, &cfg, None);
    assert_eq!(
        value_after(&args, "--tools").as_deref(),
        Some("Read,Grep,Glob,Bash")
    );
    // A user-supplied list stays one argument for the CLI to split itself.
    assert_eq!(
        values_after(&args, "--allowed-tools"),
        vec!["Read Grep Glob Bash(git diff:*)"]
    );
}

#[test]
fn the_auth_preflight_never_runs_inside_the_reviewed_project() {
    // The reviewer CLI must not pick up the reviewed repository's configuration, and the
    // preflight -- which runs before every review and on every status call -- was the one
    // invocation that did. A state directory inside the project must not be used as the
    // "neutral" directory either.
    let inside = std::env::current_dir().expect("cwd").join("state-inside");
    std::fs::create_dir_all(&inside).ok();
    let cfg = config(&["claude", "--state-dir", &inside.to_string_lossy()]);
    let neutral = crate::reviewer::neutral_dir(&cfg);
    assert!(
        !neutral.starts_with(&cfg.cwd),
        "neutral dir {} must be outside {}",
        neutral.display(),
        cfg.cwd.display()
    );
    std::fs::remove_dir_all(&inside).ok();
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

#[test]
fn codex_runs_in_the_project_and_reads_the_prompt_from_stdin() {
    let cfg = config(&["codex"]);
    assert_eq!(cwd_of(&CodexReviewer, &cfg), cfg.cwd);
    let args = argv(&CodexReviewer, &cfg, None);
    assert_eq!(args.first().map(String::as_str), Some("exec"));
    // `-` is what makes codex read the prompt from stdin rather than argv, which keeps a
    // large review request off the command line.
    assert!(args.iter().any(|a| a == "-"), "{args:?}");
    assert!(args.iter().any(|a| a == "--json"), "{args:?}");
}

#[test]
fn codex_argv_states_the_sandbox_on_every_turn_including_resumes() {
    // Regression guard for a real defect: `-s` exists only on the fresh-session form, so
    // resumed turns carried no sandbox policy at all and inherited read-only by accident.
    let cfg = config(&["codex"]);
    let expected = "sandbox_mode=\"read-only\"".to_string();

    let fresh = argv(&CodexReviewer, &cfg, None);
    assert_eq!(
        value_after(&fresh, "-s").as_deref(),
        Some("read-only"),
        "{fresh:?}"
    );
    assert!(fresh.contains(&expected), "{fresh:?}");

    let resumed = argv(
        &CodexReviewer,
        &cfg,
        Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969"),
    );
    assert!(
        resumed.contains(&expected),
        "a resumed turn must still state the sandbox policy: {resumed:?}"
    );
}

#[test]
fn codex_argv_carries_model_and_effort_on_both_paths() {
    let cfg = config(&["codex", "--model", "gpt-5.6-luna", "--effort", "xhigh"]);
    for resume in [None, Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969")] {
        let args = argv(&CodexReviewer, &cfg, resume);
        assert_eq!(value_after(&args, "-m").as_deref(), Some("gpt-5.6-luna"));
        assert!(
            args.contains(&"model_reasoning_effort=\"xhigh\"".to_string()),
            "{args:?}"
        );
    }
}

#[test]
fn codex_resume_passes_the_session_id_positionally() {
    let cfg = config(&["codex"]);
    let args = argv(
        &CodexReviewer,
        &cfg,
        Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969"),
    );
    // Order is fixed by the CLI: exec resume <SESSION_ID> [PROMPT].
    let i = args
        .iter()
        .position(|a| a == "resume")
        .expect("resume subcommand");
    assert_eq!(args[i + 1], "019faa01-a2d3-78c0-a67a-2ffe1ca75969");
    assert_eq!(args[i + 2], "-");
}

#[test]
fn codex_argv_isolates_user_configuration_by_default() {
    let cfg = config(&["codex"]);
    for resume in [None, Some("sess")] {
        let args = argv(&CodexReviewer, &cfg, resume);
        // codex exec does start configured MCP servers, so without this a reviewer with
        // cross-review registered could recurse into it.
        assert!(args.iter().any(|a| a == "--ignore-user-config"), "{args:?}");
    }

    let permissive = config(&["codex", "--allow-reviewer-config"]);
    for resume in [None, Some("sess")] {
        let args = argv(&CodexReviewer, &permissive, resume);
        assert!(
            !args.iter().any(|a| a == "--ignore-user-config"),
            "{args:?}"
        );
    }
}

#[test]
fn codex_argv_respects_a_custom_sandbox() {
    let cfg = config(&["codex", "--sandbox", "workspace-write"]);
    let args = argv(&CodexReviewer, &cfg, None);
    assert_eq!(value_after(&args, "-s").as_deref(), Some("workspace-write"));
    assert!(
        args.contains(&"sandbox_mode=\"workspace-write\"".to_string()),
        "{args:?}"
    );
}

#[test]
fn codex_writes_its_final_message_to_a_file_it_reports() {
    let cfg = config(&["codex"]);
    let inv = CodexReviewer
        .invocation(
            &cfg,
            cfg.primary(),
            Path::new("C:\\fake\\codex.exe"),
            None,
            "tmp-1",
        )
        .expect("invocation");
    let path = inv
        .last_message_file
        .clone()
        .expect("codex reports a last-message file");
    let args: Vec<String> = inv
        .command
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    // The file is authoritative for the review text, so the argv and the reported path
    // must agree or the review would be read from the wrong place.
    assert_eq!(value_after(&args, "-o").map(PathBuf::from), Some(path));
}
