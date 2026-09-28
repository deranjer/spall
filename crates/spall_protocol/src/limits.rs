//! Wire size and count limits.
//!
//! Every value that bounds an allocation lives here so the decode path can
//! check it *before* reserving memory. The byte limits mirror `docs/protocol.md`
//! ("Limits start at 64 KiB per control record ...").

/// Maximum bytes in one reliable control / topology record, including the
/// 4-byte protocol header.
pub const MAX_CONTROL_RECORD: usize = 64 * 1024;

/// Maximum bytes in one baseline bulk part payload.
pub const MAX_BULK_PART: usize = 1024 * 1024;

/// Maximum **compressed** bytes in one fully assembled baseline transfer —
/// the actual bytes that cross the wire (`spall_server::baseline::
/// transfer_from_world` compresses the transfer; see docs/reports/G3.md's G4
/// increment). Distinct from [`MAX_ASSEMBLED_TRANSFER_DECOMPRESSED`] for the
/// same reason [`MAX_SPLIT_BASELINE_BLOB`] is distinct from
/// [`MAX_SPLIT_BASELINE_DECOMPRESSED`]: a world with many small, highly
/// redundant bodies (e.g. T23/G4's debris population) compresses far smaller
/// than it decodes.
pub const MAX_ASSEMBLED_TRANSFER: usize = 64 * 1024 * 1024;

/// Maximum **decompressed** bytes accepted from one assembled baseline
/// transfer — a DoS bound on `zstd` output, independent of the wire-size cap
/// above. A world whose *compressed* transfer already fit
/// [`MAX_ASSEMBLED_TRANSFER`] can still decompress to legitimately more than
/// that (many small, mostly-air bodies each cost a full dense brick
/// decompressed); this bound is sized for that case, not just headroom on the
/// wire cap.
pub const MAX_ASSEMBLED_TRANSFER_DECOMPRESSED: usize = 256 * 1024 * 1024;

/// Maximum *decompressed* size of a material-only brick record.
pub const MAX_BRICK_MATERIAL_DECOMPRESSED: usize = 256 * 1024;

/// Actual material payload carried in a brick record (`32³` `u16` ids).
pub const MAX_BRICK_MATERIAL_PAYLOAD: usize = 64 * 1024;

/// Maximum bytes in one datagram payload after our envelope
/// (`min(1100, connection.max_datagram_size())`).
pub const MAX_DATAGRAM_PAYLOAD: usize = 1100;

/// Redundant input frames carried inside an [`crate::records::InputFrame`].
pub const MAX_REDUNDANT_INPUTS: usize = 3;

/// Maximum ordered operations in one [`crate::records::TopologyTransaction`].
pub const MAX_TRANSACTION_OPS: usize = 4096;

/// Maximum dependency / before / after / result entries in one transaction.
pub const MAX_TRANSACTION_REFS: usize = 4096;

/// Maximum region entries in a baseline manifest.
pub const MAX_BASELINE_REGIONS: usize = 8192;

/// Maximum parts in one baseline transfer
/// (`MAX_ASSEMBLED_TRANSFER / MAX_BULK_PART`, rounded up, with headroom).
pub const MAX_BASELINE_PARTS: usize = 4096;

/// Maximum cells addressed by one encoded cell run.
pub const MAX_CELL_RUN_LEN: u32 = 1 << 20;

/// Maximum **compressed** bytes in one `SplitOffBaseline` / `SourcePatchBaseline`
/// op blob (T17). Small enough that an oversized split's whole
/// `TopologyTransaction` — brush op, one child blob, one source-patch blob, plus
/// `before` / `after` / `result_hashes` — still fits [`MAX_CONTROL_RECORD`]. A
/// split whose blob would exceed this needs the bulk-stream baseline path.
pub const MAX_SPLIT_BASELINE_BLOB: usize = 28 * 1024;

/// Maximum **decompressed** bytes accepted from one split baseline op blob — a
/// DoS bound on `zstd` output, well above any blob the commit path will emit
/// under [`MAX_SPLIT_BASELINE_BLOB`].
pub const MAX_SPLIT_BASELINE_DECOMPRESSED: usize = 4 * 1024 * 1024;

/// Error returned when an encoded or declared size exceeds its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SizeLimitError {
    #[error("encoded {kind} is {actual} bytes; limit is {limit}")]
    Bytes {
        kind: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("{kind} count is {actual}; limit is {limit}")]
    Count {
        kind: &'static str,
        actual: usize,
        limit: usize,
    },
}

impl SizeLimitError {
    pub(crate) fn bytes(kind: &'static str, actual: usize, limit: usize) -> Self {
        Self::Bytes {
            kind,
            actual,
            limit,
        }
    }

    pub(crate) fn count(kind: &'static str, actual: usize, limit: usize) -> Self {
        Self::Count {
            kind,
            actual,
            limit,
        }
    }
}

/// Checks a declared or observed count against a limit before it is used to
/// size an allocation.
pub(crate) fn check_count(
    kind: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), SizeLimitError> {
    if actual > limit {
        Err(SizeLimitError::count(kind, actual, limit))
    } else {
        Ok(())
    }
}
