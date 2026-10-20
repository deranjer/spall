//! Catalog-independent storage bounds. Installation still requires game validation.
use crate::{Checkpoint, DtoError, StoredBody, StoredBrick, StoredCreaturesV1, StoredWorldMeta};
pub const MAX_STORED_CREATURE_BYTES: usize =
    spall_protocol::creature_snapshot::MAX_CREATURE_SNAPSHOT_BYTES + 64;

pub fn validate_section(
    section: Option<&StoredCreaturesV1>,
    meta: &StoredWorldMeta,
    bodies: &[StoredBody],
    bricks: &[StoredBrick],
) -> Result<(), DtoError> {
    let Some(section) = section else {
        return Ok(());
    };
    let invalid =
        || DtoError::Creature("schema, hash, world, counter or geometry namespace".into());
    section
        .snapshot
        .validate_resources()
        .map_err(|e| DtoError::Creature(e.to_string()))?;
    let geometry: std::collections::BTreeSet<_> = bodies
        .iter()
        .map(|body| body.volume_id)
        .chain(bricks.iter().map(|brick| brick.volume_id))
        .collect();
    if section.physics_origin_m.iter().any(|v| !v.is_finite())
        || section.schema != spall_core::STORE_SCHEMA_VERSION_U16
        || section.snapshot.allocator.world_id.to_u128() != meta.world_id
        || section.snapshot.next_volume_id != meta.next_volume
        || section.snapshot.canonical_hash().0 != section.snapshot_hash
        || section
            .snapshot
            .owners
            .records
            .iter()
            .flat_map(|r| &r.parts)
            .any(|part| {
                geometry.contains(&part.source_volume_id.get())
                    || !meta.cell_size_codes.contains(&part.cell_size_code)
                    || part.source_content_hash != part.canonical_source_content_hash()
            })
    {
        return Err(invalid());
    }
    Ok(())
}

pub fn encode_checkpoint_section(checkpoint: &Checkpoint) -> Result<Vec<u8>, DtoError> {
    validate_section(
        checkpoint.creatures.as_ref(),
        &checkpoint.meta,
        &checkpoint.bodies,
        &checkpoint.bricks,
    )?;
    let bytes = crate::dto::encode(&checkpoint.creatures)?;
    if bytes.len() > MAX_STORED_CREATURE_BYTES {
        return Err(DtoError::Creature("byte limit".into()));
    }
    Ok(bytes)
}

/// Strict frame and cumulative nested reader admission before materialization.
pub fn decode_section(bytes: &[u8]) -> Result<Option<StoredCreaturesV1>, DtoError> {
    if bytes.len() > MAX_STORED_CREATURE_BYTES {
        return Err(DtoError::Creature("byte limit".into()));
    }
    let (section, rest): (Option<StoredCreaturesV1>, &[u8]) = postcard::take_from_bytes(bytes)?;
    if !rest.is_empty() {
        return Err(DtoError::Creature("trailing bytes".into()));
    }
    Ok(section)
}
