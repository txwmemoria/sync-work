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
//! | `~/.syncwork/config.toml`       | node name, peers, project registry     |
//! | `~/.syncwork/identity.key`      | the secret key                         |
//! | `<repo>/syncwork.toml` (in git) | project name, recipes, context options |
//!
//! One path on every platform, deliberately in the home directory rather than the
//! platform config directory — see [`home`] for why. `SW_HOME` overrides it.
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

/// Directory holding this machine's syncwork state: `~/.syncwork`.
///
/// The home directory, not the platform's config directory, because a sandboxed
/// application can have its view of the latter redirected. Measured on Windows:
/// the Claude desktop app is packaged, so its `%APPDATA%` reads and writes land
/// in `AppData\Local\Packages\...\LocalCache\Roaming` — and an explicit absolute
/// path does not escape it, since the redirection happens at path resolution.
/// The result was two identities on one machine, one of which the peer had never
/// been paired with, so anything `sw` ran from inside that app was refused.
///
/// `~/.claude` demonstrably is *not* redirected — the same session transcripts are
/// visible from both — so state sits beside it. One path on every platform also
/// means one thing to document instead of three.
///
/// `SW_HOME` still overrides, which is what lets two independent nodes run on one
/// box (the end-to-end tests rely on it).
pub fn home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SW_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let base = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("cannot determine the home directory")?;
    let dir = base.join(".syncwork");

    if !dir.exists()
        && let Some(legacy) = legacy_dir()
        && legacy.is_dir()
    {
        return Ok(migrate(&legacy, &dir));
    }
    Ok(dir)
}

/// Where state lived before: the platform config directory.
fn legacy_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "syncwork").map(|d| d.config_dir().to_path_buf())
}

/// Move state to its new home, returning the directory to actually use.
///
/// On any failure the *old* path is returned rather than an error: a working
/// pairing must not break because a move could not be completed. A copy is
/// attempted if the rename fails, which it does when the two sit on different
/// volumes.
fn migrate(legacy: &Path, dir: &Path) -> PathBuf {
    if std::fs::rename(legacy, dir).is_ok() {
        eprintln!("moved syncwork state to {}", dir.display());
        return dir.to_path_buf();
    }
    let copied = std::fs::create_dir_all(dir).is_ok()
        && std::fs::read_dir(legacy)
            .map(|entries| {
                entries.filter_map(|e| e.ok()).all(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                        && std::fs::copy(e.path(), dir.join(e.file_name())).is_ok()
                })
            })
            .unwrap_or(false);
    if copied {
        eprintln!(
            "copied syncwork state to {} (the old copy at {} is now unused)",
            dir.display(),
            legacy.display()
        );
        return dir.to_path_buf();
    }
    tracing::warn!(
        "could not move syncwork state from {} to {}; continuing to use the old location",
        legacy.display(),
        dir.display()
    );
    legacy.to_path_buf()
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
    /// Where this peer last told us it can be reached: its own home relay.
    ///
    /// A key alone is only dialable if some public directory will translate it
    /// into an address, and that is not a safe assumption. Measured: n0's
    /// authoritative DNS served this machine's record while returning
    /// `Non-existent domain` for the peer's — even though the peer's record was
    /// present and current in the very same service's pkarr store. Half the pair
    /// was undialable for a service outage neither machine could see or fix.
    ///
    /// So a peer states its address whenever it sends a request, and it is kept
    /// here. Discovery still runs alongside this — iroh treats a supplied address
    /// as one more candidate path, not as a replacement — so a stale entry costs
    /// a failed probe rather than a failed command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
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
    /// This machine's own relay, put on every outgoing request so peers can dial
    /// back without depending on a public directory. Written by whichever process
    /// discovers it — usually the node, which is the only one that reliably knows
    /// it in time. See [`Peer::relay`] for why any of this is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
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

    /// Remember where a peer said it can be reached. True when it changed.
    ///
    /// Keyed by public key rather than by name: the name is local bookkeeping and
    /// either side may have renamed its own machine, while the key is the thing
    /// both agree on.
    pub fn remember_relay(&mut self, peer_id: &str, relay: &str) -> bool {
        match self.peers.values_mut().find(|p| p.id == peer_id) {
            Some(peer) if peer.relay.as_deref() == Some(relay) => false,
            Some(peer) => {
                peer.relay = Some(relay.to_string());
                true
            }
            // An unpaired key never reaches this point — the connection is refused
            // first — but recording an address for a machine we do not trust would
            // be wrong regardless of how it got here.
            None => false,
        }
    }

    /// Persist a peer's address, reading and writing the file around the change.
    ///
    /// Load-modify-save rather than mutating a held snapshot, because the caller
    /// is the node serving a request and its snapshot is per-request anyway. The
    /// write only happens when the address actually changed, which makes the
    /// theoretical race with a concurrent `sw link` both rare and self-correcting:
    /// the next request writes the same value again.
    pub fn record_relay(peer_id: &str, relay: &str) -> Result<bool> {
        let mut machine = Self::load()?;
        if !machine.remember_relay(peer_id, relay) {
            return Ok(false);
        }
        machine.save()?;
        Ok(true)
    }

    /// Persist this machine's own address, so short-lived commands can state it
    /// without each having to discover it first. True when it changed.
    pub fn record_self_relay(relay: &str) -> Result<bool> {
        let mut machine = Self::load()?;
        if machine.node.relay.as_deref() == Some(relay) {
            return Ok(false);
        }
        machine.node.relay = Some(relay.to_string());
        machine.save()?;
        Ok(true)
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
            relay: None,
        }
    }

    #[test]
    fn a_peers_address_is_remembered_by_key_and_rewritten_only_when_it_changes() {
        let mut m = Machine::default();
        m.peers.insert("mac".into(), peer("aaa"));

        assert!(m.remember_relay("aaa", "https://r1/"), "first address");
        assert_eq!(m.peers["mac"].relay.as_deref(), Some("https://r1/"));

        // Saving on every request would rewrite the file hundreds of times a day
        // for a value that almost never moves.
        assert!(!m.remember_relay("aaa", "https://r1/"), "unchanged");
        assert!(m.remember_relay("aaa", "https://r2/"), "moved relay");
        assert_eq!(m.peers["mac"].relay.as_deref(), Some("https://r2/"));

        // A key we have never paired with gets no entry, however it arrived.
        assert!(!m.remember_relay("zzz", "https://r1/"));
        assert_eq!(m.peers.len(), 1);
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
