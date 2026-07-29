//! Claude Code adapter (`claude -p`).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{Invocation, Parsed, Reviewer, RunOutcome};
use crate::config::Config;
use crate::errors::{self, Failure};

/// Tools removed outright rather than merely denied, so the model has no write
/// affordance to attempt in the first place.
const DENIED_TOOLS: &str = "Edit,Write,NotebookEdit";

pub struct ClaudeReviewer;

impl Reviewer for ClaudeReviewer {
    fn auth_check(&self, bin: &Path, cfg: &Config) -> Result<String, Failure> {
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
        let out =
            super::run(cmd, "", Duration::from_secs(30), &AtomicBool::new(false)).map_err(|e| {
                errors::spawn_failed("claude", &bin.display().to_string(), e.to_string())
            })?;

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
        bin: &Path,
        resume: Option<&str>,
        _tmp_id: &str,
    ) -> std::io::Result<Invocation> {
        let mut cmd = Command::new(bin);
        cmd.current_dir(&cfg.cwd);
        cmd.arg("-p");
        cmd.args(["--output-format", "json"]);
        cmd.args(["--model", &cfg.model]);
        cmd.args(["--effort", &cfg.effort]);
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
        out: &RunOutcome,
        _last_message_file: Option<&Path>,
    ) -> Result<Parsed, Failure> {
        let parsed: Value = serde_json::from_str(out.stdout.trim()).map_err(|_| {
            if out.success {
                errors::empty_review("claude", out.diagnostics())
            } else {
                super::failure_for(cfg, out)
            }
        })?;

        let session_id = parsed
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
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
                &cfg.model,
                &cfg.effort,
                out.exit,
                &evidence,
                &detail,
            ));
        }

        if text.is_empty() {
            return Err(errors::empty_review("claude", out.diagnostics()));
        }

        Ok(Parsed {
            text,
            session_id,
            denials: collect_denials(&parsed),
        })
    }
}

/// `permission_denials` tells us which read-only commands the reviewer wanted but
/// could not run. A review that hit several may be thinner than it looks.
fn collect_denials(parsed: &Value) -> Vec<String> {
    let Some(list) = parsed.get("permission_denials").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
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
        }
    }

    #[test]
    fn parses_a_successful_review() {
        let json = r###"{"type":"result","subtype":"success","is_error":false,
            "result":"## Verdict\nAPPROVE","session_id":"3d759777-4801-4e26-b6c5-4fbdb70adbbf",
            "permission_denials":[]}"###;
        let parsed = ClaudeReviewer
            .parse(&cfg(), &outcome(json, true), None)
            .expect("parse");
        assert_eq!(parsed.text, "## Verdict\nAPPROVE");
        assert_eq!(
            parsed.session_id.as_deref(),
            Some("3d759777-4801-4e26-b6c5-4fbdb70adbbf")
        );
        assert!(parsed.denials.is_empty());
    }

    #[test]
    fn surfaces_permission_denials() {
        let json = r#"{"is_error":false,"result":"ok","session_id":"s",
            "permission_denials":[{"tool_name":"Bash","tool_input":{"command":"echo pwned > EVIL.txt"}}]}"#;
        let parsed = ClaudeReviewer
            .parse(&cfg(), &outcome(json, true), None)
            .expect("parse");
        assert_eq!(parsed.denials, vec!["Bash: echo pwned > EVIL.txt"]);
    }

    #[test]
    fn empty_result_is_not_a_review() {
        let json = r#"{"is_error":false,"result":"   ","session_id":"s"}"#;
        let err = ClaudeReviewer
            .parse(&cfg(), &outcome(json, true), None)
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
        }
    }

    #[test]
    fn a_review_mentioning_429_is_not_reported_as_rate_limited() {
        let out = failure_with_review_text(
            "## Findings\n- `src/lib.rs:429` returns 429 on quota exhaustion; too many requests \
             are not retried.",
        );
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
        // The text is still shown to the user, just never matched against.
        assert!(err.detail.unwrap_or_default().contains("429"));
    }

    #[test]
    fn a_review_saying_does_not_support_is_not_reported_as_model_unavailable() {
        let out = failure_with_review_text(
            "The parser does not support nested groups; this is an invalid model of the grammar.",
        );
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
    }

    #[test]
    fn a_review_mentioning_a_missing_session_is_not_reported_as_session_not_found() {
        let out = failure_with_review_text(
            "`tools.rs:200` returns 'no conversation found' when the session not found branch is \
             hit, which is confusing.",
        );
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
    }

    #[test]
    fn a_real_cli_error_on_stderr_is_still_classified() {
        // The other half of the property: excluding model text must not blind us to the
        // CLI's own diagnosis.
        let mut out = failure_with_review_text("a perfectly ordinary review");
        out.stderr = "Error: 429 Too Many Requests".into();
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
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
        };
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "AUTH_EXPIRED_MIDRUN");
    }

    #[test]
    fn non_json_stdout_on_failure_is_classified_not_swallowed() {
        let mut out = outcome("Invalid API key · Please run /login", false);
        out.stderr = "401 unauthorized".into();
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "AUTH_EXPIRED_MIDRUN");
    }

    #[test]
    fn expired_resume_target_maps_to_session_not_found() {
        // Fixture taken from the real thing: `claude -p --resume <bogus-uuid>` exits 1 with
        // the message on stderr and stdout completely empty. The previous fixture put it in
        // a `result` field of valid JSON, which the CLI never does -- so it exercised a
        // branch that could not be reached in practice, and the automatic retry into a
        // fresh session had in fact never worked.
        let out = RunOutcome {
            stdout: String::new(),
            stderr: "No conversation found with session ID: \
                     00000000-1111-2222-3333-444444444444"
                .to_string(),
            exit: Some(1),
            success: false,
            timed_out: false,
            cancelled: false,
        };
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "SESSION_NOT_FOUND");
    }
}
