//! Whole-volume meshing: pick a strategy, mesh every resident brick with a
//! one-cell halo across seams, and return a [`spall_jobs::JobToken`] over every
//! brick revision and missing-neighbour sentinel the mesh depended on.

use std::collections::BTreeSet;

use spall_core::BrickCoord;
use spall_jobs::{Generation, JobToken, Staleness, TopologyEpoch, WorldView};
use spall_voxel::{BrickState, Volume};

use crate::culled::emit_culled;
use crate::enumerate::for_each_exposed_face;
use crate::greedy::emit_greedy;
use crate::mesh::{FaceQuad, Mesh, MeshStats, MeshStrategy};
use crate::sample::{CellBox, Occupancy, VolumeSampler};

/// Options for [`build_volume_mesh`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshOptions {
    pub strategy: MeshStrategy,
}

impl Default for MeshOptions {
    fn default() -> Self {
        Self {
            strategy: MeshStrategy::Greedy,
        }
    }
}

/// A meshed volume plus the job token that says when it is stale.
#[derive(Debug, Clone)]
pub struct VolumeMesh {
    pub mesh: Mesh,
    /// The quads the mesh was triangulated from, in canonical order.
    pub quads: Vec<FaceQuad>,
    pub stats: MeshStats,
    /// Read dependencies: every resident brick revision, plus every
    /// non-resident brick in the one-brick halo as an absent/failed sentinel.
    pub token: JobToken,
}

impl VolumeMesh {
    /// Validate the mesh against the current world.
    pub fn staleness<W: WorldView + ?Sized>(&self, world: &W) -> Staleness {
        self.token.check(world)
    }

    /// `true` when a dependency has moved and the volume must be re-meshed.
    pub fn is_stale<W: WorldView + ?Sized>(&self, world: &W) -> bool {
        !self.token.is_fresh(world)
    }
}

/// Mesh every resident brick of `volume`. `generation` and `topology_epoch`
/// stamp the returned token so a world reload or a coarse topology change also
/// invalidates the result.
pub fn build_volume_mesh(
    volume: &Volume,
    generation: Generation,
    topology_epoch: TopologyEpoch,
    opts: MeshOptions,
) -> VolumeMesh {
    let cell_m = volume.cell_size().metres();
    let resident = volume.resident_brick_coords();

    let Some(cell_box) = CellBox::of_resident(volume) else {
        return VolumeMesh {
            mesh: Mesh::default(),
            quads: Vec::new(),
            stats: MeshStats {
                strategy: opts.strategy,
                quad_count: 0,
                vertex_count: 0,
                triangle_count: 0,
                surface_area_m2: 0.0,
                exposed_unit_faces: 0,
                unresolved_halo_faces: 0,
            },
            token: JobToken::new(generation, topology_epoch),
        };
    };

    let sampler = VolumeSampler::new(volume);
    let quads = match opts.strategy {
        MeshStrategy::Culled => emit_culled(&sampler, cell_box),
        MeshStrategy::Greedy => emit_greedy(&sampler, cell_box),
    };
    let mesh = Mesh::from_quads(&quads, cell_m);

    let mut exposed_unit_faces = 0u64;
    let mut unresolved_halo_faces = 0u64;
    for_each_exposed_face(&sampler, cell_box, |face| {
        exposed_unit_faces += 1;
        if matches!(face.neighbour, Occupancy::Unknown(_)) {
            unresolved_halo_faces += 1;
        }
    });

    let stats = MeshStats {
        strategy: opts.strategy,
        quad_count: quads.len(),
        vertex_count: mesh.vertices.len(),
        triangle_count: mesh.triangle_count(),
        surface_area_m2: exposed_unit_faces as f64 * cell_m * cell_m,
        exposed_unit_faces,
        unresolved_halo_faces,
    };

    VolumeMesh {
        mesh,
        quads,
        stats,
        token: build_token(volume, &resident, generation, topology_epoch),
    }
}

/// Records one dependency per resident brick and one absent/failed sentinel per
/// non-resident brick in the 26-neighbour halo of the resident set.
fn build_token(
    volume: &Volume,
    resident: &[BrickCoord],
    generation: Generation,
    topology_epoch: TopologyEpoch,
) -> JobToken {
    let mut token = JobToken::new(generation, topology_epoch);
    let volume_id = volume.id();
    let resident_set: BTreeSet<(i64, i64, i64)> =
        resident.iter().map(|c| (c.x, c.y, c.z)).collect();

    for &coord in resident {
        if let Ok(Some(revision)) = volume.brick_revision(coord) {
            token = token.reading(volume_id, coord, revision);
        }
    }

    let mut halo: BTreeSet<(i64, i64, i64)> = BTreeSet::new();
    for &coord in resident {
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if (dx, dy, dz) == (0, 0, 0) {
                        continue;
                    }
                    let n = (coord.x + dx, coord.y + dy, coord.z + dz);
                    if !resident_set.contains(&n) {
                        halo.insert(n);
                    }
                }
            }
        }
    }

    for (x, y, z) in halo {
        let coord = BrickCoord::new(x, y, z);
        match volume.brick_state(coord) {
            Ok(BrickState::Absent) => token = token.reading_absent(volume_id, coord),
            Ok(BrickState::Failed) => token = token.reading_failed(volume_id, coord),
            // Resident (can't happen — filtered) or out of a bounded volume:
            // a hard world edge is not a dependency.
            _ => {}
        }
    }

    token
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face::FaceDir;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
    use spall_jobs::testkit::MapWorld;
    use spall_voxel::{Brick, EditPlan};

    const STONE: MaterialId = MaterialId(1);

    fn vid() -> VolumeId {
        VolumeId::new(1).unwrap()
    }

    fn gen1() -> Generation {
        Generation(1)
    }

    #[test]
    fn greedy_and_culled_report_the_same_surface_area() {
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        for x in 0..5 {
            for y in 0..3 {
                for z in 0..5 {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                }
            }
        }
        v.apply_edit(&plan).unwrap();

        let culled = build_volume_mesh(
            &v,
            gen1(),
            TopologyEpoch::START,
            MeshOptions {
                strategy: MeshStrategy::Culled,
            },
        );
        let greedy = build_volume_mesh(
            &v,
            gen1(),
            TopologyEpoch::START,
            MeshOptions {
                strategy: MeshStrategy::Greedy,
            },
        );

        assert_eq!(culled.stats.surface_area_m2, greedy.stats.surface_area_m2);
        assert_eq!(
            culled.stats.exposed_unit_faces,
            greedy.stats.exposed_unit_faces
        );
        assert!(greedy.stats.quad_count < culled.stats.quad_count);
        // 5x3x5 box: 2*(5*5) + 2*(5*3) + 2*(5*3) = 110 faces at 0.25 m.
        assert_eq!(culled.stats.exposed_unit_faces, 110);
        assert!((greedy.stats.surface_area_m2 - 110.0 * 0.0625).abs() < 1e-9);
    }

    #[test]
    fn a_fresh_mesh_is_not_stale_against_the_world_it_was_built_from() {
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(1, 1, 1), STONE);
        v.apply_edit(&plan).unwrap();

        let vm = build_volume_mesh(&v, gen1(), TopologyEpoch::START, MeshOptions::default());

        let mut world = MapWorld::new(gen1());
        let rev = v.brick_revision(BrickCoord::new(0, 0, 0)).unwrap().unwrap();
        world.set_brick(vid(), BrickCoord::new(0, 0, 0), rev);
        assert!(!vm.is_stale(&world));

        world.set_generation(Generation(2));
        assert!(vm.is_stale(&world));
    }

    #[test]
    fn a_halo_brick_arriving_makes_the_mesh_stale_and_removes_the_now_hidden_seam() {
        // One solid brick in an unbounded volume: its +X neighbour is absent, so
        // the whole x = 32 boundary is meshed against Unknown space and recorded
        // as a halo dependency.
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        v.insert_brick(BrickCoord::new(0, 0, 0), Brick::uniform(STONE, Revision(1)))
            .unwrap();

        let before = build_volume_mesh(&v, gen1(), TopologyEpoch::START, MeshOptions::default());
        let seam_before = before
            .quads
            .iter()
            .filter(|q| q.dir == FaceDir::PosX && q.plane == 32)
            .map(|q| q.area_cells())
            .sum::<i64>();
        assert_eq!(seam_before, 32 * 32, "the full +X boundary is meshed");
        assert!(before.stats.unresolved_halo_faces >= 32 * 32);
        assert!(
            before
                .token
                .reads()
                .iter()
                .any(|d| d.brick.brick == BrickCoord::new(1, 0, 0)
                    && d.state == spall_jobs::DepState::Absent),
            "the absent neighbour is a recorded dependency"
        );

        // Fresh while (1,0,0) is still absent.
        let mut world = MapWorld::new(gen1());
        world.set_brick(vid(), BrickCoord::new(0, 0, 0), Revision(1));
        assert!(!before.is_stale(&world));

        // The neighbour arrives solid: the token goes stale and a re-mesh drops
        // every now-occluded seam face.
        v.insert_brick(BrickCoord::new(1, 0, 0), Brick::uniform(STONE, Revision(1)))
            .unwrap();
        world.set_brick(vid(), BrickCoord::new(1, 0, 0), Revision(1));
        assert!(before.is_stale(&world));

        let after = build_volume_mesh(&v, gen1(), TopologyEpoch::START, MeshOptions::default());
        let seam_after = after
            .quads
            .iter()
            .filter(|q| q.dir == FaceDir::PosX && q.plane == 32)
            .count();
        assert_eq!(seam_after, 0, "the x = 32 seam is now interior");
    }

    #[test]
    fn an_empty_volume_yields_an_empty_mesh_with_a_dependency_free_token() {
        let v = Volume::new(vid(), CellSizeCode::Quarter);
        let vm = build_volume_mesh(&v, gen1(), TopologyEpoch::START, MeshOptions::default());
        assert!(vm.mesh.is_empty());
        assert_eq!(vm.stats.exposed_unit_faces, 0);
        assert!(vm.token.reads().is_empty());
    }
}
