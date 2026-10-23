# Column water: dams at sleeping regions, far springs (2026-10-22)

Session: 53 min on a 1 km natural world (seed 1, commit 51c8b35e), mostly idle,
rain at night. A body of water stood 2-3 m above the ground with a flat top and a
straight wall; the server log showed active water regions climbing from 130 to
1,137, water frames per 400 s falling from 12.6k to 4k and skipped fluid time
growing from 1 s to 136 s.

Measured with `cargo run --release -p sandbox --example water_long_probe --
natural 1 4096 SECONDS [X_M Z_M]` (`FLY=10` flies a 600 m square loop at 10 m/s),
on one development PC with every core; the server's water worker has half of
them, so its steps take longer.

## Causes

1. **Dams at sleeping regions (the slab).** A sleeping region woke only when a
   single pipe into it carried more than the sleep flow (0.5 cells^3/s in
   generated worlds). A river entering over many border pipes, each under that,
   never woke it. Its first column took the water and passed none on, and the
   upstream region filled into a flat pool with a straight wall one column into
   the sleeper. A `spall_fluid` slope with a spring line made a 9-cell (2.25 m)
   pool in 240 s with 1 of 15 regions ever active. On the 1 km world (flight,
   old code) new walls of 1-1.9 m stood along whole edges of sleeping regions by
   500-800 s, including region (46, 48), where the player stood in the session.
2. **Far springs at full rate.** Throttled regions step one slice in 2, 4 or 8;
   springs still added water every slice, so a far spring piled up 1.36 cells
   deep against 0.48 near, and far rivers ran deeper and wider.
3. **Not a client or rain effect.** Rain adds no water to the solver, the server
   evicts no terrain in `xtask play`, and the walls were in the server's state.

## Fixes

- A sleeping region also wakes when one of its border columns stands half a cell
  above or below the column inside it (`BORDER_WAKE_HEAD`). Test:
  `column_regions::a_wide_trickle_into_a_sleeping_region_flows_on_instead_of_damming`
  (fails without the fix).
- A spring adds nothing on a slice its region rested. Test:
  `column_water::a_far_spring_runs_in_slow_motion_with_its_water` (fails without).

## Before and after (standing at (374, 388) m)

Milliseconds are wall time per step of the whole water tick (solver, springs,
frames), averaged over the 100 s before.

| water time | old: active / ms per step | fixed: active / ms per step |
|---|---|---|
| 300 s | 43 / 2.6 | 77 / 3.0 |
| 400 s | 54 / 2.6 | 89 / 3.0 |
| 900 s | 70 / 3.3 | 657 / 14.0 |
| 3,000 s | 136 / 4.1 | not run |

The old code stayed calm headless because its rivers stopped at dams: no drain
took water in 3,000 s, while in the game the dams gave way and drains first took
water at 1,597 s. With the fix the rivers reach the sea at about 480 s
(identical without drains), and the sea around the mouth wakes. That is the same
growth the session showed, earlier. It is not fixed here.

## Open

- Deep water is judged quiet by flux (ENG-185). Quiet by speed (flux per cell of
  depth): 366 active at 900 s, but a 3 m pool fell asleep 1.4 cm out of level.
  Speed only once the two ends are level (within 0.02 cells): 511. Neither is in.
- Generated water starts out of rest in places: at seeding 173 columns stand over
  a metre above an open neighbour (lakes over dry cave pockets, river reaches as
  5-cell stair-steps), held only by sleep.
- Not checked in the game window.
