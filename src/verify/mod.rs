//! Remote build verification: snapshot here, build there, report back here.
//!
//! Both halves live together. The client snapshots the working tree and replays
//! the peer's output; the server prepares a worktree and runs the recipe. Keeping
//! them adjacent is deliberate — they are two ends of one conversation, and a
//! change to the exchange almost always touches both.
//!
//! `exec` lives here rather than at the crate root because the server half is its
//! only consumer.

pub mod client;
pub mod exec;
pub mod server;

pub use client::{Options, run};
