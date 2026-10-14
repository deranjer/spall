//! Saved form of the organisms (living state version 4).
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
    #[serde(default)]
    growth_ms: Option<u64>,
    #[serde(default)]
    seed_growth_ms: Option<u64>,
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
            growth_ms: Some(p.growth_ms),
            seed_growth_ms: Some(p.seed_growth_ms),
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
                growth_ms: w.growth_ms.unwrap_or(w.age_ms),
                seed_growth_ms: w.seed_growth_ms.unwrap_or(0),
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

/// Decoder for the version 3 organism wire, before active-growth credits were
/// stored. Kept separate so old continuation does not depend on serde defaults.
pub(crate) mod v3 {
    use crate::living::Organism;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    struct WireV3 {
        id: u64,
        species: u16,
        root: [i64; 3],
        age_ms: u64,
        last_update_ms: u64,
        last_seed_ms: u64,
        alive: bool,
        biomass: u16,
        removed: Vec<u16>,
        committed: u16,
    }

    #[allow(clippy::ptr_arg)]
    pub(crate) fn serialize<S: Serializer>(
        organisms: &Vec<Organism>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(organisms.iter().map(|p| {
            WireV3 {
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

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Organism>, D::Error> {
        let wire = Vec::<WireV3>::deserialize(deserializer)?;
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
                    growth_ms: w.age_ms,
                    seed_growth_ms: 0,
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
}
