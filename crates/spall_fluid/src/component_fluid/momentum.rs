//! Experimental staggered mass/momentum transport. Aperture-weighted component
//! mass is partitioned among incident normal-velocity DOFs and zero-normal wall
//! DOFs. This is a first-order dual-volume approximation, not an accuracy gate.
use super::{ComponentError, ComponentFluid, MomentumMetrics};
use crate::cut_cell::CutCellGeometry;

type Weights = Vec<[Vec<(usize, f64)>; 3]>;
type Advected = (Vec<f64>, Vec<f64>, MomentumMetrics);

pub(super) fn build_weights(g: &CutCellGeometry, open_top: bool) -> Weights {
    let n = g.components().len();
    let edges = g.portals().len();
    let mut faces: Vec<[Vec<(usize, u32)>; 3]> = (0..n)
        .map(|_| std::array::from_fn(|_| Vec::new()))
        .collect();
    for (index, e) in g.portals().iter().enumerate() {
        for c in [e.lower_component, e.upper_component] {
            faces[c as usize][e.axis as usize].push((index, e.voxel_faces));
        }
    }
    let mut walls = g.wall_faces();
    if open_top {
        for (i, top) in g.open_top_faces().into_iter().enumerate() {
            if top > 0 {
                faces[i][1].push((edges + i, top));
                walls[i][1] -= top;
            }
        }
    }
    faces
        .into_iter()
        .enumerate()
        .map(|(i, axes)| {
            std::array::from_fn(|axis| {
                let mut counts = axes[axis].clone();
                if walls[i][axis] > 0 {
                    counts.push((edges + n + 3 * i + axis, walls[i][axis]));
                }
                let total = counts
                    .iter()
                    .map(|(_, count)| f64::from(*count))
                    .sum::<f64>();
                let mut sum = 0.0;
                let len = counts.len();
                counts
                    .into_iter()
                    .enumerate()
                    .map(|(j, (face, count))| {
                        let weight = if j + 1 == len {
                            1.0 - sum
                        } else {
                            f64::from(count) / total
                        };
                        sum += weight;
                        (face, weight)
                    })
                    .collect()
            })
        })
        .collect()
}

impl ComponentFluid {
    pub(super) fn dual_mass(&self, amounts: &[f64]) -> Vec<f64> {
        let mut mass = vec![0.0; self.flux.len() + 4 * self.amounts.len()];
        for (i, axes) in self.momentum_weights.iter().enumerate() {
            let cell_mass = self.config.air_density * self.capacity[i]
                + (self.config.water_density - self.config.air_density) * amounts[i];
            for weights in axes {
                for &(face, w) in weights {
                    mass[face] += w * cell_mass;
                }
            }
        }
        mass
    }

    fn dual_velocity(&self) -> Vec<f64> {
        let mut velocity = vec![0.0; self.flux.len() + 4 * self.amounts.len()];
        for (i, e) in self.geometry.portals().iter().enumerate() {
            velocity[i] =
                self.flux[i] / (f64::from(e.voxel_faces) * self.config.voxel_size_m.powi(2));
        }
        for (i, &area) in self.top_area.iter().enumerate() {
            if area > 0.0 {
                velocity[self.flux.len() + i] = self.top_flux[i] / area;
            }
        }
        velocity
    }

    fn face_axis(&self, face: usize) -> usize {
        if face < self.flux.len() {
            self.geometry.portals()[face].axis as usize
        } else if face < self.flux.len() + self.amounts.len() {
            1
        } else {
            (face - self.flux.len() - self.amounts.len()) % 3
        }
    }

    pub fn momentum_kg_m_s(&self) -> [f64; 3] {
        let mass = self.dual_mass(&self.amounts);
        let velocity = self.dual_velocity();
        let mut total = [0.0; 3];
        for i in 0..mass.len() {
            total[self.face_axis(i)] += mass[i] * velocity[i];
        }
        total
    }

    pub fn kinetic_energy_j(&self) -> f64 {
        self.dual_mass(&self.amounts)
            .iter()
            .zip(self.dual_velocity())
            .map(|(m, u)| 0.5 * m * u * u)
            .sum()
    }

    /// Vertical potential under a bottom-filled horizontal reconstruction in
    /// each component. This diagnostic is not a general PLIC interface model.
    pub fn vertical_potential_energy_j(&self) -> f64 {
        let size = self.config.voxel_size_m;
        let voxel_volume = size.powi(3);
        let [nx, ny, _] = self.geometry.coarse_dimensions().map(|n| n as usize);
        let factor = self.geometry.factor() as usize;
        let mut moment = 0.0;
        for (i, c) in self.geometry.components().iter().enumerate() {
            let center_y = self.geometry.component_centroid(i)[1] * size;
            moment += self.config.air_density * self.capacity[i] * center_y;
            let base = c.coarse_index / nx % ny * factor;
            let mut remaining = self.amounts[i] / voxel_volume;
            for (y, &count) in c.layer_counts[..factor].iter().enumerate() {
                if count == 0 {
                    continue;
                }
                let filled = remaining.min(f64::from(count)).max(0.0);
                let thickness = filled / f64::from(count);
                moment += (self.config.water_density - self.config.air_density)
                    * filled
                    * voxel_volume
                    * (base as f64 + y as f64 + 0.5 * thickness)
                    * size;
                remaining -= filled;
            }
        }
        -self.config.gravity[1] * moment
    }

    pub(super) fn advect_momentum(
        &self,
        dt: f64,
        water: &[f64],
        exports: &[f64],
        next: &[f64],
    ) -> Result<Advected, ComponentError> {
        let old_mass = self.dual_mass(&self.amounts);
        let mass = self.dual_mass(next);
        let velocity = self.dual_velocity();
        let mut momentum: Vec<_> = old_mass.iter().zip(&velocity).map(|(m, u)| m * u).collect();
        let mut transported_mass = old_mass.clone();
        let mut outgoing = vec![0.0; mass.len()];
        let mut metrics = MomentumMetrics::default();
        for i in 0..mass.len() {
            metrics.before_kg_m_s[self.face_axis(i)] += momentum[i];
            metrics.kinetic_before_j += 0.5 * old_mass[i] * velocity[i].powi(2);
        }
        let averages: Vec<[f64; 3]> = self
            .momentum_weights
            .iter()
            .map(|axes| {
                std::array::from_fn(|axis| {
                    axes[axis].iter().map(|&(face, w)| w * velocity[face]).sum()
                })
            })
            .collect();
        let delta_density = self.config.water_density - self.config.air_density;
        for (i, e) in self.geometry.portals().iter().enumerate() {
            let signed = self.config.air_density * dt * self.flux[i] + delta_density * water[i];
            let (donor, receiver) = if signed >= 0.0 {
                (e.lower_component as usize, e.upper_component as usize)
            } else {
                (e.upper_component as usize, e.lower_component as usize)
            };
            let transferred = signed.abs();
            for (axis, &average) in averages[donor].iter().enumerate() {
                for &(face, w) in &self.momentum_weights[donor][axis] {
                    let part = transferred * w;
                    momentum[face] -= part * velocity[face];
                    transported_mass[face] -= part;
                    outgoing[face] += part;
                }
                for &(face, w) in &self.momentum_weights[receiver][axis] {
                    let part = transferred * w;
                    momentum[face] += part * average;
                    transported_mass[face] += part;
                }
            }
        }
        for (i, &export) in exports.iter().enumerate() {
            let signed = self.config.air_density * dt * self.top_flux[i] + delta_density * export;
            let outside = [
                0.0,
                if self.top_area[i] > 0.0 {
                    self.top_flux[i] / self.top_area[i]
                } else {
                    0.0
                },
                0.0,
            ];
            for (axis, &outside_velocity) in outside.iter().enumerate() {
                if signed >= 0.0 {
                    metrics.boundary_outflow_kg_m_s[axis] += signed * averages[i][axis];
                    for &(face, w) in &self.momentum_weights[i][axis] {
                        let part = signed * w;
                        momentum[face] -= part * velocity[face];
                        transported_mass[face] -= part;
                        outgoing[face] += part;
                    }
                } else {
                    metrics.boundary_outflow_kg_m_s[axis] += signed * outside_velocity;
                    for &(face, w) in &self.momentum_weights[i][axis] {
                        momentum[face] -= signed * w * outside_velocity;
                        transported_mass[face] -= signed * w;
                    }
                }
            }
        }
        let mut flux = self.flux.clone();
        let mut top = self.top_flux.clone();
        let wall_start = flux.len() + top.len();
        for i in 0..mass.len() {
            if !mass[i].is_finite() || !momentum[i].is_finite() || !transported_mass[i].is_finite()
            {
                return Err(ComponentError::InvalidState);
            }
            metrics.max_dual_mass_error_kg = metrics
                .max_dual_mass_error_kg
                .max((mass[i] - transported_mass[i]).abs());
            if old_mass[i] > 0.0 {
                let cfl = outgoing[i] / old_mass[i];
                if !cfl.is_finite() || cfl > self.config.cfl * (1.0 + 32.0 * f64::EPSILON) {
                    return Err(ComponentError::Cfl {
                        measured: cfl,
                        limit: self.config.cfl,
                    });
                }
            }
            let axis = self.face_axis(i);
            if i >= wall_start {
                metrics.wall_impulse_kg_m_s[axis] += momentum[i];
                continue;
            }
            if mass[i] == 0.0 {
                continue;
            }
            let u = momentum[i] / mass[i];
            if !u.is_finite() {
                return Err(ComponentError::InvalidState);
            }
            metrics.after_kg_m_s[axis] += momentum[i];
            metrics.kinetic_after_j += 0.5 * mass[i] * u * u;
            if i < flux.len() {
                flux[i] = u
                    * f64::from(self.geometry.portals()[i].voxel_faces)
                    * self.config.voxel_size_m.powi(2);
            } else {
                top[i - flux.len()] = u * self.top_area[i - flux.len()];
            }
        }
        metrics.balance_error_kg_m_s = std::array::from_fn(|axis| {
            metrics.after_kg_m_s[axis]
                + metrics.wall_impulse_kg_m_s[axis]
                + metrics.boundary_outflow_kg_m_s[axis]
                - metrics.before_kg_m_s[axis]
        });
        if !metrics.kinetic_before_j.is_finite()
            || !metrics.kinetic_after_j.is_finite()
            || flux.iter().chain(&top).any(|q| !q.is_finite())
        {
            return Err(ComponentError::InvalidState);
        }
        Ok((flux, top, metrics))
    }
}
