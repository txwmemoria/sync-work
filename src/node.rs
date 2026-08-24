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
use crate::{ctx, git, handoff, verify};
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
    // Recorded here because this is the one place that has waited to come online,
    // which makes it the only reliable answer to "where can this machine be
    // dialled". Every short-lived command reads it from the config afterwards.
    transport::note_self_relay(&ep, 0).await;

    // Logged rather than printed, so that it reaches the log file when the node
    // runs detached under `--log`. What is in this banner — which identity, which
    // build, which peers and projects — is exactly what someone reading the log
    // after the fact needs, and it used to be the one thing missing from it.
    tracing::info!(
        "node {id} build {} on {}",
        env!("SW_BUILD"),
        Platform::current()
    );
    tracing::info!("name {}", machine.node_name());
    if machine.peers.is_empty() {
        tracing::warn!("no peers paired — `sw pair init` here, `sw pair join <ticket>` there");
    } else {
        tracing::info!(
            "peers {}",
            machine.peers.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if machine.projects.is_empty() {
        tracing::warn!("no projects linked — run `sw link` in a checkout");
    } else {
        for (name, entry) in &machine.projects {
            let path = Path::new(&entry.path);
            if path.is_dir() {
                // `git worktree` records absolute paths, so a checkout that moved
                // leaves stale administrative entries that break the next verify.
                git::prune_worktrees(path).await;
                tracing::info!("project {name} -> {}", entry.path);
            } else {
                tracing::warn!("project {name} -> {} is missing", entry.path);
            }
        }
    }
    tracing::info!("waiting for requests");

    loop {
        let Some(incoming) = ep.accept().await else {
            break;
        };
        // Re-checked per connection so a machine that moved networks stops
        // advertising an address it left behind. Writes only on change, so this is
        // a comparison in the ordinary case.
        transport::note_self_relay(&ep, 0).await;
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_connection(conn).await {
                        tracing::error!("connection failed: {e:#}");
                    }
                }
                Err(e) => tracing::warn!("handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

async fn handle_connection(conn: Connection) -> Result<()> {
    let remote = conn.remote_id();

    // Re-read so a machine paired since startup is recognised immediately.
    let machine = Machine::load()?;
    let paths = transport::describe_paths(&conn).await;
    let (peer_name, _peer) = match transport::authorize(&machine, remote) {
        Ok(v) => v,
        Err(_) => {
            // The full key, not a prefix: an unpaired sender has no entry to look
            // up, so this line is the only place the operator can see what key to
            // expect. Truncating it here is what made this hard to diagnose.
            tracing::warn!(
                "refused unpaired peer {remote} via {paths}; \
                 known peers: [{}]. If that is your other machine, its key changed \
                 — re-pair with `sw pair init` / `sw pair join`",
                machine
                    .peers
                    .iter()
                    .map(|(n, p)| format!("{n}={:.12}", p.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            conn.close(1u8.into(), b"unpaired");
            return Ok(());
        }
    };
    // Debug, not info: the path sampled here is always the relay. A connection
    // starts relayed and only gains a direct path once holepunching completes,
    // so reporting it at connect time would claim every transfer went through
    // the relay when most of the bytes did not.
    tracing::debug!("{peer_name} connected, initial path {paths}");

    // A connection may carry several requests; loop until the peer hangs up.
    loop {
        let (tx, rx) = match conn.accept_bi().await {
            Ok(pair) => pair,
            // Normal shutdown looks like an error on the accept side.
            Err(_) => return Ok(()),
        };
        if let Err(e) = route(tx, rx, &peer_name, &remote.to_string()).await {
            tracing::error!("{peer_name}: request failed: {e:#}");
        }
        // Sampled after the work, which is when the answer is meaningful: by now
        // holepunching has had time to succeed, so this says whether the request
        // actually travelled peer-to-peer.
        tracing::info!(
            "{peer_name}: served via {}",
            transport::describe_paths(&conn).await
        );
    }
}

/// Read one request and hand it to the module that owns it.
async fn route(
    mut tx: SendStream,
    mut rx: RecvStream,
    peer_name: &str,
    peer_id: &str,
) -> Result<()> {
    let envelope: transport::Envelope = transport::read_json(&mut rx).await?;
    // Where the peer says it lives, kept so this machine can dial back without
    // depending on a public directory being willing to resolve its key. Logged
    // when it changes, because "the peer became dialable" is exactly the fact
    // someone debugging an unreachable peer is looking for.
    if let Some(relay) = &envelope.relay {
        match Machine::record_relay(peer_id, relay) {
            Ok(true) => tracing::info!("{peer_name}: reachable at {relay}"),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not record {peer_name}'s address: {e:#}"),
        }
    }
    let req = envelope.request;
    // Requests are logged as they arrive. All the interesting output — stages,
    // build logs, diagnostics — is streamed to the *requester*, so without this
    // the node looks idle even while it is compiling.
    tracing::info!("{peer_name}: {}", describe(&req));
    match req {
        Request::Ping { protocol } => {
            let machine = Machine::load()?;
            let ev = if protocol == PROTOCOL_VERSION {
                Event::Accepted {
                    host: machine.node_name(),
                    platform: Platform::current(),
                    build: env!("SW_BUILD").to_string(),
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

        Request::Handoff {
            protocol,
            project,
            branch,
        } => {
            let result = if protocol != PROTOCOL_VERSION {
                transport::HandoffResult::Failed {
                    message: version_mismatch(protocol),
                }
            } else {
                handoff::server::handle(&project, &branch).await
            };
            transport::write_json(&mut tx, &result).await?;
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

/// One-line summary of a request, for the node's log.
fn describe(req: &Request) -> String {
    match req {
        Request::Ping { .. } => "ping".into(),
        Request::Verify(v) => format!("verify {}/{}", v.project, v.recipe),
        Request::CtxManifest { project, .. } => format!("ctx manifest {project}"),
        Request::CtxFetch { project, paths, .. } => {
            format!("ctx fetch {project} ({} files)", paths.len())
        }
        Request::CtxPush { project, .. } => format!("ctx push {project}"),
        Request::Handoff {
            project, branch, ..
        } => format!("handoff {project} {branch}"),
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
    ping_exchange(&conn)
        .await
        .map_err(|e| transport::explain(&conn, e))
}

async fn ping_exchange(conn: &iroh::endpoint::Connection) -> Result<()> {
    let (mut tx, mut rx) = transport::open_request(conn).await?;
    transport::write_request(
        &mut tx,
        Request::Ping {
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
            build,
        }) => {
            println!("{}", "up".green().bold());
            println!("  name     {host}");
            println!("  platform {platform}");
            println!("  projects {repo}");
            println!(
                "  build    {}",
                if build.is_empty() {
                    "(not reported)".to_string()
                } else {
                    build
                }
            );
            // Read after the exchange, not before: a connection begins relayed and
            // only gains a direct path once holepunching succeeds, so sampling it
            // immediately after `connect` would almost always say "relay".
            println!("  path     {}", transport::describe_paths(conn).await);
            Ok(())
        }
        Some(Event::Failed { message }) => {
            println!("{}", "error".red().bold());
            bail!("{message}")
        }
        None => {
            println!("{}", "error".red().bold());
            // A clean end of stream with nothing on it is what a refusal looks
            // like once the request is already in flight.
            match transport::refusal(conn) {
                Some(explained) => bail!("{explained}"),
                None => bail!("peer closed the connection without replying"),
            }
        }
        other => bail!("unexpected reply from peer: {other:?}"),
    }
}
