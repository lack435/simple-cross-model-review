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
    fn auth_check(&self, bin: &Path) -> Result<String, Failure> {
        let mut cmd = Command::new(bin);
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
            let mut detail = out.diagnostics();
            if !text.is_empty() {
                detail = format!("{detail}\n\n{text}");
            }
            if !subtype.is_empty() {
                detail = format!("subtype: {subtype}\n{detail}");
            }
            // A resume against an expired session reports the id as unknown.
            if detail
                .to_ascii_lowercase()
                .contains("no conversation found")
                || detail.to_ascii_lowercase().contains("session not found")
            {
                return Err(errors::session_not_found(
                    "(resumed session)",
                    session_id.as_deref().unwrap_or("unknown"),
                ));
            }
            return Err(errors::classify(
                "claude",
                &cfg.model,
                &cfg.effort,
                out.exit,
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

    #[test]
    fn non_json_stdout_on_failure_is_classified_not_swallowed() {
        let mut out = outcome("Invalid API key · Please run /login", false);
        out.stderr = "401 unauthorized".into();
        let err = ClaudeReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "AUTH_EXPIRED_MIDRUN");
    }

    #[test]
    fn expired_resume_target_maps_to_session_not_found() {
        let json = r#"{"is_error":true,"subtype":"error","result":"No conversation found with session ID abc","session_id":"abc"}"#;
        let err = ClaudeReviewer
            .parse(&cfg(), &outcome(json, true), None)
            .unwrap_err();
        assert_eq!(err.code, "SESSION_NOT_FOUND");
    }
}
