//! Per-pass broad phase for dormancy proximity. Leaves use the original sphere
//! predicate; the index only skips groups whose closest possible gap is too far.

use super::{ActiveRegion, sphere_gap};

const LEAF_SIZE: usize = 8;

pub(super) struct RegionIndex<'a> {
    regions: &'a [ActiveRegion],
    order: Vec<usize>,
    nodes: Vec<Node>,
}

struct Node {
    min: [f64; 3],
    max: [f64; 3],
    max_radius: f64,
    start: usize,
    end: usize,
    children: Option<(usize, usize)>,
}

impl<'a> RegionIndex<'a> {
    pub(super) fn new(regions: &'a [ActiveRegion]) -> Self {
        let mut index = Self {
            regions,
            order: Vec::new(),
            nodes: Vec::new(),
        };
        // Small passes need no tree. Preserve the linear predicate's behavior
        // for non-finite inputs too, rather than pruning with invalid bounds.
        if regions.len() > LEAF_SIZE
            && regions
                .iter()
                .all(|r| r.centre_m.iter().all(|v| v.is_finite()) && r.radius_m.is_finite())
        {
            index.order.extend(0..regions.len());
            index.build(0, regions.len());
        }
        index
    }

    fn build(&mut self, start: usize, end: usize) -> usize {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        let mut max_radius = f64::NEG_INFINITY;
        for &i in &self.order[start..end] {
            let region = &self.regions[i];
            for axis in 0..3 {
                min[axis] = min[axis].min(region.centre_m[axis]);
                max[axis] = max[axis].max(region.centre_m[axis]);
            }
            max_radius = max_radius.max(region.radius_m);
        }
        let node = self.nodes.len();
        self.nodes.push(Node {
            min,
            max,
            max_radius,
            start,
            end,
            children: None,
        });
        if end - start > LEAF_SIZE {
            let axis = (0..3)
                .max_by(|&a, &b| (max[a] - min[a]).total_cmp(&(max[b] - min[b])))
                .unwrap();
            let mid = start + (end - start) / 2;
            self.order[start..end].select_nth_unstable_by(mid - start, |&a, &b| {
                self.regions[a].centre_m[axis]
                    .total_cmp(&self.regions[b].centre_m[axis])
                    .then(a.cmp(&b))
            });
            let left = self.build(start, mid);
            let right = self.build(mid, end);
            self.nodes[node].children = Some((left, right));
        }
        node
    }

    pub(super) fn any_near(&self, centre: [f64; 3], radius: f64, margin: f64) -> bool {
        if self.nodes.is_empty()
            || !centre.iter().all(|v| v.is_finite())
            || !radius.is_finite()
            || !margin.is_finite()
        {
            return self
                .regions
                .iter()
                .any(|r| sphere_gap(centre, radius, r.centre_m, r.radius_m) <= margin);
        }
        self.query(0, centre, radius, margin)
    }

    fn query(&self, node: usize, centre: [f64; 3], radius: f64, margin: f64) -> bool {
        let node = &self.nodes[node];
        let closest =
            std::array::from_fn(|axis| centre[axis].clamp(node.min[axis], node.max[axis]));
        // This lower bound uses the same arithmetic order as the exact test.
        // Every centre is at least this far away and no radius exceeds the max.
        if sphere_gap(centre, radius, closest, node.max_radius) > margin {
            return false;
        }
        if let Some((left, right)) = node.children {
            self.query(left, centre, radius, margin) || self.query(right, centre, radius, margin)
        } else {
            self.order[node.start..node.end].iter().any(|&i| {
                let r = &self.regions[i];
                sphere_gap(centre, radius, r.centre_m, r.radius_m) <= margin
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(regions: &[ActiveRegion], centre: [f64; 3], radius: f64, margin: f64) -> bool {
        regions
            .iter()
            .any(|r| sphere_gap(centre, radius, r.centre_m, r.radius_m) <= margin)
    }

    #[test]
    fn indexed_proximity_matches_linear_across_scales_and_input_order() {
        let mut seed = 0x5a11_u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 11) as f64 / (1_u64 << 53) as f64
        };
        let mut regions = Vec::new();
        for i in 0..257 {
            regions.push(ActiveRegion {
                centre_m: std::array::from_fn(|_| (random() - 0.5) * 500.0),
                radius_m: if i % 31 == 0 { 40.0 } else { random() },
            });
        }
        let mut queries = Vec::new();
        for _ in 0..4096 {
            queries.push((
                std::array::from_fn(|_| (random() - 0.5) * 600.0),
                random() * 10.0,
                random() * 8.0,
            ));
        }
        // Surface equality, just inside/outside it, self inclusion, and a large
        // body whose centre is distant but whose surface reaches a region.
        for r in &regions {
            for delta in [-1.0e-9, 0.0, 1.0e-9] {
                queries.push((
                    [
                        r.centre_m[0] + r.radius_m + 4.5 + delta,
                        r.centre_m[1],
                        r.centre_m[2],
                    ],
                    0.5,
                    4.0,
                ));
            }
            queries.push((r.centre_m, 0.5, 4.0));
        }
        queries.push(([1000.0, 0.0, 0.0], 1100.0, 4.0));
        queries.push(([0.0, -30_000.0, 0.0], 0.5, 4.0));
        let expected: Vec<_> = queries
            .iter()
            .map(|&(c, r, m)| linear(&regions, c, r, m))
            .collect();
        assert!(expected.contains(&true) && expected.contains(&false));
        for _ in 0..2 {
            let index = RegionIndex::new(&regions);
            for (&(centre, radius, margin), &near) in queries.iter().zip(&expected) {
                assert_eq!(
                    index.any_near(centre, radius, margin),
                    near,
                    "query {centre:?}, radius {radius}, margin {margin}"
                );
            }
            regions.reverse();
        }
    }

    #[test]
    fn empty_small_and_nonfinite_inputs_preserve_linear_behavior() {
        let mut regions = vec![
            ActiveRegion {
                centre_m: [0.0; 3],
                radius_m: 0.5
            };
            17
        ];
        for len in [0, 1, 8, 17] {
            let regions = &regions[..len];
            let index = RegionIndex::new(regions);
            for centre in [[0.0; 3], [20.0; 3], [f64::NAN; 3], [f64::INFINITY; 3]] {
                for radius in [-1.0, 0.5, f64::INFINITY, f64::NAN] {
                    for margin in [-1.0, 4.0, f64::INFINITY, f64::NAN] {
                        assert_eq!(
                            index.any_near(centre, radius, margin),
                            linear(regions, centre, radius, margin)
                        );
                    }
                }
            }
        }
        regions[0].centre_m[0] = f64::NAN;
        regions[1].radius_m = f64::INFINITY;
        let index = RegionIndex::new(&regions);
        assert_eq!(
            index.any_near([10.0; 3], 0.5, 4.0),
            linear(&regions, [10.0; 3], 0.5, 4.0)
        );
    }

    #[test]
    #[ignore = "release-mode proximity scaling measurement; not a G4 gate"]
    fn measure_4352_body_proximity_against_linear() {
        use std::{hint::black_box, time::Instant};
        let regions: Vec<_> = (0..4352)
            .map(|i| ActiveRegion {
                centre_m: [(i % 64) as f64 * 2.0, -1000.0, (i / 64) as f64 * 2.0],
                radius_m: 0.5,
            })
            .collect();
        let mut old = Vec::new();
        let mut indexed = Vec::new();
        for _ in 0..60 {
            let start = Instant::now();
            let expected: Vec<_> = regions
                .iter()
                .map(|r| linear(black_box(&regions), r.centre_m, r.radius_m, 4.0))
                .collect();
            old.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            let index = RegionIndex::new(black_box(&regions));
            let actual: Vec<_> = regions
                .iter()
                .map(|r| index.any_near(r.centre_m, r.radius_m, 4.0))
                .collect();
            indexed.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(actual, expected);
        }
        old.sort_by(f64::total_cmp);
        indexed.sort_by(f64::total_cmp);
        println!(
            "4352 bodies / 4352 regions, 60 samples, build included: linear p95={} ms; indexed p95={} ms",
            old[56], indexed[56]
        );
    }
}

