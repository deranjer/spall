# ENG-269 winter weather and animal tuning

Ordinary tuning ticket (no formal gate). Measured with the ignored test
`examples/sandbox/tests/winter_crossing_animals.rs` (release, "natural" preset,
seed 1, 1 km = 4096 cells, no player, no live water).

## Blizzards
Unit test, 400 days x 3 seeds, 20-minute day: asserts one blizzard onset per
1-2 days and at least 80 % of snowfalls medium or heavy. Analytic mean: 18 %
blizzard fronts, onset rate 0.148 per 5-minute front, about 1.7 days between
blizzards.

## Animals (1 km world, 1500 ticks)
Targets / spawned: deer 120/120, crow 60/60, bear 4/4, snow leopard 6/6,
wolf 12/12 (three packs), trout 12/0 (the test installs no live water; trout
need deep live water). A 256 m world fits only 98 deer (4000 attempt cap).

Tick cost (creature hooks only, two runs): spawn hook avg 60-83 us, worst
6.7-8.0 ms (births). Motion hook avg 0.7-0.9 ms; worst 30-52 ms during the
spawn phase, 3.2 ms worst in the second half of the run. Whole-server tick cost
was not measured.

## Wolves
`WOLF_BITE_DAMAGE` 20 -> 12 (nine bites to kill, was five); one hunt stays
capped at three bites (36 health); a pack leaves players alone 6 minutes after
a hunt (was 3). Killing a full-health player takes at least three hunts.
