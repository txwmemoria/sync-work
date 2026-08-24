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
mod handoff;
mod node;
mod service;
mod style;
mod transport;
mod verify;

use crate::style::Paint;
use clap::Parser;

/// Open the node's log file, creating it and appending.
///
/// Appends rather than truncates: a node restarted to pick up a new binary is the
/// moment its previous run's last words matter most.
fn open_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// A timestamp formatter in the machine's own timezone, with its offset shown.
///
/// The log is read by a person, next to a clock — a node log stamped in UTC has to
/// be mentally converted every time, and correlating "this is when I hit ctrl-c"
/// with a line in the peer's log is the whole reason the timestamps are there. The
/// offset is kept in the output (`+08:00`) so a line pasted into a message is still
/// unambiguous.
///
/// This must run before the async runtime exists, which is why `main` builds that
/// runtime by hand instead of using `#[tokio::main]`. Reading the local zone is
/// only sound in a single-threaded process — another thread could be in `setenv`
/// while `localtime_r` reads the environment — so the `time` crate refuses once the
/// process is multi-threaded, and tokio's worker threads are already running by the
/// time an `#[tokio::main]` body starts. That refusal is silent: it looks exactly
/// like "local time is not supported here", and the log stays UTC on macOS and
/// Linux while working on Windows.
fn local_timer()
-> Option<tracing_subscriber::fmt::time::OffsetTime<impl time::formatting::Formattable>> {
    let offset = time::UtcOffset::current_local_offset().ok()?;
    // Also handed to `style`, whose stamped output lines (`sw ctx --watch`) must
    // read exactly like the log. The format itself lives there — one definition,
    // so the two cannot drift.
    style::set_local_offset(offset);
    Some(tracing_subscriber::fmt::time::OffsetTime::new(
        offset,
        style::TIME_FORMAT,
    ))
}

fn main() {
    // First, before any thread exists. See `local_timer`.
    let timer = local_timer();

    // Parsed before the subscriber is built, because `sw node --log` decides where
    // the log goes and clap is the only thing that knows.
    let cli = cli::Cli::parse();
    let log_file = match &cli.command {
        cli::Command::Node { log: Some(path) } => match open_log(path) {
            Ok(f) => Some(f),
            // Reported and then ignored: a node that cannot write its log is still
            // a working node, and refusing to start would take the whole machine
            // out of service over a file permission.
            Err(e) => {
                eprintln!(
                    "{} could not open {}: {e}",
                    "warning:".yellow(),
                    path.display()
                );
                None
            }
        },
        _ => None,
    };

    let builder = tracing_subscriber::fmt().with_env_filter(
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
    );
    // Escape codes are for terminals and nowhere else. `tracing-subscriber` colours
    // its output whenever the feature is compiled in, without looking at where that
    // output goes — so every redirected node log was carrying escapes: launchd's on
    // macOS, systemd's, and the shell-redirected one the tests read. A log is
    // grepped, so this follows the same rule as the rest of our output.
    //
    // A missing timer falls back to the default UTC one: a machine whose timezone
    // cannot be read should still log with timestamps.
    let ansi = log_file.is_none() && style::detect();
    let builder = builder.with_ansi(ansi);
    match (log_file, timer) {
        (Some(file), Some(t)) => builder.with_writer(file).with_timer(t).init(),
        (Some(file), None) => builder.with_writer(file).init(),
        (None, Some(t)) => builder.with_writer(std::io::stderr).with_timer(t).init(),
        (None, None) => builder.with_writer(std::io::stderr).init(),
    }

    // Applied before anything is printed, so `--color` covers the whole run,
    // including the error path below.
    style::set_enabled(match cli.color {
        cli::ColorWhen::Always => true,
        cli::ColorWhen::Never => false,
        cli::ColorWhen::Auto => style::detect(),
    });

    // Built here rather than by `#[tokio::main]` so that reading the local
    // timezone above happens while this process is still single-threaded.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!(
                "{} could not start the async runtime: {e}",
                "error:".red().bold()
            );
            std::process::exit(2);
        }
    };
    match runtime.block_on(commands::dispatch(cli)) {
        // A non-zero code here is the peer's build result, propagated verbatim so
        // `sw verify && ...` and agent-driven retry loops behave correctly.
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{} {e:#}", "error:".red().bold());
            std::process::exit(2);
        }
    }
}
