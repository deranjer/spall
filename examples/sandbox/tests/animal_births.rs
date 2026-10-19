#![cfg(feature = "client")]
//! The sandbox director spawns the authored species through the engine's
//! ordered lifecycle path, and the resulting records round-trip through a
//! replica unchanged.

use sandbox::animals::AnimalDirector;
use spall_core::WorldId;
use spall_server::creature_service::CreatureService;
use spall_sim::{Simulation, SimulationConfig, fixtures};

fn simulation() -> Simulation {
    let mut setup = fixtures::flat_terrain_setup();
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(77))
        .unwrap();
    sim
}

#[test]
fn director_births_deer_and_crows_that_validate_and_replicate() {
    let mut sim = simulation();
    let anchor = [3.0, 1.0, 3.0];
    let director = AnimalDirector::new(vec![anchor], [2, 2, 0], 7)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));

    let mut replica = spall_client::replica::ReplicaWorld::from_baseline(
        {
            let mut setup = fixtures::flat_terrain_setup();
            setup.materials = sandbox::game::manifest();
            setup.terrain
        },
        spall_client::replica::ReplicaConfig::default(),
    );
    replica.install_creature_catalog(service.catalog().clone());

    let mut committed = 0;
    for _ in 0..400 {
        sim.tick().unwrap();
        if let Some(birth) = service.run_tick(&mut sim).expect("birth commits") {
            committed += 1;
            let outcome = replica.apply_transaction(&birth.topology);
            assert!(
                matches!(
                    outcome,
                    spall_client::replica::ApplyOutcome::Published { .. }
                ),
                "replica rejected a server birth: {outcome:?}"
            );
        }
    }
    assert!(committed > 0, "director never spawned anything");
    let snapshot = service
        .catalog()
        .with_view(|view| sim.capture_creature_snapshot(view))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.owners.records.len(), 4, "2 deer + 2 crows");
    let replicated: Vec<_> = replica.creatures().unwrap().records().cloned().collect();
    assert_eq!(replicated.len(), snapshot.owners.records.len());
    for server in &snapshot.owners.records {
        let client = replicated
            .iter()
            .find(|r| r.state.key == server.state.key)
            .expect("replica holds every server creature");
        // Topology is exact. Motion is captured live on the server, so the
        // at-birth record may differ from it only by physics rounding.
        assert_eq!(client.topology_hash(), server.topology_hash());
        assert_eq!(client.parts, server.parts);
        for axis in 0..3 {
            assert!(
                (client.state.root_translation_m[axis] - server.state.root_translation_m[axis])
                    .abs()
                    < 1e-3,
                "creature {} root drifted on axis {axis}",
                server.state.key.creature_id.get()
            );
        }
    }
}
