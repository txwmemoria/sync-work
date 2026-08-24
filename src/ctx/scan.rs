//! Reading a context root into a manifest, and validating paths on the way back.

use super::Root;
use super::merge::CONFLICT_TAG;
use crate::transport::{CtxEntry, CtxRoot};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Files above this size are skipped rather than shipped. A session transcript
/// this large is pathological, and silently moving hundreds of megabytes would be
/// a worse surprise than declining to.
const MAX_FILE: u64 = 64 * 1024 * 1024;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hash file contents. Truncated to 16 bytes: this is change detection between
/// two trusted peers, not a security boundary, and it keeps manifests small.
fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let full = blake3::hash(&bytes);
    Ok(hex::encode(&full.as_bytes()[..16]))
}

/// Build the manifest for one root. A missing directory is reported as absent
/// rather than as an error — a machine that has never opened a session there is
/// a normal state, and it should still be able to receive.
pub fn scan(root: &Root) -> Result<CtxRoot> {
    if !root.dir.is_dir() {
        // Existing-but-not-a-directory is a misconfiguration, not an absence, and
        // silently syncing nothing is the worst way to report it. Several AI tools
        // keep per-project state in a *file* (Aider's `.aider.chat.history.md`,
        // for one), so pointing `extra` at one is an easy mistake to make.
        if root.dir.exists() {
            tracing::warn!(
                "context root `{}` is {} — a root must be a directory, so nothing \
                 will sync from it",
                root.key,
                root.dir.display()
            );
        }
        return Ok(CtxRoot {
            key: root.key.clone(),
            present: false,
            entries: Vec::new(),
        });
    }
    let mut found = Vec::new();
    walk(&root.dir, &root.dir, &mut found)?;
    let mut entries: Vec<CtxEntry> = found
        .into_iter()
        .filter_map(|f| {
            Some(CtxEntry {
                // A file being rewritten right now may fail to read; the next
                // sync will pick it up.
                hash: hash_file(&f.path).ok()?,
                path: f.rel,
                size: f.size,
                mtime_ms: f.mtime_ms,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(CtxRoot {
        key: root.key.clone(),
        present: true,
        entries,
    })
}

/// One file the walk turned up, before anything reads its contents.
///
/// Hashing is deliberately not part of the traversal: [`fingerprint`] needs the
/// same rules about what counts as a file — no symlinks, no conflict copies, size
/// bounded — but must not read a byte.
struct Found {
    rel: String,
    path: PathBuf,
    size: u64,
    mtime_ms: u64,
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<Found>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // Symlinks are not followed: a link out of the root would drag in
        // arbitrary parts of the filesystem.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(base, &path, out)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let rel = match path.strip_prefix(base) {
            Ok(r) => rel_string(r),
            Err(_) => continue,
        };
        if rel.contains(CONFLICT_TAG) {
            continue;
        }
        if meta.len() > MAX_FILE {
            continue;
        }
        out.push(Found {
            rel,
            path,
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
        });
    }
    Ok(())
}

/// A cheap summary of what these roots hold right now: names, sizes, mtimes.
///
/// `--watch` calls this every couple of seconds, so it must not read file
/// contents — hashing every session transcript at that rate would cost more than
/// the sync it is waiting for. It only has to answer "did anything move"; the
/// manifest exchange still decides what actually differs, by hash.
pub fn fingerprint(roots: &[Root]) -> u64 {
    let mut h = blake3::Hasher::new();
    for root in roots {
        h.update(root.key.as_bytes());
        let mut found = Vec::new();
        // An unreadable or absent root contributes nothing but its name, so it
        // becoming readable later registers as a change.
        if root.dir.is_dir() {
            let _ = walk(&root.dir, &root.dir, &mut found);
        }
        found.sort_by(|a, b| a.rel.cmp(&b.rel));
        for f in found {
            h.update(f.rel.as_bytes());
            h.update(&f.size.to_le_bytes());
            h.update(&f.mtime_ms.to_le_bytes());
        }
    }
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8 bytes"))
}

/// Relative paths travel with forward slashes so both platforms agree on identity.
fn rel_string(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Reject a peer-supplied path that would escape the root. The peer is
/// authenticated, but a path is still untrusted input and this is the one place
/// where it becomes a filesystem write.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        anyhow::bail!("empty path");
    }
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            anyhow::bail!("unsafe path component in `{rel}`");
        }
        if part.contains('\\') || part.contains(':') {
            anyhow::bail!("unsafe path component in `{rel}`");
        }
        out.push(part);
    }
    Ok(out)
}

/// Read a file plus its mtime, ready to put on the wire.
pub fn read_for_send(root: &Path, rel: &str) -> Result<(Vec<u8>, u64)> {
    let path = safe_join(root, rel)?;
    let meta = std::fs::metadata(&path)?;
    let bytes = std::fs::read(&path)?;
    Ok((bytes, mtime_ms(&meta)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::repo::ROOT_CLAUDE;

    #[test]
    fn peer_supplied_paths_cannot_escape_the_root() {
        let root = Path::new("/tmp/root");
        assert!(safe_join(root, "a/b.md").is_ok());
        for bad in [
            "../etc/passwd",
            "a/../../b",
            "..",
            "",
            "a//b",
            "C:/windows",
            "a\\b",
        ] {
            assert!(safe_join(root, bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn conflict_copies_are_never_themselves_synced() {
        let dir = std::env::temp_dir().join(format!("sw-ctx-scan-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("real.jsonl"), b"a").unwrap();
        std::fs::write(dir.join("real.jsonl.conflict-mac-123"), b"b").unwrap();
        let m = scan(&Root {
            key: ROOT_CLAUDE.into(),
            dir: dir.clone(),
        })
        .unwrap();
        let paths: Vec<_> = m.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["real.jsonl"],
            "conflict copies would otherwise breed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_root_that_does_not_exist_yet_is_absent_not_an_error() {
        let m = scan(&Root {
            key: ROOT_CLAUDE.into(),
            dir: std::env::temp_dir().join("sw-definitely-not-here-9187"),
        })
        .unwrap();
        assert!(!m.present);
        assert!(m.entries.is_empty());
    }
}
