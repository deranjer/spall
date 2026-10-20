use super::{Simulation, SimulationConfig};
use crate::fixtures::{STONE, flat_terrain_setup};
use spall_core::GlobalCell;
use spall_voxel::Sample;

/// A dam gate is a server-authored edit through the same commit pipeline
/// as a player's dig: it stages, then commits a few ticks later, and
/// closing with the original material exactly undoes opening.
#[test]
fn opens_and_closes_through_the_normal_edit_pipeline() {
    let mut sim = Simulation::new(SimulationConfig::new(flat_terrain_setup())).unwrap();
    let gate = [GlobalCell::new(10, 0, 10)];
    fn sample(sim: &Simulation) -> Sample {
        sim.world()
            .terrain()
            .volume
            .sample(GlobalCell::new(10, 0, 10))
            .unwrap()
    }
    assert!(matches!(sample(&sim), Sample::Filled(_)), "starts solid");

    sim.set_dam_gate(&gate, true, STONE).unwrap();
    for _ in 0..8 {
        sim.tick().unwrap();
    }
    assert!(matches!(sample(&sim), Sample::Empty { .. }), "open cuts it");

    sim.set_dam_gate(&gate, false, STONE).unwrap();
    for _ in 0..8 {
        sim.tick().unwrap();
    }
    assert!(
        matches!(sample(&sim), Sample::Filled(_)),
        "close refills it"
    );
}

/// An empty cell list (a scene with no authored gate) is a harmless no-op,
/// not a rejected intent.
#[test]
fn empty_cells_is_a_no_op() {
    let mut sim = Simulation::new(SimulationConfig::new(flat_terrain_setup())).unwrap();
    sim.set_dam_gate(&[], true, STONE).unwrap();
    for _ in 0..4 {
        sim.tick().unwrap();
    }
}
