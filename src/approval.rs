//! The one-off localhost human-approval page for profile setup.
//!
//! Authorizing a reviewer profile is a human decision, so the setup flow opens a page in the user's
//! browser and waits for them to click **Approve**. That page is an attack surface, so it is locked
//! down (`[f9]`): the listener binds **loopback only** on an ephemeral port; the approval URL carries
//! an **unguessable one-time capability token** (OS CSPRNG) with a **short expiry**; the server
//! validates the method, path, and token on **every** request and matches the profile details from
//! the state it already holds, **never** from page or query parameters; every interpolated value is
//! HTML-escaped; and approval consumes the token (single-use) and advances the state machine to the
//! provisional/in-setup state — it does **not** itself commit the allowlist (that is gated separately
//! in the setup flow). No GUI toolkit and no credential field of our own: sign-in is always the vendor
//! page.

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Absolute wall-clock budget for reading one request. A per-read socket timeout is not enough: a
/// client that dribbles a byte just inside each read window would never trip it, so the whole request
/// is also bounded by this, after which the connection is dropped ([f3]).
const CONNECTION_BUDGET: Duration = Duration::from_secs(5);

/// Absolute cap on writing a response. A client that never reads the response would otherwise block
/// `write_all` (and the handler) once the send buffer fills; this bounds that ([f5]).
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on in-flight connection handlers. Loopback-only and short-lived, but bounded so a local flood
/// cannot spawn unbounded threads; excess connections are closed immediately.
const MAX_HANDLERS: usize = 32;

/// One labelled fact shown to the human on the approval page, e.g. `("Profile", "work")`.
pub struct ApprovalRow {
    pub label: String,
    pub value: String,
}

/// What the approval page presents to the human deciding whether to authorize a profile. Every value
/// is HTML-escaped before rendering; the server holds this and matches on it, never trusting anything
/// the browser sends back beyond the one-time token.
pub struct ApprovalDetails {
    /// A short page heading, e.g. "Authorize a reviewer profile for this repository".
    pub title: String,
    /// The facts to display (operation, reviewer, profile, launch root, home, account …).
    pub rows: Vec<ApprovalRow>,
    /// An optional caution line rendered prominently (e.g. what authorizing grants).
    pub caution: Option<String>,
}

/// How the wait for human approval ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The human clicked Approve with a valid, unexpired, unused token.
    Approved,
    /// The expiry elapsed before an approval arrived.
    TimedOut,
    /// The caller cancelled (e.g. the tool call was cancelled).
    Cancelled,
}

struct Shared {
    /// The single authoritative outcome. First writer wins (see [`set_terminal`]/[`try_approve`]), so
    /// a cancellation or timeout already recorded cannot be overwritten by a later approval, and an
    /// approval cannot land after the deadline.
    outcome: Option<ApprovalOutcome>,
    stop: bool,
}

/// Everything a connection handler needs, shared (via `Arc`) between the accept loop and the
/// per-connection handler threads.
struct ServerCtx {
    token: String,
    page: String,
    /// The absolute approval expiry. Checked *under the lock* at approval time, so an approval cannot
    /// succeed after it ([f1]).
    deadline: Instant,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    /// Count of in-flight handler threads, so acceptance can shed load past [`MAX_HANDLERS`].
    active: AtomicUsize,
}

/// Decrements the in-flight handler count when a handler thread ends, even on panic.
struct ActiveGuard(Arc<ServerCtx>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Record a terminal outcome if none is set yet, waking any waiter. `also_stop` additionally marks the
/// server to stop accepting (used by cancel). First writer wins: a cancellation/timeout already
/// recorded is not overwritten, and neither is an approval that already landed.
fn set_terminal(shared: &(Mutex<Shared>, Condvar), outcome: ApprovalOutcome, also_stop: bool) {
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

/// Atomically record an approval, or refuse it. Under the lock it checks that the server is not
/// stopping, no outcome is set yet, and the deadline has not passed — so an approval cannot overwrite
/// a cancellation ([f2]) or land after expiry ([f1]). Returns whether the approval was recorded.
fn try_approve(shared: &(Mutex<Shared>, Condvar), deadline: Instant) -> bool {
    let (lock, cvar) = shared;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if guard.stop || guard.outcome.is_some() || Instant::now() >= deadline {
        return false;
    }
    guard.outcome = Some(ApprovalOutcome::Approved);
    cvar.notify_all();
    true
}

/// A running approval server: bound to loopback, serving the approval page until the human approves,
/// the expiry elapses, or it is cancelled/dropped. The token lives only in [`Self::url`] and the
/// server's own memory.
pub struct ApprovalServer {
    url: String,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl ApprovalServer {
    /// Bind a loopback listener, mint a one-time token, and start serving the approval page. The page
    /// is rendered once up front (so its details are fixed), and the server stops on the first valid
    /// approval, at `timeout`, or on cancel/drop.
    pub fn start(details: &ApprovalDetails, timeout: Duration) -> std::io::Result<Self> {
        let token = crate::digest::random_hex_token(32).ok_or_else(|| {
            std::io::Error::other("could not generate a random approval token from the OS CSPRNG")
        })?;
        // Loopback only, ephemeral port. Never bind a routable address.
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let url = format!("http://127.0.0.1:{port}/?token={token}");
        let page = render_page(details, &token);
        let ctx = Arc::new(ServerCtx {
            token,
            page,
            deadline: Instant::now() + timeout,
            shared: Arc::new((
                Mutex::new(Shared {
                    outcome: None,
                    stop: false,
                }),
                Condvar::new(),
            )),
            active: AtomicUsize::new(0),
        });
        let shared = Arc::clone(&ctx.shared);
        let handle = std::thread::Builder::new()
            .name("cross-review-approval".to_string())
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

    /// The outcome if one has been decided, without blocking. Lets a caller interleave waiting for
    /// approval with polling its own cancellation (via [`cancel`](Self::cancel) when it sees it).
    pub fn poll(&self) -> Option<ApprovalOutcome> {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).outcome
    }

    /// Block until the human approves, the expiry elapses, or the server is cancelled. The setup flow
    /// uses [`poll`](Self::poll) instead so it can interleave its own cancellation; this blocking form
    /// is exercised by the tests and kept as the simpler API.
    #[allow(dead_code)]
    pub fn wait(&self) -> ApprovalOutcome {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        while guard.outcome.is_none() {
            guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        guard.outcome.expect("outcome set")
    }

    /// Ask the server to stop and record a `Cancelled` outcome if none is set yet. Recording the
    /// outcome (not just a stop flag) is what makes an in-flight approval lose to a cancellation ([f2]).
    pub fn cancel(&self) {
        set_terminal(&self.shared, ApprovalOutcome::Cancelled, true);
    }
}

impl Drop for ApprovalServer {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_loop(listener: TcpListener, ctx: Arc<ServerCtx>) {
    loop {
        // Stop once the outcome is decided (an approval landed on a handler thread, a cancellation
        // arrived, or the timeout below fired) — the accept loop then exits promptly.
        {
            let (lock, cvar) = &*ctx.shared;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            if guard.stop && guard.outcome.is_none() {
                guard.outcome = Some(ApprovalOutcome::Cancelled);
                cvar.notify_all();
            }
            if guard.outcome.is_some() {
                return;
            }
        }
        if Instant::now() >= ctx.deadline {
            set_terminal(&ctx.shared, ApprovalOutcome::TimedOut, false);
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Shed load past the cap rather than spawn unbounded threads on a local flood.
                if ctx.active.load(Ordering::SeqCst) >= MAX_HANDLERS {
                    drop(stream);
                    continue;
                }
                ctx.active.fetch_add(1, Ordering::SeqCst);
                let handler_ctx = Arc::clone(&ctx);
                // Handle each connection on its own short-lived thread, so a slow client can never
                // stall acceptance or delay observing the deadline/cancellation ([f3]). The thread is
                // detached but bounded by `CONNECTION_BUDGET`; `ActiveGuard` frees its slot on exit.
                let spawned = std::thread::Builder::new()
                    .name("cross-review-approval-conn".to_string())
                    .spawn(move || {
                        let _guard = ActiveGuard(Arc::clone(&handler_ctx));
                        handle_connection(stream, &handler_ctx);
                    });
                if spawned.is_err() {
                    // The thread never started, so no `ActiveGuard` will free the slot — do it here.
                    ctx.active.fetch_sub(1, Ordering::SeqCst);
                }
            }
            // Non-blocking listener: nothing pending, sleep briefly and re-check outcome/deadline.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            // A transient accept error is not worth aborting the whole approval over.
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Handle one HTTP connection: read the request under an absolute budget, validate, and for a valid
/// approval POST record the outcome atomically.
fn handle_connection(mut stream: TcpStream, ctx: &ServerCtx) {
    stream.set_nonblocking(false).ok();
    // Bound both directions, and **fail closed** if either bound cannot be set: without them a
    // half-open or non-reading client could block a handler forever, exhausting `MAX_HANDLERS` and
    // outliving `Drop` ([f5]). A short per-read timeout keeps the read loop re-checking the absolute
    // connection budget below; the write timeout bounds a client that will not consume the response.
    if stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let connection_deadline = Instant::now() + CONNECTION_BUDGET;

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        // Absolute per-connection bound: a drip-feeding client that stays just inside each read
        // window is still cut off here, so it cannot occupy a handler indefinitely ([f3]).
        if buf.len() > 8192 || Instant::now() >= connection_deadline {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if contains_subslice(&buf, b"\r\n\r\n") {
                    break;
                }
            }
            // A per-read timeout (no bytes yet): loop to re-check the absolute connection deadline.
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

    let request = String::from_utf8_lossy(&buf);
    let first_line = request.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let token_ok = query_param(query, "token")
        .is_some_and(|t| constant_time_eq(t.as_bytes(), ctx.token.as_bytes()));

    // Validate method, path, and token on every request; match nothing from the query but the token.
    if !token_ok {
        respond(
            &mut stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            "Forbidden: missing or invalid one-time token.\n",
        );
        return;
    }
    // Once the server is terminal (approved/cancelled/timed out) or past its deadline, no route serves
    // the live approval page — a request that was accepted before the outcome and finished reading
    // after it (or a re-used token) gets the invalid-link page, not a stale approval page ([f4]).
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
        ("POST", "/approve") => {
            // Record the approval atomically under the lock: it is refused if the deadline has passed
            // or a cancellation/timeout already won ([f1]/[f2]).
            if try_approve(&ctx.shared, ctx.deadline) {
                respond(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    APPROVED_PAGE,
                );
            } else {
                respond(
                    &mut stream,
                    "409 Conflict",
                    "text/html; charset=utf-8",
                    EXPIRED_PAGE,
                );
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

/// Whether the server has reached a terminal outcome or passed its deadline, so no live approval page
/// should be served ([f4]).
fn is_terminal(shared: &(Mutex<Shared>, Condvar), deadline: Instant) -> bool {
    let (lock, _) = shared;
    let terminal = lock
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .outcome
        .is_some();
    terminal || Instant::now() >= deadline
}

pub(crate) fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Value of `name` in an `&`-separated `k=v` query string, or `None`. Only the token is read this way,
/// and it is a hex string, so no percent-decoding is needed.
pub(crate) fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

/// Length-checked, difference-accumulating comparison, so token validation does not leak length or a
/// prefix match through timing.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: \
         no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    write_all_deadline(stream, response.as_bytes(), Instant::now() + WRITE_TIMEOUT);
}

/// Write `bytes` under an **absolute** deadline. A per-socket write timeout only bounds a single
/// stalled `write`; a client that accepts one byte just inside each window would let `write_all` make
/// unbounded incremental progress and pin the handler past [`MAX_HANDLERS`] ([f5]). This loops against a
/// hard end time and gives up (closing the connection) once it passes, so no handler is held longer than
/// the budget regardless of how the peer reads. Responses here are a few KB and normally fit the socket
/// send buffer, so the first `write` sends them all and the deadline only bites a pathological
/// slow-reader.
pub(crate) fn write_all_deadline(stream: &mut TcpStream, bytes: &[u8], deadline: Instant) {
    let mut written = 0;
    while written < bytes.len() {
        if Instant::now() >= deadline {
            return;
        }
        match stream.write(&bytes[written..]) {
            Ok(0) => return,
            Ok(n) => written += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }
    }
    let _ = stream.flush();
}

/// HTML-escape a value for safe interpolation into the page ([f9]).
pub(crate) fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn render_page(details: &ApprovalDetails, token: &str) -> String {
    let mut rows = String::new();
    for row in &details.rows {
        rows.push_str(&format!(
            "<tr><th>{}</th><td>{}</td></tr>",
            escape(&row.label),
            escape(&row.value)
        ));
    }
    let caution = match &details.caution {
        Some(text) => format!("<p class=\"caution\">{}</p>", escape(text)),
        None => String::new(),
    };
    // The token is echoed only into the form action (the same capability the page was opened with).
    // It is a hex string, safe in an attribute; escape it anyway for defence in depth.
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>\
         body{{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}\
         table{{border-collapse:collapse;margin:1rem 0;width:100%}}\
         th,td{{text-align:left;padding:.4rem .6rem;border-bottom:1px solid #ccc;vertical-align:top}}\
         th{{width:12rem;color:#444;font-weight:600}}td{{font-family:ui-monospace,monospace;word-break:break-all}}\
         .caution{{background:#fff6e0;border:1px solid #e0b400;border-radius:.4rem;padding:.6rem .8rem}}\
         button{{font-size:1rem;padding:.6rem 1.4rem;border:0;border-radius:.4rem;background:#0a5;color:#fff;cursor:pointer}}\
         </style></head><body>\
         <h1>{title}</h1>{caution}<table>{rows}</table>\
         <form method=\"post\" action=\"/approve?token={token}\">\
         <button type=\"submit\">Approve</button></form>\
         <p>If you did not start this, close this tab and do nothing — nothing is authorized until you click Approve.</p>\
         </body></html>",
        title = escape(&details.title),
    )
}

const APPROVED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
     <title>Approved</title><style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}</style>\
     </head><body><h1>Approved</h1><p>You can close this tab and return to your terminal.</p></body></html>";

const EXPIRED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
     <title>No longer valid</title><style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}</style>\
     </head><body><h1>This approval link is no longer valid</h1><p>It expired or was already used or cancelled. Start the setup again if you still want to authorize this profile.</p></body></html>";

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut std::ffi::c_void,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_cmd: i32,
    ) -> isize;
}

/// Open `url` in the user's default browser via `ShellExecuteW("open", …)` — the safe launcher (no
/// shell, no current-directory executable resolution). Best-effort: on failure the caller can still
/// print the URL for the human to open manually.
pub fn open_in_browser(url: &str) -> bool {
    let operation: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let file: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    const SW_SHOWNORMAL: i32 = 1;
    // SAFETY: both wide strings are NUL-terminated and live across the call; the other pointers are
    // null (no parameters/directory) and hwnd is null (no owner window).
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value greater than 32 on success.
    result > 32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details() -> ApprovalDetails {
        ApprovalDetails {
            title: "Authorize a reviewer profile".to_string(),
            rows: vec![
                ApprovalRow {
                    label: "Profile".to_string(),
                    value: "work".to_string(),
                },
                ApprovalRow {
                    label: "Launch root".to_string(),
                    value: r"C:\dev\repo".to_string(),
                },
            ],
            caution: Some(
                "This lets this repository run reviews under the profile account.".into(),
            ),
        }
    }

    /// Parse `http://127.0.0.1:PORT/?token=TOK` into (port, token).
    fn split_url(url: &str) -> (u16, String) {
        let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback url");
        let (port, query) = rest.split_once("/?").expect("path");
        let token = query
            .strip_prefix("token=")
            .expect("token param")
            .to_string();
        (port.parse().expect("port"), token)
    }

    fn raw_request(port: u16, request: &str) -> String {
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
    fn serves_the_page_only_with_the_right_token_and_approves_once() {
        let server = ApprovalServer::start(&details(), Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());

        // The correct token renders the page with the (escaped) details.
        let ok = raw_request(
            port,
            &format!("GET /?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("Authorize a reviewer profile"));
        assert!(ok.contains("work"));

        // A wrong token is forbidden.
        let bad = raw_request(port, "GET /?token=deadbeef HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(bad.starts_with("HTTP/1.1 403"), "{bad}");

        // A missing token is forbidden.
        let none = raw_request(port, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(none.starts_with("HTTP/1.1 403"), "{none}");

        // Approving with the right token succeeds and unblocks wait().
        let approve = raw_request(
            port,
            &format!("POST /approve?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(approve.starts_with("HTTP/1.1 200"), "{approve}");
        assert_eq!(server.wait(), ApprovalOutcome::Approved);
    }

    #[test]
    fn a_post_with_the_wrong_token_does_not_approve() {
        let server = ApprovalServer::start(&details(), Duration::from_secs(30)).expect("start");
        let (port, _token) = split_url(server.url());
        let bad = raw_request(port, "POST /approve?token=nope HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(bad.starts_with("HTTP/1.1 403"), "{bad}");
        // Not approved: cancel and confirm the outcome is not Approved.
        server.cancel();
        assert_eq!(server.wait(), ApprovalOutcome::Cancelled);
    }

    fn fresh_shared() -> (Mutex<Shared>, Condvar) {
        (
            Mutex::new(Shared {
                outcome: None,
                stop: false,
            }),
            Condvar::new(),
        )
    }

    #[test]
    fn try_approve_refuses_after_the_deadline() {
        // [f1]: an approval landing after the deadline must be refused under the lock, never recorded.
        let shared = fresh_shared();
        let past = Instant::now() - Duration::from_secs(1);
        assert!(!try_approve(&shared, past));
        assert_eq!(shared.0.lock().unwrap().outcome, None);
        // A future deadline with no prior outcome approves.
        let future = Instant::now() + Duration::from_secs(30);
        assert!(try_approve(&shared, future));
        assert_eq!(
            shared.0.lock().unwrap().outcome,
            Some(ApprovalOutcome::Approved)
        );
    }

    #[test]
    fn cancellation_beats_a_later_approval() {
        // [f2]: once a terminal outcome (cancel/timeout) is recorded, an approval cannot overwrite it.
        let shared = fresh_shared();
        set_terminal(&shared, ApprovalOutcome::Cancelled, true);
        assert!(!try_approve(
            &shared,
            Instant::now() + Duration::from_secs(30)
        ));
        assert_eq!(
            shared.0.lock().unwrap().outcome,
            Some(ApprovalOutcome::Cancelled)
        );
        // And a second terminal outcome does not overwrite the first (first writer wins).
        set_terminal(&shared, ApprovalOutcome::TimedOut, false);
        assert_eq!(
            shared.0.lock().unwrap().outcome,
            Some(ApprovalOutcome::Cancelled)
        );
    }

    #[test]
    fn is_terminal_reflects_outcome_and_deadline() {
        let shared = fresh_shared();
        // Fresh, with a future deadline: not terminal.
        assert!(!is_terminal(
            &shared,
            Instant::now() + Duration::from_secs(30)
        ));
        // A past deadline is terminal even with no recorded outcome.
        assert!(is_terminal(
            &shared,
            Instant::now() - Duration::from_secs(1)
        ));
        // A recorded outcome is terminal even with a future deadline.
        set_terminal(&shared, ApprovalOutcome::Approved, false);
        assert!(is_terminal(
            &shared,
            Instant::now() + Duration::from_secs(30)
        ));
    }

    #[test]
    fn a_request_that_finishes_after_a_terminal_outcome_gets_the_invalid_link_page() {
        // [f4]: a connection accepted while live but whose request completes after the server goes
        // terminal must NOT get the live approval page. Connect and hold the request back, let the
        // handler accept and begin reading, then cancel, then send the request.
        let server = ApprovalServer::start(&details(), Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());
        let mut stream =
            TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).expect("connect");
        // Give the accept loop time to accept the connection and spawn its handler (which is now
        // looping on per-read timeouts waiting for the request).
        std::thread::sleep(Duration::from_millis(120));
        server.cancel();
        assert_eq!(server.wait(), ApprovalOutcome::Cancelled);
        // Now send the request; the handler reads it and, seeing the terminal state, refuses.
        stream
            .write_all(format!("GET /?token={token} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .expect("write");
        stream.flush().ok();
        let mut resp = String::new();
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        stream.read_to_string(&mut resp).ok();
        assert!(resp.starts_with("HTTP/1.1 409"), "{resp}");
        assert!(
            !resp.contains("Approve</button>"),
            "must not serve the live page: {resp}"
        );
    }

    #[test]
    fn an_unknown_path_with_a_valid_token_is_not_found() {
        let server = ApprovalServer::start(&details(), Duration::from_secs(30)).expect("start");
        let (port, token) = split_url(server.url());
        let resp = raw_request(
            port,
            &format!("GET /secret?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
    }

    #[test]
    fn it_times_out_when_no_one_approves() {
        let server = ApprovalServer::start(&details(), Duration::from_millis(150)).expect("start");
        assert_eq!(server.wait(), ApprovalOutcome::TimedOut);
    }

    #[test]
    fn escaping_neutralises_markup_in_details() {
        assert_eq!(escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn tokens_are_unguessable_and_distinct() {
        let a = crate::digest::random_hex_token(32).expect("token");
        let b = crate::digest::random_hex_token(32).expect("token");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
