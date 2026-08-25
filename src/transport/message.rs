//! Messages exchanged over the wire.
//!
//! One bi-directional stream per request. The control plane is length-prefixed
//! JSON so it stays readable in a packet dump; bulk bodies (a patch, a context
//! file) travel as their own raw frames so nothing pays base64 overhead.
//!
//! Exchange for a verify:
//! ```text
//! client -> [JSON Request::Verify] [raw zstd patch]   (then finish)
//! server -> [JSON Event] [JSON Event] ...             (then finish)
//! ```
//! The patch frame is always sent for `Verify`, even when empty, so the reader
//! never has to guess whether another frame is coming.

use crate::config::Platform;
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"syncwork/1";

/// Bumped on any breaking change to the frames below. Mismatched peers must fail
/// loudly rather than half-work, since the two machines are upgraded separately.
///
/// v4: clients authenticate by delegation instead of by the machine key.
pub const PROTOCOL_VERSION: u32 = 4;

/// Explain a protocol disagreement in terms the operator can act on.
///
/// Mismatched peers must fail loudly rather than half-work: the two machines are
/// upgraded independently, so this will happen.
pub fn version_mismatch(theirs: u32) -> String {
    format!(
        "protocol mismatch: peer speaks v{theirs}, this node speaks v{PROTOCOL_VERSION} \
         — update `sw` on one side"
    )
}

/// Proof that a connection speaks for a paired machine.
///
/// Exists because a client must not dial with the machine key. The relay keeps
/// exactly one active connection per key — a second registration deactivates the
/// first (measured: `iroh-relay` logs "multiple connections found, deactivating
/// old connection") — so a client holding the machine key knocks this machine's
/// *node* off the relay for as long as it runs. A `sw ctx --watch` left running
/// made the machine undialable for hours; every short-lived command was already
/// doing the same for seconds at a time.
///
/// So a client binds a fresh throwaway key and carries this instead: the machine
/// key's signature over that throwaway key. The receiver verifies the signature
/// and authorizes the *machine*, exactly as if the machine key itself had dialled.
/// The QUIC handshake has already proven the sender holds the throwaway secret,
/// so the signature cannot be replayed by anyone else — stealing it without the
/// throwaway's private half buys nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    /// The machine's public key, hex — same form the peer table stores.
    pub id: String,
    /// Signature over [`delegation_message`] of the connection's key, hex.
    pub sig: String,
}

/// What the machine key signs: a domain tag plus the throwaway public key.
///
/// The tag keeps this signature meaningless anywhere else — without it, a bare
/// 32-byte message could double as some other protocol's signing input.
pub fn delegation_message(conn_key: &iroh::EndpointId) -> Vec<u8> {
    let mut msg = b"syncwork-delegation-v1:".to_vec();
    msg.extend_from_slice(conn_key.as_bytes());
    msg
}

/// A request, plus where the machine that sent it can be reached.
///
/// The sender has to state its own address, because the receiver cannot work it
/// out. An inbound relayed path names the relay the packets *arrived through* —
/// which is the receiver's own home relay, not the sender's. Dialling that back
/// only works when both machines happen to share one relay, so inferring it would
/// be a bug that hides for as long as the two machines sit in the same region.
///
/// Carried on every request rather than negotiated once, so it needs no state and
/// no extra round trip, and a machine that moves networks corrects itself with
/// its next request.
///
/// `#[serde(flatten)]` keeps this on the wire as the same flat object a bare
/// request produced, with one extra key. That is what makes it compatible in both
/// directions — an older peer ignores the unknown `relay` key, and a request from
/// an older peer leaves it `None` — so this needed no protocol bump. Which
/// matters more than usual here: the version number itself travels *inside* the
/// request, so a peer that cannot parse the outer shape cannot report a version
/// mismatch either. Wrapping the request in a new object would have turned every
/// mismatched pair into an unexplained parse failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// The sender's home relay, as the sender sees it. `None` when it does not
    /// know yet, in which case the receiver simply learns nothing this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Who this connection speaks for. `None` only from a peer old enough to
    /// have dialled with its machine key directly, which the receiver can still
    /// authorize by the connection key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Delegation>,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    /// Liveness plus a capability handshake — used by `sw status`.
    Ping {
        protocol: u32,
    },
    Verify(VerifyRequest),
    /// Describe the peer's context roots so the client can compute a sync plan.
    CtxManifest {
        protocol: u32,
        project: String,
    },
    /// Ask for specific files; the peer replies with `CtxFrame::File` + body pairs.
    CtxFetch {
        project: String,
        root: String,
        paths: Vec<String>,
    },
    /// Send files; `CtxFrame::File` + body pairs follow, terminated by `End`.
    CtxPush {
        project: String,
        root: String,
    },
    /// Ask the peer to fetch and, if it can do so safely, fast-forward a branch.
    Handoff {
        protocol: u32,
        project: String,
        branch: String,
    },
}

/// What the peer did with a handoff.
///
/// Deliberately not a single ok/error. The interesting cases are the ones where
/// the peer *declines* to move its checkout, and each calls for a different action
/// from whoever reads it — so they are distinct variants rather than one string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HandoffResult {
    /// The branch was checked out, clean, and behind; it now matches.
    FastForwarded {
        host: String,
        branch: String,
        from: String,
        to: String,
    },
    /// The commits reached the peer, but its checkout was left alone — a
    /// different branch, a dirty tree, or history that would not fast-forward.
    Fetched {
        host: String,
        branch: String,
        at: String,
        why: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRequest {
    pub protocol: u32,
    /// Guards against pointing two unrelated repos at each other.
    pub project: String,
    pub recipe: String,
    /// Commit the peer must have and check out before applying the patch.
    pub base: String,
    /// Length of the following raw frame. Zero means the tree was clean.
    pub patch_bytes: u64,
    /// Purely informational, for the peer's log and TUI.
    pub from_host: String,
    /// Whether the requester's terminal can render escape codes. The peer runs
    /// the build, so only the requester knows this, and forwarding coloured
    /// output into a pipe would corrupt it.
    #[serde(default)]
    pub color: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    /// First event on every accepted request; lets the client print where the
    /// work actually landed.
    Accepted {
        host: String,
        platform: Platform,
        repo: String,
        /// Commit the peer's binary was built from, so a node left running on
        /// stale code is visible rather than guessed at. Defaulted so a peer
        /// predating this field still deserialises.
        #[serde(default)]
        build: String,
    },
    Stage {
        name: String,
    },
    Log {
        err: bool,
        line: String,
    },
    Diag(Diagnostic),
    /// Terminal on success path. `code` is the recipe's exit status and is what
    /// the client process exits with, so a caller (or an AI) can branch on it.
    Done {
        code: i32,
        ms: u64,
    },
    /// Terminal, and the one failure a client can recover from on its own: the
    /// peer cannot obtain the base commit, so the client re-snapshots against an
    /// older commit that the peer does have.
    MissingBase {
        base: String,
    },
    /// Terminal on our own failure — patch would not apply, recipe unknown, etc.
    /// Distinct from `Done { code: nonzero }`, which means the build itself failed.
    Failed {
        message: String,
    },
}

/// A single compiler diagnostic, lifted out of cargo's JSON output so a remote
/// failure arrives as `file:line: message` rather than buried in a log blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub level: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// cargo's own pretty-printed form, kept so we can echo exactly what a local
    /// build would have shown.
    pub rendered: Option<String>,
}

// ---------------------------------------------------------------- context sync

/// One file in a context root.
///
///
/// `mtime_ms` decides which side wins; `hash` decides whether there is anything
/// to do at all. Both are needed: mtime alone produces pointless transfers after
/// a sync rewrites a file, and hash alone gives no way to order two versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtxEntry {
    /// Forward-slash relative path, so the two platforms agree on identity.
    pub path: String,
    pub size: u64,
    pub mtime_ms: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtxRoot {
    /// Logical key, e.g. `claude`. Each machine resolves it to its own local
    /// directory, so no path translation crosses the wire.
    pub key: String,
    /// False when the directory does not exist on that machine yet.
    pub present: bool,
    pub entries: Vec<CtxEntry>,
}

/// Frames exchanged during a context sync. A `File` is always followed by one
/// raw frame carrying its bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum CtxFrame {
    Roots {
        host: String,
        roots: Vec<CtxRoot>,
    },
    File {
        path: String,
        mtime_ms: u64,
        /// Set when this file is a three-way merge whose remote parent was the
        /// version with this manifest hash. A receiver still holding exactly
        /// that version may overwrite without any preservation dance — the
        /// merge already contains everything it holds. If its file has moved
        /// on since, the hash mismatches and the normal rules apply. This is
        /// what lets one side merge without the other keeping a base store.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<String>,
    },
    End,
    Failed {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address rides along inside the request object, which is what lets it
    /// cross between versions in either direction. If this ever regresses into a
    /// wrapper object, every mismatched pair loses its version-mismatch message
    /// too, since the version travels inside the request.
    #[test]
    fn an_addressed_request_is_wire_compatible_with_a_bare_one() {
        let addressed = Envelope {
            relay: Some("https://relay.example/".into()),
            from: Some(Delegation {
                id: "0".repeat(64),
                sig: "1".repeat(128),
            }),
            request: Request::Ping { protocol: 7 },
        };
        let raw = serde_json::to_vec(&addressed).unwrap();

        // Flat, not nested: the request's own keys sit alongside `relay`.
        let map: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(map["t"], "ping");
        assert_eq!(map["protocol"], 7);
        assert_eq!(map["relay"], "https://relay.example/");
        assert_eq!(map["from"]["id"], "0".repeat(64));

        // Forwards: a peer that predates the field ignores it and still reads the
        // request — including the version number it needs to report a mismatch.
        let old_reader: Request = serde_json::from_slice(&raw).unwrap();
        assert!(matches!(old_reader, Request::Ping { protocol: 7 }));

        // Backwards: a request from such a peer carries no address, and that is
        // not an error — it just teaches us nothing this time.
        let old_wire = serde_json::to_vec(&Request::Ping { protocol: 7 }).unwrap();
        let read: Envelope = serde_json::from_slice(&old_wire).unwrap();
        assert!(read.relay.is_none());
        assert!(read.from.is_none(), "no delegation is absent, not an error");
        assert!(matches!(read.request, Request::Ping { protocol: 7 }));
    }

    /// A newtype variant flattens too, so the check above must not be the only
    /// shape tested.
    #[test]
    fn a_struct_carrying_variant_survives_the_same_round_trip() {
        let addressed = Envelope {
            relay: Some("https://relay.example/".into()),
            from: None,
            request: Request::Verify(VerifyRequest {
                protocol: PROTOCOL_VERSION,
                project: "syncwork".into(),
                recipe: "ci".into(),
                base: "abc123".into(),
                patch_bytes: 0,
                from_host: "win-pc".into(),
                color: false,
            }),
        };
        let raw = serde_json::to_vec(&addressed).unwrap();
        let back: Envelope = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.relay.as_deref(), Some("https://relay.example/"));
        match back.request {
            Request::Verify(v) => assert_eq!(v.recipe, "ci"),
            other => panic!("wrong variant: {other:?}"),
        }
        // And an older peer still reads the request out of it.
        assert!(matches!(
            serde_json::from_slice::<Request>(&raw).unwrap(),
            Request::Verify(_)
        ));
    }

    #[test]
    fn events_round_trip_through_json() {
        let cases = vec![
            Event::Accepted {
                host: "mac-mini".into(),
                platform: Platform::Macos,
                repo: "/Users/x/p".into(),
                build: "abc1234".into(),
            },
            Event::Log {
                err: true,
                line: "boom".into(),
            },
            Event::Done {
                code: 101,
                ms: 4200,
            },
            Event::Failed {
                message: "patch did not apply".into(),
            },
            Event::Diag(Diagnostic {
                level: "error".into(),
                message: "mismatched types".into(),
                file: Some("src/main.rs".into()),
                line: Some(12),
                col: Some(5),
                rendered: None,
            }),
        ];
        for ev in cases {
            let raw = serde_json::to_vec(&ev).unwrap();
            let back: Event = serde_json::from_slice(&raw).unwrap();
            assert_eq!(format!("{ev:?}"), format!("{back:?}"));
        }
    }
}
