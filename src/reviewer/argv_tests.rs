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
use crate::reviewer::{claude::ClaudeReviewer, codex::CodexReviewer, EvidenceInvocation, Reviewer};

fn test_evidence<'a>(cfg: &'a Config) -> EvidenceInvocation<'a> {
    EvidenceInvocation {
        executable: Path::new("C:\\fake\\cross-review.exe"),
        bundle_file: Path::new("C:\\fake\\bundle.json"),
        nonce: "test-nonce",
        sterile_dir: cfg
            .isolate_reviewer
            .then_some(Path::new("C:\\fake\\sterile-codex-cwd")),
        // Codex injects the server through its own config; only an in-scope Claude uses this.
        mcp_config_file: None,
    }
}

/// Evidence as the parent builds it for an in-scope Claude: a sterile cwd (Claude runs from it) and
/// a generated `--mcp-config` file.
fn test_claude_evidence<'a>() -> EvidenceInvocation<'a> {
    EvidenceInvocation {
        executable: Path::new("C:\\fake\\cross-review.exe"),
        bundle_file: Path::new("C:\\fake\\bundle.json"),
        nonce: "test-nonce",
        sterile_dir: Some(Path::new("C:\\fake\\sterile-claude-cwd")),
        mcp_config_file: Some(Path::new("C:\\fake\\claude-mcp.json")),
    }
}

/// Evidence exactly as the parent (`tools.rs`) decides it for this config: Codex always; Claude only
/// when `claude_neutral_target` qualifies (the in-scope shell-less path of plan section 0). This is
/// what keeps these argv tests faithful to the real gating.
fn evidence_for(cfg: &Config) -> Option<EvidenceInvocation<'_>> {
    match cfg.primary().reviewer {
        crate::config::ReviewerKind::Codex => Some(test_evidence(cfg)),
        // Mirror the parent's real gating: an in-scope Claude needs a pinned profile too (f2/f3).
        crate::config::ReviewerKind::Claude => {
            crate::reviewer::claude::claude_evidence_enabled(cfg, cfg.primary())
                .then(test_claude_evidence)
        }
    }
}

fn config(extra: &[&str]) -> Config {
    let mut args: Vec<String> = vec!["--reviewer".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    inject_level(&mut args);
    Config::from_args(&args).expect("config")
}

/// Supply a default `--level` when the args declare none. `--model`/`--effort` were removed, so a
/// level-less entry no longer parses; these argv tests are always single-reviewer and mostly care
/// about other flags, so injecting the reviewer's pinned default (as its sole, default level) keeps
/// them one call. An explicit `--level` in `extra` suppresses injection.
fn inject_level(args: &mut Vec<String>) {
    if args
        .iter()
        .any(|a| a == "--level" || a.starts_with("--level="))
    {
        return;
    }
    let kind = args
        .iter()
        .position(|a| a == "--reviewer")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or("codex");
    let (model, effort) = match crate::config::ReviewerKind::parse(kind) {
        Some(r) => (r.default_model(), r.default_effort()),
        None => ("gpt-5.6-luna", "max"),
    };
    args.push("--level".into());
    args.push(format!("standard:{model}:{effort}"));
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
    inject_level(&mut args);
    Config::from_args(&args).expect("config")
}

/// The full argv as strings, for presence checks.
fn argv(reviewer: &dyn Reviewer, cfg: &Config, resume: Option<&str>) -> Vec<String> {
    let evidence = evidence_for(cfg);
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            resume,
            "tmp-1",
            evidence.as_ref(),
            // Ambient: no pinned account home for this invocation.
            None,
        )
        .expect("invocation");
    inv.command
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

fn program(reviewer: &dyn Reviewer, cfg: &Config) -> PathBuf {
    let evidence = evidence_for(cfg);
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            None,
            "tmp-1",
            evidence.as_ref(),
            // Ambient: no pinned account home for this invocation.
            None,
        )
        .expect("invocation");
    PathBuf::from(inv.command.get_program())
}

fn cwd_of(reviewer: &dyn Reviewer, cfg: &Config) -> PathBuf {
    let evidence = evidence_for(cfg);
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            None,
            "tmp-1",
            evidence.as_ref(),
            // Ambient: no pinned account home for this invocation.
            None,
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
    // A profile makes this the in-scope evidence path (streams); model/effort are unaffected.
    let cfg = config(&[
        "claude",
        "--claude-profile",
        "test",
        "--level",
        "only:claude-opus-4-8:xhigh",
    ]);
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
    // This config (git top-level cwd, shell-less, default rules) is in scope for evidence, so it
    // streams -- the section-7 gate needs per-call events. The off-evidence buffered `json` path is
    // covered by the parse tests.
    assert_eq!(
        value_after(&args, "--output-format").as_deref(),
        Some("stream-json")
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
fn claude_in_scope_runs_from_the_sterile_cwd_with_absolute_read_scope_and_evidence() {
    // Option (a): at a git top-level the default isolated shell-less reviewer is IN SCOPE for the
    // evidence service. It runs from the parent's verified-empty sterile directory (not the repo,
    // and not the merely-non-Git neutral dir that reopened f2), reads the repo through absolute
    // rules pinned to the working root, and carries the evidence server plus granular isolation
    // flags in place of --safe-mode. See docs/resume-cache-cwd-invalidation.md and plan section 0.
    let repo = crate::testutil::temp_dir("cross-review-argv-neutral-repo");
    // A `.git` entry (a file here, as a worktree/submodule has) makes this a git top-level.
    std::fs::write(repo.join(".git"), b"gitdir: elsewhere").expect("mark git toplevel");
    let state = crate::testutil::temp_dir("cross-review-argv-neutral-state");
    // A pinned profile is required for the evidence path (f2).
    let cfg = config_at(&repo, Some(&state), &["claude", "--claude-profile", "test"]);

    // Runs from the sterile cwd the parent's evidence carries, which is outside the repo.
    let cwd = cwd_of(&ClaudeReviewer, &cfg);
    assert_eq!(cwd, Path::new("C:\\fake\\sterile-claude-cwd"));
    assert!(
        !cwd.starts_with(&cfg.cwd),
        "sterile cwd {} must be outside the working root {}",
        cwd.display(),
        cfg.cwd.display()
    );

    let args = argv(&ClaudeReviewer, &cfg, None);
    let root = cfg.cwd.to_string_lossy().replace('\\', "/");
    // Absolute read rules, then the evidence server allow-list entry, in one --allowed-tools list.
    assert_eq!(
        values_after(&args, "--allowed-tools"),
        vec![
            format!("Read({root}/**)"),
            format!("Grep({root}/**)"),
            format!("Glob({root}/**)"),
            "mcp__cross_review_evidence".to_string(),
        ],
        "{args:?}"
    );
    // Evidence server wired via --mcp-config; granular isolation instead of --safe-mode.
    assert_eq!(
        value_after(&args, "--mcp-config").as_deref(),
        Some("C:\\fake\\claude-mcp.json")
    );
    assert!(!args.iter().any(|a| a == "--safe-mode"), "{args:?}");
    assert!(
        args.iter().any(|a| a == "--disable-slash-commands"),
        "{args:?}"
    );
    assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
    assert_eq!(
        value_after(&args, "--settings").as_deref(),
        Some("{\"disableAllHooks\":true,\"autoMemoryEnabled\":false}")
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
fn claude_evidence_is_scoped_to_the_shell_less_path_f7() {
    // Plan f7 (+ f2/f3): one profile-aware predicate gates the WHOLE treatment together, so no
    // reviewer gets it partially. A profile-pinned shell-less isolated Claude (in scope) drops
    // --safe-mode for the granular flags + evidence server; a shell-ENABLED one AND an AMBIENT one
    // (both out of scope) keep --safe-mode, get no evidence and no granular flags, and stay in the
    // repo cwd -- exactly today's behaviour.
    let repo = crate::testutil::temp_dir("cross-review-argv-f7-repo");
    std::fs::write(repo.join(".git"), b"gitdir: elsewhere").expect("mark git toplevel");
    let state = crate::testutil::temp_dir("cross-review-argv-f7-state");

    // In scope: profile-pinned, shell-less. Granular isolation + evidence, no --safe-mode.
    let in_scope = config_at(&repo, Some(&state), &["claude", "--claude-profile", "test"]);
    let a = argv(&ClaudeReviewer, &in_scope, None);
    assert!(
        !a.iter().any(|x| x == "--safe-mode"),
        "in scope must NOT keep --safe-mode: {a:?}"
    );
    assert!(a.iter().any(|x| x == "--disable-slash-commands"), "{a:?}");
    assert!(a.iter().any(|x| x == "--mcp-config"), "{a:?}");
    assert!(a.iter().any(|x| x == "mcp__cross_review_evidence"), "{a:?}");

    // Out of scope by AMBIENT (no profile, f2): keeps --safe-mode, no evidence, repo cwd -- an
    // ambient Claude's ~/.claude config is not covered by the sterile cwd, so it must not drop
    // --safe-mode.
    let ambient = config_at(&repo, Some(&state), &["claude"]);
    let amb = argv(&ClaudeReviewer, &ambient, None);
    assert!(
        amb.iter().any(|x| x == "--safe-mode"),
        "ambient (no profile) Claude keeps --safe-mode: {amb:?}"
    );
    assert!(
        !amb.iter().any(|x| x == "--mcp-config"),
        "ambient Claude gets no evidence: {amb:?}"
    );
    // Ambient shell-less at a git top-level still uses the existing neutral-cwd optimisation (a
    // non-repo dir), under --safe-mode -- unchanged from before this feature.
    assert_ne!(
        cwd_of(&ClaudeReviewer, &ambient),
        ambient.cwd,
        "ambient shell-less Claude runs from the neutral dir, not the repo"
    );

    // Out of scope by SHELL (even with a profile): keeps --safe-mode, no evidence, repo cwd.
    let out = config_at(
        &repo,
        Some(&state),
        &[
            "claude",
            "--claude-profile",
            "test",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ],
    );
    let b = argv(&ClaudeReviewer, &out, None);
    assert!(
        b.iter().any(|x| x == "--safe-mode"),
        "shell-enabled isolated Claude keeps --safe-mode: {b:?}"
    );
    assert!(
        b.iter().any(|x| x == "--strict-mcp-config"),
        "and still strict MCP: {b:?}"
    );
    assert!(
        !b.iter().any(|x| x == "--mcp-config"),
        "out of scope must get NO evidence server: {b:?}"
    );
    assert!(
        !b.iter().any(|x| x == "--disable-slash-commands"),
        "out of scope must get NO granular flags: {b:?}"
    );
    assert!(
        !b.iter().any(|x| x == "mcp__cross_review_evidence"),
        "{b:?}"
    );
    assert_eq!(
        cwd_of(&ClaudeReviewer, &out),
        out.cwd,
        "out of scope stays in the working root"
    );
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

#[test]
fn codex_sterile_directory_is_empty_outside_and_stable_across_turns() {
    let root = crate::testutil::temp_dir("codex-sterile-root");
    let state = crate::testutil::temp_dir("codex-sterile-state");
    let cfg = config_at(root.as_path(), Some(state.as_path()), &["codex"]);
    let first = crate::reviewer::codex_sterile_dir(&cfg, "session-a").expect("sterile");
    let path = first.path().to_path_buf();
    assert!(!crate::reviewer::is_within(&path, &cfg.cwd));
    assert!(std::fs::read_dir(&path).unwrap().next().is_none());
    drop(first);
    assert!(!path.exists());

    let resumed = crate::reviewer::codex_sterile_dir(&cfg, "session-a").expect("resume sterile");
    assert_eq!(resumed.path(), path);
}

#[test]
fn sterile_dir_drop_retries_past_a_transient_sharing_violation() {
    // Issue #72, made deterministic. An external process (an AV scan / the search indexer under
    // load) can hold a handle to the just-emptied sterile directory WITHOUT `FILE_SHARE_DELETE`, so
    // `SterileDir::drop`'s `remove_dir` is refused with a sharing violation and, on a single attempt,
    // leaves the directory present -- which is what made `..._is_empty_outside_and_stable_across_turns`
    // flake. Here a handle is held exactly that way and released shortly after, well inside drop's
    // retry budget, so drop must spin past the refusal and still remove the directory.
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::Duration;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let root = crate::testutil::temp_dir("codex-sterile-drop-retry-root");
    let state = crate::testutil::temp_dir("codex-sterile-drop-retry-state");
    let cfg = config_at(root.as_path(), Some(state.as_path()), &["codex"]);
    let guard = crate::reviewer::codex_sterile_dir(&cfg, "session-drop-retry").expect("sterile");
    let path = guard.path().to_path_buf();
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .share_mode(1 | 2) // READ | WRITE, deliberately no DELETE -- refuses the removal.
        .open(&path)
        .expect("open blocking handle");
    // Confirm the precondition: with this handle held, a one-shot removal really is refused.
    assert!(
        std::fs::remove_dir(&path).is_err(),
        "precondition: a non-share-delete handle must refuse the removal"
    );
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(handle); // release the sharing violation, well within drop's 1s budget
    });
    drop(guard); // must retry past the refusal until the handle is released, then remove
    releaser.join().unwrap();
    assert!(
        !path.exists(),
        "drop must remove the directory once the blocking handle is released"
    );
}

#[test]
fn sterile_dir_name_keys_on_state_dir_and_session() {
    // The name-derivation invariant, tested pure so it does not depend on the shared process temp
    // root the filesystem path reaps (that concurrency is issue #87's problem, not this one's).
    use std::path::Path;
    let name = |state: &str, session: &str| {
        super::sterile_dir_name(Path::new(state), session).expect("digest available")
    };
    // Deterministic and stable across turns: same inputs, same name -- what resume relies on.
    assert_eq!(
        name("C:\\state", "session-a"),
        name("C:\\state", "session-a")
    );
    // Same session name under two state dirs must not alias. Two processes with different
    // `--state-dir` do not share the per-session lease, so a shared directory would let one reap or
    // drop it from under the other; folding `state_dir` in gives them distinct directories.
    assert_ne!(
        name("C:\\state-a", "session"),
        name("C:\\state-b", "session")
    );
    // Different sessions under one state dir stay distinct, as before.
    assert_ne!(
        name("C:\\state", "session-a"),
        name("C:\\state", "session-b")
    );
    // Length-prefixing keeps `(state_dir, session)` from colliding with a differently-split pair:
    // ("C:\\a", "bc") and ("C:\\ab", "c") share the concatenation but not the framed encoding.
    assert_ne!(name("C:\\a", "bc"), name("C:\\ab", "c"));
    // A control-char-namespaced internal name (like the status preflight's) is distinct from the
    // bare user string, so a user session cannot hash onto the internal sterile directory.
    assert_ne!(
        name("C:\\state", "\u{0}cross-review-status"),
        name("C:\\state", "cross-review-status")
    );
}

#[test]
fn sterile_dir_name_hashes_raw_path_units_not_the_lossy_string() {
    // `to_string_lossy` is not injective: two paths with distinct unpaired surrogates both render
    // as the U+FFFD replacement string, so hashing the lossy form would give them one name while
    // the lease (which keeps the raw `PathBuf`) keeps them on distinct lock files -- the dangerous
    // "shared name, unshared lease" direction. Hashing the raw UTF-16 units keeps them distinct.
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let p1 = std::path::PathBuf::from(OsString::from_wide(&[0xD800]));
    let p2 = std::path::PathBuf::from(OsString::from_wide(&[0xD801]));
    assert_eq!(
        p1.to_string_lossy(),
        p2.to_string_lossy(),
        "precondition: the lossy strings must collide for this test to mean anything"
    );
    assert_ne!(
        super::sterile_dir_name(&p1, "s").expect("digest"),
        super::sterile_dir_name(&p2, "s").expect("digest"),
        "raw-unit hashing must distinguish paths the lossy string cannot"
    );
}

#[test]
fn contents_error_disposition_retains_races_and_propagates_the_rest() {
    // An error reading a present entry's contents never proves it gone, so a benign race retains it
    // (counts it toward the bound -- fail closed -- rather than skipping a slot that may still
    // exist), while a genuinely unexpected error stays fatal instead of being silently swept under.
    use std::io::{Error, ErrorKind};
    assert!(matches!(
        super::contents_error_disposition(Error::from(ErrorKind::NotFound)),
        Ok(super::SterileEntry::Retained)
    ));
    assert!(matches!(
        super::contents_error_disposition(Error::from(ErrorKind::PermissionDenied)),
        Ok(super::SterileEntry::Retained)
    ));
    let fatal = super::contents_error_disposition(Error::from(ErrorKind::Other));
    assert_eq!(
        fatal.expect_err("a non-race error must stay fatal").kind(),
        ErrorKind::Other
    );
}

#[test]
fn reaper_reaps_stale_empty_dirs_only() {
    use std::time::{Duration, SystemTime};
    let parent = crate::testutil::temp_dir("codex-reaper-logic");
    let empty = parent.as_path().join("empty");
    let full = parent.as_path().join("full");
    std::fs::create_dir(&empty).unwrap();
    std::fs::create_dir(&full).unwrap();
    std::fs::write(full.join("f"), "x").unwrap();
    // `stale_age = 0` makes every entry "stale", isolating the emptiness gate: the empty directory
    // is reaped, the non-empty one retained. (The non-empty case is the security-relevant one -- a
    // contaminated directory must never be swept.)
    let now = SystemTime::now();
    for entry in std::fs::read_dir(parent.as_path()).unwrap() {
        let entry = entry.unwrap();
        let is_empty = entry.file_name() == "empty";
        let d = super::classify_sterile_entry(&entry, now, Duration::ZERO).unwrap();
        assert_eq!(matches!(d, super::SterileEntry::Reaped), is_empty);
    }
    assert!(!empty.exists(), "a stale empty dir is removed");
    assert!(full.exists(), "a non-empty dir is retained");

    // The staleness gate: an empty dir younger than `stale_age` is retained, not reaped.
    let fresh = parent.as_path().join("fresh");
    std::fs::create_dir(&fresh).unwrap();
    let entry = std::fs::read_dir(parent.as_path())
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name() == "fresh")
        .unwrap();
    let d = super::classify_sterile_entry(&entry, now, Duration::from_secs(24 * 60 * 60)).unwrap();
    assert!(matches!(d, super::SterileEntry::Retained));
    assert!(fresh.exists(), "a fresh empty dir is not reaped");
}

#[test]
fn reaper_treats_a_vanished_entry_as_reaped_not_an_error() {
    // The #87 race made concrete: an entry removed by a concurrent caller between our `read_dir` and
    // our `symlink_metadata` must classify as reaped, not fail the whole turn with a `NotFound`.
    use std::time::{Duration, SystemTime};
    let parent = crate::testutil::temp_dir("codex-reaper-vanish");
    let victim = parent.as_path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    let entry = std::fs::read_dir(parent.as_path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    // Simulate the concurrent reaper winning the race: the entry is gone before we inspect it.
    std::fs::remove_dir(&victim).unwrap();
    let d = super::classify_sterile_entry(&entry, SystemTime::now(), Duration::ZERO)
        .expect("a vanished entry must not fail the sweep");
    assert!(matches!(d, super::SterileEntry::Reaped));
}

#[test]
fn codex_sterile_directory_refuses_any_existing_entry() {
    let root = crate::testutil::temp_dir("codex-sterile-dirty-root");
    let state = crate::testutil::temp_dir("codex-sterile-dirty-state");
    let cfg = config_at(root.as_path(), Some(state.as_path()), &["codex"]);
    let guard = crate::reviewer::codex_sterile_dir(&cfg, "session-dirty").expect("sterile");
    let path = guard.path().to_path_buf();
    std::mem::forget(guard);
    std::fs::write(path.join("unexpected"), "x").unwrap();
    assert!(crate::reviewer::codex_sterile_dir(&cfg, "session-dirty").is_err());
    std::fs::remove_dir_all(path).ok();
}

#[test]
fn codex_session_mode_separates_sterile_evidence_from_project_cwd() {
    let isolated = config(&["codex"]);
    assert_eq!(
        crate::reviewer::reviewer_cwd_mode(&isolated, crate::config::ReviewerKind::Codex),
        crate::reviewer::CWD_MODE_CODEX_EVIDENCE
    );
    let opted_out = config(&["codex", "--allow-reviewer-config"]);
    assert_eq!(
        crate::reviewer::reviewer_cwd_mode(&opted_out, crate::config::ReviewerKind::Codex),
        crate::reviewer::CWD_MODE_PROJECT
    );
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

#[test]
fn codex_runs_from_the_sterile_root_and_reads_the_prompt_from_stdin() {
    let cfg = config(&["codex"]);
    assert_eq!(
        cwd_of(&CodexReviewer, &cfg),
        PathBuf::from("C:\\fake\\sterile-codex-cwd")
    );
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
    let cfg = config(&["codex", "--level", "only:gpt-5.6-luna:xhigh"]);
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
fn codex_fast_mode_is_off_by_default() {
    // No override keys unless --codex-fast-mode is passed, on either path.
    let cfg = config(&["codex"]);
    for resume in [None, Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969")] {
        let args = argv(&CodexReviewer, &cfg, resume);
        assert!(
            !args.iter().any(|a| a == "service_tier=\"fast\""),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "features.fast_mode=true"),
            "{args:?}"
        );
    }
}

#[test]
fn codex_fast_mode_adds_both_config_keys_on_every_turn() {
    // `codex exec` cannot use the interactive /fast toggle, so --codex-fast-mode replicates the
    // documented persistent-on state with its two config keys, asserted on fresh and resumed turns
    // alike (like the sandbox and effort overrides). Each is passed as its own `-c <value>` pair.
    let cfg = config(&["codex", "--codex-fast-mode"]);
    for resume in [None, Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969")] {
        let args = argv(&CodexReviewer, &cfg, resume);
        for key in ["service_tier=\"fast\"", "features.fast_mode=true"] {
            let i = args
                .iter()
                .position(|a| a == key)
                .unwrap_or_else(|| panic!("expected {key} in {args:?}"));
            assert_eq!(args[i - 1], "-c", "{key} must follow a -c flag: {args:?}");
        }
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
        assert!(args.iter().any(|a| a == "--ignore-rules"), "{args:?}");
        assert!(args.iter().any(|a| a == "--strict-config"), "{args:?}");
        assert!(
            args.iter().any(|a| a == "--skip-git-repo-check"),
            "{args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a.contains("mcp_servers.cross_review_evidence.command=")),
            "{args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a == "mcp_servers.cross_review_evidence.required=true"),
            "{args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a == "mcp_servers.cross_review_evidence.enabled=true"),
            "{args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a.contains("mcp_servers.cross_review_evidence.enabled_tools=")),
            "{args:?}"
        );
        assert!(
            args.iter().any(|a| a
                == "mcp_servers.cross_review_evidence.default_tools_approval_mode=\"approve\""),
            "{args:?}"
        );
    }

    let permissive = config(&["codex", "--allow-reviewer-config"]);
    for resume in [None, Some("sess")] {
        let args = argv(&CodexReviewer, &permissive, resume);
        assert!(
            !args.iter().any(|a| a == "--ignore-user-config"),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a == "--ignore-rules"), "{args:?}");
        assert_eq!(cwd_of(&CodexReviewer, &permissive), permissive.cwd);
        assert!(
            args.iter()
                .any(|a| a == "mcp_servers.cross_review_evidence.required=true"),
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
    let evidence = test_evidence(&cfg);
    let inv = CodexReviewer
        .invocation(
            &cfg,
            cfg.primary(),
            Path::new("C:\\fake\\codex.exe"),
            None,
            "tmp-1",
            Some(&evidence),
            // Ambient: no pinned account home for this invocation.
            None,
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

/// The environment a built invocation would hand the child, as (key, value) pairs.
fn env_of(
    reviewer: &dyn Reviewer,
    cfg: &Config,
    pinned: Option<&crate::config::AuthorizedHome>,
) -> Vec<(String, String)> {
    let evidence = evidence_for(cfg);
    let inv = reviewer
        .invocation(
            cfg,
            cfg.primary(),
            Path::new("C:\\fake\\reviewer.exe"),
            None,
            "tmp-pin",
            evidence.as_ref(),
            pinned,
        )
        .expect("invocation");
    inv.command
        .get_envs()
        .filter_map(|(k, v)| {
            Some((
                k.to_string_lossy().into_owned(),
                v?.to_string_lossy().into_owned(),
            ))
        })
        .collect()
}

#[test]
fn an_invocation_applies_the_pinned_home_rather_than_resolving_its_own() {
    // The account is resolved once per attempt and threaded in. An adapter that re-read it would
    // re-pin a profile that had moved since the attempt began, so a home re-logged from account A
    // to account B mid-attempt would be launched under B while the guard still believed it was A --
    // and with a repair there are now two spawns per turn for that to happen between. Threading the
    // pin makes "the account is fixed for the attempt" a property of the signature.
    let dir = crate::testutil::temp_dir("argv-pin");
    let home = dir.as_path().join("pinned-home");
    std::fs::create_dir_all(&home).expect("home");
    let pinned = crate::config::AuthorizedHome {
        home: home.clone(),
        account: "acct-a".to_string(),
    };

    for (reviewer, var) in [
        (&CodexReviewer as &dyn Reviewer, "CODEX_HOME"),
        (&ClaudeReviewer as &dyn Reviewer, "CLAUDE_CONFIG_DIR"),
    ] {
        let cfg = Config::from_args(&[
            "--reviewer".to_string(),
            if var == "CODEX_HOME" {
                "codex"
            } else {
                "claude"
            }
            .to_string(),
            "--level".to_string(),
            if var == "CODEX_HOME" {
                "standard:gpt-5.6-luna:max"
            } else {
                "standard:claude-opus-4-8:medium"
            }
            .to_string(),
        ])
        .expect("config");
        let env = env_of(reviewer, &cfg, Some(&pinned));
        let got = env
            .iter()
            .find(|(k, _)| k == var)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{var} should be set from the pin"));
        assert_eq!(got, home.to_string_lossy(), "{var}");

        // Ambient: no pin, and the adapter must not invent one.
        let env = env_of(reviewer, &cfg, None);
        assert!(
            !env.iter().any(|(k, _)| k == var),
            "{var} must be absent when nothing was pinned"
        );
    }
}
