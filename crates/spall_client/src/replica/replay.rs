//! Transaction replay and baseline helpers (moved out of `replica.rs`).
use super::*;

pub(super) fn topology_op_touches_volume(op: &TopologyOp, volume: VolumeId) -> bool {
    match op {
        TopologyOp::IntegerBrush { volume: v, .. } | TopologyOp::CellRun { volume: v, .. } => {
            *v == volume
        }
        TopologyOp::SplitOff { source, child, .. }
        | TopologyOp::SplitOffBaseline { source, child, .. }
        | TopologyOp::SplitOffBulkBaseline { source, child, .. } => {
            *source == volume || *child == volume
        }
        TopologyOp::SourcePatchBaseline { source, .. }
        | TopologyOp::SourcePatchBulkBaseline { source, .. } => *source == volume,
    }
}

/// A batch of same-volume writes not yet applied.
pub(super) struct PendingGroup {
    volume: VolumeId,
    /// `Some(entity)` if this group builds a brand-new child volume.
    new_child: Option<EntityId>,
    writes: Vec<(GlobalCell, MaterialId)>,
}

/// Replays `ops` into `candidate`. Consecutive same-volume writes are batched
/// into a single `apply_edit`, and a `SplitOff` group is built exactly like
/// `spall_sim::transfer::build_child_volume` (bounded to the fill bricks, every
/// brick in that box resident at revision 1) so brick revisions — and therefore
/// the canonical hash — match the server.
pub(super) fn replay_ops(
    ops: &[TopologyOp],
    candidate: &mut BTreeMap<u64, Volume>,
    new_owner: &mut Vec<(VolumeId, EntityId)>,
    cell_size: CellSizeCode,
    bulk: Option<&spall_protocol::baseline::BaselineWorld>,
    newer_only: Option<&BTreeMap<u64, spall_voxel::EvictedBricks>>,
) -> Result<(), String> {
    // T17 increment 2: the out-of-band `BaselineVolume` for one volume of a
    // giant bulk split.
    let bulk_volume = |id: VolumeId| -> Result<&BaselineVolume, String> {
        bulk.and_then(|w| w.volumes.iter().find(|v| v.volume_id == id))
            .ok_or_else(|| format!("bulk split baseline is missing volume {id}"))
    };
    let mut pending: Option<PendingGroup> = None;

    for op in ops {
        match op {
            TopologyOp::IntegerBrush {
                volume,
                brush,
                material,
            } => {
                flush(pending.take(), candidate, cell_size)?;
                let plan = EditPlan::sphere(*volume, *brush, *material);
                candidate
                    .get_mut(&volume.get())
                    .ok_or_else(|| format!("brush targets unknown volume {volume}"))?
                    .apply_edit(&plan)
                    .map_err(|e| format!("brush replay failed: {e}"))?;
            }
            TopologyOp::SplitOff {
                child,
                child_entity,
                ..
            } => {
                flush(pending.take(), candidate, cell_size)?;
                new_owner.push((*child, *child_entity));
                pending = Some(PendingGroup {
                    volume: *child,
                    new_child: Some(*child_entity),
                    writes: Vec::new(),
                });
            }
            TopologyOp::CellRun {
                volume,
                start,
                len,
                material,
            } => {
                if pending.as_ref().map(|g| g.volume) != Some(*volume) {
                    flush(pending.take(), candidate, cell_size)?;
                    pending = Some(PendingGroup {
                        volume: *volume,
                        new_child: None,
                        writes: Vec::new(),
                    });
                }
                let group = pending.as_mut().expect("just set");
                let last_x = start
                    .x
                    .checked_add(i64::from(*len).saturating_sub(1))
                    .ok_or("cell run overflows i64")?;
                for x in start.x..=last_x {
                    group
                        .writes
                        .push((GlobalCell::new(x, start.y, start.z), *material));
                }
            }
            // T17: an oversized split's child geometry arrives as a compressed
            // `BaselineVolume` — inline (increment 1) or from the out-of-band
            // bulk world (increment 2). Rebuild the child volume from it — same
            // shape (bounded, authoritative revisions) as the `SplitOff` path.
            TopologyOp::SplitOffBaseline {
                child,
                child_entity,
                blob,
                ..
            } => {
                flush(pending.take(), candidate, cell_size)?;
                let bv = BaselineVolume::decode_compressed(blob)
                    .map_err(|e| format!("split baseline blob for volume {child}: {e}"))?;
                install_split_child(candidate, new_owner, *child, *child_entity, &bv)?;
            }
            TopologyOp::SplitOffBulkBaseline {
                child,
                child_entity,
                ..
            } => {
                flush(pending.take(), candidate, cell_size)?;
                let bv = bulk_volume(*child)?;
                install_split_child(candidate, new_owner, *child, *child_entity, bv)?;
            }
            // T17: the source side of an oversized split — overwrite the named
            // source bricks in the candidate with their post-cut state.
            TopologyOp::SourcePatchBaseline { source, blob } => {
                flush(pending.take(), candidate, cell_size)?;
                let bv = BaselineVolume::decode_compressed(blob)
                    .map_err(|e| format!("source patch baseline blob for volume {source}: {e}"))?;
                patch_split_source(candidate, *source, &bv, newer_only)?;
            }
            TopologyOp::SourcePatchBulkBaseline { source, .. } => {
                flush(pending.take(), candidate, cell_size)?;
                let bv = bulk_volume(*source)?;
                patch_split_source(candidate, *source, bv, newer_only)?;
            }
        }
    }
    flush(pending.take(), candidate, cell_size)
}

/// The volumes a transaction's split ops create.
pub(super) fn split_child_ids(ops: &[TopologyOp]) -> Vec<u64> {
    ops.iter()
        .filter_map(|op| match op {
            TopologyOp::SplitOff { child, .. }
            | TopologyOp::SplitOffBaseline { child, .. }
            | TopologyOp::SplitOffBulkBaseline { child, .. } => Some(child.get()),
            _ => None,
        })
        .collect()
}

/// The ops that build a transaction's new child volumes, plus the source-side
/// baseline patches (applied only where newer): the split markers, the fill runs
/// addressed to a child, and the patches that rewrite the source bricks beyond
/// the transaction's `before` set. A child's cells come from the transaction
/// itself, never from the source volume's current state. Inline brush and
/// source-removal runs are left out: their bricks are in `before` and already
/// past this transaction.
pub(super) fn split_child_ops(ops: &[TopologyOp]) -> Vec<TopologyOp> {
    let children = split_child_ids(ops);
    ops.iter()
        .filter(|op| match op {
            TopologyOp::SplitOff { .. }
            | TopologyOp::SplitOffBaseline { .. }
            | TopologyOp::SplitOffBulkBaseline { .. }
            | TopologyOp::SourcePatchBaseline { .. }
            | TopologyOp::SourcePatchBulkBaseline { .. } => true,
            TopologyOp::CellRun { volume, .. } => children.contains(&volume.get()),
            _ => false,
        })
        .cloned()
        .collect()
}

/// Insert a split child `Volume` (rebuilt from a `BaselineVolume`) into the
/// candidate and register its new owner.
pub(super) fn install_split_child(
    candidate: &mut BTreeMap<u64, Volume>,
    new_owner: &mut Vec<(VolumeId, EntityId)>,
    child: VolumeId,
    child_entity: EntityId,
    bv: &BaselineVolume,
) -> Result<(), String> {
    let child_volume = volume_from_baseline_volume(bv)?;
    new_owner.push((child, child_entity));
    candidate.insert(child.get(), child_volume);
    Ok(())
}

/// Overwrite a split source's affected bricks in the candidate from a
/// `BaselineVolume`.
///
/// With `newer_only`, a brick is replaced only when the patch's revision is
/// newer than what the replica holds (resident, or retained as a digest). Used
/// when an earlier repair patch already carried some bricks to the server's
/// current state, which must not be rolled back.
pub(super) fn patch_split_source(
    candidate: &mut BTreeMap<u64, Volume>,
    source: VolumeId,
    bv: &BaselineVolume,
    newer_only: Option<&BTreeMap<u64, spall_voxel::EvictedBricks>>,
) -> Result<(), String> {
    let volume = candidate
        .get_mut(&source.get())
        .ok_or_else(|| format!("source patch targets unknown volume {source}"))?;
    for bb in &bv.bricks {
        let coord = BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]);
        if let Some(evicted) = newer_only {
            let held = volume.brick_revision(coord).ok().flatten().or_else(|| {
                evicted
                    .get(&source.get())
                    .and_then(|e| e.get(coord))
                    .map(|d| d.revision)
            });
            if held.is_some_and(|rev| rev >= Revision(bb.revision)) {
                continue;
            }
        }
        volume
            .insert_brick(
                BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                baseline_brick(bb)?,
            )
            .map_err(|e| format!("source patch brick insert failed: {e}"))?;
    }
    Ok(())
}

pub(super) fn flush(
    group: Option<PendingGroup>,
    candidate: &mut BTreeMap<u64, Volume>,
    cell_size: CellSizeCode,
) -> Result<(), String> {
    let Some(group) = group else { return Ok(()) };
    if group.writes.is_empty() {
        return if group.new_child.is_some() {
            Err("split-off child received no cells".into())
        } else {
            Ok(())
        };
    }

    if group.new_child.is_some() {
        // Build the child volume the way the server does.
        let mut min = BrickCoord::new(i64::MAX, i64::MAX, i64::MAX);
        let mut max = BrickCoord::new(i64::MIN, i64::MIN, i64::MIN);
        for (cell, _) in &group.writes {
            let b = cell.split().0;
            min = BrickCoord::new(min.x.min(b.x), min.y.min(b.y), min.z.min(b.z));
            max = BrickCoord::new(max.x.max(b.x), max.y.max(b.y), max.z.max(b.z));
        }
        let bounds =
            spall_voxel::BrickBounds::new(min, max).ok_or("child brick bounds are inverted")?;
        let mut child = Volume::bounded(group.volume, cell_size, bounds);
        for bz in min.z..=max.z {
            for by in min.y..=max.y {
                for bx in min.x..=max.x {
                    child
                        .insert_brick(
                            BrickCoord::new(bx, by, bz),
                            Brick::uniform(MaterialId::AIR, Revision(1)),
                        )
                        .map_err(|e| format!("child brick insert failed: {e}"))?;
                }
            }
        }
        apply_writes(&mut child, group.volume, &group.writes)?;
        candidate.insert(group.volume.get(), child);
    } else {
        let volume = candidate
            .get_mut(&group.volume.get())
            .ok_or_else(|| format!("cell run targets unknown volume {}", group.volume))?;
        apply_writes(volume, group.volume, &group.writes)?;
    }
    Ok(())
}

pub(super) fn apply_writes(
    volume: &mut Volume,
    vid: VolumeId,
    writes: &[(GlobalCell, MaterialId)],
) -> Result<(), String> {
    let mut plan = EditPlan::new(vid);
    for (cell, material) in writes {
        plan.set(*cell, *material);
    }
    volume
        .apply_edit(&plan)
        .map(|_| ())
        .map_err(|e| format!("cell run replay failed: {e}"))
}

/// One [`BaselineBrick`] as a `spall_voxel::Brick` at its authoritative revision.
/// Shared by the late-join install, the hash-repair patch, and the T17
/// `SplitOffBaseline` / `SourcePatchBaseline` op replay.
pub(super) fn baseline_brick(bb: &BaselineBrick) -> Result<Brick, String> {
    if let BaselineCells::Uniform(id) = &bb.cells {
        return Ok(Brick::restored_uniform(
            MaterialId(*id),
            Revision(bb.revision),
            bb.edited,
        ));
    }
    let cells: Vec<MaterialId> = match &bb.cells {
        BaselineCells::Digest { .. } => {
            return Err("digest requires staged regional baseline".into());
        }
        BaselineCells::Uniform(id) => vec![MaterialId(*id); spall_core::CELLS_PER_BRICK],
        BaselineCells::Dense(raw) => {
            if raw.len() != spall_core::CELLS_PER_BRICK {
                return Err(format!(
                    "baseline brick has {} cells, expected {}",
                    raw.len(),
                    spall_core::CELLS_PER_BRICK
                ));
            }
            raw.iter().copied().map(MaterialId).collect()
        }
    };
    Ok(Brick::restored(&cells, Revision(bb.revision), bb.edited))
}

/// Rebuild a whole `Volume` from a decoded [`BaselineVolume`] — the T17
/// `SplitOffBaseline` child. Bounded to the recorded brick bounds, every brick
/// at its authoritative revision, so the canonical hash matches the server.
pub(super) fn volume_from_baseline_volume(bv: &BaselineVolume) -> Result<Volume, String> {
    let cs = CellSizeCode::from_u8(bv.cell_size_code).ok_or_else(|| {
        format!(
            "split baseline unknown cell-size code {}",
            bv.cell_size_code
        )
    })?;
    let mut volume = match bv.bounds {
        Some([mn, mx]) => {
            let bb = spall_voxel::BrickBounds::new(
                BrickCoord::new(mn[0], mn[1], mn[2]),
                BrickCoord::new(mx[0], mx[1], mx[2]),
            )
            .ok_or("split baseline volume has inverted bounds")?;
            Volume::bounded(bv.volume_id, cs, bb)
        }
        None => Volume::new(bv.volume_id, cs),
    };
    for bb in &bv.bricks {
        volume
            .insert_brick(
                BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                baseline_brick(bb)?,
            )
            .map_err(|e| format!("split baseline brick insert failed: {e}"))?;
    }
    Ok(volume)
}

// --- canonical form (mirrors spall_sim::world canonical_volume adapters) ---

/// A shared empty digest set for [`ReplicaWorld::evicted`].
pub(super) fn empty_evicted() -> &'static spall_voxel::EvictedBricks {
    pub(super) static EMPTY: std::sync::OnceLock<spall_voxel::EvictedBricks> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(spall_voxel::EvictedBricks::new)
}

pub(super) fn solid_cells(v: &Volume) -> u64 {
    let mut n = 0;
    for coord in v.resident_brick_coords() {
        let Some(snap) = v.snapshot_brick(coord).ok().flatten() else {
            continue;
        };
        for i in 0..spall_core::CELLS_PER_BRICK as u16 {
            let lc = spall_core::LocalCell::from_linear_index(i).expect("index < 32768");
            if !snap.get(lc).is_air() {
                n += 1;
            }
        }
    }
    n
}

pub(super) fn latest_revision(v: &Volume) -> Revision {
    Revision(v.next_revision().get().saturating_sub(1))
}

/// The centroid (body-local metres, same frame as the mesh) of every solid
/// cell of a body: an estimate of its centre of mass. A body's local frame is
/// the world cell grid, so its pose origin can be 100 m from its geometry and
/// motion applied to the origin is amplified by that lever arm; poses are
/// interpolated and extrapolated about this point instead. Zero when empty.
pub(crate) fn body_pivot_m(volume: &Volume) -> [f64; 3] {
    use spall_core::{BRICK_EDGE, LocalCell};
    let cell_m = volume.cell_size().metres();
    let edge = i64::from(BRICK_EDGE);
    let (mut sum, mut count) = ([0.0_f64; 3], 0_u64);
    for coord in volume.resident_brick_coords() {
        let Ok(Some(snap)) = volume.snapshot_brick(coord) else {
            continue;
        };
        for z in 0..BRICK_EDGE {
            for y in 0..BRICK_EDGE {
                for x in 0..BRICK_EDGE {
                    let local = LocalCell::new(x as u8, y as u8, z as u8).expect("in-brick");
                    if snap.get(local) == spall_core::MaterialId::AIR {
                        continue;
                    }
                    sum[0] += (coord.x * edge + i64::from(x)) as f64 + 0.5;
                    sum[1] += (coord.y * edge + i64::from(y)) as f64 + 0.5;
                    sum[2] += (coord.z * edge + i64::from(z)) as f64 + 0.5;
                    count += 1;
                }
            }
        }
    }
    if count == 0 {
        return [0.0; 3];
    }
    sum.map(|s| s / count as f64 * cell_m)
}

/// `rotation` applied to the body-local vector `v`.
pub(super) fn rotate_local(rotation: &spall_core::QuantizedQuat, v: [f64; 3]) -> [f64; 3] {
    let r = rotation
        .to_unit()
        .map_or(glam::DQuat::IDENTITY, |[x, y, z, w]| {
            glam::DQuat::from_xyzw(f64::from(x), f64::from(y), f64::from(z), f64::from(w))
        });
    (r * glam::DVec3::from_array(v)).to_array()
}

/// World position of the body-local point `pivot_m` under `pose`.
pub(crate) fn pivot_world(pose: &Pose, pivot_m: [f64; 3]) -> [f64; 3] {
    let r = rotate_local(&pose.rotation, pivot_m);
    [
        pose.translation_m[0] + r[0],
        pose.translation_m[1] + r[1],
        pose.translation_m[2] + r[2],
    ]
}

/// `template` with `rotation`, translated so the body-local point `pivot_m`
/// sits at the world position `pivot_pos`.
pub(super) fn pose_with_pivot_at(
    pivot_pos: [f64; 3],
    rotation: spall_core::QuantizedQuat,
    pivot_m: [f64; 3],
    template: &Pose,
) -> Pose {
    let r = rotate_local(&rotation, pivot_m);
    let mut out = *template;
    out.rotation = rotation;
    for i in 0..3 {
        out.translation_m[i] = pivot_pos[i] - r[i];
    }
    out
}

/// [`lerp_pose`] moving the body-local point `pivot_m` in a straight line (and
/// the orientation by shortest-arc nlerp) instead of the origin. With a zero
/// pivot this is exactly [`lerp_pose`].
pub(super) fn lerp_pose_about(a: &Pose, b: &Pose, t: f64, pivot_m: [f64; 3]) -> Pose {
    if pivot_m == [0.0; 3] {
        return lerp_pose(a, b, t);
    }
    let (pa, pb) = (pivot_world(a, pivot_m), pivot_world(b, pivot_m));
    let rotation = nlerp_rotation(&a.rotation, &b.rotation, t).unwrap_or(b.rotation);
    let mut pos = [0.0; 3];
    for i in 0..3 {
        pos[i] = pa[i] + (pb[i] - pa[i]) * t;
    }
    pose_with_pivot_at(pos, rotation, pivot_m, b)
}

pub(super) fn lerp_pose(a: &Pose, b: &Pose, t: f64) -> Pose {
    let mut out = *b;
    for i in 0..3 {
        out.translation_m[i] = a.translation_m[i] + (b.translation_m[i] - a.translation_m[i]) * t;
    }
    out.rotation = nlerp_rotation(&a.rotation, &b.rotation, t).unwrap_or(b.rotation);
    out
}

/// Shortest-arc normalised lerp between two orientations. `None` if either is
/// degenerate (the caller keeps the newer one).
pub(super) fn nlerp_rotation(
    a: &spall_core::QuantizedQuat,
    b: &spall_core::QuantizedQuat,
    t: f64,
) -> Option<spall_core::QuantizedQuat> {
    let [ax, ay, az, aw] = a.to_unit().ok()?;
    let [bx, by, bz, bw] = b.to_unit().ok()?;
    let qa = glam::Quat::from_xyzw(ax, ay, az, aw);
    let mut qb = glam::Quat::from_xyzw(bx, by, bz, bw);
    if qa.dot(qb) < 0.0 {
        qb = -qb;
    }
    let q = qa.lerp(qb, t as f32).normalize();
    spall_core::QuantizedQuat::from_unit(q.x, q.y, q.z, q.w).ok()
}

/// `pose` advanced by `ticks` server ticks (either sign) along constant linear
/// and angular velocity; a sleeping body does not move. No contact response:
/// callers bound `ticks` themselves.
///
/// The reported linear velocity is the centre of mass's, so it moves the
/// body-local point `pivot_m` (an estimate of the centre of mass) and the
/// orientation turns about that point. A body's frame is the world cell grid,
/// so its origin can be 100 m from its geometry; advancing the origin instead
/// is wrong by `angular_velocity x lever_arm`. A zero pivot is the origin model.
pub(crate) fn advance_pose_about(
    pose: &Pose,
    linear_velocity_m_s: [f32; 3],
    angular_velocity_rad_s: [f32; 3],
    sleeping: bool,
    ticks: f64,
    server_tick_hz: f64,
    pivot_m: [f64; 3],
) -> Pose {
    if sleeping {
        return *pose;
    }
    let dt = ticks / server_tick_hz;
    let mut pivot_pos = pivot_world(pose, pivot_m);
    for (t, v) in pivot_pos.iter_mut().zip(linear_velocity_m_s) {
        *t += f64::from(v) * dt;
    }
    let mut rotation = pose.rotation;
    let w = glam::Vec3::from_array(angular_velocity_rad_s);
    if w.length_squared() > 1.0e-12
        && let Ok([x, y, z, wq]) = pose.rotation.to_unit()
    {
        let q = (glam::Quat::from_scaled_axis(w * dt as f32) * glam::Quat::from_xyzw(x, y, z, wq))
            .normalize();
        if let Ok(turned) = spall_core::QuantizedQuat::from_unit(q.x, q.y, q.z, q.w) {
            rotation = turned;
        }
    }
    pose_with_pivot_at(pivot_pos, rotation, pivot_m, pose)
}
