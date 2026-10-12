//! A record of every verify this machine took part in.
//!
//! The node used to throw each result away the moment it was reported, which
//! left "when did this project last build clean on the Mac?" unanswerable.
//! Both ends now append one JSON line per completed verify to
//! `~/.syncwork/history.jsonl` — the client for runs it requested, the node for
//! runs it served — and `sw tui` reads it back.
//!
//! Recording is strictly best-effort: a verify whose result cannot be written
//! down is still a successful verify, so nothing here returns an error to the
//! caller.

use crate::config::machine;
use serde::{Deserialize, Serialize};
use std::io::Write;

const FILE: &str = "history.jsonl";

/// Which side of the exchange this machine was on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// This machine asked the peer to build.
    Sent,
    /// This machine built for the peer.
    Served,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub ts_ms: u64,
    pub direction: Direction,
    pub peer: String,
    pub project: String,
    pub recipe: String,
    /// The recipe's exit code; `None` when the run failed before any exit code
    /// existed (patch refused, recipe unknown, connection lost).
    pub code: Option<i32>,
    pub ms: u64,
}

/// Past this size the file is cut back to its newest [`KEEP`] lines — about
/// 200 bytes each, so a trim happens every few thousand verifies.
const MAX_BYTES: u64 = 512 * 1024;
const KEEP: usize = 2000;

/// Append one entry. Never fails the caller — see the module docs.
pub fn record(entry: &Entry) {
    let write = || -> std::io::Result<()> {
        let dir = machine::home().map_err(std::io::Error::other)?;
        std::fs::create_dir_all(&dir)?;
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let path = dir.join(FILE);
        // One O_APPEND write per line: the node and a client may both finish a
        // verify at the same moment, and interleaving whole lines is safe where
        // interleaving bytes would not be.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(&line)?;
        if file.metadata()?.len() > MAX_BYTES {
            drop(file);
            trim(&path, KEEP)?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        tracing::debug!("could not record history: {e}");
    }
}

/// Rewrite the file as its newest `keep` lines, via a rename so a reader never
/// sees it half-written. A line the other process appends between the read
/// and the rename is lost; that window opens once per few thousand verifies,
/// which a best-effort record can afford.
fn trim(path: &std::path::Path, keep: usize) -> std::io::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = raw.lines().collect();
    let kept = &lines[lines.len().saturating_sub(keep)..];
    let tmp = path.with_extension("jsonl.tmp");
    let mut out = kept.join("\n");
    out.push('\n');
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

/// The most recent `n` entries, newest first. Lines that fail to parse are
/// skipped — an older binary may have written a shape this one no longer knows.
///
/// Reads only the file's tail: the dashboard asks every few seconds, and
/// wanting fifty entries should not mean parsing two thousand.
pub fn recent(n: usize) -> Vec<Entry> {
    let Ok(dir) = machine::home() else {
        return Vec::new();
    };
    tail(&dir.join(FILE), n, (n as u64 * 512).max(64 * 1024))
}

fn tail(path: &std::path::Path, n: usize, window: u64) -> Vec<Entry> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(window);
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let raw = String::from_utf8_lossy(&buf);
    let mut lines = raw.lines();
    // Starting mid-file almost always lands inside a line; that fragment is
    // not an entry.
    if start > 0 {
        lines.next();
    }
    let mut entries: Vec<Entry> = lines.filter_map(|l| serde_json::from_str(l).ok()).collect();
    entries.reverse();
    entries.truncate(n);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_round_trips_newest_first_and_skips_junk() {
        let _guard = crate::ctx::base::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("sw-hist-{}", crate::ctx::now_ms()));
        // SAFETY: the lock above serialises every test that touches SW_HOME.
        unsafe { std::env::set_var("SW_HOME", &home) };

        assert!(recent(10).is_empty());
        for (i, code) in [(1u64, Some(0)), (2, Some(101)), (3, None)] {
            record(&Entry {
                ts_ms: i,
                direction: Direction::Sent,
                peer: "mac".into(),
                project: "demo".into(),
                recipe: "verify".into(),
                code,
                ms: i * 100,
            });
        }
        // A line from some other version of the format must not break reading.
        let file = home.join("history.jsonl");
        let mut raw = std::fs::read_to_string(&file).unwrap();
        raw.push_str("not json at all\n");
        std::fs::write(&file, raw).unwrap();

        let got = recent(2);
        assert_eq!(got.len(), 2, "cap respected");
        assert_eq!(got[0].ts_ms, 3, "newest first");
        assert_eq!(got[0].code, None);
        assert_eq!(got[1].code, Some(101));

        unsafe { std::env::remove_var("SW_HOME") };
        std::fs::remove_dir_all(&home).ok();
    }

    /// The file is bounded, and reading from the middle of it never yields a
    /// torn first line as an entry.
    #[test]
    fn the_file_is_trimmed_and_read_from_its_tail() {
        let dir = std::env::temp_dir().join(format!("sw-hist-tail-{}", crate::ctx::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let line = |i: u64| {
            serde_json::to_string(&Entry {
                ts_ms: i,
                direction: Direction::Served,
                peer: "pc".into(),
                project: "demo".into(),
                recipe: "verify".into(),
                code: Some(0),
                ms: 1,
            })
            .unwrap()
        };
        let all: Vec<String> = (0..100).map(line).collect();
        std::fs::write(&path, all.join("\n") + "\n").unwrap();

        trim(&path, 10).unwrap();
        let kept = std::fs::read_to_string(&path).unwrap();
        assert_eq!(kept.lines().count(), 10);
        assert!(kept.lines().next().unwrap().contains("\"ts_ms\":90"));

        // A window smaller than the file, deliberately not on a line boundary.
        let got = tail(&path, 3, (line(99).len() as u64) * 4 + 7);
        assert_eq!(
            got.iter().map(|e| e.ts_ms).collect::<Vec<_>>(),
            [99, 98, 97]
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
