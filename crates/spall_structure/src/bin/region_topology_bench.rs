use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_jobs::{Generation, TopologyEpoch};
use spall_structure::{AnchorPlane, CancelToken, ResidencyMode, StructureIndex};
use spall_voxel::{EditPlan, RegionLayout, Volume};

fn main() {
    let id = VolumeId::new(1).expect("valid volume id");
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    let mut beam = EditPlan::filled_box(
        id,
        GlobalCell::new(0, 1, 1),
        GlobalCell::new(95, 1, 1),
        MaterialId(1),
    );
    beam.set(GlobalCell::new(0, 0, 1), MaterialId(1));
    volume.apply_edit(&beam).expect("apply cross-region beam");

    let layout = RegionLayout::new([2, 2, 2]).expect("nonzero region span");
    let brick_groups = layout.group_bricks(volume.resident_brick_coords());
    let index = StructureIndex::build(
        &volume,
        AnchorPlane::at(0),
        ResidencyMode::AllResident,
        Generation(1),
        TopologyEpoch::START,
        &CancelToken::new(),
    )
    .expect("complete structural analysis");
    let components = index.graph().components();
    assert_eq!(components.len(), 1, "beam stays connected across regions");
    assert_eq!(
        components[0].cell_count, 97,
        "all beam and anchor cells remain"
    );
    let report = index.report();
    assert_eq!(
        report.supported_cells, 97,
        "support crosses the region boundary"
    );

    println!(
        "region_span_bricks=2 resident_bricks={} active_regions={} component_count={} component_cells={} supported_cells={} dense_bytes={}",
        volume.resident_brick_coords().len(),
        brick_groups.len(),
        components.len(),
        components[0].cell_count,
        report.supported_cells,
        volume.memory_report().total_dense_bytes(),
    );
    for (region, bricks) in brick_groups {
        println!(
            "region=({},{},{}) bricks={}",
            region.x,
            region.y,
            region.z,
            bricks.len()
        );
    }
    let read_bricks = index.graph().read_revisions().count();
    println!("structural_bricks_read={read_bricks}");
}
