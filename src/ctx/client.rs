//! The initiating side of a context sync.

use super::{Applied, Root};
use crate::config::{Config, Identity, Machine, Peer};
use crate::ctx;
use crate::style::Paint;
use crate::transport::{self, CtxFrame, CtxRoot, PROTOCOL_VERSION, Request};
use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use std::path::Path;

pub struct Options {
    pub peer: Option<String>,
    pub dry_run: bool,
    /// Treat an unreachable peer as success.
    ///
    /// For unattended use — the Stop hook fires at the end of every session,
    /// including on a laptop with the other machine asleep, and an error there
    /// would be noise about a condition the user cannot act on. A *refused*
    /// peer is still an error: that is a misconfiguration, not a transient.
    pub if_reachable: bool,
    /// Keep running, syncing whenever the local roots change.
    ///
    /// The Stop hook already covers "a session ended", which is the moment that
    /// matters most. This covers the rest: a long session where you want to move
    /// machines mid-thought, and pulling what the *other* machine wrote without
    /// having to remember to ask.
    pub watch: bool,
}

/// How often `--watch` looks at the local roots. Cheap — names, sizes and mtimes,
/// no file contents — so this can be short enough to feel immediate.
const POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Polls between syncs that run even with nothing changed locally, so the *peer's*
/// writes get pulled. Watching only local files would make this a one-way sync.
const HEARTBEAT_POLLS: u32 = 30;

/// Totals reported at the end of a run.
#[derive(Default)]
struct Totals {
    pulled: usize,
    pushed: usize,
    extended: usize,
    conflicts: usize,
    skipped: usize,
    identical: usize,
}

impl Options {
    /// Whether to narrate a sync that did nothing.
    ///
    /// A one-shot run should say what it found, even if that is "in sync" — it was
    /// asked a question and silence is not an answer. A watch is the opposite: it
    /// runs for hours and must only speak when something actually moved.
    fn chatty(&self) -> bool {
        !self.watch
    }
}

pub async fn sync(repo: &Path, mut opts: Options) -> Result<i32> {
    let cfg = Config::load(repo)?;
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;
    let (peer_name, peer) = machine.resolve_peer(opts.peer.as_deref())?;

    let roots = ctx::resolve_roots(&cfg, repo);
    if roots.is_empty() {
        bail!(
            "nothing to sync: no context roots resolve on this machine. Declare one \
             under `[context.roots]` in {}",
            crate::config::CONFIG_FILE
        );
    }
    if opts.watch && opts.dry_run {
        bail!(
            "--watch and --dry-run contradict each other: one waits to write, the other never does"
        );
    }
    // A watch is unattended by definition, and the other machine being asleep is
    // the normal state, not a failure to report every minute.
    opts.if_reachable |= opts.watch;

    // One endpoint for the whole run, including a watch that lasts hours: binding
    // costs a socket and a relay handshake, and paying that every two seconds would
    // dwarf the sync.
    transport::with_endpoint(&identity, |ep| async move {
        if opts.watch {
            watch(&ep, &cfg, &peer, &peer_name, &roots, &opts).await
        } else {
            sync_with(&ep, &cfg, &peer, &peer_name, &roots, &opts).await
        }
    })
    .await
}

/// Sync on every local change, and periodically regardless, until interrupted.
///
/// Failures do not end the loop. The whole point is to be left running, and the
/// peer going to sleep, the network dropping, or a laptop lid closing are all
/// normal — a watch that exited on the first of them would be useless precisely
/// when it is meant to be unattended.
async fn watch(
    ep: &iroh::Endpoint,
    cfg: &Config,
    peer: &Peer,
    peer_name: &str,
    roots: &[Root],
    opts: &Options,
) -> Result<i32> {
    eprintln!(
        "{} {peer_name}, {} root{} — syncing on change, ctrl-c to stop",
        "watch".bold(),
        roots.len(),
        if roots.len() == 1 { "" } else { "s" }
    );

    let mut seen = ctx::fingerprint(roots);
    // Starts at the limit so the first pass syncs immediately rather than after a
    // minute of looking idle.
    let mut polls_since_sync = HEARTBEAT_POLLS;

    loop {
        let now = ctx::fingerprint(roots);
        if now != seen || polls_since_sync >= HEARTBEAT_POLLS {
            seen = now;
            polls_since_sync = 0;
            // Reported and swallowed: see above. `if_reachable` already keeps a
            // sleeping peer quiet, so anything that reaches here is worth a line.
            if let Err(e) = sync_with(ep, cfg, peer, peer_name, roots, opts).await {
                eprintln!("{} {e:#}", "watch".yellow().bold());
            }
        } else {
            polls_since_sync += 1;
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("{} stopped", "watch".dimmed());
                return Ok(0);
            }
            _ = tokio::time::sleep(POLL) => {}
        }
    }
}

async fn sync_with(
    ep: &iroh::Endpoint,
    cfg: &Config,
    peer: &Peer,
    peer_name: &str,
    roots: &[Root],
    opts: &Options,
) -> Result<i32> {
    let conn = match transport::connect(ep, peer).await {
        Ok(c) => c,
        Err(e) if opts.if_reachable => {
            if opts.chatty() {
                eprintln!("{} {peer_name} is not reachable, skipping", "ctx".dimmed());
            }
            tracing::debug!("skipped context sync: {e:#}");
            return Ok(0);
        }
        Err(e) => return Err(e),
    };

    // Wrapped once, rather than at each `?` inside: a refusal can surface from
    // any step of the exchange, in a different shape each time.
    exchange(&conn, cfg, peer_name, roots, opts)
        .await
        .map_err(|e| transport::explain(&conn, e))
}

async fn exchange(
    conn: &iroh::endpoint::Connection,
    cfg: &Config,
    peer_name: &str,
    roots: &[Root],
    opts: &Options,
) -> Result<i32> {
    // One manifest exchange covers every root, so the plan is computed from a
    // single consistent view rather than drifting between round trips.
    let project = &cfg.project.name;
    let (remote_host, remote_roots) = fetch_manifests(conn, project).await?;
    if opts.chatty() {
        eprintln!("{} {peer_name} ({remote_host})", "ctx".bold());
    }

    let quiet_ms = cfg.context.quiet_secs.saturating_mul(1000);
    let now = ctx::now_ms();
    let mut totals = Totals::default();

    for root in roots {
        let local = ctx::scan(root)?;
        let Some(remote) = remote_roots.iter().find(|r| r.key == root.key) else {
            eprintln!(
                "  {} {} — peer does not have this root configured",
                "skip".yellow(),
                root.key
            );
            continue;
        };
        if !local.present && !remote.present {
            continue;
        }

        let plan = ctx::plan(&local, remote, now, quiet_ms);
        totals.identical += plan.identical;
        totals.skipped += plan.skipped.len();

        let label = root.key.cyan().to_string();
        if plan.is_empty() && plan.skipped.is_empty() {
            if opts.chatty() {
                eprintln!("  {label} in sync ({} files)", plan.identical);
            }
        } else {
            eprintln!(
                "  {label}  {} to pull, {} to push, {} identical",
                plan.pull.len(),
                plan.push.len(),
                plan.identical
            );
        }
        for (p, why) in &plan.skipped {
            eprintln!("    {} {p} ({why})", "skip".yellow());
        }

        if opts.dry_run {
            for p in &plan.pull {
                eprintln!("    {} {p}", "pull".green());
            }
            for p in &plan.push {
                eprintln!("    {} {p}", "push".blue());
            }
            continue;
        }

        if !plan.pull.is_empty() {
            pull(conn, project, root, &plan.pull, &remote_host, &mut totals).await?;
        }
        if !plan.push.is_empty() {
            push(conn, project, root, &plan.push, &mut totals).await?;
        }
    }

    if opts.dry_run {
        eprintln!("{} nothing written", "dry-run".dimmed());
        return Ok(0);
    }

    // A watch that reported "0 pulled, 0 pushed" every minute would bury the lines
    // that matter.
    if !opts.chatty() && totals.pulled == 0 && totals.pushed == 0 && totals.conflicts == 0 {
        return Ok(0);
    }
    let mut parts = vec![
        format!("{} pulled", totals.pulled),
        format!("{} pushed", totals.pushed),
    ];
    if totals.extended > 0 {
        parts.push(format!("{} appended", totals.extended));
    }
    if totals.skipped > 0 {
        parts.push(format!("{} skipped", totals.skipped));
    }
    eprintln!("{} {}", "  ok".green().bold(), parts.join(", "));

    if totals.conflicts > 0 {
        eprintln!(
            "{} {} file{} diverged; the local version was preserved \
             as *.conflict-<host>-<stamp>",
            "note".yellow().bold(),
            totals.conflicts,
            if totals.conflicts == 1 { "" } else { "s" },
        );
    }
    Ok(0)
}

async fn fetch_manifests(conn: &Connection, project: &str) -> Result<(String, Vec<CtxRoot>)> {
    let (mut tx, mut rx) = transport::open_request(conn).await?;
    transport::write_request(
        &mut tx,
        Request::CtxManifest {
            protocol: PROTOCOL_VERSION,
            project: project.to_string(),
        },
    )
    .await?;
    tx.finish().ok();
    match transport::read_json::<CtxFrame>(&mut rx).await? {
        CtxFrame::Roots { host, roots } => Ok((host, roots)),
        CtxFrame::Failed { message } => bail!("{message}"),
        other => bail!("unexpected reply to manifest request: {other:?}"),
    }
}

async fn pull(
    conn: &Connection,
    project: &str,
    root: &Root,
    paths: &[String],
    remote_host: &str,
    totals: &mut Totals,
) -> Result<()> {
    let (mut tx, mut rx) = transport::open_request(conn).await?;
    transport::write_request(
        &mut tx,
        Request::CtxFetch {
            project: project.to_string(),
            root: root.key.clone(),
            paths: paths.to_vec(),
        },
    )
    .await?;
    tx.finish().ok();

    loop {
        match transport::read_json::<CtxFrame>(&mut rx).await? {
            CtxFrame::File { path, mtime_ms } => {
                let bytes = transport::read_frame(&mut rx).await?;
                let applied = ctx::apply_incoming(&root.dir, &path, mtime_ms, &bytes, remote_host)?;
                totals.pulled += 1;
                report(&path, &applied, totals, "pull");
            }
            CtxFrame::End => return Ok(()),
            CtxFrame::Failed { message } => bail!("peer refused fetch: {message}"),
            other => bail!("unexpected frame during fetch: {other:?}"),
        }
    }
}

async fn push(
    conn: &Connection,
    project: &str,
    root: &Root,
    paths: &[String],
    totals: &mut Totals,
) -> Result<()> {
    let (mut tx, mut rx) = transport::open_request(conn).await?;
    transport::write_request(
        &mut tx,
        Request::CtxPush {
            project: project.to_string(),
            root: root.key.clone(),
        },
    )
    .await?;

    for p in paths {
        let (bytes, mtime_ms) = match ctx::read_for_send(&root.dir, p) {
            Ok(v) => v,
            // A session file can vanish between the scan and the send.
            Err(e) => {
                eprintln!("    {} {p} ({e})", "skip".yellow());
                continue;
            }
        };
        transport::write_json(
            &mut tx,
            &CtxFrame::File {
                path: p.clone(),
                mtime_ms,
            },
        )
        .await?;
        transport::write_frame(&mut tx, &bytes).await?;
        totals.pushed += 1;
        eprintln!("    {} {p}", "push".blue());
    }
    transport::write_json(&mut tx, &CtxFrame::End).await?;
    tx.finish().ok();

    match transport::read_json::<CtxFrame>(&mut rx).await? {
        CtxFrame::End => Ok(()),
        CtxFrame::Failed { message } => bail!("peer refused push: {message}"),
        other => bail!("unexpected reply to push: {other:?}"),
    }
}

fn report(path: &str, applied: &Applied, totals: &mut Totals, verb: &str) {
    match applied {
        Applied::Conflict(backup) => {
            totals.conflicts += 1;
            eprintln!(
                "    {} {path} (diverged; kept local copy as {})",
                "conflict".red().bold(),
                backup.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        Applied::Extended => {
            totals.extended += 1;
            eprintln!("    {} {path} {}", verb.green(), "(appended)".dimmed());
        }
        _ => eprintln!("    {} {path}", verb.green()),
    }
}
