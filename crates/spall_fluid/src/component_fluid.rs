//! Experimental ENG-122 pressure/transport operators over exact open components.
//! This is not the production MAC/PLIC solver. The two-point graph projection
//! uses component-centroid connections; it still needs nonorthogonal/interface
//! accuracy and sealed-air/velocity-advection gates before production activation.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::cut_cell::{CutCellGeometry, GeometryError};

#[derive(Debug, Clone, Copy)]
pub struct ComponentConfig {
    pub voxel_size_m: f64,
    pub water_density: f64,
    pub air_density: f64,
    pub gravity: [f64; 3],
    pub open_top: bool,
    pub relative_tolerance: f64,
    pub absolute_tolerance: f64,
    pub max_iterations: u32,
    pub cfl: f64,
}

impl Default for ComponentConfig {
    fn default() -> Self {
        Self {
            voxel_size_m: 0.25,
            water_density: 1000.0,
            air_density: 1.2,
            gravity: [0.0, -9.81, 0.0],
            open_top: true,
            relative_tolerance: 1e-8,
            absolute_tolerance: 1e-10,
            max_iterations: 1000,
            cfl: 0.45,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ComponentError {
    #[error("invalid component fluid configuration or timestep")]
    InvalidConfig,
    #[error("component geometry/amounts: {0}")]
    Geometry(#[from] GeometryError),
    #[error("invalid component amounts or volume fluxes")]
    InvalidState,
    #[error("pressure failed to converge after {iterations} iterations, residual {residual}")]
    PressureNotConverged { iterations: u32, residual: f64 },
    #[error("transport CFL {measured} exceeds limit {limit}")]
    Cfl { measured: f64, limit: f64 },
    #[error("transport amount {amount_m3} outside [0, {capacity_m3}] in component {component}")]
    Capacity {
        component: usize,
        amount_m3: f64,
        capacity_m3: f64,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectionMetrics {
    pub iterations: u32,
    pub residual: f64,
    pub max_divergence_per_s: f64,
    pub max_connection_speed_m_s: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransportMetrics {
    pub limiter_passes: u32,
    pub limited_water_m3: f64,
    pub moved_water_m3: f64,
}

#[derive(Debug, Clone)]
pub struct ComponentFluid {
    geometry: Arc<CutCellGeometry>,
    config: ComponentConfig,
    capacity: Vec<f64>,
    /// Cubic metres, independently retained on each side of an internal wall.
    amounts: Vec<f64>,
    pressure: Vec<f64>,
    flux: Vec<f64>,
    top_flux: Vec<f64>,
    top_area: Vec<f64>,
    distances: Vec<f64>,
    height_deltas: Vec<[f64; 3]>,
    top_distance: Vec<f64>,
    pinned: Vec<bool>,
    regions: Vec<Vec<usize>>,
    outflow: f64,
}

impl ComponentFluid {
    pub fn new(
        geometry: Arc<CutCellGeometry>,
        fine_fractions: &[f64],
        config: ComponentConfig,
    ) -> Result<Self, ComponentError> {
        let finite_positive = [
            config.voxel_size_m,
            config.water_density,
            config.air_density,
            config.relative_tolerance,
            config.absolute_tolerance,
            config.cfl,
        ];
        if finite_positive.iter().any(|v| !v.is_finite() || *v <= 0.0)
            || config.gravity.iter().any(|v| !v.is_finite())
            || config.max_iterations == 0
            || config.cfl > 1.0
        {
            return Err(ComponentError::InvalidConfig);
        }
        let volume = config.voxel_size_m.powi(3);
        if !volume.is_finite() || volume <= 0.0 {
            return Err(ComponentError::InvalidConfig);
        }
        let amounts = geometry
            .aggregate_amounts(fine_fractions)?
            .into_iter()
            .map(|v| v * volume)
            .collect();
        let capacity: Vec<_> = geometry
            .components()
            .iter()
            .map(|c| f64::from(c.voxel_count) * volume)
            .collect();
        if capacity.iter().any(|v| !v.is_finite() || *v <= 0.0) {
            return Err(ComponentError::InvalidConfig);
        }
        let n = capacity.len();
        let centers: Vec<_> = (0..n)
            .map(|i| {
                geometry
                    .component_centroid(i)
                    .map(|v| v * config.voxel_size_m)
            })
            .collect();
        let mut distances = Vec::new();
        let mut height_deltas = Vec::new();
        for portal in geometry.portals() {
            let delta: [f64; 3] = std::array::from_fn(|axis| {
                centers[portal.upper_component as usize][axis]
                    - centers[portal.lower_component as usize][axis]
            });
            let length = delta.iter().map(|d| d * d).sum::<f64>().sqrt();
            if !length.is_finite() || length <= 0.0 {
                return Err(ComponentError::InvalidState);
            }
            distances.push(length);
            height_deltas.push(delta);
        }
        let top_area: Vec<_> = geometry
            .open_top_faces()
            .into_iter()
            .map(|faces| {
                if config.open_top {
                    f64::from(faces) * config.voxel_size_m.powi(2)
                } else {
                    0.0
                }
            })
            .collect();
        let top_y = f64::from(geometry.fine_spec().dimensions()[1]) * config.voxel_size_m;
        let top_distance: Vec<_> = centers.iter().map(|c| top_y - c[1]).collect();
        let mut adjacency = vec![Vec::new(); n];
        for portal in geometry.portals() {
            let (a, b) = (
                portal.lower_component as usize,
                portal.upper_component as usize,
            );
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
        let mut seen = vec![false; n];
        let mut pinned = vec![false; n];
        let mut regions = Vec::new();
        for root in 0..n {
            if seen[root] {
                continue;
            }
            let mut queue = VecDeque::from([root]);
            seen[root] = true;
            let mut anchored = false;
            let mut region = Vec::new();
            while let Some(i) = queue.pop_front() {
                region.push(i);
                anchored |= top_area[i] > 0.0;
                for &next in &adjacency[i] {
                    if !seen[next] {
                        seen[next] = true;
                        queue.push_back(next);
                    }
                }
            }
            // One explicit pressure gauge per all-Neumann connected region.
            // No coupling is added across walls or isolated domains.
            pinned[root] = !anchored;
            regions.push(region);
        }
        let edge_count = geometry.portals().len();
        Ok(Self {
            geometry,
            config,
            capacity,
            amounts,
            pressure: vec![0.0; n],
            flux: vec![0.0; edge_count],
            top_flux: vec![0.0; n],
            top_area,
            distances,
            height_deltas,
            top_distance,
            pinned,
            regions,
            outflow: 0.0,
        })
    }

    pub fn amounts_m3(&self) -> &[f64] {
        &self.amounts
    }
    pub fn pressure_pa(&self) -> &[f64] {
        &self.pressure
    }
    pub fn flux_m3_s(&self) -> &[f64] {
        &self.flux
    }
    pub fn water_volume_m3(&self) -> f64 {
        self.amounts.iter().sum()
    }
    pub fn outflow_m3(&self) -> f64 {
        self.outflow
    }

    /// Horizontal hydrostatic reference per connected open region. Inverting
    /// exact open-layer capacity does not alter amounts or transport fluxes.
    /// The pressure split retains density departures as buoyancy; it is not a
    /// still-water detector and never skips a requested operator iteration.
    fn hydrostatic_reference(&self) -> Result<(Vec<f64>, Vec<f64>), ComponentError> {
        let size = self.config.voxel_size_m;
        let voxel_volume = size.powi(3);
        let ny = self.geometry.fine_spec().dimensions()[1] as usize;
        let coarse_nx = self.geometry.coarse_dimensions()[0] as usize;
        let coarse_ny = self.geometry.coarse_dimensions()[1] as usize;
        let factor = self.geometry.factor() as usize;
        let mut reference_density = vec![self.config.air_density; self.amounts.len()];
        let mut pressure = vec![0.0; self.amounts.len()];
        for region in &self.regions {
            let mut layers = vec![0u64; ny];
            let water = region.iter().map(|&i| self.amounts[i]).sum::<f64>() / voxel_volume;
            if !water.is_finite() {
                return Err(ComponentError::InvalidState);
            }
            for &i in region {
                let c = &self.geometry.components()[i];
                let base = c.coarse_index / coarse_nx % coarse_ny * factor;
                for (y, &count) in c.layer_counts[..factor].iter().enumerate() {
                    layers[base + y] += u64::from(count);
                }
            }
            let mut remaining = water;
            let mut height = ny as f64;
            for (y, &count) in layers.iter().enumerate() {
                if count > 0 && remaining < count as f64 {
                    height = y as f64 + remaining / count as f64;
                    break;
                }
                remaining -= count as f64;
            }
            for &i in region {
                let c = &self.geometry.components()[i];
                let base = c.coarse_index / coarse_nx % coarse_ny * factor;
                let fraction = c.volume_below(height - base as f64) / f64::from(c.voxel_count);
                reference_density[i] = self.config.air_density
                    + fraction * (self.config.water_density - self.config.air_density);
                let y = self.geometry.component_centroid(i)[1] * size;
                pressure[i] = -self.config.gravity[1]
                    * (self.config.air_density * (ny as f64 * size - y)
                        + (self.config.water_density - self.config.air_density)
                            * (height * size - y).max(0.0));
            }
            if let Some(&root) = region.first().filter(|&&root| self.pinned[root]) {
                let gauge = pressure[root];
                for &i in region {
                    pressure[i] -= gauge;
                }
            }
        }
        Ok((reference_density, pressure))
    }

    /// Inject a bounded predicted volume-flux field for operator tests or a
    /// future velocity-advection stage. Positive flux goes lower -> upper.
    pub fn set_predicted_flux(&mut self, flux: &[f64]) -> Result<(), ComponentError> {
        if flux.len() != self.flux.len() || flux.iter().any(|v| !v.is_finite()) {
            return Err(ComponentError::InvalidState);
        }
        self.flux.copy_from_slice(flux);
        Ok(())
    }

    /// Two-phase finite-volume pressure projection. Each component owns a
    /// pressure row; density uses water/open capacity, never water/whole box.
    /// Vertical gravity uses a hydrostatic pressure-reference split; density
    /// departures and horizontal forces drive the perturbation projection.
    /// The centroid approximation is still experimental. A failed
    /// solve installs no pressure or flux. Sealed-air compression is not yet
    /// implemented: closed regions here enforce incompressibility.
    pub fn project(&mut self, dt: f64, gravity: bool) -> Result<ProjectionMetrics, ComponentError> {
        if !dt.is_finite() || dt <= 0.0 {
            return Err(ComponentError::InvalidConfig);
        }
        let n = self.amounts.len();
        let density: Vec<_> = self
            .amounts
            .iter()
            .zip(&self.capacity)
            .map(|(v, c)| {
                self.config.air_density
                    + (v / c) * (self.config.water_density - self.config.air_density)
            })
            .collect();
        let mut candidate = self.flux.clone();
        let (reference_density, reference_pressure) = if gravity {
            self.hydrostatic_reference()?
        } else {
            (density.clone(), vec![0.0; n])
        };
        let mut top = self.top_flux.clone();
        let mut weights = Vec::new();
        let mut diag = vec![0.0; n];
        for (i, edge) in self.geometry.portals().iter().enumerate() {
            let (a, b) = (edge.lower_component as usize, edge.upper_component as usize);
            let area = f64::from(edge.voxel_faces) * self.config.voxel_size_m.powi(2);
            let weight = dt * area / (0.5 * (density[a] + density[b]) * self.distances[i]);
            if !weight.is_finite() || weight <= 0.0 {
                return Err(ComponentError::InvalidState);
            }
            weights.push(weight);
            diag[a] += weight;
            diag[b] += weight;
            if gravity {
                // Analytically cancel the hydrostatic reference gradient with
                // its pressure correction before solving for perturbations.
                // Residual buoyancy and horizontal forces still drive motion.
                let residual_density = 0.5
                    * ((density[a] - reference_density[a]) + (density[b] - reference_density[b]));
                let mean_density = 0.5 * (density[a] + density[b]);
                let delta = self.height_deltas[i];
                let g = self.config.gravity[0] * delta[0]
                    + self.config.gravity[2] * delta[2]
                    + residual_density / mean_density * self.config.gravity[1] * delta[1];
                candidate[i] += area * dt * g / self.distances[i];
            }
        }
        let mut top_weights = vec![0.0; n];
        for i in 0..n {
            if self.top_area[i] > 0.0 {
                let w = dt * self.top_area[i] / (density[i] * self.top_distance[i]);
                if !w.is_finite() || w <= 0.0 {
                    return Err(ComponentError::InvalidState);
                }
                top_weights[i] = w;
                diag[i] += w;
                if gravity {
                    top[i] += dt
                        * self.top_area[i]
                        * self.config.gravity[1]
                        * (density[i] - reference_density[i])
                        / density[i];
                }
            }
            if self.pinned[i] {
                diag[i] = 1.0;
            }
        }
        let mut rhs = divergence(&self.geometry, &candidate, &top);
        for (i, value) in rhs.iter_mut().enumerate() {
            *value = if self.pinned[i] { 0.0 } else { -*value };
        }
        if candidate
            .iter()
            .chain(&top)
            .chain(&rhs)
            .any(|v| !v.is_finite())
        {
            return Err(ComponentError::InvalidState);
        }
        let apply = |x: &[f64], out: &mut [f64]| {
            out.fill(0.0);
            for (edge, &weight) in self.geometry.portals().iter().zip(&weights) {
                let (a, b) = (edge.lower_component as usize, edge.upper_component as usize);
                let pa = if self.pinned[a] { 0.0 } else { x[a] };
                let pb = if self.pinned[b] { 0.0 } else { x[b] };
                let d = weight * (pa - pb);
                if !self.pinned[a] {
                    out[a] += d;
                }
                if !self.pinned[b] {
                    out[b] -= d;
                }
            }
            for i in 0..n {
                out[i] = if self.pinned[i] {
                    x[i]
                } else {
                    out[i] + top_weights[i] * x[i]
                };
            }
        };
        let mut p = vec![0.0; n];
        let mut residual = rhs.clone();
        let mut z: Vec<_> = residual.iter().zip(&diag).map(|(r, d)| r / d).collect();
        let mut direction = z.clone();
        let mut rz = dot(&residual, &z);
        let initial = dot(&rhs, &rhs).sqrt();
        let threshold = self
            .config
            .absolute_tolerance
            .max(initial * self.config.relative_tolerance);
        let mut norm = initial;
        let mut iterations = 0;
        let mut ap = vec![0.0; n];
        while norm > threshold && iterations < self.config.max_iterations {
            apply(&direction, &mut ap);
            let denominator = dot(&direction, &ap);
            if !denominator.is_finite() || denominator <= 0.0 {
                break;
            }
            let alpha = rz / denominator;
            for i in 0..n {
                p[i] += alpha * direction[i];
                residual[i] -= alpha * ap[i];
            }
            norm = dot(&residual, &residual).sqrt();
            iterations += 1;
            if norm <= threshold {
                break;
            }
            for i in 0..n {
                z[i] = residual[i] / diag[i];
            }
            let next_rz = dot(&residual, &z);
            let beta = next_rz / rz;
            for i in 0..n {
                direction[i] = z[i] + beta * direction[i];
            }
            rz = next_rz;
        }
        // Verify the true residual rather than accepting recursive CG drift.
        apply(&p, &mut ap);
        norm = rhs
            .iter()
            .zip(&ap)
            .map(|(b, a)| (b - a).powi(2))
            .sum::<f64>()
            .sqrt();
        if !norm.is_finite() || norm > threshold {
            return Err(ComponentError::PressureNotConverged {
                iterations,
                residual: norm,
            });
        }
        for (i, edge) in self.geometry.portals().iter().enumerate() {
            candidate[i] +=
                weights[i] * (p[edge.lower_component as usize] - p[edge.upper_component as usize]);
        }
        for i in 0..n {
            top[i] += top_weights[i] * p[i];
        }
        if p.iter()
            .chain(&candidate)
            .chain(&top)
            .any(|v| !v.is_finite())
        {
            return Err(ComponentError::InvalidState);
        }
        let div = divergence(&self.geometry, &candidate, &top);
        let max_divergence = div
            .iter()
            .zip(&self.capacity)
            .map(|(d, v)| (d / v).abs())
            .fold(0.0, f64::max);
        let mut speed = 0.0_f64;
        for (edge, q) in self.geometry.portals().iter().zip(&candidate) {
            speed = speed
                .max(q.abs() / (f64::from(edge.voxel_faces) * self.config.voxel_size_m.powi(2)));
        }
        for (&q, &area) in top.iter().zip(&self.top_area) {
            if area > 0.0 {
                speed = speed.max(q.abs() / area);
            }
        }
        if !max_divergence.is_finite() || !speed.is_finite() {
            return Err(ComponentError::InvalidState);
        }
        for (p, reference) in p.iter_mut().zip(reference_pressure) {
            *p += reference;
        }
        if p.iter().any(|p| !p.is_finite()) {
            return Err(ComponentError::InvalidState);
        }
        self.pressure = p;
        self.flux = candidate;
        self.top_flux = top;
        Ok(ProjectionMetrics {
            iterations,
            residual: norm,
            max_divergence_per_s: max_divergence,
            max_connection_speed_m_s: speed,
        })
    }

    /// First-order donor transport over matched portals. Rejects a requested
    /// CFL above the budget; no hidden clipping, retries or lost fluid time.
    /// Pairwise equal/opposite transfers conserve water, including top outflow.
    /// An iterative paired-flux limiter enforces capacity; after 64 reductions
    /// an unresolved violation is an atomic failure. Amounts are never clipped.
    pub fn transport(&mut self, dt: f64) -> Result<TransportMetrics, ComponentError> {
        if !dt.is_finite() || dt <= 0.0 {
            return Err(ComponentError::InvalidConfig);
        }
        let mut outgoing = vec![0.0; self.amounts.len()];
        for (edge, &q) in self.geometry.portals().iter().zip(&self.flux) {
            let donor = if q >= 0.0 {
                edge.lower_component
            } else {
                edge.upper_component
            } as usize;
            outgoing[donor] += q.abs();
        }
        for (o, q) in outgoing.iter_mut().zip(&self.top_flux) {
            *o += q.max(0.0);
        }
        let measured = outgoing
            .iter()
            .zip(&self.capacity)
            .map(|(q, v)| dt * q / v)
            .fold(0.0, f64::max);
        if !measured.is_finite() || measured > self.config.cfl {
            return Err(ComponentError::Cfl {
                measured,
                limit: self.config.cfl,
            });
        }
        let fractions: Vec<_> = self
            .amounts
            .iter()
            .zip(&self.capacity)
            .map(|(v, c)| v / c)
            .collect();
        let mut transfers = Vec::with_capacity(self.flux.len());
        for (edge, &q) in self.geometry.portals().iter().zip(&self.flux) {
            let (a, b) = (edge.lower_component as usize, edge.upper_component as usize);
            let water = dt * q * if q >= 0.0 { fractions[a] } else { fractions[b] };
            transfers.push(water);
        }
        let mut exports: Vec<_> = self
            .top_flux
            .iter()
            .zip(&fractions)
            .map(|(q, f)| dt * q.max(0.0) * f)
            .collect();
        let requested =
            transfers.iter().map(|v| v.abs()).sum::<f64>() + exports.iter().sum::<f64>();
        let mut incoming = vec![0.0; self.amounts.len()];
        let mut leaving = vec![0.0; self.amounts.len()];
        let mut next = vec![0.0; self.amounts.len()];
        let mut in_scale = vec![1.0; self.amounts.len()];
        let mut out_scale = vec![1.0; self.amounts.len()];
        for pass in 0..=64 {
            incoming.fill(0.0);
            leaving.copy_from_slice(&exports);
            for (edge, &water) in self.geometry.portals().iter().zip(&transfers) {
                let (a, b) = (edge.lower_component as usize, edge.upper_component as usize);
                let (donor, receiver) = if water >= 0.0 { (a, b) } else { (b, a) };
                leaving[donor] += water.abs();
                incoming[receiver] += water.abs();
            }
            in_scale.fill(1.0);
            out_scale.fill(1.0);
            let mut bad = None;
            for i in 0..next.len() {
                next[i] = self.amounts[i] + (incoming[i] - leaving[i]);
                if !next[i].is_finite() {
                    return Err(ComponentError::InvalidState);
                }
                if next[i] > self.capacity[i] {
                    in_scale[i] = ((self.capacity[i] - self.amounts[i] + leaving[i]) / incoming[i])
                        .clamp(0.0, 1.0)
                        * (1.0 - 32.0 * f64::EPSILON);
                    bad = Some(i);
                } else if next[i] < 0.0 {
                    out_scale[i] = ((self.amounts[i] + incoming[i]) / leaving[i]).clamp(0.0, 1.0)
                        * (1.0 - 32.0 * f64::EPSILON);
                    bad = Some(i);
                }
            }
            if let Some(i) = bad {
                if pass == 64 {
                    return Err(ComponentError::Capacity {
                        component: i,
                        amount_m3: next[i],
                        capacity_m3: self.capacity[i],
                    });
                }
                // Reduce each paired transfer once, with the same value used on
                // both sides. Outgoing capacity remains available to incoming
                // flow, so saturated chains/cycles are not globally blocked.
                for (edge, water) in self.geometry.portals().iter().zip(&mut transfers) {
                    let (a, b) = (edge.lower_component as usize, edge.upper_component as usize);
                    let (donor, receiver) = if *water >= 0.0 { (a, b) } else { (b, a) };
                    *water *= out_scale[donor].min(in_scale[receiver]);
                }
                for (i, export) in exports.iter_mut().enumerate() {
                    *export *= out_scale[i];
                }
            } else {
                let outflow = exports.iter().sum::<f64>();
                let moved = transfers.iter().map(|v| v.abs()).sum::<f64>() + outflow;
                if !(self.outflow + outflow).is_finite()
                    || !moved.is_finite()
                    || !requested.is_finite()
                {
                    return Err(ComponentError::InvalidState);
                }
                self.amounts = next;
                self.outflow += outflow;
                return Ok(TransportMetrics {
                    limiter_passes: pass,
                    limited_water_m3: (requested - moved).max(0.0),
                    moved_water_m3: moved,
                });
            }
        }
        unreachable!("bounded limiter returns accepted state or explicit failure")
    }

    /// Atomic experimental force/projection/transport iteration. Velocity
    /// advection, sealed-air compression and edit displacement remain separate
    /// acceptance requirements; this method does not stand in for a full step.
    pub fn advance_operators(&mut self, dt: f64) -> Result<ProjectionMetrics, ComponentError> {
        let mut candidate = self.clone();
        let metrics = candidate.project(dt, true)?;
        candidate.transport(dt)?;
        *self = candidate;
        Ok(metrics)
    }
}

fn divergence(geometry: &CutCellGeometry, flux: &[f64], top: &[f64]) -> Vec<f64> {
    let mut div = top.to_vec();
    for (edge, &q) in geometry.portals().iter().zip(flux) {
        div[edge.lower_component as usize] += q;
        div[edge.upper_component as usize] -= q;
    }
    div
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
