# ENG-208 — Shared validated snow footprints

User authorized ground snow and footprints following the precipitation-angle
correction. Depends on ENG-206 authoritative snow and ENG-207 presentation.

## Movement policy

`spall_sim::footprints::MotionFootprints` consumes server capsule state after
movement, never a client stamp position. It proposes alternating feet after
0.65 m of grounded horizontal travel, offset 9 cm to either side of the facing
direction. A minimum six ticks between steps limits each player to ten candidates
per second. Residual distance is bounded; stationary observations do not emit
deferred marks. Grounded turns retain distance and rotate the foot placement.

Airborne movement, movement-epoch changes, observation gaps and position jumps
discard stride credit. Initial spawn/landing cannot emit a mark. Disconnect
cleanup removes the player's history, and reset clears the tracker. Spectator
flight holds the actual server player neutral, so camera movement creates no
footfalls. The snow owner must still validate each candidate against actual
terrain contact, authoritative exposed-face masks, depth and current revisions.

## Verification

`cargo test -p spall_sim footprints::tests --lib`: four passed. Scenarios cover
measured travel/alternating feet/turns, stationary and airborne movement/landing,
teleport/epoch/gap discontinuities, independent walkers/rate bound/disconnect.
An initial test-only import referred to `spall_core::InputSeq`; it was corrected
to the existing protocol type before the passing run. `cargo fmt -p spall_sim`
passed. Evidence: `.local/runs/eng198-checks/footfall-policy.log`.

This initial policy acceptance does not claim snow contact, shared network
replication, recovery, or GPU footprint acceptance. Integration evidence follows
after the ENG-206 snow dependency is delivered.
