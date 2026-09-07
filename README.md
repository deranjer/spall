# Spall: custom voxel engine

Planning baseline: 2026-09-06. This repository currently contains specifications, not an implemented engine. Commands in these documents are requirements for future tooling.

Build a custom engine for one survival/building game: Minecraft/Vintage Story-style world interaction, detailed voxel materials and Teardown-inspired lighting, **full-world destruction and multiplayer from the foundation**. No editor, menus, or UI framework is required. A render window, direct controls, command-line tools, and automated scenarios are required.

User requirements are full-world destruction and multiplayer. The remaining numbers below are proposed engineering defaults, not confirmed product requirements or measured performance.

## Engine and game boundary

Spall is the engine. Keep one repository and Cargo workspace, with engine libraries under `crates/spall_*`, a shipped playable example package `sandbox` under `examples/sandbox`, and build tooling under `tools/xtask`. The actual survival game can later live under `games/survival`; do not scaffold it before it is needed.

The engine provides voxel simulation, destruction, rendering, physics, replication, persistence, and client/server runtime hosts. The sandbox supplies its material catalog, tool rules, scenes, and thin `sandbox-server` / `sandbox-client` binaries. Games depend on Spall; engine crates must never import sandbox or survival-game code. Add a small statically linked rules interface when real game behavior first requires it. No dynamic plugin system or generic framework is needed.

Earlier `ve_*` design names are now `spall_*`; the former `ve_game` role belongs to the sandbox example's game-rules module (`sandbox_game` in this plan), not an engine crate. The example is built and tested with the engine and gradually becomes the networked destruction sandbox described below.

## Recommended stack

| Area | Choice | Reason and boundary |
| --- | --- | --- |
| Language/build | Rust stable, edition 2024, Cargo workspace | One build system; explicit ownership and compiler-checked subsystem interfaces suit agent implementation. Pin the exact compiler and commit Cargo.lock in task T00. |
| Graphics | wgpu, WGSL shaders | Custom renderer with native D3D12 and Vulkan paths. Start with rasterized voxel surfaces; add compute-based voxel lighting. No dependency on experimental hardware ray queries. |
| Window/input | winit | Native render window and input only; no UI toolkit. |
| Math | glam | Engine-facing vectors/transforms; conversions to physics types stay inside the physics adapter. |
| Physics | rapier3d behind a narrow adapter | Use an existing rigid-body solver, character queries, and collision system. Prototype editable voxel colliders and compound cuboids before choosing the production representation. |
| Entity storage | hecs | Players, bodies, tools, and future creatures. Individual voxels are array entries, never entities. Explicit system order; no custom ECS. |
| Transport | Quinn/QUIC with rustls; Tokio in transport/I/O only | Reliable streams for topology and baseline data; unreliable datagrams for inputs and motion. Local single-player uses the same server rules. |
| Jobs | Rayon pool plus bounded queues | CPU meshing, topology analysis, collision builds, generation, and compression. Workers produce versioned results; they do not mutate the live world. |
| Data | Serde + TOML for authored definitions; explicit versioned binary DTOs with postcard | Human-editable materials/scenarios, compact wire records. Serialization derives do not replace a protocol specification. |
| Persistence | SQLite via rusqlite, compressed brick blobs via zstd | Transactional checkpoints and event journal without inventing a crash-safe region filesystem first. One writer; measure throughput in the persistence gate. |
| Integrity | BLAKE3 | Canonical topology, content manifest, and checkpoint hashes. Not a substitute for authentication. |
| Tools | clap, tracing, serde_json, image | CLI, structured logs/metrics, JSON scenarios, PNG captures. |
| Verification | cargo test, proptest, Criterion; GPU validation and captures | Exact CPU invariants, protocol failure tests, realistic performance scenarios, visual inspection. |

These are library choices, not a pre-existing game engine. We own voxel storage, destruction, structural rules, streaming, replication, rendering, persistence schemas, and game logic. We reuse platform plumbing and a physics solver.

Select mutually compatible **released stable** dependency versions in T00, compile a window/server/transport smoke test, then pin them. Do not copy APIs from latest documentation into a project locked to another version. Record enabled features and licenses in `docs/dependencies.md` when dependencies are installed. Do not create that file with guessed versions.

wgpu supports native D3D12 and Vulkan; its documented hardware ray-query extension is experimental and Vulkan-only at the time checked. The proposed baseline uses ordinary compute shaders for voxel tracing. [wgpu documentation](https://wgpu.rs/doc/wgpu/), [native features](https://wgpu.rs/doc/wgpu/struct.FeaturesWGPU.html).

Rapier documents voxel shapes, compound colliders, and a character controller. Actual edit cost, dynamic voxel contact stability, and large-collapse throughput remain prototype questions. [Collider documentation](https://rapier.rs/docs/user_guides/rust/colliders/), [character controller](https://rapier.rs/docs/user_guides/rust/character_controller/).

Quinn exposes both streams and unreliable unordered datagrams, matching the proposed split between world changes and motion. We still have to build replication, prioritization, admission control, and recovery. [Quinn connection API](https://docs.rs/quinn/latest/quinn/struct.Connection.html).

## Decisions that drive everything else

1. **Authoritative server from the first playable build.** Clients send intent. The server validates tools, chooses affected cells, creates bodies, simulates physics, and assigns IDs. A dedicated server must run without a GPU.
2. **Every in-bounds solid voxel can be modified and destroyed.** Terrain, buildings, placed blocks, and detached moving pieces use the same material/edit concepts. No terrain-only immutability shortcut.
3. **Separate topology from motion.** Which cells exist, their materials, and which body owns them must agree exactly. Floating-point movement is server state replicated to clients; do not require lockstep physics.
4. **Store solids in sparse fixed-size bricks.** Start with 32 cubed cells per brick, 25 cm terrain cells, and optional 6.25 cm local voxels for detailed objects after the core gate. Detached terrain retains its cell size and can span multiple bricks.
5. **Prove the hard parts in a small world.** First acceptance scene: 64 x 32 x 64 metres with two clients. Next: eight players in a 256 x 128 x 256 metre bounded world. Large streamed worlds follow measured success; an infinite world and unbounded active debris are not promised.
6. **Rendering quality has its own gate.** Flat cubes with ambient occlusion do not satisfy the lighting goal. Demonstrate indirect light, soft shadows, emissive illumination, and stable lighting during destruction in a small scene early.
7. **No silent correctness tradeoffs under load.** Queue/rate-limit expensive actions and expose overload metrics. Do not delete gameplay debris, classify unknown support as permanent support, or drop topology events to hit a frame-rate target.

All numbers, including voxel sizes, player count, and world size, become frozen compatibility choices only after the feasibility gates. Changing cell size later needs a new world format or explicit conversion.

## What full-world destruction means

The initial structural model uses six-face connectivity to explicit world-boundary support. Severing the last supporting connection releases a connected component as a voxel rigid body. Detached pieces can be hit, cut again, sleep, wake, and survive save/load and late join. Support crosses brick and storage-region boundaries. Merely reaching an unloaded brick never proves support.

World support is a documented boundary condition at the lower world plane, not an indestructible bedrock material: cells touching that plane can still be removed. Removing all of them can release the entire remaining connected solid. That is a worst-case performance test, not a reason to add hidden permanent anchors. A bounded world also needs a declared rule for bodies crossing its outer limits; initially reject outward player placement and persist out-of-bounds debris in a dormant external-body set.

Connectivity is the first collapse rule. It allows unrealistic long cantilevers while any connection remains. Material-dependent support/bond strength is a later required realism milestone, with an explicit approximate model. Neither milestone claims engineering-grade structural simulation or arbitrary real-time destruction of a whole continent.

## Delivery order

| Gate | Demonstration |
| --- | --- |
| G0: reproducible foundation | One command builds, tests, starts a server and two clients, runs a bounded scenario, and collects results. |
| G1: destruction + replication | Two clients cut through a cross-brick structure; it falls, gets cut in motion, and converges after injected packet loss. |
| G2: graphics feasibility | The same editable geometry shows soft shadows, indirect light, and emissive bounce in a stable camera capture and a moving scene. |
| G3: persistence + streaming | Join during a collapse; reconnect; save/restart; unload/reload structural neighbors without restoring removed terrain. |
| G4: multiplayer engine slice | Eight clients, bounded bandwidth, character prediction, realistic stress scenes, material strength, and sustained performance. |
| G5: larger game world | Measured streaming/LOD and multiple physics regions; then survival content, inventory, crafting, creatures, liquids, and fire. |

The first useful deliverable is a networked destruction sandbox. It must already exercise real terrain, moving voxel bodies, and the actual protocol.

## Read and assign work

- [Architecture and invariants](docs/architecture.md): subsystem ownership, voxel representation, structural destruction, physics, and renderer.
- [Protocol and persistence](docs/protocol.md): exact replication and recovery contracts.
- [Implementation tasks](docs/tasks.md): dependency-ordered, bounded assignments with acceptance tests.
- [Validation and operating contract](docs/validation.md): launch commands, fixtures, budgets, and gate evidence.
- [Agent working rules](AGENTS.md): requirements every implementation agent must follow.

Start with **T00 only**. Follow its dependency graph; do not hand one agent an instruction to implement the whole engine. The graphics, structural, and replication gate reviews require an experienced integrator even when smaller implementation tasks are delegated.

## Alternatives and tradeoffs

Rust adds learning cost, and wgpu limits access to some advanced native GPU features. If the first rendering gate establishes that mandatory hardware ray tracing or low-level GPU control is necessary, make an explicit renderer/backend decision before building the full pipeline. The fallback is C++20 + SDL3 + Vulkan + Jolt + CMake, with greater build, lifetime, and integration complexity. Do not maintain two stacks speculatively.

Unreal, Unity, Godot, and a full Bevy application are outside this custom-engine brief. Go remains useful for external tools, but a second implementation language is unnecessary initially. No plugin architecture, custom scripting VM, visual editor, or bespoke physics solver is part of the foundation.

This is substantial engine development. Full destruction, streamed persistence, multiplayer recovery, and modern lighting each contain research risk. Plan in testable gates rather than promising completion in a few short agent sessions. Re-estimate after G1/G2 using actual throughput and integration defects.

## Research notes

The recommendation is our proposed architecture, not a claim that another game uses this exact stack. Dennis Gustafsson describes the bandwidth and synchronization difficulties of destruction multiplayer and the importance of distinguishing structural changes from motion. That supports addressing replication before building content. [Teardown multiplayer development account](https://blog.voxagon.se/2026/03/13/teardown-multiplayer.html).

His rendering account discusses tracing in voxel space, motivating a lighting prototype that uses voxel data directly. We should judge our renderer against its own measured quality and performance. [From screen space to voxel space](https://blog.voxagon.se/2018/10/17/from-screen-space-to-voxel-space.html).

Other primary references: [winit](https://github.com/rust-windowing/winit), [hecs](https://github.com/Ralith/hecs), [Rayon](https://github.com/rayon-rs/rayon), [Serde](https://github.com/serde-rs/serde), [Jolt](https://github.com/jrouwe/JoltPhysics).
