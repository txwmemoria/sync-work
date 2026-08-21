//! Pairing tickets.
//!
//! A ticket describes a *machine*: its public key, its name, and its platform.
//! Note what is absent — no repository path and no project name. That is why one
//! pairing covers every project the two machines share, and it is the single most
//! important property of this type.

use crate::config::{Machine, Peer, Platform};
use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Everything the other machine needs to add us to its peer table.
///
/// Note what is absent: no repository path and no project name. A ticket
/// describes a *machine*, so one pairing covers every project the two share.
///
/// The addresses are included as a hint so a first connection on the same LAN
/// does not have to wait on discovery, but they are not required: the id alone
/// is enough once discovery resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub id: String,
    pub name: String,
    pub platform: Platform,
    #[serde(default)]
    pub addr: Option<EndpointAddr>,
    /// When this ticket is a *reply*, the id of the machine it answers.
    ///
    /// Pairing is three commands: init, join, join back. From inside the second
    /// and third of those the local state looks identical — an empty peer table
    /// either way — so nothing local can tell whether another round trip is
    /// needed. Carrying the acknowledgement makes the final `join` able to say
    /// "done" instead of prompting for a fourth step that does nothing.
    #[serde(default)]
    pub acking: Option<String>,
}

const TICKET_PREFIX: &str = "sw1";

impl Ticket {
    /// Encode as a single copy-pasteable token. Base32 without padding survives
    /// terminals, chat clients, and line wrapping better than base64 or raw JSON.
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        let body = data_encoding::BASE32_NOPAD.encode(&json);
        Ok(format!("{TICKET_PREFIX}:{body}"))
    }

    pub fn decode(s: &str) -> Result<Self> {
        let s = s.trim();
        let body = s
            .strip_prefix(&format!("{TICKET_PREFIX}:"))
            .context("not a syncwork ticket (expected a value starting with `sw1:`)")?;
        // Tolerate a ticket that got lowercased in transit by a chat client.
        let raw = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .context("ticket is corrupt")?;
        let t: Ticket =
            serde_json::from_slice(&raw).context("ticket is not valid syncwork data")?;
        EndpointId::from_str(&t.id).context("ticket contains an invalid endpoint id")?;
        Ok(t)
    }

    pub fn as_peer(&self) -> Peer {
        Peer {
            id: self.id.clone(),
            platform: self.platform,
        }
    }
}

/// Build a ticket describing this machine.
pub async fn local_ticket(
    ep: &Endpoint,
    machine: &Machine,
    name: Option<String>,
    acking: Option<String>,
) -> Result<Ticket> {
    // Wait for a relay handshake so the ticket carries a reachable address.
    // Without this the first connection has to fall back to discovery, which
    // makes pairing look flaky when it is merely slow.
    ep.online().await;
    Ok(Ticket {
        id: ep.id().to_string(),
        name: name.unwrap_or_else(|| machine.node_name()),
        platform: Platform::current(),
        addr: Some(ep.addr()),
        acking,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tickets_round_trip() {
        let t = sample();
        let encoded = t.encode().unwrap();
        assert!(encoded.starts_with("sw1:"));
        let back = Ticket::decode(&encoded).unwrap();
        assert_eq!(back.id, t.id);
        assert_eq!(back.name, t.name);
        assert_eq!(back.platform, Platform::Macos);
    }

    #[test]
    fn a_reply_ticket_says_who_it_answers() {
        // An opening ticket acknowledges nobody, so the recipient knows a reply
        // is still owed.
        let opening = sample();
        assert!(opening.acking.is_none());

        // A reply names the machine it answers, which is how the third `join`
        // knows the pairing is already mutual.
        let reply = Ticket {
            id: "1".repeat(64),
            acking: Some(opening.id.clone()),
            ..sample()
        };
        let back = Ticket::decode(&reply.encode().unwrap()).unwrap();
        assert_eq!(back.acking.as_deref(), Some(opening.id.as_str()));
    }

    #[test]
    fn tickets_carry_no_paths_or_project_names() {
        // A ticket describes a machine. If it ever carried a repo path again,
        // pairing would silently become per-project once more.
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains("repo"), "ticket leaked a path: {json}");
        assert!(!json.contains("project"), "ticket leaked a project: {json}");
    }

    #[test]
    fn tickets_survive_a_chat_client_lowercasing_them() {
        let encoded = sample().encode().unwrap();
        let mangled = format!("sw1:{}", encoded[4..].to_ascii_lowercase());
        assert_eq!(Ticket::decode(&mangled).unwrap().id, sample().id);
    }

    #[test]
    fn malformed_tickets_are_rejected() {
        assert!(Ticket::decode("hello").is_err(), "missing prefix");
        assert!(Ticket::decode("sw1:!!!!").is_err(), "not base32");
        // Well-formed base32 that is not a ticket.
        let junk = data_encoding::BASE32_NOPAD.encode(b"{}");
        assert!(Ticket::decode(&format!("sw1:{junk}")).is_err());
    }
}
