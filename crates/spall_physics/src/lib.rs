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

pub mod character;
pub mod collider;
pub mod fixtures;
pub mod mass;
pub mod merge;
pub mod metrics;
pub mod occupancy;
pub mod report;
pub mod world;

pub use character::{CharacterMove, CharacterParams, CharacterState, PlayerInput, step_character};
pub use collider::{ColliderBuild, Representation, build_collider};
pub use mass::{BodyMassProperties, MassProperties, analytic_mass_properties};
pub use merge::{BoxSpan, greedy_boxes};
pub use metrics::{DurationSamples, PercentileSummary};
pub use occupancy::{ExtractError, OccupancyGrid};
pub use report::{FeasibilityReport, RepresentationReport, SleepWakeReport, run_feasibility};
pub use world::{
    BodyId, BodyKind, BodySpec, BodyState, ContactImpulse, PhysicsConfig, PhysicsWorld, StepTiming,
};
