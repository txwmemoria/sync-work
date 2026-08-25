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

/// Append one entry. Never fails the caller — see the module docs.
pub fn record(entry: &Entry) {
    let write = || -> std::io::Result<()> {
        let dir = machine::home().map_err(std::io::Error::other)?;
        std::fs::create_dir_all(&dir)?;
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        // One O_APPEND write per line: the node and a client may both finish a
        // verify at the same moment, and interleaving whole lines is safe where
        // interleaving bytes would not be.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(FILE))?
            .write_all(&line)
    };
    if let Err(e) = write() {
        tracing::debug!("could not record history: {e}");
    }
}

/// The most recent `n` entries, newest first. Lines that fail to parse are
/// skipped — an older binary may have written a shape this one no longer knows.
pub fn recent(n: usize) -> Vec<Entry> {
    let Ok(dir) = machine::home() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(dir.join(FILE)) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
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
}
