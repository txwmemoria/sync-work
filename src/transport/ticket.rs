//! Pairing tickets.
//!
//! A ticket describes a *machine*: its public key, its name, and its platform.
//! Note what is absent — no repository path and no project name. That is why one
//! pairing covers every project the two machines share, and it is the single most
//! important property of this type.

use crate::config::{Machine, Peer, Platform};
use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Everything the other machine needs to add us to its peer table.
///
/// Note what is absent: no repository path and no project name. A ticket
/// describes a *machine*, so one pairing covers every project the two share.
///
/// No addresses either. An earlier version carried them "as a hint", but nothing
/// ever read them: `as_peer` keeps only the key and platform, and `connect` dials
/// by key alone. Removing them took the encoded ticket from 518 characters to 206.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: String,
    pub name: String,
    pub platform: Platform,
    /// When this ticket is a *reply*, the id of the machine it answers.
    ///
    /// Pairing is three commands: init, join, join back. From inside the second
    /// and third of those the local state looks identical — an empty peer table
    /// either way — so nothing local can tell whether another round trip is
    /// needed. Carrying the acknowledgement makes the final `join` able to say
    /// "done" instead of prompting for a fourth step that does nothing.
    pub acking: Option<String>,
}

const TICKET_PREFIX: &str = "sw1";

/// The on-the-wire form.
///
/// Separate from [`Ticket`] so the type people handle keeps readable strings while
/// the encoded token stays short. JSON spent 64 characters of hex on a 32-byte key
/// and repeated the field names in every ticket; postcard writes the key as bytes
/// and the names not at all, which is the difference between 206 characters and
/// about 70. Readability buys nothing here — the token is opaque base32 either
/// way, unlike the wire protocol, where a packet dump is worth being able to read.
#[derive(Serialize, Deserialize)]
struct Wire {
    key: [u8; 32],
    name: String,
    platform: Platform,
    acking: Option<[u8; 32]>,
}

fn key_bytes(hex_id: &str) -> Result<[u8; 32]> {
    let raw = hex::decode(hex_id).context("endpoint id is not hex")?;
    raw.try_into()
        .map_err(|_| anyhow::anyhow!("endpoint id is not 32 bytes"))
}

impl Ticket {
    /// Encode as a single copy-pasteable token. Base32 without padding survives
    /// terminals, chat clients, and line wrapping better than base64 would.
    pub fn encode(&self) -> Result<String> {
        let wire = Wire {
            key: key_bytes(&self.id)?,
            name: self.name.clone(),
            platform: self.platform,
            acking: self.acking.as_deref().map(key_bytes).transpose()?,
        };
        let bytes = postcard::to_stdvec(&wire)?;
        Ok(format!(
            "{TICKET_PREFIX}:{}",
            data_encoding::BASE32_NOPAD.encode(&bytes)
        ))
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
        let wire: Wire = postcard::from_bytes(&raw).context("ticket is not valid syncwork data")?;
        let t = Ticket {
            id: hex::encode(wire.key),
            name: wire.name,
            platform: wire.platform,
            acking: wire.acking.map(hex::encode),
        };
        EndpointId::from_str(&t.id).context("ticket contains an invalid endpoint id")?;
        Ok(t)
    }

    pub fn as_peer(&self) -> Peer {
        Peer {
            id: self.id.clone(),
            platform: self.platform,
            // Not carried in the ticket: a ticket is copied by hand and may sit in
            // a chat window for a day, while a relay assignment follows whichever
            // network the machine is on. The address is learned instead from the
            // peer's first request, where it is current by construction.
            relay: None,
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
    // Waited on even though the ticket carries no address: coming online is what
    // publishes this key to discovery, and the peer has to be able to find it
    // there before the ticket is of any use.
    ep.online().await;
    Ok(Ticket {
        id: ep.id().to_string(),
        name: name.unwrap_or_else(|| machine.node_name()),
        platform: Platform::current(),
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
    fn tickets_stay_short_enough_to_paste() {
        // A ticket describes a machine, and the encoding carries no field names,
        // so "does it leak a path" cannot be tested by searching for one. Length
        // is the proxy that still catches it: a repository path or a project name
        // would be tens of characters, well past this bound. It also guards the
        // reason the encoding changed — this was 518 characters when it carried
        // addresses nothing read, and 206 as JSON.
        let n = sample().encode().unwrap().len();
        assert!(n < 120, "ticket grew to {n} characters");

        // A reply carries a second key, and must still be pasteable.
        let reply = Ticket {
            acking: Some("1".repeat(64)),
            ..sample()
        };
        let n = reply.encode().unwrap().len();
        assert!(n < 180, "reply ticket grew to {n} characters");
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
