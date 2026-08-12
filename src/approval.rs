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
// The whole module is exercised by tests but has no production caller until the setup MCP tool
// (Phase 3 task #15) wires it in; remove this once that lands.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    outcome: Option<ApprovalOutcome>,
    stop: bool,
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

        let shared = Arc::new((
            Mutex::new(Shared {
                outcome: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let deadline = Instant::now() + timeout;
        let server_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("cross-review-approval".to_string())
            .spawn(move || serve_loop(listener, token, page, deadline, server_shared))?;

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

    /// Block until the human approves, the expiry elapses, or the server is cancelled.
    pub fn wait(&self) -> ApprovalOutcome {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        while guard.outcome.is_none() {
            guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        guard.outcome.expect("outcome set")
    }

    /// Ask the server to stop and record a `Cancelled` outcome if none is set yet.
    pub fn cancel(&self) {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.stop = true;
        cvar.notify_all();
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

fn serve_loop(
    listener: TcpListener,
    token: String,
    page: String,
    deadline: Instant,
    shared: Arc<(Mutex<Shared>, Condvar)>,
) {
    let (lock, cvar) = &*shared;
    let finish = |outcome: ApprovalOutcome| {
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if guard.outcome.is_none() {
            guard.outcome = Some(outcome);
            cvar.notify_all();
        }
    };
    loop {
        {
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            if guard.stop {
                drop(guard);
                finish(ApprovalOutcome::Cancelled);
                return;
            }
        }
        if Instant::now() >= deadline {
            finish(ApprovalOutcome::TimedOut);
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if handle_connection(stream, &token, &page) {
                    finish(ApprovalOutcome::Approved);
                    return;
                }
            }
            // Non-blocking listener: nothing pending, sleep briefly and re-check stop/deadline.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            // A transient accept error is not worth aborting the whole approval over.
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Handle one HTTP connection. Returns `true` only for a valid single-use approval POST.
fn handle_connection(mut stream: TcpStream, expected_token: &str, page: &str) -> bool {
    // The accepted stream inherits non-blocking; make it blocking with a short read timeout so a
    // half-open or slow client cannot stall the loop.
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Stop once the request headers are complete, or a bound is hit (a request line +
                // headers this long is not one of ours).
                if buf.len() > 8192 || ends_headers(&buf) {
                    break;
                }
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
        .is_some_and(|t| constant_time_eq(t.as_bytes(), expected_token.as_bytes()));

    // Validate method, path, and token on every request; match nothing from the query but the token.
    match (method, path) {
        _ if !token_ok => {
            respond(
                &mut stream,
                "403 Forbidden",
                "text/plain; charset=utf-8",
                "Forbidden: missing or invalid one-time token.\n",
            );
            false
        }
        ("GET", "/") => {
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", page);
            false
        }
        ("POST", "/approve") => {
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                APPROVED_PAGE,
            );
            true
        }
        _ => {
            respond(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                "Not found.\n",
            );
            false
        }
    }
}

fn ends_headers(buf: &[u8]) -> bool {
    buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" || contains_subslice(buf, b"\r\n\r\n")
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Value of `name` in an `&`-separated `k=v` query string, or `None`. Only the token is read this way,
/// and it is a hex string, so no percent-decoding is needed.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

/// Length-checked, difference-accumulating comparison, so token validation does not leak length or a
/// prefix match through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// HTML-escape a value for safe interpolation into the page ([f9]).
fn escape(value: &str) -> String {
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
