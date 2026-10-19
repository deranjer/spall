//! Pure source reconstruction shared by the joint Simulation restore candidate.
//! Returned volumes retain saved IDs and counters; nothing is installed here.
use spall_core::{BrickCoord, CellSizeCode, CreatureKey, MaterialId, Revision};
use spall_protocol::{
    BaselineCells, CreatureOwnershipError, CreatureSnapshotV2, CreatureValidationCatalog,
};
use spall_voxel::{Brick, BrickBounds, RevisionResumeError, Volume};

/// Unpublished runtime source volumes for one validated portable owner.
pub struct ReconstructedCreatureSources {
    pub key: CreatureKey,
    pub parts: Vec<(u32, Volume)>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreatureSourceRestoreError {
    #[error(transparent)]
    Portable(#[from] CreatureOwnershipError),
    #[error("invalid source volume reconstruction: {0}")]
    Source(&'static str),
    #[error(transparent)]
    Revision(#[from] RevisionResumeError),
}

/// Validates the entire bounded population before copying any source cells.
/// This creates candidate volumes only: geometry namespace/counter agreement,
/// host/source ownership and physics still require a joint prepared installer.
pub fn reconstruct_creature_sources(
    snapshot: &CreatureSnapshotV2,
    catalog: &CreatureValidationCatalog<'_>,
) -> Result<Vec<ReconstructedCreatureSources>, CreatureSourceRestoreError> {
    snapshot.validate(catalog)?;
    let mut owners = Vec::with_capacity(snapshot.owners.records.len());
    for owner in &snapshot.owners.records {
        let mut parts = Vec::with_capacity(owner.parts.len());
        for part in &owner.parts {
            let cell_size = CellSizeCode::from_u8(part.cell_size_code)
                .ok_or(CreatureSourceRestoreError::Source("cell size"))?;
            let mut volume = match part.bounds {
                Some([min, max]) => Volume::bounded(
                    part.source_volume_id,
                    cell_size,
                    BrickBounds::new(
                        BrickCoord::new(min[0], min[1], min[2]),
                        BrickCoord::new(max[0], max[1], max[2]),
                    )
                    .ok_or(CreatureSourceRestoreError::Source("bounds"))?,
                ),
                None => Volume::new(part.source_volume_id, cell_size),
            };
            for source in &part.bricks {
                let revision = Revision(source.revision);
                let brick = match &source.cells {
                    BaselineCells::Uniform(material) => {
                        Brick::restored_uniform(MaterialId(*material), revision, source.edited)
                    }
                    BaselineCells::Dense(ids) => {
                        // Validation admitted exact dense sizes and cumulative
                        // work before this bounded per-brick scratch allocation.
                        let cells = ids.iter().copied().map(MaterialId).collect::<Vec<_>>();
                        Brick::restored(&cells, revision, source.edited)
                    }
                    BaselineCells::Digest { .. } => {
                        return Err(CreatureSourceRestoreError::Source("digest"));
                    }
                };
                volume
                    .insert_brick(
                        BrickCoord::new(source.coord[0], source.coord[1], source.coord[2]),
                        brick,
                    )
                    .map_err(|_| CreatureSourceRestoreError::Source("resident brick"))?;
            }
            volume.resume_revision_counter(Revision(part.next_brick_revision))?;
            parts.push((part.part_id, volume));
        }
        owners.push(ReconstructedCreatureSources {
            key: owner.state.key,
            parts,
        });
    }
    Ok(owners)
}
