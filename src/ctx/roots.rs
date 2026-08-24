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

use crate::config::{Config, RootSpec};
use std::path::{Path, PathBuf};

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

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Where one declared root lives on *this* machine, or `None` if it cannot be
/// placed here — an unset home directory, say.
fn locate(spec: &RootSpec, repo: &Path) -> Option<PathBuf> {
    match spec {
        RootSpec::ClaudeProject => {
            // `CLAUDE_CONFIG_DIR` replaces the whole `~/.claude`, which is how
            // Claude Code documents it.
            let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(dir) => PathBuf::from(dir),
                None => home_dir()?.join(".claude"),
            };
            // Derived from *this* machine's checkout path, which is why the two
            // sides never need to exchange absolute paths.
            Some(base.join("projects").join(encode_project_dir(repo)))
        }
        RootSpec::Home { path, env } => {
            let base = match env.as_deref().and_then(std::env::var_os) {
                Some(dir) => PathBuf::from(dir),
                None => home_dir()?,
            };
            Some(base.join(path))
        }
        RootSpec::Repo { path } => Some(repo.join(path)),
    }
}

/// Resolve the roots this machine should sync for `repo`.
///
/// A root that cannot be placed here is dropped rather than erroring: the peer may
/// well have it, and the manifest already models a root being absent on one side.
pub fn resolve_roots(cfg: &Config, repo: &Path) -> Vec<Root> {
    cfg.context
        .resolved_roots()
        .into_iter()
        .filter_map(|(key, spec)| locate(&spec, repo).map(|dir| Root { key, dir }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each root kind is a rule evaluated locally. What matters is that a rule
    /// only ever combines something the *machine* knows (its home, its checkout)
    /// with something the *config* knows (a relative path) — never an absolute
    /// path from the config, which would be meaningless on the other machine.
    #[test]
    fn each_root_kind_resolves_against_this_machine() {
        let repo = Path::new("/tmp/app");

        let inside = locate(
            &RootSpec::Repo {
                path: "docs/ai".into(),
            },
            repo,
        )
        .unwrap();
        assert_eq!(inside, repo.join("docs/ai"));

        // A home-relative root lands under this machine's home, wherever that is.
        let home = locate(
            &RootSpec::Home {
                path: ".codex/sessions".into(),
                env: None,
            },
            repo,
        )
        .unwrap();
        assert!(home.ends_with(".codex/sessions"), "got {}", home.display());
        assert!(home.is_absolute());

        // An env override replaces the base entirely. `CLAUDE_CONFIG_DIR` is the
        // one real instance, and the end-to-end tests rely on it to give each of
        // the two machines its own context directory on one host.
        let key = "SW_TEST_ROOT_BASE";
        unsafe { std::env::set_var(key, "/elsewhere") };
        let overridden = locate(
            &RootSpec::Home {
                path: "sessions".into(),
                env: Some(key.into()),
            },
            repo,
        )
        .unwrap();
        unsafe { std::env::remove_var(key) };
        assert_eq!(overridden, Path::new("/elsewhere").join("sessions"));
    }

    #[test]
    fn project_dir_encoding_matches_claude_code() {
        // Ground truth taken from a real ~/.claude/projects listing.
        assert_eq!(
            encode_project_dir(Path::new(r"D:\projects\sync-work")),
            "D--projects-sync-work"
        );
        assert_eq!(
            encode_project_dir(Path::new(r"C:\Users\dev")),
            "C--Users-dev"
        );
        assert_eq!(
            encode_project_dir(Path::new(r"C:\Users\dev\AppData\Roaming\Some App\logs")),
            "C--Users-dev-AppData-Roaming-Some-App-logs"
        );
        // The POSIX form a Mac would produce keeps its leading separator.
        assert_eq!(
            encode_project_dir(Path::new("/Users/me/projects/app")),
            "-Users-me-projects-app"
        );
    }
}
