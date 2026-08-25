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
    /// May be empty when every platform block declares its own steps — the
    /// fully explicit style for commands that share nothing across platforms.
    /// What is required is that the platform actually *executing* the recipe
    /// ends up with steps from somewhere; [`Config::recipe`] checks that.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
    /// Append `--message-format=json-diagnostic-rendered-ansi` handling so
    /// failures come back as structured file:line diagnostics instead of a log
    /// blob. This is what lets an AI act on a remote failure directly.
    #[serde(default = "default_true")]
    pub cargo_json: bool,
    /// Skipped when empty so the file `sw init` writes has no empty `[env]` tables
    /// in it — a starter config should read as an example, not as a form.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
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

/// Starter recipe sets for `sw init`.
///
/// The tool itself is ecosystem-agnostic — a recipe step is any shell command —
/// so all a template does is spare the first edit. `verify` is defined in every
/// one, because that is what a bare `sw verify` runs: the command and its
/// default recipe share a name, so there is no mapping to memorize.
#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Template {
    Rust,
    Node,
    Python,
    Go,
    Blank,
}

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

/// How to find one context root on whichever machine is asking.
///
/// Declared rather than hardcoded because there is more than one AI tool, and the
/// only thing they have in common is that their state lives *outside* the repo and
/// so never travels by git. What differs is where — and that is a fact about the
/// tool, which belongs in the committed config where both machines read the same
/// answer, not in per-machine setup that has to be repeated and kept in step.
///
/// Deliberately not a path: an absolute path from one machine is meaningless on the
/// other. Each variant is a *rule* both machines evaluate locally, so the wire only
/// ever carries the root's name.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RootSpec {
    /// Claude Code's per-project directory, whose name it derives from the
    /// checkout's absolute path. Its own rule because that encoding is a Claude
    /// Code implementation detail, verified against a real installation.
    ClaudeProject,
    /// A path under the user's home directory — `.codex/sessions`, say. The shape
    /// most tools have: same relative location on every machine, different home.
    Home {
        path: String,
        /// Environment variable that overrides the home directory when set, for a
        /// tool that offers one.
        #[serde(default)]
        env: Option<String>,
    },
    /// A path inside the checkout, for state git does not track. Anything committed
    /// already travels by git and does not belong here.
    Repo { path: String },
}

/// What `sw ctx` carries between the machines.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextConfig {
    /// Named roots to sync. The name is the only thing that crosses the wire; each
    /// machine resolves it against its own filesystem.
    #[serde(default)]
    pub roots: BTreeMap<String, RootSpec>,
    /// Leave alone any file touched this recently — it is probably being written
    /// right now by a live session.
    #[serde(default = "default_quiet_secs")]
    pub quiet_secs: u64,

    // ---- superseded by `roots`, still read so an existing checkout keeps working
    /// Was: sync Claude Code's directory. `None` means the file predates `roots`
    /// and should get the old default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<bool>,
    /// Was: additional repo-relative directories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

fn default_quiet_secs() -> u64 {
    5
}

/// Name of the root every project gets unless it says otherwise.
pub const ROOT_CLAUDE: &str = "claude";

impl ContextConfig {
    /// The roots to sync, with the pre-`roots` fields folded in.
    ///
    /// A `syncwork.toml` is committed, so both checkouts read the identical file
    /// and an old one must keep working rather than silently syncing nothing —
    /// which is what a plain rename of these fields would have caused, on a
    /// feature whose failure mode is "no context moved" and nobody notices.
    pub fn resolved_roots(&self) -> BTreeMap<String, RootSpec> {
        let mut out = self.roots.clone();
        // The old default: Claude on unless explicitly disabled. Only applied when
        // `roots` says nothing about it, so a config that deliberately drops it is
        // respected.
        if !out.contains_key(ROOT_CLAUDE) && self.claude.unwrap_or(self.roots.is_empty()) {
            out.insert(ROOT_CLAUDE.to_string(), RootSpec::ClaudeProject);
        }
        for path in &self.extra {
            out.insert(
                format!("extra:{path}"),
                RootSpec::Repo { path: path.clone() },
            );
        }
        out
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            roots: BTreeMap::from([(ROOT_CLAUDE.to_string(), RootSpec::ClaudeProject)]),
            quiet_secs: default_quiet_secs(),
            claude: None,
            extra: Vec::new(),
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
        let resolved = r.resolve(name, platform);
        if resolved.steps.is_empty() {
            anyhow::bail!(
                "recipe `{name}` has no steps for {platform} — add them to \
                 [recipes.{name}] or [recipes.{name}.{platform}]"
            );
        }
        Ok(resolved)
    }

    pub fn template(project: &str, remote: Option<&str>, kind: Template) -> Self {
        fn recipe(steps: &[&str]) -> Recipe {
            Recipe {
                steps: steps.iter().map(|s| s.to_string()).collect(),
                cargo_json: true,
                env: BTreeMap::new(),
                windows: None,
                macos: None,
                linux: None,
            }
        }
        let mut recipes = BTreeMap::new();
        // `verify` is what a bare `sw verify` runs — the command and the recipe
        // share one name so there is no default to memorize. `check` is the
        // conventional fast gate where the ecosystem has one.
        match kind {
            Template::Rust => {
                recipes.insert(
                    "verify".into(),
                    recipe(&["cargo build --release --all-targets", "cargo test --all"]),
                );
                recipes.insert(
                    "check".into(),
                    recipe(&["cargo clippy --all-targets -- -D warnings"]),
                );
            }
            Template::Node => {
                recipes.insert("verify".into(), recipe(&["npm ci", "npm test"]));
            }
            Template::Python => {
                recipes.insert("verify".into(), recipe(&["pytest"]));
            }
            Template::Go => {
                recipes.insert(
                    "verify".into(),
                    recipe(&["go build ./...", "go test ./..."]),
                );
                recipes.insert("check".into(), recipe(&["go vet ./..."]));
            }
            Template::Blank => {
                recipes.insert(
                    "verify".into(),
                    recipe(&["echo replace me: this recipe runs on the peer"]),
                );
            }
        }
        Config {
            project: Project {
                name: project.to_string(),
                remote: remote.map(str::to_string).unwrap_or_else(default_remote),
            },
            recipes,
            context: ContextConfig::default(),
        }
    }
}

/// Point out keys the parser would silently ignore.
///
/// Serde tolerates unknown fields on purpose — two machines can run different
/// versions of `sw` against the same committed file, and a newer field must not
/// break the older binary. The cost of that tolerance is the silent typo:
/// `[recipes.build.mac]` parses fine and does nothing, because the platform key
/// is `macos`. Nothing would ever say so. This walks the raw document and names
/// every key the typed config has no home for, so `sw doctor` can.
pub fn lint(repo_root: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(repo_root.join(CONFIG_FILE)) else {
        return Vec::new();
    };
    // `toml::Table`, not `toml::Value`: in toml 0.9 a whole document only
    // parses as the former — `Value::from_str` wants a single value and fails.
    let Ok(doc) = raw.parse::<toml::Table>() else {
        return Vec::new(); // a parse failure is loud on its own
    };
    let mut warnings = Vec::new();
    // A plain function, not a closure: the closure would hold `warnings`
    // mutably for the whole body and block every direct push below.
    fn check(
        warnings: &mut Vec<String>,
        table: &toml::Value,
        at: &str,
        known: &[&str],
        hint: &str,
    ) {
        if let Some(t) = table.as_table() {
            for key in t.keys() {
                if !known.contains(&key.as_str()) {
                    warnings.push(format!("`{at}` has an unknown key `{key}`{hint}"));
                }
            }
        }
    }

    for key in doc.keys() {
        if !["project", "recipes", "context"].contains(&key.as_str()) {
            warnings.push(format!("`syncwork.toml` has an unknown key `{key}`"));
        }
    }
    if let Some(project) = doc.get("project") {
        check(&mut warnings, project, "project", &["name", "remote"], "");
    }
    if let Some(recipes) = doc.get("recipes").and_then(|r| r.as_table()) {
        for (name, recipe) in recipes {
            check(
                &mut warnings,
                recipe,
                &format!("recipes.{name}"),
                &["steps", "cargo_json", "env", "windows", "macos", "linux"],
                " — platform overrides are windows / macos / linux",
            );
            for platform in ["windows", "macos", "linux"] {
                if let Some(ov) = recipe.get(platform) {
                    check(
                        &mut warnings,
                        ov,
                        &format!("recipes.{name}.{platform}"),
                        &["steps", "cargo_json", "env"],
                        "",
                    );
                }
            }
        }
    }
    if let Some(context) = doc.get("context") {
        check(
            &mut warnings,
            context,
            "context",
            &["roots", "quiet_secs", "claude", "extra"],
            "",
        );
        if let Some(roots) = context.get("roots").and_then(|r| r.as_table()) {
            for (name, root) in roots {
                check(
                    &mut warnings,
                    root,
                    &format!("context.roots.{name}"),
                    &["kind", "path", "env"],
                    "",
                );
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `syncwork.toml` is committed, so both machines read the identical file and
    /// one written before `roots` existed has to keep working. The failure mode of
    /// getting this wrong is silent — no context moves, and nobody notices for a
    /// week — so each legacy shape is pinned here.
    #[test]
    fn a_config_predating_declared_roots_still_syncs_what_it_used_to() {
        // Nothing said about context at all: Claude, as before.
        let bare: Config = toml::from_str("[project]\nname = \"demo\"\n").unwrap();
        assert_eq!(
            bare.context.resolved_roots(),
            BTreeMap::from([(ROOT_CLAUDE.into(), RootSpec::ClaudeProject)])
        );

        // The old fields, both of them.
        let legacy: Config = toml::from_str(
            "[project]\nname = \"demo\"\n\n[context]\nclaude = true\nextra = [\"notes\"]\n",
        )
        .unwrap();
        assert_eq!(
            legacy.context.resolved_roots(),
            BTreeMap::from([
                (ROOT_CLAUDE.into(), RootSpec::ClaudeProject),
                (
                    "extra:notes".into(),
                    RootSpec::Repo {
                        path: "notes".into()
                    }
                ),
            ])
        );

        // And an explicit opt-out stays opted out.
        let off: Config =
            toml::from_str("[project]\nname = \"demo\"\n\n[context]\nclaude = false\n").unwrap();
        assert!(off.context.resolved_roots().is_empty());
    }

    #[test]
    fn declared_roots_carry_their_own_rule_for_finding_the_directory() {
        let cfg: Config = toml::from_str(
            r#"
            [project]
            name = "demo"

            [context.roots.claude]
            kind = "claude-project"

            [context.roots.codex]
            kind = "home"
            path = ".codex/sessions"

            [context.roots.notes]
            kind = "repo"
            path = "docs/ai"
            "#,
        )
        .unwrap();
        let roots = cfg.context.resolved_roots();
        assert_eq!(roots.len(), 3);
        assert_eq!(roots["claude"], RootSpec::ClaudeProject);
        assert_eq!(
            roots["codex"],
            RootSpec::Home {
                path: ".codex/sessions".into(),
                env: None
            }
        );
        assert_eq!(
            roots["notes"],
            RootSpec::Repo {
                path: "docs/ai".into()
            }
        );

        // Declaring roots without a `claude` entry means exactly that — the old
        // default must not sneak back in and sync a directory nobody asked for.
        let without: Config = toml::from_str(
            "[project]\nname = \"demo\"\n\n[context.roots.notes]\nkind = \"repo\"\npath = \"d\"\n",
        )
        .unwrap();
        assert!(!without.context.resolved_roots().contains_key(ROOT_CLAUDE));
    }

    /// The tolerance that keeps old binaries reading new configs is also what
    /// swallows typos. A real user wrote `[recipes.build.mac]` — legal TOML,
    /// silently ignored, the mac-specific command never ran. Lint names it.
    #[test]
    fn lint_names_the_keys_the_parser_would_silently_swallow() {
        let dir = std::env::temp_dir().join(format!(
            "sw-lint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join(CONFIG_FILE),
            "[project]\nname = \"demo\"\n\n[recipes.build]\nsteps = [\"sh build.sh\"]\n\n\
             [recipes.build.mac]\nsteps = [\"x\"]\n\n[context.roots.notes]\nkind = \"repo\"\n\
             path = \"d\"\nenvv = \"typo\"\n",
        )
        .unwrap();
        let warnings = lint(&dir);
        assert!(
            warnings.iter().any(|w| w.contains("recipes.build")
                && w.contains("`mac`")
                && w.contains("windows / macos / linux")),
            "mac typo not caught: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("`envv`")),
            "root key typo not caught: {warnings:?}"
        );

        // A clean file stays quiet.
        std::fs::write(
            dir.join(CONFIG_FILE),
            "[project]\nname = \"demo\"\n\n[recipes.verify]\nsteps = [\"cargo test\"]\n\n\
             [recipes.verify.macos]\nsteps = [\"cargo test -j2\"]\n",
        )
        .unwrap();
        assert!(lint(&dir).is_empty(), "false positives: {:?}", lint(&dir));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two platforms whose commands share nothing should not have to invent a
    /// fake "base" for one to override — the explicit style is legal, and the
    /// platform left without steps errors at run time, by name.
    #[test]
    fn a_recipe_may_be_declared_per_platform_with_no_base() {
        let cfg: Config = toml::from_str(
            "[project]\nname = \"demo\"\n\n[recipes.build.windows]\nsteps = [\"build.cmd\"]\n\n\
             [recipes.build.macos]\nsteps = [\"sh build.sh mac\"]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.recipe("build", Platform::Windows).unwrap().steps,
            ["build.cmd"]
        );
        assert_eq!(
            cfg.recipe("build", Platform::Macos).unwrap().steps,
            ["sh build.sh mac"]
        );
        let err = cfg
            .recipe("build", Platform::Linux)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no steps for linux") && err.contains("recipes.build.linux"),
            "unhelpful error: {err}"
        );
    }

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
