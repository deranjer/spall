use super::*;

pub(super) type VegetationKey = (u64, i64, i64, u64);
pub(super) type VegetationRequest = (
    u64,
    Arc<spall_ecology::living::VisualFrame>,
    [f32; 3],
    std::collections::HashSet<BrickCoord>,
    Option<Volume>,
);

/// The open cell above the soil in column `(x, z)` and the soil's material,
/// found by walking from the last column's surface (`hint`): neighbouring
/// columns differ by a few cells, so the walk is short. `None` where the
/// terrain is not resident or its mesh is not built yet, so cover never
/// stands in the air waiting for the ground it grows on.
pub(super) fn cover_surface(
    volume: &Volume,
    ready: &std::collections::HashSet<BrickCoord>,
    x: i64,
    z: i64,
    hint: &mut i64,
) -> Option<(i64, u16)> {
    let mut y = *hint;
    for _ in 0..48 {
        match volume.sample(GlobalCell::new(x, y, z)).ok()? {
            Sample::Unknown(_) => return None,
            Sample::Filled(_) => y += 1,
            Sample::Empty { .. } => match volume.sample(GlobalCell::new(x, y - 1, z)).ok()? {
                Sample::Filled(m) => {
                    if !ready.contains(&GlobalCell::new(x, y - 1, z).split().0) {
                        return None;
                    }
                    *hint = y;
                    return Some((y, m.0));
                }
                Sample::Empty { .. } => y -= 1,
                Sample::Unknown(_) => return None,
            },
        }
    }
    None
}

/// Distance fog covers this part of the view radius, from where it starts to
/// where it is complete (a little inside the edge, so the last chunks are
/// already the horizon's colour).
pub(super) const FOG_START_FRACTION: f32 = 0.62;
pub(super) const FOG_END_FRACTION: f32 = 0.97;

/// The shortest time between foliage rebuilds. A rebuild takes tens of
/// milliseconds on a worker thread and uploads megabytes, and while walking the
/// terrain changes every frame as chunks stream in; rebuilding on every change
/// kept a core busy and the upload queue full (a deep forest ran at half the
/// frame rate with it, measured), for foliage that is a few hundred
/// milliseconds late at most.
/// Minimum time between ground-snow geometry rebuilds (each costs ~10 ms).
/// Furthest ground snow is generated, whatever the view distance: beyond this the
/// geometry rebuild no longer fits a frame budget.
pub(super) const SNOW_MAX_RADIUS_M: f32 = 96.0;
pub(super) const SNOW_REBUILD_INTERVAL: Duration = Duration::from_millis(100);
/// Longest one update may spend meshing snow tiles; the rest wait for the next.
pub(super) const SNOW_BUILD_BUDGET: Duration = Duration::from_millis(4);
pub(super) const FOLIAGE_REBUILD_INTERVAL: Duration = Duration::from_millis(400);

/// Trees farther than this (metres) are drawn with half-resolution crowns.
pub(super) const LEAF_LOD_M: f32 = 36.0;

/// Set `SPALL_VEGETATION_TRACE=1` to log each foliage rebuild and upload.
pub(super) fn vegetation_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SPALL_VEGETATION_TRACE").is_some())
}

pub(super) type CoverKey = (i64, i64, u8, u64, usize, u64, usize, u64);

/// How long the client holds back cover in a column whose surface changed
/// before the server's record of the destroyed cover reaches it, seconds. The
/// hole is seen at once but the record rides the next once-a-second frame; if
/// the server never records it (the edit did not reach the topsoil) the new
/// surface is simply adopted after this.
pub(super) const COVER_PENDING_S: f32 = 3.0;

/// The surface each column had when cover was last drawn on it, and since when
/// it has differed (see [`COVER_PENDING_S`]).
pub(super) type SeenSurfaces = std::collections::HashMap<(i64, i64), (i64, Option<Instant>)>;
/// Soft foliage waits for the same solid geometry that supports it. Network
/// vegetation can arrive before the progressive terrain/trunk mesh batches.
pub(super) fn foliage_support_ready(
    plant: &spall_ecology::living::VisualPlant,
    tree: bool,
    ready: &std::collections::HashSet<BrickCoord>,
) -> bool {
    let soil = GlobalCell::new(plant.root[0], plant.root[1] - 1, plant.root[2])
        .split()
        .0;
    ready.contains(&soil)
        && (!tree
            || plant.tips.as_slice() == [plant.root]
            || plant
                .tips
                .iter()
                .all(|at| ready.contains(&GlobalCell::new(at[0], at[1], at[2]).split().0)))
}

pub(super) struct VegetationWorker {
    pub(super) tx: mpsc::SyncSender<VegetationRequest>,
    pub(super) rx: mpsc::Receiver<(u64, Vec<Instance>)>,
}
impl VegetationWorker {
    pub(super) fn spawn(material: u32) -> Result<Self, ClientError> {
        let (tx, requests) = mpsc::sync_channel::<VegetationRequest>(1);
        let (results, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("spall-client-foliage".into())
            .spawn(move || {
                let mut grid: Option<(
                    spall_ecology::cover::CoverField,
                    spall_ecology::cover::CoverGrid,
                )> = None;
                let mut cover_cache: Option<(CoverKey, Vec<spall_ecology::living::SoftPart>)> =
                    None;
                let mut seen: SeenSurfaces = SeenSurfaces::new();
                for (id, frame, eye, ready, terrain) in requests {
                    let job_started = Instant::now();
                    // Leaves draw as far as the terrain and trunks do, so a tree
                    // never stands bare at a distance and then sprouts as you near.
                    let foliage_radius_m = view_radius_m();
                    let mut frame = (*frame).clone();
                    frame.plants.retain(|p| {
                        let tree = frame.species.iter().any(|s| s.id == p.species && s.tree);
                        foliage_support_ready(p, tree, &ready)
                    });
                    // The ground cover: its own budget, thinning with distance,
                    // computed from the climate raster and the terrain here.
                    let mut cover_parts = Vec::new();
                    if let Some(volume) = terrain.as_ref().filter(|_| {
                        !frame.cover.is_empty() && std::env::var_os("SPALL_NO_COVER").is_none()
                    }) {
                        if grid.as_ref().map(|(f, _)| f) != Some(&frame.field) {
                            grid = frame.field.expand().ok().map(|g| (frame.field.clone(), g));
                        }
                        if let Some((_, grid)) = &grid {
                            let key: CoverKey = (
                                (eye[0] / 2.0).floor() as i64,
                                (eye[2] / 2.0).floor() as i64,
                                frame.season as u8,
                                volume.state_stamp(),
                                ready.len(),
                                frame.seed,
                                frame.cleared.len(),
                                frame.time_ms / 10_000,
                            );
                            match &cover_cache {
                                Some((k, parts)) if *k == key => cover_parts = parts.clone(),
                                _ => {
                                    let mut hint = (eye[1] / 0.25).floor() as i64;
                                    let memory =
                                        spall_ecology::cover::CoverMemory::new(&frame.cleared);
                                    if seen.len() > 400_000 {
                                        seen.clear();
                                    }
                                    let mut surface = |x: i64, z: i64| {
                                        let found = cover_surface(volume, &ready, x, z, &mut hint)?;
                                        match seen.get_mut(&(x, z)) {
                                            None => {
                                                seen.insert((x, z), (found.0, None));
                                            }
                                            Some((y, since)) if *y == found.0 => *since = None,
                                            // The server has the column on record: it
                                            // owns the regrowth, adopt the new ground.
                                            Some((y, since)) if memory.is_recorded(x, z) => {
                                                *y = found.0;
                                                *since = None;
                                            }
                                            Some((y, since)) => {
                                                let from = *since.get_or_insert_with(Instant::now);
                                                if from.elapsed().as_secs_f32() < COVER_PENDING_S {
                                                    return None;
                                                }
                                                *y = found.0;
                                                *since = None;
                                            }
                                        }
                                        Some(found)
                                    };
                                    cover_parts = spall_ecology::cover::cover_parts(
                                        grid,
                                        &frame.cover,
                                        frame.seed,
                                        frame.season,
                                        eye,
                                        foliage_radius_m,
                                        &mut surface,
                                        &memory,
                                        spall_ecology::cover::INSTANCE_BUDGET,
                                    );
                                    cover_cache = Some((key, cover_parts.clone()));
                                }
                            }
                        }
                    }
                    // Leaves are drawn on the same quarter-cell grid as the wood
                    // they grow from: a leaf cube in a cell that holds wood (or any
                    // solid) would share its faces with it and flicker, so it is
                    // left out and the wood shows.
                    let crown: Vec<_> = frame
                        .parts_lod(eye, foliage_radius_m, LEAF_LOD_M)
                        .into_iter()
                        .filter(|p| {
                            // A leaf block is one quarter-cell, or at a distance a
                            // block of 2 x 2 x 2 of them.
                            let block = [(0.25, 1), (0.5, 2)]
                                .into_iter()
                                .find(|(edge, _)| p.size == [*edge; 3])
                                .map(|(_, cells)| cells);
                            let leaf = block.is_some() && p.yaw == 0. && p.lean == 0.;
                            !leaf
                                || terrain.as_ref().is_none_or(|volume| {
                                    let n = block.unwrap_or(1);
                                    let at = p
                                        .center
                                        .map(|c| ((c - 0.125 * n as f32) / 0.25).round() as i64);
                                    !(0..n).any(|dy| {
                                        (0..n).any(|dz| {
                                            (0..n).any(|dx| {
                                                matches!(
                                                    volume.sample(GlobalCell::new(
                                                        at[0] + dx,
                                                        at[1] + dy,
                                                        at[2] + dz
                                                    )),
                                                    Ok(Sample::Filled(_))
                                                )
                                            })
                                        })
                                    })
                                })
                        })
                        .collect();
                    let instances = crown
                        .into_iter()
                        .chain(cover_parts)
                        .map(|p| {
                            Instance::new(
                                p.center,
                                material + u32::from(p.colour),
                                p.size,
                                (glam::Quat::from_rotation_y(p.yaw)
                                    * glam::Quat::from_rotation_z(p.lean))
                                .to_array(),
                            )
                        })
                        .collect();
                    let compact = spall_render::compact_vegetation_instances(instances);
                    if vegetation_trace() {
                        eprintln!(
                            "spall-vegetation-trace: job {id} built {} instances in {:.1} ms",
                            compact.len(),
                            job_started.elapsed().as_secs_f32() * 1000.0
                        );
                    }
                    if results.send((id, compact)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| ClientError::Render(format!("foliage worker: {e}")))?;
        Ok(Self { tx, rx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vegetation_arriving_before_solid_batches_waits_for_ground_and_trunks() {
        let mut plant = spall_ecology::living::VisualPlant {
            id: 1,
            species: 1,
            root: [0, 31, 0],
            biomass: 100,
            tips: vec![[0, 34, 0]],
        };
        let mut ready = std::collections::HashSet::new();
        assert!(!foliage_support_ready(&plant, false, &ready));
        assert!(!foliage_support_ready(&plant, true, &ready));
        ready.insert(GlobalCell::new(0, 30, 0).split().0);
        assert!(foliage_support_ready(&plant, false, &ready));
        assert!(!foliage_support_ready(&plant, true, &ready));
        ready.insert(GlobalCell::new(0, 34, 0).split().0);
        assert!(foliage_support_ready(&plant, true, &ready));
        ready.remove(&GlobalCell::new(0, 30, 0).split().0);
        assert!(!foliage_support_ready(&plant, true, &ready));
        plant.tips = vec![plant.root];
        ready.insert(GlobalCell::new(0, 30, 0).split().0);
        assert!(foliage_support_ready(&plant, true, &ready));
    }
}
