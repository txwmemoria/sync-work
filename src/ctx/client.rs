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
}

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

pub async fn sync(repo: &Path, opts: Options) -> Result<i32> {
    let cfg = Config::load(repo)?;
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;
    let (peer_name, peer) = machine.resolve_peer(opts.peer.as_deref())?;

    let roots = ctx::resolve_roots(&cfg, repo);
    if roots.is_empty() {
        bail!("nothing to sync: [context] has claude = false and no extra paths");
    }

    let ep = transport::bind(&identity).await?;
    let out = sync_with(&ep, &cfg, &peer, &peer_name, &roots, &opts).await;
    ep.close().await;
    out
}

async fn sync_with(
    ep: &iroh::Endpoint,
    cfg: &Config,
    peer: &Peer,
    peer_name: &str,
    roots: &[Root],
    opts: &Options,
) -> Result<i32> {
    let conn = transport::connect(ep, peer).await?;

    // One manifest exchange covers every root, so the plan is computed from a
    // single consistent view rather than drifting between round trips.
    let project = &cfg.project.name;
    let (remote_host, remote_roots) = fetch_manifests(&conn, project).await?;
    eprintln!("{} {peer_name} ({remote_host})", "ctx".bold());

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
            eprintln!("  {label} in sync ({} files)", plan.identical);
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
            pull(&conn, project, root, &plan.pull, &remote_host, &mut totals).await?;
        }
        if !plan.push.is_empty() {
            push(&conn, project, root, &plan.push, &mut totals).await?;
        }
    }

    if opts.dry_run {
        eprintln!("{} nothing written", "dry-run".dimmed());
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
    let (mut tx, mut rx) = conn.open_bi().await?;
    transport::write_json(
        &mut tx,
        &Request::CtxManifest {
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
    let (mut tx, mut rx) = conn.open_bi().await?;
    transport::write_json(
        &mut tx,
        &Request::CtxFetch {
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
    let (mut tx, mut rx) = conn.open_bi().await?;
    transport::write_json(
        &mut tx,
        &Request::CtxPush {
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
