//! A one-off localhost page for the human to paste an OAuth **code** back into the setup flow.
//!
//! Some vendor logins (Claude's `auth login`) do not redirect to a localhost callback; the browser
//! shows an authorization **code** the human must paste back into the CLI's stdin. In an MCP context
//! there is no terminal for that, so this serves a small loopback page: it shows the vendor's auth URL
//! and a single text field into which the human pastes the code, and captures it server-side. The
//! setup flow then writes that code to the vendor login child's stdin.
//!
//! Same attack-surface lockdown as [`crate::approval`] ([f9]): loopback-only ephemeral port; an
//! unguessable one-time capability token with a short expiry validated on **every** request; bounded
//! reads (headers **and** body) and writes; HTML-escaped output; `no-store`; terminal-state
//! invalidation; first-writer-wins outcome. The submitted code is a short-lived secret: it is held only
//! in memory, handed straight to the child's stdin, and **never logged or returned in any error**.

#![cfg(windows)]

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::approval::{
    constant_time_eq, contains_subslice, escape, query_param, write_all_deadline,
};

const CONNECTION_BUDGET: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HANDLERS: usize = 32;
/// Absolute cap on a request (headers + body). A pasted code is short; anything larger is refused.
const MAX_REQUEST: usize = 16 * 1024;
/// Cap on the decoded code length, so a hostile client cannot make us hold an unbounded string.
const MAX_CODE_LEN: usize = 4096;

/// How the wait for the human's code ended.
#[derive(Clone, Debug)]
pub enum CodeOutcome {
    /// The human submitted a (non-empty) code with a valid, unexpired, unused token.
    Submitted(String),
    /// The expiry elapsed before a code arrived.
    TimedOut,
    /// The caller cancelled (e.g. the tool call was cancelled).
    Cancelled,
}

struct Shared {
    outcome: Option<CodeOutcome>,
    stop: bool,
    /// The login finished successfully (the child exited 0), so the page should stop asking for a code
    /// and show a "sign-in complete" message. Independent of `outcome`: a browser-callback account
    /// completes without ever submitting a code, so this is set by the caller, not by a page request.
    completed: bool,
    /// A `/status` request has served `done` to the page at least once, so the polling page has been
    /// told to dismiss. Lets the caller stop waiting the moment the page has been updated (#97).
    done_delivered: bool,
}

struct ServerCtx {
    token: String,
    page: String,
    deadline: Instant,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    active: AtomicUsize,
}

struct ActiveGuard(Arc<ServerCtx>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn set_terminal(shared: &(Mutex<Shared>, Condvar), outcome: CodeOutcome, also_stop: bool) {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if also_stop {
        guard.stop = true;
    }
    if guard.outcome.is_none() {
        guard.outcome = Some(outcome);
        cvar.notify_all();
    }
}

/// Atomically record a submitted code, or refuse it (server stopping, the login already completed, an
/// outcome already set, or the deadline passed). Returns whether it was recorded. The `completed` check
/// closes a race in the callback path (f2): once the login has succeeded, the server lingers briefly so
/// the page can observe `done` (#97), and a stale `/submit` arriving in that window must not record a
/// code the caller will never feed — nor stop the status listener out from under the polling page.
fn try_submit(shared: &(Mutex<Shared>, Condvar), deadline: Instant, code: String) -> bool {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if guard.stop || guard.completed || guard.outcome.is_some() || Instant::now() >= deadline {
        return false;
    }
    guard.outcome = Some(CodeOutcome::Submitted(code));
    cvar.notify_all();
    true
}

/// A running code-entry server, bound to loopback, serving the code-entry page until the human submits
/// a code, the expiry elapses, or it is cancelled/dropped.
pub struct CodeEntryServer {
    url: String,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl CodeEntryServer {
    /// Bind a loopback listener, mint a one-time token, and start serving the code-entry page showing
    /// `auth_url` (the vendor's authorization URL) and a field for the code.
    pub fn start(title: &str, auth_url: &str, timeout: Duration) -> std::io::Result<Self> {
        let token = crate::digest::random_hex_token(32).ok_or_else(|| {
            std::io::Error::other("could not generate a random code-entry token from the OS CSPRNG")
        })?;
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let url = format!("http://127.0.0.1:{port}/?token={token}");
        let page = render_page(title, auth_url, &token);
        let ctx = Arc::new(ServerCtx {
            token,
            page,
            deadline: Instant::now() + timeout,
            shared: Arc::new((
                Mutex::new(Shared {
                    outcome: None,
                    stop: false,
                    completed: false,
                    done_delivered: false,
                }),
                Condvar::new(),
            )),
            active: AtomicUsize::new(0),
        });
        let shared = Arc::clone(&ctx.shared);
        let handle = std::thread::Builder::new()
            .name("cross-review-codeentry".to_string())
            .spawn(move || serve_loop(listener, ctx))?;
        Ok(Self {
            url,
            shared,
            handle: Some(handle),
        })
    }

    /// The URL to open in the browser. Carries the one-time token; treat it as a secret.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The outcome if decided, without blocking (so a caller can interleave its own cancellation).
    pub fn poll(&self) -> Option<CodeOutcome> {
        let (lock, _) = &*self.shared;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .outcome
            .clone()
    }

    /// Ask the server to stop and record `Cancelled` if no outcome is set yet.
    pub fn cancel(&self) {
        set_terminal(&self.shared, CodeOutcome::Cancelled, true);
    }

    /// Signal that the login finished successfully (the child exited 0), and report whether a polling
    /// page can still be told. A page polling `/status` picks `completed` up and swaps to a "sign-in
    /// complete" message instead of lingering with a code field a browser-callback account never needs
    /// (#97). This does not stop the server or set an `outcome`; the caller drops the server after
    /// giving the page a moment to observe it.
    ///
    /// Returns `false` — meaning there is nothing to wait for — when an `outcome` is already set or the
    /// server is stopping: recording a submitted code (or a cancel/timeout) stops the accept loop, so no
    /// `/status` request can be served after that point. The check-and-set is atomic under the lock and
    /// mirrors [`try_submit`]'s `completed` check, so exactly one of the two wins a race: either this
    /// marks `completed` (and later submits are refused) or a submit has already set `outcome` (and this
    /// returns `false`). That closes the submit/child-exit race where the caller would otherwise wait
    /// the full grace against an already-dead listener (f3).
    pub fn complete(&self) -> bool {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if guard.outcome.is_some() || guard.stop {
            return false;
        }
        guard.completed = true;
        cvar.notify_all();
        true
    }

    /// Block until a `/status` request has served `done` to the polling page — i.e. the page has been
    /// told the login is complete — or `max` elapses. Bounded and best-effort: if no browser page is
    /// polling (headless, or the human closed the tab), it simply waits out `max` and returns. Call
    /// after [`complete`](Self::complete).
    pub fn wait_page_dismissed(&self, max: Duration) {
        let (lock, cvar) = &*self.shared;
        let deadline = Instant::now() + max;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        while !guard.done_delivered {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (g, _) = cvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
        }
    }
}

impl Drop for CodeEntryServer {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_loop(listener: TcpListener, ctx: Arc<ServerCtx>) {
    loop {
        {
            let (lock, cvar) = &*ctx.shared;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            if guard.stop && guard.outcome.is_none() {
                guard.outcome = Some(CodeOutcome::Cancelled);
                cvar.notify_all();
            }
            if guard.outcome.is_some() {
                return;
            }
        }
        if Instant::now() >= ctx.deadline {
            set_terminal(&ctx.shared, CodeOutcome::TimedOut, false);
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if ctx.active.load(Ordering::SeqCst) >= MAX_HANDLERS {
                    drop(stream);
                    continue;
                }
                ctx.active.fetch_add(1, Ordering::SeqCst);
                let handler_ctx = Arc::clone(&ctx);
                let spawned = std::thread::Builder::new()
                    .name("cross-review-codeentry-conn".to_string())
                    .spawn(move || {
                        let _guard = ActiveGuard(Arc::clone(&handler_ctx));
                        handle_connection(stream, &handler_ctx);
                    });
                if spawned.is_err() {
                    ctx.active.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn handle_connection(mut stream: TcpStream, ctx: &ServerCtx) {
    stream.set_nonblocking(false).ok();
    if stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let connection_deadline = Instant::now() + CONNECTION_BUDGET;

    // Read the headers, then (for a POST) the Content-Length body, both under the absolute budget.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let mut header_end: Option<usize> = None;
    let mut content_length: usize = 0;
    loop {
        if buf.len() > MAX_REQUEST || Instant::now() >= connection_deadline {
            break;
        }
        if header_end.is_none() {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                header_end = Some(pos + 4);
                // The *declared* length, not clamped: an oversized or unparseable value is rejected in
                // the POST handler rather than silently accepted (f4).
                content_length = parse_content_length(&buf[..pos]);
            }
        }
        if let Some(end) = header_end {
            // Stop **immediately** if the declared body cannot fit the request cap (including an absurd
            // usize::MAX length): waiting for a body that the handler will reject anyway would tie the
            // handler up for the whole connection budget (f5). Otherwise wait for the exact body;
            // `saturating_add` keeps the comparison overflow-free.
            if content_length > MAX_REQUEST || buf.len() >= end.saturating_add(content_length) {
                break;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        }
    }

    let head_end = header_end.unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end.min(buf.len())]);
    let first_line = head.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let token_ok = query_param(query, "token")
        .is_some_and(|t| constant_time_eq(t.as_bytes(), ctx.token.as_bytes()));

    if !token_ok {
        respond(
            &mut stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            "Forbidden: missing or invalid one-time token.\n",
        );
        return;
    }
    // The page polls this to auto-dismiss (#97): `done` once the login has succeeded, `expired` once it
    // is over any other way, `pending` while a code may still be needed. Served *before* the terminal
    // 409 below so a timed-out/cancelled server still answers the poller with a status it understands,
    // not the 409 HTML page. Coarse and secret-free.
    if method == "GET" && path == "/status" {
        let status = page_status(&ctx.shared, ctx.deadline);
        respond(&mut stream, "200 OK", "text/plain; charset=utf-8", status);
        return;
    }
    if is_terminal(&ctx.shared, ctx.deadline) {
        respond(
            &mut stream,
            "409 Conflict",
            "text/html; charset=utf-8",
            EXPIRED_PAGE,
        );
        return;
    }
    match (method, path) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", &ctx.page),
        ("POST", "/submit") => {
            // Framing checks (f4/f5): the body must be declared (non-zero Content-Length), fit the
            // request cap, and be **fully received** (exactly Content-Length bytes). `checked_add`
            // makes an absurd length (usize::MAX) fail closed rather than overflow the range arithmetic.
            let hend = header_end.unwrap_or(buf.len());
            let end = match hend.checked_add(content_length) {
                Some(e) if content_length != 0 && e <= MAX_REQUEST => e,
                _ => {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain; charset=utf-8",
                        "Bad request.\n",
                    );
                    return;
                }
            };
            if buf.len() < end {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    "Incomplete request body.\n",
                );
                return;
            }
            // Parse **exactly** the declared body, not any extra buffered bytes.
            let body = &buf[hend..end];
            match form_field(body, "code") {
                // Empty code: refuse without recording.
                Some(code) if code.is_empty() => {
                    respond(
                        &mut stream,
                        "409 Conflict",
                        "text/html; charset=utf-8",
                        EXPIRED_PAGE,
                    );
                }
                Some(code) => {
                    if try_submit(&ctx.shared, ctx.deadline, code) {
                        respond(
                            &mut stream,
                            "200 OK",
                            "text/html; charset=utf-8",
                            SUBMITTED_PAGE,
                        );
                    } else {
                        // A race lost to cancel/timeout.
                        respond(
                            &mut stream,
                            "409 Conflict",
                            "text/html; charset=utf-8",
                            EXPIRED_PAGE,
                        );
                    }
                }
                None => respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    "Missing code.\n",
                ),
            }
        }
        _ => respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            "Not found.\n",
        ),
    }
}

/// The coarse status the polling page reads from `/status`. `done` when the login has succeeded
/// (serving it also records [`done_delivered`](Shared::done_delivered) so the caller can stop waiting),
/// `expired` once the wait is over any other way, `pending` while a code may still be submitted.
fn page_status(shared: &(Mutex<Shared>, Condvar), deadline: Instant) -> &'static str {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if guard.completed {
        if !guard.done_delivered {
            guard.done_delivered = true;
            cvar.notify_all();
        }
        return "done";
    }
    let expired = guard.stop
        || matches!(
            guard.outcome,
            Some(CodeOutcome::TimedOut) | Some(CodeOutcome::Cancelled)
        )
        || Instant::now() >= deadline;
    if expired {
        "expired"
    } else {
        "pending"
    }
}

fn is_terminal(shared: &(Mutex<Shared>, Condvar), deadline: Instant) -> bool {
    let (lock, _) = shared;
    let terminal = lock
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .outcome
        .is_some();
    terminal || Instant::now() >= deadline
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers);
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

/// The value of form field `name` in an `application/x-www-form-urlencoded` body, percent-decoded.
/// Returns `None` if the field is absent **or its decoded value exceeds [`MAX_CODE_LEN`]** — an
/// overlong code is *rejected*, never silently truncated (f4).
fn form_field(body: &[u8], name: &str) -> Option<String> {
    // Only the code is read; it is a short opaque string, so decode just the matching field.
    if !contains_subslice(body, name.as_bytes()) {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    for pair in text.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                let decoded = percent_decode(v);
                let trimmed = decoded.trim();
                if trimmed.chars().count() > MAX_CODE_LEN {
                    return None;
                }
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: \
         no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Bound the write by an absolute deadline, not just the per-socket timeout (f5): see
    // `approval::write_all_deadline`.
    write_all_deadline(stream, response.as_bytes(), Instant::now() + WRITE_TIMEOUT);
}

fn render_page(title: &str, auth_url: &str, token: &str) -> String {
    // `token` is a hex CSPRNG string (`random_hex_token`), so it is safe to embed directly in the JS
    // string literal below; it cannot break out of the quotes.
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>\
         body{{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}\
         a{{word-break:break-all}}input{{font-family:ui-monospace,monospace;font-size:1rem;padding:.5rem;width:100%;box-sizing:border-box}}\
         button{{font-size:1rem;padding:.6rem 1.4rem;margin-top:.8rem;border:0;border-radius:.4rem;background:#0a5;color:#fff;cursor:pointer}}\
         .caution{{background:#fff6e0;border:1px solid #e0b400;border-radius:.4rem;padding:.6rem .8rem}}\
         .code-step{{margin-top:1.5rem;padding-top:1rem;border-top:1px solid #ddd;color:#444}}\
         </style></head><body>\
         <div id=\"main\">\
         <h1>{title}</h1>\
         <p class=\"caution\">Only continue if you started this profile setup.</p>\
         <p><strong>A Claude sign-in tab was opened behind this page.</strong> Finish signing in there \
         (or open <a href=\"{url}\">the sign-in link</a> again). If it shows <em>Sign in successful</em>, \
         you're done — this page will update on its own; you can then close it.</p>\
         <div class=\"code-step\">\
         <p>Most sign-ins finish in that tab with nothing to do here. <strong>Only</strong> if the sign-in \
         shows you an authorization code to copy, paste it below:</p>\
         <form method=\"post\" action=\"/submit?token={token}\">\
         <input type=\"text\" name=\"code\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"Authorization code (only if the sign-in shows one)\">\
         <br><button type=\"submit\">Submit code</button></form>\
         <p>Any code is sent only to this local page and passed straight to the sign-in; nothing is authorized until sign-in completes and you approve.</p>\
         </div></div>\
         <script>(function(){{\
         var t={token:?};var done=false;var fails=0;\
         function show(h,b){{done=true;document.getElementById('main').innerHTML='<h1>'+h+'</h1><p>'+b+'</p>';}}\
         function over(){{show('This page is no longer needed','The sign-in is over. You can close this tab.');}}\
         function poll(){{if(done)return;\
         fetch('/status?token='+encodeURIComponent(t),{{cache:'no-store'}})\
         .then(function(r){{return r.text();}}).then(function(s){{fails=0;s=(s||'').trim();\
         if(s==='done'){{show('Sign-in complete','Your reviewer profile finished signing in. You can close this tab.');try{{window.close();}}catch(e){{}}}}\
         else if(s==='expired'){{over();}}\
         else{{setTimeout(poll,1000);}}}})\
         .catch(function(){{fails++;if(fails>=3){{over();}}else{{setTimeout(poll,1000);}}}});}}\
         setTimeout(poll,1000);}})();</script>\
         </body></html>",
        title = escape(title),
        url = escape(auth_url),
        token = token,
    )
}

const SUBMITTED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
     <title>Code received</title><style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}</style>\
     </head><body><h1>Code received</h1><p>You can close this tab and return to your terminal.</p></body></html>";

const EXPIRED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
     <title>No longer valid</title><style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}</style>\
     </head><body><h1>This link is no longer valid</h1><p>It expired or was already used or cancelled. Start the setup again if you still want to sign in.</p></body></html>";

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn split_url(url: &str) -> (u16, String) {
        let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback url");
        let (port, query) = rest.split_once("/?").expect("path");
        let token = query.strip_prefix("token=").expect("token").to_string();
        (port.parse().expect("port"), token)
    }

    fn raw(port: u16, request: &str) -> String {
        let mut stream =
            TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream.write_all(request.as_bytes()).expect("write");
        stream.flush().ok();
        let mut response = String::new();
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        stream.read_to_string(&mut response).ok();
        response
    }

    #[test]
    fn serves_the_page_and_captures_a_pasted_code_once() {
        let server = CodeEntryServer::start(
            "Finish sign-in",
            "https://vendor/auth?x=1",
            Duration::from_secs(30),
        )
        .expect("start");
        let (port, token) = split_url(server.url());

        // The page renders with the (escaped) auth URL and a code field.
        let page = raw(
            port,
            &format!("GET /?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("name=\"code\""));
        assert!(page.contains("https://vendor/auth?x=1"));

        // A wrong token is forbidden.
        let bad = raw(port, "GET /?token=deadbeef HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(bad.starts_with("HTTP/1.1 403"), "{bad}");

        // Submitting a percent-encoded code captures it (decoded).
        let body = "code=abc%2D123%20xyz";
        let post = format!(
            "POST /submit?token={token} HTTP/1.1\r\nHost: x\r\nContent-Type: \
             application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let resp = raw(port, &post);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        match server.poll() {
            Some(CodeOutcome::Submitted(code)) => assert_eq!(code, "abc-123 xyz"),
            other => panic!("expected Submitted, got {other:?}"),
        }
    }

    #[test]
    fn a_submit_with_the_wrong_token_does_not_record() {
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        let (port, _t) = split_url(server.url());
        let body = "code=xyz";
        let resp = raw(
            port,
            &format!(
                "POST /submit?token=nope HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        server.cancel();
        assert!(matches!(server.poll(), Some(CodeOutcome::Cancelled)));
    }

    #[test]
    fn a_post_without_a_body_or_with_an_overlong_code_is_rejected() {
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());

        // No Content-Length (no declared body): bad request, not a partial parse.
        let no_body = raw(
            port,
            &format!("POST /submit?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(no_body.starts_with("HTTP/1.1 400"), "{no_body}");

        // An overlong code is rejected (not truncated and accepted).
        let big = format!("code={}", "a".repeat(super::MAX_CODE_LEN + 100));
        let overlong = raw(
            port,
            &format!(
                "POST /submit?token={token} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{big}",
                big.len()
            ),
        );
        assert!(overlong.starts_with("HTTP/1.1 400"), "{overlong}");

        // An absurd Content-Length must fail closed, never overflow/panic the handler (f5).
        let huge = raw(
            port,
            &format!(
                "POST /submit?token={token} HTTP/1.1\r\nHost: x\r\nContent-Length: \
                 18446744073709551615\r\n\r\ncode=x"
            ),
        );
        assert!(huge.starts_with("HTTP/1.1 400"), "{huge}");
        // Nothing was recorded by any of them.
        assert!(server.poll().is_none());
    }

    #[test]
    fn it_times_out_when_no_code_is_submitted() {
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_millis(150)).expect("start");
        std::thread::sleep(Duration::from_millis(300));
        assert!(matches!(server.poll(), Some(CodeOutcome::TimedOut)));
    }

    #[test]
    fn percent_decode_handles_plus_and_hex() {
        assert_eq!(percent_decode("a+b%2Dc%2f"), "a b-c/");
    }

    fn status_body(port: u16, token: &str) -> String {
        let resp = raw(
            port,
            &format!("GET /status?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        resp.rsplit("\r\n\r\n").next().unwrap().trim().to_string()
    }

    #[test]
    fn status_reports_pending_then_done_after_complete() {
        // #97: the page polls /status to auto-dismiss. Fresh it is `pending`; once the caller signals a
        // successful callback login via complete(), it flips to `done` — with no code ever submitted.
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());

        assert_eq!(status_body(port, &token), "pending");
        assert!(
            server.complete(),
            "fresh server has a live listener to signal"
        );
        assert_eq!(status_body(port, &token), "done");
        // The wait returns promptly now that a /status GET has delivered `done` to the page.
        server.wait_page_dismissed(Duration::from_secs(5));
        // complete() is independent of the code-wait outcome: no code was submitted.
        assert!(!matches!(server.poll(), Some(CodeOutcome::Submitted(_))));
    }

    #[test]
    fn page_status_maps_each_terminal_state() {
        // The status logic itself. cancel()/timeout stop the *server*, so `expired` cannot be observed
        // over HTTP (the listener is gone by then — the polling page handles that as repeated fetch
        // failures instead); exercise the mapping directly here.
        let future = Instant::now() + Duration::from_secs(30);
        let mk = |outcome, stop, completed| {
            (
                Mutex::new(Shared {
                    outcome,
                    stop,
                    completed,
                    done_delivered: false,
                }),
                Condvar::new(),
            )
        };

        assert_eq!(page_status(&mk(None, false, false), future), "pending");
        // A submitted code is still in-flight (waiting on the child), not expired.
        assert_eq!(
            page_status(
                &mk(Some(CodeOutcome::Submitted("x".into())), false, false),
                future
            ),
            "pending"
        );
        assert_eq!(page_status(&mk(None, false, true), future), "done");
        assert_eq!(
            page_status(&mk(Some(CodeOutcome::Cancelled), true, false), future),
            "expired"
        );
        assert_eq!(
            page_status(&mk(Some(CodeOutcome::TimedOut), false, false), future),
            "expired"
        );
        // Past the deadline is expired even with no recorded outcome yet.
        assert_eq!(
            page_status(
                &mk(None, false, false),
                Instant::now() - Duration::from_secs(1)
            ),
            "expired"
        );
        // `done` wins over an also-expired condition.
        assert_eq!(
            page_status(
                &mk(None, true, true),
                Instant::now() - Duration::from_secs(1)
            ),
            "done"
        );
    }

    #[test]
    fn complete_reports_no_wait_once_an_outcome_is_recorded() {
        // f3: gating the caller's grace on complete()'s return closes the submit/child-exit race.
        // Once a code has been submitted, the server has stopped, so there is no live /status listener
        // to deliver `done` — complete() must report false so the caller does not wait for nothing.
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());
        let body = "code=abc";
        let post = format!(
            "POST /submit?token={token} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert!(raw(port, &post).starts_with("HTTP/1.1 200"));
        assert!(matches!(server.poll(), Some(CodeOutcome::Submitted(_))));
        assert!(
            !server.complete(),
            "an already-submitted session has no live listener to wait on"
        );
    }

    #[test]
    fn a_submit_after_completion_is_refused() {
        // f2: once the login has succeeded (complete()), the server lingers briefly so the page can see
        // `done`. A stale /submit arriving in that window must be refused, not recorded — the caller has
        // already stopped feeding stdin, and recording an outcome would stop the status listener.
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());
        assert!(server.complete());
        let body = "code=late";
        let resp = raw(
            port,
            &format!(
                "POST /submit?token={token} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        // Refused as a spent/expired link, and nothing was recorded as Submitted.
        assert!(resp.starts_with("HTTP/1.1 409"), "{resp}");
        assert!(!matches!(server.poll(), Some(CodeOutcome::Submitted(_))));
        // /status still reports the login as done.
        assert_eq!(status_body(port, &token), "done");
    }

    #[test]
    fn status_requires_the_token() {
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        let (port, _t) = split_url(server.url());
        let resp = raw(port, "GET /status?token=nope HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
    }

    #[test]
    fn wait_page_dismissed_returns_when_no_page_polls() {
        // Headless / closed-tab case: nothing ever fetches /status, so the wait must fall through on its
        // own bound rather than hang. complete() is set but done_delivered never will be.
        let server =
            CodeEntryServer::start("t", "https://v/a", Duration::from_secs(30)).expect("start");
        assert!(server.complete());
        let start = Instant::now();
        server.wait_page_dismissed(Duration::from_millis(200));
        assert!(start.elapsed() >= Duration::from_millis(150));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
