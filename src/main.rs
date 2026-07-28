//! cross-review: an MCP server that hands work to a different model for review.
//!
//! One request, one response. The calling agent decides what to do with the feedback.

mod config;
mod errors;
mod mcp;
mod prompt;
mod registry;
mod reviewer;
mod session;
mod tools;

use std::sync::Arc;

use config::{Config, USAGE};
use tools::{App, VERSION};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("cross-review {VERSION}");
        return;
    }

    let doctor = args.iter().any(|a| a == "--doctor");
    let args: Vec<String> = args.into_iter().filter(|a| a != "--doctor").collect();

    let cfg = match Config::from_args(&args) {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("cross-review: {message}\n");
            eprintln!("Run with --help for usage.");
            std::process::exit(2);
        }
    };

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
