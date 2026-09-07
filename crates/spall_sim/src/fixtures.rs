//! Small, CPU-cheap authoritative-world fixtures for the acceptance scenarios.
//!
//! Every fixture is a pure function: no RNG, no wall-clock, no I/O. Geometry is
//! kept well under a few thousand cells so the structural analysis, the collider
//! rebuilds, and a handful of physics steps all run inside CPU CI.

use glam::{DQuat, DVec3};
use spall_core::{
    CellSizeCode, GlobalCell, MaterialDef, MaterialFlags, MaterialId, MaterialManifest,
    RenderProps, SimProps, VolumeId,
};
use spall_physics::PhysicsConfig;
use spall_structure::AnchorPlane;
use spall_voxel::{EditPlan, Volume};

use crate::world::WorldSetup;

/// Fixture stone: id 1, 2600 kg/m³.
pub const STONE: MaterialId = MaterialId(1);
/// Fixture dirt: id 2, 1500 kg/m³.
pub const DIRT: MaterialId = MaterialId(2);

fn material(id: u16, name: &str, density: f32) -> MaterialDef {
    MaterialDef {
        id: MaterialId(id),
        name: name.into(),
        render: RenderProps {
            albedo: [0.5, 0.5, 0.5],
            roughness: 0.9,
            metalness: 0.0,
            emissive: [0.0; 3],
        },
        sim: SimProps {
            density_kg_m3: density,
            friction: 0.8,
            restitution: 0.05,
            hardness: 4.0,
            bond_strength: 12.0,
            flags: MaterialFlags(
                MaterialFlags::OPAQUE.0 | MaterialFlags::COLLIDES.0 | MaterialFlags::STRUCTURAL.0,
            ),
        },
    }
}

/// A minimal validated manifest: air, stone, dirt.
pub fn stone_manifest() -> MaterialManifest {
    MaterialManifest::validated(vec![
        MaterialDef {
            id: MaterialId::AIR,
            name: "air".into(),
            render: RenderProps {
                albedo: [0.0; 3],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 0.0,
                friction: 0.0,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags::NONE,
            },
        },
        material(1, "stone", 2600.0),
        material(2, "dirt", 1500.0),
    ])
    .expect("hand-built manifest is valid")
}

fn box_plan(v: VolumeId, a: GlobalCell, b: GlobalCell, m: MaterialId) -> EditPlan {
    EditPlan::filled_box(v, a, b, m)
}

/// A flat anchored stone slab, `x,z in 0..24`, `y in 0..2`, with a resident air
/// layer above. Anchor plane at `y = 0`. Nothing detaches from a plain surface
/// cut — this fixture exercises the terrain edit + collider swap path and
/// conservation with `child_cells == 0`.
pub fn flat_terrain_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(23, 1, 23),
        STONE,
    ))
    .unwrap();
    // Resident air above so a downward query crosses empty cells, not Unknown.
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(0, 2, 0),
        GlobalCell::new(23, 9, 23),
        MaterialId::AIR,
    ))
    .unwrap();

    WorldSetup {
        terrain: v,
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 9, 23)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// A stone floor plus a raised beam held up by a single column. Cutting the
/// column disconnects the beam as exactly one unsupported component — the
/// terrain-to-body transfer path.
///
/// - floor: `x 0..24`, `z 0..3`, `y 0..2` (anchored at `y = 0`)
/// - column: `x 10..11`, `z 1..2`, `y 2..7`
/// - beam:   `x 4..20`, `z 1..2`, `y 8..9`
pub fn bridged_terrain_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(23, 1, 3),
        STONE,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(10, 2, 1),
        GlobalCell::new(11, 7, 2),
        STONE,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(4, 8, 1),
        GlobalCell::new(20, 9, 2),
        STONE,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(0, 10, 0),
        GlobalCell::new(23, 15, 3),
        MaterialId::AIR,
    ))
    .unwrap();

    WorldSetup {
        terrain: v,
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 15, 3)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// A solid stone block, `size` cells on a side, built in a body-local frame with
/// its `(0,0,0)` corner at the local origin. Used with
/// [`crate::world::SimWorld::spawn_body`] to stand up a moving/rotating body.
pub fn solid_block(size: i64) -> impl FnOnce(VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(size - 1, size - 1, size - 1),
            STONE,
        ))
        .unwrap();
        v
    }
}

/// A body-local "dumbbell": two solid stone cubes joined by a one-cell-thick
/// bridge along X, so a cut through the bridge disconnects it into two bodies.
/// Cube side `s`, bridge length `gap` (>= 1).
pub fn dumbbell(s: i64, gap: i64) -> impl FnOnce(VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        // left cube
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(s - 1, s - 1, s - 1),
            STONE,
        ))
        .unwrap();
        // right cube
        let rx = s + gap;
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(rx, 0, 0),
            GlobalCell::new(rx + s - 1, s - 1, s - 1),
            STONE,
        ))
        .unwrap();
        // 1x1 bridge along the mid row
        let mid = s / 2;
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(s, mid, mid),
            GlobalCell::new(rx - 1, mid, mid),
            STONE,
        ))
        .unwrap();
        v
    }
}

/// A 45° rotation about an oblique axis, as a `DQuat`.
pub fn oblique_spin() -> DQuat {
    DQuat::from_axis_angle(
        DVec3::new(1.0, 2.0, -0.5).normalize(),
        std::f64::consts::FRAC_PI_4,
    )
}
