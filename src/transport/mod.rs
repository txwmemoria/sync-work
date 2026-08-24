//! The wire: message types, framing, and the authenticated QUIC endpoint.

pub mod endpoint;
pub mod frame;
pub mod message;
pub mod ticket;

pub use endpoint::{
    authorize, connect, describe_paths, explain, note_self_relay, open_request, refusal,
    self_relay, with_endpoint,
};
pub use frame::{read_event, read_frame, read_json, write_frame, write_json, write_request};
pub use message::{
    ALPN, CtxEntry, CtxFrame, CtxRoot, Diagnostic, Envelope, Event, HandoffResult,
    PROTOCOL_VERSION, Request, VerifyRequest, version_mismatch,
};
pub use ticket::{Ticket, local_ticket};
