# Animal lifecycle

An animal is a plain server-authoritative entity. This replaced an exact-ownership
model (per-part source volumes, an owner/component/generation record, a
death-to-dynamic-corpse transfer and fracture) that gameplay never used: nothing
in the game can kill an animal, and animals are not saved. The old contract is in
git history before the "simple creatures" change of 2026-10-20.

## Model

```
CreatureRecord { creature_id, species, translation_m, rotation_xyzw,
                 locomotion_tag, clip_phase_tick }
```

* **Identity.** `creature_id` is a nonzero `u64` from a monotone counter
  (`spall_core::CreatureAllocator`). Ids are never reused within a server run, not
  even across a world reset, so a motion entry can never address a different
  animal.
* **Species and look.** `species` is a game key (at most 63 bytes). The game owns
  the species list, SPVX assets, habitat and spawn rules (`examples/sandbox`); the
  engine never interprets the key. The client picks the clip from the species and
  `locomotion_tag`, and samples it at `clip_phase_tick`.
* **Pose and motion.** `spall_sim` keeps the record. The game's
  `CreatureDirector` (`spall_server::creature_service`) asks the simulation to
  walk (deer) or fly (crow) a creature; the simulation sweeps a capsule around it
  against terrain with the kinematic controller players use, and stores the
  resulting pose. Clients receive `CreatureMotionFrame`s at 20 Hz.
* **Spawn and despawn.** Replicated records on the reliable ordered stream, with
  the late-join list taken at the same instant a joining client's catch-up queue
  starts. See [protocol](protocol.md#creatures).
* **Collision.** Animals collide with terrain through their capsule. Players,
  bodies and other animals do not collide with them.
* **Persistence.** None. A world with animals cannot be saved; startup and
  checkpoint capture refuse it. Persisting simple records later only needs a
  small section keyed by id.

## Planned: hunting (not built)

Death despawns the entity and the game emits drops (items, or a corpse). If a
corpse is wanted it is an ordinary voxel body created through the existing
split-to-body path, or a simple prop, never a special animal-owned aggregate.
Damage and hit detection would be a capsule or box test against the record's
pose, resolved by the server. None of this exists yet; `despawn_creature` and the
`CreatureDirector::despawns` hook are the only pieces in place.
