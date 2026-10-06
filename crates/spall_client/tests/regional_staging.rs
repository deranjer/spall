use spall_client::replica::StagedBaseline;
use spall_client::{ReplicaConfig, ReplicaWorld};
use spall_core::{EntityId, VolumeId};
use spall_protocol::segment::VolumeHeader;
use spall_protocol::{BaselineBrick, BaselineCells, BaselineOwner, Hash32};

fn digest(coord: [i64; 3]) -> BaselineBrick {
    BaselineBrick {
        coord,
        revision: 2,
        edited: true,
        cells: BaselineCells::Digest {
            content_hash: Hash32::of(b"known terrain"),
            solid_cells: 1,
        },
    }
}

#[test]
fn digest_staging_rejects_bounds_duplicates_and_geometry_overlap() {
    let id = VolumeId::new(1).unwrap();
    let mut staged = StagedBaseline::new(7);
    staged
        .open_volume(
            id,
            &VolumeHeader {
                owner: BaselineOwner::Terrain,
                cell_size_code: 0,
                bounds: Some([[0, 0, 0], [0, 0, 0]]),
            },
        )
        .unwrap();
    assert!(staged.insert_brick(id, &digest([1, 0, 0])).is_err());
    staged.insert_brick(id, &digest([0, 0, 0])).unwrap();
    assert!(staged.insert_brick(id, &digest([0, 0, 0])).is_err());
    let geometry = BaselineBrick {
        cells: BaselineCells::Uniform(1),
        ..digest([0, 0, 0])
    };
    assert!(staged.insert_brick(id, &geometry).is_err());
    // An incomplete staging failure leaves the live replica untouched.
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    let before = replica.world_hash();
    assert!(replica.install_staged(staged).is_err());
    assert_eq!(replica.world_hash(), before);
}

#[test]
fn body_geometry_can_never_be_replaced_by_a_digest() {
    let id = VolumeId::new(2).unwrap();
    let mut staged = StagedBaseline::new(7);
    staged
        .open_volume(
            id,
            &VolumeHeader {
                owner: BaselineOwner::Body(EntityId::new(2).unwrap()),
                cell_size_code: 0,
                bounds: None,
            },
        )
        .unwrap();
    assert!(staged.insert_brick(id, &digest([0, 0, 0])).is_err());
}
