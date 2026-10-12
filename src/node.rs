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
use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use std::path::Path;

/// Name of the lock that enforces one node per state directory.
const LOCK_FILE: &str = "node.lock";

/// Take the machine's node lock, or explain who has it.
///
/// Two nodes on one machine share its identity and both register for it at the
/// relay, so a request lands on whichever won the race while the other sits idle —
/// and the loser may be running older code, or holding a worktree mid-build.
/// Testing this tool produced three at once without noticing.
///
/// An OS file lock rather than a pid file: the kernel drops it when the process
/// ends, however it ends, so there is no stale state to detect or clean up. The
/// returned handle must outlive the node, which is why it is returned rather than
/// dropped here.
fn acquire_lock() -> Result<std::fs::File> {
    let dir = crate::config::machine::home()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join(LOCK_FILE);
    let file = std::fs::File::create(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    // A few tries, not one: `is_running` takes a momentary shared lock to ask
    // the question, and a node starting in that instant must not conclude it
    // is the second one. A real second node holds the lock for life, so the
    // retries cannot mask it.
    let mut attempt = file.try_lock();
    for _ in 0..5 {
        if !matches!(attempt, Err(std::fs::TryLockError::WouldBlock)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        attempt = file.try_lock();
    }
    match attempt {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => bail!(
            "another `sw node` is already running on this machine — it holds {}. \
             Stop it with `sw service stop`, or set SW_HOME to run an independent \
             node against its own state directory.",
            path.display()
        ),
        // A filesystem that cannot lock (some network mounts) must not stop the
        // node from running: losing the guard is worse than losing the node.
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(
                "cannot lock {}: {e}; not checking for a second node",
                path.display()
            );
            Ok(file)
        }
    }
}

/// Whether this machine's node is running, by asking its lock rather than the
/// process table. Exact for this state directory, and costs one file open —
/// the process query (`service::running_nodes`) launches PowerShell on
/// Windows, far too heavy for anything that asks every few seconds.
///
/// `None` when the lock cannot be consulted at all.
pub fn is_running() -> Option<bool> {
    let path = crate::config::machine::home().ok()?.join(LOCK_FILE);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        // No lock file: no node has ever run with this state directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    // Shared, so two observers never block each other; released on drop.
    match file.try_lock_shared() {
        Ok(()) => Some(false),
        Err(std::fs::TryLockError::WouldBlock) => Some(true),
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

pub async fn serve() -> Result<()> {
    // Held for the lifetime of the node, released by the OS when it exits.
    let _lock = acquire_lock()?;
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;
    let id = identity.id_string();
    // The machine key, not a throwaway: this key IS the address peers dial, and
    // the node is the one process allowed to hold it on the wire.
    transport::with_machine_endpoint(
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

    // A connection may carry several requests; loop until the peer hangs up. Who
    // it speaks for is decided per request, from each envelope — see `identify`.
    let mut authed: Option<(String, String)> = None;
    loop {
        let (tx, mut rx) = match conn.accept_bi().await {
            Ok(pair) => pair,
            // Normal shutdown looks like an error on the accept side.
            Err(_) => return Ok(()),
        };
        let envelope: transport::Envelope = transport::read_json(&mut rx).await?;

        let (peer_name, peer_id) = match &authed {
            Some(v) => v.clone(),
            None => match identify(&conn, remote, &envelope).await {
                Some(v) => {
                    authed = Some(v.clone());
                    v
                }
                None => {
                    conn.close(1u8.into(), b"unpaired");
                    return Ok(());
                }
            },
        };

        // Where the peer says it lives, kept so this machine can dial back
        // without depending on a public directory being willing to resolve its
        // key. Logged when it changes, because "the peer became dialable" is
        // exactly the fact someone debugging an unreachable peer is looking for.
        if let Some(relay) = &envelope.relay {
            match Machine::record_relay(&peer_id, relay) {
                Ok(true) => tracing::info!("{peer_name}: reachable at {relay}"),
                Ok(false) => {}
                Err(e) => tracing::warn!("could not record {peer_name}'s address: {e:#}"),
            }
        }

        if let Err(e) = route(tx, envelope.request, rx, &peer_name).await {
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

/// Decide which paired machine a connection speaks for, or log why it does not.
///
/// Two accepted shapes. A client presents a delegation — its connection key is a
/// throwaway, signed by its machine key, because dialling with the machine key
/// itself would knock that machine's own node off the relay (the relay keeps one
/// active connection per key). A bare machine key in the peer table is also
/// accepted, which is what a pre-delegation peer sends.
///
/// Returns `(peer name, machine key)` — the machine key, not the connection key,
/// is what the relay cache and every log line should name.
async fn identify(
    conn: &Connection,
    remote: iroh::EndpointId,
    envelope: &transport::Envelope,
) -> Option<(String, String)> {
    // Re-read so a machine paired since the node started is recognised
    // immediately.
    let machine = Machine::load().ok()?;

    let outcome = match &envelope.from {
        Some(delegation) => transport::verify_delegation(&machine, remote, delegation),
        None => transport::authorize(&machine, remote),
    };
    match outcome {
        Ok((name, peer)) => Some((name, peer.id)),
        Err(e) => {
            let paths = transport::describe_paths(conn).await;
            // The full key, not a prefix: an unpaired sender has no entry to look
            // up, so this line is the only place the operator can see what key to
            // expect. Truncating it here is what made this hard to diagnose.
            let claimed = envelope
                .from
                .as_ref()
                .map(|d| format!(" claiming machine {}", d.id))
                .unwrap_or_default();
            tracing::warn!(
                "refused peer {remote}{claimed} via {paths}: {e:#}; known peers: [{}]. \
                 If that is your other machine, its key changed — re-pair with \
                 `sw pair init` / `sw pair join`",
                machine
                    .peers
                    .iter()
                    .map(|(n, p)| format!("{n}={:.12}", p.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            None
        }
    }
}

/// Hand one request to the module that owns it.
async fn route(
    mut tx: SendStream,
    req: Request,
    mut rx: RecvStream,
    peer_name: &str,
) -> Result<()> {
    // Requests are logged as they arrive. All the interesting output — stages,
    // build logs, diagnostics — is streamed to the *requester*, so without this
    // the node looks idle even while it is compiling.
    //
    // Pings are the exception: `sw tui` on the other machine sends one every
    // few seconds, and at info level they buried the verify and ctx lines
    // this log exists to show. `SW_LOG=debug` brings them back.
    if matches!(req, Request::Ping { .. }) {
        tracing::debug!("{peer_name}: {}", describe(&req));
    } else {
        tracing::info!("{peer_name}: {}", describe(&req));
    }
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
                            message: format!(
                                "{e:#} (scanning context on {})",
                                Machine::load().map(|m| m.node_name()).unwrap_or_default()
                            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard asks the lock, not the process table, so the lock's two
    /// states must read back correctly — and asking must not take the lock
    /// away from a node that starts a moment later.
    #[test]
    fn the_lock_answers_whether_a_node_is_running() {
        let _guard = crate::ctx::base::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("sw-nodelock-{}", crate::ctx::now_ms()));
        // SAFETY: the lock above serialises every test that touches SW_HOME.
        unsafe { std::env::set_var("SW_HOME", &home) };

        assert_eq!(is_running(), Some(false), "never ran");
        let held = acquire_lock().expect("first node takes the lock");
        assert_eq!(is_running(), Some(true), "lock held");
        drop(held);
        assert_eq!(is_running(), Some(false), "released on exit");
        // Asking leaves nothing held behind.
        let again = acquire_lock().expect("a node can still start after being asked about");
        drop(again);

        unsafe { std::env::remove_var("SW_HOME") };
        std::fs::remove_dir_all(&home).ok();
    }
}
