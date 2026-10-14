//! Saved form of the organisms (living state version 3).
//!
//! A tree's skeleton (`wood`, `parents`) is a pure function of its root, its
//! species and its id, so it is not saved: only which cells were removed is.
//! A 1 km world with twenty thousand trees would otherwise save hundreds of
//! thousands of skeleton cells at every checkpoint.
//!
//! `skeleton()` is therefore part of the format. Changing the shape a species
//! grows requires a new state version and an upgrade that keeps the old shape
//! for the trees already saved.
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::living::Organism;

#[derive(Serialize, Deserialize)]
struct Wire {
    id: u64,
    species: u16,
    root: [i64; 3],
    age_ms: u64,
    last_update_ms: u64,
    last_seed_ms: u64,
    alive: bool,
    biomass: u16,
    /// Indices of removed skeleton cells.
    removed: Vec<u16>,
    committed: u16,
}

// `serde(with)` hands the field over as `&Vec<_>`.
#[allow(clippy::ptr_arg)]
pub(crate) fn serialize<S: Serializer>(
    organisms: &Vec<Organism>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(organisms.iter().map(|p| {
        Wire {
            id: p.id,
            species: p.species,
            root: p.root,
            age_ms: p.age_ms,
            last_update_ms: p.last_update_ms,
            last_seed_ms: p.last_seed_ms,
            alive: p.alive,
            biomass: p.biomass,
            removed: p
                .removed
                .iter()
                .enumerate()
                .filter(|(_, removed)| **removed)
                .map(|(i, _)| i as u16)
                .collect(),
            committed: p.committed,
        }
    }))
}

/// Organisms without their skeletons: `wood` and `parents` are empty and
/// `removed` is as long as its highest set index. [`crate::living::LivingState`]
/// grows the skeletons back before anything uses the state.
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Organism>, D::Error> {
    let wire = Vec::<Wire>::deserialize(deserializer)?;
    Ok(wire
        .into_iter()
        .map(|w| {
            let mut removed = Vec::new();
            for i in w.removed {
                let i = usize::from(i);
                if removed.len() <= i {
                    removed.resize(i + 1, false);
                }
                removed[i] = true;
            }
            Organism {
                id: w.id,
                species: w.species,
                root: w.root,
                age_ms: w.age_ms,
                last_update_ms: w.last_update_ms,
                last_seed_ms: w.last_seed_ms,
                alive: w.alive,
                biomass: w.biomass,
                wood: Vec::new().into(),
                parents: Vec::new().into(),
                removed: std::sync::Arc::new(removed),
                committed: w.committed,
            }
        })
        .collect())
}
