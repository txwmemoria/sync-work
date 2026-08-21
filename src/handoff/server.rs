//! The node side of a handoff.
//!
//! Fetches, then advances the checkout only when doing so is unambiguously safe.
//! Everything here is a refusal to guess: the peer's working tree belongs to
//! whoever is sitting at it, and silently moving it — or worse, merging — would be
//! a far more expensive surprise than an extra manual `git pull`.
//!
//! Safe means all four: the requested branch is the one checked out, the tree is
//! clean, the remote ref exists, and the local branch is strictly behind it. Any
//! other combination fetches and reports why it stopped.

use crate::config::{Config, Machine};
use crate::git;
use crate::transport::HandoffResult;
use std::path::Path;

pub async fn handle(project: &str, branch: &str) -> HandoffResult {
    let host = Machine::load().map(|m| m.node_name()).unwrap_or_default();

    let repo = match Machine::resolve_project(project) {
        Ok(r) => r,
        Err(message) => return HandoffResult::Failed { message },
    };
    let cfg = match Config::load(&repo) {
        Ok(c) => c,
        Err(e) => {
            return HandoffResult::Failed {
                message: format!("{e:#}"),
            };
        }
    };

    match run(&repo, &cfg.project.remote, branch, &host).await {
        Ok(result) => result,
        Err(e) => HandoffResult::Failed {
            message: format!("{e:#}"),
        },
    }
}

async fn run(repo: &Path, remote: &str, branch: &str, host: &str) -> anyhow::Result<HandoffResult> {
    git::fetch(repo, remote).await?;

    let target = format!("{remote}/{branch}");
    if !git::has_ref(repo, &target).await {
        return Ok(HandoffResult::Fetched {
            host: host.into(),
            branch: branch.into(),
            at: "-".into(),
            why: format!("`{target}` does not exist here even after fetching"),
        });
    }
    let at = git::short_rev(repo, &target).await;

    // Detached HEAD is a legitimate state — the verify worktree lives in one — so
    // it is reported, not treated as an error.
    let current = match git::current_branch(repo).await {
        Ok(b) => b,
        Err(_) => {
            return Ok(HandoffResult::Fetched {
                host: host.into(),
                branch: branch.into(),
                at,
                why: "HEAD is detached here".into(),
            });
        }
    };
    if current != branch {
        return Ok(HandoffResult::Fetched {
            host: host.into(),
            branch: branch.into(),
            at,
            why: format!("`{current}` is checked out here, not `{branch}`"),
        });
    }
    if git::is_dirty(repo).await? {
        return Ok(HandoffResult::Fetched {
            host: host.into(),
            branch: branch.into(),
            at,
            why: "the working tree here has uncommitted changes".into(),
        });
    }

    let from = git::short_rev(repo, "HEAD").await;
    if from == at {
        return Ok(HandoffResult::Fetched {
            host: host.into(),
            branch: branch.into(),
            at,
            why: "already up to date".into(),
        });
    }
    // Strictly behind, or it is not a fast-forward. Diverged history is the one
    // case where the person at the other machine has to decide.
    if !git::is_ancestor(repo, "HEAD", &target).await {
        return Ok(HandoffResult::Fetched {
            host: host.into(),
            branch: branch.into(),
            at,
            why: format!("local `{branch}` at {from} has diverged; not a fast-forward"),
        });
    }

    git::merge_ff(repo, &target).await?;
    Ok(HandoffResult::FastForwarded {
        host: host.into(),
        branch: branch.into(),
        from,
        to: at,
    })
}
