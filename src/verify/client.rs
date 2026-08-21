//! The initiating half: snapshot this working tree, have the peer build it,
//! replay the output locally, and exit with the peer's exit code.
//!
//! Exit-code transparency is the whole interface. It means `sw verify` composes
//! like any other build command — in a shell `&&` chain, in a git hook, or as a
//! tool an AI agent drives in a fix-and-retry loop without a human relaying the
//! failure by hand.

use crate::config::{Config, Identity, Machine, Peer};
use crate::git;
use crate::style::Paint;
use crate::transport::{self, Diagnostic, Event, PROTOCOL_VERSION, Request, VerifyRequest};
use anyhow::{Result, bail};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

pub struct Options {
    pub recipe: String,
    pub peer: Option<String>,
    pub base: Option<String>,
    /// Push the current branch before verifying. Off by default — the point of
    /// the patch transport is that verification does not need a push.
    pub push: bool,
}

/// What a single attempt produced.
enum Outcome {
    Done(i32),
    /// The peer cannot obtain our base commit; retry against an older one.
    MissingBase(String),
    Failed(String),
}

pub async fn run(repo: &Path, opts: Options) -> Result<i32> {
    let cfg = Config::load(repo)?;
    let machine = Machine::load()?;
    let identity = Identity::load_or_create()?;
    let (peer_name, peer) = machine.resolve_peer(opts.peer.as_deref())?;

    if opts.push {
        let branch = git::current_branch(repo).await?;
        eprintln!("{} {branch} -> {}", "push".bold(), cfg.project.remote);
        git::push_current(repo, &cfg.project.remote).await?;
    }

    let base = match &opts.base {
        Some(r) => git::resolve_ref(repo, r).await?,
        None => git::head_sha(repo).await?,
    };

    transport::with_endpoint(&identity, |ep| async move {
        verify_against(&ep, repo, &cfg, &peer, &peer_name, &base, &opts).await
    })
    .await
}

async fn verify_against(
    ep: &iroh::Endpoint,
    repo: &Path,
    cfg: &Config,
    peer: &Peer,
    peer_name: &str,
    base: &str,
    opts: &Options,
) -> Result<i32> {
    let conn = transport::connect(ep, peer).await?;
    // Wrapped once: a refusal can surface from any step of the exchange, in a
    // different shape each time, so no individual `?` is the right place.
    attempts(&conn, repo, cfg, peer_name, base, opts)
        .await
        .map_err(|e| transport::explain(&conn, e))
}

async fn attempts(
    conn: &iroh::endpoint::Connection,
    repo: &Path,
    cfg: &Config,
    peer_name: &str,
    base: &str,
    opts: &Options,
) -> Result<i32> {
    // First attempt against HEAD keeps the patch as small as possible.
    match attempt(repo, cfg, conn, base, &opts.recipe, peer_name).await? {
        Outcome::Done(code) => Ok(code),
        Outcome::Failed(msg) => bail!("{msg}"),
        Outcome::MissingBase(missing) => {
            // The peer has never seen our HEAD — typically local-only commits.
            // Fall back to the newest commit it can fetch and widen the patch to
            // cover the difference, so verification still needs no push.
            let fallback = git::shared_base(repo, &cfg.project.remote).await?;
            if fallback == missing {
                bail!(
                    "peer cannot obtain commit {missing:.12} and no older shared commit exists; \
                     push the branch once with `sw verify --push`"
                );
            }
            eprintln!(
                "{} peer lacks {:.12}; retrying against {:.12} with a wider patch",
                "note".yellow().bold(),
                missing,
                fallback
            );
            match attempt(repo, cfg, conn, &fallback, &opts.recipe, peer_name).await? {
                Outcome::Done(code) => Ok(code),
                Outcome::Failed(msg) => bail!("{msg}"),
                Outcome::MissingBase(b) => bail!(
                    "peer cannot obtain commit {b:.12} either; push the branch with \
                     `sw verify --push`"
                ),
            }
        }
    }
}

async fn attempt(
    repo: &Path,
    cfg: &Config,
    conn: &iroh::endpoint::Connection,
    base: &str,
    recipe: &str,
    peer_name: &str,
) -> Result<Outcome> {
    let snap = git::snapshot(repo, base, &git::state_dir(repo)).await?;

    let desc = if snap.is_clean() {
        format!("{base:.12} clean")
    } else {
        format!(
            "{base:.12} + {} file{} ({} → {})",
            snap.files_changed,
            if snap.files_changed == 1 { "" } else { "s" },
            human_bytes(snap.raw_len),
            human_bytes(snap.patch.len())
        )
    };
    eprintln!(
        "{} {peer_name}  {}  {}",
        "verify".bold(),
        recipe.cyan(),
        desc.dimmed()
    );

    let (mut tx, mut rx) = transport::open_request(conn).await?;
    let req = Request::Verify(VerifyRequest {
        protocol: PROTOCOL_VERSION,
        project: cfg.project.name.clone(),
        recipe: recipe.to_string(),
        base: base.to_string(),
        patch_bytes: snap.patch.len() as u64,
        from_host: Machine::load().map(|m| m.node_name()).unwrap_or_default(),
        color: crate::style::enabled(),
    });
    transport::write_json(&mut tx, &req).await?;
    transport::write_frame(&mut tx, &snap.patch).await?;
    tx.finish().ok();

    let started = Instant::now();
    let mut errors = 0usize;
    let mut warnings = 0usize;

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    while let Some(ev) = transport::read_event(&mut rx).await? {
        match ev {
            Event::Accepted {
                host,
                platform,
                repo,
                build,
            } => {
                let stamp = if build.is_empty() {
                    String::new()
                } else {
                    format!(" {}", format!("[{build}]").dimmed())
                };
                eprintln!(
                    "{} {host} ({platform}){stamp}  {}",
                    "  on".dimmed(),
                    repo.dimmed()
                );
            }
            Event::Stage { name } => {
                eprintln!("{} {name}", "  ›".blue().bold());
            }
            // Build output is replayed onto the matching local stream so that
            // redirecting stdout still separates program output from progress.
            Event::Log { err, line } => {
                if err {
                    let mut h = stderr.lock();
                    let _ = writeln!(h, "{line}");
                } else {
                    let mut h = stdout.lock();
                    let _ = writeln!(h, "{line}");
                }
            }
            Event::Diag(d) => {
                if d.level == "error" {
                    errors += 1;
                } else {
                    warnings += 1;
                }
                print_diagnostic(&d);
            }
            Event::MissingBase { base } => return Ok(Outcome::MissingBase(base)),
            Event::Failed { message } => return Ok(Outcome::Failed(message)),
            Event::Done { code, ms } => {
                let wall = started.elapsed().as_millis() as u64;
                let summary = format!(
                    "{} error{}, {} warning{}",
                    errors,
                    if errors == 1 { "" } else { "s" },
                    warnings,
                    if warnings == 1 { "" } else { "s" }
                );
                if code == 0 {
                    eprintln!(
                        "{} {peer_name} in {} {}",
                        "  ok".green().bold(),
                        fmt_ms(ms),
                        format!("({summary}, {} wall)", fmt_ms(wall)).dimmed()
                    );
                } else {
                    eprintln!(
                        "{} {peer_name} exit {code} in {} {}",
                        "fail".red().bold(),
                        fmt_ms(ms),
                        format!("({summary})").dimmed()
                    );
                }
                return Ok(Outcome::Done(code));
            }
        }
    }
    // The stream ended without a terminal event — the peer died mid-build.
    Ok(Outcome::Failed(
        "peer closed the connection without finishing".into(),
    ))
}

/// Prefer cargo's own rendering when present, so remote output is byte-identical
/// to what a local build would have printed. The compact form is the fallback
/// for diagnostics that arrived without it.
fn print_diagnostic(d: &Diagnostic) {
    let mut h = std::io::stderr().lock();
    if let Some(r) = &d.rendered {
        let _ = write!(h, "{r}");
        if !r.ends_with('\n') {
            let _ = writeln!(h);
        }
        return;
    }
    let loc = match (&d.file, d.line, d.col) {
        (Some(f), Some(l), Some(c)) => format!("{f}:{l}:{c}: "),
        (Some(f), Some(l), None) => format!("{f}:{l}: "),
        (Some(f), _, _) => format!("{f}: "),
        _ => String::new(),
    };
    let tag = if d.level == "error" {
        d.level.red().bold().to_string()
    } else {
        d.level.yellow().bold().to_string()
    };
    let _ = writeln!(h, "{tag}: {loc}{}", d.message);
}

fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

fn human_bytes(n: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally_across_scales() {
        assert_eq!(fmt_ms(0), "0ms");
        assert_eq!(fmt_ms(999), "999ms");
        assert_eq!(fmt_ms(1000), "1.0s");
        assert_eq!(fmt_ms(59_999), "60.0s");
        assert_eq!(fmt_ms(60_000), "1m00s");
        assert_eq!(fmt_ms(3_725_000), "62m05s");
    }

    #[test]
    fn byte_sizes_read_naturally_across_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
