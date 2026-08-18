//! Codex adapter (`codex exec`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{Headroom, Invocation, Parsed, Reviewer, RunOutcome};
use crate::config::{Config, ReviewerSpec};
use crate::errors::{self, Failure};
use crate::metrics::Usage;

pub struct CodexReviewer;

impl Reviewer for CodexReviewer {
    fn auth_check(
        &self,
        bin: &Path,
        cfg: &Config,
        spec: &ReviewerSpec,
        cancel: &AtomicBool,
    ) -> Result<String, Failure> {
        let mut cmd = Command::new(bin);
        // Run outside the project, like `invocation`, so this preflight is not the one
        // invocation that loads the reviewed repository's configuration. `login status`
        // takes no `--ignore-user-config`, and must not have one anyway: auth is exactly
        // what it is checking.
        cmd.current_dir(super::neutral_dir(cfg));
        // Profile: refuse an unauthorized non-ambient profile (the `?`); for an authorized one, run
        // the liveness check in a controlled environment against that home. This is only a *liveness*
        // gate (signed-in), which is cacheable; the identity + auth-method assertion is NOT done here —
        // it must re-run on every spawn (a cached subscription check could miss a later method
        // downgrade), so it lives in the per-spawn preflight in the worker. Ambient leaves the
        // environment untouched.
        let home = cfg.resolve_authorized_home(spec)?;
        if let Some(home) = &home {
            super::apply_controlled_env(&mut cmd, "CODEX_HOME", home);
        }
        cmd.arg("login").arg("status");
        let out = super::run(cmd, "", Duration::from_secs(30), cancel).map_err(|e| {
            errors::spawn_failed("codex", &bin.display().to_string(), e.to_string())
        })?;

        // A cancelled probe reports CANCELLED, not a misclassified auth failure: `run` kills the
        // child on cancellation, leaving `success` false, which the exit-code check below would
        // otherwise read as "not signed in".
        if out.cancelled {
            return Err(errors::cancelled());
        }

        // The exit code is the signal here, not the text: `codex login status` writes
        // "Logged in using ChatGPT" to stderr, and prints nothing at all when its
        // streams are redirected (verified). Matching on stdout would report a
        // signed-in user as unauthenticated.
        if !out.success {
            return Err(errors::not_authenticated("codex", out.diagnostics()));
        }
        let reported = out
            .diagnostics()
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("signed in")
            .to_string();
        Ok(reported)
    }

    fn invocation(
        &self,
        cfg: &Config,
        spec: &ReviewerSpec,
        bin: &Path,
        resume: Option<&str>,
        tmp_id: &str,
        evidence: Option<&super::EvidenceInvocation<'_>>,
        pinned_home: Option<&crate::config::AuthorizedHome>,
    ) -> std::io::Result<Invocation> {
        let evidence = evidence.ok_or_else(|| {
            std::io::Error::other("Codex invocation requires a verified evidence capability")
        })?;
        let last_message_file = super::tmp_file(cfg, tmp_id, "codex-last.txt")?;

        let mut cmd = Command::new(bin);
        cmd.current_dir(evidence.sterile_dir.unwrap_or(&cfg.cwd));
        // Profile: for an authorized non-ambient profile, run in a controlled environment against
        // that home so the review bills the profile account and no inherited provider-auth variable
        // can override it. Ambient leaves the environment untouched. An unauthorized profile cannot
        // reach here (the review path is refused upstream); map its `Failure` defensively.
        //
        // NOTE (validate in Phase 3, when profiles are actually authorized): `exec` spawns the
        // evidence MCP server (our own binary, by absolute path) which the config pins to
        // `env={}`; the controlled environment here must still leave that server able to start and
        // read the tree. The allowlist carries the OS essentials (`SystemRoot`, `PATH`, `TEMP`, …),
        // so it should, but this end-to-end interaction is only exercisable once a profile is
        // authorized -- confirm it with `smoke.ps1` then.
        // The account pinned for this attempt, never a fresh resolve: see the trait doc.
        if let Some(home) = pinned_home {
            super::apply_controlled_env(&mut cmd, "CODEX_HOME", &home.home);
        }
        cmd.arg("exec");

        match resume {
            Some(session_id) => {
                // Positional order is fixed: resume <SESSION_ID> [PROMPT].
                cmd.arg("resume").arg(session_id).arg("-");
            }
            None => {
                cmd.arg("-");
                // `-s` exists only on the fresh-session form.
                cmd.args(["-s", &cfg.sandbox]);
            }
        }

        cmd.arg("--json");
        cmd.arg("--skip-git-repo-check");
        cmd.arg("--strict-config");
        cmd.args(["-m", &spec.model]);
        // Stated on every turn, including resumes, via the config override that `resume`
        // does accept. A resumed session does appear to retain the policy it was created
        // with -- verified: a write attempt on turn 2 of a `-s read-only` session was
        // refused -- but relying on that meant the sandbox was the one setting inherited
        // by accident rather than asserted, while `-m` and effort were both re-passed.
        // Verified that `resume` accepts this override and still refuses writes.
        cmd.args(["-c", &format!("sandbox_mode=\"{}\"", cfg.sandbox)]);
        // No shell is involved, so the quotes are part of the value and make this a
        // TOML string rather than relying on the raw-literal fallback.
        cmd.args(["-c", &format!("model_reasoning_effort=\"{}\"", spec.effort)]);
        if cfg.isolate_reviewer {
            // `codex exec` does start configured MCP servers (verified: a marker server
            // ran and left a file), so a reviewer that also has cross-review registered
            // could call back into us. `-c mcp_servers={}` does not help -- dotted
            // overrides merge into the existing table rather than replacing it -- so skip
            // the user config entirely. Auth still resolves from CODEX_HOME, and model,
            // effort and sandbox are all passed explicitly above.
            cmd.arg("--ignore-user-config");
            cmd.arg("--ignore-rules");
        }
        let command = toml_path(evidence.executable)?;
        let bundle = toml_path(evidence.bundle_file)?;
        let nonce = toml_string(evidence.nonce);
        let evidence_cwd = toml_path(evidence.sterile_dir.unwrap_or(&cfg.cwd))?;
        let enabled_tools =
            serde_json::to_string(&crate::evidence::TOOLS).map_err(std::io::Error::other)?;
        cmd.args([
            "-c",
            &format!(
                "mcp_servers.{}.command={command}",
                crate::evidence::SERVER_NAME
            ),
        ]);
        cmd.args([
            "-c",
            &format!(
                "mcp_servers.{}.args=[{}, {bundle}, {nonce}]",
                crate::evidence::SERVER_NAME,
                toml_string(crate::evidence::SERVER_FLAG)
            ),
        ]);
        for value in [
            format!("mcp_servers.{}.required=true", crate::evidence::SERVER_NAME),
            format!("mcp_servers.{}.enabled=true", crate::evidence::SERVER_NAME),
            format!(
                "mcp_servers.{}.enabled_tools={enabled_tools}",
                crate::evidence::SERVER_NAME
            ),
            format!(
                "mcp_servers.{}.disabled_tools=[]",
                crate::evidence::SERVER_NAME
            ),
            format!("mcp_servers.{}.env={{}}", crate::evidence::SERVER_NAME),
            format!(
                "mcp_servers.{}.cwd={evidence_cwd}",
                crate::evidence::SERVER_NAME
            ),
            format!(
                "mcp_servers.{}.startup_timeout_sec=15",
                crate::evidence::SERVER_NAME
            ),
            format!(
                "mcp_servers.{}.tool_timeout_sec={}",
                crate::evidence::SERVER_NAME,
                crate::evidence::CODEX_TOOL_TIMEOUT_SECS
            ),
            format!(
                "mcp_servers.{}.default_tools_approval_mode=\"approve\"",
                crate::evidence::SERVER_NAME
            ),
        ] {
            cmd.args(["-c", &value]);
        }
        cmd.arg("-o").arg(&last_message_file);

        Ok(Invocation {
            command: cmd,
            last_message_file: Some(last_message_file),
        })
    }

    fn parse(
        &self,
        _cfg: &Config,
        spec: &ReviewerSpec,
        out: &RunOutcome,
        last_message_file: Option<&Path>,
    ) -> Result<Parsed, Failure> {
        let events = parse_events(&out.stdout);

        if !out.success {
            // Evidence excludes stdout on purpose. The JSONL stream carries
            // `agent_message` items, so classifying on it would let the reviewer's own
            // prose choose the failure code -- a review mentioning 429 becoming
            // RATE_LIMITED. Only stderr and the stream's own error events qualify.
            let mut evidence = out.stderr.trim().to_string();
            if !events.errors.is_empty() {
                evidence = format!("{}\n{}", events.errors.join("\n"), evidence);
            }

            let mut detail = out.diagnostics();
            if !events.errors.is_empty() {
                detail = format!("{}\n\n{}", events.errors.join("\n"), detail);
            }

            // Every evidence signal in the turn is collected before any of them is acted on, and
            // the graver ones dominate (round-4 finding f10). Deciding on "is *any* message an
            // abandoned call" let a timeout sitting beside a genuinely dead service — a
            // `cross_review_evidence: connection closed` — be reported as the retryable one.
            //
            // **All three sources, at their own scoping.** The round-4 fix aggregated only the
            // stream errors and stderr and missed the per-item errors entirely, so a dead service
            // reported on an `mcp_tool_call` item beside a stream-level abandon was still masked —
            // the same defect the finding named, one source further along, and the round-4 test
            // passed only because it put the fatal error in a stream event rather than on an item
            // (round-6, f10 held open). Item errors are server-scoped by the parser; stream errors
            // and stderr are not and must name the server themselves.
            let evidence_signals: Vec<EvidenceSignal> =
                collect_evidence_signals(&events, &out.stderr, true)
                    .into_iter()
                    .map(|(signal, _)| signal)
                    .collect();
            let unusable = evidence_signals
                .iter()
                .any(|s| matches!(s, EvidenceSignal::StartupFailure | EvidenceSignal::Unusable));
            if unusable {
                return Err(errors::evidence_unavailable(detail));
            }
            // The turn died on an abandoned evidence call rather than a service that never came
            // up. Nothing usable survives — the CLI exited non-zero, so there is no verdict to
            // trust — but the caller is owed the true cause and its true remedy, which is to retry
            // rather than to go looking at an executable and a state directory that are fine.
            if evidence_signals.contains(&EvidenceSignal::AbandonedCall) {
                // The same proof the completed path requires (round-4 finding f11): this code's
                // own message tells the caller the service was running and answering, so it may
                // only be used where something actually answered. A lone abandoned call with
                // nothing successful beside it is a service that never demonstrably worked, and
                // saying otherwise would be the failure code misreporting the reviewer's state —
                // the very thing this change set out to stop doing.
                if events.evidence_calls_succeeded > 0 {
                    return Err(errors::evidence_call_abandoned(detail));
                }
                return Err(errors::evidence_unavailable(detail));
            }
            // An expired resume target is detected inside `classify`.
            return Err(errors::classify(
                "codex",
                &spec.model,
                &spec.effort,
                out.exit,
                &evidence,
                &detail,
            ));
        }

        // An evidence error that means the *service* is unusable still invalidates the review, even
        // a complete one: everything it says may rest on evidence it could not get and does not know
        // it missed. An abandoned *call* is different in kind — the service answered before and
        // after, and the reviewer was handed the tool error in-band, so it could account for the gap
        // exactly as it accounts for a refused shell command. That one is carried as a warning on a
        // review that otherwise completed, in the same channel as a truncated capture: the caller is
        // told the evidence was short, rather than losing a finished review to it (#71).
        //
        // The demotion is **conditional on proof, not on phrasing** (round-1 finding f3). The
        // earlier draft's warning asserted that the service stayed up while checking nothing of the
        // sort; here a turn qualifies only if at least one evidence call actually *succeeded*
        // alongside the abandoned one. With no successful call there is no evidence the service was
        // ever usable, so the fail-closed reading stands and the review is invalidated as before.
        // Both branches classify through the same function, in the same order, for the reason
        // round-2 finding f6 found the hard way: this path filtered on "is it an abandoned call"
        // alone, so an item error carrying the decisive `required MCP servers failed to initialize`
        // marker *and* a timeout phrase was demoted to a warning and the review returned. A
        // startup failure must outrank an abandoned call wherever the two are told apart, not just
        // where I happened to write the precedence first.
        //
        // Round-7 finding f15: this path read only the item errors, so a server-named failure
        // arriving as a *stream* error on a turn that exited zero — `RunOutcome::success` is the
        // process status, not the absence of error events — returned a verdict over unusable
        // evidence. It now draws on the same collector as the failed branch.
        let service_answered = events.evidence_calls_succeeded > 0;
        let signals = collect_evidence_signals(&events, &out.stderr, false);
        let demoted =
            |signal: EvidenceSignal| service_answered && signal == EvidenceSignal::AbandonedCall;
        let abandoned_evidence_calls: Vec<String> = signals
            .iter()
            .filter(|(signal, _)| demoted(*signal))
            .map(|(_, message)| message.clone())
            .collect();
        let unusable_service: Vec<&String> = signals
            .iter()
            .filter(|(signal, _)| !demoted(*signal) && *signal != EvidenceSignal::Unrelated)
            .map(|(_, message)| message)
            .collect();
        if !unusable_service.is_empty() {
            let errors = unusable_service
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let diagnostics = out.diagnostics();
            let detail = if diagnostics.trim().is_empty() {
                errors
            } else {
                format!("{errors}\n\n--- complete reviewer CLI diagnostics ---\n{diagnostics}")
            };
            return Err(errors::evidence_unavailable(detail));
        }

        // The final-message file is authoritative; the event stream is the fallback. It is read
        // capped at the same 8 MiB per-stream cap: the CLI writes it directly rather than through
        // the pipes we cap, but "not capped by the pipe" is not "unbounded", and a real final
        // message is kilobytes. An over-cap read is checked *before* any decode -- 8 MiB of bytes
        // can be cut mid-codepoint -- and becomes OUTPUT_TRUNCATED rather than a truncated verdict
        // parsed as if complete. This tightens an earlier behaviour where an over-8-MiB final
        // message would have been returned as a valid review.
        let from_file = match last_message_file {
            Some(p) => match crate::vcs::read_capped(p, super::MAX_OUTPUT_BYTES) {
                Ok((_, true)) => {
                    let mib = super::MAX_OUTPUT_BYTES / (1024 * 1024);
                    return Err(errors::output_truncated(
                        "codex",
                        mib,
                        format!("the reviewer's final-message file exceeded {mib} MiB"),
                    ));
                }
                // Within cap: decode only now. Invalid UTF-8 falls back to the event stream, as an
                // unreadable file did before.
                Ok((bytes, false)) => String::from_utf8(bytes)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                Err(_) => None,
            },
            None => None,
        };

        let text = match from_file {
            // The final-message file is written by the CLI directly, not through the pipes
            // we cap, so a capped event stream does not put this review in doubt.
            Some(text) => text,
            None => {
                // Without the file the fallback is the event stream -- and under truncation
                // that stream is not trustworthy. `last_message` would be the last one that
                // *fit*, so the reviewer's actual conclusion may be among the discarded
                // bytes, and returning an earlier message as the verdict would be a
                // silently wrong review rather than a visible failure.
                if let Some(truncated) = super::truncation_failure(spec, out) {
                    return Err(truncated);
                }
                events.last_message.clone().unwrap_or_default()
            }
        };

        if text.is_empty() {
            let detail = if events.errors.is_empty() {
                out.diagnostics()
            } else {
                events.errors.join("\n")
            };
            return Err(errors::empty_review("codex", detail));
        }

        // Outside the match, so every surviving-review path reports the cap -- including
        // the one where only stderr was capped and the review came from the event stream.
        //
        // Note what this does *not* claim. A surviving review here came from a final-message file
        // that read *within* the 8 MiB cap (an over-cap file returned OUTPUT_TRUNCATED above), so
        // its content is complete as far as the cap can tell. Whether the CLI finished *writing*
        // that file is a different question, it has not been observed for a run that produced this
        // much output, and it is not asserted here.
        let mut warnings = Vec::new();
        if out.truncated() {
            warnings.push(
                "The reviewer produced more output than the collection cap allows, so its \
                 transcript was truncated. The review below is reported as the reviewer gave \
                 it, but anything that appeared only in the transcript is lost, and output at \
                 that volume is itself abnormal."
                    .to_string(),
            );
        }
        if events.input_inconsistent {
            warnings.push(
                "The reviewer's own usage report was inconsistent -- it counted more cached \
                 input than total input -- so the uncached (fresh) input for this run is left \
                 unreported rather than shown as a measured zero. The token totals below may \
                 therefore understate the input."
                    .to_string(),
            );
        }
        if events.evidence_calls > 0 {
            warnings.push(format!(
                "The repository evidence service completed {} tool call(s) during this turn.",
                events.evidence_calls
            ));
        }
        if !abandoned_evidence_calls.is_empty() {
            warnings.push(format!(
                "{abandoned} repository evidence call(s) were abandoned by the reviewer CLI's own \
                 per-call timeout, so the reviewer did not receive that evidence and the review \
                 below may rest on less than it intended to read. {succeeded} other evidence \
                 call(s) in this turn completed, which is why this is reported rather than treated \
                 as an unusable service. Two limits on that, stated rather than implied: it shows \
                 the service answered somewhere in the turn, not that it was healthy either side \
                 of the abandoned call, and what the reviewer did about the gap it was told of \
                 in-band is its own account to give and is not checked here. {detail}",
                abandoned = abandoned_evidence_calls.len(),
                succeeded = events.evidence_calls_succeeded,
                detail = abandoned_evidence_calls.join("; ")
            ));
        }

        let denial_count = policy_denial_count(&out.stderr);
        let denials = collect_denials(&out.stderr);

        Ok(Parsed {
            text,
            session_id: super::normalize_session_id(events.thread_id),
            denials,
            denial_count,
            // The router writes these to stderr, so a capped stderr drops the later ones and
            // the retained count is only a floor. This is the sole path that can produce a
            // truncated stream and still return a review, so it is the only one that sets it.
            denial_count_is_floor: out.stderr_truncated,
            warnings,
            usage: Usage {
                api_calls: (events.turns_seen > 0).then_some(events.turns_seen),
                ..events.usage
            },
            usage_is_cumulative: true,
        })
    }

    /// Codex writes its usage headroom to the per-session rollout log, not to `exec --json`
    /// stdout: locate the rollout by the `thread_id` stdout announced, read a bounded tail, and
    /// take the last `token_count.rate_limits`. Fail-open to `Unknown` on any miss. This is a
    /// pure post-turn read that changes no invocation. See `docs/usage-remaining-gate.md`.
    fn observe_headroom(&self, cfg: &Config, spec: &ReviewerSpec, out: &RunOutcome) -> Headroom {
        let Some(thread_id) = parse_events(&out.stdout).thread_id else {
            return Headroom::Unknown;
        };
        // Read the rollout from the same home the review ran under (profile-aware), not the ambient
        // one -- otherwise the headroom would describe a different account than the usage key.
        let Some(home) = codex_home(cfg, spec) else {
            return Headroom::Unknown;
        };
        match find_rollout(&home, &thread_id) {
            Some(path) => headroom_from_rollout(&path),
            None => Headroom::Unknown,
        }
    }

    /// The effective read home is `$CODEX_HOME` selected for `spec` — the profile's authorized home,
    /// the ambient `$CODEX_HOME` for an ambient spec, or `None` for an unauthorized profile. This is
    /// the same directory the rollout read above uses, and the one `account_fingerprint` (the default
    /// `fingerprint_at` over this) and the gate's reread share. Profile-aware.
    fn effective_read_home(&self, cfg: &Config, spec: &ReviewerSpec) -> Option<PathBuf> {
        codex_home(cfg, spec)
    }

    /// Read `tokens.account_id` straight from `home/auth.json`, without the authorization seam. The
    /// logged-in ChatGPT account *identifier* (never the OAuth tokens beside it), no CLI call.
    fn fingerprint_at(&self, home: &Path) -> Option<String> {
        codex_account_id(home)
    }

    /// The per-spawn identity+method probe: read the home's `auth.json` (no CLI call). Fails closed on
    /// a missing/unreadable/unrecognised file. Runs on every profile spawn, never cached.
    fn resolve_home_identity(
        &self,
        _bin: &Path,
        _cfg: &Config,
        home: &Path,
        _cancel: &AtomicBool,
    ) -> Result<super::ResolvedIdentity, Failure> {
        codex_resolve_identity(home)
    }

    /// Bare `codex login` (subscription/browser OAuth), under the controlled environment with
    /// `CODEX_HOME` pointed at the staging dir. Never `--with-api-key`/`--with-access-token`.
    fn login_command(&self, bin: &Path, home: &Path) -> Result<Command, Failure> {
        let mut cmd = Command::new(bin);
        super::apply_controlled_env(&mut cmd, "CODEX_HOME", home);
        cmd.arg("login");
        Ok(cmd)
    }

    /// Codex writes its subscription tokens to `auth.json` — the single credential + account file.
    fn credential_files(&self) -> &'static [&'static str] {
        &["auth.json"]
    }

    /// Both the account and the method come from `auth.json`, so the setup confirmation is a single
    /// **handle-relative** read of that file through the held home (no CLI call, no by-path window,
    /// f-a5). Fails closed on a missing/unrecognised shape.
    fn confirm_setup_identity(
        &self,
        _bin: &Path,
        _cfg: &Config,
        home: &crate::profile::SecuredProfileDir,
        _scratch_cwd: &Path,
        _cancel: &AtomicBool,
    ) -> Result<super::ResolvedIdentity, Failure> {
        let bytes = home
            .read_credential(std::ffi::OsStr::new("auth.json"))
            .map_err(|e| {
                errors::profile_identity_mismatch(
                    "codex",
                    &format!("could not read the provisioned auth file: {e}"),
                )
            })?;
        codex_identity_from_bytes(&bytes)
    }
}

fn toml_string(value: &str) -> String {
    // JSON string escaping is a strict subset of TOML basic-string escaping for these paths and
    // tokens, and unlike ad-hoc quoting handles spaces, quotes, backslashes and Unicode together.
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn toml_path(path: &Path) -> std::io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path is not representable in Codex TOML configuration: {path:?}"),
        )
    })?;
    Ok(toml_string(value))
}

/// The *ambient* Codex home: `$CODEX_HOME`, or `~/.codex` when unset — the same resolution the CLI
/// uses for its session and auth state when no profile redirects it.
fn ambient_codex_home() -> Option<std::path::PathBuf> {
    // A `#[cfg(test)]` seam so a call-site test can point the ambient home at a seeded temp dir
    // *without* mutating the process-wide `$CODEX_HOME`. The codebase deliberately keeps env
    // mutation out of tests (so ambient reads like `codex_home_reads_ambient_…` stay parallel-safe);
    // a thread-local override is per-test-thread, so it races nothing (issue #81 follow-up, f17). It
    // compiles out of the release binary entirely.
    #[cfg(test)]
    if let Some(home) = test_ambient_home_override() {
        return Some(home);
    }
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        let p = std::path::PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    super::home_dir().map(|h| h.join(".codex"))
}

#[cfg(test)]
thread_local! {
    /// Test-only override for [`ambient_codex_home`]. Set for the duration of a call-site test via
    /// [`AmbientHomeGuard`]; `None` (the default) leaves the real `$CODEX_HOME`/`~/.codex` resolution.
    static TEST_AMBIENT_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_ambient_home_override() -> Option<PathBuf> {
    TEST_AMBIENT_HOME.with(|o| o.borrow().clone())
}

/// RAII guard that points [`ambient_codex_home`] at `home` on this thread until it drops. Scoped so a
/// test cannot leak the override into a sibling test sharing the thread (they do not, but the guard
/// makes it structural). Restores the previous value on drop, so nesting is well-behaved.
#[cfg(test)]
pub(crate) struct AmbientHomeGuard(Option<PathBuf>);

#[cfg(test)]
pub(crate) fn override_ambient_home(home: PathBuf) -> AmbientHomeGuard {
    let prev = TEST_AMBIENT_HOME.with(|o| o.replace(Some(home)));
    AmbientHomeGuard(prev)
}

#[cfg(test)]
impl Drop for AmbientHomeGuard {
    fn drop(&mut self) {
        TEST_AMBIENT_HOME.with(|o| *o.borrow_mut() = self.0.take());
    }
}

/// The Codex home to read an account from for `spec`: an authorized profile's home, else the ambient
/// home. Threaded through [`super::home_for_reads`] so a read cannot cross an account-authorization
/// boundary; `None` for a non-ambient profile that is unauthorized (accounting then fails open) or
/// when no home can be resolved.
fn codex_home(cfg: &Config, spec: &ReviewerSpec) -> Option<std::path::PathBuf> {
    super::home_for_reads(cfg, spec, ambient_codex_home)
}

/// Resolve a Codex profile home's account + auth method from `$CODEX_HOME/auth.json` (pure,
/// file-based — the airtight controlled env guarantees this is the file the reviewer uses).
/// `auth_mode == "chatgpt"` is the subscription method; `tokens.account_id` is the account. A
/// missing, unreadable, or unrecognised file **fails closed** (`Err`), never a silent pass. Verified
/// against Codex CLI 0.144.5; the shape is version-pinned so a drift refuses. See
/// `docs/reviewer-account-profiles-impl.md`.
fn codex_resolve_identity(home: &Path) -> Result<super::ResolvedIdentity, Failure> {
    let bytes = std::fs::read(home.join("auth.json")).map_err(|e| {
        errors::profile_identity_mismatch(
            "codex",
            &format!("could not read the profile auth file: {e}"),
        )
    })?;
    codex_identity_from_bytes(&bytes)
}

/// Parse a Codex `auth.json`'s account + method from its **bytes**, so the same check serves a by-path
/// read (the review probe) and a handle-relative read (the setup confirmation, f-a5). `auth_mode ==
/// "chatgpt"` is the subscription method; `tokens.account_id` is the account. Any missing/unrecognised
/// shape **fails closed**.
fn codex_identity_from_bytes(bytes: &[u8]) -> Result<super::ResolvedIdentity, Failure> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| {
        errors::profile_identity_mismatch("codex", "the profile auth file is not valid JSON")
    })?;
    let account = v
        .get("tokens")
        .and_then(|t| t.get("account_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            errors::profile_identity_mismatch(
                "codex",
                "the profile auth file has no tokens.account_id",
            )
        })?
        .to_string();
    let method = match v.get("auth_mode").and_then(Value::as_str) {
        Some("chatgpt") => super::AuthMethod::Subscription,
        _ => super::AuthMethod::Other,
    };
    Ok(super::ResolvedIdentity { account, method })
}

/// Read `tokens.account_id` from `$CODEX_HOME/auth.json`. Returns `None` on any miss so the gate
/// fails open rather than gating on an identity it could not establish.
fn codex_account_id(home: &Path) -> Option<String> {
    let bytes = std::fs::read(home.join("auth.json")).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("tokens")?
        .get("account_id")?
        .as_str()
        .map(str::to_string)
}

/// Locate the rollout log for a thread by descending the newest day directories under
/// `sessions/` (bounded: at most a few `read_dir`s, never a recursive `**` walk), matching the
/// `-<thread_id>.jsonl` filename suffix. See `docs/usage-remaining-gate.md`.
fn find_rollout(home: &Path, thread_id: &str) -> Option<std::path::PathBuf> {
    let sessions = home.join("sessions");
    let suffix = format!("-{thread_id}.jsonl");
    // Descend year -> month -> day, taking the newest few names at each level (lexicographic
    // order equals chronological for the zero-padded YYYY/MM/DD layout). Two days cover a
    // midnight rollover.
    let newest = |dir: &Path, keep: usize| -> Vec<std::path::PathBuf> {
        let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        names.sort();
        names.into_iter().rev().take(keep).collect()
    };
    for year in newest(&sessions, 2) {
        for month in newest(&year, 2) {
            for day in newest(&month, 2) {
                if let Ok(entries) = std::fs::read_dir(&day) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        if name.to_string_lossy().ends_with(&suffix) {
                            return Some(entry.path());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Read a bounded tail of a rollout log and return the headroom from its last `token_count`
/// event's `rate_limits`. Bounded so a large rollout cannot cost unbounded work.
fn headroom_from_rollout(path: &Path) -> Headroom {
    // Read at most the last few MiB: the token_count events refresh every turn, so the last one
    // is near the end.
    const TAIL_BYTES: u64 = 4 * 1024 * 1024;
    let text = match read_tail(path, TAIL_BYTES) {
        Some(t) => t,
        None => return Headroom::Unknown,
    };
    let mut latest: Option<Headroom> = None;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') || !line.contains("rate_limits") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = value.get("payload").unwrap_or(&value);
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        if let Some(rl) = payload.get("rate_limits") {
            latest = Some(rate_limits_to_headroom(rl));
        }
    }
    latest.unwrap_or(Headroom::Unknown)
}

/// Read up to `max` bytes from the end of a file as UTF-8 (lossy), or `None` if it cannot be
/// opened. A tail read may start mid-line; the caller only keeps lines that parse as JSON.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Convert a Codex `rate_limits` object to `Headroom`, tied to the *limiting* (highest-used,
/// lowest-remaining) window and that window's own reset time. `Unknown` if no window carries a
/// `used_percent`.
fn rate_limits_to_headroom(rl: &Value) -> Headroom {
    let mut limiting: Option<(f64, Option<u64>)> = None;
    for key in ["primary", "secondary"] {
        let Some(win) = rl.get(key) else { continue };
        let Some(used) = win.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        // Fail open on a nonsensical value rather than closed: a non-finite or out-of-range
        // `used_percent` is skipped, so a garbled field cannot clamp to "0% remaining" and gate
        // an entry that is actually fine (round-1-impl finding f6).
        if !used.is_finite() || !(0.0..=100.0).contains(&used) {
            continue;
        }
        let resets_at = win.get("resets_at").and_then(Value::as_u64);
        // Keep the window with the highest used_percent (least remaining).
        if limiting.map(|(u, _)| used > u).unwrap_or(true) {
            limiting = Some((used, resets_at));
        }
    }
    match limiting {
        Some((used, resets_at)) => Headroom::Fraction {
            remaining_pct: (100.0 - used).clamp(0.0, 100.0),
            resets_at,
        },
        None => Headroom::Unknown,
    }
}

#[derive(Default, Debug, PartialEq)]
struct Events {
    thread_id: Option<String>,
    last_message: Option<String>,
    errors: Vec<String>,
    usage: Usage,
    /// How many `turn.completed` events carried usage. Reported as the model-call count
    /// so the figure means the same thing as Claude's `num_turns`: model calls billed
    /// inside this one review turn.
    turns_seen: u64,
    /// Set when a reading counted more cached input than total input -- a violation of the
    /// subset relationship Codex documents. Surfaced as a warning so the unreported fresh
    /// input is explained rather than read as a silent gap.
    input_inconsistent: bool,
    evidence_calls: u32,
    /// Evidence calls that completed **without** a transport-level error — the positive proof that
    /// the service was answering this turn. `evidence_calls` counts attempts, which cannot
    /// distinguish "the service served us" from "every call died", so a demotion resting on the
    /// attempt count would be resting on nothing (round-1 finding f3).
    evidence_calls_succeeded: u32,
    evidence_infrastructure_errors: Vec<String>,
}

/// Read `codex exec --json` output. The stream is JSONL, but the CLI also emits
/// plain-text notices (for example "Reading additional input from stdin..."), so
/// anything that is not JSON is skipped rather than treated as a failure.
fn parse_events(stdout: &str) -> Events {
    let mut events = Events::default();

    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");

        match kind {
            "thread.started" => {
                if let Some(id) = value.get("thread_id").and_then(Value::as_str) {
                    events.thread_id = Some(id.to_string());
                }
            }
            "item.completed" => {
                let item = value.get("item");
                let is_message = item
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    .map(|t| t == "agent_message")
                    .unwrap_or(false);
                if is_message {
                    if let Some(text) = item.and_then(|i| i.get("text")).and_then(Value::as_str) {
                        // Keep the last one: that is the reviewer's conclusion.
                        events.last_message = Some(text.trim().to_string());
                    }
                }
                let is_evidence = item.and_then(|i| i.get("type")).and_then(Value::as_str)
                    == Some("mcp_tool_call")
                    && item.and_then(|i| i.get("server")).and_then(Value::as_str)
                        == Some(crate::evidence::SERVER_NAME);
                if is_evidence {
                    events.evidence_calls = events.evidence_calls.saturating_add(1);
                    if let Some(error) = item.and_then(|i| i.get("error")).filter(|e| !e.is_null())
                    {
                        events.evidence_infrastructure_errors.push(match error {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        });
                    } else {
                        events.evidence_calls_succeeded =
                            events.evidence_calls_succeeded.saturating_add(1);
                    }
                }
            }
            // Last-wins, and these are the *thread's* running totals rather than this
            // turn's. Verified against `codex exec --json`: two trivial turns on one
            // thread reported output_tokens 5 then 10, where the second turn's reply was
            // a single word. The values match `total_token_usage` in Codex's own rollout
            // log, whose per-turn `last_token_usage` this stream never emits -- so the
            // per-turn figure has to be a delta, taken by the caller. See
            // `Parsed::usage_is_cumulative`.
            //
            // An earlier version summed these, with a comment arguing that keeping only
            // the last would under-report the run. The opposite is true: the last value
            // *is* the run, and adding them multiplies it.
            "turn.completed" => {
                if let Some(usage) = value.get("usage") {
                    let field =
                        |name: &str| -> Option<u64> { usage.get(name).and_then(Value::as_u64) };
                    events.turns_seen += 1;

                    // Converted to the convention `Usage` documents, which is Anthropic's:
                    // `input_tokens` there is the *uncached remainder*, with cache reads
                    // counted beside it, so the three input figures sum to the prompt.
                    // Codex follows OpenAI's opposite convention -- `cached_input_tokens`
                    // is a subset of `input_tokens`, verified as 9,984 of 13,133 on a
                    // fresh thread -- so passing it through unchanged made
                    // `billable_input()` count the cached portion twice.
                    let total_in = field("input_tokens");
                    let cached = field("cached_input_tokens");
                    events.usage.cache_read_tokens = cached;
                    // Both derived last-wins, together, from this event: usage is the
                    // thread's running total and only the latest one is kept, so the
                    // inconsistency flag must track the same event -- accumulating it would
                    // leave a stale warning on a stream whose final reading is valid.
                    let (fresh, inconsistent) = match (total_in, cached) {
                        // Codex documents cached as a subset of the input total, so this
                        // subtraction should never underflow. If it ever does, the fresh
                        // remainder is *unknowable*, not zero: `checked_sub` leaves it
                        // unreported rather than asserting a measured `Some(0)` beside
                        // Claude's real figures -- the same rule the cumulative delta keeps.
                        (Some(total), Some(cached)) => match total.checked_sub(cached) {
                            Some(fresh) => (Some(fresh), false),
                            None => (None, true),
                        },
                        (total, None) => (total, false),
                        (None, _) => (None, false),
                    };
                    events.usage.input_tokens = fresh;
                    events.input_inconsistent = inconsistent;
                    events.usage.output_tokens = field("output_tokens");
                    // `cache_creation_tokens` is deliberately never set. Codex reports
                    // cached input but does not distinguish writes from reads, and this
                    // figure sits directly beside Claude's measured one -- so it stays
                    // unreported rather than becoming an asserted zero.
                }
            }
            "error" | "turn.failed" | "thread.error" => {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        value
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| line.to_string());
                events.errors.push(message);
            }
            _ => {}
        }
    }

    events
}

const MCP_INIT_FAILURE_MARKER: &str = "required mcp servers failed to initialize";

/// Whether a message is the **one abandonment shape this project has actually observed**.
///
/// Deliberately a conjunction over the observed text rather than an inference from it: Codex reports
/// a call it gave up on as a *failed tool call* carrying its own timeout phrase, and both halves must
/// be present. The earlier single-substring test, and the startup-word inference that grew up beside
/// it to compensate, produced five findings in this one mechanism across five review rounds (f3, f6,
/// f9, f10, f12) — every one of them a hole in guessing a service's state from prose. So the
/// guessing is gone: this matches what was seen, and everything else about the evidence server is
/// `Unusable` and fails closed.
///
/// The cost is stated rather than hidden. If a future Codex version phrases an abandon differently,
/// this stops matching and such a turn fails as `EVIDENCE_UNAVAILABLE` — a worse message for the
/// caller, but the safe direction, and a visible one that leads back here.
fn is_evidence_call_abandoned(evidence: &str) -> bool {
    let lower = evidence.to_ascii_lowercase();
    lower.contains("timed out awaiting tools/call") && lower.contains("tool call failed")
}

/// Every evidence signal in a turn, paired with the message it came from, collected from **all** the
/// sources that can carry one, each at its correct scoping.
///
/// This exists because the same defect appeared four times in four places — a rule applied to one
/// branch (f6), one half of a judgement (f9), or one of the sources (f10 on the failed path, f15 on
/// the completed one). Each fix was correct and each left the next instance standing. One collector
/// both branches call is what stops a fifth: a source added here is seen by everything.
///
/// - **Item errors** are scoped: `parse_events` records them only from an `mcp_tool_call` whose
///   `server` field is this evidence server, so their text need not name it.
/// - **Stream errors** are unscoped and must name the server themselves, or they are `Unrelated` —
///   an ordinary `"stream disconnected before completion"` says nothing about the evidence service.
/// - **stderr** participates only when `include_stderr` is set, which is the failed-turn path. On a
///   *completed* turn stderr is ordinary CLI logging that may mention the server's name in passing,
///   and reading a benign mention as "unusable" would fail every healthy review. That asymmetry is
///   deliberate rather than another missed source: on a failed turn the text is being read to
///   explain a failure that already happened, which is what `classify` reads it for too.
fn collect_evidence_signals(
    events: &Events,
    stderr: &str,
    include_stderr: bool,
) -> Vec<(EvidenceSignal, String)> {
    let stderr = stderr.trim();
    events
        .evidence_infrastructure_errors
        .iter()
        .map(|m| (evidence_message_signal(m, true), m.clone()))
        .chain(
            events
                .errors
                .iter()
                .map(|m| (evidence_message_signal(m, false), m.clone())),
        )
        .chain(
            Some(stderr)
                .filter(|m| include_stderr && !m.is_empty())
                .map(|m| (evidence_message_signal(m, false), m.to_string())),
        )
        .collect()
}

/// What one evidence error message is evidence *of*. One classifier, one precedence order, used by
/// both `parse` branches — the alternative is what round-2 finding f6 caught: two places deciding
/// the same question, one of them missing the decisive marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvidenceSignal {
    /// The service could not start, by its own explicit marker. Outranks everything.
    StartupFailure,
    /// The observed abandonment shape, with the service otherwise answering.
    AbandonedCall,
    /// About this server, but not a shape we recognise. Fail closed.
    Unusable,
    /// Not about this server at all. Only reachable for an unscoped message; carries no claim
    /// either way, so it neither condemns the evidence service nor excuses it.
    Unrelated,
}

/// Classify one message. `server_scoped` says whether the caller already knows the message belongs
/// to this evidence server: item errors do (the parser filters on the item's `server` field), while
/// the failed-turn path's stream errors do not and must name the server themselves.
///
/// The scoping applies to **both** halves of the judgement (round-3 finding f9), and every branch
/// that is not an explicitly recognised shape ends at `Unusable`, which is fatal. There is no
/// inferred middle any more.
fn evidence_message_signal(message: &str, server_scoped: bool) -> EvidenceSignal {
    let lower = message.to_ascii_lowercase();
    // The server's own explicit marker is decisive at either scoping.
    if lower.contains(MCP_INIT_FAILURE_MARKER) {
        return EvidenceSignal::StartupFailure;
    }
    // Is this message about our server at all? A scoped one is by construction; an unscoped one has
    // to say so. Without this an unrelated stream error ("stream disconnected before completion")
    // would read as an unusable evidence service.
    let ours = server_scoped || lower.contains(&crate::evidence::SERVER_NAME.to_ascii_lowercase());
    if !ours {
        return EvidenceSignal::Unrelated;
    }
    if is_evidence_call_abandoned(message) {
        EvidenceSignal::AbandonedCall
    } else {
        EvidenceSignal::Unusable
    }
}

const POLICY_DENIAL_MARKER: &str = "rejected: blocked by policy";

/// Count shell commands the Codex router refused before execution. This is separate from
/// the JSON event parser because the router writes these diagnostics to stderr, not to the
/// `--json` stream. The count is used to make a timeout self-diagnosing rather than looking
/// like an unexplained stalled model.
pub(crate) fn policy_denial_count(stderr: &str) -> usize {
    stderr.lines().filter(|line| is_policy_denial(line)).count()
}

pub(crate) fn is_policy_denial(line: &str) -> bool {
    line.to_ascii_lowercase().contains(POLICY_DENIAL_MARKER)
}

/// Render the refused command, keeping the useful part of Codex's router diagnostic while
/// avoiding a repeated timestamp/prefix and bounding one denial's contribution to the MCP
/// response. The full stderr remains in a timeout failure's diagnostic detail.
fn collect_denials(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| is_policy_denial(line))
        .take(100)
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            let command = lower.find("error=`").and_then(|start| {
                let start = start + "error=`".len();
                lower[start..]
                    .find("` rejected: blocked by policy")
                    .and_then(|end| line.get(start..start + end))
            });
            let command = command.unwrap_or_else(|| line.trim());
            let mut command = command.trim().to_string();
            if command.chars().count() > 1000 {
                command = command.chars().take(1000).collect::<String>() + "...";
            }
            command
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_STREAM: &str = r###"Reading additional input from stdin...
{"type":"thread.started","thread_id":"019faa01-a2d3-78c0-a67a-2ffe1ca75969"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"thinking"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"## Verdict\nREQUEST CHANGES"}}
{"type":"turn.completed","usage":{"input_tokens":14124,"output_tokens":5}}"###;

    #[test]
    fn evidence_config_strings_escape_windows_paths_and_unicode_as_toml() {
        let value = "C:\\path with space\\quote\"\\雪\\bundle.json";
        assert_eq!(toml_string(value), serde_json::to_string(value).unwrap());
    }

    #[test]
    fn an_unrepresentable_evidence_path_fails_instead_of_becoming_lossy() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let path = std::path::PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
        ]));
        assert!(toml_path(&path).is_err());
    }

    #[test]
    fn usage_is_read_from_the_event_stream() {
        let events = parse_events(REAL_STREAM);
        assert_eq!(events.usage.input_tokens, Some(14_124));
        assert_eq!(events.usage.output_tokens, Some(5));
        assert_eq!(events.turns_seen, 1);
    }

    #[test]
    fn rate_limits_uses_the_limiting_window_and_its_own_reset() {
        // Real shape from a rollout token_count event: single primary window.
        let rl: Value = serde_json::from_str(
            r#"{"primary":{"used_percent":83.0,"window_minutes":10080,"resets_at":1786826027},"secondary":null}"#,
        )
        .unwrap();
        match rate_limits_to_headroom(&rl) {
            Headroom::Fraction {
                remaining_pct,
                resets_at,
            } => {
                assert!((remaining_pct - 17.0).abs() < 1e-9);
                assert_eq!(resets_at, Some(1786826027));
            }
            other => panic!("expected Fraction, got {other:?}"),
        }

        // Two windows: the limiting one is the *higher* used_percent (less remaining), and its
        // own reset is kept -- not the nearer reset of the other window (round-1 finding f3).
        let two: Value = serde_json::from_str(
            r#"{"primary":{"used_percent":40.0,"resets_at":100},"secondary":{"used_percent":90.0,"resets_at":999}}"#,
        )
        .unwrap();
        match rate_limits_to_headroom(&two) {
            Headroom::Fraction {
                remaining_pct,
                resets_at,
            } => {
                assert!((remaining_pct - 10.0).abs() < 1e-9);
                assert_eq!(
                    resets_at,
                    Some(999),
                    "must keep the limiting window's reset"
                );
            }
            other => panic!("expected Fraction, got {other:?}"),
        }
    }

    #[test]
    fn rate_limits_without_a_window_is_unknown() {
        let rl: Value = serde_json::from_str(r#"{"primary":null,"secondary":null}"#).unwrap();
        assert_eq!(rate_limits_to_headroom(&rl), Headroom::Unknown);
    }

    #[test]
    fn codex_account_id_reads_the_identifier_only() {
        let dir = std::env::temp_dir().join(format!("cr-codex-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"account_id":"acct-123","access_token":"SECRET"},"last_refresh":"x"}"#,
        )
        .unwrap();
        assert_eq!(codex_account_id(&dir), Some("acct-123".to_string()));
        assert_eq!(codex_account_id(&dir.join("nope")), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_resolve_identity_reads_method_and_account() {
        use crate::reviewer::AuthMethod;
        let dir = std::env::temp_dir().join(format!("cr-codex-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Subscription (chatgpt) with an account id.
        std::fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"account_id":"acct-9"}}"#,
        )
        .unwrap();
        let id = codex_resolve_identity(&dir).unwrap();
        assert_eq!(id.account, "acct-9");
        assert_eq!(id.method, AuthMethod::Subscription);
        // An API-key mode resolves to Other (refused upstream).
        std::fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"apikey","tokens":{"account_id":"acct-9"}}"#,
        )
        .unwrap();
        assert_eq!(
            codex_resolve_identity(&dir).unwrap().method,
            AuthMethod::Other
        );
        // A missing account id fails closed.
        std::fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{}}"#,
        )
        .unwrap();
        assert_eq!(
            codex_resolve_identity(&dir).unwrap_err().code,
            "PROFILE_IDENTITY_MISMATCH"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_home_identity_is_the_per_spawn_probe_and_asserts_against_the_authorized_account() {
        use crate::reviewer::Reviewer;
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("cr-codex-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"account_id":"acct-1"}}"#,
        )
        .unwrap();
        let cfg = cfg();
        let cancel = AtomicBool::new(false);
        // The per-spawn probe (trait method) reads the home and, combined with the assertion, passes
        // for the authorized account and refuses a different one — this is the worker's per-spawn path.
        let resolved = CodexReviewer
            .resolve_home_identity(Path::new("codex.exe"), &cfg, &dir, &cancel)
            .expect("resolve");
        assert!(crate::reviewer::assert_profile_identity("codex", &resolved, "acct-1").is_ok());
        assert_eq!(
            crate::reviewer::assert_profile_identity("codex", &resolved, "acct-2")
                .unwrap_err()
                .code,
            "PROFILE_IDENTITY_MISMATCH"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_home_reads_ambient_for_ambient_and_refuses_a_denied_profile() {
        use crate::reviewer::Reviewer;
        // Isolate the allowlist store so the denied-profile assertions hold regardless of what the
        // developer machine has genuinely authorized (issue #99).
        let _base = crate::profile::isolate_profile_base();
        // Ambient entry: reads the ambient home ($CODEX_HOME or ~/.codex).
        let ambient = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--level".into(),
            "standard:gpt-5.6-luna:max".into(),
        ])
        .expect("config");
        assert!(codex_home(&ambient, ambient.primary()).is_some());
        // `effective_read_home` (issue #81) is exactly `codex_home`, preserving the ambient fallback.
        assert_eq!(
            CodexReviewer.effective_read_home(&ambient, ambient.primary()),
            codex_home(&ambient, ambient.primary())
        );

        // A named profile is deny-all in Phase 1, so the account read fails open to None rather
        // than falling back to the ambient home (which would read the wrong account).
        let profiled = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--level".into(),
            "standard:gpt-5.6-luna:max".into(),
            "--codex-profile".into(),
            "work".into(),
        ])
        .expect("config");
        assert!(codex_home(&profiled, profiled.primary()).is_none());
        // The unauthorized-profile contract is preserved through the seam and its composition:
        // effective_read_home is None, so the default account_fingerprint (fingerprint_at over it)
        // is None too -- an unauthorized entry is never keyed and so never gated (issue #81, f10).
        assert!(CodexReviewer
            .effective_read_home(&profiled, profiled.primary())
            .is_none());
        assert!(CodexReviewer
            .account_fingerprint(&profiled, profiled.primary())
            .is_none());
    }

    #[test]
    fn headroom_from_rollout_takes_the_last_token_count() {
        let dir = std::env::temp_dir().join(format!("cr-codex-roll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-08-10T10-00-00-abc.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":{"primary":{"used_percent":10.0,"resets_at":1}}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":{"primary":{"used_percent":95.0,"resets_at":2}}}}"#,
                "\n",
            ),
        )
        .unwrap();
        match headroom_from_rollout(&path) {
            Headroom::Fraction { remaining_pct, .. } => {
                assert!(
                    (remaining_pct - 5.0).abs() < 1e-9,
                    "must use the last event"
                );
            }
            other => panic!("expected Fraction, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cumulative_usage_is_taken_last_wins_and_converted_to_our_convention() {
        // Real values, captured from `codex exec --json`: two one-word turns on a single
        // thread. The second reply was the word "two", yet reports output_tokens 10 --
        // because these are the *thread's* running totals, not the turn's.
        //
        // An earlier version summed them, arguing that keeping only the last would
        // under-report the run. That is backwards, and it is how a thread total came to
        // be recorded as one turn's cost.
        let stream = concat!(
            r#"{"type":"turn.completed","usage":{"input_tokens":13133,"cached_input_tokens":9984,"output_tokens":5,"reasoning_output_tokens":0}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":26285,"cached_input_tokens":22016,"output_tokens":10,"reasoning_output_tokens":0}}"#,
        );
        let events = parse_events(stream);

        // Last wins, not the sum: 26,285 rather than 39,418.
        assert_eq!(events.usage.output_tokens, Some(10));
        // And converted out of OpenAI's convention, where `cached_input_tokens` is a
        // subset of `input_tokens`, into the one `Usage` documents, where the fields are
        // disjoint and sum to the prompt. Passing it through unchanged made
        // `billable_input()` count the cached portion twice.
        assert_eq!(events.usage.cache_read_tokens, Some(22_016));
        assert_eq!(events.usage.input_tokens, Some(26_285 - 22_016));
        assert_eq!(
            events.usage.billable_input(),
            26_285,
            "the converted fields must still sum to what Codex reported"
        );
        assert_eq!(events.turns_seen, 2);
        // Codex does not distinguish cache writes from reads, so that field stays
        // unreported rather than being guessed at -- a zero here would be an assertion,
        // and it sits directly beside Claude's measured figure.
        assert_eq!(events.usage.cache_creation_tokens, None);
    }

    #[test]
    fn a_cached_count_above_the_total_leaves_fresh_input_unreported() {
        // Codex documents cached input as a subset of the total, so cached > total should be
        // impossible. If it ever happens, the fresh remainder is unknowable, not zero: it is
        // left unreported (and flagged) rather than clamped to a measured-looking Some(0).
        let stream = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":150,"output_tokens":5}}"#;
        let events = parse_events(stream);
        assert_eq!(events.usage.cache_read_tokens, Some(150));
        assert_eq!(
            events.usage.input_tokens, None,
            "an impossible subset is unknowable, not a measured zero"
        );
        assert!(
            events.input_inconsistent,
            "the inconsistency is flagged so it can be surfaced as a warning"
        );
    }

    #[test]
    fn a_later_valid_reading_clears_an_earlier_inconsistency() {
        // Usage is last-wins, so the inconsistency flag must be too: an early bad event
        // followed by a valid one leaves valid final numbers, and warning about omitted
        // input then would be stale.
        let stream = concat!(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":150,"output_tokens":5}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":26285,"cached_input_tokens":22016,"output_tokens":10}}"#,
        );
        let events = parse_events(stream);
        assert_eq!(events.usage.input_tokens, Some(26_285 - 22_016));
        assert!(
            !events.input_inconsistent,
            "the final reading is valid, so no stale warning"
        );
    }

    #[test]
    fn a_codex_turn_declares_its_usage_cumulative_and_a_claude_turn_does_not() {
        // The two CLIs differ and the difference is invisible in the numbers, so the
        // adapter states it rather than leaving the caller to infer -- inferring it is
        // what produced eight rounds of inflated figures.
        let stream = r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#;
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .expect("parse");
        assert!(parsed.usage_is_cumulative);
    }

    #[test]
    fn a_stream_with_no_usage_reports_none_rather_than_zero_calls() {
        // `api_calls: Some(0)` would read as "this turn made no model calls", which is
        // a claim; `None` is the honest "the CLI did not say".
        let stream = r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#;
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .expect("parse");
        assert!(parsed.usage.is_empty());
        assert_eq!(parsed.usage.api_calls, None);
    }

    fn cfg() -> Config {
        Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--level".into(),
            "standard:gpt-5.6-luna:max".into(),
        ])
        .expect("config")
    }

    fn outcome(stdout: &str, success: bool) -> RunOutcome {
        RunOutcome {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit: if success { Some(0) } else { Some(1) },
            success,
            timed_out: false,
            cancelled: false,
            policy_stalled: false,
            policy_denials: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
            stdout_cap_hit: None,
        }
    }

    #[test]
    fn extracts_thread_id_and_final_message_from_real_stream() {
        let events = parse_events(REAL_STREAM);
        assert_eq!(
            events.thread_id.as_deref(),
            Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969")
        );
        assert_eq!(
            events.last_message.as_deref(),
            Some("## Verdict\nREQUEST CHANGES")
        );
        assert!(events.errors.is_empty());
    }

    #[test]
    fn ignores_non_json_notices() {
        let events = parse_events("Reading additional input from stdin...\nnot json at all\n");
        assert_eq!(events, Events::default());
    }

    #[test]
    fn required_evidence_startup_failure_has_its_own_contract() {
        let mut failed = outcome(
            r#"{"type":"turn.failed","error":{"message":"required MCP servers failed to initialize: cross_review_evidence"}}"#,
            false,
        );
        failed.stderr = "required MCP servers failed to initialize: cross_review_evidence".into();
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &failed, None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");
    }

    #[test]
    fn evidence_transport_error_invalidates_an_otherwise_completed_review() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":null,"error":"connection closed","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");
    }

    #[test]
    fn model_argument_error_is_not_misread_as_service_death() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{},"result":{"content":[{"type":"text","text":"invalid_arguments"}],"is_error":true},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"REQUEST CHANGES"}}"#,
        );
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .expect("ordinary tool error does not invalidate transport");
        assert_eq!(parsed.text, "REQUEST CHANGES");
    }

    // A bounded evidence read that timed out returns an in-band `read_timeout` tool error
    // (top-level error:null, status:completed) — the shape the #61 watchdog produces before Codex's
    // 30s abandon. Like any ordinary tool error it must NOT invalidate an otherwise complete review
    // (findings f6/f11); only a top-level transport error does (the test above).
    #[test]
    fn an_in_band_read_timeout_does_not_invalidate_the_review() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":{"content":[{"type":"text","text":"read_timeout: reading 'x' exceeded the evidence read budget"}],"is_error":true},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .expect("a timely in-band read_timeout does not invalidate the review");
        assert_eq!(parsed.text, "APPROVE");
    }

    // #71: the failure this repository lost two plan reviews to. Codex abandons one `tools/call` on
    // its own 30s ceiling and reports it as a tool call that "failed" for `cross_review_evidence`,
    // which the startup predicate matched on substrings alone — so a review nineteen minutes in was
    // reported as a service that could not start and a review that "was not started".
    #[test]
    fn an_abandoned_evidence_call_is_not_reported_as_a_startup_failure() {
        let abandoned = "tool call error: tool call failed for `cross_review_evidence/repository_read`\n\nCaused by:\n    timed out awaiting tools/call after 30s";
        // The observed shape, at either scoping: a failed tool call carrying the timeout phrase.
        assert!(is_evidence_call_abandoned(abandoned));
        for scoped in [true, false] {
            assert_eq!(
                evidence_message_signal(abandoned, scoped),
                EvidenceSignal::AbandonedCall
            );
            // The explicit startup marker outranks it, even in a message carrying both phrases.
            assert_eq!(
                evidence_message_signal(
                    "required MCP servers failed to initialize: cross_review_evidence",
                    scoped
                ),
                EvidenceSignal::StartupFailure
            );
            assert_eq!(
                evidence_message_signal(
                    "required MCP servers failed to initialize: cross_review_evidence (timed out awaiting tools/call)",
                    scoped
                ),
                EvidenceSignal::StartupFailure
            );
        }
        // A dead transport is not an abandoned call, and is no longer inferred about: anything
        // about this server that is not a recognised shape fails closed (round-5 f12).
        assert!(!is_evidence_call_abandoned(
            "cross_review_evidence: connection closed"
        ));
        assert_eq!(
            evidence_message_signal("cross_review_evidence: connection closed", false),
            EvidenceSignal::Unusable
        );
    }

    // A failed turn whose cause was an abandoned call gets its own code and its own remediation:
    // "retry", not "check your executable and state directory", which are not the problem. It takes
    // the same proof of availability the completed path takes (round-4 f11) — the real incident this
    // models had many successful evidence calls before the abandon.
    #[test]
    fn an_abandoned_call_that_kills_the_turn_has_its_own_failure_code() {
        let abandon = r#"{"type":"error","message":"tool call error: tool call failed for `cross_review_evidence/repository_read`\n\nCaused by:\n    timed out awaiting tools/call after 30s"}"#;
        let served = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_scope","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#;
        let mut failed = outcome(&format!("{served}\n{abandon}"), false);
        failed.stderr = String::new();
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &failed, None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_CALL_ABANDONED");
        assert!(error.summary.to_lowercase().contains("gave up"));
        // The old text claimed the review never started, which was false for a 19-minute review.
        assert!(!error.summary.contains("was not started"));

        // Round-4 f11: with nothing successful beside it, there is no evidence the service was ever
        // answering, so the code that says it was must not be used.
        let mut alone = outcome(abandon, false);
        alone.stderr = String::new();
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &alone, None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");
    }

    // Round-4 f10: a timeout must not shield a genuinely dead service sitting beside it. The graver
    // signal in the turn decides, not whichever one the branch happened to test for first.
    #[test]
    fn a_timeout_beside_a_dead_service_reports_the_dead_service() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_scope","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"error","message":"tool call error: tool call failed for `cross_review_evidence/repository_read`\n\nCaused by:\n    timed out awaiting tools/call after 30s"}"#,
            "\n",
            r#"{"type":"error","message":"cross_review_evidence: connection closed"}"#,
        );
        let mut failed = outcome(stream, false);
        failed.stderr = String::new();
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &failed, None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");

        // An unrelated stream error is not evidence about our service either way, so it neither
        // condemns it nor is mistaken for it.
        assert_eq!(
            evidence_message_signal("stream disconnected before completion", false),
            EvidenceSignal::Unrelated
        );
    }

    // Round-7 f15: the mirror of f10 on the completed path. `RunOutcome::success` is the process
    // status, not the absence of error events, so a turn can exit zero with a server-named failure
    // in a *stream* error — which that path did not read at all, and would have returned a verdict
    // over unusable evidence.
    #[test]
    fn a_completed_turn_fails_on_a_stream_level_service_error() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_scope","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"error","message":"cross_review_evidence: connection closed"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");

        // An unrelated stream error on a healthy turn is still not a failure, and — the reason
        // stderr is excluded from this path — a *successful* run whose stderr merely mentions the
        // server by name must still return its review.
        let healthy = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_scope","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"error","message":"stream disconnected before completion"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let mut ok = outcome(healthy, true);
        ok.stderr = "starting MCP server cross_review_evidence".into();
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &ok, None)
            .expect("a benign mention must not fail a healthy review");
        assert_eq!(parsed.text, "APPROVE");
    }

    // Round-6, f10 held open: the same masking, one source further along. A dead service reported on
    // an evidence *item* rather than as a stream error must dominate a stream-level abandon just the
    // same — and the round-4 test could not catch it, because it put the fatal error in a stream
    // event. Note the item error need not name the server: the parser already scoped it.
    #[test]
    fn an_item_level_service_error_still_dominates_a_stream_level_abandon() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_scope","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_list","arguments":{},"result":null,"error":"connection closed","status":"failed"}}"#,
            "\n",
            r#"{"type":"error","message":"tool call error: tool call failed for `cross_review_evidence/repository_read`\n\nCaused by:\n    timed out awaiting tools/call after 30s"}"#,
        );
        let mut failed = outcome(stream, false);
        failed.stderr = String::new();
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &failed, None)
            .unwrap_err();
        assert_eq!(
            error.code, "EVIDENCE_UNAVAILABLE",
            "an item-level dead service must not be masked by a stream-level abandon"
        );
    }

    // A review that completed despite an abandoned call is returned, with the shortfall named —
    // the same treatment a truncated capture gets. Losing a finished review to one late read is
    // what made this failure expensive rather than merely annoying.
    #[test]
    fn an_abandoned_call_warns_but_does_not_discard_a_completed_review() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":null,"error":"tool call failed: timed out awaiting tools/call after 30s","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_list","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"REQUEST CHANGES"}}"#,
        );
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .expect("an abandoned call does not discard a completed review");
        assert_eq!(parsed.text, "REQUEST CHANGES");
        let warning = parsed
            .warnings
            .iter()
            .find(|w| w.contains("abandoned"))
            .expect("the shortfall is named");
        assert!(warning.contains("may rest on less"));
    }

    // Round-1 f3, first half: the demotion must rest on proof that the service was answering, not
    // on the wording of the error. With no successful evidence call in the turn there is nothing
    // showing the service was ever usable, so the fail-closed reading stands.
    #[test]
    fn an_abandoned_call_with_no_successful_call_still_invalidates_the_review() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":null,"error":"tool call failed: timed out awaiting tools/call after 30s","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");
    }

    // Round-1 f3, second half: on the failed-turn path the text being classified is a blob of
    // stderr plus every stream error event, so a timeout phrase that belongs to some *other* MCP
    // server must not be read as ours. It also must not suppress the startup classification.
    #[test]
    fn another_servers_call_timeout_is_not_read_as_ours() {
        let foreign = "tool call error: tool call failed for `some_other_server/do_thing`\n\nCaused by:\n    timed out awaiting tools/call after 30s";
        // It is the abandonment *shape*, but not about our server, so unscoped it carries no claim
        // about the evidence service at all.
        assert!(is_evidence_call_abandoned(foreign));
        assert_eq!(
            evidence_message_signal(foreign, false),
            EvidenceSignal::Unrelated,
            "a timeout in another server's call is not evidence about ours"
        );

        let mut failed = outcome(
            &format!(
                r#"{{"type":"error","message":"{}"}}"#,
                foreign.replace('\n', "\\n")
            ),
            false,
        );
        failed.stderr = "cross_review_evidence failed to initialize".into();
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &failed, None)
            .unwrap_err();
        assert_eq!(
            error.code, "EVIDENCE_UNAVAILABLE",
            "a foreign timeout must not shield a real evidence startup failure"
        );
    }

    // Round-2 f6: the decisive startup marker must outrank the abandon demotion on the *completed*
    // path too, not only on the failed-turn one. A successful call elsewhere in the turn must not
    // buy a pass for a message that says the server failed to initialize.
    #[test]
    fn a_completed_turn_still_fails_on_a_startup_marker_beside_a_successful_call() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_list","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":null,"error":"required MCP servers failed to initialize: cross_review_evidence (timed out awaiting tools/call)","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");
        // And the classifier itself ranks them, on both scopings.
        let both = "required MCP servers failed to initialize: cross_review_evidence (timed out awaiting tools/call)";
        assert_eq!(
            evidence_message_signal(both, true),
            EvidenceSignal::StartupFailure
        );
        assert_eq!(
            evidence_message_signal(both, false),
            EvidenceSignal::StartupFailure
        );
    }

    // Round-3 f9: an item error is scoped to this server by `item.server`, so it need not repeat
    // the server name in its own text. The startup classification must honour that scoping too —
    // otherwise a message that plainly describes a service that never came up is demoted to an
    // abandoned call because it did not name the server it was already known to be about.
    #[test]
    fn a_server_scoped_startup_failure_need_not_name_the_server() {
        let no_name = "failed to initialize: timed out awaiting tools/call";
        // Round-5 f12 deleted the startup-word inference, so this is no longer *labelled* a startup
        // failure — it does not carry the explicit marker, and it is not the observed abandonment
        // shape either. It lands in `Unusable`, which is what f9 actually required: it must not be
        // demoted to a warning. The classification is now weaker and the outcome identical, which is
        // the trade f12 asked for.
        assert_eq!(
            evidence_message_signal(no_name, true),
            EvidenceSignal::Unusable,
            "an item error is already scoped to this server and must fail closed"
        );
        // Unscoped, the same text could be about any server: it is not our startup failure, not our
        // abandoned call, and not evidence that our service is unusable either.
        assert_eq!(
            evidence_message_signal(no_name, false),
            EvidenceSignal::Unrelated
        );

        // End to end: a successful call beside it must not buy it a demotion to a warning.
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_list","arguments":{},"result":{"content":[]},"error":null,"status":"completed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":null,"error":"failed to initialize: timed out awaiting tools/call","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");

        // The ordinary abandoned call still reads as one: "failed" alone is too weak to outrank it.
        assert_eq!(
            evidence_message_signal(
                "tool call failed: timed out awaiting tools/call after 30s",
                true
            ),
            EvidenceSignal::AbandonedCall
        );
    }

    // But a service that is actually unusable still invalidates the review, exactly as before: the
    // widening above is one narrowly-matched case, not a general softening.
    #[test]
    fn a_mixed_stream_still_fails_on_the_unusable_service_error() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_read","arguments":{"path":"x"},"result":null,"error":"tool call failed: timed out awaiting tools/call after 30s","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"cross_review_evidence","tool":"repository_list","arguments":{},"result":null,"error":"connection closed","status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"APPROVE"}}"#,
        );
        let error = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(stream, true), None)
            .unwrap_err();
        assert_eq!(error.code, "EVIDENCE_UNAVAILABLE");
        assert!(
            error
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("connection closed"),
            "the fatal cause is what is reported, not the abandoned call beside it"
        );
    }

    #[test]
    fn reasoning_items_are_not_mistaken_for_the_review() {
        let events = parse_events(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"internal"}}"#,
        );
        assert!(events.last_message.is_none());
    }

    #[test]
    fn last_agent_message_wins() {
        let events = parse_events(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"first\"}}\n\
             {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"second\"}}",
        );
        assert_eq!(events.last_message.as_deref(), Some("second"));
    }

    #[test]
    fn collects_error_events() {
        let events =
            parse_events(r#"{"type":"error","message":"stream disconnected before completion"}"#);
        assert_eq!(events.errors, vec!["stream disconnected before completion"]);
    }

    #[test]
    fn falls_back_to_event_stream_when_no_output_file() {
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &outcome(REAL_STREAM, true), None)
            .expect("parse");
        assert_eq!(parsed.text, "## Verdict\nREQUEST CHANGES");
        assert_eq!(
            parsed.session_id.as_deref(),
            Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969")
        );
    }

    #[test]
    fn output_file_takes_precedence_over_event_stream() {
        let dir = std::env::temp_dir().join("cross-review-tests");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("last-precedence.txt");
        std::fs::write(&path, "  authoritative verdict  ").expect("write");

        let parsed = CodexReviewer
            .parse(
                &cfg(),
                cfg().primary(),
                &outcome(REAL_STREAM, true),
                Some(&path),
            )
            .expect("parse");
        assert_eq!(parsed.text, "authoritative verdict");
        // The thread id still comes from the stream, which is its only source.
        assert_eq!(
            parsed.session_id.as_deref(),
            Some("019faa01-a2d3-78c0-a67a-2ffe1ca75969")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn successful_run_surfaces_commands_refused_by_the_cli_policy() {
        let mut out = outcome(REAL_STREAM, true);
        out.stderr = r###"2026-08-05T15:32:49Z ERROR codex_core::tools::router: error=`"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -Command "git grep -n CursorCert"` rejected: blocked by policy
ordinary diagnostic
"###
        .to_string();

        assert_eq!(policy_denial_count(&out.stderr), 1);
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .expect("parse");
        assert_eq!(parsed.denial_count, 1);
        assert_eq!(
            parsed.denials,
            vec![
                r###""C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -Command "git grep -n CursorCert""###
            ]
        );
    }

    #[test]
    fn policy_denial_examples_parse_markers_without_relying_on_case() {
        let mut out = outcome(REAL_STREAM, true);
        out.stderr = "router: ERROR=`git ls-files` REJECTED: BLOCKED BY POLICY".to_string();

        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .expect("parse");
        assert_eq!(parsed.denial_count, 1);
        assert_eq!(parsed.denials, vec!["git ls-files"]);
    }

    #[test]
    fn a_capped_stderr_marks_the_denial_count_as_a_lower_bound() {
        // The router writes refusals to stderr, so once stderr hits the collection cap the
        // later ones are gone and the retained count understates the truth. It must be
        // reported as a floor rather than as the exact total.
        let mut out = outcome(REAL_STREAM, true);
        out.stderr = "router: error=`git grep foo` rejected: blocked by policy".to_string();

        let intact = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .expect("parse");
        assert_eq!(intact.denial_count, 1);
        assert!(
            !intact.denial_count_is_floor,
            "an untruncated stderr is exact"
        );

        out.stderr_truncated = true;
        let capped = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .expect("parse");
        assert_eq!(capped.denial_count, 1);
        assert!(
            capped.denial_count_is_floor,
            "a capped stderr dropped later refusals, so the count is a floor"
        );
    }

    #[test]
    fn policy_denial_count_is_not_limited_by_the_example_cap() {
        let mut out = outcome(REAL_STREAM, true);
        out.stderr = (0..101)
            .map(|n| format!("router: error=`git grep {n}` rejected: blocked by policy"))
            .collect::<Vec<_>>()
            .join("\n");

        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .expect("parse");
        assert_eq!(parsed.denial_count, 101);
        assert_eq!(parsed.denials.len(), 100);
    }

    #[test]
    fn empty_output_file_falls_back_rather_than_reporting_an_empty_review() {
        let dir = std::env::temp_dir().join("cross-review-tests");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("last-empty.txt");
        std::fs::write(&path, "   \n").expect("write");

        let parsed = CodexReviewer
            .parse(
                &cfg(),
                cfg().primary(),
                &outcome(REAL_STREAM, true),
                Some(&path),
            )
            .expect("parse");
        assert_eq!(parsed.text, "## Verdict\nREQUEST CHANGES");
        std::fs::remove_file(&path).ok();
    }

    /// A failed run whose `agent_message` text contains a phrase the classifier matches.
    ///
    /// Same property as the Claude adapter: the JSONL stream carries the reviewer's own
    /// prose, so classifying on stdout would let the review pick the failure code.
    fn failure_with_agent_message(text: &str) -> RunOutcome {
        let event = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": text},
        });
        RunOutcome {
            stdout: format!(
                "{{\"type\":\"thread.started\",\"thread_id\":\"t1\"}}\n{}\n",
                event
            ),
            stderr: String::new(),
            exit: Some(1),
            success: false,
            timed_out: false,
            cancelled: false,
            policy_stalled: false,
            policy_denials: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_lossy: false,
            stdout_incomplete: false,
            stdout_cap_hit: None,
        }
    }

    #[test]
    fn an_agent_message_mentioning_429_is_not_reported_as_rate_limited() {
        let out = failure_with_agent_message(
            "`server.rs:429` should return 429 when the quota is exhausted.",
        );
        let err = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
        assert!(err.detail.unwrap_or_default().contains("429"));
    }

    #[test]
    fn an_agent_message_mentioning_a_missing_session_is_not_session_not_found() {
        let out = failure_with_agent_message(
            "The error path prints 'session not found' but the session exists.",
        );
        let err = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
    }

    #[test]
    fn a_stream_error_event_is_still_classified() {
        // Error events are the CLI's own, so they remain evidence.
        let mut out = failure_with_agent_message("ordinary review prose");
        out.stdout.push_str(
            "{\"type\":\"error\",\"message\":\"stream error: 429 rate limit exceeded\"}\n",
        );
        let err = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED");
    }

    #[test]
    fn rate_limit_on_failure_is_classified() {
        let mut out = outcome("", false);
        out.stderr = "Error: 429 Too Many Requests".into();
        let err = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED");
    }

    #[test]
    fn missing_resume_target_maps_to_session_not_found() {
        let mut out = outcome("", false);
        out.stderr = "Error: no session found with id abc".into();
        let err = CodexReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "SESSION_NOT_FOUND");
    }

    #[test]
    fn successful_run_with_no_message_anywhere_is_an_empty_review() {
        let err = CodexReviewer
            .parse(
                &cfg(),
                cfg().primary(),
                &outcome(r#"{"type":"turn.completed"}"#, true),
                None,
            )
            .unwrap_err();
        assert_eq!(err.code, "EMPTY_REVIEW");
    }

    #[test]
    fn a_truncated_event_stream_with_no_message_is_reported_as_truncation() {
        // Only once the final-message file has been tried and found wanting: that file is
        // written by the CLI and is unaffected by our pipe cap, so a truncated stream that
        // still yielded a review must not be a failure at all.
        let truncated = RunOutcome {
            stdout_truncated: true,
            ..outcome(r#"{"type":"turn.completed"}"#, true)
        };
        let err = CodexReviewer
            .parse(&cfg(), cfg().primary(), &truncated, None)
            .unwrap_err();
        assert_eq!(err.code, "OUTPUT_TRUNCATED");

        // A truncated stream whose review did survive in the file is a success.
        let dir = std::env::temp_dir().join("cross-review-codex-truncation-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join(format!("{}-last.txt", std::process::id()));
        std::fs::write(&file, "## Verdict\nAPPROVE").expect("write");
        let parsed = CodexReviewer
            .parse(&cfg(), cfg().primary(), &truncated, Some(&file))
            .expect("the file is authoritative");
        assert_eq!(parsed.text, "## Verdict\nAPPROVE");
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn an_over_cap_final_message_file_is_output_truncated_not_a_partial_review() {
        // The final-message file is read capped at the 8 MiB per-stream cap. A file over the cap
        // must never be parsed as a review: it becomes OUTPUT_TRUNCATED, not a truncated verdict
        // presented as complete.
        let dir = std::env::temp_dir().join("cross-review-codex-file-cap-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join(format!("{}-oversized-last.txt", std::process::id()));
        // One byte over the cap trips read_capped's over-cap flag.
        let oversized = vec![b'a'; super::super::MAX_OUTPUT_BYTES + 1];
        std::fs::write(&file, &oversized).expect("write");

        let err = CodexReviewer
            .parse(
                &cfg(),
                cfg().primary(),
                &outcome(r#"{"type":"turn.completed"}"#, true),
                Some(&file),
            )
            .unwrap_err();
        assert_eq!(err.code, "OUTPUT_TRUNCATED");
        std::fs::remove_file(&file).ok();
    }
}
