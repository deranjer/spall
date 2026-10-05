# ENG-122 candidate component amounts: format decision

Experimental, not registered in `WireTag`, `WaterState`, store checkpoints or
journals. This payload is only a proposed replacement for the amount/ledger
arrays within a future explicitly versioned canonical restart state.

The fine MAC phase/pressure reference does not change this decision. Its
increment-7 channel failure is resolved by increment 8's paired-transfer
repair, but conservative momentum accuracy remains unaccepted. The numerical
candidate is cloned only for atomic in-memory steps;
that clone is not a restart payload or a durable transaction. No reference
pressure, velocity, air-model or runtime topology layout is serialized.

## Increment 5: component totals cannot recover phase placement

The new experimental `PhaseWater` retains horizontal water fractions at fine
voxel locations. Opposite pools inside the same open coarse component can have
identical component amounts, yet different wet basins and directional face
apertures. Tests demonstrate both ambiguities. Consequently the SCWA version-1
candidate described below is **insufficient to recover this phase state**,
even when its geometry hash and every amount/capacity are valid.

Do not reconstruct fine fractions from this payload by bottom filling, splitting
by capacity, or treating a shared air connection as a shared water level. Coarse
amounts remain a useful summary, but phase placement is canonical information.
A future restart contract must explicitly version and losslessly retain the
accepted phase representation (including its geometry/coordinate identity),
alongside amounts, trapped ledgers and the other full-domain recovery state.
Dynamic basin anchors are derived snapshot labels, not stable persistent IDs.

No new codec, implicit SCWA conversion, wire tag or production/save integration
is implemented in increment 5. The phase reference is experimental; pressure,
momentum and interface accuracy must pass before freezing its restart format.

## Original experimental component amount payload

`spall_protocol::component_water` owns the pure codec. Simulation algorithms
do not depend on protocol. Component keys are fine global anchor coordinates,
sorted strictly by (z,y,x), with separate water and trapped amounts in m3.
No runtime component indices, ECS IDs, native layouts or enum discriminants are
encoded. Opposite sides of one coarse-cell wall retain independent amounts.

All numeric fields are little endian. Floating values use explicit IEEE-754
f64 bits. Header length is 90 bytes; each component record is 44 bytes:

| Field | Encoding |
| --- | --- |
| Magic and payload version | ASCII SCWA; u16 version 1 |
| Fine origin | 3 x i64 |
| Fine dimensions | 3 x u32 |
| Coarsening factor | u32, 1 through 8 |
| Voxel size in metres | u64 f64 bits |
| Exact geometry identity | 32-byte BLAKE3 hash |
| Component count | u32, at most 200,000 and fine voxel count |
| Each component anchor | 3 x i64 |
| Each open voxel count | u32, 1 through factor cubed |
| Each water and trapped amount | 2 x u64 f64 bits |

Geometry identity hashes, in order: ASCII `Spall open component geometry v1`
plus a zero byte; origin; dimensions; factor; voxel size bits; one byte per
fine voxel (0 solid, 1 open), x fastest then y then z. All integers use LE.
The owner computes this identity from its immutable resident terrain snapshot,
not from supplied payload bytes. Grid alignment, scale and every fine occupancy
decision participate; matching anchors and capacities alone are insufficient.

Decode checks the fixed header, supported version, count ceiling, exact byte
length, expected geometry hash and domain validity before allocating records.
Amounts must be finite, nonnegative and within open capacity; trapped amounts
must be finite and nonnegative. Duplicate/unsorted/out-of-domain anchors and
invalid capacities fail. The owner must additionally compare **every** anchor
and capacity against rebuilt geometry, compare origin/dimensions/factor/scale
against the owning domain, and require complete component coverage
before atomically applying any recovered state. The codec cannot infer geometry
membership or prove these records exhaust it.

This is not a complete recovery adapter. Future canonical state must retain
configuration, source/gate/sink settings, domain identity, frame/fluid time,
source/drain/outflow totals and all trapped material atomically with terrain.
Pressure and velocity may reset only under the existing documented recovery
policy. Geometry changes require validated overlapping-space remapping and
displacement before a new hash/state pair is committed. Wire presentation can
aggregate amounts but cannot become canonical recovery data.

Old schema-1 saves have a single coarse amount and ledger. A cell with multiple
open spaces has no uniquely recoverable allocation. Migration must reject that
ambiguity or require a separately validated explicit conversion; it must not
divide amounts by guessed proportions. No implicit conversion is implemented.

Tests exercise independent wall-side bytes at equal coarse totals, exact round
trips with negative coordinates and trapped amounts, every truncated prefix,
count overflow, unknown versions, extra bytes, geometry mismatch, NaN/infinite/
negative/over-capacity amounts and malformed anchors. Geometry hash tests change
membership at equal open count and change origin/factor independently.
