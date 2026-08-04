//! cross-review: an MCP server that hands work to a different model for review.
//!
//! One request, one response. The calling agent decides what to do with the feedback.

// Windows-only by design: the tool drives Windows CLIs, uses job objects for process-tree
// cleanup and share-mode locking for its state file. Said here so a build on another host
// fails with the reason rather than a pile of unrelated-looking errors.
#[cfg(not(windows))]
compile_error!(
    "cross-review targets Windows only: it depends on job objects and Windows file \
     share-mode locking."
);

mod cancel;
mod config;
mod errors;
mod mcp;
mod metrics;
mod prompt;
mod registry;
mod reviewer;
mod session;
#[cfg(test)]
mod testutil;
mod tools;
mod vcs;
mod winjob;

use std::sync::Arc;

use config::{Config, USAGE};
use tools::{version_line, App};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_line());
        return;
    }

    let doctor = args.iter().any(|a| a == "--doctor");
    let usage_report = args.iter().any(|a| a == "--usage");
    let args: Vec<String> = args
        .into_iter()
        .filter(|a| a != "--doctor" && a != "--usage")
        .collect();

    let cfg = match Config::from_args(&args) {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("cross-review: {message}\n");
            eprintln!("Run with --help for usage.");
            std::process::exit(2);
        }
    };

    if usage_report {
        // Ahead of the state-directory creation below, so this really is read-only: the
        // README promises `--usage` only reads the local log, and creating a directory
        // to report on would contradict that -- most visibly when it is pointed at a
        // path collected from other machines. No CLI is launched and nothing is billed.
        print!("{}", App::new(cfg).usage_report());
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&cfg.state_dir) {
        // Not fatal: reviews still work, only resuming across restarts is lost.
        eprintln!(
            "cross-review: warning: could not create state directory {}: {e}",
            cfg.state_dir.display()
        );
    }

    let app = Arc::new(App::new(cfg));

    if doctor {
        // Human-facing preflight, so a misconfiguration can be found without
        // starting an agent session.
        print!("{}", app.status());
        return;
    }

    mcp::serve(app);
}
