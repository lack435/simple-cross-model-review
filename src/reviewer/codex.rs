//! Codex adapter (`codex exec`).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{Invocation, Parsed, Reviewer, RunOutcome};
use crate::config::Config;
use crate::errors::{self, Failure};

pub struct CodexReviewer;

impl Reviewer for CodexReviewer {
    fn auth_check(&self, bin: &Path) -> Result<String, Failure> {
        let mut cmd = Command::new(bin);
        cmd.arg("login").arg("status");
        let out =
            super::run(cmd, "", Duration::from_secs(30), &AtomicBool::new(false)).map_err(|e| {
                errors::spawn_failed("codex", &bin.display().to_string(), e.to_string())
            })?;

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
        bin: &Path,
        resume: Option<&str>,
        tmp_id: &str,
    ) -> std::io::Result<Invocation> {
        let last_message_file = super::tmp_file(cfg, tmp_id, "codex-last.txt")?;

        let mut cmd = Command::new(bin);
        cmd.current_dir(&cfg.cwd);
        cmd.arg("exec");

        match resume {
            Some(session_id) => {
                // Positional order is fixed: resume <SESSION_ID> [PROMPT].
                cmd.arg("resume").arg(session_id).arg("-");
            }
            None => {
                cmd.arg("-");
                // Only the fresh-session form accepts a sandbox policy; a resumed
                // session keeps the policy it was created with.
                cmd.args(["-s", &cfg.sandbox]);
            }
        }

        cmd.arg("--json");
        cmd.arg("--skip-git-repo-check");
        cmd.args(["-m", &cfg.model]);
        // No shell is involved, so the quotes are part of the value and make this a
        // TOML string rather than relying on the raw-literal fallback.
        cmd.args(["-c", &format!("model_reasoning_effort=\"{}\"", cfg.effort)]);
        if cfg.isolate_mcp {
            // `codex exec` does start configured MCP servers (verified: a marker server
            // ran and left a file), so a reviewer that also has cross-review registered
            // could call back into us. `-c mcp_servers={}` does not help -- dotted
            // overrides merge into the existing table rather than replacing it -- so skip
            // the user config entirely. Auth still resolves from CODEX_HOME, and model,
            // effort and sandbox are all passed explicitly below.
            cmd.arg("--ignore-user-config");
        }
        cmd.arg("-o").arg(&last_message_file);

        Ok(Invocation {
            command: cmd,
            last_message_file: Some(last_message_file),
        })
    }

    fn parse(
        &self,
        cfg: &Config,
        out: &RunOutcome,
        last_message_file: Option<&Path>,
    ) -> Result<Parsed, Failure> {
        let events = parse_events(&out.stdout);

        if !out.success {
            let mut detail = out.diagnostics();
            if !events.errors.is_empty() {
                detail = format!("{}\n\n{}", events.errors.join("\n"), detail);
            }
            if detail.to_ascii_lowercase().contains("no session")
                || detail.to_ascii_lowercase().contains("session not found")
                || detail.to_ascii_lowercase().contains("no rollout")
            {
                return Err(errors::session_not_found("(resumed session)", "unknown"));
            }
            return Err(errors::classify(
                "codex",
                &cfg.model,
                &cfg.effort,
                out.exit,
                &detail,
            ));
        }

        // The final-message file is authoritative; the event stream is the fallback.
        let from_file = last_message_file
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let text = from_file
            .or_else(|| events.last_message.clone())
            .unwrap_or_default();

        if text.is_empty() {
            let detail = if events.errors.is_empty() {
                out.diagnostics()
            } else {
                events.errors.join("\n")
            };
            return Err(errors::empty_review("codex", detail));
        }

        Ok(Parsed {
            text,
            session_id: events.thread_id,
            denials: Vec::new(),
        })
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Events {
    thread_id: Option<String>,
    last_message: Option<String>,
    errors: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_STREAM: &str = r###"Reading additional input from stdin...
{"type":"thread.started","thread_id":"019faa01-a2d3-78c0-a67a-2ffe1ca75969"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"thinking"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"## Verdict\nREQUEST CHANGES"}}
{"type":"turn.completed","usage":{"input_tokens":14124,"output_tokens":5}}"###;

    fn cfg() -> Config {
        Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config")
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
            .parse(&cfg(), &outcome(REAL_STREAM, true), None)
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
            .parse(&cfg(), &outcome(REAL_STREAM, true), Some(&path))
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
    fn empty_output_file_falls_back_rather_than_reporting_an_empty_review() {
        let dir = std::env::temp_dir().join("cross-review-tests");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("last-empty.txt");
        std::fs::write(&path, "   \n").expect("write");

        let parsed = CodexReviewer
            .parse(&cfg(), &outcome(REAL_STREAM, true), Some(&path))
            .expect("parse");
        assert_eq!(parsed.text, "## Verdict\nREQUEST CHANGES");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rate_limit_on_failure_is_classified() {
        let mut out = outcome("", false);
        out.stderr = "Error: 429 Too Many Requests".into();
        let err = CodexReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED");
    }

    #[test]
    fn missing_resume_target_maps_to_session_not_found() {
        let mut out = outcome("", false);
        out.stderr = "Error: no session found with id abc".into();
        let err = CodexReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "SESSION_NOT_FOUND");
    }

    #[test]
    fn successful_run_with_no_message_anywhere_is_an_empty_review() {
        let err = CodexReviewer
            .parse(&cfg(), &outcome(r#"{"type":"turn.completed"}"#, true), None)
            .unwrap_err();
        assert_eq!(err.code, "EMPTY_REVIEW");
    }
}
