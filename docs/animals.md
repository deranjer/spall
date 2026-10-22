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
within 7 m (or sprinting inside 10 m) it bluff-charges, swipes (`swipe`) and
backs off at a walk for 6 s, after which only a player inside 4 m bothers it
for 15 s. Most charges are bluffs that stop 3.5 m short and cuff the air. A
player still inside 4 m when the bear finishes rearing, or one bluffed player
in about seven by a seeded roll (15 %), gets a real charge that closes to
1.8 m (ENG-240). Either way, the paw lands 0.3 s into the swipe on a player
within 2.5 m of the bear's root, so stepping into a bluff gets you hit too. A
hit removes `BEAR_SWIPE_DAMAGE` (60 of 100) through `Simulation::damage_player`;
two kill, and the player respawns at their spawn with full health. The server
refuses a creature hit on a player more than 3 m from the creature's root. A
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

## Sound list (candidates, not yet auditioned or integrated)

No animal makes a sound yet. The client's audio (`spall_client::audio`) only plays
non-positional ambience, footsteps and breathing. Animal sounds first need
**positional playback**: a one-shot or loop at a world position, attenuated by
distance and panned by direction, and triggered from replicated creature state.
Most cues below fire when a creature's locomotion tag changes (for example
`LOCOMOTION_REAR`) or while a tag is held (walk, run).

All links are Freesound searches filtered to **CC0**. Accepted files go into
`examples/sandbox/assets/audio/animals/` and `CREDITS.md` the same way as the
biome audio. Range is roughly how far away the sound should still be audible.

### Deer

| Cue | Trigger | Range | Candidates |
| --- | --- | --- | --- |
| Hoof steps, walk | `walk` (MOVE) | 15 m | [search](https://freesound.org/search/?q=deer+hooves+walking&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Hoof gallop | `run` (fleeing) | 40 m | [search](https://freesound.org/search/?q=deer+running+hooves&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Alarm snort / whistle | Entering `alert`, start of flight | 60 m | [search](https://freesound.org/search/?q=deer+snort&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Bleat / grunt | Occasional while resting | 25 m | [search](https://freesound.org/search/?q=deer+bleat&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Grass tearing / chewing | Each grazing bite | 8 m | [search](https://freesound.org/search/?q=animal+grazing+grass&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Bedding down | Entering `sleep` | 10 m | [search](https://freesound.org/search/?q=animal+lying+down+leaves&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Brush crash | Fleeing through undergrowth | 30 m | [search](https://freesound.org/search/?q=crashing+through+bushes&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Bear

| Cue | Trigger | Range | Candidates |
| --- | --- | --- | --- |
| Heavy pad steps | `walk` | 15 m | [search](https://freesound.org/search/?q=heavy+animal+footsteps&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Charging gallop | `run` (charge, bluff) | 40 m | [search](https://freesound.org/search/?q=large+animal+running&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Huff / woof, jaw pop | Entering `stand` (rearing at a player) | 40 m | [search](https://freesound.org/search/?q=bear+huff&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Roar / growl | Bluff charge starts | 80 m | [search](https://freesound.org/search/?q=bear+roar&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first), [search](https://freesound.org/search/?q=bear+growl&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Swipe whoosh | `swipe` (cuff, kill, fishing swipe) | 15 m | [search](https://freesound.org/search/?q=claw+swipe+whoosh&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Hit on player | `HitPlayer` lands (pairs with the player's pain grunt) | 15 m | [search](https://freesound.org/search/?q=body+hit+thud&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Tearing and chewing meat | `eat` on a carcass | 15 m | [search](https://freesound.org/search/?q=animal+eating+meat&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Wading splash | Walking in water (fishing) | 20 m | [search](https://freesound.org/search/?q=wading+water+splash&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Sniffing / low grunts | Occasional while wandering or stalking | 15 m | [search](https://freesound.org/search/?q=bear+grunt&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Snow leopard

| Cue | Trigger | Range | Candidates |
| --- | --- | --- | --- |
| Soft pad steps | `walk` | 8 m | [search](https://freesound.org/search/?q=cat+footsteps+snow&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Chuff (prusten) | Occasional while resting | 25 m | [search](https://freesound.org/search/?q=big+cat+chuff&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Yowl / call | Rare, mostly at dusk | 150 m | [search](https://freesound.org/search/?q=snow+leopard&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first), [search](https://freesound.org/search/?q=big+cat+yowl&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Growl / hiss | Reserved for a later player reaction (leopards ignore players today) | 20 m | [search](https://freesound.org/search/?q=big+cat+growl&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Crow

| Cue | Trigger | Range | Candidates |
| --- | --- | --- | --- |
| Caw (varied) | Occasional while circling | 120 m | [search](https://freesound.org/search/?q=crow+caw&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Alarm caws | Spooked by a player | 120 m | [search](https://freesound.org/search/?q=crow+alarm&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Wing flaps | `takeoff`, `flap` near the listener | 20 m | [search](https://freesound.org/search/?q=bird+wings+flapping&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Landing flutter | `land` | 15 m | [search](https://freesound.org/search/?q=bird+landing&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Pecking taps | `peck` | 6 m | [search](https://freesound.org/search/?q=bird+pecking&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Twig snap | Placing a nest cell | 15 m | [search](https://freesound.org/search/?q=twig+snap&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Soft roost calls | Settling on the nest at dusk | 40 m | [search](https://freesound.org/search/?q=crow+roost&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Trout

| Cue | Trigger | Range | Candidates |
| --- | --- | --- | --- |
| Small splash | A trout turning near the surface; scattering from a bear | 10 m | [search](https://freesound.org/search/?q=fish+splash&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Flopping | Stranded on a drained bed | 8 m | [search](https://freesound.org/search/?q=fish+flopping&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Catch splash | A bear catches a trout | 20 m | [search](https://freesound.org/search/?q=big+splash+water&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Shared

| Cue | Trigger | Range | Candidates |
| --- | --- | --- | --- |
| Hammer hit on an animal | A strike kills a creature | 20 m | [search](https://freesound.org/search/?q=meat+impact+thud&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Body fall | Carcass lands | 20 m | [search](https://freesound.org/search/?q=body+fall+ground&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |
| Cutting a carcass | Player or bear removes carcass cells | 10 m | [search](https://freesound.org/search/?q=flesh+tear&f=license%3A%22Creative+Commons+0%22&s=Rating+highest+first) |

### Ambient birdsong (to vary the biome beds)

Birdsong plays only in short bursts (3-8 s, every 40-120 s, or every 4-15 s
with a crow within 60 m), where a birdsong biome (meadow, alpine, temperate
forest) is in earshot or a crow is near. Each burst picks at random between the
biome's own track and a pool in `assets/audio/birds/`: blackbird (isolated),
robin (Madeira forest, first 2:25) and blackcap (first 56 s), all CC0 and
credited in `CREDITS.md`. A recording keeps playing silently between bursts, so
the next burst resumes on a different phrase. Further CC0 candidates
(Freesound's CC0 filter; not yet auditioned):

| Bird | Candidates |
| --- | --- |
| Blackbird | [solo, good quality](https://freesound.org/people/Sacha.Julien/sounds/725332/), [isolated](https://freesound.org/people/richwise/sounds/811988/), [252](https://freesound.org/people/nigelcoop/sounds/116791/) |
| Robin | [Madeira forest](https://freesound.org/people/Sacha.Julien/sounds/725201/), [good quality](https://freesound.org/people/Sacha.Julien/sounds/725331/), [European robin](https://freesound.org/people/TheKingOfGeeks360/sounds/855901/) |
| Song thrush | [richwise 795708](https://freesound.org/people/richwise/sounds/795708/), [richwise 785669](https://freesound.org/people/richwise/sounds/785669/) |
| Blackcap | [short and good](https://freesound.org/people/Sacha.Julien/sounds/725333/) |
| Thrush nightingale | [01](https://freesound.org/people/LilMati/sounds/365658/), [04](https://freesound.org/people/LilMati/sounds/365668/) |
| Starling | [common starling](https://freesound.org/people/Vrymaa/sounds/737756/) |
| Single bird, quiet forest | [HECKFRICKER](https://freesound.org/people/HECKFRICKER/sounds/730301/) |
| Swift | [chimney swift, single](https://freesound.org/people/TheKingOfGeeks360/sounds/856266/) |
