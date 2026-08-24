//! The wire: message types, framing, and the authenticated QUIC endpoint.

pub mod endpoint;
pub mod frame;
pub mod message;
pub mod ticket;

pub use endpoint::{
    authorize, connect, describe_paths, explain, lan_probe, note_self_relay, open_request, refusal,
    self_relay, verify_delegation, with_endpoint, with_machine_endpoint,
};
pub use frame::{
    push_json, read_event, read_frame, read_json, write_frame, write_json, write_request,
};
pub use message::{
    ALPN, CtxEntry, CtxFrame, CtxRoot, Delegation, Diagnostic, Envelope, Event, HandoffResult,
    PROTOCOL_VERSION, Request, VerifyRequest, delegation_message, version_mismatch,
};
pub use ticket::{Ticket, local_ticket};
