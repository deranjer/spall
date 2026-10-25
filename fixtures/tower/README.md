# Lookout tower

A 9 x 9 m granite lookout tower (ENG-255), 22.5 m above the ground to its platform and
about 24 m to the beacon and rail. It is the goal of the Winter Crossing
scenario: reach the top. `--scenario-winter` places it near the north edge of
the 1 km map (ENG-251; see architecture.md). Play it alone with:

```powershell
cargo xtask play --editor-scene fixtures/tower
```

`assets/tower.spvox` is the building (25 cm cells, origin at the back-left of
the footing). `assets/door.spvox` is the cabin's door unchanged, so the tower
uses `sandbox::game::CABIN_DOOR` (6 x 10 cells, hinge at the lower-left cell,
opens from +X toward +Z). In this fixture the hinge is cell (39, 4, 24).

## Sections

Heights are metres above the ground-floor planks (which sit 0.5 m above the
ground). Each landing is a two-cell slab with a hole over the section below it.
Stairs are a solid wedge at the back (+Z) of the tower; ladders (material
`ladder`, four cells wide, climbed facing +X) are at the left (-X).

| section | kind   | from -> to (m) | notes                                     |
|---------|--------|----------------|-------------------------------------------|
| S1      | stairs | 0 -> 3         | 12 steps, 0.25 m rise, 0.5 m tread        |
| L1      | ladder | 3 -> 8         | 5 m                                       |
| S2      | stairs | 8 -> 11        |                                           |
| L2      | ladder | 11 -> 16       | 5 m                                       |
| S3      | stairs | 16 -> 19       |                                           |
| L3      | ladder | 19 -> 22       | final ladder to the platform              |

Stairs use a two-cell tread per one-cell rise (26 degrees), not 1:1. A 1:1
staircase (45 degrees) is above the capsule's slide angle and its corners defeat
Rapier's autostep slope check (the capsule stalls near the top of a flight), so
it is not walkable. Every ladder top is flush with its landing, with solid slab
behind it, more than 2 m of headroom above the landing edge and over 1 m of
clear floor in front.

Light slits in the walls light each section. The top platform has a rail (1 m
posts every six cells and two rails) and a beacon in its middle: a granite
plinth with a 2 x 2 x 3 column of the existing `emissive.lamp` material. No new
material was added; a stronger beacon would need engine work.

## Win zone marker

The scene (`scenes/main.ron`) holds a voxel-less entity named exactly
`win_zone`. Its `transform.translation` is `(12.5, 23.0, 10.5)` scene metres:
the centre of the zone on the platform surface (the platform top face is at
y = 23.0, and the cell above it is free with at least 2 m of headroom). The
zone is the disc of **radius 1.0 m** (horizontal distance, XZ) around that
point; a player whose feet are within it, standing on the platform (feet no more
than 1 m above and 0.25 m below `translation.y`), has reached the top. The
sandbox importer skips entities without a voxel asset, so the marker has no
terrain effect.

To place the tower elsewhere, take `translation - tower placement` (here
`(12.5, 23.0, 10.5) - (6.0, 0.5, 6.0) = (6.5, 22.5, 4.5)` m) as the offset from
the `tower.spvox` entity's translation and add it to the new placement. The scenario
reads the entity by name (`editor_scene::load_prefab`).

Regenerate with `cargo run -p spall_editor --example tower`; tests:
`cargo test -p sandbox --features client --test tower`.
