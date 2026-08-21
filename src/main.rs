//! `sw` — peer-to-peer cross-platform build verification.
//!
//! Both machines run this same binary. Neither is a server; either side can ask
//! the other to build the working tree it currently has, without committing,
//! pushing, or touching the remote. Two machines pair once, and every project
//! linked on both of them travels over that one relationship, served by a single
//! resident node per machine.
//!
//! ## Module map
//!
//! ```text
//! cli, commands   the command-line surface and local bookkeeping
//! config          two tiers: repo `syncwork.toml`, and machine-level state
//! transport       message types, framing, the authenticated QUIC endpoint
//! node            the resident accept loop; routes each request to its owner
//! verify          remote build verification — client, server, recipe executor
//! ctx             AI context sync — path derivation, merge policy, both ends
//! git             git plumbing: snapshots, worktrees, patches
//! service         keeping the node alive across reboots
//! style           terminal styling, switched off when output is not a terminal
//! ```
//!
//! `verify` and `ctx` each own *both* ends of their conversation. That symmetry is
//! deliberate: a change to either exchange almost always touches both halves, and
//! having one live in `node` while the other lived elsewhere was a standing
//! invitation to let them drift.

mod cli;
mod commands;
mod config;
mod ctx;
mod git;
mod node;
mod service;
mod style;
mod transport;
mod verify;

use crate::style::Paint;
use clap::Parser;

#[tokio::main]
async fn main() {
    // Decided once, before anything is printed.
    style::set_enabled(style::detect());

    tracing_subscriber::fmt()
        .with_env_filter(
            // Our own info lines (the node's per-request log) plus errors from
            // anywhere. Dependency *warnings* are deliberately excluded: they
            // describe transport internals the operator cannot act on, and they
            // interleave with our output mid-line, e.g.
            //
            //   peer mac-mini ... WARN noq_proto: failed closing path err=...
            //   error: could not reach peer; is `sw node` running there?
            //
            // where the actionable message is the second line. `SW_LOG=debug`
            // brings everything back when diagnosing connectivity.
            //
            // The crate name comes from the environment rather than being spelled
            // out: a tracing target defaults to `module_path!()`, whose root is
            // the *binary* name (`sw`), not the package name (`syncwork`).
            // Hard-coding the package name silently dropped every info event.
            tracing_subscriber::EnvFilter::try_from_env("SW_LOG")
                .unwrap_or_else(|_| format!("error,{}=info", env!("CARGO_CRATE_NAME")).into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = cli::Cli::parse();
    match commands::dispatch(cli).await {
        // A non-zero code here is the peer's build result, propagated verbatim so
        // `sw verify && ...` and agent-driven retry loops behave correctly.
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{} {e:#}", "error:".red().bold());
            std::process::exit(2);
        }
    }
}
