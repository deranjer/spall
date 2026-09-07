//! Length-framed record I/O over a QUIC stream.
//!
//! Every reliable record on a control or bulk stream is written as:
//!
//! ```text
//! u32 LE  inner length   (bytes that follow; checked against the channel cap)
//! ..      inner bytes     (a `spall_protocol` framed record: u16 schema | u16 tag | body)
//! ```
//!
//! [`read_framed`] validates the declared length against the caller's cap
//! *before* it allocates, so a hostile 4 GiB length prefix costs nothing. A
//! clean stream FIN at a frame boundary is reported as `Ok(None)`.

use quinn::{RecvStream, SendStream};

/// Bytes of framing overhead added per record (the `u32` length prefix).
pub const LENGTH_PREFIX: usize = 4;

/// A framing-layer failure.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The declared inner length is larger than this channel allows. Rejected
    /// before any buffer is reserved.
    #[error("framed record declares {declared} bytes; channel cap is {limit}")]
    Oversize { declared: usize, limit: usize },
    /// The caller tried to write a record larger than the channel cap.
    #[error("record is {actual} bytes; channel cap is {limit}")]
    WriteOversize { actual: usize, limit: usize },
    /// The stream ended in the middle of a frame.
    #[error("stream ended mid-frame after {got} of {want} bytes")]
    Incomplete { got: usize, want: usize },
    /// A bulk transfer exceeded its fixed protocol count ceiling.
    #[error("bulk transfer contains more than {limit} parts")]
    BulkPartCount { limit: usize },
    /// Parts in a bulk stream must belong to one transfer.
    #[error("bulk stream mixes transfer ids")]
    TransferMismatch,
    /// Parts in a bulk stream must be contiguous and ordered.
    #[error("bulk part index {found} is not the expected {expected}")]
    PartOrder { expected: u32, found: u32 },
    /// The declared per-part content hash did not match the payload.
    #[error("bulk part {index} hash does not match its payload")]
    PartHashMismatch { index: u32 },
    /// The underlying QUIC stream errored.
    #[error("quic stream: {0}")]
    Stream(String),
}

/// Writes one length-framed record. `inner` is the already `spall_protocol`
/// -framed record bytes. Fails without touching the stream if `inner` exceeds
/// `cap`.
pub async fn write_framed(
    stream: &mut SendStream,
    inner: &[u8],
    cap: usize,
) -> Result<(), FrameError> {
    if inner.len() > cap {
        return Err(FrameError::WriteOversize {
            actual: inner.len(),
            limit: cap,
        });
    }
    let len = inner.len() as u32;
    let mut buf = Vec::with_capacity(LENGTH_PREFIX + inner.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(inner);
    stream
        .write_all(&buf)
        .await
        .map_err(|e| FrameError::Stream(e.to_string()))
}

/// Reads one length-framed record. Returns `Ok(None)` on a clean FIN before the
/// next frame starts. The declared length is bounds-checked against `cap`
/// before allocation.
pub async fn read_framed(
    stream: &mut RecvStream,
    cap: usize,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut len_bytes = [0u8; LENGTH_PREFIX];
    match stream.read_exact(&mut len_bytes).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(quinn::ReadExactError::FinishedEarly(got)) => {
            return Err(FrameError::Incomplete {
                got,
                want: LENGTH_PREFIX,
            });
        }
        Err(quinn::ReadExactError::ReadError(e)) => return Err(FrameError::Stream(e.to_string())),
    }

    let declared = u32::from_le_bytes(len_bytes) as usize;
    if declared > cap {
        return Err(FrameError::Oversize {
            declared,
            limit: cap,
        });
    }

    let mut body = vec![0u8; declared];
    match stream.read_exact(&mut body).await {
        Ok(()) => Ok(Some(body)),
        Err(quinn::ReadExactError::FinishedEarly(got)) => Err(FrameError::Incomplete {
            got,
            want: declared,
        }),
        Err(quinn::ReadExactError::ReadError(e)) => Err(FrameError::Stream(e.to_string())),
    }
}
