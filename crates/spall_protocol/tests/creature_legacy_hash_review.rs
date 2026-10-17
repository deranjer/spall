//! Coordinator compatibility pin captured before ENG-217 integration.
use spall_core::{BrickCoord, CellSizeCode, EntityId, Revision, VolumeId};
use spall_protocol::{
    CanonicalBrick, CanonicalLayer, CanonicalOwner, CanonicalVolume, canonical_topology_hash,
};

#[test]
fn legacy_terrain_and_body_hash_remains_byte_identical() {
    let terrain = CanonicalVolume {
        volume_id: VolumeId::new(1).unwrap(),
        cell_size: CellSizeCode::Quarter,
        owner: CanonicalOwner::Terrain,
        bricks: vec![CanonicalBrick {
            coord: BrickCoord::new(-1, 0, 9),
            revision: Revision(7),
            layers: vec![CanonicalLayer {
                kind: 0,
                bytes: vec![1, 2, 3, 4],
            }],
        }],
    };
    let body = CanonicalVolume {
        volume_id: VolumeId::new(8).unwrap(),
        cell_size: CellSizeCode::Sixteenth,
        owner: CanonicalOwner::Body(EntityId::new(55).unwrap()),
        bricks: vec![CanonicalBrick {
            coord: BrickCoord::new(3, -2, 0),
            revision: Revision(11),
            layers: vec![CanonicalLayer {
                kind: 0,
                bytes: vec![8, 0, 0, 1],
            }],
        }],
    };
    let hash = canonical_topology_hash(&[terrain.clone(), body.clone()]);
    assert_eq!(
        hash.to_string(),
        "83da04a97d7864ada9de47468b9c285c770eb371cc6a48e48ac51579a013a025"
    );
    assert_eq!(canonical_topology_hash(&[body, terrain]), hash);
}
