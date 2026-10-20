use super::*;

/// Runs [`build_instances`] on a dedicated background thread instead of the
/// window's render/input thread. Every dispatched request gets a freshly
/// walked instance list unconditionally — this no longer consults
/// `ReplicaWorld::terrain_resident_hash()` to decide whether anything
/// actually changed first, because that hash walk costs as much as (or more
/// than) `build_instances` itself (see `perf_probe` below); it's cheaper
/// overall to occasionally rebuild an unchanged scene in the background than
/// to also pay for the "did it change" check.
///
/// This exists because of a measured feedback loop, not just to shave frame
/// time: on a big scene a rebuild can cost 100s of ms (`perf_probe`), and
/// when that ran synchronously in `RedrawRequested`, a slow rebuild delayed
/// the *next* frame, during which the player (still receiving predicted
/// motion from the net thread, which this never blocked) covered more
/// ground before the window got to check again — pushing it straight back
/// past `REBUILD_DISTANCE_M` and into another rebuild. Once a rebuild's cost
/// exceeds roughly `REBUILD_DISTANCE_M` / walking speed, that loop never
/// breaks on its own: FPS collapses further with every step, while the GPU
/// stays idle throughout (this is pure CPU volume-walking, no draw-call
/// cost) and the one thread paying for it doesn't move an aggregate
/// multi-core CPU reading much — which is exactly the "incredible lag, flat
/// CPU/GPU graphs" symptom this was chasing.
///
/// At most one request is in flight at a time (`InteractiveApp` only sends a
/// new one once the last result has been drained); a request superseded by
/// player movement before the worker gets to it is simply answered a little
/// stale; the renderer always has *something* to draw meanwhile, because it
/// keeps presenting the last completed result rather than blocking on a new
/// one.
pub(super) struct RebuildWorker {
    pub(super) request_tx: mpsc::Sender<[f64; 3]>,
    pub(super) result_rx: mpsc::Receiver<RebuildOutcome>,
    pub(super) in_flight: bool,
    /// Hammer swings awaiting the server, shared with the worker thread.
    pub(super) pending: Arc<Mutex<Vec<PendingCut>>>,
}

pub struct RebuildOutcome {
    pub center_m: [f64; 3],
    /// `true` for the terrain result of a pass; `false` for the lighting
    /// result that follows it (emitters and sky only).
    pub terrain_included: bool,
    /// Partial mesh batches keep the current request in flight until complete.
    pub terrain_complete: bool,
    /// Legacy instanced terrain; only [`rebuild_pass`] fills it.
    pub instances: Vec<Instance>,
    /// Changed greedy terrain chunks; unchanged chunks retain their GPU buffers.
    pub mesh_updates: Vec<(BrickCoord, Vec<spall_render::GpuVertex>, Vec<u32>)>,
    /// Chunks that left render residency or became empty.
    pub mesh_removed: Vec<BrickCoord>,
    /// Chunks in `mesh_updates` whose bricks did not change, only the level of
    /// detail it was meshed at: their shadows need no redraw.
    pub mesh_detail_only: Vec<BrickCoord>,
    pub mesh_elapsed: Duration,
    /// Emissive cells of the visible terrain (torches and lamps), for the
    /// point lights; sent with the lighting result.
    pub emitters: Option<Vec<Instance>>,
    /// Sky occupancy built from the same volume snapshot as `instances`, or
    /// `None` when it is identical to the last one sent (nothing to recompute).
    pub sky: Option<spall_render::indirect::LightingVolume>,
    pub sky_stats: crate::sky::SkyOccupancyStats,
    pub elapsed: Duration,
}

/// The sky occupancy grid around `center_m` (kept where it is until the player
/// has moved far enough to matter, so a slowly moving player produces an
/// identical grid), or `None` when it is identical to the last one sent.
pub fn rebuild_sky(
    volume: &Volume,
    center_m: [f64; 3],
    sky_anchor: &mut Option<[f64; 3]>,
    last_sky: &mut Option<spall_render::indirect::LightingVolume>,
    absent_is_open: bool,
) -> (
    Option<spall_render::indirect::LightingVolume>,
    crate::sky::SkyOccupancyStats,
    Duration,
) {
    let (sky, stats, elapsed, _held) = rebuild_sky_deferring(
        volume,
        center_m,
        sky_anchor,
        last_sky,
        absent_is_open,
        false,
        SKY_ANCHOR_DRIFT_M,
    );
    (sky, stats, elapsed)
}

/// Same as [`rebuild_sky`], but with `defer_minor` a *small* occupancy change
/// (see [`sky_change_is_minor`]) is withheld: `last_sky` is left untouched and
/// the final flag reports that, so the caller retries on a later pass. Every
/// accepted sky grid costs the GPU a full time-sliced visibility and bounce
/// sweep, so slow background edits (growing trees add a cell or two) must not
/// each trigger one.
pub(super) fn rebuild_sky_deferring(
    volume: &Volume,
    center_m: [f64; 3],
    sky_anchor: &mut Option<[f64; 3]>,
    last_sky: &mut Option<spall_render::indirect::LightingVolume>,
    absent_is_open: bool,
    defer_minor: bool,
    drift_m: f64,
) -> (
    Option<spall_render::indirect::LightingVolume>,
    crate::sky::SkyOccupancyStats,
    Duration,
    bool,
) {
    let start = Instant::now();
    let sky_center = match *sky_anchor {
        Some(anchor) if within_sky_anchor(anchor, center_m, drift_m) => anchor,
        _ => center_m,
    };
    *sky_anchor = Some(sky_center);
    let (grid, sky_stats) = crate::sky::build_sky_occupancy(volume, sky_center, absent_is_open);
    let changed = last_sky
        .as_ref()
        .is_none_or(|last| last.origin() != grid.origin() || last.cells() != grid.cells());
    if changed
        && defer_minor
        && last_sky
            .as_ref()
            .is_some_and(|last| sky_change_is_minor(last, &grid))
    {
        return (None, sky_stats, start.elapsed(), true);
    }
    let sky = changed.then(|| {
        *last_sky = Some(grid.clone());
        grid
    });
    (sky, sky_stats, start.elapsed(), false)
}

/// Most differing light cells still treated as a "minor" sky change.
pub(super) const MINOR_SKY_CHANGE_CELLS: usize = 128;
/// Longest a minor sky change is held back before it is applied anyway.
pub(super) const MINOR_SKY_CHANGE_MAX_DEFER: Duration = Duration::from_secs(4);

/// `true` when `new` is the same grid as `last` apart from at most
/// [`MINOR_SKY_CHANGE_CELLS`] cells. A moved origin is never minor.
pub(super) fn sky_change_is_minor(
    last: &spall_render::indirect::LightingVolume,
    new: &spall_render::indirect::LightingVolume,
) -> bool {
    if last.origin() != new.origin() {
        return false;
    }
    let mut differing = 0;
    for (a, b) in last.cells().iter().zip(new.cells()) {
        if a != b {
            differing += 1;
            if differing > MINOR_SKY_CHANGE_CELLS {
                return false;
            }
        }
    }
    true
}

/// Per-brick emissive cells of the visible terrain, keyed by the brick's
/// revision: only changed bricks are rescanned. Uniform bricks are skipped (a
/// solid emissive block is a wall, not a torch).
#[derive(Default)]
pub(super) struct EmitterCache(HashMap<BrickCoord, (spall_core::Revision, Vec<Instance>)>);

/// Most emissive cells kept per brick and overall, so a glowing wall cannot
/// flood the point-light selection.
pub(super) const MAX_EMITTERS_PER_BRICK: usize = 64;
pub(super) const MAX_EMITTERS: usize = 2048;

impl EmitterCache {
    /// The emissive cells (as unit cubes at their centres) of every visible
    /// brick of `volume`. `emissive[id]` says whether material `id` emits.
    pub(super) fn update(
        &mut self,
        volume: &Volume,
        center_m: [f64; 3],
        emissive: &[bool],
        volatile: &std::collections::HashSet<BrickCoord>,
    ) -> Vec<Instance> {
        use spall_core::LocalCell;
        if !emissive.iter().any(|&e| e) {
            return Vec::new();
        }
        let cell_m = CELL_M;
        let visible = visible_terrain_bricks(volume, center_m);
        let keep: std::collections::HashSet<_> = visible.iter().copied().collect();
        self.0.retain(|coord, _| keep.contains(coord));
        let mut out = Vec::new();
        for coord in visible {
            let Ok(Some(snap)) = volume.snapshot_brick(coord) else {
                continue;
            };
            if !snap.is_dense() {
                self.0.remove(&coord);
                continue;
            }
            let revision = snap.revision();
            let is_volatile = volatile.contains(&coord);
            if !is_volatile
                && let Some((cached, found)) = self.0.get(&coord)
                && *cached == revision
            {
                out.extend_from_slice(found);
                continue;
            }
            let mut found = Vec::new();
            'scan: for z in 0..32u8 {
                for y in 0..32u8 {
                    for x in 0..32u8 {
                        let local = LocalCell::new(x, y, z).expect("in-brick");
                        let material = snap.get(local).0;
                        if !emissive
                            .get(usize::from(material))
                            .copied()
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        let centre = [
                            ((coord.x * 32 + i64::from(x)) as f32 + 0.5) * cell_m,
                            ((coord.y * 32 + i64::from(y)) as f32 + 0.5) * cell_m,
                            ((coord.z * 32 + i64::from(z)) as f32 + 0.5) * cell_m,
                        ];
                        found.push(Instance::new(
                            centre,
                            u32::from(material),
                            [cell_m; 3],
                            IDENTITY_ROTATION,
                        ));
                        if found.len() >= MAX_EMITTERS_PER_BRICK {
                            break 'scan;
                        }
                    }
                }
            }
            out.extend_from_slice(&found);
            if is_volatile {
                // A preview's revision is not the server's: never remember it.
                self.0.remove(&coord);
            } else {
                self.0.insert(coord, (revision, found));
            }
        }
        out.truncate(MAX_EMITTERS);
        out
    }
}

impl RebuildWorker {
    /// Spawns the worker thread. It exits on its own once `request_tx`'s
    /// last sender (owned by the `InteractiveApp` this returns into) drops --
    /// no explicit shutdown signal or join needed. `emissive[id]` says which
    /// materials emit light (for the torch point lights).
    pub(super) fn spawn(
        session: Arc<InteractiveSession>,
        absent_is_open: bool,
        emissive: Vec<bool>,
        quality: Arc<std::sync::atomic::AtomicU8>,
    ) -> Result<Self, ClientError> {
        let (request_tx, request_rx) = mpsc::channel::<[f64; 3]>();
        let (result_tx, result_rx) = mpsc::channel();
        let pending: Arc<Mutex<Vec<PendingCut>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_pending = pending.clone();
        std::thread::Builder::new()
            .name("spall-client-rebuild".into())
            .spawn(move || {
                let mut sky_anchor: Option<[f64; 3]> = None;
                let mut last_sky: Option<spall_render::indirect::LightingVolume> = None;
                let mut mesh_cache = TerrainMeshCache::default();
                let mut emitter_cache = EmitterCache::default();
                let mut last_sky_generation: Option<u64> = None;
                let mut last_sky_sent_at = Instant::now();
                for center_m in request_rx {
                    let Some(replica) = session.replica.get() else {
                        continue;
                    };
                    let (mut volume, generation) = {
                        let replica = replica.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(volume) = replica.terrain_volume().cloned() else {
                            continue;
                        };
                        (volume, replica.terrain_generation())
                    };
                    // Unconfirmed hammer swings are drawn on this private copy
                    // (never on the replica); their bricks bypass the caches.
                    let mut previews = worker_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let volatile = apply_pending_cuts(&mut volume, &mut previews, Instant::now());
                    worker_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .retain(|p| p.at.elapsed() < PENDING_CUT_LIFETIME);

                    // Geometry first: it is what an edit or a step changes on
                    // screen, and it is cheap once the mesh cache is warm.
                    let started = Instant::now();
                    let (mesh_updates, mesh_removed, mesh_detail_only) =
                        update_terrain_mesh_cache_progressive(
                            &volume,
                            center_m,
                            &mut mesh_cache,
                            &volatile,
                            |mesh_updates, mesh_removed, mesh_detail_only| {
                                let elapsed = started.elapsed();
                                let _ = result_tx.send(RebuildOutcome {
                                    center_m,
                                    terrain_included: true,
                                    terrain_complete: false,
                                    instances: Vec::new(),
                                    mesh_updates,
                                    mesh_removed,
                                    mesh_detail_only,
                                    mesh_elapsed: elapsed,
                                    emitters: None,
                                    sky: None,
                                    sky_stats: crate::sky::SkyOccupancyStats::default(),
                                    elapsed,
                                });
                                // Do not finish a minute-long pass over an old
                                // replica while new authoritative edits are arriving.
                                session
                                    .replica
                                    .get()
                                    .and_then(|r| r.try_lock().ok().map(|r| r.terrain_generation()))
                                    .is_none_or(|current| current == generation)
                            },
                        );
                    let mesh_elapsed = started.elapsed();
                    let geometry = RebuildOutcome {
                        center_m,
                        terrain_included: true,
                        terrain_complete: true,
                        instances: Vec::new(),
                        mesh_updates,
                        mesh_removed,
                        mesh_detail_only,
                        mesh_elapsed,
                        emitters: None,
                        sky: None,
                        sky_stats: crate::sky::SkyOccupancyStats::default(),
                        elapsed: mesh_elapsed,
                    };
                    if result_tx.send(geometry).is_err() {
                        return; // the window is gone
                    }

                    // Then the lighting: emitters and sky. The sky grid is only
                    // rebuilt when the terrain changed or the player left the
                    // area the last grid was built around; rebuilding it on every
                    // step cost 15-40 ms for an identical result.
                    let drift_m = spall_render::sky_visibility::LightingQuality::ALL
                        [usize::from(quality.load(std::sync::atomic::Ordering::Relaxed)).min(3)]
                    .anchor_drift_m();
                    let anchored =
                        sky_anchor.is_some_and(|a| within_sky_anchor(a, center_m, drift_m));
                    let emitters = emitter_cache.update(&volume, center_m, &emissive, &volatile);
                    let (sky, sky_stats, elapsed) =
                        if last_sky_generation == Some(generation) && anchored {
                            (
                                None,
                                crate::sky::SkyOccupancyStats::default(),
                                Duration::ZERO,
                            )
                        } else {
                            // A preview (pending hammer swing) is the player's own
                            // edit and must light immediately; only slow background
                            // growth is deferred, and never for longer than the cap.
                            let defer_minor = volatile.is_empty()
                                && last_sky_sent_at.elapsed() < MINOR_SKY_CHANGE_MAX_DEFER;
                            let (sky, stats, elapsed, held) = rebuild_sky_deferring(
                                &volume,
                                center_m,
                                &mut sky_anchor,
                                &mut last_sky,
                                absent_is_open,
                                defer_minor,
                                spall_render::sky_visibility::LightingQuality::ALL[usize::from(
                                    quality.load(std::sync::atomic::Ordering::Relaxed),
                                )
                                .min(3)]
                                .anchor_drift_m(),
                            );
                            // Held: retry next pass instead of recording this generation.
                            if !held {
                                last_sky_generation = Some(generation);
                            }
                            if sky.is_some() {
                                last_sky_sent_at = Instant::now();
                            }
                            (sky, stats, elapsed)
                        };
                    let lighting = RebuildOutcome {
                        center_m,
                        terrain_included: false,
                        terrain_complete: false,
                        instances: Vec::new(),
                        mesh_updates: Vec::new(),
                        mesh_removed: Vec::new(),
                        mesh_detail_only: Vec::new(),
                        mesh_elapsed: Duration::ZERO,
                        emitters: Some(emitters),
                        sky,
                        sky_stats,
                        elapsed,
                    };
                    if result_tx.send(lighting).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| ClientError::Gpu(format!("spawning the terrain-rebuild thread: {e}")))?;
        Ok(Self {
            request_tx,
            result_rx,
            in_flight: false,
            pending,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A few changed light cells (a growing tree) are deferrable; a carve-sized
    /// change or a moved grid origin is not.
    #[test]
    fn sky_change_minor_only_for_few_cells_at_the_same_origin() {
        use spall_render::indirect::LightingVolume;
        let origin = glam::Vec3::ZERO;
        let base = LightingVolume::empty(origin);
        let mut few = base.clone();
        for x in 0..MINOR_SKY_CHANGE_CELLS as i32 {
            few.set(glam::IVec3::new(x, 0, 0), 1);
        }
        assert!(sky_change_is_minor(&base, &few));
        few.set(glam::IVec3::new(0, 1, 0), 1);
        assert!(!sky_change_is_minor(&base, &few));
        let moved = LightingVolume::empty(glam::Vec3::new(8., 0., 0.));
        assert!(!sky_change_is_minor(&base, &moved));
    }
}
