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

/// Bind the local endpoint using the persistent identity.
///
/// Deliberately private: [`with_endpoint`] is the only way to obtain an endpoint,
/// which makes it impossible for a caller to skip the close.
async fn bind(identity: &Identity) -> Result<Endpoint> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(identity.secret.clone())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("failed to bind iroh endpoint")?;
    Ok(ep)
}

/// Bind, run `f`, and close — whatever `f` returns.
///
/// The manual `bind` / `close` pairing is easy to get wrong, because the failure
/// is silent in the happy case and only shows up as library noise stamped on top
/// of a real error message. Wrapping it means the close cannot be skipped by an
/// early return, so there is one less way to produce output like:
///
/// ```text
/// ERROR iroh::socket: Endpoint dropped without calling `Endpoint::close`.
/// error: no peers paired yet; run `sw pair init` on one machine
/// ```
pub async fn with_endpoint<T, F, Fut>(identity: &Identity, f: F) -> Result<T>
where
    F: FnOnce(Endpoint) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let ep = bind(identity).await?;
    // Endpoint is a cheap handle; the clone lets `f` own one while this scope
    // keeps the one it closes.
    let out = f(ep.clone()).await;
    ep.close().await;
    out
}

/// Connect to a peer. Falls back from the address hint to discovery-by-id,
/// because a hint recorded at pairing time goes stale when a laptop changes
/// network, while the id never does.
pub async fn connect(ep: &Endpoint, peer: &Peer) -> Result<iroh::endpoint::Connection> {
    let id = EndpointId::from_str(&peer.id)
        .with_context(|| format!("peer id `{}` is not a valid endpoint id", peer.id))?;
    ep.connect(id, ALPN).await.with_context(|| {
        format!(
            "could not reach peer {:.12}; is `sw node` running there?",
            peer.id
        )
    })
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
            addr: None,
            acking: None,
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
