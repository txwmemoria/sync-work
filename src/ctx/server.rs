//! The node side of a context sync.
//!
//! Both directions apply the same preserve-the-loser rule, so a push and a pull
//! of the same file behave identically.

use super::Applied;
use crate::config::{Config, Machine};
use crate::ctx;
use crate::transport::{self, CtxFrame};
use anyhow::{Context as _, Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use std::path::Path;

/// Answer a manifest request.
pub fn manifest(repo: &Path) -> Result<CtxFrame> {
    let cfg = Config::load(repo)?;
    let roots = ctx::resolve_roots(&cfg, repo);
    let mut out = Vec::new();
    for r in &roots {
        out.push(ctx::scan(r)?);
    }
    Ok(CtxFrame::Roots {
        host: Machine::load()?.node_name(),
        roots: out,
    })
}

/// Stream the requested files back to the caller.
pub async fn fetch(
    tx: &mut SendStream,
    repo: &Path,
    root_key: &str,
    paths: &[String],
) -> Result<()> {
    let cfg = Config::load(repo)?;
    let Some(root) = ctx::resolve_roots(&cfg, repo)
        .into_iter()
        .find(|r| r.key == root_key)
    else {
        transport::write_json(
            tx,
            &CtxFrame::Failed {
                message: format!("no root `{root_key}`"),
            },
        )
        .await?;
        return Ok(());
    };

    for p in paths {
        match ctx::read_for_send(&root.dir, p) {
            Ok((bytes, mtime_ms)) => {
                transport::write_json(
                    tx,
                    &CtxFrame::File {
                        path: p.clone(),
                        mtime_ms,
                        replaces: None,
                    },
                )
                .await?;
                transport::write_frame(tx, &bytes).await?;
            }
            // Omit rather than abort: one unreadable file should not fail the run.
            Err(e) => tracing::warn!("skipping {p}: {e:#}"),
        }
    }
    transport::write_json(tx, &CtxFrame::End).await?;
    Ok(())
}

/// Receive pushed files, applying the same preserve-the-loser rule the client
/// uses so the two directions behave identically.
pub async fn push(
    tx: &mut SendStream,
    rx: &mut RecvStream,
    repo: &Path,
    root_key: &str,
    peer_name: &str,
) -> Result<()> {
    let cfg = Config::load(repo)?;
    let Some(root) = ctx::resolve_roots(&cfg, repo)
        .into_iter()
        .find(|r| r.key == root_key)
    else {
        transport::write_json(
            tx,
            &CtxFrame::Failed {
                message: format!("no root `{root_key}`"),
            },
        )
        .await?;
        return Ok(());
    };
    std::fs::create_dir_all(&root.dir)
        .with_context(|| format!("creating {}", root.dir.display()))?;

    let project = cfg.project.name.as_str();
    loop {
        match transport::read_json::<CtxFrame>(rx).await? {
            CtxFrame::File {
                path,
                mtime_ms,
                replaces,
            } => {
                let bytes = transport::read_frame(rx).await?;
                // A merged file names the version it replaces. If that is still
                // exactly what we hold, the merge contains everything here and
                // may overwrite outright; if our copy moved on meanwhile, fall
                // through to the ordinary preserve-the-loser rules.
                let still_parent = replaces.is_some_and(|h| {
                    ctx::read_for_send(&root.dir, &path)
                        .map(|(cur, _)| ctx::hash_bytes(&cur) == h)
                        .unwrap_or(false)
                });
                let outcome = if still_parent {
                    ctx::merge::apply_exact(
                        &root.dir,
                        &path,
                        mtime_ms,
                        &bytes,
                        Some((project, root_key)),
                    )
                    .map(|()| {
                        tracing::info!("{path} merged (both sides kept)");
                        Applied::Replaced
                    })
                } else {
                    ctx::apply_incoming(
                        &root.dir,
                        &path,
                        mtime_ms,
                        &bytes,
                        peer_name,
                        Some((project, root_key)),
                    )
                };
                match outcome {
                    Ok(Applied::Conflict(b)) => {
                        tracing::warn!("{path} diverged; kept local as {}", b.display())
                    }
                    Ok(_) => {}
                    Err(e) => {
                        transport::write_json(
                            tx,
                            &CtxFrame::Failed {
                                message: format!("{e:#}"),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
            CtxFrame::End => {
                transport::write_json(tx, &CtxFrame::End).await?;
                return Ok(());
            }
            other => bail!("unexpected frame during push: {other:?}"),
        }
    }
}
