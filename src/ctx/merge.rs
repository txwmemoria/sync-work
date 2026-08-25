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
    /// Diverged on both sides, but a common ancestor is on record: these are
    /// merged three-way instead of one side flatly winning.
    pub reconcile: Vec<String>,
    pub skipped: Vec<(String, &'static str)>,
    /// Same hash on both sides. A list rather than a count because a moment of
    /// known convergence is exactly when the merge base gets refreshed.
    pub identical: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.pull.is_empty() && self.push.is_empty() && self.reconcile.is_empty()
    }
}

/// Decide, per file, which way it should move.
///
/// `quiet_ms` guards against copying a transcript that is being appended to at
/// this instant: a file touched more recently than that on either side is left
/// for the next run.
/// `has_base` answers "do we hold a recorded common ancestor for this path" —
/// passed in because the planner knows nothing of projects or state directories.
pub fn plan(
    local: &CtxRoot,
    remote: &CtxRoot,
    now: u64,
    quiet_ms: u64,
    has_base: &dyn Fn(&str) -> bool,
) -> Plan {
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
                    plan.identical.push(p.to_string());
                } else if fresh(le) || fresh(re) {
                    plan.skipped.push((p.to_string(), "recently modified"));
                } else if has_base(p) {
                    // Both sides changed a file we hold an ancestor for: merge,
                    // regardless of which mtime is newer — with a base, "newer
                    // wins" would throw the older side's changes away for nothing.
                    plan.reconcile.push(p.to_string());
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

/// Merge two divergent versions against their common ancestor, line by line.
///
/// `None` when the merge cannot be done safely: the same lines were edited on
/// both sides, or the content is not text. Callers then fall back to the
/// newer-wins-plus-conflict-copy rule, so the worst case of merging is exactly
/// the old behavior — never conflict markers written into a file.
pub fn three_way(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let (base, ours, theirs) = (
        std::str::from_utf8(base).ok()?,
        std::str::from_utf8(ours).ok()?,
        std::str::from_utf8(theirs).ok()?,
    );
    let mut opts = diffy::MergeOptions::new();
    opts.set_conflict_style(diffy::ConflictStyle::Diff3);
    match opts.merge(base, ours, theirs) {
        Ok(clean) => Some(clean.into_bytes()),
        Err(marked) => keep_both_insertions(&marked),
    }
}

/// Resolve the one conflict shape that is not actually ambiguous.
///
/// diff3 reports "both sides inserted different lines at the same spot" as a
/// conflict, because it cannot order the insertions — append two different
/// memory entries on two machines and that is exactly what you get, so the
/// primary use case of this whole mechanism lands in the conflict branch. But
/// nothing was edited or deleted: keeping both insertions loses nothing. Any
/// conflict whose original section is non-empty is a genuine edit collision and
/// stays unresolved.
///
/// The two blocks are ordered by content, not by which side is "ours": both
/// machines may merge the same pair concurrently with the roles swapped, and
/// sorting is what makes them produce byte-identical results anyway.
fn keep_both_insertions(marked: &str) -> Option<Vec<u8>> {
    #[derive(PartialEq)]
    enum Zone {
        Normal,
        Ours,
        Original,
        Theirs,
    }
    let mut zone = Zone::Normal;
    let (mut out, mut ours, mut theirs) = (String::new(), String::new(), String::new());

    for line in marked.split_inclusive('\n') {
        match (line.trim_end_matches(['\r', '\n']), &zone) {
            ("<<<<<<< ours", Zone::Normal) => zone = Zone::Ours,
            ("||||||| original", Zone::Ours) => zone = Zone::Original,
            // A non-empty original section means real edits collided; a stray
            // marker-lookalike line inside content lands here too and safely
            // aborts the resolution rather than corrupting anything.
            (_, Zone::Original) if line.trim_end_matches(['\r', '\n']) != "=======" => {
                return None;
            }
            ("=======", Zone::Original) => zone = Zone::Theirs,
            (">>>>>>> theirs", Zone::Theirs) => {
                let (a, b) = if ours <= theirs {
                    (&ours, &theirs)
                } else {
                    (&theirs, &ours)
                };
                out.push_str(a);
                out.push_str(b);
                ours.clear();
                theirs.clear();
                zone = Zone::Normal;
            }
            (_, Zone::Ours) => ours.push_str(line),
            (_, Zone::Theirs) => theirs.push_str(line),
            (_, Zone::Normal) => out.push_str(line),
            _ => return None,
        }
    }
    if zone == Zone::Normal {
        Some(out.into_bytes())
    } else {
        None
    }
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
///
/// `stamp` names the (project, root key) whose base store to update when the
/// write converges the two sides — created, extended and replaced all leave this
/// machine holding exactly what the sender holds, which is the definition of a
/// common ancestor. A conflict is the one outcome that must NOT stamp: the sides
/// still disagree.
pub fn apply_incoming(
    root: &Path,
    rel: &str,
    mtime_ms: u64,
    bytes: &[u8],
    host: &str,
    stamp: Option<(&str, &str)>,
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
    if let Some((project, root_key)) = stamp
        && !matches!(outcome, Applied::Conflict(_))
        && super::base::mergeable(rel)
        && let Err(e) = super::base::store(project, root_key, rel, bytes)
    {
        // Best effort: a missing base only costs a future merge, never data.
        tracing::debug!("could not record merge base for {rel}: {e:#}");
    }
    Ok(outcome)
}

/// Write a file with no preservation at all.
///
/// Only sound when the caller has already proven nothing is being destroyed —
/// concretely: an incoming merge declared which version it replaces, and the
/// local file still hashes to exactly that. The merge then contains everything
/// this machine holds, and this moment is a convergence, so the base is stamped.
pub fn apply_exact(
    root: &Path,
    rel: &str,
    mtime_ms: u64,
    bytes: &[u8],
    stamp: Option<(&str, &str)>,
) -> Result<()> {
    let dest = safe_join(root, rel)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
    if let Ok(f) = std::fs::File::options().write(true).open(&dest) {
        let _ = f.set_modified(UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms));
    }
    if let Some((project, root_key)) = stamp
        && super::base::mergeable(rel)
        && let Err(e) = super::base::store(project, root_key, rel, bytes)
    {
        tracing::debug!("could not record merge base for {rel}: {e:#}");
    }
    Ok(())
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
    use super::super::scan::mtime_ms;
    use super::*;
    use crate::config::repo::ROOT_CLAUDE;

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
        let p = plan(&local, &remote, 100_000, 5_000, &|_| false);
        assert_eq!(p.pull, ["a.jsonl", "only-remote.md"]);
        assert_eq!(p.push, ["only-local.md"]);
        // Identical content is untouched even though the mtimes differ wildly.
        assert_eq!(p.identical, ["same.md"]);
    }

    #[test]
    fn files_being_written_right_now_are_left_for_the_next_run() {
        let now = 100_000;
        let local = root(vec![entry("hot.jsonl", 1, now - 500, "aa")]);
        let remote = root(vec![]);
        let p = plan(&local, &remote, now, 5_000, &|_| false);
        assert!(
            p.push.is_empty(),
            "a file touched 500ms ago must not be shipped"
        );
        assert_eq!(p.skipped.len(), 1);

        // Once it has settled, it moves.
        let p = plan(&local, &remote, now + 10_000, 5_000, &|_| false);
        assert_eq!(p.push, ["hot.jsonl"]);
    }

    #[test]
    fn equal_mtime_with_different_content_is_surfaced_not_guessed() {
        let local = root(vec![entry("x.md", 1, 5_000, "aa")]);
        let remote = root(vec![entry("x.md", 2, 5_000, "bb")]);
        let p = plan(&local, &remote, 100_000, 1_000, &|_| false);
        assert!(p.pull.is_empty() && p.push.is_empty());
        assert_eq!(p.skipped[0].1, "same mtime, different content");
    }

    /// The three cases that justify keeping a merge base at all. A naive
    /// two-way line union gets the middle one wrong: with only two versions
    /// there is no telling "B added a line" apart from "A deleted it".
    #[test]
    fn three_way_keeps_both_additions_and_respects_deletions() {
        let base = b"- [A](a.md)\n- [B](b.md)\n";

        // Both machines appended a different entry: both survive.
        let ours = b"- [A](a.md)\n- [B](b.md)\n- [C](c.md)\n";
        let theirs = b"- [A](a.md)\n- [B](b.md)\n- [D](d.md)\n";
        let merged = three_way(base, ours, theirs).unwrap();
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("[C]") && text.contains("[D]"), "got:\n{text}");
        assert!(text.contains("[A]") && text.contains("[B]"));

        // One side deleted an entry, the other appended elsewhere: the deletion
        // sticks instead of the entry resurrecting — the case a two-way line
        // union gets wrong. (The edits must not touch adjacent lines; adjacent
        // delete-vs-insert is a genuine overlap and conflicts, as in git.)
        let wide = b"- [A](a.md)\n- [B](b.md)\n- [C](c.md)\n";
        let ours = b"- [A](a.md)\n- [C](c.md)\n";
        let theirs = b"- [A](a.md)\n- [B](b.md)\n- [C](c.md)\n- [D](d.md)\n";
        let merged = three_way(wide, ours, theirs).unwrap();
        let text = String::from_utf8(merged).unwrap();
        assert!(!text.contains("[B]"), "deleted entry resurrected:\n{text}");
        assert!(text.contains("[D]") && text.contains("[C]"));

        // Both sides rewrote the same line: no safe answer, so no answer —
        // the caller falls back to newer-wins-plus-conflict-copy.
        let ours = b"- [A](a.md) changed here\n- [B](b.md)\n";
        let theirs = b"- [A](a.md) changed there\n- [B](b.md)\n";
        assert!(three_way(base, ours, theirs).is_none());

        // Binary content never merges.
        assert!(three_way(b"\xff\xfe", b"\xff", b"\xfe").is_none());
    }

    /// A clean overwrite means this machine now holds exactly what the sender
    /// holds — a convergence — so the base gets stamped; a conflict means the
    /// sides still disagree, so it must not be.
    #[test]
    fn bases_are_stamped_on_convergence_and_never_on_conflict() {
        let _guard = super::super::base::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("sw-stamp-{}", now_ms()));
        // SAFETY: the lock above serialises every test that touches SW_HOME.
        unsafe { std::env::set_var("SW_HOME", &home) };
        let dir = std::env::temp_dir().join(format!("sw-stampdir-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();

        let stamp = Some(("proj", "claude"));
        apply_incoming(&dir, "m.md", 1_000, b"- one\n", "mac", stamp).unwrap();
        assert_eq!(
            super::super::base::load("proj", "claude", "m.md").as_deref(),
            Some(b"- one\n".as_slice())
        );

        // Divergence: conflict copy, and the base stays at the last agreement.
        std::fs::write(dir.join("m.md"), b"- mine\n").unwrap();
        let got = apply_incoming(&dir, "m.md", 2_000, b"- theirs\n", "mac", stamp).unwrap();
        assert!(matches!(got, Applied::Conflict(_)));
        assert_eq!(
            super::super::base::load("proj", "claude", "m.md").as_deref(),
            Some(b"- one\n".as_slice()),
            "a conflict is not a convergence"
        );

        // Non-markdown never gets a base, converged or not.
        apply_incoming(&dir, "t.jsonl", 3_000, b"x", "mac", stamp).unwrap();
        assert!(super::super::base::load("proj", "claude", "t.jsonl").is_none());

        unsafe { std::env::remove_var("SW_HOME") };
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn divergent_markdown_with_a_base_plans_a_merge_not_a_coin_toss() {
        let local = root(vec![entry("MEMORY.md", 10, 5_000, "aa")]);
        let remote = root(vec![entry("MEMORY.md", 12, 9_000, "bb")]);

        // Without a base: old behavior, newer side wins.
        let p = plan(&local, &remote, 100_000, 1_000, &|_| false);
        assert_eq!(p.pull, ["MEMORY.md"]);
        assert!(p.reconcile.is_empty());

        // With one: reconcile, regardless of which mtime is newer.
        let p = plan(&local, &remote, 100_000, 1_000, &|_| true);
        assert_eq!(p.reconcile, ["MEMORY.md"]);
        assert!(p.pull.is_empty() && p.push.is_empty());
    }

    #[test]
    fn appends_overwrite_cleanly_but_divergence_is_preserved() {
        let dir = std::env::temp_dir().join(format!("sw-ctx-test-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();

        // A transcript that grew on the other machine: prefix match, no conflict.
        std::fs::write(dir.join("t.jsonl"), b"line1\nline2\n").unwrap();
        let got = apply_incoming(
            &dir,
            "t.jsonl",
            1_000,
            b"line1\nline2\nline3\n",
            "mac",
            None,
        )
        .unwrap();
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
        let got =
            apply_incoming(&dir, "t.jsonl", 2_000, b"totally different\n", "mac", None).unwrap();
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
            apply_incoming(&dir, "sub/new.md", 3_000, b"hi", "mac", None).unwrap(),
            Applied::Created
        );
        assert_eq!(std::fs::read(dir.join("sub/new.md")).unwrap(), b"hi");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn written_files_keep_the_senders_mtime_so_sync_converges() {
        let dir = std::env::temp_dir().join(format!("sw-ctx-mtime-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        apply_incoming(&dir, "a.md", 1_700_000_000_000, b"x", "mac", None).unwrap();
        let meta = std::fs::metadata(dir.join("a.md")).unwrap();
        // Without this the receiver would look newer and push the file straight
        // back on the next run, forever.
        assert_eq!(mtime_ms(&meta), 1_700_000_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }
}
