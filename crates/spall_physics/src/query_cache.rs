//! A small, revision-aware `NativeVoxels` collider scoped to just a
//! character's own movement-query region (ENG-69 round 17).
//!
//! **Why this exists:** ENG-69 rounds 15-16 confirmed, through the real
//! `PredictedPlayer` reconciliation path, that a `MergedCuboids`-vs-
//! `NativeVoxels` representation mismatch between client and server produces
//! a small (~0.075 m), real, felt jitter whenever a character's sweep grazes
//! a `MergedCuboids` box seam — and that forcing *both* sides to
//! `NativeVoxels` eliminates it completely (`g1_tower_strafe_trace_with_
//! matched_representations`: 0.0000 m across 1250 reconciled events). But a
//! full-terrain `NativeVoxels` collider is infeasible for the *server*: it's
//! one whole-volume body with no regional split, and `NativeVoxels`' own
//! `MAX_ACTIVE_COLLIDER_CELLS` feasibility gate (bounding per-tick rebuild
//! cost) is tens of millions of cells too small for a terrain-scale world
//! (round 15's finding).
//!
//! [`CharacterQueryCache`] is the narrower alternative that gate was
//! designed to leave room for: instead of one `NativeVoxels` collider over
//! the *whole* terrain, build one over just a small, moving window centred
//! on wherever a character is currently querying. The window is sized with
//! enough margin that a character's sweep, autostep, and ground-snap never
//! actually reach its edge before it gets rebuilt — so from the character's
//! point of view it behaves exactly like a full `NativeVoxels` collider
//! would, at a bounded, small cost per rebuild instead of a whole-terrain
//! one. The general terrain collider (whatever [`crate::collider`]'s policy
//! picks — `MergedCuboids` for any terrain-scale world) is **untouched**:
//! this cache is additive, for character queries specifically, not a
//! replacement for the terrain collider every other query still uses.
//!
//! **Status: validated prototype, not yet integrated.** This module proves
//! the cache tracks a full-`NativeVoxels` reference exactly across window
//! rebuilds, edits, and prediction-replay-style position jumps
//! (`tests` below), and measures its cost. It does **not** yet:
//! - hook into `ClientPhysics`/`SimWorld`'s real edit-notification path (the
//!   `revision` parameter here is caller-supplied, not wired to a real
//!   commit/replication event yet);
//! - exclude the whole-terrain collider from a character's query when both
//!   coexist in the same [`crate::PhysicsWorld`] (needed to avoid a double
//!   hit once this actually runs alongside the server's normal terrain body
//!   — the validation tests below deliberately use a `PhysicsWorld` with
//!   only the window collider present, sidestepping this rather than
//!   solving it);
//! - preserve dynamic-body collisions through that same coexistence problem
//!   (today's tests don't exercise dynamic bodies at all).
//!
//! Those three are exactly the remaining "integrating it" work.

use std::time::Duration;

use spall_core::GlobalCell;
use spall_voxel::Volume;

use crate::character::{CharacterMove, CharacterParams};
use crate::collider::Representation;
use crate::occupancy::{ExtractError, OccupancyGrid};
use crate::world::{BodyId, BodyKind, BodySpec, PhysicsWorld};

/// Half-width (metres) of the cached window's cube, centred on the last
/// query position. Must comfortably exceed, in one tick, the sum of: the
/// capsule's own half-extents (`CharacterParams::DEFAULT`: ~0.3 m radius +
/// ~0.85 m half-height), [`crate::character::tuning::MAX_STEP_M`] (0.5 m
/// autostep reach), [`crate::character::tuning::GROUND_SNAP_M`] (0.2 m), and
/// one tick's desired movement (`WALK_SPEED_M_S / 60` ≈ 0.075 m at a walk,
/// more under a fast fall) — with slack left over so [`REBUILD_MARGIN_M`]
/// triggers a rebuild well before any of that could reach the window's
/// actual edge. `4.0 m` is several times that sum.
pub const WINDOW_RADIUS_M: f32 = 4.0;

/// Rebuild once the query position comes within this distance (on any axis)
/// of the current window's edge. Chosen so the *next* tick's worst-case
/// reach (see [`WINDOW_RADIUS_M`]'s doc — roughly 2 m even generously
/// counting autostep + snap + capsule extent) still lands comfortably inside
/// the *old* window during the tick the rebuild is triggered, before the
/// rebuilt window takes over on the following query.
pub const REBUILD_MARGIN_M: f32 = 1.5;

/// A small, revision-aware `NativeVoxels` collider tracking one character's
/// movement queries — see the module doc for the design and its current
/// (prototype, not-yet-integrated) status.
pub struct CharacterQueryCache {
    body: Option<BodyId>,
    centre_m: [f64; 3],
    revision: u64,
    cell_m: f32,
}

impl Default for CharacterQueryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CharacterQueryCache {
    pub fn new() -> Self {
        Self {
            body: None,
            centre_m: [f64::NAN; 3],
            revision: u64::MAX,
            cell_m: 0.0,
        }
    }

    /// The window centre's current radius of coverage, for tests that want
    /// to assert on it directly rather than through behaviour.
    pub fn window_centre_m(&self) -> Option<[f64; 3]> {
        self.body.map(|_| self.centre_m)
    }

    /// Ensures the cached window covers `position_m` with margin and matches
    /// `revision`, rebuilding from `volume` first if not. `cell_m` is
    /// `volume`'s cell size in metres. Returns the window's [`BodyId`]
    /// (`None` if the window turned out to hold no solid cell at all — open
    /// air, nothing to collide with) and, when this call actually rebuilt,
    /// the rebuild's cost (`None` on a cache hit — nothing to measure).
    ///
    /// `revision` is an opaque, caller-supplied token: any change forces a
    /// rebuild regardless of position. Wiring this to a real edit/commit
    /// notification is part of "integrating it" (module doc) — for now the
    /// caller decides what counts as "changed".
    pub fn ensure_covers(
        &mut self,
        world: &mut PhysicsWorld,
        volume: &Volume,
        cell_m: f32,
        position_m: [f64; 3],
        revision: u64,
    ) -> Result<(Option<BodyId>, Option<Duration>), ExtractError> {
        let needs_rebuild = self.body.is_none()
            || revision != self.revision
            || cell_m != self.cell_m
            || Self::outside_safe_zone(self.centre_m, position_m);
        if !needs_rebuild {
            return Ok((self.body, None));
        }
        let cost = self.rebuild(world, volume, cell_m, position_m, revision)?;
        Ok((self.body, Some(cost)))
    }

    /// [`Self::ensure_covers`] followed by a sweep through the resulting
    /// window — the whole point of this cache from a caller's perspective:
    /// a drop-in replacement for [`PhysicsWorld::sweep_character`] scoped to
    /// a small window instead of the whole terrain. Returns the sweep result
    /// and, when this call rebuilt the window, that rebuild's cost.
    #[allow(clippy::too_many_arguments)]
    pub fn sweep(
        &mut self,
        world: &mut PhysicsWorld,
        volume: &Volume,
        cell_m: f32,
        params: CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        revision: u64,
    ) -> Result<(CharacterMove, Option<Duration>), ExtractError> {
        let (_, rebuild_cost) = self.ensure_covers(world, volume, cell_m, position_m, revision)?;
        let moved = world.sweep_character(params, position_m, desired_translation_m, dt_s);
        Ok((moved, rebuild_cost))
    }

    fn outside_safe_zone(centre_m: [f64; 3], position_m: [f64; 3]) -> bool {
        let safe = f64::from(WINDOW_RADIUS_M - REBUILD_MARGIN_M);
        (0..3).any(|axis| (position_m[axis] - centre_m[axis]).abs() > safe)
    }

    fn rebuild(
        &mut self,
        world: &mut PhysicsWorld,
        volume: &Volume,
        cell_m: f32,
        position_m: [f64; 3],
        revision: u64,
    ) -> Result<Duration, ExtractError> {
        let radius_cells = (f64::from(WINDOW_RADIUS_M) / f64::from(cell_m)).ceil() as i64;
        let centre_cell = [
            (position_m[0] / f64::from(cell_m)).floor() as i64,
            (position_m[1] / f64::from(cell_m)).floor() as i64,
            (position_m[2] / f64::from(cell_m)).floor() as i64,
        ];
        let min = GlobalCell::new(
            centre_cell[0] - radius_cells,
            centre_cell[1] - radius_cells,
            centre_cell[2] - radius_cells,
        );
        let max = GlobalCell::new(
            centre_cell[0] + radius_cells,
            centre_cell[1] + radius_cells,
            centre_cell[2] + radius_cells,
        );
        let grid = OccupancyGrid::from_region(volume, min, max)?;

        self.centre_m = position_m;
        self.revision = revision;
        self.cell_m = cell_m;

        if grid.solid_count() == 0 {
            // Open air everywhere in the window — nothing to collide with.
            // Matches `ClientPhysics::set_terrain`'s `lenient_occupancy`
            // convention: no collider at all rather than an empty one.
            if let Some(id) = self.body.take() {
                world.remove_collider(id);
            }
            world.step();
            return Ok(Duration::ZERO);
        }

        let cost = if let Some(id) = self.body {
            world.rebuild_collider(id, &grid, Representation::NativeVoxels)
        } else {
            let start = std::time::Instant::now();
            let id = world.add_body(BodySpec {
                kind: BodyKind::Fixed,
                representation: Representation::NativeVoxels,
                grid,
                cell_m,
                density_kg_m3: 1.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            self.body = Some(id);
            start.elapsed()
        };
        world.step();
        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::character::{CharacterState, step_character};
    use crate::world::PhysicsConfig;
    use crate::{Representation as Rep, fixtures};
    use spall_core::{MaterialId, PlayerInput, VolumeId};
    use spall_voxel::EditPlan;

    const DT: f32 = 1.0 / 60.0;
    const CELL_M: f32 = fixtures::CELL_M;

    /// A fresh `PhysicsWorld` holding one full-`NativeVoxels` body over the
    /// *whole* `volume` — the diagnostic reference every test below compares
    /// the windowed cache against. Infeasible in production at terrain scale
    /// (ENG-69 round 15) but fine for a test-scale g1 world.
    fn reference_world(volume: &Volume) -> (PhysicsWorld, BodyId) {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let grid = OccupancyGrid::from_volume(volume).unwrap().unwrap();
        let id = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Rep::NativeVoxels,
            grid,
            cell_m: CELL_M,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        world.step();
        (world, id)
    }

    fn rebuild_reference(world: &mut PhysicsWorld, id: BodyId, volume: &Volume) {
        let grid = OccupancyGrid::from_volume(volume).unwrap().unwrap();
        world.rebuild_collider(id, &grid, Rep::NativeVoxels);
        world.step();
    }

    /// Flat plain, far from both the tower (`z` 8.0-11.75 m) and the ramp
    /// (`z` 25.0-28.75 m) — a clean boundary-crossing test isolated from any
    /// real terrain feature, since a false positive here would mean the
    /// window mechanism itself has a bug, not that a real seam was crossed.
    const CLEAR_START_M: [f64; 3] = [5.0, 46.0 * 0.25, 50.0];

    fn walk_forward() -> PlayerInput {
        PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        }
    }

    #[test]
    fn window_tracks_the_full_native_voxels_reference_across_boundary_crossings() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let (ref_world, _ref_id) = reference_world(&volume);
        let mut cand_world = PhysicsWorld::new(PhysicsConfig::default());
        let mut cache = CharacterQueryCache::new();

        let params = CharacterParams::DEFAULT;
        let mut ref_state = CharacterState::at(CLEAR_START_M);
        let mut cand_state = CharacterState::at(CLEAR_START_M);
        let input = walk_forward();
        let mut rebuilds = 0usize;
        let mut max_gap = 0.0_f64;

        // 300 ticks at WALK_SPEED_M_S (4.5 m/s) covers ~22.5 m — several
        // times the window's ~5 m safe-zone diameter
        // (`2 * (WINDOW_RADIUS_M - REBUILD_MARGIN_M)`), so this genuinely
        // exercises multiple rebuilds, not just one.
        for _ in 0..300 {
            ref_state = step_character(ref_state, input, DT, |pos, desired| {
                ref_world.sweep_character(params, pos, desired, DT)
            });
            cand_state = step_character(cand_state, input, DT, |pos, desired| {
                let (mv, cost) = cache
                    .sweep(
                        &mut cand_world,
                        &volume,
                        CELL_M,
                        params,
                        pos,
                        desired,
                        DT,
                        0,
                    )
                    .expect("window build over flat, fully-resident terrain");
                if cost.is_some() {
                    rebuilds += 1;
                }
                mv
            });
            let dx = ref_state.position_m[0] - cand_state.position_m[0];
            let dy = ref_state.position_m[1] - cand_state.position_m[1];
            let dz = ref_state.position_m[2] - cand_state.position_m[2];
            max_gap = max_gap.max((dx * dx + dy * dy + dz * dz).sqrt());
        }

        eprintln!(
            "boundary-crossing walk: {rebuilds} window rebuilds over ~22.5 m, max gap vs \
             reference {max_gap:.6} m"
        );
        assert!(
            rebuilds >= 3,
            "expected several window rebuilds over a ~22.5 m walk, got {rebuilds} — the test \
             isn't exercising boundary crossings as intended"
        );
        assert!(
            max_gap < 1.0e-4,
            "windowed cache diverged from the full-NativeVoxels reference by {max_gap:.6} m \
             during an ordinary flat-ground walk — the window/margin sizing has a bug"
        );
    }

    #[test]
    fn window_tracks_the_reference_through_a_mid_walk_edit() {
        let mut volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let (mut ref_world, ref_id) = reference_world(&volume);
        let mut cand_world = PhysicsWorld::new(PhysicsConfig::default());
        let mut cache = CharacterQueryCache::new();

        let params = CharacterParams::DEFAULT;
        let mut ref_state = CharacterState::at(CLEAR_START_M);
        let mut cand_state = CharacterState::at(CLEAR_START_M);
        let input = walk_forward();
        let mut revision = 0u64;

        let step = |ref_world: &mut PhysicsWorld,
                    cand_world: &mut PhysicsWorld,
                    cache: &mut CharacterQueryCache,
                    volume: &Volume,
                    revision: u64,
                    ref_state: &mut CharacterState,
                    cand_state: &mut CharacterState| {
            *ref_state = step_character(*ref_state, input, DT, |pos, desired| {
                ref_world.sweep_character(params, pos, desired, DT)
            });
            *cand_state = step_character(*cand_state, input, DT, |pos, desired| {
                cache
                    .sweep(
                        cand_world, volume, CELL_M, params, pos, desired, DT, revision,
                    )
                    .expect("window build")
                    .0
            });
        };

        // Walk 60 ticks (~4.5 m) on the original geometry.
        for _ in 0..60 {
            step(
                &mut ref_world,
                &mut cand_world,
                &mut cache,
                &volume,
                revision,
                &mut ref_state,
                &mut cand_state,
            );
        }
        let before_dx = ref_state.position_m[0] - cand_state.position_m[0];
        assert!(
            before_dx.abs() < 1.0e-4,
            "cache already disagreed with the reference before the edit: {before_dx}"
        );

        // A solid wall, 2 m ahead of the current position, spanning well
        // past the capsule's width on z — should stop both runs at the same
        // point once they reach it.
        let wall_x_cell = ((ref_state.position_m[0] + 2.0) / f64::from(CELL_M)) as i64;
        volume
            .apply_edit(&EditPlan::filled_box(
                volume.id(),
                // y 46..53 cells (11.5-13.25 m, from ground to ~1.75 m tall
                // — well over MAX_STEP_M so it genuinely blocks rather than
                // getting auto-stepped), z 196..204 cells (49.0-51.0 m, a
                // full metre either side of the z=50.0 m walking line).
                spall_core::GlobalCell::new(wall_x_cell, 46, 196),
                spall_core::GlobalCell::new(wall_x_cell + 1, 53, 204),
                MaterialId(1),
            ))
            .expect("wall edit");
        rebuild_reference(&mut ref_world, ref_id, &volume);
        revision += 1; // tells the cache the geometry changed

        // Walk another 120 ticks (~9 m — enough to reach and stop at the new
        // wall, and enough travel for the cache to rebuild several more
        // times past the edit too).
        for _ in 0..120 {
            step(
                &mut ref_world,
                &mut cand_world,
                &mut cache,
                &volume,
                revision,
                &mut ref_state,
                &mut cand_state,
            );
        }

        let dx = ref_state.position_m[0] - cand_state.position_m[0];
        let dz = ref_state.position_m[2] - cand_state.position_m[2];
        let gap = (dx * dx + dz * dz).sqrt();
        eprintln!(
            "mid-walk edit: reference stopped at x={:.3}, cache-driven run stopped at x={:.3} \
             (gap {gap:.6} m)",
            ref_state.position_m[0], cand_state.position_m[0]
        );
        assert!(
            gap < 1.0e-4,
            "windowed cache disagreed with the reference by {gap:.6} m after a mid-walk edit"
        );
        // Both runs should actually have been *stopped* by the new wall,
        // not walked through it — otherwise this test would pass by
        // agreeing on the wrong (unblocked) answer.
        let travelled = cand_state.position_m[0] - CLEAR_START_M[0];
        assert!(
            travelled < 6.5,
            "the wall should have blocked forward travel well short of the full ~13.5 m the \
             180-tick walk would otherwise cover; travelled {travelled:.3} m"
        );
    }

    #[test]
    fn replay_style_rewind_past_a_window_boundary_stays_correct() {
        // `PredictedPlayer::reconcile` rebases to an older authoritative
        // state and replays recorded inputs forward from there — a real
        // "jump backward, then walk forward again" pattern the cache's
        // window (centred on wherever the *live* character currently is)
        // has to handle correctly, not just ordinary forward walking.
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let mut cand_world = PhysicsWorld::new(PhysicsConfig::default());
        let mut cache = CharacterQueryCache::new();
        let params = CharacterParams::DEFAULT;
        let input = walk_forward();

        // Walk forward 200 ticks (~15 m) through the cache alone, recording
        // every state — this is "the predicted history".
        let mut history = vec![CharacterState::at(CLEAR_START_M)];
        let mut state = CharacterState::at(CLEAR_START_M);
        for _ in 0..200 {
            state = step_character(state, input, DT, |pos, desired| {
                cache
                    .sweep(
                        &mut cand_world,
                        &volume,
                        CELL_M,
                        params,
                        pos,
                        desired,
                        DT,
                        0,
                    )
                    .expect("window build")
                    .0
            });
            history.push(state);
        }

        // Rewind to tick 50 (well behind the cache's current window, which
        // is centred near tick 200's position — ~150 ticks * 0.075 m/tick
        // ≈ 11 m back, several window-widths away) and replay ticks 50..200
        // through the *same* cache instance, exactly as a reconcile would.
        let rewind_to = 50;
        let mut replay_costs = Vec::new();
        let mut replay_state = history[rewind_to];
        for _ in rewind_to..200 {
            replay_state = step_character(replay_state, input, DT, |pos, desired| {
                let (mv, cost) = cache
                    .sweep(
                        &mut cand_world,
                        &volume,
                        CELL_M,
                        params,
                        pos,
                        desired,
                        DT,
                        0,
                    )
                    .expect("window build after rewind");
                if let Some(c) = cost {
                    replay_costs.push(c);
                }
                mv
            });
        }

        // The replayed trajectory should land back on the *same* state the
        // original forward walk reached at tick 200 — replaying identical
        // inputs from an identical rebased state through identical geometry
        // is deterministic, window rebuilds included.
        let dx = replay_state.position_m[0] - history[200].position_m[0];
        let dy = replay_state.position_m[1] - history[200].position_m[1];
        let dz = replay_state.position_m[2] - history[200].position_m[2];
        let gap = (dx * dx + dy * dy + dz * dz).sqrt();
        let total_replay_cost: Duration = replay_costs.iter().sum();
        eprintln!(
            "replay rewind: {} rebuilds during the {}-tick replay, total {total_replay_cost:?} \
             ({:?}/rebuild avg), trajectory gap vs the original forward walk {gap:.6} m",
            replay_costs.len(),
            200 - rewind_to,
            total_replay_cost
                .checked_div(replay_costs.len().max(1) as u32)
                .unwrap_or_default(),
        );
        assert!(
            gap < 1.0e-4,
            "replaying through a rewind produced a different trajectory than the original \
             forward walk: {gap:.6} m gap — the cache is not deterministic across a rewind"
        );
        assert!(
            !replay_costs.is_empty(),
            "the rewind should have forced at least one rebuild (it starts ~11 m behind the \
             cache's current window) — got none, so this test isn't exercising the rewind path"
        );
        // Generous, and deliberately debug-build-safe (this codebase's own
        // convention — see `character::tests::perf_probe` and collision-
        // decision.md's "release build" note — timing assertions in a
        // `cargo test` debug build must tolerate a 10-30x slowdown vs.
        // release): a correctness/shape check that the rewind costs
        // *something* bounded, not a runaway rebuild-every-tick pathology,
        // not a tight timing budget. Run `--release` for numbers actually
        // comparable to collision-decision.md's own reference measurements.
        assert!(
            total_replay_cost < Duration::from_secs(2),
            "replay-with-rewind cost {total_replay_cost:?} for {} ticks looks pathological",
            200 - rewind_to
        );
    }

    #[test]
    fn single_window_rebuild_cost_is_small() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let mut cache = CharacterQueryCache::new();
        let (_, cost) = cache
            .ensure_covers(&mut world, &volume, CELL_M, CLEAR_START_M, 0)
            .expect("window build");
        let cost = cost.expect("first call always rebuilds");
        eprintln!(
            "single window rebuild: {cost:?} (cross-reference: collision-decision.md's own \
             32768-cell / 32^3 native-voxel measurement is ~1.3 ms on the reference host in \
             release — this window is the same order of magnitude, ~{}^3 cells; run this test \
             with `--release` for a comparable number, a debug build is 10-30x slower)",
            (2.0 * f64::from(WINDOW_RADIUS_M) / f64::from(CELL_M)).round() as i64
        );
        // Debug-build-safe (see the replay test's comment above) — catches
        // a true regression (an accidental full-terrain rebuild, an
        // infinite loop) without being tripped by debug/release variance.
        assert!(
            cost < Duration::from_millis(500),
            "a single window rebuild took {cost:?} — investigate before relying on this for a \
             per-tick budget"
        );
    }

    #[test]
    fn simulated_multiplayer_worst_case_rebuild_cost() {
        // Every player's window happening to need a rebuild on the exact
        // same tick is a pessimistic upper bound, not the expected steady
        // state (a player only rebuilds roughly once per ~5 m walked, not
        // every tick) — reported for the design record, not asserted as a
        // hard per-tick budget the way the single-rebuild test above is.
        const PLAYERS: usize = 32;
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let mut caches: Vec<CharacterQueryCache> =
            (0..PLAYERS).map(|_| CharacterQueryCache::new()).collect();

        // Spread players across the flat plain, clear of the tower/ramp,
        // each far enough apart that their windows don't overlap the same
        // cells (an 8 m grid comfortably clears the 4 m window radius).
        let mut total = Duration::ZERO;
        for (i, cache) in caches.iter_mut().enumerate() {
            // 8 columns x 4 rows, 6 m apart: x in [6, 48] m, z in [40, 58] m
            // — clear of the tower (z 8.0-11.75 m) and ramp (z 25.0-28.75 m)
            // — and, with the 4 m window radius added, comfortably inside
            // the 64 m x 64 m world on every side.
            let gx = (i % 8) as f64 * 6.0 + 6.0;
            let gz = (i / 8) as f64 * 6.0 + 40.0;
            let position_m = [gx, 46.0 * f64::from(CELL_M), gz];
            let (_, cost) = cache
                .ensure_covers(&mut world, &volume, CELL_M, position_m, 0)
                .expect("window build");
            total += cost.expect("first call always rebuilds");
        }

        eprintln!(
            "simulated multiplayer worst case: {PLAYERS} players all rebuilding on the same \
             tick, total {total:?} ({:?}/player avg) — one server tick's budget is 16.7 ms",
            total.checked_div(PLAYERS as u32).unwrap_or_default()
        );
    }
}
