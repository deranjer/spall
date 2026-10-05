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
// Component membership changes only when the contact active set changes.
// Open rows have a physical pressure boundary and require no gauge work.
struct Gauge {
    closed_rows: Vec<Vec<usize>>,
}
impl Gauge {
    fn new(labels: &[usize], closed: &[bool]) -> Self {
        let mut groups = vec![Vec::new(); labels.len()];
        for (i, &c) in labels.iter().enumerate() {
            if closed[c] {
                groups[c].push(i);
            }
        }
        Self {
            closed_rows: groups.into_iter().filter(|g| !g.is_empty()).collect(),
        }
    }
}

struct ScaledEdge {
    a: usize,
    b: usize,
    wa: f64,
    wb: f64,
}
fn apply(x: &[f64], boundary_diagonal: &[f64], edges: &[ScaledEdge], out: &mut [f64]) {
    for i in 0..x.len() {
        out[i] = boundary_diagonal[i] * x[i];
    }
    for e in edges {
        let jump = e.wa * x[e.a] - e.wb * x[e.b];
        out[e.a] += e.wa * jump;
        out[e.b] -= e.wb * jump;
    }
}
// Euclidean null-space projection in diagonally equilibrated coordinates.
// The constant physical pressure mode is root_diagonal, not a constant vector.
fn scaled_gauge(gauge: &Gauge, root_diagonal: &[f64], v: &mut [f64]) {
    for rows in &gauge.closed_rows {
        let sum: f64 = rows.iter().map(|&i| root_diagonal[i] * v[i]).sum();
        let weight: f64 = rows.iter().map(|&i| root_diagonal[i].powi(2)).sum();
        if weight > 0.0 {
            for &i in rows {
                v[i] -= root_diagonal[i] * (sum / weight);
            }
        }
    }
}
fn physical_norm(v: &[f64], root_diagonal: &[f64]) -> f64 {
    let squares: f64 = v
        .iter()
        .zip(root_diagonal)
        .map(|(v, d)| (v * d).powi(2))
        .sum();
    if squares.is_finite() && squares > 0.0 {
        return squares.sqrt();
    }
    let scale = v
        .iter()
        .zip(root_diagonal)
        .map(|(v, d)| (v * d).abs())
        .fold(0.0, f64::max);
    if scale == 0.0 || !scale.is_finite() {
        return scale;
    }
    scale
        * v.iter()
            .zip(root_diagonal)
            .map(|(v, d)| (v * d / scale).powi(2))
            .sum::<f64>()
            .sqrt()
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
    // Preserve existing Cartesian distance regularization. Embedded caps use
    // their actual half-cell liquid inertia: imposing this floor there can hold
    // a tiny cap fixed while pressure repeatedly accelerates an exposed patch.
    // No geometry, amount or free-boundary participation is thresholded.
    let distance = |d: f64| d.max(0.01);
    for (r, cell) in cells.iter().enumerate() {
        if cell.cap.area > 0.0 {
            let center = add(sub(centers[r], cell.center), cell.cap.center);
            let i = indices[r];
            let u = std::array::from_fn(|axis| grid.cut_surface_velocity[axis][i]);
            // The surface normal is derived from the canonical face momentum
            // after accepted water transfers. Independently transporting its
            // old normal lets it disagree with the Cartesian liquid outflow
            // and repeatedly feed a spurious pressure impulse back into faces.
            // Preserve transported tangent state and the new normal correction.
            let sampled = sample_velocity(&grid.u, &grid.v, &grid.w, dims, centers[r]);
            let normal_velocity = dot(sampled, cell.normal);
            let correction = normal_velocity - dot(u, cell.normal);
            for axis in 0..3 {
                grid.cut_surface_velocity[axis][i] += cell.normal[axis] * correction;
            }
            free.push(Free {
                row: r,
                air: Some(indices[r]),
                axis: None,
                face: 0,
                sign: 1.0,
                area: cell.cap.area,
                distance: grid.fraction[indices[r]] / (2.0 * cell.cap.area),
                velocity: normal_velocity,
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
    // Solve phi = physical_pressure + potential(center) - base[row].
    // The cap's atmospheric potential is zero in its own row's coordinates;
    // subtracting a large common potential after A*phi loses tiny corrections.
    // Affine offsets enter both the RHS and final pressure impulse exactly.
    let mut component_base = vec![None; n];
    for f in &free {
        if f.axis.is_none() {
            component_base[labels[f.row]].get_or_insert(f.potential);
        }
    }
    let mut base: Vec<_> = labels
        .iter()
        .map(|&c| component_base[c].unwrap_or(reference))
        .collect();
    drop(component_base);
    for f in &free {
        if f.axis.is_none() {
            base[f.row] = f.potential;
        }
    }
    // Positive outward flow can consume only the receiving cell's actual air
    // volume. A closing interface then has prescribed displacement flux and
    // unknown contact pressure instead of atmospheric Dirichlet pressure.
    // Include Cartesian patches as well as embedded caps (both sides of a gap).
    let mut held = vec![None; free.len()];
    let mut attempts = 0;
    let mut total_iterations = 0;
    let mut warm = grid.pressure_pa.iter().any(|p| *p != 0.0).then(|| {
        indices
            .iter()
            .enumerate()
            .map(|(r, i)| grid.pressure_pa[*i] + (potential(centers[r]) - base[r]))
            .collect::<Vec<_>>()
    });
    // Use sqrt(stiffness), never stiffness itself: 2*A^2/C can overflow
    // for real positive subnormal fractions. D^-1/2 A D^-1/2 is the same SPD
    // equation, with physical residual/velocity checks retained below.
    let edge_root: Vec<_> = edges
        .iter()
        .map(|e| e.area.sqrt() / e.distance.sqrt() / h)
        .collect();
    let free_root: Vec<_> = free
        .iter()
        .map(|f| {
            if f.axis.is_none() {
                std::f64::consts::SQRT_2 * f.area / grid.fraction[indices[f.row]].sqrt() / h
            } else {
                f.area.sqrt() / f.distance.sqrt() / h
            }
        })
        .collect();
    let corrected_free = |f: &Free, x: &[f64], inv: &[f64]| {
        if f.axis.is_none() {
            let root_c = grid.fraction[indices[f.row]].sqrt();
            f.velocity + dt / rho / h * (x[f.row] / root_c) * (inv[f.row] / root_c) * (2.0 * f.area)
        } else {
            f.velocity
                - dt / rho / h / f.distance * ((f.potential - base[f.row]) - x[f.row] * inv[f.row])
        }
    };
    let (phi, inverse_root, initial, final_residual, before) = loop {
        let mut closed = vec![true; n];
        for (j, f) in free.iter().enumerate() {
            if held[j].is_none() {
                closed[labels[f.row]] = false;
            }
        }
        let mut root_diagonal = vec![0.0_f64; n];
        let mut flux = vec![0.0; n];
        for (e, &k) in edges.iter().zip(&edge_root) {
            root_diagonal[e.a] = root_diagonal[e.a].hypot(k);
            root_diagonal[e.b] = root_diagonal[e.b].hypot(k);
            flux[e.a] += e.area * e.velocity;
            flux[e.b] -= e.area * e.velocity;
        }
        for (j, f) in free.iter().enumerate() {
            if let Some(q) = held[j] {
                flux[f.row] += q;
            } else {
                root_diagonal[f.row] = root_diagonal[f.row].hypot(free_root[j]);
                flux[f.row] += f.area * f.velocity;
            }
        }
        let inverse_root: Vec<_> = root_diagonal
            .iter()
            .map(|d| if *d > 0.0 { 1.0 / d } else { 0.0 })
            .collect();
        let mut diagonal = vec![0.0; n];
        let mut boundary_diagonal = vec![0.0; n];
        let mut response = vec![0.0_f64; n];
        let mut rhs: Vec<_> = flux
            .iter()
            .zip(&inverse_root)
            .map(|(q, inv)| -rho / dt / h * q * inv)
            .collect();
        let mut scaled_edges = Vec::with_capacity(edges.len());
        let mut lower = vec![Vec::new(); n];
        let mut upper = vec![Vec::new(); n];
        for (e, &k) in edges.iter().zip(&edge_root) {
            let wa = k * inverse_root[e.a];
            let wb = k * inverse_root[e.b];
            diagonal[e.a] += wa * wa;
            diagonal[e.b] += wb * wb;
            let jump = k * (base[e.b] - base[e.a]);
            rhs[e.a] += wa * jump;
            rhs[e.b] -= wb * jump;
            response[e.a] = response[e.a].max(dt / rho / h / e.distance * inverse_root[e.a]);
            response[e.b] = response[e.b].max(dt / rho / h / e.distance * inverse_root[e.b]);
            let (a, b) = if e.a < e.b { (e.a, e.b) } else { (e.b, e.a) };
            lower[b].push((a, wa * wb));
            upper[a].push((b, wa * wb));
            scaled_edges.push(ScaledEdge {
                a: e.a,
                b: e.b,
                wa,
                wb,
            });
        }
        for (j, f) in free.iter().enumerate() {
            if held[j].is_some() {
                continue;
            }
            let k = free_root[j];
            let w = k * inverse_root[f.row];
            diagonal[f.row] += w * w;
            boundary_diagonal[f.row] += w * w;
            rhs[f.row] += w * (k * (f.potential - base[f.row]));
            let sensitivity = if f.axis.is_none() {
                let c = grid.fraction[indices[f.row]].sqrt();
                dt / rho / h * (2.0 * f.area / c) * (inverse_root[f.row] / c)
            } else {
                dt / rho / h / f.distance * inverse_root[f.row]
            };
            response[f.row] = response[f.row].max(sensitivity);
        }
        let before = flux
            .iter()
            .enumerate()
            .map(|(r, v)| v.abs() / h / grid.fraction[indices[r]])
            .fold(0.0, f64::max);
        let mut sums = vec![0.0; n];
        let mut counts = vec![0usize; n];
        for (r, &c) in labels.iter().enumerate() {
            if closed[c] {
                sums[c] -= rho / dt / h * flux[r];
                counts[c] += 1;
            }
        }
        for c in 0..n {
            if counts[c] > 0 && (sums[c] / counts[c] as f64).abs() > 1e-7 {
                return Err(MacError::IncompatibleEnclosedPressureRegion);
            }
        }
        let gauge = Gauge::new(&labels, &closed);
        scaled_gauge(&gauge, &root_diagonal, &mut rhs);
        // Krylov vectors use x = (physical_pressure + potential - base) * sqrt(D).
        // Convert back with inverse_root for physical pressure and face impulses.
        let mut phi: Vec<_> = warm
            .take()
            .map(|v| v.iter().zip(&root_diagonal).map(|(p, d)| p * d).collect())
            .unwrap_or_else(|| {
                rhs.iter()
                    .zip(&diagonal)
                    .map(|(r, d)| if *d > 0.0 { r / d } else { 0.0 })
                    .collect()
            });
        scaled_gauge(&gauge, &root_diagonal, &mut phi);
        let mut product = vec![0.0; n];
        apply(&phi, &boundary_diagonal, &scaled_edges, &mut product);
        let mut residual: Vec<_> = rhs.iter().zip(&product).map(|(b, a)| b - a).collect();
        scaled_gauge(&gauge, &root_diagonal, &mut residual);
        let initial = physical_norm(&residual, &root_diagonal);
        let target = grid
            .config
            .pressure_absolute_tolerance
            .max(grid.config.pressure_relative_tolerance * physical_norm(&rhs, &root_diagonal))
            // For a full cell (no moving embedded face), pressure residual
            // changes its amount by dt^2/rho * residual. Reserve two orders
            // of margin inside the unchanged 1e-10 donor bound.
            .min(1e-12 * rho / dt.powi(2));
        let mut z = vec![0.0; n];
        precondition(&residual, &diagonal, &lower, &upper, &mut z);
        scaled_gauge(&gauge, &root_diagonal, &mut z);
        let mut direction = z.clone();
        let mut rz: f64 = residual.iter().zip(&z).map(|(r, z)| r * z).sum();
        // A tiny row can satisfy the global flux norm while retaining a large
        // pressure/velocity error. Also resolve each row's diagonal-scaled
        // potential defect; no volume threshold removes a row from this gate.
        // Also bound the remaining pressure-induced velocity correction by
        // 1e-8 m/s. A fixed Pa tolerance alone is unsafe as cap inertia vanishes.
        let locally_converged = |residual: &[f64]| {
            residual.iter().enumerate().all(|(i, r)| {
                diagonal[i] == 0.0 || {
                    let defect = r / diagonal[i];
                    (defect * inverse_root[i]).abs()
                        <= grid.config.pressure_absolute_tolerance.max(
                            grid.config.pressure_relative_tolerance
                                * (rhs[i] / diagonal[i] * inverse_root[i]).abs(),
                        )
                        && (defect * response[i]).abs() <= 1e-8
                }
            })
        };
        let mut iterations = 0;
        while (physical_norm(&residual, &root_diagonal) > target || !locally_converged(&residual))
            && iterations + total_iterations < grid.config.pressure_max_iterations
        {
            if physical_norm(&residual, &root_diagonal) <= target || !rz.is_finite() {
                // A finite pressure hint on a vanishing-inertia cap can also
                // overflow the CG dot product. The same budgeted relaxation
                // removes that hint without changing the equation or tolerance.
                // Local symmetric relaxation corrects weak rows whose dot
                // products disappear beneath the large rows' roundoff. Each
                // correction consumes one iteration of the original budget.
                precondition(&residual, &diagonal, &lower, &upper, &mut z);
                for i in 0..n {
                    phi[i] += z[i];
                }
                scaled_gauge(&gauge, &root_diagonal, &mut phi);
                apply(&phi, &boundary_diagonal, &scaled_edges, &mut product);
                for i in 0..n {
                    residual[i] = rhs[i] - product[i];
                }
                scaled_gauge(&gauge, &root_diagonal, &mut residual);
                precondition(&residual, &diagonal, &lower, &upper, &mut z);
                scaled_gauge(&gauge, &root_diagonal, &mut z);
                direction.copy_from_slice(&z);
                rz = residual.iter().zip(&z).map(|(r, z)| r * z).sum();
                iterations += 1;
                continue;
            }
            apply(&direction, &boundary_diagonal, &scaled_edges, &mut product);
            let dp: f64 = direction.iter().zip(&product).map(|(a, b)| a * b).sum();
            if !dp.is_finite() || dp <= 0.0 {
                return Err(MacError::MomentumInvalidState);
            }
            let alpha = rz / dp;
            for i in 0..n {
                phi[i] += alpha * direction[i];
                residual[i] -= alpha * product[i];
            }
            scaled_gauge(&gauge, &root_diagonal, &mut residual);
            // Recursive CG residuals can lose the remaining correction through
            // cancellation. Verify apparent convergence against the actual
            // operator and restart from that residual within the same budget.
            let reliable = physical_norm(&residual, &root_diagonal) <= target;
            if reliable {
                apply(&phi, &boundary_diagonal, &scaled_edges, &mut product);
                for i in 0..n {
                    residual[i] = rhs[i] - product[i];
                }
                scaled_gauge(&gauge, &root_diagonal, &mut residual);
            }
            precondition(&residual, &diagonal, &lower, &upper, &mut z);
            scaled_gauge(&gauge, &root_diagonal, &mut z);
            let next: f64 = residual.iter().zip(&z).map(|(a, b)| a * b).sum();
            let beta = if reliable { 0.0 } else { next / rz };
            for i in 0..n {
                direction[i] = z[i] + beta * direction[i];
            }
            rz = next;
            iterations += 1;
        }
        apply(&phi, &boundary_diagonal, &scaled_edges, &mut product);
        for i in 0..n {
            residual[i] = rhs[i] - product[i];
        }
        scaled_gauge(&gauge, &root_diagonal, &mut residual);
        let final_residual = physical_norm(&residual, &root_diagonal);
        if final_residual > target * 1.01 || !locally_converged(&residual) {
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
            let q = f.area * corrected_free(f, &phi, &inverse_root);
            let capacity = (1.0 - grid.fraction[air]) * h / dt;
            let donor_capacity = grid.fraction[air] * (h / dt);
            let constraint =
                if f.axis.is_some() && cells[f.row].cap.area > 0.0 && q < -donor_capacity {
                    // Bound this cut-patch inflow by the upstream cell's water.
                    // Paired transport still enforces the aggregate donor bound.
                    // Empty donors supply zero; every positive amount participates.
                    // Signed cap displacement and full-row retreat remain free.
                    Some(-donor_capacity)
                } else if q > capacity {
                    Some(capacity)
                } else {
                    None
                };
            if held[j] != constraint {
                held[j] = constraint;
                changed = true;
            }
        }
        if !changed {
            break (phi, inverse_root, initial, final_residual, before);
        }
        warm = Some(
            phi.iter()
                .zip(&inverse_root)
                .map(|(p, inv)| p * inv)
                .collect(),
        );
        attempts += 1;
        if attempts > 32 || total_iterations > grid.config.pressure_max_iterations {
            return Err(MacError::MomentumPressureNotConverged {
                residual: final_residual,
            });
        }
    };
    let mut free_velocity: Vec<_> = free
        .iter()
        .map(|f| corrected_free(f, &phi, &inverse_root))
        .collect();
    grid.pressure_pa.fill(0.0);
    for r in 0..n {
        grid.pressure_pa[indices[r]] = phi[r] * inverse_root[r] + (base[r] - potential(centers[r]));
    }
    grid.cut_surface_flux = std::array::from_fn(|a| vec![0.0; known[a].len()]);
    let mut flux = vec![0.0; n];
    for e in &edges {
        let u = e.velocity
            - dt / rho / h / e.distance
                * ((base[e.b] - base[e.a])
                    + (phi[e.b] * inverse_root[e.b] - phi[e.a] * inverse_root[e.a]));
        if grid.config.pressure_diagnostics && u.abs() > 10.0 {
            eprintln!(
                "{{\"type\":\"cut_fast_edge\",\"axis\":{},\"face\":{},\"a\":{},\"b\":{},\"fraction_a\":{:.17e},\"fraction_b\":{:.17e},\"area\":{:.17e},\"distance\":{:.17e},\"predicted_velocity\":{:.17e},\"corrected_velocity\":{:.17e},\"phi_a\":{:.17e},\"phi_b\":{:.17e}}}",
                e.axis,
                e.face,
                indices[e.a],
                indices[e.b],
                grid.fraction[indices[e.a]],
                grid.fraction[indices[e.b]],
                e.area,
                e.distance,
                e.velocity,
                u,
                phi[e.a] * inverse_root[e.a] + base[e.a] - reference,
                phi[e.b] * inverse_root[e.b] + base[e.b] - reference
            );
        }
        let q = e.area * u;
        grid.cut_surface_flux[e.axis][e.face] += q * h * h;
        flux[e.a] += q;
        flux[e.b] -= q;
    }
    for (j, f) in free.iter().enumerate() {
        let u = held[j].map_or_else(|| free_velocity[j], |q| q / f.area);
        if grid.config.pressure_diagnostics && u.abs() > 10.0 {
            eprintln!(
                "{{\"type\":\"cut_fast_free\",\"axis_or_cap\":{},\"face\":{},\"cell\":{},\"fraction\":{:.17e},\"area\":{:.17e},\"distance\":{:.17e},\"predicted_velocity\":{:.17e},\"corrected_velocity\":{:.17e},\"phi\":{:.17e},\"boundary_phi\":{:.17e},\"held\":{}}}",
                f.axis.unwrap_or(3),
                f.face,
                indices[f.row],
                grid.fraction[indices[f.row]],
                f.area,
                f.distance,
                f.velocity,
                u,
                phi[f.row] * inverse_root[f.row] + base[f.row] - reference,
                f.potential - reference,
                held[j].is_some()
            );
        }
        if f.axis.is_none() {
            continue;
        }
        flux[f.row] += f.area * u;
        if let Some(axis) = f.axis {
            grid.cut_surface_flux[axis][f.face] += f.sign * f.area * u * h * h;
        }
    }
    for (j, f) in free.iter().enumerate().filter(|(_, f)| f.axis.is_none()) {
        // Zero net Cartesian outflow fixes a free atmospheric cap's normal
        // to zero by continuity. Avoid cancellation in its predictor/pressure
        // subtraction, and require agreement with the solved normal within
        // the existing pressure-response accuracy. Nonzero outflow retains
        // the pressure result; contact caps retain prescribed displacement.
        let u = held[j].map_or_else(
            || {
                if flux[f.row] == 0.0 {
                    0.0
                } else {
                    free_velocity[j]
                }
            },
            |q| q / f.area,
        );
        if !u.is_finite() || (held[j].is_none() && (u - free_velocity[j]).abs() > 1e-8) {
            return Err(MacError::MomentumPressureNotConverged {
                residual: final_residual,
            });
        }
        free_velocity[j] = u;
        flux[f.row] += f.area * u;
        let i = indices[f.row];
        for axis in 0..3 {
            grid.cut_surface_velocity[axis][i] += cells[f.row].normal[axis] * (u - f.velocity);
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
    if grid.config.pressure_diagnostics {
        // Audit projected boundary flux before transport, without omitting tiny rows
        // or replacing the existing volume-normalized divergence metric.
        if let Some((r, net)) = flux.iter().enumerate().max_by(|(a, qa), (b, qb)| {
            (qa.abs() / grid.fraction[indices[*a]])
                .total_cmp(&(qb.abs() / grid.fraction[indices[*b]]))
        }) {
            let mut cartesian = [0.0; 6];
            let mut cap_flux = 0.0;
            let mut cap_velocity = 0.0;
            let mut magnitude = 0.0;
            for e in &edges {
                if e.a == r || e.b == r {
                    let u = e.velocity
                        - dt / rho / h / e.distance
                            * ((base[e.b] - base[e.a])
                                + (phi[e.b] * inverse_root[e.b] - phi[e.a] * inverse_root[e.a]));
                    let q = e.area * u;
                    let side = usize::from(e.a == r);
                    cartesian[2 * e.axis + side] += if side == 1 { q } else { -q };
                    magnitude += q.abs();
                }
            }
            for (j, f) in free.iter().enumerate().filter(|(_, f)| f.row == r) {
                let u = held[j].map_or_else(|| free_velocity[j], |q| q / f.area);
                let q = f.area * u;
                magnitude += q.abs();
                if let Some(axis) = f.axis {
                    cartesian[2 * axis + usize::from(f.sign > 0.0)] += q;
                } else {
                    cap_flux = q;
                    cap_velocity = u;
                }
            }
            let cell = grid.spec.cell_at(indices[r]);
            eprintln!(
                "{{\"type\":\"cut_row_flux_audit\",\"cell\":[{},{},{}],\"fraction\":{:.17e},\"dt_s\":{:.17e},\"cartesian_outflow_m3_s\":{:?},\"embedded_outflow_m3_s\":{:.17e},\"sum_absolute_outflow_m3_s\":{:.17e},\"net_outflow_m3_s\":{:.17e},\"relative_closure_error\":{:.17e},\"cap_area_cells2\":{:.17e},\"cap_normal\":{:?},\"cap_normal_velocity_m_s\":{:.17e},\"pressure_pa\":{:.17e}}}",
                cell.x,
                cell.y,
                cell.z,
                grid.fraction[indices[r]],
                dt,
                cartesian.map(|q| q * h * h),
                cap_flux * h * h,
                magnitude * h * h,
                net * h * h,
                if magnitude > 0.0 {
                    net.abs() / magnitude
                } else {
                    0.0
                },
                cells[r].cap.area,
                cells[r].normal,
                cap_velocity,
                grid.pressure_pa[indices[r]]
            );
        }
    }
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
    let mut work_bytes = 0;
    for (axis, velocity) in grid.cut_surface_velocity.iter().enumerate() {
        let result = super::momentum::water_only_upwind(old, new, velocity, &blocked, &lanes)?;
        work_bytes = work_bytes.max(result.work_bytes);
        candidate[axis] = result.velocity;
    }
    let bytes = work_bytes
        + 10 * old.len() * size_of::<f64>()
        + blocked.capacity() * size_of::<bool>()
        + lanes.capacity() * size_of::<(Option<usize>, Option<usize>, f64)>();
    Ok((candidate, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pressure_row_trace_preserves_the_projected_water_and_momentum_state() {
        let fixture = include_bytes!("../../fixtures/eng122-trench-crop-v1.water-replay");
        let mut original = MacGridWorld::read_reconstructed_replay(fixture.as_slice()).unwrap();
        let mut traced = original.clone();
        traced.config.pressure_diagnostics = true;
        for _ in 0..3 {
            let a = original.step(0.05).unwrap();
            let b = traced.step(0.05).unwrap();
            assert_eq!(
                a.divergence_after_max_s.to_bits(),
                b.divergence_after_max_s.to_bits()
            );
            assert_eq!(original.fraction, traced.fraction);
            assert_eq!(original.pressure_pa, traced.pressure_pa);
            assert_eq!(original.u, traced.u);
            assert_eq!(original.v, traced.v);
            assert_eq!(original.w, traced.w);
            assert_eq!(original.cut_surface_flux, traced.cut_surface_flux);
            assert_eq!(original.cut_surface_velocity, traced.cut_surface_velocity);
        }
    }
    #[test]
    fn sealed_partial_cell_has_exactly_stationary_atmospheric_cap() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        for fraction in [0.25, 1e-40, 1e-120, 1e-300, f64::from_bits(7)] {
            let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [3, 3, 3], 27).unwrap();
            let mut solid = vec![true; 27];
            solid[13] = false;
            let mut grid = MacGridWorld::new(
                &SolidBoundary { spec, solid },
                super::super::MacConfig {
                    cell_size_m: 1.0,
                    gravity_m_s2: [0.0; 3],
                    reconstructed_surface_support: true,
                    ..Default::default()
                },
            )
            .unwrap();
            grid.set_freely_displaced_air().unwrap();
            grid.set_fraction(GlobalCell::new(1, 1, 1), fraction)
                .unwrap();
            // A stale normal predictor in a walled pore must be removed by
            // projection: no Cartesian boundary can move or transfer liquid.
            for velocity in [0.1, 1.0 / 3.0, -0.1] {
                let mut grid = grid.clone();
                grid.v.fill(velocity);
                let before = grid.fraction.clone();
                let result = grid.project(0.01).unwrap();
                assert!(result.converged);
                assert_eq!(grid.fraction, before);
                assert_eq!(
                    grid.cut_surface_velocity[1][13], 0.0,
                    "C={fraction}, predictor={velocity}"
                );
                assert_eq!(result.divergence_after, 0.0);
                assert!(grid.cut_surface_flux.iter().flatten().all(|q| *q == 0.0));
            }
        }
    }
    #[test]
    fn wet_donors_preserve_local_uniform_momentum_and_zero_pressure_work() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        for direction in [-1.0, 1.0] {
            let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [48, 4, 3], 576).unwrap();
            let mut grid = MacGridWorld::new(
                &SolidBoundary {
                    spec,
                    solid: vec![false; 576],
                },
                super::super::MacConfig {
                    cell_size_m: 1.0,
                    gravity_m_s2: [0.0; 3],
                    reconstructed_surface_support: true,
                    ..Default::default()
                },
            )
            .unwrap();
            grid.set_freely_displaced_air().unwrap();
            for x in 0..48 {
                grid.set_fraction(GlobalCell::new(x, 1, 1), if x == 24 { 0.75 } else { 0.25 })
                    .unwrap();
            }
            grid.u.fill(direction);
            grid.enforce_wall_velocities();
            let fractions = grid.fraction.clone();
            let measure = |g: &MacGridWorld| {
                let mut momentum = [0.0; 3];
                let mut energy = 0.0;
                let mut mass = 0.0;
                for x in 20..29 {
                    let m = g.fraction[g.cell_index(x, 1, 1)]
                        * g.config.density_kg_m3
                        * g.cell_volume();
                    mass += m;
                    for (axis, faces) in [
                        [g.u[g.u_index(x, 1, 1)], g.u[g.u_index(x + 1, 1, 1)]],
                        [g.v[g.v_index(x, 1, 1)], g.v[g.v_index(x, 2, 1)]],
                        [g.w[g.w_index(x, 1, 1)], g.w[g.w_index(x, 1, 2)]],
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        momentum[axis] += 0.5 * m * (faces[0] + faces[1]);
                        energy += 0.25 * m * (faces[0].powi(2) + faces[1].powi(2));
                    }
                }
                (momentum, energy, mass)
            };
            let (before_momentum, before_energy, mass) = measure(&grid);
            assert!(grid.project(0.01).unwrap().converged);
            assert_eq!(grid.fraction, fractions);
            let (after_momentum, after_energy, _) = measure(&grid);
            for (axis, (before, after)) in before_momentum.iter().zip(after_momentum).enumerate() {
                assert!(
                    (after - before).abs() < mass * 1e-8,
                    "direction={direction}, axis={axis}, before={before_momentum:?}, after={after_momentum:?}"
                );
            }
            assert!(
                (after_energy - before_energy).abs() < before_energy * 1e-8,
                "direction={direction}, before={before_energy}, after={after_energy}"
            );
            for x in 20..29 {
                assert!((grid.u[grid.u_index(x, 1, 1)] - direction).abs() < 1e-8);
                assert!(grid.pressure_pa[grid.cell_index(x, 1, 1)].abs() < 1e-5);
            }
        }
    }

    #[test]
    fn full_cell_free_boundary_retreat_keeps_uniform_translation() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [6, 4, 3], 72).unwrap();
        let mut grid = MacGridWorld::new(
            &SolidBoundary {
                spec,
                solid: vec![false; 72],
            },
            super::super::MacConfig {
                cell_size_m: 1.0,
                gravity_m_s2: [0.0; 3],
                reconstructed_surface_support: true,
                ..Default::default()
            },
        )
        .unwrap();
        grid.set_freely_displaced_air().unwrap();
        let wet = GlobalCell::new(2, 1, 1);
        let receiver = GlobalCell::new(3, 1, 1);
        grid.set_fraction(wet, 1.0).unwrap();
        grid.u.fill(1.0);
        grid.enforce_wall_velocities();
        grid.project(0.01).unwrap();
        // A full row has no embedded cap. Its Cartesian atmospheric boundary
        // must remain free to retreat, rather than becoming a hidden wall.
        assert_eq!(grid.u[grid.u_index(2, 1, 1)], 1.0);
        assert_eq!(grid.u[grid.u_index(3, 1, 1)], 1.0);
        grid.advect_fraction_fct(0.01, None).unwrap();
        assert!((grid.fraction[grid.cell_index_global(wet).unwrap()] - 0.99).abs() < 1e-14);
        assert!((grid.fraction[grid.cell_index_global(receiver).unwrap()] - 0.01).abs() < 1e-14);
        assert_eq!(grid.water_volume_m3(), 1.0);
    }

    #[test]
    fn exposed_air_patch_cannot_supply_water_to_a_moving_fragment() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        for fraction in [0.25, 1e-7, 1e-40, 1e-300] {
            for direction in [-1.0, 1.0] {
                let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [6, 4, 3], 72).unwrap();
                let mut grid = MacGridWorld::new(
                    &SolidBoundary {
                        spec,
                        solid: vec![false; 72],
                    },
                    super::super::MacConfig {
                        cell_size_m: 1.0,
                        gravity_m_s2: [0.0; 3],
                        reconstructed_surface_support: true,
                        ..Default::default()
                    },
                )
                .unwrap();
                grid.set_freely_displaced_air().unwrap();
                let wet = GlobalCell::new(2, 1, 1);
                let receiver = GlobalCell::new(if direction > 0.0 { 3 } else { 1 }, 1, 1);
                grid.set_fraction(wet, fraction).unwrap();
                grid.u.fill(direction);
                grid.enforce_wall_velocities();
                let before = grid.water_volume_m3();
                let projection = grid.project(0.01).unwrap();
                assert!(projection.converged);
                let inward = grid.u_index(if direction > 0.0 { 2 } else { 3 }, 1, 1);
                let outward = grid.u_index(if direction > 0.0 { 3 } else { 2 }, 1, 1);
                // Inward transfer has no water donor; outward motion and signed
                // embedded-interface displacement must remain available.
                assert_eq!(grid.u[inward], 0.0, "C={fraction}, direction={direction}");
                assert!(
                    grid.u[outward] * direction > 0.0,
                    "C={fraction}, direction={direction}"
                );
                assert_eq!(
                    grid.fraction[grid.cell_index_global(wet).unwrap()].to_bits(),
                    fraction.to_bits()
                );
                let (exterior, _, _, _) = grid.advect_fraction_fct(0.01, None).unwrap();
                assert_eq!(exterior, 0.0);
                assert!((grid.water_volume_m3() - before).abs() <= before * 1e-12);
                assert!(grid.fraction[grid.cell_index_global(receiver).unwrap()] > 0.0);
            }
        }
    }

    #[test]
    fn disconnected_closed_gauges_preserve_open_pressure_and_row_differences() {
        let labels = [0, 1, 0, 3, 1, 3, 6];
        let gauge = Gauge::new(&labels, &[true, false, false, true, false, false, true]);
        let initial = [3.0, -7.0, 9.0, 1e-120, 1e300, 3e-120, 42.0];
        let mut projected = initial;
        scaled_gauge(&gauge, &[1.0; 7], &mut projected);
        assert_eq!(projected[1].to_bits(), initial[1].to_bits());
        assert_eq!(projected[4].to_bits(), initial[4].to_bits());
        assert_eq!(projected[0] + projected[2], 0.0);
        assert!((projected[3] + projected[5]).abs() < 1e-135);
        assert_eq!(projected[6], 0.0);
        assert_eq!(projected[2] - projected[0], initial[2] - initial[0]);
        assert!(((projected[5] - projected[3]) / (initial[5] - initial[3]) - 1.0).abs() < 1e-15);
        // Independent component offsets must leave the same pressure solution.
        let mut shifted = initial;
        shifted[0] += 100.0;
        shifted[2] += 100.0;
        shifted[6] -= 9.0;
        scaled_gauge(&gauge, &[1.0; 7], &mut shifted);
        assert_eq!(projected, shifted);
    }

    #[test]
    fn equilibrated_closed_gauge_preserves_physical_pressure_jumps() {
        let gauge = Gauge::new(&[0, 0, 0], &[true, false, false]);
        let roots = [2.0, 3.0, 4.0];
        let mut original = [3.0, 10.0, 11.0];
        let jump = original[1] / roots[1] - original[0] / roots[0];
        let mut shifted: [f64; 3] = std::array::from_fn(|i| original[i] + 100.0 * roots[i]);
        scaled_gauge(&gauge, &roots, &mut original);
        scaled_gauge(&gauge, &roots, &mut shifted);
        assert!((original[1] / roots[1] - original[0] / roots[0] - jump).abs() < 1e-14);
        assert!(
            original
                .iter()
                .zip(roots)
                .map(|(v, d)| v * d)
                .sum::<f64>()
                .abs()
                < 1e-14
        );
        for (a, b) in original.iter().zip(shifted) {
            assert!((a - b).abs() < 1e-13);
        }
    }

    #[test]
    fn stale_reconstructed_normal_cannot_inject_canonical_momentum() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [5, 5, 5], 125).unwrap();
        let mut grid = MacGridWorld::new(
            &SolidBoundary {
                spec,
                solid: vec![false; 125],
            },
            super::super::MacConfig {
                cell_size_m: 0.75,
                gravity_m_s2: [0.0; 3],
                reconstructed_surface_support: true,
                ..Default::default()
            },
        )
        .unwrap();
        grid.set_freely_displaced_air().unwrap();
        grid.set_fraction(GlobalCell::new(2, 2, 2), 0.25).unwrap();
        let mut stale = grid.clone();
        stale.cut_surface_velocity = std::array::from_fn(|_| vec![0.0; 125]);
        let wet = stale.cell_index_global(GlobalCell::new(2, 2, 2)).unwrap();
        stale.cut_surface_velocity[1][wet] = 10_000.0;
        grid.project(0.05).unwrap();
        stale.project(0.05).unwrap();
        assert_eq!(stale.u, grid.u);
        assert_eq!(stale.v, grid.v);
        assert_eq!(stale.w, grid.w);
        assert_eq!(stale.cut_surface_flux, grid.cut_surface_flux);
        assert_eq!(stale.pressure_pa, grid.pressure_pa);
        assert!(stale.reconstructed_interface_speed_m_s() < 1e-7);
    }

    #[test]
    fn vanishing_thickness_cap_cannot_turn_a_pressure_hint_into_motion() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [5, 5, 5], 125).unwrap();
        for fraction in [1e-40, 1e-120, 1e-300, 1e-311, f64::from_bits(1)] {
            let mut grid = MacGridWorld::new(
                &SolidBoundary {
                    spec,
                    solid: vec![false; 125],
                },
                super::super::MacConfig {
                    cell_size_m: 0.75,
                    gravity_m_s2: [0.0; 3],
                    reconstructed_surface_support: true,
                    ..Default::default()
                },
            )
            .unwrap();
            grid.set_freely_displaced_air().unwrap();
            let cell = GlobalCell::new(2, 2, 2);
            grid.set_fraction(cell, fraction).unwrap();
            let i = grid.cell_index_global(cell).unwrap();
            grid.pressure_pa[i] = 10_000.0;
            let before = grid.water_volume_m3();
            let p = grid.project(0.05).unwrap();
            assert!(p.converged);
            assert!(p.iterations <= grid.config.pressure_max_iterations as usize);
            assert!(grid.max_face_component_velocity_m_s() < 1e-7);
            assert!(grid.reconstructed_interface_speed_m_s() < 1e-7);
            assert_eq!(grid.water_volume_m3().to_bits(), before.to_bits());
            assert_eq!(grid.fraction[i].to_bits(), fraction.to_bits());
        }
    }

    #[test]
    fn tiny_fragment_pressure_guess_cannot_create_motion_without_forces() {
        use crate::{DomainSpec, SolidBoundary};
        use spall_core::GlobalCell;
        let boundary = SolidBoundary {
            spec: DomainSpec::new(GlobalCell::new(0, 0, 0), [5, 5, 5], 125).unwrap(),
            solid: vec![false; 125],
        };
        let mut grid = MacGridWorld::new(
            &boundary,
            super::super::MacConfig {
                cell_size_m: 0.75,
                gravity_m_s2: [0.0; 3],
                reconstructed_surface_support: true,
                ..Default::default()
            },
        )
        .unwrap();
        grid.set_freely_displaced_air().unwrap();
        for fraction in [1e-40, 1e-120, 1e-300] {
            let mut grid = grid.clone();
            for cell in [
                GlobalCell::new(2, 2, 2),
                GlobalCell::new(3, 2, 2),
                GlobalCell::new(2, 3, 2),
                GlobalCell::new(2, 2, 3),
            ] {
                grid.set_fraction(cell, fraction).unwrap();
                let i = grid.cell_index_global(cell).unwrap();
                grid.pressure_pa[i] = 10_000.0;
            }
            grid.project(0.05).unwrap();
            assert!(
                grid.max_face_component_velocity_m_s() < 1e-7,
                "speed={}",
                grid.max_face_component_velocity_m_s()
            );
            assert!(grid.reconstructed_interface_speed_m_s() < 1e-7);
        }
    }
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
            let gauge = Gauge::new(&[0, 0, 0, 3], &[true, false, false, true]);
            scaled_gauge(&gauge, &[1.0; 4], &mut px);
            scaled_gauge(&gauge, &[1.0; 4], &mut py);
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
