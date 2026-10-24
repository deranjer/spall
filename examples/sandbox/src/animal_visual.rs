//! Sandbox-owned extraction of animated SPVX voxels into bounded render boxes.
//!
//! This bridge does not create entities or world volumes. It expands each
//! source run once, retains part/material/tint identity, and composes a sampled
//! hierarchy only when a preview pose is requested.

use std::collections::{BTreeMap, BTreeSet};

use glam::{Quat, Vec3};
use spall_asset::{Asset, Part, PartPose, Transform, VoxelRun};
use spall_core::{MaterialId, MaterialManifest};
use thiserror::Error;

/// Hard stop even if a caller accidentally requests a larger preview budget.
pub const MAX_ANIMAL_VISUAL_CELLS: usize = 250_000;
const MAX_PARTS: usize = 1_024;

#[derive(Debug, Error)]
pub enum AnimalVisualError {
    #[error("animal visual asset is invalid: {0}")]
    Invalid(String),
    #[error("SPVX animation sampling failed: {0}")]
    Asset(#[from] spall_asset::AssetError),
}

type Result<T> = std::result::Result<T, AnimalVisualError>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimalCell {
    pub part_id: u32,
    /// Authored mapped material identity; tint is kept separately.
    pub material: MaterialId,
    pub material_slot: u16,
    pub tint_slot: u16,
    pub tint_srgb: Option<[u8; 3]>,
    /// Cell center in asset-pivot-relative metres.
    pub center: Vec3,
    /// Exact source cell edge length in metres on each oriented axis.
    pub size: Vec3,
    /// Composed part orientation in `[x,y,z,w]` convention.
    pub rotation: Quat,
}

#[derive(Debug, Clone, Copy)]
struct LocalCell {
    at: [i32; 3],
    material: MaterialId,
    material_slot: u16,
    tint_slot: u16,
    tint: Option<[u8; 3]>,
}

#[derive(Debug, Clone)]
struct PartGeometry {
    part: Part,
    cells: Vec<LocalCell>,
}

/// Immutable cached geometry and validated hierarchy for one decoded asset.
#[derive(Debug, Clone)]
pub struct AnimalVisual {
    asset: Asset,
    parts: Vec<PartGeometry>,
    cell_edge_m: f32,
    pivot_cells: Vec3,
    cell_count: usize,
}

/// Build the explicit symbolic mapping for a game material manifest. Asset
/// keys remain names on disk; the returned numeric IDs are local to this
/// manifest and are never written back into SPVX.
pub fn material_mapping(manifest: &MaterialManifest) -> BTreeMap<String, MaterialId> {
    manifest
        .entries()
        .iter()
        .map(|entry| (entry.name.clone(), entry.id))
        .collect()
}

impl AnimalVisual {
    /// Cache part-local cells after checking all run expansions and references.
    /// `max_cells` is caller policy and can only lower the fixed hard cap.
    pub fn new(
        asset: Asset,
        materials: &BTreeMap<String, MaterialId>,
        max_cells: usize,
    ) -> Result<Self> {
        if asset.parts.is_empty() || asset.parts.len() > MAX_PARTS {
            return invalid("part count is empty or exceeds 1024");
        }
        let cell_edge_m = match asset.cell_size_code {
            0 => 0.25,
            1 => 0.0625,
            _ => return invalid("unsupported cell-size code"),
        };
        let pivot_cells = Vec3::new(
            asset.pivot_subcells[0] as f32,
            asset.pivot_subcells[1] as f32,
            asset.pivot_subcells[2] as f32,
        ) / 256.0;

        let mut part_by_id = BTreeMap::new();
        for part in &asset.parts {
            if part.id == 0 || part_by_id.insert(part.id, part.clone()).is_some() {
                return invalid("part IDs must be nonzero and unique");
            }
            validate_transform(part.rest, "rest transform")?;
        }
        if part_by_id.get(&1).is_none_or(|root| root.parent_id != 0)
            || asset
                .parts
                .iter()
                .filter(|part| part.parent_id == 0)
                .count()
                != 1
        {
            return invalid("hierarchy must have the single part-1 root");
        }

        let part_ids: BTreeSet<u32> = part_by_id.keys().copied().collect();
        for clip in &asset.clips {
            for track in &clip.tracks {
                if !part_ids.contains(&track.part_id) {
                    return invalid("animation track references an unknown part");
                }
                let mut last_tick = None;
                for key in &track.keys {
                    if key.tick > clip.duration_ticks
                        || last_tick.is_some_and(|last| key.tick <= last)
                    {
                        return invalid("animation keys are unordered or outside clip duration");
                    }
                    last_tick = Some(key.tick);
                }
            }
        }

        // Parent-before-child topological order, with explicit cycle and
        // missing-parent rejection for callers that mutated the public DTO.
        let mut ordered = Vec::with_capacity(part_by_id.len());
        let mut emitted = BTreeSet::new();
        while ordered.len() < part_by_id.len() {
            let next = part_by_id
                .values()
                .find(|part| {
                    !emitted.contains(&part.id)
                        && (part.parent_id == 0 || emitted.contains(&part.parent_id))
                })
                .cloned();
            let Some(part) = next else {
                return invalid("part hierarchy has a missing parent or cycle");
            };
            if part.id != 1 && !part_ids.contains(&part.parent_id) {
                return invalid("part hierarchy references an unknown parent");
            }
            emitted.insert(part.id);
            ordered.push(part);
        }

        let budget = max_cells.min(MAX_ANIMAL_VISUAL_CELLS);
        let mut total = 0usize;
        for run in &asset.runs {
            validate_run(run, &part_ids, &asset, materials)?;
            let length = usize::try_from(run.length_x)
                .map_err(|_| AnimalVisualError::Invalid("run length overflows usize".into()))?;
            total = total
                .checked_add(length)
                .ok_or_else(|| AnimalVisualError::Invalid("expanded cell count overflow".into()))?;
            if total > budget {
                return invalid(format!(
                    "expanded asset has more than the {budget}-cell preview budget"
                ));
            }
        }

        let mut cells_by_part: BTreeMap<u32, Vec<LocalCell>> =
            ordered.iter().map(|part| (part.id, Vec::new())).collect();
        for run in &asset.runs {
            let mapped = materials[&asset.material_keys[usize::from(run.material_slot - 1)]];
            let tint = if run.tint_slot == 0 {
                None
            } else {
                Some(asset.tints_srgb[usize::from(run.tint_slot - 1)])
            };
            let cells = cells_by_part
                .get_mut(&run.part_id)
                .ok_or_else(|| AnimalVisualError::Invalid("run references unknown part".into()))?;
            for dx in 0..run.length_x {
                let x = i32::try_from(i64::from(run.start[0]) + i64::from(dx))
                    .map_err(|_| AnimalVisualError::Invalid("run endpoint exceeds i32".into()))?;
                cells.push(LocalCell {
                    at: [x, run.start[1], run.start[2]],
                    material: mapped,
                    material_slot: run.material_slot,
                    tint_slot: run.tint_slot,
                    tint,
                });
            }
        }
        let parts = ordered
            .into_iter()
            .map(|part| PartGeometry {
                cells: cells_by_part.remove(&part.id).unwrap_or_default(),
                part,
            })
            .collect();
        Ok(Self {
            asset,
            parts,
            cell_edge_m,
            pivot_cells,
            cell_count: total,
        })
    }

    pub fn name(&self) -> &str {
        &self.asset.name
    }

    pub fn cell_count(&self) -> usize {
        self.cell_count
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }

    pub fn clip_duration(&self, name: &str) -> Option<u32> {
        self.asset
            .clips
            .iter()
            .find(|clip| clip.name == name)
            .map(|clip| clip.duration_ticks)
    }

    /// Sample a named source clip then turn the pose into visible cell boxes.
    pub fn sample(&self, clip: &str, tick: f64) -> Result<Vec<AnimalCell>> {
        let poses = spall_asset::sample_clip(&self.asset, clip, tick)?;
        self.compose(&poses)
    }

    /// Sample a named source clip into one [`PartFrame`] per part, in
    /// [`Self::part_cells`] order: the pose without its cells.
    pub fn sample_part_frames(&self, clip: &str, tick: f64) -> Result<Vec<PartFrame>> {
        let poses = spall_asset::sample_clip(&self.asset, clip, tick)?;
        self.part_frames(&poses)
    }

    /// Each part's cells as offsets from the part's joint, metres, with their
    /// mapped material and tint; parent-before-child part order.
    pub fn part_cells(
        &self,
    ) -> impl Iterator<Item = impl Iterator<Item = (Vec3, MaterialId, Option<[u8; 3]>)>> {
        let edge = self.cell_edge_m;
        self.parts.iter().map(move |geometry| {
            geometry
                .cells
                .iter()
                .map(move |cell| (cell_offset(cell, edge), cell.material, cell.tint))
        })
    }

    /// Edge length of every cell, metres.
    pub fn cell_edge_m(&self) -> f32 {
        self.cell_edge_m
    }

    /// Every clip's name and duration in ticks.
    pub fn clips(&self) -> impl Iterator<Item = (&str, u32)> {
        self.asset
            .clips
            .iter()
            .map(|clip| (clip.name.as_str(), clip.duration_ticks))
    }

    /// Compose caller-supplied parent-relative poses. Exact part coverage is
    /// required so missing or stale pose references cannot silently default.
    pub fn compose(&self, poses: &BTreeMap<u32, PartPose>) -> Result<Vec<AnimalCell>> {
        let frames = self.part_frames(poses)?;
        let mut output = Vec::with_capacity(self.cell_count);
        for (geometry, frame) in self.parts.iter().zip(&frames) {
            if !frame.visible {
                continue;
            }
            for cell in &geometry.cells {
                let center =
                    frame.translation_m + frame.rotation * cell_offset(cell, self.cell_edge_m);
                if !center.is_finite() {
                    return invalid("transformed cell center is non-finite");
                }
                output.push(AnimalCell {
                    part_id: geometry.part.id,
                    material: cell.material,
                    material_slot: cell.material_slot,
                    tint_slot: cell.tint_slot,
                    tint_srgb: cell.tint,
                    center,
                    size: Vec3::splat(self.cell_edge_m),
                    rotation: frame.rotation,
                });
            }
        }
        Ok(output)
    }

    /// The hierarchy half of [`Self::compose`]: each part's composed placement.
    pub fn part_frames(&self, poses: &BTreeMap<u32, PartPose>) -> Result<Vec<PartFrame>> {
        if poses.len() != self.parts.len()
            || self
                .parts
                .iter()
                .any(|part| !poses.contains_key(&part.part.id))
        {
            return invalid("pose map must contain exactly one entry per part");
        }
        let mut world = BTreeMap::<u32, (Transform, bool)>::new();
        let mut frames = Vec::with_capacity(self.parts.len());
        for geometry in &self.parts {
            let part = &geometry.part;
            let pose = poses
                .get(&part.id)
                .ok_or_else(|| AnimalVisualError::Invalid("pose is missing a part".into()))?;
            validate_transform(pose.transform, "sampled pose")?;
            let (parent, parent_visible) = if part.parent_id == 0 {
                (Transform::IDENTITY, true)
            } else {
                *world.get(&part.parent_id).ok_or_else(|| {
                    AnimalVisualError::Invalid("pose hierarchy parent is missing".into())
                })?
            };
            let rotation = (parent.rotation * pose.transform.rotation).normalize();
            let translation = parent.translation + parent.rotation * pose.transform.translation;
            if !rotation.is_finite()
                || !translation.is_finite()
                || (rotation.length_squared() - 1.0).abs() > 1e-4
            {
                return invalid("composed transform is non-finite or non-unit");
            }
            let visible = parent_visible && pose.visible;
            world.insert(
                part.id,
                (
                    Transform {
                        translation,
                        rotation,
                        scale: Vec3::ONE,
                    },
                    visible,
                ),
            );
            frames.push(PartFrame {
                translation_m: (translation - self.pivot_cells) * self.cell_edge_m,
                rotation,
                visible,
            });
        }
        Ok(frames)
    }
}

/// One part's composed placement in a pose: a cell at offset `o` from
/// [`AnimalVisual::part_cells`] has its centre at `translation_m + rotation * o`
/// (asset-pivot-relative metres) and is oriented by `rotation`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartFrame {
    pub translation_m: Vec3,
    pub rotation: Quat,
    /// The part and all its ancestors are shown.
    pub visible: bool,
}

fn cell_offset(cell: &LocalCell, edge_m: f32) -> Vec3 {
    Vec3::new(
        cell.at[0] as f32 + 0.5,
        cell.at[1] as f32 + 0.5,
        cell.at[2] as f32 + 0.5,
    ) * edge_m
}

/// The server's view of every species' posed voxels (ENG-237): what a strike
/// hits and what a carcass is made of. Samples the same clip a client draws for
/// the record, at the record's clip phase, around its root pose.
pub fn creature_shape() -> std::result::Result<spall_sim::CreatureShape, crate::animals::AnimalError>
{
    let mapping = material_mapping(&crate::game::manifest());
    let visuals = crate::animals::species()?
        .into_iter()
        .map(|s| {
            AnimalVisual::new(s.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)
                .map(|visual| (s.key, visual))
                .map_err(|e| crate::animals::AnimalError::Asset(s.key, e.to_string()))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(spall_sim::CreatureShape::new(move |record| {
        let Some((key, visual)) = visuals.iter().find(|(key, _)| *key == record.species) else {
            return Vec::new();
        };
        let clip = crate::animals::locomotion_clip(key, record.locomotion_tag).unwrap_or("idle");
        let duration = f64::from(visual.clip_duration(clip).unwrap_or(1).max(1));
        let phase = (record.clip_phase_tick as f64).rem_euclid(duration);
        let Ok(cells) = visual.sample(clip, phase) else {
            return Vec::new();
        };
        let [x, y, z, w] = record.rotation_xyzw;
        let root_q = Quat::from_xyzw(x, y, z, w).normalize();
        let root = record.translation_m;
        cells
            .iter()
            .map(|cell| {
                let offset = root_q * cell.center;
                spall_sim::CreatureVoxel {
                    centre_m: [
                        root[0] + f64::from(offset.x),
                        root[1] + f64::from(offset.y),
                        root[2] + f64::from(offset.z),
                    ],
                    edge_m: cell.size.max_element(),
                    material: cell.material,
                }
            })
            .collect()
    }))
}

fn validate_run(
    run: &VoxelRun,
    part_ids: &BTreeSet<u32>,
    asset: &Asset,
    materials: &BTreeMap<String, MaterialId>,
) -> Result<()> {
    if run.length_x == 0 || !part_ids.contains(&run.part_id) {
        return invalid("voxel run has zero length or references an unknown part");
    }
    let end = i64::from(run.start[0]) + i64::from(run.length_x) - 1;
    if end > i64::from(i32::MAX) {
        return invalid("voxel run endpoint exceeds i32");
    }
    if run.material_slot == 0
        || usize::from(run.material_slot) > asset.material_keys.len()
        || run.tint_slot as usize > asset.tints_srgb.len()
    {
        return invalid("voxel run references an unknown material or tint slot");
    }
    let key = &asset.material_keys[usize::from(run.material_slot - 1)];
    let material = materials
        .get(key)
        .ok_or_else(|| AnimalVisualError::Invalid(format!("unmapped material key {key:?}")))?;
    if material.is_air() {
        return invalid("voxel run maps to air");
    }
    Ok(())
}

fn validate_transform(transform: Transform, label: &str) -> Result<()> {
    let q = transform.rotation;
    let norm = q.length_squared();
    if !transform.translation.is_finite()
        || !transform.scale.is_finite()
        || transform.scale != Vec3::ONE
        || !q.is_finite()
        || !norm.is_finite()
        || norm <= 0.0
        || (norm - 1.0).abs() > 1e-4
    {
        return invalid(format!(
            "{label} must have finite translation, a unit rotation, and unit scale"
        ));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(AnimalVisualError::Invalid(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<(&'static str, &'static [u8], &'static str)> {
        vec![
            (
                "Trout",
                include_bytes!("../assets/animals/trout.spvox"),
                "swim",
            ),
            (
                "Crow",
                include_bytes!("../assets/animals/crow.spvox"),
                "flap",
            ),
            (
                "Deer",
                include_bytes!("../assets/animals/deer.spvox"),
                "walk",
            ),
        ]
    }

    fn mapping() -> BTreeMap<String, MaterialId> {
        material_mapping(&crate::game::manifest())
    }

    fn visual(bytes: &[u8]) -> AnimalVisual {
        AnimalVisual::new(
            spall_asset::decode(bytes).unwrap(),
            &mapping(),
            MAX_ANIMAL_VISUAL_CELLS,
        )
        .unwrap()
    }

    #[test]
    fn fixtures_preserve_cells_materials_tints_pivots_and_parts() {
        for (name, bytes, _) in fixtures() {
            let asset = spall_asset::decode(bytes).unwrap();
            let authored_cells: usize = asset.runs.iter().map(|run| run.length_x as usize).sum();
            let expected_tinted: usize = asset
                .runs
                .iter()
                .filter(|run| run.tint_slot != 0)
                .map(|run| run.length_x as usize)
                .sum();
            let visual =
                AnimalVisual::new(asset.clone(), &mapping(), MAX_ANIMAL_VISUAL_CELLS).unwrap();
            assert_eq!(visual.name(), name);
            assert_eq!(visual.cell_count(), authored_cells);
            assert_eq!(visual.part_count(), asset.parts.len());
            let cells = visual.sample("idle", 0.0).unwrap();
            assert_eq!(cells.len(), authored_cells);
            assert_eq!(
                cells.iter().filter(|cell| cell.tint_srgb.is_some()).count(),
                expected_tinted
            );
            assert!(cells.iter().all(|cell| cell.size == Vec3::splat(0.0625)));
            assert!(cells.iter().all(|cell| !cell.material.is_air()));
        }
    }

    #[test]
    fn child_pivots_and_sampled_actions_change_oriented_geometry() {
        for (name, bytes, action) in fixtures() {
            let visual = visual(bytes);
            let rest = visual.sample(action, 0.0).unwrap();
            let moved = visual
                .sample(
                    action,
                    f64::from(visual.clip_duration(action).unwrap()) / 4.0,
                )
                .unwrap();
            assert_eq!(rest.len(), moved.len(), "{name}");
            assert!(
                rest.iter()
                    .zip(&moved)
                    .any(|(a, b)| a.center.distance(b.center) > 1e-5
                        || a.rotation.dot(b.rotation).abs() < 0.9999),
                "{name} action did not move geometry"
            );
        }
        let trout = visual(fixtures()[0].1);
        let cells = trout.sample("idle", 0.0).unwrap();
        let tail = cells.iter().find(|cell| cell.part_id == 2).unwrap();
        assert!(
            tail.center.x < -0.25,
            "tail part must remain behind root pivot: {:?}",
            tail.center
        );
    }

    #[test]
    fn inherited_visibility_hides_descendants() {
        let visual = visual(fixtures()[0].1);
        let mut poses = spall_asset::sample_clip(&visual.asset, "idle", 0.0).unwrap();
        poses.get_mut(&1).unwrap().visible = false;
        assert!(visual.compose(&poses).unwrap().is_empty());

        let visual = self::visual(fixtures()[2].1);
        let mut poses = spall_asset::sample_clip(&visual.asset, "idle", 0.0).unwrap();
        poses.get_mut(&2).unwrap().visible = false;
        let visible = visual.compose(&poses).unwrap();
        assert!(
            visible
                .iter()
                .all(|cell| ![2, 3, 4].contains(&cell.part_id))
        );
        assert!(visible.iter().any(|cell| cell.part_id == 1));
        assert!(visible.iter().any(|cell| cell.part_id == 5));
    }

    #[test]
    fn parent_rotation_child_joint_and_asset_pivot_compose_in_order() {
        let mut asset = spall_asset::decode(fixtures()[0].1).unwrap();
        asset.parts.truncate(2);
        asset.clips.clear();
        asset.pivot_subcells = [256, 0, 0];
        asset.parts[0].rest.translation = Vec3::new(2.0, 0.0, 0.0);
        asset.parts[0].rest.rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        asset.parts[1].rest.translation = Vec3::new(0.0, 2.0, 0.0);
        asset.parts[1].rest.rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        asset.runs = vec![VoxelRun {
            part_id: 2,
            start: [0, 0, 0],
            length_x: 1,
            material_slot: 1,
            tint_slot: 1,
        }];
        let poses = asset
            .parts
            .iter()
            .map(|part| {
                (
                    part.id,
                    PartPose {
                        transform: part.rest,
                        visible: true,
                    },
                )
            })
            .collect();
        let first_tint = asset.tints_srgb[0];
        let visual = AnimalVisual::new(asset, &mapping(), 1).unwrap();
        let cells = visual.compose(&poses).unwrap();
        assert_eq!(cells.len(), 1);
        // Ry(90) maps the cell centre to (.5,.5,-.5); the child
        // joint then Rz(90), root translation and asset pivot follow.
        assert!(
            cells[0]
                .center
                .distance(Vec3::new(-0.09375, 0.03125, -0.03125))
                < 1e-6
        );
        assert_eq!(cells[0].tint_srgb, Some(first_tint));
    }

    #[test]
    fn mutated_public_asset_references_and_transforms_fail_safely() {
        let mut asset = spall_asset::decode(fixtures()[0].1).unwrap();
        asset.runs[0].part_id = u32::MAX;
        assert!(AnimalVisual::new(asset, &mapping(), MAX_ANIMAL_VISUAL_CELLS).is_err());

        let mut asset = spall_asset::decode(fixtures()[0].1).unwrap();
        asset.parts[0].rest.scale.x = 2.0;
        assert!(AnimalVisual::new(asset, &mapping(), MAX_ANIMAL_VISUAL_CELLS).is_err());

        let mut asset = spall_asset::decode(fixtures()[0].1).unwrap();
        asset.runs[0].material_slot = u16::MAX;
        assert!(AnimalVisual::new(asset, &mapping(), MAX_ANIMAL_VISUAL_CELLS).is_err());
    }

    #[test]
    fn missing_mappings_and_expansion_over_budget_reject_before_expansion() {
        let asset = spall_asset::decode(fixtures()[0].1).unwrap();
        assert!(
            AnimalVisual::new(asset.clone(), &BTreeMap::new(), MAX_ANIMAL_VISUAL_CELLS).is_err()
        );
        assert!(AnimalVisual::new(asset, &mapping(), 1).is_err());
    }
}
