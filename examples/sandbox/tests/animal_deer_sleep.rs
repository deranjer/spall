#![cfg(feature = "client")]
//! Deer lie down and sleep at night, and a player who comes close wakes them.

use sandbox::animals::{AnimalDirector, LOCOMOTION_RUN, LOCOMOTION_SLEEP};
use spall_core::time_of_day::{DAY_CYCLE_TICKS, DayClock};
use spall_core::{EntityId, PLAYER_ENTITY_BASE};
use spall_server::creature_service::{
    CreatureAction, CreatureActionOutcome, CreatureDirector, CreatureIdPlan, CreatureService,
};
use spall_sim::{Simulation, SimulationConfig, fixtures};

/// 21:00 on the first day: the sim starts at 06:00, so this offset puts the
/// director's clock in the middle of the evening without ticking there.
const NIGHT: u64 = DAY_CYCLE_TICKS * 5 / 8;

/// The sandbox director with its clock shifted by [`NIGHT`].
struct AtNight(AnimalDirector);

impl CreatureDirector for AtNight {
    fn motion(&mut self, tick: u64, sim: &Simulation) -> Vec<CreatureAction> {
        self.0.motion(tick + NIGHT, sim)
    }
    fn on_action_outcome(&mut self, creature_id: u64, outcome: CreatureActionOutcome) {
        self.0.on_action_outcome(creature_id, outcome);
    }
    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<spall_protocol::CreatureRecord> {
        self.0.plan(tick + NIGHT, sim, ids)
    }
}

fn step(sim: &mut Simulation, service: &CreatureService, ticks: u32) {
    for _ in 0..ticks {
        sim.tick().unwrap();
        service.run_tick(sim).unwrap();
        service.run_motion(sim).unwrap();
    }
}

fn deer(sim: &Simulation) -> spall_protocol::CreatureRecord {
    sim.creature_records()
        .into_iter()
        .find(|c| c.species == "deer")
        .unwrap()
}

#[test]
fn a_deer_sleeps_at_night_until_a_player_walks_up() {
    assert!(DayClock::default().state_at(NIGHT).sun_height() < 0.0);
    let mut setup = fixtures::flat_terrain_setup();
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[1], 9)
        .unwrap()
        .with_radius_m(0.0, 0.5);
    let service = CreatureService::new(Box::new(AtNight(director)));
    step(&mut sim, &service, 60);
    let bed = deer(&sim);
    assert_eq!(
        bed.locomotion_tag, LOCOMOTION_SLEEP,
        "deer did not lie down"
    );

    // A player standing 12 m off would make an awake deer bolt; a sleeping
    // one stays put.
    let at = |offset: f64| [bed.translation_m[0] - offset, 0.6, bed.translation_m[2]];
    sim.add_player(EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap(), at(12.0));
    step(&mut sim, &service, 240);
    let still = deer(&sim);
    assert_eq!(still.locomotion_tag, LOCOMOTION_SLEEP);
    let drift = (still.translation_m[0] - bed.translation_m[0])
        .hypot(still.translation_m[2] - bed.translation_m[2]);
    assert!(drift < 0.05, "sleeping deer moved {drift:.2} m");

    // Closer still wakes it, and it runs.
    sim.add_player(EntityId::new(PLAYER_ENTITY_BASE + 2).unwrap(), at(6.0));
    step(&mut sim, &service, 24);
    assert_eq!(
        deer(&sim).locomotion_tag,
        LOCOMOTION_RUN,
        "woken deer did not flee"
    );
}
