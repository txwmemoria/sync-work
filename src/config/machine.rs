//! Machine-level state: who this node is, which machines it trusts, and where
//! each project lives *here*.
//!
//! This is the half of the configuration that is emphatically not per-repository.
//! An earlier design kept the peer table inside each repo's committed
//! `syncwork.toml`, which meant pairing had to be repeated for every project and
//! a machine needed one resident node per checkout. For anyone working across
//! more than one repo that is the wrong shape entirely.
//!
//! The split is:
//!
//! | lives in                        | holds                                  |
//! |---------------------------------|----------------------------------------|
//! | `~/.config/syncwork/config.toml`| node name, peers, project registry     |
//! | `~/.config/syncwork/identity.key`| the secret key                        |
//! | `<repo>/syncwork.toml` (in git) | project name, recipes, context options |
//!
//! Peer entries carry no filesystem paths. Each machine records where a project
//! lives on *its own* disk, so a project is joined across machines purely by the
//! name in its committed `syncwork.toml`.

use crate::config::Platform;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.toml";
pub const IDENTITY_FILE: &str = "identity.key";

/// Directory holding this machine's syncwork state.
///
/// `SW_HOME` overrides it, which is what lets two independent nodes run on one
/// box — used by the end-to-end tests, and occasionally useful for an isolated
/// second identity.
pub fn home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SW_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("", "", "syncwork")
        .context("cannot determine the configuration directory")?;
    Ok(dirs.config_dir().to_path_buf())
}

// ---------------------------------------------------------------- identity

/// The node's long-lived keypair. Its public half is the address other machines
/// dial and the credential they authorise.
pub struct Identity {
    pub secret: iroh::SecretKey,
}

impl Identity {
    /// Load the persistent key, generating it on first run. Identity must be
    /// stable across restarts or every reboot would invalidate every pairing.
    pub fn load_or_create() -> Result<Self> {
        let path = home()?.join(IDENTITY_FILE);
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let bytes = hex::decode(raw.trim()).context("identity.key is not valid hex")?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("identity.key must be 32 bytes"))?;
            return Ok(Self {
                secret: iroh::SecretKey::from_bytes(&arr),
            });
        }
        let secret = iroh::SecretKey::generate();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, hex::encode(secret.to_bytes()))?;
        restrict_permissions(&path)?;
        Ok(Self { secret })
    }

    pub fn id_string(&self) -> String {
        self.secret.public().to_string()
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    // On Windows the file inherits the user-profile ACL, which is already
    // user-only. Tightening further would mean pulling in a Win32 ACL crate.
    Ok(())
}

// ---------------------------------------------------------------- peers

/// Another machine. Deliberately path-free: where a project lives is that
/// machine's own business, recorded in its own registry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Peer {
    /// iroh `EndpointId` — a public key.
    pub id: String,
    pub platform: Platform,
}

/// Where a project lives on this machine.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectEntry {
    pub path: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NodeInfo {
    /// Human-readable name other machines see. Defaults to the hostname.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Machine {
    #[serde(default)]
    pub node: NodeInfo,
    /// Other machines only — this one is never listed, since the table is local.
    #[serde(default)]
    pub peers: BTreeMap<String, Peer>,
    /// Project name (from its committed `syncwork.toml`) to local path.
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectEntry>,
}

impl Machine {
    pub fn path() -> Result<PathBuf> {
        Ok(home()?.join(CONFIG_FILE))
    }

    /// A machine that has never been configured is an empty one, not an error.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
            }
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn node_name(&self) -> String {
        self.node.name.clone().unwrap_or_else(default_node_name)
    }

    /// Pick the peer to talk to. With exactly one paired machine no flag is
    /// needed, which is the whole point for a two-machine setup.
    pub fn resolve_peer(&self, requested: Option<&str>) -> Result<(String, Peer)> {
        if let Some(name) = requested {
            let p = self
                .peers
                .get(name)
                .with_context(|| format!("no peer `{name}`; run `sw pair list`"))?;
            return Ok((name.to_string(), p.clone()));
        }
        match self.peers.len() {
            0 => bail!("no peers paired yet; run `sw pair init` on one machine"),
            1 => {
                let (n, p) = self.peers.iter().next().expect("len checked");
                Ok((n.clone(), p.clone()))
            }
            _ => {
                let names: Vec<&str> = self.peers.keys().map(String::as_str).collect();
                bail!("multiple peers ({}); pass --peer <name>", names.join(", "))
            }
        }
    }

    /// Resolve a project name to a checkout on this machine, for a peer's request.
    ///
    /// The error is a plain `String` because it travels back over the wire and is
    /// printed on the *other* machine — so it is phrased for whoever is sitting
    /// there, and names the machine and the fix.
    pub fn resolve_project(project: &str) -> std::result::Result<PathBuf, String> {
        let machine = Self::load().map_err(|e| format!("{e:#}"))?;
        let host = machine.node_name();
        match machine.project_path(project) {
            Some(p) if p.is_dir() => Ok(p),
            Some(p) => Err(format!(
                "project `{project}` is linked on {host} to {}, which no longer exists — \
                 re-run `sw link` there",
                p.display()
            )),
            None => Err(format!(
                "project `{project}` is not linked on {host} — run `sw link` in your clone \
                 of it there"
            )),
        }
    }

    /// Resolve a project name to its checkout on this machine.
    pub fn project_path(&self, name: &str) -> Option<PathBuf> {
        self.projects.get(name).map(|p| PathBuf::from(&p.path))
    }

    /// Record where a project lives here. Returns true when something changed,
    /// so callers can stay quiet on the common no-op path.
    pub fn link(&mut self, name: &str, path: &Path) -> bool {
        let path = path.display().to_string();
        match self.projects.get(name) {
            Some(existing) if existing.path == path => false,
            _ => {
                self.projects
                    .insert(name.to_string(), ProjectEntry { path });
                true
            }
        }
    }

    pub fn unlink(&mut self, name: &str) -> bool {
        self.projects.remove(name).is_some()
    }
}

pub fn default_node_name() -> String {
    let raw = gethostname::gethostname().to_string_lossy().to_string();
    // Names become TOML table keys and service identifiers, so keep the alphabet
    // boring rather than quoting defensively everywhere downstream.
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "node".into()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> Peer {
        Peer {
            id: id.into(),
            platform: Platform::Macos,
        }
    }

    #[test]
    fn a_single_peer_needs_no_flag_and_several_do() {
        let mut m = Machine::default();
        assert!(m.resolve_peer(None).is_err(), "nothing paired yet");

        m.peers.insert("mac".into(), peer("aaa"));
        assert_eq!(m.resolve_peer(None).unwrap().0, "mac");

        m.peers.insert("linux".into(), peer("bbb"));
        assert!(m.resolve_peer(None).is_err(), "ambiguous without --peer");
        assert_eq!(m.resolve_peer(Some("linux")).unwrap().0, "linux");
        assert!(m.resolve_peer(Some("nope")).is_err());
    }

    #[test]
    fn the_registry_maps_project_names_to_local_paths() {
        let mut m = Machine::default();
        assert!(m.project_path("app").is_none());

        // Two projects coexist — the case the per-repo design could not serve.
        assert!(m.link("app", Path::new("/home/me/app")));
        assert!(m.link("other", Path::new("/home/me/work/other")));
        assert_eq!(
            m.project_path("app").unwrap(),
            PathBuf::from("/home/me/app")
        );
        assert_eq!(
            m.project_path("other").unwrap(),
            PathBuf::from("/home/me/work/other")
        );

        // Re-linking the same path is a no-op so callers can stay silent.
        assert!(!m.link("app", Path::new("/home/me/app")));
        // A moved checkout updates in place rather than duplicating.
        assert!(m.link("app", Path::new("/home/me/moved")));
        assert_eq!(m.projects.len(), 2);

        assert!(m.unlink("app"));
        assert!(!m.unlink("app"));
    }

    #[test]
    fn machine_config_round_trips_through_toml() {
        let mut m = Machine::default();
        m.node.name = Some("win-desktop".into());
        m.peers.insert("mac".into(), peer("abc"));
        m.link("app", Path::new("/p/app"));

        let raw = toml::to_string_pretty(&m).unwrap();
        let back: Machine = toml::from_str(&raw).unwrap();
        assert_eq!(back.node_name(), "win-desktop");
        assert_eq!(back.peers["mac"].id, "abc");
        assert_eq!(back.projects["app"].path, "/p/app");
        // Peer entries must not carry paths; that was the old mistake.
        assert!(!raw.contains("repo"), "peer table leaked a path: {raw}");
    }

    #[test]
    fn an_unconfigured_machine_loads_as_empty() {
        let m = Machine::default();
        assert!(m.peers.is_empty());
        assert!(m.projects.is_empty());
        // Falls back to the hostname rather than panicking.
        assert!(!m.node_name().is_empty());
    }

    #[test]
    fn node_names_are_safe_as_toml_keys() {
        let n = default_node_name();
        assert!(!n.is_empty());
        assert!(
            n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "got {n}"
        );
    }
}
