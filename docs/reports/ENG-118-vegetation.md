# ENG-118: living biome vegetation

Generated playable worlds now establish ten tree species and ten ground-plant
species. This runs in `sandbox::worldgen_scene`, so both `sandbox-server
--worldgen` and the editor's Generate World → Run in game use it at every
supported arena size. The generator currently exposes **one preset,
Showcase**, containing Meadow, Alpine, Swamp and Desert. Hand-authored scenes
and diagnostic fixtures are not procedural worlds and are not seeded over.

## Species catalogue

Stable IDs and game-owned definitions live in `examples/sandbox/src/vegetation.rs`.
Profiles specify soils, biome mask, moisture range, wood material, dimensions,
branch/crown form, spacing, growth/seeding cadence and four seasonal colours.
Shapes vary deterministically by specimen. No tree asset is copied into worlds.

| ID | Tree | Allowed biomes | Winter |
|---|---|---|---|
| 1 | Oak | Meadow | Bare |
| 2 | Maple | Meadow | Bare |
| 3 | Birch | Meadow, Alpine | Bare |
| 4 | Aspen | Meadow, Alpine | Bare |
| 5 | Scots pine | Meadow, Alpine | Evergreen |
| 6 | Spruce | Alpine | Evergreen |
| 7 | Larch | Alpine | Bare |
| 8 | Willow | Swamp | Bare |
| 9 | Cypress | Swamp | Evergreen |
| 10 | Acacia | Desert | Retains foliage |

| ID | Ground plant | Allowed biomes | Seasonal appearance |
|---|---|---|---|
| 11 | Meadow grass | Meadow | Green to straw/grey |
| 12 | Fescue | Meadow, Alpine | Green to dry straw |
| 13 | Alpine tussock | Alpine | Pale green to dormant straw |
| 14 | Sedge | Swamp | Green to brown/grey |
| 15 | Reed | Swamp | Brown heads, reduced dry winter stems |
| 16 | Fern | Meadow, Swamp | Green fronds, bronze autumn, winter dieback |
| 17 | Clover | Meadow | Low lobed leaves, winter dieback |
| 18 | Wildflower | Meadow | Spring/summer flowers, winter dieback |
| 19 | Desert bunchgrass | Desert | Pale green to dry straw |
| 20 | Sage scrub | Desert | Grey-green branching foliage, retained in winter |

These are initial gameplay interpretations of the four available biomes, not
full botanical or climate models. Reeds establish on dry swamp banks; submerged
plants and snow-covered alpine soil are currently excluded.

## Behaviour and ownership

Wood is ordinary destructible **0.25 m** terrain cells. Wider trunks comprise
multiple cells; growth paths retain explicit, face-connected parents. Initial
wood is generated before simulation/collision creation. Subsequent wood growth
is submitted as bounded, single-cell server-authored intents to the existing
transaction, support, collision, journal and replication pipeline. Progress
advances only after committed terrain occupancy is acknowledged. No plant
receives a hidden anchor, special physics solver or per-voxel entity.

Foliage, blades, fronds, flowers and scrub leaves are deterministic, non-solid
geometry. Leaves use 0.25 m cells; grass blades are about 2.5 cm wide.
They participate in ordinary opaque lighting and shadow rendering. The server
sends retained live growth tips and plant records; clients derive the same
geometry and seasonal colours. Leaves disappear when their wood/root no longer
exists. A soft presentation update can lag a terrain edit by up to one second.
Detached wood remains under the engine's existing body ownership; detached
foliage/dead-leaf litter is not simulated in this increment.

Establishment validates current soil and known air, exact generated biome,
biome-derived moisture, original water footprint, horizontal spacing and a
4 m clearance around player spawns. Seeds resolve the current surface before
establishment; they cannot tunnel through roofs or plant on walls/rock. New
ground plants start with low biomass and grow. Established plants reproduce
at species-specific intervals; a successful dispersal directly establishes a
child rather than retaining a dormant seed bank. Winter suspends wood growth,
ground biomass recovery and seeding. Trees whose roots are destroyed retire;
lost branches and their descendants never regrow. Excavated/covered ground
plants retire, freeing capacity for later offspring. Stable IDs are never reused.

The independent plant season clock begins in summer by default. Each season
currently lasts **15 minutes of server simulation time**, configurable in the
saved plant state. Deciduous trees change colour in autumn and lose leaves in
winter; spring restores foliage, and flowers bloom in spring/summer. This is
plant season awareness, not worldwide weather, lighting or snow simulation.
Moisture is a biome input, not yet coupled to dynamic water or rainfall.

Bounds: 512 tree records, 8,192 ground records, 262,144 retained wood cells;
128 organisms per one-second fair work pass and at most four outstanding growth
intents. Rendering culls at 48 m, prioritises nearby plants and caps soft geometry
at 150,000 instances. Larger maps use the same placement path with bounded,
sparser initial coverage. These are budgets, not measured frame-rate guarantees.

## Persistence and networking

`spall_ecology::living` owns a separate version-1 DTO for species definitions,
stable plant IDs, retained wood paths, retired branches, ages, biomass, random
seed, work cursor and season/interval clock. ENG-116's standalone prototype
encoding and ENG-117's diagnostic clearing remain compatible and separate.
Terrain `GEN_VERSION = 3` remains unchanged: the base terrain generator has not
changed; the game adds the independently versioned vegetation plan afterwards.

Store schema 2 gains an additive checksummed `checkpoint_vegetation` row in the
same checkpoint-publication transaction. Existing schema-2 checkpoints are
backfilled with empty vegetation in the same transaction when the auxiliary table
is first added; old
worlds are recovered as saved, never regenerated or reseeded over user edits.
Deleted/corrupt vegetation rows in upgraded saves fail recovery. Appended
`VegetationState` and small `VegetationClock` journal variants preserve previous
discriminants and recover the exact plant clock. State snapshots follow their
tick's committed topology; small clock records avoid writing the entire forest
at 60 Hz. Pending uncommitted growth requests are safely regenerated on recovery.

Wire tag **19** adds version-1 vegetation keyframes, using bounded 48 KiB chunks,
a 4 MiB assembly limit and BLAKE3 verification. Frames go to every live session
once per second, including late joiners. Replacing world baselines clear the
client's vegetation and reject queued frames older than the replacement tick.
The existing wire envelope remains schema 3; tag 19 is additive. Matched new
client/server builds are required to display vegetation.

## Launch and evidence

From the ecology checkout:

```powershell
cargo xtask play --worldgen showcase --seed 1 --worldgen-size 512 --season autumn
```

Choose `spring`, `summer`, `autumn` or `winter`. The editor's Generate World
button defaults to summer and uses the same server setup. `--season` is also
available directly on `sandbox-server --worldgen showcase`.

Validation and final evidence are recorded in the adjacent
`ENG-118-evidence` directory. Specimen galleries use generated seed-1, 512-cell
world specimens and the production geometry/shadow path, in stable ID order
from left to right. They are deliberately arranged for comparison; they are not
screenshots of natural forest distribution. Three additional `world-*.png` captures
use the actual generated terrain, spawn viewpoint and production geometry.
`default-1024` includes captures and counts for the default arena size.

Measured seed-1 results:

| Arena size (cells) | Trees | Ground plants | Saved state bytes | Summer soft instances | Winter soft instances |
|---|---:|---:|---:|---:|---:|
| 512 | 255 | 2,932 | 214,431 | 150,000 | 111,971 |
| 1,024 (default) | 512 | 7,843 | 556,261 | 149,494 | 80,318 |

All twenty species were present in both worlds. The 512-cell summer/autumn
view reaches the 150,000-instance ceiling: farther foliage is omitted. The
default world reaches the 512-tree establishment limit. Both limits are
explicit budgets; density and distance LOD need further tuning. GPU captures
completed without validation errors; frame-rate/hardware gates were not run.

The six vegetation integration tests cover deterministic biome placement,
seasonal appearance, root/soil destruction, offspring establishment, committed
wood growth plus exact checkpoint/journal recovery, and two-client QUIC
replication with a late join. Catalogue and protocol chunk tests also pass. A
storage regression test covers read-only legacy schema-2 recovery, the atomic
writer upgrade/backfill, and refusal of an upgraded checkpoint with a missing
vegetation row.

Completed checks (all passed): `cargo test -p sandbox --features client --test vegetation`,
`cargo test -p spall_protocol vegetation::tests`, `cargo xtask check`,
`cargo run -p sandbox --features client --example vegetation-gallery --
.local/vegetation-gallery`, and `git diff --check`. The final `cargo xtask check` ran alone after
review changes and completed successfully, including workspace documentation
tests. `cargo test -p spall_store --test durability` passed all 25 tests.
The archived `xtask-check-release.log` is the final full verification record.

Remaining work: art tuning, density/LOD and whole-game performance measurements,
dynamic moisture/canopy competition, dormant seed banks and leaf litter, and the
future worldwide season clock. A 4,096-cell full-world/GPU performance gate and
manual interactive traversal have not been measured by this pass. ENG-113 and
ENG-114's existing overall statuses/gates are not closed by vegetation tests.
