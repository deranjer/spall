# Woodshed

A 3 x 4 m plank shed (ENG-256): pitched roof, a hinged door in the front wall,
a woodpile along the left wall and a chopping block in the back-right corner.
Not yet placed on the 1 km map (ENG-251). Play it alone with:

```powershell
cargo xtask play --editor-scene fixtures/shed
```

`assets/shed.spvox` is the building (25 cm cells, origin at the back-left of the
footing). `assets/door.spvox` is the cabin's door unchanged, so the shed uses
`sandbox::game::CABIN_DOOR` (6 x 10 cells, hinge at the lower-left cell, opens
from +X toward +Z). In this fixture the hinge is cell (27, 4, 24).

## Axe spawn marker

The scene (`scenes/main.ron`) holds a voxel-less entity named exactly
`axe_spawn`. Its `transform.translation` is the axe's rest point in scene
metres: the centre of the chopping block's top face. The sandbox importer skips
entities without a voxel asset, so the marker has no terrain effect.

To place the shed elsewhere, take `translation - shed placement` (here
`(8.25, 1.5, 9.0) - (6.0, 0.5, 6.0) = (2.25, 1.0, 3.0)` m) as the offset from the
`shed.spvox` entity's translation, and add it to the new placement. A later
scenario ticket reads the entity by name from the scene file.

Regenerate with `cargo run -p spall_editor --example shed`; tests:
`cargo test -p sandbox --test shed`.
