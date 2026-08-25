//! Build and edit `syncwork.toml` from the command line.
//!
//! The config file is where this tool's actual power lives — recipes are
//! arbitrary shell commands, roots are arbitrary sync targets — so getting a
//! working one should not require reading a reference first. `sw init` asks its
//! questions one at a time; `sw recipe` and `sw root` make the later edits.
//!
//! Edits go through `toml_edit`, not serde round-tripping: the file is the
//! user's, and rewriting it wholesale would discard their comments and
//! ordering. After every mutation the document is re-parsed as a [`Config`]
//! before anything touches disk, so a bug here produces an error, never a file
//! the rest of the tool refuses to read.

use crate::cli::{RecipeCmd, RootCmd};
use crate::config::{self, Config, Template};
use crate::style::Paint;
use crate::{ctx, git};
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

// ---------------------------------------------------------------- sw init

/// Ask on a terminal; take the default anywhere else.
///
/// The "anywhere else" half is what keeps this safe to add to a command that
/// existing scripts (and the e2e suite) already call: a prompt nobody is there
/// to answer would hang them forever.
fn ask(question: &str, default: &str) -> String {
    if !std::io::stdin().is_terminal() {
        return default.to_string();
    }
    print!("{question} [{default}]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default.to_string();
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn ask_template() -> Template {
    loop {
        match ask(
            "starter recipes (rust / node / python / go / blank)",
            "rust",
        )
        .as_str()
        {
            "rust" => return Template::Rust,
            "node" => return Template::Node,
            "python" => return Template::Python,
            "go" => return Template::Go,
            "blank" => return Template::Blank,
            other => println!("  no template named `{other}`"),
        }
    }
}

pub async fn init(
    repo: &Path,
    name: Option<String>,
    remote: Option<String>,
    template: Option<Template>,
    yes: bool,
) -> Result<i32> {
    let path = repo.join(config::CONFIG_FILE);
    if path.exists() {
        bail!(
            "{} already exists — edit it with `sw recipe` / `sw root`, or by hand",
            path.display()
        );
    }

    let dir_name = repo
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());

    // Flags win; otherwise ask, one question at a time. `--yes` (or a non-tty
    // stdin, inside `ask`) turns every remaining question into its default.
    let name = match name {
        Some(n) => n,
        None if yes => dir_name,
        None => ask("project name", &dir_name),
    };
    let remote = match remote {
        Some(r) => r,
        None if yes => "origin".into(),
        None => ask("git remote the peer fetches from", "origin"),
    };
    let template = match template {
        Some(t) => t,
        None if yes => Template::Rust,
        None if !std::io::stdin().is_terminal() => Template::Rust,
        None => ask_template(),
    };

    let cfg = Config::template(&name, Some(&remote), template);
    cfg.save(repo)?;
    git::ensure_ignored(repo, config::STATE_DIR_IGNORE).await?;
    super::project::link(repo, &name, false)?;

    println!("{} {}", "created".green().bold(), path.display());
    println!("  added .sw/ to .gitignore");
    for (rname, r) in &cfg.recipes {
        println!("  recipe {}  {}", rname.cyan(), r.steps.join("  &&  "));
    }
    println!();
    println!(
        "{}",
        "Commit syncwork.toml, then in your clone on the other machine:".bold()
    );
    println!("  {}", "sw link".cyan());
    println!();
    println!("If the machines are not paired yet, do that once — from anywhere:");
    println!("  {}", "sw pair init".cyan());
    Ok(0)
}

// ---------------------------------------------------------------- shared

fn load_doc(repo: &Path) -> Result<DocumentMut> {
    let path = repo.join(config::CONFIG_FILE);
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no {} at {}; run `sw init`",
            config::CONFIG_FILE,
            repo.display()
        )
    })?;
    raw.parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

/// Re-parse the edited document as a [`Config`], then write it.
///
/// The parse is the safety net: `toml_edit` will happily produce structures
/// serde rejects, and the failure must land here as an error — not later, as a
/// node refusing to read the project's config.
fn save_doc(repo: &Path, doc: &DocumentMut) -> Result<()> {
    let rendered = doc.to_string();
    toml::from_str::<Config>(&rendered).context("edit would produce an invalid syncwork.toml")?;
    std::fs::write(repo.join(config::CONFIG_FILE), rendered)
        .with_context(|| format!("writing {}", config::CONFIG_FILE))?;
    Ok(())
}

/// A `[section.name]` table that renders as its own header block, like the ones
/// `sw init` writes — not the inline `{ ... }` form `toml_edit` defaults to for
/// implicitly created tables.
fn subtable<'a>(doc: &'a mut DocumentMut, section: &str, name: &str) -> &'a mut Table {
    let section = doc
        .entry(section)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("section is a table");
    // Keep `[context]` itself from rendering as an empty header when it only
    // exists to hold children.
    section.set_implicit(true);
    section
        .entry(name)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("entry is a table")
}

// ---------------------------------------------------------------- sw recipe

pub fn recipe(repo: &Path, cmd: Option<RecipeCmd>) -> Result<i32> {
    match cmd.unwrap_or(RecipeCmd::List) {
        RecipeCmd::List => {
            let cfg = Config::load(repo)?;
            if cfg.recipes.is_empty() {
                println!("no recipes — add one: sw recipe add verify \"cargo test\"");
                return Ok(0);
            }
            for (name, r) in &cfg.recipes {
                println!("{}", name.cyan().bold());
                for step in &r.steps {
                    println!("  {step}");
                }
                for (platform, ov) in [
                    ("windows", &r.windows),
                    ("macos", &r.macos),
                    ("linux", &r.linux),
                ] {
                    if let Some(ov) = ov
                        && let Some(steps) = &ov.steps
                    {
                        println!("  {} {}", "on".dimmed(), platform);
                        for step in steps {
                            println!("    {step}");
                        }
                    }
                }
            }
            Ok(0)
        }

        RecipeCmd::Add {
            name,
            steps,
            env,
            platform,
        } => {
            let mut doc = load_doc(repo)?;

            // A platform block may stand alone — two platforms whose commands
            // share nothing need no fake base to override. Whichever platform
            // ends up with no steps is refused at run time, by name.
            let table = match &platform {
                None => subtable(&mut doc, "recipes", &name),
                Some(p) => {
                    if !["windows", "macos", "linux"].contains(&p.as_str()) {
                        bail!("no platform `{p}` — windows, macos or linux");
                    }
                    let recipe = subtable(&mut doc, "recipes", &name);
                    recipe.set_implicit(true);
                    recipe
                        .entry(p.as_str())
                        .or_insert(Item::Table(Table::new()))
                        .as_table_mut()
                        .expect("override is a table")
                }
            };

            let mut arr = toml_edit::Array::new();
            for s in &steps {
                arr.push(s.as_str());
            }
            table["steps"] = value(arr);
            for pair in &env {
                let (k, v) = pair
                    .split_once('=')
                    .with_context(|| format!("`{pair}` is not KEY=VALUE"))?;
                table
                    .entry("env")
                    .or_insert(Item::Table(Table::new()))
                    .as_table_mut()
                    .context("env is not a table")?
                    .insert(k, value(v));
            }

            save_doc(repo, &doc)?;
            let target = match platform {
                Some(p) => format!("{name} (on {p})"),
                None => name.clone(),
            };
            println!("{} {target}", "set".green().bold());
            println!("  run it: {}", format!("sw verify {name}").cyan());
            println!("  the peer runs its own copy — commit, and pull there");
            Ok(0)
        }

        RecipeCmd::Remove { name } => {
            let mut doc = load_doc(repo)?;
            let removed = doc
                .get_mut("recipes")
                .and_then(|r| r.as_table_mut())
                .and_then(|t| t.remove(&name))
                .is_some();
            if !removed {
                bail!("no recipe `{name}`");
            }
            save_doc(repo, &doc)?;
            println!("{} {name}", "removed".green().bold());
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------- sw root

pub fn root(repo: &Path, cmd: Option<RootCmd>) -> Result<i32> {
    match cmd.unwrap_or(RootCmd::List) {
        RootCmd::List => {
            let cfg = Config::load(repo)?;
            let declared = cfg.context.resolved_roots();
            if declared.is_empty() {
                println!("no context roots — nothing will sync for this project");
                return Ok(0);
            }
            // Resolved locations answer the question people actually have —
            // "which directory is this, on THIS machine" — and expose a root
            // that resolves nowhere here.
            let resolved = ctx::resolve_roots(&cfg, repo);
            for (name, spec) in &declared {
                let here = resolved
                    .iter()
                    .find(|r| &r.key == name)
                    .map(|r| r.dir.display().to_string())
                    .unwrap_or_else(|| "(does not resolve on this machine)".into());
                println!("{}  {spec:?}", name.cyan().bold());
                println!("  here: {here}");
            }
            Ok(0)
        }

        RootCmd::Add {
            name,
            kind,
            path,
            env,
        } => {
            let mut doc = load_doc(repo)?;
            let table = subtable(&mut doc, "context", "roots");
            table.set_implicit(true);
            let entry = table
                .entry(&name)
                .or_insert(Item::Table(Table::new()))
                .as_table_mut()
                .context("root is not a table")?;

            match kind.as_str() {
                "claude-project" => {
                    if path.is_some() || env.is_some() {
                        bail!("claude-project derives its path itself — drop --path/--env");
                    }
                    entry.insert("kind", value("claude-project"));
                    entry.remove("path");
                    entry.remove("env");
                }
                "home" | "repo" => {
                    let p = path.with_context(|| format!("kind `{kind}` needs --path"))?;
                    entry.insert("kind", value(kind.as_str()));
                    entry.insert("path", value(p));
                    match (kind.as_str(), env) {
                        ("home", Some(e)) => {
                            entry.insert("env", value(e));
                        }
                        ("home", None) => {
                            entry.remove("env");
                        }
                        (_, Some(_)) => bail!("--env only applies to the home kind"),
                        _ => {}
                    }
                }
                other => bail!("no kind `{other}` — claude-project, home or repo"),
            }

            save_doc(repo, &doc)?;
            println!("{} {name}", "set".green().bold());
            println!("  check where it points: {}", "sw root".cyan());
            Ok(0)
        }

        RootCmd::Remove { name } => {
            let mut doc = load_doc(repo)?;
            let removed = doc
                .get_mut("context")
                .and_then(|c| c.get_mut("roots"))
                .and_then(|r| r.as_table_mut())
                .and_then(|t| t.remove(&name))
                .is_some();
            if !removed {
                bail!("no root `{name}`");
            }
            save_doc(repo, &doc)?;
            println!("{} {name}", "removed".green().bold());
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootSpec;

    /// A scratch repo holding a config with a comment in it. The comment is the
    /// test: CLI edits must leave the user's file recognisably theirs.
    fn scratch(tag: &str) -> std::path::PathBuf {
        // Tagged per test: the suite runs tests in parallel inside one process,
        // so pid + timestamp alone produced the same directory for both and the
        // interleaved writes corrupted the file.
        let dir = std::env::temp_dir().join(format!(
            "sw-configure-{tag}-{}-{}",
            std::process::id(),
            ctx::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(config::CONFIG_FILE),
            "# hands off my comment\n[project]\nname = \"demo\"\n\n[recipes.ci]\nsteps = [\"cargo test\"]\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn recipe_edits_preserve_the_users_file_and_validate_the_result() {
        let dir = scratch("recipe");

        recipe(
            &dir,
            Some(RecipeCmd::Add {
                name: "e2e".into(),
                steps: vec!["bash tests/e2e.sh".into()],
                env: vec!["CI=1".into()],
                platform: None,
            }),
        )
        .unwrap();

        let raw = std::fs::read_to_string(dir.join(config::CONFIG_FILE)).unwrap();
        assert!(
            raw.contains("# hands off my comment"),
            "comment lost:\n{raw}"
        );
        let cfg: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg.recipes["e2e"].steps, ["bash tests/e2e.sh"]);
        assert_eq!(cfg.recipes["e2e"].env["CI"], "1");
        // The pre-existing recipe is untouched.
        assert_eq!(cfg.recipes["ci"].steps, ["cargo test"]);

        // A platform override lands inside the recipe, not beside it.
        recipe(
            &dir,
            Some(RecipeCmd::Add {
                name: "ci".into(),
                steps: vec!["cargo test -- --test-threads=1".into()],
                env: vec![],
                platform: Some("windows".into()),
            }),
        )
        .unwrap();
        let cfg = Config::load(&dir).unwrap();
        let win = cfg.recipes["ci"].windows.as_ref().unwrap();
        assert_eq!(
            win.steps.as_deref().unwrap(),
            ["cargo test -- --test-threads=1"]
        );

        // A platform block with no base is the explicit per-platform style,
        // and creating it directly is legal.
        recipe(
            &dir,
            Some(RecipeCmd::Add {
                name: "ghost".into(),
                steps: vec!["x".into()],
                env: vec![],
                platform: Some("macos".into()),
            }),
        )
        .unwrap();
        let cfg = Config::load(&dir).unwrap();
        let ghost = &cfg.recipes["ghost"];
        assert!(ghost.steps.is_empty(), "no base steps were asked for");
        assert_eq!(
            ghost.macos.as_ref().unwrap().steps.as_deref().unwrap(),
            ["x"]
        );

        recipe(&dir, Some(RecipeCmd::Remove { name: "e2e".into() })).unwrap();
        assert!(!Config::load(&dir).unwrap().recipes.contains_key("e2e"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_edits_write_the_declared_forms_and_reject_nonsense() {
        let dir = scratch("root");

        root(
            &dir,
            Some(RootCmd::Add {
                name: "notes".into(),
                kind: "repo".into(),
                path: Some("docs/ai".into()),
                env: None,
            }),
        )
        .unwrap();
        root(
            &dir,
            Some(RootCmd::Add {
                name: "codex".into(),
                kind: "home".into(),
                path: Some(".codex/sessions".into()),
                env: Some("CODEX_HOME".into()),
            }),
        )
        .unwrap();

        let roots = Config::load(&dir).unwrap().context.resolved_roots();
        assert_eq!(
            roots["notes"],
            RootSpec::Repo {
                path: "docs/ai".into()
            }
        );
        assert_eq!(
            roots["codex"],
            RootSpec::Home {
                path: ".codex/sessions".into(),
                env: Some("CODEX_HOME".into())
            }
        );
        // Declaring roots explicitly means the implicit claude default is gone —
        // the CLI must behave exactly like hand-written config here.
        assert!(!roots.contains_key("claude"));

        for (kind, path, env) in [
            ("repo", None, None),                         // repo needs a path
            ("claude-project", Some("x".into()), None),   // derives its own path
            ("repo", Some("x".into()), Some("E".into())), // env is home-only
            ("volcano", Some("x".into()), None),          // not a kind
        ] {
            assert!(
                root(
                    &dir,
                    Some(RootCmd::Add {
                        name: "bad".into(),
                        kind: kind.into(),
                        path,
                        env,
                    }),
                )
                .is_err(),
                "kind={kind} should have been refused"
            );
        }

        root(
            &dir,
            Some(RootCmd::Remove {
                name: "notes".into(),
            }),
        )
        .unwrap();
        assert!(
            !Config::load(&dir)
                .unwrap()
                .context
                .resolved_roots()
                .contains_key("notes")
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
