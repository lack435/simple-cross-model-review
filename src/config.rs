//! Configuration. Everything is supplied as CLI arguments on the MCP server entry,
//! so a project's `.mcp.json` / `config.toml` is the single source of truth and there
//! is no machine-level config file to drift out of sync.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::vcs::DiffMode;

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
            // Pinned by full id on purpose. Bare aliases like `opus` can change resolution
            // as releases ship (verified once: `--model opus` reported claude-opus-4-8), so
            // writing the exact id keeps each reviewer fixed to the one chosen here rather
            // than moving out from under us.
            Self::Claude => "claude-opus-4-8",
            Self::Codex => "gpt-5.6-luna",
        }
    }

    pub fn default_effort(self) -> &'static str {
        match self {
            Self::Claude => "medium",
            Self::Codex => "max",
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

/// Which version-control system produced the change under review.
///
/// Selected by `--vcs`, defaulting to `auto`. `auto` is resolved once, at startup, by a
/// filesystem-only check (see `detect_vcs`): it never runs `p4` -- or any process -- to
/// decide, so an ordinary git repository behaves exactly as it did before Perforce existed
/// and never touches a Perforce server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vcs {
    Git,
    Perforce,
}

impl Vcs {
    /// The CLI this backend drives. Used in reviewer- and caller-facing prose so a message
    /// never says "git" to a Perforce user or the reverse.
    pub fn cli(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Perforce => "p4",
        }
    }

    /// Human name of the system, for prose that reads better than the bare CLI name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Perforce => "Perforce",
        }
    }

    /// The read-only history commands a shelled reviewer can run for itself, named so the
    /// reviewer prompt points it at the right tool rather than at git's.
    pub fn read_commands_phrase(self) -> &'static str {
        match self {
            Self::Git => "`git diff`, `git log` and `git show`",
            Self::Perforce => "`p4 describe`, `p4 opened` and `p4 diff`",
        }
    }

    /// Parse a `--vcs` value. `auto` returns `None` -- it is resolved from the filesystem
    /// after the working root is known, not here.
    fn parse_arg(s: &str) -> Result<Option<Self>, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(None),
            "git" => Ok(Some(Self::Git)),
            "perforce" | "p4" => Ok(Some(Self::Perforce)),
            other => Err(format!(
                "unknown --vcs '{other}' (expected 'auto', 'git' or 'perforce')"
            )),
        }
    }
}

/// Resolve `--vcs auto` from the filesystem alone.
///
/// git wins if a `.git` entry -- file *or* directory, since worktrees and submodules use a
/// `.git` file -- exists at the working root or any ancestor. Only then, with no git marker
/// anywhere above, is the root treated as Perforce. This never spawns a process: deciding
/// the backend must not itself hit a Perforce server, and a git repository must resolve
/// without any Perforce involvement at all.
fn detect_vcs(cwd: &Path) -> Vcs {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Vcs::Git;
        }
        dir = d.parent();
    }
    Vcs::Perforce
}

/// Built-in tools the Claude reviewer may use at all. Omitting Write, Edit and Bash
/// here removes them from the session entirely, so the model has nothing to attempt.
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
pub const DEFAULT_CLAUDE_TOOLS: &str = "Read,Grep,Glob";

/// Permission rules confining the Claude reviewer's reads to the working root.
///
/// Bare `Read`/`Grep`/`Glob` grants are not path-scoped: they permit any absolute path
/// the user can read, which makes the boundary "cannot write" rather than "cannot leave
/// the repository". Scoping each rule fixes that. Verified for all three tools: reading,
/// grepping and globbing `C:\Windows` were each denied, while the project root and its
/// subdirectories stayed readable.
///
/// The scope is deliberately *relative*. These rules are gitignore-style globs, so
/// interpolating an absolute path into one makes the path's own characters significant: a
/// project at `C:\work\[ab]` would produce `Read(//C:/work/[ab]/**)`, where `[ab]` is a
/// character class matching the siblings `C:\work\a` and `C:\work\b` — simultaneously
/// failing to read the real project and granting reads outside it. Absolute paths also
/// have to survive UNC roots and drive roots. `./**` sidesteps all of it: nothing is
/// interpolated, so there is nothing to escape. The reviewer's working directory is set
/// to the project root, which is what `.` resolves against.
///
/// Each rule is returned separately and passed as its own argument, so a project path
/// containing spaces or commas cannot be split apart by the CLI's list parsing.
pub fn scoped_claude_rules() -> Vec<String> {
    ["Read", "Grep", "Glob"]
        .iter()
        .map(|tool| format!("{tool}(./**)"))
        .collect()
}

/// Whether one `--allow-tools` value grants Bash.
///
/// Three shapes have to work. Our defaults arrive as separate scoped entries; a
/// user-supplied list arrives as one string for the CLI to split, and the CLI accepts both
/// whitespace and commas as separators. Getting this wrong is not symmetrical -- a missed
/// grant only costs a redundant capture, while a false one withholds the diff from a
/// reviewer that cannot fetch it -- but a false negative still tells the caller the reviewer
/// has no shell when it has one, so both are worth getting right.
///
/// Separators are recognised at paren depth zero only, since a grant's own argument may
/// contain either: `Bash(git diff:*)` has a space in it, and a pattern may have a comma.
/// Entries are compared whole, so `BashOutput` -- which reads a background shell's output
/// and can run nothing -- is not mistaken for a shell.
fn permits_bash(rule: &str) -> bool {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut grants = Vec::new();
    for (i, ch) in rule.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                grants.push(&rule[start..i]);
                start = i + ch.len_utf8();
            }
            c if depth == 0 && c.is_whitespace() => {
                grants.push(&rule[start..i]);
                start = i + ch.len_utf8();
            }
            _ => {}
        }
    }
    grants.push(&rule[start..]);
    grants
        .into_iter()
        .map(str::trim)
        .any(|grant| grant == "Bash" || grant.starts_with("Bash("))
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;
pub const DEFAULT_WAIT_SECS: u64 = 60;
pub const MAX_WAIT_SECS: u64 = 300;

/// Refuse to resume a review session idle longer than this. Past this window the reviewer's
/// prompt cache may no longer be warm -- its lifetime depends on how the CLI is
/// authenticated (an hour on a subscription, five minutes on an API key), which this server
/// cannot see -- so a resume risks paying to re-read the whole conversation, and the further
/// a session is from its last turn the more its context may have drifted from the work in
/// front of it. The default sits just under the one-hour lifetime. The caller is told to
/// start fresh rather than silently handed the stale resume. Zero disables the check.
pub const DEFAULT_RESUME_MAX_IDLE_SECS: u64 = 55 * 60;

/// Refuse to resume a review session that has already run this many turns. Every turn
/// re-processes the whole conversation so far, so a long-running session is both expensive
/// and prone to losing the thread. Zero disables the check.
pub const DEFAULT_RESUME_MAX_TURNS: u32 = 10;

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
    /// Refuse to resume a session idle longer than this. Zero disables the check. See
    /// `DEFAULT_RESUME_MAX_IDLE_SECS`. A tripped session is refused with
    /// SESSION_NOT_RESUMABLE, not silently restarted, so the caller decides to start fresh.
    pub resume_max_idle: Duration,
    /// Refuse to resume a session that has already run this many turns. Zero disables the
    /// check. See `DEFAULT_RESUME_MAX_TURNS`.
    pub resume_max_turns: u32,
    pub state_dir: PathBuf,
    /// Codex sandbox policy.
    pub sandbox: String,
    /// Claude permission rules, one per entry. Kept as separate strings so each reaches
    /// the CLI as its own argument and a path with spaces cannot be mis-split.
    pub allowed_tools: Vec<String>,
    pub tools: String,
    pub preamble: Option<String>,
    pub no_preamble: bool,
    /// Run the reviewer without the configuration it would normally pick up from the
    /// project and the user.
    ///
    /// This is a security boundary, not tidiness. A reviewed repository can commit
    /// configuration that executes commands: a `.claude/settings.json` hook runs a shell
    /// command automatically, outside the tool allow-list entirely. Verified -- a
    /// `SessionStart` hook committed to a project ran on a plain `claude -p` invocation.
    /// It also stops a reviewer that has cross-review registered from recursing into it.
    ///
    /// The cost is that the reviewer loses project context it might have wanted, notably
    /// CLAUDE.md. That file is attacker-controlled text aimed straight at the reviewer,
    /// so excluding it is defensible on its own terms; pass the conventions that matter
    /// in `instructions` instead.
    pub isolate_reviewer: bool,
    /// Append a line to `usage-<machine>.jsonl` in the state directory for every
    /// finished turn. Named for the machine so several machines' logs can share one
    /// directory; the reader takes every `usage*.jsonl` it finds there.
    ///
    /// On by default. The data is entirely local -- token counts the reviewer CLI already
    /// reported, plus sizes and timings this server already knows -- and without it the
    /// cost of a review is invisible to the tool that caused it. `--no-metrics` turns it
    /// off for anyone who would rather the server wrote one less file.
    pub metrics: bool,
    /// What the server captures and hands the reviewer as "the change".
    ///
    /// Defaults to `auto`, which supplies a working-tree diff only when the reviewer has
    /// no shell to fetch one itself. That closes a real asymmetry: most reviews are
    /// reviews *of a change*, and without this the caller of a shell-less reviewer had to
    /// paste the diff into `instructions` -- spending its own context on it, missing
    /// untracked files, and getting a confident review of the current tree when it forgot.
    ///
    /// Meaningful only when `vcs == Git`. The Perforce backend is driven by the per-call
    /// `change` request argument, not by anything on the server entry -- there is no
    /// launch-time changelist default.
    pub diff: DiffMode,
    /// Which VCS the capture backend drives, resolved from `--vcs` (default `auto`).
    pub vcs: Vcs,
    /// On a resumed turn, send the reviewer only what changed since its previous turn rather
    /// than the whole captured change again.
    ///
    /// A re-review resumes the reviewer's own conversation, so the earlier full change is still
    /// in its context; re-sending it every turn just pays to re-cache a near-duplicate, and for
    /// the Claude reviewer that cache write is billed at a premium, so the per-turn cost climbs
    /// even when nothing but the fixes changed. Reviewing only the delta collapses that.
    ///
    /// Backend-agnostic: one switch governs incremental resume for whichever backend is active.
    /// The two backends realise it differently, because their change objects differ:
    ///
    /// - **Git** captures only the commits added since the previous turn (`<prior-HEAD>..HEAD`),
    ///   guarded by an ancestry check: a rewritten branch (rebase, amend, force-push) whose
    ///   prior commit is no longer an ancestor of HEAD falls back to the full range.
    /// - **Perforce** re-captures the changelist(s) each turn (a pending changelist mutates in
    ///   place, so there is no prior revision to delta against) and collapses files that are
    ///   byte-identical to what the reviewer was already shown, guarded by a per-file
    ///   fingerprint. See `docs/perforce-resume-delta.md`.
    ///
    /// `--no-incremental-resume` turns it off for both.
    pub resume_incremental_diff: bool,
}

impl Config {
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let mut reviewer: Option<ReviewerKind> = None;
        let mut model: Option<String> = None;
        let mut effort: Option<String> = None;
        let mut bin: Option<PathBuf> = None;
        let mut cwd: Option<PathBuf> = None;
        let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
        let mut resume_max_idle_secs = DEFAULT_RESUME_MAX_IDLE_SECS;
        let mut resume_max_turns = DEFAULT_RESUME_MAX_TURNS;
        let mut state_dir: Option<PathBuf> = None;
        let mut sandbox = "read-only".to_string();
        let mut allowed_tools: Option<String> = None;
        let mut tools: Option<String> = None;
        let mut preamble_file: Option<PathBuf> = None;
        let mut no_preamble = false;
        let mut isolate_reviewer = true;
        let mut metrics = true;
        let mut resume_incremental_diff = true;
        // `--diff` is parsed *after* the loop, because how its value is interpreted (and
        // whether it is even legal) depends on `--vcs`, which may appear later on the command
        // line. Kept raw until the backend is known.
        let mut diff_raw: Option<String> = None;
        let mut vcs_arg: Option<Vcs> = None;

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
                "--session-max-turns" => {
                    let v = take("--session-max-turns")?;
                    resume_max_turns = v.parse().map_err(|_| {
                        format!("--session-max-turns must be an integer (0 disables), got '{v}'")
                    })?;
                }
                "--session-max-idle-seconds" => {
                    let v = take("--session-max-idle-seconds")?;
                    resume_max_idle_secs = v.parse().map_err(|_| {
                        format!(
                            "--session-max-idle-seconds must be an integer (0 disables), got '{v}'"
                        )
                    })?;
                }
                "--state-dir" => state_dir = Some(PathBuf::from(take("--state-dir")?)),
                "--sandbox" => sandbox = take("--sandbox")?,
                "--allow-tools" | "--allowed-tools" => allowed_tools = Some(take("--allow-tools")?),
                "--tools" => tools = Some(take("--tools")?),
                "--diff" => diff_raw = Some(take("--diff")?),
                "--vcs" => vcs_arg = Vcs::parse_arg(&take("--vcs")?)?,
                "--preamble-file" => preamble_file = Some(PathBuf::from(take("--preamble-file")?)),
                "--no-preamble" => no_preamble = true,
                // The original name only spoke of MCP; kept working because it is in
                // published example configs.
                "--allow-reviewer-config" | "--allow-reviewer-mcp" => isolate_reviewer = false,
                "--no-metrics" => metrics = false,
                "--no-incremental-resume" => resume_incremental_diff = false,
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

        // `--vcs auto` (or unset) resolves from the filesystem now that `cwd` is known. This
        // is the only place a backend is chosen, and it spawns nothing.
        let vcs = vcs_arg.unwrap_or_else(|| detect_vcs(&cwd));

        // `--diff` is git-specific; passing it under Perforce is a configuration mistake
        // worth failing on rather than silently ignoring. The Perforce backend has no
        // launch-time flag of its own -- the changelists are named per call in the `change`
        // request argument -- so there is nothing symmetrical to reject here for git.
        let diff = match vcs {
            Vcs::Git => match diff_raw {
                Some(s) => DiffMode::parse(&s)?,
                None => DiffMode::Auto,
            },
            Vcs::Perforce => {
                if diff_raw.is_some() {
                    return Err(
                        "--diff applies to git; the working root resolved to Perforce. \
                         Name the changelists to review in the `change` argument of \
                         cross_model_review, or pass --vcs git."
                            .into(),
                    );
                }
                DiffMode::Auto
            }
        };

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

        // Computed before the struct literal because the default rules are derived from
        // `cwd`, which the literal moves.
        // A user-supplied list is passed through as one argument; the CLI splits it.
        let allowed_tools = match allowed_tools {
            Some(list) => vec![list],
            None => scoped_claude_rules(),
        };
        let state_dir = state_dir.unwrap_or_else(|| default_state_dir(&cwd));

        Ok(Self {
            reviewer,
            model: model.unwrap_or_else(|| reviewer.default_model().to_string()),
            effort,
            bin,
            state_dir,
            cwd,
            timeout: Duration::from_secs(timeout_secs),
            resume_max_idle: Duration::from_secs(resume_max_idle_secs),
            resume_max_turns,
            sandbox,
            allowed_tools,
            tools: tools.unwrap_or_else(|| DEFAULT_CLAUDE_TOOLS.to_string()),
            preamble,
            no_preamble,
            isolate_reviewer,
            metrics,
            diff,
            vcs,
            resume_incremental_diff,
        })
    }

    /// True when the reviewer has any shell at all.
    pub fn reviewer_has_shell(&self) -> bool {
        match self.reviewer {
            // Codex runs under a sandbox policy rather than a tool allow-list, so its
            // shell is always present and always write-denied.
            ReviewerKind::Codex => true,
            // Claude's shell has to be both present *and* permitted, and those are two
            // different flags. `--tools ...,Bash` puts the tool in the session;
            // `--allowed-tools` decides what may run, and the reviewer runs under
            // `--permission-mode dontAsk`, so a rule that does not mention Bash denies it
            // outright rather than prompting. `--tools ...,Bash` on its own therefore gives
            // the reviewer a tool it can never use -- and answering "yes, it has a shell"
            // there makes `--diff auto` withhold the capture too, leaving it with neither.
            //
            // So the answer is conservative in the direction that costs nothing: unless a
            // shell is established, the diff gets supplied.
            //
            // Entries are compared whole, not as substrings: `BashOutput` is a separate
            // tool that reads a background shell's output and can run nothing.
            // Both halves compare exactly, and they must agree: a case-insensitive tool
            // check against a case-sensitive rule check made `--tools ...,bash
            // --allow-tools bash` "in the session but not permitted", which is a
            // contradiction rather than an answer. Exact is the right convention of the
            // two, because the CLI's own tool names are exact -- so `bash` most likely
            // enables nothing, and calling that a shell would be the unsafe direction.
            ReviewerKind::Claude => {
                let in_session = self.tools.split(',').any(|tool| tool.trim() == "Bash");
                in_session && self.allowed_tools.iter().any(|rule| permits_bash(rule))
            }
        }
    }

    /// Whether this configuration intends to hand the reviewer a change.
    ///
    /// Intent only: whether one actually arrives depends on the working root really being a
    /// repository of that kind, which is a runtime question. For git, `auto` supplies a diff
    /// exactly when the reviewer cannot fetch one itself, so a shelled reviewer is left to do
    /// its own looking rather than handed a stale snapshot alongside live access. For
    /// Perforce the changelists are named per call and always captured, so the intent is
    /// always to supply -- whether a change actually arrives depends on the capture, which is
    /// the runtime question this caller-facing intent does not promise.
    pub fn supplies_change(&self) -> bool {
        // Each backend matches exhaustively: a new mode or backend states its answer here
        // rather than opting itself in by falling through a wildcard.
        match self.vcs {
            Vcs::Git => match self.diff {
                DiffMode::None => false,
                DiffMode::Auto => !self.reviewer_has_shell(),
                DiffMode::Staged | DiffMode::Head | DiffMode::Rev(_) => true,
            },
            // The `change` argument is required for a Perforce review (enforced in `tools.rs`
            // before a job is created), so a Perforce backend always intends to hand over a
            // change.
            Vcs::Perforce => true,
        }
    }

    /// Whether the reviewer can fetch the change/history itself with its own tools.
    ///
    /// This is what decides whether the prompt may tell the reviewer to go and look. It is
    /// not the same as "a change was supplied": a shelled Codex reviewer can run `git log`
    /// even when a diff is also handed over.
    ///
    /// For Perforce it is much stricter than merely having a shell, and deliberately
    /// conservative. `p4 describe` has to reach the server *and* be a permitted command, and:
    /// a Codex reviewer only has network under `danger-full-access` (the default read-only
    /// sandbox denies it -- verified: p4 came back policy-blocked); a Claude reviewer's shell
    /// is a prefix allow-list that need not include `p4` at all (a `Bash(git diff:*)` grant
    /// does not), cannot be reliably inspected for it, and would be client-less here anyway.
    /// So p4 self-serve is promised to no Claude reviewer and only to a networked Codex one --
    /// erring toward "rely on the captured change", which is always the safe direction.
    pub fn reviewer_can_self_serve_change(&self) -> bool {
        match self.vcs {
            Vcs::Git => self.reviewer_has_shell(),
            Vcs::Perforce => {
                matches!(self.reviewer, ReviewerKind::Codex)
                    && self.sandbox.trim() == "danger-full-access"
            }
        }
    }

    /// What the *caller* is told the capture will contain, per backend.
    ///
    /// The tool description needs the shape of the capture so the calling agent knows what to
    /// put in `instructions`. Git derives it from the `--diff` mode; Perforce from the
    /// changelist list. Kept here, next to `supplies_change`, so a backend cannot advertise a
    /// capture it does not perform.
    pub fn capture_caller_summary(&self) -> (String, String) {
        match self.vcs {
            Vcs::Git => {
                let (captures, caveat) = self.diff.caller_summary();
                (captures, caveat.to_string())
            }
            Vcs::Perforce => (
                "the changelist(s) you name in `change`: for each, its diff, a listing of the \
                 opened or affected files, the contents of files opened for add, and the \
                 changelist description"
                    .to_string(),
                "Note that only the named changelists are covered; files edited without \
                 `p4 edit`, and work in other changelists, are not shown, and a submitted \
                 changelist's diff is a server revision that the live tree may differ from."
                    .to_string(),
            ),
        }
    }

    /// What the reviewer can actually read and run, in its own words.
    ///
    /// This has to be generated rather than fixed: the Claude reviewer has no shell by
    /// default, and a preamble that promised `git diff` was straightforwardly false. A
    /// reviewer that believes it can run git will burn its turn finding out otherwise,
    /// and a reviewer with no shell cannot compute a diff at all -- so it needs telling
    /// to say that plainly instead of guessing at what changed.
    ///
    /// `diff_supplied` is the *runtime* answer, not `supplies_change()`: a configured change
    /// that could not be captured (no git/p4, not a repository) must not be announced, or the
    /// reviewer goes looking below for a section that is not there.
    pub fn reviewer_capabilities(&self, diff_supplied: bool) -> String {
        let mut out = String::new();
        match self.reviewer {
            ReviewerKind::Codex if self.reviewer_can_self_serve_change() => {
                out.push_str(&format!(
                    "You can read any file in this project and inspect the change history \
                     yourself, but only through simple, single read commands: {}, ripgrep \
                     (`rg`), and `Get-Content`/`cat`. Run ONE command per call and keep it \
                     simple -- do not chain commands with `;`, do not pipe them together, and do \
                     not use `git grep` or `git ls-files`. The non-interactive CLI refuses \
                     chained, piped, and non-allowed forms, and a refusal is final. If a command \
                     is refused, do not repeat it or a near-variant: reach for a simpler allowed \
                     command that gets the same thing -- `rg PATTERN` in place of `git grep`, \
                     reading a specific file in place of listing -- and carry on reviewing with \
                     what works. Do not abandon the review because one command was refused; note \
                     something under \"What I could not check\" only when no allowed command can \
                     get it. Writes are blocked by the sandbox.",
                    self.vcs.read_commands_phrase(),
                ));
            }
            ReviewerKind::Codex => {
                // Reached for Perforce under a sandbox that denies network: `p4` needs to
                // reach the server, so telling the reviewer it can inspect the changelist
                // itself would be false (verified: p4 came back policy-blocked). It still has
                // a shell for direct reads of the working tree and ripgrep.
                // Whether to rely on a captured change is decided by the diff_supplied tail
                // below, not asserted here: capture can fail (no client, p4 absent), and a
                // flat "rely on the change below" would point at a section that is not there.
                out.push_str(&format!(
                     "You can read any file in this project through simple, single read commands: \
                     ripgrep (`rg`) and `Get-Content`/`cat`. Run ONE command per call and keep it \
                     simple -- do not chain commands with `;`, do not pipe them together, and do \
                     not use `git grep` or `git ls-files`. The non-interactive CLI refuses \
                     chained, piped, and non-allowed forms, and a refusal is final. If a command \
                     is refused, do not repeat it or a near-variant: reach for a simpler allowed \
                     command that gets the same thing -- `rg PATTERN` in place of `git grep`, \
                     reading a specific file in place of listing -- and carry on with what works, \
                     rather than abandoning the review. Writes are blocked by the sandbox. Running \
                     `{}` needs to reach the {} server, which this sandbox's network policy denies, \
                     so you cannot inspect the changelist yourself.",
                    self.vcs.cli(),
                    self.vcs.name(),
                ));
            }
            ReviewerKind::Claude if self.reviewer_has_shell() => {
                // Not "read-only", unlike the Codex arm above. That one is a sandbox
                // policy; this one is a prefix allow-list, which this file's own
                // `DEFAULT_CLAUDE_TOOLS` documents cannot express read-only -- verified,
                // with `Bash(git diff:*)` permitting `git diff --output=<file>` and
                // creating a file. Telling the reviewer otherwise would be handing it a
                // false security boundary in the one message it has to trust.
                out.push_str(
                    "You can read and search files in this project, and run the shell commands \
                     that have been allow-listed -- that list is a prefix match, not a \
                     read-only guarantee, so do not treat it as one. Anything outside it is \
                     denied rather than queued for approval, so a refusal is final -- note it \
                     and move on.",
                );
            }
            ReviewerKind::Claude => {
                out.push_str(&format!(
                    "You can read and search files in this project, and nothing else: Read, Grep \
                     and Glob, scoped to this directory tree.\n\n\
                     You have no shell. You cannot run `{}`, so you cannot obtain the change \
                     history yourself, and you cannot reconstruct it from the version-control \
                     metadata with the tools you have.",
                    self.vcs.cli(),
                ));
            }
        }

        if diff_supplied {
            out.push_str(
                "\n\nYou do not need a shell for the change itself: it was captured for you and \
                 appears below under \"Change under review\". Review that, not your guess at what \
                 changed. If judging it needs history the section does not include, say so under \
                 \"What I could not check\".",
            );
        } else if !self.reviewer_can_self_serve_change() {
            // Not merely `!reviewer_has_shell()`: a Codex reviewer under a no-network sandbox
            // has a shell but still cannot reach Perforce, so with no captured change it is in
            // the same position as a shell-less reviewer and needs the same guidance.
            out.push_str(
                "\n\nIf the request depends on seeing what changed and the diff was not included \
                 in it, review the current state of the code and say plainly, under \"What I \
                 could not check\", that you had no access to the diff. Do not guess at what \
                 changed.",
            );
        }
        out
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

pub fn fnv1a64(s: &str) -> u64 {
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
                              default: claude -> claude-opus-4-8, codex -> gpt-5.6-luna
  --effort <level>            Reasoning effort.
                              claude: low|medium|high|xhigh|max          (default medium)
                              codex:  low|medium|high|xhigh|max|ultra    (default max)
  --bin <path>                Path to the reviewer CLI. Default: resolved from PATH.
  --cwd <path>                Working root for the reviewer. Default: this process's cwd.
  --timeout-seconds <n>       Hard kill for a single review turn. Default: 1800.
  --session-max-turns <n>     Refuse to resume a review session once it has run this many
                              turns; the caller must start fresh (fresh=true) or use a new
                              session name. Each turn re-processes the whole conversation, so
                              a long session grows expensive and prone to drift. 0 disables.
                              Default: 10.
  --session-max-idle-seconds <n>
                              Refuse to resume a review session idle longer than this many
                              seconds. Past it the reviewer's prompt cache may no longer be
                              warm and its context may have drifted, so a resume risks
                              re-reading the whole conversation. The caller is told to start
                              fresh rather than silently handed the stale resume. 0 disables.
                              Default: 3300 (55 minutes, just under the 1h cache lifetime).
  --state-dir <path>          Where named sessions are recorded.
                              Default: %LOCALAPPDATA%\cross-review\<project>-<hash>
  --sandbox <mode>            Codex sandbox policy. Default: read-only.
  --tools <list>              Claude built-in tools. Default: Read,Grep,Glob
                              (no Bash: see the README on why a prefix allow-list
                              cannot express read-only). Bash here is not enough on
                              its own -- it also needs an --allow-tools rule, since
                              the reviewer runs under dontAsk and is otherwise
                              handed a tool it can never call.
  --allow-tools <list>        Claude permission rules. Default: Read/Grep/Glob scoped
                              to the working root, so reads cannot leave the project.
  --vcs <auto|git|perforce>   Which version control the capture backend drives.
                              auto    git if a .git entry exists at or above the working
                                      root, else Perforce. Filesystem-only: never runs p4
                                      to decide. (default)
                              git     git backend, configured by --diff.
                              perforce  Perforce backend. The changelists to review are
                                      named per call in the cross_model_review `change`
                                      argument (there is no launch-time changelist flag).
  --diff <spec>               git only: what to capture and hand the reviewer as "the
                              change". Rejected under --vcs perforce (name changelists in
                              the `change` request argument there).
                              auto    supply a working-tree diff only when the reviewer
                                      has no usable shell to fetch one itself -- i.e.
                                      Claude without Bash both enabled and allow-listed.
                                      The Codex reviewer always has one, so auto supplies
                                      nothing there. (default)
                              none    supply nothing; paste your own into 'instructions'
                              staged  git diff --cached
                              HEAD    git diff HEAD, plus untracked file contents
                              a..b    two commits, e.g. main...HEAD: no working tree,
                                      no untracked files
                              <rev>   that commit against the WORKING TREE, e.g. HEAD~3,
                                      plus untracked file contents -- git's own semantics,
                                      not ours. Two spellings are rejected because nothing
                                      distinguishes them from the other shape: revision-set
                                      shorthand (^!, ^@, ^-), which is a range with no ..
                                      to see, and :/<pattern> containing .., which is a
                                      range whose left end is a message search.
                              A capture that was configured and could not be produced is
                              reported to the caller with the review, not skipped in
                              silence. Not affected by --no-preamble; use --diff none.
  --no-incremental-resume     on a resumed turn, send the WHOLE captured change again
                              instead of only what changed since the reviewer's previous
                              turn. The incremental default resumes the reviewer's own
                              conversation, so the earlier change is still in its context,
                              and re-sending it every turn pays to re-cache a near-duplicate
                              -- billed at a premium for the Claude reviewer. Both backends
                              honour it: git sends only the commits added since the prior
                              turn (a rewritten branch falls back to the full range on its
                              own); Perforce collapses files byte-identical to what the
                              reviewer was already shown. This flag forces full capture.
  --preamble-file <path>      Replace the built-in reviewer preamble.
  --no-preamble               Send the caller's instructions with no preamble at all.
  --allow-reviewer-config     Let the reviewer load project and user configuration
                              (hooks, settings, plugins, skills, MCP servers, CLAUDE.md).
                              Off by default, and it is a security boundary: a reviewed
                              repository can commit a hook that runs a shell command
                              with no tool call and so no permission check. Only pass
                              this for repositories you already trust.
                              (--allow-reviewer-mcp is an accepted older name.)
  --no-metrics                Stop recording per-turn token usage to
                              usage-<machine>.jsonl in the state directory. On by
                              default; the data is local and is what makes "where did
                              the usage go?" answerable.

OTHER:
  --doctor                    Check the reviewer CLI and auth, then exit.
  --usage                     Print the recorded per-turn usage summary, then exit.
                              Reads every usage*.jsonl in the state directory, so
                              point --state-dir at a copied-together directory to roll
                              up several machines at once.
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
        assert_eq!(claude.model, "claude-opus-4-8");
        assert_eq!(claude.effort, "medium");

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(codex.model, "gpt-5.6-luna");
        assert_eq!(codex.effort, "max");
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
    fn incremental_resume_is_on_by_default_and_can_be_turned_off() {
        let cfg = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert!(
            cfg.resume_incremental_diff,
            "incremental resume is the default"
        );

        let cfg = Config::from_args(&args(&["--reviewer", "claude", "--no-incremental-resume"]))
            .expect("config");
        assert!(!cfg.resume_incremental_diff);
    }

    #[test]
    fn resume_limits_default_override_and_disable() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.resume_max_turns, DEFAULT_RESUME_MAX_TURNS);
        assert_eq!(cfg.resume_max_idle.as_secs(), DEFAULT_RESUME_MAX_IDLE_SECS);

        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--session-max-turns",
            "3",
            "--session-max-idle-seconds",
            "120",
        ]))
        .expect("config");
        assert_eq!(cfg.resume_max_turns, 3);
        assert_eq!(cfg.resume_max_idle.as_secs(), 120);

        // Zero on either disables that check rather than being rejected.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--session-max-turns",
            "0",
            "--session-max-idle-seconds",
            "0",
        ]))
        .expect("config");
        assert_eq!(cfg.resume_max_turns, 0);
        assert!(cfg.resume_max_idle.is_zero());

        // A non-integer is a configuration mistake, not a value to guess at.
        assert!(Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--session-max-turns",
            "lots"
        ]))
        .is_err());
        assert!(Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--session-max-idle-seconds",
            "soon"
        ]))
        .is_err());
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
    fn reviewer_isolation_is_on_unless_opted_out() {
        assert!(
            Config::from_args(&args(&["--reviewer", "codex"]))
                .unwrap()
                .isolate_reviewer
        );
        // Both the current name and the older one must turn it off: the older one is in
        // published example configs.
        for flag in ["--allow-reviewer-config", "--allow-reviewer-mcp"] {
            assert!(
                !Config::from_args(&args(&["--reviewer", "codex", flag]))
                    .unwrap()
                    .isolate_reviewer,
                "{flag} should disable isolation"
            );
        }
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
            !cfg.allowed_tools.iter().any(|rule| rule.contains("Bash")),
            "no Bash pattern may be pre-approved by default: {:?}",
            cfg.allowed_tools
        );
    }

    #[test]
    fn default_read_rules_are_scoped_to_the_working_root() {
        // Bare Read/Grep/Glob grants are not path-scoped, which would make the boundary
        // "cannot write" rather than "cannot leave the project".
        let cfg = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert_eq!(cfg.allowed_tools.len(), 3);
        for tool in ["Read", "Grep", "Glob"] {
            let expected = format!("{tool}(./**)");
            assert!(
                cfg.allowed_tools.contains(&expected),
                "expected {expected} in {:?}",
                cfg.allowed_tools
            );
        }
    }

    #[test]
    fn scoped_rules_interpolate_no_path() {
        // Rules are gitignore-style globs, so an absolute path would make its own
        // characters significant: a project at C:\work\[ab] would yield a character class
        // matching the siblings C:\work\a and C:\work\b. Relative scoping avoids the
        // whole escaping problem, so no rule may contain a drive letter or separator.
        for rule in scoped_claude_rules() {
            assert!(!rule.contains(':'), "{rule} interpolates a path");
            assert!(!rule.contains('\\'), "{rule} interpolates a path");
            assert!(rule.ends_with("(./**)"), "{rule}");
        }
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
        // Passed through as a single argument for the CLI to split itself.
        assert_eq!(cfg.allowed_tools, vec!["Read Grep Glob Bash(git diff:*)"]);
        assert!(cfg.reviewer_has_shell());
    }

    /// A Claude shell needs the tool *and* a permission rule, and getting this wrong is
    /// expensive in one specific direction: `--diff auto` withholds the capture from a
    /// reviewer believed to have a shell, so a Bash that `dontAsk` denies would leave it
    /// with no shell and no diff at all.
    #[test]
    fn a_claude_shell_needs_both_the_tool_and_a_rule_permitting_it() {
        // In the session, but nothing permits it: the default rules are Read/Grep/Glob, and
        // the reviewer runs under `--permission-mode dontAsk`, so Bash is denied.
        let listed_only = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
        ]))
        .expect("config");
        assert!(!listed_only.reviewer_has_shell());
        assert!(
            listed_only.supplies_change(),
            "and the diff must be supplied, since it has no usable shell"
        );

        // Permitted but absent from the session is the same answer for the other reason.
        let permitted_only = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ]))
        .expect("config");
        assert!(!permitted_only.reviewer_has_shell());

        // A bare `Bash` grant counts, as well as a scoped one.
        let bare = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Bash",
        ]))
        .expect("config");
        assert!(bare.reviewer_has_shell());

        // The CLI accepts commas as well as whitespace in a supplied list, and a grant's
        // own argument may contain either, so separators count only outside parentheses.
        for list in [
            "Read,Grep,Glob,Bash(git diff:*)",
            "Read Grep Glob Bash(git log --format=a,b)",
            "Bash",
        ] {
            let cfg = Config::from_args(&args(&[
                "--reviewer",
                "claude",
                "--tools",
                "Read,Grep,Glob,Bash",
                "--allow-tools",
                list,
            ]))
            .expect("config");
            assert!(cfg.reviewer_has_shell(), "{list}");
        }

        // `BashOutput` reads a background shell's output and can run nothing, so neither
        // half of the question may match it as a substring.
        let lookalike = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,BashOutput",
            "--allow-tools",
            "Read,BashOutput",
        ]))
        .expect("config");
        assert!(!lookalike.reviewer_has_shell());
    }

    #[test]
    fn capabilities_are_stated_truthfully_per_reviewer() {
        // A reviewer told it can run git when it cannot wastes its turn discovering that,
        // and the caller is never told the diff had to be supplied. This was observed:
        // a real review reported it could not run git diff, git log or git show.
        let claude = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert!(!claude.reviewer_has_shell());
        let text = claude.reviewer_capabilities(false);
        assert!(text.contains("no shell"), "{text}");
        assert!(text.contains("cannot run `git`"), "{text}");
        // And it must be told to say so rather than guess at the change.
        assert!(text.contains("Do not guess"), "{text}");

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert!(codex.reviewer_has_shell());
        let text = codex.reviewer_capabilities(false);
        assert!(text.contains("git diff"), "{text}");
        assert!(!text.contains("no shell"), "{text}");
        // Codex's shell has its writes denied by the OS sandbox, but the CLI may still
        // refuse a command form in non-interactive mode. A small model at low effort batched
        // its whole reconnaissance into one compound command, had it refused wholesale, and
        // gave up -- so the guidance steers it to one simple command at a time and to fall
        // back rather than abandon the review.
        assert!(text.contains("ONE command per call"), "{text}");
        assert!(text.contains("a refusal is final"), "{text}");
        assert!(text.contains("carry on reviewing with"), "{text}");
    }

    /// The prompt the reviewer itself reads must not claim a boundary the mechanism does
    /// not provide. Codex's shell is a sandbox policy; Claude's is a prefix allow-list,
    /// which `DEFAULT_CLAUDE_TOOLS` documents cannot express read-only -- verified, with
    /// `Bash(git diff:*)` permitting `--output=<file>` and creating a file.
    #[test]
    fn an_opted_in_claude_shell_is_never_described_to_it_as_read_only() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ]))
        .expect("config");
        assert!(cfg.reviewer_has_shell());

        let text = cfg.reviewer_capabilities(false);
        // The word may appear only where it is being denied, never as a description of
        // what the shell is.
        assert!(!text.contains("read-only shell"), "{text}");
        assert!(!text.contains("run the read-only"), "{text}");
        assert!(text.contains("prefix match"), "{text}");
        assert!(text.contains("not a read-only guarantee"), "{text}");
        // The denial behaviour still has to be stated: refusals are final, not queued.
        assert!(text.contains("a refusal is final"), "{text}");
    }

    #[test]
    fn a_supplied_diff_replaces_the_no_diff_warning() {
        // The two must never both appear. "You had no access to the diff" alongside a
        // diff is exactly the confusion that makes a reviewer hedge a finding it could
        // have verified.
        let cfg = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        let text = cfg.reviewer_capabilities(true);
        assert!(text.contains("Change under review"), "{text}");
        assert!(!text.contains("Do not guess"), "{text}");
        assert!(!text.contains("no access to the diff"), "{text}");
        // It still has no shell, and still needs to say so about anything else.
        assert!(text.contains("no shell"), "{text}");
    }

    #[test]
    fn auto_supplies_a_diff_only_to_a_reviewer_that_cannot_fetch_one() {
        // The whole asymmetry this closes: Codex can run git diff itself, Claude cannot.
        let claude = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert!(claude.supplies_change());

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert!(!codex.supplies_change());

        // And a Claude reviewer given Bash back -- in the session *and* permitted, since
        // either alone leaves it unable to run anything -- can fetch its own.
        let with_bash = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ]))
        .expect("config");
        assert!(!with_bash.supplies_change());
    }

    #[test]
    fn an_explicit_diff_mode_overrides_the_auto_decision_in_both_directions() {
        // A caller who curates its own diff must be able to turn ours off, and one that
        // wants a specific range must get it even when the reviewer has a shell.
        let off =
            Config::from_args(&args(&["--reviewer", "claude", "--diff", "none"])).expect("config");
        assert!(!off.supplies_change());

        let ranged = Config::from_args(&args(&["--reviewer", "codex", "--diff", "main...HEAD"]))
            .expect("config");
        assert!(ranged.supplies_change());
        assert_eq!(ranged.diff, DiffMode::Rev("main...HEAD".into()));
    }

    #[test]
    fn diff_defaults_to_auto_and_rejects_an_option_shaped_value() {
        let cfg = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert_eq!(cfg.diff, DiffMode::Auto);

        // `git diff --output=<file>` writes, so the value cannot be allowed to become a
        // git option -- the same prefix-matching hole that kept Bash out of the defaults.
        let err = Config::from_args(&args(&["--reviewer", "claude", "--diff", "--output=x"]))
            .unwrap_err();
        assert!(err.contains("git option"), "{err}");
    }

    #[test]
    fn shell_detection_matches_whole_tool_names() {
        // BashOutput reads a background shell's output; it cannot run anything, so it must
        // not be mistaken for shell access by a substring match.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,BashOutput",
        ]))
        .expect("config");
        assert!(!cfg.reviewer_has_shell());
        assert!(cfg.reviewer_capabilities(false).contains("no shell"));
    }

    #[test]
    fn claude_regains_shell_capability_when_bash_is_restored() {
        // Both halves: `--tools` puts Bash in the session, `--allow-tools` permits it.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ]))
        .expect("config");
        assert!(cfg.reviewer_has_shell());
        let text = cfg.reviewer_capabilities(false);
        assert!(!text.contains("no shell"), "{text}");
        assert!(text.contains("allow-listed"), "{text}");
    }

    #[test]
    fn help_text_matches_the_real_defaults() {
        // The help text claimed Bash was default long after it was removed.
        assert!(USAGE.contains("Default: Read,Grep,Glob\n"));
        assert!(!USAGE.contains("Read,Grep,Glob,Bash"));
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

    #[test]
    fn vcs_flag_parses_and_auto_detects_from_the_filesystem() {
        // Explicit values.
        let git = Config::from_args(&args(&["--reviewer", "codex", "--vcs", "git"])).expect("cfg");
        assert_eq!(git.vcs, Vcs::Git);
        for p4 in ["perforce", "p4"] {
            let cfg = Config::from_args(&args(&["--reviewer", "codex", "--vcs", p4])).expect("cfg");
            assert_eq!(cfg.vcs, Vcs::Perforce, "{p4}");
        }
        let err = Config::from_args(&args(&["--reviewer", "codex", "--vcs", "svn"])).unwrap_err();
        assert!(err.contains("--vcs"), "{err}");

        // auto: a directory with a `.git` entry is git, without one is Perforce -- and the
        // detection is filesystem-only, so it does not need a live server.
        let with_git = crate::testutil::temp_dir("cross-review-vcs-git");
        std::fs::create_dir(with_git.join(".git")).expect("mkdir .git");
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--cwd",
            &with_git.to_string_lossy(),
        ]))
        .expect("cfg");
        assert_eq!(cfg.vcs, Vcs::Git);

        let without_git = crate::testutil::temp_dir("cross-review-vcs-p4");
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--cwd",
            &without_git.to_string_lossy(),
        ]))
        .expect("cfg");
        assert_eq!(cfg.vcs, Vcs::Perforce);
    }

    #[test]
    fn diff_under_perforce_is_rejected() {
        // `--diff` is git-specific; the changelists are named per call in `change`, so there
        // is no launch-time Perforce flag to mis-target the other way.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--vcs",
            "perforce",
            "--diff",
            "HEAD",
        ]))
        .unwrap_err();
        assert!(err.contains("--diff"), "{err}");

        // And `--change` no longer exists at all: it is an unknown argument now, not a
        // backend mismatch.
        let err = Config::from_args(&args(&["--reviewer", "codex", "--change", "1"])).unwrap_err();
        assert!(err.contains("--change"), "{err}");
    }

    #[test]
    fn perforce_always_intends_to_supply_a_change() {
        // The changelists are required per call, so a Perforce backend always intends to hand
        // over a change; there is no configured-but-empty state any more.
        let cfg =
            Config::from_args(&args(&["--reviewer", "claude", "--vcs", "perforce"])).expect("cfg");
        assert!(cfg.supplies_change());
    }

    #[test]
    fn perforce_capabilities_and_summary_name_p4_not_git() {
        let cfg =
            Config::from_args(&args(&["--reviewer", "claude", "--vcs", "perforce"])).expect("cfg");

        // The shell-less Claude reviewer is told it cannot run p4, not git.
        let caps = cfg.reviewer_capabilities(false);
        assert!(caps.contains("cannot run `p4`"), "{caps}");
        assert!(!caps.contains("`git`"), "{caps}");

        // The caller summary points at the `change` argument rather than fixed changelists.
        let (captures, caveat) = cfg.capture_caller_summary();
        assert!(captures.contains("`change`"), "{captures}");
        assert!(caveat.contains("p4 edit"), "{caveat}");
    }

    #[test]
    fn a_no_network_codex_reviewer_is_not_promised_perforce_self_serve() {
        // Under the default read-only sandbox, p4 cannot reach the server, so the Codex
        // reviewer must be told to rely on the captured change rather than run p4 itself.
        let read_only =
            Config::from_args(&args(&["--reviewer", "codex", "--vcs", "perforce"])).expect("cfg");
        assert!(!read_only.reviewer_can_self_serve_change());
        let caps = read_only.reviewer_capabilities(true);
        assert!(caps.contains("network policy denies"), "{caps}");
        assert!(
            !caps.contains("inspect the change history yourself"),
            "{caps}"
        );

        // When the capture failed (diff_supplied = false), a network-denied Codex reviewer --
        // which has a shell but cannot reach Perforce -- must still be told it had no diff and
        // not to guess, and must not be told to rely on a captured change that is not there.
        let no_capture = read_only.reviewer_capabilities(false);
        assert!(no_capture.contains("no access to the diff"), "{no_capture}");
        assert!(no_capture.contains("Do not guess"), "{no_capture}");
        assert!(!no_capture.contains("captured for you"), "{no_capture}");

        // A sandbox that grants network restores p4 self-serve.
        let networked = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--vcs",
            "perforce",
            "--sandbox",
            "danger-full-access",
        ]))
        .expect("cfg");
        assert!(networked.reviewer_can_self_serve_change());
        assert!(networked
            .reviewer_capabilities(false)
            .contains("p4 describe"));

        // Git history is local, so a read-only Codex reviewer can still inspect it.
        let git = Config::from_args(&args(&["--reviewer", "codex", "--vcs", "git"])).expect("cfg");
        assert!(git.reviewer_can_self_serve_change());
        assert!(git.reviewer_capabilities(false).contains("git diff"));

        // A Claude reviewer with a git-only Bash allow-list has a shell, but that allow-list
        // need not include p4 (and p4 here would be client-less), so it must NOT be promised
        // Perforce self-serve -- it is told to rely on the captured change instead.
        let claude_bash = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--vcs",
            "perforce",
            "--tools",
            "Read,Grep,Glob,Bash",
            "--allow-tools",
            "Read Grep Glob Bash(git diff:*)",
        ]))
        .expect("cfg");
        assert!(claude_bash.reviewer_has_shell());
        assert!(!claude_bash.reviewer_can_self_serve_change());
    }

    #[test]
    fn codex_capabilities_name_the_active_vcs_not_git() {
        // Even in the network-denied Perforce case, the reviewer is told about p4/Perforce,
        // never git -- a message that named git to a Perforce user would be the wrong CLI.
        let cfg =
            Config::from_args(&args(&["--reviewer", "codex", "--vcs", "perforce"])).expect("cfg");
        let caps = cfg.reviewer_capabilities(false);
        assert!(caps.contains("Perforce"), "{caps}");
        assert!(caps.contains("`p4`"), "{caps}");
        assert!(!caps.contains("git diff"), "{caps}");
    }
}
