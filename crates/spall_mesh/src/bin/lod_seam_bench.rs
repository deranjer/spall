use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_mesh::{Mesh, stitch_heightfield_edge};
use spall_voxel::{EditPlan, Sample, Volume};

fn main() {
    // Two adjacent chunk edges at a 2:1 near/far resolution boundary. The far
    // profile is already coarsened by its LOD builder; each value covers two
    // fine intervals.
    let fine_heights: [i64; 8] = [4, 5, 5, 4, 3, 3, 4, 6];
    let coarse_heights: [i64; 4] = [5, 4, 6, 5];
    let fine_before = fine_heights;
    let coarse_before = coarse_heights;
    let volume_id = VolumeId::new(1).expect("volume ID");
    let mut authoritative = Volume::new(volume_id, CellSizeCode::Quarter);
    let mut voxel = EditPlan::new(volume_id);
    voxel.set(GlobalCell::new(2, 3, 4), MaterialId(1));
    authoritative
        .apply_edit(&voxel)
        .expect("seed authoritative geometry");
    let volume_before = authoritative
        .snapshot_brick(BrickCoord::new(0, 0, 0))
        .unwrap()
        .unwrap()
        .content_hash();

    let mut mismatched_intervals = 0usize;
    let mut baseline_unmatched_area = 0u64;
    for (i, &fine) in fine_heights.iter().enumerate() {
        let coarse = coarse_heights[i / 2];
        let difference = fine.abs_diff(coarse);
        if difference > 0 {
            mismatched_intervals += 1;
            baseline_unmatched_area += difference;
        }
    }

    let patch = stitch_heightfield_edge(&fine_heights, &coarse_heights, 2, 32, 0, MaterialId(1))
        .expect("valid 2:1 heightfield edge");
    let patch_area: u64 = patch.quads.iter().map(|q| q.area_cells() as u64).sum();
    let mesh = Mesh::from_quads(&patch.quads, 0.25);
    assert_eq!(patch.mismatched_intervals, mismatched_intervals);
    assert_eq!(patch.unmatched_area_cells, baseline_unmatched_area);
    assert_eq!(patch_area, baseline_unmatched_area);
    assert_eq!(fine_heights, fine_before);
    assert_eq!(coarse_heights, coarse_before);
    let volume_after = authoritative
        .snapshot_brick(BrickCoord::new(0, 0, 0))
        .unwrap()
        .unwrap()
        .content_hash();
    let authoritative_unchanged = volume_before == volume_after
        && authoritative.sample(GlobalCell::new(2, 3, 4)).unwrap() == Sample::Filled(MaterialId(1));
    assert!(
        authoritative_unchanged,
        "render-only seam work leaves voxel data unchanged"
    );
    assert_eq!(mesh.triangle_count(), patch.quads.len() * 2);
    for quad in &patch.quads {
        let corners = quad.corners();
        assert!(corners.iter().all(|point| point[0] == 32));
        let z_min = corners.iter().map(|point| point[2]).min().unwrap();
        let z_max = corners.iter().map(|point| point[2]).max().unwrap();
        let y_min = corners.iter().map(|point| point[1]).min().unwrap();
        let y_max = corners.iter().map(|point| point[1]).max().unwrap();
        assert_eq!(z_max - z_min, 1);
        assert_eq!(y_max - y_min, quad.area_cells());
    }

    let cell_area_m2 = 0.25_f64 * 0.25;
    println!(
        "lod_ratio=2 fine_intervals={} coarse_intervals={} mismatched_intervals={} baseline_unmatched_area_cells={} transition_quads={} transition_triangles={} residual_boundary_area_cells=0 unmatched_area_m2={:.6} authoritative_voxel_access=none authoritative_volume_hash_unchanged={authoritative_unchanged}",
        fine_heights.len(),
        coarse_heights.len(),
        patch.mismatched_intervals,
        baseline_unmatched_area,
        patch.quads.len(),
        mesh.triangle_count(),
        baseline_unmatched_area as f64 * cell_area_m2,
    );
}
