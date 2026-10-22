//! A baseline being built off to the side (moved out of `replica.rs`).
use super::*;

/// A baseline being built off to the side while its segments arrive (`docs/protocol.md` late-join
/// step 3). Nothing here is visible to prediction or rendering; [`ReplicaWorld::install_staged`]
/// swaps it in atomically once the transfer is verified complete. Its memory is the world itself
/// (class D in `docs/reports/large-world-baseline-design.md`): the caller budgets it before the
/// first brick is staged.
pub struct StagedBaseline {
    pub(super) reuse: BTreeMap<u64, Volume>,
    pub(super) evicted: BTreeMap<u64, spall_voxel::EvictedBricks>,
    pub(super) checkpoint_tick: u64,
    pub(super) volumes: BTreeMap<u64, Volume>,
    pub(super) owner: BTreeMap<u64, CanonicalOwner>,
    pub(super) bodies: BTreeMap<u64, ReplicaBody>,
    pub(super) volume_of_entity: BTreeMap<u64, u64>,
    pub(super) terrain_id: Option<VolumeId>,
    /// The volume currently open (header seen, `last` not yet).
    pub(super) open: Option<u64>,
    pub(super) bricks: u64,
}

impl StagedBaseline {
    pub fn new(checkpoint_tick: u64) -> Self {
        Self {
            reuse: BTreeMap::new(),
            evicted: BTreeMap::new(),
            checkpoint_tick,
            volumes: BTreeMap::new(),
            owner: BTreeMap::new(),
            bodies: BTreeMap::new(),
            volume_of_entity: BTreeMap::new(),
            terrain_id: None,
            open: None,
            bricks: 0,
        }
    }

    pub(crate) fn set_reuse(&mut self, volumes: BTreeMap<u64, Volume>) {
        self.reuse = volumes;
    }

    /// Bricks staged so far.
    pub fn brick_count(&self) -> u64 {
        self.bricks
    }

    /// Volumes staged so far.
    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    /// Bodies staged so far (every non-terrain volume).
    pub fn body_count(&self) -> usize {
        self.bodies.len()
    }

    /// Opens a volume from its header. It must not already exist and no other volume may be open.
    pub fn open_volume(
        &mut self,
        vid: VolumeId,
        header: &spall_protocol::segment::VolumeHeader,
    ) -> Result<(), String> {
        use spall_protocol::BaselineOwner;
        if let Some(open) = self.open {
            return Err(format!("volume {open} still open when {vid} began"));
        }
        if self.volumes.contains_key(&vid.get()) {
            return Err(format!("baseline volume {vid} appears twice"));
        }
        let cs = CellSizeCode::from_u8(header.cell_size_code).ok_or_else(|| {
            format!(
                "baseline volume {vid} has unknown cell-size code {}",
                header.cell_size_code
            )
        })?;
        let volume = match header.bounds {
            Some([mn, mx]) => {
                let bb = spall_voxel::BrickBounds::new(
                    BrickCoord::new(mn[0], mn[1], mn[2]),
                    BrickCoord::new(mx[0], mx[1], mx[2]),
                )
                .ok_or_else(|| format!("baseline volume {vid} has inverted bounds"))?;
                Volume::bounded(vid, cs, bb)
            }
            None => Volume::new(vid, cs),
        };
        match header.owner {
            BaselineOwner::Terrain => {
                if self.terrain_id.is_some() {
                    return Err("baseline has a second terrain volume".to_string());
                }
                self.owner.insert(vid.get(), CanonicalOwner::Terrain);
                self.terrain_id = Some(vid);
            }
            BaselineOwner::Body(entity) => {
                if self.volume_of_entity.contains_key(&entity.get()) {
                    return Err(format!("baseline entity {entity} owns two volumes"));
                }
                self.owner.insert(vid.get(), CanonicalOwner::Body(entity));
                self.volume_of_entity.insert(entity.get(), vid.get());
                self.bodies.insert(
                    entity.get(),
                    ReplicaBody {
                        entity,
                        volume_id: vid,
                        track: MotionTrack::default(),
                    },
                );
            }
        }
        self.volumes.insert(vid.get(), volume);
        self.open = Some(vid.get());
        Ok(())
    }

    /// Stages one brick into the open volume `vid`.
    pub fn insert_brick(
        &mut self,
        vid: VolumeId,
        bb: &spall_protocol::BaselineBrick,
    ) -> Result<(), String> {
        if self.open != Some(vid.get()) {
            return Err(format!(
                "brick for volume {vid}, which is not the open volume"
            ));
        }
        if let spall_protocol::BaselineCells::Digest {
            content_hash,
            solid_cells,
        } = bb.cells
        {
            let coord = BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]);
            let volume = &self.volumes[&vid.get()];
            if self.terrain_id != Some(vid)
                || solid_cells > spall_core::CELLS_PER_BRICK as u32
                || volume
                    .bounds()
                    .is_some_and(|bounds| !bounds.contains(coord))
                || volume.brick_revision(coord).ok().flatten().is_some()
                || self
                    .evicted
                    .get(&vid.get())
                    .is_some_and(|e| e.contains(coord))
            {
                return Err("invalid, duplicate or non-terrain baseline digest".into());
            }
            self.evicted
                .entry(vid.get())
                .or_default()
                .record(
                    coord,
                    spall_voxel::BrickDigest {
                        revision: Revision(bb.revision),
                        content_hash: BrickHash::from_bytes(content_hash.0),
                        solid_cells,
                        modified_air: bb.edited && solid_cells == 0,
                    },
                )
                .map_err(|e| e.to_string())?;
            self.bricks += 1;
            return Ok(());
        }
        let coord = BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]);
        if self
            .evicted
            .get(&vid.get())
            .is_some_and(|e| e.contains(coord))
        {
            return Err("baseline geometry overlaps a retained digest".into());
        }
        let candidate = self.reuse.get(&vid.get()).and_then(|v| {
            v.snapshot_brick(BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]))
                .ok()
                .flatten()
        });
        let brick = match (&bb.cells, candidate) {
            (spall_protocol::BaselineCells::Dense(raw), Some(candidate))
                if raw.len() == spall_core::CELLS_PER_BRICK =>
            {
                let cells: Vec<_> = raw.iter().copied().map(MaterialId).collect();
                Brick::restored_reusing(&cells, Revision(bb.revision), bb.edited, &candidate)
            }
            _ => baseline_brick(bb)?,
        };
        self.volumes
            .get_mut(&vid.get())
            .expect("open volume is staged")
            .insert_brick(
                BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                brick,
            )
            .map_err(|e| format!("baseline brick insert into {vid} failed: {e}"))?;
        self.bricks += 1;
        Ok(())
    }

    /// Closes the open volume `vid`.
    pub fn close_volume(&mut self, vid: VolumeId) {
        if self.open == Some(vid.get()) {
            self.open = None;
        }
    }

    /// Stages one segment. Structural validation (order, contiguity, completeness) belongs to the
    /// `SequenceValidator`; this builds the volumes and fails on anything the voxel layer refuses
    /// (bounds, cell-size, duplicate coordinates).
    pub fn add_segment(
        &mut self,
        seg: &spall_protocol::segment::BaselineSegment,
    ) -> Result<(), String> {
        for v in &seg.volumes {
            if let Some(h) = &v.header {
                self.open_volume(v.volume_id, h)?;
            }
            for b in &v.bricks {
                self.insert_brick(v.volume_id, b)?;
            }
            if v.last {
                self.close_volume(v.volume_id);
            }
        }
        Ok(())
    }
}
