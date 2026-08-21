//! Command implementations, and the dispatch that picks between them.
//!
//! The two commands with a peer on the other end (`verify`, `ctx`) live with
//! their protocol halves instead; what remains here is local bookkeeping.

pub mod hook;
pub mod pair;
pub mod project;

use crate::cli::{Cli, Command, HookCmd, ServiceCmd, discover};
use crate::config::Config;
use crate::{ctx, handoff, node, service, verify};
use anyhow::Result;

pub async fn dispatch(cli: Cli) -> Result<i32> {
    match cli.command {
        // ---------------------------------------------- machine-scoped
        Command::Node => {
            node::serve().await?;
            Ok(0)
        }
        Command::Status { peer } => {
            node::ping(peer.as_deref()).await?;
            Ok(0)
        }
        Command::Pair(cmd) => pair::run(cmd).await,
        Command::Hook(cmd) => {
            // Repo-scoped: the hook is per checkout, and carries its path.
            let repo = discover(&cli.repo).await?;
            match cmd {
                HookCmd::Install => hook::install(&repo),
                HookCmd::Uninstall => hook::uninstall(&repo),
                HookCmd::Status => hook::status(&repo),
            }
        }
        Command::Projects => project::list(),
        Command::Service(cmd) => {
            match cmd {
                ServiceCmd::Install { print } => {
                    if print {
                        service::print_plan()?
                    } else {
                        service::install()?
                    }
                }
                ServiceCmd::Restart => service::restart()?,
                ServiceCmd::Stop => service::stop()?,
                ServiceCmd::Start => service::start()?,
                ServiceCmd::Uninstall => service::uninstall()?,
                ServiceCmd::Status => service::status()?,
                ServiceCmd::Logs {
                    follow,
                    lines,
                    path,
                } => service::logs(follow, lines, path)?,
            }
            Ok(0)
        }

        // ---------------------------------------------- repository-scoped
        Command::Init { name } => {
            let repo = discover(&cli.repo).await?;
            project::init(&repo, name).await
        }
        Command::Link => {
            let repo = discover(&cli.repo).await?;
            let cfg = Config::load(&repo)?;
            project::link(&repo, &cfg.project.name, false)?;
            Ok(0)
        }
        Command::Unlink { name } => {
            let name = match name {
                Some(n) => n,
                None => {
                    let repo = discover(&cli.repo).await?;
                    Config::load(&repo)?.project.name
                }
            };
            project::unlink(&name)
        }
        Command::Verify {
            recipe,
            peer,
            base,
            push,
        } => {
            let repo = discover(&cli.repo).await?;
            let cfg = Config::load(&repo)?;
            // Registering the checkout you are standing in is never surprising,
            // so the initiating side never needs an explicit `sw link`.
            project::link(&repo, &cfg.project.name, true)?;
            verify::run(
                &repo,
                verify::Options {
                    recipe,
                    peer,
                    base,
                    push,
                },
            )
            .await
        }
        Command::Handoff { peer, no_push } => {
            let repo = discover(&cli.repo).await?;
            handoff::run(&repo, handoff::Options { peer, no_push }).await
        }
        Command::Ctx {
            peer,
            dry_run,
            if_reachable,
        } => {
            let repo = discover(&cli.repo).await?;
            let cfg = Config::load(&repo)?;
            project::link(&repo, &cfg.project.name, true)?;
            ctx::sync(
                &repo,
                ctx::Options {
                    peer,
                    dry_run,
                    if_reachable,
                },
            )
            .await
        }
    }
}
