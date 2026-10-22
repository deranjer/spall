//! Client-side drawing of replicated animals: places each replicated record's
//! authored SPVX pose at its presented root and emits lit cubes.
//!
//! Every clip is posed once per whole tick at startup (animation runs at
//! 60 ticks/s, so the client never needs finer phases): a table of part
//! placements, not cells, so it stays small. A frame then culls creatures the
//! camera cannot see and, for the rest, does one matrix-vector product per
//! cell.

use std::time::Instant;

use glam::{Mat3, Quat, Vec3};
use spall_client::replica_creatures::ReplicaCreatures;
use spall_client::window::{CreatureCull, CreatureView};
use spall_render::CubeInstance;

use crate::animal_visual::{AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, PartFrame, material_mapping};
use crate::animals::{AnimalError, LOCOMOTION_IDLE, locomotion_clip, species};

#[derive(Debug)]
pub struct AnimalView {
    species: Vec<SpeciesDraw>,
    started: Instant,
}

/// One clip posed at every whole tick: `frames[tick * parts + part]`.
#[derive(Debug)]
struct ClipPoses {
    name: String,
    duration: usize,
    frames: Vec<PartFrame>,
    /// Ticks the asset could not be posed at; drawn as nothing, as before.
    valid: Vec<bool>,
}

#[derive(Debug)]
struct SpeciesDraw {
    key: &'static str,
    clips: Vec<ClipPoses>,
    /// Every cell's offset from its part's joint (metres) and material, in
    /// part order; part `i` owns `cells[part_ends[i - 1]..part_ends[i]]`.
    cells: Vec<(Vec3, u32)>,
    part_ends: Vec<usize>,
    edge_m: f32,
    /// Farthest any cell reaches from the root, over every posed tick, metres.
    radius_m: f32,
}

impl SpeciesDraw {
    fn new(key: &'static str, visual: &AnimalVisual) -> Self {
        let edge_m = visual.cell_edge_m();
        let mut cells = Vec::with_capacity(visual.cell_count());
        let mut part_ends = Vec::with_capacity(visual.part_count());
        let mut part_reach = Vec::with_capacity(visual.part_count());
        for part in visual.part_cells() {
            let mut reach = 0.0_f32;
            for (offset, material) in part {
                reach = reach.max(offset.length());
                cells.push((offset, u32::from(material.0)));
            }
            part_ends.push(cells.len());
            part_reach.push(reach);
        }
        // A cell's corner is half a diagonal past its centre.
        let corner = edge_m * 3.0_f32.sqrt() * 0.5;
        let mut radius_m = 0.0_f32;
        let parts = part_ends.len();
        let clips = visual
            .clips()
            .map(|(name, duration)| {
                let duration = duration.max(1) as usize;
                let mut frames = Vec::with_capacity(duration * parts);
                let mut valid = Vec::with_capacity(duration);
                for tick in 0..duration {
                    match visual.sample_part_frames(name, tick as f64) {
                        Ok(posed) if posed.len() == parts => {
                            for (frame, reach) in posed.iter().zip(&part_reach) {
                                if frame.visible {
                                    radius_m =
                                        radius_m.max(frame.translation_m.length() + reach + corner);
                                }
                            }
                            frames.extend(posed);
                            valid.push(true);
                        }
                        _ => {
                            frames.extend(std::iter::repeat_n(
                                PartFrame {
                                    translation_m: Vec3::ZERO,
                                    rotation: Quat::IDENTITY,
                                    visible: false,
                                },
                                parts,
                            ));
                            valid.push(false);
                        }
                    }
                }
                ClipPoses {
                    name: name.to_owned(),
                    duration,
                    frames,
                    valid,
                }
            })
            .collect();
        Self {
            key,
            clips,
            cells,
            part_ends,
            edge_m,
            radius_m,
        }
    }

    /// Part placements of `clip` at `phase` ticks, wrapped into the clip.
    fn pose(&self, clip: &str, phase: f64) -> Option<&[PartFrame]> {
        let clip = self.clips.iter().find(|c| c.name == clip)?;
        let tick = (phase.rem_euclid(clip.duration as f64).floor() as usize) % clip.duration;
        let parts = self.part_ends.len();
        clip.valid[tick].then(|| &clip.frames[tick * parts..(tick + 1) * parts])
    }

    fn emit(&self, pose: &[PartFrame], root_q: Quat, root_t: Vec3, out: &mut Vec<CubeInstance>) {
        let size = [self.edge_m; 3];
        let mut start = 0;
        for (frame, &end) in pose.iter().zip(&self.part_ends) {
            let range = start..end;
            start = end;
            if !frame.visible {
                continue;
            }
            let rotation = (root_q * frame.rotation).normalize();
            let basis = Mat3::from_quat(rotation);
            let origin = root_t + root_q * frame.translation_m;
            let rotation = rotation.to_array();
            out.extend(self.cells[range].iter().map(|&(offset, material)| {
                CubeInstance::new(
                    (origin + basis * offset).to_array(),
                    material,
                    size,
                    rotation,
                )
            }));
        }
    }
}

/// Whether any of a creature whose cells lie within `radius_m` of `centre_m`
/// can be in view.
fn in_view(cull: &CreatureCull, centre_m: [f64; 3], radius_m: f32) -> bool {
    let radius = f64::from(radius_m);
    let horizontal = (centre_m[0] - cull.eye_m[0]).hypot(centre_m[2] - cull.eye_m[2]);
    if horizontal > f64::from(cull.max_distance_m) + radius {
        return false;
    }
    let to = Vec3::new(
        (centre_m[0] - cull.apex_m[0]) as f32,
        (centre_m[1] - cull.apex_m[1]) as f32,
        (centre_m[2] - cull.apex_m[2]) as f32,
    );
    let distance = to.length();
    if distance <= radius_m {
        return true;
    }
    let cos = (to.dot(Vec3::from_array(cull.forward)) / distance).clamp(-1.0, 1.0);
    cos.acos() <= cull.half_angle_rad + (radius_m / distance).asin()
}

impl AnimalView {
    pub fn new() -> Result<Self, AnimalError> {
        let mapping = material_mapping(&crate::game::manifest());
        let species = species()?
            .into_iter()
            .map(|s| {
                let key = s.key;
                AnimalVisual::new(s.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)
                    .map(|visual| SpeciesDraw::new(key, &visual))
                    .map_err(|e| AnimalError::Asset(key, e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            species,
            started: Instant::now(),
        })
    }
}

impl CreatureView for AnimalView {
    fn instances(
        &self,
        creatures: &ReplicaCreatures,
        _tick: f64,
        now: Instant,
        cull: Option<&CreatureCull>,
    ) -> Vec<CubeInstance> {
        // Pose first, then write every cube into one allocation.
        let mut posed = Vec::new();
        for record in creatures.records() {
            let Some(draw) = self.species.iter().find(|s| s.key == record.species) else {
                continue;
            };
            let presented = creatures.presentation(record, now);
            if cull.is_some_and(|cull| !in_view(cull, presented.translation_m, draw.radius_m)) {
                continue;
            }
            let clip = locomotion_clip(draw.key, presented.locomotion_tag).unwrap_or("idle");
            // Resting animals idle on a local clock, offset per creature so a herd
            // does not breathe in step.
            let phase = if presented.locomotion_tag == LOCOMOTION_IDLE {
                now.saturating_duration_since(self.started).as_secs_f64() * 60.0
                    + (record.creature_id % 17) as f64 * 11.0
            } else {
                presented.clip_phase_tick
            };
            let Some(pose) = draw.pose(clip, phase) else {
                continue;
            };
            let [x, y, z, w] = presented.rotation_xyzw;
            let root_q = Quat::from_xyzw(x, y, z, w).normalize();
            let root_t = Vec3::new(
                presented.translation_m[0] as f32,
                presented.translation_m[1] as f32,
                presented.translation_m[2] as f32,
            );
            posed.push((draw, pose, root_q, root_t));
        }
        let mut out = Vec::with_capacity(
            posed
                .iter()
                .map(|(draw, ..)| draw.cells.len())
                .sum::<usize>(),
        );
        for (draw, pose, root_q, root_t) in posed {
            draw.emit(pose, root_q, root_t, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visuals() -> Vec<(&'static str, AnimalVisual)> {
        let mapping = material_mapping(&crate::game::manifest());
        species()
            .unwrap()
            .into_iter()
            .map(|s| {
                let visual = AnimalVisual::new(s.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS);
                (s.key, visual.unwrap())
            })
            .collect()
    }

    #[test]
    fn tabled_poses_draw_the_same_cubes_as_sampling_the_clip() {
        let root_q = Quat::from_rotation_y(0.7);
        let root_t = Vec3::new(10.0, 2.0, -5.0);
        let mut table_bytes = 0;
        for (key, visual) in visuals() {
            let draw = SpeciesDraw::new(key, &visual);
            table_bytes += draw.clips.iter().map(|c| c.frames.len()).sum::<usize>()
                * std::mem::size_of::<PartFrame>();
            for (clip, duration) in visual.clips() {
                for tick in [0, duration / 3, duration.saturating_sub(1)] {
                    let sampled = visual.sample(clip, f64::from(tick)).unwrap();
                    let mut drawn = Vec::new();
                    // A fractional phase within the tick draws that tick.
                    let pose = draw.pose(clip, f64::from(tick) + 0.4).unwrap();
                    draw.emit(pose, root_q, root_t, &mut drawn);
                    assert_eq!(drawn.len(), sampled.len(), "{key} {clip} {tick}");
                    for (cube, cell) in drawn.iter().zip(&sampled) {
                        let centre = root_t + root_q * cell.center;
                        let rotation = root_q * cell.rotation;
                        assert!(
                            Vec3::from_array(cube.offset).distance(centre) < 1e-4
                                && cube.material == u32::from(cell.material.0)
                                && cube.size == cell.size.to_array()
                                && Quat::from_array(cube.rotation).dot(rotation).abs() > 0.99999,
                            "{key} {clip} {tick}: {cube:?} vs {cell:?}"
                        );
                    }
                    assert!(
                        sampled
                            .iter()
                            .all(|cell| cell.center.length() <= draw.radius_m),
                        "{key} {clip} {tick}: a cell lies outside the cull radius"
                    );
                }
            }
        }
        eprintln!("pose tables: {table_bytes} bytes for every species");
        assert!(table_bytes < 4 << 20, "{table_bytes} bytes");
    }

    #[test]
    fn creatures_behind_or_beyond_the_view_are_culled_but_edges_are_kept() {
        let cull = CreatureCull {
            apex_m: [0.0, 0.0, 3.0],
            forward: [0.0, 0.0, -1.0],
            half_angle_rad: 1.0,
            eye_m: [0.0, 0.0, 0.0],
            max_distance_m: 80.0,
        };
        assert!(in_view(&cull, [0.0, 0.0, -20.0], 1.0), "straight ahead");
        assert!(!in_view(&cull, [0.0, 0.0, 20.0], 1.0), "behind");
        assert!(
            !in_view(&cull, [0.0, 0.0, -90.0], 1.0),
            "past the view radius"
        );
        assert!(in_view(&cull, [0.0, 0.0, 1.0], 1.0), "at the eye");
        // Centre just outside the cone's edge, with the body reaching into it.
        let angle = 1.0_f64 + 0.04;
        let at = [20.0 * angle.sin(), 0.0, 3.0 - 20.0 * angle.cos()];
        assert!(in_view(&cull, at, 1.0), "body over the edge");
        assert!(!in_view(&cull, at, 0.5), "body clear of the edge");
    }
}
