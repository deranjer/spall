//! Client application of the shared T18 residency policy.

use std::collections::{BTreeMap, BTreeSet};

use spall_core::{BrickCoord, GlobalCell, Revision, VolumeId};
use spall_protocol::{Hash32, RepairKey, RepairRequest};
use spall_voxel::{
    BrickCacheKey, CacheBudget, CollisionReadiness, InterestRadii, MemoryReport, ResidencyCache,
};

use crate::ReplicaWorld;

/// Bounded client topology cache. Dynamic-body volumes stay complete while
/// terrain bricks can be evicted and re-enter through dependency baselines.
pub struct ClientResidency {
    pub cache: ResidencyCache,
    pub collision: CollisionReadiness,
    body_pins: BTreeSet<BrickCacheKey>,
}

impl ClientResidency {
    pub fn new(budget: CacheBudget, max_collision_sweep_bricks: usize) -> Self {
        Self {
            cache: ResidencyCache::new(budget),
            collision: CollisionReadiness::new(max_collision_sweep_bricks),
            body_pins: BTreeSet::new(),
        }
    }

    /// Reconciles accounting after a baseline, repair, or topology transaction.
    /// Replica geometry is already authoritative, so every registered revision
    /// is clean from the client's perspective.
    pub fn sync(&mut self, replica: &ReplicaWorld) {
        let mut volumes = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(terrain) = replica.terrain_volume() {
            volumes.push((terrain.id(), false));
        }
        volumes.extend(replica.body_volumes().map(|(_, volume)| (volume, true)));
        for (volume_id, pin_complete) in volumes {
            let Some(volume) = replica.volume(volume_id) else {
                continue;
            };
            for coord in volume.resident_brick_coords() {
                let snap = volume
                    .snapshot_brick(coord)
                    .ok()
                    .flatten()
                    .expect("resident coordinate");
                let key = BrickCacheKey::new(volume_id, coord);
                seen.insert(key);
                let bytes = usize::from(snap.is_dense()) * MemoryReport::DENSE_BRICK_BYTES;
                if self
                    .cache
                    .state(key)
                    .is_some_and(|state| state.revision != snap.revision())
                {
                    self.collision.invalidate(key);
                }
                self.cache.register(key, snap.revision(), bytes, true);
                if pin_complete && self.body_pins.insert(key) {
                    self.cache.pin(key);
                }
            }
        }
        let stale: Vec<_> = self
            .cache
            .keys()
            .filter(|key| !seen.contains(key))
            .collect();
        for key in stale {
            self.cache.remove(key);
            self.collision.invalidate(key);
        }
        self.body_pins.retain(|key| seen.contains(key));
    }

    pub fn update_terrain_interest(
        &mut self,
        replica: &ReplicaWorld,
        center: BrickCoord,
        radii: InterestRadii,
    ) {
        self.cache
            .update_interest(replica.terrain_volume_id(), center, radii);
    }

    pub fn enforce_budget(&mut self, replica: &mut ReplicaWorld) -> Vec<BrickCacheKey> {
        let plan = self.cache.plan_evictions();
        let mut evicted = Vec::new();
        for key in plan.evict {
            if replica.evict_brick(key.volume, key.coord) {
                self.cache.remove(key);
                self.collision.invalidate(key);
                evicted.push(key);
            }
        }
        evicted
    }

    /// T23 / G3 row 7, slice E: retained-digest **terrain** bricks that have
    /// come back inside the enter-radius of `center` and should be pulled back
    /// with a bounded `RepairRequest`. The digest stays retained until the
    /// patch lands (`docs/reports/G3-residency-hash.md` reload lifecycle), so
    /// `world_hash` is exact throughout. Nearest-first, at most `max_bricks`.
    /// Empty when nothing is evicted or the box would exceed `max_bricks`.
    pub fn wanted_reloads(
        replica: &ReplicaWorld,
        center: BrickCoord,
        enter_radius: i64,
        max_bricks: usize,
    ) -> Vec<RepairKey> {
        let terrain = replica.terrain_volume_id();
        let evicted = replica.evicted(terrain);
        if evicted.is_empty() {
            return Vec::new();
        }
        let Some(desired) = ResidencyCache::desired_coords(center, enter_radius, max_bricks) else {
            return Vec::new();
        };
        desired
            .into_iter()
            .filter(|coord| evicted.contains(*coord))
            .map(|coord| RepairKey::Brick {
                volume: terrain,
                coord,
            })
            .collect()
    }

    pub fn desired_terrain(
        replica: &ReplicaWorld,
        center: BrickCoord,
        enter_radius: i64,
        max_bricks: usize,
    ) -> Option<Vec<BrickCacheKey>> {
        let volume: VolumeId = replica.terrain_volume_id();
        ResidencyCache::desired_coords(center, enter_radius, max_bricks).map(|coords| {
            coords
                .into_iter()
                .map(|coord| BrickCacheKey::new(volume, coord))
                .collect()
        })
    }
}

/// Consecutive mover steps a repair transfer owns its brick before a retry is
/// eligible.  This is deliberately longer than the loss profile's RTT: a QUIC
/// bulk stream may still be retransmitting, and repeatedly asking for the same
/// brick creates a queue of obsolete full-patch transfers behind it.
const RELOAD_COOLDOWN_STEPS: u32 = 120;

/// Global request ceiling for one mover iteration. Per-brick cooldown alone is
/// insufficient: entering a wide interest box could otherwise enqueue every
/// missing brick in one burst and starve prediction/control traffic.
pub const MAX_RELOAD_REQUESTS_PER_STEP: usize = 4;

/// Consecutive steps a resident terrain brick must be out of the retain box
/// before the pass evicts it. The hysteresis keeps the pass from dropping and
/// re-pulling a brick as the player's predicted position jitters across a
/// boundary.
const EVICT_SETTLE_STEPS: u32 = 8;

/// T23 / G3 row 7, slice E2 — the client-session terrain residency pass, run
/// once per mover iteration against the predicted player capsule. Keeps a
/// Chebyshev `radius`-brick box around the player resident, evicts the rest of
/// the terrain after a hysteresis (retaining a digest, so `world_hash` is exact
/// — slice B), and emits a rate-limited `RepairRequest` for every evicted brick
/// that has come back into the box so the server re-sends it (slice E1's reload
/// lifecycle drops the digest as the patch lands). Body volumes are never
/// touched. `radius` must be wide enough to cover the predicted-collision
/// region — the mover rebuilds its predicted collider from resident geometry
/// only, so a brick under the player's near path must stay loaded.
///
/// ## ENG-30 row 7 increment 14 — client-side dense-byte admission
///
/// `docs/reports/G3.md` increment 33 (ENG-30 row 7 increment 13) gave the
/// server-side `ResidencyPass` a real `max_dense_bytes` ceiling enforced on
/// admission (an interest-driven reload deferred, not admitted, past it) and
/// explicitly deferred `ClientResidencyPass` as out of scope. This pass has a
/// single predicted player and no pending-edit/swept-collision pin sources of
/// its own (it never originates edits, and eviction is already governed
/// unconditionally by the box, never by budget), so every reload request this
/// pass emits is "interest-driven, non-required" in the server's terms — the
/// admission cap below applies uniformly to all of them, using the same
/// conservative (cost every candidate as if `Dense`,
/// [`spall_voxel::MemoryReport::DENSE_BRICK_BYTES`]) estimate the server uses,
/// corrected to the real measured total every step (this pass's reload is
/// asynchronous — a `RepairRequest` only becomes resident once the server's
/// patch lands — so the projected total also counts bricks already requested
/// but not yet completed, the conservative upper bound for what could land
/// before the next step's fresh measurement). `budget_bricks` — historically a
/// reported ceiling only — is enforced by the same check, matching the
/// server's "either cap" admission rule.
pub struct ClientResidencyPass {
    radius: i64,
    budget_bricks: usize,
    /// T23 / G3 row 7 increment 14: hard ceiling on resident terrain dense
    /// bytes, enforced on the reload-admission path the same way as
    /// `budget_bricks`. `u64::MAX` disables this cap while still enforcing
    /// `budget_bricks` — the historical behaviour before this field existed.
    max_dense_bytes: u64,
    /// Consecutive steps each resident, out-of-box brick has waited.
    out_of_box: BTreeMap<BrickCoord, u32>,
    reload_cooldown: BTreeMap<BrickCoord, u32>,
    pending_reloads: BTreeSet<BrickCoord>,
    evictions_total: u64,
    reloads_requested_total: u64,
    reloads_completed_total: u64,
    budget_miss_steps_total: u64,
    /// T23 / G3 row 7 increment 14: a desired (box-driven) reload request was
    /// skipped this step because admitting it would have exceeded the brick or
    /// dense-byte cap. The brick stays evicted; a later step retries once
    /// pressure eases.
    admission_deferred_total: u64,
}

impl ClientResidencyPass {
    /// `radius` is the Chebyshev brick radius kept resident around the player.
    /// `budget_bricks` and `max_dense_bytes` are both hard ceilings enforced on
    /// the reload-admission path (T23 / G3 row 7 increment 14) — the pass
    /// still evicts strictly by the box, never below it, so a tight cap only
    /// ever defers a desired reload back into the box, never forces an
    /// eviction inside it.
    pub fn new(budget_bricks: usize, radius: i64, max_dense_bytes: u64) -> Self {
        Self {
            radius: radius.max(0),
            budget_bricks,
            max_dense_bytes,
            out_of_box: BTreeMap::new(),
            reload_cooldown: BTreeMap::new(),
            pending_reloads: BTreeSet::new(),
            evictions_total: 0,
            reloads_requested_total: 0,
            reloads_completed_total: 0,
            budget_miss_steps_total: 0,
            admission_deferred_total: 0,
        }
    }

    pub fn evictions_total(&self) -> u64 {
        self.evictions_total
    }

    pub fn reloads_requested_total(&self) -> u64 {
        self.reloads_requested_total
    }

    pub fn reloads_completed_total(&self) -> u64 {
        self.reloads_completed_total
    }

    pub fn budget_miss_steps_total(&self) -> u64 {
        self.budget_miss_steps_total
    }

    /// T23 / G3 row 7 increment 14: cumulative desired reloads skipped because
    /// admitting them would have exceeded the brick or dense-byte cap.
    pub fn admission_deferred_total(&self) -> u64 {
        self.admission_deferred_total
    }

    /// Whether the run has held more resident terrain than `budget_bricks`
    /// (informational — the box is never forced below what the player needs).
    pub fn over_budget(&self, replica: &ReplicaWorld) -> bool {
        replica
            .volume(replica.terrain_volume_id())
            .map(|v| v.resident_brick_count() > self.budget_bricks)
            .unwrap_or(false)
    }

    fn box_around(&self, center: BrickCoord) -> BTreeSet<BrickCoord> {
        let mut set = BTreeSet::new();
        for dz in -self.radius..=self.radius {
            for dy in -self.radius..=self.radius {
                for dx in -self.radius..=self.radius {
                    set.insert(BrickCoord::new(center.x + dx, center.y + dy, center.z + dz));
                }
            }
        }
        set
    }

    /// One pass. Returns the `RepairRequest`s the caller should send.
    pub fn step(
        &mut self,
        replica: &mut ReplicaWorld,
        player_feet_m: [f64; 3],
    ) -> Vec<RepairRequest> {
        let terrain = replica.terrain_volume_id();
        let Some(cell_m) = replica.volume(terrain).map(|v| v.cell_size().metres()) else {
            return Vec::new();
        };
        if cell_m <= 0.0 {
            return Vec::new();
        }
        let center = GlobalCell::new(
            (player_feet_m[0] / cell_m).floor() as i64,
            (player_feet_m[1] / cell_m).floor() as i64,
            (player_feet_m[2] / cell_m).floor() as i64,
        )
        .split()
        .0;
        let keep = self.box_around(center);

        // A repair completes when the patch atomically reinstalls the brick and
        // drops its retained digest. Count completions, not merely requests.
        let completed: Vec<_> = self
            .pending_reloads
            .iter()
            .copied()
            .filter(|coord| !replica.evicted(terrain).contains(*coord))
            .collect();
        for coord in completed {
            self.pending_reloads.remove(&coord);
            self.reload_cooldown.remove(&coord);
            self.reloads_completed_total += 1;
        }

        // Reload requests: retained-digest bricks back inside the box.
        self.reload_cooldown.retain(|_, wait| {
            *wait = wait.saturating_sub(1);
            *wait > 0
        });

        // T23 / G3 row 7 increment 14: a conservative running admission
        // estimate for this step. Resident bricks/bytes are measured fresh
        // (the correction half of "conservative-then-corrected" — a prior
        // step's admitted requests either completed, feeding this fresh
        // measurement, or are still `pending_reloads` and are added back in
        // below as the worst case for what has not landed yet). Every
        // candidate is then costed as if fully `Dense`
        // (`MemoryReport::DENSE_BRICK_BYTES`), so the enforced cap can only be
        // tighter than the true footprint once requests complete, never
        // looser.
        let resident_terrain_bricks = replica
            .volume(terrain)
            .map(|v| v.resident_brick_count())
            .unwrap_or(0);
        let resident_dense_bytes = replica
            .volume(terrain)
            .map(|v| v.memory_report().total_dense_bytes() as u64)
            .unwrap_or(0);
        let mut projected_bricks = resident_terrain_bricks + self.pending_reloads.len();
        let mut projected_dense_bytes = resident_dense_bytes.saturating_add(
            self.pending_reloads.len() as u64 * MemoryReport::DENSE_BRICK_BYTES as u64,
        );

        let mut out = Vec::new();
        let evicted_now: Vec<(BrickCoord, Revision)> = replica
            .evicted(terrain)
            .iter()
            .map(|(c, d)| (c, d.revision))
            .collect();
        for (coord, revision) in evicted_now {
            if out.len() >= MAX_RELOAD_REQUESTS_PER_STEP {
                break;
            }
            if !keep.contains(&coord) || self.reload_cooldown.contains_key(&coord) {
                continue;
            }
            let projected_bricks_candidate = projected_bricks.saturating_add(1);
            let projected_dense_candidate =
                projected_dense_bytes.saturating_add(MemoryReport::DENSE_BRICK_BYTES as u64);
            let fits = projected_bricks_candidate <= self.budget_bricks
                && projected_dense_candidate <= self.max_dense_bytes;
            if !fits {
                self.admission_deferred_total += 1;
                continue;
            }
            projected_bricks = projected_bricks_candidate;
            projected_dense_bytes = projected_dense_candidate;
            self.reload_cooldown.insert(coord, RELOAD_COOLDOWN_STEPS);
            self.pending_reloads.insert(coord);
            self.reloads_requested_total += 1;
            out.push(RepairRequest {
                key: RepairKey::Brick {
                    volume: terrain,
                    coord,
                },
                expected_revision: revision,
                current_revision: Revision::ZERO,
                expected_hash: Hash32::ZERO,
                current_hash: Hash32::ZERO,
            });
        }

        // Evict resident terrain bricks that have been out of the box for
        // `EVICT_SETTLE_STEPS` consecutive steps.
        let resident: Vec<BrickCoord> = replica
            .volume(terrain)
            .map(|v| v.resident_brick_coords())
            .unwrap_or_default();
        let resident_set: BTreeSet<BrickCoord> = resident.iter().copied().collect();
        self.out_of_box.retain(|c, _| resident_set.contains(c));
        for coord in resident {
            if keep.contains(&coord) {
                self.out_of_box.remove(&coord);
                continue;
            }
            let waited = self.out_of_box.entry(coord).or_insert(0);
            *waited += 1;
            if *waited >= EVICT_SETTLE_STEPS && replica.evict_brick(terrain, coord) {
                self.evictions_total += 1;
                self.out_of_box.remove(&coord);
            }
        }
        if self.over_budget(replica) {
            self.budget_miss_steps_total += 1;
        }
        out
    }
}
