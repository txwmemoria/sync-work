//! Configuration, in two tiers.
//!
//! | tier   | file                              | holds                        |
//! |--------|-----------------------------------|------------------------------|
//! | repo   | `<repo>/syncwork.toml` (in git)   | project name, recipes        |
//! | machine| `~/.config/syncwork/config.toml`   | peers, project registry      |
//!
//! Keeping them apart is what lets one node serve many repositories: nothing
//! machine-specific is ever committed, and no filesystem path is ever shared.

pub mod machine;
pub mod platform;
pub mod repo;

pub use machine::{Identity, Machine, Peer};
pub use platform::Platform;
pub use repo::{CONFIG_FILE, Config, ResolvedRecipe, STATE_DIR, STATE_DIR_IGNORE};
