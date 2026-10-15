# ENG-206/207/208 snow integration and playtest

## Implemented scope

Ordinary rain uses a mostly downward fall vector shared with streak orientation;
ordinary snow uses compact falling flakes. Blizzard retains stronger drift.
Exposed static terrain accumulates a thin, non-colliding snow coating. Actual
server-grounded travel creates alternating boot marks, shared between clients.
Cold clear weather preserves cover; snowfall refills tracks and warm weather
melts cover. Winter dormancy continues using the integrated seasonal clock.

F10 retains the weather profiles, nature audio controls and precipitation toggle,
and adds a separate ground-snow/track visibility toggle plus nearby snow counts.
Visibility toggles are local; accumulation and footprints remain authoritative.

## Changed areas

- Protocol/net: bounded versioned snow frames, epoch/sequence/reset fences.
- Simulation: sky exposure, exact fractional accumulation/melt, bounded discovery,
  contact/stride validation, track caps/refill/expiry and recovery state.
- Server/store: regional publication, exact checkpoint and geometry journal state,
  corruption checks and storage-only earlier topology/motion header migration.
- Client/render: terrain revision checks, greedy coating patches, clipped boot
  marks, bounded cache/geometry, F10 wiring and structured capture diagnostics.

## Measured checks

`cargo xtask check` passed: **1431 tests passed, 98 ignored, zero failures**.
This run includes the actual SQLite snow restart scenario. Later natural-world
extent/discovery/paging corrections receive additional focused verification;
the original full run is not represented as having tested those later changes.
Evidence: `.local/runs/eng198-checks/snow-workspace-final.log`.

`cargo test -p spall_server --test snow_replication -- --nocapture` passed in
20.39 s: two real QUIC clients share 16 surfaces and eight footprint IDs from
two separate paths, and reset clears both replicas. Actual SQLite restart
preserves exact Q32 snow continuation, removes transient tracks, accepts the
unchanged saved topology header 6 and rejects a foreign snow epoch.

Actual release precipitation captures in `.local/runs/eng207-angle/shots/`
were inspected. Actual generated-terrain capture in
`.local/runs/eng208-snow-native-b/shots/` showed 2112 nearby frames, 20036 snow
faces, 6328 merged patches and up to 18 mm depth with zero stale/hidden/capped
surfaces. The initial zero-frame failure was fixed by querying the explicit
authoritative terrain extent rather than requiring bounded voxel volumes.

The first 4096-cell-edge / **1024 m** winter run showed 14 rendered footprint
marks and up to 25 mm depth. It exposed lexical regional publication delaying
the surrounding view, prompting a nearest-first delivery correction. These
early captures alone do not accept final nearby coverage.

The corrected nearest-first 1 km tour completed and entered player-input mode.
It showed 2420 nearby frames, 103455 coated faces, 8189 merged patches, 12
footprint marks and up to 28 mm depth at the F10 capture. Nearby coating was
visually inspected. The 8192-patch cap was reached in the outer view (one
deferred surface); stale and unknown-face rejection counts were zero.
F10 displayed weather/nature audio, winter -8 C, ground snow enabled and the
nearby snow counts. The one-hour session subsequently expired normally during
a usage-limit pause; a refreshed session supplies the final close footprint view.

Final correction checks: `cargo test -p spall_sim snow --lib` **16 passed**;
`cargo test -p spall_server --test snow_replication --test snow_persistence --
--nocapture` **2 passed**; strict all-target/all-feature clippy for sim, server,
client, render and store **passed**; full formatting and diff checks **passed**.
The new network fixture helper was simplified to satisfy strict argument-count
lint. Logs: `snow-policy-final.log`, `snow-server-final.log`, `snow-lint-final.log`.

## Final close-up and live handoff

The refreshed 1024 m world is `.local/runs/eng208-1024m-final/`, launched with
the same release/winter command and a 216000-tick (one-hour) bound. The capture
tour ends in `menu;play`, releasing the cursor and leaving F10 open. It uses
Clear at -8 C after snowfall, preserving coating while allowing inspection.
The first snow capture now had 1104 nearby frames; after walking it showed 13
marks and up to 28 mm depth. Nature and weather audio remain enabled.

A second bounded live client joined this same server and captured the actual
path at `(463, 53.5, 536.5)` m, looking down. Its final capture showed 20 shared
tracks, 98832 coated faces and no stale/unknown-face rejections. The boot-shaped
grey marks are visibly present in
`.local/runs/eng208-1024m-closeup/shot-02.png`; this image was inspected. The
earlier camera looked behind the path; capture contact diagnostics now include
at most 16 authoritative contact cells/offsets/strengths to make that distinction
reviewable. This was a camera-placement correction, not a hidden render fix.

The second client exited normally and the main client/server remained open.
It used the existing launcher's 8192 MiB 1 km baseline admission budget after the
default 4096 MiB budget correctly refused the staged world. This is a local
visual run, not acceptance of a reduced-memory, impaired-link or join-time gate.
The fresh window has a one-hour limit and can be closed normally when finished.
Additional evidence: `snow-1024m-final.log`, `snow-closeup.log`, and passing
`cargo clippy -p spall_client --all-targets --all-features -- -D warnings` in
`snow-capture-lint.log`.

## Remaining limits

Snow is a visual coating and shaded footprint overlay, with no collision or
moving-body accumulation. Rendering is camera-local and explicitly capped.
Uniform weather temperature is used; regional climate modulation is deferred.
If several edits hit a still-undiscovered tile before its query, retaining the
latest exposure birth avoids retroactive coating but can lose some climate
credit on unchanged faces. That conservative case remains a documented limit.

Matched weather-off/on CPU/GPU, allocation, steady bandwidth, impaired-link
snow churn, full-world topology history and long-soak targets are **unmeasured**.
Ignored hardware/performance tests are not counted as passes. Live screenshot
counts and incidental frame rates do not establish the 1 ms owner / 2 ms GPU /
8 KiB per second targets. Next unblocked work is ENG-195's remaining combined
weather performance and destruction/streaming acceptance.
