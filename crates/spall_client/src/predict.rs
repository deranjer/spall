//! Client-side player prediction and reconciliation (T19).
//!
//! `docs/protocol.md`: "The local client keeps a bounded input/state history,
//! predicts capsule movement, and replays unacknowledged inputs after an
//! authoritative correction. Collision topology changes invalidate affected
//! history: restore the authoritative player state and rebuild prediction
//! against a known revision."
//!
//! [`PredictedPlayer`] runs the *same* [`spall_physics::step_character`] kernel
//! the server runs, against [`ClientPhysics`] — a physics world holding just the
//! terrain collider, rebuilt from the replica. Because physics is not lockstep,
//! the predicted state drifts from the authoritative one; [`PredictedPlayer::reconcile`]
//! snaps to each snapshot and replays the still-unacknowledged inputs, and the
//! residual is reported as a bounded correction.

use std::collections::{BTreeSet, VecDeque};

use serde::Serialize;
use spall_core::{BrickCoord, GlobalCell, MaterialId, PlayerInput, Tick};
pub use spall_physics::WindowStats;
use spall_physics::{
    BodyId, BodyKind, BodySpec, CharacterMove, CharacterParams, CharacterQueryCache,
    CharacterState, OccupancyGrid, PhysicsConfig, PhysicsWorld, Representation, step_character,
};
use spall_protocol::InputSeq;
use spall_voxel::{Sample, Volume};

/// Bounded predicted-input history depth.
pub const PREDICTION_HISTORY: usize = 128;
/// Terrain cell size, metres (the 0.25 m world grid).
pub const CELL_M: f32 = 0.25;
/// Cells per brick edge (`docs/architecture.md`'s fixed brick size).
const BRICK_CELLS: i64 = 32;

/// A physics world that mirrors only the replica's terrain collider, so the
/// predictor sweeps the capsule against the geometry the server used.
pub struct ClientPhysics {
    world: PhysicsWorld,
    terrain: Option<BodyId>,
    /// Bricks the collider currently installed on `terrain` was actually built
    /// from, as of the last [`set_terrain`](Self::set_terrain) — empty
    /// whenever nothing is resident yet. Residency streams bricks
    /// independently of each other, so this set is routinely non-contiguous
    /// for a step or two while the player walks (one trailing brick evicted,
    /// one leading brick still mid-reload); [`covers`](Self::covers) is what
    /// tells a caller whether *the player's own* brick is actually one of
    /// them, as opposed to merely "some geometry exists somewhere".
    resident_bricks: BTreeSet<BrickCoord>,
    /// ENG-69 round 18: [`Self::sweep`]'s own small window, tried before
    /// falling back to `terrain` (`spall_physics::query_cache`'s own doc has
    /// the full design/rationale — this is the client half of the same fix
    /// `spall_sim::world::SimWorld::advance_players` applies server-side).
    /// Kept *alongside* `terrain`, not instead of it: a window build fails
    /// outright (rather than silently approximating) whenever its margin
    /// would reach a cell the replica hasn't streamed in yet — common near
    /// the edge of the client's own residency radius, an everyday case here
    /// in a way it mostly isn't for the server's complete-knowledge terrain
    /// — so `terrain`'s existing, already-validated full-resident-set
    /// collider stays as the fallback for exactly that case.
    query_cache: CharacterQueryCache,
    /// Bumped every [`Self::set_terrain`] call — the `revision` token
    /// [`Self::query_cache`] uses to know its window is stale even when the
    /// player hasn't moved far enough to cross it on position alone (an
    /// edit landing inside an otherwise-unmoved window).
    revision: u64,
    /// Live counters proving [`Self::query_cache`] is actually in the
    /// sweep path, not just present and unused — ENG-69 round 18 asked for
    /// this explicitly after the round-17 prototype never got wired to
    /// anything live. Surfaced through [`Self::window_stats`] to the
    /// interactive HUD (`window.rs`'s `Hud::report`). The type itself lives
    /// in `spall_physics::query_cache` (re-exported here) so
    /// `spall_sim::world::SimWorld` can track the same shape server-side —
    /// see its own `window_stats` field.
    window_stats: WindowStats,
}

impl Default for ClientPhysics {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientPhysics {
    pub fn new() -> Self {
        Self {
            world: PhysicsWorld::new(PhysicsConfig::default()),
            terrain: None,
            resident_bricks: BTreeSet::new(),
            query_cache: CharacterQueryCache::new(),
            revision: 0,
            window_stats: WindowStats::default(),
        }
    }

    /// Live proof [`Self::query_cache`] is actually serving sweeps, not
    /// just present — see [`WindowStats`]'s own fields.
    pub fn window_stats(&self) -> WindowStats {
        self.window_stats
    }

    /// (Re)builds the terrain collider from `volume`. The caller gates this on a
    /// cheap dirty check (`ReplicaWorld::terrain_hash`).
    ///
    /// Unlike [`OccupancyGrid::from_volume`], this tolerates a *non-contiguous*
    /// resident set: it still takes the tight bounding box of every resident
    /// solid cell, but a cell whose brick is not (yet) resident is treated as
    /// empty — no collision — rather than failing the whole extraction. Under
    /// eviction/reload churn the resident set routinely has a gap for a step
    /// or two (one trailing brick evicted, one leading brick still mid-reload
    /// after a lossy repair round trip); failing the entire collider over that
    /// gap would blank out geometry that *is* loaded and correct, well away
    /// from the gap. An empty resident set (no solid cell anywhere) still
    /// drops the collider entirely so the capsule falls through a
    /// fully-removed floor.
    ///
    /// Always builds [`Representation::NativeVoxels`], **not**
    /// [`spall_physics::choose_representation`]'s budget-based pick, even
    /// though that budget is exactly what `spall_sim::collider::plan_collider`
    /// uses for the server's own terrain body. ENG-69 round 7 tried matching
    /// the server's policy (commit `196d9ff`, since reverted in spirit here)
    /// on the theory that a representation *mismatch* was the bug; a live
    /// retest with the same terrain came back with an unchanged ~0.15-0.2 m
    /// horizontal-only correction, disproving it — the terrain's greedy box
    /// count apparently never crossed the budget on either side, so both
    /// sides were already building `MergedCuboids`.
    ///
    /// The real issue is more fundamental than *which* representation: this
    /// grid's extent is the tight bounding box of whatever bricks happen to
    /// be resident *right now* (see the non-contiguous-resident-set note
    /// above), while the server's grid for the same logical terrain spans the
    /// complete, fixed volume. `greedy_boxes`' box-growth loop only stops
    /// early where the *next cell* isn't solid — and at this grid's edge,
    /// "not solid" and "not yet loaded" are indistinguishable from inside the
    /// array. For any solid region larger than the client's own streaming
    /// radius (an ordinary large flat floor easily qualifies), a box that
    /// would run further on the server's complete grid gets cut short here,
    /// planting a seam the server's collider does not have — anywhere within
    /// the resident set, not only at its visible boundary, because greedy
    /// merging is a global, order-dependent process (an earlier box's extent
    /// changes what is left `consumed` for a later one). `MergedCuboids`
    /// resolves that seam as a real geometric discontinuity a sliding
    /// kinematic character can catch on. `NativeVoxels` has no internal
    /// seams at all — parry's `Voxels` shape suppresses contact response
    /// between connected adjacent voxels — so while its own resident-boundary
    /// edge is still an (unavoidable, partial-knowledge) approximation, nothing
    /// *interior* to what the client has already loaded can produce a false
    /// seam the player might be standing on or walking across. This is
    /// heavier to rebuild than a small `MergedCuboids` compound would be
    /// (cost scales with the resident cell count, not the box count — see
    /// `spall_sim::collider`'s own doc on the same trade-off), but rebuilds
    /// are already bounded by [`crate::residency`]'s / the replica's own
    /// resident-cell ceiling, and correctness here matters more than shaving
    /// that cost.
    pub fn set_terrain(&mut self, volume: &Volume) {
        self.resident_bricks = volume.resident_brick_coords().into_iter().collect();
        match lenient_occupancy(volume) {
            Some(grid) => match self.terrain {
                Some(id) => {
                    self.world
                        .rebuild_collider(id, &grid, Representation::NativeVoxels);
                }
                None => {
                    let id = self.world.add_body(BodySpec {
                        kind: BodyKind::Fixed,
                        representation: Representation::NativeVoxels,
                        grid,
                        cell_m: CELL_M,
                        density_kg_m3: 1.0,
                        mass_properties: None,
                        translation_m: [0.0; 3],
                        linvel_m_s: [0.0; 3],
                    });
                    self.terrain = Some(id);
                }
            },
            None => {
                if let Some(id) = self.terrain {
                    self.world.remove_collider(id);
                }
            }
        }
        // Refresh the broad-phase BVH so the next character sweep sees the new
        // collider (the sweep runs no physics step of its own).
        self.world.step();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Sweeps the capsule one tick, preferring [`Self::query_cache`]'s own
    /// small `NativeVoxels` window (real terrain excluded, since the window
    /// replaces it exactly) over `terrain`'s whole-resident-set collider —
    /// falling back to the latter, unfiltered, whenever the window can't be
    /// built this call (see [`Self::query_cache`]'s own doc for why that
    /// happens routinely here, unlike server-side). `volume` is the
    /// replica's current terrain — the caller already holds/clones it each
    /// tick for the existing dirty check, so this asks for nothing new.
    pub fn sweep(
        &mut self,
        volume: &Volume,
        params: CharacterParams,
        feet_m: [f64; 3],
        desired_m: [f32; 3],
        dt_s: f32,
    ) -> CharacterMove {
        if let Some(terrain_id) = self.terrain
            && let Ok((Some(_window_id), cost)) = self.query_cache.ensure_covers(
                &mut self.world,
                volume,
                CELL_M,
                feet_m,
                self.revision,
            )
        {
            self.window_stats.window_sweeps += 1;
            if cost.is_some() {
                self.window_stats.window_rebuilds += 1;
            }
            return self.world.sweep_character_excluding(
                params,
                feet_m,
                desired_m,
                dt_s,
                &[terrain_id],
            );
        }
        // No window *this call* — either the cache couldn't build one (too
        // close to the residency edge) or it legitimately found no solid
        // cell nearby. Falls back to the whole-resident-set sweep, but must
        // also exclude `query_cache`'s own body if one still exists: a
        // failed `ensure_covers` returns early (`CharacterQueryCache::
        // rebuild`) *before* touching the cache's stored body, so a
        // previous call's successfully-built window can still be sitting in
        // `self.world`, unrefreshed, at a stale position — ENG-69 round 19
        // found this was never excluded from the fallback sweep, letting a
        // stale window silently double up with real terrain in the same
        // query.
        self.window_stats.terrain_fallbacks += 1;
        if let Some(stale_window_id) = self.query_cache.window_body_id() {
            return self.world.sweep_character_excluding(
                params,
                feet_m,
                desired_m,
                dt_s,
                &[stale_window_id],
            );
        }
        self.world.sweep_character(params, feet_m, desired_m, dt_s)
    }

    pub fn has_terrain(&self) -> bool {
        self.terrain.is_some()
    }

    /// Whether the brick under `feet_m` was actually resident (and so part of
    /// the collider) as of the last [`set_terrain`]. `has_terrain` alone
    /// cannot tell "the player's own footing is loaded" from "some other,
    /// unrelated patch of the world is loaded" — the collider body persists
    /// across a residency gap so a later refill can rebuild it, and stays
    /// "present" throughout. A caller predicting movement should hold rather
    /// than extrapolate through a point this returns `false` for: it means
    /// the client does not yet know whether that ground is solid, not that it
    /// has confirmed open air.
    pub fn covers(&self, feet_m: [f64; 3]) -> bool {
        let cell_m = f64::from(CELL_M);
        let cell = GlobalCell::new(
            (feet_m[0] / cell_m).floor() as i64,
            (feet_m[1] / cell_m).floor() as i64,
            (feet_m[2] / cell_m).floor() as i64,
        );
        self.resident_bricks.contains(&cell.split().0)
    }
}

/// See [`ClientPhysics::set_terrain`]. `None` when no resident brick holds a
/// solid cell at all (nothing to collide with yet, or a genuinely empty
/// world); `Some` grid otherwise, built over the tight bounding box of every
/// resident solid cell with non-resident cells inside that box left empty.
fn lenient_occupancy(volume: &Volume) -> Option<OccupancyGrid> {
    let coords = volume.resident_brick_coords();
    let mut min = [i64::MAX; 3];
    let mut max = [i64::MIN; 3];
    for c in &coords {
        let base = [c.x * BRICK_CELLS, c.y * BRICK_CELLS, c.z * BRICK_CELLS];
        for lz in 0..BRICK_CELLS {
            for ly in 0..BRICK_CELLS {
                for lx in 0..BRICK_CELLS {
                    let cell = GlobalCell::new(base[0] + lx, base[1] + ly, base[2] + lz);
                    if let Ok(Sample::Filled(_)) = volume.sample(cell) {
                        min[0] = min[0].min(cell.x);
                        min[1] = min[1].min(cell.y);
                        min[2] = min[2].min(cell.z);
                        max[0] = max[0].max(cell.x);
                        max[1] = max[1].max(cell.y);
                        max[2] = max[2].max(cell.z);
                    }
                }
            }
        }
    }
    if min[0] > max[0] {
        return None;
    }
    let dims = [
        (max[0] - min[0] + 1) as u32,
        (max[1] - min[1] + 1) as u32,
        (max[2] - min[2] + 1) as u32,
    ];
    let cells = dims[0] as usize * dims[1] as usize * dims[2] as usize;
    let mut solid = vec![false; cells];
    let mut material = vec![MaterialId::AIR; cells];
    for gz in 0..dims[2] {
        for gy in 0..dims[1] {
            for gx in 0..dims[0] {
                let cell =
                    GlobalCell::new(min[0] + gx as i64, min[1] + gy as i64, min[2] + gz as i64);
                if let Ok(Sample::Filled(m)) = volume.sample(cell) {
                    let idx = (gx + dims[0] * (gy + dims[1] * gz)) as usize;
                    solid[idx] = true;
                    material[idx] = m;
                }
                // `Sample::Empty` and `Sample::Unknown` both leave this cell
                // as air: real air collides with nothing, and an unresident
                // cell must not be treated as solid either — but nor may it
                // fail the whole box the way `OccupancyGrid::from_region`
                // rightly does for callers who need a *complete* region (a
                // body's own geometry, a checkpoint capture). `covers` is
                // this module's answer to "was this cell actually resident",
                // kept separately from the grid itself.
            }
        }
    }
    OccupancyGrid::from_solid_mask(
        GlobalCell::new(min[0], min[1], min[2]),
        dims,
        solid,
        material,
    )
    .ok()
}

/// One individual predicted-vs-authoritative comparison — [`ReconcileOutcome::
/// comparison`], `Some` only when a `history` record was tagged for the
/// exact tick `authoritative` represents. See that field's own doc for what
/// it means when there isn't one.
#[derive(Debug, Clone, Copy)]
pub struct CorrectionEvent {
    /// The matched record's own input sequence — which locally-sent
    /// `InputFrame` this comparison actually used, for cross-referencing
    /// against a client-side send log (e.g. the `sent_log` pattern
    /// `crates/spall_client/tests/g1_realistic_input_timing_trace.rs` keeps).
    pub seq: InputSeq,
    /// Full 3-D distance between what was predicted for that tick and what
    /// the server actually reported, metres.
    pub error_m: f64,
    /// `|Y|` component of the same gap.
    pub vertical_m: f64,
    /// `XZ`-plane magnitude of the same gap.
    pub horizontal_m: f64,
    /// Whether the record's input was exactly [`PlayerInput::NEUTRAL`] (no
    /// movement, no buttons) — see [`PredictedPlayer::idle_corrections`].
    pub idle: bool,
}

/// Everything one [`PredictedPlayer::reconcile`] call actually did —
/// returned unconditionally, not only when a [`Self::comparison`] was
/// possible. ENG-69 round 21: an earlier version returned
/// `Option<CorrectionEvent>`, `None` whenever no `history` record could be
/// matched to `server_tick` — including the case where *every* record got
/// silently dropped and `predicted` hard-snapped straight onto
/// `authoritative`. A caller that only acts on `Some` cannot tell "a clean
/// small correction" from "a large silent resync just happened and there's
/// no comparison for it" — precisely the blind spot that let a synthetic,
/// single-process, lockstep test report "zero corrections" while a real,
/// independently-scheduled client's felt jitter visibly got worse: the test
/// never diverged from its own count-based assumption (client and server
/// ticked in the same loop iteration, always exactly once each), so it
/// never exercised the branch a real client's clock drift, stalls, or a
/// delayed first snapshot actually hit.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileOutcome {
    pub server_tick: Tick,
    /// `server_tick` minus the previous call's `server_tick` (or minus
    /// `spawn_tick` on the first call), signed and *not* saturated —
    /// reported as-is so a real ordering bug (a snapshot arriving out of
    /// order) would show up as a negative delta instead of being clamped
    /// away and looking identical to "nothing happened yet".
    pub server_tick_delta: i64,
    /// `history.len()` before this call touched it.
    pub history_len_before: usize,
    /// Records dropped as "the server has already covered this tick" — not
    /// replayed.
    pub records_removed: usize,
    /// Records kept and re-simulated from `authoritative` — this call's
    /// actual replay depth. `history.len()` after is exactly this.
    pub records_replayed: usize,
    pub predicted_before: CharacterState,
    pub predicted_after: CharacterState,
    /// `Some` only when a `history` record was tagged for exactly
    /// `server_tick` (see `Record::tick`'s doc for the tagging/re-anchoring
    /// scheme). A hand-authored fixed-offset or single-process lockstep
    /// test harness hits this every time by construction; a real client
    /// does not — clock drift between two independently-scheduled ~60 Hz
    /// loops, a stall, a delayed first snapshot, or `PREDICTION_HISTORY`
    /// eviction can all legitimately leave no record at that exact tick.
    /// `None` here does **not** mean nothing happened: `records_removed`
    /// and `predicted_before` vs. `predicted_after` (or
    /// [`PredictedPlayer::unmatched_reconciles`]/
    /// `max_unmatched_displacement_m`) are what actually happened this
    /// call regardless.
    pub comparison: Option<CorrectionEvent>,
}

/// One predicted input, kept so it can be re-simulated after a correction.
#[derive(Debug, Clone, Copy)]
struct Record {
    /// The local clock's best estimate of which server tick this record
    /// represents. Assigned from [`PredictedPlayer`]'s own free-running
    /// `next_tick` counter (incremented once per [`PredictedPlayer::tick`]
    /// call, starting from [`PredictedPlayer::new`]'s `spawn_tick`) and
    /// re-anchored *exactly* onto the true `server_tick` at the end of
    /// every [`PredictedPlayer::reconcile`] call — so whatever drift
    /// accumulated between two reconciles (an independently-scheduled
    /// client clock running at a slightly different rate than the server,
    /// or a stall that skipped some local ticks) can never compound past
    /// one reconcile interval.
    ///
    /// ENG-69 round 21: this field replaces round 19/20's positional/count-
    /// based `history` draining (`keep_from = min(server_tick_delta,
    /// history.len())`, dropping *however many records that count implies*
    /// rather than the *specific* ones the server actually covered).
    /// Counting only gives the right answer when local-tick-count equals
    /// elapsed-server-tick-count between two reconciles — true in a
    /// single-process test that ticks client and server together every
    /// loop iteration, not guaranteed for a real client on its own
    /// wall-clock timer. Retaining by explicit per-record `tick` comparison
    /// (`tick.0 > server_tick.0`) instead of by count is correct regardless
    /// of whether that correspondence held this interval.
    tick: Tick,
    /// The input sequence this record was predicted for — kept for
    /// identity (a `ReconcileOutcome`/log line can name exactly which input
    /// a comparison used), not load-bearing for retain/replay itself (see
    /// `tick` above, which round 19 found `seq` cannot correctly stand in
    /// for once held-input reuse is in play).
    seq: InputSeq,
    input: PlayerInput,
    dt: f32,
    predicted_after: CharacterState,
}

/// The local player's predicted state plus the history needed to reconcile it.
pub struct PredictedPlayer {
    pub params: CharacterParams,
    predicted: CharacterState,
    authoritative: CharacterState,
    acked: InputSeq,
    /// The tick the *next* pushed [`Record`] will be tagged with — see that
    /// field's own doc for the full self-healing tagging scheme.
    next_tick: Tick,
    /// `server_tick` of the last [`Self::reconcile`] call — `spawn_tick`
    /// (`Self::new`) before the first one. Purely diagnostic
    /// (`ReconcileOutcome::server_tick_delta`); retain/replay itself no
    /// longer uses a delta at all, see `Record::tick`.
    last_server_tick: Tick,
    history: VecDeque<Record>,
    start_pos_m: [f64; 3],
    max_distance_from_start_m: f64,

    // --- metrics -------------------------------------------------------------
    /// Snapshots whose predicted-at-ack state differed from authoritative.
    pub corrections: u64,
    /// Largest such difference, metres.
    pub max_correction_m: f64,
    /// Of `corrections`, how many happened on a record whose input was
    /// perfectly neutral (no movement, no buttons) — isolates a resting-contact
    /// disagreement (both sides idle, nothing to sweep) from a collision-sweep
    /// difference incurred while actually walking, which the plain lifetime
    /// counters above cannot tell apart (see ENG-69's round-6/7 investigation
    /// into corrections that keep firing "even while standing still").
    pub idle_corrections: u64,
    /// Largest correction magnitude ever seen on an idle record.
    pub max_idle_correction_m: f64,
    /// Largest vertical (Y) component of `err` ever seen across all
    /// corrections — large relative to `max_correction_m` points at the two
    /// sides resting at different heights (a terrain/ground-snap disagreement);
    /// small relative to it (with `max_horizontal_correction_m` large instead)
    /// points at a swept-move/collision-response disagreement instead.
    pub max_vertical_correction_m: f64,
    /// Largest horizontal (XZ) component of `err` ever seen across all
    /// corrections. See `max_vertical_correction_m`.
    pub max_horizontal_correction_m: f64,
    pub total_ticks: u64,
    pub grounded_ticks: u64,
    /// Set only if the predictor ever reported "grounded" while authority was
    /// clearly falling — the bug "removing a floor during replay leaves the
    /// player hovering". Correct code never sets it.
    pub hovered_after_floor_removal: bool,
    /// `reconcile` calls whose `ReconcileOutcome::comparison` was `None` —
    /// see that field's own doc. ENG-69 round 21: tracked separately from
    /// `corrections` precisely so "zero corrections" can never silently
    /// mean "we stopped being able to tell" instead of "nothing happened".
    pub unmatched_reconciles: u64,
    /// Largest `predicted_before`-to-`predicted_after` displacement ever
    /// seen on an unmatched reconcile — the actually-felt jump size for the
    /// cases `corrections`/`max_correction_m` cannot see at all.
    pub max_unmatched_displacement_m: f64,
}

impl PredictedPlayer {
    /// `spawn_tick` is the server tick `spawn` itself is authoritative as of
    /// — always available at construction time in practice, since a
    /// `PredictedPlayer` is only ever created from a `CharacterState` that
    /// just arrived *with* a tick attached (`net.rs`'s first `MotionSnapshot`
    /// for this player; a test harness's `Simulation::current_tick()` before
    /// its first `tick()` call, i.e. `Tick(0)`). This is what lets
    /// [`Self::reconcile`] treat every call uniformly, the very first one
    /// included, instead of needing a "no anchor yet" special case — ENG-69
    /// round 19's own first-draft fix special-cased the first call as "drop
    /// nothing," which left one stale record stuck at the front of
    /// `history` forever, silently shifting every later comparison one tick
    /// off (invisible except right when input changes between the
    /// mis-selected record and the current tick).
    pub fn new(params: CharacterParams, spawn: CharacterState, spawn_tick: Tick) -> Self {
        Self {
            params,
            predicted: spawn,
            authoritative: spawn,
            acked: InputSeq(0),
            next_tick: Tick(spawn_tick.0 + 1),
            last_server_tick: spawn_tick,
            history: VecDeque::new(),
            start_pos_m: spawn.position_m,
            max_distance_from_start_m: 0.0,
            corrections: 0,
            max_correction_m: 0.0,
            idle_corrections: 0,
            max_idle_correction_m: 0.0,
            max_vertical_correction_m: 0.0,
            max_horizontal_correction_m: 0.0,
            total_ticks: 0,
            grounded_ticks: 0,
            hovered_after_floor_removal: false,
            unmatched_reconciles: 0,
            max_unmatched_displacement_m: 0.0,
        }
    }

    pub fn predicted(&self) -> CharacterState {
        self.predicted
    }

    pub fn authoritative(&self) -> CharacterState {
        self.authoritative
    }

    /// Advances the prediction one tick and records the input for replay.
    /// `volume` is the replica's current terrain, passed through to
    /// [`ClientPhysics::sweep`]'s own window cache — the caller already
    /// holds/clones it each tick for the existing terrain-hash dirty check.
    /// `seq` is the input's own sequence number — kept on the pushed
    /// [`Record`] for identity, not for retain/replay (see that field's own
    /// doc).
    pub fn tick(
        &mut self,
        phys: &mut ClientPhysics,
        volume: &Volume,
        input: PlayerInput,
        seq: InputSeq,
        dt_s: f32,
    ) -> CharacterState {
        let params = self.params;
        self.predicted = step_character(self.predicted, input, dt_s, |p, d| {
            phys.sweep(volume, params, p, d, dt_s)
        });
        self.history.push_back(Record {
            tick: self.next_tick,
            seq,
            input,
            dt: dt_s,
            predicted_after: self.predicted,
        });
        self.next_tick = Tick(self.next_tick.0 + 1);
        while self.history.len() > PREDICTION_HISTORY {
            self.history.pop_front();
        }
        self.total_ticks += 1;
        if self.predicted.grounded {
            self.grounded_ticks += 1;
        }
        self.max_distance_from_start_m = self
            .max_distance_from_start_m
            .max(self.distance_travelled_m());
        self.predicted
    }

    /// Reconciles against an authoritative snapshot at `server_tick`. Drops
    /// exactly the `history` records the server has actually simulated as of
    /// that tick (identified by each record's own [`Record::tick`], not a
    /// count), and replays the rest — the still-genuinely-outstanding
    /// ones — from the authoritative state to produce the new predicted
    /// "now". Returns a [`ReconcileOutcome`] describing exactly what this
    /// call did, unconditionally — see that type's own doc for why it is
    /// not `Option<CorrectionEvent>` any more (ENG-69 round 21: a caller
    /// that only reacted to `Some` could not tell a clean small correction
    /// apart from a large, completely uncounted resync).
    ///
    /// `acked` (the last input sequence the server *accepted* — i.e. treated
    /// as fresh, `Simulation::player_acked_input`/`MotionSnapshot::
    /// acked_input`) is still recorded (`Self::acked`... no external reader
    /// today, kept for parity with the wire concept it names) but is
    /// deliberately **not** used to decide what counts as "already
    /// simulated by the server" — that was ENG-69 round 19's bug.
    /// `spall_sim::player::Player::last_input_seq` (what `acked` reports)
    /// only advances on a *fresh* accepted frame, but the server steps the
    /// player forward every tick regardless, reusing the last input via
    /// `effective_input()` whenever a fresh frame hasn't arrived
    /// (`HELD_INPUT_TIMEOUT_TICKS`). `server_tick`
    /// (`MotionSnapshot::server_tick`) has no such gap — it is the server's
    /// own tick counter, advanced every tick unconditionally.
    pub fn reconcile(
        &mut self,
        phys: &mut ClientPhysics,
        volume: &Volume,
        authoritative: CharacterState,
        acked: InputSeq,
        server_tick: Tick,
    ) -> ReconcileOutcome {
        let server_tick_delta = server_tick.0 as i64 - self.last_server_tick.0 as i64;
        self.last_server_tick = server_tick;

        let history_len_before = self.history.len();
        let predicted_before = self.predicted;

        // Identity-based lookup, *before* any mutation: the record (if any)
        // whose own tag is exactly `server_tick` — not an index derived from
        // a count. `Record` is `Copy`, so this ends the borrow on
        // `self.history` immediately and the running-counter updates below
        // can freely borrow `self` mutably.
        let matched = self.history.iter().find(|r| r.tick == server_tick).copied();
        let comparison = matched.map(|rec| {
            let err = rec.predicted_after.distance_m(&authoritative);
            // A record only ever holds the *sanitized* input actually fed to
            // `step_character` (see `tick`), so this is exactly the input
            // that produced `predicted_after` — comparing it to `NEUTRAL`
            // tells whether the two sides had anything to sweep at all.
            let idle = rec.input.movement == [0.0, 0.0, 0.0] && rec.input.buttons == 0;
            if err > 1.0e-4 {
                self.corrections += 1;
                if idle {
                    self.idle_corrections += 1;
                    self.max_idle_correction_m = self.max_idle_correction_m.max(err);
                }
            }
            self.max_correction_m = self.max_correction_m.max(err);
            let dy = (rec.predicted_after.position_m[1] - authoritative.position_m[1]).abs();
            let dx = rec.predicted_after.position_m[0] - authoritative.position_m[0];
            let dz = rec.predicted_after.position_m[2] - authoritative.position_m[2];
            let horizontal_m = (dx * dx + dz * dz).sqrt();
            self.max_vertical_correction_m = self.max_vertical_correction_m.max(dy);
            self.max_horizontal_correction_m = self.max_horizontal_correction_m.max(horizontal_m);
            CorrectionEvent {
                seq: rec.seq,
                error_m: err,
                vertical_m: dy,
                horizontal_m,
                idle,
            }
        });

        if !authoritative.grounded
            && authoritative.velocity_m_s[1] < -1.0
            && self.predicted.grounded
            && self.predicted.position_m[1] > authoritative.position_m[1] + 0.3
        {
            self.hovered_after_floor_removal = true;
        }

        self.authoritative = authoritative;
        self.acked = acked;
        // Drop exactly the records the server has covered — identified by
        // each one's own tag, not a count (see `Record::tick`'s doc for why
        // that distinction matters).
        self.history.retain(|r| r.tick.0 > server_tick.0);
        let records_replayed = self.history.len();
        let records_removed = history_len_before - records_replayed;

        // Re-anchor every surviving record's tag exactly onto `server_tick`
        // — closes whatever drift accumulated *this* interval in one step
        // (an independently-scheduled client clock running fractionally
        // faster or slower than the server, or a stall that skipped some
        // local ticks, both show up as drift here). The retain decision
        // above used each record's tag as carried into this call, however
        // drifted; everything from here on is exact again, `next_tick`
        // included, so drift can never compound past one reconcile
        // interval.
        for (i, rec) in self.history.iter_mut().enumerate() {
            rec.tick = Tick(server_tick.0 + 1 + i as u64);
        }
        self.next_tick = Tick(server_tick.0 + 1 + self.history.len() as u64);

        let params = self.params;
        let mut state = authoritative;
        for rec in self.history.iter_mut() {
            let dt = rec.dt;
            state = step_character(state, rec.input, dt, |p, d| {
                phys.sweep(volume, params, p, d, dt)
            });
            rec.predicted_after = state;
        }
        self.predicted = state;

        if comparison.is_none() {
            self.unmatched_reconciles += 1;
            let displacement = predicted_before.distance_m(&self.predicted);
            self.max_unmatched_displacement_m = self.max_unmatched_displacement_m.max(displacement);
        }

        ReconcileOutcome {
            server_tick,
            server_tick_delta,
            history_len_before,
            records_removed,
            records_replayed,
            predicted_before,
            predicted_after: self.predicted,
            comparison,
        }
    }

    /// A committed edit invalidated the geometry the predicted history walked
    /// through: drop it and rebase on the last authoritative state rather than
    /// replaying movement through geometry that no longer exists.
    pub fn invalidate(&mut self) {
        self.history.clear();
        self.predicted = self.authoritative;
    }

    /// Fraction of predicted ticks spent grounded.
    pub fn ground_contact_ratio(&self) -> f64 {
        if self.total_ticks == 0 {
            0.0
        } else {
            self.grounded_ticks as f64 / self.total_ticks as f64
        }
    }

    /// Horizontal distance the predicted feet have travelled from spawn, metres.
    pub fn distance_travelled_m(&self) -> f64 {
        let dx = self.predicted.position_m[0] - self.start_pos_m[0];
        let dz = self.predicted.position_m[2] - self.start_pos_m[2];
        (dx * dx + dz * dz).sqrt()
    }

    /// Current gap between the predicted and authoritative feet, metres.
    pub fn prediction_error_m(&self) -> f64 {
        self.predicted.distance_m(&self.authoritative)
    }

    /// A machine-readable summary of one scripted run.
    pub fn summary(&self, held_button_release_ok: bool) -> PlayerMovementSummary {
        PlayerMovementSummary {
            ticks: self.total_ticks,
            distance_travelled_m: self.distance_travelled_m(),
            max_distance_from_start_m: self.max_distance_from_start_m,
            final_prediction_error_m: self.prediction_error_m(),
            corrections: self.corrections,
            max_correction_m: self.max_correction_m,
            ground_contact_ratio: self.ground_contact_ratio(),
            hovered_after_floor_removal: self.hovered_after_floor_removal,
            held_button_release_ok,
            final_predicted_pos_m: self.predicted.position_m,
            final_authoritative_pos_m: self.authoritative.position_m,
        }
    }
}

/// Reported per scripted player in [`crate::ClientSummary`].
#[derive(Debug, Clone, Serialize)]
pub struct PlayerMovementSummary {
    pub ticks: u64,
    pub distance_travelled_m: f64,
    /// Farthest horizontal displacement reached at any predicted tick. Paired
    /// with the final displacement to prove an outbound-and-return traversal.
    pub max_distance_from_start_m: f64,
    pub final_prediction_error_m: f64,
    pub corrections: u64,
    pub max_correction_m: f64,
    pub ground_contact_ratio: f64,
    pub hovered_after_floor_removal: bool,
    pub held_button_release_ok: bool,
    pub final_predicted_pos_m: [f64; 3],
    pub final_authoritative_pos_m: [f64; 3],
}
