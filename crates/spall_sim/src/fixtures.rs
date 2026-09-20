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

use crate::body::BodyPose;
use crate::world::WorldSetup;

/// Physics config for the fixtures: rapier's CCD solver pass off, matching
/// `spall_server::serve` (`setup.physics.disable_ccd = true`). No authoritative
/// body ever enables per-body CCD, and the CCD broad-phase BVH can otherwise
/// retain a stale proxy for a collider a voxel edit removed and re-inserted in
/// the same step, then panic mid-sweep — reachable now that body-on-body
/// fracture (T21 increment 3) rebuilds a dynamic collider under a live impact.
fn sim_physics_config() -> PhysicsConfig {
    PhysicsConfig {
        disable_ccd: true,
        ..PhysicsConfig::default()
    }
}

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

// --- playground (interactive `cargo xtask play --scene playground`) ------
//
// Not a gate fixture: this manifest exists purely to make the hands-on
// playground scene visually varied ("spice things up" -- every material in
// `stone_manifest()` above renders the same flat grey `albedo`, `dirt`
// included). Kept entirely separate from `stone_manifest()` so no existing
// pinned fixture hash or gate scenario is affected.

fn coloured_material(id: u16, name: &str, density: f32, albedo: [f32; 3]) -> MaterialDef {
    MaterialDef {
        id: MaterialId(id),
        name: name.into(),
        render: RenderProps {
            albedo,
            roughness: 0.85,
            metalness: 0.0,
            emissive: [0.0; 3],
        },
        sim: SimProps {
            density_kg_m3: density,
            friction: 0.8,
            restitution: 0.15,
            hardness: 4.0,
            bond_strength: 12.0,
            flags: MaterialFlags(
                MaterialFlags::OPAQUE.0 | MaterialFlags::COLLIDES.0 | MaterialFlags::STRUCTURAL.0,
            ),
        },
    }
}

/// Playground grass (plaza floor primary tile), id 10.
pub const PLAYGROUND_GRASS: MaterialId = MaterialId(10);
/// Playground loam (plaza floor secondary/checkerboard tile), id 11.
pub const PLAYGROUND_LOAM: MaterialId = MaterialId(11);
/// Playground brick (stairs), id 12.
pub const PLAYGROUND_BRICK: MaterialId = MaterialId(12);
/// Playground sandstone (maze walls), id 13.
pub const PLAYGROUND_SANDSTONE: MaterialId = MaterialId(13);
/// Playground slate (jump-course platforms), id 14.
pub const PLAYGROUND_SLATE: MaterialId = MaterialId(14);
/// Debris red, id 15.
pub const DEBRIS_RED: MaterialId = MaterialId(15);
/// Debris orange, id 16.
pub const DEBRIS_ORANGE: MaterialId = MaterialId(16);
/// Debris yellow, id 17.
pub const DEBRIS_YELLOW: MaterialId = MaterialId(17);
/// Debris green, id 18.
pub const DEBRIS_GREEN: MaterialId = MaterialId(18);
/// Debris blue, id 19.
pub const DEBRIS_BLUE: MaterialId = MaterialId(19);
/// Debris purple, id 20.
pub const DEBRIS_PURPLE: MaterialId = MaterialId(20);

/// Every non-debris, non-air material [`crate::fixtures::playground_scene`]
/// paints the terrain with, plus a bright palette [`crate::playground`]'s
/// spawner picks a random material from for each dropped body.
pub fn playground_manifest() -> MaterialManifest {
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
        coloured_material(10, "grass", 1200.0, [0.25, 0.55, 0.2]),
        coloured_material(11, "loam", 1400.0, [0.4, 0.28, 0.15]),
        coloured_material(12, "brick", 2000.0, [0.65, 0.25, 0.18]),
        coloured_material(13, "sandstone", 2200.0, [0.82, 0.7, 0.45]),
        coloured_material(14, "slate", 2400.0, [0.35, 0.38, 0.42]),
        coloured_material(15, "debris-red", 2000.0, [0.85, 0.15, 0.15]),
        coloured_material(16, "debris-orange", 2000.0, [0.9, 0.5, 0.1]),
        coloured_material(17, "debris-yellow", 2000.0, [0.9, 0.85, 0.15]),
        coloured_material(18, "debris-green", 2000.0, [0.2, 0.75, 0.3]),
        coloured_material(19, "debris-blue", 2000.0, [0.2, 0.4, 0.9]),
        coloured_material(20, "debris-purple", 2000.0, [0.6, 0.25, 0.8]),
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
        physics: sim_physics_config(),
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
        physics: sim_physics_config(),
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
        physics: sim_physics_config(),
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
        physics: sim_physics_config(),
    }
}

/// T17 increment 2 / ENG-64: a scene whose detached component is too large for
/// even a compressed inline op blob. An anchored floor holds a `40³`
/// hash-patterned `STONE`/`DIRT` block through one column; cutting the column
/// detaches the whole block, whose compressed `BaselineVolume` exceeds
/// `MAX_SPLIT_BASELINE_BLOB`, forcing the bulk-stream baseline commit path. See
/// [`spall_voxel::fixtures::bulk_split_scene`].
pub fn bulk_split_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::bulk_split_scene(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 63, 63)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: sim_physics_config(),
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
        physics: sim_physics_config(),
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

/// The interactive playground (`cargo xtask play --scene playground`):
/// [`spall_voxel::fixtures::playground_scene`]'s plaza / stairs / maze / jump
/// course, plus [`crate::playground::PlaygroundSpawner`]'s drop zones spread
/// across all four areas. Not a T23/G4 gate scene -- built for a hands-on
/// feel, not reproducible evidence.
pub fn playground_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::playground_scene(id),
        // Matches `playground_scene`'s own padded air envelope (see that
        // function's doc comment for why `y` needs the `[-20, 50]` padding).
        terrain_collider_region: (GlobalCell::new(0, -20, 0), GlobalCell::new(239, 50, 383)),
        materials: playground_manifest(),
        anchor: AnchorPlane::at(0),
        physics: sim_physics_config(),
    }
}

/// Feet spawn positions (metres) for [`playground_setup`] -- the plaza,
/// clear of the checkerboard's edges. `y = 1.0 m` matches the plaza floor's
/// top surface.
pub const PLAYGROUND_SPAWNS: [[f64; 3]; 4] = [
    [16.0, 1.0, 16.0],
    [16.0, 1.0, 18.0],
    [18.0, 1.0, 16.0],
    [18.0, 1.0, 18.0],
];

/// The T11a / ENG-62 G1 full-workload world
/// ([`spall_voxel::fixtures::g1_full_envelope_scene`]): the full
/// `64 x 32 x 64 m` gate envelope with real, resident, walkable terrain
/// (rolling-hill heightmap, dirt over stone) across its whole footprint, plus
/// a hollow tower/bridge spanning brick boundaries. The collider region tops
/// out at `y = 100` cells (`25 m`) — comfortably above the tallest structure
/// (the tower's roof at `y = 92`) — rather than the full `128`-cell envelope
/// height, since nothing above that has any geometry to collide with.
pub fn g1_full_envelope_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::g1_full_envelope_scene(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(255, 100, 255)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// Feet spawn positions (metres) for [`g1_full_envelope_setup`]: the four
/// corners of the envelope, each on the local terrain surface
/// ([`spall_voxel::fixtures::g1_surface_height`] at that column, times the
/// `0.25 m` cell size), clear of the tower/bridge footprint
/// (`x 24..=103`, `z 32..=47`).
pub const G1_WORKLOAD_SPAWNS: [[f64; 3]; 4] = [
    [2.0, 13.5, 2.0],
    [55.0, 13.25, 55.0],
    [2.0, 12.5, 55.0],
    [55.0, 14.25, 2.0],
];

/// A hollow body-local cube shell: `size` cells outer, `wall` cells of solid
/// stone, air core. The G1 gate's "moving hollow test volume" — used with
/// [`crate::world::SimWorld::spawn_body`] to stand up a body that free-falls
/// and settles, not terrain.
pub fn hollow_block(size: i64, wall: i64) -> impl FnOnce(VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&box_plan(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(size - 1, size - 1, size - 1),
            STONE,
        ))
        .expect("hollow test-volume outer shell edit");
        let lo = wall;
        let hi = size - 1 - wall;
        if lo <= hi {
            v.apply_edit(&box_plan(
                id,
                GlobalCell::new(lo, lo, lo),
                GlobalCell::new(hi, hi, hi),
                MaterialId::AIR,
            ))
            .expect("hollow test-volume interior edit");
        }
        v
    }
}

/// Spawns the G1 gate's "moving hollow test volume": an `8`-cell (`2 m`)
/// hollow stone cube, `1`-cell wall, a few metres above the tower's roof so it
/// free-falls onto the tower and rolls off — real, visible motion for the
/// destruction-capture evidence, distinct from the tower/bridge terrain.
pub fn spawn_g1_hollow_test_volume(
    world: &mut crate::world::SimWorld,
) -> Result<spall_core::EntityId, crate::world::WorldError> {
    // Drop point: above the tower roof (base 45 + height 48 = 93 cells =
    // 23.25 m), offset so the falling cube does not spawn embedded in the
    // tower's stone shell.
    world.spawn_body(
        hollow_block(8, 1),
        BodyPose::new(DQuat::IDENTITY, [7.0, 27.0, 9.0]),
        [0.0; 3],
        [0.3, 0.0, 0.2],
        2600.0,
        0,
    )
}

/// T21 / ENG-28 increment 4: [`walk_arena_setup`] with a small column-and-beam
/// in the player lane ([`spall_voxel::fixtures::sleep_wake_arena`]) — cutting
/// the column detaches the beam as a body a scripted client can walk away
/// from and back to, exercising the networked dormancy deactivate/reactivate
/// path end to end. Same collider region and spawns as [`walk_arena_setup`]:
/// the added geometry sits entirely inside the existing bounds.
pub fn sleep_wake_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::sleep_wake_arena(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(119, 13, 15)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: sim_physics_config(),
    }
}

/// The T23 / G3 integrated-acceptance world
/// ([`spall_voxel::fixtures::separated_regions_scene`]): two independent
/// collapsible bridge structures in one bounded `256 x 128 x 256 m` world, a
/// "west" region at the origin and an "east" region offset by
/// [`spall_voxel::fixtures::SEPARATED_REGIONS_EAST_OFFSET`]. Cutting one region's
/// column detaches only that region's beam, so the harness can drive a
/// multi-region collapse with geographically separated players
/// ([`SEPARATED_REGION_SPAWNS`] puts alternating slots in the two regions).
pub fn separated_regions_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::separated_regions_scene(id),
        // Union box over both regions; only the two structures are resident, so
        // the collider grid is bounded by that, not the full world envelope.
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(95, 19, 79)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// Feet spawn positions (metres) for [`separated_regions_setup`]; index is the
/// player / connection slot. Even slots stand in the west region, odd slots in
/// the east region (offset `+18 m` on `x` and `z`), so connected players start
/// geographically separated. Floor top is `y = 1.0 m`; all positions clear the
/// column footprint.
pub const SEPARATED_REGION_SPAWNS: [[f64; 3]; 4] = [
    [1.0, 1.0, 1.0],
    [18.5, 1.0, 18.5],
    [2.0, 1.0, 1.5],
    [19.25, 1.0, 18.0],
];

/// The T23 / G3 **full-envelope** integrated-acceptance world
/// ([`spall_voxel::fixtures::separated_regions_full_envelope_scene`]): the same
/// two independent collapsible bridge structures as
/// [`separated_regions_setup`], but the east region is offset `110 m` (`> 100
/// m`) from the west region along `x` alone, connected by a continuous stone
/// causeway so a scripted player can walk the whole distance. Used to drive
/// genuinely-separated players and a scripted region-to-region traversal
/// through the multi-process harness (open G3 item, row 2).
pub fn separated_regions_full_envelope_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    let e = spall_voxel::fixtures::SEPARATED_REGIONS_FAR_EAST_OFFSET;
    WorldSetup {
        terrain: spall_voxel::fixtures::separated_regions_full_envelope_scene(id),
        // Union box over both regions and the connecting causeway; only this
        // corridor is resident, so the collider grid stays bounded by it, not
        // the full 256 x 128 x 256 m world envelope.
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(23 + e.x, 19, 7)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// Feet spawn positions (metres) for [`separated_regions_full_envelope_setup`];
/// index is the player / connection slot. Even slots stand in the west region;
/// odd slots stand in the east region, `110 m` away on `x` alone (the region's
/// own local layout is otherwise identical). Floor top is `y = 1.0 m`; all
/// positions clear the column footprint.
///
/// Slot 0 (the scripted mover, [`fixtures/scenarios/t23-g3-full-envelope.json`])
/// spawns at `x = 7.0 m` rather than the `x = 1.0 m` [`SEPARATED_REGION_SPAWNS`]
/// uses — **not** the same local offset. A fresh authoritative player capsule
/// spawned within roughly the first few metres of `x = 0` on *any* of this
/// crate's bounded-volume fixtures (reproduced on the already-merged, unrelated
/// [`separated_regions_setup`] too, with no terrain edit involved) does not
/// respond to horizontal input for several hundred ticks after creation — a
/// pre-existing defect in the shared T19 kinematic-character / collider-query
/// path, not something this scene introduces, and out of scope to fix here.
/// Spawning past that band (empirically, `x >= ~6.5 m`) sidesteps it cleanly;
/// slot 2 (stationary, no script) is left at its original offset since a
/// player that never receives non-neutral input is unaffected either way.
pub const SEPARATED_REGION_FAR_SPAWNS: [[f64; 3]; 4] = [
    [7.0, 1.0, 1.0],
    [111.0, 1.0, 1.0],
    [2.0, 1.0, 1.5],
    [112.0, 1.0, 1.5],
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
        physics: sim_physics_config(),
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
        physics: sim_physics_config(),
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

/// Feet spawn positions (metres) for the T23 / G4 eight-client workload
/// ([`g4_workload_setup`]): two four-player clusters sharing the west/east
/// regions' floors from [`separated_regions_setup`]. Each cluster is
/// **clustered** (its four players within ~1.4 m of each other, exercising
/// dense interest-management overlap); the two clusters are **separated** by
/// the region offset (`>=18 m`, the same floor `SEPARATED_REGION_SPAWNS`
/// uses). Full-envelope (`>100 m`) player separation is a distinct open item
/// (G3.md row 2) this increment does not depend on.
pub const G4_WORKLOAD_SPAWNS: [[f64; 3]; 8] = [
    // West cluster.
    [1.0, 1.0, 1.0],
    [2.0, 1.0, 1.0],
    [1.0, 1.0, 2.0],
    [2.0, 1.0, 2.0],
    // East cluster (region offset applied: +18 m on x and z).
    [19.0, 1.0, 19.0],
    [20.0, 1.0, 19.0],
    [19.0, 1.0, 20.0],
    [20.0, 1.0, 20.0],
];

/// T23 / G4 workload (row 12) debris-landing floor, cell bounds. A stone slab
/// well clear of [`separated_regions_scene`](spall_voxel::fixtures::separated_regions_scene)'s
/// west and east regions *and* of the near-observer body cluster
/// (`spawn_g4_workload_bodies` requires the near-observer sub-count to stay
/// exactly 64 -- `G4_NEAR_OBSERVER_RADIUS_M`, `12 m`, from
/// `G4_WORKLOAD_SPAWNS[0]` -- so this floor's nearest point must clear that
/// radius with real margin). Added only by [`g4_workload_setup`]; it never
/// touches `separated_regions_scene` itself, so every G3 fixture/scenario
/// hash pinned against that scene (`fixture_digests_are_pinned`, the agreed
/// `t23-g3*` hashes) stays exactly unchanged.
///
/// Gives the workload's non-near-observer active debris
/// (`spawn_g4_workload_bodies`) real ground to land on and accumulate as
/// rubble, inside the declared `256 x 128 x 256 m` envelope, instead of
/// free-falling in open space hundreds of metres outside it (2026-09-18
/// acceptance audit, finding 4: "the active debris falls into empty space
/// ... despite the 256 m terrain envelope").
///
/// `separated_regions_scene`'s own resident air envelope only spans
/// `x [0,95]` cells (`24 m`) -- not wide enough to place a floor `> 12 m`
/// from the west observer *and* clear of the east region without the two
/// clearances eating each other's margin in such a small box. So
/// [`g4_workload_setup`] first extends the resident air envelope, air-filling
/// only the *new* `x [96,255]` territory (`z`/`y` unchanged) so it never
/// re-writes -- and so never erases -- `separated_regions_scene`'s own
/// already-solid `x [0,95]` range, before adding this floor at
/// `x [160,223]` -- comfortably far down that extension (`~40 m` from the
/// west observer, `~15 m` clear of the east region's own `x` extent) and
/// still entirely within `z [8, 71]` cells (`2-17.75 m`, clear of *both*
/// regions regardless of `x`, since west only has solid cells at `z <= 7`
/// and east only at `z >= 72`). The added envelope is one contiguous
/// extension of the existing box (not a second disjoint region), so it costs
/// one more (large but uniform, therefore cheap) `EditPlan::filled_box` air
/// fill, not the giant per-cell walk that keeps the separate 64-brick
/// collapse fixture (`giant_collapse_setup`) out of this scene.
const G4_DEBRIS_ENVELOPE_MAX: GlobalCell = GlobalCell::new(255, 19, 79);
const G4_DEBRIS_FLOOR_MIN: GlobalCell = GlobalCell::new(160, 0, 8);
const G4_DEBRIS_FLOOR_MAX: GlobalCell = GlobalCell::new(223, 3, 71);

/// World-space (metres) centre of [`G4_DEBRIS_FLOOR_MIN`]..=[`G4_DEBRIS_FLOOR_MAX`]'s
/// top surface -- where [`spawn_g4_workload_bodies`] drops the
/// non-near-observer active population. `y = 1.0 m` matches the west/east
/// floors' own top-surface height (`separated_regions_scene`'s doc comment).
const G4_DEBRIS_DROP_CENTRE: [f64; 3] = [47.875, 1.0, 9.875];

/// A second, small stone floor dedicated to the near-observer active
/// population -- its own dead ground, not shared with the tiny, already
/// structure-filled west region floor (`~5.75 x 1.75 m`, holding the west
/// column and beam too) or with the player capsule spawned at `observer`
/// itself. Sits at `z [12, 43]` cells (`3.0-10.75 m`) -- clear of the west
/// region's own solid cells, all of which are at `z <= 7` cells (`1.75 m`,
/// `separated_regions_scene`'s doc comment) -- and entirely inside the
/// resident air envelope that scene already fills (`x [0,95]`, `z [0,79]`,
/// `y [0,19]` cells), so (like [`G4_DEBRIS_FLOOR_MIN`]) it costs one more
/// small, cheap `EditPlan::filled_box` and no envelope extension.
const G4_NEAR_FLOOR_MIN: GlobalCell = GlobalCell::new(0, 0, 12);
const G4_NEAR_FLOOR_MAX: GlobalCell = GlobalCell::new(31, 3, 43);

/// World-space (metres) centre of [`G4_NEAR_FLOOR_MIN`]..=[`G4_NEAR_FLOOR_MAX`]'s
/// top surface -- where [`spawn_g4_workload_bodies`] drops the near-observer
/// population. `~6.5 m` from `observer` -- comfortably inside
/// `G4_NEAR_OBSERVER_RADIUS_M` (`12 m`) even at the flat grid's farthest
/// corner (`~11.3 m`) -- and `y = 1.0 m` again matches the west/east floors'
/// own top-surface height.
const G4_NEAR_DROP_CENTRE: [f64; 3] = [3.875, 1.0, 6.875];

/// The T23 / G4 eight-client workload world (row 12): [`separated_regions_setup`]'s
/// terrain -- two independent collapsible bridge structures in one bounded
/// `256 x 128 x 256 m` world -- plus an extended resident air envelope and
/// one additional stone floor ([`G4_DEBRIS_FLOOR_MIN`]..=[`G4_DEBRIS_FLOOR_MAX`])
/// for the workload's active debris population to land on
/// ([`G4_DEBRIS_ENVELOPE_MAX`]'s doc comment explains why the floor cannot
/// simply reuse `separated_regions_scene`'s existing envelope as-is). The
/// workload's body population ([`spawn_g4_workload_bodies`]) is added
/// separately after the [`Simulation`] is constructed: bodies are
/// independent volumes placed by world-space transform, not terrain cells,
/// so most of them need no additional terrain residency at all -- only the
/// near-observer and remaining-active populations, which are actually
/// simulated (not immediately dormant), need real ground under them to
/// demonstrate real accumulation rather than an indefinite freefall.
///
/// [`Simulation`]: crate::Simulation
pub fn g4_workload_setup() -> WorldSetup {
    let mut setup = separated_regions_setup();
    let id = setup.terrain.id();
    // The collider region must grow to cover the new debris floor too, or
    // its solid cells would be resident terrain with no built collider at
    // all -- debris would fall straight through it.
    setup.terrain_collider_region.1 = G4_DEBRIS_ENVELOPE_MAX;
    // Extend into the *new* x territory only (`96..=255`) -- re-filling
    // `separated_regions_scene`'s own `x [0,95]` range with air here would
    // run after (and so silently erase) the solid west/east geometry that
    // scene already wrote.
    setup
        .terrain
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(96, 0, 0),
            G4_DEBRIS_ENVELOPE_MAX,
            MaterialId::AIR,
        ))
        .expect("g4 workload debris envelope extension");
    setup
        .terrain
        .apply_edit(&EditPlan::filled_box(
            id,
            G4_DEBRIS_FLOOR_MIN,
            G4_DEBRIS_FLOOR_MAX,
            STONE,
        ))
        .expect("g4 workload debris floor stays inside the extended envelope");
    setup
        .terrain
        .apply_edit(&EditPlan::filled_box(
            id,
            G4_NEAR_FLOOR_MIN,
            G4_NEAR_FLOOR_MAX,
            STONE,
        ))
        .expect("g4 workload near-observer floor stays inside the already-resident envelope");
    setup
}

/// Fixture debris density (kg/m³) for every [`spawn_g4_workload_bodies`] body —
/// the same stone density as the rest of this module's fixtures.
const G4_BODY_DENSITY_KG_M3: f32 = 2600.0;

/// Total **active** (awake) debris bodies required by the G4 workload, row 12:
/// "256 active bodies (64 near one observer)".
pub const G4_ACTIVE_BODY_COUNT: usize = 256;
/// Of the active bodies, how many sit within [`G4_NEAR_OBSERVER_RADIUS_M`] of
/// the observer position.
pub const G4_NEAR_OBSERVER_BODY_COUNT: usize = 64;
/// "Near one observer" radius (metres) the 64-body sub-cluster is built
/// within. A chosen interest-adjacent distance for this fixture; the actual
/// T20 per-connection interest radius is a separate, unwired CLI knob (G3.md
/// row 14 stays open).
pub const G4_NEAR_OBSERVER_RADIUS_M: f64 = 12.0;
/// **Sleeping** (dormant, persisted) debris population required by row 12.
pub const G4_SLEEPING_BODY_COUNT: usize = 4096;

/// What [`spawn_g4_workload_bodies`] actually built — returned so a caller
/// (the server scene, CPU tests) can assert on the real counts rather than
/// just the requested ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct G4BodyCounts {
    /// Awake bodies spawned (never deactivated).
    pub active_total: usize,
    /// Of `active_total`, how many were placed within the near-observer
    /// radius.
    pub active_near_observer: usize,
    /// Bodies spawned then immediately deactivated (T21 dormancy): they exist
    /// and are persisted, but carry no physics-step cost.
    pub sleeping_total: usize,
}

/// Populates the T23 / G4 workload's debris population (row 12: "256 active
/// bodies (64 near one observer), 4096 sleeping persistent bodies") on an
/// already-constructed [`crate::world::SimWorld`]. `observer` must be
/// [`G4_WORKLOAD_SPAWNS`]`[0]` -- every position this function drops debris
/// at (including the near-observer cluster's own dedicated floor,
/// [`G4_NEAR_FLOOR_MIN`]..=[`G4_NEAR_FLOOR_MAX`]) is terrain
/// [`g4_workload_setup`] bakes in for exactly that spawn point; a debug
/// assertion catches any other value immediately rather than silently
/// dropping debris on the wrong (or no) floor.
///
/// Every body is a small `2x2x2`-cell (8-cell) solid stone cube
/// ([`solid_block`]) — deliberately minimal but genuinely multi-cell,
/// collidable geometry (`docs/validation.md`: "Geometry fixtures must specify
/// occupied cells and collider complexity, not only body count"), so spawning
/// ~4.3k of them stays inside CPU-CI cost.
///
/// The 64 near-observer bodies drop as a flat `8x8 m`, single-layer grid
/// (`1 m` spacing) onto their own dedicated floor
/// ([`G4_NEAR_FLOOR_MIN`]..=[`G4_NEAR_FLOOR_MAX`], centred under
/// [`G4_NEAR_DROP_CENTRE`]) -- **not** the tiny, already structure-filled
/// west region floor `observer` itself stands on (2026-09-18 acceptance
/// audit, finding 4 discovery; see that floor's own doc comment for the
/// chain of narrower attempts this replaced and why each one failed). A flat
/// single layer, each body its own 1 m cell with nothing else falling
/// through the same column, avoids the body-on-body pileup instability a
/// compact multi-layer stack of identical same-size cubes turned out to
/// have: even once every stack shape cleared the beam/column/player, bodies
/// still occasionally launched off the world from contacts with their own
/// close-packed neighbours (`disable_ccd = true`, this project's fixed
/// physics config, does not stop a high-enough-energy contact from
/// tunnelling a fast body through thin terrain in one step). The remaining active
/// population drops onto
/// [`G4_DEBRIS_FLOOR_MIN`]..=[`G4_DEBRIS_FLOOR_MAX`] (2026-09-18 acceptance
/// audit, finding 4: real ground to land on and accumulate as rubble,
/// instead of free-falling forever hundreds of metres outside the declared
/// envelope), well clear of both terrain regions and of the near-observer
/// cluster so nothing starts overlapping; under gravity it remains part of
/// the physics step for the whole run, which *is* "active" for a proof run
/// of this length. Every sleeping body sits in its own field, also inside
/// the `256 x 128 x 256 m` envelope -- spawned then immediately
/// [`crate::world::SimWorld::deactivate_body`]d (T21 dormancy) before any
/// physics step ever runs, so a dormant body carries zero physics-step cost
/// (and needs no floor: it is never simulated falling) while its
/// authoritative record stays resident and persisted, exactly matching
/// "sleeping (dormant but persistent)".
pub fn spawn_g4_workload_bodies(
    world: &mut crate::world::SimWorld,
    observer: [f64; 3],
) -> G4BodyCounts {
    // G4_NEAR_FLOOR_MIN/MAX (and every other absolute position this function
    // uses) is terrain `g4_workload_setup` bakes in for exactly
    // G4_WORKLOAD_SPAWNS[0] -- this is the only `observer` value any of it
    // actually lands correctly for. `observer` stays a parameter (documented
    // as "typically G4_WORKLOAD_SPAWNS[0]") so a future caller intending a
    // genuinely different position gets a clear, immediate failure here
    // instead of debris silently landing on the wrong (or no) floor.
    debug_assert_eq!(
        observer, G4_WORKLOAD_SPAWNS[0],
        "spawn_g4_workload_bodies' fixed drop positions are only real ground for \
         G4_WORKLOAD_SPAWNS[0] -- g4_workload_setup would need its own floor changes first \
         to support a different observer"
    );

    let mut counts = G4BodyCounts::default();

    // 64 active bodies on a flat 8x8 grid at 1 m spacing, centred on
    // G4_NEAR_DROP_CENTRE (the dedicated near-observer floor -- not
    // `observer` itself; see this function's doc comment), elevated so they
    // drop a real distance without starting inside the floor.
    let near_side = 8usize;
    debug_assert_eq!(near_side * near_side, G4_NEAR_OBSERVER_BODY_COUNT);
    for i in 0..near_side {
        for j in 0..near_side {
            let x = G4_NEAR_DROP_CENTRE[0] + (i as f64 - near_side as f64 / 2.0 + 0.5) * 1.0;
            let z = G4_NEAR_DROP_CENTRE[2] + (j as f64 - near_side as f64 / 2.0 + 0.5) * 1.0;
            let y = G4_NEAR_DROP_CENTRE[1] + 3.0;
            spawn_one(
                world,
                [x, y, z],
                BodyKindWanted::ActiveNearObserver,
                &mut counts,
            );
        }
    }

    // The rest of the active population: dropped from a modest height onto
    // the dedicated debris floor, centred so its footprint stays well inside
    // that floor's bounds (row 12 finding 4).
    let remaining_active = G4_ACTIVE_BODY_COUNT - counts.active_near_observer;
    spawn_grid(
        world,
        remaining_active,
        [
            G4_DEBRIS_DROP_CENTRE[0],
            G4_DEBRIS_DROP_CENTRE[1] + 3.0,
            G4_DEBRIS_DROP_CENTRE[2],
        ],
        BodyKindWanted::Active,
        &mut counts,
    );

    // Every sleeping body: a separate field, still inside the declared
    // envelope but well clear of everything else. No floor needed -- a
    // dormant body is deactivated before its first physics step, so it is
    // never actually simulated falling.
    spawn_grid(
        world,
        G4_SLEEPING_BODY_COUNT,
        [150.0, 3.0, 150.0],
        BodyKindWanted::Sleeping,
        &mut counts,
    );

    counts
}

/// Which counter [`spawn_one`] should bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKindWanted {
    /// Awake, and counted toward the near-observer sub-cluster.
    ActiveNearObserver,
    /// Awake, not near the observer.
    Active,
    /// Spawned then immediately deactivated (T21 dormancy).
    Sleeping,
}

/// Places `count` bodies on a flat `x`/`z` grid at 1 m spacing, centred on
/// `origin` (unlike a from-corner grid, this keeps the footprint's extent
/// symmetric around a caller-chosen point regardless of `count`, which is
/// what lets [`spawn_g4_workload_bodies`] centre the active-debris grid
/// inside [`G4_DEBRIS_FLOOR_MIN`]..=[`G4_DEBRIS_FLOOR_MAX`] without
/// duplicating this function's own sizing formula at the call site).
fn spawn_grid(
    world: &mut crate::world::SimWorld,
    count: usize,
    origin: [f64; 3],
    kind: BodyKindWanted,
    counts: &mut G4BodyCounts,
) {
    if count == 0 {
        return;
    }
    let side = (count as f64).sqrt().ceil() as usize + 1;
    let spacing = 1.0;
    let half = side as f64 / 2.0;
    let mut placed = 0usize;
    'outer: for i in 0..side {
        for j in 0..side {
            if placed >= count {
                break 'outer;
            }
            let x = origin[0] + (i as f64 - half) * spacing;
            let z = origin[2] + (j as f64 - half) * spacing;
            let y = origin[1];
            spawn_one(world, [x, y, z], kind, counts);
            placed += 1;
        }
    }
    debug_assert_eq!(placed, count);
}

fn spawn_one(
    world: &mut crate::world::SimWorld,
    at: [f64; 3],
    kind: BodyKindWanted,
    counts: &mut G4BodyCounts,
) {
    let entity = world
        .spawn_body(
            solid_block(2),
            BodyPose::new(DQuat::IDENTITY, at),
            [0.0; 3],
            [0.0; 3],
            G4_BODY_DENSITY_KG_M3,
            0,
        )
        .expect("g4 workload debris body spawns");
    match kind {
        BodyKindWanted::ActiveNearObserver => {
            counts.active_total += 1;
            counts.active_near_observer += 1;
        }
        BodyKindWanted::Active => {
            counts.active_total += 1;
        }
        BodyKindWanted::Sleeping => {
            let deactivated = world.deactivate_body(entity);
            debug_assert!(deactivated, "a freshly spawned body always deactivates");
            counts.sleeping_total += 1;
        }
    }
}

/// T23 / G4 row 12's "one 64-brick connected collapse" attempted in isolation:
/// [`spall_voxel::fixtures::giant_collapse_scene`] holds the anchored floor,
/// column, and 64-brick block on its own — not merged into
/// [`g4_workload_setup`]'s world, because a terrain split's resident-air
/// envelope must be contiguous across everything else resident in the same
/// volume (`separated_regions_scene`'s doc comment), and placing a
/// 128-cell-per-axis structure far enough from the west/east regions to avoid
/// spatial overlap would force a multi-hundred-million-cell resident air fill
/// via the per-cell `EditPlan` path — infeasible to build at all, let alone in
/// CPU CI. Isolating the attempt makes its real outcome (commit or a specific,
/// evidenced failure) unambiguous and keeps the attempt reproducible on its
/// own.
pub fn giant_collapse_setup() -> WorldSetup {
    let id = VolumeId::new(1).unwrap();
    WorldSetup {
        terrain: spall_voxel::fixtures::giant_collapse_scene(id),
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(127, 159, 127)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }
}

/// A 45° rotation about an oblique axis, as a `DQuat`.
pub fn oblique_spin() -> DQuat {
    DQuat::from_axis_angle(
        DVec3::new(1.0, 2.0, -0.5).normalize(),
        std::f64::consts::FRAC_PI_4,
    )
}
