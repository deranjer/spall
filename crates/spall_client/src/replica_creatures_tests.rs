//! The replica creature store: ordered spawn/despawn, idempotent join lists, and
//! motion that is ignored unless it belongs to a held creature.
use super::*;
use spall_protocol::{CreatureMotionEntry, WIRE_SCHEMA_VERSION};

fn spawn(creature_id: u64, x: f64) -> CreatureSpawn {
    CreatureSpawn {
        schema: WIRE_SCHEMA_VERSION,
        creature: CreatureRecord {
            creature_id,
            species: "deer".into(),
            translation_m: [x, 2.0, 8.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            locomotion_tag: 0,
            clip_phase_tick: 0,
        },
    }
}

fn motion_frame(server_tick: u64, creature_id: u64, x: f64) -> CreatureMotionFrame {
    CreatureMotionFrame {
        schema: WIRE_SCHEMA_VERSION,
        server_tick,
        entries: vec![CreatureMotionEntry {
            creature_id,
            translation_m: [x, 2.0, 8.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [1.0, 0.0, 0.0],
            locomotion_tag: 1,
            clip_phase_tick: server_tick,
        }],
    }
}

#[test]
fn spawn_and_despawn_follow_the_stream_and_a_join_list_is_idempotent() {
    let mut creatures = ReplicaCreatures::new();
    // The join list, delivered twice (a retried join), installs each creature once.
    for _ in 0..2 {
        creatures.spawn(&spawn(1, 10.0)).unwrap();
        creatures.spawn(&spawn(2, 20.0)).unwrap();
    }
    assert_eq!(
        creatures
            .records()
            .map(|c| c.creature_id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    // A live spawn after the list, then a despawn, in stream order.
    creatures.spawn(&spawn(3, 30.0)).unwrap();
    assert!(creatures.despawn(2));
    assert!(!creatures.despawn(2));
    assert_eq!(
        creatures
            .records()
            .map(|c| c.creature_id)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert!(creatures.get(2).is_none());

    // An invalid record is refused and changes nothing.
    let mut bad = spawn(9, 0.0);
    bad.creature.species.clear();
    assert_eq!(creatures.spawn(&bad), Err(ReplicaCreatureError::Invalid));
    assert_eq!(creatures.len(), 2);

    // A baseline reset forgets everyone.
    creatures.reset();
    assert!(creatures.is_empty());
}

#[test]
fn motion_for_unknown_or_despawned_ids_is_ignored() {
    let mut creatures = ReplicaCreatures::new();
    let t0 = Instant::now();
    // Motion before any spawn is dropped, not deferred.
    assert_eq!(creatures.apply_motion(&motion_frame(10, 1, 5.0), t0), 0);
    creatures.spawn(&spawn(1, 0.0)).unwrap();
    // Motion for another id is dropped; for the held id it applies.
    assert_eq!(creatures.apply_motion(&motion_frame(10, 99, 5.0), t0), 0);
    assert_eq!(creatures.apply_motion(&motion_frame(10, 1, 5.0), t0), 1);
    // After the despawn the id is unknown again, and its track is gone.
    assert!(creatures.despawn(1));
    assert_eq!(creatures.apply_motion(&motion_frame(20, 1, 6.0), t0), 0);
    assert_eq!(creatures.motion_entries_applied(), 1);
}

#[test]
fn motion_frames_interpolate_hold_and_ignore_stale_entries() {
    let mut creatures = ReplicaCreatures::new();
    creatures.spawn(&spawn(7, 0.0)).unwrap();
    let t0 = Instant::now();
    assert_eq!(creatures.apply_motion(&motion_frame(10, 7, 10.0), t0), 1);
    let later = t0 + Duration::from_millis(100);
    assert_eq!(creatures.apply_motion(&motion_frame(16, 7, 11.0), later), 1);
    // A stale tick is ignored.
    assert_eq!(creatures.apply_motion(&motion_frame(13, 7, 99.0), later), 0);

    let record = creatures.get(7).unwrap().clone();
    // Render time = now - delay: halfway between the two arrivals.
    let halfway = t0 + CREATURE_RENDER_DELAY + Duration::from_millis(50);
    let mid = creatures.presentation(&record, halfway);
    assert!(
        (mid.translation_m[0] - 10.5).abs() < 1e-6,
        "{:?}",
        mid.translation_m
    );
    assert_eq!(mid.locomotion_tag, 1);
    // Past the newest sample it extrapolates along velocity, capped.
    let far = later + CREATURE_RENDER_DELAY + Duration::from_secs(5);
    let held = creatures.presentation(&record, far);
    assert!(
        (held.translation_m[0] - (11.0 + 0.15)).abs() < 1e-6,
        "{:?}",
        held.translation_m
    );
}

#[test]
fn steady_motion_presents_without_hold_and_jump() {
    let mut creatures = ReplicaCreatures::new();
    creatures.spawn(&spawn(7, 0.0)).unwrap();
    let id = 7;
    let record = creatures.get(id).unwrap().clone();

    // The server moves a walker 0.2 m every 8 ticks (133 ms); frames are drawn
    // every 16 ms and each draw first takes whatever poses have arrived.
    let t0 = Instant::now();
    let move_every = Duration::from_millis(133);
    let mut sent = 0_u32;
    let mut presented = Vec::new();
    for frame in 0..150_u32 {
        let now = t0 + Duration::from_millis(16) * frame;
        while t0 + move_every * sent <= now {
            let tick = 10 + u64::from(sent) * 8;
            creatures.apply_motion(
                &motion_frame(tick, id, 0.2 * f64::from(sent)),
                t0 + move_every * sent,
            );
            sent += 1;
        }
        presented.push(creatures.presentation(&record, now).translation_m[0]);
    }
    // Once the render delay has passed, every frame advances by about the
    // steady per-frame distance: no frame holds still and none jumps.
    let per_frame = 0.2 * 16.0 / 133.0;
    for pair in presented[20..].windows(2) {
        let step = pair[1] - pair[0];
        assert!(
            step > per_frame * 0.5 && step < per_frame * 1.5,
            "uneven step {step} (expected about {per_frame}); presented {presented:?}"
        );
    }
}
