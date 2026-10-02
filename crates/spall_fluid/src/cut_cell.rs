//! Experimental geometry for ENG-122. Not installed in the production solver.
//!
//! A coarse cell can contain several disconnected open spaces. Keeping one
//! capacity or aperture per cell would connect those spaces through a thin wall.
//! This snapshot retains each six-connected space and matches open fine-voxel
//! faces across coarse-cell boundaries. All indices belong to this snapshot;
//! they are never persistent IDs or a substitute for revision validation.

use std::collections::{BTreeMap, VecDeque};

use spall_core::GlobalCell;

use crate::{DomainError, DomainSpec, SolidBoundary};

const SOLID: u32 = u32::MAX;

/// Explicit build limits; pathological geometry may approach fine-grid cost.
#[derive(Debug, Clone, Copy)]
pub struct GeometryLimits {
    pub max_fine_cells: usize,
    pub max_components: usize,
    pub max_portals: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeometryError {
    #[error("invalid cut-cell factor {0}; expected 1..=8")]
    InvalidFactor(u32),
    #[error("cut-cell domain: {0}")]
    Domain(#[from] DomainError),
    #[error("cut-cell {kind} count {requested} exceeds limit {limit}")]
    Limit {
        kind: &'static str,
        requested: usize,
        limit: usize,
    },
    #[error("water amounts must match the fine domain, be finite in 0..=1, and not overlap solid")]
    InvalidAmounts,
}

/// One six-connected space inside one coarse cell. Integer capacity and layer
/// counts preserve fine geometry; no solid volume is treated as fluid air.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenComponent {
    pub coarse_index: usize,
    /// Lowest X/Y/Z-linear fine cell, a deterministic geometry anchor.
    pub anchor: GlobalCell,
    pub voxel_count: u32,
    /// Counts at each local fine-voxel Y layer; unused layers are zero.
    pub layer_counts: [u16; 8],
    centre_sum_twice: [u32; 3],
}

impl OpenComponent {
    /// Centroid in fine-voxel units relative to the coarse cell's lower corner.
    pub fn centroid(&self) -> [f64; 3] {
        self.centre_sum_twice
            .map(|sum| f64::from(sum) / (2.0 * f64::from(self.voxel_count)))
    }

    /// Exact geometric volume below a horizontal surface, in fine-voxel
    /// volumes. `local_height` is relative to the coarse cell's lower corner.
    pub fn volume_below(&self, local_height: f64) -> f64 {
        self.layer_counts
            .iter()
            .enumerate()
            .map(|(y, count)| f64::from(*count) * (local_height - y as f64).clamp(0.0, 1.0))
            .sum()
    }
}

/// An aggregate of genuinely open, matched fine faces between two components
/// in adjacent coarse cells. Domain sides have no portal; open-top handling is
/// a future solver policy, not silently added to the geometry graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPortal {
    pub lower_component: u32,
    pub upper_component: u32,
    pub axis: u8,
    pub voxel_faces: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutCellGeometry {
    fine_spec: DomainSpec,
    factor: u32,
    coarse_dimensions: [u32; 3],
    components: Vec<OpenComponent>,
    portals: Vec<OpenPortal>,
    fine_component: Vec<u32>,
}

impl CutCellGeometry {
    pub fn fine_spec(&self) -> DomainSpec {
        self.fine_spec
    }

    /// Component centroid in domain-local fine-voxel units. Keeping the
    /// origin separate avoids losing precision at large global coordinates.
    pub fn component_centroid(&self, index: usize) -> [f64; 3] {
        let component = &self.components[index];
        let [nx, ny, _] = self.coarse_dimensions.map(|v| v as usize);
        let cell = component.coarse_index;
        let base = [cell % nx, cell / nx % ny, cell / (nx * ny)];
        let centre = component.centroid();
        std::array::from_fn(|axis| base[axis] as f64 * f64::from(self.factor) + centre[axis])
    }

    /// Open fine-voxel faces on the upper Y domain side for each component.
    /// Other domain sides remain closed unless a caller defines another policy.
    pub fn open_top_faces(&self) -> Vec<u32> {
        let [nx, ny, nz] = self.fine_spec.dimensions().map(|v| v as usize);
        let mut counts = vec![0; self.components.len()];
        for z in 0..nz {
            for x in 0..nx {
                let id = self.fine_component[x + nx * (ny - 1 + ny * z)];
                if id != SOLID {
                    counts[id as usize] += 1;
                }
            }
        }
        counts
    }
    pub fn build(
        boundary: &SolidBoundary,
        factor: u32,
        limits: GeometryLimits,
    ) -> Result<Self, GeometryError> {
        if !(1..=8).contains(&factor) {
            return Err(GeometryError::InvalidFactor(factor));
        }
        let spec = boundary.spec();
        check_limit("fine cells", spec.cell_count(), limits.max_fine_cells)?;
        let dims = spec.dimensions();
        if dims.iter().any(|n| n % factor != 0) {
            return Err(DomainError::UnalignedCoarsening { factor }.into());
        }
        let coarse_dimensions = dims.map(|n| n / factor);
        let [nx, ny, nz] = dims.map(|n| n as usize);
        let f = factor as usize;
        let coarse_count = spec.cell_count() / (f * f * f);
        let mut fine_component = vec![SOLID; spec.cell_count()];
        let mut components = Vec::new();
        let mut queue = VecDeque::new();
        for coarse_index in 0..coarse_count {
            let cnx = coarse_dimensions[0] as usize;
            let cny = coarse_dimensions[1] as usize;
            let base = [
                coarse_index % cnx * f,
                coarse_index / cnx % cny * f,
                coarse_index / (cnx * cny) * f,
            ];
            for lz in 0..f {
                for ly in 0..f {
                    for lx in 0..f {
                        let start = base[0] + lx + nx * (base[1] + ly + ny * (base[2] + lz));
                        if boundary.solid[start] || fine_component[start] != SOLID {
                            continue;
                        }
                        check_limit("components", components.len() + 1, limits.max_components)?;
                        let id = u32::try_from(components.len())
                            .ok()
                            .filter(|id| *id != SOLID)
                            .ok_or(DomainError::CellCountOverflow)?;
                        let mut component = OpenComponent {
                            coarse_index,
                            anchor: spec.cell_at(start),
                            voxel_count: 0,
                            layer_counts: [0; 8],
                            centre_sum_twice: [0; 3],
                        };
                        fine_component[start] = id;
                        queue.push_back(start);
                        while let Some(i) = queue.pop_front() {
                            let local = [
                                i % nx - base[0],
                                i / nx % ny - base[1],
                                i / (nx * ny) - base[2],
                            ];
                            component.voxel_count += 1;
                            component.layer_counts[local[1]] += 1;
                            for (sum, position) in component.centre_sum_twice.iter_mut().zip(local)
                            {
                                *sum += (2 * position + 1) as u32;
                            }
                            for (axis, stride) in [1, nx, nx * ny].into_iter().enumerate() {
                                for next in [
                                    (local[axis] > 0).then(|| i - stride),
                                    (local[axis] + 1 < f).then(|| i + stride),
                                ]
                                .into_iter()
                                .flatten()
                                {
                                    if !boundary.solid[next] && fine_component[next] == SOLID {
                                        fine_component[next] = id;
                                        queue.push_back(next);
                                    }
                                }
                            }
                        }
                        components.push(component);
                    }
                }
            }
        }
        let mut faces = BTreeMap::<(u32, u32, u8), u32>::new();
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = x + nx * (y + ny * z);
                    let a = fine_component[i];
                    if a == SOLID {
                        continue;
                    }
                    for (axis, stride) in [1, nx, nx * ny].into_iter().enumerate() {
                        let coord = [x, y, z][axis];
                        if coord + 1 >= [nx, ny, nz][axis] || (coord + 1) % f != 0 {
                            continue;
                        }
                        let b = fine_component[i + stride];
                        if b == SOLID {
                            continue;
                        }
                        let key = (a, b, axis as u8);
                        if !faces.contains_key(&key) {
                            check_limit("portals", faces.len() + 1, limits.max_portals)?;
                        }
                        *faces.entry(key).or_default() += 1;
                    }
                }
            }
        }
        let portals = faces
            .into_iter()
            .map(|((a, b, axis), voxel_faces)| OpenPortal {
                lower_component: a,
                upper_component: b,
                axis,
                voxel_faces,
            })
            .collect();
        Ok(Self {
            fine_spec: spec,
            factor,
            coarse_dimensions,
            components,
            portals,
            fine_component,
        })
    }

    pub fn components(&self) -> &[OpenComponent] {
        &self.components
    }
    pub fn portals(&self) -> &[OpenPortal] {
        &self.portals
    }
    pub fn coarse_dimensions(&self) -> [u32; 3] {
        self.coarse_dimensions
    }
    pub fn factor(&self) -> u32 {
        self.factor
    }

    pub fn component_at(&self, fine_cell: GlobalCell) -> Option<u32> {
        let i = self.fine_spec.index_of(fine_cell)?;
        (self.fine_component[i] != SOLID).then_some(self.fine_component[i])
    }

    /// Aggregate exact fine-voxel amounts into independent component volumes.
    /// Units are fine-voxel volumes; multiply by voxel_size_m^3 for cubic metres.
    /// Solidity and capacity are retained; nothing is discarded or normalized
    /// to whole-cell fluid air. Input must describe this snapshot's fine domain.
    pub fn aggregate_amounts(&self, fractions: &[f64]) -> Result<Vec<f64>, GeometryError> {
        if fractions.len() != self.fine_component.len() {
            return Err(GeometryError::InvalidAmounts);
        }
        let mut amounts = vec![0.0; self.components.len()];
        for (&fraction, &component) in fractions.iter().zip(&self.fine_component) {
            if !fraction.is_finite()
                || !(0.0..=1.0).contains(&fraction)
                || (component == SOLID && fraction != 0.0)
            {
                return Err(GeometryError::InvalidAmounts);
            }
            if component != SOLID {
                amounts[component as usize] += fraction;
            }
        }
        Ok(amounts)
    }
}

fn check_limit(kind: &'static str, requested: usize, limit: usize) -> Result<(), GeometryError> {
    if requested > limit {
        Err(GeometryError::Limit {
            kind,
            requested,
            limit,
        })
    } else {
        Ok(())
    }
}
