//! Codex adapter (`codex exec`).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::{Invocation, Parsed, Reviewer, RunOutcome};
use crate::config::Config;
use crate::errors::{self, Failure};
use crate::metrics::Usage;

pub struct CodexReviewer;

impl Reviewer for CodexReviewer {
    fn auth_check(&self, bin: &Path, cfg: &Config) -> Result<String, Failure> {
        let mut cmd = Command::new(bin);
        // Run outside the project, like `invocation`, so this preflight is not the one
        // invocation that loads the reviewed repository's configuration. `login status`
        // takes no `--ignore-user-config`, and must not have one anyway: auth is exactly
        // what it is checking.
        cmd.current_dir(super::neutral_dir(cfg));
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
                // `-s` exists only on the fresh-session form.
                cmd.args(["-s", &cfg.sandbox]);
            }
        }

        cmd.arg("--json");
        cmd.arg("--skip-git-repo-check");
        cmd.args(["-m", &cfg.model]);
        // Stated on every turn, including resumes, via the config override that `resume`
        // does accept. A resumed session does appear to retain the policy it was created
        // with -- verified: a write attempt on turn 2 of a `-s read-only` session was
        // refused -- but relying on that meant the sandbox was the one setting inherited
        // by accident rather than asserted, while `-m` and effort were both re-passed.
        // Verified that `resume` accepts this override and still refuses writes.
        cmd.args(["-c", &format!("sandbox_mode=\"{}\"", cfg.sandbox)]);
        // No shell is involved, so the quotes are part of the value and make this a
        // TOML string rather than relying on the raw-literal fallback.
        cmd.args(["-c", &format!("model_reasoning_effort=\"{}\"", cfg.effort)]);
        if cfg.isolate_reviewer {
            // `codex exec` does start configured MCP servers (verified: a marker server
            // ran and left a file), so a reviewer that also has cross-review registered
            // could call back into us. `-c mcp_servers={}` does not help -- dotted
            // overrides merge into the existing table rather than replacing it -- so skip
            // the user config entirely. Auth still resolves from CODEX_HOME, and model,
            // effort and sandbox are all passed explicitly above.
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

            // An expired resume target is detected inside `classify`.
            return Err(errors::classify(
                "codex",
                &cfg.model,
                &cfg.effort,
                out.exit,
                &evidence,
                &detail,
            ));
        }

        // The final-message file is authoritative; the event stream is the fallback.
        let from_file = last_message_file
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

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
                if let Some(truncated) = super::truncation_failure(cfg, out) {
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
        // Note what this does *not* claim. That our cap did not touch the final-message
        // file is structural: we cap only bytes read from the pipes. Whether the CLI
        // finished writing that file is a different question, it has not been observed
        // for a run that produced this much output, and it is not asserted here.
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

        Ok(Parsed {
            text,
            session_id: events.thread_id,
            denials: Vec::new(),
            warnings,
            usage: Usage {
                api_calls: (events.turns_seen > 0).then_some(events.turns_seen),
                ..events.usage
            },
            usage_is_cumulative: true,
        })
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
                    events.usage.input_tokens = match (total_in, cached) {
                        (Some(total), Some(cached)) => Some(total.saturating_sub(cached)),
                        (total, None) => total,
                        (None, _) => None,
                    };
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
    fn usage_is_read_from_the_event_stream() {
        let events = parse_events(REAL_STREAM);
        assert_eq!(events.usage.input_tokens, Some(14_124));
        assert_eq!(events.usage.output_tokens, Some(5));
        assert_eq!(events.turns_seen, 1);
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
    fn a_codex_turn_declares_its_usage_cumulative_and_a_claude_turn_does_not() {
        // The two CLIs differ and the difference is invisible in the numbers, so the
        // adapter states it rather than leaving the caller to infer -- inferring it is
        // what produced eight rounds of inflated figures.
        let stream = r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#;
        let parsed = CodexReviewer
            .parse(&cfg(), &outcome(stream, true), None)
            .expect("parse");
        assert!(parsed.usage_is_cumulative);
    }

    #[test]
    fn a_stream_with_no_usage_reports_none_rather_than_zero_calls() {
        // `api_calls: Some(0)` would read as "this turn made no model calls", which is
        // a claim; `None` is the honest "the CLI did not say".
        let stream = r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#;
        let parsed = CodexReviewer
            .parse(&cfg(), &outcome(stream, true), None)
            .expect("parse");
        assert!(parsed.usage.is_empty());
        assert_eq!(parsed.usage.api_calls, None);
    }

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
            stdout_truncated: false,
            stderr_truncated: false,
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
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn an_agent_message_mentioning_429_is_not_reported_as_rate_limited() {
        let out = failure_with_agent_message(
            "`server.rs:429` should return 429 when the quota is exhausted.",
        );
        let err = CodexReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
        assert!(err.detail.unwrap_or_default().contains("429"));
    }

    #[test]
    fn an_agent_message_mentioning_a_missing_session_is_not_session_not_found() {
        let out = failure_with_agent_message(
            "The error path prints 'session not found' but the session exists.",
        );
        let err = CodexReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "REVIEWER_FAILED", "misclassified as {}", err.code);
    }

    #[test]
    fn a_stream_error_event_is_still_classified() {
        // Error events are the CLI's own, so they remain evidence.
        let mut out = failure_with_agent_message("ordinary review prose");
        out.stdout.push_str(
            "{\"type\":\"error\",\"message\":\"stream error: 429 rate limit exceeded\"}\n",
        );
        let err = CodexReviewer.parse(&cfg(), &out, None).unwrap_err();
        assert_eq!(err.code, "RATE_LIMITED");
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

    #[test]
    fn a_truncated_event_stream_with_no_message_is_reported_as_truncation() {
        // Only once the final-message file has been tried and found wanting: that file is
        // written by the CLI and is unaffected by our pipe cap, so a truncated stream that
        // still yielded a review must not be a failure at all.
        let truncated = RunOutcome {
            stdout_truncated: true,
            ..outcome(r#"{"type":"turn.completed"}"#, true)
        };
        let err = CodexReviewer.parse(&cfg(), &truncated, None).unwrap_err();
        assert_eq!(err.code, "OUTPUT_TRUNCATED");

        // A truncated stream whose review did survive in the file is a success.
        let dir = std::env::temp_dir().join("cross-review-codex-truncation-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join(format!("{}-last.txt", std::process::id()));
        std::fs::write(&file, "## Verdict\nAPPROVE").expect("write");
        let parsed = CodexReviewer
            .parse(&cfg(), &truncated, Some(&file))
            .expect("the file is authoritative");
        assert_eq!(parsed.text, "## Verdict\nAPPROVE");
        std::fs::remove_file(&file).ok();
    }
}
