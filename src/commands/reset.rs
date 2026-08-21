//! Clear this machine's syncwork state so it can be set up again from scratch.
//!
//! Deliberately destructive and deliberately narrow. It removes the machine's
//! identity, its peer table and its project registry — everything under the state
//! directory — and stops and unregisters the node. It does not touch anything the
//! machine does not own: a repository's committed `syncwork.toml` belongs to the
//! project, and AI context belongs to Claude Code.
//!
//! The peer is left holding a key that no longer exists, so pairing has to be
//! redone on *both* machines. That is stated before anything is deleted, because a
//! reset that quietly breaks the other machine would be a bad surprise.

use crate::config::{Identity, Machine, machine};
use crate::service;
use crate::style::Paint;
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};

pub fn run(assume_yes: bool) -> Result<i32> {
    let dir = machine::home()?;
    if !dir.exists() {
        println!("nothing to reset — {} does not exist", dir.display());
        return Ok(0);
    }

    // Loaded before the prompt so the summary says what is actually being lost,
    // not just which directory is going away.
    let machine = Machine::load().unwrap_or_default();
    let identity = Identity::load_or_create().ok();

    println!("{}", "this will remove".bold());
    println!("  {}", dir.display());
    if let Some(id) = &identity {
        println!("    identity  {:.16}…", id.id_string());
    }
    println!(
        "    peers     {}",
        if machine.peers.is_empty() {
            "none".to_string()
        } else {
            machine.peers.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    );
    println!(
        "    projects  {}",
        if machine.projects.is_empty() {
            "none".to_string()
        } else {
            machine
                .projects
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    // The service is machine-global: it starts a node that reads the *default*
    // state directory, never an SW_HOME override. So resetting an overridden
    // directory must leave it alone — otherwise testing against a throwaway
    // directory would silently unregister the real autostart, which is exactly
    // what happened once.
    let owns_service = std::env::var_os("SW_HOME").is_none();
    if owns_service && service::is_registered() {
        println!("  the autostart registration, and the running node");
    }
    println!();
    println!("{}", "it will NOT touch".bold());
    println!("  any repository's syncwork.toml, or its .sw/ build caches");
    println!("  AI session context under ~/.claude");
    if !machine.peers.is_empty() {
        println!();
        println!(
            "{} this machine gets a new identity, so {} will refuse it until you",
            "note".yellow().bold(),
            machine
                .peers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(" and ")
        );
        println!("     pair again — on both sides.");
    }

    if !assume_yes {
        // A prompt nobody can answer is worse than refusing outright, so a
        // non-interactive caller has to say so explicitly.
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal; pass --yes to reset without confirmation");
        }
        print!("\ntype `reset` to confirm: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("could not read confirmation")?;
        if line.trim() != "reset" {
            println!("cancelled");
            return Ok(1);
        }
    }

    // Stop before deleting: on Windows nothing else will, and a node left running
    // would keep serving with an identity whose file is gone.
    if owns_service {
        let _ = service::stop();
        if service::is_registered() {
            let _ = service::uninstall();
        }
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;

    println!();
    println!("{} {}", "removed".green().bold(), dir.display());
    println!();
    println!("To set up again:");
    println!("  {}   on this machine", "sw pair init".cyan());
    println!("  {}   on the other one", "sw pair join <ticket>".cyan());
    println!("  {}         in each checkout you verify", "sw link".cyan());
    println!("  {} to start the node again", "sw service install".cyan());
    Ok(0)
}
