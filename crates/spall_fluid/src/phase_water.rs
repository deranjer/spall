//! Experimental ENG-122 phase-placement prerequisite, not a pressure solver.
//! Dense fine-voxel fractions retain basin provenance lost by coarse totals.
//! Interfaces are horizontal within each fine voxel. General PLIC orientation,
//! pressure/momentum coupling, edit remapping and durable recovery are pending.

use std::collections::VecDeque;
use std::sync::Arc;

use spall_core::GlobalCell;

use crate::cut_cell::{CutCellGeometry, GeometryError};

#[derive(Debug, Clone, Copy)]
pub struct PhaseLimits {
    pub max_fine_cells: usize,
    pub max_faces: usize,
    pub max_basins: usize,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PhaseError {
    #[error("phase geometry: {0}")]
    Geometry(#[from] GeometryError),
    #[error("phase {kind} count {requested} exceeds limit {limit}")]
    Limit {
        kind: &'static str,
        requested: usize,
        limit: usize,
    },
    #[error("invalid phase configuration, amounts, or face flux")]
    InvalidState,
    #[error("phase transport CFL {measured} exceeds limit {limit}")]
    Cfl { measured: f64, limit: f64 },
    #[error("phase capacity limiter failed after 64 passes")]
    LimiterExhausted,
}

/// Snapshot-local indices, never persistent IDs. Positive flux goes lower to
/// upper on the indicated coordinate axis. Includes faces within coarse cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseFace {
    pub lower: usize,
    pub upper: usize,
    pub axis: u8,
}

// Two axis bits and thirty lower-index bits. Endpoints are never persistent IDs.
const MAX_PACKED_FINE_CELLS: usize = 1 << 30;

fn pack_face(lower: usize, axis: u8) -> Result<u32, PhaseError> {
    if lower >= MAX_PACKED_FINE_CELLS || axis > 2 {
        return Err(PhaseError::InvalidState);
    }
    Ok(((lower as u32) << 2) | u32::from(axis))
}

fn decode_face(packed: u32, strides: [usize; 3]) -> PhaseFace {
    let lower = (packed >> 2) as usize;
    let axis = (packed & 3) as u8;
    PhaseFace {
        lower,
        upper: lower + strides[axis as usize],
        axis,
    }
}

/// Allocation-free decoded view. Ordering and endpoints match the original
/// fine-face list; iteration returns values instead of stored record references.
#[derive(Debug, Clone, Copy)]
pub struct PhaseFaces<'a> {
    packed: &'a [u32],
    strides: [usize; 3],
}

impl<'a> PhaseFaces<'a> {
    pub fn len(self) -> usize {
        self.packed.len()
    }
    pub fn is_empty(self) -> bool {
        self.packed.is_empty()
    }
    pub fn iter(
        self,
    ) -> impl ExactSizeIterator<Item = PhaseFace> + DoubleEndedIterator + Clone + 'a {
        self.packed
            .iter()
            .map(move |&packed| decode_face(packed, self.strides))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PortalPhaseAreas {
    pub overlap_m2: f64,
    pub lower_donor_m2: f64,
    pub upper_donor_m2: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaterBasin {
    /// Deterministic lowest linear fine-cell anchor of this phase snapshot.
    /// Basin identity may change when water joins or separates.
    pub anchor: GlobalCell,
    pub water_m3: f64,
    pub wet_cells: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PhaseTransportMetrics {
    pub moved_water_m3: f64,
    pub limited_water_m3: f64,
    pub limiter_passes: u32,
    /// Allocated transfer/incoming/outgoing array bytes at numeric workspace
    /// peak. Excludes state, caller flux, and the subsequent basin-check scratch.
    pub numeric_scratch_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct PhaseWater {
    geometry: Arc<CutCellGeometry>,
    // Share snapshots without copying the fine array, including when installing
    // an accepted Vec. Arc<Vec<_>> avoids Vec -> Arc<[ _ ]> relocation scratch.
    fractions: Arc<Vec<f64>>,
    faces: Arc<[u32]>,
    strides: [usize; 3],
    voxel_size_m: f64,
    limits: PhaseLimits,
}

impl PhaseWater {
    pub fn new(
        geometry: Arc<CutCellGeometry>,
        fractions: &[f64],
        voxel_size_m: f64,
        limits: PhaseLimits,
    ) -> Result<Self, PhaseError> {
        let spec = geometry.fine_spec();
        check_limit("fine cells", spec.cell_count(), limits.max_fine_cells)?;
        check_limit(
            "packed fine cells",
            spec.cell_count(),
            MAX_PACKED_FINE_CELLS,
        )?;
        if !voxel_size_m.is_finite()
            || voxel_size_m <= 0.0
            || !voxel_size_m.powi(3).is_finite()
            || voxel_size_m.powi(3) <= 0.0
            || !(spec.cell_count() as f64 * voxel_size_m.powi(3)).is_finite()
        {
            return Err(PhaseError::InvalidState);
        }
        geometry.aggregate_amounts(fractions)?;
        let dims = spec.dimensions().map(|n| n as usize);
        let strides = [1, dims[0], dims[0] * dims[1]];
        let mut faces = Vec::new();
        for lower in 0..spec.cell_count() {
            if geometry.component_at(spec.cell_at(lower)).is_none() {
                continue;
            }
            let pos = [
                lower % dims[0],
                lower / dims[0] % dims[1],
                lower / strides[2],
            ];
            for axis in 0..3 {
                let upper = lower + strides[axis];
                if pos[axis] + 1 < dims[axis]
                    && geometry.component_at(spec.cell_at(upper)).is_some()
                {
                    check_limit("faces", faces.len() + 1, limits.max_faces)?;
                    faces.push(pack_face(lower, axis as u8)?);
                }
            }
        }
        // Immutable face topology is retained for the life of the snapshot;
        // release growth slack before measuring or keeping its array storage.
        faces.shrink_to_fit();
        let state = Self {
            geometry,
            fractions: fractions.to_vec().into(),
            faces: faces.into(),
            strides,
            voxel_size_m,
            limits,
        };
        state.basins()?;
        Ok(state)
    }

    pub fn fractions(&self) -> &[f64] {
        &self.fractions
    }
    pub fn voxel_size_m(&self) -> f64 {
        self.voxel_size_m
    }

    /// Atomic candidate from a coupled solver on this exact immutable geometry.
    /// The derived packed topology is shared; all phase validation is retained.
    pub(crate) fn with_fractions(&self, fractions: Vec<f64>) -> Result<Self, PhaseError> {
        self.geometry.aggregate_amounts(&fractions)?;
        self.basins_for(&fractions)?;
        Ok(Self {
            geometry: self.geometry.clone(),
            fractions: fractions.into(),
            faces: self.faces.clone(),
            strides: self.strides,
            voxel_size_m: self.voxel_size_m,
            limits: self.limits,
        })
    }
    pub fn geometry(&self) -> &Arc<CutCellGeometry> {
        &self.geometry
    }

    pub(crate) fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.geometry, &other.geometry)
            && Arc::ptr_eq(&self.fractions, &other.fractions)
            && self.voxel_size_m.to_bits() == other.voxel_size_m.to_bits()
    }
    pub fn faces(&self) -> PhaseFaces<'_> {
        PhaseFaces {
            packed: &self.faces,
            strides: self.strides,
        }
    }
    /// Allocated fraction/face array storage; excludes shared geometry and
    /// temporary basin/transport scratch space. This fine reference is costly.
    pub fn array_storage_bytes(&self) -> usize {
        self.fractions.capacity() * size_of::<f64>() + self.faces.len() * size_of::<u32>()
    }
    pub fn water_volume_m3(&self) -> f64 {
        self.fractions.iter().sum::<f64>() * self.voxel_size_m.powi(3)
    }
    /// Presentation/operator summary only; insufficient to recover phase state.
    pub fn component_amounts_m3(&self) -> Vec<f64> {
        self.geometry
            .aggregate_amounts(&self.fractions)
            .expect("private phase state retains valid fine fractions")
            .into_iter()
            .map(|v| v * self.voxel_size_m.powi(3))
            .collect()
    }

    /// Existing water overlap at a face, in m2. Horizontal neighbours overlap
    /// by min(fill); a vertical face is wet only if its lower voxel reaches the
    /// top and its upper voxel contains water at its bottom. No sleep threshold.
    pub fn wet_face_areas_m2(&self) -> Vec<f64> {
        self.faces()
            .iter()
            .map(|f| self.overlap(f) * self.voxel_size_m.powi(2))
            .collect()
    }

    /// Ordered like geometry.portals(). Dry receivers must not suppress donor
    /// flow, so existing overlap and each directional donor aperture differ.
    /// Coarse pressure cannot recover internal basin DOFs from this summary.
    pub fn portal_phase_areas_m2(&self) -> Vec<PortalPhaseAreas> {
        let mut areas = vec![PortalPhaseAreas::default(); self.geometry.portals().len()];
        let spec = self.geometry.fine_spec();
        let face_area = self.voxel_size_m.powi(2);
        for face in self.faces().iter() {
            let a = self
                .geometry
                .component_at(spec.cell_at(face.lower))
                .unwrap();
            let b = self
                .geometry
                .component_at(spec.cell_at(face.upper))
                .unwrap();
            if a == b {
                continue;
            }
            let index = self
                .geometry
                .portals()
                .binary_search_by_key(&(a, b, face.axis), |p| {
                    (p.lower_component, p.upper_component, p.axis)
                })
                .expect("all matched cross-component faces have a geometry portal");
            areas[index].overlap_m2 += self.overlap(face) * face_area;
            areas[index].lower_donor_m2 += self.donor_fraction(face, true) * face_area;
            areas[index].upper_donor_m2 += self.donor_fraction(face, false) * face_area;
        }
        areas
    }

    fn donor_fraction(&self, face: PhaseFace, positive: bool) -> f64 {
        let donor = if positive { face.lower } else { face.upper };
        if face.axis != 1 {
            self.fractions[donor]
        } else if positive {
            f64::from(self.fractions[donor] == 1.0)
        } else {
            f64::from(self.fractions[donor] > 0.0)
        }
    }

    fn overlap(&self, f: PhaseFace) -> f64 {
        Self::overlap_in(&self.fractions, f)
    }

    pub(crate) fn overlap_in(fractions: &[f64], f: PhaseFace) -> f64 {
        if f.axis == 1 {
            f64::from(fractions[f.lower] == 1.0 && fractions[f.upper] > 0.0)
        } else {
            fractions[f.lower].min(fractions[f.upper])
        }
    }

    /// Distinct wet regions, including separate pools inside one open coarse
    /// component. Air-only connections never merge them. Partial face overlap
    /// counts as connectivity; mere corner or interface contact does not.
    pub fn basins(&self) -> Result<Vec<WaterBasin>, PhaseError> {
        self.basins_for(&self.fractions)
    }

    fn basins_for(&self, fractions: &[f64]) -> Result<Vec<WaterBasin>, PhaseError> {
        let spec = self.geometry.fine_spec();
        let dims = spec.dimensions().map(|n| n as usize);
        let strides = [1, dims[0], dims[0] * dims[1]];
        let mut seen = vec![false; fractions.len()];
        let mut basins = Vec::new();
        for root in 0..fractions.len() {
            if seen[root] || fractions[root] == 0.0 {
                continue;
            }
            check_limit("basins", basins.len() + 1, self.limits.max_basins)?;
            let mut queue = VecDeque::from([root]);
            seen[root] = true;
            let mut amount = 0.0;
            let mut cells = 0;
            while let Some(i) = queue.pop_front() {
                amount += fractions[i];
                cells += 1;
                let pos = [i % dims[0], i / dims[0] % dims[1], i / strides[2]];
                for axis in 0..3 {
                    for next in [
                        (pos[axis] > 0).then(|| i - strides[axis]),
                        (pos[axis] + 1 < dims[axis]).then(|| i + strides[axis]),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let face = PhaseFace {
                            lower: i.min(next),
                            upper: i.max(next),
                            axis: axis as u8,
                        };
                        if !seen[next] && Self::overlap_in(fractions, face) > 0.0 {
                            seen[next] = true;
                            queue.push_back(next);
                        }
                    }
                }
            }
            basins.push(WaterBasin {
                anchor: spec.cell_at(root),
                water_m3: amount * self.voxel_size_m.powi(3),
                wet_cells: cells,
            });
        }
        Ok(basins)
    }

    /// Prescribed fine-face volume flux, not a pressure or momentum step.
    /// Closed domain; no sources, sinks or implicit coarse-flux distribution.
    /// Uniform face velocity and horizontal fine interfaces determine donor
    /// water flux. Paired transfers are limited without clipping amounts.
    pub fn transport(
        &mut self,
        dt: f64,
        flux_m3_s: &[f64],
        cfl: f64,
    ) -> Result<PhaseTransportMetrics, PhaseError> {
        self.transport_internal(dt, flux_m3_s, cfl, false)
            .map(|(metrics, _)| metrics)
    }

    /// The coupled ledger receives exactly the accepted signed transfers in
    /// fine-voxel fraction units. Default transport still drops this array
    /// before basin validation to retain its established scratch lifetime.
    pub(crate) fn transport_with_transfers(
        &mut self,
        dt: f64,
        flux_m3_s: &[f64],
        cfl: f64,
    ) -> Result<(PhaseTransportMetrics, Vec<f64>), PhaseError> {
        self.transport_internal(dt, flux_m3_s, cfl, true)
            .map(|(m, t)| (m, t.expect("requested accepted transfers")))
    }

    fn transport_internal(
        &mut self,
        dt: f64,
        flux_m3_s: &[f64],
        cfl: f64,
        retain_transfers: bool,
    ) -> Result<(PhaseTransportMetrics, Option<Vec<f64>>), PhaseError> {
        if !dt.is_finite()
            || dt <= 0.0
            || !cfl.is_finite()
            || !(0.0..=1.0).contains(&cfl)
            || cfl == 0.0
            || flux_m3_s.len() != self.faces.len()
            || flux_m3_s.iter().any(|q| !q.is_finite())
        {
            return Err(PhaseError::InvalidState);
        }
        let volume = self.voxel_size_m.powi(3);
        let mut outgoing = vec![0.0; self.fractions.len()];
        let mut transfers = Vec::with_capacity(self.faces.len());
        for (f, &q) in self.faces().iter().zip(flux_m3_s) {
            let donor = if q >= 0.0 { f.lower } else { f.upper };
            let swept = dt * q.abs() / volume;
            if !swept.is_finite() {
                return Err(PhaseError::InvalidState);
            }
            outgoing[donor] += swept;
            let wet = self.donor_fraction(f, q >= 0.0);
            transfers.push(dt * q / volume * wet);
        }
        let measured = outgoing.iter().copied().fold(0.0, f64::max);
        if !measured.is_finite() || measured > cfl * (1.0 + 32.0 * f64::EPSILON) {
            return Err(PhaseError::Cfl {
                measured,
                limit: cfl,
            });
        }
        let original_moved: f64 = transfers.iter().map(|t| t.abs()).sum();
        let mut passes = 0;
        let mut incoming = vec![0.0; self.fractions.len()];
        let numeric_scratch_bytes =
            (incoming.capacity() + outgoing.capacity() + transfers.capacity()) * size_of::<f64>();
        let next = loop {
            incoming.fill(0.0);
            outgoing.fill(0.0);
            for (f, &t) in self.faces().iter().zip(&transfers) {
                let (a, b) = if t >= 0.0 {
                    (f.lower, f.upper)
                } else {
                    (f.upper, f.lower)
                };
                outgoing[a] += t.abs();
                incoming[b] += t.abs();
            }
            if self.fractions.iter().enumerate().all(|(i, &amount)| {
                let v = (amount + incoming[i]) - outgoing[i];
                v.is_finite() && (0.0..=1.0).contains(&v)
            }) {
                for (i, amount) in incoming.iter_mut().enumerate() {
                    *amount = (self.fractions[i] + *amount) - outgoing[i];
                }
                break incoming;
            }
            if passes == 64 {
                return Err(PhaseError::LimiterExhausted);
            }
            // Reuse the two sum arrays as receiver/donor scales. Preserve the
            // old sums locally and retain exactly the previous arithmetic order.
            for i in 0..self.fractions.len() {
                let inc = incoming[i];
                let out = outgoing[i];
                let v = (self.fractions[i] + inc) - out;
                incoming[i] = 1.0;
                outgoing[i] = 1.0;
                if v < 0.0 {
                    outgoing[i] = ((self.fractions[i] + inc) / out).clamp(0.0, 1.0)
                        * (1.0 - 32.0 * f64::EPSILON);
                } else if v > 1.0 {
                    incoming[i] = ((1.0 - self.fractions[i] + out) / inc).clamp(0.0, 1.0)
                        * (1.0 - 32.0 * f64::EPSILON);
                }
            }
            for (f, t) in self.faces().iter().zip(&mut transfers) {
                let (a, b) = if *t >= 0.0 {
                    (f.lower, f.upper)
                } else {
                    (f.upper, f.lower)
                };
                *t *= outgoing[a].min(incoming[b]);
            }
            passes += 1;
        };
        let moved: f64 = transfers.iter().map(|t| t.abs()).sum();
        // Ordinary transport releases both buffers before basin validation.
        // Graph routing retains accepted transfers until its ledger is built.
        drop(outgoing);
        let transfers = if retain_transfers {
            Some(transfers)
        } else {
            drop(transfers);
            None
        };
        // Basin limit is part of atomic acceptance, including fragmentation.
        self.basins_for(&next)?;
        self.fractions = next.into();
        Ok((
            PhaseTransportMetrics {
                moved_water_m3: moved * volume,
                limited_water_m3: (original_moved - moved).max(0.0) * volume,
                limiter_passes: passes,
                numeric_scratch_bytes,
            },
            transfers,
        ))
    }
}

fn check_limit(kind: &'static str, requested: usize, limit: usize) -> Result<(), PhaseError> {
    if requested > limit {
        Err(PhaseError::Limit {
            kind,
            requested,
            limit,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_index_ceiling_does_not_wrap_or_alias_axes() {
        let lower = MAX_PACKED_FINE_CELLS - 2;
        for axis in 0..3 {
            let code = pack_face(lower, axis).unwrap();
            assert_eq!(decode_face(code, [1, 3, 9]).lower, lower);
            assert_eq!(decode_face(code, [1, 3, 9]).axis, axis);
        }
        assert!(pack_face(MAX_PACKED_FINE_CELLS, 0).is_err());
        assert!(pack_face(0, 3).is_err());
        assert!(
            check_limit(
                "packed fine cells",
                MAX_PACKED_FINE_CELLS + 1,
                MAX_PACKED_FINE_CELLS
            )
            .is_err()
        );
    }
}
