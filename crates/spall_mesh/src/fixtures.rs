//! Deterministic fixture volumes for the T05 acceptance shapes, plus helpers to
//! mesh them.
//!
//! Shapes required by the ticket: a solid cube, a tunnel (a hole straight
//! through a block), a checkerboard, geometry at negative coordinates, two
//! adjacent bricks, and a rotated hollow volume (the volume is meshed
//! volume-local; the rotation is a render-layer model transform).
//!
//! Each builder produces a volume **bounded** to exactly the bricks its
//! geometry occupies, so the outer boundary reads as open space and the mesh is
//! self-contained (no unresolved halo). The absent-neighbour halo path is
//! covered separately with an unbounded volume in `build`.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_jobs::{Generation, TopologyEpoch};
use spall_voxel::{BrickBounds, EditPlan, Volume};

use crate::build::{MeshOptions, VolumeMesh, build_volume_mesh};
use crate::mesh::MeshStrategy;

/// Fixture material ids (not a real manifest): stone, dirt.
pub const STONE: MaterialId = MaterialId(1);
pub const DIRT: MaterialId = MaterialId(2);

fn vid() -> VolumeId {
    VolumeId::new(1).unwrap()
}

/// Build a bounded volume containing exactly `cells`, bounded to the bricks
/// those cells touch.
fn build(cells: impl IntoIterator<Item = (GlobalCell, MaterialId)>) -> Volume {
    let cells: Vec<_> = cells.into_iter().collect();
    let mut lo = [i64::MAX; 3];
    let mut hi = [i64::MIN; 3];
    for (cell, _) in &cells {
        let (b, _) = cell.split();
        for (a, v) in [b.x, b.y, b.z].into_iter().enumerate() {
            lo[a] = lo[a].min(v);
            hi[a] = hi[a].max(v);
        }
    }
    let bounds = BrickBounds::new(
        BrickCoord::new(lo[0], lo[1], lo[2]),
        BrickCoord::new(hi[0], hi[1], hi[2]),
    )
    .expect("non-empty cell set");

    let mut v = Volume::bounded(vid(), CellSizeCode::Quarter, bounds);
    let mut plan = EditPlan::new(v.id());
    for (cell, material) in cells {
        plan.set(cell, material);
    }
    v.apply_edit(&plan).unwrap();
    v
}

/// A solid `n^3` stone cube with its minimum corner at `origin`.
pub fn cube(origin: [i64; 3], n: i64) -> Volume {
    build((0..n).flat_map(move |z| {
        (0..n).flat_map(move |y| {
            (0..n).map(move |x| {
                (
                    GlobalCell::new(origin[0] + x, origin[1] + y, origin[2] + z),
                    STONE,
                )
            })
        })
    }))
}

/// A solid `n^3` cube with a 2x2 tunnel bored straight through along Z. Both Z
/// ends are open, so the shaft's X/Y walls must be meshed.
pub fn tunnel(n: i64) -> Volume {
    let lo = n / 2 - 1;
    build((0..n).flat_map(move |z| {
        (0..n).flat_map(move |y| {
            (0..n).filter_map(move |x| {
                let bored = (lo..lo + 2).contains(&x) && (lo..lo + 2).contains(&y);
                (!bored).then_some((GlobalCell::new(x, y, z), STONE))
            })
        })
    }))
}

/// A single-layer checkerboard of solid cells in the `y = 0` plane.
pub fn checkerboard(n: i64) -> Volume {
    build((0..n).flat_map(move |z| {
        (0..n).filter_map(move |x| ((x + z) % 2 == 0).then_some((GlobalCell::new(x, 0, z), STONE)))
    }))
}

/// An 8-cell cube straddling the origin: negative cell and brick coordinates.
pub fn negative_corner() -> Volume {
    cube([-5, -5, -5], 8)
}

/// A solid slab two bricks wide along X, so a full brick seam at `x = 32` runs
/// through the solid interior and must contribute no faces. The top layer is a
/// second material.
pub fn adjacent_bricks() -> Volume {
    build((0..48).flat_map(move |x| {
        (0..4).flat_map(move |y| {
            (0..4).map(move |z| {
                let m = if y == 3 { DIRT } else { STONE };
                (GlobalCell::new(x, y, z), m)
            })
        })
    }))
}

/// An open-topped hollow stone box: a solid 2-cell floor, 2-cell-thick walls,
/// and no lid — so a capture under a rotation shows the interior floor and
/// walls through the opening. This is the "rotated hollow volume" case.
pub fn hollow_box() -> Volume {
    let inner = 2..10;
    let mut cells = Vec::new();
    for z in 0..12 {
        for y in 0..12 {
            for x in 0..12 {
                let walls = !inner.contains(&x) || !inner.contains(&z);
                let solid = y < 10 && (y < 2 || walls);
                if solid {
                    cells.push((GlobalCell::new(x, y, z), STONE));
                }
            }
        }
    }
    build(cells)
}

/// One named acceptance shape and the yaw (radians) a capture should place it
/// under. Only `rotated_hollow` is rotated; the rest render axis-aligned.
pub struct AcceptanceShape {
    pub name: &'static str,
    pub volume: Volume,
    pub yaw: f32,
}

/// The full set of T05 acceptance shapes.
pub fn acceptance_shapes() -> Vec<AcceptanceShape> {
    vec![
        AcceptanceShape {
            name: "cube",
            volume: cube([0, 0, 0], 8),
            yaw: 0.0,
        },
        AcceptanceShape {
            name: "tunnel",
            volume: tunnel(10),
            yaw: 0.0,
        },
        AcceptanceShape {
            name: "checkerboard",
            volume: checkerboard(12),
            yaw: 0.0,
        },
        AcceptanceShape {
            name: "negative_corner",
            volume: negative_corner(),
            yaw: 0.0,
        },
        AcceptanceShape {
            name: "adjacent_bricks",
            volume: adjacent_bricks(),
            yaw: 0.0,
        },
        AcceptanceShape {
            name: "rotated_hollow",
            volume: hollow_box(),
            yaw: 0.8,
        },
    ]
}

/// Mesh a fixture volume at generation 1 / epoch 0 with the given strategy.
pub fn mesh_shape(volume: &Volume, strategy: MeshStrategy) -> VolumeMesh {
    build_volume_mesh(
        volume,
        Generation(1),
        TopologyEpoch::START,
        MeshOptions { strategy },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face::FaceDir;
    use crate::mesh::mesh_digest_hex;

    fn greedy(v: &Volume) -> VolumeMesh {
        mesh_shape(v, MeshStrategy::Greedy)
    }

    fn culled(v: &Volume) -> VolumeMesh {
        mesh_shape(v, MeshStrategy::Culled)
    }

    fn shapes() -> Vec<(&'static str, Volume)> {
        vec![
            ("cube", cube([0, 0, 0], 4)),
            ("tunnel", tunnel(6)),
            ("checkerboard", checkerboard(8)),
            ("negative_corner", negative_corner()),
            ("adjacent_bricks", adjacent_bricks()),
            ("cross_brick_cube", cube([28, 0, 0], 8)),
            ("hollow_box", hollow_box()),
        ]
    }

    /// Every acceptance shape: greedy and culled agree on surface area and on
    /// the exact set of exposed unit faces, and a bounded fixture has no
    /// unresolved halo.
    #[test]
    fn every_shape_meshes_consistently_between_strategies() {
        for (name, v) in shapes() {
            let g = greedy(&v);
            let c = culled(&v);
            assert_eq!(
                g.stats.surface_area_m2, c.stats.surface_area_m2,
                "{name}: greedy vs culled area"
            );
            assert_eq!(
                g.stats.exposed_unit_faces, c.stats.exposed_unit_faces,
                "{name}: exposed face count"
            );
            assert_eq!(g.stats.unresolved_halo_faces, 0, "{name}: no open halo");
            assert!(!g.mesh.is_empty(), "{name}: produced geometry");
            assert_eq!(g.mesh.vertices.len(), g.stats.quad_count * 4);
            assert_eq!(g.mesh.indices.len(), g.stats.quad_count * 6);

            let unit_faces = |m: &VolumeMesh| -> std::collections::BTreeSet<_> {
                m.quads
                    .iter()
                    .flat_map(|q| q.unit_faces().collect::<Vec<_>>())
                    .collect()
            };
            assert_eq!(unit_faces(&g), unit_faces(&c), "{name}: unit-face coverage");
        }
    }

    #[test]
    fn cube_greedy_merges_each_of_six_faces_into_one_quad() {
        let g = greedy(&cube([0, 0, 0], 4));
        assert_eq!(g.stats.quad_count, 6);
        assert_eq!(g.stats.exposed_unit_faces, 6 * 16);
        assert!((g.stats.surface_area_m2 - 6.0).abs() < 1e-9);
    }

    #[test]
    fn tunnel_adds_interior_wall_area_over_a_solid_block() {
        let solid = greedy(&cube([0, 0, 0], 6));
        let bored = greedy(&tunnel(6));
        assert!(bored.stats.exposed_unit_faces > solid.stats.exposed_unit_faces);
        let shaft_walls = bored
            .quads
            .iter()
            .filter(|q| {
                matches!(
                    q.dir,
                    FaceDir::PosX | FaceDir::NegX | FaceDir::PosY | FaceDir::NegY
                )
            })
            .filter(|q| (1..6).contains(&q.plane))
            .count();
        assert!(shaft_walls >= 4, "four shaft walls, got {shaft_walls}");
    }

    #[test]
    fn checkerboard_cannot_merge_and_negative_coords_mesh_like_positive() {
        let g = greedy(&checkerboard(8));
        let c = culled(&checkerboard(8));
        assert_eq!(g.stats.quad_count, c.stats.quad_count);
        assert!(g.quads.iter().all(|q| q.u_len == 1 && q.v_len == 1));

        let g_neg = greedy(&negative_corner());
        let c_pos = greedy(&cube([100, 100, 100], 8));
        assert_eq!(
            g_neg.stats.exposed_unit_faces,
            c_pos.stats.exposed_unit_faces
        );
        assert_eq!(g_neg.stats.surface_area_m2, c_pos.stats.surface_area_m2);
    }

    #[test]
    fn a_brick_seam_through_solid_matter_contributes_no_faces() {
        let g = greedy(&adjacent_bricks());
        let seam_faces = g
            .quads
            .iter()
            .filter(|q| matches!(q.dir, FaceDir::PosX | FaceDir::NegX))
            .filter(|q| q.plane == 32)
            .count();
        assert_eq!(seam_faces, 0, "no faces on the interior x = 32 brick seam");
        assert!(g.quads.iter().any(|q| q.material == DIRT));
        assert!(g.quads.iter().any(|q| q.material == STONE));
    }

    #[test]
    fn hollow_box_meshes_an_inner_and_an_outer_shell() {
        let g = greedy(&hollow_box());
        let inner = g
            .quads
            .iter()
            .filter(|q| matches!(q.dir, FaceDir::PosX | FaceDir::NegX))
            .filter(|q| (2..=10).contains(&q.plane))
            .count();
        assert!(inner > 0, "interior faces of the hollow box must be meshed");
        assert!(
            g.quads
                .iter()
                .any(|q| q.dir == FaceDir::NegX && q.plane == 0)
        );
        assert!(
            g.quads
                .iter()
                .any(|q| q.dir == FaceDir::PosX && q.plane == 12)
        );
    }

    // Pinned mesh digests. Regenerate deliberately if geometry or vertex layout
    // changes; a silent change here means the visible mesh moved.
    #[test]
    fn fixture_mesh_digests_are_pinned() {
        if std::env::var("DUMP_MESH_DIGESTS").is_ok() {
            for (name, v) in shapes() {
                println!("{name} = {}", mesh_digest_hex(&greedy(&v).mesh));
            }
        }
        assert_eq!(
            mesh_digest_hex(&greedy(&cube([0, 0, 0], 4)).mesh),
            CUBE_GREEDY
        );
        assert_eq!(mesh_digest_hex(&greedy(&tunnel(6)).mesh), TUNNEL_GREEDY);
        assert_eq!(
            mesh_digest_hex(&greedy(&checkerboard(8)).mesh),
            CHECKERBOARD_GREEDY
        );
        assert_eq!(
            mesh_digest_hex(&greedy(&negative_corner()).mesh),
            NEGATIVE_CORNER_GREEDY
        );
        assert_eq!(
            mesh_digest_hex(&greedy(&adjacent_bricks()).mesh),
            ADJACENT_BRICKS_GREEDY
        );
        assert_eq!(
            mesh_digest_hex(&greedy(&hollow_box()).mesh),
            HOLLOW_BOX_GREEDY
        );
    }

    const CUBE_GREEDY: &str = "4d685c6f28f5c1ab73c4e993df93dc6b0ee706125298d343785e28920e548ae6";
    const TUNNEL_GREEDY: &str = "caa4e00c03d0ee884f7084778971568f51e75df5cf52bed816862bf0fdff0ca8";
    const CHECKERBOARD_GREEDY: &str =
        "5fa70ebbcf7a2b06a7df089928040f111900768ef8b7fc5f626c5add8f60a265";
    const NEGATIVE_CORNER_GREEDY: &str =
        "6f4bed68d248bb5e2959a64f9a60cc7fcbaefab24fd22237d83599981e9e4865";
    const ADJACENT_BRICKS_GREEDY: &str =
        "6c1ab76237e8a5000ba3e539f69ec96a3eed5707043d1a33553a9f84514b25e3";
    const HOLLOW_BOX_GREEDY: &str =
        "f8234cc0407cf60832aa4cae60b84c75b7bca7c6fed11a05ae3560684f618637";
}
