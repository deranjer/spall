//! `spall_structure` — six-face structural connectivity, support propagation,
//! and split plans for authoritative destruction.
//!
//! No GPU, window, or network dependency. Given a [`spall_voxel::Volume`] and a
//! declared support plane, this crate:
//!
//! - labels the six-face-connected components inside every changed brick
//!   ([`label`]);
//! - joins `(brick, local component)` nodes across matching solid boundary
//!   cells into a cross-brick [`graph`], and finds the global components with a
//!   bounded, cancellable, resumable search (union-find alone cannot undo a
//!   disconnection, so deletions recompute);
//! - classifies each global component as [`Support::Supported`],
//!   [`Support::Unsupported`], or [`Support::Unknown`] when connectivity runs
//!   into a brick that is not resident ([`support`]);
//! - emits canonical X-run cell membership for every unsupported component and a
//!   conservation ledger the split must balance ([`split`]).
//!
//! - adds a material-dependent strength stage ([`strength`], T22): a still-
//!   connected component can fail when a bond on its load path carries more than
//!   its material's capacity, and the resulting broken bonds are authoritative,
//!   persisted [`strength::DamageState`] so a restart cannot heal a failing beam.
//!
//! [`StructureIndex`] is the incremental entry point: it carries a
//! [`spall_jobs::JobToken`] for the exact brick revisions it read, so a
//! consumer can reject a stale analysis the same way it would reject any other
//! off-tick job result. [`StructureIndex::apply_edit`] re-labels only the
//! bricks an [`spall_voxel::EditOutcome`] touched.
//!
//! Everything is all-resident G1 behaviour. Streamed residency and structural
//! dependency loading are T18.

pub mod graph;
pub mod label;
pub mod split;
pub mod strength;
pub mod support;

#[cfg(any(test, feature = "oracle"))]
pub mod oracle;

#[cfg(test)]
mod scenario_tests;

pub use graph::{
    AnchorPlane, CancelToken, GlobalComponent, GlobalComponentId, Interrupted, NodeKey,
    ResidencyMode, SearchBudget, SupportGraph,
};
pub use label::{BrickLabels, LocalComponent, label_brick};
pub use split::{CellSpanX, ComponentMembership, ConservationError, ConservationLedger};
pub use strength::{
    BondFailure, BrokenBond, DamageState, DetachedComponent, STRENGTH_ALGO_VERSION, StrengthParams,
    StrengthReport, evaluate as evaluate_strength,
};
pub use support::{ClassifiedComponent, StructureIndex, Support, SupportReport};
