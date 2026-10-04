//! Liquid control volumes clipped by the same PLIC plane as water transport.
//! Embedded atmospheric faces are geometric boundary DOFs, not pressure anchors.
use super::{InterfacePlane, MacError, MacGridWorld, ProjectionStats, sample_velocity};

type Point = [f64; 3];
fn add(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[i] + b[i])
}
fn sub(a: Point, b: Point) -> Point {
    std::array::from_fn(|i| a[i] - b[i])
}
fn scale(a: Point, k: f64) -> Point {
    a.map(|v| v * k)
}
fn dot(a: Point, b: Point) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
fn cross(a: Point, b: Point) -> Point {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[derive(Clone, Default)]
struct Patch {
    points: Vec<Point>,
    area: f64,
    center: Point,
}
impl Patch {
    fn new(points: Vec<Point>) -> Self {
        let mut result = Self {
            points,
            ..Self::default()
        };
        if result.points.len() < 3 {
            return result;
        }
        let a = result.points[0];
        let mut triangles = Vec::new();
        for j in 1..result.points.len() - 1 {
            let (b, c) = (result.points[j], result.points[j + 1]);
            let v = cross(sub(b, a), sub(c, a));
            let area = v[0].hypot(v[1]).hypot(v[2]) * 0.5;
            result.area += area;
            triangles.push((area, scale(add(add(a, b), c), 1.0 / 3.0)));
        }
        if result.area > 0.0 {
            result.center = triangles.into_iter().fold([0.0; 3], |center, (area, p)| {
                add(center, scale(p, area / result.area))
            });
        }
        result
    }
}
fn square(axis: usize, side: f64) -> Vec<Point> {
    [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]
        .map(|(a, b)| {
            let mut p = [0.0; 3];
            p[axis] = side;
            p[(axis + 1) % 3] = a;
            p[(axis + 2) % 3] = b;
            p
        })
        .to_vec()
}
fn clip(points: &[Point], plane: InterfacePlane, crossings: &mut Vec<Point>) -> Vec<Point> {
    let mut out = Vec::new();
    if points.is_empty() {
        return out;
    }
    let mut a = *points.last().unwrap();
    let mut da = dot(plane.normal, a) - plane.alpha;
    for &b in points {
        let db = dot(plane.normal, b) - plane.alpha;
        if (da <= 0.0) != (db <= 0.0) {
            let p = if da <= 0.0 {
                add(a, scale(sub(b, a), da / (da - db)))
            } else {
                add(b, scale(sub(a, b), db / (db - da)))
            };
            out.push(p);
            crossings.push(p);
        }
        if db <= 0.0 {
            out.push(b);
        }
        a = b;
        da = db;
    }
    out
}
struct Cell {
    center: Point,
    faces: [Patch; 6],
    cap: Patch,
    normal: Point,
}
fn geometry(original: Option<InterfacePlane>, fraction: f64) -> Result<Cell, MacError> {
    // Reflect into the positive corner before clipping: a signed plane offset
    // cannot retain a tiny positive fragment beside a negative unit offset.
    let plane =
        original.map(|p| InterfacePlane::from_fraction_resolved(p.normal.map(f64::abs), fraction));
    let mut cuts = Vec::new();
    let mut faces = std::array::from_fn(|i| {
        let p = square(i / 2, (i % 2) as f64);
        Patch::new(plane.map_or_else(|| p.clone(), |q| clip(&p, q, &mut cuts)))
    });
    let mut normal = [0.0; 3];
    if let Some(p) = plane {
        normal = scale(p.normal, 1.0 / dot(p.normal, p.normal).sqrt());
        // Exact duplicate removal; no positive-size fragment is thresholded.
        cuts.sort_by(|a, b| {
            a[0].total_cmp(&b[0])
                .then(a[1].total_cmp(&b[1]))
                .then(a[2].total_cmp(&b[2]))
        });
        cuts.dedup();
        if !cuts.is_empty() {
            let center = scale(
                cuts.iter().copied().fold([0.0; 3], add),
                1.0 / cuts.len() as f64,
            );
            let axis = (0..3)
                .min_by(|&a, &b| normal[a].abs().total_cmp(&normal[b].abs()))
                .unwrap();
            let mut e = [0.0; 3];
            e[axis] = 1.0;
            let u = cross(normal, e);
            let v = cross(normal, u);
            cuts.sort_by(|a, b| {
                let a = sub(*a, center);
                let b = sub(*b, center);
                dot(a, v)
                    .atan2(dot(a, u))
                    .total_cmp(&dot(b, v).atan2(dot(b, u)))
            });
        }
    }
    let mut cap = Patch::new(cuts);
    let patches = faces.iter().chain(std::iter::once(&cap));
    let all: Vec<_> = patches
        .clone()
        .flat_map(|p| p.points.iter().copied())
        .collect();
    if all.is_empty() {
        return Err(MacError::InvalidConfig);
    }
    let reference = scale(
        all.iter().copied().fold([0.0; 3], add),
        1.0 / all.len() as f64,
    );
    let mut volume = 0.0;
    let mut tetrahedra = Vec::new();
    // Normalize each axis independently before computing tetrahedron weights.
    // A single largest extent leaves a subnormal slab's determinant underflowing.
    // All weights share the same affine determinant, so centroids are unchanged.
    let extent: Point = std::array::from_fn(|a| {
        all.iter()
            .map(|p| (p[a] - reference[a]).abs())
            .fold(0.0, f64::max)
    });
    if extent.iter().any(|v| *v <= 0.0) {
        return Err(MacError::InvalidConfig);
    }
    let normalized = |p: Point| std::array::from_fn(|a| (p[a] - reference[a]) / extent[a]);
    for patch in patches {
        if patch.points.len() < 3 {
            continue;
        }
        let a = patch.points[0];
        for j in 1..patch.points.len() - 1 {
            let (b, c) = (patch.points[j], patch.points[j + 1]);
            let tetra = dot(normalized(a), cross(normalized(b), normalized(c))).abs() / 6.0;
            volume += tetra;
            tetrahedra.push((tetra, scale(add(add(reference, a), add(b, c)), 0.25)));
        }
    }
    if volume <= 0.0 || !volume.is_finite() {
        return Err(MacError::InvalidConfig);
    }
    let mut center = tetrahedra
        .into_iter()
        .fold([0.0; 3], |center, (v, p)| add(center, scale(p, v / volume)));
    if let Some(p) = original {
        for axis in 0..3 {
            if p.normal[axis] < 0.0 {
                center[axis] = 1.0 - center[axis];
                for patch in faces.iter_mut().chain(std::iter::once(&mut cap)) {
                    patch.center[axis] = 1.0 - patch.center[axis];
                    for point in &mut patch.points {
                        point[axis] = 1.0 - point[axis];
                    }
                }
                faces.swap(2 * axis, 2 * axis + 1);
            }
        }
        normal = scale(p.normal, 1.0 / dot(p.normal, p.normal).sqrt());
    }
    Ok(Cell {
        center,
        faces,
        cap,
        normal,
    })
}

struct Edge {
    a: usize,
    b: usize,
    axis: usize,
    face: usize,
    area: f64,
    distance: f64,
    velocity: f64,
}
struct Free {
    row: usize,
    air: Option<usize>,
    axis: Option<usize>,
    face: usize,
    sign: f64,
    area: f64,
    distance: f64,
    velocity: f64,
    potential: f64,
}
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}
fn gauge(v: &mut [f64], labels: &[usize], closed: &[bool]) {
    let mut sum = vec![0.0; v.len()];
    let mut count = vec![0usize; v.len()];
    for (i, &c) in labels.iter().enumerate() {
        if closed[c] {
            sum[c] += v[i];
            count[c] += 1;
        }
    }
    for (i, &c) in labels.iter().enumerate() {
        if closed[c] {
            v[i] -= sum[c] / count[c] as f64;
        }
    }
}
fn apply(x: &[f64], diagonal: &[f64], edges: &[Edge], h2: f64, out: &mut [f64]) {
    for i in 0..x.len() {
        out[i] = diagonal[i] * x[i];
    }
    for e in edges {
        let k = e.area / e.distance / h2;
        out[e.a] -= k * x[e.b];
        out[e.b] -= k * x[e.a];
    }
}
fn norm(v: &[f64]) -> f64 {
    v.iter().map(|v| v * v).sum::<f64>().sqrt()
}

// Symmetric Gauss-Seidel: (D+L) D^-1 (D+L)^T. Unlike a directional
// relaxation, this is a symmetric positive preconditioner for the actual
// liquid graph, including its small geometric interfaces and closed gauges.
fn precondition(
    residual: &[f64],
    diagonal: &[f64],
    lower: &[Vec<(usize, f64)>],
    upper: &[Vec<(usize, f64)>],
    out: &mut [f64],
) {
    for i in 0..out.len() {
        out[i] = if diagonal[i] > 0.0 {
            (residual[i] + lower[i].iter().map(|(j, k)| k * out[*j]).sum::<f64>()) / diagonal[i]
        } else {
            0.0
        };
    }
    for i in (0..out.len()).rev() {
        if diagonal[i] > 0.0 {
            out[i] += upper[i].iter().map(|(j, k)| k * out[*j]).sum::<f64>() / diagonal[i];
        }
    }
}

pub(super) fn project(grid: &mut MacGridWorld, dt: f64) -> Result<ProjectionStats, MacError> {
    let dims = grid.dims();
    let h = grid.config.cell_size_m;
    let rho = grid.config.density_kg_m3;
    let planes = grid.reconstruct_planes();
    let mut rows = vec![None; grid.fraction.len()];
    let mut cells = Vec::new();
    let mut indices = Vec::new();
    let mut centers = Vec::new();
    for (i, row) in rows.iter_mut().enumerate() {
        if grid.solid[i] || grid.fraction[i] <= 0.0 {
            continue;
        }
        *row = Some(cells.len());
        indices.push(i);
        let cell = geometry(planes[i], grid.fraction[i]).inspect_err(|_| {
            if grid.config.pressure_diagnostics {
                eprintln!(
                    "{{\"type\":\"cut_geometry_failure\",\"cell\":{i},\"fraction\":{:.17e},\"plane_normal\":{:?},\"plane_alpha\":{:.17e}}}",
                    grid.fraction[i],
                    planes[i].map_or([0.0; 3], |p| p.normal),
                    planes[i].map_or(0.0, |p| p.alpha)
                );
            }
        })?;
        let p = [i % dims[0], i / dims[0] % dims[1], i / (dims[0] * dims[1])];
        centers.push(std::array::from_fn(|a| p[a] as f64 + cell.center[a]));
        cells.push(cell);
    }
    let n = cells.len();
    if grid.cut_surface_velocity[0].len() != grid.fraction.len() {
        // The embedded correction is a reconstruction DOF. Keep its corrected
        // normal velocity rather than recreating it from extended Cartesian
        // faces, which can feed an underresolved cap error back into pressure.
        grid.cut_surface_velocity = std::array::from_fn(|_| vec![0.0; grid.fraction.len()]);
        for (r, &i) in indices.iter().enumerate() {
            let u = sample_velocity(&grid.u, &grid.v, &grid.w, dims, centers[r]);
            for (axis, v) in u.into_iter().enumerate() {
                grid.cut_surface_velocity[axis][i] = v;
            }
        }
    }
    let mut edges = Vec::new();
    let mut free = Vec::new();
    let mut known: [Vec<bool>; 3] = std::array::from_fn(|a| {
        let mut d = dims;
        d[a] += 1;
        vec![false; d.iter().product()]
    });
    let mut area: [Vec<f64>; 3] = std::array::from_fn(|a| vec![0.0; known[a].len()]);
    let mut face_mass: [Vec<f64>; 3] = std::array::from_fn(|a| vec![0.0; known[a].len()]);
    let potential = |p: Point| -rho * h * dot(grid.config.gravity_m_s2, p);
    // The same distance regularization as the existing ghost-fluid theta floor;
    // shared by matrix, pressure impulse and face inertia. Geometry/amounts
    // and the existence of a free boundary are never fabricated by this floor.
    let distance = |d: f64| d.max(0.01);
    for (r, cell) in cells.iter().enumerate() {
        if cell.cap.area > 0.0 {
            let center = add(sub(centers[r], cell.center), cell.cap.center);
            let u = std::array::from_fn(|axis| grid.cut_surface_velocity[axis][indices[r]]);
            free.push(Free {
                row: r,
                air: Some(indices[r]),
                axis: None,
                face: 0,
                sign: 1.0,
                area: cell.cap.area,
                distance: distance(grid.fraction[indices[r]] / (2.0 * cell.cap.area)),
                velocity: dot(u, cell.normal),
                potential: potential(center),
            });
        }
    }
    for axis in 0..3 {
        let mut ext = dims;
        ext[axis] += 1;
        for fi in 0..known[axis].len() {
            let p = [fi % ext[0], fi / ext[0] % ext[1], fi / (ext[0] * ext[1])];
            let mut lo = p;
            let a = if p[axis] > 0 {
                lo[axis] -= 1;
                Some(grid.cell_index(lo[0], lo[1], lo[2]))
            } else {
                None
            };
            let b = (p[axis] < dims[axis]).then(|| grid.cell_index(p[0], p[1], p[2]));
            if a.is_some_and(|i| grid.solid[i]) || b.is_some_and(|i| grid.solid[i]) {
                continue;
            }
            if (a.is_none() || b.is_none())
                && !(axis == 1 && p[axis] == dims[axis] && grid.config.open_top)
            {
                continue;
            }
            let ar = a.and_then(|i| rows[i]);
            let br = b.and_then(|i| rows[i]);
            face_mass[axis][fi] =
                a.map_or(0.0, |i| grid.fraction[i]) + b.map_or(0.0, |i| grid.fraction[i]);
            let u = match axis {
                0 => grid.u[fi],
                1 => grid.v[fi],
                _ => grid.w[fi],
            };
            let mut overlap = Patch::default();
            if let (Some(l), Some(r)) = (ar, br) {
                let face = &cells[l].faces[axis * 2 + 1];
                overlap = if let Some(mut plane) = planes[b.unwrap()] {
                    // Neighbour local coordinates = left coordinates - e_axis.
                    plane.alpha += plane.normal[axis];
                    Patch::new(clip(&face.points, plane, &mut Vec::new()))
                } else {
                    face.clone()
                };
                // Keep roundoff in polygon quadrature from making a negative
                // exposed area; no water fraction is adjusted here.
                overlap.area = overlap
                    .area
                    .min(face.area)
                    .min(cells[r].faces[axis * 2].area);
                if overlap.area > 0.0 {
                    edges.push(Edge {
                        a: l,
                        b: r,
                        axis,
                        face: fi,
                        area: overlap.area,
                        distance: distance(centers[r][axis] - centers[l][axis]),
                        velocity: u,
                    });
                }
            }
            for (row, side, sign) in [(ar, 1, 1.0), (br, 0, -1.0)] {
                let Some(r) = row else {
                    continue;
                };
                let face = &cells[r].faces[axis * 2 + side];
                let neighbour = if side == 1 { b } else { a };
                let neighbour_row = if side == 1 { br } else { ar };
                // Clip the exposed polygon itself. Subtracting almost equal
                // area moments can put a tiny remainder's centroid far
                // outside the cell and create an enormous pressure impulse.
                let exposed = if neighbour_row.is_none() {
                    face.clone()
                } else if let Some(mut plane) = planes[neighbour.unwrap()] {
                    plane.alpha += if side == 1 {
                        plane.normal[axis]
                    } else {
                        -plane.normal[axis]
                    };
                    plane.normal = plane.normal.map(|v| -v);
                    plane.alpha = -plane.alpha;
                    Patch::new(clip(&face.points, plane, &mut Vec::new()))
                } else {
                    Patch::default()
                };
                area[axis][fi] += exposed.area;
                if exposed.area <= 0.0 {
                    continue;
                }
                let center = exposed.center;
                let local = add(sub(centers[r], cells[r].center), center);
                free.push(Free {
                    row: r,
                    air: if side == 1 { b } else { a },
                    axis: Some(axis),
                    face: fi,
                    sign,
                    area: exposed.area,
                    distance: distance((center[axis] - cells[r].center[axis]).abs()),
                    velocity: sign * u,
                    potential: potential(local),
                });
            }
            area[axis][fi] += overlap.area;
            known[axis][fi] = area[axis][fi] > 0.0;
        }
    }
    // Use the same half-cell liquid mass as accepted-transfer momentum.
    // A face fragment receives its share of that mass by wetted area.
    for e in &mut edges {
        e.distance = distance(
            (grid.fraction[indices[e.a]] + grid.fraction[indices[e.b]])
                / (2.0 * area[e.axis][e.face]),
        );
    }
    for f in &mut free {
        if let Some(axis) = f.axis {
            f.distance = distance(face_mass[axis][f.face] / (2.0 * area[axis][f.face]));
        }
    }
    let mut parent: Vec<_> = (0..n).collect();
    for e in &edges {
        let a = root(&mut parent, e.a);
        let b = root(&mut parent, e.b);
        parent[a] = b;
    }
    let labels: Vec<_> = (0..n).map(|i| root(&mut parent, i)).collect();
    let reference = free.first().map_or(0.0, |f| f.potential);
    // Positive outward flow can consume only the receiving cell's actual air
    // volume. A closing interface then has prescribed displacement flux and
    // unknown contact pressure instead of atmospheric Dirichlet pressure.
    // Include Cartesian patches as well as embedded caps (both sides of a gap).
    let mut lower = vec![Vec::new(); n];
    let mut upper = vec![Vec::new(); n];
    for e in &edges {
        let (a, b) = if e.a < e.b { (e.a, e.b) } else { (e.b, e.a) };
        let k = e.area / e.distance / (h * h);
        lower[b].push((a, k));
        upper[a].push((b, k));
    }
    let mut held = vec![None; free.len()];
    let mut attempts = 0;
    let mut total_iterations = 0;
    let mut warm = grid.pressure_pa.iter().any(|p| *p != 0.0).then(|| {
        indices
            .iter()
            .enumerate()
            .map(|(r, i)| grid.pressure_pa[*i] + potential(centers[r]) - reference)
            .collect::<Vec<_>>()
    });
    let (phi, initial, final_residual, before) = loop {
        let mut closed = vec![true; n];
        for (j, f) in free.iter().enumerate() {
            if held[j].is_none() {
                closed[labels[f.row]] = false;
            }
        }
        let mut diagonal = vec![0.0; n];
        let mut rhs = vec![0.0; n];
        let mut flux = vec![0.0; n];
        for e in &edges {
            let k = e.area / e.distance / h.powi(2);
            diagonal[e.a] += k;
            diagonal[e.b] += k;
            flux[e.a] += e.area * e.velocity;
            flux[e.b] -= e.area * e.velocity;
        }
        for (j, f) in free.iter().enumerate() {
            if let Some(q) = held[j] {
                flux[f.row] += q;
                continue;
            }
            let k = f.area / f.distance / h.powi(2);
            diagonal[f.row] += k;
            rhs[f.row] += k * (f.potential - reference);
            flux[f.row] += f.area * f.velocity;
        }
        let before = flux
            .iter()
            .enumerate()
            .map(|(r, v)| v.abs() / h / grid.fraction[indices[r]])
            .fold(0.0, f64::max);
        for r in 0..n {
            rhs[r] -= rho / dt / h * flux[r];
        }
        let mut sums = vec![0.0; n];
        let mut counts = vec![0usize; n];
        for (r, &c) in labels.iter().enumerate() {
            if closed[c] {
                sums[c] += rhs[r];
                counts[c] += 1;
            }
        }
        for c in 0..n {
            if counts[c] > 0 && (sums[c] / counts[c] as f64).abs() > 1e-7 {
                return Err(MacError::IncompatibleEnclosedPressureRegion);
            }
        }
        gauge(&mut rhs, &labels, &closed);
        let mut phi: Vec<_> = warm.take().unwrap_or_else(|| {
            rhs.iter()
                .zip(&diagonal)
                .map(|(r, d)| if *d > 0.0 { r / d } else { 0.0 })
                .collect()
        });
        gauge(&mut phi, &labels, &closed);
        let mut product = vec![0.0; n];
        apply(&phi, &diagonal, &edges, h * h, &mut product);
        let mut residual: Vec<_> = rhs.iter().zip(&product).map(|(b, a)| b - a).collect();
        gauge(&mut residual, &labels, &closed);
        let initial = norm(&residual);
        let target = grid
            .config
            .pressure_absolute_tolerance
            .max(grid.config.pressure_relative_tolerance * norm(&rhs))
            // For a full cell (no moving embedded face), pressure residual
            // changes its amount by dt^2/rho * residual. Reserve two orders
            // of margin inside the unchanged 1e-10 donor bound.
            .min(1e-12 * rho / dt.powi(2));
        let mut z = vec![0.0; n];
        precondition(&residual, &diagonal, &lower, &upper, &mut z);
        gauge(&mut z, &labels, &closed);
        let mut direction = z.clone();
        let mut rz: f64 = residual.iter().zip(&z).map(|(r, z)| r * z).sum();
        let mut iterations = 0;
        while norm(&residual) > target
            && iterations + total_iterations < grid.config.pressure_max_iterations
        {
            apply(&direction, &diagonal, &edges, h * h, &mut product);
            let dp: f64 = direction.iter().zip(&product).map(|(a, b)| a * b).sum();
            if !dp.is_finite() || dp <= 0.0 {
                return Err(MacError::MomentumInvalidState);
            }
            let alpha = rz / dp;
            for i in 0..n {
                phi[i] += alpha * direction[i];
                residual[i] -= alpha * product[i];
            }
            gauge(&mut residual, &labels, &closed);
            // Recursive CG residuals can lose the remaining correction through
            // cancellation. Verify apparent convergence against the actual
            // operator and restart from that residual within the same budget.
            let reliable = norm(&residual) <= target;
            if reliable {
                apply(&phi, &diagonal, &edges, h * h, &mut product);
                for i in 0..n {
                    residual[i] = rhs[i] - product[i];
                }
                gauge(&mut residual, &labels, &closed);
            }
            precondition(&residual, &diagonal, &lower, &upper, &mut z);
            gauge(&mut z, &labels, &closed);
            let next: f64 = residual.iter().zip(&z).map(|(a, b)| a * b).sum();
            let beta = if reliable { 0.0 } else { next / rz };
            for i in 0..n {
                direction[i] = z[i] + beta * direction[i];
            }
            rz = next;
            iterations += 1;
        }
        apply(&phi, &diagonal, &edges, h * h, &mut product);
        for i in 0..n {
            residual[i] = rhs[i] - product[i];
        }
        gauge(&mut residual, &labels, &closed);
        let final_residual = norm(&residual);
        if final_residual > target * 1.01 {
            return Err(MacError::MomentumPressureNotConverged {
                residual: final_residual,
            });
        }
        total_iterations += iterations;
        let mut changed = false;
        for (j, f) in free.iter().enumerate() {
            let Some(air) = f.air else {
                continue;
            };
            let q = f.area
                * (f.velocity - dt / rho / h / f.distance * (f.potential - reference - phi[f.row]));
            let capacity = (1.0 - grid.fraction[air]) * h / dt;
            let constraint = if q > capacity { Some(capacity) } else { None };
            if held[j] != constraint {
                held[j] = constraint;
                changed = true;
            }
        }
        if !changed {
            break (phi, initial, final_residual, before);
        }
        warm = Some(phi);
        attempts += 1;
        if attempts > 32 || total_iterations > grid.config.pressure_max_iterations {
            return Err(MacError::MomentumPressureNotConverged {
                residual: final_residual,
            });
        }
    };
    grid.pressure_pa.fill(0.0);
    for r in 0..n {
        grid.pressure_pa[indices[r]] = phi[r] + reference - potential(centers[r]);
    }
    grid.cut_surface_flux = std::array::from_fn(|a| vec![0.0; known[a].len()]);
    let mut flux = vec![0.0; n];
    for e in &edges {
        let u = e.velocity - dt / rho / h / e.distance * (phi[e.b] - phi[e.a]);
        let q = e.area * u;
        grid.cut_surface_flux[e.axis][e.face] += q * h * h;
        flux[e.a] += q;
        flux[e.b] -= q;
    }
    for (j, f) in free.iter().enumerate() {
        let u = held[j].map_or_else(
            || f.velocity - dt / rho / h / f.distance * (f.potential - reference - phi[f.row]),
            |q| q / f.area,
        );
        flux[f.row] += f.area * u;
        if f.axis.is_none() {
            // Retain the normal correction while preserving tangential state.
            let i = indices[f.row];
            for axis in 0..3 {
                grid.cut_surface_velocity[axis][i] += cells[f.row].normal[axis] * (u - f.velocity);
            }
        }
        if let Some(axis) = f.axis {
            grid.cut_surface_flux[axis][f.face] += f.sign * f.area * u * h * h;
        }
    }
    for (axis, wetted) in area.iter().enumerate() {
        let velocities = match axis {
            0 => &mut grid.u,
            1 => &mut grid.v,
            _ => &mut grid.w,
        };
        for (i, u) in velocities.iter_mut().enumerate() {
            *u = if wetted[i] > 0.0 {
                grid.cut_surface_flux[axis][i] / (wetted[i] * h * h)
            } else {
                0.0
            };
        }
    }
    // One shared signed transfer per Cartesian face; no atmospheric inflow.
    if grid.config.open_top {
        for z in 0..dims[2] {
            for x in 0..dims[0] {
                let i = grid.v_index(x, dims[1], z);
                grid.cut_surface_flux[1][i] = grid.cut_surface_flux[1][i].max(0.0);
            }
        }
    }
    grid.extrapolate_surface_velocities(&vec![false; grid.fraction.len()], Some(&known));
    for (r, &i) in indices.iter().enumerate() {
        if cells[r].cap.area == 0.0 {
            let u = sample_velocity(&grid.u, &grid.v, &grid.w, dims, centers[r]);
            for (axis, v) in u.into_iter().enumerate() {
                grid.cut_surface_velocity[axis][i] = v;
            }
        }
    }
    let after = flux
        .iter()
        .enumerate()
        .map(|(r, v)| v.abs() / h / grid.fraction[indices[r]])
        .fold(0.0, f64::max);
    Ok(ProjectionStats {
        iterations: total_iterations as usize,
        active_cells: n,
        residual_initial: initial,
        residual_final: final_residual,
        divergence_before: before,
        divergence_after: after,
        converged: true,
        ..ProjectionStats::default()
    })
}

pub(super) fn transport_velocity(
    grid: &MacGridWorld,
    old: &[f64],
    new: &[f64],
    edges: &[(Option<usize>, Option<usize>, f64, bool)],
    water: &[f64],
) -> Result<([Vec<f64>; 3], usize), MacError> {
    if old.len() != new.len()
        || edges.len() != water.len()
        || grid
            .cut_surface_velocity
            .iter()
            .any(|v| v.len() != old.len())
    {
        return Err(MacError::InvalidConfig);
    }
    // Transport the reconstruction using the SAME final paired water transfers
    // as momentum. Empty/drying cells need no air mass or artificial inertia.
    let lanes: Vec<_> = edges
        .iter()
        .zip(water)
        .map(|(e, f)| (e.0, e.1, *f))
        .collect();
    let blocked = vec![false; old.len()];
    let mut candidate = [Vec::new(), Vec::new(), Vec::new()];
    for (axis, velocity) in grid.cut_surface_velocity.iter().enumerate() {
        candidate[axis] =
            super::momentum::water_only_upwind(old, new, velocity, &blocked, &lanes)?.0;
    }
    let bytes = 10 * old.len() * size_of::<f64>()
        + blocked.capacity() * size_of::<bool>()
        + lanes.capacity() * size_of::<(Option<usize>, Option<usize>, f64)>();
    Ok((candidate, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trench_subnormal_anisotropic_fragment_reconstructs_without_deletion() {
        let fraction = f64::from_bits(7);
        let normal = [-4.569712586733911e-147, 5.519002140292094e-246, 1.0];
        let reflected = InterfacePlane::from_fraction_resolved(normal.map(f64::abs), fraction);
        // The exact wedge is bounded by both dominant axes, not the fallback
        // root search's roughly 1e-20 thickness.
        assert!(reflected.alpha > 1e-246 && reflected.alpha < 1e-230);
        let cell = geometry(
            Some(InterfacePlane::from_fraction_resolved(normal, fraction)),
            fraction,
        )
        .unwrap();
        assert!(cell.cap.area > 0.0);
        assert!(
            cell.center
                .iter()
                .all(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)
        );
        assert!(cell.center[2] > 0.0 && cell.center[2] < reflected.alpha);
        // Axis-aligned minimum-positive slabs retain their centroid too.
        let smallest = f64::from_bits(1);
        let slab = geometry(
            Some(InterfacePlane::from_fraction_resolved(
                [0.0, 0.0, 1.0],
                smallest,
            )),
            smallest,
        )
        .unwrap();
        assert!(slab.cap.area > 0.0);
    }
    #[test]
    fn pressure_preconditioner_is_symmetric_positive_on_closed_and_tiny_graphs() {
        let x = [1.0, -2.0, 1.0, 0.0];
        let y = [-3.0, 1.0, 2.0, 0.0];
        for scale in [1.0, 1e-120] {
            let diagonal = [2.0 * scale, 5.0 * scale, 3.0 * scale, 0.0];
            let lower = vec![
                vec![],
                vec![(0, 2.0 * scale)],
                vec![(1, 3.0 * scale)],
                vec![],
            ];
            let upper = vec![
                vec![(1, 2.0 * scale)],
                vec![(2, 3.0 * scale)],
                vec![],
                vec![],
            ];
            let mut px = [0.0; 4];
            let mut py = [0.0; 4];
            precondition(&x, &diagonal, &lower, &upper, &mut px);
            precondition(&y, &diagonal, &lower, &upper, &mut py);
            gauge(&mut px, &[0, 0, 0, 3], &[true, false, false, true]);
            gauge(&mut py, &[0, 0, 0, 3], &[true, false, false, true]);
            let inner =
                |a: &[f64; 4], b: &[f64; 4]| a.iter().zip(b).map(|(a, b)| a * b).sum::<f64>();
            let xy = inner(&x, &py);
            let yx = inner(&y, &px);
            assert!((xy - yx).abs() <= 1e-12 * xy.abs().max(yx.abs()));
            assert!(inner(&x, &px) > 0.0 && inner(&y, &py) > 0.0);
            assert!(px.iter().chain(&py).all(|v| v.is_finite()));
            assert_eq!(px[3], 0.0);
        }
    }
    #[test]
    fn unchanged_boundary_refresh_preserves_dynamic_reconstruction() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let dims = [12, 6, 2];
        let boundary = SolidBoundary {
            spec: DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 144).unwrap(),
            solid: (0..144)
                .map(|i| i / 12 % 6 < if i % 12 < 6 { 2 } else { 1 })
                .collect(),
        };
        let mut control = MacGridWorld::new(
            &boundary,
            super::super::MacConfig {
                cell_size_m: 1.0,
                reconstructed_surface_support: true,
                ..Default::default()
            },
        )
        .unwrap();
        control.set_freely_displaced_air().unwrap();
        for z in 0..2 {
            for x in 0..6 {
                control.set_fraction(GlobalCell::new(x, 2, z), 0.2).unwrap();
            }
        }
        let mut refreshed = control.clone();
        for step in 0..600 {
            control.step(0.05).unwrap();
            refreshed.step(0.05).unwrap();
            assert_eq!(
                refreshed.refresh_boundary_retaining(&boundary).unwrap(),
                0.0
            );
            refreshed.commit_boundary(boundary.solid.clone()).unwrap();
            assert_eq!(
                refreshed.cut_surface_velocity, control.cut_surface_velocity,
                "step={step}"
            );
            assert_eq!(refreshed.pressure_pa, control.pressure_pa, "step={step}");
            assert_eq!(refreshed.fraction, control.fraction, "step={step}");
            assert_eq!(refreshed.u, control.u, "step={step}");
            assert_eq!(refreshed.v, control.v, "step={step}");
            assert_eq!(refreshed.w, control.w, "step={step}");
        }
    }
    #[test]
    fn interface_predictor_uses_accepted_water_and_preserves_momentum_and_energy() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let boundary = SolidBoundary {
            spec: DomainSpec::new(GlobalCell::new(0, 0, 0), [4, 1, 1], 4).unwrap(),
            solid: vec![false; 4],
        };
        let mut grid = MacGridWorld::new(&boundary, super::super::MacConfig::default()).unwrap();
        let edges = [
            (Some(0), Some(1), 999.0, false),
            (Some(2), Some(1), -999.0, false),
        ];
        for scale in [1.0, 1e-120] {
            let old = [scale, 0.0, 0.75 * scale, 0.0];
            let new = [0.4 * scale, scale, 0.35 * scale, 0.0];
            grid.cut_surface_velocity = [vec![2.0, 0.0, -2.0, 0.0], vec![0.0; 4], vec![0.0; 4]];
            let (next, _) =
                transport_velocity(&grid, &old, &new, &edges, &[0.6 * scale, 0.4 * scale]).unwrap();
            let momentum =
                |mass: &[f64], u: &[f64]| mass.iter().zip(u).map(|(m, u)| m * u).sum::<f64>();
            let energy =
                |mass: &[f64], u: &[f64]| mass.iter().zip(u).map(|(m, u)| m * u * u).sum::<f64>();
            assert!(
                (momentum(&old, &grid.cut_surface_velocity[0]) - momentum(&new, &next[0])).abs()
                    < scale * 1e-10
            );
            assert!(energy(&new, &next[0]) <= energy(&old, &grid.cut_surface_velocity[0]));
            assert!((next[0][1] - 0.4).abs() < 1e-12);
            assert_eq!(next[0][3], 0.0);
        }
    }
    #[test]
    fn tiny_moving_liquid_retains_volume_and_bounded_velocity_without_air_inertia() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let boundary = SolidBoundary {
            spec: DomainSpec::new(GlobalCell::new(0, 0, 0), [6, 4, 3], 72).unwrap(),
            solid: vec![false; 72],
        };
        let mut grid = MacGridWorld::new(
            &boundary,
            super::super::MacConfig {
                cell_size_m: 1.0,
                gravity_m_s2: [0.0; 3],
                reconstructed_surface_support: true,
                ..Default::default()
            },
        )
        .unwrap();
        grid.set_freely_displaced_air().unwrap();
        grid.set_fraction(GlobalCell::new(2, 1, 1), 1e-40).unwrap();
        grid.set_fraction(GlobalCell::new(3, 1, 1), 1e-80).unwrap();
        grid.u.fill(1.0);
        grid.enforce_wall_velocities();
        let volume = grid.water_volume_m3();
        let energy = grid.kinetic_energy_j();
        for step in 0..120 {
            grid.step(0.01).unwrap();
            assert!(
                (grid.water_volume_m3() - volume).abs() < volume * 1e-9,
                "step={step}"
            );
            assert!(grid.max_face_component_velocity_m_s() < 2.0, "step={step}");
            assert!(grid.kinetic_energy_j() <= energy * 1.05, "step={step}");
            assert!(grid.reconstructed_interface_speed_m_s().is_finite());
            assert!(
                grid.cut_surface_velocity
                    .iter()
                    .flatten()
                    .all(|u| u.is_finite())
            );
        }
        let retained = grid.allocated_bytes();
        let cached_bytes = 3 * grid.fraction.len() * size_of::<f64>();
        let mut changed = grid.solid.clone();
        changed[0] = true;
        grid.commit_boundary(changed).unwrap();
        assert_eq!(retained - grid.allocated_bytes(), cached_bytes);
        assert!(grid.cut_surface_velocity.iter().all(Vec::is_empty));
    }
    #[test]
    fn a_tiny_exposed_face_has_a_centroid_inside_its_actual_polygon() {
        for width in [f64::EPSILON, 1e-12, 1e-6] {
            let plane = InterfacePlane {
                normal: [0.0, -1.0, 0.0],
                alpha: -(1.0 - width),
            };
            let patch = Patch::new(clip(&square(0, 1.0), plane, &mut Vec::new()));
            assert!(patch.area > 0.0);
            assert!((patch.area - width).abs() <= f64::EPSILON);
            assert!(patch.center[1] >= 1.0 - width && patch.center[1] <= 1.0);
            assert!((patch.center[2] - 0.5).abs() < 1e-14);
        }
    }
    #[test]
    fn tiny_reflected_fragments_keep_nonzero_geometric_support() {
        for fraction in [1e-50, 1e-188, 1e-300, f64::from_bits(1)] {
            for normal in [[1.0, 1.0, 1.0], [-1.0, 1.0, -1.0], [1e-102, 0.5, 0.5]] {
                let g = geometry(
                    Some(InterfacePlane::from_fraction_resolved(normal, fraction)),
                    fraction,
                )
                .unwrap();
                assert!(g.cap.area > 0.0 && g.cap.area.is_finite());
                assert!(
                    g.center
                        .iter()
                        .all(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)
                );
                assert!(g.faces.iter().any(|p| p.area > 0.0));
                assert!(g.center[1] > 0.0);
            }
        }
    }
    #[test]
    fn clipped_liquid_has_correct_centroid_and_embedded_surface() {
        for depth in [0.2, 0.75] {
            let g = geometry(
                Some(InterfacePlane::from_fraction([0.0, 1.0, 0.0], depth)),
                depth,
            )
            .unwrap();
            assert!((g.center[1] - depth / 2.0).abs() < 1e-14);
            assert!((g.cap.area - 1.0).abs() < 1e-14);
            assert!((g.cap.center[1] - depth).abs() < 1e-14);
            assert_eq!(g.faces[3].area, 0.0);
            assert!((g.faces[1].area - depth).abs() < 1e-14);
            assert_eq!(g.faces[2].area, 1.0);
        }
    }
}
