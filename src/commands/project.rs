//! Project bookkeeping: create a config, register a checkout, list, forget.
//!
//! `link` is the piece that makes one node able to serve many repositories. It
//! records "project name -> path on *this* machine", which is exactly the mapping
//! that cannot be committed, because the two machines disagree on it.

use crate::config::{self, Config, Machine};
use crate::git;
use crate::style::Paint;
use anyhow::{Result, bail};
use std::path::Path;

pub async fn init(repo: &Path, name: Option<String>) -> Result<i32> {
    let name = match name {
        Some(n) => n,
        None => repo
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".into()),
    };
    let path = repo.join(config::CONFIG_FILE);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    Config::template(&name).save(repo)?;
    git::ensure_ignored(repo).await?;
    link(repo, &name, false)?;

    println!("{} {}", "created".green().bold(), path.display());
    println!("  added .sw/ to .gitignore");
    println!();
    println!(
        "{}",
        "Commit syncwork.toml, then in your clone on the other machine:".bold()
    );
    println!("  {}", "sw link".cyan());
    println!();
    println!("If the machines are not paired yet, do that once — from anywhere:");
    println!("  {}", "sw pair init".cyan());
    Ok(0)
}

/// Record where this project lives on this machine.
///
/// `quiet` is set for the implicit call made by `verify` and `ctx`: registering
/// the checkout you are standing in is never surprising, so it should stay silent
/// unless it actually changed something.
pub fn link(repo: &Path, project: &str, quiet: bool) -> Result<()> {
    let mut m = Machine::load()?;
    if m.link(project, repo) {
        m.save()?;
        println!(
            "{} {project} -> {}",
            "linked".green().bold(),
            repo.display()
        );
        if !quiet {
            println!("  do the same in your clone on the other machine");
        }
    } else if !quiet {
        println!("{project} already linked to {}", repo.display());
    }
    Ok(())
}

pub fn unlink(name: &str) -> Result<i32> {
    let mut m = Machine::load()?;
    if m.unlink(name) {
        m.save()?;
        println!("unlinked {name}");
    } else {
        println!("{name} was not linked");
    }
    Ok(0)
}

pub fn list() -> Result<i32> {
    let m = Machine::load()?;
    if m.projects.is_empty() {
        println!("no projects linked; run `sw link` in a checkout");
        return Ok(0);
    }
    for (name, entry) in &m.projects {
        // A path that no longer holds a syncwork repo is the common stale-entry
        // case — a moved or deleted checkout — so it is worth flagging here
        // rather than at the moment a peer's request fails.
        let ok = Path::new(&entry.path).join(config::CONFIG_FILE).exists();
        let note = if ok {
            String::new()
        } else {
            format!("  {}", "(missing or not a syncwork repo)".red())
        };
        println!("{}{}", name.bold(), note);
        println!("  {}", entry.path);
    }
    Ok(0)
}
