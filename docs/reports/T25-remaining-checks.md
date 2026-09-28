# T25 remaining-check evidence (2026-09-24)

This report records the audit of checklist items 5 and 6 and the acceptance-key
gap found while reviewing item 1.

## 5. Asset import against authored game fixtures

**Not complete.** The repository contains no shipped/authored game `.spvox`
asset or `content-v1.ron` manifest to exercise. `examples/sandbox/src/content.rs`
implements a bounded static, single-root importer and explicitly rejects
assemblies (`PART`), structural profiles (`STRC`), animation (`ANIM`), and
voxel display tints. Inventing a multi-part or animated game object here would
not test authored game meaning.

The versioned contract already exists in `docs/spvox-format.md` (SPVX major
version 1, minor 1). It defines `PART` hierarchy/transforms, `ANIM` tracks,
and `PALT` plus per-voxel tint slots. Before adding runtime support, the game
needs to commit a representative authored asset and state which of those
features it uses. Then the fixture can specify its expected hierarchy,
animation sampling, and/or tint composition through save/load and late join.
Until that fixture exists, unsupported forms continue to fail explicitly.

## 6. G2 capture review

The documented Vulkan settled still and motion commands completed on the
available **NVIDIA GeForce RTX 4080 SUPER / Vulkan** adapter:

```text
cargo xtask capture --scene g2-loop --output .local/runs/t25-g2-vulkan-loop
cargo xtask capture --scene g2-motion --output .local/runs/t25-g2-vulkan-motion
```

Both runs used 1920x1080; the settled loop measured 15 warm-up and 120 frames
per mode/scene, and the motion suite measured 120 frames per sequence. Vulkan
settled-loop GPU frame p95/p99 across scenes and modes was 0.37/0.37 to
3.14/6.48 ms; CPU encode p95 was 0.39–0.64 ms and serial client-frame p95 was
0.77–3.74 ms. The motion sequences had GPU p95/p99: static-noise
0.71/1.24 ms, moving-occluder 6.91/8.51 ms, and occluder-jump 6.18/6.76 ms.
Motion CPU encode p95 was 0.71–0.83 ms. `quality_flags` was empty. Visual
inspection of the Vulkan colored-room still showed the expected red/blue
walls, neutral floor, and warm central emitter; no obvious geometry or
lighting defect was visible in that still.

The existing D3D12 G2 evidence is also from this same RTX 4080 SUPER, as
recorded in `docs/reports/G2.md`: settled client-frame p95 estimate <=2.9 ms
and 120-frame motion checks without quality flags. Thus D3D12 and Vulkan have
both been exercised, but **distinct physical D3D12 and Vulkan adapters remain
unrun**. This host exposed only one known adapter; Windows display-device
inventory access was denied. The run outputs above contain `summary.json` and
image captures for follow-up visual review.

## Item 1 acceptance audit

The current harvest outbox event captures `PlayerId`, harvest `RequestId`, and
removed materials. Its progression-store dedupe key is `(PlayerId,
RequestId)`, and its event identity is derived from those values. The checklist
requires `(WorldId, TransactionId)`. The existing implementation supports
replay-safe awards for a request but does not satisfy that exact key contract;
do not treat item 1 as fully accepted until the journal's world and committed
transaction identity are carried into the versioned event and used by the
game-owned store.
