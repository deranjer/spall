# Simple creatures (2026-10-20)

Audit pattern 5: animals became plain server-authoritative entities
(`CreatureRecord`: id, species, pose, locomotion tag, clip phase). Corpse,
fracture, exact voxel ownership, articulated physics and animal persistence were
removed. Details: [animal lifecycle](../animal-lifecycle.md).

Version bumps: `PROTOCOL_VERSION` 14 -> 15; `STORE_SCHEMA_VERSION` 6 -> 7.

Net lines vs `ab3f6e60` (added/removed, replica.rs split moves ~1,000 lines each way):
physics +25/-18,252; protocol +140/-9,479; sim +403/-9,377; server +258/-2,046;
client +1,182/-1,910; store +3/-228; core +32/-209; sandbox +161/-410; docs +239/-1,289.

Behaviour changes: crows are swept as a capsule instead of an exact articulated
pose; animals no longer collide with rigid bodies; client no longer needs a
creature catalog. Saving a world with animals is still refused.

Live check (release, natural 1024 world, `--animals 12`, headless client over QUIC,
1800 ticks): 18 creatures replicated, 3,171 motion entries applied, max presented
displacement 25.6 m; deer visible and animated in a windowed screenshot.
