//! Claude Code adapter (`claude -p`).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{Headroom, HeadroomLevel, Invocation, Parsed, Reviewer, RunOutcome};
use crate::config::{Config, ReviewerSpec};
use crate::errors::{self, Failure};
use crate::metrics::Usage;

/// Tools removed outright rather than merely denied, so the model has no write
/// affordance to attempt in the first place.
const DENIED_TOOLS: &str = "Edit,Write,NotebookEdit";

pub struct ClaudeReviewer;

impl Reviewer for ClaudeReviewer {
    fn auth_check(
        &self,
        bin: &Path,
        cfg: &Config,
        spec: &ReviewerSpec,
        cancel: &AtomicBool,
    ) -> Result<String, Failure> {
        // Profile: refuse an unauthorized non-ambient profile (the `?`), and for an authorized one run
        // the check in a controlled environment against that home. This is only a *liveness* gate
        // (signed-in), which is cacheable; the identity + auth-method assertion is NOT done here — it
        // must re-run on every spawn (a cached subscription check could miss a later method downgrade),
        // so it lives in the per-spawn preflight in the worker. Ambient leaves the environment
        // untouched -- byte-for-byte today's behaviour.
        let home = cfg.resolve_authorized_home(spec)?;
        let out = run_auth_status(bin, cfg, home.as_deref(), cancel)?;

        // A cancelled probe reports CANCELLED, not a misclassified auth failure: `run` kills the
        // child on cancellation, leaving `success` false and the output partial, which the checks
        // below would otherwise read as "not signed in".
        if out.cancelled {
            return Err(errors::cancelled());
        }

        // `claude auth status` prints JSON on success.
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(out.stdout.trim()) {
            let logged_in = map
                .get("loggedIn")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !logged_in {
                return Err(errors::not_authenticated("claude", out.diagnostics()));
            }
            let method = map
                .get("authMethod")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let account = map
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| map.get("orgName").and_then(Value::as_str))
                .unwrap_or("unknown account");
            return Ok(format!("signed in via {method} as {account}"));
        }

        if out.success {
            // Unrecognised but successful output: trust the exit code rather than
            // blocking a review over a format change.
            return Ok("signed in (auth status output was not recognised)".to_string());
        }
        Err(errors::not_authenticated("claude", out.diagnostics()))
    }

    fn invocation(
        &self,
        cfg: &Config,
        spec: &ReviewerSpec,
        bin: &Path,
        resume: Option<&str>,
        _tmp_id: &str,
        _evidence: Option<&super::EvidenceInvocation<'_>>,
    ) -> std::io::Result<Invocation> {
        let mut cmd = Command::new(bin);
        // Run from a neutral, non-git directory when it is safe and beneficial to (see
        // `claude_neutral_target` and `docs/resume-cache-cwd-invalidation.md`): Claude Code
        // derives a per-invocation git context from its cwd, which the parent agent's
        // between-turn commits change, invalidating the reviewer's cached conversation. Running
        // outside the repo removes that context so a resume stays cache-read-cheap. When it
        // switches, the read scope moves from the relative `./**` (which would now point at the
        // neutral dir) to absolute rules pinned to `cfg.cwd`.
        let neutral = super::claude_neutral_target(cfg, spec.reviewer);
        let (cwd, allowed_tools): (&Path, &[String]) = match &neutral {
            Some((dir, rules)) => (dir.as_path(), rules),
            None => (cfg.cwd.as_path(), &cfg.allowed_tools),
        };
        cmd.current_dir(cwd);
        // Profile: for an authorized non-ambient profile, run in a controlled environment against
        // that home (so the review bills the profile account, and no inherited provider-auth
        // variable can override it). Ambient leaves the environment untouched. An unauthorized
        // profile cannot reach here -- the review path is refused upstream by
        // `resolve_authorized_home` -- but map its `Failure` defensively rather than panic.
        if let Some(home) = cfg
            .resolve_authorized_home(spec)
            .map_err(|e| std::io::Error::other(e.summary))?
        {
            super::apply_controlled_env(&mut cmd, "CLAUDE_CONFIG_DIR", &home);
        }
        cmd.arg("-p");
        if cfg.chain_gates_on_usage() {
            // Armed: stream-json carries the `rate_limit_event` we read headroom from; its terminal
            // `result` event otherwise carries the same fields the buffered document does.
            // `--verbose` is required with `-p` + `stream-json`. Nothing else is added (in
            // particular not `--include-partial-messages`, verified unnecessary). See
            // `docs/usage-remaining-gate.md`.
            cmd.args(["--output-format", "stream-json", "--verbose"]);
        } else {
            cmd.args(["--output-format", "json"]);
        }
        cmd.args(["--model", &spec.model]);
        cmd.args(["--effort", &spec.effort]);
        // dontAsk denies anything outside the allow-list instead of prompting, so a
        // non-interactive run can neither hang nor escalate.
        cmd.args(["--permission-mode", "dontAsk"]);
        cmd.args(["--tools", &cfg.tools]);
        // One argument per rule: a project path containing a space or comma would
        // otherwise be split into fragments by the CLI's list parsing.
        cmd.arg("--allowed-tools");
        for rule in allowed_tools {
            cmd.arg(rule);
        }
        cmd.args(["--disallowed-tools", DENIED_TOOLS]);
        if cfg.isolate_reviewer {
            // The tool allow-list is not the only way a repository can get code to run.
            // A committed `.claude/settings.json` can define a hook, and Claude executes
            // that shell command automatically -- no tool call, so no allow-list check.
            // Verified: a `SessionStart` hook committed to a project ran on a plain
            // `claude -p` invocation and created a file. Reviewing a repository would
            // otherwise mean executing whatever that repository chose to define.
            //
            // --safe-mode disables hooks along with settings, plugins, skills, commands
            // and MCP servers, while leaving auth, model selection and permissions
            // working normally. --bare would also do it but redefines authentication
            // (API key only, no OAuth), which would break subscription sign-in.
            cmd.arg("--safe-mode");
            // Redundant under --safe-mode, kept so MCP isolation does not silently
            // depend on one flag's exact scope.
            cmd.arg("--strict-mcp-config");
        }
        if let Some(session_id) = resume {
            cmd.args(["--resume", session_id]);
        }
        Ok(Invocation {
            command: cmd,
            last_message_file: None,
        })
    }

    fn parse(
        &self,
        cfg: &Config,
        spec: &ReviewerSpec,
        out: &RunOutcome,
        _last_message_file: Option<&Path>,
    ) -> Result<Parsed, Failure> {
        // Armed (usage-observing) runs use `--output-format stream-json`, a JSONL event stream
        // rather than one buffered document, so they take the JSONL path below. A disarmed run is
        // byte-for-byte the buffered `json` path this reviewer has always used.
        if cfg.chain_gates_on_usage() {
            return parse_stream_json(spec, out);
        }
        // The review is stdout-only for this reviewer, so a stdout that hit the cap is
        // not a parse failure to diagnose -- it is a document with its end missing, and
        // saying so is the only accurate report available.
        let parsed: Value = serde_json::from_str(out.stdout.trim()).map_err(|_| {
            if let Some(truncated) = super::truncation_failure(spec, out) {
                truncated
            } else if out.success {
                errors::empty_review("claude", out.diagnostics())
            } else {
                super::failure_for(cfg, spec, out)
            }
        })?;
        from_result_document(spec, out, &parsed)
    }

    /// Armed runs raise the stdout byte cap (`stream-json` carries the review text more than
    /// once), add a line cap, and terminate the child at either bound. Disarmed keeps the
    /// historic retain-and-drain default.
    fn output_limits(&self, cfg: &Config) -> super::StdoutLimits {
        if cfg.chain_gates_on_usage() {
            super::StdoutLimits {
                max_bytes: super::MAX_ARMED_STREAM_BYTES,
                max_lines: super::MAX_ARMED_STREAM_LINES,
                terminate_at_cap: true,
            }
        } else {
            super::StdoutLimits::default_retain()
        }
    }

    /// Claude reports headroom as a `rate_limit_event` in its `stream-json` output. When the
    /// chain is armed the invocation uses `stream-json` (so the event is present); otherwise the
    /// buffered `json` document carries none and this yields `Unknown`. Reads only the CLI-owned
    /// event fields, never model prose. See `docs/usage-remaining-gate.md`.
    fn observe_headroom(&self, _cfg: &Config, _spec: &ReviewerSpec, out: &RunOutcome) -> Headroom {
        last_rate_limit_event(&out.stdout)
            .map(headroom_from_rate_limit_event)
            .unwrap_or(Headroom::Unknown)
    }

    /// The logged-in account, read from the CLI's local account file `~/.claude.json`
    /// (`oauthAccount.accountUuid`, with the org uuid to disambiguate) — a local file, the
    /// account *identifier* only, never the credentials in `~/.claude/.credentials.json`.
    fn account_fingerprint(&self, cfg: &Config, spec: &ReviewerSpec) -> Option<String> {
        claude_account_id(&claude_config_path(cfg, spec)?)
    }

    /// Read the account uuid straight from `home/.claude.json`, without the authorization seam.
    fn fingerprint_at(&self, home: &Path) -> Option<String> {
        claude_account_id(&home.join(".claude.json"))
    }

    /// The per-spawn identity+method probe: run `claude auth status` under the controlled environment
    /// against `home` (for the method), and read the account from `home/.claude.json`. Runs on every
    /// profile spawn, never cached. A cancelled probe reports CANCELLED; unrecognised output fails
    /// closed.
    fn resolve_home_identity(
        &self,
        bin: &Path,
        cfg: &Config,
        home: &Path,
        cancel: &AtomicBool,
    ) -> Result<super::ResolvedIdentity, Failure> {
        let out = run_auth_status(bin, cfg, Some(home), cancel)?;
        if out.cancelled {
            return Err(errors::cancelled());
        }
        let status: Value = serde_json::from_str(out.stdout.trim()).map_err(|_| {
            errors::profile_identity_mismatch(
                "claude",
                "the profile `auth status` output was not valid JSON",
            )
        })?;
        claude_resolve_identity(&status, &home.join(".claude.json"))
    }
}

/// Run `claude auth status` for `home` (an authorized profile home, or `None` for ambient) under the
/// same isolation and controlled environment a review uses, returning the raw outcome. Shared by the
/// liveness gate in [`auth_check`](ClaudeReviewer::auth_check) and the per-spawn identity probe in
/// [`resolve_home_identity`](ClaudeReviewer::resolve_home_identity) so both run the CLI identically.
fn run_auth_status(
    bin: &Path,
    cfg: &Config,
    home: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<RunOutcome, Failure> {
    let mut cmd = Command::new(bin);
    // Isolated and run outside the project, like `invocation`: the reviewer CLI must never load the
    // reviewed repository's configuration, and this runs before every review and on every status call.
    cmd.current_dir(super::neutral_dir(cfg));
    if let Some(home) = home {
        super::apply_controlled_env(&mut cmd, "CLAUDE_CONFIG_DIR", home);
    }
    if cfg.isolate_reviewer {
        cmd.arg("--safe-mode");
        cmd.arg("--strict-mcp-config");
    }
    cmd.arg("auth").arg("status");
    super::run(cmd, "", Duration::from_secs(30), cancel)
        .map_err(|e| errors::spawn_failed("claude", &bin.display().to_string(), e.to_string()))
}

/// Turn a Claude result document — from the buffered `json` path or the terminal `result`
/// event of the armed `stream-json` path — into a `Parsed`. Classification reads only the CLI's
/// own structured fields; `result` is the review *content*, never evidence.
fn from_result_document(
    spec: &ReviewerSpec,
    out: &RunOutcome,
    parsed: &Value,
) -> Result<Parsed, Failure> {
    {
        let session_id = super::normalize_session_id(
            parsed
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        );
        let text = parsed
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        let is_error = parsed
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error || !out.success {
            let subtype = parsed.get("subtype").and_then(Value::as_str).unwrap_or("");

            // Evidence must contain nothing the model wrote: a partial review mentioning
            // line 429, or the phrase "does not support", would otherwise be reported as
            // RATE_LIMITED or MODEL_UNAVAILABLE, sending the user off to edit --model over
            // a coincidence in prose.
            //
            // Note what does NOT qualify: `out.diagnostics()`, because it appends raw
            // stdout and stdout here *is* the result JSON. An earlier version of this
            // split used it and so still classified the review text -- dropping the
            // explicit re-append of `text` achieved nothing on its own. Only stderr and
            // fields the CLI itself owns are evidence.
            let mut evidence = out.stderr.trim().to_string();
            if !subtype.is_empty() {
                evidence = format!("subtype: {subtype}\n{evidence}");
            }
            if let Some(status) = parsed.get("api_error_status").filter(|v| !v.is_null()) {
                evidence = format!("api_error_status: {status}\n{evidence}");
            }
            // Other CLI-owned structured fields that can carry an actionable failure reason (a
            // rate limit surfaced only here, say). Structured metadata, not model prose, so they
            // are evidence; the review text never is (round-1-impl finding f3).
            for field in ["stop_reason", "terminal_reason"] {
                if let Some(v) = parsed
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    evidence = format!("{field}: {v}\n{evidence}");
                }
            }

            // The review text is still shown, just never matched against.
            let mut detail = evidence.clone();
            if !text.is_empty() {
                detail = format!("{detail}\n\n{text}");
            }
            // An expired resume target is detected inside `classify`, which every failure
            // path reaches -- including the one where stdout never parses.
            return Err(errors::classify(
                "claude",
                &spec.model,
                &spec.effort,
                out.exit,
                &evidence,
                &detail,
            ));
        }

        if text.is_empty() {
            return Err(errors::empty_review("claude", out.diagnostics()));
        }

        // Gated on either stream. An earlier revision gated on stderr alone, reasoning that
        // a truncated stdout could not have parsed -- but it can: a complete result
        // document followed by enough trailing whitespace to reach the cap trims back to
        // valid JSON, with bytes discarded after it. The review is intact in that case, so
        // this is a warning and not a failure, but the cap was hit and the README promises
        // that is never silent.
        let mut warnings = Vec::new();
        if out.truncated() {
            warnings.push(
                "The reviewer produced more output than the collection cap allows, so its \
                 output was truncated. The review itself parsed as a complete document, so it \
                 is intact, but anything the reviewer wrote beyond the cap is lost and output \
                 at that volume is abnormal."
                    .to_string(),
            );
        }

        let denials = collect_denials(parsed);
        let denial_count = parsed
            .get("permission_denials")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);

        Ok(Parsed {
            text,
            session_id,
            denials,
            denial_count,
            // Claude's denials come from the final result document. If stdout hit the cap the
            // document does not parse and this returns OUTPUT_TRUNCATED above, so a count that
            // reaches here counted the whole document -- it is never a floor.
            denial_count_is_floor: false,
            warnings,
            usage: collect_usage(parsed),
            // Claude's result document describes the turn that just ran, not the
            // conversation. Verified by the fields themselves: `num_turns` is this
            // invocation's model-call count, and a resumed turn does not inherit the
            // previous one's totals.
            usage_is_cumulative: false,
        })
    }
}

/// Parse the armed `--output-format stream-json` JSONL stream into a `Parsed`, enforcing the
/// armed truncation contract at read time. `result` (model prose) is used only as content; a
/// `rejected` rate-limit event is mapped to `RATE_LIMITED` so the reactive chain still advances;
/// classification never runs raw stdout through the generic classifier. See
/// `docs/usage-remaining-gate.md`.
fn parse_stream_json(spec: &ReviewerSpec, out: &RunOutcome) -> Result<Parsed, Failure> {
    // The armed reader terminated the child the moment a raw byte/line bound was overrun and
    // recorded which; that is authoritative, so report OUTPUT_TRUNCATED naming the bound before
    // trusting a possibly-partial stream (round-2-impl finding f10). A byte overrun may still have
    // let a terminal `result` line through, but the stream is truncated either way.
    if let Some(kind) = out.stdout_cap_hit {
        let bound = match kind {
            super::StreamCapKind::Bytes => {
                format!("{} MiB", super::MAX_ARMED_STREAM_BYTES / (1024 * 1024))
            }
            super::StreamCapKind::Lines => format!("{} event lines", super::MAX_ARMED_STREAM_LINES),
        };
        return Err(errors::output_truncated_at(
            "claude",
            &bound,
            out.diagnostics(),
        ));
    }
    // The stream ended before it was fully drained -- a pipe read error or the collect deadline --
    // so it may be a partial prefix; do not parse it as whole (round-2-impl finding f11).
    if out.stdout_incomplete {
        return Err(errors::output_incomplete("claude", out.diagnostics()));
    }

    let mut result_event: Option<Value> = None;
    // The last CLI-owned failure event (a top-level `error`, or a `result` with `is_error`), kept
    // so a failure represented only by such an event is classified from its structured fields
    // rather than becoming a generic REVIEWER_FAILED (round-2-impl finding f3).
    let mut error_event: Option<Value> = None;
    let mut rate_rejected = false;
    for line in out.stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = value.get("type").and_then(Value::as_str);
        let is_error = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match ty {
            // Keep the last result event: it is the terminal document.
            Some("result") => {
                if is_error {
                    error_event = Some(value.clone());
                }
                result_event = Some(value);
            }
            // A top-level structured error event.
            Some("error") => error_event = Some(value),
            Some("rate_limit_event")
                if value
                    .get("rate_limit_info")
                    .and_then(|i| i.get("status"))
                    .and_then(Value::as_str)
                    == Some("rejected") =>
            {
                rate_rejected = true;
            }
            _ => {}
        }
    }

    // A `rejected` rate-limit event maps to RATE_LIMITED **whether or not** a terminal result
    // event also appears (round-1-impl finding f1): the plan's contract is that the CLI's own
    // structured "no capacity" signal drives fall-through, and checking it only on the no-result
    // path would let a result event mask it and strand the chain. Checked before the result so
    // the rejection wins; `result` content is still never used as evidence.
    if rate_rejected {
        return Err(errors::rate_limited(
            "claude",
            "the reviewer reported a usage limit (rate_limit_event status=rejected)",
        ));
    }

    // A successful terminal result is the review. `from_result_document` also classifies a result
    // that carries `is_error`, from its CLI-owned fields.
    if let Some(parsed) = result_event {
        return from_result_document(spec, out, &parsed);
    }
    // No result, but a structured error event: classify it through the same CLI-owned-fields path.
    if let Some(err) = error_event {
        return from_result_document(spec, out, &err);
    }
    // No result and no structured error event. Classify from stderr and the exit code (CLI-owned),
    // never the JSONL body, which can carry model prose.
    if out.success {
        return Err(errors::empty_review("claude", out.diagnostics()));
    }
    Err(errors::classify(
        "claude",
        &spec.model,
        &spec.effort,
        out.exit,
        out.stderr.trim(),
        &format!(
            "the reviewer produced no terminal result event.\n{}",
            out.stderr.trim()
        ),
    ))
}

/// The *ambient* Claude config home: `$CLAUDE_CONFIG_DIR` when set, else `~` — the same resolution
/// the CLI uses when no profile redirects it.
fn ambient_claude_home() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let p = std::path::PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    super::home_dir()
}

/// Path to Claude Code's account file `.claude.json` in the config home selected for `spec`.
///
/// Profile-aware: an authorized profile's home, else the ambient home. Threaded through
/// [`super::home_for_reads`] so the fingerprint read cannot cross an account-authorization boundary —
/// a non-ambient profile that is unauthorized yields `None` (fail open for accounting). `None` also
/// when no home can be resolved at all.
fn claude_config_path(cfg: &Config, spec: &ReviewerSpec) -> Option<std::path::PathBuf> {
    super::home_for_reads(cfg, spec, ambient_claude_home).map(|h| h.join(".claude.json"))
}

/// Read a stable account identifier from `~/.claude.json`: the OAuth account uuid, combined with
/// the organization uuid so two accounts in different orgs never collide. `None` on any miss so
/// the gate fails open.
fn claude_account_id(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let acct = v.get("oauthAccount")?;
    let uuid = acct.get("accountUuid").and_then(Value::as_str)?;
    let org = acct
        .get("organizationUuid")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!("{org}/{uuid}"))
}

/// Resolve a Claude profile home's account + auth method: the **method** from the `auth status` JSON
/// already obtained (`authMethod == "claude.ai"` + `apiProvider == "firstParty"` is the subscription),
/// the **account** (authoritative) from `.claude.json` via [`claude_account_id`]. As a CLI-side
/// cross-check, the org the CLI reports (`orgId`) must match the org uuid in the file (the prefix of
/// the fingerprint); a mismatch means the CLI resolved a different account than the file, so it
/// **fails closed**. Verified against Claude Code `auth status` (keys `authMethod`, `subscriptionType`,
/// `apiProvider`, `orgId`, `email`); version-pinned so an unrecognised shape refuses. See
/// `docs/reviewer-account-profiles-impl.md`.
fn claude_resolve_identity(
    auth_status: &Value,
    config_path: &Path,
) -> Result<super::ResolvedIdentity, Failure> {
    let account = claude_account_id(config_path).ok_or_else(|| {
        errors::profile_identity_mismatch(
            "claude",
            "the profile config file has no oauth account uuid",
        )
    })?;
    let method = match (
        auth_status.get("authMethod").and_then(Value::as_str),
        auth_status.get("apiProvider").and_then(Value::as_str),
    ) {
        (Some("claude.ai"), Some("firstParty")) => super::AuthMethod::Subscription,
        _ => super::AuthMethod::Other,
    };
    // CLI-side org cross-check: the fingerprint is "{orgUuid}/{accountUuid}", so `orgId` from the
    // CLI must be that prefix. Only enforced when both are present -- an absent orgId is not treated
    // as a mismatch (the account equality remains the primary check upstream).
    if let Some(cli_org) = auth_status.get("orgId").and_then(Value::as_str) {
        if !account.starts_with(&format!("{cli_org}/")) {
            return Err(errors::profile_identity_mismatch(
                "claude",
                "the organization the CLI reports does not match the profile config file",
            ));
        }
    }
    Ok(super::ResolvedIdentity { account, method })
}

/// The last `rate_limit_event.rate_limit_info` object in a `stream-json` stream, if any. Scans
/// JSONL lines and keeps the last match (the freshest reading). Non-JSON and other event types
/// are skipped.
fn last_rate_limit_event(stdout: &str) -> Option<Value> {
    let mut last = None;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') || !line.contains("rate_limit_event") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("rate_limit_event") {
            if let Some(info) = value.get("rate_limit_info") {
                last = Some(info.clone());
            }
        }
    }
    last
}

/// Map a `rate_limit_info` object to `Headroom`. An unrecognised `status` yields `Unknown` so an
/// unlisted future value cannot mis-gate.
fn headroom_from_rate_limit_event(info: Value) -> Headroom {
    let status = info.get("status").and_then(Value::as_str).unwrap_or("");
    match HeadroomLevel::from_status(status) {
        Some(level) => Headroom::Level {
            level,
            resets_at: info.get("resetsAt").and_then(Value::as_u64),
        },
        None => Headroom::Unknown,
    }
}

/// Pull the turn's token accounting out of the result document.
///
/// `num_turns` is the field that explains the bill: it is the number of model calls the
/// reviewer made inside this one review turn, and every one of them re-sent the whole
/// conversation so far. A turn that reads as "one review" is routinely ten calls over a
/// context that grows with each of them.
fn collect_usage(parsed: &Value) -> Usage {
    let u = parsed.get("usage");
    // Absent stays absent. A field this CLI stopped reporting must read as unknown in the
    // log, not as a measured zero -- the log is read by someone asking where their tokens
    // went, and a confident zero is the worst available answer.
    let field = |name: &str| -> Option<u64> { u.and_then(|u| u.get(name)).and_then(Value::as_u64) };
    Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_creation_tokens: field("cache_creation_input_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
        cost_usd: parsed.get("total_cost_usd").and_then(Value::as_f64),
        api_calls: parsed.get("num_turns").and_then(Value::as_u64),
        api_duration_ms: parsed.get("duration_api_ms").and_then(Value::as_u64),
    }
}

/// `permission_denials` tells us which read-only commands the reviewer wanted but
/// could not run. A review that hit several may be thinner than it looks.
fn collect_denials(parsed: &Value) -> Vec<String> {
    let Some(list) = parsed.get("permission_denials").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .take(100)
        .map(|entry| {
            let tool = entry
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let detail = entry
                .get("tool_input")
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str)
                .or_else(|| {
                    entry
                        .get("tool_input")
                        .and_then(|i| i.get("file_path"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("");
            if detail.is_empty() {
                tool.to_string()
            } else {
                format!("{tool}: {detail}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::from_args(&["--reviewer".into(), "claude".into()]).expect("config")
    }

    fn outcome(stdout: &str, success: bool) -> RunOutcome {
        RunOutcome {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit: if success { Some(0) } else { Some(1) },
            success,
            timed_out: false,
            cancelled: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
            stdout_cap_hit: None,
        }
    }

    #[test]
    fn rate_limit_event_maps_status_to_level() {
        let stream = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","resetsAt":1786393200}}"#,
            "\n",
            r#"{"type":"result","is_error":false,"result":"ok"}"#,
        );
        match ClaudeReviewer.observe_headroom(&cfg(), cfg().primary(), &outcome(stream, true)) {
            Headroom::Level { level, resets_at } => {
                assert_eq!(level, HeadroomLevel::Warning);
                assert_eq!(resets_at, Some(1786393200));
            }
            other => panic!("expected Level, got {other:?}"),
        }
    }

    #[test]
    fn buffered_json_has_no_rate_limit_event_so_headroom_is_unknown() {
        // The disarmed default: a single buffered result document carries no rate_limit_event.
        let json = r#"{"type":"result","is_error":false,"result":"ok","session_id":"s"}"#;
        assert_eq!(
            ClaudeReviewer.observe_headroom(&cfg(), cfg().primary(), &outcome(json, true)),
            Headroom::Unknown
        );
    }

    #[test]
    fn unrecognised_status_is_unknown_not_mis_gated() {
        let info: Value = serde_json::from_str(r#"{"status":"some_future_state"}"#).unwrap();
        assert_eq!(headroom_from_rate_limit_event(info), Headroom::Unknown);
    }

    #[test]
    fn claude_account_id_combines_org_and_account_uuid() {
        let dir = std::env::temp_dir().join(format!("cr-claude-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".claude.json");
        std::fs::write(
            &path,
            r#"{"userID":"u","oauthAccount":{"accountUuid":"acc-9","emailAddress":"x@y.z","organizationUuid":"org-1"}}"#,
        )
        .unwrap();
        assert_eq!(claude_account_id(&path), Some("org-1/acc-9".to_string()));
        assert_eq!(claude_account_id(&dir.join("missing.json")), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claude_resolve_identity_checks_method_and_org() {
        use crate::reviewer::AuthMethod;
        let dir = std::env::temp_dir().join(format!("cr-claude-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join(".claude.json");
        std::fs::write(
            &cfg_path,
            r#"{"oauthAccount":{"accountUuid":"acc-9","organizationUuid":"org-1"}}"#,
        )
        .unwrap();
        let status = |json: &str| serde_json::from_str::<Value>(json).unwrap();
        // Subscription (claude.ai / firstParty), org matches the file.
        let id = claude_resolve_identity(
            &status(r#"{"authMethod":"claude.ai","apiProvider":"firstParty","orgId":"org-1"}"#),
            &cfg_path,
        )
        .unwrap();
        assert_eq!(id.account, "org-1/acc-9");
        assert_eq!(id.method, AuthMethod::Subscription);
        // An API-key method resolves to Other.
        assert_eq!(
            claude_resolve_identity(
                &status(r#"{"authMethod":"apiKey","apiProvider":"firstParty","orgId":"org-1"}"#),
                &cfg_path,
            )
            .unwrap()
            .method,
            AuthMethod::Other
        );
        // The CLI reporting a different org than the file fails closed.
        assert_eq!(
            claude_resolve_identity(
                &status(r#"{"authMethod":"claude.ai","apiProvider":"firstParty","orgId":"org-2"}"#),
                &cfg_path,
            )
            .unwrap_err()
            .code,
            "PROFILE_IDENTITY_MISMATCH"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn armed_invocation_switches_to_stream_json() {
        let armed = Config::from_args(&[
            "--reviewer".into(),
            "claude".into(),
            "--min-usage-status".into(),
            "warning".into(),
        ])
        .expect("config");
        let inv = ClaudeReviewer
            .invocation(
                &armed,
                armed.primary(),
                Path::new("claude"),
                None,
                "id",
                None,
            )
            .expect("invocation");
        let argv: Vec<String> = inv
            .command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--output-format" && w[1] == "stream-json"),
            "{argv:?}"
        );
        assert!(argv.iter().any(|a| a == "--verbose"), "{argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--include-partial-messages"),
            "{argv:?}"
        );
        // Disarmed keeps buffered json.
        let dis: Vec<String> = ClaudeReviewer
            .invocation(
                &cfg(),
                cfg().primary(),
                Path::new("claude"),
                None,
                "id",
                None,
            )
            .expect("invocation")
            .command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            dis.windows(2)
                .any(|w| w[0] == "--output-format" && w[1] == "json"),
            "{dis:?}"
        );
    }

    #[test]
    fn armed_parse_extracts_the_terminal_result_event() {
        // A stream-json stream: system + assistant + rate_limit_event + terminal result. The
        // result's fields are parsed exactly as the buffered document's would be.
        let stream = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r###"{"type":"assistant","message":{"content":[{"type":"text","text":"## Verdict APPROVE"}]}}"###,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            "\n",
            r###"{"type":"result","is_error":false,"result":"## Verdict APPROVE","session_id":"s-1","usage":{"input_tokens":5}}"###,
        );
        let parsed = parse_stream_json(cfg().primary(), &outcome(stream, true)).expect("parsed");
        assert_eq!(parsed.text, "## Verdict APPROVE");
        assert_eq!(parsed.session_id.as_deref(), Some("s-1"));
        assert_eq!(parsed.usage.input_tokens, Some(5));
    }

    #[test]
    fn armed_rejected_rate_limit_without_result_is_rate_limited() {
        // The CLI emitted a rejected rate_limit_event and no terminal result: must map to
        // RATE_LIMITED so the reactive fall-through fires, not a generic failure.
        let stream = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1}}"#,
        );
        let err = parse_stream_json(cfg().primary(), &outcome(stream, false)).unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED", "{err:?}");
    }

    #[test]
    fn armed_rejected_rate_limit_wins_even_with_a_result_event() {
        // f1: a rejected rate_limit_event maps to RATE_LIMITED even when a (possibly successful)
        // result event also appears, so the reactive chain still advances.
        let stream = concat!(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#,
            "\n",
            r#"{"type":"result","is_error":false,"result":"ok","session_id":"s"}"#,
        );
        let err = parse_stream_json(cfg().primary(), &outcome(stream, true)).unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED", "{err:?}");
    }

    #[test]
    fn armed_cap_hit_is_a_defined_truncation_naming_the_bound() {
        // The reader (not the parser) detects the overrun and records which bound; the parser
        // reports OUTPUT_TRUNCATED naming it, before trusting the possibly-partial stream.
        let mut lines = outcome("{\"type\":\"system\"}\n", true);
        lines.stdout_cap_hit = Some(crate::reviewer::StreamCapKind::Lines);
        let err = parse_stream_json(cfg().primary(), &lines).unwrap_err();
        assert_eq!(err.code, "OUTPUT_TRUNCATED", "{err:?}");
        assert!(err.summary.contains("event lines"), "{err:?}");

        let mut bytes = outcome("{\"type\":\"system\"}\n", true);
        bytes.stdout_cap_hit = Some(crate::reviewer::StreamCapKind::Bytes);
        let err = parse_stream_json(cfg().primary(), &bytes).unwrap_err();
        assert_eq!(err.code, "OUTPUT_TRUNCATED", "{err:?}");
        assert!(err.summary.contains("MiB"), "{err:?}");
    }

    #[test]
    fn armed_incomplete_stream_is_not_parsed_as_whole() {
        // A pipe error / collect-deadline cut (stdout_incomplete) must not be read as a complete
        // (possibly empty) review (round-2-impl finding f11).
        let mut inc = outcome(r#"{"type":"result","is_error":false,"result":"ok"}"#, true);
        inc.stdout_incomplete = true;
        let err = parse_stream_json(cfg().primary(), &inc).unwrap_err();
        assert_eq!(err.code, "OUTPUT_INCOMPLETE", "{err:?}");
    }

    #[test]
    fn armed_error_event_without_result_is_classified_not_generic() {
        // A structured error event (no result) is classified from its CLI-owned fields rather
        // than falling through to a generic failure (round-2-impl finding f3).
        let stream = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"result","is_error":true,"subtype":"error_max_turns"}"#,
        );
        // A result-with-is_error is a failure; from_result_document classifies it (not EMPTY_REVIEW).
        let err = parse_stream_json(cfg().primary(), &outcome(stream, false)).unwrap_err();
        assert_ne!(err.code, "EMPTY_REVIEW", "{err:?}");
    }

    #[test]
    fn a_truncated_stdout_is_reported_as_truncation_not_as_an_empty_review() {
        // This reviewer's review is stdout-only, so a stdout that hit the cap is a
        // document with its end missing -- not a CLI that wrote nothing. EMPTY_REVIEW
        // would send the caller to retry something that will do the same thing again.
        // Escaped rather than raw: the value contains `"##`, which closes an `r#"` and an
        // `r##"` literal alike.
        let cut_short = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"## Verdi";
        let truncated = RunOutcome {
            stdout_truncated: true,
            ..outcome(cut_short, true)
        };
        let failure = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &truncated, None)
            .expect_err("truncated JSON cannot parse");
        assert_eq!(failure.code, "OUTPUT_TRUNCATED");

        // The same unparseable stdout without the cap having been hit is still an empty
        // review, so the new code cannot swallow the old diagnosis.
        let failure = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(cut_short, true), None)
            .expect_err("still a failure");
        assert_eq!(failure.code, "EMPTY_REVIEW");
    }

    #[test]
    fn a_capped_stdout_that_still_parses_is_reported_as_a_warning() {
        // The case that makes gating on stderr alone wrong: a complete result document
        // followed by trailing whitespace up to the cap trims back to valid JSON, so the
        // review parses and is intact -- but stdout *was* truncated, and the README
        // promises that hitting the cap is never silent.
        let padded = format!(
            "{}{}",
            r#"{"type":"result","subtype":"success","result":"APPROVE","session_id":"s-1"}"#,
            " ".repeat(64)
        );
        let truncated = RunOutcome {
            stdout_truncated: true,
            ..outcome(&padded, true)
        };
        let parsed = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &truncated, None)
            .expect("a complete document still parses");
        assert_eq!(parsed.text, "APPROVE");
        assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
        assert!(
            parsed.warnings[0].contains("truncated"),
            "{:?}",
            parsed.warnings
        );

        // And an untruncated run of the same shape carries no warning.
        let parsed = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(&padded, true), None)
            .expect("parse");
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    #[test]
    fn parses_a_successful_review() {
        let json = r###"{"type":"result","subtype":"success","is_error":false,
            "result":"## Verdict\nAPPROVE","session_id":"3d759777-4801-4e26-b6c5-4fbdb70adbbf",
            "permission_denials":[]}"###;
        let parsed = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(json, true), None)
            .expect("parse");
        assert_eq!(parsed.text, "## Verdict\nAPPROVE");
        assert_eq!(
            parsed.session_id.as_deref(),
            Some("3d759777-4801-4e26-b6c5-4fbdb70adbbf")
        );
        assert!(parsed.denials.is_empty());
    }

    #[test]
    fn usage_is_taken_from_the_result_document() {
        // These fields were parsed and thrown away, which is why a review's cost was
        // invisible to the tool that caused it.
        let json = r#"{"is_error":false,"result":"ok","session_id":"s","num_turns":11,
            "duration_api_ms":412000,"total_cost_usd":3.87,
            "usage":{"input_tokens":142,"output_tokens":9021,
                     "cache_creation_input_tokens":648000,"cache_read_input_tokens":5170000}}"#;
        let parsed = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(json, true), None)
            .expect("parse");

        assert_eq!(parsed.usage.input_tokens, Some(142));
        assert_eq!(parsed.usage.output_tokens, Some(9_021));
        assert_eq!(parsed.usage.cache_creation_tokens, Some(648_000));
        assert_eq!(parsed.usage.cache_read_tokens, Some(5_170_000));
        assert_eq!(parsed.usage.cost_usd, Some(3.87));
        assert_eq!(parsed.usage.api_calls, Some(11));
        assert_eq!(parsed.usage.api_duration_ms, Some(412_000));

        // The figure that actually explains the bill: `input_tokens` on its own is the
        // uncached remainder, and reporting only that reads as a nearly free turn.
        assert_eq!(parsed.usage.billable_input(), 142 + 648_000 + 5_170_000);
    }

    #[test]
    fn a_result_with_no_usage_block_is_still_a_valid_review() {
        // A CLI that changes its reporting must cost us the accounting, not the review.
        // Escaped rather than raw: the value contains `"##`, which closes an `r#"`.
        let json = "{\"is_error\":false,\"result\":\"## Verdict\\nAPPROVE\",\"session_id\":\"s\"}";
        let parsed = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(json, true), None)
            .expect("parse");
        assert_eq!(parsed.text, "## Verdict\nAPPROVE");
        assert!(parsed.usage.is_empty());
    }

    #[test]
    fn surfaces_permission_denials() {
        let json = r#"{"is_error":false,"result":"ok","session_id":"s",
            "permission_denials":[{"tool_name":"Bash","tool_input":{"command":"echo pwned > EVIL.txt"}}]}"#;
        let parsed = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(json, true), None)
            .expect("parse");
        assert_eq!(parsed.denials, vec!["Bash: echo pwned > EVIL.txt"]);
        assert_eq!(parsed.denial_count, 1);
    }

    #[test]
    fn empty_result_is_not_a_review() {
        let json = r#"{"is_error":false,"result":"   ","session_id":"s"}"#;
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &outcome(json, true), None)
            .unwrap_err();
        assert_eq!(err.code, "EMPTY_REVIEW");
    }

    /// A failed run whose review text contains a phrase the classifier looks for.
    ///
    /// These are the regressions that catch classifying on model prose. The first attempt
    /// at the evidence/detail split passed `out.diagnostics()` as evidence, which appends
    /// raw stdout -- and stdout here *is* the result JSON, so the review text was still
    /// being matched. Only stderr and CLI-owned fields are evidence now.
    fn failure_with_review_text(text: &str) -> RunOutcome {
        let json = serde_json::json!({
            "is_error": true,
            "subtype": "error_during_execution",
            "result": text,
            "session_id": "s",
            "api_error_status": null,
        });
        RunOutcome {
            stdout: json.to_string(),
            // Empty: nothing the CLI itself said went wrong.
            stderr: String::new(),
            exit: Some(1),
            success: false,
            timed_out: false,
            cancelled: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
            stdout_cap_hit: None,
        }
    }

    #[test]
    fn a_review_mentioning_429_is_not_reported_as_rate_limited() {
        let out = failure_with_review_text(
            "## Findings\n- `src/lib.rs:429` returns 429 on quota exhaustion; too many requests \
             are not retried.",
        );
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
        // The text is still shown to the user, just never matched against.
        assert!(err.detail.unwrap_or_default().contains("429"));
    }

    #[test]
    fn a_review_saying_does_not_support_is_not_reported_as_model_unavailable() {
        let out = failure_with_review_text(
            "The parser does not support nested groups; this is an invalid model of the grammar.",
        );
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
    }

    #[test]
    fn a_review_mentioning_a_missing_session_is_not_reported_as_session_not_found() {
        let out = failure_with_review_text(
            "`tools.rs:200` returns 'no conversation found' when the session not found branch is \
             hit, which is confusing.",
        );
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
    }

    #[test]
    fn a_real_cli_error_on_stderr_is_still_classified() {
        // The other half of the property: excluding model text must not blind us to the
        // CLI's own diagnosis.
        let mut out = failure_with_review_text("a perfectly ordinary review");
        out.stderr = "Error: 429 Too Many Requests".into();
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED");
    }

    #[test]
    fn a_cli_reported_api_error_status_is_classified() {
        let json = serde_json::json!({
            "is_error": true,
            "subtype": "error",
            "result": "harmless prose",
            "session_id": "s",
            "api_error_status": 401,
        });
        let out = RunOutcome {
            stdout: json.to_string(),
            stderr: String::new(),
            exit: Some(1),
            success: false,
            timed_out: false,
            cancelled: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
            stdout_cap_hit: None,
        };
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "AUTH_EXPIRED_MIDRUN");
    }

    #[test]
    fn non_json_stdout_on_failure_is_classified_not_swallowed() {
        let mut out = outcome("Invalid API key · Please run /login", false);
        out.stderr = "401 unauthorized".into();
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "AUTH_EXPIRED_MIDRUN");
    }

    #[test]
    fn expired_resume_target_maps_to_session_not_found() {
        // Fixture taken from the real thing: `claude -p --resume <bogus-uuid>` exits 1 with
        // the message on stderr and stdout completely empty. The previous fixture put it in
        // a `result` field of valid JSON, which the CLI never does -- so it exercised a
        // branch that could not be reached in practice. Getting this classification right is
        // what lets an expired resume surface as SESSION_NOT_FOUND at all.
        let out = RunOutcome {
            stdout: String::new(),
            stderr: "No conversation found with session ID: \
                     00000000-1111-2222-3333-444444444444"
                .to_string(),
            exit: Some(1),
            success: false,
            timed_out: false,
            cancelled: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
            stdout_cap_hit: None,
        };
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "SESSION_NOT_FOUND");
    }
}
