//! T23 / G3 row 7, slice A — the canonical topology hash must be **identical**
//! across cache placements: the logical view (resident bricks + retained evicted
//! digests) reproduces `canonical_volume_for` exactly, for any subset of clean
//! bricks moved into the evicted set, in any order.
//!
//! This is the compatibility identity the design review demands, stronger than
//! an empty-map test:
//!
//! ```text
//! logical(full, {})            == canonical_volume_for(full)
//! logical(evict_subset, digs)  == canonical_volume_for(full)
//! ```

use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_protocol::{CanonicalOwner, canonical_topology_hash};
use spall_sim::{canonical_logical_volume_for, canonical_volume_for};
use spall_voxel::{EditPlan, EvictedBricks, Volume};

const STONE: MaterialId = MaterialId(1);
const DIRT: MaterialId = MaterialId(2);

fn vid() -> VolumeId {
    VolumeId::new(1).unwrap()
}

/// Solid boxes across several bricks (including negative coords) plus a
/// mined-to-air tombstone brick.
fn scene() -> Volume {
    let id = vid();
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    for (a, b, m) in [
        (GlobalCell::new(1, 1, 1), GlobalCell::new(20, 3, 4), STONE),
        (GlobalCell::new(40, 0, 0), GlobalCell::new(50, 2, 2), DIRT),
        (
            GlobalCell::new(-30, 0, -10),
            GlobalCell::new(-20, 1, -5),
            STONE,
        ),
        (GlobalCell::new(2, 70, 2), GlobalCell::new(6, 72, 6), STONE),
        (GlobalCell::new(70, 4, 70), GlobalCell::new(80, 6, 78), DIRT),
    ] {
        v.apply_edit(&EditPlan::filled_box(id, a, b, m)).unwrap();
    }
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(40, 0, 0),
        GlobalCell::new(50, 2, 2),
        MaterialId::AIR,
    ))
    .unwrap();
    v
}

fn hash(cv: &spall_protocol::CanonicalVolume) -> spall_protocol::Hash32 {
    canonical_topology_hash(std::slice::from_ref(cv))
}

#[test]
fn logical_volume_with_no_evictions_is_byte_identical_to_canonical_volume_for() {
    let v = scene();
    let owner = CanonicalOwner::Terrain;
    let expected = canonical_volume_for(&v, owner);
    let logical = canonical_logical_volume_for(&v, &EvictedBricks::new(), owner).unwrap();
    assert_eq!(logical, expected);
    assert_eq!(hash(&logical), hash(&expected));
}

#[test]
fn evicting_any_subset_in_any_order_leaves_the_canonical_hash_unchanged() {
    let full = scene();
    let owner = CanonicalOwner::Terrain;
    let reference = hash(&canonical_volume_for(&full, owner));

    let coords = full.resident_brick_coords();
    assert!(coords.len() >= 4, "scene should span several bricks");

    let subsets = [
        vec![coords[0]],
        vec![coords[1], coords[coords.len() - 1]],
        coords.clone(),
        vec![coords[2], coords[0], coords[3]],
    ];

    for subset in &subsets {
        for order in [
            subset.clone(),
            subset.iter().rev().copied().collect::<Vec<_>>(),
        ] {
            let mut v = scene();
            let mut evicted = EvictedBricks::new();
            for &c in &order {
                evicted.record_from(&v, c).unwrap();
                v.evict_brick(c);
            }
            // Some bricks really are gone from the live cache...
            assert_eq!(
                v.resident_brick_count() + evicted.len(),
                coords.len(),
                "each key contributes exactly once"
            );
            // ...but the canonical hash is exactly the pre-eviction value.
            let logical = canonical_logical_volume_for(&v, &evicted, owner).unwrap();
            assert_eq!(
                hash(&logical),
                reference,
                "subset {subset:?} order {order:?} moved the hash"
            );
        }
    }
}

#[test]
fn a_body_volume_evicts_independently_of_terrain() {
    let mut terrain = scene();
    let mut body = Volume::new(VolumeId::new(7).unwrap(), CellSizeCode::Quarter);
    body.apply_edit(&EditPlan::filled_box(
        VolumeId::new(7).unwrap(),
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(9, 9, 9),
        STONE,
    ))
    .unwrap();

    let entity = spall_core::EntityId::new(7).unwrap();
    let terrain_ref =
        canonical_topology_hash(&[canonical_volume_for(&terrain, CanonicalOwner::Terrain)]);
    let body_ref =
        canonical_topology_hash(&[canonical_volume_for(&body, CanonicalOwner::Body(entity))]);

    // Evict one terrain brick; body untouched.
    let tc = terrain.resident_brick_coords()[0];
    let mut te = EvictedBricks::new();
    te.record_from(&terrain, tc).unwrap();
    terrain.evict_brick(tc);

    assert_eq!(
        canonical_topology_hash(&[canonical_logical_volume_for(
            &terrain,
            &te,
            CanonicalOwner::Terrain
        )
        .unwrap()]),
        terrain_ref
    );
    assert_eq!(
        canonical_topology_hash(&[canonical_volume_for(&body, CanonicalOwner::Body(entity))]),
        body_ref
    );
}
