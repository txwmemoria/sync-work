//! The shadow base store: what a context file looked like when both machines
//! last agreed on it.
//!
//! Three-way merge needs a common ancestor, and a two-machine sync has a natural
//! one: the content at the last moment the two sides were identical. This module
//! keeps a copy of exactly that, per project and root, under the machine state
//! directory (`~/.syncwork/ctx-base/`).
//!
//! A base is only ever written at moments of *known* convergence — when a sync
//! plan finds the two hashes equal, or when an incoming file cleanly replaces the
//! local one (the receiver then holds precisely what the sender holds). Never
//! after a merge: the merge result exists on one side only until the next round
//! confirms it, and a base that was never common to both sides would corrupt
//! every merge built on it.
//!
//! Only markdown files get a base. They are the line-oriented, entry-shaped
//! files (memory indexes, notes) that genuinely change on both machines; session
//! transcripts are append-only and already handled exactly by the prefix rule,
//! and shadowing multi-megabyte `.jsonl` files on every sync would cost more
//! than the merges are worth.

use crate::config::machine;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Whether a file participates in three-way merging (and so gets a base).
pub fn mergeable(rel: &str) -> bool {
    rel.ends_with(".md")
}

/// Where the base copy for one file lives.
///
/// The root key can contain characters that are not filesystem-safe
/// (`extra:docs/ai`), so it is flattened the same way Claude Code flattens
/// paths; the relative path is validated by the same rule as every other
/// peer-influenced path before it touches the filesystem.
fn base_path(project: &str, root_key: &str, rel: &str) -> Result<PathBuf> {
    let flat = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let dir = machine::home()?
        .join("ctx-base")
        .join(flat(project))
        .join(flat(root_key));
    super::scan::safe_join(&dir, rel)
}

/// The content both sides held at the last known convergence, if recorded.
pub fn load(project: &str, root_key: &str, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(base_path(project, root_key, rel).ok()?).ok()
}

/// Record `bytes` as the new common ancestor for this file.
///
/// Callers must only do this at a moment of known convergence — see the module
/// docs. Skips the write when the stored base is already identical, because this
/// runs on every sync for every unchanged `.md` file.
pub fn store(project: &str, root_key: &str, rel: &str, bytes: &[u8]) -> Result<()> {
    let path = base_path(project, root_key, rel)?;
    if let Ok(existing) = std::fs::read(&path)
        && existing == bytes
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, bytes).with_context(|| format!("writing base {}", path.display()))
}

/// Serialises tests that repoint `SW_HOME`: the variable is process-global and
/// the test harness runs tests in parallel, so two unlocked tests corrupt each
/// other's view of the state directory.
#[cfg(test)]
pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_markdown_is_mergeable() {
        assert!(mergeable("memory/MEMORY.md"));
        assert!(mergeable("MEMORY.md"));
        assert!(
            !mergeable("session.jsonl"),
            "transcripts stay on the prefix rule"
        );
        assert!(!mergeable("notes.txt"));
    }

    #[test]
    fn bases_round_trip_and_unsafe_paths_are_refused() {
        let _guard = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("sw-base-{}", super::super::now_ms()));
        // SAFETY: the lock above serialises every test that touches SW_HOME.
        unsafe { std::env::set_var("SW_HOME", &home) };

        assert!(load("proj", "claude", "memory/MEMORY.md").is_none());
        store("proj", "claude", "memory/MEMORY.md", b"- one\n").unwrap();
        assert_eq!(
            load("proj", "claude", "memory/MEMORY.md").as_deref(),
            Some(b"- one\n".as_slice())
        );

        // The key gets flattened; the rel path stays hierarchical but validated.
        store("proj", "extra:docs/ai", "a/b.md", b"x").unwrap();
        assert!(load("proj", "extra:docs/ai", "a/b.md").is_some());
        assert!(store("proj", "claude", "../escape.md", b"x").is_err());

        unsafe { std::env::remove_var("SW_HOME") };
        std::fs::remove_dir_all(&home).ok();
    }
}
