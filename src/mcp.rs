//! MCP over stdio: newline-delimited JSON-RPC 2.0.
//!
//! Hand-rolled rather than pulled from a crate. The protocol surface we need is four
//! methods wide, and keeping the dependency list at serde is what lets this ship as a
//! single small executable with nothing to install.
//!
//! stdout carries protocol traffic only. Anything diagnostic goes to stderr.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::cancel::{CancelAction, RequestCancel};
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

/// Progress updates are intentionally sparse: enough to prove the wait is live without
/// turning a twenty-minute review into a flood of protocol messages.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

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

    drain_in_flight(&app, in_flight);

    eprintln!("cross-review: stdin closed, shutting down");
}

/// Releases parked waiters, then joins every handler thread.
///
/// Split out of `serve` for the same reason `start_tool_call` was: `serve` owns stdin and
/// cannot be driven from a test, and the order of these two steps is the whole of the fix
/// for a shutdown that used to wait out a long poll. Releasing after the join would
/// reinstate that silently -- every registry-level test would still pass -- so the ordering
/// needs a test of its own, and a test needs a seam.
fn drain_in_flight(app: &Arc<App>, in_flight: Vec<std::thread::JoinHandle<()>>) {
    // A `cross_model_review_result` parked in `Registry::wait` has no other way to learn
    // stdin has closed, so its full collect wait budget -- the cap derived from the review
    // timeout, up to tens of minutes at defaults -- would hold the join below for the rest
    // of it and then write to a stdout nobody is reading.
    app.begin_shutdown();

    let unfinished = in_flight.iter().filter(|h| !h.is_finished()).count();
    if unfinished > 0 {
        eprintln!("cross-review: stdin closed, finishing {unfinished} in-flight tool call(s)");
    }
    for handle in in_flight {
        let _ = handle.join();
    }
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
            let progress = ProgressReporter::start(&app, &writer, &params, &request);
            let result = dispatch_tool(&app, &params, &request);
            // Stop and join before sending the response. The MCP progress contract says
            // notifications stop after completion; joining closes the race rather than
            // merely making a late notification unlikely.
            drop(progress);
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
            // killed either way, since `attach_owned`/`attach_wait` only run inside
            // `dispatch_tool`.
            send(writer, handler_thread_unavailable_response(&id, &tool, &e));
            None
        }
    }
}

/// Periodic MCP `notifications/progress` for a long-running result call.
///
/// MCP makes this opt-in: the client places an opaque `progressToken` in request metadata.
/// When it does not, this object is never created and the transport stays exactly as it was.
struct ProgressReporter {
    done: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProgressReporter {
    fn start(
        app: &Arc<App>,
        writer: &Writer,
        params: &Value,
        request: &Arc<RequestCancel>,
    ) -> Option<Self> {
        Self::start_with_interval(app, writer, params, request, PROGRESS_INTERVAL)
    }

    fn start_with_interval(
        app: &Arc<App>,
        writer: &Writer,
        params: &Value,
        request: &Arc<RequestCancel>,
        interval: Duration,
    ) -> Option<Self> {
        // Both result tools emit progress while their blocking wait is open: a consult can take a
        // few minutes too, and the progress snapshot (phase, elapsed, liveness) is kind-agnostic. The
        // kind selects which job a `session`-addressed progress request resolves, so it cannot report
        // the other kind's job under a reused name (consult f2).
        let expected_kind = match params.get("name").and_then(Value::as_str) {
            Some("cross_model_review_result") => crate::registry::JobKind::Review,
            Some("cross_model_consult_result") => crate::registry::JobKind::Consult,
            _ => return None,
        };
        let token = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .filter(|token| token.is_string() || token.is_number())?
            .clone();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        // Do not announce a request the client has already abandoned.
        if request.is_cancelled() {
            return None;
        }
        // Nor a wait that the result call is about to reject, or a review that already
        // finished between calls.
        let initial = app.review_progress(&args, expected_kind)?;

        send_progress(writer, &token, 0, initial);

        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_done = Arc::clone(&done);
        let app = Arc::clone(app);
        let writer = Arc::clone(writer);
        let request = Arc::clone(request);
        let handle = std::thread::Builder::new()
            .name("review-progress".to_string())
            .spawn(move || {
                let (lock, changed) = &*thread_done;
                let mut complete = lock.lock().unwrap_or_else(|e| e.into_inner());
                let mut progress = 0u64;
                loop {
                    let (next, waited) = changed
                        .wait_timeout(complete, interval)
                        .unwrap_or_else(|e| e.into_inner());
                    complete = next;
                    if *complete {
                        break;
                    }
                    if !waited.timed_out() {
                        continue;
                    }
                    if request.is_cancelled() {
                        break;
                    }
                    let Some(message) = app.review_progress(&args, expected_kind) else {
                        break;
                    };
                    progress = progress.saturating_add(1);
                    // Sent while holding the completion lock. `Drop` cannot mark the
                    // reporter done until this finishes, which guarantees that no
                    // notification can slip out after the final tool response.
                    send_progress(&writer, &token, progress, message);
                }
            })
            .ok()?;

        Some(Self {
            done,
            handle: Some(handle),
        })
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        let (lock, changed) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        changed.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn send_progress(writer: &Writer, token: &Value, progress: u64, message: String) {
    // `message` joined notifications/progress in MCP 2025-03-26. It is sent to the
    // 2024-11-05 fallback too as an additive field on purpose: MCP request/notification
    // parameter objects are extensible, and omitting it would reduce a progress update to
    // an unexplained counter for old clients. Clients that do not know the field ignore it.
    send(
        writer,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                "progress": progress,
                "message": message,
            }
        }),
    );
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

/// Honour `notifications/cancelled`: send no response, and act on the review per the request's
/// ownership.
///
/// Suppressing the response is what the spec asks for, in every case. What happens to the review
/// is not uniform (see `CancelAction`): a cancelled start call is a `Kill` — its `review_id` was
/// never delivered, so the reviewer is stopped to save the money a result nobody can collect would
/// otherwise spend — while a cancelled result poll is a `Detach` — the caller holds the id and can
/// still collect, so the reviewer is left running and only the parked wait is woken. `Nothing`
/// covers a request with no review bound or one already answered.
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

    // stderr is the only visibility this path has, so the outcomes are told apart. `Kill` is a
    // cancelled start call, whose review_id was never delivered and so can never be collected;
    // `Detach` is a cancelled result poll, which leaves the review running and collectible and only
    // wakes the parked wait; `Nothing` is a request with no review bound (or one already answered).
    match entry.cancel() {
        CancelAction::Nothing => {
            eprintln!("cross-review: request {shown} cancelled ({reason})")
        }
        CancelAction::Kill(review_id) if app.cancel_review(&review_id) => eprintln!(
            "cross-review: request {shown} cancelled ({reason}); stopped review {review_id}"
        ),
        CancelAction::Kill(review_id) => eprintln!(
            "cross-review: request {shown} cancelled ({reason}); review {review_id} had already \
             finished"
        ),
        CancelAction::Detach => {
            // The review is left running; only wake the parked poll so its handler thread does not
            // linger for the rest of the (now much larger) wait budget. The request's cancelled
            // flag was set by `entry.cancel()` above, before this wake, so the state lock inside
            // `wake` orders the two and the waiter cannot miss it.
            app.wake_waiters();
            eprintln!(
                "cross-review: result poll {shown} cancelled ({reason}); detached the wait, review \
                 left running"
            );
        }
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

    // `cross_model_review_result` is handled on its own: it carries the machine-readable envelope
    // as `structuredContent` (MCP 2025-06-18) alongside the `_OUT` block already in the text.
    // `review_result_both` resolves the target and waits *once*, then renders both channels from the
    // same final snapshot, so they can never describe different reviews or different states.
    if name == "cross_model_review_result" {
        return match app.review_result_both(&args, request) {
            Ok((text, structured)) => text_result_with_structured(text, structured),
            Err(failure) => text_result(failure.render_for_agent(), true),
        };
    }
    // The consult's collect, on its own for the same reason: it carries a structured value alongside
    // the text, and both come from one blocking wait so they cannot describe different states.
    if name == "cross_model_consult_result" {
        return match app.consult_result_both(&args, request) {
            Ok((text, structured)) => text_result_with_structured(text, structured),
            Err(failure) => text_result(failure.render_for_agent(), true),
        };
    }

    let outcome = match name {
        "cross_model_review" => app.start_review(&args, request),
        "cross_model_consult" => app.start_consult(&args, request),
        "cross_model_review_status" => Ok(app.status()),
        "cross_model_review_cancel" => app.cancel(&args),
        "cross_model_setup_profile" => app.setup_profile(&args, request),
        other => {
            return text_result(
                format!(
                    "Unknown tool '{other}'. This server provides cross_model_review, \
                     cross_model_consult, cross_model_review_result, cross_model_consult_result, \
                     cross_model_review_status, cross_model_review_cancel, and \
                     cross_model_setup_profile."
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

/// A successful tool result carrying the text channel plus, when present, the `structuredContent`
/// sibling (MCP 2025-06-18). A client that ignores `structuredContent` is unaffected — the same
/// envelope is in the text `_OUT` block.
fn text_result_with_structured(text: String, structured: Option<Value>) -> Value {
    let mut v = json!({"content": [{"type": "text", "text": text}], "isError": false});
    if let Some(sc) = structured {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("structuredContent".to_string(), sc);
        }
    }
    v
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

/// The distinct reviewer CLI names across the chain, as a human phrase ("codex", "claude", or
/// "codex and claude"). Order-preserving and deduplicated, so a two-Codex chain still reads
/// "codex".
fn distinct_reviewer_clis(cfg: &crate::config::Config) -> String {
    let mut names: Vec<&str> = Vec::new();
    for spec in &cfg.reviewers {
        let name = spec.reviewer.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    match names.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [first, rest @ ..] => format!("{first} and {}", rest.join(" and ")),
    }
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
         cross_model_review_result waits for and returns the review, with periodic progress \
         notifications when the client supports them. In this project's usage, reviews commonly \
         take at least five minutes; complex changes can take 20 minutes or longer, so a running \
         status is expected. Sessions are named, so \
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
    // `supplies_change` is asked first because the two are not exclusive: `--diff HEAD` with
    // a shelled reviewer captures *and* hands over a change, which the shell branch would
    // have described as "inspect the change history itself" while a diff sat in the prompt.
    // Said once, and only as strongly as the mechanism behind it. Codex's shell runs under a
    // sandbox whose write refusals are the OS's -- verified; Claude's is an opt-in
    // allow-list, which `README.md` shows cannot express "read-only" at all. Calling both
    // read-only would be the kind of unearned claim this project spends the README avoiding.
    //
    // "Read-only" is a claim about writes and nothing more. Codex's *reads* are not confined
    // to the project, and no way to confine them was found (`README.md`), so this clause must
    // not grow into one that suggests otherwise.
    // `.first()` rather than `primary()` throughout: a defensively-constructed empty chain (which
    // `from_args` never produces) must render safe metadata for `tools/list` rather than panic --
    // its reviews are refused in-band with INVALID_REVIEWER_CHAIN.
    let shell_clause = match cfg.reviewers.first().map(|s| s.reviewer) {
        Some(crate::config::ReviewerKind::Codex) => {
            "It has a read-only shell; its non-interactive command policy may refuse some forms"
        }
        Some(crate::config::ReviewerKind::Claude) => {
            "It has a shell, because one was enabled explicitly -- its allow-list is a soft \
             boundary rather than a read-only guarantee"
        }
        None => "",
    };

    let read_commands = cfg.vcs.read_commands_phrase();
    let cli = cfg.vcs.cli();
    let primary_isolated_codex = cfg
        .reviewers
        .first()
        .is_some_and(|spec| spec.reviewer == crate::config::ReviewerKind::Codex)
        && cfg.isolate_reviewer;
    let access = if cfg.reviewers.is_empty() {
        "The reviewer chain is misconfigured, so every review is refused until it is fixed. Run \
         cross_model_review_status for the specifics."
            .to_string()
    } else if primary_isolated_codex {
        let (captures, caveat) = cfg.capture_caller_summary();
        // Git derives the change live through repository_diff; Perforce pages an immutable
        // pre-captured changelist snapshot through repository_change (retire-capture-modes).
        let delivery = match cfg.vcs {
            crate::config::Vcs::Git => "on demand",
            crate::config::Vcs::Perforce => {
                "as an immutable snapshot the reviewer pages through repository_change"
            }
        };
        format!(
            "The reviewer receives a required, read-only repository evidence service with \
             bounded scope/list/search/read/diff/history/revision tools. Its process shell \
             runs from a sterile non-repository directory and is not the normal repository \
             interface. When the working root is a {vcs} repository, it obtains {captures} \
             {delivery}. Do not paste a diff or whole files into 'instructions' -- describe the \
             intent and what you want scrutinised instead. {caveat}",
            vcs = cfg.vcs.name(),
        )
    } else if cfg.reviewer_has_shell() && !cfg.supplies_change() {
        format!(
            "The reviewer can read and search files in this repository. {shell_clause}, so it \
             can run {read_commands} and inspect the change history itself. You do not need to \
             paste code. Describe what changed and what you want scrutinised."
        )
    } else if cfg.supplies_change() {
        // Worth stating positively. The caller pastes a diff because it believes it has
        // to; left to infer, it will keep spending its own context on one this server
        // already fetched.
        // Qualified rather than flat: `supplies_change` is the configured intent, and
        // whether a change actually arrives depends on the working root being a repository
        // of that kind, which is only known at capture time. The reviewer is told the
        // runtime answer; the caller can only be told the intent, so it must not be
        // promised more than that.
        // What is captured is what the backend's spec selects, so the description asks
        // `capture_caller_summary` rather than restating one of them.
        let (captures, caveat) = cfg.capture_caller_summary();
        // The shell clause is the one part that cannot be stated unconditionally here: a
        // capture is configured for both kinds of reviewer, but only one of them lacks a
        // shell, and telling the caller a shelled reviewer has none would be a plain lie.
        let shell = if cfg.reviewer_has_shell() {
            format!("{shell_clause}, and it is also handed the change directly")
        } else {
            "It has NO shell of its own, but it does not need one for the change".to_string()
        };
        format!(
            "The reviewer can read and search files in this repository, so you do not need to \
             paste whole files. {shell}: when the working root is a {vcs} repository, this \
             server captures {captures}, and hands them to the reviewer with your request. Do \
             not paste a diff into 'instructions' -- describe the intent of the change and what \
             you want scrutinised instead. {caveat}",
            vcs = cfg.vcs.name(),
        )
    } else if cfg.vcs == crate::config::Vcs::Git {
        // Git derives the change live through repository_diff (retire-capture-modes); there is no
        // static capture to describe and nothing to paste. A git review needs the evidence path,
        // so an ambient (unprofiled) Claude reviewer is refused before it runs.
        "The reviewer can read and search files in this repository, and derives the change under \
         review live through the read-only evidence service's repository_diff tool -- it diffs the \
         working tree against the branch's fork point on demand, including untracked files. You do \
         not need to paste a diff or describe the change. (A git review requires the evidence path; \
         an ambient Claude reviewer without a pinned --claude-profile is refused.)"
            .to_string()
    } else {
        format!(
            "The reviewer can read and search files in this repository, so you do not need to \
             paste whole files. It has NO shell, so it cannot run `{cli}` and cannot obtain a \
             diff. If the review depends on what changed rather than on the current state of \
             the code, include the diff or a precise description of the change in \
             'instructions' -- otherwise the reviewer can only judge the code as it now \
             stands, and will say so."
        )
    };
    // The capability text above describes the primary, which is what runs first. When a
    // fallback chain is configured, say so honestly rather than implying the primary is the
    // only reviewer: a fallback may be a different model or family with different shell/capture
    // behaviour. See docs/reviewer-fallback-chain.md.
    let access = if cfg.reviewers.len() > 1 {
        let fallbacks: Vec<String> = cfg.reviewers[1..].iter().map(|s| s.describe()).collect();
        format!(
            "{access}\n\nIf the primary reviewer reports a rate or usage limit, this server \
             automatically falls back, in order, to: {}. A fallback may be a different model or \
             family, so its shell and capture behaviour can differ from what is described above.",
            fallbacks.join("; ")
        )
    } else {
        access
    };
    let caller_hint = match cfg.reviewers.first().map(|s| s.reviewer) {
        Some(crate::config::ReviewerKind::Claude) => {
            "The reviewer is a Claude model, so this is most useful when you are not one."
        }
        Some(crate::config::ReviewerKind::Codex) => {
            "The reviewer is an OpenAI model, so this is most useful when you are not one."
        }
        None => "",
    };

    // Names the whole chain, not just the primary: with a fallback configured the status
    // reflects every reviewer, so a description naming one CLI would understate what is checked.
    let status_description = format!(
        "Check that the review tool is usable: whether the {clis} CLI is installed, whether it is \
         signed in, which model and effort are pinned (for every entry in the reviewer chain), \
         and which review sessions exist. Costs nothing and calls no model. Worth calling first \
         if a review has just failed, or to confirm the setup before relying on it.",
        clis = distinct_reviewer_clis(cfg)
    );

    let mut tools = vec![
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
            "name": "cross_model_consult",
            "description": format!(
                "Ask {reviewer} an informal question about your work -- a lightweight second pair of \
                 eyes, not a gated review. Use it for \"does this direction look right?\", \"where is \
                 X handled in this code?\", or \"am I missing a simpler approach?\". The reviewer \
                 reads the repository through a read-only evidence service and answers in prose: \
                 there is no findings ledger, no verdict, and nothing to converge on.\n\n\
                 Returns immediately with a review_id; collect the answer with \
                 cross_model_consult_result. Reuse the same 'session' to ask a follow-up with the \
                 earlier exchange still in context.\n\n\
                 For **git** the change under review is always available live through the \
                 repository_diff evidence tool (base 'branch-base', head 'worktree'), so \
                 include_change has no effect -- ask about the change directly. For **Perforce**, \
                 the reviewer reads the tree by default; pass include_change: true to also capture \
                 and show the changelist(s) you name in 'change'. A consult certifies nothing \
                 either way.\n\n\
                 It requires the evidence service, so it runs only on a reviewer that provides one \
                 (Codex, or a profile-pinned shell-less Claude); otherwise it fails with \
                 EVIDENCE_UNAVAILABLE. If it fails, the consult did not happen -- tell the user what \
                 the error says rather than answering yourself."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description":
                            "What you want a second opinion on, or want found in the code. Passed to \
                             the reviewer verbatim. Say why you are asking, not just what to look at."
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
                            "Name for this consultation, chosen by you. Reusing a name continues the \
                             same conversation for a follow-up. Defaults to 'default'."
                    },
                    "fresh": {
                        "type": "boolean",
                        "description":
                            "Start a new conversation even if this session name already exists. \
                             Defaults to false."
                    },
                    "include_change": {
                        "type": "boolean",
                        "description":
                            "Perforce only: capture and show the reviewer the changelist(s) named \
                             in 'change', not just the tree (defaults to false, tree-only). For git \
                             it has no effect -- the change is always available live through the \
                             repository_diff evidence tool. Fixed for the session: a resume must \
                             pass the same value (use fresh:true to change it)."
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "cross_model_consult_result",
            "description": format!(
                "Wait for and return the answer from a cross_model_consult. Blocks until the consult \
                 is done: omit wait_seconds to wait to completion in one call. When the MCP client \
                 supplies a progress token, it emits live phase and elapsed-time updates during the \
                 wait. A consult usually takes a few minutes; a deeper question can take longer.\n\n\
                 If the wait_seconds budget elapses before it finishes it returns status=running; \
                 just call again with the same review_id. Abandoning this call does NOT cancel the \
                 consult -- it keeps running and stays collectible by review_id. Use \
                 cross_model_review_cancel to actually stop it.\n\n\
                 The completed result is the reviewer's prose answer (an `answer` field in the \
                 structured content), with the reviewer, cost, and any refused commands -- there is \
                 no verdict or findings list. A review_id from cross_model_review must be collected \
                 with cross_model_review_result, not here (and vice versa)."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "review_id": {
                        "type": "string",
                        "description": "The review_id returned by cross_model_consult. Preferred."
                    },
                    "session": {
                        "type": "string",
                        "description":
                            "Session name, as an alternative to review_id. Returns that session's \
                             most recent consult."
                    },
                    "wait_seconds": {
                        "type": "integer",
                        "description":
                            "How long to wait before returning, in seconds. Omit to block until the \
                             consult is done; pass 0 for an immediate snapshot."
                    }
                },
                "additionalProperties": false
            },
            // The consult envelope this tool returns as `structuredContent`: a running/completed shape
            // with a prose `answer` and the run facts, but no verdict or findings. Advertised so a
            // client can validate the structured channel, as the review result does (consult f6).
            "outputSchema": crate::tools::consult_output_schema()
        }),
        json!({
            "name": "cross_model_review_result",
            "description": format!(
                "Wait for and return the review from {reviewer}. Blocks until the review is done: \
                 omit wait_seconds to wait to completion in one call, so no poll loop is needed. \
                 When the MCP client supplies a progress token, it emits live phase, elapsed-time, \
                 reviewer liveness, and output-activity updates during the wait. In this project's \
                 usage, reviews commonly take at least five minutes, and complex changes can take \
                 20 minutes or longer.\n\n\
                 If the wait_seconds budget elapses before the review finishes it returns \
                 status=running; that is normal, just call again with the same review_id. \
                 Abandoning this call does NOT cancel the review: the reviewer keeps running and \
                 the result stays collectible by review_id, so if your client's own tool timeout \
                 cuts the call short (you get a client-side timeout rather than a result), simply \
                 call again with the same review_id. Use cross_model_review_cancel to actually stop \
                 a reviewer.\n\n\
                 A completed result carries an `outcome` field saying what to do next, and that is \
                 the field to switch on: `converged` (stop), `changes_requested` (act on \
                 `findings`, then re-review the same session), `escalate` (the reviewer's own \
                 judgement needs a person), or `rebaseline` (this session cannot continue -- a \
                 human decides, then starts a fresh review carrying the still-open findings). When \
                 `structured` is false the reviewer produced no machine-readable record this turn: \
                 `findings` is empty because there is nothing in it, not because there was nothing \
                 wrong.\n\n\
                 `review_prose` carries what the reviewer said outside its findings list -- its \
                 reasoning, and why a finding is still open -- on every turn that ran, whatever the \
                 outcome; it is null only when no reviewer ran. Read it: `findings` is the machine \
                 record, not the whole review. Beside it, `captured` says what change the reviewer \
                 was actually shown, `denial_count` how many commands it was refused (a lower bound \
                 when `denial_count_is_floor` is true), `warnings` everything that qualifies the \
                 turn, and `resumable` whether this session can be continued. A review run on a \
                 truncated capture, or one that spent its turn on blocked commands, is thinner than \
                 it looks -- those fields are how you tell.\n\n\
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
                        "maximum": cfg.max_wait_secs(),
                        "description": format!(
                            "How long to wait for the review before returning, in seconds. Omit it \
                             to block until the review is done (the default is the {max}s cap, which \
                             covers a whole review), or pass 0 for an immediate snapshot. Capped at \
                             {max}, which tracks the review budget so a single call can collect a \
                             20-minute review. Progress notifications make the long wait observable. \
                             If this budget elapses before the review finishes, the call returns \
                             status=running; call again with the same review_id. (Separately, if \
                             your client's own tool timeout is shorter and cuts the call short, you \
                             get a client-side timeout rather than a status=running result -- the \
                             review keeps running regardless, so just call again with the same \
                             review_id.)",
                            max = cfg.max_wait_secs()
                        )
                    }
                },
                "additionalProperties": false
            },
            // The machine-readable envelope this tool also returns as `structuredContent`
            // (MCP 2025-06-18): a discriminated running/completed union. Advertised so a client can
            // validate the structured channel; the same envelope is in the text `_OUT` block too.
            "outputSchema": crate::findings::output_schema()
        }),
        json!({
            "name": "cross_model_review_status",
            "description": status_description,
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "cross_model_review_cancel",
            "description":
                "Stop a review that is still running, and free the reviewer. This is the only \
                 operation that stops a running reviewer: abandoning a cross_model_review_result \
                 poll only detaches the wait and leaves the reviewer running. Use it when the work \
                 it was reviewing has changed underneath it, or the user has moved on. The review \
                 session itself survives, so a later cross_model_review with the same session name \
                 still resumes.",
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
        json!({
            "name": "cross_model_setup_profile",
            "description":
                "Authorize THIS repository to run reviews under a reviewer account profile — a \
                 dedicated config home that fixes which account a review bills, regardless of what \
                 the desktop app is signed into. This is a one-time, human-approved step: it opens a \
                 localhost page in your browser and blocks until you click Approve, so a person — not \
                 an agent — decides. By default it only authorizes an ALREADY-provisioned, signed-in \
                 profile (no login). Pass \"login\": true to provision one: it drives the vendor login \
                 in your browser and BLOCKS for several minutes while you sign in (for some reviewers a \
                 local page asks you to paste a code shown in the browser), then authorizes the \
                 result — this both first-provisions a new home and re-signs-in (switches the account \
                 of) an existing one. Nothing is authorized unless you approve, and authorization is \
                 scoped to the directory this server was launched from.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "reviewer": {
                        "type": "string",
                        "enum": ["codex", "claude"],
                        "description": "Which reviewer family the profile is for."
                    },
                    "profile": {
                        "type": "string",
                        "description":
                            "A named profile (letters, digits, '.', '_', '-'), resolved under the \
                             fixed profile root. Provide this OR 'home', not both."
                    },
                    "home": {
                        "type": "string",
                        "description":
                            "An explicit absolute config-home path (the escape hatch), instead of a \
                             named profile. Provide this OR 'profile', not both."
                    },
                    "login": {
                        "type": "boolean",
                        "description":
                            "Run the vendor login to provision (first-time) or re-sign-in (switch \
                             account) the profile home. The call BLOCKS for minutes while a human \
                             completes OAuth in the browser. Default false (authorize an existing, \
                             signed-in home only)."
                    }
                },
                "required": ["reviewer"],
                "additionalProperties": false
            }
        }),
    ];

    // The Perforce backend names its changelists per call, so those inputs exist only for a
    // Perforce-configured server. A git server never advertises them, and rejects them if
    // passed (additionalProperties is false), so a caller cannot silently misuse them.
    if cfg.vcs == crate::config::Vcs::Perforce {
        if let Some(props) = tools[0]["inputSchema"]["properties"].as_object_mut() {
            props.insert(
                "change".to_string(),
                json!({
                    "type": ["string", "array"],
                    "items": {"type": ["string", "integer"]},
                    "description":
                        "Required. The Perforce changelist number(s) to review: a single number \
                         (\"43650\"), a comma-separated string (\"43650,43651\"), or an array \
                         ([\"43650\",\"43651\"]). Submitted and pending changelists are both \
                         supported; the server captures each and hands the reviewer the diff, \
                         file list, added-file contents and description. A review session is \
                         bound to its changelist set -- reuse the same set to re-review, or pass \
                         fresh:true to switch to a different one."
                }),
            );
            props.insert(
                "include_shelved".to_string(),
                json!({
                    "type": "boolean",
                    "description":
                        "Optional, default false. When a pending changelist has nothing open in \
                         this workspace (it is shelved, or belongs to another client), pull its \
                         shelved snapshot with `p4 describe -S` instead of reporting no diff. Off \
                         by default because shelved files are often work-in-progress checkpoints \
                         rather than the change to review."
                }),
            );
        }
        // `change` is required for a Perforce review (start_review refuses a call without it),
        // so the schema says so too rather than describing it as required in prose alone.
        if let Some(required) = tools[0]["inputSchema"]["required"].as_array_mut() {
            required.push(json!("change"));
        }

        // The consult also takes the Perforce inputs, but only when include_change: true, so its
        // `change` is *conditionally* required: rejected when include_change is false, required when
        // true. That cannot be expressed as a flat `required` entry, so it is NOT pushed there (the
        // runtime is the real validator) and the description states the condition. Matched by name,
        // not index, so it survives a reordering of the tool list (consult include_change f6). The
        // review string above starts "Required." and describes a review, so it is not reused here.
        if let Some(consult) = tools
            .iter_mut()
            .find(|t| t["name"] == json!("cross_model_consult"))
        {
            if let Some(props) = consult["inputSchema"]["properties"].as_object_mut() {
                props.insert(
                    "change".to_string(),
                    json!({
                        "type": ["string", "array"],
                        "items": {"type": ["string", "integer"]},
                        "description":
                            "The Perforce changelist number(s) to include: a single number \
                             (\"43650\"), a comma-separated string (\"43650,43651\"), or an array \
                             ([\"43650\",\"43651\"]). Required only when include_change: true, and \
                             rejected when it is false (a tree-only consult names no changelist). \
                             Bound to the session -- reuse the same set to follow up, or pass \
                             fresh:true to switch."
                    }),
                );
                props.insert(
                    "include_shelved".to_string(),
                    json!({
                        "type": "boolean",
                        "description":
                            "Optional, default false. Only meaningful with include_change: true. \
                             When a pending changelist has nothing open in this workspace (it is \
                             shelved, or belongs to another client), pull its shelved snapshot with \
                             `p4 describe -S` instead of reporting no diff."
                    }),
                );
            }
        }
    }

    // The review "level" preset is advertised only when the primary entry declares at least one
    // level. A server with no levels behaves exactly as before: no `level` property, and
    // additionalProperties:false rejects a stray one. The advertised menu is the primary's — the
    // runtime (start_review) is the real validator and also handles a gate-selected fallback that
    // lacks a level. See `docs/review-levels-plan.md` §3.
    let primary = cfg.primary();
    if !primary.levels.is_empty() {
        let menu = primary
            .levels
            .iter()
            .map(|(name, lv)| format!("{name} = model={}/effort={}", lv.model, lv.effort))
            .collect::<Vec<_>>()
            .join("; ");
        let default_hint = match &primary.default_level {
            Some(d) => format!("Omit to use the default level '{d}'."),
            None => "Omit to use the reviewer's base model/effort.".to_string(),
        };
        let names = primary.level_names();
        let level_prop = json!({
            "type": "string",
            "enum": names,
            "description": format!(
                "Optional depth preset. Chosen at start and fixed for the session: a resume keeps \
                 the session's original level (pass fresh:true to change it). Levels: {menu}. \
                 {default_hint}"
            )
        });
        // Both start tools resolve `level` through the shared start(), so both advertise it. Matched
        // by name rather than index so the injection is robust to tool ordering (consult f4).
        for tool in tools.iter_mut() {
            let is_start = matches!(
                tool["name"].as_str(),
                Some("cross_model_review") | Some("cross_model_consult")
            );
            if is_start {
                if let Some(props) = tool["inputSchema"]["properties"].as_object_mut() {
                    props.insert("level".to_string(), level_prop.clone());
                }
            }
        }
    }

    tools
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
    fn the_wait_seconds_schema_maximum_tracks_a_non_default_timeout() {
        // The advertised cap must follow --timeout-seconds, so the schema and the runtime clamp in
        // review_result cannot silently disagree.
        let cfg = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--timeout-seconds".into(),
            "3600".into(),
        ])
        .expect("config");
        let expected = cfg.max_wait_secs();
        let app = Arc::new(App::new(cfg));

        let tools = tool_definitions(&app);
        let result_tool = tools
            .iter()
            .find(|t| t["name"] == "cross_model_review_result")
            .expect("cross_model_review_result tool");
        let maximum = result_tool["inputSchema"]["properties"]["wait_seconds"]["maximum"]
            .as_u64()
            .expect("wait_seconds.maximum is an integer");
        assert_eq!(
            maximum, expected,
            "the advertised cap must match the runtime cap"
        );
        assert!(maximum > 300, "the cap should exceed the old fixed 300");
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
    fn lists_exactly_the_seven_tools_with_valid_schemas() {
        let response = handle_sync(&app(), "tools/list", &Value::Null, &json!(2));
        let tools = response["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "cross_model_review",
                "cross_model_consult",
                "cross_model_consult_result",
                "cross_model_review_result",
                "cross_model_review_status",
                "cross_model_review_cancel",
                "cross_model_setup_profile"
            ]
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn perforce_server_exposes_change_inputs_and_git_does_not() {
        // The default app resolves to git (this repo has a .git entry): no changelist inputs.
        let git = handle_sync(&app(), "tools/list", &Value::Null, &json!(9));
        let git_props = &git["result"]["tools"][0]["inputSchema"]["properties"];
        assert!(git_props.get("change").is_none(), "{git_props}");
        assert!(git_props.get("include_shelved").is_none(), "{git_props}");

        // A Perforce-configured server advertises both, and still forbids anything else.
        let cfg = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--vcs".into(),
            "perforce".into(),
        ])
        .expect("cfg");
        let p4app = Arc::new(App::new(cfg));
        let p4 = handle_sync(&p4app, "tools/list", &Value::Null, &json!(9));
        let review = &p4["result"]["tools"][0]["inputSchema"];
        assert!(review["properties"].get("change").is_some(), "{review}");
        assert!(
            review["properties"].get("include_shelved").is_some(),
            "{review}"
        );
        assert_eq!(review["additionalProperties"], false);
        // instructions is always required; a Perforce server also requires `change`.
        let required = review["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "instructions"), "{review}");
        assert!(required.iter().any(|v| v == "change"), "{review}");
        // Array entries may be strings or integers, matching what start_review accepts.
        assert_eq!(
            review["properties"]["change"]["items"]["type"],
            json!(["string", "integer"])
        );
    }

    #[test]
    fn review_tool_requires_instructions_and_forbids_extra_properties() {
        let response = handle_sync(&app(), "tools/list", &Value::Null, &json!(2));
        let tool = &response["result"]["tools"][0];
        assert_eq!(tool["inputSchema"]["required"][0], "instructions");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn level_property_is_advertised_only_when_levels_are_configured() {
        // No levels: the property is absent, so additionalProperties:false rejects a stray `level`
        // and the schema reads exactly as before.
        let plain = handle_sync(&app(), "tools/list", &Value::Null, &json!(2));
        assert!(
            plain["result"]["tools"][0]["inputSchema"]["properties"]
                .get("level")
                .is_none(),
            "{plain}"
        );

        // With levels declared on the primary, `level` is advertised as an enum of the declared
        // names, and is optional (never in `required`).
        let cfg = Config::from_args(&[
            "--reviewer".into(),
            "codex".into(),
            "--level".into(),
            "fast:gpt-5.6-luna:high".into(),
            "--level".into(),
            "thorough:gpt-5.6-luna:max".into(),
            "--default-level".into(),
            "thorough".into(),
        ])
        .expect("cfg");
        let leveled = Arc::new(App::new(cfg));
        let resp = handle_sync(&leveled, "tools/list", &Value::Null, &json!(2));
        let schema = &resp["result"]["tools"][0]["inputSchema"];
        assert_eq!(
            schema["properties"]["level"]["enum"],
            json!(["fast", "thorough"])
        );
        assert_eq!(schema["additionalProperties"], false);
        let required = schema["required"].as_array().expect("required array");
        assert!(!required.iter().any(|v| v == "level"), "{schema}");
        assert!(schema["properties"]["level"]["description"]
            .as_str()
            .unwrap()
            .contains("thorough"));
    }

    /// The caller is told the git change is derived live through repository_diff, not pre-captured
    /// under a --diff mode (retire-capture-modes).
    #[test]
    fn review_tool_description_describes_live_git_delivery() {
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

        // A git review (any reviewer) describes the live repository_diff delivery, and never a
        // static --diff capture.
        for args in [vec![], vec!["--reviewer", "codex"]] {
            let text = describe(&args);
            assert!(text.contains("repository_diff"), "{args:?}: {text}");
            assert!(!text.contains("--diff"), "{args:?}: {text}");
        }
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
        request.attach_owned("rv-1-1");
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
    fn cancelling_an_owned_start_request_stops_its_review() {
        use std::sync::atomic::Ordering;

        let app = app();
        let (review_id, reviewer_cancel) = app
            .registry()
            .try_start("default", crate::registry::JobKind::Review, 1, false)
            .expect("start");

        // `attach_owned` is the start call's binding: its review_id was never delivered, so a
        // cancellation must stop the reviewer, not merely detach.
        let request = Arc::new(RequestCancel::new());
        request.attach_owned(&review_id);
        let pending = pending();
        pending
            .lock()
            .unwrap()
            .insert(request_key(&json!(9)), Arc::clone(&request));

        handle_cancellation(&app, &pending, &json!({"requestId": 9, "reason": "user"}));

        // The half that costs money: the flag reviewer::run polls, not merely the
        // request's own. Without it the child keeps working to its full turn budget.
        assert!(reviewer_cancel.load(Ordering::SeqCst));
        assert!(!request.try_claim_response());
    }

    #[test]
    fn a_cancellation_mid_poll_detaches_the_wait_and_leaves_the_review_running() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let app = app();
        let (review_id, reviewer_cancel) = app
            .registry()
            .try_start("default", crate::registry::JobKind::Review, 1, false)
            .expect("start");

        let request = Arc::new(RequestCancel::new());
        let pending = pending();
        pending
            .lock()
            .unwrap()
            .insert(request_key(&json!(9)), Arc::clone(&request));

        // A budget far longer than this test can take: only the detach wake should end the wait,
        // and if it does not this joins slowly rather than quietly passing.
        let poller = {
            let app = Arc::clone(&app);
            let request = Arc::clone(&request);
            let args = json!({"review_id": review_id.clone(), "wait_seconds": 300});
            std::thread::spawn(move || app.review_result(&args, &request))
        };

        // Give the poll time to reach attach_wait and park, then cancel it.
        std::thread::sleep(Duration::from_millis(100));
        let started = std::time::Instant::now();
        handle_cancellation(&app, &pending, &json!({"requestId": 9}));

        let result = poller.join().expect("poller");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cancellation did not end the wait"
        );
        // Either a running snapshot (cancelled after parking) or CANCELLED (cancelled before the
        // attach_wait); both are correct, and both must leave the review running.
        match &result {
            Ok(text) => assert!(text.contains("status:    running"), "{text}"),
            Err(failure) => assert_eq!(failure.code, "CANCELLED"),
        }
        // The review must NOT have been stopped -- a poll cancellation detaches, it does not kill.
        assert!(
            !reviewer_cancel.load(Ordering::SeqCst),
            "a poll cancellation must not stop the review"
        );
        assert_eq!(
            app.registry()
                .snapshot(&review_id)
                .expect("still tracked")
                .status,
            crate::registry::Status::Running,
        );
        // Cancelled, so the handler thread would send nothing.
        assert!(!request.try_claim_response());
    }

    /// The ordering `drain_in_flight` exists to hold: waiters are released before the join,
    /// not after. Moving the release below the join leaves every registry-level test green
    /// and silently restores a five-minute shutdown, so it is pinned here.
    #[test]
    fn draining_releases_a_parked_poll_before_joining_it() {
        use std::time::Duration;

        let app = app();
        let (id, _cancel) = app
            .registry()
            .try_start("default", crate::registry::JobKind::Review, 1, false)
            .expect("start");

        let handle = {
            let app = Arc::clone(&app);
            let args = json!({"review_id": id, "wait_seconds": 30});
            std::thread::spawn(move || {
                let _ = app.review_result(&args, &RequestCancel::new());
            })
        };

        // Long enough for the poll to actually reach `Registry::wait`, so the join below
        // is joining a parked thread rather than one that has not got there yet.
        std::thread::sleep(Duration::from_millis(100));
        let started = std::time::Instant::now();
        drain_in_flight(&app, vec![handle]);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the join waited out the poll's budget"
        );
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
            // A writer may be between writeln!'s JSON-body write and its newline write.
            // Parse only complete frames so polling this recorder from another thread does
            // not turn that harmless instant into a flaky parse failure.
            let complete = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map(|last| &bytes[..=last])
                .unwrap_or_default();
            String::from_utf8(complete.to_vec())
                .expect("utf-8")
                .lines()
                .map(|l| serde_json::from_str(l).expect("each line is one JSON message"))
                .collect()
        }

        fn raw_ends_with_newline(&self) -> bool {
            self.0
                .lock()
                .unwrap()
                .last()
                .is_some_and(|byte| *byte == b'\n')
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

    #[test]
    fn a_result_wait_emits_standard_progress_until_the_call_finishes() {
        let app = app();
        let (review_id, _cancel) = app
            .registry()
            .try_start("progress", crate::registry::JobKind::Review, 1, false)
            .expect("start");
        app.registry().report_activity(&review_id, 4096);

        let recorder = Recorder::default();
        let writer = recorder.writer();
        let request = Arc::new(RequestCancel::new());
        let params = json!({
            "name": "cross_model_review_result",
            "arguments": {"review_id": review_id},
            "_meta": {"progressToken": "progress-7"}
        });
        let reporter = ProgressReporter::start_with_interval(
            &app,
            &writer,
            &params,
            &request,
            Duration::from_millis(20),
        )
        .expect("progress reporter");

        // Bounded rather than sleeping for an assumed number of scheduler slices.
        let give_up = std::time::Instant::now() + Duration::from_secs(2);
        while recorder.responses().len() < 3 {
            assert!(
                std::time::Instant::now() < give_up,
                "progress notifications did not arrive"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(reporter);

        let notifications = recorder.responses();
        assert!(notifications.len() >= 3);
        for (index, notification) in notifications.iter().enumerate() {
            assert_eq!(notification["method"], "notifications/progress");
            assert_eq!(notification["params"]["progressToken"], "progress-7");
            assert_eq!(notification["params"]["progress"], index as u64);
            let message = notification["params"]["message"]
                .as_str()
                .expect("progress message");
            assert!(message.contains("reviewer process running"), "{message}");
            assert!(message.contains("4 KiB"), "{message}");
            assert!(message.contains("20 minutes or longer"), "{message}");
        }

        // Drop joins the reporter before the tool response would be sent, so completion
        // is a hard boundary rather than a best-effort flag checked by a sleeping thread.
        let after_drop = notifications.len();
        assert!(
            recorder.raw_ends_with_newline(),
            "protocol messages must be newline-terminated"
        );
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(recorder.responses().len(), after_drop);
    }

    #[test]
    fn progress_stops_when_the_client_cancels_the_request() {
        let app = app();
        let (review_id, _cancel) = app
            .registry()
            .try_start(
                "cancel-progress",
                crate::registry::JobKind::Review,
                1,
                false,
            )
            .expect("start");
        let recorder = Recorder::default();
        let writer = recorder.writer();
        let request = Arc::new(RequestCancel::new());
        let params = json!({
            "name": "cross_model_review_result",
            "arguments": {"review_id": review_id},
            "_meta": {"progressToken": "cancel-progress"}
        });
        let reporter = ProgressReporter::start_with_interval(
            &app,
            &writer,
            &params,
            &request,
            Duration::from_millis(20),
        )
        .expect("progress reporter");
        request.cancel();
        let before = recorder.responses().len();
        assert!(
            before >= 1,
            "the synchronous initial notification is missing"
        );
        std::thread::sleep(Duration::from_millis(60));
        drop(reporter);
        assert_eq!(
            recorder.responses().len(),
            before,
            "a cancelled request received another progress notification"
        );
    }

    #[test]
    fn progress_is_opt_in_and_only_for_a_live_result_wait() {
        let app = app();
        let (review_id, _cancel) = app
            .registry()
            .try_start("progress", crate::registry::JobKind::Review, 1, false)
            .expect("start");
        let recorder = Recorder::default();
        let writer = recorder.writer();

        let no_token = json!({
            "name": "cross_model_review_result",
            "arguments": {"review_id": review_id}
        });
        let request = Arc::new(RequestCancel::new());
        assert!(ProgressReporter::start_with_interval(
            &app,
            &writer,
            &no_token,
            &request,
            Duration::from_millis(1)
        )
        .is_none());

        app.registry().finish(
            &review_id,
            crate::registry::Outcome::failed(crate::errors::cancelled()),
        );
        let finished = json!({
            "name": "cross_model_review_result",
            "arguments": {"review_id": review_id},
            "_meta": {"progressToken": "already-finished"}
        });
        assert!(ProgressReporter::start_with_interval(
            &app,
            &writer,
            &finished,
            &request,
            Duration::from_millis(1)
        )
        .is_none());

        let wrong_tool = json!({
            "name": "cross_model_review_status",
            "_meta": {"progressToken": 9}
        });
        assert!(ProgressReporter::start_with_interval(
            &app,
            &writer,
            &wrong_tool,
            &request,
            Duration::from_millis(1)
        )
        .is_none());
        assert!(recorder.responses().is_empty());
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
