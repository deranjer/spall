//! Game-authored, fully visible clearing for the client-local ecology fixture.

use std::collections::BTreeMap;

use spall_client::EcologyDemoSetup;
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_ecology::{
    EcologyConfig, EcologyInputs, EcologyState, SpeciesDefinition, SpeciesId, place_grass_patch,
    place_tree,
};
use spall_physics::PhysicsConfig;
use spall_sim::WorldSetup;
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, Volume};

/// 16 m square, 24 m of known air, no arena wall or hidden terrain crop.
pub fn setup() -> EcologyDemoSetup {
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    let grass = MaterialId(104);
    let mut inputs = EcologyInputs::default();
    for bz in 0..2 {
        for bx in 0..2 {
            for by in 0..3 {
                let mut cells = vec![MaterialId::AIR; 32768];
                for z in 0..32 {
                    for x in 0..32 {
                        let (gx, gz) = (bx * 32 + x, bz * 32 + z);
                        // A shallow stepped hillside makes grounding visible.
                        let top = 4 + gx / 24 + gz / 32;
                        for y in 0..32 {
                            let gy = by * 32 + y;
                            if gy <= top {
                                cells[(x + 32 * (y + 32 * z)) as usize] = if gy == top {
                                    grass
                                } else {
                                    crate::game::materials::DIRT
                                };
                            }
                        }
                        inputs.moisture.insert(GlobalCell::new(gx, top, gz), 128);
                    }
                }
                terrain
                    .insert_brick(
                        BrickCoord {
                            x: bx,
                            y: by,
                            z: bz,
                        },
                        Brick::restored(&cells, Revision::ZERO, false),
                    )
                    .unwrap();
            }
        }
    }
    let species_id = SpeciesId(1);
    let species = SpeciesDefinition {
        version: 1,
        id: species_id,
        wood: crate::game::materials::WOOD,
        soil_materials: [
            grass,
            crate::game::materials::DIRT,
            MaterialId(214),
            MaterialId(213),
        ],
        min_moisture: 20,
        max_moisture: 220,
        min_sky_exposure: 12,
        min_spacing_cells: 16,
        seed_radius_cells: 24,
        seed_lifetime_ms: 30_000,
        seedling_ms: 3_000,
        juvenile_ms: 7_000,
        cell_growth_ms: 200,
    };
    let config = EcologyConfig {
        update_interval_ms: 1_000,
        max_work_per_update: 128,
        max_seed_records: 32,
        max_plants: 12,
        grass_regrowth_per_interval: 5,
        spawn_clearance_cells: 16,
        bounds: Some((GlobalCell::new(0, 0, 0), GlobalCell::new(63, 95, 63))),
    };
    let mut state = EcologyState::default();
    let focus = GlobalCell::new(32, 7, 32);
    assert!(place_tree(&mut state, &terrain, species, &inputs, config, focus).is_some());
    let anchor = GlobalCell::new(40, 7, 32);
    let grass_patch_id = place_grass_patch(&mut state, species_id, anchor, 100).unwrap();
    EcologyDemoSetup {
        world: WorldSetup {
            terrain,
            terrain_collider_region: config.bounds.unwrap(),
            materials: crate::game::manifest(),
            anchor: AnchorPlane::at(0),
            physics: PhysicsConfig {
                disable_ccd: true,
                ..Default::default()
            },
        },
        focus,
        state,
        inputs,
        definitions: BTreeMap::from([(species_id, species)]),
        config,
        grass_patch_id,
        grass_material: crate::game::materials::MOSS,
        foliage_material: MaterialId(112),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_voxel::Sample;

    #[test]
    fn clearing_is_fully_resident_and_initial_vegetation_touches_soil() {
        let setup = setup();
        for root in setup
            .state
            .plants
            .values()
            .map(|p| p.root)
            .chain(setup.state.grass.values().map(|p| p.anchor))
        {
            assert!(matches!(
                setup.world.terrain.sample(root),
                Ok(Sample::Empty { .. })
            ));
            assert!(matches!(
                setup
                    .world
                    .terrain
                    .sample(GlobalCell::new(root.x, root.y - 1, root.z)),
                Ok(Sample::Filled(_))
            ));
        }
        for z in 0..64 {
            for x in 0..64 {
                assert!(matches!(
                    setup.world.terrain.sample(GlobalCell::new(x, 90, z)),
                    Ok(Sample::Empty { .. })
                ));
                assert!(matches!(
                    setup.world.terrain.sample(GlobalCell::new(x, 0, z)),
                    Ok(Sample::Filled(_))
                ));
            }
        }
    }
}
