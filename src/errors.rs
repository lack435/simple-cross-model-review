//! The failure contract.
//!
//! When a review cannot happen, the calling agent must not paper over it by
//! reviewing its own work. Every failure therefore carries a machine-readable code,
//! a plain statement of what broke, and the exact remediation to relay to the user.
//!
//! `handler_thread_unavailable` is the one exception, and returns finished text rather
//! than a `Failure`: it is the only failure here that is not about a review, so the
//! sentences `Failure` renders around one do not fit. Its code is in the text but cannot
//! be read off a field. Nothing matches on it today; a caller that needed to would be the
//! signal to make it a `Failure` and give the type a render mode that suits it.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub summary: String,
    pub remediation: String,
    pub detail: Option<String>,
}

impl Failure {
    pub fn new(
        code: &'static str,
        summary: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            summary: summary.into(),
            remediation: remediation.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let trimmed = detail.trim();
        if !trimmed.is_empty() {
            self.detail = Some(truncate(trimmed, 4000));
        }
        self
    }

    /// The text handed back to the calling agent. Deliberately blunt: an agent that
    /// skims this should still come away knowing it has to stop and talk to the user.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("CROSS-MODEL REVIEW FAILED\n");
        out.push_str(&format!("code: {}\n\n", self.code));
        out.push_str(&self.summary);
        out.push_str("\n\n");
        out.push_str("=== ACTION REQUIRED ===\n");
        out.push_str(
            "The external review did not run, so there is no review feedback. Stop the current \
             task now. Do not review the work yourself in place of the external reviewer, and do \
             not continue as if the review had passed.\n\n\
             Report this to the user:\n\n",
        );
        out.push_str(&self.remediation);
        out.push_str("\n\nThen wait for the user before proceeding.\n");
        out.push_str("=======================\n");
        if let Some(detail) = &self.detail {
            out.push_str("\n--- diagnostic output from the reviewer CLI ---\n");
            out.push_str(detail);
            out.push('\n');
        }
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n... [truncated]")
}

// ---------------------------------------------------------------------------
// Constructors, one per failure mode the calling agent can encounter.
// ---------------------------------------------------------------------------

pub fn cli_not_found(reviewer: &str, tried: &[String]) -> Failure {
    let install = match reviewer {
        "codex" => "npm install -g @openai/codex   (or install the Codex desktop app)",
        _ => "npm install -g @anthropic-ai/claude-code",
    };
    Failure::new(
        "CLI_NOT_FOUND",
        format!("The '{reviewer}' CLI could not be found, so no reviewer is available."),
        format!(
            "The cross-model review tool cannot run because the '{reviewer}' CLI is not \
             installed or not on PATH.\n\n\
             To fix it, either:\n\
             \x20 1. Install it:  {install}\n\
             \x20 2. Or point the MCP server at an existing install by adding\n\
             \x20    --bin \"C:\\full\\path\\to\\{reviewer}.exe\" to the cross-review args\n\
             \x20    in this project's MCP configuration.\n\n\
             Then restart the MCP server (restart the agent session) and retry."
        ),
    )
    .with_detail(format!("Looked for:\n{}", tried.join("\n")))
}

pub fn not_authenticated(reviewer: &str, detail: impl Into<String>) -> Failure {
    let (login_cmd, note) = match reviewer {
        "codex" => ("codex login", "This signs in with your ChatGPT account."),
        _ => (
            "claude auth login",
            "This signs in with your Anthropic account.",
        ),
    };
    Failure::new(
        "NOT_AUTHENTICATED",
        format!("The '{reviewer}' CLI is installed but not signed in, so it cannot review."),
        format!(
            "The cross-model review tool cannot run because the '{reviewer}' CLI is not \
             authenticated.\n\n\
             To fix it, run this in a terminal:\n\n\
             \x20 {login_cmd}\n\n\
             {note}\n\n\
             Once it reports success, retry the review."
        ),
    )
    .with_detail(detail)
}

pub fn auth_expired(reviewer: &str, detail: impl Into<String>) -> Failure {
    let login_cmd = if reviewer == "codex" {
        "codex login"
    } else {
        "claude auth login"
    };
    Failure::new(
        "AUTH_EXPIRED_MIDRUN",
        format!(
            "The '{reviewer}' CLI rejected the request as unauthorized. Its credentials have \
             most likely expired."
        ),
        format!(
            "The cross-model review failed because the '{reviewer}' CLI's credentials were \
             rejected (expired or revoked).\n\n\
             To fix it, run this in a terminal:\n\n\
             \x20 {login_cmd}\n\n\
             Then retry the review."
        ),
    )
    .with_detail(detail)
}

pub fn model_unavailable(
    reviewer: &str,
    model: &str,
    effort: &str,
    detail: impl Into<String>,
) -> Failure {
    Failure::new(
        "MODEL_UNAVAILABLE",
        format!(
            "The '{reviewer}' CLI rejected model '{model}' or effort '{effort}'. The pinned \
             reviewer model is not usable on this account."
        ),
        format!(
            "The cross-model review failed because the '{reviewer}' CLI rejected the configured \
             reviewer model.\n\n\
             \x20 model:  {model}\n\
             \x20 effort: {effort}\n\n\
             The model may have been renamed, may not be available on this account or plan, or \
             the effort level may not be valid for it. Fix it by editing the --model / --effort \
             values in the cross-review entry of this project's MCP configuration, then restart \
             the agent session.\n\n\
             The review has NOT been performed."
        ),
    )
    .with_detail(detail)
}

pub fn rate_limited(reviewer: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "RATE_LIMITED",
        format!("The '{reviewer}' CLI reported a rate or usage limit, so the review was refused."),
        format!(
            "The cross-model review could not run because the '{reviewer}' account has hit a \
             rate limit or usage cap.\n\n\
             Nothing is broken in the setup. Either wait for the limit to reset and retry, or \
             use an account or plan with remaining capacity.\n\n\
             The review has NOT been performed."
        ),
    )
    .with_detail(detail)
}

pub fn timed_out(reviewer: &str, secs: u64, detail: impl Into<String>) -> Failure {
    Failure::new(
        "TIMEOUT",
        format!("The '{reviewer}' reviewer did not finish within {secs} seconds and was stopped."),
        format!(
            "The cross-model review was cancelled after {secs} seconds without completing.\n\n\
             This usually means the review request was very large, or the reviewer CLI stalled. \
             Options:\n\
             \x20 1. Retry with a narrower review request (fewer files, a more specific question).\n\
             \x20 2. Raise the budget by adding --timeout-seconds <larger value> to the\n\
             \x20    cross-review args in this project's MCP configuration.\n\n\
             The review has NOT been performed."
        ),
    )
    .with_detail(detail)
}

/// A timeout accompanied by Codex command-policy refusals needs different guidance from a
/// generic slow model. Raising the budget only gives the reviewer more time to retry commands
/// that the non-interactive CLI will never approve.
pub fn timed_out_after_policy_denials(
    reviewer: &str,
    secs: u64,
    count: usize,
    count_is_floor: bool,
    detail: impl Into<String>,
) -> Failure {
    // A capped stderr drops later refusals, so the retained count is a lower bound; say
    // "at least N" rather than assert an exact total the collection could not have seen.
    let count_phrase = if count_is_floor {
        format!("at least {count}")
    } else {
        count.to_string()
    };
    Failure::new(
        "TIMEOUT",
        format!(
            "The '{reviewer}' reviewer timed out after its CLI refused {count_phrase} shell \
             command(s) by policy."
        ),
        format!(
            "The cross-model review was cancelled after {secs} seconds. The reviewer encountered \
             non-interactive command-policy refusals, so increasing the timeout is unlikely to \
             help. Use the direct read commands the reviewer was told to prefer, configure a \
             narrowly scoped allow rule for commands you trust, or use the other reviewer \
             direction. The review has NOT been performed."
        ),
    )
    .with_detail(detail)
}

pub fn cancelled() -> Failure {
    Failure::new(
        "CANCELLED",
        "The review was cancelled before it finished.",
        "The cross-model review was cancelled, so there is no review feedback. This happens \
         when cross_model_review_cancel is called on the review, or when a cross_model_review \
         start call is abandoned before its review_id is delivered. Abandoning a \
         cross_model_review_result collect does NOT cause this -- that only detaches the wait, \
         and the review keeps running and stays collectible by its review_id, so try collecting \
         it again before starting over. Start a new review if one is still needed.",
    )
}

pub fn spawn_failed(reviewer: &str, bin: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "SPAWN_FAILED",
        format!("The '{reviewer}' CLI at '{bin}' could not be started."),
        format!(
            "The cross-model review tool found the '{reviewer}' CLI at:\n\n\
             \x20 {bin}\n\n\
             but the operating system refused to start it. The file may be corrupt, blocked by \
             policy or antivirus, or not an executable. Try running it directly in a terminal to \
             see the underlying error.\n\n\
             The review has NOT been performed."
        ),
    )
    .with_detail(detail)
}

pub fn reviewer_crashed(reviewer: &str, exit: Option<i32>, detail: impl Into<String>) -> Failure {
    let exit_desc = match exit {
        Some(c) => format!("exit code {c}"),
        None => "no exit code (killed by a signal)".to_string(),
    };
    Failure::new(
        "REVIEWER_FAILED",
        format!(
            "The '{reviewer}' CLI exited unsuccessfully ({exit_desc}) without producing a review."
        ),
        format!(
            "The cross-model review failed: the '{reviewer}' CLI exited with an error \
             ({exit_desc}) and returned no review.\n\n\
             The diagnostic output below is the reviewer's own error message; it usually says \
             what went wrong. Show it to the user so they can decide how to proceed.\n\n\
             The review has NOT been performed."
        ),
    )
    .with_detail(detail)
}

pub fn empty_review(reviewer: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "EMPTY_REVIEW",
        format!("The '{reviewer}' CLI completed but returned no review text."),
        format!(
            "The cross-model review produced no output. The '{reviewer}' CLI exited successfully \
             but wrote nothing, which usually indicates the request was rejected silently or the \
             CLI's output format changed.\n\n\
             Retry once. If it happens again, report it to the user rather than continuing \
             without a review."
        ),
    )
    .with_detail(detail)
}

/// The reviewer produced more output than we are willing to hold, and what we kept could
/// not be made sense of.
///
/// Separate from `EMPTY_REVIEW` because the advice differs. An empty review means the CLI
/// wrote nothing and retrying is reasonable; a truncated one means it wrote far too much,
/// and the half we kept parsed as nothing. Reporting the second as the first sends the
/// caller to retry an operation that will do the same thing again.
pub fn output_truncated(reviewer: &str, megabytes: usize, detail: impl Into<String>) -> Failure {
    Failure::new(
        "OUTPUT_TRUNCATED",
        format!(
            "The '{reviewer}' CLI produced more than {megabytes} MiB of output, so it was \
             truncated and the review could not be read from what remained."
        ),
        format!(
            "The cross-model review produced far more output than a review should. Collection is \
             capped at {megabytes} MiB per stream, so the transcript is incomplete and the \
             reviewer's own result could not be recovered from it.\n\n\
             This is not a normal failure: real reviews are kilobytes. Report it to the user \
             rather than retrying blindly, and do not continue as if the review had passed."
        ),
    )
    .with_detail(detail)
}

/// stdout ended before it was fully read -- a pipe read error, or the drain deadline expiring --
/// so the transcript is a partial prefix even though the process may have exited cleanly. Kept
/// apart from `OUTPUT_TRUNCATED`, whose message asserts the CLI exceeded the size cap, which is
/// not what happened here and can occur with a small transcript.
pub fn output_incomplete(reviewer: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "OUTPUT_INCOMPLETE",
        format!(
            "The '{reviewer}' CLI's output stream ended before it was fully read, so the review \
             could not be recovered from the partial output."
        ),
        "The reviewer's output was cut off mid-stream (a pipe error or a drain timeout), so the \
         transcript is incomplete and the result could not be parsed from what arrived. This is \
         not a normal failure. Report it to the user rather than retrying blindly, and do not \
         continue as if the review had passed."
            .to_string(),
    )
    .with_detail(detail)
}

pub fn session_not_found(session: &str, id: &str) -> Failure {
    Failure::new(
        "SESSION_NOT_FOUND",
        format!(
            "Review session '{session}' refers to reviewer session '{id}', which no longer exists."
        ),
        format!(
            "The saved review session '{session}' could not be resumed because the underlying \
             reviewer session ('{id}') is gone. Reviewer CLIs expire old sessions.\n\n\
             Retry the review with fresh=true to start a new review session. Earlier review \
             history for '{session}' is not recoverable."
        ),
    )
}

/// A stored session exists but policy forbids resuming it: it is too old, has run too many
/// turns, or its reviewer, model or working root no longer matches this server.
///
/// Deliberately refused rather than silently restarted. The calling agent asked for
/// continuity -- the same reviewer, still holding its earlier findings -- and would act on
/// the answer as though it had that. Silently handing it a fresh review with no memory is
/// the one way this tool can mislead without anything appearing to go wrong, so the caller
/// is made to choose. Agent-correctable: it just starts fresh, so it gets a plain
/// correction, not the stop-and-tell-the-user contract.
pub fn session_not_resumable(session: &str, reason: String) -> Failure {
    Failure {
        code: "SESSION_NOT_RESUMABLE",
        summary: format!("Review session '{session}' exists but cannot be resumed: {reason}"),
        remediation: format!(
            "This is a deliberate guard against resuming a stale review conversation, not a \
             setup problem. To review now, call cross_model_review again with fresh=true -- \
             that starts a new session under the name '{session}' with no memory of earlier \
             turns -- or use a different session name. Carry any earlier findings that still \
             matter into the new instructions, since the reviewer will not remember them."
        ),
        detail: None,
    }
}

/// Bad tool arguments. This is the calling agent's mistake, not a setup problem, so it
/// gets a plain correction instead of the stop-and-tell-the-user contract.
pub fn bad_request(summary: impl Into<String>) -> Failure {
    let summary = summary.into();
    Failure {
        code: "BAD_REQUEST",
        remediation: format!("{summary} Correct the tool arguments and call it again."),
        summary,
        detail: None,
    }
}

/// The worker thread unwound without recording a result.
pub fn worker_panicked(review_id: &str) -> Failure {
    Failure::new(
        "INTERNAL_ERROR",
        format!("The cross-review worker for review '{review_id}' failed unexpectedly."),
        "The cross-model review tool hit an internal error and the review did not \
         complete. Retrying is reasonable; if it recurs, this is a bug in cross-review \
         itself and the server's stderr output will contain the panic message.\n\n\
         The review has NOT been performed.",
    )
}

/// The server could not start a thread to run a `tools/call` handler.
///
/// The one failure here that is not about a review, which is why it is written out rather
/// than built from a `Failure`: every render mode of that type hard-codes a sentence about
/// the review, and both would misstate this. The stop-and-escalate form asserts "The
/// external review did not run", false when the call that could not be handled was
/// `cross_model_review_result` and the review it was collecting is still running; the
/// agent-correctable form is headed "REQUEST REJECTED", which blames a request that was
/// fine. What holds for all four tools is only that this call did not happen, and that no
/// review anywhere changed state because of it. That is what this says, and it says nothing
/// either way about whether a review exists.
///
/// The OS error is inlined here for the same reason `with_detail` is not used elsewhere on
/// this path: its rendered header attributes the text to the reviewer CLI, which never saw
/// this request.
pub fn handler_thread_unavailable(tool: &str, os_error: &str) -> String {
    let tool = if tool.is_empty() {
        "cross-review"
    } else {
        tool
    };
    format!(
        "TOOL CALL NOT HANDLED\n\
         code: INTERNAL_ERROR\n\n\
         The cross-review server could not start a thread to run the '{tool}' call \
         ({os_error}), so the call did not happen.\n\n\
         The operating system refused to create the thread. In practice that means the \
         machine running the server is out of memory or has hit its cap on threads per \
         process; either way it is not a problem with the review setup, the reviewer CLI, or \
         the arguments. Nothing was sent to the reviewer, and no review changed state: any \
         review already in progress is unaffected and can still be collected with \
         cross_model_review_result.\n\n\
         Call the tool again. If it fails the same way a second time, stop and tell the user \
         the server cannot start threads on this machine, rather than carrying on without \
         the review.\n"
    )
}

/// Another server process holds this named session.
pub fn session_leased(session: &str, detail: String) -> Failure {
    let mut failure = Failure {
        code: "SESSION_BUSY",
        summary: format!(
            "Review session '{session}' is currently held by another cross-review server \
             process, most likely a second agent session open on this same project."
        ),
        remediation: format!(
            "Wait for the other review of session '{session}' to finish and retry, or start \
             this review under a different session name."
        ),
        detail: None,
    };
    if !detail.trim().is_empty() {
        failure = failure.with_detail(detail);
    }
    failure
}

/// Stdin has closed, so the review was refused rather than started with nowhere to go.
///
/// Reported instead of starting the reviewer because the server is on its way out: the
/// `review_id` would be unusable, and the reviewer turn would be billed for a result
/// nothing could collect.
pub fn server_shutting_down() -> Failure {
    Failure {
        code: "SERVER_SHUTTING_DOWN",
        summary: "This server's stdin is no longer readable, so it is shutting down and did \
                  not start the review. It is draining the calls already in flight and will \
                  then exit, so a review started now could not be collected: review ids do \
                  not survive the server process."
            .to_string(),
        remediation: "Nothing was spent and there is no review to collect. Reconnect to the \
                      cross-review MCP server and start the review again. If this arrived \
                      unprompted, the client closed the connection while the call was still \
                      in flight."
            .to_string(),
        detail: None,
    }
}

/// A review is already in flight for this named session.
pub fn session_busy(session: &str, review_id: &str) -> Failure {
    Failure {
        code: "SESSION_BUSY",
        summary: format!(
            "Review session '{session}' already has a review in progress (review_id \
             '{review_id}'). A session handles one review turn at a time."
        ),
        remediation: format!(
            "Collect the in-flight review first by calling cross_model_review_result with \
             review_id '{review_id}', or cancel it with cross_model_review_cancel. Then start \
             the next turn."
        ),
        detail: None,
    }
}

/// Too many reviews are already running in this server process.
pub fn too_many_running(limit: u32) -> Failure {
    Failure {
        code: "TOO_MANY_RUNNING",
        summary: format!(
            "This server already has {limit} review(s) running, which is its per-process \
             --max-concurrent-reviews limit, so it did not start another."
        ),
        remediation: "Collect an outstanding review with cross_model_review_result, or cancel one \
                      with cross_model_review_cancel, then start this one. If several reviews were \
                      started and left uncollected, that is the situation the limit guards against: \
                      finish or cancel them rather than starting more."
            .to_string(),
        detail: None,
    }
}

impl Failure {
    /// True when the failure is the agent's own fault and it should just retry
    /// differently, rather than stopping to involve the user.
    pub fn is_agent_correctable(&self) -> bool {
        matches!(
            self.code,
            "BAD_REQUEST" | "SESSION_BUSY" | "SESSION_NOT_RESUMABLE" | "TOO_MANY_RUNNING"
        )
    }

    /// Agent-correctable failures skip the stop-and-escalate ceremony.
    pub fn render_for_agent(&self) -> String {
        if self.is_agent_correctable() {
            let mut out = format!(
                "REQUEST REJECTED\ncode: {}\n\n{}\n\n{}\n",
                self.code, self.summary, self.remediation
            );
            if let Some(d) = &self.detail {
                out.push_str(&format!("\n{d}\n"));
            }
            out
        } else {
            self.render()
        }
    }
}

/// Map a reviewer CLI's stderr/stdout onto a specific failure. Ordering matters:
/// auth and quota problems are checked before the generic crash case so the user
/// gets an actionable message instead of a raw stack trace.
/// `evidence` is matched against; `detail` is what the user is shown.
///
/// They are separate because the reviewer's own prose must never drive classification. A
/// partial review mentioning line 429, or the phrase "does not support", would otherwise
/// be reported as RATE_LIMITED or MODEL_UNAVAILABLE, complete with remediation telling the
/// user to change --model over a coincidence in text. Only the CLI's own stderr/stdout is
/// evidence about what went wrong.
pub fn classify(
    reviewer: &str,
    model: &str,
    effort: &str,
    exit: Option<i32>,
    evidence: &str,
    detail: &str,
) -> Failure {
    let hay = evidence.to_ascii_lowercase();

    let has = |needles: &[&str]| needles.iter().any(|n| hay.contains(n));

    // Checked here rather than in each adapter, because a reviewer CLI can report an
    // expired session on a path where its structured output never arrives. Claude reports
    // it on stderr with stdout empty, so the JSON never parses and the adapter's own check
    // was unreachable. Classifying it here is what lets an expired resume surface cleanly as
    // SESSION_NOT_FOUND, which the caller acts on by retrying with fresh=true. (It once drove
    // an automatic retry into a fresh session; that retry was removed, but the classification
    // is still what makes the failure legible.)
    if has(&[
        "no conversation found",
        "session not found",
        "no rollout",
        "no session found",
    ]) {
        return session_not_found("(resumed session)", "unknown").with_detail(detail);
    }

    if has(&[
        "not logged in",
        "not authenticated",
        "please log in",
        "please login",
        "run `codex login`",
        "run codex login",
        "no credentials",
        "missing api key",
        "credentials not found",
        "authentication required",
    ]) {
        return not_authenticated(reviewer, detail);
    }

    if has(&[
        "401",
        "unauthorized",
        "invalid_api_key",
        "invalid api key",
        "token expired",
        "expired credentials",
        "oauth token",
    ]) {
        return auth_expired(reviewer, detail);
    }

    if has(&[
        "429",
        "rate limit",
        "rate_limit",
        "quota",
        "usage limit",
        "too many requests",
        "overloaded",
    ]) {
        return rate_limited(reviewer, detail);
    }

    if has(&[
        "unknown model",
        "model not found",
        "invalid model",
        "unsupported model",
        "model_not_found",
        "does not support",
        "invalid reasoning",
        "unknown reasoning",
        "invalid effort",
        "not a valid value for",
    ]) {
        return model_unavailable(reviewer, model, effort, detail);
    }

    reviewer_crashed(reviewer, exit, detail)
}
