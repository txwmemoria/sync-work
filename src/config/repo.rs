//! Repository-level configuration: `syncwork.toml`, committed to the repo.
//!
//! This file describes the *project* — its name and how to build it — and
//! nothing about the machines involved. Both checkouts therefore read an
//! identical file, which is what makes "run the same recipe on both platforms"
//! a guarantee rather than a manual convention.
//!
//! The project name here is also the join key: it is how two machines agree that
//! their respective checkouts are the same project. Machine-level state lives in
//! [`super::machine`].

use super::Platform;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const CONFIG_FILE: &str = "syncwork.toml";
pub const STATE_DIR: &str = ".sw";
/// The gitignore form of [`STATE_DIR`].
pub const STATE_DIR_IGNORE: &str = ".sw/";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Recipe {
    pub steps: Vec<String>,
    /// Append `--message-format=json-diagnostic-rendered-ansi` handling so
    /// failures come back as structured file:line diagnostics instead of a log
    /// blob. This is what lets an AI act on a remote failure directly.
    #[serde(default = "default_true")]
    pub cargo_json: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub windows: Option<Box<RecipeOverride>>,
    #[serde(default)]
    pub macos: Option<Box<RecipeOverride>>,
    #[serde(default)]
    pub linux: Option<Box<RecipeOverride>>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecipeOverride {
    pub steps: Option<Vec<String>>,
    pub cargo_json: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A recipe with platform overrides folded in — what actually gets executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedRecipe {
    pub name: String,
    pub steps: Vec<String>,
    pub cargo_json: bool,
    pub env: BTreeMap<String, String>,
}

impl Recipe {
    pub fn resolve(&self, name: &str, platform: Platform) -> ResolvedRecipe {
        let ov = match platform {
            Platform::Windows => self.windows.as_deref(),
            Platform::Macos => self.macos.as_deref(),
            Platform::Linux => self.linux.as_deref(),
        };
        let mut env = self.env.clone();
        let mut steps = self.steps.clone();
        let mut cargo_json = self.cargo_json;
        if let Some(ov) = ov {
            if let Some(s) = &ov.steps {
                steps = s.clone();
            }
            if let Some(c) = ov.cargo_json {
                cargo_json = c;
            }
            env.extend(ov.env.clone());
        }
        ResolvedRecipe {
            name: name.to_string(),
            steps,
            cargo_json,
            env,
        }
    }
}

// ---------------------------------------------------------------- root

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub name: String,
    /// Git remote used to make a base commit available to a peer that does not
    /// have it yet. Self-hosted GitLab is the expected case.
    #[serde(default = "default_remote")]
    pub remote: String,
}

fn default_remote() -> String {
    "origin".into()
}

/// What `sw ctx` carries between the machines.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextConfig {
    /// Sync Claude Code's per-project session and memory directory. Each machine
    /// derives the directory name from its own checkout path.
    #[serde(default = "default_true")]
    pub claude: bool,
    /// Additional repo-relative directories. Intended for state git does not
    /// track; anything committed already travels by git.
    #[serde(default)]
    pub extra: Vec<String>,
    /// Leave alone any file touched this recently — it is probably being written
    /// right now by a live session.
    #[serde(default = "default_quiet_secs")]
    pub quiet_secs: u64,
}

fn default_quiet_secs() -> u64 {
    5
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            claude: true,
            extra: Vec::new(),
            quiet_secs: default_quiet_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub project: Project,
    #[serde(default)]
    pub recipes: BTreeMap<String, Recipe>,
    #[serde(default)]
    pub context: ContextConfig,
}

impl Config {
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(CONFIG_FILE);
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!("no {CONFIG_FILE} at {}; run `sw init`", repo_root.display())
        })?;
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, repo_root: &Path) -> Result<()> {
        let path = repo_root.join(CONFIG_FILE);
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn recipe(&self, name: &str, platform: Platform) -> Result<ResolvedRecipe> {
        let r = self
            .recipes
            .get(name)
            .with_context(|| format!("no recipe `{name}` in {CONFIG_FILE}"))?;
        Ok(r.resolve(name, platform))
    }

    pub fn template(project: &str) -> Self {
        let mut recipes = BTreeMap::new();
        recipes.insert(
            "ci".to_string(),
            Recipe {
                steps: vec![
                    "cargo build --release --all-targets".into(),
                    "cargo test --all".into(),
                ],
                cargo_json: true,
                env: BTreeMap::new(),
                windows: None,
                macos: None,
                linux: None,
            },
        );
        recipes.insert(
            "check".to_string(),
            Recipe {
                steps: vec!["cargo clippy --all-targets -- -D warnings".into()],
                cargo_json: true,
                env: BTreeMap::new(),
                windows: None,
                macos: None,
                linux: None,
            },
        );
        Config {
            project: Project {
                name: project.to_string(),
                remote: default_remote(),
            },
            recipes,
            context: ContextConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_override_replaces_steps_and_merges_env() {
        let toml_src = r#"
            [project]
            name = "demo"

            [recipes.ci]
            steps = ["cargo test"]
            env = { RUST_BACKTRACE = "1" }

            [recipes.ci.windows]
            steps = ["cargo test -- --test-threads=1"]
            env = { CC = "clang-cl" }
        "#;
        let cfg: Config = toml::from_str(toml_src).unwrap();

        let win = cfg.recipe("ci", Platform::Windows).unwrap();
        assert_eq!(win.steps, ["cargo test -- --test-threads=1"]);
        assert_eq!(win.env.get("RUST_BACKTRACE").map(String::as_str), Some("1"));
        assert_eq!(win.env.get("CC").map(String::as_str), Some("clang-cl"));

        // A platform with no override block falls back to the base steps.
        let mac = cfg.recipe("ci", Platform::Macos).unwrap();
        assert_eq!(mac.steps, ["cargo test"]);
        assert!(!mac.env.contains_key("CC"));
    }
}
