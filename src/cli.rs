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
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SW_BUILD"), ")"),
    about = "Cross-platform build verification between peer machines",
    long_about = None
)]
pub struct Cli {
    /// Operate on this repo instead of discovering one from the current directory.
    #[arg(long, short = 'C', global = true, value_name = "DIR")]
    pub repo: Option<PathBuf>,

    /// When to colourise output. `auto` decorates only a terminal.
    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
pub enum Command {
    /// Write a starter syncwork.toml into this repo and register it.
    ///
    /// Asks for anything not given by a flag, one question at a time — but only
    /// on a terminal. Piped or scripted runs take the defaults silently, so no
    /// automation can hang on a prompt.
    Init {
        /// Project name; defaults to the repository directory name.
        #[arg(long)]
        name: Option<String>,
        /// Git remote the peer fetches missing commits from.
        #[arg(long)]
        remote: Option<String>,
        /// Starter recipes to write.
        #[arg(long, value_enum)]
        template: Option<crate::config::Template>,
        /// Take the defaults for anything not given, without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List, add or remove recipes in syncwork.toml.
    Recipe {
        #[command(subcommand)]
        cmd: Option<RecipeCmd>,
    },
    /// List, add or remove context roots in syncwork.toml.
    Root {
        #[command(subcommand)]
        cmd: Option<RootCmd>,
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
    /// Clear this machine's identity, peers and project registry, to set up again.
    Reset {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Check everything: identity, node, projects, peers, and build agreement.
    Doctor {
        /// Only check this peer.
        #[arg(long)]
        peer: Option<String>,
    },
    /// Run the resident node. One per machine; serves every linked project.
    Node {
        /// Append the node's log to this file instead of writing to stderr.
        ///
        /// The node owns its log rather than having a shell redirect it, because
        /// on Windows there is no way to do both of "redirect the output" and
        /// "do not inherit the launching process's handles" — and inheriting them
        /// means any caller reading `sw service start`'s output waits forever on a
        /// pipe the node holds for life. Redirection through PowerShell also wrote
        /// UTF-16 and wrapped every stderr line in an error record.
        #[arg(long, value_name = "PATH")]
        log: Option<std::path::PathBuf>,
    },
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
        /// Exit 0 when the peer is unreachable instead of failing. For hooks and
        /// other unattended callers.
        #[arg(long)]
        if_reachable: bool,
        /// Keep running, syncing whenever the local context changes.
        ///
        /// Complements the Stop hook rather than replacing it: the hook covers
        /// "a session ended", this covers a long session you want to continue on
        /// the other machine, and pulling what that machine writes without being
        /// asked. Implies `--if-reachable`, since a sleeping peer is normal for
        /// something left running.
        #[arg(long, conflicts_with = "dry_run")]
        watch: bool,
    },
    /// Push the current branch, then have the peer fetch and fast-forward it.
    Handoff {
        #[arg(long)]
        peer: Option<String>,
        /// Skip the push, for when the commits are already on the remote.
        #[arg(long)]
        no_push: bool,
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
    /// Run `sw ctx` automatically when a Claude Code session ends.
    #[command(subcommand)]
    Hook(HookCmd),
}

#[derive(Subcommand)]
pub enum RecipeCmd {
    /// Show every recipe and its steps (the default).
    List,
    /// Add a recipe, or replace one that exists. Each STEP is one shell command.
    ///
    /// With --platform, sets that platform's override on an existing recipe
    /// instead of the base steps.
    Add {
        name: String,
        /// Shell commands, run in order on the peer. Quote each one.
        #[arg(required = true)]
        steps: Vec<String>,
        /// Environment for the steps, as KEY=VALUE. Repeatable.
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Override steps for one platform only (windows / macos / linux).
        #[arg(long)]
        platform: Option<String>,
    },
    /// Remove a recipe.
    Remove { name: String },
}

#[derive(Subcommand)]
pub enum RootCmd {
    /// Show every context root and where it resolves on this machine (the default).
    List,
    /// Declare a context root to sync.
    Add {
        name: String,
        /// claude-project, home, or repo.
        #[arg(long)]
        kind: String,
        /// For home/repo kinds: the relative path.
        #[arg(long)]
        path: Option<String>,
        /// For the home kind: environment variable that overrides the base.
        #[arg(long)]
        env: Option<String>,
    },
    /// Remove a context root.
    Remove { name: String },
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
pub enum HookCmd {
    /// Add the Stop hook to .claude/settings.local.json.
    Install,
    /// Remove it.
    Uninstall,
    /// Report whether it is installed.
    Status,
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
