//! Where context lives, derived independently on each machine.
//!
//! Claude Code names its per-project state directory after the project's
//! *absolute path*, replacing every character outside `[A-Za-z0-9-]` with `-`:
//!
//! ```text
//! D:\projects\sync-work   ->  D--projects-sync-work
//! /Users/me/projects/app  ->  -Users-me-projects-app
//! ```
//!
//! The two machines therefore disagree on the directory name. Rather than
//! translating paths across the wire, each side derives its *own* name from its
//! *own* checkout path; the wire only ever carries the logical root key
//! (`claude`) and paths relative to it.
//!
//! A happy consequence of the encoding: `:`, `/` and `\` all collapse to `-`, so
//! it makes no difference that `git rev-parse` reports forward slashes on Windows
//! while Claude Code encoded from backslashes.

use crate::config::Config;
use std::path::{Path, PathBuf};

/// Logical key for the Claude Code project directory.
pub const ROOT_CLAUDE: &str = "claude";

/// A logical root paired with where it lives on *this* machine.
#[derive(Debug, Clone)]
pub struct Root {
    pub key: String,
    pub dir: PathBuf,
}

/// Encode an absolute path the way Claude Code names its project directories.
pub fn encode_project_dir(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Base of Claude Code's configuration, honouring the documented override.
fn claude_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join(".claude"))
}

/// Resolve the roots this machine should sync for `repo`.
pub fn resolve_roots(cfg: &Config, repo: &Path) -> Vec<Root> {
    let mut roots = Vec::new();
    if cfg.context.claude
        && let Some(home) = claude_home()
    {
        // Derived from *this* machine's checkout path, which is why the two
        // sides never need to exchange absolute paths.
        let dir = home.join("projects").join(encode_project_dir(repo));
        roots.push(Root {
            key: ROOT_CLAUDE.into(),
            dir,
        });
    }
    for rel in &cfg.context.extra {
        roots.push(Root {
            key: format!("extra:{rel}"),
            dir: repo.join(rel),
        });
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_encoding_matches_claude_code() {
        // Ground truth taken from a real ~/.claude/projects listing.
        assert_eq!(
            encode_project_dir(Path::new(r"D:\projects\sync-work")),
            "D--projects-sync-work"
        );
        assert_eq!(
            encode_project_dir(Path::new(r"C:\Users\sdkpo")),
            "C--Users-sdkpo"
        );
        assert_eq!(
            encode_project_dir(Path::new(r"C:\Users\sdkpo\AppData\Roaming\Beem\logs")),
            "C--Users-sdkpo-AppData-Roaming-Beem-logs"
        );
        // The POSIX form a Mac would produce keeps its leading separator.
        assert_eq!(
            encode_project_dir(Path::new("/Users/me/projects/app")),
            "-Users-me-projects-app"
        );
    }
}
