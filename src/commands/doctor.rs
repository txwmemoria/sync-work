//! One command answering "why isn't this working".
//!
//! Every check here exists because it was needed and done by hand at least once:
//! comparing identities across two shells to find that one resolved a redirected
//! config directory, checking whether a node was running at all, noticing that a
//! peer was serving from a binary built before the fix. None of that should
//! require filesystem archaeology.
//!
//! Exit code is 1 if any check failed and 0 otherwise, so this composes in a
//! script. Warnings do not fail: "the peer is asleep" is not a broken setup.

use crate::config::{self, Config, Identity, Machine};
use crate::style::Paint;
use crate::transport::{self, Event, PROTOCOL_VERSION, Request};
use crate::{git, service};
use anyhow::Result;
use std::path::Path;

/// Tally so the exit code reflects what was found.
#[derive(Default)]
struct Report {
    failed: usize,
    warned: usize,
}

impl Report {
    fn ok(&mut self, label: &str, detail: impl std::fmt::Display) {
        println!("  {} {label:<11} {detail}", "✓".green());
    }
    fn warn(&mut self, label: &str, detail: impl std::fmt::Display) {
        self.warned += 1;
        println!("  {} {label:<11} {detail}", "!".yellow().bold());
    }
    fn fail(&mut self, label: &str, detail: impl std::fmt::Display) {
        self.failed += 1;
        println!("  {} {label:<11} {detail}", "✗".red().bold());
    }
    fn note(&self, detail: impl std::fmt::Display) {
        println!("      {}", detail.to_string().dimmed());
    }
}

pub async fn run(peer_filter: Option<&str>) -> Result<i32> {
    let mut r = Report::default();
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;

    println!("{}", "this machine".bold());
    r.ok("name", machine.node_name());
    r.ok("identity", format!("{:.16}…", identity.id_string()));
    // Printed because two shells on one machine can resolve different
    // directories — a sandboxed terminal may have its config path redirected,
    // which produces a second identity and a pairing that looks corrupt.
    match config::machine::home() {
        Ok(p) => r.ok("state", p.display()),
        Err(e) => r.fail("state", format!("{e:#}")),
    }
    r.ok("build", env!("SW_BUILD"));

    match service::running_nodes() {
        Some(0) => r.fail(
            "node",
            "not running — start it with `sw service start`, or `sw node` to watch it",
        ),
        Some(1) => r.ok("node", "running"),
        Some(n) => {
            // Two nodes share one identity and fight over the same endpoint.
            r.warn("node", format!("{n} running — expected one"));
            r.note("a manual `sw node` alongside the service will contend for the endpoint");
        }
        None => r.warn("node", "could not tell"),
    }
    if service::is_registered() {
        r.ok("autostart", "registered");
    } else {
        r.warn("autostart", "not registered — `sw service install`");
    }

    println!("\n{}", "projects".bold());
    if machine.projects.is_empty() {
        r.warn("none", "nothing linked — run `sw link` in a checkout");
    }
    for (name, entry) in &machine.projects {
        let path = Path::new(&entry.path);
        if !path.is_dir() {
            r.fail(name, format!("{} is gone", entry.path));
            continue;
        }
        match Config::load(path) {
            Err(_) => r.fail(
                name,
                format!("{} has no {}", entry.path, config::CONFIG_FILE),
            ),
            // A stale registry entry pointing at a different project would make
            // the peer build the wrong thing, so the name is checked, not assumed.
            Ok(cfg) if cfg.project.name != *name => {
                r.fail(name, format!("{} holds `{}`", entry.path, cfg.project.name));
            }
            Ok(_) => {
                let dirty = git::is_dirty(path).await.unwrap_or(false);
                let branch = git::current_branch(path)
                    .await
                    .unwrap_or_else(|_| "detached".into());
                let state = if dirty { ", dirty" } else { "" };
                r.ok(name, format!("{} ({branch}{state})", entry.path));
            }
        }
    }

    println!("\n{}", "peers".bold());
    if machine.peers.is_empty() {
        r.fail("none", "nothing paired — `sw pair init`");
    }
    for (name, peer) in &machine.peers {
        if let Some(want) = peer_filter
            && want != name
        {
            continue;
        }
        check_peer(&mut r, &identity, name, peer).await;
    }

    println!();
    if r.failed > 0 {
        println!(
            "{} {} failed, {} warned",
            "problems:".red().bold(),
            r.failed,
            r.warned
        );
        return Ok(1);
    }
    if r.warned > 0 {
        println!("{} {} warning(s)", "ok:".green().bold(), r.warned);
    } else {
        println!("{}", "all checks passed".green().bold());
    }
    Ok(0)
}

async fn check_peer(r: &mut Report, identity: &Identity, name: &str, peer: &crate::config::Peer) {
    let outcome = transport::with_endpoint(identity, |ep| {
        let peer = peer.clone();
        async move {
            let conn = transport::connect(&ep, &peer).await?;
            let (mut tx, mut rx) = transport::open_request(&conn).await?;
            transport::write_json(
                &mut tx,
                &Request::Ping {
                    protocol: PROTOCOL_VERSION,
                },
            )
            .await?;
            tx.finish().ok();
            let ev = transport::read_event(&mut rx).await?;
            let paths = transport::describe_paths(&conn).await;
            match ev {
                Some(e) => Ok((e, paths)),
                None => match transport::refusal(&conn) {
                    Some(explained) => anyhow::bail!("{explained}"),
                    None => anyhow::bail!("peer closed the connection without replying"),
                },
            }
        }
    })
    .await;

    match outcome {
        Ok((Event::Accepted { host, build, .. }, paths)) => {
            r.ok(name, format!("{host} reachable"));
            r.note(paths);
            // Reaching it at all proves the keys agree: the peer authorises by
            // public key, so a stale record on its side would have refused us.
            r.ok("keys", "agree (the peer accepted this machine's key)");
            let local = env!("SW_BUILD");
            if build.is_empty() {
                r.warn("build", "peer did not report one — it predates this check");
            } else if build != local {
                r.warn("build", format!("peer {build}, here {local}"));
                r.note("if you just updated, the peer's node is still on old code: `sw service restart` there");
            } else {
                r.ok("build", format!("{build} on both"));
            }
        }
        Ok((Event::Failed { message }, _)) => r.fail(name, message),
        Ok((other, _)) => r.fail(name, format!("unexpected reply: {other:?}")),
        Err(e) => {
            // Unreachable is a warning: the other machine being asleep is not a
            // broken configuration. A refusal is a failure, and says so itself.
            let text = format!("{e:#}");
            if text.contains("refused us as unpaired") {
                r.fail(name, "refused this machine");
                r.note(text);
            } else {
                r.warn(name, "not reachable");
                r.note(text);
            }
        }
    }
}
