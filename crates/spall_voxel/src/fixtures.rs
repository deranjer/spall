//! Deterministic, CPU-cheap fixture volumes for the foundation tests and the
//! later geometry / structure / physics tasks (T05–T07) that build on them.
//!
//! Every fixture is a pure function of its `VolumeId` — no RNG, no wall-clock,
//! no I/O — so [`digest`] gives a stable content hash that pins the fixture
//! against accidental change. That digest is a **local regression check**, not
//! the replicated topology hash (that is `spall_protocol`'s
//! `spall.topology.v1`).
//!
//! Fixture material ids are arbitrary small integers, not a real manifest:
//! [`STONE`] and [`DIRT`]. Air is [`spall_core::MaterialId::AIR`].

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};

use crate::brick::{Brick, BrickHash};
use crate::edit::EditPlan;
use crate::volume::{BrickBounds, Volume};

/// Fixture stand-in for solid rock.
pub const STONE: MaterialId = MaterialId(1);
/// Fixture stand-in for a soil surface layer.
pub const DIRT: MaterialId = MaterialId(2);

const DIGEST_DOMAIN: &[u8] = b"spall.voxel.fixture.v1";

/// A 2x2x2-brick bounded terrain volume: a solid stone slab (brick layer
/// `y = 0`, cells `y` in `0..32`) under a resident, empty air layer (brick
/// layer `y = 1`). Everything is uniform storage, so this costs almost nothing.
///
/// The air layer is *resident* (not merely absent) so a ray fired from above
/// the slab crosses `Empty` cells and strikes the surface at global `y = 32`
/// rather than reporting `Unknown`.
pub fn flat_terrain(id: VolumeId) -> Volume {
    let bounds =
        BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 1, 1)).expect("valid bounds");
    let mut v = Volume::bounded(id, CellSizeCode::Quarter, bounds);
    for x in 0..=1 {
        for z in 0..=1 {
            v.insert_brick(BrickCoord::new(x, 0, z), Brick::uniform(STONE, Revision(1)))
                .expect("in bounds");
            v.insert_brick(
                BrickCoord::new(x, 1, z),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .expect("in bounds");
        }
    }
    v
}

/// A hollow stone tower centred on the world origin: a solid outer box
/// `x,z in -6..=5`, `y in -8..=23` with a 2-cell-thick shell (interior void
/// `x,z in -4..=3`, `y in -6..=21`). It straddles the corner where eight bricks
/// meet, so it exercises cross-brick geometry and negative coordinates.
pub fn hollow_tower(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    let shell = EditPlan::filled_box(
        id,
        GlobalCell::new(-6, -8, -6),
        GlobalCell::new(5, 23, 5),
        STONE,
    );
    v.apply_edit(&shell).expect("shell edit");
    let hollow = EditPlan::filled_box(
        id,
        GlobalCell::new(-4, -6, -4),
        GlobalCell::new(3, 21, 3),
        MaterialId::AIR,
    );
    v.apply_edit(&hollow).expect("hollow edit");
    v
}

/// A 2x2-cell stone beam running along X from cell `-40` to `39` at
/// `y in 0..=1`, `z in 0..=1`. It crosses four bricks on the X axis
/// (`x` brick `-2..=1`), matching the `bridge-cross-brick` acceptance fixture.
pub fn cross_brick_bridge(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    let beam = EditPlan::filled_box(
        id,
        GlobalCell::new(-40, 0, 0),
        GlobalCell::new(39, 1, 1),
        STONE,
    );
    v.apply_edit(&beam).expect("beam edit");
    v
}

/// A small excavatable staircase slope inside a single brick: for `x in 0..16`,
/// stone fills `y in 0..x` with a `DIRT` surface cell at `y = x`, over
/// `z in 0..16`.
pub fn sloped_terrain(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    let mut plan = EditPlan::new(id);
    for x in 0..16 {
        for z in 0..16 {
            for y in 0..x {
                plan.set(GlobalCell::new(x, y, z), STONE);
            }
            plan.set(GlobalCell::new(x, x, z), DIRT);
        }
    }
    v.apply_edit(&plan).expect("slope edit");
    v
}

/// The T10 replication scene: an anchored stone floor, a single slender column,
/// and a raised beam the column alone holds up, plus a resident air layer above.
/// Cutting the column detaches the beam as exactly one unsupported component —
/// the terrain-to-body transfer the replication tests exercise. Shared verbatim
/// by the authoritative server (`spall_sim`) and the client replica baseline
/// (`spall_client`) so both start from an identical volume.
///
/// - floor:  `x 0..=23`, `z 0..=3`,  `y 0..=1`  (anchored at `y = 0`)
/// - column: `x 10..=11`, `z 1..=2`, `y 2..=7`
/// - beam:   `x 4..=20`, `z 1..=2`,  `y 8..=9`
/// - air:    `x 0..=23`, `z 0..=3`,  `y 10..=15` (resident, not merely absent)
pub fn bridge_scene(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    for (a, b, m) in [
        (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 1, 3), STONE),
        (GlobalCell::new(10, 2, 1), GlobalCell::new(11, 7, 2), STONE),
        (GlobalCell::new(4, 8, 1), GlobalCell::new(20, 9, 2), STONE),
        (
            GlobalCell::new(0, 10, 0),
            GlobalCell::new(23, 15, 3),
            MaterialId::AIR,
        ),
    ] {
        v.apply_edit(&EditPlan::filled_box(id, a, b, m))
            .expect("bridge scene edit");
    }
    v
}

/// A replication scene whose detached component is deliberately **too
/// fragmented to encode inline** (T17 / ENG-64): an anchored floor, one slender
/// column, and a raised `24 x 24 x 24` cell block whose material alternates
/// `STONE` / `DIRT` per cell (`(x + y + z)` parity). Cutting the column detaches
/// the whole checkerboard block as one body; its `13 824` single-cell canonical
/// runs blow the inline `CellRun` budget, so the commit falls back to the
/// compressed-baseline-blob op path.
///
/// - floor:  `x 0..=31`, `z 0..=31`, `y 0..=1`   (anchored at `y = 0`)
/// - column: `x 15..=16`, `z 15..=16`, `y 2..=7`
/// - block:  `x 4..=27`, `z 4..=27`, `y 8..=31`  (checkerboard STONE/DIRT)
/// - air:    `x 0..=31`, `z 0..=31`, `y 2..=31`  (resident; solids written over it)
pub fn checkerboard_split_scene(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    // Resident air envelope first, so the solid writes below win.
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 2, 0),
        GlobalCell::new(31, 31, 31),
        MaterialId::AIR,
    ))
    .expect("air envelope edit");
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(31, 1, 31),
        STONE,
    ))
    .expect("floor edit");
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(15, 2, 15),
        GlobalCell::new(16, 7, 16),
        STONE,
    ))
    .expect("column edit");

    let mut block = EditPlan::new(id);
    for z in 4..=27 {
        for y in 8..=31 {
            for x in 4..=27 {
                let m = if (x + y + z) % 2 == 0 { STONE } else { DIRT };
                block.set(GlobalCell::new(x, y, z), m);
            }
        }
    }
    v.apply_edit(&block).expect("checkerboard block edit");
    v
}

/// A replication scene whose detached component's geometry is too large for
/// even a **compressed inline** op blob (T17 increment 2 / ENG-64): an anchored
/// floor, one column, and a raised `54 x 46 x 54` cell block whose material is
/// `STONE` / `DIRT` by a splitmix64 hash — a per-cell coin flip, so the
/// material layer is genuinely incompressible. Cutting the column detaches the
/// whole block as one body; its compressed `BaselineVolume` exceeds
/// `MAX_SPLIT_BASELINE_BLOB` (28 KiB), so the commit ships it out of band as a
/// bulk `BaselineWorld` on a stream.
///
/// - floor:  `x 0..=63`, `z 0..=63`, `y 0..=1`   (anchored at `y = 0`)
/// - column: `x 28..=35`, `z 28..=35`, `y 2..=13`
/// - block:  `x 6..=59`, `z 6..=59`, `y 14..=59`  (hash-patterned STONE/DIRT)
pub fn bulk_split_scene(id: VolumeId) -> Volume {
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 1, 1))
        .expect("valid bulk-split bounds");
    let mut v = Volume::bounded(id, CellSizeCode::Quarter, bounds);
    for z in 0..=1 {
        for y in 0..=1 {
            for x in 0..=1 {
                v.insert_brick(
                    BrickCoord::new(x, y, z),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .expect("in bounds");
            }
        }
    }
    let mut plan = EditPlan::new(id);
    for z in 0..=63 {
        for x in 0..=63 {
            plan.set(GlobalCell::new(x, 0, z), STONE);
            plan.set(GlobalCell::new(x, 1, z), STONE);
        }
    }
    for z in 28..=35 {
        for y in 2..=13 {
            for x in 28..=35 {
                plan.set(GlobalCell::new(x, y, z), STONE);
            }
        }
    }
    for z in 6..=59 {
        for y in 14..=59 {
            for x in 6..=59 {
                // splitmix64 finalizer → `h & 1` is a per-cell coin flip, so the
                // block's material layer is genuinely incompressible and the
                // child blob overruns the inline op cap.
                let mut h = (x as u64)
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .wrapping_add((y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
                    .wrapping_add((z as u64).wrapping_mul(0x1656_67B1_9E37_79F9));
                h ^= h >> 30;
                h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                h ^= h >> 27;
                h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
                h ^= h >> 31;
                let m = if h & 1 == 0 { STONE } else { DIRT };
                plan.set(GlobalCell::new(x, y, z), m);
            }
        }
    }
    v.apply_edit(&plan).expect("bulk-split scene edit");
    v
}

/// Like [`bridge_scene`], but shifted and widened so the load-bearing geometry
/// **crosses the `x = 32` brick boundary**. A single column straddling the seam
/// holds up a beam that also spans both bricks; cutting the column must detach
/// the beam as exactly one unsupported component whose cells were owned across
/// two bricks. This is the cross-brick structural-support / brick-boundary
/// ownership-transfer case the single-brick `bridge_scene` cannot show.
///
/// - floor:  `x 20..=44`, `z 0..=3`,  `y 0..=1`   (anchored at `y = 0`; bricks x0+x1)
/// - column: `x 31..=32`, `z 1..=2`,  `y 2..=6`   (straddles the `x = 32` seam)
/// - beam:   `x 22..=42`, `z 1..=2`,  `y 7..=8`   (held only by the column; bricks x0+x1)
/// - air:    `x 20..=44`, `z 0..=3`,  `y 9..=15`  (resident, not merely absent)
pub fn cross_brick_bridge_scene(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    for (a, b, m) in [
        (GlobalCell::new(20, 0, 0), GlobalCell::new(44, 1, 3), STONE),
        (GlobalCell::new(31, 2, 1), GlobalCell::new(32, 6, 2), STONE),
        (GlobalCell::new(22, 7, 1), GlobalCell::new(42, 8, 2), STONE),
        (
            GlobalCell::new(20, 9, 0),
            GlobalCell::new(44, 15, 3),
            MaterialId::AIR,
        ),
    ] {
        v.apply_edit(&EditPlan::filled_box(id, a, b, m))
            .expect("cross-brick bridge scene edit");
    }
    v
}

/// The T19 player-movement arena: a flat anchored stone floor with a low step
/// ledge and a resident air ceiling, sized for CPU CI.
///
/// - floor: `x 0..=119`, `z 0..=15`, `y 0..=3` — top surface at `y = 1.0 m`,
///   anchored at `y = 0`; the lane is 30 m long.
/// - step: `x 88..=119`, `z 0..=15`, `y 4..=5` — a 0.5 m ledge at `x = 22 m`
///   for the autostep / grounding case.
/// - air: `x 0..=119`, `z 0..=15`, `y 6..=13` — resident, not merely absent.
///
/// Scripted players spawn on the floor near `x = 0` and walk `+X`; a scenario
/// cut that removes a run of floor cells under a player exercises "removing a
/// floor during replay cannot leave the player hovering".
pub fn walk_arena(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    for (a, b, m) in [
        (GlobalCell::new(0, 0, 0), GlobalCell::new(119, 3, 15), STONE),
        (
            GlobalCell::new(88, 4, 0),
            GlobalCell::new(119, 5, 15),
            STONE,
        ),
        (
            GlobalCell::new(0, 6, 0),
            GlobalCell::new(119, 13, 15),
            MaterialId::AIR,
        ),
    ] {
        v.apply_edit(&EditPlan::filled_box(id, a, b, m))
            .expect("walk arena edit");
    }
    v
}

/// Cell offset from the west structure to the east structure in
/// [`separated_regions_scene`]: `+72` cells (`18 m`) on both `x` and `z`, so the
/// two collapsible towers sit in distinct interest regions with clear ground
/// between them.
pub const SEPARATED_REGIONS_EAST_OFFSET: GlobalCell = GlobalCell::new(72, 0, 72);

/// The T23 / G3 integrated-acceptance scene: **two independent collapsible
/// structures** in a single bounded world, one near the world origin ("west")
/// and one offset by [`SEPARATED_REGIONS_EAST_OFFSET`] ("east"). Each is a
/// compact bridge — an anchored floor, one slender column, and a raised beam the
/// column alone holds up — so cutting either column detaches that region's beam
/// as one unsupported component without touching the other region. Used to drive
/// geographically separated players and a multi-region collapse through the
/// multi-process harness.
///
/// The volume is **bounded to `256 x 128 x 256 m`** (`32 x 16 x 32` bricks at
/// `CellSizeCode::Quarter`) — the G3 operating envelope — but only the two
/// regions are resident, so the structural analysis and collider rebuilds stay
/// CPU-cheap. Wider physical separation and forced resident-cache eviction are
/// tracked as open G3 items.
///
/// Per region (west shown; east is the same shifted by the offset):
/// - floor:  `x 0..=23`, `z 0..=7`,  `y 0..=3`  — top surface at `y = 1.0 m`,
///   anchored at `y = 0`
/// - column: `x 10..=11`, `z 3..=4`, `y 4..=9`
/// - beam:   `x 4..=20`, `z 3..=4`,  `y 10..=11`
/// - air:    `x 0..=23`, `z 0..=7`,  `y 4..=19` — resident, written first
pub fn separated_regions_scene(id: VolumeId) -> Volume {
    // 256 m / 0.25 m = 1024 cells = 32 bricks per horizontal axis; 128 m = 16
    // bricks vertically. Inclusive max brick coord is one less.
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(31, 15, 31))
        .expect("valid G3 world bounds");
    let mut v = Volume::bounded(id, CellSizeCode::Quarter, bounds);

    let e = SEPARATED_REGIONS_EAST_OFFSET;

    // One resident air envelope over both regions and the ground between them,
    // written first so the solid writes below win. The terrain occupancy grid is
    // extracted over the bounding box of the solid cells, and every brick in
    // that box must be resident — a gap of absent bricks between the two regions
    // would fail extraction.
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(23 + e.x, 19, 7 + e.z),
        MaterialId::AIR,
    ))
    .expect("separated-regions air envelope");

    let shift = |c: GlobalCell, by: GlobalCell| GlobalCell::new(c.x + by.x, c.y + by.y, c.z + by.z);
    let region = [
        (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 3, 7), STONE),
        (GlobalCell::new(10, 4, 3), GlobalCell::new(11, 9, 4), STONE),
        (GlobalCell::new(4, 10, 3), GlobalCell::new(20, 11, 4), STONE),
    ];
    for by in [GlobalCell::new(0, 0, 0), e] {
        for (a, b, m) in region {
            v.apply_edit(&EditPlan::filled_box(id, shift(a, by), shift(b, by), m))
                .expect("separated-regions scene edit");
        }
    }
    v
}

/// East-region offset for the full-envelope T23 scene: 110 m on the x axis.
pub const SEPARATED_REGIONS_FAR_EAST_OFFSET: GlobalCell = GlobalCell::new(440, 0, 0);

/// Two separated regions joined by a continuous, narrow stone causeway.
pub fn separated_regions_full_envelope_scene(id: VolumeId) -> Volume {
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(31, 15, 31))
        .expect("valid G3 world bounds");
    let mut v = Volume::bounded(id, CellSizeCode::Quarter, bounds);
    let e = SEPARATED_REGIONS_FAR_EAST_OFFSET;
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(23 + e.x, 19, 7),
        MaterialId::AIR,
    ))
    .expect("far-scene air envelope");
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(24, 0, 0),
        GlobalCell::new(e.x - 1, 3, 7),
        STONE,
    ))
    .expect("far-scene causeway");
    let region = [
        (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 3, 7), STONE),
        (GlobalCell::new(10, 4, 3), GlobalCell::new(11, 9, 4), STONE),
        (GlobalCell::new(4, 10, 3), GlobalCell::new(20, 11, 4), STONE),
    ];
    for offset in [GlobalCell::new(0, 0, 0), e] {
        for (a, b, material) in region {
            let shift = |c: GlobalCell| GlobalCell::new(c.x + offset.x, c.y, c.z + offset.z);
            v.apply_edit(&EditPlan::filled_box(id, shift(a), shift(b), material))
                .expect("far-scene region edit");
        }
    }
    v
}

/// Bricks per axis of [`giant_collapse_scene`]'s detachable block —
/// `4^3 = 64`, the literal G1/G4 "64-brick connected body" stress case
/// (`docs/collision-decision.md`, `docs/validation.md` G1/G4: "Include ... a
/// 64-brick connected body stress case").
pub const GIANT_COLLAPSE_BRICKS: i64 = 4;

/// An anchored floor + slender column holding up a **fully solid** block
/// spanning exactly `4 x 4 x 4 = 64` connected bricks. Cutting the column
/// detaches the whole block as one component whose bounding grid is the
/// literal 64-brick stress case.
///
/// The block is built by directly [`Volume::insert_brick`]-ing 64
/// `Brick::uniform` bricks rather than a `2 097 152`-cell `EditPlan` fill —
/// same resident content, but the fixture costs 64 brick insertions instead of
/// two million per-cell writes. The wire encoding of a uniform brick
/// (`spall_protocol::baseline::BaselineCells::Uniform`) is independently tiny,
/// regardless of this construction shortcut.
///
/// - floor:  `x 0..=7`, `z 0..=7`, `y 0..=1`   (anchored at `y = 0`)
/// - column: `x 3..=4`, `z 3..=4`, `y 2..=31`  (fills brick row `y = 0` up to
///   the block's brick-aligned base)
/// - block:  `x 0..=127`, `z 0..=127`, `y 32..=159` — brick coordinates
///   `(0..=3, 1..=4, 0..=3)`, exactly 64 bricks, every one solid `STONE`
///
/// The occupancy/collider extraction this fixture is built for requires every
/// brick in its bounding box to be resident (`separated_regions_scene`'s doc
/// comment carries the same rule) — the brick-row-`y = 0` layer around the
/// floor/column is therefore filled resident air first, directly by brick
/// (not a per-cell edit), before the floor and column are cut into it.
pub fn giant_collapse_scene(id: VolumeId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    for bx in 0..GIANT_COLLAPSE_BRICKS {
        for bz in 0..GIANT_COLLAPSE_BRICKS {
            v.insert_brick(
                BrickCoord::new(bx, 0, bz),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .expect("giant-collapse air envelope insert");
        }
    }
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(7, 1, 7),
        STONE,
    ))
    .expect("giant-collapse floor edit");
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(3, 2, 3),
        GlobalCell::new(4, 31, 4),
        STONE,
    ))
    .expect("giant-collapse column edit");
    for bx in 0..GIANT_COLLAPSE_BRICKS {
        for by in 0..GIANT_COLLAPSE_BRICKS {
            for bz in 0..GIANT_COLLAPSE_BRICKS {
                v.insert_brick(
                    BrickCoord::new(bx, 1 + by, bz),
                    Brick::uniform(STONE, Revision(1)),
                )
                .expect("giant-collapse block brick insert");
            }
        }
    }
    v
}

/// Bricks per axis of [`g1_full_envelope_scene`]'s world: `8 x 4 x 8` at
/// `CellSizeCode::Quarter` (`32` cells/brick, `0.25 m`/cell) is exactly the G1
/// gate's `64 x 32 x 64 m` envelope (`docs/validation.md` "G1").
pub const G1_ENVELOPE_BRICKS_X: i64 = 8;
pub const G1_ENVELOPE_BRICKS_Y: i64 = 4;
pub const G1_ENVELOPE_BRICKS_Z: i64 = 8;

/// Flat base surface height (global cell `y`) for
/// [`g1_full_envelope_scene`]'s terrain, outside [`g1_in_ramp`]'s footprint.
/// A continuously-varying heightmap (the first attempt at this fixture) built
/// a real terrain but made the greedy-box decomposition of its rolling
/// surface exceed [`crate::collider::PRIMITIVE_BUDGET`] (measured: 7137
/// boxes for a smooth two-axis sine blend, over the 4096 budget) — falling
/// back to the exact native-voxel collider, whose feasibility gate then
/// rejects the whole ~6 000 000-cell region outright
/// (`ColliderInfeasible::TooLarge`). A flat plain with one deliberate ramp
/// feature greedy-merges into a handful of large boxes instead, comfortably
/// inside budget, while still giving the gate's "excavatable slope" as a real
/// dig-into-the-hillside feature rather than a single bespoke single-brick
/// staircase (contrast [`sloped_terrain`]).
const G1_FLAT_HEIGHT: i64 = 46;

/// The excavatable slope: a linear ramp from [`G1_FLAT_HEIGHT`] down to `34`
/// cells over `32` cells (`8 m`) of `x`, `16` cells (`4 m`) wide on `z`.
/// Kept `>= 34` (inside brick row `y = 1`, `32..=63`) so the ramp — like the
/// flat plain around it — never needs brick row `y = 0` to be anything but
/// uniformly solid.
const G1_RAMP_X0: i64 = 180;
const G1_RAMP_X1: i64 = 211;
const G1_RAMP_Z0: i64 = 100;
const G1_RAMP_Z1: i64 = 115;
const G1_RAMP_BOTTOM_HEIGHT: i64 = 34;

fn g1_in_ramp(x: i64, z: i64) -> bool {
    (G1_RAMP_X0..=G1_RAMP_X1).contains(&x) && (G1_RAMP_Z0..=G1_RAMP_Z1).contains(&z)
}

/// Surface height (global cell `y`) for [`g1_full_envelope_scene`]'s terrain:
/// [`G1_FLAT_HEIGHT`] everywhere except the linear ramp inside
/// [`g1_in_ramp`]'s footprint.
fn g1_surface_height(x: i64, z: i64) -> i64 {
    if g1_in_ramp(x, z) {
        let rx = x - G1_RAMP_X0;
        let span = G1_RAMP_X1 - G1_RAMP_X0;
        let drop = G1_FLAT_HEIGHT - G1_RAMP_BOTTOM_HEIGHT;
        G1_FLAT_HEIGHT - (rx * drop) / span
    } else {
        G1_FLAT_HEIGHT
    }
}

/// Carves a hollow rectangular shell between `a` and `b` inclusive: solid
/// `material` walls `wall` cells thick, air interior. `a`/`b` are whatever
/// coordinate frame the caller's edits already use (world-relative for
/// terrain, body-local for a standalone body) — [`GlobalCell`] is generic
/// over both, matching [`solid_block`] / [`dumbbell`] below.
fn hollow_box(
    v: &mut Volume,
    id: VolumeId,
    a: GlobalCell,
    b: GlobalCell,
    wall: i64,
    material: MaterialId,
) {
    v.apply_edit(&EditPlan::filled_box(id, a, b, material))
        .expect("hollow shell outer edit");
    let inner_a = GlobalCell::new(a.x + wall, a.y + wall, a.z + wall);
    let inner_b = GlobalCell::new(b.x - wall, b.y - wall, b.z - wall);
    if inner_a.x <= inner_b.x && inner_a.y <= inner_b.y && inner_a.z <= inner_b.z {
        v.apply_edit(&EditPlan::filled_box(id, inner_a, inner_b, MaterialId::AIR))
            .expect("hollow shell interior edit");
    }
}

/// Tower footprint: `x 24..=39`, `z 32..=47` — 16 cells (4 m) each side,
/// straddling the brick boundaries at `x = 32` and `z = 32`, well clear of the
/// ramp footprint, standing on the flat plain.
const G1_TOWER_X0: i64 = 24;
const G1_TOWER_X1: i64 = 39;
const G1_TOWER_Z0: i64 = 32;
const G1_TOWER_Z1: i64 = 47;
/// The tower's base — the flat plain height, so it sits flush with no
/// embedding or floating.
const G1_TOWER_BASE_Y: i64 = G1_FLAT_HEIGHT;
/// `48` cells = `12 m` (`docs/validation.md` "a 12 m hollow tower/bridge").
const G1_TOWER_HEIGHT: i64 = 48;

/// The T11a / ENG-62 G1 full-workload world: a `64 x 32 x 64 m` bounded
/// envelope holding real, resident, walkable terrain across its whole
/// footprint — not just isolated structures in an otherwise-absent volume
/// (contrast [`separated_regions_scene`]) — plus a hollow tower/bridge that
/// spans brick boundaries and can be cut down.
///
/// - **Ground**: [`g1_surface_height`]'s flat plain + one excavatable ramp,
///   stone below the surface with a one-cell dirt cap. Built by direct brick
///   construction ([`Brick::uniform`] for the 192 always-solid / always-air
///   bricks, one shared [`Brick::restored`] flat-surface brick cloned across
///   62 of the 64 `y = 1` columns, two bespoke `Brick::restored` bricks for
///   the ramp) rather than a `~8 400 000`-cell `EditPlan` fill — same trick as
///   [`giant_collapse_scene`]'s block.
/// - **Tower**: a hollow `4 x 4 x 12 m` stone shaft ([`G1_TOWER_X0`] etc.),
///   one cell of wall, straddling two brick boundaries, standing on the
///   terrain.
/// - **Bridge**: a hollow `~16 m` stone tunnel cantilevered east off the
///   tower at mid-height, crossing a third brick boundary (`x = 64`) —
///   supported only by its solid-cell connection to the tower (T07
///   connectivity, not a physical beam analysis), so cutting the tower
///   detaches it.
///
/// The G1 gate's "moving hollow test volume" is a **body**, not terrain — see
/// `spall_sim::fixtures::spawn_g1_hollow_test_volume` in the sibling crate.
pub fn g1_full_envelope_scene(id: VolumeId) -> Volume {
    let bounds = BrickBounds::new(
        BrickCoord::new(0, 0, 0),
        BrickCoord::new(
            G1_ENVELOPE_BRICKS_X - 1,
            G1_ENVELOPE_BRICKS_Y - 1,
            G1_ENVELOPE_BRICKS_Z - 1,
        ),
    )
    .expect("valid G1 full-envelope bounds");
    let mut v = Volume::bounded(id, CellSizeCode::Quarter, bounds);

    // Brick row y = 0 (global y 0..=31): every column's surface height is
    // >= 34 (see g1_surface_height), so this row is always fully below the
    // surface — one uniform stone brick per column, no per-cell cost.
    for bx in 0..G1_ENVELOPE_BRICKS_X {
        for bz in 0..G1_ENVELOPE_BRICKS_Z {
            v.insert_brick(
                BrickCoord::new(bx, 0, bz),
                Brick::uniform(STONE, Revision(1)),
            )
            .expect("g1 bedrock brick insert");
        }
    }
    // Brick rows y = 2..=3 (global y 64..=127): every column's surface height
    // is <= 58, so these rows are always fully above the surface — uniform
    // resident air, so a ray or a collider query above the hills still finds
    // real (not merely absent) empty space.
    for by in 2..G1_ENVELOPE_BRICKS_Y {
        for bx in 0..G1_ENVELOPE_BRICKS_X {
            for bz in 0..G1_ENVELOPE_BRICKS_Z {
                v.insert_brick(
                    BrickCoord::new(bx, by, bz),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .expect("g1 sky brick insert");
            }
        }
    }
    // Brick row y = 1 (global y 32..=63): the surface transition. Every
    // column's height falls in this row. The flat plain gives every
    // non-ramp column the identical layer, so it is built once and cloned;
    // only the (at most) two bricks the ramp's footprint overlaps
    // (`x 180..=211` crosses the `x = 192` brick boundary; `z 100..=115`
    // stays inside one `z` brick) are built from their own computed layer.
    let flat_layer = {
        let mut cells = vec![MaterialId::AIR; spall_core::CELLS_PER_BRICK];
        for local_z in 0..32i64 {
            for local_x in 0..32i64 {
                for local_y in 0..32i64 {
                    let gy = 32 + local_y;
                    let m = if gy < G1_FLAT_HEIGHT - 1 {
                        STONE
                    } else if gy == G1_FLAT_HEIGHT - 1 {
                        DIRT
                    } else {
                        MaterialId::AIR
                    };
                    let idx = (local_x + 32 * (local_y + 32 * local_z)) as usize;
                    cells[idx] = m;
                }
            }
        }
        Brick::restored(&cells, Revision(1), true)
    };
    let mut cells = vec![MaterialId::AIR; spall_core::CELLS_PER_BRICK];
    for bx in 0..G1_ENVELOPE_BRICKS_X {
        for bz in 0..G1_ENVELOPE_BRICKS_Z {
            let brick_touches_ramp = (G1_RAMP_X0 / 32..=G1_RAMP_X1 / 32).contains(&bx)
                && (G1_RAMP_Z0 / 32..=G1_RAMP_Z1 / 32).contains(&bz);
            let brick = if brick_touches_ramp {
                for local_z in 0..32i64 {
                    let gz = bz * 32 + local_z;
                    for local_x in 0..32i64 {
                        let gx = bx * 32 + local_x;
                        let h = g1_surface_height(gx, gz);
                        for local_y in 0..32i64 {
                            let gy = 32 + local_y;
                            let m = if gy < h - 1 {
                                STONE
                            } else if gy == h - 1 {
                                DIRT
                            } else {
                                MaterialId::AIR
                            };
                            let idx = (local_x + 32 * (local_y + 32 * local_z)) as usize;
                            cells[idx] = m;
                        }
                    }
                }
                Brick::restored(&cells, Revision(1), true)
            } else {
                flat_layer.clone()
            };
            v.insert_brick(BrickCoord::new(bx, 1, bz), brick)
                .expect("g1 surface brick insert");
        }
    }

    // Hollow tower.
    hollow_box(
        &mut v,
        id,
        GlobalCell::new(G1_TOWER_X0, G1_TOWER_BASE_Y, G1_TOWER_Z0),
        GlobalCell::new(
            G1_TOWER_X1,
            G1_TOWER_BASE_Y + G1_TOWER_HEIGHT - 1,
            G1_TOWER_Z1,
        ),
        1,
        STONE,
    );

    // Hollow bridge: cantilevered east off the tower at mid-height, 16 m
    // long, crossing the x = 64 brick boundary.
    let bridge_y0 = G1_TOWER_BASE_Y + 18;
    hollow_box(
        &mut v,
        id,
        GlobalCell::new(G1_TOWER_X1 + 1, bridge_y0, G1_TOWER_Z0 + 4),
        GlobalCell::new(G1_TOWER_X1 + 1 + 63, bridge_y0 + 7, G1_TOWER_Z0 + 11),
        1,
        STONE,
    );

    v
}

/// BLAKE3 digest over a volume's resident bricks in canonical `(z, y, x)`
/// order: cell size, then per brick `(coord, content hash, revision)`. Stable
/// across runs and platforms for a given fixture; use it to pin fixtures in
/// tests. Not the replicated topology hash.
pub fn digest(volume: &Volume) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(DIGEST_DOMAIN.len() as u32).to_le_bytes());
    h.update(DIGEST_DOMAIN);
    h.update(&[volume.cell_size().to_u8()]);

    let coords = volume.resident_brick_coords();
    h.update(&(coords.len() as u32).to_le_bytes());
    for c in coords {
        h.update(&c.x.to_le_bytes());
        h.update(&c.y.to_le_bytes());
        h.update(&c.z.to_le_bytes());
        let snap = volume
            .snapshot_brick(c)
            .expect("resident coord is in bounds")
            .expect("coord came from the resident set");
        h.update(&BrickHash::to_bytes(snap.content_hash()));
        h.update(&snap.revision().get().to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// Lowercase hex of [`digest`].
pub fn digest_hex(volume: &Volume) -> String {
    digest(volume).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::Sample;

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    #[test]
    fn fixture_digests_are_pinned() {
        assert_eq!(digest_hex(&flat_terrain(vid(1))), FLAT_TERRAIN_DIGEST);
        assert_eq!(digest_hex(&hollow_tower(vid(1))), HOLLOW_TOWER_DIGEST);
        assert_eq!(digest_hex(&cross_brick_bridge(vid(1))), BRIDGE_DIGEST);
        assert_eq!(digest_hex(&sloped_terrain(vid(1))), SLOPE_DIGEST);
    }

    #[test]
    fn checkerboard_split_scene_block_is_a_fragmented_connected_component() {
        let v = checkerboard_split_scene(vid(1));
        assert_eq!(
            v.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            Sample::Filled(STONE),
            "anchored floor"
        );
        assert_eq!(
            v.sample(GlobalCell::new(15, 4, 15)).unwrap(),
            Sample::Filled(STONE),
            "column"
        );
        let a = v.sample(GlobalCell::new(4, 8, 4)).unwrap();
        let b = v.sample(GlobalCell::new(5, 8, 4)).unwrap();
        assert!(
            matches!(a, Sample::Filled(_)) && matches!(b, Sample::Filled(_)) && a != b,
            "adjacent block cells carry different materials (checkerboard): {a:?} vs {b:?}"
        );
        assert_eq!(digest_hex(&v), CHECKERBOARD_SPLIT_DIGEST);
    }

    #[test]
    fn bulk_split_scene_block_is_a_large_high_entropy_component() {
        let v = bulk_split_scene(vid(1));
        assert_eq!(
            v.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            Sample::Filled(STONE),
            "anchored floor"
        );
        assert_eq!(
            v.sample(GlobalCell::new(31, 6, 31)).unwrap(),
            Sample::Filled(STONE),
            "column"
        );
        // The block spans brick seams on every axis and mixes materials.
        assert!(
            v.resident_brick_coords()
                .iter()
                .any(|c| c.x == 1 && c.y == 1 && c.z == 1),
            "block reaches brick (1,1,1)"
        );
        let a = v.sample(GlobalCell::new(20, 20, 20)).unwrap();
        let b = v.sample(GlobalCell::new(21, 20, 20)).unwrap();
        assert!(matches!(a, Sample::Filled(_)) && matches!(b, Sample::Filled(_)));
        assert_eq!(digest_hex(&v), BULK_SPLIT_DIGEST);
    }

    #[test]
    fn giant_collapse_scene_block_spans_exactly_64_bricks() {
        let v = giant_collapse_scene(vid(1));
        assert_eq!(
            v.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            Sample::Filled(STONE),
            "anchored floor"
        );
        assert_eq!(
            v.sample(GlobalCell::new(3, 4, 3)).unwrap(),
            Sample::Filled(STONE),
            "column"
        );
        assert_eq!(
            v.sample(GlobalCell::new(0, 32, 0)).unwrap(),
            Sample::Filled(STONE),
            "block corner"
        );
        assert_eq!(
            v.sample(GlobalCell::new(127, 159, 127)).unwrap(),
            Sample::Filled(STONE),
            "block far corner"
        );
        let block_bricks: std::collections::BTreeSet<BrickCoord> = v
            .resident_brick_coords()
            .into_iter()
            .filter(|c| c.y >= 1 && c.y <= 4)
            .collect();
        assert_eq!(
            block_bricks.len(),
            64,
            "the detachable block occupies exactly 64 bricks"
        );
    }

    #[test]
    fn fixture_digest_is_independent_of_volume_id_and_run() {
        assert_eq!(
            digest(&hollow_tower(vid(1))),
            digest(&hollow_tower(vid(999)))
        );
        assert_eq!(
            digest(&cross_brick_bridge(vid(2))),
            digest(&cross_brick_bridge(vid(7)))
        );
    }

    #[test]
    fn flat_terrain_is_solid_below_and_resident_air_above() {
        let v = flat_terrain(vid(1));
        assert_eq!(
            v.sample(GlobalCell::new(10, 31, 10)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(10, 32, 10)).unwrap(),
            Sample::Empty { modified: false }
        );
        assert_eq!(v.resident_brick_count(), 8);
    }

    #[test]
    fn hollow_tower_spans_eight_bricks_and_is_hollow() {
        let v = hollow_tower(vid(1));
        assert_eq!(v.resident_brick_count(), 8);
        // Wall solid, interior void, both around negative coordinates.
        assert_eq!(
            v.sample(GlobalCell::new(-5, 0, 0)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            Sample::Empty { modified: true }
        );
        let coords: std::collections::BTreeSet<_> = v.resident_brick_coords().into_iter().collect();
        for x in [-1, 0] {
            for y in [-1, 0] {
                for z in [-1, 0] {
                    assert!(coords.contains(&BrickCoord::new(x, y, z)));
                }
            }
        }
    }

    #[test]
    fn bridge_crosses_four_bricks_on_x() {
        let v = cross_brick_bridge(vid(1));
        let xs: std::collections::BTreeSet<i64> =
            v.resident_brick_coords().iter().map(|c| c.x).collect();
        assert_eq!(xs, std::collections::BTreeSet::from([-2, -1, 0, 1]));
        assert_eq!(
            v.sample(GlobalCell::new(-40, 0, 0)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(39, 1, 1)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(40, 0, 0)).unwrap(),
            Sample::Empty { modified: true }
        );
    }

    #[test]
    fn cross_brick_bridge_scene_column_and_beam_span_the_x_seam() {
        let v = cross_brick_bridge_scene(vid(1));
        let x_bricks: std::collections::BTreeSet<i64> =
            v.resident_brick_coords().iter().map(|c| c.x).collect();
        assert!(
            x_bricks.contains(&0) && x_bricks.contains(&1),
            "scene must occupy both x-bricks, got {x_bricks:?}"
        );
        // Column cells on both sides of the x = 32 seam.
        assert_eq!(
            v.sample(GlobalCell::new(31, 4, 1)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(32, 4, 1)).unwrap(),
            Sample::Filled(STONE)
        );
        // Beam cells on both sides of the seam.
        assert_eq!(
            v.sample(GlobalCell::new(24, 7, 1)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(40, 8, 2)).unwrap(),
            Sample::Filled(STONE)
        );
    }

    #[test]
    fn slope_has_a_dirt_surface_over_stone() {
        let v = sloped_terrain(vid(1));
        assert_eq!(
            v.sample(GlobalCell::new(10, 10, 5)).unwrap(),
            Sample::Filled(DIRT)
        );
        assert_eq!(
            v.sample(GlobalCell::new(10, 4, 5)).unwrap(),
            Sample::Filled(STONE)
        );
        assert_eq!(
            v.sample(GlobalCell::new(10, 11, 5)).unwrap(),
            Sample::Empty { modified: true }
        );
    }

    const FLAT_TERRAIN_DIGEST: &str =
        "630925223666650e47a724ba9686efd7787eb5868581518eb25c1f8c64c24d88";
    const HOLLOW_TOWER_DIGEST: &str =
        // T02 review corrected per-brick revision allocation to (z,y,x).
        // Geometry/content is unchanged; the digest also includes revisions.
        "e164e7dc4ef3565cfc181de28442875ff6dc91f0321dfd69fb0e0a483f382933";
    const BRIDGE_DIGEST: &str = "70905cb1ba1414ca3eef09912281e3ddfa94cb23fbe1ff9f69fb15b22b856ab8";
    const SLOPE_DIGEST: &str = "30023f777e4a2c0cc1f0515785581b6d4cd95dd3d304a8e7bdc51e1d50ec5862";
    const CHECKERBOARD_SPLIT_DIGEST: &str =
        "46de0b892ccac5b223bf95a3d8ddd4dc6db64234787d958276bd9da4f9239434";
    const BULK_SPLIT_DIGEST: &str =
        "03f5ef01144a1d927f4caf372181c052cd375cbd4b644f6f3e504838982aceea";
}
