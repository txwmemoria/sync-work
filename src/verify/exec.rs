//! Recipe execution with streamed output.
//!
//! Two things here are load-bearing:
//!
//! 1. Steps run through a real shell chosen per platform, so a recipe written
//!    once behaves the same on both machines.
//! 2. Compiler output comes back structured. A remote failure that arrives as
//!    `src/net.rs:88: mismatched types` is something a caller can act on
//!    directly; the same failure as 400 lines of log is not.
//!
//! For (2) we do not try to parse human-readable cargo output. We ask cargo for
//! JSON and then treat *every* stdout line as "maybe JSON": if it parses as a
//! cargo message we lift it into a `Diagnostic`, otherwise it passes through as
//! a log line. That handles `cargo test`, where compiler JSON and raw harness
//! output are interleaved on the same stream.

use crate::config::{Platform, ResolvedRecipe};
use crate::transport::{Diagnostic, Event};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

const MSG_FORMAT: &str = "--message-format=json-diagnostic-rendered-ansi";

/// Run every step in order, stopping at the first failure.
///
/// Returns the exit code to report: 0 when all steps pass, otherwise the code of
/// the step that failed. That code is what the requesting process exits with, so
/// a caller can branch on it without scraping output.
pub async fn run_recipe(
    cwd: &Path,
    recipe: &ResolvedRecipe,
    platform: Platform,
    color: bool,
    tx: &mpsc::Sender<Event>,
) -> Result<i32> {
    for (i, step) in recipe.steps.iter().enumerate() {
        let n = i + 1;
        let total = recipe.steps.len();
        let _ = tx
            .send(Event::Stage {
                name: format!("[{n}/{total}] {step}"),
            })
            .await;

        let code = run_step(cwd, step, recipe, platform, color, tx).await?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

async fn run_step(
    cwd: &Path,
    step: &str,
    recipe: &ResolvedRecipe,
    platform: Platform,
    color: bool,
    tx: &mpsc::Sender<Event>,
) -> Result<i32> {
    let effective = if recipe.cargo_json {
        inject_message_format(step).unwrap_or_else(|| step.to_string())
    } else {
        step.to_string()
    };

    let (shell, shell_args) = platform.shell();
    let mut cmd = Command::new(shell);
    cmd.args(shell_args)
        .arg(&effective)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // The child cannot detect the requester's terminal across the network, so
    // the decision is made there and carried in the request.
    cmd.env("CARGO_TERM_COLOR", if color { "always" } else { "never" });
    cmd.env("SW_REMOTE", "1");
    for (k, v) in &recipe.env {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{shell}` for step"))?;
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;

    // Both streams are drained concurrently; a build that writes a lot to stderr
    // while stdout is idle must not block on a full pipe buffer.
    let out_tx = tx.clone();
    let want_json = recipe.cargo_json;
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match want_json.then(|| parse_cargo_line(&line)).flatten() {
                Some(CargoLine::Diag(d)) => {
                    let _ = out_tx.send(Event::Diag(d)).await;
                }
                // Structural cargo chatter (artifacts, build-finished) is noise.
                Some(CargoLine::Ignore) => {}
                None => {
                    let _ = out_tx.send(Event::Log { err: false, line }).await;
                }
            }
        }
    });

    let err_tx = tx.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = err_tx.send(Event::Log { err: true, line }).await;
        }
    });

    let status = child.wait().await?;
    let _ = out_task.await;
    let _ = err_task.await;

    // Signal-terminated processes have no exit code; report the conventional
    // 128+signal so the caller still sees a non-zero result.
    Ok(status.code().unwrap_or(runtime_signal_code(&status)))
}

#[cfg(unix)]
fn runtime_signal_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| 128 + s).unwrap_or(-1)
}

#[cfg(not(unix))]
fn runtime_signal_code(_status: &std::process::ExitStatus) -> i32 {
    -1
}

// ---------------------------------------------------------------- cargo json

/// Add the JSON message format to a cargo invocation, if it is one that accepts
/// it and the recipe has not already specified a format.
fn inject_message_format(step: &str) -> Option<String> {
    let t = step.trim();
    let mut words = t.split_whitespace();
    if words.next()? != "cargo" {
        return None;
    }
    // Skip toolchain selectors like `cargo +nightly build`.
    let sub = words.find(|w| !w.starts_with('+'))?;
    if !matches!(
        sub,
        "build" | "b" | "check" | "c" | "test" | "t" | "clippy" | "bench" | "rustc"
    ) {
        return None;
    }
    if t.contains("--message-format") {
        return None;
    }
    // Anything after a bare `--` belongs to the test harness, not to cargo.
    Some(match t.find(" -- ") {
        Some(i) => format!("{} {}{}", &t[..i], MSG_FORMAT, &t[i..]),
        None => format!("{t} {MSG_FORMAT}"),
    })
}

enum CargoLine {
    Diag(Diagnostic),
    Ignore,
}

#[derive(Deserialize)]
struct CargoMsg {
    reason: String,
    #[serde(default)]
    message: Option<CargoDiag>,
}

#[derive(Deserialize)]
struct CargoDiag {
    level: String,
    message: String,
    #[serde(default)]
    rendered: Option<String>,
    #[serde(default)]
    spans: Vec<CargoSpan>,
}

#[derive(Deserialize)]
struct CargoSpan {
    file_name: String,
    line_start: u32,
    column_start: u32,
    #[serde(default)]
    is_primary: bool,
}

/// Classify one stdout line. `None` means "not a cargo message, treat as a log".
fn parse_cargo_line(line: &str) -> Option<CargoLine> {
    let trimmed = line.trim_start();
    // Cheap reject before attempting a parse — most lines are not JSON.
    if !trimmed.starts_with('{') {
        return None;
    }
    let msg: CargoMsg = serde_json::from_str(trimmed).ok()?;
    if msg.reason != "compiler-message" {
        return Some(CargoLine::Ignore);
    }
    let d = msg.message?;
    // `note` and `help` arrive as separate top-level messages but are already
    // embedded in the rendered form of the error they belong to.
    if !matches!(d.level.as_str(), "error" | "warning") {
        return Some(CargoLine::Ignore);
    }
    let primary = d
        .spans
        .iter()
        .find(|s| s.is_primary)
        .or_else(|| d.spans.first());
    Some(CargoLine::Diag(Diagnostic {
        level: d.level,
        message: d.message,
        file: primary.map(|s| s.file_name.clone()),
        line: primary.map(|s| s.line_start),
        col: primary.map(|s| s.column_start),
        rendered: d.rendered,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_format_goes_before_the_harness_separator() {
        assert_eq!(
            inject_message_format("cargo test --all -- --test-threads=1").unwrap(),
            format!("cargo test --all {MSG_FORMAT} -- --test-threads=1")
        );
        assert_eq!(
            inject_message_format("cargo build --release").unwrap(),
            format!("cargo build --release {MSG_FORMAT}")
        );
        assert_eq!(
            inject_message_format("cargo +nightly clippy").unwrap(),
            format!("cargo +nightly clippy {MSG_FORMAT}")
        );
    }

    #[test]
    fn message_format_is_not_injected_where_it_would_break_things() {
        // Not cargo at all.
        assert!(inject_message_format("cmake --build build").is_none());
        // A cargo subcommand that does not understand the flag.
        assert!(inject_message_format("cargo fmt --check").is_none());
        assert!(inject_message_format("cargo run --release").is_none());
        // Already specified by the recipe author — do not fight them.
        assert!(inject_message_format("cargo build --message-format=short").is_none());
    }

    #[test]
    fn compiler_errors_become_diagnostics_with_a_location() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","rendered":"error[E0308]...","spans":[{"file_name":"src/net.rs","line_start":88,"column_start":21,"is_primary":true}]}}"#;
        let Some(CargoLine::Diag(d)) = parse_cargo_line(line) else {
            panic!("expected a diagnostic");
        };
        assert_eq!(d.level, "error");
        assert_eq!(d.file.as_deref(), Some("src/net.rs"));
        assert_eq!(d.line, Some(88));
        assert_eq!(d.col, Some(21));
    }

    #[test]
    fn non_diagnostic_cargo_json_is_dropped_but_plain_output_survives() {
        // Structural cargo chatter: swallowed.
        assert!(matches!(
            parse_cargo_line(r#"{"reason":"compiler-artifact","target":{}}"#),
            Some(CargoLine::Ignore)
        ));
        // `note` level: swallowed, it is already inside the parent's rendered text.
        assert!(matches!(
            parse_cargo_line(
                r#"{"reason":"compiler-message","message":{"level":"note","message":"n","spans":[]}}"#
            ),
            Some(CargoLine::Ignore)
        ));
        // Test harness output must pass through untouched.
        assert!(parse_cargo_line("test config::tests::foo ... ok").is_none());
        // A line that merely looks like JSON must not be swallowed.
        assert!(parse_cargo_line("{not json").is_none());
    }
}
