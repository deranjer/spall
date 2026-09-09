use spall_client::{ClientResidency, ReplicaConfig, ReplicaWorld};
use spall_core::{BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, VolumeId};
use spall_voxel::{CacheBudget, EditPlan, InterestRadii, Volume};

fn terrain() -> Volume {
    let id = VolumeId::new(1).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for brick_x in 0..5 {
        let x = brick_x * 32;
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(x, 0, 0),
                GlobalCell::new(x, 0, 0),
                MaterialId(1),
            ))
            .unwrap();
    }
    volume
}

#[test]
fn client_terrain_uses_the_shared_budget_and_body_geometry_stays_complete() {
    let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
    let body = EntityId::new(7).unwrap();
    let body_volume = VolumeId::new(8).unwrap();
    let mut geometry = Volume::new(body_volume, CellSizeCode::Quarter);
    geometry
        .apply_edit(&EditPlan::filled_box(
            body_volume,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(63, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    replica.install_body(body, geometry);

    let mut residency = ClientResidency::new(CacheBudget::new(3, usize::MAX), 64);
    residency.sync(&replica);
    residency.update_terrain_interest(
        &replica,
        BrickCoord::new(0, 0, 0),
        InterestRadii::new(0, 1).unwrap(),
    );
    let evicted = residency.enforce_budget(&mut replica);
    assert!(!evicted.is_empty());
    assert_eq!(
        replica.volume(body_volume).unwrap().resident_brick_count(),
        2,
        "a relevant dynamic body is never partitioned into partial ownership"
    );
    assert!(residency.cache.resident_bricks() <= 3);
}
