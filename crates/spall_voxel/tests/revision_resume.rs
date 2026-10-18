use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_voxel::{Brick, EditPlan, RevisionResumeError, Volume};

#[test]
fn resuming_a_saved_counter_preserves_retired_gaps_and_rejects_rollback_atomically() {
    let id = VolumeId::new(1).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    let coord = BrickCoord::new(0, 0, 0);
    volume
        .insert_brick(coord, Brick::uniform(MaterialId(1), Revision(7)))
        .unwrap();
    volume.resume_revision_counter(Revision(50)).unwrap();
    let stamp = volume.state_stamp();
    let hash = volume
        .snapshot_brick(coord)
        .unwrap()
        .unwrap()
        .content_hash();
    assert_eq!(
        volume.resume_revision_counter(Revision(0)),
        Err(RevisionResumeError::Zero)
    );
    assert_eq!(
        volume.resume_revision_counter(Revision(49)),
        Err(RevisionResumeError::Rollback {
            saved: 49,
            current: 50
        })
    );
    volume.resume_revision_counter(Revision(50)).unwrap();
    assert_eq!(volume.state_stamp(), stamp);
    assert_eq!(
        volume
            .snapshot_brick(coord)
            .unwrap()
            .unwrap()
            .content_hash(),
        hash
    );
    volume
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(2),
        ))
        .unwrap();
    assert_eq!(volume.brick_revision(coord).unwrap().unwrap(), Revision(50));
    assert_eq!(volume.next_revision(), Revision(51));
}
