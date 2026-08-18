//! Configuration. Everything is supplied as CLI arguments on the MCP server entry,
//! so a project's `.mcp.json` / `config.toml` is the single source of truth and there
//! is no machine-level config file to drift out of sync.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::profile::ProfileSelector;
use crate::reviewer::HeadroomLevel;
use crate::vcs::DiffMode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewerKind {
    Claude,
    Codex,
}

/// A per-entry proactive usage-remaining minimum. Two shapes, one per reviewer family, because
/// the two CLIs expose differently-shaped signals: Codex a numeric remaining percentage, Claude
/// a categorical status. The parser rejects a family/flag mismatch, so a Codex entry only ever
/// carries `Remaining` and a Claude entry only `Status`. `None` (unset) never gates. See
/// `docs/usage-remaining-gate.md`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UsageMinimum {
    /// Never gated (flag unset, or set to a value that never gates).
    None,
    /// Codex: skip the entry when its last-observed remaining is known and below this percentage
    /// (`1..=100`).
    Remaining(u8),
    /// Claude: skip the entry when its last-observed status is below this level (the level named
    /// is the *lowest acceptable* one).
    Status(HeadroomLevel),
}

impl UsageMinimum {
    /// Whether this minimum ever causes a proactive skip. `None` does not; a set value does.
    /// Used to decide whether the chain arms usage observation at all.
    pub fn is_gating(&self) -> bool {
        !matches!(self, UsageMinimum::None)
    }
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

/// A named review "level": a `(model, effort)` preset the caller can select per review at start
/// time. Declared per entry with `--level NAME:MODEL:EFFORT`. Resolving a level overwrites the
/// running entry's `model`/`effort` for that review only; it is deliberately **not** part of the
/// reviewer's identity (see [`ReviewerSpec`]). See `docs/review-levels-plan.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelOverride {
    pub model: String,
    pub effort: String,
}

/// One reviewer entry in the fallback chain: the identity of a reviewer the server may run.
///
/// This is the per-entry slice lifted out of `Config` so a chain can hold an ordered list of
/// them (`Config::reviewers`). Only a reviewer's *identity* lives here — the process-global
/// behaviour flags (`sandbox`, `tools`, `allowed_tools`, `isolate_reviewer`, `preamble`) stay
/// on `Config`, because they are already family-scoped in effect: `--sandbox` is read only by
/// the Codex invocation and `--tools`/`--allow-tools` only by the Claude one, so a global value
/// applies to whichever entries are of that family and is inert for the others.
///
/// See `docs/reviewer-fallback-chain.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewerSpec {
    pub reviewer: ReviewerKind,
    pub model: String,
    pub effort: String,
    /// Explicit path to the reviewer CLI. When absent we resolve it from PATH.
    pub bin: Option<PathBuf>,
    /// Proactive usage-remaining minimum. A *gating policy*, deliberately **not** part of the
    /// reviewer's identity: it is excluded from the fully-identical-duplicate rule
    /// ([`Config::validate_chain`]) and from resume matching ([`Config::resume_entry_index`]),
    /// so a duplicate that differs only by minimum is still a duplicate and editing a threshold
    /// between runs does not break resume. See `docs/usage-remaining-gate.md`.
    pub usage_minimum: UsageMinimum,
    /// Which config home — and thus which account — this entry's reviewer runs under. Part of the
    /// entry's *identity* (like `bin`, which can distinguish an account): a fallback entry carries
    /// its own, duplicate detection and resume matching include it. `Ambient` is today's behaviour
    /// (inherit the environment). See `docs/reviewer-account-profiles.md`.
    pub profile: ProfileSelector,
    /// Named review levels declared for this entry (`--level NAME:MODEL:EFFORT`). Selecting one at
    /// review start overwrites this entry's `model`/`effort` for that review. A *menu*, deliberately
    /// **not** part of identity — excluded from [`same_reviewer_identity`](Self::same_reviewer_identity)
    /// and [`validate_chain`](Config::validate_chain) (like `usage_minimum`), so two entries differing
    /// only in their level menu are still one identity, and editing the menu between runs does not by
    /// itself break resume. Empty means the entry offers no levels (today's behaviour: the fixed
    /// `model`/`effort` is used). See `docs/review-levels-plan.md`.
    pub levels: BTreeMap<String, LevelOverride>,
    /// Which declared level applies when a review omits `level`. `None` falls back to this entry's
    /// fixed `model`/`effort` (backward-compatible). Validated at `finalize` to name a declared
    /// level. Also identity-excluded.
    pub default_level: Option<String>,
}

impl ReviewerSpec {
    /// Identity for the duplicate rule: reviewer, model, effort, and bin -- **not**
    /// `usage_minimum`, which is a gating policy, not identity. `validate_chain` uses this rather
    /// than `==` so two entries differing only by minimum are still caught as duplicates. The bin
    /// is compared by **path identity** (Windows-case- and separator-insensitive, via
    /// `RawBin::identity_matches`), not byte-exactly, so two spellings of the same executable are
    /// one duplicate -- the same rule `resume_entry_index` uses to bind a resume to its creating
    /// entry (#55). See `docs/usage-remaining-gate.md` and `docs/path-comparison-plan.md`.
    pub fn same_reviewer_identity(&self, other: &ReviewerSpec) -> bool {
        self.reviewer == other.reviewer
            && self.model == other.model
            && self.effort == other.effort
            && self.profile == other.profile
            && self.raw_bin().identity_matches(&other.raw_bin())
    }
    /// This entry's binary *as configured*, tagged so a new PATH entry is distinguishable from a
    /// legacy session record with no stored bin. Persisted on the session for resume matching.
    pub fn raw_bin(&self) -> crate::session::RawBin {
        match &self.bin {
            None => crate::session::RawBin::PathSearch,
            Some(path) => crate::session::RawBin::Explicit(path.to_string_lossy().into_owned()),
        }
    }

    /// A short identity label for prompts, errors, and the chain description. Includes the
    /// explicit `--bin` when one is set, so two same-reviewer/same-model entries that differ only
    /// by binary (a distinct install or account) are distinguishable wherever this is shown. A
    /// PATH-resolved entry omits it here because its resolved path is not known from the config
    /// alone; the run path uses [`describe_with_bin`](Self::describe_with_bin) once it has
    /// resolved one, so the *rendered* identity still names the executable that actually ran. Two
    /// PATH entries with the same reviewer/model would be a fully-identical duplicate, which
    /// `validate_chain` rejects, so this cannot be ambiguous.
    pub fn describe(&self) -> String {
        self.describe_bin(&self.model, &self.effort, self.bin.as_deref())
    }

    /// Like [`describe`](Self::describe) but renders `over`'s `(model, effort)` in place of this
    /// entry's base pair, so a headline shown *before* the run starts still names the pair the
    /// review will actually run at when a level override or resume pin is in play (issue #106).
    /// `None` renders the base pair, identical to [`describe`](Self::describe). On the run path the
    /// override has already been folded into the entry's own `model`/`effort` (`effective_entry`),
    /// so `describe`/`describe_with_bin` there already name the effective pair; this exists for the
    /// start response, which is rendered before any entry is published as active.
    pub fn describe_effective(&self, over: Option<&LevelOverride>) -> String {
        let (model, effort) = match over {
            Some(o) => (o.model.as_str(), o.effort.as_str()),
            None => (self.model.as_str(), self.effort.as_str()),
        };
        self.describe_bin(model, effort, self.bin.as_deref())
    }

    /// Like [`describe`](Self::describe) but pins the *resolved* binary rather than only the
    /// configured one, so a PATH-resolved entry still names the executable (and thus the account)
    /// that actually ran. Used on the run path once `self.bin` has been resolved for an entry;
    /// `describe` is kept for the cases where no resolved path is available yet (the chain
    /// listing, or a fallback whose preflight failed before a binary was verified).
    pub fn describe_with_bin(&self, resolved: &Path) -> String {
        self.describe_bin(&self.model, &self.effort, Some(resolved))
    }

    fn describe_bin(&self, model: &str, effort: &str, bin: Option<&Path>) -> String {
        let mut out = format!(
            "{} ({}, model={}, effort={}",
            self.reviewer.vendor(),
            self.reviewer.as_str(),
            model,
            effort,
        );
        if let Some(bin) = bin {
            out.push_str(&format!(", bin={}", bin.display()));
        }
        out.push(')');
        out
    }

    /// The `(model, effort)` a declared level resolves to, or `None` if this entry declares no
    /// level of that name. The single lookup point for level resolution.
    pub fn resolve_level(&self, name: &str) -> Option<&LevelOverride> {
        self.levels.get(name)
    }

    /// Declared level names, sorted (the map is a `BTreeMap`). Used to advertise the level menu in
    /// the MCP schema and in `status`.
    pub fn level_names(&self) -> Vec<&str> {
        self.levels.keys().map(String::as_str).collect()
    }

    /// Whether this entry can *produce* the given `(model, effort)` — either as its fixed base pair
    /// or via one of its declared levels. Resume matching uses this instead of a bare
    /// `model == … && effort == …` so a session that started at a non-default level (whose
    /// persisted pair is the level's, not the base's) still binds back to its creating entry. A
    /// level menu is not identity, so a pair reachable two ways still names one entry; `reviewer`,
    /// `profile`, and bin still constrain the match (see `resume_entry_index`).
    pub fn produces_pair(&self, model: &str, effort: &str) -> bool {
        (self.model == model && self.effort == effort)
            || self
                .levels
                .values()
                .any(|lv| lv.model == model && lv.effort == effort)
    }

    /// A one-line render of this entry's level menu for `status`, or `None` when it declares no
    /// levels. Names the default (or the fixed base pair when no `--default-level` is set), so an
    /// operator can see that, e.g., the default effort is a level's, not the base `--effort` (f6).
    pub fn describe_levels(&self) -> Option<String> {
        if self.levels.is_empty() {
            return None;
        }
        let menu = self
            .levels
            .iter()
            .map(|(name, lv)| format!("{name}={}/{}", lv.model, lv.effort))
            .collect::<Vec<_>>()
            .join(", ");
        let default = match &self.default_level {
            Some(d) => format!("default level '{d}'"),
            None => format!("default = base {}/{}", self.model, self.effort),
        };
        Some(format!("levels: {menu} ({default})"))
    }
}

/// A chain entry under construction during argument parsing.
///
/// Each `--reviewer` starts one; the identity flags bind to the most recent. Setting the same
/// identity flag twice within one entry is rejected — it is almost always a forgotten
/// `--reviewer`, and guessing which value wins would hide the mistake.
struct PendingEntry {
    reviewer: ReviewerKind,
    model: Option<String>,
    effort: Option<String>,
    bin: Option<PathBuf>,
    usage_minimum: Option<UsageMinimum>,
    /// The account profile under construction. `None` becomes `ProfileSelector::Ambient` at
    /// `finalize`. A profile flag and an explicit-home flag for the same family are mutually
    /// exclusive on one entry, so a second setter call is an error, not a precedence contest.
    profile: Option<ProfileSelector>,
    /// Declared `--level` presets for this entry, keyed by name. A `BTreeMap` so a repeated name is
    /// caught (and the menu renders in a stable order).
    levels: BTreeMap<String, LevelOverride>,
    /// `--default-level`, validated at `finalize` to name a declared level.
    default_level: Option<String>,
}

impl PendingEntry {
    fn new(reviewer: ReviewerKind) -> Self {
        Self {
            reviewer,
            model: None,
            effort: None,
            bin: None,
            usage_minimum: None,
            profile: None,
            levels: BTreeMap::new(),
            default_level: None,
        }
    }

    /// The reviewer family a profile/home flag names, checked against this entry so a
    /// `--codex-profile` on a Claude entry (or vice versa) is a clear error rather than silently
    /// binding to the wrong reviewer.
    fn check_profile_family(&self, flag: &str, family: ReviewerKind) -> Result<(), String> {
        if self.reviewer != family {
            return Err(format!(
                "{flag} applies to a {} reviewer, but the --reviewer entry before it is '{}'",
                family.as_str(),
                self.reviewer.as_str()
            ));
        }
        if self.profile.is_some() {
            return Err(format!(
                "a profile or home was given twice for the same --reviewer '{}' (a profile name and \
                 an explicit home are mutually exclusive on one entry)",
                self.reviewer.as_str()
            ));
        }
        Ok(())
    }

    fn set_profile_name(
        &mut self,
        flag: &str,
        family: ReviewerKind,
        name: String,
    ) -> Result<(), String> {
        self.check_profile_family(flag, family)?;
        crate::profile::validate_profile_name(&name)?;
        self.profile = Some(ProfileSelector::Named(name));
        Ok(())
    }

    fn set_profile_home(
        &mut self,
        flag: &str,
        family: ReviewerKind,
        path: PathBuf,
    ) -> Result<(), String> {
        self.check_profile_family(flag, family)?;
        if path.as_os_str().is_empty() {
            return Err(format!("{flag} requires a non-empty path"));
        }
        // Must be absolute: a relative or drive-relative home would resolve against whatever working
        // directory the reviewer child happened to run under, so the same config could bind different
        // accounts and weaken authorization (code-review f4). Symlink canonicalization happens later,
        // at provisioning, once the directory exists.
        if !path.is_absolute() {
            return Err(format!(
                "{flag} requires an absolute path, got '{}'",
                path.display()
            ));
        }
        self.profile = Some(ProfileSelector::ExplicitHome(path));
        Ok(())
    }

    /// Bind `--min-usage-remaining <1..=100>` to this entry. Codex only: Claude exposes no
    /// numeric remaining, so a numeric minimum there would silently collapse to a categorical
    /// decision — rejected rather than mislead. `0`/`101`/non-integer are errors.
    fn set_min_usage_remaining(&mut self, v: &str) -> Result<(), String> {
        if self.reviewer != ReviewerKind::Codex {
            return Err(format!(
                "--min-usage-remaining applies to a numeric-usage reviewer (codex); reviewer '{}' \
                 exposes only a categorical usage status. Use --min-usage-status <ample|warning> \
                 for it.",
                self.reviewer.as_str()
            ));
        }
        if self.usage_minimum.is_some() {
            return Err(format!(
                "a usage minimum was given twice for the same --reviewer '{}' (did you forget a \
                 --reviewer before the second one?)",
                self.reviewer.as_str()
            ));
        }
        let pct: u8 = v.parse().map_err(|_| {
            format!("--min-usage-remaining must be an integer in 1..=100, got '{v}'")
        })?;
        if !(1..=100).contains(&pct) {
            return Err(format!(
                "--min-usage-remaining must be in 1..=100, got {pct} (0 would never gate; omit \
                 the flag instead)"
            ));
        }
        self.usage_minimum = Some(UsageMinimum::Remaining(pct));
        Ok(())
    }

    /// Bind `--min-usage-status <ample|warning>` to this entry. Claude only: Codex exposes a
    /// numeric percentage, for which `--min-usage-remaining` is the honest flag.
    fn set_min_usage_status(&mut self, v: &str) -> Result<(), String> {
        if self.reviewer != ReviewerKind::Claude {
            return Err(format!(
                "--min-usage-status applies to a categorical-usage reviewer (claude); reviewer \
                 '{}' exposes a numeric usage remaining. Use --min-usage-remaining <1..=100> for \
                 it.",
                self.reviewer.as_str()
            ));
        }
        if self.usage_minimum.is_some() {
            return Err(format!(
                "a usage minimum was given twice for the same --reviewer '{}' (did you forget a \
                 --reviewer before the second one?)",
                self.reviewer.as_str()
            ));
        }
        let level = match v {
            // The lowest *acceptable* level: `ample` gates on warning-or-rejected, `warning`
            // gates only on rejected. `exhausted` would never gate, so it is not offered.
            "ample" => HeadroomLevel::Ample,
            "warning" => HeadroomLevel::Warning,
            other => {
                return Err(format!(
                    "--min-usage-status must be 'ample' or 'warning', got '{other}'"
                ))
            }
        };
        self.usage_minimum = Some(UsageMinimum::Status(level));
        Ok(())
    }

    fn set_model(&mut self, v: String) -> Result<(), String> {
        if self.model.is_some() {
            return Err(format!(
                "--model given twice for the same --reviewer '{}' (did you forget a --reviewer \
                 before the second one?)",
                self.reviewer.as_str()
            ));
        }
        self.model = Some(v);
        Ok(())
    }

    fn set_effort(&mut self, v: String) -> Result<(), String> {
        if self.effort.is_some() {
            return Err(format!(
                "--effort given twice for the same --reviewer '{}' (did you forget a --reviewer \
                 before the second one?)",
                self.reviewer.as_str()
            ));
        }
        self.effort = Some(v);
        Ok(())
    }

    fn set_bin(&mut self, v: PathBuf) -> Result<(), String> {
        if self.bin.is_some() {
            return Err(format!(
                "--bin given twice for the same --reviewer '{}' (did you forget a --reviewer \
                 before the second one?)",
                self.reviewer.as_str()
            ));
        }
        self.bin = Some(v);
        Ok(())
    }

    /// Declare `--level NAME:MODEL:EFFORT` on this entry. Unlike the identity flags, `--level`
    /// repeats — each call adds one named preset — but a *duplicate name* on one entry is an error
    /// (which of two mappings would win is a guess, and almost always a typo). Model ids and effort
    /// names carry no colons, so the value is exactly three colon-separated non-empty parts. The
    /// effort is validated (warn-not-fail) at `finalize`, mirroring `--effort`.
    fn set_level(&mut self, v: &str) -> Result<(), String> {
        let parts: Vec<&str> = v.split(':').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(format!(
                "--level must be NAME:MODEL:EFFORT with three non-empty, colon-separated parts, \
                 got '{v}'"
            ));
        }
        let (name, model, effort) = (parts[0], parts[1], parts[2]);
        if self.levels.contains_key(name) {
            return Err(format!(
                "--level '{name}' declared twice for the same --reviewer '{}' (did you forget a \
                 --reviewer before the second one, or repeat a name?)",
                self.reviewer.as_str()
            ));
        }
        self.levels.insert(
            name.to_string(),
            LevelOverride {
                model: model.to_string(),
                effort: effort.to_string(),
            },
        );
        Ok(())
    }

    /// Bind `--default-level NAME` to this entry. Validated at `finalize` (the levels it may name
    /// can be declared after it on the command line).
    fn set_default_level(&mut self, v: String) -> Result<(), String> {
        if self.default_level.is_some() {
            return Err(format!(
                "--default-level given twice for the same --reviewer '{}' (did you forget a \
                 --reviewer before the second one?)",
                self.reviewer.as_str()
            ));
        }
        self.default_level = Some(v);
        Ok(())
    }

    /// Fill per-entry defaults, mirroring the single-reviewer defaults exactly. Fallible only for
    /// the one cross-field rule that cannot be checked at parse time: `--default-level` must name a
    /// level that was declared, and the `--level` it names may appear after it on the command line.
    fn finalize(self) -> Result<ReviewerSpec, String> {
        if let Some(dl) = &self.default_level {
            if !self.levels.contains_key(dl) {
                return Err(format!(
                    "--default-level '{dl}' for --reviewer '{}' names no declared --level (declared: {})",
                    self.reviewer.as_str(),
                    if self.levels.is_empty() {
                        "none".to_string()
                    } else {
                        self.levels.keys().cloned().collect::<Vec<_>>().join(", ")
                    }
                ));
            }
        }
        Ok(ReviewerSpec {
            model: self
                .model
                .unwrap_or_else(|| self.reviewer.default_model().to_string()),
            effort: self
                .effort
                .unwrap_or_else(|| self.reviewer.default_effort().to_string()),
            bin: self.bin,
            reviewer: self.reviewer,
            usage_minimum: self.usage_minimum.unwrap_or(UsageMinimum::None),
            profile: self.profile.unwrap_or(ProfileSelector::Ambient),
            levels: self.levels,
            default_level: self.default_level,
        })
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

    /// A stable lowercase identity token persisted in the session record, so a resume cannot
    /// cross backends. Kept distinct from `cli`/`name` because those are prose and may change.
    pub fn backend_id(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Perforce => "perforce",
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

/// The absolute-path analogue of [`scoped_claude_rules`], for when the Claude reviewer runs
/// from a neutral working directory and so cannot rely on `.` resolving to the working root
/// (see `docs/resume-cache-cwd-invalidation.md`).
///
/// Returns `None` -- fail closed -- for any `root` we cannot turn into a safe, literal glob
/// prefix, so a caller that gets `None` keeps the reviewer at `cfg.cwd` with the relative
/// rules rather than emit a rule that might mis-scope. Refused: a non-Unicode path, a UNC or
/// verbatim (`\\?\`) path, anything that is not a drive-letter absolute path, any path with a
/// non-normal `.`/`..` segment (which `normalize_dir` can leave in place when canonicalisation
/// fails, and which the matcher might resolve outside the root), and any path
/// containing a glob metacharacter (`* ? [ ] { }`) -- the exact hazard that made the relative
/// rule deliberately relative.
///
/// The emitted shape was verified empirically against the reviewer CLI: forward slashes, a
/// drive letter, no leading `//`, and a trailing `/**` -- e.g. `Read(C:/dev/repo/**)`. (The
/// `//`-prefixed form a naive interpolation would produce matches nothing, so it is not used.)
///
/// The matcher's boundary was probed directly from a neutral cwd with this rule (results
/// recorded here because `cargo test` makes no CLI calls, so the unit tests below can only check
/// the emitted string): a file under the root reads OK; a path outside the root, a *same-prefix
/// sibling* (`.../repo-sibling/...`, kept out by the trailing `/**`), and a `..` traversal out of
/// the root are each **denied**; a case-variant of an in-root path reads OK (Windows paths are
/// case-insensitive). So the rule scopes to exactly the root and no broader.
pub fn absolute_scoped_rules(root: &std::path::Path) -> Option<Vec<String>> {
    let forward = root.to_str()?.replace('\\', "/");
    let trimmed = forward.trim_end_matches('/');
    // Reject non-normal `.`/`..` segments: after the matcher resolves them the real scope could
    // fall outside the intended root. Checked on the literal segments rather than via
    // `Path::components`, which normalises a mid-path `.` away and so would miss `a/./b`.
    if trimmed.split('/').any(|seg| seg == "." || seg == "..") {
        return None;
    }
    // Drive-letter absolute only (`X:/...`). Excludes UNC (`//server`), verbatim (`//?/...`),
    // rooted-without-drive, and relative -- none of which we will represent.
    let b = trimmed.as_bytes();
    let drive_absolute = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/';
    if !drive_absolute {
        return None;
    }
    // A glob metacharacter left literal would change the pattern's meaning, granting or denying
    // the wrong paths. This is the case the relative rule was written to avoid; fail closed.
    if trimmed.contains(['*', '?', '[', ']', '{', '}']) {
        return None;
    }
    Some(
        ["Read", "Grep", "Glob"]
            .iter()
            .map(|tool| format!("{tool}({trimmed}/**)"))
            .collect(),
    )
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

/// Upper bound on `--timeout-seconds`, rejected above this at parse time. Besides catching a
/// typo, it keeps every `Instant::now() + timeout` deadline clear of overflow: the collect wait
/// cap is derived from the timeout, which makes those deadline sites easier to reach.
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// Headroom folded into the collect wait cap on top of the capture budget and the reviewer turn.
/// It covers the tail after the reviewer clock stops — output drain, transcript parse, session
/// persistence — so a single blocking `cross_model_review_result` observes the terminal state in
/// every realistic case. It is a sizing, not an enforced deadline; see
/// `docs/single-blocking-collect.md` for the two residual terms that are not themselves bounded.
pub const FINALIZATION_GRACE_SECS: u64 = 30;

/// Sizing for one fallback entry's preflight, folded into the chain's collect-wait cap.
///
/// Bounds the cancellable auth invocation (the 30 s `auth_check` timeout in the adapters) plus
/// its output-drain grace. `resolve_bin` is a fast but uninterruptible PATH scan and is *not*
/// covered here; it is an acknowledged residual, exactly as `docs/reviewer-fallback-chain.md`
/// and `docs/single-blocking-collect.md` describe. Only *fallback* entries add this term — the
/// selected entry is preflighted in `start_review`, before the collect wait begins.
pub const PREFLIGHT_CAP_SECS: u64 = 40;

/// Default per-process cap on concurrently-running reviews. A backstop against a runaway caller
/// accumulating full-budget reviews across distinct session names, not a normal limit — a serial
/// review flow runs one at a time. `0` disables the check. It is per process: N servers sharing a
/// state directory admit up to N times this.
pub const DEFAULT_MAX_CONCURRENT_REVIEWS: u32 = 8;

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

/// End a review session that has gone this many turns without minting or resolving a finding, while
/// findings are still open. Zero disables the check.
///
/// Three, because a reconstruction over this project's own 37 recorded ledgered sessions found that
/// no session ever went even two consecutive turns without a mint or a resolution — so a threshold of
/// two would have fired zero times, and three carries a further turn of margin. It is a bound on an
/// otherwise unbounded worst case rather than a response to an observed failure; see
/// `docs/finding-liveness.md`, which also records why issue #78's *per-finding* half was closed
/// won't-fix instead.
pub const DEFAULT_STAGNANT_SESSION_TURNS: u32 = 3;

/// How many times a degraded turn may ask the reviewer to re-emit its machine block, in the same
/// conversation. One by default: a missing block is a contract slip, not a capability gap, so a
/// reviewer told precisely what was wrong either complies immediately or is unlikely to on a third
/// telling -- and every attempt adds tail latency to a turn that already ran long. Zero restores the
/// single-shot behaviour exactly. See `docs/unstructured-turn-recovery.md`.
pub const DEFAULT_BLOCK_REPAIR_ATTEMPTS: u32 = 1;
/// Upper bound on `--block-repair-attempts`. More than a few re-asks is not persistence, it is a
/// loop billing a reviewer that is not going to comply.
pub const MAX_BLOCK_REPAIR_ATTEMPTS: u32 = 3;
/// Per-attempt timeout for a block repair, clamped to `--timeout`. A repair re-states a block the
/// reviewer has already computed; one that cannot do so in three minutes is not going to.
pub const DEFAULT_BLOCK_REPAIR_TIMEOUT_SECS: u64 = 180;

/// How many `blocked by policy` shell refusals a Codex turn may accumulate before it becomes
/// eligible for a fail-fast kill (issue #68). `0` disables the fail-fast entirely. The default
/// matches the real incident, which stalled after four composed-command refusals.
pub const DEFAULT_MAX_POLICY_DENIALS: usize = 4;
/// How long a Codex turn's raw stdout may stay silent, once it has passed the denial threshold,
/// before the fail-fast concludes it is stuck and terminates it (issue #68). Deliberately
/// conservative: `codex exec --json` emits nothing to stdout while the model is reasoning, and a
/// measurement of 45 real max-effort turns put the largest legitimate silent gap at ~110s (p95
/// ~98s, none over 120s), so 300s clears observed reality with wide margin. Lower it (via
/// `--max-policy-idle-seconds`) for setups that want faster fails, e.g. at lower effort where
/// reasoning gaps are shorter. See `docs/` and the issue-68 plan.
pub const DEFAULT_MAX_POLICY_IDLE_SECS: u64 = 300;

#[derive(Clone, Debug)]
pub struct Config {
    /// The reviewer chain, in fallback order. Always non-empty; `reviewers[0]` is the primary
    /// and matches the single-reviewer behaviour that predates this field. A fresh review walks
    /// the chain from the front, advancing only on `RATE_LIMITED`; a single-entry chain behaves
    /// exactly as one reviewer did before. See `docs/reviewer-fallback-chain.md`.
    pub reviewers: Vec<ReviewerSpec>,
    /// Working root handed to the reviewer. Defaults to the server's cwd, which is
    /// the project root when a harness launches us from `.mcp.json`.
    pub cwd: PathBuf,
    /// The immutable directory this process was launched from, canonicalized once at parse time
    /// from `std::env::current_dir()` *before* any `--cwd` is applied.
    ///
    /// This is the key the Phase 3 authorization allowlist is keyed on — never [`Self::cwd`], which
    /// `--cwd` can point anywhere (it is user- and repo-settable, and a reviewed repository's
    /// committed config could set it). Keying authorization on `cwd` would let a repo redirect itself
    /// into another root's approval; keying on the launch root, captured from the ambient environment
    /// and independent of every flag, cannot be steered that way. When `--cwd` differs from the launch
    /// root the difference is an out-of-band redirection that Phase 3 requires be confirmed on its own,
    /// rather than inheriting the launch root's authorization. See
    /// `docs/reviewer-account-profiles-impl.md` (Phase 3 → Allowlist store, `[f7]`).
    pub launch_root: PathBuf,
    pub timeout: Duration,
    /// Refuse to resume a session idle longer than this. Zero disables the check. See
    /// `DEFAULT_RESUME_MAX_IDLE_SECS`. A tripped session is refused with
    /// SESSION_NOT_RESUMABLE, not silently restarted, so the caller decides to start fresh.
    pub resume_max_idle: Duration,
    /// Refuse to resume a session that has already run this many turns. Zero disables the
    /// check. See `DEFAULT_RESUME_MAX_TURNS`.
    pub resume_max_turns: u32,
    /// End a session that has gone this many turns without minting or resolving a finding, while
    /// findings are still open. Zero disables the check. See `DEFAULT_STAGNANT_SESSION_TURNS`.
    pub stagnant_session_turns: u32,
    /// How many block-repair attempts a degraded turn may make (0 disables). See
    /// `DEFAULT_BLOCK_REPAIR_ATTEMPTS`.
    pub block_repair_attempts: u32,
    /// Per-attempt repair timeout, already clamped to `timeout` at parse time.
    pub block_repair_timeout: Duration,
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
    /// Per-process cap on how many reviews may be `Running` at once. `0` disables the check. See
    /// `DEFAULT_MAX_CONCURRENT_REVIEWS`. Enforced in `Registry::try_start` under the state lock, so
    /// two concurrent starts cannot both slip past it.
    pub max_concurrent_reviews: u32,
    /// Fail-fast threshold for a Codex reviewer stuck on policy-refused shell commands (issue #68):
    /// the number of `blocked by policy` refusals a turn may accumulate before it becomes eligible
    /// to be killed. `0` disables the fail-fast. Codex-only — the Claude reviewer has no such
    /// command router. See `DEFAULT_MAX_POLICY_DENIALS`.
    pub max_policy_denials: usize,
    /// The idle window that pairs with `max_policy_denials`: once a Codex turn is over the denial
    /// threshold, its raw stdout must stay silent for this long before the fail-fast terminates it.
    /// A turn that keeps producing stdout is making progress and is never killed on this path. See
    /// `DEFAULT_MAX_POLICY_IDLE_SECS`.
    pub max_policy_idle: Duration,
}

/// A resolved, authorized profile home together with the account the allowlist authorized it for.
///
/// The `account` is captured at authorization time and carried through the run: the pre-spawn identity
/// probe asserts the home still resolves to it, and the post-review switch guard `[f4]` compares the
/// final fingerprint against it. See [`Config::resolve_authorized_home_with_account`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedHome {
    pub home: PathBuf,
    pub account: String,
}

impl Config {
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        // The immutable launch root, captured from the ambient process cwd *before* any argument
        // is read, so no flag — least of all `--cwd` — can influence it. This is the Phase 3
        // authorization key (see the field doc). Canonicalized like `cwd` is below, via
        // `normalize_dir`, so the same directory keys one allowlist entry regardless of spelling.
        // A process whose current directory cannot be read is already broken (the `cwd` default
        // relies on the same call); failing closed here is the honest outcome for an authorization
        // key that cannot be established.
        let launch_root = normalize_dir(
            std::env::current_dir()
                .map_err(|e| format!("cannot determine the launch directory: {e}"))?,
        );

        // The chain is built as a list of entries. A repeated `--reviewer` starts a new entry;
        // the identity flags `--model`/`--effort`/`--bin` bind to the most recent `--reviewer`.
        // Argument order is fallback order. See `docs/reviewer-fallback-chain.md`.
        let mut entries: Vec<PendingEntry> = Vec::new();
        let mut cwd: Option<PathBuf> = None;
        let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
        let mut resume_max_idle_secs = DEFAULT_RESUME_MAX_IDLE_SECS;
        let mut resume_max_turns = DEFAULT_RESUME_MAX_TURNS;
        let mut stagnant_session_turns = DEFAULT_STAGNANT_SESSION_TURNS;
        let mut block_repair_attempts = DEFAULT_BLOCK_REPAIR_ATTEMPTS;
        let mut block_repair_timeout_secs = DEFAULT_BLOCK_REPAIR_TIMEOUT_SECS;
        let mut max_concurrent_reviews = DEFAULT_MAX_CONCURRENT_REVIEWS;
        let mut max_policy_denials = DEFAULT_MAX_POLICY_DENIALS;
        let mut max_policy_idle_secs = DEFAULT_MAX_POLICY_IDLE_SECS;
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
                    let kind = ReviewerKind::parse(&v).ok_or_else(|| {
                        format!("unknown --reviewer '{v}' (expected 'claude' or 'codex')")
                    })?;
                    entries.push(PendingEntry::new(kind));
                }
                "--model" => {
                    let v = take("--model")?;
                    entries
                        .last_mut()
                        .ok_or("--model must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_model(v)?;
                }
                "--effort" => {
                    let v = take("--effort")?;
                    entries
                        .last_mut()
                        .ok_or("--effort must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_effort(v)?;
                }
                "--bin" => {
                    let v = take("--bin")?;
                    entries
                        .last_mut()
                        .ok_or("--bin must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_bin(PathBuf::from(v))?;
                }
                "--level" => {
                    let v = take("--level")?;
                    entries
                        .last_mut()
                        .ok_or("--level must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_level(&v)?;
                }
                "--default-level" => {
                    let v = take("--default-level")?;
                    entries
                        .last_mut()
                        .ok_or("--default-level must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_default_level(v)?;
                }
                "--codex-profile" => {
                    let v = take("--codex-profile")?;
                    entries
                        .last_mut()
                        .ok_or("--codex-profile must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_profile_name("--codex-profile", ReviewerKind::Codex, v)?;
                }
                "--claude-profile" => {
                    let v = take("--claude-profile")?;
                    entries
                        .last_mut()
                        .ok_or("--claude-profile must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_profile_name("--claude-profile", ReviewerKind::Claude, v)?;
                }
                "--codex-home" => {
                    let v = take("--codex-home")?;
                    entries
                        .last_mut()
                        .ok_or("--codex-home must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_profile_home("--codex-home", ReviewerKind::Codex, PathBuf::from(v))?;
                }
                "--claude-config-dir" => {
                    let v = take("--claude-config-dir")?;
                    entries
                        .last_mut()
                        .ok_or("--claude-config-dir must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_profile_home("--claude-config-dir", ReviewerKind::Claude, PathBuf::from(v))?;
                }
                "--min-usage-remaining" => {
                    let v = take("--min-usage-remaining")?;
                    entries
                        .last_mut()
                        .ok_or("--min-usage-remaining must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_min_usage_remaining(&v)?;
                }
                "--min-usage-status" => {
                    let v = take("--min-usage-status")?;
                    entries
                        .last_mut()
                        .ok_or("--min-usage-status must follow a --reviewer (it binds to the reviewer entry before it)")?
                        .set_min_usage_status(&v)?;
                }
                "--cwd" => cwd = Some(PathBuf::from(take("--cwd")?)),
                "--timeout-seconds" => {
                    let v = take("--timeout-seconds")?;
                    timeout_secs = v
                        .parse()
                        .map_err(|_| format!("--timeout-seconds must be an integer, got '{v}'"))?;
                    if timeout_secs == 0 {
                        return Err("--timeout-seconds must be greater than 0".into());
                    }
                    if timeout_secs > MAX_TIMEOUT_SECS {
                        return Err(format!(
                            "--timeout-seconds must be at most {MAX_TIMEOUT_SECS} ({}h), got {timeout_secs}",
                            MAX_TIMEOUT_SECS / 3600
                        ));
                    }
                }
                "--max-concurrent-reviews" => {
                    let v = take("--max-concurrent-reviews")?;
                    max_concurrent_reviews = v.parse().map_err(|_| {
                        format!(
                            "--max-concurrent-reviews must be an integer (0 disables), got '{v}'"
                        )
                    })?;
                }
                "--max-policy-denials" => {
                    let v = take("--max-policy-denials")?;
                    max_policy_denials = v.parse().map_err(|_| {
                        format!("--max-policy-denials must be an integer (0 disables), got '{v}'")
                    })?;
                }
                "--max-policy-idle-seconds" => {
                    let v = take("--max-policy-idle-seconds")?;
                    max_policy_idle_secs = v.parse().map_err(|_| {
                        format!("--max-policy-idle-seconds must be a positive integer, got '{v}'")
                    })?;
                    if max_policy_idle_secs == 0 {
                        return Err("--max-policy-idle-seconds must be greater than 0; use \
                             --max-policy-denials 0 to disable the policy fail-fast"
                            .to_string());
                    }
                }
                "--session-max-turns" => {
                    let v = take("--session-max-turns")?;
                    resume_max_turns = v.parse().map_err(|_| {
                        format!("--session-max-turns must be an integer (0 disables), got '{v}'")
                    })?;
                }
                "--stagnant-session-turns" => {
                    let v = take("--stagnant-session-turns")?;
                    stagnant_session_turns = v.parse().map_err(|_| {
                        format!(
                            "--stagnant-session-turns must be an integer (0 disables), got '{v}'"
                        )
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
                "--block-repair-attempts" => {
                    let v = take("--block-repair-attempts")?;
                    let n: u32 = v.parse().map_err(|_| {
                        format!(
                            "--block-repair-attempts must be an integer (0 disables), got '{v}'"
                        )
                    })?;
                    if n > MAX_BLOCK_REPAIR_ATTEMPTS {
                        return Err(format!(
                            "--block-repair-attempts must be at most {MAX_BLOCK_REPAIR_ATTEMPTS}, got {n}"
                        ));
                    }
                    block_repair_attempts = n;
                }
                "--block-repair-timeout-seconds" => {
                    let v = take("--block-repair-timeout-seconds")?;
                    block_repair_timeout_secs = v.parse().map_err(|_| {
                        format!(
                            "--block-repair-timeout-seconds must be a positive integer, got '{v}'"
                        )
                    })?;
                    if block_repair_timeout_secs == 0 {
                        return Err(
                            "--block-repair-timeout-seconds must be greater than 0; use                              --block-repair-attempts 0 to disable repairs"
                                .to_string(),
                        );
                    }
                }
                "--state-dir" => {
                    let dir = PathBuf::from(take("--state-dir")?);
                    // Must be absolute, for the same reason a reviewer home must be (see
                    // `check_profile_home`): a relative state dir resolves against whatever working
                    // directory each process happens to run under, so two callers passing the same
                    // relative `--state-dir` from different launch directories would key their
                    // per-session lease (`session::session_lock_path`) and their Codex sterile
                    // directory (`reviewer::sterile_dir_name`) on the same text but resolve to
                    // *different* actual locks -- letting them share and stomp one sterile directory.
                    // Requiring absolute keeps the lease and the sterile name on one unambiguous
                    // identity. The default state dir is always absolute.
                    if !dir.is_absolute() {
                        return Err(format!(
                            "--state-dir requires an absolute path, got '{}'",
                            dir.display()
                        ));
                    }
                    state_dir = Some(dir);
                }
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

        if entries.is_empty() {
            return Err(
                "--reviewer is required (use '--reviewer codex' when the caller is Claude \
                        Code, '--reviewer claude' when the caller is Codex)"
                    .to_string(),
            );
        }

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

        // Finalise each entry into a `ReviewerSpec`, filling per-entry defaults. `finalize` is
        // fallible only for the `--default-level` cross-check (a level name must be declared).
        let reviewers: Vec<ReviewerSpec> = entries
            .into_iter()
            .map(|e| e.finalize())
            .collect::<Result<_, _>>()?;
        // The unknown-effort warning stays per entry and non-fatal (a bad value surfaces later as
        // MODEL_UNAVAILABLE on first use), exactly as it did for the single reviewer — and now also
        // covers each declared level's effort, since a level's effort reaches the reviewer the same
        // way the base one does.
        let warn_effort = |effort: &str, reviewer: ReviewerKind, whence: &str| {
            if !reviewer.known_efforts().contains(&effort) {
                eprintln!(
                    "cross-review: warning: effort '{effort}'{whence} is not one of the known \
                     levels for {} ({}). Passing it through anyway.",
                    reviewer.as_str(),
                    reviewer.known_efforts().join(", ")
                );
            }
        };
        for spec in &reviewers {
            warn_effort(&spec.effort, spec.reviewer, "");
            for (name, lv) in &spec.levels {
                warn_effort(&lv.effort, spec.reviewer, &format!(" for --level '{name}'"));
            }
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
        // Backstop the explicit `--state-dir` check above for the *derived* default: `default_state_dir`
        // joins `%LOCALAPPDATA%` (or, without it, `cwd`), and a relative `LOCALAPPDATA` -- or a relative
        // `cwd` reaching the fallback -- would yield a relative state dir that resolves against each
        // process's launch directory. Two callers would then key the per-session lease and the Codex
        // sterile directory on one text but open different locks, letting them stomp one sterile
        // directory. The state dir must be absolute however it was arrived at; fail closed if it is not.
        if !state_dir.is_absolute() {
            return Err(format!(
                "the state directory resolved to a relative path ('{}'); set an absolute --state-dir, \
                 or an absolute %LOCALAPPDATA% and --cwd",
                state_dir.display()
            ));
        }

        Ok(Self {
            reviewers,
            state_dir,
            cwd,
            launch_root,
            timeout: Duration::from_secs(timeout_secs),
            resume_max_idle: Duration::from_secs(resume_max_idle_secs),
            resume_max_turns,
            stagnant_session_turns,
            block_repair_attempts,
            // Clamped to the turn budget: a repair that outlived the review timeout would be a
            // second, larger budget nobody configured.
            block_repair_timeout: Duration::from_secs(
                block_repair_timeout_secs.min(timeout_secs.max(1)),
            ),
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
            max_concurrent_reviews,
            max_policy_denials,
            max_policy_idle: Duration::from_secs(max_policy_idle_secs),
        })
    }

    /// The primary reviewer: the first entry in the chain, tried before any fallback.
    ///
    /// Non-run-path code (config display, caller-facing summaries) reads the primary here.
    /// Code on the *run* path must instead use the active entry the walk selected, never the
    /// primary — see `docs/reviewer-fallback-chain.md`.
    pub fn primary(&self) -> &ReviewerSpec {
        // `reviewers` is non-empty by construction (`from_args` requires `--reviewer`, and
        // `validate_chain` rejects an empty vector), so indexing [0] cannot panic.
        &self.reviewers[0]
    }

    /// Whether any chain entry configures a usage minimum. When false, the whole proactive
    /// gate is inert: no observation, no store, and Claude keeps its buffered `json` output —
    /// behaviour is byte-for-byte the pre-gate walk. Mirrors `chain_needs_capture()`. See
    /// `docs/usage-remaining-gate.md`.
    pub fn chain_gates_on_usage(&self) -> bool {
        self.reviewers.iter().any(|s| s.usage_minimum.is_gating())
    }

    /// Resolve an entry's authorized config home, or refuse.
    ///
    /// This is the single way any review-path code obtains a profile home: `Ambient` returns
    /// `Ok(None)` (inherit the environment, today's behaviour); a named profile or explicit home is
    /// resolved and then authorized, returning `Err(PROFILE_NOT_AUTHORIZED)` unless this working root
    /// is approved for it. Home resolution and authorization are one operation so a caller cannot
    /// obtain a home without passing the check. See `docs/reviewer-account-profiles-impl.md`.
    ///
    /// Authorization consults the per-machine allowlist store ([`crate::allowlist`]), keyed on the
    /// immutable [`launch_root`](Self::launch_root), against the full `(effective_home +
    /// reviewer_family + account_fingerprint)` tuple `[f19]`. The fingerprint is the account
    /// *currently* in the profile home, so a profile silently re-logged to a different account is
    /// refused until reauthorized. A profile is authorized only when the store holds an entry matching
    /// all four; with no store (or no entry) every non-ambient profile is refused — the same
    /// fail-closed default the earlier deny-all stub had, now driven by real data.
    ///
    /// Not fully atomic yet: the pre-spawn per-home lock + generation recheck `[f5]` — which makes
    /// authorization→probe→spawn one critical section against a concurrent setup — land with the
    /// setup tool (#15), which owns the other side of that per-home lock. The post-review
    /// start-vs-final switch guard `[f4]` (the actual fail-closed backstop for an external re-login) is
    /// wired in the worker: the account this returns is asserted pre-spawn and re-checked after the
    /// review. Until #15 can write an entry the store is empty, so this check denies every non-ambient
    /// profile and the race window authorizes nothing meanwhile.
    pub fn resolve_authorized_home(
        &self,
        spec: &ReviewerSpec,
    ) -> Result<Option<PathBuf>, crate::errors::Failure> {
        Ok(self
            .resolve_authorized_home_with_account(spec)?
            .map(|authorized| authorized.home))
    }

    /// Resolve *and authorize* a profile home, returning both the home and the **authorized account**
    /// — the account fingerprint the allowlist entry was matched on (equal to the account in the home
    /// at this instant, since the tuple match requires it).
    ///
    /// This is the only place the authorized account is established, and it is captured here so it can
    /// be carried forward: the pre-spawn identity probe asserts the home *still* resolves to this
    /// account (not a fresh self-read, which would be tautological — the gotcha the switch was made to
    /// fix), and the post-review switch guard `[f4]` compares the final fingerprint against this same
    /// value, refusing a review whose account was swapped mid-flight before it is recorded or
    /// delivered. `Ok(None)` for ambient (no account to bind).
    pub fn resolve_authorized_home_with_account(
        &self,
        spec: &ReviewerSpec,
    ) -> Result<Option<AuthorizedHome>, crate::errors::Failure> {
        if spec.profile.is_ambient() {
            return Ok(None);
        }
        let refuse = |detail: &str| {
            crate::errors::profile_not_authorized(spec.reviewer.as_str(), &spec.profile.label())
                .with_detail(detail.to_string())
        };

        let base = crate::profile::profile_base();
        let home = crate::profile::resolve_home(&spec.profile, spec.reviewer, base.as_deref())
            .map_err(|e| refuse(&e))?
            // `resolve_home` returns `None` only for `Ambient`, handled above.
            .expect("a non-ambient selector resolves to a home");

        match self.profile_authorized(spec, &home)? {
            Some(account) => Ok(Some(AuthorizedHome { home, account })),
            None => Err(refuse(
                "no authorization on file for this launch root, profile home, and account. Run the \
                 profile setup to authorize this repository for this profile.",
            )),
        }
    }

    /// The authorized account for `spec`'s (non-ambient) profile home `home`, or `None` if this
    /// launch root is not authorized for it.
    ///
    /// Reads the account currently in `home` (directly, not through the authorization seam — see
    /// [`crate::reviewer::Reviewer::fingerprint_at`]) and asks the allowlist store whether the full
    /// four-field tuple is on file; on a match it returns that account. Fail-closed on every
    /// uncertainty: no store base, an unreadable account in the home, or a store that is not on file
    /// all yield `Ok(None)` (refuse), while an *untrusted/corrupt* store surfaces as
    /// [`PROFILE_NOT_AUTHORIZED`](crate::errors::profile_not_authorized) with the underlying reason —
    /// never a silent allow.
    fn profile_authorized(
        &self,
        spec: &ReviewerSpec,
        home: &Path,
    ) -> Result<Option<String>, crate::errors::Failure> {
        let refuse = |detail: String| {
            crate::errors::profile_not_authorized(spec.reviewer.as_str(), &spec.profile.label())
                .with_detail(detail)
        };
        // No resolvable base means nowhere an authorization could have been recorded: deny.
        let Some(store) = crate::allowlist::AllowlistStore::current() else {
            return Ok(None);
        };
        // The account currently in the home. A profile with no readable account (never provisioned,
        // or mid re-login) cannot match any entry, so it is unauthorized — not an error.
        let Some(fingerprint) = crate::reviewer::for_kind(spec.reviewer).fingerprint_at(home)
        else {
            return Ok(None);
        };
        let query = crate::allowlist::AllowEntry {
            launch_root: self.launch_root.to_string_lossy().into_owned(),
            effective_home: home.to_string_lossy().into_owned(),
            reviewer_family: spec.reviewer.as_str().to_string(),
            account_fingerprint: fingerprint.clone(),
        };
        let authorized = store
            .is_authorized(&query)
            .map_err(|e| refuse(format!("the authorization store could not be trusted: {e}")))?;
        Ok(authorized.then_some(fingerprint))
    }

    /// The chain index of the entry that matches a stored session's identity, if any.
    ///
    /// A resume must run the entry that *created* the session (which may be a fallback), not the
    /// primary. Matching is on the full raw identity: reviewer, the *produced* `(model, effort)`
    /// pair (base or a declared level — see [`ReviewerSpec::produces_pair`]), profile, and raw bin.
    /// A match must be *unique*: a legacy record with no stored raw bin (`raw_bin` is `None`) matches
    /// on reviewer/pair/profile alone, and a modern record additionally on raw bin — either way,
    /// only if *exactly one* entry matches, so an ambiguous record is refused rather than bound to a
    /// guessed entry. See `docs/reviewer-fallback-chain.md` and `docs/review-levels-plan.md`.
    pub fn resume_entry_index(&self, record: &crate::session::SessionRecord) -> Option<usize> {
        let base_match = |s: &ReviewerSpec| {
            s.reviewer.as_str() == record.reviewer
                // The persisted pair is what actually ran, which may be a *level's* pair rather than
                // the entry's fixed base pair. Match either, so a session started at a non-default
                // level still binds back to its creating entry (levels are a menu, not identity;
                // reviewer/profile/bin below still constrain the match). See `docs/review-levels-plan.md`.
                && s.produces_pair(&record.model, &record.effort)
                // The profile is part of entry identity, so a resume binds to the entry with the
                // *same* account, not merely the same reviewer/model/effort/bin. A legacy record has
                // no stored selector, so it matches on the other fields (and is refused later by the
                // fail-closed identity check); a new record's selector must match the entry's.
                && match &record.profile_identity {
                    Some(pi) => s.profile.matches_id(&pi.selector),
                    None => true,
                }
        };
        match &record.raw_bin {
            Some(raw) => {
                // Require *exactly one* match, not merely the first. Without levels this is a no-op
                // (`validate_chain` forbids two entries of identical reviewer/model/effort/profile/bin,
                // so at most one ever matched). With levels, two entries of the same
                // reviewer/profile/bin can both *produce* the persisted pair — one via its base pair,
                // another via a declared level. They would invoke identically, so binding to the first
                // is harmless in effect, but guessing is still refused: an ambiguous resume rebaselines
                // rather than silently picking one. Fail-closed, matching the legacy branch (impl f1).
                let mut matches = self
                    .reviewers
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| base_match(s) && s.raw_bin().identity_matches(raw));
                match (matches.next(), matches.next()) {
                    (Some((i, _)), None) => Some(i),
                    _ => None,
                }
            }
            None => {
                // Legacy record: match on the fields it carries, but only if unambiguous.
                let mut matches = self
                    .reviewers
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| base_match(s));
                match (matches.next(), matches.next()) {
                    (Some((i, _)), None) => Some(i),
                    _ => None,
                }
            }
        }
    }

    /// Validate the reviewer chain's *semantics*, distinct from parse syntax.
    ///
    /// Returns `Err` with a caller-facing message when the chain cannot function as a fallback
    /// chain. `App::new` runs this and, on `Err`, serves every request an `INVALID_REVIEWER_CHAIN`
    /// failure rather than exiting — so the server is up and rejects requests with a legible
    /// error, as `docs/reviewer-fallback-chain.md` describes. The rule set is deliberately small:
    ///
    /// - **An identity-equivalent entry is invalid.** Two entries equal in reviewer, model,
    ///   effort, *and* bin cannot be a fallback for each other. The whole spec is compared, not
    ///   just `(reviewer, model)`: a distinct `bin` can be a distinct install/account, and model
    ///   aliases (`opus` vs `claude-opus-4-8`) cannot be canonicalised without asserting a
    ///   mapping this tool has not verified, so a same-model/different-bin (or different-effort)
    ///   fallback stays valid. The bin is compared by path identity (Windows-case- and
    ///   separator-insensitive, via `ReviewerSpec::same_reviewer_identity`), not byte-exactly, so
    ///   two spellings of the same executable are still one duplicate -- matching the rule
    ///   `resume_entry_index` uses to bind a resume to its creating entry.
    /// - **The empty chain is rejected defensively**, though `from_args` cannot produce one.
    pub fn validate_chain(reviewers: &[ReviewerSpec]) -> Result<(), String> {
        if reviewers.is_empty() {
            return Err("the reviewer chain is empty; at least one --reviewer is required".into());
        }
        for (i, a) in reviewers.iter().enumerate() {
            for b in &reviewers[i + 1..] {
                // Identity excludes `usage_minimum` (a gating policy, not identity), so two
                // entries differing only by minimum are still rejected as duplicates. The bin is
                // compared by path identity (case/separator-insensitive) inside the method (#55).
                if a.same_reviewer_identity(b) {
                    return Err(format!(
                        "reviewer chain has a duplicate entry ({}): a fallback with the same \
                         reviewer, model, effort and bin (comparing the bin path case- and \
                         separator-insensitively) as an earlier entry can never help, because it \
                         shares the same account and model and so the same rate limit. Remove it, \
                         or change its model/bin to a genuinely different reviewer.",
                        a.describe()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Longest a single `cross_model_review_result` call will block, in seconds.
    ///
    /// Sized to cover the whole review lifecycle — the capture budget, the reviewer turn, and a
    /// finalization grace for the tail after the reviewer clock stops — so one blocking call
    /// observes the terminal state in every realistic case rather than a `running` snapshot. It is
    /// a practical sizing, not a proven ceiling (two tail terms are not themselves deadline-bounded;
    /// see `docs/single-blocking-collect.md`), which is acceptable because a boundary miss now costs
    /// one more poll rather than a lost review. Saturating, so no `--timeout-seconds` overflows it.
    pub fn max_wait_secs(&self) -> u64 {
        // Today's single-reviewer budget: capture + one turn + finalization. For N == 1 this is
        // the whole cap, byte-for-byte as before.
        // A degraded turn can spend its whole turn budget and then up to `attempts` repair budgets
        // on top, so the collect deadline has to include them or a blocking collect advertised as
        // covering a whole review would time out on exactly the turns the repair exists to rescue.
        // Zero attempts contributes zero, byte-for-byte as before.
        // Each attempt is the child's own timeout *plus* the pre-spawn identity probe that runs
        // before it -- a real CLI invocation with its own 30s auth-status timeout, not a free
        // check, so a worst-case repaired turn would otherwise be able to outrun the deadline this
        // function exists to define. `PREFLIGHT_CAP_SECS` is the same allowance the chain's
        // preflight uses, and for the same call.
        let per_repair = self
            .block_repair_timeout
            .as_secs()
            .saturating_add(PREFLIGHT_CAP_SECS);
        let repair = per_repair.saturating_mul(self.block_repair_attempts as u64);
        let single = crate::vcs::CAPTURE_BUDGET
            .as_secs()
            .saturating_add(self.timeout.as_secs())
            .saturating_add(repair)
            .saturating_add(FINALIZATION_GRACE_SECS);
        // Each *fallback* entry can add its own preflight + turn + drain to the walk, because a
        // rate-limited attempt is not guaranteed to fail fast. Expressed as "today's budget plus
        // the fallback terms" so the single-entry invariant holds by construction. This is a
        // practical sizing, not a proven ceiling (the resolve_bin residual is uninterruptible);
        // see docs/reviewer-fallback-chain.md.
        let fallbacks = (self.reviewers.len() as u64).saturating_sub(1);
        // A fallback entry's turn can degrade and repair too, so it carries the repair term as well.
        let per_fallback = PREFLIGHT_CAP_SECS
            .saturating_add(self.timeout.as_secs())
            .saturating_add(repair)
            .saturating_add(crate::reviewer::DRAIN_GRACE.as_secs());
        single.saturating_add(fallbacks.saturating_mul(per_fallback))
    }

    /// Whether the reviewer's read allow-list is exactly the built-in scoped default -- i.e. the
    /// caller did not override it with `--allow-tools`. Only the default set can be safely
    /// translated to absolute form for a neutral working directory; a caller-supplied *relative*
    /// rule would silently lose access there, so its presence keeps the reviewer at `cfg.cwd`.
    /// See `docs/resume-cache-cwd-invalidation.md`.
    pub fn allowed_tools_are_default(&self) -> bool {
        self.allowed_tools == scoped_claude_rules()
    }

    /// True when the reviewer has any shell at all.
    pub fn reviewer_has_shell(&self) -> bool {
        self.reviewer_has_shell_of(self.primary().reviewer)
    }

    /// `reviewer_has_shell`, evaluated for a specific chain entry rather than the primary. The
    /// tool/allow-list inputs it consults are process-global, so only the reviewer kind varies.
    pub fn reviewer_has_shell_of(&self, reviewer: ReviewerKind) -> bool {
        match reviewer {
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
        self.supplies_change_of(self.primary().reviewer)
    }

    /// `supplies_change`, evaluated for a specific chain entry rather than the primary.
    ///
    /// Under `--diff auto` the answer is per-reviewer (a shell-less entry needs the diff, a
    /// shelled one does not), which is why `chain_needs_capture` folds it across every entry.
    pub fn supplies_change_of(&self, reviewer: ReviewerKind) -> bool {
        // Each backend matches exhaustively: a new mode or backend states its answer here
        // rather than opting itself in by falling through a wildcard.
        match self.vcs {
            Vcs::Git => match self.diff {
                DiffMode::None => false,
                // An isolated Codex reviewer now runs from a sterile non-repository cwd. Its
                // evidence service, not the mere presence of a shell, supplies the selected
                // change, so auto must capture it. The explicit config opt-out retains the old
                // project-cwd self-serve behaviour.
                DiffMode::Auto => {
                    !self.reviewer_has_shell_of(reviewer)
                        || (reviewer == ReviewerKind::Codex && self.isolate_reviewer)
                }
                DiffMode::Staged | DiffMode::Head | DiffMode::Rev(_) => true,
            },
            // The `change` argument is required for a Perforce review (enforced in `tools.rs`
            // before a job is created), so a Perforce backend always intends to hand over a
            // change.
            Vcs::Perforce => true,
        }
    }

    /// Whether the capture must be gathered at all, folded across the whole chain.
    ///
    /// The change is captured whenever *any* entry the walk might reach would need it — not
    /// merely the primary — because a `Codex → Claude` chain must have the diff ready for the
    /// shell-less Claude fallback even though the Codex primary would fetch its own. A shelled
    /// entry handed the captured change is the harmless `--diff HEAD` case. See
    /// `docs/reviewer-fallback-chain.md`.
    pub fn chain_needs_capture(&self) -> bool {
        self.reviewers
            .iter()
            .any(|spec| self.supplies_change_of(spec.reviewer))
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
    /// Primary-entry convenience used by the config tests; the run path uses the `_of` variant
    /// with the active entry.
    #[cfg(test)]
    pub fn reviewer_can_self_serve_change(&self) -> bool {
        self.reviewer_can_self_serve_change_of(self.primary().reviewer)
    }

    /// `reviewer_can_self_serve_change`, evaluated for a specific chain entry.
    pub fn reviewer_can_self_serve_change_of(&self, reviewer: ReviewerKind) -> bool {
        if reviewer == ReviewerKind::Codex && self.isolate_reviewer {
            // The evidence service supplies repository data; the process shell is deliberately
            // rooted elsewhere and is not the mechanism behind this predicate.
            return false;
        }
        match self.vcs {
            Vcs::Git => self.reviewer_has_shell_of(reviewer),
            Vcs::Perforce => {
                matches!(reviewer, ReviewerKind::Codex)
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
    /// Primary-entry convenience used by the config tests; the run path uses the `_of` variant
    /// with the active entry.
    #[cfg(test)]
    pub fn reviewer_capabilities(&self, diff_supplied: bool) -> String {
        let evidence_enabled =
            crate::reviewer::claude::claude_evidence_enabled(self, self.primary());
        self.reviewer_capabilities_of(self.primary().reviewer, diff_supplied, evidence_enabled)
    }

    /// `reviewer_capabilities`, rendered for a specific chain entry.
    ///
    /// This is rendered per *active* entry at turn time, from the captured change, so a
    /// mixed-family fallback is told the truth about *its* shell and self-serve ability — the
    /// captured block itself is capability-neutral. See `docs/reviewer-fallback-chain.md`.
    pub fn reviewer_capabilities_of(
        &self,
        reviewer: ReviewerKind,
        diff_supplied: bool,
        evidence_enabled: bool,
    ) -> String {
        if reviewer == ReviewerKind::Codex {
            let history = match self.vcs {
                Vcs::Git => "Use `repository_history` and `repository_revision` for commit history and revisions.",
                Vcs::Perforce => "`repository_history` and `repository_revision` report unsupported because this service performs no new Perforce network calls.",
            };
            let shell = if self.isolate_reviewer {
                "Your process shell runs from a sterile non-repository directory, so repo-relative shell commands are not a source of evidence -- the evidence tools above are the intended way to read and search this project, and they cover it completely. Do not reach for the shell to read or grep files. Any exceptional absolute-path read must be a single, simple command: composed shell (a pipeline with `|`, a `;`-separated pair, `Select-String`, or a `$var`/expression form) is refused non-interactively by the CLI command policy, and that refusal is final -- do not retry it as a variant, just use the evidence tools instead. Writes remain blocked by the sandbox."
            } else {
                "Configuration isolation was explicitly disabled, so your shell runs from the reviewed repository and may self-serve additional context. The evidence tools remain the preferred bounded interface; writes remain subject to the configured sandbox."
            };
            let mut out = format!(
                "Use the read-only repository evidence tools as the primary and complete \
                 interface to this project. Start with `repository_scope` when you need scope or \
                 drift state: it also reports which files the recursive scans cover, and a null \
                 `drifted` there means drift could not be determined rather than that nothing \
                 changed. Use `repository_list`, `repository_search`, and `repository_read` \
                 for the live tree. {history} Use continuation cursors whenever \
                 a result is truncated. Their paths are relative to the reviewed repository root. \
                 {shell}"
            );
            if diff_supplied {
                out.push_str(
                    "\n\nThe selected change was captured before this turn and is available through \
                     `repository_change`; page it to completion when the review depends on the \
                     diff. Do not reconstruct it with a repo-relative shell command.",
                );
            } else if self.vcs == Vcs::Git {
                out.push_str(
                    "\n\nThe change under review is the live working tree. Diff it on demand with \
                     `repository_diff`: `base: \"branch-base\"`, `head: \"worktree\"` gives the whole \
                     change -- the working tree against the branch's fork point, including untracked \
                     files. Page that canonical diff to completion; a formal APPROVE is accepted only \
                     if you were served it end to end. Narrow with `path`, or diff a commit id, for \
                     focused exploration on top. Do not reconstruct the diff with a shell command.",
                );
            } else {
                out.push_str(
                    "\n\nNo selected change was captured. If the request depends on a diff, state \
                     that limitation under \"What I could not check\" rather than guessing.",
                );
            }
            return out;
        }
        // In-scope shell-less Claude (plan section 0): the same read-only evidence service Codex
        // has, but the selected change is ALSO in this prompt and remains authoritative, so the
        // tools are for looking *past* it. Self-contained (early return): it states its own
        // no-shell boundary and diff handling, so it does not fall through to the shared shell-based
        // block below.
        if reviewer == ReviewerKind::Claude && evidence_enabled {
            let history = match self.vcs {
                Vcs::Git => " `repository_history` and `repository_revision` walk commit history and read revisions.",
                Vcs::Perforce => " `repository_history` and `repository_revision` report unsupported (this service makes no new Perforce network calls).",
            };
            let mut out = format!(
                "You can read and search files with Read, Grep and Glob (scoped to this directory \
                 tree, reachable by absolute path), and you have read-only repository evidence \
                 tools: `repository_scope`, `repository_list`, `repository_search`, \
                 `repository_read`, `repository_change`, and `repository_diff`.{history} Their \
                 paths are relative to the reviewed repository root, and continuation cursors page a \
                 truncated result. You have no shell."
            );
            if diff_supplied {
                out.push_str(
                    "\n\nThe change under review is captured for you and appears below under \
                     \"Change under review\"; it stays the authoritative selected change. Use the \
                     evidence tools to look *past* it -- read the live tree, search it, and walk \
                     history for context and to verify what the captured diff shows.",
                );
            } else if self.vcs == Vcs::Git {
                out.push_str(
                    "\n\nThe change under review is the live working tree. Diff it on demand with \
                     `repository_diff`: `base: \"branch-base\"`, `head: \"worktree\"` gives the whole \
                     change -- the working tree against the branch's fork point, including untracked \
                     files. Page that canonical diff to completion; a formal APPROVE is accepted only \
                     if you were served it end to end. Narrow with `path`, or diff a commit id, for \
                     focused exploration on top. If the evidence service cannot produce the change, \
                     the review is inconclusive: do NOT approve -- say so under \"What I could not \
                     check\".",
                );
            } else {
                out.push_str(
                    "\n\nNo selected change was captured (an empty or unavailable range). Obtain \
                     the change yourself through the evidence tools -- read the working tree and \
                     walk history. If you can neither see a captured change nor obtain one through \
                     the evidence tools, the review is inconclusive: do NOT approve -- say so under \
                     \"What I could not check\".",
                );
            }
            return out;
        }
        let mut out = String::new();
        match reviewer {
            ReviewerKind::Codex if self.reviewer_can_self_serve_change_of(reviewer) => {
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
            ReviewerKind::Claude if self.reviewer_has_shell_of(reviewer) => {
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
        } else if !self.reviewer_can_self_serve_change_of(reviewer) {
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

    /// Describe the reviewer chain for callers and tool descriptions.
    ///
    /// A single-entry chain reads exactly as one reviewer did before. A multi-entry chain names
    /// the primary and then each fallback in order, so `tools/list`, `status`, and the config
    /// display advertise the chain honestly rather than implying the primary is the only one.
    pub fn describe_reviewer(&self) -> String {
        self.describe_reviewer_effective(0, None)
    }

    /// Like [`describe_reviewer`](Self::describe_reviewer) but renders the entry at `start_index`
    /// with `over`'s `(model, effort)` applied, so the start response's headline names the pair the
    /// review actually runs at when a level override or resume pin is in play — rather than the
    /// entry's base pair, which would contradict the `level:` line the same response prints (issue
    /// #106). Only `start_index` is adjusted: a mid-run fallback runs at its own base pair
    /// (`effective_entry`), so the other entries keep theirs. With `over == None` this is identical
    /// to [`describe_reviewer`](Self::describe_reviewer), which delegates here.
    pub fn describe_reviewer_effective(
        &self,
        start_index: usize,
        over: Option<&LevelOverride>,
    ) -> String {
        // Defensive: `from_args` never produces an empty chain, but a degraded `App` built from an
        // invalid config must not panic here if something renders it before the chain-error guard.
        if self.reviewers.is_empty() {
            return "(no reviewer configured)".to_string();
        }
        // The override belongs to whichever entry actually starts; every other entry is described at
        // its base pair.
        let describe_at = |i: usize, spec: &ReviewerSpec| -> String {
            if i == start_index {
                spec.describe_effective(over)
            } else {
                spec.describe()
            }
        };
        let primary = describe_at(0, &self.reviewers[0]);
        if self.reviewers.len() == 1 {
            return primary;
        }
        let fallbacks: Vec<String> = self.reviewers[1..]
            .iter()
            .enumerate()
            .map(|(k, s)| describe_at(k + 1, s))
            .collect();
        format!(
            "{primary}, falling back on a rate/usage limit to: {}",
            fallbacks.join(", ")
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
    // FROZEN persistence key -- not a comparison. This `to_lowercase` fold is baked into a
    // durable directory name under %LOCALAPPDATA%\cross-review\, so changing it (e.g. to
    // `pathcmp`'s ASCII fold for "consistency") would relocate the default state dir and orphan
    // in-flight sessions for users whose cwd has a case-bearing non-ASCII character. Do NOT unify
    // it with `pathcmp` without a migration. See docs/path-comparison-plan.md (Family C).
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
  --codex-profile <name>      Run the codex reviewer under a dedicated account profile (a named
  --claude-profile <name>     config home under %CROSS_REVIEW_HOME% or %LOCALAPPDATA%\\cross-review),
                              claude equivalent --claude-profile, so a review bills the intended
                              account regardless of the desktop app. Name: letters, digits, '.',
                              '_', '-'. Using a profile requires the working root to be authorized
                              for it first; until then a review is refused (PROFILE_NOT_AUTHORIZED).
  --codex-home <abs>          Run the reviewer under an explicit absolute config home instead of a
  --claude-config-dir <abs>   named profile (local/trusted-only escape hatch). Mutually exclusive
                              with the profile flag for the same reviewer; also authorization-gated.
  --min-usage-remaining <n>   Proactive gate (codex only): skip this entry when its
                              last-observed usage remaining is known and below n% (1..=100),
                              advancing to the next reviewer before spending a call. Optional;
                              unset never gates. Needs a two+ entry chain to fall back to.
  --min-usage-status <lvl>    Proactive gate (claude only): claude reports a categorical usage
                              status, not a percentage. 'ample' skips this entry when the
                              status is a warning or worse; 'warning' skips only when rejected.
                              Optional; unset never gates.
  --cwd <path>                Working root for the reviewer. Default: this process's cwd.
  --timeout-seconds <n>       Hard kill for a single review turn. Default: 1800. Max: 86400 (24h).
  --max-concurrent-reviews <n>
                              Per-process cap on reviews running at once. A backstop against a
                              runaway caller, not a normal limit; a serial flow runs one at a
                              time. Two servers sharing a state dir admit up to 2x this. The
                              caller is told to collect or cancel an outstanding review. 0
                              disables. Default: 8.
  --session-max-turns <n>     Refuse to resume a review session once it has run this many
                              turns; the caller must start fresh (fresh=true) or use a new
                              session name. Each turn re-processes the whole conversation, so
                              a long session grows expensive and prone to drift. 0 disables.
                              Default: 10.
  --stagnant-session-turns <n>
                              End a review session that has gone this many turns without any
                              finding being raised or resolved while findings are still open.
                              The session is marked terminal, the turn reports
                              session_stagnant / rebaseline with its still-open findings
                              intact, and a later resume is refused. This is a bound on a
                              stalled loop, not a judgement that anything went unexamined --
                              the reviewer is never told about it and nothing infers that a
                              carried finding is resolved. 0 disables. Default: 3.
  --session-max-idle-seconds <n>
                              Refuse to resume a review session idle longer than this many
                              seconds. Past it the reviewer's prompt cache may no longer be
                              warm and its context may have drifted, so a resume risks
                              re-reading the whole conversation. The caller is told to start
                              fresh rather than silently handed the stale resume. 0 disables.
                              Default: 3300 (55 minutes, just under the 1h cache lifetime).
  --block-repair-attempts <n>
                              When a reviewer answers without a usable machine-readable
                              findings block, ask it once more -- in the same conversation,
                              for the block alone -- rather than throwing the whole turn
                              away. Costs up to n further short calls on a degraded turn,
                              and adds up to n x --block-repair-timeout-seconds to it.
                              0 disables (single-shot, as before). Max 3. Default: 1.
  --block-repair-timeout-seconds <n>
                              Per-attempt timeout for the above, clamped to --timeout-seconds.
                              A repair re-states a block the reviewer already computed.
                              Default: 180.
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
  --max-policy-denials N      Fail a Codex review fast once it has been refused N
                              shell commands "by policy" AND its stdout then goes
                              silent (see --max-policy-idle-seconds), instead of
                              letting it burn the whole turn to TIMEOUT. Reported as
                              POLICY_BLOCKED. Default 4; 0 disables. Codex-only.
  --max-policy-idle-seconds S The stdout-silence window that pairs with
                              --max-policy-denials: a past-threshold Codex turn is
                              killed only after S seconds with no stdout progress, so
                              a turn still emitting output is never cut. Default 300
                              (max-effort Codex reasons silently for up to ~110s of
                              observed real turns); lower it for faster fails.

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
        let err = Config::from_args(&args(&[])).unwrap_err();
        assert!(err.contains("--reviewer is required"), "{err}");
    }

    #[test]
    fn a_relative_state_dir_is_rejected() {
        // A relative `--state-dir` resolves against each process's launch directory, so two callers
        // passing the same relative text from different directories would key the per-session lease
        // and the Codex sterile directory on one name but resolve to different locks -- letting them
        // stomp one sterile directory. It is rejected at the door, like a relative reviewer home.
        let err = Config::from_args(&args(&["--reviewer", "codex", "--state-dir", ".state"]))
            .unwrap_err();
        assert!(
            err.contains("--state-dir requires an absolute path"),
            "{err}"
        );
        // An absolute one is accepted.
        Config::from_args(&args(&["--reviewer", "codex", "--state-dir", "C:\\state"]))
            .expect("absolute state dir");
    }

    #[test]
    fn policy_fail_fast_flags_default_parse_and_validate() {
        // Defaults (issue #68): 4 denials, 300s idle window.
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.max_policy_denials, DEFAULT_MAX_POLICY_DENIALS);
        assert_eq!(cfg.max_policy_denials, 4);
        assert_eq!(cfg.max_policy_idle, Duration::from_secs(300));

        // Both parse.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--max-policy-denials",
            "6",
            "--max-policy-idle-seconds",
            "120",
        ]))
        .expect("config");
        assert_eq!(cfg.max_policy_denials, 6);
        assert_eq!(cfg.max_policy_idle, Duration::from_secs(120));

        // 0 disables the denial threshold (the whole fail-fast).
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--max-policy-denials", "0"]))
            .expect("config");
        assert_eq!(cfg.max_policy_denials, 0);

        // Non-integer values are rejected for both.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--max-policy-denials",
            "lots",
        ]))
        .unwrap_err();
        assert!(err.contains("--max-policy-denials"), "{err}");
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--max-policy-idle-seconds",
            "soon",
        ]))
        .unwrap_err();
        assert!(err.contains("--max-policy-idle-seconds"), "{err}");

        // A zero idle window is nonsensical (it would kill instantly): rejected, pointing at the
        // real disable switch.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--max-policy-idle-seconds",
            "0",
        ]))
        .unwrap_err();
        assert!(err.contains("--max-policy-denials 0"), "{err}");

        // Documented in --help.
        assert!(USAGE.contains("--max-policy-denials"));
        assert!(USAGE.contains("--max-policy-idle-seconds"));
    }

    #[test]
    fn profile_defaults_to_ambient() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.primary().profile, ProfileSelector::Ambient);
    }

    #[test]
    fn codex_profile_binds_a_named_profile() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--codex-profile", "work"]))
            .expect("config");
        assert_eq!(
            cfg.primary().profile,
            ProfileSelector::Named("work".to_string())
        );
    }

    #[test]
    fn claude_config_dir_binds_an_explicit_home() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--claude-config-dir",
            r"C:\homes\work",
        ]))
        .expect("config");
        assert_eq!(
            cfg.primary().profile,
            ProfileSelector::ExplicitHome(PathBuf::from(r"C:\homes\work"))
        );
    }

    #[test]
    fn a_profile_flag_on_the_wrong_family_is_rejected() {
        let err = Config::from_args(&args(&["--reviewer", "claude", "--codex-profile", "work"]))
            .unwrap_err();
        assert!(err.contains("applies to a codex reviewer"), "{err}");
    }

    #[test]
    fn a_profile_name_and_an_explicit_home_on_one_entry_conflict() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--codex-profile",
            "work",
            "--codex-home",
            r"C:\x",
        ]))
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn an_invalid_profile_name_is_rejected_at_parse() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--codex-profile",
            "../evil",
        ]))
        .unwrap_err();
        assert!(
            err.contains("invalid character") || err.contains("traversal"),
            "{err}"
        );
    }

    #[test]
    fn an_explicit_home_must_be_absolute() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--codex-home",
            r"relative\path",
        ]))
        .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn profiles_bind_per_chain_entry() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--codex-profile",
            "work",
            "--reviewer",
            "codex",
            "--codex-profile",
            "personal",
        ]))
        .expect("config");
        assert_eq!(
            cfg.reviewers[0].profile,
            ProfileSelector::Named("work".to_string())
        );
        assert_eq!(
            cfg.reviewers[1].profile,
            ProfileSelector::Named("personal".to_string())
        );
        // Same reviewer/model/effort but different profiles are not identity-duplicates.
        assert!(!cfg.reviewers[0].same_reviewer_identity(&cfg.reviewers[1]));
    }

    #[test]
    fn launch_root_is_captured_and_independent_of_cwd() {
        // The launch root is the ambient process cwd, captured before any flag. A `--cwd` pointing
        // elsewhere sets `cwd` but must not move `launch_root` — the Phase 3 authorization key must
        // not be steerable by a flag a reviewed repo can set. (The path need not exist: `--cwd`
        // accepts an arbitrary directory, and `normalize_dir` falls back to the given path when it
        // cannot canonicalize, so the two simply differ.)
        let expected = normalize_dir(std::env::current_dir().expect("cwd"));
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--cwd",
            r"C:\some\other\root",
        ]))
        .expect("config");
        assert_eq!(cfg.launch_root, expected);
        assert_ne!(
            cfg.launch_root, cfg.cwd,
            "a --cwd elsewhere must not move the launch root"
        );

        // With no --cwd, cwd defaults from the same process cwd, so the two coincide.
        let plain = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(plain.launch_root, plain.cwd);
    }

    #[test]
    fn ambient_profile_authorizes_to_no_home() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.resolve_authorized_home(cfg.primary()).unwrap(), None);
        // The account-bearing variant is also `None` for ambient (no profile account to bind — the
        // switch guard therefore never fires for an ambient review).
        assert_eq!(
            cfg.resolve_authorized_home_with_account(cfg.primary())
                .unwrap(),
            None
        );
    }

    #[test]
    fn with_account_denies_an_unauthorized_named_profile() {
        // Isolate the allowlist store so this is hermetic regardless of what the developer machine
        // has genuinely authorized (issue #99).
        let _base = crate::profile::isolate_profile_base();
        // The account-bearing resolver refuses an unauthorized profile just like the home-only one:
        // the profile home does not exist, so its account cannot be read and no allowlist entry can
        // match, so no account is surfaced — it is refused, not silently returned.
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--codex-profile", "work"]))
            .expect("config");
        let err = cfg
            .resolve_authorized_home_with_account(cfg.primary())
            .unwrap_err();
        assert_eq!(err.code, "PROFILE_NOT_AUTHORIZED");
    }

    #[test]
    fn a_named_profile_is_denied_in_phase_1() {
        // Hermetic store: deny regardless of the developer machine's real allowlist (issue #99).
        let _base = crate::profile::isolate_profile_base();
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--codex-profile", "work"]))
            .expect("config");
        let err = cfg.resolve_authorized_home(cfg.primary()).unwrap_err();
        assert_eq!(err.code, "PROFILE_NOT_AUTHORIZED");
    }

    #[test]
    fn an_explicit_home_is_denied_in_phase_1() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--claude-config-dir",
            r"C:\homes\work",
        ]))
        .expect("config");
        let err = cfg.resolve_authorized_home(cfg.primary()).unwrap_err();
        assert_eq!(err.code, "PROFILE_NOT_AUTHORIZED");
    }

    #[test]
    fn identity_flag_before_any_reviewer_is_rejected() {
        // `--model` binds to the most recent `--reviewer`; with none before it, that is almost
        // always a forgotten `--reviewer`, so it is a parse error rather than a silent bind.
        let err = Config::from_args(&args(&["--model", "x"])).unwrap_err();
        assert!(err.contains("--model must follow a --reviewer"), "{err}");
    }

    #[test]
    fn defaults_are_pinned_per_reviewer() {
        let claude = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert_eq!(claude.primary().model, "claude-opus-4-8");
        assert_eq!(claude.primary().effort, "medium");

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(codex.primary().model, "gpt-5.6-luna");
        assert_eq!(codex.primary().effort, "max");
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
        assert_eq!(cfg.primary().model, "gpt-5.6-sol");
        assert_eq!(cfg.primary().effort, "ultra");
    }

    #[test]
    fn min_usage_remaining_binds_to_codex_and_arms_the_chain() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--min-usage-remaining",
            "10",
        ]))
        .expect("config");
        assert_eq!(cfg.primary().usage_minimum, UsageMinimum::Remaining(10));
        assert!(cfg.chain_gates_on_usage());
    }

    #[test]
    fn unset_minimum_does_not_arm_the_chain() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.primary().usage_minimum, UsageMinimum::None);
        assert!(!cfg.chain_gates_on_usage());
    }

    #[test]
    fn min_usage_remaining_rejects_claude_family() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--min-usage-remaining",
            "10",
        ]))
        .unwrap_err();
        assert!(err.contains("--min-usage-status"), "{err}");
    }

    #[test]
    fn min_usage_remaining_range_is_1_to_100() {
        for bad in ["0", "101", "x"] {
            let err = Config::from_args(&args(&[
                "--reviewer",
                "codex",
                "--min-usage-remaining",
                bad,
            ]))
            .unwrap_err();
            assert!(err.contains("--min-usage-remaining"), "{bad}: {err}");
        }
        assert!(Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--min-usage-remaining",
            "1"
        ]))
        .is_ok());
        assert!(Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--min-usage-remaining",
            "100"
        ]))
        .is_ok());
    }

    #[test]
    fn min_usage_status_binds_to_claude_and_rejects_codex() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--min-usage-status",
            "warning",
        ]))
        .expect("config");
        assert_eq!(
            cfg.primary().usage_minimum,
            UsageMinimum::Status(HeadroomLevel::Warning)
        );

        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--min-usage-status",
            "warning",
        ]))
        .unwrap_err();
        assert!(err.contains("--min-usage-remaining"), "{err}");

        let bad = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--min-usage-status",
            "plenty",
        ]))
        .unwrap_err();
        assert!(bad.contains("'ample' or 'warning'"), "{bad}");
    }

    #[test]
    fn min_usage_before_any_reviewer_errors() {
        let err = Config::from_args(&args(&["--min-usage-remaining", "10"])).unwrap_err();
        assert!(err.contains("must follow a --reviewer"), "{err}");
    }

    #[test]
    fn min_usage_twice_in_one_entry_errors() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--min-usage-remaining",
            "10",
            "--min-usage-remaining",
            "20",
        ]))
        .unwrap_err();
        assert!(err.contains("usage minimum was given twice"), "{err}");
    }

    #[test]
    fn duplicate_entry_differing_only_by_minimum_is_still_rejected() {
        // The minimum is a gating policy, not identity: two otherwise-identical codex entries
        // are still a fully-identical duplicate even if their minimums differ.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--min-usage-remaining",
            "10",
            "--reviewer",
            "codex",
            "--min-usage-remaining",
            "20",
        ]))
        .expect("parses");
        assert!(Config::validate_chain(&cfg.reviewers).is_err());
    }

    // ---- reviewer fallback chain (issue #48) ----

    #[test]
    fn single_reviewer_is_a_one_entry_chain() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.reviewers.len(), 1);
        assert_eq!(cfg.primary().reviewer, ReviewerKind::Codex);
        // Capture + timeout + the default block-repair budget + grace.
        let single = crate::vcs::CAPTURE_BUDGET.as_secs()
            + cfg.timeout.as_secs()
            + (cfg.block_repair_timeout.as_secs() + PREFLIGHT_CAP_SECS)
                * cfg.block_repair_attempts as u64
            + FINALIZATION_GRACE_SECS;
        assert_eq!(cfg.max_wait_secs(), single);

        // With repairs disabled the budget is byte-for-byte what it was before they existed.
        let no_repair = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--block-repair-attempts",
            "0",
        ]))
        .expect("config");
        assert_eq!(
            no_repair.max_wait_secs(),
            crate::vcs::CAPTURE_BUDGET.as_secs()
                + no_repair.timeout.as_secs()
                + FINALIZATION_GRACE_SECS
        );
    }

    #[test]
    fn a_repeated_reviewer_starts_a_new_chain_entry_in_order() {
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--model",
            "claude-opus-4-8",
            "--effort",
            "medium",
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--effort",
            "max",
        ]))
        .expect("config");
        assert_eq!(cfg.reviewers.len(), 2);
        assert_eq!(cfg.reviewers[0].reviewer, ReviewerKind::Claude);
        assert_eq!(cfg.reviewers[0].model, "claude-opus-4-8");
        assert_eq!(cfg.reviewers[1].reviewer, ReviewerKind::Codex);
        assert_eq!(cfg.reviewers[1].model, "gpt-5.6-luna");
        // Each entry takes its own per-reviewer defaults when unspecified.
        let defaulted = Config::from_args(&args(&["--reviewer", "claude", "--reviewer", "codex"]))
            .expect("config");
        assert_eq!(defaulted.reviewers[0].model, "claude-opus-4-8");
        assert_eq!(defaulted.reviewers[1].model, "gpt-5.6-luna");
    }

    #[test]
    fn a_doubled_identity_flag_in_one_entry_is_rejected() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--model",
            "a",
            "--model",
            "b",
        ]))
        .unwrap_err();
        assert!(err.contains("--model given twice"), "{err}");
    }

    #[test]
    fn a_multi_entry_budget_adds_a_term_per_fallback() {
        let cfg = Config::from_args(&args(&["--reviewer", "claude", "--reviewer", "codex"]))
            .expect("config");
        // A fallback entry's turn can degrade and repair too, so the repair term appears in both.
        let repair = (cfg.block_repair_timeout.as_secs() + PREFLIGHT_CAP_SECS)
            * cfg.block_repair_attempts as u64;
        let single = crate::vcs::CAPTURE_BUDGET.as_secs()
            + cfg.timeout.as_secs()
            + repair
            + FINALIZATION_GRACE_SECS;
        let per_fallback = PREFLIGHT_CAP_SECS
            + cfg.timeout.as_secs()
            + repair
            + crate::reviewer::DRAIN_GRACE.as_secs();
        assert_eq!(cfg.max_wait_secs(), single + per_fallback);
    }

    #[test]
    fn validate_chain_rejects_a_fully_identical_entry_but_allows_a_different_one() {
        let dup = Config::from_args(&args(&["--reviewer", "codex", "--reviewer", "codex"]))
            .expect("config");
        assert!(Config::validate_chain(&dup.reviewers).is_err());

        // Same reviewer, different model: a legitimate same-family fallback, honoured.
        let same_family = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-sol",
        ]))
        .expect("config");
        assert!(Config::validate_chain(&same_family.reviewers).is_ok());

        assert!(Config::validate_chain(&[]).is_err());
    }

    #[test]
    fn validate_chain_rejects_a_case_or_separator_only_bin_duplicate() {
        // Two entries whose only difference is the case (and separator style) of the --bin path
        // are the same install -- same account, same rate limit -- so the chain must reject them
        // as a duplicate, matching the identity rule resume_entry_index uses.
        let dup = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--bin",
            "C:\\Tools\\codex.exe",
            "--reviewer",
            "codex",
            "--bin",
            "c:/tools/codex.exe",
        ]))
        .expect("config");
        assert!(Config::validate_chain(&dup.reviewers).is_err());

        // A genuinely different bin (different install) is a valid fallback.
        let distinct = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--bin",
            "C:\\Tools\\codex.exe",
            "--reviewer",
            "codex",
            "--bin",
            "C:\\Other\\codex.exe",
        ]))
        .expect("config");
        assert!(Config::validate_chain(&distinct.reviewers).is_ok());
    }

    #[test]
    fn chain_needs_capture_folds_across_entries() {
        // Isolated Codex and shell-less Claude both receive auto through their non-shell evidence
        // path, so a mixed chain captures once for either entry.
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--reviewer", "claude"]))
            .expect("config");
        assert!(cfg.supplies_change_of(ReviewerKind::Codex));
        assert!(cfg.supplies_change_of(ReviewerKind::Claude));
        assert!(cfg.chain_needs_capture());
    }

    #[test]
    fn describe_reviewer_names_the_whole_chain() {
        let one = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert!(!one.describe_reviewer().contains("falling back"));

        let two = Config::from_args(&args(&["--reviewer", "codex", "--reviewer", "claude"]))
            .expect("config");
        let desc = two.describe_reviewer();
        assert!(desc.contains("falling back"), "{desc}");
        assert!(desc.contains("claude"), "{desc}");
    }

    #[test]
    fn describe_reviewer_effective_shows_the_start_entrys_override_pair() {
        // Base effort is max; the `standard` default level resolves to xhigh. The start response
        // headline must name xhigh, matching the `level:` line, rather than the base max (issue
        // #106: the two lines contradicted).
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--effort",
            "max",
            "--level",
            "standard:gpt-5.6-luna:xhigh",
        ]))
        .expect("config");

        // No override -> base pair, identical to describe_reviewer.
        assert!(cfg.describe_reviewer().contains("effort=max"));
        assert!(cfg
            .describe_reviewer_effective(0, None)
            .contains("effort=max"));

        // With the level override -> the effective pair, not the base.
        let ov = LevelOverride {
            model: "gpt-5.6-luna".into(),
            effort: "xhigh".into(),
        };
        let desc = cfg.describe_reviewer_effective(0, Some(&ov));
        assert!(desc.contains("effort=xhigh"), "{desc}");
        assert!(!desc.contains("effort=max"), "{desc}");
    }

    #[test]
    fn describe_reviewer_effective_only_touches_the_start_entry() {
        // A two-entry chain: the override belongs to whichever entry actually starts. When the gate
        // selects the fallback (start_index 1), the primary keeps its base pair and only the
        // fallback shows the override -- a mid-run fallback runs at its own base (`effective_entry`).
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--effort",
            "max",
            "--reviewer",
            "claude",
            "--effort",
            "high",
        ]))
        .expect("config");
        let ov = LevelOverride {
            model: cfg.reviewers[1].model.clone(),
            effort: "low".into(),
        };

        // Override on the primary (start_index 0): the primary shows it, the fallback keeps `high`.
        let on_primary = cfg.describe_reviewer_effective(0, Some(&ov));
        let (primary, fallback) = on_primary
            .split_once("falling back")
            .expect("two-entry chain names a fallback");
        assert!(primary.contains("effort=low"), "{on_primary}");
        assert!(fallback.contains("effort=high"), "{on_primary}");

        // Override on the fallback (start_index 1): the primary keeps `max`, the fallback shows it.
        let on_fallback = cfg.describe_reviewer_effective(1, Some(&ov));
        let (primary, fallback) = on_fallback
            .split_once("falling back")
            .expect("two-entry chain names a fallback");
        assert!(primary.contains("effort=max"), "{on_fallback}");
        assert!(fallback.contains("effort=low"), "{on_fallback}");
    }

    #[test]
    fn describe_with_bin_names_the_resolved_executable_even_for_a_path_entry() {
        // A PATH-resolved entry omits the bin from `describe` (its resolved path is unknown from
        // config alone), but once the run path has resolved one, `describe_with_bin` names it so
        // the rendered identity tells the caller which executable -- and thus which account --
        // actually ran, as docs/reviewer-fallback-chain.md promises.
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        let spec = &cfg.reviewers[0];
        assert!(spec.bin.is_none(), "the test relies on a PATH entry");
        assert!(!spec.describe().contains("bin="), "{}", spec.describe());

        let resolved = spec.describe_with_bin(Path::new("C:/tools/codex.exe"));
        assert!(resolved.contains("bin=C:/tools/codex.exe"), "{resolved}");
        // Still the same entry, just with the executable pinned.
        assert!(resolved.contains("model="), "{resolved}");

        // An explicit --bin already shows in `describe`, and `describe_with_bin` pins the
        // resolved path it was actually launched from (they usually coincide).
        let explicit = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--bin",
            "C:/configured/codex.exe",
        ]))
        .expect("config");
        assert!(
            explicit.reviewers[0]
                .describe()
                .contains("bin=C:/configured/codex.exe"),
            "{}",
            explicit.reviewers[0].describe()
        );
    }

    #[test]
    fn resume_entry_index_matches_the_creating_entry() {
        use crate::session::{RawBin, SessionRecord};
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--model",
            "claude-opus-4-8",
            "--effort",
            "medium",
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--effort",
            "max",
        ]))
        .expect("config");
        let mut rec = SessionRecord {
            reviewer: "codex".into(),
            cli_session_id: "t".into(),
            model: "gpt-5.6-luna".into(),
            effort: "max".into(),
            cwd: String::new(),
            kind: Some(crate::session::KIND_REVIEW.to_string()),
            turns: 1,
            created_unix: 0,
            updated_unix: 0,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: Some(RawBin::PathSearch),
            resolved_bin: None,
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: None,
            profile_identity: None,
        };
        // Matches the fallback entry (index 1), not the primary.
        assert_eq!(cfg.resume_entry_index(&rec), Some(1));
        // A model the chain no longer has: no match.
        rec.model = "gpt-5.6-sol".into();
        assert_eq!(cfg.resume_entry_index(&rec), None);
    }

    #[test]
    fn levels_parse_with_defaults_and_reject_bad_input() {
        // A valid menu populates the entry (sorted by the BTreeMap), and the default names one.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--level",
            "fast:gpt-5.6-luna:high",
            "--level",
            "standard:gpt-5.6-luna:xhigh",
            "--default-level",
            "standard",
        ]))
        .expect("config");
        let p = cfg.primary();
        assert_eq!(p.level_names(), vec!["fast", "standard"]);
        let std = p.resolve_level("standard").expect("declared");
        assert_eq!(
            (std.model.as_str(), std.effort.as_str()),
            ("gpt-5.6-luna", "xhigh")
        );
        assert_eq!(p.default_level.as_deref(), Some("standard"));

        // A duplicate level name on one entry is a mistake, not a precedence contest.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--level",
            "fast:gpt-5.6-luna:high",
            "--level",
            "fast:gpt-5.6-luna:max",
        ]))
        .unwrap_err();
        assert!(err.contains("declared twice"), "{err}");

        // Malformed value: not three colon-separated parts.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--level",
            "fast:gpt-5.6-luna",
        ]))
        .unwrap_err();
        assert!(err.contains("NAME:MODEL:EFFORT"), "{err}");

        // --default-level must name a level that was declared.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--level",
            "fast:gpt-5.6-luna:high",
            "--default-level",
            "thorough",
        ]))
        .unwrap_err();
        assert!(err.contains("names no declared --level"), "{err}");
    }

    #[test]
    fn resume_entry_index_matches_a_level_pair_not_only_the_base() {
        use crate::session::{RawBin, SessionRecord};
        // Base pair is (claude-opus-4-8, medium); the entry also declares thorough=(…, high).
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "claude",
            "--effort",
            "medium",
            "--level",
            "thorough:claude-opus-4-8:high",
        ]))
        .expect("config");
        let rec = |effort: &str| SessionRecord {
            reviewer: "claude".into(),
            cli_session_id: "t".into(),
            model: "claude-opus-4-8".into(),
            effort: effort.into(),
            cwd: String::new(),
            kind: Some(crate::session::KIND_REVIEW.to_string()),
            turns: 1,
            created_unix: 0,
            updated_unix: 0,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: Some(RawBin::PathSearch),
            resolved_bin: None,
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: None,
            profile_identity: None,
        };
        // A session persisted at the base pair binds back.
        assert_eq!(cfg.resume_entry_index(&rec("medium")), Some(0));
        // A session persisted at the *level's* pair — what actually ran when it started at
        // `thorough` — also binds back. This is exactly the regression a base-only match would miss.
        assert_eq!(cfg.resume_entry_index(&rec("high")), Some(0));
        // A pair the entry can produce neither way does not match.
        assert_eq!(cfg.resume_entry_index(&rec("low")), None);
    }

    #[test]
    fn resume_binds_to_the_entry_with_the_matching_profile() {
        use crate::session::{ProfileIdentity, ProfileSelectorId, RawBin, SessionRecord};
        // Two codex entries identical but for their profile.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--codex-profile",
            "work",
            "--reviewer",
            "codex",
            "--codex-profile",
            "personal",
        ]))
        .expect("config");
        let named = |name: &str| {
            Some(ProfileIdentity {
                selector: ProfileSelectorId::Named(name.into()),
                effective_home: None,
                account_fingerprint: None,
            })
        };
        let mut rec = SessionRecord {
            reviewer: "codex".into(),
            cli_session_id: "t".into(),
            model: "gpt-5.6-luna".into(),
            effort: "max".into(),
            cwd: String::new(),
            kind: Some(crate::session::KIND_REVIEW.to_string()),
            turns: 1,
            created_unix: 0,
            updated_unix: 0,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: Some(RawBin::PathSearch),
            resolved_bin: None,
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: None,
            profile_identity: named("personal"),
        };
        // The selector is part of entry identity, so a record created under 'personal' binds to
        // entry 1 and 'work' to entry 0 -- same reviewer/model/effort/bin never misbinds across
        // profiles.
        assert_eq!(cfg.resume_entry_index(&rec), Some(1));
        rec.profile_identity = named("work");
        assert_eq!(cfg.resume_entry_index(&rec), Some(0));
        // A legacy record has no stored selector and matches on the other fields, but it is now
        // ambiguous across the two same-identity entries, so the exact-one legacy rule refuses.
        rec.raw_bin = None;
        rec.profile_identity = None;
        assert_eq!(cfg.resume_entry_index(&rec), None);
    }

    #[test]
    fn resume_entry_index_matches_a_case_or_separator_only_bin_difference() {
        use crate::session::{RawBin, SessionRecord};
        // The chain entry configured one spelling of the bin; the stored record has a case- and
        // separator-only variant. It is the same install, so the resume must still bind to it
        // (this is the #55 fix -- an exact-string compare wrongly refused here).
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--effort",
            "max",
            "--bin",
            "C:\\Tools\\codex.exe",
        ]))
        .expect("config");
        let rec = SessionRecord {
            reviewer: "codex".into(),
            cli_session_id: "t".into(),
            model: "gpt-5.6-luna".into(),
            effort: "max".into(),
            cwd: String::new(),
            kind: Some(crate::session::KIND_REVIEW.to_string()),
            turns: 1,
            created_unix: 0,
            updated_unix: 0,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: Some(RawBin::Explicit("c:/tools/codex.exe".into())),
            resolved_bin: None,
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: None,
            profile_identity: None,
        };
        assert_eq!(cfg.resume_entry_index(&rec), Some(0));

        // A genuinely different bin still does not match.
        let other = SessionRecord {
            raw_bin: Some(RawBin::Explicit("C:\\Other\\codex.exe".into())),
            ..rec
        };
        assert_eq!(cfg.resume_entry_index(&other), None);
    }

    #[test]
    fn resolve_bin_absolutizes_a_relative_explicit_bin() {
        // A relative --bin must resolve to an absolute path, so the stored/compared/run form does
        // not depend on the process cwd (a relative path could otherwise resolve to a different
        // executable after a cwd change, which the resume gate could not detect). `Cargo.toml`
        // exists relative to the crate root, which is the cwd under `cargo test`; resolve_bin only
        // checks is_file(), so it stands in for a relative executable here.
        let cfg = Config::from_args(&args(&["--reviewer", "codex", "--bin", "Cargo.toml"]))
            .expect("config");
        let resolved = crate::reviewer::resolve_bin(&cfg.reviewers[0]).expect("resolve");
        assert!(
            resolved.is_absolute(),
            "resolved bin should be absolute: {resolved:?}"
        );
        assert!(resolved.ends_with("Cargo.toml"), "{resolved:?}");
    }

    #[test]
    fn a_legacy_record_matches_only_when_unambiguous() {
        use crate::session::SessionRecord;
        let mk = |reviewer: &str, model: &str| SessionRecord {
            reviewer: reviewer.into(),
            cli_session_id: "t".into(),
            model: model.into(),
            effort: "max".into(),
            cwd: String::new(),
            kind: Some(crate::session::KIND_REVIEW.to_string()),
            turns: 1,
            created_unix: 0,
            updated_unix: 0,
            cumulative_usage: None,
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
            include_change: None,
            diff_mode: None,
            raw_bin: None, // legacy: no stored bin
            resolved_bin: None,
            findings_ledger: None,
            terminal_reason: None,
            reviewer_cwd_mode: None,
            profile_identity: None,
        };
        // Two same-model/different-bin codex entries: a legacy record is ambiguous -> no match.
        let ambiguous = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--bin",
            "C:\\a\\codex.exe",
            "--reviewer",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--bin",
            "C:\\b\\codex.exe",
        ]))
        .expect("config");
        assert_eq!(
            ambiguous.resume_entry_index(&mk("codex", "gpt-5.6-luna")),
            None
        );
        // Exactly one match: resumes.
        let single = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(
            single.resume_entry_index(&mk("codex", "gpt-5.6-luna")),
            Some(0)
        );
    }

    #[test]
    fn equals_form_is_accepted() {
        // MCP config files vary in how they split args, so both forms must work.
        let cfg =
            Config::from_args(&args(&["--reviewer=claude", "--effort=xhigh"])).expect("config");
        assert_eq!(cfg.primary().reviewer, ReviewerKind::Claude);
        assert_eq!(cfg.primary().effort, "xhigh");
    }

    #[test]
    fn reviewer_aliases_resolve() {
        for alias in ["codex", "chatgpt", "openai", "gpt", "CODEX"] {
            let cfg = Config::from_args(&args(&["--reviewer", alias])).expect("config");
            assert_eq!(cfg.primary().reviewer, ReviewerKind::Codex, "alias {alias}");
        }
        for alias in ["claude", "claude-code", "anthropic"] {
            let cfg = Config::from_args(&args(&["--reviewer", alias])).expect("config");
            assert_eq!(
                cfg.primary().reviewer,
                ReviewerKind::Claude,
                "alias {alias}"
            );
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

        // Over the 24h ceiling is rejected, so the Instant + timeout deadline sites cannot overflow.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--timeout-seconds",
            "99999999",
        ]))
        .unwrap_err();
        assert!(err.contains("at most"), "{err}");

        // Exactly the ceiling is accepted.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--timeout-seconds",
            &MAX_TIMEOUT_SECS.to_string(),
        ]))
        .expect("the ceiling itself is allowed");
        assert_eq!(cfg.timeout.as_secs(), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn max_concurrent_reviews_defaults_and_parses() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.max_concurrent_reviews, DEFAULT_MAX_CONCURRENT_REVIEWS);

        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--max-concurrent-reviews",
            "0",
        ]))
        .expect("config");
        assert_eq!(cfg.max_concurrent_reviews, 0);

        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--max-concurrent-reviews",
            "lots",
        ]))
        .unwrap_err();
        assert!(err.contains("must be an integer"), "{err}");
    }

    #[test]
    fn the_collect_cap_covers_the_whole_lifecycle_and_tracks_the_timeout() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        // Capture budget + reviewer turn + the block-repair budget + finalization grace, so a
        // single blocking collect can cover a whole review -- including a degraded turn that spends
        // its whole turn budget and then re-asks for the block -- rather than a fixed 300s window.
        let expected = crate::vcs::CAPTURE_BUDGET.as_secs()
            + cfg.timeout.as_secs()
            + (cfg.block_repair_timeout.as_secs() + PREFLIGHT_CAP_SECS)
                * cfg.block_repair_attempts as u64
            + FINALIZATION_GRACE_SECS;
        assert_eq!(cfg.max_wait_secs(), expected);
        assert!(cfg.max_wait_secs() > 300);

        // The repair budget is part of it, not a documentation footnote: a turn that repairs must
        // still fit inside the deadline the collect advertises.
        let repairing = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--block-repair-attempts",
            "3",
        ]))
        .expect("config");
        assert!(repairing.max_wait_secs() >= cfg.max_wait_secs() + 2 * (180 + PREFLIGHT_CAP_SECS));

        let bigger =
            Config::from_args(&args(&["--reviewer", "codex", "--timeout-seconds", "3600"]))
                .expect("config");
        assert!(bigger.max_wait_secs() > cfg.max_wait_secs());
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
    fn the_stagnant_session_gate_defaults_overrides_and_disables() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(
            cfg.stagnant_session_turns, DEFAULT_STAGNANT_SESSION_TURNS,
            "the watchdog is on by default; a disabled-by-default gate bounds nothing"
        );

        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--stagnant-session-turns",
            "5",
        ]))
        .expect("config");
        assert_eq!(cfg.stagnant_session_turns, 5);

        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--stagnant-session-turns",
            "0",
        ]))
        .expect("config");
        assert_eq!(
            cfg.stagnant_session_turns, 0,
            "0 disables rather than errors"
        );

        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--stagnant-session-turns",
            "soon",
        ]))
        .expect_err("a non-integer is rejected");
        assert!(err.contains("--stagnant-session-turns"), "{err}");
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
        // Off the evidence path (isolation disabled), a shell-less Claude is told plainly that it
        // has no shell and cannot run git, and to say so rather than guess at the change.
        let claude = Config::from_args(&args(&["--reviewer", "claude", "--allow-reviewer-config"]))
            .expect("config");
        assert!(!claude.reviewer_has_shell());
        let text = claude.reviewer_capabilities(false);
        assert!(text.contains("no shell"), "{text}");
        assert!(text.contains("cannot run `git`"), "{text}");
        // And it must be told to say so rather than guess at the change.
        assert!(text.contains("Do not guess"), "{text}");

        // In scope (profile-pinned, git top-level, shell-less, default rules), it has the read-only
        // evidence tools instead, is still told it has no shell, and is NOT told it cannot run git --
        // because it can, through repository_history/revision.
        let claude_evidence =
            Config::from_args(&args(&["--reviewer", "claude", "--claude-profile", "test"]))
                .expect("config");
        assert!(!claude_evidence.reviewer_has_shell());
        let text = claude_evidence.reviewer_capabilities(false);
        assert!(text.contains("repository_scope"), "{text}");
        assert!(text.contains("no shell"), "{text}");
        assert!(!text.contains("cannot run `git`"), "{text}");

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert!(codex.reviewer_has_shell());
        let text = codex.reviewer_capabilities(false);
        assert!(text.contains("repository_scope"), "{text}");
        assert!(!text.contains("no shell"), "{text}");
        // Codex's shell has its writes denied by the OS sandbox, but the CLI may still
        // refuse a command form in non-interactive mode. A small model at low effort batched
        // its whole reconnaissance into one compound command, had it refused wholesale, and
        // gave up -- so the guidance steers it to one simple command at a time and to fall
        // back rather than abandon the review.
        assert!(text.contains("sterile non-repository directory"), "{text}");
        assert!(text.contains("repository_search"), "{text}");
        // Fix 2 (issue #68): the isolated-Codex capability text steers firmly at the evidence
        // tools and warns that composed shell is refused non-interactively, so a single refusal
        // does not send the reviewer hunting through variants (the behaviour that burned the turn).
        assert!(text.contains("intended way to read and search"), "{text}");
        assert!(text.contains("Select-String"), "{text}");
        assert!(text.contains("refusal is final"), "{text}");
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
        // Isolated Codex receives auto through repository_change; Claude receives it in-prompt.
        let claude = Config::from_args(&args(&["--reviewer", "claude"])).expect("config");
        assert!(claude.supplies_change());

        let codex = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert!(codex.supplies_change());

        let codex_opt_out =
            Config::from_args(&args(&["--reviewer", "codex", "--allow-reviewer-config"]))
                .expect("config");
        assert!(!codex_opt_out.supplies_change());

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
    fn absolute_scoped_rules_pin_a_normal_drive_path_and_fail_closed_otherwise() {
        // A normal drive path becomes forward-slashed, drive-lettered, `/**`-suffixed rules --
        // the shape verified to permit reads under the root and deny outside it.
        let rules = absolute_scoped_rules(std::path::Path::new("C:\\dev\\repo")).expect("rules");
        assert_eq!(
            rules,
            vec![
                "Read(C:/dev/repo/**)",
                "Grep(C:/dev/repo/**)",
                "Glob(C:/dev/repo/**)"
            ]
        );
        // A trailing separator is trimmed rather than doubling the slash.
        assert_eq!(
            absolute_scoped_rules(std::path::Path::new("C:\\dev\\repo\\")).unwrap()[0],
            "Read(C:/dev/repo/**)"
        );

        // Fail closed (None) for everything we cannot represent as a safe literal prefix.
        for hostile in [
            "C:\\work\\[ab]",       // glob character class -- the documented hazard
            "C:\\a*b",              // star
            "C:\\a?b",              // question mark
            "C:\\a{b}",             // brace
            "\\\\server\\share",    // UNC
            "\\\\?\\C:\\dev\\repo", // verbatim prefix
            "relative\\path",       // not absolute
            "/rooted/no/drive",     // rooted without a drive letter
            "C:\\dev\\..\\repo",    // parent-dir component
            "C:\\dev\\.\\x\\repo",  // current-dir component
        ] {
            assert!(
                absolute_scoped_rules(std::path::Path::new(hostile)).is_none(),
                "must fail closed on {hostile:?}"
            );
        }
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
    fn state_dir_key_is_a_frozen_golden_for_a_unicode_cwd() {
        // Family C freeze guard. The hash folds its input with `to_lowercase` (full Unicode), and
        // that fold is a DURABLE key: changing it (e.g. to pathcmp's ASCII fold) would relocate
        // this directory and orphan existing sessions. A plain "two calls agree" check would not
        // catch a fold swap on an ASCII input (to_lowercase and to_ascii_lowercase agree there),
        // nor a change of hash algorithm -- so this pins the exact expected file name for a
        // Unicode-sensitive input (uppercase E-acute, which the two folds treat differently). If
        // it fails, the persistence key moved: do not "fix" the test, understand the migration.
        let dir = default_state_dir(Path::new("C:\\dev\\caf\u{00c9}"));
        let name = dir.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "caf\u{00c9}-855ad2df5623c351");
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
        assert_eq!(cfg.primary().effort, "hyper");
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
        assert!(caps.contains("no new Perforce network calls"), "{caps}");
        assert!(
            !caps.contains("inspect the change history yourself"),
            "{caps}"
        );

        // When the capture failed (diff_supplied = false), a network-denied Codex reviewer --
        // which has a shell but cannot reach Perforce -- must still be told it had no diff and
        // not to guess, and must not be told to rely on a captured change that is not there.
        let no_capture = read_only.reviewer_capabilities(false);
        assert!(
            no_capture.contains("No selected change was captured"),
            "{no_capture}"
        );
        assert!(no_capture.contains("rather than guessing"), "{no_capture}");
        assert!(!no_capture.contains("captured for you"), "{no_capture}");

        // Disabling isolation and granting network restores p4 self-serve at the process level,
        // but Codex is still directed to the evidence service as its bounded primary interface.
        let networked = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--vcs",
            "perforce",
            "--sandbox",
            "danger-full-access",
            "--allow-reviewer-config",
        ]))
        .expect("cfg");
        assert!(networked.reviewer_can_self_serve_change());
        let networked_caps = networked.reviewer_capabilities(false);
        assert!(
            networked_caps.contains("evidence tools remain the preferred bounded interface"),
            "{networked_caps}"
        );
        assert!(
            networked_caps.contains("no new Perforce network calls"),
            "{networked_caps}"
        );

        // Git history is local and self-serve remains technically available without
        // isolation, while the review contract still points Codex at the evidence tools.
        let git = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--vcs",
            "git",
            "--allow-reviewer-config",
        ]))
        .expect("cfg");
        assert!(git.reviewer_can_self_serve_change());
        let git_caps = git.reviewer_capabilities(false);
        assert!(git_caps.contains("repository_history"), "{git_caps}");
        assert!(
            git_caps.contains("evidence tools remain the preferred bounded interface"),
            "{git_caps}"
        );

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
        assert!(caps.contains("repository_history"), "{caps}");
        assert!(!caps.contains("git diff"), "{caps}");
    }
}

#[cfg(test)]
mod block_repair_flag_tests {
    use super::*;

    /// Same helper the sibling `tests` module uses.
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_repair_flags_default_and_parse() {
        let cfg = Config::from_args(&args(&["--reviewer", "codex"])).expect("config");
        assert_eq!(cfg.block_repair_attempts, DEFAULT_BLOCK_REPAIR_ATTEMPTS);
        assert_eq!(
            cfg.block_repair_timeout.as_secs(),
            DEFAULT_BLOCK_REPAIR_TIMEOUT_SECS
        );

        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--block-repair-attempts",
            "0",
            "--block-repair-timeout-seconds",
            "60",
        ]))
        .expect("config");
        assert_eq!(cfg.block_repair_attempts, 0);
        assert_eq!(cfg.block_repair_timeout.as_secs(), 60);
    }

    #[test]
    fn too_many_attempts_are_refused_rather_than_clamped() {
        // Silently clamping would leave the caller believing it configured something it did not.
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--block-repair-attempts",
            "9",
        ]))
        .unwrap_err();
        assert!(err.contains("at most 3"), "{err}");
    }

    #[test]
    fn a_zero_repair_timeout_is_refused_and_points_at_the_flag_that_disables_repairs() {
        let err = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--block-repair-timeout-seconds",
            "0",
        ]))
        .unwrap_err();
        assert!(err.contains("--block-repair-attempts 0"), "{err}");
    }

    #[test]
    fn the_repair_timeout_is_clamped_to_the_turn_budget() {
        // A repair that outlived the review timeout would be a second, larger budget nobody
        // configured -- and it would blow past the collect deadline derived from the same numbers.
        let cfg = Config::from_args(&args(&[
            "--reviewer",
            "codex",
            "--timeout-seconds",
            "90",
            "--block-repair-timeout-seconds",
            "600",
        ]))
        .expect("config");
        assert_eq!(cfg.block_repair_timeout.as_secs(), 90);
    }
}
