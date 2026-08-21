//! The wire: message types, framing, and the authenticated QUIC endpoint.

pub mod endpoint;
pub mod frame;
pub mod message;
pub mod ticket;

pub use endpoint::{authorize, bind, connect};
pub use frame::{read_event, read_frame, read_json, write_frame, write_json};
pub use message::{
    ALPN, CtxEntry, CtxFrame, CtxRoot, Diagnostic, Event, PROTOCOL_VERSION, Request, VerifyRequest,
    version_mismatch,
};
pub use ticket::{Ticket, local_ticket};
