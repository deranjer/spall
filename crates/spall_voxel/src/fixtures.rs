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
}
