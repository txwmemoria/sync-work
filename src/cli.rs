//! Command-line surface.
//!
//! Definitions only — every command's implementation lives in [`crate::commands`]
//! or, for the two capabilities that have a peer on the other end, in
//! [`crate::verify`] and [`crate::ctx`].
//!
//! Commands fall into two scopes, and the distinction is load-bearing:
//!
//! * **machine** — `pair`, `node`, `service`, `projects`, `status`. These must work
//!   from anywhere, *including outside a git repository*, because they concern the
//!   machine rather than any one checkout.
//! * **repository** — `init`, `link`, `verify`, `ctx`. These discover the repo from
//!   the working directory or `-C`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "sw",
    version,
    about = "Cross-platform build verification between peer machines",
    long_about = None
)]
pub struct Cli {
    /// Operate on this repo instead of discovering one from the current directory.
    #[arg(long, short = 'C', global = true, value_name = "DIR")]
    pub repo: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Write a starter syncwork.toml into this repo and register it.
    Init {
        /// Project name; defaults to the repository directory name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Register this checkout so the node here can serve it.
    Link,
    /// Forget a project on this machine.
    Unlink {
        /// Project name; defaults to the one in the current repo.
        name: Option<String>,
    },
    /// List the projects registered on this machine.
    Projects,
    /// Run the resident node. One per machine; serves every linked project.
    Node,
    /// Build the current working tree on the peer and report the result here.
    Verify {
        /// Recipe from syncwork.toml.
        #[arg(default_value = "ci")]
        recipe: String,
        /// Target peer; optional when exactly one machine is paired.
        #[arg(long)]
        peer: Option<String>,
        /// Snapshot against this revision instead of HEAD.
        #[arg(long)]
        base: Option<String>,
        /// Push the branch first. Not normally needed.
        #[arg(long)]
        push: bool,
    },
    /// Sync AI session context (Claude Code transcripts and memory) with the peer.
    Ctx {
        #[arg(long)]
        peer: Option<String>,
        /// Show what would move without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check that the peer is reachable and speaking the same protocol.
    Status {
        #[arg(long)]
        peer: Option<String>,
    },
    /// Pair machines. Done once, then every shared project uses it.
    #[command(subcommand)]
    Pair(PairCmd),
    /// Keep `sw node` running across reboots.
    #[command(subcommand)]
    Service(ServiceCmd),
}

#[derive(Subcommand)]
pub enum PairCmd {
    /// Print a ticket for the other machine to consume.
    Init {
        /// Name to record for this machine; defaults to the hostname.
        #[arg(long)]
        name: Option<String>,
    },
    /// Consume a ticket, then print one to send back.
    Join {
        ticket: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List paired machines.
    List,
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Register the node to start at login.
    Install {
        /// Show exactly what would be written and run, without doing it.
        #[arg(long)]
        print: bool,
    },
    /// Stop and start the service, so an updated binary takes effect.
    Restart,
    /// Stop the node. Needed before an update: `cargo install` replaces the
    /// binary the node is running from.
    Stop,
    /// Start the node.
    Start,
    /// Remove the registration.
    Uninstall,
    /// Report whether the node is registered.
    Status,
    /// Show the node's log, wherever this platform keeps it.
    Logs {
        /// Keep streaming new lines.
        #[arg(long, short = 'f')]
        follow: bool,
        /// How many existing lines to show first.
        #[arg(long, short = 'n', default_value = "50")]
        lines: u32,
        /// Print where the log lives and exit, for following it with your own
        /// tool. On Windows a long-running `sw` blocks the next `cargo install`.
        #[arg(long)]
        path: bool,
    },
}

/// Locate the repository for a repo-scoped command.
pub async fn discover(cli_repo: &Option<PathBuf>) -> Result<PathBuf> {
    let start = match cli_repo {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    crate::git::discover_root(&start).await
}
