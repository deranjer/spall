# Terrain tree assets

This folder is an openable Spall editor project. It contains the canonical
`.spvox` fixtures in `assets/` and a `Main` scene with both trees assigned to
entities. The palm and willow are placed 8 m apart.

Both assets use cell-size code `0` (0.25 m cells), a ground-level pivot, and
explicit display tints. They use the portable `wood.oak` and `foliage.oak`
material keys; the tints distinguish their appearance without changing
material identity.

The palm has a narrow, subtly bent trunk and eight long fronds. The willow has
a broad crown and hanging foliage curtains. Regenerate both files from the
repository root with:

```powershell
cargo run -p spall_editor --example terrain_trees
```

Open this folder in Spall Editor with **Open Project**. To recreate the project
and assets, run the command above. Pass an output directory as the first
argument to write the project elsewhere.
