//! The node side of a verify request.
//!
//! Prepares a worktree at the requested base with the requester's patch applied,
//! runs the recipe there, and streams every stage, log line, and diagnostic back.
//!
//! Events are produced by several stages but written by one. The channel between
//! them is what keeps the executor from needing to know anything about the wire,
//! and it is why `exec` is testable without a network.

use super::exec;
use crate::config::{Config, Machine, Platform};
use crate::git;
use crate::transport::{self, Event, PROTOCOL_VERSION, VerifyRequest, version_mismatch};
use anyhow::Result;
use iroh::endpoint::SendStream;
use std::time::Instant;
use tokio::sync::mpsc;

pub async fn handle(
    mut tx: SendStream,
    peer_name: &str,
    v: VerifyRequest,
    patch: Vec<u8>,
) -> Result<()> {
    let platform = Platform::current();

    let (etx, mut erx) = mpsc::channel::<Event>(1024);
    let label = peer_name.to_string();

    let work = async move {
        let etx = etx;
        let send = |ev: Event| {
            let etx = etx.clone();
            async move {
                let _ = etx.send(ev).await;
            }
        };

        if v.protocol != PROTOCOL_VERSION {
            send(Event::Failed {
                message: version_mismatch(v.protocol),
            })
            .await;
            return;
        }

        let repo = match Machine::resolve_project(&v.project) {
            Ok(r) => r,
            Err(message) => {
                send(Event::Failed { message }).await;
                return;
            }
        };

        let cfg = match Config::load(&repo) {
            Ok(c) => c,
            Err(e) => {
                send(Event::Failed {
                    message: format!("{e:#}"),
                })
                .await;
                return;
            }
        };
        // The registry could name a checkout of some other project entirely.
        if cfg.project.name != v.project {
            send(Event::Failed {
                message: format!(
                    "{} holds project `{}`, not `{}` — the registry entry is stale",
                    repo.display(),
                    cfg.project.name,
                    v.project
                ),
            })
            .await;
            return;
        }

        send(Event::Accepted {
            host: Machine::load().map(|m| m.node_name()).unwrap_or_default(),
            platform,
            repo: repo.display().to_string(),
        })
        .await;

        let recipe = match cfg.recipe(&v.recipe, platform) {
            Ok(r) => r,
            Err(e) => {
                send(Event::Failed {
                    message: format!("{e:#}"),
                })
                .await;
                return;
            }
        };

        let staged = etx.clone();
        let wt =
            match git::prepare_worktree(&repo, &v.base, &patch, &cfg.project.remote, &label, |s| {
                // try_send: a dropped progress line is preferable to blocking the
                // git plumbing, and the channel is sized well past the handful of
                // stages a prepare emits.
                let _ = staged.try_send(Event::Stage { name: s });
            })
            .await
            {
                Ok(w) => w,
                Err(git::PrepareError::MissingBase(base)) => {
                    send(Event::MissingBase { base }).await;
                    return;
                }
                Err(git::PrepareError::Other(e)) => {
                    send(Event::Failed {
                        message: format!("{e:#}"),
                    })
                    .await;
                    return;
                }
            };

        let started = Instant::now();
        let ev = match exec::run_recipe(&wt, &recipe, platform, v.color, &etx).await {
            Ok(code) => Event::Done {
                code,
                ms: started.elapsed().as_millis() as u64,
            },
            Err(e) => Event::Failed {
                message: format!("{e:#}"),
            },
        };
        // Mirrored into the node's own log: the requester sees the full build
        // output, but whoever is watching this machine should still see that a
        // request arrived and how it ended.
        match &ev {
            Event::Done { code, ms } => {
                tracing::info!(
                    "{label}: {}/{} -> exit {code} in {ms}ms",
                    v.project,
                    v.recipe
                )
            }
            Event::Failed { message } => {
                tracing::warn!("{label}: {}/{} -> {message}", v.project, v.recipe)
            }
            _ => {}
        }
        send(ev).await;
        // `etx` drops here, which is what ends the drain below.
    };

    let drain = async {
        while let Some(ev) = erx.recv().await {
            transport::write_json(&mut tx, &ev).await?;
        }
        anyhow::Ok(())
    };

    let (_, written) = tokio::join!(work, drain);
    written?;
    tx.finish().ok();
    Ok(())
}
