//! Binding, dialling, and authorisation.
//!
//! There is no server and no central registry. Each machine holds one long-lived
//! keypair and a peer is simply a public key; both machines run the identical
//! binary and either may initiate.
//!
//! Authorisation is deliberately crude and strong: a request is served only if
//! the connection's remote id appears in this machine's peer table. An iroh
//! connection is authenticated by construction — the remote id *is* the public
//! key that terminated the QUIC handshake — so matching against that table is
//! sufficient. There is no token to leak or rotate.

use super::ALPN;
use crate::config::{Identity, Machine, Peer};
use anyhow::{Context, Result, bail};
use iroh::TransportAddr;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointId};
use std::collections::BTreeMap;
use std::str::FromStr;

/// Bind an endpoint on the given key.
///
/// Deliberately private: [`with_endpoint`] and [`with_machine_endpoint`] are the
/// only ways to obtain one, which makes it impossible for a caller to skip the
/// close.
async fn bind(secret: iroh::SecretKey) -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("failed to bind iroh endpoint")?;
    Ok(ep)
}

async fn with_bound<T, F, Fut>(secret: iroh::SecretKey, f: F) -> Result<T>
where
    F: FnOnce(Endpoint) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let ep = bind(secret).await?;
    // Endpoint is a cheap handle; the clone lets `f` own one while this scope
    // keeps the one it closes.
    let out = f(ep.clone()).await;
    ep.close().await;
    out
}

/// Bind a *client* endpoint: a throwaway key, speaking for the machine.
///
/// Never the machine key, and this is load-bearing: the relay keeps one active
/// connection per key and deactivates the previous one, so a client dialling
/// with the machine key knocks this machine's own node off the relay for as
/// long as the client runs. `sw ctx --watch` made that permanent — the peer's
/// `sw doctor` timed out against a machine whose node was up the whole time.
/// Every requests carries [`super::Delegation`] instead, signed here, so the
/// receiver authorizes the machine without the machine key ever dialling.
///
/// The bind/close pairing is wrapped for the same reason as ever: a skipped
/// close is silent in the happy case and shows up as library noise stamped on
/// top of a real error message.
pub async fn with_endpoint<T, F, Fut>(identity: &Identity, f: F) -> Result<T>
where
    F: FnOnce(Endpoint) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let secret = iroh::SecretKey::generate();
    let delegation = sign_delegation(identity, secret.public());
    if let Ok(mut slot) = DELEGATION.lock() {
        *slot = Some(delegation);
    }
    with_bound(secret, f).await
}

/// Bind the endpoint that *is* this machine: the node, and pairing.
///
/// The node must hold the machine key — that key is the address peers dial.
/// Pairing must too, because coming online is what publishes the key the ticket
/// carries. Everything else goes through [`with_endpoint`].
pub async fn with_machine_endpoint<T, F, Fut>(identity: &Identity, f: F) -> Result<T>
where
    F: FnOnce(Endpoint) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    with_bound(identity.secret.clone(), f).await
}

/// This process's delegation, set when its client endpoint is created.
///
/// Process-global for the same reason as the relay below: it is a property of
/// the single endpoint this process binds, and threading it through every
/// request-writing call site would be plumbing for a value that cannot differ.
static DELEGATION: std::sync::Mutex<Option<super::Delegation>> = std::sync::Mutex::new(None);

/// The delegation to put on an outgoing request, if this process dials as a client.
pub fn delegation() -> Option<super::Delegation> {
    DELEGATION.lock().ok().and_then(|slot| slot.clone())
}

/// Sign a throwaway connection key with the machine key.
fn sign_delegation(identity: &Identity, conn_key: EndpointId) -> super::Delegation {
    let sig = identity.secret.sign(&super::delegation_message(&conn_key));
    super::Delegation {
        id: identity.id_string(),
        sig: hex::encode(sig.to_bytes()),
    }
}

/// Check a delegation against the connection that presented it.
///
/// Three things must hold, and each failure names itself: the signature parses,
/// it covers *this connection's* key (a signature over some other key proves
/// nothing about this sender), and the signing machine is one we trust.
pub fn verify_delegation(
    machine: &Machine,
    conn_key: EndpointId,
    d: &super::Delegation,
) -> Result<(String, Peer)> {
    let signer = EndpointId::from_str(&d.id)
        .map_err(|_| anyhow::anyhow!("delegation names an invalid machine key"))?;
    let raw = hex::decode(&d.sig).context("delegation signature is not hex")?;
    let sig = iroh::Signature::try_from(raw.as_slice())
        .map_err(|_| anyhow::anyhow!("delegation signature is malformed"))?;
    signer
        .verify(&super::delegation_message(&conn_key), &sig)
        .map_err(|_| {
            anyhow::anyhow!("delegation signature does not cover this connection's key")
        })?;
    authorize(machine, signer)
}

/// Where to dial a peer: its key, plus the last address it gave us.
///
/// Both, not either. The key is what discovery resolves and never changes; the
/// cached relay is what makes the dial work when discovery declines to answer.
/// iroh inserts a supplied address as one more candidate path and *still* starts
/// a lookup if none of them gets selected, so this is additive — a relay that has
/// gone stale costs a probe, not the command.
fn dial_addr(peer: &Peer) -> Result<iroh::EndpointAddr> {
    let id = EndpointId::from_str(&peer.id)
        .with_context(|| format!("peer id `{}` is not a valid endpoint id", peer.id))?;
    let mut addr = iroh::EndpointAddr::new(id);
    if let Some(relay) = &peer.relay {
        match iroh::RelayUrl::from_str(relay) {
            Ok(url) => addr = addr.with_relay_url(url),
            // Not fatal: a malformed cached address must not stop us falling back
            // to the thing that worked before there was a cache.
            Err(e) => tracing::warn!("ignoring unusable cached address `{relay}`: {e}"),
        }
    }
    Ok(addr)
}

/// Connect to a peer, by cached address and by discovery at once.
pub async fn connect(ep: &Endpoint, peer: &Peer) -> Result<iroh::endpoint::Connection> {
    let addr = dial_addr(peer)?;
    let cached = !addr.addrs.is_empty();
    let conn = match ep.connect(addr, ALPN).await {
        Ok(conn) => conn,
        Err(e) if !cached && format!("{e}").contains("No addressing information") => {
            // Distinct from "the node is down": nothing was even tried, because
            // nobody would say where to try. Worth its own message — the fix is
            // not on this machine.
            bail!(
                "no address for peer {:.12}: nothing cached here, and public discovery \
                 had no answer for its key. The peer's address is recorded the first \
                 time it sends a request to this machine, so run `sw doctor` there once \
                 and try again.",
                peer.id
            )
        }
        // Deliberately not `.context()` on the original error. iroh's dial errors
        // nest a source chain of identical messages, so anyhow's `{:#}` rendered
        // them as `timed out: timed out: timed out` — three copies of one fact.
        Err(e) => bail!(
            "could not reach peer {:.12} ({e}); is `sw node` running there?",
            peer.id
        ),
    };
    // A machine that has never run a node has nothing recorded, so this is the
    // one moment a client can learn its own address: the dial is done, which for
    // a relayed path means our relay client is up.
    if self_relay().is_none() {
        note_self_relay(ep, 50).await;
    }
    Ok(conn)
}

/// This machine's own address, as a peer would dial it.
///
/// Read from the machine config rather than sampled per process, because the two
/// are not equally reliable. What a peer dials is this machine's *node*, and the
/// node establishes its address once at startup after waiting to come online. A
/// short-lived client — `sw ctx` from a Stop hook, say — routinely finishes its
/// whole exchange before its own endpoint has found a relay: measured at about
/// three seconds from bind, which is longer than the command takes.
///
/// So the node writes it down and everything else reads it. Whoever discovers it
/// first records it, which is what keeps a node-less machine working too.
pub fn self_relay() -> Option<String> {
    Machine::load().ok()?.node.relay
}

/// Record this machine's own address, waiting up to `patience` tenths of a second.
///
/// Returns as soon as it is known, so the node — which has already awaited
/// `online()` — pays nothing. The wait is for the cold client above, and is
/// bounded because it need not succeed: a dial that went direct across a LAN may
/// have no relay at all, and then the peer simply learns our address later.
pub async fn note_self_relay(ep: &Endpoint, patience: u32) {
    for remaining in (0..=patience).rev() {
        if let Some(url) = ep.addr().relay_urls().next().map(|u| u.to_string()) {
            match Machine::record_self_relay(&url) {
                // Debug, not info: this fires inside ordinary commands like
                // `sw status`, and a line about our own plumbing has no business
                // in their output. `sw doctor` reports the address on request.
                Ok(true) => tracing::debug!("this machine is reachable at {url}"),
                Ok(false) => {}
                Err(e) => tracing::warn!("could not record this machine's address: {e:#}"),
            }
            return;
        }
        if remaining > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

/// Resolved relay addresses, cached for the life of the process.
///
/// A relay path reports a URL, not an address — and an address is exactly what
/// you need when a firewall is in the way. The lookup is cached because this
/// runs on every served request while the answer does not change within a run.
static RELAY_IPS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, String>>> =
    std::sync::OnceLock::new();

/// Resolve a relay host to a comma-separated list of addresses.
///
/// Best-effort and explicitly labelled as a lookup rather than the socket in use:
/// iroh does not expose the relay connection's own address, and a relay hostname
/// may answer with several records. Useful for "which address do I allow through
/// the firewall", not as proof of which one carried the bytes.
async fn relay_addrs(host: &str) -> String {
    let cache = RELAY_IPS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(host)
    {
        return hit.clone();
    }

    let resolved = match tokio::net::lookup_host((host, 443)).await {
        Ok(addrs) => {
            let mut ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            ips.sort();
            ips.dedup();
            if ips.is_empty() {
                "unresolved".to_string()
            } else {
                ips.join(",")
            }
        }
        Err(_) => "unresolved".to_string(),
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(host.to_string(), resolved.clone());
    }
    resolved
}

/// Describe the live network paths of a connection: relay or direct, the remote
/// address, and the round-trip time.
///
/// A connection starts out relayed and gains a direct path once holepunching
/// succeeds, so this also answers "is my traffic actually going peer-to-peer?"
/// — which is otherwise invisible. The selected path is marked `*`.
pub async fn describe_paths(conn: &iroh::endpoint::Connection) -> String {
    // Collected first so the borrow of `paths()` is not held across the DNS
    // await below.
    struct Seen {
        selected: bool,
        relay_host: Option<String>,
        shown: String,
        rtt_ms: u128,
    }
    let seen: Vec<Seen> = conn
        .paths()
        .iter()
        .map(|p| Seen {
            selected: p.is_selected(),
            relay_host: match p.remote_addr() {
                TransportAddr::Relay(url) => url.host_str().map(|h| h.to_string()),
                _ => None,
            },
            shown: p.remote_addr().to_string(),
            rtt_ms: p.rtt().as_millis(),
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    for s in seen {
        let mark = if s.selected { "*" } else { "" };
        match &s.relay_host {
            Some(host) => parts.push(format!(
                "{mark}relay {host} [{}] {}ms",
                relay_addrs(host).await,
                s.rtt_ms
            )),
            None => parts.push(format!("{mark}{} {}ms", s.shown, s.rtt_ms)),
        }
    }
    if parts.is_empty() {
        "no path".into()
    } else {
        parts.join(", ")
    }
}

/// Explain a refusal, if that is what closed the connection.
///
/// The far node closes with the reason `unpaired` when it does not recognise our
/// key. That surfaces two different ways depending on timing — as an error from
/// `open_bi`, or as a clean end-of-stream once the request is already in flight —
/// so both paths have to consult the same place. Getting only one of them was how
/// `sw status` came to report `unexpected reply from peer: None`, which says
/// nothing about the actual problem.
pub fn refusal(conn: &iroh::endpoint::Connection) -> Option<String> {
    let reason = conn.close_reason()?.to_string();
    if !reason.contains("unpaired") {
        return None;
    }
    Some(
        "the peer refused us as unpaired. Its record of this machine's key is stale — \
         most often because this machine's identity was regenerated, or because the \
         pairing was done from a shell that resolves a different config directory. \
         Re-pair: `sw pair init` here, `sw pair join <ticket>` there. `sw pair list` \
         on both sides shows the keys each expects."
            .to_string(),
    )
}

/// Prefer the refusal explanation over whatever transport error surfaced.
///
/// A refusal reaches the client as at least three different shapes depending on
/// when the peer closed: an error from `open_bi`, a clean end-of-stream, or
/// `connection lost` from a read already in flight. Each of the first two was
/// patched at its own call site and the third still arrived raw. No individual
/// site can be trusted to remember this, so every client exchange funnels its
/// error through here instead.
pub fn explain(conn: &iroh::endpoint::Connection, e: anyhow::Error) -> anyhow::Error {
    match refusal(conn) {
        Some(text) => anyhow::anyhow!("{text}"),
        None => e,
    }
}

/// Open a request stream, translating a refusal into something actionable.
pub async fn open_request(
    conn: &iroh::endpoint::Connection,
) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
    match conn.open_bi().await {
        Ok(pair) => Ok(pair),
        Err(e) => {
            if let Some(explained) = refusal(conn) {
                bail!("{explained}")
            }
            Err(anyhow::Error::new(e).context("could not open a request stream"))
        }
    }
}

/// Reject a connection whose remote key is not in this machine's peer table.
pub fn authorize(machine: &Machine, remote: EndpointId) -> Result<(String, Peer)> {
    let remote = remote.to_string();
    match machine.peers.iter().find(|(_, p)| p.id == remote) {
        Some((name, p)) => Ok((name.clone(), p.clone())),
        None => bail!("unpaired peer {remote:.12} attempted a request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Platform;
    use crate::transport::Ticket;

    fn sample() -> Ticket {
        Ticket {
            // A syntactically valid ed25519 public key in iroh's hex form.
            id: "0".repeat(64),
            name: "mac-mini".into(),
            platform: Platform::Macos,
            acking: None,
        }
    }

    #[test]
    fn a_dial_carries_the_cached_address_alongside_the_key() {
        // Nothing cached: the key alone, which leaves iroh dependent on a public
        // directory answering for it.
        let bare = dial_addr(&sample().as_peer()).unwrap();
        assert!(bare.addrs.is_empty());

        let mut peer = sample().as_peer();
        peer.relay = Some("https://relay.example./".into());
        let addr = dial_addr(&peer).unwrap();
        assert_eq!(addr.relay_urls().count(), 1, "relay is offered to the dial");

        // A cached address that cannot be parsed must degrade to the bare key
        // rather than failing the dial — otherwise one bad config line would take
        // out a pair that discovery could still have resolved.
        peer.relay = Some("not a url".into());
        assert!(dial_addr(&peer).unwrap().addrs.is_empty());
    }

    /// The delegation is an authorization boundary, so the refusals are the
    /// point: every way a signature can fail to bind THIS machine key to THIS
    /// connection key must be refused, or an attacker who saw one valid
    /// delegation could reuse it from their own connection.
    #[test]
    fn a_delegation_admits_the_machine_and_nothing_else() {
        let machine_key = iroh::SecretKey::generate();
        let identity = Identity {
            secret: machine_key.clone(),
        };
        let mut table = Machine::default();
        table.peers.insert(
            "mac".into(),
            crate::config::Peer {
                id: machine_key.public().to_string(),
                platform: Platform::Macos,
                relay: None,
            },
        );

        let conn_key = iroh::SecretKey::generate().public();
        let d = sign_delegation(&identity, conn_key);

        // The happy path authorizes the MACHINE — its name and its key, not the
        // throwaway the connection used.
        let (name, peer) = verify_delegation(&table, conn_key, &d).unwrap();
        assert_eq!(name, "mac");
        assert_eq!(peer.id, machine_key.public().to_string());

        // A delegation lifted from one connection proves nothing on another:
        // the signature covers the connection key.
        let other_conn = iroh::SecretKey::generate().public();
        assert!(verify_delegation(&table, other_conn, &d).is_err());

        // A valid signature from a machine we never paired with is still refused.
        let stranger = iroh::SecretKey::generate();
        let sd = sign_delegation(
            &Identity {
                secret: stranger.clone(),
            },
            conn_key,
        );
        assert!(verify_delegation(&table, conn_key, &sd).is_err());

        // Claiming a paired machine while signing with a different key fails on
        // the signature, not on the table lookup.
        let forged = super::super::Delegation {
            id: machine_key.public().to_string(),
            ..sd
        };
        assert!(verify_delegation(&table, conn_key, &forged).is_err());

        // Garbage in either field is an error, not a panic.
        for (id, sig) in [
            ("not hex".to_string(), d.sig.clone()),
            (d.id.clone(), "zz".to_string()),
            (d.id.clone(), hex::encode([0u8; 10])),
        ] {
            let bad = super::super::Delegation { id, sig };
            assert!(verify_delegation(&table, conn_key, &bad).is_err());
        }
    }

    #[test]
    fn authorize_admits_only_the_paired_key() {
        let mut m = Machine::default();
        m.peers.insert("mac".into(), sample().as_peer());

        let paired = EndpointId::from_str(&"0".repeat(64)).unwrap();
        assert_eq!(authorize(&m, paired).unwrap().0, "mac");

        // Any other key, however well-formed, is refused.
        let stranger = iroh::SecretKey::generate().public();
        assert!(authorize(&m, stranger).is_err());
    }
}
