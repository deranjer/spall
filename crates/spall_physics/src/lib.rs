//! `spall_physics` — the narrow Rapier adapter for editable voxel collision.
//!
//! Scope is the T06 feasibility question: can pinned Rapier represent editable,
//! concave voxel geometry — terrain and detached bodies — with stable contacts,
//! correct mass, and an affordable rebuild after an edit? Two representations
//! are built from the *same* [`occupancy::OccupancyGrid`] and compared:
//!
//! * [`collider::Representation::NativeVoxels`] — one `parry` voxel shape.
//! * [`collider::Representation::MergedCuboids`] — a compound of the axis-aligned
//!   boxes from [`merge::greedy_boxes`]; exact occupied space, the correctness
//!   baseline.
//!
//! [`mass::analytic_mass_properties`] is the independent reference both
//! representations' Rapier-derived mass must match. [`world::PhysicsWorld`] is
//! the fixed-step wrapper; Rapier handles never leave it — callers use
//! [`world::BodyId`]. [`report`] and the `collision-bench` binary run the
//! feasibility scenarios and emit p50/p95/p99 step, rebuild, and memory figures.
//!
//! No GPU, window, or network dependency. This crate does not run the server
//! tick; body/collider ownership and the authoritative edit path are T08.

pub mod articulated;
pub mod character;
pub mod collider;
pub mod coordinates;
pub mod fixtures;
pub mod fracture;
pub mod mass;
pub mod merge;
pub mod metrics;
pub mod occupancy;
pub mod query_cache;
pub mod region_set;
pub mod report;
pub mod world;

pub use articulated::{
    ArticulatedBodyId, ArticulatedError, ArticulatedMove, ArticulatedPose, ArticulatedPoseSnapshot,
    ArticulatedTransition, DynamicCorpseComponentRestore, DynamicCorpseRestore, PartTransform,
    SourceCellHit, SourcePart,
};
pub use character::{
    CharacterMove, CharacterParams, CharacterState, PlayerInput, WaterSample, step_character,
    step_character_in_water,
};
pub use collider::{
    ColliderBuild, MERGED_CUBOID_PRIMITIVE_BUDGET, Representation, build_collider,
    choose_representation,
};
pub use coordinates::PhysicsOrigin;
pub use fracture::{
    DerivedFractureComponentId, FRACTURE_PLAN_VERSION, FractureComponent, FractureError,
    FracturePartSnapshot, FractureSnapshot, FractureSnapshotToken, PreparedFracturePlan,
    SourceCellRef, prepare_fracture,
};
pub use mass::{
    BodyMassProperties, MassProperties, analytic_mass_properties, articulated_mass_properties,
};
pub use merge::{BoxSpan, greedy_boxes};
pub use metrics::{DurationSamples, PercentileSummary};
pub use occupancy::{ExtractError, OccupancyGrid};
pub use query_cache::{CharacterQueryCache, REBUILD_MARGIN_M, WINDOW_RADIUS_M, WindowStats};
pub use region_set::{PhysicsRegionSet, PhysicsRegionSetError, RegionalBodyState};
pub use report::{FeasibilityReport, RepresentationReport, SleepWakeReport, run_feasibility};
pub use world::{
    ArticulatedBatchOperation, ArticulatedBatchOutcome, ArticulatedContactWitness,
    ArticulatedOwnerSnapshot, BodyCommitFence, BodyId, BodyKind, BodySpec, BodyState,
    CONTACT_WITNESS_VERSION, ColliderExclusion, ContactCellAttribution, ContactImpulse,
    ContactWitnessUnattributed, CorpseEditOutcome, CorpseTransferOutcome, FractureCommitOutcome,
    MAX_CONTACT_WITNESSES_PER_STEP, MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS,
    MAX_CORPSE_RESTORE_BATCH_OWNERS, MAX_CORPSE_RESTORE_BATCH_PRIMITIVES,
    MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS, PhysicsConfig, PhysicsWorld, PreparedArticulatedBatch,
    PreparedBodySpec, PreparedCollider, PreparedDynamicCorpseBatch, SourceContactWitness,
    StepTiming, ValidatedArticulatedBatch,
};
