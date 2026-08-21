//! Branch handoff: publish here, advance there, in one action.
//!
//! `sw verify` already carries uncommitted work to the peer without touching the
//! remote, so this is not about transport — it is about the moment you decide
//! these commits should exist in the remote. That decision stays explicit, which
//! is why there is no auto-push on commit: pushed history is expensive to take
//! back.
//!
//! The receiving side never guesses. It fetches always, and advances the checkout
//! only when the requested branch is the one checked out, the tree is clean, and
//! the move is a strict fast-forward. Anything else is reported with the reason.

pub mod client;
pub mod server;

pub use client::{Options, run};
