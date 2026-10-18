//! Invalid runtime correlation IDs must reject before any authoritative tick work.
use spall_protocol::{SPLIT_BULK_TRANSFER_ID_BIT, TransferId, WORLD_RESET_TRANSFER_ID_BIT};
use spall_sim::{Simulation, SimulationConfig, fixtures};

#[test]
fn invalid_transfer_reservations_do_not_advance_the_tick_or_world() {
    let split = SPLIT_BULK_TRANSFER_ID_BIT;
    let reset = WORLD_RESET_TRANSFER_ID_BIT;
    let lifecycle = 1_u64 << 61;
    for ids in [
        vec![TransferId(0)],
        vec![TransferId(1)],
        vec![TransferId(reset | 1)],
        vec![TransferId(lifecycle | 1)],
        vec![TransferId(split)],
        vec![TransferId(split | reset | 1)],
        vec![TransferId(split | lifecycle | 1)],
        vec![TransferId(split | 1), TransferId(split | 1)],
    ] {
        let mut sim =
            Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
        let before = (
            sim.current_tick(),
            sim.world().world_hash(),
            sim.world().registry().counters(),
            sim.journal_cursor(),
            sim.world().topology_epoch(),
        );
        assert!(sim.tick_with_transfer_ids(ids).is_err());
        assert_eq!(
            (
                sim.current_tick(),
                sim.world().world_hash(),
                sim.world().registry().counters(),
                sim.journal_cursor(),
                sim.world().topology_epoch(),
            ),
            before,
        );
    }
}

#[test]
fn idle_tick_needs_no_transfer_ids_and_keeps_geometry_counters() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let counters = sim.world().registry().counters();
    let tick = sim.current_tick();
    assert_eq!(sim.transfer_reservation_bound(), 0);
    sim.tick_with_transfer_ids(Vec::new()).unwrap();
    assert!(sim.current_tick() > tick);
    assert_eq!(sim.world().registry().counters(), counters);
    assert_eq!(sim.journal_cursor(), 0);
    assert_eq!(sim.transfer_reservation_bound(), 0);
}
