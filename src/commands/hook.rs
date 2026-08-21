//! Wiring `sw ctx` into Claude Code so context sync is not something to remember.
//!
//! A Stop hook fires when a session ends, which is the exact right moment: the
//! transcript has stopped growing, so `quiet_secs` will not skip it, and there is
//! nothing left to lose if the machine is closed straight afterwards.
//!
//! The hook goes in `.claude/settings.local.json`, never `settings.json`. The
//! latter is committed, and a hook invoking `sw` would then fail for every
//! teammate who does not have it installed. The command also carries an absolute
//! `-C <repo>` rather than relying on the working directory, which a hook does
//! not guarantee.

use crate::git;
use crate::style::Paint;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Substring identifying a hook this command installed, for status and removal.
///
/// Matching on the flag rather than on the whole command line means a hook still
/// gets recognised after the repo moves, or after someone edits the path by hand.
const MARKER: &str = "ctx --if-reachable";

/// Gitignore entry for the file this command writes. An unignored settings file
/// would leave the tree permanently dirty, and a dirty tree is exactly what makes
/// `sw handoff` decline to fast-forward on the other machine.
const HOOK_IGNORE: &str = ".claude/settings.local.json";

fn settings_path(repo: &Path) -> PathBuf {
    repo.join(".claude").join("settings.local.json")
}

fn hook_command(repo: &Path) -> String {
    // Quoted: a checkout under "Documents and Settings" or similar would
    // otherwise split into two arguments.
    format!("sw -C \"{}\" ctx --if-reachable", repo.display())
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(json!({})),
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(_) => Ok(json!({})),
    }
}

fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Every command string under `hooks.Stop`, flattened.
fn stop_commands(settings: &Value) -> Vec<String> {
    settings
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter_map(|h| h.get("command").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a `hooks.Stop` entry is one this command installed.
fn entry_is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter()
                .filter_map(|h| h.get("command").and_then(Value::as_str))
                .any(|c| c.contains(MARKER))
        })
        .unwrap_or(false)
}

pub async fn install(repo: &Path) -> Result<i32> {
    let path = settings_path(repo);
    let mut settings = read_settings(&path)?;

    if stop_commands(&settings).iter().any(|c| c.contains(MARKER)) {
        println!("already installed in {}", path.display());
        return Ok(0);
    }

    // Built up rather than assigned wholesale: this file belongs to Claude Code
    // and may hold unrelated settings and other hooks.
    let hooks = settings
        .as_object_mut()
        .context("settings root is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let stop = hooks
        .as_object_mut()
        .context("`hooks` is not a JSON object")?
        .entry("Stop")
        .or_insert_with(|| json!([]));
    stop.as_array_mut()
        .context("`hooks.Stop` is not a JSON array")?
        .push(json!({
            "hooks": [ { "type": "command", "command": hook_command(repo) } ]
        }));

    write_settings(&path, &settings)?;
    // Ignored by the *repo*, not left to a global config that differs per machine.
    git::ensure_ignored(repo, HOOK_IGNORE).await?;

    println!("{} {}", "installed".green().bold(), path.display());
    println!("  {}", hook_command(repo).dimmed());
    println!();
    println!("Context now syncs when a Claude Code session ends.");
    println!(
        "{}",
        "Do the same in your checkout on the other machine.".dimmed()
    );
    Ok(0)
}

pub fn uninstall(repo: &Path) -> Result<i32> {
    let path = settings_path(repo);
    let mut settings = read_settings(&path)?;

    let Some(stop) = settings
        .get_mut("hooks")
        .and_then(|h| h.get_mut("Stop"))
        .and_then(Value::as_array_mut)
    else {
        println!("no Stop hooks in {}", path.display());
        return Ok(0);
    };

    let before = stop.len();
    stop.retain(|entry| !entry_is_ours(entry));
    if stop.len() == before {
        println!("not installed in {}", path.display());
        return Ok(0);
    }

    // An empty array left behind is harmless but untidy, and it makes the file
    // look like something is configured when nothing is.
    if stop.is_empty()
        && let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut)
    {
        hooks.remove("Stop");
    }
    write_settings(&path, &settings)?;
    println!("{} {}", "removed".green().bold(), path.display());
    Ok(0)
}

pub fn status(repo: &Path) -> Result<i32> {
    let path = settings_path(repo);
    let settings = read_settings(&path)?;
    let commands = stop_commands(&settings);
    let ours: Vec<&String> = commands.iter().filter(|c| c.contains(MARKER)).collect();

    println!("{} {}", "settings".bold(), path.display());
    if ours.is_empty() {
        println!(
            "  {} no context-sync hook — run `sw hook install`",
            "!".yellow()
        );
    } else {
        for c in &ours {
            println!("  {} {c}", "✓".green());
        }
    }
    // Other Stop hooks are listed too: if one of them fails, Claude Code reports
    // the whole Stop step as failed, which looks like our hook misbehaving.
    let others: Vec<&String> = commands.iter().filter(|c| !c.contains(MARKER)).collect();
    if !others.is_empty() {
        println!("  other Stop hooks:");
        for c in others {
            println!("    {}", c.dimmed());
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install/uninstall are exercised through the JSON rather than the
    /// filesystem, which keeps them testable and pins the property that actually
    /// matters: this file belongs to Claude Code, so nothing else in it may be
    /// disturbed.
    fn add(settings: &mut Value, repo: &Path) {
        let hooks = settings
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}));
        let stop = hooks
            .as_object_mut()
            .unwrap()
            .entry("Stop")
            .or_insert_with(|| json!([]));
        stop.as_array_mut().unwrap().push(json!({
            "hooks": [ { "type": "command", "command": hook_command(repo) } ]
        }));
    }

    #[test]
    fn installing_preserves_unrelated_settings_and_other_hooks() {
        let repo = Path::new("/tmp/app");
        let mut s: Value = serde_json::from_str(
            r#"{
                "permissions": { "allow": ["Bash(ls)"] },
                "hooks": { "Stop": [ { "hooks": [
                    { "type": "command", "command": "echo unrelated" }
                ] } ] }
            }"#,
        )
        .unwrap();

        add(&mut s, repo);

        // Ours is present, and carries an absolute -C because a hook's working
        // directory is not guaranteed.
        let cmds = stop_commands(&s);
        assert!(cmds.iter().any(|c| c.contains(MARKER)));
        assert!(cmds.iter().any(|c| c.contains("-C \"/tmp/app\"")));
        // The pre-existing hook survives.
        assert!(cmds.iter().any(|c| c == "echo unrelated"));
        // So does everything outside `hooks`.
        assert_eq!(s["permissions"]["allow"][0], "Bash(ls)");
    }

    #[test]
    fn uninstalling_removes_only_our_entry() {
        let repo = Path::new("/tmp/app");
        let mut s = json!({
            "hooks": { "Stop": [ { "hooks": [
                { "type": "command", "command": "echo unrelated" }
            ] } ] }
        });
        add(&mut s, repo);
        assert_eq!(stop_commands(&s).len(), 2);

        let stop = s
            .get_mut("hooks")
            .and_then(|h| h.get_mut("Stop"))
            .and_then(Value::as_array_mut)
            .unwrap();
        stop.retain(|e| !entry_is_ours(e));

        let left = stop_commands(&s);
        assert_eq!(left, vec!["echo unrelated".to_string()]);
    }

    #[test]
    fn a_missing_or_empty_settings_file_reads_as_an_empty_object() {
        // First run on a machine: absent, and absent is not an error.
        let absent = read_settings(Path::new("/definitely/not/here/settings.json")).unwrap();
        assert_eq!(absent, json!({}));
        assert!(stop_commands(&absent).is_empty());

        // Claude Code has been seen to leave the file empty; `serde_json` would
        // reject that as EOF, so it is treated as empty rather than as corrupt.
        let dir = std::env::temp_dir().join(format!("sw-hook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.local.json");
        std::fs::write(&p, "  \n").unwrap();
        assert_eq!(read_settings(&p).unwrap(), json!({}));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_json_is_reported_rather_than_overwritten() {
        let dir = std::env::temp_dir().join(format!("sw-hook-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.local.json");
        std::fs::write(&p, "{ not json").unwrap();
        // Silently replacing a file Claude Code owns would lose real settings.
        assert!(read_settings(&p).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_hook_goes_in_the_local_settings_file() {
        // Never settings.json: that is committed, and a hook invoking `sw` would
        // then fail for every teammate who does not have it.
        let p = settings_path(Path::new("/tmp/app"));
        assert!(p.ends_with("settings.local.json"), "got {}", p.display());
        assert!(p.to_string_lossy().contains(".claude"));
    }
}
