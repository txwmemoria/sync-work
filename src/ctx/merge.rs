//! Deciding which side of a context file wins, and applying it without loss.
//!
//! Newer mtime wins. But before the loser is overwritten it is compared, locally,
//! against the winner: if the loser's bytes are a prefix of the winner's, the
//! winner is a strict extension of it — the normal case for append-only
//! transcripts — and the overwrite is clean. Only genuinely divergent files
//! produce a `.conflict-<host>-<stamp>` copy. Nothing is ever discarded.

use super::scan::{now_ms, safe_join};
use crate::transport::{CtxEntry, CtxRoot};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Marker for preserved losing versions. Never itself synced, or conflicts would
/// breed across machines.
pub const CONFLICT_TAG: &str = ".conflict-";

#[derive(Debug, Default)]
pub struct Plan {
    pub pull: Vec<String>,
    pub push: Vec<String>,
    pub skipped: Vec<(String, &'static str)>,
    pub identical: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.pull.is_empty() && self.push.is_empty()
    }
}

/// Decide, per file, which way it should move.
///
/// `quiet_ms` guards against copying a transcript that is being appended to at
/// this instant: a file touched more recently than that on either side is left
/// for the next run.
pub fn plan(local: &CtxRoot, remote: &CtxRoot, now: u64, quiet_ms: u64) -> Plan {
    let l: BTreeMap<&str, &CtxEntry> = local.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let r: BTreeMap<&str, &CtxEntry> = remote
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut plan = Plan::default();
    let mut paths: Vec<&str> = l.keys().copied().chain(r.keys().copied()).collect();
    paths.sort_unstable();
    paths.dedup();

    for p in paths {
        let fresh = |e: &CtxEntry| now.saturating_sub(e.mtime_ms) < quiet_ms;
        match (l.get(p), r.get(p)) {
            (Some(le), None) => {
                if fresh(le) {
                    plan.skipped.push((p.to_string(), "being written locally"));
                } else {
                    plan.push.push(p.to_string());
                }
            }
            (None, Some(re)) => {
                if fresh(re) {
                    plan.skipped.push((p.to_string(), "being written on peer"));
                } else {
                    plan.pull.push(p.to_string());
                }
            }
            (Some(le), Some(re)) => {
                if le.hash == re.hash {
                    plan.identical += 1;
                } else if fresh(le) || fresh(re) {
                    plan.skipped.push((p.to_string(), "recently modified"));
                } else if re.mtime_ms > le.mtime_ms {
                    plan.pull.push(p.to_string());
                } else if le.mtime_ms > re.mtime_ms {
                    plan.push.push(p.to_string());
                } else {
                    // Equal mtimes, different content: no basis for ordering, so
                    // pulling would be an arbitrary choice. Surface it instead.
                    plan.skipped
                        .push((p.to_string(), "same mtime, different content"));
                }
            }
            (None, None) => unreachable!("path came from one of the two maps"),
        }
    }
    plan
}

// ---------------------------------------------------------------- applying

/// Outcome of writing one incoming file.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Created,
    /// Cleanly extended: the previous contents were a prefix of the new ones.
    Extended,
    /// Diverged; the previous version was preserved alongside.
    Conflict(PathBuf),
    Replaced,
}

/// Write an incoming file, preserving anything it would otherwise destroy.
pub fn apply_incoming(
    root: &Path,
    rel: &str,
    mtime_ms: u64,
    bytes: &[u8],
    host: &str,
) -> Result<Applied> {
    let dest = safe_join(root, rel)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let outcome = match std::fs::read(&dest) {
        Err(_) => Applied::Created,
        Ok(existing) => {
            if existing == bytes {
                Applied::Replaced
            } else if bytes.len() >= existing.len() && bytes[..existing.len()] == existing[..] {
                // The incoming version is a strict extension of what we hold —
                // an append. Nothing is lost by overwriting.
                Applied::Extended
            } else {
                let backup = conflict_path(&dest, host);
                std::fs::rename(&dest, &backup)
                    .with_context(|| format!("preserving {}", dest.display()))?;
                Applied::Conflict(backup)
            }
        }
    };

    std::fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
    // Carry the mtime across so the next sync sees the two sides as equal
    // instead of treating the freshly written copy as newer and bouncing it back.
    if let Ok(f) = std::fs::File::options().write(true).open(&dest) {
        let _ = f.set_modified(UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms));
    }
    Ok(outcome)
}

fn conflict_path(dest: &Path, host: &str) -> PathBuf {
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // Seconds, not millis: shorter and still unique enough for a manual rescue.
    let stamp = now_ms() / 1000;
    dest.with_file_name(format!("{name}{CONFLICT_TAG}{host}-{stamp}"))
}

#[cfg(test)]
mod tests {
    use super::super::roots::ROOT_CLAUDE;
    use super::super::scan::mtime_ms;
    use super::*;

    fn entry(path: &str, size: u64, mtime: u64, hash: &str) -> CtxEntry {
        CtxEntry {
            path: path.into(),
            size,
            mtime_ms: mtime,
            hash: hash.into(),
        }
    }

    fn root(entries: Vec<CtxEntry>) -> CtxRoot {
        CtxRoot {
            key: ROOT_CLAUDE.into(),
            present: true,
            entries,
        }
    }

    #[test]
    fn newer_side_wins_and_matching_hashes_are_left_alone() {
        let local = root(vec![
            entry("a.jsonl", 10, 1_000, "aa"),
            entry("same.md", 5, 1_000, "cc"),
            entry("only-local.md", 1, 1_000, "dd"),
        ]);
        let remote = root(vec![
            entry("a.jsonl", 20, 2_000, "bb"),
            entry("same.md", 5, 9_000, "cc"),
            entry("only-remote.md", 1, 1_000, "ee"),
        ]);
        let p = plan(&local, &remote, 100_000, 5_000);
        assert_eq!(p.pull, ["a.jsonl", "only-remote.md"]);
        assert_eq!(p.push, ["only-local.md"]);
        // Identical content is untouched even though the mtimes differ wildly.
        assert_eq!(p.identical, 1);
    }

    #[test]
    fn files_being_written_right_now_are_left_for_the_next_run() {
        let now = 100_000;
        let local = root(vec![entry("hot.jsonl", 1, now - 500, "aa")]);
        let remote = root(vec![]);
        let p = plan(&local, &remote, now, 5_000);
        assert!(
            p.push.is_empty(),
            "a file touched 500ms ago must not be shipped"
        );
        assert_eq!(p.skipped.len(), 1);

        // Once it has settled, it moves.
        let p = plan(&local, &remote, now + 10_000, 5_000);
        assert_eq!(p.push, ["hot.jsonl"]);
    }

    #[test]
    fn equal_mtime_with_different_content_is_surfaced_not_guessed() {
        let local = root(vec![entry("x.md", 1, 5_000, "aa")]);
        let remote = root(vec![entry("x.md", 2, 5_000, "bb")]);
        let p = plan(&local, &remote, 100_000, 1_000);
        assert!(p.pull.is_empty() && p.push.is_empty());
        assert_eq!(p.skipped[0].1, "same mtime, different content");
    }

    #[test]
    fn appends_overwrite_cleanly_but_divergence_is_preserved() {
        let dir = std::env::temp_dir().join(format!("sw-ctx-test-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();

        // A transcript that grew on the other machine: prefix match, no conflict.
        std::fs::write(dir.join("t.jsonl"), b"line1\nline2\n").unwrap();
        let got = apply_incoming(&dir, "t.jsonl", 1_000, b"line1\nline2\nline3\n", "mac").unwrap();
        assert_eq!(got, Applied::Extended);
        assert_eq!(
            std::fs::read(dir.join("t.jsonl")).unwrap(),
            b"line1\nline2\nline3\n"
        );
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(CONFLICT_TAG))
            .collect();
        assert!(
            strays.is_empty(),
            "an append must not produce a conflict copy"
        );

        // Genuine divergence: the losing version survives on disk.
        let got = apply_incoming(&dir, "t.jsonl", 2_000, b"totally different\n", "mac").unwrap();
        match got {
            Applied::Conflict(backup) => {
                assert_eq!(std::fs::read(&backup).unwrap(), b"line1\nline2\nline3\n");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(dir.join("t.jsonl")).unwrap(),
            b"totally different\n"
        );

        // A brand new file is simply created.
        assert_eq!(
            apply_incoming(&dir, "sub/new.md", 3_000, b"hi", "mac").unwrap(),
            Applied::Created
        );
        assert_eq!(std::fs::read(dir.join("sub/new.md")).unwrap(), b"hi");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn written_files_keep_the_senders_mtime_so_sync_converges() {
        let dir = std::env::temp_dir().join(format!("sw-ctx-mtime-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        apply_incoming(&dir, "a.md", 1_700_000_000_000, b"x", "mac").unwrap();
        let meta = std::fs::metadata(dir.join("a.md")).unwrap();
        // Without this the receiver would look newer and push the file straight
        // back on the next run, forever.
        assert_eq!(mtime_ms(&meta), 1_700_000_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }
}
