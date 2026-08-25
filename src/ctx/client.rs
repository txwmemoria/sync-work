//! The initiating side of a context sync.

use super::{Applied, Root};
use crate::config::{Config, Identity, Machine, Peer};
use crate::ctx;
use crate::style::Paint;
use crate::transport::{self, CtxFrame, CtxRoot, PROTOCOL_VERSION, Request};
use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use std::path::Path;

/// Every progress line goes through this instead of `eprintln!`.
///
/// The prefix is empty for a one-shot run and a log-format timestamp under
/// `--watch` — lines that arrive hours apart carry *when* as part of the
/// information, and matching the log's format means a watch line and a node-log
/// line about the same sync can be read side by side.
macro_rules! say {
    ($($arg:tt)*) => {
        eprintln!("{}{}", crate::style::stamp(), format_args!($($arg)*))
    };
}

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
    merged: usize,
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
    // From here on every line is stamped: watch output arrives over hours, and a
    // pull line without a time answers only half the question it raises.
    crate::style::set_stamped(true);
    say!(
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
                say!("{} {e:#}", "watch".yellow().bold());
            }
        } else {
            polls_since_sync += 1;
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                say!("{} stopped", "watch".dimmed());
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
                say!("{} {peer_name} is not reachable, skipping", "ctx".dimmed());
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
        say!("{} {peer_name} ({remote_host})", "ctx".bold());
    }

    let quiet_ms = cfg.context.quiet_secs.saturating_mul(1000);
    let now = ctx::now_ms();
    let mut totals = Totals::default();

    for root in roots {
        let local = ctx::scan(root)?;
        let Some(remote) = remote_roots.iter().find(|r| r.key == root.key) else {
            say!(
                "  {} {} — peer does not have this root configured",
                "skip".yellow(),
                root.key
            );
            continue;
        };
        if !local.present && !remote.present {
            continue;
        }

        let plan = ctx::plan(&local, remote, now, quiet_ms, &|p| {
            ctx::base::mergeable(p) && ctx::base::load(project, &root.key, p).is_some()
        });
        totals.identical += plan.identical.len();
        totals.skipped += plan.skipped.len();

        // A moment of known convergence is exactly when the merge base should be
        // recorded — it is also how existing deployments grow bases at all.
        for p in &plan.identical {
            if ctx::base::mergeable(p)
                && let Ok((bytes, _)) = ctx::read_for_send(&root.dir, p)
                && let Err(e) = ctx::base::store(project, &root.key, p, &bytes)
            {
                tracing::debug!("could not record merge base for {p}: {e:#}");
            }
        }

        let label = root.key.cyan().to_string();
        if plan.is_empty() && plan.skipped.is_empty() {
            if opts.chatty() {
                say!("  {label} in sync ({} files)", plan.identical.len());
            }
        } else {
            say!(
                "  {label}  {} to pull, {} to push, {} to merge, {} identical",
                plan.pull.len(),
                plan.push.len(),
                plan.reconcile.len(),
                plan.identical.len()
            );
        }
        for (p, why) in &plan.skipped {
            say!("    {} {p} ({why})", "skip".yellow());
        }

        if opts.dry_run {
            for p in &plan.pull {
                say!("    {} {p}", "pull".green());
            }
            for p in &plan.push {
                say!("    {} {p}", "push".blue());
            }
            for p in &plan.reconcile {
                say!("    {} {p}", "merge".cyan());
            }
            continue;
        }

        if !plan.pull.is_empty() {
            pull(conn, project, root, &plan.pull, &remote_host, &mut totals).await?;
        }
        if !plan.push.is_empty() {
            push(
                conn,
                project,
                root,
                &plan.push,
                &Default::default(),
                &mut totals,
            )
            .await?;
        }
        if !plan.reconcile.is_empty() {
            reconcile(
                conn,
                project,
                root,
                &plan.reconcile,
                remote,
                &remote_host,
                &mut totals,
            )
            .await?;
        }
    }

    if opts.dry_run {
        say!("{} nothing written", "dry-run".dimmed());
        return Ok(0);
    }

    // A watch that reported "0 pulled, 0 pushed" every minute would bury the lines
    // that matter.
    if !opts.chatty()
        && totals.pulled == 0
        && totals.pushed == 0
        && totals.merged == 0
        && totals.conflicts == 0
    {
        return Ok(0);
    }
    let mut parts = vec![
        format!("{} pulled", totals.pulled),
        format!("{} pushed", totals.pushed),
    ];
    if totals.merged > 0 {
        parts.push(format!("{} merged", totals.merged));
    }
    if totals.extended > 0 {
        parts.push(format!("{} appended", totals.extended));
    }
    if totals.skipped > 0 {
        parts.push(format!("{} skipped", totals.skipped));
    }
    say!("{} {}", "  ok".green().bold(), parts.join(", "));

    if totals.conflicts > 0 {
        say!(
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
            CtxFrame::File { path, mtime_ms, .. } => {
                let bytes = transport::read_frame(&mut rx).await?;
                let applied = ctx::apply_incoming(
                    &root.dir,
                    &path,
                    mtime_ms,
                    &bytes,
                    remote_host,
                    Some((project, &root.key)),
                )?;
                totals.pulled += 1;
                report(&path, &applied, totals, "pull");
            }
            CtxFrame::End => return Ok(()),
            CtxFrame::Failed { message } => bail!("peer refused fetch: {message}"),
            other => bail!("unexpected frame during fetch: {other:?}"),
        }
    }
}

/// Merge files that changed on both sides, using the recorded common ancestor.
///
/// The merge happens entirely here, on the client: it fetches the remote
/// version, merges three-way against its base, writes the result locally, and
/// pushes it back tagged with the hash of the remote version it consumed. The
/// peer needs no base store of its own — it just checks that its file still is
/// that version and overwrites. When the same lines changed on both sides the
/// merge is refused, and the file falls back to the old newer-wins rule, whose
/// worst case is the conflict copy — never markers spliced into the file.
#[allow(clippy::too_many_arguments)]
async fn reconcile(
    conn: &Connection,
    project: &str,
    root: &Root,
    paths: &[String],
    remote: &CtxRoot,
    remote_host: &str,
    totals: &mut Totals,
) -> Result<()> {
    let theirs = fetch_raw(conn, project, root, paths).await?;

    let mut merged_paths = Vec::new();
    let mut replaces = std::collections::BTreeMap::new();
    for (path, their_mtime, their_bytes) in &theirs {
        let (our_bytes, our_mtime) = match ctx::read_for_send(&root.dir, path) {
            Ok(v) => v,
            Err(e) => {
                say!("    {} {path} ({e})", "skip".yellow());
                continue;
            }
        };
        let base = ctx::base::load(project, &root.key, path);

        let outcome = base
            .as_deref()
            .and_then(|b| ctx::merge::three_way(b, &our_bytes, their_bytes));
        match outcome {
            Some(merged) => {
                // Written with a fresh mtime: the merge postdates both parents.
                ctx::merge::apply_exact(&root.dir, path, ctx::now_ms(), &merged, None)?;
                let their_hash = remote
                    .entries
                    .iter()
                    .find(|e| &e.path == path)
                    .map(|e| e.hash.clone())
                    .unwrap_or_else(|| ctx::hash_bytes(their_bytes));
                replaces.insert(path.clone(), their_hash);
                merged_paths.push(path.clone());
            }
            None => {
                // Same lines edited on both sides (or not text): the merge would
                // have to guess, so it refuses, and the file takes the same path
                // it always did — newer side wins, the loser is preserved.
                if *their_mtime > our_mtime {
                    let applied = ctx::apply_incoming(
                        &root.dir,
                        path,
                        *their_mtime,
                        their_bytes,
                        remote_host,
                        Some((project, &root.key)),
                    )?;
                    totals.pulled += 1;
                    report(path, &applied, totals, "pull");
                } else {
                    push(
                        conn,
                        project,
                        root,
                        std::slice::from_ref(path),
                        &Default::default(),
                        totals,
                    )
                    .await?;
                }
            }
        }
    }

    if !merged_paths.is_empty() {
        push(conn, project, root, &merged_paths, &replaces, totals).await?;
    }
    Ok(())
}

/// Fetch files without applying them — the reconcile path needs the raw bytes.
async fn fetch_raw(
    conn: &Connection,
    project: &str,
    root: &Root,
    paths: &[String],
) -> Result<Vec<(String, u64, Vec<u8>)>> {
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

    let mut out = Vec::new();
    loop {
        match transport::read_json::<CtxFrame>(&mut rx).await? {
            CtxFrame::File { path, mtime_ms, .. } => {
                let bytes = transport::read_frame(&mut rx).await?;
                out.push((path, mtime_ms, bytes));
            }
            CtxFrame::End => return Ok(out),
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
    replaces: &std::collections::BTreeMap<String, String>,
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
                say!("    {} {p} ({e})", "skip".yellow());
                continue;
            }
        };
        transport::write_json(
            &mut tx,
            &CtxFrame::File {
                path: p.clone(),
                mtime_ms,
                replaces: replaces.get(p).cloned(),
            },
        )
        .await?;
        transport::write_frame(&mut tx, &bytes).await?;
        if replaces.contains_key(p) {
            totals.merged += 1;
            say!("    {} {p} (kept both sides)", "merge".cyan());
        } else {
            totals.pushed += 1;
            say!("    {} {p}", "push".blue());
        }
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
            say!(
                "    {} {path} (diverged; kept local copy as {})",
                "conflict".red().bold(),
                backup.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        Applied::Extended => {
            totals.extended += 1;
            say!("    {} {path} {}", verb.green(), "(appended)".dimmed());
        }
        _ => say!("    {} {path}", verb.green()),
    }
}
