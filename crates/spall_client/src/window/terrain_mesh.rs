use super::*;

/// A chunk's mesh, with the level of detail it was built at and which of its
/// sides were closed (see [`terrain_lod`]): a chunk is rebuilt when either
/// changes as well as when its bricks do.
pub(super) struct CachedMesh {
    mesh: spall_mesh::VolumeMesh,
    lod: u8,
    closed: u8,
}

#[derive(Default)]
pub(super) struct TerrainMeshCache(HashMap<BrickCoord, CachedMesh>);

/// Chunks farther than this (metres, from the camera to the chunk's centre) are
/// meshed in blocks of two cells, and farther than the second distance in
/// blocks of four: a quarter and a sixteenth of the cells, far fewer triangles
/// where a triangle is smaller than a pixel anyway. The fog hides the change.
pub(super) const TERRAIN_LOD_FROM_M: [f64; 2] = [40.0, 72.0];
/// Most chunks whose bricks are unchanged that change level in one rebuild pass.
pub(super) const TERRAIN_LOD_FLIPS_PER_PASS: usize = 24;
/// A chunk keeps its level until it has moved this far past the distance, so
/// one standing on the line does not flip between levels as the camera sways.
pub(super) const TERRAIN_LOD_HYSTERESIS_M: f64 = 6.0;

/// The level of detail of a chunk `distance_m` from the camera: 0 full, 1
/// blocks of two cells, 2 blocks of four. `previous` is its level now, if it
/// has one. `SPALL_NO_TERRAIN_LOD=1` keeps everything at full detail.
pub(super) fn terrain_lod(distance_m: f64, previous: Option<u8>) -> u8 {
    // Read once: this runs for every chunk on every rebuild pass.
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("SPALL_NO_TERRAIN_LOD").is_some()) {
        return 0;
    }
    let mut lod = 0u8;
    for (i, from) in TERRAIN_LOD_FROM_M.iter().enumerate() {
        let level = i as u8 + 1;
        let threshold = match previous {
            Some(p) if p >= level => from - TERRAIN_LOD_HYSTERESIS_M,
            Some(_) => from + TERRAIN_LOD_HYSTERESIS_M,
            None => *from,
        };
        if distance_m >= threshold {
            lod = level;
        }
    }
    lod
}

/// Cells to a block of a mesh at `lod`.
pub(super) fn lod_stride(lod: u8) -> i64 {
    1 << lod
}

/// Which sides of the chunk at `coord` face a chunk of another level, bit
/// `2 * axis + positive`: those sides are closed (see
/// `spall_mesh::build_brick_mesh_lod`) so the seam cannot crack.
pub(super) fn closed_sides(coord: BrickCoord, lod: u8, lods: &HashMap<BrickCoord, u8>) -> u8 {
    let mut sides = 0u8;
    for axis in 0..3usize {
        for positive in [false, true] {
            let mut n = [coord.x, coord.y, coord.z];
            n[axis] += if positive { 1 } else { -1 };
            let neighbour = BrickCoord::new(n[0], n[1], n[2]);
            if lods.get(&neighbour).is_some_and(|other| *other != lod) {
                sides |= 1 << (2 * axis + usize::from(positive));
            }
        }
    }
    sides
}

pub(super) struct VolumeMeshWorld<'a>(&'a Volume);

impl WorldView for VolumeMeshWorld<'_> {
    fn generation(&self) -> Generation {
        Generation::START
    }
    fn topology_epoch(&self) -> TopologyEpoch {
        TopologyEpoch::START
    }
    fn brick_status(&self, brick: BrickRef) -> BrickStatus {
        match self.0.brick_state(brick.brick) {
            Ok(spall_voxel::BrickState::Resident { revision, .. }) => {
                BrickStatus::Resident(revision)
            }
            Ok(spall_voxel::BrickState::Failed) => BrickStatus::Failed,
            Ok(spall_voxel::BrickState::Absent) | Err(_) => BrickStatus::Absent,
        }
    }
}

pub(super) fn visible_terrain_bricks(volume: &Volume, center_m: [f64; 3]) -> Vec<BrickCoord> {
    let cell_m = f64::from(CELL_M);
    let center = GlobalCell::new(
        (center_m[0] / cell_m).floor() as i64,
        (center_m[1] / cell_m).floor() as i64,
        (center_m[2] / cell_m).floor() as i64,
    );
    let horiz = (view_radius_m() / CELL_M).ceil() as i64;
    let up = (VIEW_HEIGHT_UP_M / CELL_M).ceil() as i64;
    let down = (VIEW_HEIGHT_DOWN_M / CELL_M).ceil() as i64;
    let (min_brick, _) =
        GlobalCell::new(center.x - horiz, center.y - down, center.z - horiz).split();
    let (max_brick, _) = GlobalCell::new(center.x + horiz, center.y + up, center.z + horiz).split();
    let mut coords = Vec::new();
    for z in min_brick.z..=max_brick.z {
        for y in min_brick.y..=max_brick.y {
            for x in min_brick.x..=max_brick.x {
                let coord = BrickCoord::new(x, y, z);
                if volume.snapshot_brick(coord).ok().flatten().is_some() {
                    coords.push(coord);
                }
            }
        }
    }
    coords
}

pub type MeshUpdate<K> = (K, Vec<spall_render::GpuVertex>, Vec<u32>);

/// Brings `cache` up to date with `volume` around `center_m` and returns the
/// chunks to upload and to drop. Bricks in `volatile` are carved by unconfirmed
/// previews: a revision number identifies contents only along the server's
/// history, so they are always rebuilt and never remembered (a stale mesh could
/// otherwise survive under a colliding revision).
pub(super) fn update_terrain_mesh_cache(
    volume: &Volume,
    center_m: [f64; 3],
    cache: &mut TerrainMeshCache,
    volatile: &std::collections::HashSet<BrickCoord>,
) -> (Vec<MeshUpdate<BrickCoord>>, Vec<BrickCoord>) {
    let mut all_updates = Vec::new();
    let mut all_removed = Vec::new();
    let (updates, removed, _) = update_terrain_mesh_cache_progressive(
        volume,
        center_m,
        cache,
        volatile,
        |updates, removed, _| {
            all_updates.extend(updates);
            all_removed.extend(removed);
            true
        },
    );
    all_updates.extend(updates);
    all_removed.extend(removed);
    (all_updates, all_removed)
}

pub(super) fn update_terrain_mesh_cache_progressive(
    volume: &Volume,
    center_m: [f64; 3],
    cache: &mut TerrainMeshCache,
    volatile: &std::collections::HashSet<BrickCoord>,
    mut publish: impl FnMut(Vec<MeshUpdate<BrickCoord>>, Vec<BrickCoord>, Vec<BrickCoord>) -> bool,
) -> (
    Vec<MeshUpdate<BrickCoord>>,
    Vec<BrickCoord>,
    Vec<BrickCoord>,
) {
    let started = Instant::now();
    let mut visible = visible_terrain_bricks(volume, center_m);
    visible.sort_by(|a, b| {
        let distance = |c: &BrickCoord| {
            [c.x, c.y, c.z]
                .into_iter()
                .zip(center_m)
                .map(|(v, eye)| ((v as f64 + 0.5) * 8.0 - eye).powi(2))
                .sum::<f64>()
        };
        distance(a)
            .total_cmp(&distance(b))
            .then(a.sort_key().cmp(&b.sort_key()))
    });
    let world = VolumeMeshWorld(volume);
    // Each chunk's level of detail from its distance to the camera, and from
    // those which of its sides meet a chunk of another level.
    let mut flips_left = TERRAIN_LOD_FLIPS_PER_PASS;
    let lods: HashMap<BrickCoord, u8> = visible
        .iter()
        .map(|coord| {
            let distance = [coord.x, coord.y, coord.z]
                .into_iter()
                .zip(center_m)
                .map(|(v, eye)| ((v as f64 + 0.5) * 8.0 - eye).powi(2))
                .sum::<f64>()
                .sqrt();
            // A chunk being carved by a preview is always full detail.
            let lod = if volatile.contains(coord) {
                0
            } else {
                let cached = cache.0.get(coord);
                let wanted = terrain_lod(distance, cached.map(|c| c.lod));
                match cached {
                    // Changing the level of a chunk whose bricks are unchanged
                    // costs a rebuild and an upload; a walk crosses a shell of
                    // hundreds at once, so only so many change per pass, the
                    // nearest first, and the rest keep the level they have.
                    Some(c) if c.lod != wanted && c.mesh.token.is_fresh(&world) => {
                        if flips_left > 0 {
                            flips_left -= 1;
                            wanted
                        } else {
                            c.lod
                        }
                    }
                    _ => wanted,
                }
            };
            (*coord, lod)
        })
        .collect();
    let mut updates = Vec::new();
    let mut removed = Vec::new();
    // Updates that only change the detail of a chunk whose bricks did not.
    let mut detail_only = Vec::new();
    for coord in &visible {
        let is_volatile = volatile.contains(coord);
        let lod = lods[coord];
        let closed = closed_sides(*coord, lod, &lods);
        let only_detail = !is_volatile
            && cache
                .0
                .get(coord)
                .is_some_and(|cached| cached.mesh.token.is_fresh(&world));
        if !is_volatile
            && cache.0.get(coord).is_some_and(|cached| {
                cached.lod == lod && cached.closed == closed && cached.mesh.token.is_fresh(&world)
            })
        {
            continue;
        }
        let Ok(mesh) = spall_mesh::build_brick_mesh_lod(
            volume,
            *coord,
            lod_stride(lod),
            closed,
            Generation::START,
            TopologyEpoch::START,
        ) else {
            continue;
        };
        let (vertices, indices) = spall_render::to_gpu(&mesh.mesh, glam::Mat4::IDENTITY);
        if mesh.mesh.is_empty() {
            // A preview's empty mesh must clear whatever the GPU still holds.
            if cache
                .0
                .get(coord)
                .is_some_and(|old| !old.mesh.mesh.is_empty())
                || is_volatile
            {
                removed.push(*coord);
            }
            if is_volatile {
                cache.0.remove(coord);
            } else {
                cache.0.insert(*coord, CachedMesh { mesh, lod, closed });
            }
        } else {
            if is_volatile {
                cache.0.remove(coord);
            } else {
                cache.0.insert(*coord, CachedMesh { mesh, lod, closed });
            }
            if only_detail {
                detail_only.push(*coord);
            }
            updates.push((*coord, vertices, indices));
        }
        if updates.len() >= 4
            && !publish(
                std::mem::take(&mut updates),
                std::mem::take(&mut removed),
                std::mem::take(&mut detail_only),
            )
        {
            break;
        }
    }
    let visible: std::collections::HashSet<_> = visible.into_iter().collect();
    for coord in cache
        .0
        .keys()
        .copied()
        .filter(|coord| !visible.contains(coord))
        .collect::<Vec<_>>()
    {
        cache.0.remove(&coord);
        removed.push(coord);
    }
    updates.sort_by_key(|(coord, _, _)| coord.sort_key());
    removed.sort_by_key(|coord| coord.sort_key());
    tracing::debug!(target: "spall_client::terrain_mesh", rebuild_ms = started.elapsed().as_secs_f64()*1000.0, updated = updates.len(), removed = removed.len(), "rebuilt incremental greedy chunks");
    (updates, removed, detail_only)
}

/// One rebuild pass: terrain instances plus a sky occupancy grid (only when it
/// actually changed) from `replica`'s current terrain volume around `center_m`.
/// This is the legacy instanced-cube terrain path, kept for latency
/// instrumentation and as a reference; the window's worker draws terrain from
/// greedy meshes (see [`RebuildWorker`]). `sky_anchor` / `last_sky` carry state
/// across calls, same as the worker's loop locals. Returns `None` only when the
/// replica has no terrain yet.
pub fn rebuild_pass(
    replica: &Arc<Mutex<ReplicaWorld>>,
    center_m: [f64; 3],
    sky_anchor: &mut Option<[f64; 3]>,
    last_sky: &mut Option<spall_render::indirect::LightingVolume>,
    absent_is_open: bool,
) -> Option<RebuildOutcome> {
    let volume = replica
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .terrain_volume()
        .cloned()?;
    let start = Instant::now();
    let instances = build_instances(&volume, center_m);
    let (sky, sky_stats, _) = rebuild_sky(&volume, center_m, sky_anchor, last_sky, absent_is_open);
    Some(RebuildOutcome {
        center_m,
        terrain_included: true,
        terrain_complete: true,
        instances,
        mesh_updates: Vec::new(),
        mesh_removed: Vec::new(),
        mesh_detail_only: Vec::new(),
        mesh_elapsed: Duration::ZERO,
        emitters: None,
        sky,
        sky_stats,
        elapsed: start.elapsed(),
    })
}

/// The greedy terrain chunks the window draws around `center_m` and the
/// emissive cells (as unit cubes) that light it with point lights: exactly what
/// the rebuild worker produces on its first pass. For renders in tests and
/// measurements that must exercise the live terrain path.
pub fn live_terrain_for_render(
    volume: &Volume,
    center_m: [f64; 3],
    materials: &[spall_render::Material],
) -> (Vec<MeshUpdate<BrickCoord>>, Vec<Instance>) {
    let none = std::collections::HashSet::new();
    let (meshes, _) =
        update_terrain_mesh_cache(volume, center_m, &mut TerrainMeshCache::default(), &none);
    let emissive: Vec<bool> = materials.iter().map(|m| m.emissive > 0.0).collect();
    let emitters = EmitterCache::default().update(volume, center_m, &emissive, &none);
    (meshes, emitters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::VolumeId;

    #[test]
    fn terrain_lod_follows_distance_with_hysteresis_and_new_chunks_use_the_plain_edge() {
        // New chunks: the plain distances.
        assert_eq!(terrain_lod(10.0, None), 0);
        assert_eq!(terrain_lod(39.0, None), 0);
        assert_eq!(terrain_lod(41.0, None), 1);
        assert_eq!(terrain_lod(80.0, None), 2);
        // A chunk keeps its level through the hysteresis band either way.
        assert_eq!(terrain_lod(43.0, Some(0)), 0);
        assert_eq!(terrain_lod(47.0, Some(0)), 1);
        assert_eq!(terrain_lod(37.0, Some(1)), 1);
        assert_eq!(terrain_lod(33.0, Some(1)), 0);
        assert_eq!(terrain_lod(75.0, Some(1)), 1);
        assert_eq!(terrain_lod(79.0, Some(1)), 2);
        assert_eq!(terrain_lod(69.0, Some(2)), 2);
        assert_eq!(terrain_lod(65.0, Some(2)), 1);
        assert_eq!(lod_stride(0), 1);
        assert_eq!(lod_stride(1), 2);
        assert_eq!(lod_stride(2), 4);
    }

    #[test]
    fn sides_toward_a_chunk_of_another_level_are_closed() {
        let mut lods = HashMap::new();
        let at = BrickCoord::new(5, 2, 5);
        lods.insert(at, 1u8);
        lods.insert(BrickCoord::new(6, 2, 5), 0); // +x: finer
        lods.insert(BrickCoord::new(4, 2, 5), 1); // -x: same
        lods.insert(BrickCoord::new(5, 3, 5), 2); // +y: coarser
        // -y, +z and -z have no chunk in the set: not closed.
        assert_eq!(closed_sides(at, 1, &lods), 0b001010);
        // Every chunk of one level closes nothing.
        let same: HashMap<_, _> = [at, BrickCoord::new(6, 2, 5)]
            .into_iter()
            .map(|c| (c, 1u8))
            .collect();
        assert_eq!(closed_sides(at, 1, &same), 0);
    }

    #[test]
    fn progressive_mesh_batches_cover_the_same_world_and_start_nearby() {
        let mut volume = Volume::new(
            spall_core::VolumeId::new(1).unwrap(),
            spall_core::CellSizeCode::Quarter,
        );
        let mut edit = spall_voxel::EditPlan::new(volume.id());
        // A small block in each of nine chunks (a lone cell would vanish from a
        // coarse mesh, as the farther ones are).
        for x in 0..9 {
            for d in 0..3 {
                for e in 0..3 {
                    for f in 0..3 {
                        edit.set(
                            GlobalCell::new(x * 32 + 16 + d, 16 + e, 16 + f),
                            MaterialId(1),
                        );
                    }
                }
            }
        }
        volume.apply_edit(&edit).unwrap();
        let mut cache = TerrainMeshCache::default();
        let mut published = Vec::new();
        let (remaining, _, _) = update_terrain_mesh_cache_progressive(
            &volume,
            [4.; 3],
            &mut cache,
            &Default::default(),
            |updates, _, _| {
                published.push(updates);
                true
            },
        );
        assert_eq!(published[0].len(), 4);
        assert_eq!(
            published[0].iter().map(|u| u.0.x).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        let mut all: Vec<_> = published
            .into_iter()
            .flatten()
            .chain(remaining)
            .map(|u| u.0)
            .collect();
        all.sort_by_key(|c| c.sort_key());
        all.dedup();
        assert_eq!(all.len(), 9);
        let (warm, removed) =
            update_terrain_mesh_cache(&volume, [4.; 3], &mut cache, &Default::default());
        assert!(warm.is_empty() && removed.is_empty());
        assert!(parse_shots("move:10,0,-1;move:10,0,1").is_ok());
        assert!(parse_shots("move:NaN,0,1").is_err());
    }

    #[test]
    fn a_walk_changes_only_so_many_chunk_levels_per_pass_and_settles() {
        let mut volume = Volume::new(
            spall_core::VolumeId::new(1).unwrap(),
            spall_core::CellSizeCode::Quarter,
        );
        let mut edit = spall_voxel::EditPlan::new(volume.id());
        for x in 0..40 {
            for d in 0..3 {
                for e in 0..3 {
                    for f in 0..3 {
                        edit.set(
                            GlobalCell::new(x * 32 + 16 + d, 16 + e, 16 + f),
                            MaterialId(1),
                        );
                    }
                }
            }
        }
        volume.apply_edit(&edit).unwrap();
        let none = std::collections::HashSet::new();
        let mut cache = TerrainMeshCache::default();
        update_terrain_mesh_cache(&volume, [4.0, 4.0, 4.0], &mut cache, &none);
        let levels = |cache: &TerrainMeshCache| -> HashMap<BrickCoord, u8> {
            cache.0.iter().map(|(c, m)| (*c, m.lod)).collect()
        };
        let before = levels(&cache);
        assert!(before.values().any(|l| *l == 0) && before.values().any(|l| *l > 0));

        // The camera moves 150 m along the row: far more chunks than the budget
        // change level, but only the budget does in one pass...
        let far = [154.0, 4.0, 4.0];
        let mut detail = Vec::new();
        let (_, _, tail) = update_terrain_mesh_cache_progressive(
            &volume,
            far,
            &mut cache,
            &none,
            |_, _, published| {
                detail.extend(published);
                true
            },
        );
        detail.extend(tail);
        let after = levels(&cache);
        let changed = after
            .iter()
            .filter(|(c, l)| before.get(c) != Some(l))
            .count();
        assert!(
            changed > 0 && changed <= TERRAIN_LOD_FLIPS_PER_PASS,
            "{changed}"
        );
        // ...and those are detail-only updates (the bricks did not change).
        assert!(!detail.is_empty());

        // Passes go on until every chunk has the level its distance calls for.
        for _ in 0..20 {
            update_terrain_mesh_cache(&volume, far, &mut cache, &none);
        }
        let (quiet, removed) = update_terrain_mesh_cache(&volume, far, &mut cache, &none);
        assert!(quiet.is_empty() && removed.is_empty());
        for (coord, level) in levels(&cache) {
            let distance = ((coord.x as f64 + 0.5) * 8.0 - far[0]).hypot(
                ((coord.y as f64 + 0.5) * 8.0 - far[1])
                    .hypot((coord.z as f64 + 0.5) * 8.0 - far[2]),
            );
            assert_eq!(terrain_lod(distance, Some(level)), level, "{coord:?}");
        }
    }

    #[test]
    fn reports_incremental_mesh_and_edit_to_mesh_latency() {
        let mut volume = spall_mesh::fixtures::cube([0, 0, 0], 8);
        let center = [1.0, 1.0, 1.0];
        let mut cache = TerrainMeshCache::default();
        let started = Instant::now();
        let (initial, _) =
            update_terrain_mesh_cache(&volume, center, &mut cache, &Default::default());
        let initial_elapsed = started.elapsed();
        assert_eq!(initial.len(), 1);

        let mut edit = spall_voxel::EditPlan::new(volume.id());
        edit.set(GlobalCell::new(0, 0, 0), MaterialId::AIR);
        volume.apply_edit(&edit).expect("fixture edit should apply");
        let started = Instant::now();
        let (changed, _) =
            update_terrain_mesh_cache(&volume, center, &mut cache, &Default::default());
        let edit_to_mesh = started.elapsed();
        assert_eq!(changed.len(), 1, "only the edited brick should be rebuilt");
        eprintln!(
            "incremental_rebuild_ms={:.3} edit_to_mesh_ms={:.3} updated_bricks={}",
            initial_elapsed.as_secs_f64() * 1000.0,
            edit_to_mesh.as_secs_f64() * 1000.0,
            changed.len()
        );
    }

    /// The live terrain path: an unconfirmed swing re-meshes only its bricks, is
    /// never remembered by the mesh cache, and the real mesh returns once the
    /// preview is gone.
    #[test]
    fn pending_cut_remeshes_without_poisoning_the_mesh_cache() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let center = [2.0, 13.5, 2.0];
        let (cell, _) = visible_cells(&volume, center)
            .into_iter()
            .map(|(m, c)| (GlobalCell::new(c[0], c[1], c[2]), m))
            .next()
            .expect("the fixture has terrain");
        let none = std::collections::HashSet::new();
        let mut cache = TerrainMeshCache::default();
        let (initial, removed) = update_terrain_mesh_cache(&volume, center, &mut cache, &none);
        assert!(!initial.is_empty() && removed.is_empty());
        let (quiet, _) = update_terrain_mesh_cache(&volume, center, &mut cache, &none);
        assert!(quiet.is_empty(), "nothing changed, nothing re-meshed");

        let now = Instant::now();
        let mut preview = volume.clone();
        let mut pending = vec![PendingCut {
            cell,
            radius_cells: 1,
            at: now,
        }];
        let volatile = apply_pending_cuts(&mut preview, &mut pending, now);
        let (carved, _) = update_terrain_mesh_cache(&preview, center, &mut cache, &volatile);
        assert!(!carved.is_empty(), "the preview changes what is drawn");
        assert!(
            carved.len() <= volatile.len(),
            "only the carved bricks and their neighbours are re-meshed"
        );

        // The preview is gone: the real bricks come back, identical to before.
        let (restored, _) = update_terrain_mesh_cache(&volume, center, &mut cache, &none);
        assert!(
            !restored.is_empty(),
            "the real meshes replace the preview's"
        );
        let by_coord = |updates: &[MeshUpdate<BrickCoord>]| {
            updates
                .iter()
                .map(|(c, v, i)| (c.sort_key(), v.len(), i.clone()))
                .collect::<Vec<_>>()
        };
        let original: Vec<_> = by_coord(&initial)
            .into_iter()
            .filter(|(key, _, _)| by_coord(&restored).iter().any(|(k, _, _)| k == key))
            .collect();
        assert_eq!(by_coord(&restored), original);
        let (settled, _) = update_terrain_mesh_cache(&volume, center, &mut cache, &none);
        assert!(settled.is_empty());
    }
}
