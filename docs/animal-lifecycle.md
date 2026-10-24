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

## Death and carcasses

A cutting-tool strike whose aim meets a creature's posed voxels before any
terrain or body kills it (`Simulation::creature_struck`, resolved by the
server; the client's claimed target is ignored). `Simulation::kill_creature`
despawns the record, announced with the usual `CreatureDespawn`, and writes the
carcass: each posed source voxel maps to the 0.25 m terrain cell its centre
falls in, the whole shape drops onto the ground below, and only empty cells are
filled, as single-cell `Place` transactions with server request ids (the same
path vegetation growth uses). Cells go in by layers of support: first those
touching solid terrain, then those touching that layer, and so on, each layer
queued only once the previous one has committed (`advance_carcasses`, every
tick), so no cell ever lands unsupported and splits off as a body. Cells that
cannot reach the ground through face contact are left out. The first cell carries the strike's request id,
so the swing commits like a normal edit. The carcass is ordinary terrain:
editable and replicated. The game supplies the posed voxels
through `HostConfig::creature_shape`.

Not built: health and damage (every strike kills), drops, bears eating
carcasses, and carcasses finer than terrain cells (that needs runtime-born
fine-cell bodies, which do not replicate today).
