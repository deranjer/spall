use spall_core::VolumeId;
use spall_voxel::{RegionLayout, fixtures};

fn main() {
    let volume = fixtures::separated_regions_scene(VolumeId::new(1).expect("valid volume id"));
    let bricks = volume.resident_brick_coords();
    let memory = volume.memory_report();
    println!(
        "scene=separated_regions resident_bricks={} dense_bricks={} dense_bytes={}",
        bricks.len(),
        memory.dense_bricks,
        memory.total_dense_bytes()
    );

    for span in [1, 2, 4, 8, 16] {
        let layout = RegionLayout::new([span, span, span]).expect("nonzero span");
        let groups = layout.group_bricks(bricks.iter().copied());
        let mut largest = 0usize;
        let mut largest_bytes = 0u64;
        for group in groups.values() {
            largest = largest.max(group.len());
            largest_bytes = largest_bytes
                .max(group.len() as u64 * spall_voxel::MemoryReport::DENSE_BRICK_BYTES as u64);
        }
        println!(
            "bricks_per_region={span} active_regions={} largest_region_bricks={largest} largest_region_dense_bytes={largest_bytes}",
            groups.len()
        );
    }
}
