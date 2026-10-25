# Timber cabin

Play from the repository root:

```powershell
cargo xtask play --editor-scene fixtures/cabin
```

Four authored player starts are inside the 8 x 10 m timber shell. The default
view faces the front door. Within 3 m, a label beside the door reads **E Open**
or **E Close**. Press **E** to turn it 90 degrees, then walk through the 1.5 m
wide, 2.5 m tall opening onto the covered porch and down the stone step to the lawn. Windows
are open apertures. The bed platform, bench, desk, lamp, hearth and chimney
are voxel furnishings. The hearth is unlit.
Everything solid uses the normal editable/destructible terrain path.

`assets/cabin.spvox` is the reusable building (25 cm cells), with its own
foundation, floor, porch, hearth and chimney. Bed, table, bench and lamp are
separate reusable assets with local origins and independent scene placements.
They can be moved or duplicated in the editor; runtime pickup is not implemented.
`door.spvox` is also separate. Its lower-left hinge cell records its open/closed
state, so damage, replication and saves use ordinary voxel transactions. The
door refuses a turn when its swing area contains terrain, a player or a body;
step back if necessary. Panel holes remain after opening/closing, and breaking
the hinge disables interaction. A wooden knob stands one cell proud of each face, 1 m up on the side away from the hinge; panel cells beside the leaf (one cell either side) turn with it. This first door opens from local +X toward +Z;
place it with identity rotation and scale in a quarter-metre scene.

`ground.spvox` supplies the lawn and
`starts.spvox` supplies separate non-solid player markers. The project opens
in Spall Editor. Regenerate with:

```powershell
cargo run -p spall_editor --example cabin
```

CPU traversal and connectivity checks: `cargo test -p sandbox --test cabin`.

