//! The resident node: accept requests from paired machines and route them.
//!
//! One node per machine, serving every project registered on it. Each request
//! names a project; the node resolves that name against its own registry to find
//! the local checkout. Nothing about the requester's filesystem layout is trusted
//! or even transmitted.
//!
//! This module is only the router. The work behind each request lives with the
//! feature that owns it — [`crate::verify::server`] and [`crate::ctx::server`] —
//! so each capability keeps both ends of its own conversation in one place.
//!
//! Machine state and each project's `syncwork.toml` are re-read per request
//! rather than cached at startup. Linking a project, pairing a machine, or editing
//! a recipe are all normal things to do while the node is up, and needing to
//! restart it to pick those up would be a papercut in exactly the moment you are
//! trying to get set up.

use crate::config::{Identity, Machine, Platform};
use crate::style::Paint;
use crate::transport::{self, CtxFrame, Event, PROTOCOL_VERSION, Request, version_mismatch};
use crate::{ctx, git, verify};
use anyhow::{Result, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use std::path::Path;

pub async fn serve() -> Result<()> {
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;
    let id = identity.id_string();
    transport::with_endpoint(
        &identity,
        |ep| async move { accept_loop(ep, machine, id).await },
    )
    .await
}

async fn accept_loop(ep: iroh::Endpoint, machine: Machine, id: String) -> Result<()> {
    ep.online().await;

    eprintln!("{} {}", "node".green().bold(), id.dimmed());
    eprintln!("  name     {}", machine.node_name());
    eprintln!("  platform {}", Platform::current());
    if machine.peers.is_empty() {
        eprintln!(
            "  {} no peers paired — run `sw pair init` here, `sw pair join <ticket>` there",
            "!".yellow()
        );
    } else {
        eprintln!(
            "  peers    {}",
            machine.peers.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if machine.projects.is_empty() {
        eprintln!(
            "  {} no projects linked — run `sw link` in a checkout",
            "!".yellow()
        );
    } else {
        eprintln!("  projects");
        for (name, entry) in &machine.projects {
            let path = Path::new(&entry.path);
            let missing = if path.is_dir() {
                // `git worktree` records absolute paths, so a checkout that moved
                // leaves stale administrative entries that break the next verify.
                git::prune_worktrees(path).await;
                String::new()
            } else {
                format!("  {}", "(path missing)".red())
            };
            eprintln!("    {name} -> {}{missing}", entry.path);
        }
    }
    eprintln!("{}", "waiting for requests (ctrl-c to stop)".dimmed());

    loop {
        let Some(incoming) = ep.accept().await else {
            break;
        };
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_connection(conn).await {
                        eprintln!("{} {e:#}", "connection failed:".red());
                    }
                }
                Err(e) => eprintln!("{} {e}", "handshake failed:".red()),
            }
        });
    }
    Ok(())
}

async fn handle_connection(conn: Connection) -> Result<()> {
    let remote = conn.remote_id();

    // Re-read so a machine paired since startup is recognised immediately.
    let machine = Machine::load()?;
    let (peer_name, _peer) = match transport::authorize(&machine, remote) {
        Ok(v) => v,
        Err(e) => {
            // Close rather than reply: an unpaired caller learns nothing about
            // this machine beyond the fact that something is listening.
            eprintln!("{} {e}", "refused:".yellow());
            conn.close(1u8.into(), b"unpaired");
            return Ok(());
        }
    };

    // A connection may carry several requests; loop until the peer hangs up.
    loop {
        let (tx, rx) = match conn.accept_bi().await {
            Ok(pair) => pair,
            // Normal shutdown looks like an error on the accept side.
            Err(_) => return Ok(()),
        };
        if let Err(e) = route(tx, rx, &peer_name).await {
            eprintln!("{} {e:#}", "request failed:".red());
        }
    }
}

/// Read one request and hand it to the module that owns it.
async fn route(mut tx: SendStream, mut rx: RecvStream, peer_name: &str) -> Result<()> {
    let req: Request = transport::read_json(&mut rx).await?;
    match req {
        Request::Ping { protocol } => {
            let machine = Machine::load()?;
            let ev = if protocol == PROTOCOL_VERSION {
                Event::Accepted {
                    host: machine.node_name(),
                    platform: Platform::current(),
                    // A ping has no single repo, so report what is served instead.
                    repo: if machine.projects.is_empty() {
                        "no projects linked".into()
                    } else {
                        machine
                            .projects
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                }
            } else {
                Event::Failed {
                    message: version_mismatch(protocol),
                }
            };
            transport::write_json(&mut tx, &ev).await?;
            tx.finish().ok();
            Ok(())
        }

        Request::Verify(v) => {
            // The patch frame always follows a Verify, even when empty, so it must
            // be read before the request can be rejected for any reason.
            let patch = transport::read_frame(&mut rx).await?;
            verify::server::handle(tx, peer_name, v, patch).await
        }

        Request::CtxManifest { protocol, project } => {
            let frame = if protocol != PROTOCOL_VERSION {
                CtxFrame::Failed {
                    message: version_mismatch(protocol),
                }
            } else {
                match Machine::resolve_project(&project) {
                    Err(message) => CtxFrame::Failed { message },
                    Ok(repo) => match ctx::server::manifest(&repo) {
                        Ok(f) => f,
                        Err(e) => CtxFrame::Failed {
                            message: format!("{e:#}"),
                        },
                    },
                }
            };
            transport::write_json(&mut tx, &frame).await?;
            tx.finish().ok();
            Ok(())
        }

        Request::CtxFetch {
            project,
            root,
            paths,
        } => {
            match Machine::resolve_project(&project) {
                Err(message) => {
                    transport::write_json(&mut tx, &CtxFrame::Failed { message }).await?
                }
                Ok(repo) => ctx::server::fetch(&mut tx, &repo, &root, &paths).await?,
            }
            tx.finish().ok();
            Ok(())
        }

        Request::CtxPush { project, root } => {
            match Machine::resolve_project(&project) {
                Err(message) => {
                    transport::write_json(&mut tx, &CtxFrame::Failed { message }).await?
                }
                Ok(repo) => ctx::server::push(&mut tx, &mut rx, &repo, &root, peer_name).await?,
            }
            tx.finish().ok();
            Ok(())
        }
    }
}

/// Handshake with a peer without running anything — backs `sw status`.
pub async fn ping(peer_name: Option<&str>) -> Result<()> {
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;

    println!("{} {}", "this machine".bold(), machine.node_name());
    println!("  id       {:.16}…", identity.id_string());
    println!("  platform {}", Platform::current());
    println!(
        "  projects {}",
        if machine.projects.is_empty() {
            "none linked".to_string()
        } else {
            machine
                .projects
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    // Resolved before binding. On an unpaired machine — which is exactly the
    // state `sw status` gets run in first — there is nobody to dial, so opening a
    // socket only to fail would be wasted work and would report the failure
    // underneath a line of iroh's own shutdown noise.
    let (name, peer) = machine.resolve_peer(peer_name)?;
    print!("{} {} ... ", "peer".bold(), name);
    use std::io::Write;
    std::io::stdout().flush().ok();

    transport::with_endpoint(&identity, |ep| async move { ping_peer(&ep, &peer).await }).await
}

async fn ping_peer(ep: &iroh::Endpoint, peer: &crate::config::Peer) -> Result<()> {
    let conn = transport::connect(ep, peer).await?;
    let (mut tx, mut rx) = conn.open_bi().await?;
    transport::write_json(
        &mut tx,
        &Request::Ping {
            protocol: PROTOCOL_VERSION,
        },
    )
    .await?;
    tx.finish().ok();

    match transport::read_event(&mut rx).await? {
        Some(Event::Accepted {
            host,
            platform,
            repo,
        }) => {
            println!("{}", "up".green().bold());
            println!("  name     {host}");
            println!("  platform {platform}");
            println!("  projects {repo}");
            Ok(())
        }
        Some(Event::Failed { message }) => {
            println!("{}", "error".red().bold());
            bail!("{message}")
        }
        other => bail!("unexpected reply from peer: {other:?}"),
    }
}
