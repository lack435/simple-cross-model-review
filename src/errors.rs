//! The failure contract.
//!
//! When a review cannot happen, the calling agent must not paper over it by
//! reviewing its own work. Every failure therefore carries a machine-readable code,
//! a plain statement of what broke, and the exact remediation to relay to the user.

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

pub fn cancelled() -> Failure {
    Failure::new(
        "CANCELLED",
        "The review was cancelled before it finished.",
        "The cross-model review was cancelled, so there is no review feedback. Start a new \
         review if one is still needed.",
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

impl Failure {
    /// True when the failure is the agent's own fault and it should just retry
    /// differently, rather than stopping to involve the user.
    pub fn is_agent_correctable(&self) -> bool {
        matches!(self.code, "BAD_REQUEST" | "SESSION_BUSY")
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
    // was unreachable -- which meant the automatic retry into a fresh session could never
    // fire, despite a test that appeared to cover it.
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
