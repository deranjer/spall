//! Experimental phase-aware pressure aggregation and transfer routing.
//! This defines a piecewise-constant pressure basis and conservative ledgers,
//! not a pressure/momentum timestep or a production solver. Internal hydraulic
//! resistance, hydrostatic balance, interpolation and staggered dual inertia
//! still require the coupling/accuracy gates.

use crate::phase_water::{PhaseError, PhaseFace, PhaseTransportMetrics, PhaseWater};
use spall_core::GlobalCell;
use std::{collections::BTreeMap, sync::Arc};

const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub struct GraphLimits {
    pub max_fine_cells: usize,
    pub max_rows: usize,
    pub max_connections: usize,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GraphError {
    #[error("phase graph: {0}")]
    Phase(#[from] PhaseError),
    #[error("phase graph {kind} count {requested} exceeds limit {limit}")]
    Limit {
        kind: &'static str,
        requested: usize,
        limit: usize,
    },
    #[error("invalid phase graph coefficient or transfer state")]
    InvalidState,
    #[error("phase graph belongs to a different immutable phase snapshot")]
    StaleSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseClass {
    Wet,
    Dry,
}

/// Snapshot-local pressure row. Neither indices nor anchors are restart IDs.
#[derive(Debug, Clone)]
pub struct PhaseRow {
    pub component: u32,
    pub class: PhaseClass,
    pub anchor: GlobalCell,
    pub fine_cells: u32,
    pub capacity_m3: f64,
    pub water_m3: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct PhaseConnection {
    pub lower_row: u32,
    pub upper_row: u32,
    pub axis: u8,
    pub fine_faces: u32,
}

impl PhaseConnection {
    fn key(&self) -> (u32, u32, u8) {
        (self.lower_row, self.upper_row, self.axis)
    }
}

#[derive(Debug, Clone)]
pub struct PhaseGraph {
    snapshot: PhaseWater,
    labels: Vec<u32>,
    rows: Vec<PhaseRow>,
    connections: Arc<[PhaseConnection]>,
}

/// Galerkin operator P^T A P for this piecewise-constant basis. Fine pressure
/// coefficients must come from the physical solver; no inertia is invented.
#[derive(Debug, Clone)]
pub struct AggregatedPressure {
    connections: Arc<[PhaseConnection]>,
    weights: Vec<f64>,
    top_weights: Vec<f64>,
}

#[derive(Debug, Clone)]
pub struct TransferLedger {
    pub water_delta_m3: Vec<f64>,
    pub momentum_delta_kg_m_s: Vec<[f64; 3]>,
    /// Signed lower-row -> upper-row water volume, ordered by connections().
    pub connection_water_m3: Vec<f64>,
}

impl PhaseGraph {
    pub fn build(phase: &PhaseWater, limits: GraphLimits) -> Result<Self, GraphError> {
        let geometry = phase.geometry();
        let spec = geometry.fine_spec();
        limit("fine cells", spec.cell_count(), limits.max_fine_cells)?;
        let fractions = phase.fractions();
        let dims = spec.dimensions().map(|n| n as usize);
        let strides = [1, dims[0], dims[0] * dims[1]];
        let voxel_volume = phase.voxel_size_m().powi(3);
        let mut labels = vec![NONE; fractions.len()];
        let mut rows = Vec::new();
        // Each queue is confined to one original coarse open component.
        let mut queue = Vec::new();
        for root in 0..fractions.len() {
            let Some(component) = geometry.component_at_index(root) else {
                continue;
            };
            if labels[root] != NONE {
                continue;
            }
            limit("rows", rows.len() + 1, limits.max_rows.min(NONE as usize))?;
            let row = rows.len() as u32;
            let wet = fractions[root] > 0.0;
            labels[root] = row;
            queue.clear();
            queue.push(root);
            let mut cursor = 0;
            let mut water = 0.0;
            while cursor < queue.len() {
                let i = queue[cursor];
                cursor += 1;
                water += fractions[i];
                let pos = [i % dims[0], i / dims[0] % dims[1], i / strides[2]];
                for axis in 0..3 {
                    for next in [
                        (pos[axis] > 0).then(|| i - strides[axis]),
                        (pos[axis] + 1 < dims[axis]).then(|| i + strides[axis]),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if labels[next] != NONE
                            || geometry.component_at_index(next) != Some(component)
                            || (fractions[next] > 0.0) != wet
                        {
                            continue;
                        }
                        let face = PhaseFace {
                            lower: i.min(next),
                            upper: i.max(next),
                            axis: axis as u8,
                        };
                        if wet && PhaseWater::overlap_in(fractions, face) == 0.0 {
                            continue;
                        }
                        labels[next] = row;
                        queue.push(next);
                    }
                }
            }
            rows.push(PhaseRow {
                component,
                class: if wet {
                    PhaseClass::Wet
                } else {
                    PhaseClass::Dry
                },
                anchor: spec.cell_at(root),
                fine_cells: queue.len() as u32,
                capacity_m3: queue.len() as f64 * voxel_volume,
                water_m3: water * voxel_volume,
            });
        }
        let mut counts = BTreeMap::<(u32, u32, u8), u32>::new();
        for f in phase.faces().iter() {
            let (a, b) = (labels[f.lower], labels[f.upper]);
            if a == b {
                continue;
            }
            let key = (a.min(b), a.max(b), f.axis);
            if !counts.contains_key(&key) {
                limit("connections", counts.len() + 1, limits.max_connections)?;
            }
            let count = counts.entry(key).or_default();
            *count = count.checked_add(1).ok_or(GraphError::InvalidState)?;
        }
        let connections: Vec<_> = counts
            .into_iter()
            .map(|((a, b, axis), fine_faces)| PhaseConnection {
                lower_row: a,
                upper_row: b,
                axis,
                fine_faces,
            })
            .collect();
        Ok(Self {
            snapshot: phase.clone(),
            labels,
            rows,
            connections: connections.into(),
        })
    }

    pub fn rows(&self) -> &[PhaseRow] {
        &self.rows
    }
    pub fn connections(&self) -> &[PhaseConnection] {
        &self.connections
    }
    pub fn row_at_index(&self, index: usize) -> Option<u32> {
        self.labels.get(index).copied().filter(|&r| r != NONE)
    }
    /// Excludes the shared immutable phase/geometry, build queue/map and operators.
    pub fn array_storage_bytes(&self) -> usize {
        self.labels.capacity() * size_of::<u32>()
            + self.rows.capacity() * size_of::<PhaseRow>()
            + self.connections.len() * size_of::<PhaseConnection>()
    }

    fn binding(&self, phase: &PhaseWater) -> Result<(), GraphError> {
        if self.snapshot.same_snapshot(phase) {
            Ok(())
        } else {
            Err(GraphError::StaleSnapshot)
        }
    }
    fn connection(&self, a: u32, b: u32, axis: u8) -> usize {
        self.connections
            .binary_search_by_key(&(a.min(b), a.max(b), axis), PhaseConnection::key)
            .expect("cross-row fine face has a connection")
    }

    pub fn pressure_operator(
        &self,
        phase: &PhaseWater,
        fine_weights: &[f64],
        fine_top_weights: &[f64],
    ) -> Result<AggregatedPressure, GraphError> {
        self.binding(phase)?;
        if fine_weights.len() != phase.faces().len()
            || fine_top_weights.len() != self.labels.len()
            || fine_weights
                .iter()
                .chain(fine_top_weights)
                .any(|v| !v.is_finite() || *v < 0.0)
        {
            return Err(GraphError::InvalidState);
        }
        let mut weights = vec![0.0; self.connections.len()];
        for (f, &weight) in phase.faces().iter().zip(fine_weights) {
            let (a, b) = (self.labels[f.lower], self.labels[f.upper]);
            if a != b {
                weights[self.connection(a, b, f.axis)] += weight;
            }
        }
        let dims = phase
            .geometry()
            .fine_spec()
            .dimensions()
            .map(|n| n as usize);
        let mut top_weights = vec![0.0; self.rows.len()];
        for (i, &weight) in fine_top_weights.iter().enumerate() {
            if weight == 0.0 {
                continue;
            }
            if self.labels[i] == NONE || i / dims[0] % dims[1] != dims[1] - 1 {
                return Err(GraphError::InvalidState);
            }
            top_weights[self.labels[i] as usize] += weight;
        }
        if weights.iter().chain(&top_weights).any(|v| !v.is_finite()) {
            return Err(GraphError::InvalidState);
        }
        Ok(AggregatedPressure {
            connections: self.connections.clone(),
            weights,
            top_weights,
        })
    }

    /// A read-only routing ledger, not velocity advection or pressure coupling.
    /// Velocities are the caller's donor velocities per fine face. Both liquid
    /// mass and all three momentum components use the same accepted transfers.
    pub fn transfer_ledger(
        &self,
        phase: &PhaseWater,
        accepted_fraction_transfers: &[f64],
        donor_velocities: &[[f64; 3]],
        density: f64,
    ) -> Result<TransferLedger, GraphError> {
        self.binding(phase)?;
        if !density.is_finite()
            || density <= 0.0
            || accepted_fraction_transfers.len() != phase.faces().len()
            || donor_velocities.len() != phase.faces().len()
            || accepted_fraction_transfers.iter().any(|v| !v.is_finite())
            || donor_velocities.iter().flatten().any(|v| !v.is_finite())
        {
            return Err(GraphError::InvalidState);
        }
        let mut ledger = TransferLedger {
            water_delta_m3: vec![0.0; self.rows.len()],
            momentum_delta_kg_m_s: vec![[0.0; 3]; self.rows.len()],
            connection_water_m3: vec![0.0; self.connections.len()],
        };
        let volume = phase.voxel_size_m().powi(3);
        for ((f, &transfer), velocity) in phase
            .faces()
            .iter()
            .zip(accepted_fraction_transfers)
            .zip(donor_velocities)
        {
            let (a, b) = (self.labels[f.lower], self.labels[f.upper]);
            if a == b {
                continue;
            }
            let water = transfer * volume;
            ledger.water_delta_m3[a as usize] -= water;
            ledger.water_delta_m3[b as usize] += water;
            ledger.connection_water_m3[self.connection(a, b, f.axis)] +=
                if a < b { water } else { -water };
            for (axis, &v) in velocity.iter().enumerate() {
                let p = water * density * v;
                ledger.momentum_delta_kg_m_s[a as usize][axis] -= p;
                ledger.momentum_delta_kg_m_s[b as usize][axis] += p;
            }
        }
        if ledger
            .water_delta_m3
            .iter()
            .chain(&ledger.connection_water_m3)
            .chain(ledger.momentum_delta_kg_m_s.iter().flatten())
            .any(|v| !v.is_finite())
        {
            return Err(GraphError::InvalidState);
        }
        Ok(ledger)
    }

    /// Validates water transport and routes exactly its limited transfers.
    /// Original phase/graph remain immutable even on late ledger failure.
    pub fn transport_candidate(
        &self,
        phase: &PhaseWater,
        dt: f64,
        flux: &[f64],
        cfl: f64,
        density: f64,
        donor_velocities: &[[f64; 3]],
    ) -> Result<(PhaseWater, PhaseTransportMetrics, TransferLedger), GraphError> {
        self.binding(phase)?;
        let mut candidate = phase.clone();
        let (metrics, transfers) = candidate.transport_with_transfers(dt, flux, cfl)?;
        let ledger = self.transfer_ledger(phase, &transfers, donor_velocities, density)?;
        Ok((candidate, metrics, ledger))
    }
}

impl AggregatedPressure {
    pub fn apply(&self, pressure: &[f64]) -> Result<Vec<f64>, GraphError> {
        if pressure.len() != self.top_weights.len() || pressure.iter().any(|p| !p.is_finite()) {
            return Err(GraphError::InvalidState);
        }
        let mut out: Vec<_> = pressure
            .iter()
            .zip(&self.top_weights)
            .map(|(p, w)| p * w)
            .collect();
        for (edge, &weight) in self.connections.iter().zip(&self.weights) {
            let (a, b) = (edge.lower_row as usize, edge.upper_row as usize);
            let difference = weight * (pressure[a] - pressure[b]);
            out[a] += difference;
            out[b] -= difference;
        }
        if out.iter().any(|v| !v.is_finite()) {
            return Err(GraphError::InvalidState);
        }
        Ok(out)
    }
    pub fn array_storage_bytes(&self) -> usize {
        (self.weights.capacity() + self.top_weights.capacity()) * size_of::<f64>()
    }
}

fn limit(kind: &'static str, requested: usize, bound: usize) -> Result<(), GraphError> {
    if requested > bound {
        Err(GraphError::Limit {
            kind,
            requested,
            limit: bound,
        })
    } else {
        Ok(())
    }
}
