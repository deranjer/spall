# Terrain forest (playable test scene)

A 40 m x 40 m grass lawn with 16 jittered trees cycling through all four tree
assets: `palm_tree`, `weeping_willow`, and the second-pass `palm_tree_2` and
`weeping_willow_2`. A clearing at the centre is the player spawn.

Open it in Spall Editor with **Open Project**, then press **Run** to build the
sandbox and play the scene in an interactive window (WASD, mouse, Space to
jump, Escape releases the cursor). From a terminal:

```powershell
cargo xtask play --editor-scene fixtures/terrain-trees-forest
```

Regenerate the project (needs `terrain_trees` and `terrain_trees_v2` run first,
since it copies their assets):

```powershell
cargo run -p spall_editor --example forest_scene
```

## Playback notes

- Placed assets become destructible terrain. Runtime cells carry a material and
  no per-cell tint, so an authored tint is kept as a *material variant*: `grass`
  -> sandbox grass, `dirt` -> dirt, `wood.oak` -> wood, `foliage.oak` -> the
  playground's green (the sandbox manifest has no leaf material yet),
  `stone.granite`, `sandstone`; each tinted cell takes the nearest of up to 12
  committed colour variants of its material (`sandbox::appearance`,
  `docs/reports/ENG-95.md`), so the authored greens survive digging and saves.
  The base mapping is `sandbox::editor_scene::material_mapping`.
- Tree count is bounded by the engine's exact whole-terrain collider budget
  (4096 greedy boxes, `spall_physics::MERGED_CUBOID_PRIMITIVE_BUDGET`). A 33-tree
  version of this scene was rejected by `plan_collider` (`TooLarge`, 6891 boxes),
  so denser forests need an engine change, not a bigger scene.
- Walking past the lawn edge falls into open air; there is no kill floor.
