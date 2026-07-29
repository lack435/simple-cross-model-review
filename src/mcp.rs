//! MCP over stdio: newline-delimited JSON-RPC 2.0.
//!
//! Hand-rolled rather than pulled from a crate. The protocol surface we need is four
//! methods wide, and keeping the dependency list at serde is what lets this ship as a
//! single small executable with nothing to install.
//!
//! stdout carries protocol traffic only. Anything diagnostic goes to stderr.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::tools::{version_line, App, VERSION};

/// Versions we can speak. They are equivalent for a tools-only server; we echo the
/// client's choice when we recognise it so a newer client is not downgraded.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const FALLBACK_PROTOCOL: &str = "2024-11-05";

pub fn serve(app: Arc<App>) {
    let stdin = std::io::stdin();
    let writer = Arc::new(Mutex::new(std::io::stdout()));
    // Handler threads are joined at shutdown; exiting while one is mid-flight would
    // drop a response the client is still waiting on.
    let mut in_flight: Vec<std::thread::JoinHandle<()>> = Vec::new();

    eprintln!(
        "{}: serving MCP on stdio, reviewer = {}",
        version_line(),
        app.cfg().describe_reviewer()
    );

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("cross-review: stdin read error: {e}");
                break;
            }
        };
        // Some clients prefix the first write with a UTF-8 BOM, which is not valid
        // JSON. Dropping it costs nothing and turns a hard failure into a non-event.
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("cross-review: ignoring unparseable message: {e}");
                send(
                    &writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32700, "message": format!("parse error: {e}")}
                    }),
                );
                continue;
            }
        };

        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        // A message with no id is a notification: acknowledge nothing.
        let Some(id) = id else {
            if method == "notifications/cancelled" {
                eprintln!("cross-review: client cancelled a request");
            }
            continue;
        };

        match method.as_str() {
            // Tool calls can block for minutes, so each gets its own thread. Without
            // this a long poll would stall pings and cancellations on the same pipe.
            "tools/call" => {
                let app = Arc::clone(&app);
                let writer = Arc::clone(&writer);
                let spawned = std::thread::Builder::new()
                    .name("tools-call".to_string())
                    .spawn(move || {
                        let result = dispatch_tool(&app, &params);
                        send(
                            &writer,
                            json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        );
                    });
                match spawned {
                    Ok(handle) => {
                        in_flight.retain(|h| !h.is_finished());
                        in_flight.push(handle);
                    }
                    Err(e) => eprintln!("cross-review: could not spawn handler thread: {e}"),
                }
            }
            _ => {
                let response = handle_sync(&app, &method, &params, &id);
                send(&writer, response);
            }
        }
    }

    let pending = in_flight.iter().filter(|h| !h.is_finished()).count();
    if pending > 0 {
        eprintln!("cross-review: stdin closed, finishing {pending} in-flight tool call(s)");
    }
    for handle in in_flight {
        let _ = handle.join();
    }

    eprintln!("cross-review: stdin closed, shutting down");
}

fn handle_sync(app: &App, method: &str, params: &Value, id: &Value) -> Value {
    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) {
                requested
            } else {
                FALLBACK_PROTOCOL
            };
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": protocol,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "cross-review", "version": VERSION},
                    "instructions": server_instructions(app),
                }
            })
        }
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tool_definitions(app)}
        }),
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        // Declared unsupported in capabilities, but some clients probe anyway and an
        // empty list is friendlier than an error.
        "resources/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"resources": []}}),
        "resources/templates/list" => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"resourceTemplates": []}})
        }
        "prompts/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"prompts": []}}),
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        }),
    }
}

/// Tool failures are reported as `isError` results, not JSON-RPC errors: the calling
/// model needs to read the remediation text, and protocol-level errors are not
/// consistently surfaced to it.
fn dispatch_tool(app: &App, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome = match name {
        "cross_model_review" => app.start_review(&args),
        "cross_model_review_result" => app.review_result(&args),
        "cross_model_review_status" => Ok(app.status()),
        "cross_model_review_cancel" => app.cancel(&args),
        other => {
            return text_result(
                format!(
                    "Unknown tool '{other}'. This server provides cross_model_review, \
                     cross_model_review_result, cross_model_review_status, and \
                     cross_model_review_cancel."
                ),
                true,
            )
        }
    };

    match outcome {
        Ok(text) => text_result(text, false),
        Err(failure) => text_result(failure.render_for_agent(), true),
    }
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn send(writer: &Arc<Mutex<std::io::Stdout>>, message: Value) {
    let mut out = writer.lock().unwrap_or_else(|e| e.into_inner());
    let line = match serde_json::to_string(&message) {
        Ok(line) => line,
        Err(e) => {
            eprintln!("cross-review: could not serialise response: {e}");
            return;
        }
    };
    if let Err(e) = writeln!(out, "{line}") {
        eprintln!("cross-review: could not write response: {e}");
        return;
    }
    let _ = out.flush();
}

fn server_instructions(app: &App) -> String {
    let cfg = app.cfg();
    let reviewer = cfg.describe_reviewer();
    format!(
        "This server sends your work to {reviewer} for an independent review, and returns what it \
         says.\n\n\
         Use it when you want a second pair of eyes from a different model: before handing \
         substantial work back to the user, after a change you are unsure about, or when the user \
         asks for a review.\n\n\
         Reviews are asynchronous. cross_model_review starts one and returns a review_id; \
         cross_model_review_result waits for and returns the review. Sessions are named, so \
         calling cross_model_review again with the same session name gives you a re-review from a \
         reviewer that still remembers its earlier findings.\n\n\
         If a tool call comes back as an error, the review did not happen. Stop, and tell the \
         user what the error says. Do not review your own work in place of the external reviewer.",
    )
}

fn tool_definitions(app: &App) -> Vec<Value> {
    let cfg = app.cfg();
    let reviewer = cfg.describe_reviewer();

    // Stated accurately per reviewer, because it changes what the caller must supply. A
    // reviewer with no shell cannot obtain a diff, and a description that implied
    // otherwise invited requests like "review the branch diff" that silently could not be
    // carried out -- with no permission denial to surface, since the tool is absent.
    let access = if cfg.reviewer_has_shell() {
        "The reviewer has read-only access to this repository: it can read files and run \
         read-only shell commands such as `git diff` and `git log`, so it can inspect the \
         change history itself. You do not need to paste code. Describe what changed and \
         what you want scrutinised."
    } else {
        "The reviewer can read and search files in this repository, so you do not need to \
         paste whole files. It has NO shell, so it cannot run `git` and cannot obtain a \
         diff. If the review depends on what changed rather than on the current state of \
         the code, include the diff or a precise description of the change in \
         'instructions' -- otherwise the reviewer can only judge the code as it now \
         stands, and will say so."
    };
    let caller_hint = match cfg.reviewer {
        crate::config::ReviewerKind::Claude => {
            "The reviewer is a Claude model, so this is most useful when you are not one."
        }
        crate::config::ReviewerKind::Codex => {
            "The reviewer is an OpenAI model, so this is most useful when you are not one."
        }
    };

    vec![
        json!({
            "name": "cross_model_review",
            "description": format!(
                "Send work to {reviewer} for an independent code review. {caller_hint}\n\n\
                 Returns immediately with a review_id; the review itself runs in the background. \
                 Collect it with cross_model_review_result.\n\n\
                 {access}\n\n\
                 To re-review after acting on feedback, call this again with the same 'session' \
                 value: the reviewer keeps its earlier findings in context and will tell you what \
                 is now resolved.\n\n\
                 If this fails, the review did not happen: stop and tell the user what the error \
                 says rather than reviewing the work yourself."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instructions": {
                        "type": "string",
                        "description":
                            "What to review and what to look for, passed to the reviewer verbatim. \
                             Include the intent of the change and any specific worries, not just a \
                             list of files. A reviewer told why the change exists finds better \
                             problems than one told only what changed."
                    },
                    "context_paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description":
                            "Optional paths to point the reviewer at first. Starting points, not \
                             limits; it can read anything else it needs."
                    },
                    "session": {
                        "type": "string",
                        "description":
                            "Name for this review conversation, chosen by you (for example \
                             'default' or 'auth-refactor'). Reusing a name continues that review \
                             with its history intact. Defaults to 'default'."
                    },
                    "fresh": {
                        "type": "boolean",
                        "description":
                            "Start a new reviewer conversation even if this session name already \
                             exists. Use when the work has moved on and earlier findings would \
                             only mislead. Defaults to false."
                    }
                },
                "required": ["instructions"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "cross_model_review_result",
            "description": format!(
                "Wait for and return the review from {reviewer}.\n\n\
                 This call blocks while the reviewer works, up to wait_seconds, so a single call \
                 usually suffices. If it returns status=running, call it again with the same \
                 review_id.\n\n\
                 If it fails, the review did not happen: stop and tell the user what the error \
                 says."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "review_id": {
                        "type": "string",
                        "description": "The review_id returned by cross_model_review. Preferred."
                    },
                    "session": {
                        "type": "string",
                        "description":
                            "Session name, as an alternative to review_id. Returns that session's \
                             most recent review."
                    },
                    "wait_seconds": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": crate::config::MAX_WAIT_SECS,
                        "description": format!(
                            "How long to wait for the review before returning. Defaults to {}, \
                             capped at {}. Prefer a large value: waiting once beats polling.",
                            crate::config::DEFAULT_WAIT_SECS,
                            crate::config::MAX_WAIT_SECS
                        )
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "cross_model_review_status",
            "description": format!(
                "Check that the review tool is usable: whether the {} CLI is installed, whether \
                 it is signed in, which model and effort are pinned, and which review sessions \
                 exist. Costs nothing and calls no model. Worth calling first if a review has \
                 just failed, or to confirm the setup before relying on it.",
                cfg.reviewer.as_str()
            ),
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "cross_model_review_cancel",
            "description":
                "Stop a review that is still running. Use it when the work it was reviewing has \
                 changed underneath it, or the user has moved on. The review session itself \
                 survives, so a later cross_model_review with the same session name still resumes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "review_id": {
                        "type": "string",
                        "description": "The review_id to cancel."
                    }
                },
                "required": ["review_id"],
                "additionalProperties": false
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn app() -> Arc<App> {
        let cfg = Config::from_args(&["--reviewer".into(), "codex".into()]).expect("config");
        Arc::new(App::new(cfg))
    }

    #[test]
    fn initialize_echoes_a_supported_protocol_version() {
        let response = handle_sync(
            &app(),
            "initialize",
            &json!({"protocolVersion": "2025-06-18"}),
            &json!(1),
        );
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["serverInfo"]["name"], "cross-review");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_for_an_unknown_protocol_version() {
        let response = handle_sync(
            &app(),
            "initialize",
            &json!({"protocolVersion": "1999-01-01"}),
            &json!(1),
        );
        assert_eq!(response["result"]["protocolVersion"], FALLBACK_PROTOCOL);
    }

    #[test]
    fn initialize_tolerates_missing_params() {
        let response = handle_sync(&app(), "initialize", &Value::Null, &json!(1));
        assert_eq!(response["result"]["protocolVersion"], FALLBACK_PROTOCOL);
    }

    #[test]
    fn lists_exactly_the_four_tools_with_valid_schemas() {
        let response = handle_sync(&app(), "tools/list", &Value::Null, &json!(2));
        let tools = response["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "cross_model_review",
                "cross_model_review_result",
                "cross_model_review_status",
                "cross_model_review_cancel"
            ]
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn review_tool_requires_instructions_and_forbids_extra_properties() {
        let response = handle_sync(&app(), "tools/list", &Value::Null, &json!(2));
        let tool = &response["result"]["tools"][0];
        assert_eq!(tool["inputSchema"]["required"][0], "instructions");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn unknown_method_is_a_jsonrpc_error() {
        let response = handle_sync(&app(), "does/not/exist", &Value::Null, &json!(3));
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn ping_is_answered() {
        let response = handle_sync(&app(), "ping", &Value::Null, &json!(4));
        assert!(response["result"].is_object());
        assert!(response.get("error").is_none());
    }

    #[test]
    fn unknown_tool_is_an_is_error_result_not_a_protocol_error() {
        let result = dispatch_tool(&app(), &json!({"name": "nope", "arguments": {}}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool 'nope'"));
    }

    #[test]
    fn a_tool_failure_is_reported_as_is_error_with_guidance() {
        // No 'instructions' argument.
        let result = dispatch_tool(
            &app(),
            &json!({"name": "cross_model_review", "arguments": {}}),
        );
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("BAD_REQUEST"));
    }

    #[test]
    fn status_tool_never_errors_even_when_the_cli_is_absent() {
        let cfg = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--bin".into(),
            "C:\\definitely\\not\\here\\codex.exe".into(),
        ])
        .expect("config");
        let app = Arc::new(App::new(cfg));
        let result = dispatch_tool(&app, &json!({"name": "cross_model_review_status"}));
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ready:         NO"));
        assert!(text.contains("CLI_NOT_FOUND"));
    }
}
