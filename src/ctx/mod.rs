//! AI context sync: carry session state between the two machines.
//!
//! Code moves over git. What does not move is everything an AI tool keeps
//! *outside* the repo — Claude Code's per-project transcripts and memory. Switch
//! machines and that context is gone.
//!
//! Existing tools solve this by routing `~/.claude` through third-party cloud
//! storage, by manual export/import, or by standing up a separate file-sync
//! daemon with hand-written path mappings. None of those fit here: the P2P
//! channel is already established and authenticated, so context can ride it
//! directly with nothing leaving the two machines.
//!
//! The split keeps the decisions testable without a network: `roots`, `scan` and
//! `merge` are pure filesystem-and-policy code; `client` and `server` are the two
//! ends of the exchange.

pub mod client;
pub mod merge;
pub mod roots;
pub mod scan;
pub mod server;

pub use client::{Options, sync};
pub use merge::{Applied, apply_incoming, plan};
pub use roots::{Root, resolve_roots};
pub use scan::{now_ms, read_for_send, scan};
