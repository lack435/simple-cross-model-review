//! MCP over stdio: newline-delimited JSON-RPC 2.0.
//!
//! Hand-rolled rather than pulled from a crate. The protocol surface we need is four
//! methods wide, and keeping the dependency list at serde is what lets this ship as a
//! single small executable with nothing to install.
//!
//! stdout carries protocol traffic only. Anything diagnostic goes to stderr.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::cancel::RequestCancel;
use crate::tools::{version_line, App, VERSION};

/// Versions we can speak. They are equivalent for a tools-only server; we echo the
/// client's choice when we recognise it so a newer client is not downgraded.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const FALLBACK_PROTOCOL: &str = "2024-11-05";

/// `tools/call` handlers still running, keyed by the client's request id, so that
/// `notifications/cancelled` can reach the right one. Every other method is answered on
/// the reader thread and has finished before the next line is even read.
type Pending = Arc<Mutex<HashMap<String, Arc<RequestCancel>>>>;

/// Where responses go. A trait object rather than `Stdout` so a test can read back what
/// a response path actually wrote; in the server it is always stdout.
type Writer = Arc<Mutex<dyn Write + Send>>;

pub fn serve(app: Arc<App>) {
    let stdin = std::io::stdin();
    let writer: Writer = Arc::new(Mutex::new(std::io::stdout()));
    // Handler threads are joined at shutdown; exiting while one is mid-flight would
    // drop a response the client is still waiting on.
    let mut in_flight: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

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
                handle_cancellation(&app, &pending, &params);
            }
            continue;
        };

        match method.as_str() {
            // Tool calls can block for minutes, so each gets its own thread. Without
            // this a long poll would stall pings and cancellations on the same pipe.
            "tools/call" => {
                // Reaped before the spawn, not after: on the failure path below there is
                // no new handle to push, and a vector that only ever grows would hold an
                // OS thread handle per finished call for the life of the process.
                in_flight.retain(|h| !h.is_finished());
                let builder = std::thread::Builder::new().name("tools-call".to_string());
                if let Some(handle) = start_tool_call(builder, &app, &writer, &pending, id, params)
                {
                    in_flight.push(handle);
                }
            }
            _ => {
                let response = handle_sync(&app, &method, &params, &id);
                send(&writer, response);
            }
        }
    }

    let unfinished = in_flight.iter().filter(|h| !h.is_finished()).count();
    if unfinished > 0 {
        eprintln!("cross-review: stdin closed, finishing {unfinished} in-flight tool call(s)");
    }
    for handle in in_flight {
        let _ = handle.join();
    }

    eprintln!("cross-review: stdin closed, shutting down");
}

/// Runs one `tools/call` on its own thread, and answers the request itself if the thread
/// cannot be started. Returns the handle to join at shutdown, or `None` when it answered.
///
/// Split out of `serve` for the sake of that second case. `serve` owns stdin and cannot be
/// driven from a test, whereas this takes the `Builder` it spawns from -- and a `Builder`
/// asked for an impossible stack fails deterministically, so the failure branch is
/// reachable without the process having to actually run out of threads.
///
/// Call it from the reader thread. That is what makes the pending map safe to touch here
/// without further synchronisation, since `handle_cancellation` runs there too; see the
/// comment on the send below for what is at stake if it ever moves.
fn start_tool_call(
    builder: std::thread::Builder,
    app: &Arc<App>,
    writer: &Writer,
    pending: &Pending,
    id: Value,
    params: Value,
) -> Option<std::thread::JoinHandle<()>> {
    let key = request_key(&id);
    let request = Arc::new(RequestCancel::new());
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), Arc::clone(&request));

    // Read off before `params` moves into the closure, for the failure message below.
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // The captures are cloned inside a block rather than shadowing the originals: a
    // failed `spawn` drops the closure instead of handing it back, so `writer` and `id`
    // have to survive it to answer the request.
    let spawned = {
        let app = Arc::clone(app);
        let writer = Arc::clone(writer);
        let pending_here = Arc::clone(pending);
        let thread_key = key.clone();
        let id = id.clone();
        builder.spawn(move || {
            let mut entry = PendingEntry {
                pending: &pending_here,
                key: &thread_key,
                released: false,
            };
            let result = dispatch_tool(&app, &params, &request);
            // Released before the send, so a cancellation arriving after this
            // point finds nothing in the map at all.
            entry.release();
            // A cancelled request gets no response: the client has stopped
            // waiting for one, and the spec says not to send it. Claiming
            // rather than merely checking is what settles the one remaining
            // window -- a notification already past the map lookup and about
            // to call `cancel` -- so a response and a kill of the review it
            // named cannot both happen. A response that loses the race and
            // goes unsent is fine; the spec expects the client to tolerate
            // one either way.
            if !request.try_claim_response() {
                return;
            }
            send(
                &writer,
                json!({"jsonrpc": "2.0", "id": id, "result": result}),
            );
        })
    };

    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            eprintln!("cross-review: could not spawn handler thread: {e}");
            // Unconditional, with no `try_claim_response`: the claim exists to settle a
            // race with `handle_cancellation`, and there is none to settle when this is
            // called from the reader thread -- `serve` is that thread and so is
            // `handle_cancellation`, so nothing can have landed between the insert above
            // and the remove just now. That is a precondition on the caller now rather
            // than something this function can see, so it is stated in the doc comment.
            // Off the reader thread the cost is only a response sent for a request that
            // was cancelled in the window, which the spec tolerates: no review can be
            // killed either way, since `attach_review` only runs inside `dispatch_tool`.
            send(writer, handler_thread_unavailable_response(&id, &tool, &e));
            None
        }
    }
}

/// Removes a handler's entry from the pending map however the handler ends.
///
/// Normally `release` does it the moment the work is done. The `Drop` is the net for a
/// `dispatch_tool` that unwinds, which would otherwise strand the entry for the life of
/// the process -- the same hazard `FinishGuard` covers for the review worker.
struct PendingEntry<'a> {
    pending: &'a Pending,
    key: &'a str,
    released: bool,
}

impl PendingEntry<'_> {
    fn release(&mut self) {
        if !self.released {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(self.key);
            self.released = true;
        }
    }
}

impl Drop for PendingEntry<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Honour `notifications/cancelled`: stop the work and send no response.
///
/// Suppressing the response is what the spec asks for. Stopping the review is what saves
/// money: a reviewer nobody is waiting on otherwise runs out its full timeout budget, and
/// holds the session lease for just as long, so the next review of the same session is
/// refused as busy until it expires.
///
/// Runs on the reader thread, which is also the only thread that inserts into the map.
/// A notification therefore cannot overtake the insert for the request it names.
fn handle_cancellation(app: &App, pending: &Pending, params: &Value) {
    let reason = clamp(
        params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason given"),
    );

    let Some(request_id) = params.get("requestId") else {
        eprintln!("cross-review: ignoring notifications/cancelled with no requestId ({reason})");
        return;
    };
    // The lookup uses the id exactly as sent; only the echo of it is clamped.
    let key = request_key(request_id);
    let shown = clamp(&key);

    // Removed here rather than by the handler, so a duplicate notification for the same
    // id finds nothing and cancels nothing. The handler's own removal is then a no-op.
    let entry = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    let Some(entry) = entry else {
        // Routine, not an error: a cancellation racing a response that already went out
        // lands here, as does one for a request answered on the reader thread -- which
        // includes `initialize`, the one request the spec says must not be cancelled.
        eprintln!(
            "cross-review: cancellation for request {shown}, which is not in flight ({reason})"
        );
        return;
    };

    // stderr is the only visibility this path has, so the three outcomes are told apart:
    // no review bound to the request, one that was still running, one already finished.
    match entry.cancel() {
        None => eprintln!("cross-review: request {shown} cancelled ({reason})"),
        Some(review_id) if app.cancel_review(&review_id) => eprintln!(
            "cross-review: request {shown} cancelled ({reason}); stopped review {review_id}"
        ),
        Some(review_id) => eprintln!(
            "cross-review: request {shown} cancelled ({reason}); review {review_id} had already \
             finished"
        ),
    }
}

/// A stable map key for a JSON-RPC id, which may be a string or a number.
///
/// Serialising rather than stringifying keeps `1` and `"1"` distinct, as the protocol
/// requires: they are different requests and a client may legitimately have both open.
fn request_key(id: &Value) -> String {
    id.to_string()
}

/// Bound a client-supplied string before it reaches our diagnostics. Not a security
/// boundary -- just so a client cannot put an unbounded line, or something that renders
/// as other than what it says, into a log a human is meant to read.
///
/// `is_control` alone covers only Cc, which leaves the zero-width and bidi-override
/// characters that are the usual way to make a log line lie. std has no `is_format` and
/// a unicode-tables dependency is not worth it here, so those blocks are named outright.
fn clamp(text: &str) -> String {
    const LIMIT: usize = 200;
    let kept: Vec<char> = text
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c,
                    '\u{200b}'..='\u{200f}'
                    | '\u{2028}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}')
        })
        .collect();
    let mut out: String = kept.iter().take(LIMIT).collect();
    // Marked, so a clipped value is never mistaken for a complete one.
    if kept.len() > LIMIT {
        out.push('…');
    }
    out
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
fn dispatch_tool(app: &App, params: &Value, request: &RequestCancel) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let outcome = match name {
        "cross_model_review" => app.start_review(&args, request),
        "cross_model_review_result" => app.review_result(&args, request),
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

/// The answer to a `tools/call` whose handler thread could not be started.
///
/// An `isError` result rather than a JSON-RPC error, even though a thread that will not
/// start is a server fault and -32603 is what the protocol has for exactly that. The
/// reason `dispatch_tool` gives is about who reads the text rather than whose fault it
/// is, and that carries over: this reply is only useful if the calling model reads the
/// remediation, and protocol-level errors are not consistently surfaced to it. Answering
/// at all is the bulk of the fix -- sending nothing left the client to discover the
/// failure by its own tool timeout, minutes later and with no cause attached.
fn handler_thread_unavailable_response(id: &Value, tool: &str, error: &std::io::Error) -> Value {
    let text = crate::errors::handler_thread_unavailable(tool, &error.to_string());
    json!({"jsonrpc": "2.0", "id": id, "result": text_result(text, true)})
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn send(writer: &Writer, message: Value) {
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
            .to_string()
    } else if cfg.supplies_diff() {
        // Worth stating positively. The caller pastes a diff because it believes it has
        // to; left to infer, it will keep spending its own context on one this server
        // already fetched.
        // Qualified rather than flat: `supplies_diff` is the configured intent, and
        // whether a diff actually arrives depends on the working root being a git
        // repository, which is only known at capture time. The reviewer is told the
        // runtime answer; the caller can only be told the intent, so it must not be
        // promised more than that.
        // What is captured is what `--diff` selects, so the description asks the mode
        // rather than restating one of them: under `--diff staged` or a range this text
        // otherwise promised a working-tree capture with untracked files that those modes
        // deliberately exclude.
        let (captures, caveat) = cfg.diff.caller_summary();
        format!(
            "The reviewer can read and search files in this repository, so you do not need to \
             paste whole files. It has NO shell of its own, but it does not need one for the \
             change: when the working root is a git repository, this server captures \
             {captures}, and hands them to the reviewer with your request. Do not paste a diff \
             into 'instructions' -- describe the intent of the change and what you want \
             scrutinised instead. {caveat}"
        )
    } else {
        "The reviewer can read and search files in this repository, so you do not need to \
         paste whole files. It has NO shell, so it cannot run `git` and cannot obtain a \
         diff. If the review depends on what changed rather than on the current state of \
         the code, include the diff or a precise description of the change in \
         'instructions' -- otherwise the reviewer can only judge the code as it now \
         stands, and will say so."
            .to_string()
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

    /// The caller is told what `--diff` actually captures. A description hardcoded to the
    /// working-tree modes told a `--diff main...HEAD` caller that untracked files and
    /// uncommitted work were included, which is exactly backwards for a range.
    #[test]
    fn review_tool_description_matches_the_configured_diff_mode() {
        let describe = |args: &[&str]| {
            let mut all: Vec<String> = vec!["--reviewer".into(), "claude".into()];
            all.extend(args.iter().map(|a| (*a).to_string()));
            let app = Arc::new(App::new(Config::from_args(&all).expect("config")));
            let response = handle_sync(&app, "tools/list", &Value::Null, &json!(1));
            response["result"]["tools"][0]["description"]
                .as_str()
                .expect("description")
                .to_string()
        };

        let ranged = describe(&["--diff", "main...HEAD"]);
        assert!(ranged.contains("`git diff main...HEAD`"), "{ranged}");
        assert!(!ranged.contains("untracked files, and hands"), "{ranged}");
        assert!(
            ranged.contains("commit what you want reviewed first"),
            "{ranged}"
        );

        let staged = describe(&["--diff", "staged"]);
        assert!(staged.contains("`git diff --cached`"), "{staged}");
        assert!(staged.contains("staged work only"), "{staged}");

        // The default for a shell-less reviewer still describes the working tree.
        let auto = describe(&[]);
        assert!(auto.contains("the contents of untracked files"), "{auto}");
        assert!(auto.contains("covers uncommitted work"), "{auto}");

        // A reviewer with a shell fetches its own, and is told so instead.
        let shelled = describe(&["--tools", "Read,Grep,Glob,Bash"]);
        assert!(
            shelled.contains("can inspect the change history itself"),
            "{shelled}"
        );
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

    fn pending() -> Pending {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn a_numeric_and_a_string_request_id_are_different_requests() {
        assert_ne!(request_key(&json!(1)), request_key(&json!("1")));
    }

    #[test]
    fn cancelling_an_in_flight_request_flags_it_and_drops_it_from_the_map() {
        let app = app();
        let pending = pending();
        let request = Arc::new(RequestCancel::new());
        request.attach_review("rv-1-1");
        pending
            .lock()
            .unwrap()
            .insert(request_key(&json!(7)), Arc::clone(&request));

        handle_cancellation(&app, &pending, &json!({"requestId": 7, "reason": "user"}));

        assert!(!request.try_claim_response(), "no response may be sent");
        // Dropped here so a duplicate notification cannot cancel anything twice, and so
        // the handler thread's own removal is a no-op.
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelling_a_request_stops_the_review_it_is_bound_to() {
        use std::sync::atomic::Ordering;

        let app = app();
        let (review_id, reviewer_cancel) = app
            .registry()
            .try_start("default", 1, false)
            .expect("start");

        let request = Arc::new(RequestCancel::new());
        request.attach_review(&review_id);
        let pending = pending();
        pending
            .lock()
            .unwrap()
            .insert(request_key(&json!(9)), Arc::clone(&request));

        handle_cancellation(&app, &pending, &json!({"requestId": 9, "reason": "user"}));

        // The half that costs money: the flag reviewer::run polls, not merely the
        // request's own. Without it the child keeps working to its 900s budget.
        assert!(reviewer_cancel.load(Ordering::SeqCst));
        assert!(!request.try_claim_response());
    }

    #[test]
    fn a_cancellation_mid_poll_ends_the_wait_rather_than_parking_the_thread() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let app = app();
        let (review_id, reviewer_cancel) = app
            .registry()
            .try_start("default", 1, false)
            .expect("start");

        let request = Arc::new(RequestCancel::new());
        let pending = pending();
        pending
            .lock()
            .unwrap()
            .insert(request_key(&json!(9)), Arc::clone(&request));

        // Stands in for the review worker: watches its cancel flag the way reviewer::run
        // does, and records a terminal state once it is set. That `finish` is what wakes
        // the poll, so the test exercises the real chain rather than shortcutting it.
        let worker = {
            let app = Arc::clone(&app);
            let review_id = review_id.clone();
            std::thread::spawn(move || {
                // Bounded: `cargo test` has no per-test timeout, so an unbounded spin
                // would turn a broken invariant into a silent CI hang instead of a
                // failure anyone can read.
                let give_up = std::time::Instant::now() + Duration::from_secs(10);
                while !reviewer_cancel.load(Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < give_up,
                        "the reviewer's cancel flag was never set"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                app.registry().finish(
                    &review_id,
                    crate::registry::Outcome::failed(crate::errors::cancelled()),
                );
            })
        };

        // A budget far longer than this test can take: if the cancellation fails to end
        // the wait, this hangs rather than quietly passing.
        let poller = {
            let app = Arc::clone(&app);
            let request = Arc::clone(&request);
            let args = json!({"review_id": review_id, "wait_seconds": 300});
            std::thread::spawn(move || {
                app.review_result(&args, &request)
                    .err()
                    .map(|failure| failure.code)
            })
        };

        std::thread::sleep(Duration::from_millis(100));
        handle_cancellation(&app, &pending, &json!({"requestId": 9}));

        worker.join().expect("worker");
        assert_eq!(poller.join().expect("poller"), Some("CANCELLED"));
        // Cancelled, so the handler thread would send nothing.
        assert!(!request.try_claim_response());
    }

    #[test]
    fn a_cancellation_for_an_unknown_request_is_ignored() {
        let app = app();
        let pending = pending();
        let request = Arc::new(RequestCancel::new());
        pending
            .lock()
            .unwrap()
            .insert(request_key(&json!(7)), Arc::clone(&request));

        // Racing a response that already went out, or a mismatched id: neither may take
        // down an unrelated in-flight call.
        handle_cancellation(&app, &pending, &json!({"requestId": "7"}));
        handle_cancellation(&app, &pending, &json!({"reason": "no id at all"}));

        assert!(request.try_claim_response(), "it may still be answered");
        assert_eq!(pending.lock().unwrap().len(), 1);
    }

    #[test]
    fn unknown_tool_is_an_is_error_result_not_a_protocol_error() {
        let result = dispatch_tool(
            &app(),
            &json!({"name": "nope", "arguments": {}}),
            &RequestCancel::new(),
        );
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
            &RequestCancel::new(),
        );
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("BAD_REQUEST"));
    }

    /// Collects what a response path writes, standing in for stdout.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Recorder {
        fn writer(&self) -> Writer {
            Arc::new(Mutex::new(self.clone()))
        }

        fn responses(&self) -> Vec<Value> {
            let bytes = self.0.lock().unwrap().clone();
            // Newline-delimited JSON is the whole transport, and this is the only test
            // that reads bytes back, so the framing is checked rather than assumed. A
            // message never contains a bare newline: serde escapes them inside strings.
            assert!(
                bytes.is_empty() || bytes.ends_with(b"\n"),
                "every response must be newline-terminated"
            );
            String::from_utf8(bytes)
                .expect("utf-8")
                .lines()
                .map(|l| serde_json::from_str(l).expect("each line is one JSON message"))
                .collect()
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The regression this guards: a `tools/call` whose handler thread will not start used
    /// to be logged and dropped, leaving the client to discover it by its own tool timeout.
    ///
    /// The failure is provoked rather than simulated. A `Builder` asking for a `usize::MAX`
    /// stack is rejected by the OS outright -- on Windows, `os error 87` -- so `spawn`
    /// returns `Err` deterministically, with no threads exhausted and nothing left running.
    #[test]
    fn a_handler_that_cannot_be_spawned_is_answered_rather_than_left_hanging() {
        let recorder = Recorder::default();
        let pending = pending();
        // What the OS will say, asked of it rather than hardcoded: CI is Windows-only, but
        // an assertion on "os error 87" would still be pinning one of several ways
        // CreateThread can refuse.
        let expected = std::thread::Builder::new()
            .stack_size(usize::MAX)
            .spawn(|| {})
            .expect_err("an impossible stack size must be refused")
            .to_string();

        let handle = start_tool_call(
            std::thread::Builder::new().stack_size(usize::MAX),
            &app(),
            &recorder.writer(),
            &pending,
            json!("req-11"),
            json!({"name": "cross_model_review_result", "arguments": {}}),
        );

        assert!(handle.is_none(), "the guard stack size must defeat spawn");
        // Nothing is left behind to be cancelled or joined.
        assert!(pending.lock().unwrap().is_empty());

        let responses = recorder.responses();
        assert_eq!(responses.len(), 1, "exactly one response, and not none");
        let response = &responses[0];
        // Addressed to the request that would have gone unanswered, or the client cannot
        // match it -- and as the same JSON type it was sent as.
        assert_eq!(response["id"], "req-11");
        assert_eq!(response["jsonrpc"], "2.0");
        // An isError result, not a JSON-RPC error: see the response builder's comment.
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);

        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("INTERNAL_ERROR"));
        // The tool is named, and the OS error carried through: stderr is the only other
        // place either lands, and the caller cannot read stderr.
        assert!(text.contains("cross_model_review_result"));
        assert!(text.contains(&expected), "want {expected:?} in: {text}");
        // The claim the shared stop-and-escalate text would have made here is false: this
        // call never reached the reviewer, so a review already running is untouched. Nor
        // may it presume one exists -- the same call from `cross_model_review` would be
        // the first in the flow, with no review to collect.
        assert!(!text.contains("The external review did not run"));
        assert!(text.contains("any review already in progress is unaffected"));
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
        let result = dispatch_tool(
            &app,
            &json!({"name": "cross_model_review_status"}),
            &RequestCancel::new(),
        );
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ready:         NO"));
        assert!(text.contains("CLI_NOT_FOUND"));
    }
}
