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
    WorldSetup {
        terrain: spall_voxel::fixtures::bridge_scene(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 15, 3)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// Like [`bridged_terrain_setup`], but the column and beam **cross the `x = 32`
/// brick boundary**: cutting the seam-straddling column detaches the beam as one
/// unsupported component whose cells were owned across two bricks. Exercises
/// cross-brick structural support propagation and brick-boundary ownership
/// transfer — see [`spall_voxel::fixtures::cross_brick_bridge_scene`].
pub fn cross_brick_bridged_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::cross_brick_bridge_scene(id),
        terrain_collider_region: (GlobalCell::new(20, 0, 0), GlobalCell::new(44, 15, 3)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// T17 / ENG-64: a scene whose detached component is too fragmented to encode
/// inline. An anchored floor holds a `24 x 24 x 24` checkerboard `STONE`/`DIRT`
/// block through one column; cutting the column detaches the whole block, whose
/// ~13.8k single-cell runs overflow the inline `CellRun` budget and force the
/// compressed-baseline-blob commit path. See
/// [`spall_voxel::fixtures::checkerboard_split_scene`].
pub fn checkerboard_split_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::checkerboard_split_scene(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(31, 31, 31)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// The T19 movement arena: a flat anchored floor + a 0.5 m step ledge + a
/// resident air ceiling ([`spall_voxel::fixtures::walk_arena`]). Scripted
/// players walk this lane; [`WALK_ARENA_SPAWNS`] gives feet positions in metres
/// near `x = 0` and clear of the step.
pub fn walk_arena_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::walk_arena(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(119, 13, 15)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// Feet spawn positions (metres) for [`walk_arena_setup`]; index is the player /
/// connection slot. The floor top is `y = 1.0 m`.
pub const WALK_ARENA_SPAWNS: [[f64; 3]; 4] = [
    [1.0, 1.0, 1.5],
    [1.0, 1.0, 2.5],
    [1.0, 1.0, 1.0],
    [1.0, 1.0, 3.0],
];

/// Like [`bridged_terrain_setup`], but the whole scene is translated so its
/// occupancy's minimum corner is far from the world origin, and the floor sits
/// **only under the beam's own x-range**. A detached beam whose collider is
/// mis-placed near body-local zero (`ENG-55`) misses the floor entirely and
/// free-falls; a correctly offset collider lands the beam on the floor.
///
/// - floor:  `x 24..=39`, `z 0..=2`, `y 0..=1`
/// - column: `x 31`,       `z 1`,     `y 2..=6`
/// - beam:   `x 24..=39`,  `z 1`,     `y 7..=8`
pub fn far_bridged_terrain_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    // Resident air envelope first, so later solid writes win.
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(24, 0, 0),
        GlobalCell::new(39, 20, 2),
        MaterialId::AIR,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(24, 0, 0),
        GlobalCell::new(39, 1, 2),
        STONE,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(31, 2, 1),
        GlobalCell::new(31, 6, 1),
        STONE,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(24, 7, 1),
        GlobalCell::new(39, 8, 1),
        STONE,
    ))
    .unwrap();

    WorldSetup {
        terrain: v,
        terrain_collider_region: (GlobalCell::new(24, 0, 0), GlobalCell::new(39, 20, 2)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// A raised solid stone slab whose occupancy minimum is far from the world
/// origin: `x 24..=43`, `y 8..=9`, `z 0..=3`, wrapped in a resident air
/// envelope. The anchor plane is at `y = 8`, so the whole slab is anchored:
/// erasing its low-x cells leaves the rest supported (nothing detaches — the
/// non-splitting terrain-edit + collider-rebuild path), while the tight
/// occupancy origin moves and the rebuilt collider must track it (`ENG-55`).
pub fn far_raised_block_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(24, 6, 0),
        GlobalCell::new(43, 20, 3),
        MaterialId::AIR,
    ))
    .unwrap();
    v.apply_edit(&box_plan(
        id,
        GlobalCell::new(24, 8, 0),
        GlobalCell::new(43, 9, 3),
        STONE,
    ))
    .unwrap();
    WorldSetup {
        terrain: v,
        terrain_collider_region: (GlobalCell::new(24, 6, 0), GlobalCell::new(43, 20, 3)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(8),
        physics: PhysicsConfig::default(),
    }
}

/// A [`dumbbell`] whose `(0,0,0)`-anchored geometry is shifted so its occupancy
/// minimum sits at `off` cells from the volume origin. Used to give a moving,
/// rotating parent body a non-zero occupancy-grid origin before it is cut, so
/// the split exercises the parent-COM/​collider-origin path (`ENG-55`).
pub fn offset_dumbbell(s: i64, gap: i64, off: GlobalCell) -> impl FnOnce(VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        let rx = s + gap;
        let mid = s / 2;
        let shift = |a: GlobalCell| GlobalCell::new(a.x + off.x, a.y + off.y, a.z + off.z);
        v.apply_edit(&box_plan(
            id,
            shift(GlobalCell::new(0, 0, 0)),
            shift(GlobalCell::new(s - 1, s - 1, s - 1)),
            STONE,
        ))
        .unwrap();
        v.apply_edit(&box_plan(
            id,
            shift(GlobalCell::new(rx, 0, 0)),
            shift(GlobalCell::new(rx + s - 1, s - 1, s - 1)),
            STONE,
        ))
        .unwrap();
        v.apply_edit(&box_plan(
            id,
            shift(GlobalCell::new(s, mid, mid)),
            shift(GlobalCell::new(rx - 1, mid, mid)),
            STONE,
        ))
        .unwrap();
        v
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

/// A body-local mixed-material "tadpole": a large stone head, a one-cell stone
/// bridge, and a fused **stone + dirt** block. Cutting the bridge keeps the head
/// as the parent (the larger component) and detaches the fused block as a single
/// multi-material child whose centre of mass sits well off its geometric centre
/// (stone is denser than dirt, so the COM is pulled toward the stone half).
///
/// - head:   `x 0..6`, `y 0..6`, `z 0..6`  (216 cells, stone)
/// - bridge: `x 6..9`, `y 2`, `z 2`        (stone)
/// - child:  `x 9..17`, `y 0..4`, `z 0..4` — stone `x 9..13`, dirt `x 13..17`
///   (128 cells)
pub fn mixed_material_split_body() -> impl FnOnce(VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(5, 5, 5),
            STONE,
        ))
        .unwrap();
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(6, 2, 2),
            GlobalCell::new(8, 2, 2),
            STONE,
        ))
        .unwrap();
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(9, 0, 0),
            GlobalCell::new(12, 3, 3),
            STONE,
        ))
        .unwrap();
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(13, 0, 0),
            GlobalCell::new(16, 3, 3),
            DIRT,
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
