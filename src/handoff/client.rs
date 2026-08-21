//! The initiating side of a handoff.
//!
//! Collapses "push here, pull there" into one action while keeping the decision
//! it represents explicit. Publishing is deliberately still a thing you ask for:
//! `sw verify` already moves uncommitted work to the peer without a remote, so the
//! only reason to push is that these commits should exist in the remote — and
//! automating that would mean auto-publishing, which is usually wrong.

use crate::config::{Config, Identity, Machine};
use crate::git;
use crate::style::Paint;
use crate::transport::{self, HandoffResult, PROTOCOL_VERSION, Request};
use anyhow::{Result, bail};
use std::path::Path;

pub struct Options {
    pub peer: Option<String>,
    /// Skip the push, for when the commits are already on the remote.
    pub no_push: bool,
}

pub async fn run(repo: &Path, opts: Options) -> Result<i32> {
    let cfg = Config::load(repo)?;
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;
    let (peer_name, peer) = machine.resolve_peer(opts.peer.as_deref())?;

    let branch = git::current_branch(repo).await?;
    let remote = &cfg.project.remote;

    if opts.no_push {
        eprintln!("{} {branch} (not pushing)", "handoff".bold());
    } else {
        eprintln!("{} {branch} -> {remote}", "push".bold());
        git::push_current(repo, remote).await?;
    }

    let project = cfg.project.name.clone();
    transport::with_endpoint(&identity, |ep| async move {
        let conn = transport::connect(&ep, &peer).await?;
        let exchange = async {
            let (mut tx, mut rx) = transport::open_request(&conn).await?;
            transport::write_json(
                &mut tx,
                &Request::Handoff {
                    protocol: PROTOCOL_VERSION,
                    project,
                    branch: branch.clone(),
                },
            )
            .await?;
            tx.finish().ok();

            let result: HandoffResult = transport::read_json(&mut rx).await?;
            anyhow::Ok(result)
        };
        let result = exchange.await.map_err(|e| transport::explain(&conn, e))?;
        report(&peer_name, &result);
        match result {
            HandoffResult::Failed { message } => bail!("{message}"),
            _ => Ok(0),
        }
    })
    .await
}

fn report(peer_name: &str, result: &HandoffResult) {
    match result {
        HandoffResult::FastForwarded {
            host,
            branch,
            from,
            to,
        } => {
            eprintln!("{} {host} {branch} {from} -> {to}", "  ok".green().bold());
        }
        // Not an error: the commits arrived, and the peer declining to move a
        // checkout it cannot safely move is the intended behaviour. Saying why is
        // the whole value.
        HandoffResult::Fetched {
            host,
            branch,
            at,
            why,
        } => {
            eprintln!("{} {host} fetched {branch} at {at}", "  ok".green().bold());
            eprintln!("  {} {why}", "note".yellow().bold());
        }
        HandoffResult::Failed { .. } => {
            eprintln!("{} {peer_name}", "fail".red().bold());
        }
    }
}
