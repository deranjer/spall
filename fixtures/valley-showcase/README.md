# Spall Valley Showcase

A playable 20 m by 14 m valley with a meandering stream, a second tributary, a pond,
and a destructible stone-and-timber dam. The scene includes all four current
tree assets and the sealed, doorway, and roofless lighting-room buildings.
Terrain and props are ordinary voxel content, so the sandbox uses its existing
Rapier character/body physics, tools, and destruction.

Run from the repository root:

```powershell
cargo xtask play --editor-scene fixtures/valley-showcase
```

Open the admin menu with F10. Its lighting buttons change between studio,
daylight, overcast, sunset, and night while the game is running. Flight mode
(F) moves a spectator camera and can be exited with F again. R recentres the
camera view. Walking and jumping use the authoritative character physics.

| Input | Action |
| --- | --- |
| WASD | Walk |
| Mouse | Look around |
| Space | Jump |
| Escape | Release the mouse |
| Left click | Capture the mouse |
| F1 | Toggle terrain visibility |
| F2 | Toggle detached bodies |
| F3 | Toggle capsule debug markers |
| F4 | Cycle render debug views |
| F5 | Toggle visibility-aware skylight |
| F6 | Toggle diffuse bounce |
| F10 | Open or close the admin menu |
| F | Toggle spectator flight |
| Ctrl / Space | Descend / ascend while flying (Space jumps while walking) |
| R | Reset the camera view to the player |

The admin menu's "Water" section controls the reservoir behind the dam:

- The reservoir starts dry — its spring is off, so there is nothing behind
  the dam at first. **Spring off / normal / fast / max** pick the fill rate:
  each step up refills a bigger authored footprint on the spring's ledge (not
  the same cells more often — a spring cell is already full after one step,
  so only more source area actually fills faster). Picking off stops the
  fill where it is (already-placed water keeps flowing; no more is added).
- **Open dam gate / Close dam gate** cuts (or refills) a straight notch
  through the wall's crest, dead centre under the footbridge — exactly where
  the dam's old permanent spillway notch was, which lines up with the plunge
  pool and the river channel below — as ordinary terrain edits — real holes
  and real stone, replicated to every client, not a scripted animation. It's
  a shallow overflow, like that old spillway (now sealed) used to be, not a
  hole partway down the wall: it only spills once the reservoir is nearly
  full, straight over the top into the river below, rather than draining the
  basin through a void. The gate starts closed, so the lake keeps rising past
  its old overflow height until you open it.
- **Reset world** rebuilds the whole scene (terrain, water, and every player
  back at a spawn) from these files, so a reset also closes the gate and
  empties the reservoir again.

The stream and pond are authored as blue voxel surfaces in this scene. They
are static solid display geometry. Live two-phase fluid flow remains available
in the separate local inspection viewer:

```powershell
cargo run --release -p sandbox --features client --bin sandbox-client -- --grid-fluid-demo
```

That water viewer has editable reservoir, canal, breach, basin, and tunnel
fixtures. The authored water in this scene is also seeded into the server-owned
fluid grid and advanced on authoritative simulation ticks. Client water
replication/presentation and durable water saves are still pending.

Regenerate the project assets and scene:

```powershell
cargo run -p spall_editor --example valley_scene
```
