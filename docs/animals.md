# Animated animal foundations

User authorized implementation on 2026-10-14 after the SPVX/world feasibility
audit. The first pass establishes reusable assets, CPU animation and live-habitat
validation. It does not claim that animals spawn in the shipped game.

## Ready assignments

| Slice | Ticket | Scope | Prerequisite |
| --- | --- | --- | --- |
| A0 | ENG-201 | CPU `spall_asset` SPVX assembly decoding and transform sampling | Existing SPVX v1.1 contract, T25 attachment point |
| A1 | ENG-202 | Deterministic articulated trout, crow and deer fixtures | Existing SPVX v1.1 contract |
| A2 | ENG-203 | Sandbox live terrain/water habitat candidate validation | Existing terrain and column-water snapshots |
| A3 | ENG-204 | Articulated render extraction and bounded GPU preview | Reviewed A0/A1 contracts |
| A4 | ENG-205 | Authoritative creature lifecycle and world integration contract | Accepted A0-A3 foundations |
| A4-P | ENG-209 | Exact articulated collision and source-cell query feasibility | Accepted source assets; independent of runtime lifecycle APIs |
| A4-R | ENG-210 | Articulated movement and collision-response rig feasibility | Reviewed ENG-209 measurements |
| Zoo | ENG-211 | Interactive aquarium, aviary and paddock showroom | Accepted A0-A3 assets and render bridge; independent of runtime actors |

These assignments are independently ready. A1 writes the frozen binary format,
not an invented animation interface. A2 consumes existing world queries and
does not depend on the asset codec. The coordinator checks A1 using A0 before
accepting the combined foundation. Workers use isolated managed worktrees,
Luna 6 at Medium reasoning (the user's explicit model choice), and do not push
or integrate their own work. Coordinator owns shared docs and Loopira updates.

## Frozen boundaries

The interactive zoo is a visual inspection scene using the production renderer
and all three source assets at their original cell scale. Launch with
`cargo run -p sandbox --features client --example animal-zoo`. Controls:
0 overview, 1 trout, 2 crow, 3 deer; WASD move, Q/E vertical, Shift faster;
click for mouse look; Space pause; Tab idle/action; Esc release/exit.
The free camera has no collision. The aquarium water and aviary are display
geometry, and specimens play authored clips without navigation, contact response,
spawning, damage, persistence or replication. This does not accept ENG-210.
For bounded evidence use `-- --capture --out .local/animal-zoo` (12 images),
or `-- --frames 90` (native presentation, resize and display-mode smoke).
See [ENG-211 evidence](reports/ENG-211-animal-zoo.md).

- Engine-owned asset parsing/sampling is CPU-only and has no game species,
  graphics, window, network or physics dependencies. Existing static importers
  continue their explicit rejection of unsupported assemblies until separately
  integrated; adding a library is not runtime animation support.
- SPVX v1.1 `PART`/`ANIM` encodings, 60 animation ticks per second, exact voxel
  runs, portable materials and hash rules come from `spvox-format.md`. Do not
  infer gameplay joints, damage or physical bodies from an authored hierarchy.
- Use unit-scale articulated parts. No skinning, animated topology, finer
  cell-size codes, second physics engine or per-voxel entities. An animated
  consumer may clearly reject unsupported structural/layer features.
- Authored animals use 6.25 cm cells; small-animal detail remains limited. Each
  asset has explicit reusable identity, anatomy, rest transforms and clip
  expectations. Generator determinism does not prove visual quality.
- Habitat checks validate a declared axis-aligned body envelope against current
  immutable terrain and water. Unknown terrain and exhausted query budgets
  reject candidates. Generation biome/climate can select habitat, but cannot
  establish live occupancy, clearance or support.
- Candidate validation is not pathfinding, articulation-aware collision or
  flight/swimming physics. Terrain removal and water draining must invalidate
  previously suitable candidates when queried again.

## Foundation acceptance

A0 needs bounded malformed-input/hash/canonical-order/hierarchy/reference tests
and rest/interpolation/loop/ping-pong pose tests. A1 needs deterministic
regeneration and independently decoded fixtures, including meaningful tail,
wing and leg motion. A2 needs full-volume edge collision, unknown terrain,
depth/draining, missing support, step/slope and work-budget tests. CPU checks
are separate from rendered captures and population/performance gates.

## Current runtime (2026-10-20)

Live animals are plain server-authoritative entities: an id, a species key, a
root pose and a clip. They are drawn from the species' SPVX asset and own no
world voxels, so they cannot be cut apart voxel by voxel and are not saved with
the world. The server moves deer and crows with a capsule swept against terrain;
clients receive spawn, despawn and motion records. Players and bodies pass
through them. `--animals N` spawns N deer, N/2 crows and N/8 (rounded up) each
of grizzly bears and snow leopards, and N/4 trout, in a worldgen sandbox. Each walking species has its
own pace and foot capsule, sized to fit inside its body; placement envelopes and
foot height are measured from the asset's idle and walk poses.

Deer notice players: one within 28 m makes a deer stop and watch it (`alert`
clip); one within 14 m, or sprinting inside 28 m, makes it bolt at 7.5 m/s
(`run` clip), faster than a sprinting player. A deer that watched for a few
seconds with the player still near leaves anyway, and a fleeing deer calms
down once every player is 45 m away.

Deer resting on grass sometimes graze (`graze` clip) and really eat it: each
bite clears the ground cover under the muzzle through the server's
destroyed-cover record, so the patch replicates and regrows like a dug one.
Circling crows now and then land, hop and peck on the ground, then take off
back to their circle; a player within 10 m (20 m sprinting) sends them up. A
circling bird blocked by terrain climbs at most 12 m before reversing its
circle. Snow leopards spawn only on ground 8 m above their spawn anchor or on
steep ground, and wander no more than 6 m below where they were born; a world
with no high ground near the anchors has none.

Trout are born only where the live column water is deep enough for the whole
body (with 0.25 m below it), within six times the land spawn ring, and wander
inside the water at 0.6 m/s (`swim` clip); every step is checked to stay
underwater. A trout whose water drains sinks straight to the bed and lies still
until the water returns. A world with no deep water near the anchor has no
trout, and the other species keep their placement attempts.

At night (sun below the horizon) a resting or wandering deer lies down with its
`sleep` clip and stays put until morning; each deer's bedtime and waking trail
sunset and sunrise by up to one in-game hour. While asleep it ignores players
beyond 10 m (20 m sprinting); closer, it wakes and flees as usual.

Crows nest. Each picks the highest tree top within reach of its circle and
builds a 9-cell wood cup on it over several trips, one single-cell server edit
at a time and ground up, so nothing splits off. Nests are ordinary terrain: they
replicate, can be cut, and are saved like any edit. At night a crow roosts in
its finished nest until morning (a player within 10 m, or 20 m sprinting, still
spooks it). It rebuilds missing cells, and picks a new site if the tree top is
gone.

Bears hunt. A hungry bear stalks a deer it notices within 40 m (0.7 m/s, unseen
by deer), charges from 14 m at 10 m/s for up to 5 s, and kills on contact; the
despawn reaches every client like a hammer kill. It then eats the carcass top
down, one terrain cell every 0.5 s through single-cell server edits
(`CreatureAction::EditCell`), until it is gone or after 48 bites, and is hungry
again 4 minutes later. Deer flee a bear that is not stalking; a charging bear
counts as sprinting.

Bears do not like being run at. A player within 10 m, or one sprinting inside
22 m, makes a bear rear up (`stand` clip) and watch; if the player is still
within 7 m (or sprinting inside 10 m) it bluff-charges to 2.2 m, cuffs the air
(`swipe`) and backs off at a walk for 6 s, after which only a player inside 4 m
bothers it for 15 s. It does not hurt players yet (player health, ENG-223). A
bear chased off a carcass goes back to it. Bears are about 2.75 m long and
1.3 m at the hump.

A hungry bear with no deer in notice fishes: it walks into water at most 0.8 m
deep (or stops on the bank) beside a trout within 40 m, waits, and swipes
(`swipe` clip) at a trout within 1.5 m of its paw, catching it half the time. A
caught trout simply despawns on every client, with no carcass; the bear eats it
for 5 s (`eat`) and is hungry again 90 s later. Hovering trout swim away from a
bear standing in the water within 2 m.

An animal can die. A hammer (cutting tool) strike that meets an animal's posed
voxels before any terrain kills it with one blow: the server despawns it, and
its voxels, at 0.25 m terrain cells dropped onto the ground below, are written
into the terrain as ordinary committed edits. The carcass replicates and can be
cut away like any terrain; it is not saved, since worlds with animals are not.

The model, what it replaced, and the planned hunting design are in
[animal lifecycle](animal-lifecycle.md). Habitat, species and spawn rules are
game content in `examples/sandbox`.

Planned, not built: health and damage, drops,
finer-than-terrain carcasses,
swimming and flight physics beyond the capsule, saving animals, and the
population/performance measurements a populated world needs.
