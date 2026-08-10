//! Claude Code adapter (`claude -p`).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{Invocation, Parsed, Reviewer, RunOutcome};
use crate::config::{Config, ReviewerSpec};
use crate::errors::{self, Failure};
use crate::metrics::Usage;

/// Tools removed outright rather than merely denied, so the model has no write
/// affordance to attempt in the first place.
const DENIED_TOOLS: &str = "Edit,Write,NotebookEdit";

pub struct ClaudeReviewer;

impl Reviewer for ClaudeReviewer {
    fn auth_check(&self, bin: &Path, cfg: &Config, cancel: &AtomicBool) -> Result<String, Failure> {
        let mut cmd = Command::new(bin);
        // Isolated and run outside the project, like `invocation`. The stated policy is
        // that the reviewer CLI never loads the reviewed repository's configuration, and
        // this preflight -- which runs before every review and on every status call -- was
        // the one invocation that broke it by construction, inheriting the project as its
        // working directory with no isolation flags.
        cmd.current_dir(super::neutral_dir(cfg));
        if cfg.isolate_reviewer {
            cmd.arg("--safe-mode");
            cmd.arg("--strict-mcp-config");
        }
        cmd.arg("auth").arg("status");
        let out = super::run(cmd, "", Duration::from_secs(30), cancel).map_err(|e| {
            errors::spawn_failed("claude", &bin.display().to_string(), e.to_string())
        })?;

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
    ) -> std::io::Result<Invocation> {
        let mut cmd = Command::new(bin);
        cmd.current_dir(&cfg.cwd);
        cmd.arg("-p");
        cmd.args(["--output-format", "json"]);
        cmd.args(["--model", &spec.model]);
        cmd.args(["--effort", &spec.effort]);
        // dontAsk denies anything outside the allow-list instead of prompting, so a
        // non-interactive run can neither hang nor escalate.
        cmd.args(["--permission-mode", "dontAsk"]);
        cmd.args(["--tools", &cfg.tools]);
        // One argument per rule: a project path containing a space or comma would
        // otherwise be split into fragments by the CLI's list parsing.
        cmd.arg("--allowed-tools");
        for rule in &cfg.allowed_tools {
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

        let denials = collect_denials(&parsed);
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
            usage: collect_usage(&parsed),
            // Claude's result document describes the turn that just ran, not the
            // conversation. Verified by the fields themselves: `num_turns` is this
            // invocation's model-call count, and a resumed turn does not inherit the
            // previous one's totals.
            usage_is_cumulative: false,
        })
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
        }
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
        };
        let err = ClaudeReviewer
            .parse(&cfg(), cfg().primary(), &out, None)
            .unwrap_err();
        assert_eq!(err.code, "SESSION_NOT_FOUND");
    }
}
