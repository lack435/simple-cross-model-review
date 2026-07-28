//! Prompt assembly.
//!
//! The caller's instructions are passed through verbatim. All we add is a preamble
//! that tells the reviewer what role it is playing and what shape of answer is useful,
//! and only on the first turn of a session -- a resumed session already has it.

use std::path::Path;

pub const DEFAULT_PREAMBLE: &str = r#"You are an independent code reviewer. You are a different model from the agent that wrote this work and asked for this review, and that is the entire point: find the real problems it could not see in its own output.

Ground rules:
- Your access to this repository is read-only. Do not attempt to create, modify, or delete files, and do not run commands that change state. Read-only shell commands (git diff, git log, git show, ripgrep) are available.
- Read the code before judging it. Verify claims against what is actually there rather than what the request says is there.
- Cite concrete locations as `path/to/file.ext:123`.
- Order findings by what actually matters: correctness first, then security, then broken contracts and interfaces, then maintainability. Skip pure style preferences unless you were asked about them.
- Separate what you verified from what you suspect. If you could not check something, say so instead of guessing.
- Be accurate about severity in both directions. Do not soften a real defect to be agreeable, and do not manufacture findings to look thorough. "I found nothing wrong" is a valid and useful review when it is true.

Structure your response like this:

## Verdict
One of APPROVE, APPROVE WITH COMMENTS, REQUEST CHANGES, or BLOCKED, plus one sentence saying why.

## Findings
For each finding: severity (critical / major / minor), the location, what is wrong, why it matters, and a concrete suggested fix. If there are no findings, say so plainly.

## What I could not check
Anything you lacked the access or context to verify. Omit this section if it is empty."#;

pub const FOLLOW_UP_GUIDANCE: &str = "This is a follow-up turn in the same review session. Re-review with your earlier findings in mind, and state explicitly which of your previous findings are now resolved, which are still open, and whether the new work introduced anything new. Use the same response structure as before.";

pub struct PromptParts<'a> {
    pub instructions: &'a str,
    pub context_paths: &'a [String],
    pub cwd: &'a Path,
    pub turn: u32,
    pub resumed: bool,
    pub preamble: Option<&'a str>,
}

pub fn build(parts: &PromptParts) -> String {
    let mut out = String::new();

    if !parts.resumed {
        if let Some(preamble) = parts.preamble {
            out.push_str(preamble.trim_end());
            out.push_str("\n\n");
        }
        out.push_str("## Review request\n\n");
    } else {
        out.push_str(&format!(
            "## Follow-up review request (turn {})\n\n",
            parts.turn
        ));
    }

    out.push_str(parts.instructions.trim());
    out.push('\n');

    if !parts.context_paths.is_empty() {
        out.push_str("\n## Paths the requesting agent flagged\n\n");
        for path in parts.context_paths {
            out.push_str(&format!("- {path}\n"));
        }
        out.push_str(
            "\nThese are starting points, not boundaries. Read whatever else you need in order \
             to judge the work.\n",
        );
    }

    if !parts.resumed {
        out.push_str(&format!(
            "\n## Working directory\n\n{}\n",
            parts.cwd.display()
        ));
    }

    if parts.resumed {
        out.push_str(&format!("\n{FOLLOW_UP_GUIDANCE}\n"));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures() -> (PathBuf, Vec<String>) {
        (PathBuf::from("C:\\repo"), vec!["src/a.rs".to_string()])
    }

    #[test]
    fn first_turn_includes_preamble_and_cwd() {
        let (cwd, paths) = fixtures();
        let text = build(&PromptParts {
            instructions: "  Review the parser change.  ",
            context_paths: &paths,
            cwd: &cwd,
            turn: 1,
            resumed: false,
            preamble: Some(DEFAULT_PREAMBLE),
        });
        assert!(text.contains("independent code reviewer"));
        assert!(text.contains("## Review request"));
        // Instructions are passed through verbatim, just trimmed.
        assert!(text.contains("Review the parser change."));
        assert!(text.contains("- src/a.rs"));
        assert!(text.contains("C:\\repo"));
        assert!(!text.contains("Follow-up"));
    }

    #[test]
    fn resumed_turn_omits_preamble_and_asks_for_deltas() {
        let (cwd, paths) = fixtures();
        let text = build(&PromptParts {
            instructions: "I addressed the nil deref.",
            context_paths: &paths,
            cwd: &cwd,
            turn: 3,
            resumed: true,
            preamble: Some(DEFAULT_PREAMBLE),
        });
        assert!(!text.contains("independent code reviewer"));
        assert!(text.contains("## Follow-up review request (turn 3)"));
        assert!(text.contains("which of your previous findings are now resolved"));
        // The working directory was already established on turn 1.
        assert!(!text.contains("## Working directory"));
    }

    #[test]
    fn preamble_can_be_suppressed() {
        let (cwd, paths) = fixtures();
        let text = build(&PromptParts {
            instructions: "just look at it",
            context_paths: &paths,
            cwd: &cwd,
            turn: 1,
            resumed: false,
            preamble: None,
        });
        assert!(!text.contains("independent code reviewer"));
        assert!(text.starts_with("## Review request"));
    }

    #[test]
    fn no_context_paths_means_no_paths_section() {
        let text = build(&PromptParts {
            instructions: "x",
            context_paths: &[],
            cwd: Path::new("C:\\repo"),
            turn: 1,
            resumed: false,
            preamble: None,
        });
        assert!(!text.contains("flagged"));
    }
}
