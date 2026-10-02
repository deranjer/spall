//! ENG-122 geometry invariants; not a coupled fluid-solver acceptance gate.
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryError, GeometryLimits};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};
use std::collections::{HashSet, VecDeque};

fn limits() -> GeometryLimits {
    GeometryLimits {
        max_fine_cells: 30_000,
        max_components: 30_000,
        max_portals: 90_000,
    }
}

fn boundary(
    origin: GlobalCell,
    dims: [u32; 3],
    solid: impl Fn(GlobalCell) -> bool,
) -> SolidBoundary {
    let id = VolumeId::new(7).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    let max = GlobalCell::new(
        origin.x + i64::from(dims[0]) - 1,
        origin.y + i64::from(dims[1]) - 1,
        origin.z + i64::from(dims[2]) - 1,
    );
    for bz in origin.z.div_euclid(32)..=max.z.div_euclid(32) {
        for by in origin.y.div_euclid(32)..=max.y.div_euclid(32) {
            for bx in origin.x.div_euclid(32)..=max.x.div_euclid(32) {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut plan = EditPlan::new(id);
    for z in origin.z..=max.z {
        for y in origin.y..=max.y {
            for x in origin.x..=max.x {
                let cell = GlobalCell::new(x, y, z);
                if solid(cell) {
                    plan.set(cell, MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    SolidBoundary::capture(&volume, DomainSpec::new(origin, dims, 30_000).unwrap()).unwrap()
}

fn reachable(g: &CutCellGeometry, start: u32) -> HashSet<u32> {
    let mut found = HashSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some(c) = queue.pop_front() {
        for edge in g.portals() {
            let next = if edge.lower_component == c {
                Some(edge.upper_component)
            } else if edge.upper_component == c {
                Some(edge.lower_component)
            } else {
                None
            };
            if let Some(next) = next
                && found.insert(next)
            {
                queue.push_back(next);
            }
        }
    }
    found
}

#[test]
fn one_voxel_internal_walls_remain_separate_at_factors_three_through_eight() {
    for factor in 3..=8 {
        let fine = boundary(
            GlobalCell::new(0, 0, 0),
            [factor * 2, factor, factor],
            |c| c.x == 1,
        );
        assert_eq!(fine.coarsened(factor).unwrap().solid_cell_count(), 0);
        let g = CutCellGeometry::build(&fine, factor, limits()).unwrap();
        let left = g.component_at(GlobalCell::new(0, 0, 0)).unwrap();
        let right = g.component_at(GlobalCell::new(2, 0, 0)).unwrap();
        assert_ne!(left, right);
        assert!(!reachable(&g, left).contains(&right));
        assert_eq!(g.component_at(GlobalCell::new(1, 0, 0)), None);
        assert_eq!(
            g.components().iter().map(|c| c.voxel_count).sum::<u32>(),
            (2 * factor - 1) * factor * factor
        );
    }
}

#[test]
fn a_one_voxel_hole_reconnects_the_wall_spaces() {
    let fine = boundary(GlobalCell::new(0, 0, 0), [6, 3, 3], |c| {
        c.x == 1 && !(c.y == 1 && c.z == 1)
    });
    let g = CutCellGeometry::build(&fine, 3, limits()).unwrap();
    assert!(
        reachable(&g, g.component_at(GlobalCell::new(0, 1, 1)).unwrap())
            .contains(&g.component_at(GlobalCell::new(5, 1, 1)).unwrap())
    );
    assert_eq!(g.components().len(), 2);
}

#[test]
fn apertures_require_matching_voxels_on_both_sides_of_the_face() {
    let fine = boundary(GlobalCell::new(0, 0, 0), [6, 3, 3], |c| {
        (c.x == 2 && !(c.y == 0 && c.z == 0)) || (c.x == 3 && !(c.y == 1 && c.z == 0))
    });
    assert!(
        CutCellGeometry::build(&fine, 3, limits())
            .unwrap()
            .portals()
            .is_empty()
    );
    let opened = boundary(GlobalCell::new(0, 0, 0), [6, 3, 3], |c| {
        (c.x == 2 || c.x == 3) && !(c.y == 0 && c.z == 0)
    });
    let g = CutCellGeometry::build(&opened, 3, limits()).unwrap();
    assert_eq!(g.portals().len(), 1);
    assert_eq!(g.portals()[0].voxel_faces, 1);
}

#[test]
fn unaligned_submerged_stairs_retain_seed_volume_without_fictitious_air() {
    let fine = boundary(GlobalCell::new(0, 0, 0), [48, 24, 24], |c| {
        c.y < 3 + c.x / 5
    });
    let g = CutCellGeometry::build(&fine, 3, limits()).unwrap();
    let mut amounts = vec![0.0; 48 * 24 * 24];
    for z in 0..24 {
        for y in 0..15 {
            for x in 0..48 {
                if y >= 3 + x / 5 {
                    amounts[x + 48 * (y + 24 * z)] = 1.0;
                }
            }
        }
    }
    let seeded = g.aggregate_amounts(&amounts).unwrap();
    assert_eq!(seeded.iter().sum::<f64>(), amounts.iter().sum::<f64>());
    assert_eq!(seeded.iter().sum::<f64>() * 0.25_f64.powi(3), 138.375);
    for (c, amount) in g.components().iter().zip(seeded) {
        if amount > 0.0 {
            assert_eq!(amount, f64::from(c.voxel_count));
        }
    }
}

#[test]
fn horizontal_partial_surface_uses_exact_open_volume_at_each_height() {
    let fine = boundary(GlobalCell::new(0, 0, 0), [3, 3, 3], |c| {
        c.x == 0 && c.y == 0
    });
    let g = CutCellGeometry::build(&fine, 3, limits()).unwrap();
    let c = &g.components()[0];
    assert_eq!(c.voxel_count, 24);
    assert_eq!(c.layer_counts, [6, 9, 9, 0, 0, 0, 0, 0]);
    assert_eq!(c.volume_below(-1.0), 0.0);
    assert_eq!(c.volume_below(0.5), 3.0);
    assert_eq!(c.volume_below(1.25), 8.25);
    assert_eq!(c.volume_below(9.0), 24.0);
    assert_eq!(c.centroid(), [1.625, 1.625, 1.5]);
}

#[test]
fn graph_matches_fine_connectivity_across_negative_brick_coordinates() {
    let origin = GlobalCell::new(-2, -2, -2);
    // Exhaust eight occupancy decisions, extruded along Z. This covers walls,
    // holes, isolated spaces and diagonal contact across a brick boundary.
    for mask in 0..256_u32 {
        let fine = boundary(origin, [4, 2, 2], |c| {
            mask & (1 << ((c.x + 2) + 4 * (c.y + 2))) != 0
        });
        let g = CutCellGeometry::build(&fine, 2, limits()).unwrap();
        assert_eq!(g, CutCellGeometry::build(&fine, 2, limits()).unwrap());
        let mut open = HashSet::new();
        for z in -2..0 {
            for y in -2..0 {
                for x in -2..2 {
                    let c = GlobalCell::new(x, y, z);
                    if fine.is_solid(c) == Some(false) {
                        open.insert(c);
                    }
                }
            }
        }
        assert_eq!(
            g.components()
                .iter()
                .map(|c| c.voxel_count as usize)
                .sum::<usize>(),
            open.len()
        );
        let mut remaining = open.clone();
        while let Some(&start) = remaining.iter().next() {
            let mut fine_reachable = HashSet::from([start]);
            let mut frontier = vec![start];
            while let Some(c) = frontier.pop() {
                for (dx, dy, dz) in [
                    (1, 0, 0),
                    (-1, 0, 0),
                    (0, 1, 0),
                    (0, -1, 0),
                    (0, 0, 1),
                    (0, 0, -1),
                ] {
                    let next = GlobalCell::new(c.x + dx, c.y + dy, c.z + dz);
                    if open.contains(&next) && fine_reachable.insert(next) {
                        frontier.push(next);
                    }
                }
            }
            let connected = reachable(&g, g.component_at(start).unwrap());
            for &c in &open {
                assert_eq!(
                    connected.contains(&g.component_at(c).unwrap()),
                    fine_reachable.contains(&c),
                    "mask={mask}, cell={c:?}"
                );
            }
            remaining.retain(|c| !fine_reachable.contains(c));
        }
    }
}

#[test]
fn invalid_amounts_and_build_limits_are_errors_not_truncated_results() {
    let fine = boundary(GlobalCell::new(0, 0, 0), [6, 3, 3], |c| c.x == 1);
    assert!(matches!(
        CutCellGeometry::build(&fine, 0, limits()),
        Err(GeometryError::InvalidFactor(0))
    ));
    assert!(matches!(
        CutCellGeometry::build(&fine, 2, limits()),
        Err(GeometryError::Domain(_))
    ));
    for (field, limit) in [
        (
            "fine cells",
            GeometryLimits {
                max_fine_cells: 53,
                ..limits()
            },
        ),
        (
            "components",
            GeometryLimits {
                max_components: 2,
                ..limits()
            },
        ),
        (
            "portals",
            GeometryLimits {
                max_portals: 0,
                ..limits()
            },
        ),
    ] {
        assert!(
            matches!(CutCellGeometry::build(&fine, 3, limit), Err(GeometryError::Limit { kind, .. }) if kind == field)
        );
    }
    let g = CutCellGeometry::build(&fine, 3, limits()).unwrap();
    assert!(g.aggregate_amounts(&[]).is_err());
    for value in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let mut amounts = vec![0.0; 54];
        amounts[0] = value;
        assert!(g.aggregate_amounts(&amounts).is_err());
    }
    let mut amounts = vec![0.0; 54];
    amounts[1] = 0.5;
    assert!(g.aggregate_amounts(&amounts).is_err());
}
