//! Converts bounded authoritative snow tile frames into camera-local renderer
//! geometry. Exposure is revalidated against the current immutable terrain
//! replica; absent/failed bricks, stale revisions, and unknown above-cells are
//! never treated as exposed air.

use spall_core::{BrickCoord, GlobalCell};
use spall_protocol::snow::{
    SNOW_MAX_DEPTH_Q16, SNOW_MIN_FOOTPRINT_DEPTH_Q16, SNOW_SCHEMA, SNOW_TILE_CELLS, SnowFootprint,
    SnowSurface, SnowSurfaceFrame,
};
use spall_render::{SnowFootprintMark, SnowSurfacePatch};
use spall_voxel::{Sample, Volume};

const CELL_M: f32 = 0.25;
const CAMERA_BUCKET_M: f32 = 8.0;
const CAMERA_RADIUS_M: f32 = 72.0;
const MAX_INPUT_FRAMES: usize = 4_096;
const MIN_RENDER_DEPTH_Q16: u16 = 33; // About 0.5 mm; zero-depth cohorts stay invisible.

/// Compact counters for weather HUDs and bounded capture diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SnowPresentationStats {
    pub frames_considered: usize,
    pub surface_cohorts: usize,
    pub confirmed_faces: usize,
    pub rendered_faces: usize,
    pub patches: usize,
    pub footprints: usize,
    pub max_depth_mm: u16,
    pub cap_limited_surfaces: usize,
    pub revision_mismatches: usize,
    pub hidden_or_unknown_faces: usize,
}
/// Cached camera-local snow mesh inputs. Call `update` when frames publish or
/// the camera moves into another 8 m bucket; a stable frame/volume/bucket key
/// returns the same slices without rescanning terrain cells.
#[derive(Debug, Default)]
pub struct SnowPresentation {
    cache_key: Option<u64>,
    patches: Vec<SnowSurfacePatch>,
    footprints: Vec<SnowFootprintMark>,
    stats: SnowPresentationStats,
}

impl SnowPresentation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Diagnostic counters from the current cached mesh.
    pub fn stats(&self) -> SnowPresentationStats {
        self.stats
    }

    /// Returns merged exposed-face rectangles and clipped footprint marks.
    /// `camera` is in world metres; the volume uses quarter-metre cells.
    pub fn update<'a>(
        &'a mut self,
        frames: &[SnowSurfaceFrame],
        volume: &Volume,
        camera: [f32; 3],
    ) -> (&'a [SnowSurfacePatch], &'a [SnowFootprintMark]) {
        if camera.iter().any(|value| !value.is_finite())
            || (volume.cell_size().metres() - f64::from(CELL_M)).abs() > f64::EPSILON
        {
            self.cache_key = None;
            self.patches.clear();
            self.footprints.clear();
            self.stats = SnowPresentationStats::default();
            return (&self.patches, &self.footprints);
        }
        let camera_bucket = camera.map(|value| (value / CAMERA_BUCKET_M).floor() as i64);
        let key = cache_key(frames, volume.state_stamp(), camera_bucket);
        if self.cache_key == Some(key) {
            return (&self.patches, &self.footprints);
        }
        self.cache_key = Some(key);
        self.patches.clear();
        self.footprints.clear();
        self.stats = SnowPresentationStats::default();
        if frames.is_empty() {
            return (&self.patches, &self.footprints);
        }

        let current_epoch = frames
            .iter()
            .map(|frame| frame.world_epoch)
            .max()
            .unwrap_or(0);
        let mut nearby: Vec<&SnowSurfaceFrame> = frames
            .iter()
            .filter(|frame| {
                frame.schema == SNOW_SCHEMA
                    && frame.world_epoch == current_epoch
                    && tile_distance_squared(frame, camera) <= CAMERA_RADIUS_M * CAMERA_RADIUS_M
            })
            .collect();
        nearby.sort_by(|a, b| {
            tile_distance_squared(a, camera)
                .total_cmp(&tile_distance_squared(b, camera))
                .then_with(|| a.tile_z.cmp(&b.tile_z))
                .then_with(|| a.tile_x.cmp(&b.tile_x))
                .then_with(|| a.top_cell_y.cmp(&b.top_cell_y))
        });
        nearby.truncate(MAX_INPUT_FRAMES);
        self.stats.frames_considered = nearby.len();

        let mut patch_budget = spall_render::snow::MAX_SNOW_PATCHES;
        let mut mark_budget = spall_render::snow::MAX_SNOW_FOOTPRINTS;
        'frames: for frame in nearby {
            let tile_min_x = i64::from(frame.tile_x) * i64::from(SNOW_TILE_CELLS);
            let tile_min_z = i64::from(frame.tile_z) * i64::from(SNOW_TILE_CELLS);
            for surface in &frame.surfaces {
                if patch_budget == 0 {
                    break 'frames;
                }
                if surface.top_cell_y != frame.top_cell_y
                    || surface.depth_q16 < MIN_RENDER_DEPTH_Q16
                    || surface.depth_q16 > SNOW_MAX_DEPTH_Q16
                    || surface.exposed_faces.iter().all(|mask| *mask == 0)
                {
                    continue;
                }
                self.stats.surface_cohorts += 1;
                self.stats.max_depth_mm = self
                    .stats
                    .max_depth_mm
                    .max(((u32::from(surface.depth_q16) * 1_000) / 65_536) as u16);
                let (valid_faces, revision_mismatches, hidden_or_unknown) =
                    validate_surface_faces(surface, tile_min_x, tile_min_z, volume);
                self.stats.revision_mismatches += revision_mismatches;
                self.stats.hidden_or_unknown_faces += hidden_or_unknown;
                let face_count = valid_faces
                    .iter()
                    .map(|word| word.count_ones() as usize)
                    .sum::<usize>();
                self.stats.confirmed_faces += face_count;
                let rects = greedy_rectangles(valid_faces);
                if rects.len() > patch_budget {
                    self.stats.cap_limited_surfaces += 1;
                    break 'frames;
                }
                self.stats.rendered_faces += face_count;
                patch_budget -= rects.len();
                let depth_m = f32::from(surface.depth_q16) / 65_536.0;
                for (x, z, width, height) in rects {
                    let min_x = tile_min_x + x as i64;
                    let min_z = tile_min_z + z as i64;
                    self.patches.push(SnowSurfacePatch {
                        centre: [
                            (min_x as f32 + width as f32 * 0.5) * CELL_M,
                            (frame.top_cell_y as f32 + 1.0) * CELL_M,
                            (min_z as f32 + height as f32 * 0.5) * CELL_M,
                        ],
                        half_extent_xz: [width as f32 * CELL_M * 0.5, height as f32 * CELL_M * 0.5],
                        depth_m,
                        variation: u32::from_le_bytes(
                            surface.id.0[..4].try_into().expect("four ID bytes"),
                        ),
                    });
                }
                if mark_budget > 0 && surface.depth_q16 >= SNOW_MIN_FOOTPRINT_DEPTH_Q16 {
                    for mark in frame.footprints.iter().filter(|mark| {
                        mark.surface_id == surface.id
                            && mark.geometry_revision == surface.geometry_revision
                            && mark.contact_cell[1] == surface.top_cell_y
                            && face_valid(&valid_faces, mark.contact_cell, tile_min_x, tile_min_z)
                    }) {
                        if mark_budget == 0 {
                            break;
                        }
                        let [cell_x, cell_y, cell_z] = mark.contact_cell;
                        let origin_x = cell_x as f32 * CELL_M;
                        let origin_z = cell_z as f32 * CELL_M;
                        let x = origin_x + f32::from(mark.offset_mm[0]) * 0.001;
                        let z = origin_z + f32::from(mark.offset_mm[1]) * 0.001;
                        self.footprints.push(SnowFootprintMark {
                            centre: [
                                x,
                                (cell_y as f32 + 1.0) * CELL_M + depth_m.max(0.0015) + 0.0015,
                                z,
                            ],
                            cell_centre_xz: [origin_x + CELL_M * 0.5, origin_z + CELL_M * 0.5],
                            heading_rad: f32::from(mark.heading_q15) / 32768.0
                                * std::f32::consts::PI,
                            foot: mark.foot,
                            strength: f32::from(mark.strength_q8) / 255.0,
                        });
                        mark_budget -= 1;
                    }
                }
            }
        }
        self.stats.patches = self.patches.len();
        self.stats.footprints = self.footprints.len();
        (&self.patches, &self.footprints)
    }
}

fn cache_key(frames: &[SnowSurfaceFrame], volume_stamp: u64, camera_bucket: [i64; 3]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut write = |value: u64| {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    };
    write(volume_stamp);
    for coordinate in camera_bucket {
        write(coordinate as u64);
    }
    write(frames.len() as u64);
    for frame in frames {
        write(frame.world_epoch);
        write(frame.snapshot_seq);
        write(frame.world_revision);
        write(frame.tile_x as u32 as u64);
        write(frame.tile_z as u32 as u64);
        write(frame.top_cell_y as u32 as u64);
    }
    hash
}

fn tile_distance_squared(frame: &SnowSurfaceFrame, camera: [f32; 3]) -> f32 {
    let min_x = frame.tile_x as f32 * 4.0;
    let min_z = frame.tile_z as f32 * 4.0;
    let dx = if camera[0] < min_x {
        min_x - camera[0]
    } else if camera[0] > min_x + 4.0 {
        camera[0] - min_x - 4.0
    } else {
        0.0
    };
    let dz = if camera[2] < min_z {
        min_z - camera[2]
    } else if camera[2] > min_z + 4.0 {
        camera[2] - min_z - 4.0
    } else {
        0.0
    };
    dx * dx + dz * dz
}

fn validate_surface_faces(
    surface: &SnowSurface,
    tile_min_x: i64,
    tile_min_z: i64,
    volume: &Volume,
) -> ([u64; 4], usize, usize) {
    let mut valid = [0u64; 4];
    let mut revision_mismatches = 0;
    let mut hidden_or_unknown = 0;
    let y = i64::from(surface.top_cell_y);
    for index in 0..256 {
        let bit = 1u64 << (index % 64);
        if surface.exposed_faces[index / 64] & bit == 0 {
            continue;
        }
        let x = tile_min_x + (index % 16) as i64;
        let z = tile_min_z + (index / 16) as i64;
        let solid = GlobalCell::new(x, y, z);
        let above = GlobalCell::new(x, y + 1, z);
        let (brick, _) = solid.split();
        match volume.brick_revision(brick) {
            Ok(Some(revision)) if revision.0 == surface.geometry_revision => {}
            Ok(Some(_)) => {
                revision_mismatches += 1;
                continue;
            }
            Ok(None) | Err(_) => {
                hidden_or_unknown += 1;
                continue;
            }
        }
        match (volume.sample(solid), volume.sample(above)) {
            (Ok(Sample::Filled(_)), Ok(Sample::Empty { .. })) => valid[index / 64] |= bit,
            _ => hidden_or_unknown += 1,
        }
    }
    (valid, revision_mismatches, hidden_or_unknown)
}

fn face_valid(mask: &[u64; 4], cell: [i32; 3], tile_min_x: i64, tile_min_z: i64) -> bool {
    let x = i64::from(cell[0]) - tile_min_x;
    let z = i64::from(cell[2]) - tile_min_z;
    if !(0..16).contains(&x) || !(0..16).contains(&z) {
        return false;
    }
    let index = z as usize * 16 + x as usize;
    mask[index / 64] & (1u64 << (index % 64)) != 0
}

/// Greedy, non-overlapping rectangles over a single validated 16×16 cohort.
/// Cohorts/depth/Y layers are converted independently, so merging never bridges
/// a sloped face, another surface, or an unexposed cell.
fn greedy_rectangles(mask: [u64; 4]) -> Vec<(usize, usize, usize, usize)> {
    let mut remaining = mask;
    let mut result = Vec::new();
    loop {
        let Some(index) = (0..4).find_map(|word| {
            (remaining[word] != 0).then_some(word * 64 + remaining[word].trailing_zeros() as usize)
        }) else {
            break;
        };
        let x = index % 16;
        let z = index / 16;
        let mut width = 0;
        while x + width < 16 && mask_bit(&remaining, x + width, z) {
            width += 1;
        }
        let mut height = 1;
        'rows: while z + height < 16 {
            for dx in 0..width {
                if !mask_bit(&remaining, x + dx, z + height) {
                    break 'rows;
                }
            }
            height += 1;
        }
        for dz in 0..height {
            for dx in 0..width {
                clear_mask_bit(&mut remaining, x + dx, z + dz);
            }
        }
        result.push((x, z, width, height));
    }
    result
}

fn mask_bit(mask: &[u64; 4], x: usize, z: usize) -> bool {
    let index = z * 16 + x;
    mask[index / 64] & (1u64 << (index % 64)) != 0
}

fn clear_mask_bit(mask: &mut [u64; 4], x: usize, z: usize) {
    let index = z * 16 + x;
    mask[index / 64] &= !(1u64 << (index % 64));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greedy_rectangles_merge_flat_surfaces_without_crossing_holes() {
        let mut mask = [0u64; 4];
        for z in 0..3 {
            for x in 0..4 {
                if !(x == 2 && z == 1) {
                    let index = z * 16 + x;
                    mask[index / 64] |= 1u64 << (index % 64);
                }
            }
        }
        let rects = greedy_rectangles(mask);
        let covered: usize = rects
            .iter()
            .map(|(_, _, width, height)| width * height)
            .sum();
        assert_eq!(covered, 11);
        assert_eq!(rects.len(), 4);
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    use spall_core::{CellSizeCode, LocalCell, MaterialId, Revision, VolumeId};
    use spall_voxel::Brick;

    fn surface(top_cell_y: i32, depth_q16: u16) -> SnowSurface {
        let mut exposed_faces = [0u64; 4];
        exposed_faces[0] = 1;
        SnowSurface {
            id: spall_protocol::snow::SnowSurfaceId([7; 16]),
            top_cell_y,
            geometry_revision: 1,
            exposure_generation: 1,
            exposed_faces,
            depth_q16,
        }
    }

    fn frame(top_cell_y: i32, depth_q16: u16) -> SnowSurfaceFrame {
        SnowSurfaceFrame {
            schema: SNOW_SCHEMA,
            world_epoch: 1,
            world_revision: 1,
            effective_tick: 120,
            snapshot_seq: 1,
            tile_x: 0,
            tile_z: 0,
            top_cell_y,
            surfaces: vec![surface(top_cell_y, depth_q16)],
            footprints: Vec::new(),
        }
    }

    fn plane(top_y: u8, add_above: bool) -> Volume {
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let mut brick = Brick::uniform(MaterialId::AIR, Revision(1));
        assert!(brick.set_cell(LocalCell::new(0, top_y, 0).unwrap(), MaterialId(3),));
        if add_above && top_y < 31 {
            assert!(brick.set_cell(LocalCell::new(0, top_y + 1, 0).unwrap(), MaterialId(4),));
        }
        volume
            .insert_brick(BrickCoord::new(0, 0, 0), brick)
            .unwrap();
        volume
    }

    #[test]
    fn converter_keeps_only_current_revision_exposed_solid_faces() {
        let mut presentation = SnowPresentation::new();
        let camera = [0.5, 1.0, 0.5];
        let supported = plane(0, false);
        let (patches, _) = presentation.update(&[frame(0, 132)], &supported, camera);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].centre, [0.125, 0.25, 0.125]);
        assert_eq!(patches[0].half_extent_xz, [0.125, 0.125]);

        let roofed = plane(0, true);
        let (patches, _) = presentation.update(&[frame(0, 132)], &roofed, camera);
        assert!(
            patches.is_empty(),
            "a newly filled above-cell blocks stale snow"
        );

        let stale = plane(0, false);
        let mut stale_frame = frame(0, 132);
        stale_frame.surfaces[0].geometry_revision = 2;
        let (patches, _) = presentation.update(&[stale_frame], &stale, camera);
        assert!(
            patches.is_empty(),
            "a brick revision mismatch suppresses the face"
        );
    }

    #[test]
    fn unknown_above_brick_suppresses_snow_and_unknown_support_is_never_air() {
        let mut presentation = SnowPresentation::new();
        let top_of_brick = plane(31, false);
        let (patches, _) = presentation.update(&[frame(31, 132)], &top_of_brick, [0.5, 8.0, 0.5]);
        assert!(
            patches.is_empty(),
            "unloaded brick above the surface is unknown"
        );

        let empty = Volume::new(VolumeId::new(2).unwrap(), CellSizeCode::Quarter);
        let (patches, _) = presentation.update(&[frame(0, 132)], &empty, [0.5, 1.0, 0.5]);
        assert!(
            patches.is_empty(),
            "unloaded support cell cannot receive snow"
        );
    }

    #[test]
    fn footprint_requires_a_deep_confirmed_contact_face_and_stays_in_its_cell() {
        let mut presentation = SnowPresentation::new();
        let volume = plane(0, false);
        let mut snapshot = frame(0, 132);
        snapshot.footprints.push(SnowFootprint {
            id: 1,
            surface_id: snapshot.surfaces[0].id,
            geometry_revision: 1,
            contact_cell: [0, 0, 0],
            offset_mm: [240, 125],
            heading_q15: 8192,
            foot: 1,
            created_tick: 119,
            strength_q8: 255,
        });
        let (patches, marks) = presentation.update(&[snapshot.clone()], &volume, [0.5, 1.0, 0.5]);
        assert_eq!(patches.len(), 1);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].foot, 1);
        assert!(marks[0].centre[1] > patches[0].centre[1]);
        assert!(marks[0].heading_rad > 0.0);

        snapshot.snapshot_seq += 1;
        snapshot.surfaces[0].depth_q16 = SNOW_MIN_FOOTPRINT_DEPTH_Q16 - 1;
        let (_, marks) = presentation.update(&[snapshot], &volume, [0.5, 1.0, 0.5]);
        assert!(marks.is_empty());
    }
}
