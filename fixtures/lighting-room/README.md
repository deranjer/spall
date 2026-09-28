# Lighting room (playable lighting test scene)

A 40 m lawn with three identical stone houses (6 m interior, one-voxel walls,
3 m high) that differ only in how they open to the sky:

| House | Opening |
| --- | --- |
| left (`House (sealed)`) | none: sealed walls and roof |
| middle (`House (doorway)`) | a 1.5 m x 2.5 m doorway facing the spawn |
| right (`House (roofless)`) | no roof |

The player spawns at the lawn centre, facing the houses. Sky lighting should
reach the roofless house, spill through the doorway and fall off, and never
enter the sealed one. The scene is saved with the `daylight` environment.

```powershell
cargo xtask play --editor-scene fixtures/lighting-room
```

In the window: `F4` cycles debug views (albedo, normals, depth, shadow cascades,
shadow visibility, sky visibility, roughness), `F5` switches visibility-aware
skylight off and on (off = the old unconditional ambient), `F6` switches the
diffuse bounce (colour bleed from sunlit surfaces, emissive light) off and on,
`F1`/`F2`/`F3` toggle
terrain, bodies and the collision capsule. Digging a wall or roof with the tool
changes the lighting after the next terrain rebuild. The doorway house holds a
1 m emissive lamp (sandbox `LAMP`), so its interior is lit by the lamp alone at
night and shows F6's bounce.

Regenerate the project with:

```powershell
cargo run -p spall_editor --example lighting_room
```

Evidence for this scene is in `docs/reports/ENG-96.md`.
