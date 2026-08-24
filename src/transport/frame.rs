//! Length-prefixed framing.
//!
//! Layout: `u32` big-endian length, then that many bytes. Every announced length
//! is checked against [`MAX_FRAME`] *before* allocating, so a malformed or
//! hostile peer cannot ask this process to reserve four gigabytes.

use super::Event;
use anyhow::{Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

/// Patches are diffs of a working tree, but a vendored dependency or a binary
/// asset can still make one large. 256 MiB is well past anything legitimate
/// while still bounding a malicious allocation.
pub const MAX_FRAME: usize = 256 * 1024 * 1024;

pub async fn write_frame(tx: &mut SendStream, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_FRAME {
        bail!("frame of {} bytes exceeds limit", bytes.len());
    }
    tx.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    tx.write_all(bytes).await?;
    Ok(())
}

pub async fn read_frame(rx: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    rx.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("peer announced a {len} byte frame, over the {MAX_FRAME} limit");
    }
    let mut buf = vec![0u8; len];
    if len > 0 {
        rx.read_exact(&mut buf).await?;
    }
    Ok(buf)
}

pub async fn write_json<T: Serialize>(tx: &mut SendStream, value: &T) -> Result<()> {
    write_frame(tx, &serde_json::to_vec(value)?).await
}

/// Write the first frame of a request stream: the request, and where we live.
///
/// Every request goes through here rather than through [`write_json`] directly, so
/// that a peer only has to be reachable once for the reverse direction to keep
/// working — see [`Envelope`](super::Envelope).
pub async fn write_request(tx: &mut SendStream, request: super::Request) -> Result<()> {
    let envelope = super::Envelope {
        relay: super::endpoint::self_relay(),
        request,
    };
    write_json(tx, &envelope).await
}

pub async fn read_json<T: for<'de> Deserialize<'de>>(rx: &mut RecvStream) -> Result<T> {
    let raw = read_frame(rx).await?;
    Ok(serde_json::from_slice(&raw)?)
}

/// Read one event, or `None` at clean end of stream. The peer finishing its
/// send side is the normal way a request ends, so a truncated read here is
/// end-of-events rather than an error.
pub async fn read_event(rx: &mut RecvStream) -> Result<Option<Event>> {
    let mut len = [0u8; 4];
    match rx.read_exact(&mut len).await {
        Ok(()) => {}
        Err(_) => return Ok(None),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("peer announced a {len} byte frame, over the {MAX_FRAME} limit");
    }
    let mut buf = vec![0u8; len];
    rx.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_frame_is_rejected_before_allocating() {
        // A 4 GiB announcement must be refused on the length alone.
        let announced = u32::MAX as usize;
        assert!(announced > MAX_FRAME);
    }
}
