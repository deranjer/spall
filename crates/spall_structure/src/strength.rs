//! Material-dependent structural strength (T22).
//!
//! Connectivity-only collapse — a component with no path to the anchor plane
//! detaches — is T07 ([`crate::support`]). This module adds the stricter G4
//! test on top: a component that is still grounded can nonetheless fail when a
//! bond along its load path carries more than its material can hold.
//!
//! The model is frozen in `docs/structural-strength.md`
//! (`strength_algo_version = 1`). It is a gameplay approximation — no
//! finite-element method, no stress tensor, no impact loads (T21), and
//! all-resident residency only (streamed support is T18). Every step runs on
//! `u64`/`u128` integers with canonical `(z, y, x)` tie-breaking, so a report is
//! bit-identical on every machine and every run.
//!
//! Entry point: [`evaluate`]. Persisted authoritative state: [`DamageState`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spall_core::{
    CELLS_PER_BRICK, CellSizeCode, GlobalCell, LocalCell, MaterialFlags, MaterialId,
    MaterialManifest,
};
use spall_voxel::Volume;

use crate::graph::AnchorPlane;

/// Version of the frozen equations in `docs/structural-strength.md`. A saved
/// [`DamageState`] is only meaningful under the algorithm that produced it; a
/// mismatch on load must fail the handshake (wiring is increment 2).
pub const STRENGTH_ALGO_VERSION: u16 = 1;

/// Grams of axial bond capacity per unit of `bond_strength`.
const BOND_SCALE: f64 = 100_000.0;
/// A bending (lateral) joint holds this fraction of the axial capacity.
const LATERAL_NUM: u128 = 1;
const LATERAL_DEN: u128 = 2;

/// Canonical order key: `(z, y, x)`, matching the rest of `spall_structure`.
#[inline]
fn key(c: GlobalCell) -> (i64, i64, i64) {
    (c.z, c.y, c.x)
}

/// A single broken inter-cell face bond, stored canonically with
/// `key(a) <= key(b)`.
///
/// The endpoints are private: [`BrokenBond::new`] is the only constructor and it
/// always orders them, so a non-canonical bond cannot be built. This matters
/// because [`DamageState`] hashes, de-dupes, and binary-searches on endpoint
/// order — a reversed pair would hash differently and escape [`DamageState::contains`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrokenBond {
    a: GlobalCell,
    b: GlobalCell,
}

impl BrokenBond {
    /// Orders the two endpoints into canonical form.
    pub fn new(p: GlobalCell, q: GlobalCell) -> Self {
        if key(p) <= key(q) {
            Self { a: p, b: q }
        } else {
            Self { a: q, b: p }
        }
    }

    /// The lower-keyed endpoint (`key(a) <= key(b)`).
    #[inline]
    pub fn a(self) -> GlobalCell {
        self.a
    }

    /// The higher-keyed endpoint (`key(a) <= key(b)`).
    #[inline]
    pub fn b(self) -> GlobalCell {
        self.b
    }

    fn order_key(self) -> ((i64, i64, i64), (i64, i64, i64)) {
        (key(self.a), key(self.b))
    }
}

/// The authoritative set of broken bonds for one volume — a canonically sorted,
/// de-duplicated list. This is a persistent authoritative layer: it participates
/// in revision / hash / checkpoint like every other layer, so a replicated world
/// hash covers structural damage. Recovery loads it and [`evaluate`] resumes
/// with those bonds already broken, so a failing beam cannot heal on restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DamageState {
    bonds: Vec<BrokenBond>,
}

impl DamageState {
    /// An undamaged volume.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a canonical state from any iterator of bonds.
    pub fn from_bonds<I: IntoIterator<Item = BrokenBond>>(bonds: I) -> Self {
        let mut v: Vec<BrokenBond> = bonds.into_iter().collect();
        v.sort_by_key(|b| b.order_key());
        v.dedup();
        Self { bonds: v }
    }

    /// The broken bonds, canonical `(key(a), key(b))` order.
    pub fn bonds(&self) -> &[BrokenBond] {
        &self.bonds
    }

    pub fn len(&self) -> usize {
        self.bonds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bonds.is_empty()
    }

    /// Whether the face bond between `p` and `q` is recorded broken.
    pub fn contains(&self, p: GlobalCell, q: GlobalCell) -> bool {
        let target = BrokenBond::new(p, q).order_key();
        self.bonds
            .binary_search_by_key(&target, |b| b.order_key())
            .is_ok()
    }

    /// Inserts a bond, keeping the list canonical. Returns `true` if it was new.
    pub fn insert(&mut self, bond: BrokenBond) -> bool {
        match self
            .bonds
            .binary_search_by_key(&bond.order_key(), |b| b.order_key())
        {
            Ok(_) => false,
            Err(at) => {
                self.bonds.insert(at, bond);
                true
            }
        }
    }

    /// The subset of bonds with both endpoints in `members` — the damage that
    /// travels with a detached child body (wiring is increment 2).
    pub fn subset_within(&self, members: &BTreeSet<(i64, i64, i64)>) -> Self {
        Self {
            bonds: self
                .bonds
                .iter()
                .copied()
                .filter(|b| members.contains(&key(b.a)) && members.contains(&key(b.b)))
                .collect(),
        }
    }

    /// Little-endian `i64`-sextuple-per-bond encoding, canonical order — the
    /// bytes fed to BLAKE3 for the authoritative structural layer hash.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.bonds.len() * 48);
        out.extend_from_slice(&(self.bonds.len() as u64).to_le_bytes());
        for b in &self.bonds {
            for v in [b.a.x, b.a.y, b.a.z, b.b.x, b.b.y, b.b.z] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }
}

/// Per-material fixed-integer tables, derived once from the world manifest.
#[derive(Debug, Clone, Default)]
pub struct StrengthParams {
    /// Dead load of one cell, in grams.
    weight: BTreeMap<u16, u64>,
    /// Axial capacity of one face bond governed by this material, in grams. `0`
    /// for a non-structural or unknown material.
    axial_cap: BTreeMap<u16, u64>,
}

impl StrengthParams {
    /// Derives the tables for `cell_size` from `manifest`. This is the only
    /// floating-point step in the whole model; every value is immediately
    /// quantized to an integer and is the fixed input to propagation.
    pub fn from_manifest(manifest: &MaterialManifest, cell_size: CellSizeCode) -> Self {
        let v_cell_m3 = cell_size.metres().powi(3);
        let mut weight = BTreeMap::new();
        let mut axial_cap = BTreeMap::new();
        for def in manifest.entries() {
            if def.id.is_air() {
                continue;
            }
            weight.insert(
                def.id.raw(),
                grams(f64::from(def.sim.density_kg_m3) * v_cell_m3),
            );
            let cap = if def.sim.flags.contains(MaterialFlags::STRUCTURAL) {
                round_pos(f64::from(def.sim.bond_strength) * BOND_SCALE)
            } else {
                0
            };
            axial_cap.insert(def.id.raw(), cap);
        }
        Self { weight, axial_cap }
    }

    fn weight_of(&self, m: MaterialId) -> u64 {
        self.weight.get(&m.raw()).copied().unwrap_or(0)
    }

    fn axial_cap_of(&self, m: MaterialId) -> u64 {
        self.axial_cap.get(&m.raw()).copied().unwrap_or(0)
    }
}

/// `round(kg · 1000)` as grams, guarding non-finite / non-positive inputs.
fn grams(kg: f64) -> u64 {
    round_pos(kg * 1000.0)
}

/// `round(x)` as `u64`, or `0` for a non-finite / non-positive `x`.
fn round_pos(x: f64) -> u64 {
    if !x.is_finite() || x <= 0.0 {
        0
    } else {
        x.round() as u64
    }
}

/// One bond failure, in the order it was broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondFailure {
    pub bond: BrokenBond,
    /// Load carried through the bond, grams (× moment arm for a lateral bond).
    pub demand: u128,
    /// The bond's capacity, grams.
    pub capacity: u128,
    /// Iteration index at which it broke (0-based).
    pub iteration: u32,
    /// `true` for a bending joint, `false` for an axial one.
    pub lateral: bool,
}

/// A connected mass with no anchored cell under the final damage state — handed
/// to the unchanged T07/T08 split path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedComponent {
    /// Member cells, canonical `(z, y, x)` order.
    pub cells: Vec<GlobalCell>,
    pub cell_count: u64,
}

/// The result of a strength evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrengthReport {
    /// Input damage ∪ every bond broken by this pass, canonical.
    pub damage: DamageState,
    /// Bond failures in the order they occurred.
    pub failures: Vec<BondFailure>,
    /// Components left with no anchor after the cascade, canonical order.
    pub detached: Vec<DetachedComponent>,
    /// `true` when the cascade reached a fixed point (no `MAX_ITERS` cut-off).
    pub stable: bool,
    /// Cascade iterations run.
    pub iterations: u32,
    /// Same-material connected clusters over the final live-bond graph.
    pub clusters: usize,
    /// Supported non-anchor cells that received a load test in the final state.
    pub evaluated_cells: u64,
}

impl StrengthReport {
    /// Total cells across every detached component.
    pub fn detached_cells(&self) -> u64 {
        self.detached.iter().map(|c| c.cell_count).sum()
    }
}

const DIRS: [(i64, i64, i64); 6] = [
    (-1, 0, 0),
    (1, 0, 0),
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
];

/// Runs the frozen T22 model over every resident brick of `volume`.
///
/// `initial_damage` is the volume's persisted [`DamageState`]; on a fresh world
/// pass [`DamageState::new`]. The returned [`StrengthReport::damage`] is the new
/// authoritative state to persist.
pub fn evaluate(
    volume: &Volume,
    anchor: AnchorPlane,
    params: &StrengthParams,
    initial_damage: &DamageState,
) -> StrengthReport {
    // --- 1. Collect solid cells in canonical order. ------------------------
    let mut solids: Vec<(GlobalCell, MaterialId)> = Vec::new();
    for coord in volume.resident_brick_coords() {
        let Some(snap) = volume.snapshot_brick(coord).ok().flatten() else {
            continue;
        };
        for li in 0..CELLS_PER_BRICK as u16 {
            let lc = LocalCell::from_linear_index(li).expect("index < 32768");
            let m = snap.get(lc);
            if m.is_air() {
                continue;
            }
            let g = GlobalCell::from_parts(coord, lc).expect("resident brick within i64 range");
            solids.push((g, m));
        }
    }
    solids.sort_by_key(|(p, _)| key(*p));
    solids.dedup_by_key(|(p, _)| key(*p));

    let n = solids.len();
    let pos: Vec<GlobalCell> = solids.iter().map(|(g, _)| *g).collect();
    let mat: Vec<MaterialId> = solids.iter().map(|(_, m)| *m).collect();
    let index: BTreeMap<(i64, i64, i64), usize> =
        pos.iter().enumerate().map(|(i, g)| (key(*g), i)).collect();

    // --- 2. Static face adjacency (independent of breakage). --------------
    // adj[i] holds (neighbour index, is_lateral) in the fixed DIRS order.
    let mut adj: Vec<Vec<(usize, bool)>> = vec![Vec::new(); n];
    for i in 0..n {
        let g = pos[i];
        for (dx, dy, dz) in DIRS {
            if let Some(&j) = index.get(&(g.z + dz, g.y + dy, g.x + dx)) {
                adj[i].push((j, dy == 0));
            }
        }
    }
    let live_bond_count: u64 = adj.iter().map(|a| a.len() as u64).sum::<u64>() / 2;
    let max_iters = live_bond_count.saturating_mul(2).saturating_add(64);

    // --- 3. Seed the broken set from persisted damage. --------------------
    let mut broken: BTreeSet<(usize, usize)> = BTreeSet::new();
    for b in initial_damage.bonds() {
        if let (Some(&i), Some(&j)) = (index.get(&key(b.a)), index.get(&key(b.b))) {
            broken.insert((i.min(j), i.max(j)));
        }
    }
    let is_live = |i: usize, j: usize, broken: &BTreeSet<(usize, usize)>| {
        !broken.contains(&(i.min(j), i.max(j)))
    };

    let anchor_indices: Vec<usize> = (0..n).filter(|&i| pos[i].y == anchor.y).collect();

    // --- 4. Cascade: break one worst bond per iteration. -----------------
    let mut damage = initial_damage.clone();
    let mut failures: Vec<BondFailure> = Vec::new();
    let mut iterations = 0u32;
    let stable;
    loop {
        let field = build_field(&adj, &broken, &anchor_indices, &pos, n, &is_live);
        let load = accumulate_loads(&field, &mat, params, n);

        let mut worst: Option<Worst> = None;
        for i in 0..n {
            let Some(p) = field.parent[i] else { continue };
            let lateral = field.parent_lateral[i];
            let demand: u128 = if lateral {
                u128::from(load[i]).saturating_mul(1 + u128::from(field.h[i]))
            } else {
                u128::from(load[i])
            };
            let base = params.axial_cap_of(mat[i]).min(params.axial_cap_of(mat[p]));
            let capacity: u128 = if lateral {
                u128::from(base) * LATERAL_NUM / LATERAL_DEN
            } else {
                u128::from(base)
            };
            if demand > capacity {
                let cand = Worst {
                    i,
                    p,
                    demand,
                    capacity,
                    lateral,
                    bond_key: BrokenBond::new(pos[i], pos[p]).order_key(),
                };
                worst = Some(match worst {
                    Some(w) if !cand.worse_than(&w) => w,
                    _ => cand,
                });
            }
        }

        match worst {
            None => {
                stable = true;
                break;
            }
            Some(_) if iterations >= max_iters as u32 => {
                stable = false;
                break;
            }
            Some(w) => {
                broken.insert((w.i.min(w.p), w.i.max(w.p)));
                let bond = BrokenBond::new(pos[w.i], pos[w.p]);
                damage.insert(bond);
                failures.push(BondFailure {
                    bond,
                    demand: w.demand,
                    capacity: w.capacity,
                    iteration: iterations,
                    lateral: w.lateral,
                });
                iterations += 1;
            }
        }
    }

    // --- 5. Final connectivity: detached components and clusters. --------
    let final_field = build_field(&adj, &broken, &anchor_indices, &pos, n, &is_live);
    let evaluated_cells = final_field.parent.iter().filter(|p| p.is_some()).count() as u64;

    let (detached, clusters) = final_components(&adj, &broken, &pos, &mat, anchor, n, &is_live);

    StrengthReport {
        damage,
        failures,
        detached,
        stable,
        iterations,
        clusters,
        evaluated_cells,
    }
}

/// The support field for one cascade iteration.
struct Field {
    parent: Vec<Option<usize>>,
    parent_lateral: Vec<bool>,
    h: Vec<u64>,
    /// BFS visitation order (non-decreasing support distance).
    order: Vec<usize>,
}

fn build_field(
    adj: &[Vec<(usize, bool)>],
    broken: &BTreeSet<(usize, usize)>,
    anchors: &[usize],
    _pos: &[GlobalCell],
    n: usize,
    is_live: &impl Fn(usize, usize, &BTreeSet<(usize, usize)>) -> bool,
) -> Field {
    const INF: u64 = u64::MAX;
    let mut d = vec![INF; n];
    let mut order = Vec::with_capacity(n);
    let mut q = VecDeque::new();
    for &a in anchors {
        if d[a] == INF {
            d[a] = 0;
            q.push_back(a);
        }
    }
    while let Some(u) = q.pop_front() {
        order.push(u);
        for &(v, _) in &adj[u] {
            if is_live(u, v, broken) && d[v] == INF {
                d[v] = d[u] + 1;
                q.push_back(v);
            }
        }
    }

    // Parent = live neighbour one step closer to an anchor, smallest by key.
    // `adj` is already in DIRS order and indices follow canonical cell order,
    // but neighbour *indices* are not key-sorted, so compare keys explicitly by
    // walking candidates and keeping the smallest neighbour index — which *is*
    // canonical, because `pos` is sorted by key.
    let mut parent = vec![None; n];
    let mut parent_lateral = vec![false; n];
    for &i in &order {
        if d[i] == 0 {
            continue;
        }
        let mut best: Option<(usize, bool)> = None;
        for &(v, lateral) in &adj[i] {
            if !is_live(i, v, broken) || d[v] + 1 != d[i] {
                continue;
            }
            match best {
                Some((bv, _)) if v >= bv => {}
                _ => best = Some((v, lateral)),
            }
        }
        if let Some((v, lateral)) = best {
            parent[i] = Some(v);
            parent_lateral[i] = lateral;
        }
    }

    let mut h = vec![0u64; n];
    for &i in &order {
        if let Some(p) = parent[i] {
            h[i] = h[p] + u64::from(parent_lateral[i]);
        }
    }

    Field {
        parent,
        parent_lateral,
        h,
        order,
    }
}

/// Post-order accumulated load: `L(c) = weight(c) + Σ L(child)`, saturating.
fn accumulate_loads(
    field: &Field,
    mat: &[MaterialId],
    params: &StrengthParams,
    n: usize,
) -> Vec<u64> {
    let mut load = vec![0u64; n];
    for &i in &field.order {
        load[i] = params.weight_of(mat[i]);
    }
    // A child always has `d = parent.d + 1`, so reverse BFS order visits every
    // child before its parent.
    for &i in field.order.iter().rev() {
        if let Some(p) = field.parent[i] {
            load[p] = load[p].saturating_add(load[i]);
        }
    }
    load
}

struct Worst {
    i: usize,
    p: usize,
    demand: u128,
    capacity: u128,
    lateral: bool,
    bond_key: ((i64, i64, i64), (i64, i64, i64)),
}

impl Worst {
    /// `true` when `self` is more overloaded than `other` — higher
    /// `demand / capacity`, ties broken by canonical bond key. Division is
    /// avoided by cross-multiplication in `u128` (saturating; fixture-scale
    /// values never approach the ceiling).
    fn worse_than(&self, other: &Worst) -> bool {
        let lhs = self.demand.saturating_mul(other.capacity);
        let rhs = other.demand.saturating_mul(self.capacity);
        match lhs.cmp(&rhs) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => self.bond_key < other.bond_key,
        }
    }
}

/// Connected components over the final live-bond graph: the unanchored ones
/// (canonical order) plus a count of same-material clusters.
fn final_components(
    adj: &[Vec<(usize, bool)>],
    broken: &BTreeSet<(usize, usize)>,
    pos: &[GlobalCell],
    mat: &[MaterialId],
    anchor: AnchorPlane,
    n: usize,
    is_live: &impl Fn(usize, usize, &BTreeSet<(usize, usize)>) -> bool,
) -> (Vec<DetachedComponent>, usize) {
    let mut comp = vec![usize::MAX; n];
    let mut detached = Vec::new();
    let mut next = 0usize;
    for start in 0..n {
        if comp[start] != usize::MAX {
            continue;
        }
        let cid = next;
        next += 1;
        comp[start] = cid;
        let mut stack = vec![start];
        let mut members = Vec::new();
        let mut anchored = false;
        while let Some(u) = stack.pop() {
            members.push(u);
            anchored |= pos[u].y == anchor.y;
            for &(v, _) in &adj[u] {
                if comp[v] == usize::MAX && is_live(u, v, broken) {
                    comp[v] = cid;
                    stack.push(v);
                }
            }
        }
        if !anchored {
            members.sort_by(|&x, &y| key(pos[x]).cmp(&key(pos[y])));
            let cells: Vec<GlobalCell> = members.iter().map(|&x| pos[x]).collect();
            detached.push(DetachedComponent {
                cell_count: cells.len() as u64,
                cells,
            });
        }
    }
    detached.sort_by(|a, b| key(a.cells[0]).cmp(&key(b.cells[0])));

    // Clusters: same as components, but an edge also needs equal material.
    let mut cluster = vec![usize::MAX; n];
    let mut clusters = 0usize;
    for start in 0..n {
        if cluster[start] != usize::MAX {
            continue;
        }
        let cid = clusters;
        clusters += 1;
        cluster[start] = cid;
        let mut stack = vec![start];
        while let Some(u) = stack.pop() {
            for &(v, _) in &adj[u] {
                if cluster[v] == usize::MAX && mat[v] == mat[u] && is_live(u, v, broken) {
                    cluster[v] = cid;
                    stack.push(v);
                }
            }
        }
    }

    (detached, clusters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{
        CellSizeCode, MaterialDef, MaterialFlags, MaterialId, RenderProps, SimProps, VolumeId,
    };
    use spall_voxel::{EditPlan, Volume};

    const COLUMN: MaterialId = MaterialId(1); // strong, only used for anchored columns
    const BEAM_WEAK: MaterialId = MaterialId(2);
    const BEAM_STRONG: MaterialId = MaterialId(3);

    fn mat(id: u16, name: &str, density: f32, bond: f32) -> MaterialDef {
        MaterialDef {
            id: MaterialId(id),
            name: name.into(),
            render: RenderProps {
                albedo: [0.5; 3],
                roughness: 0.9,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: density,
                friction: 0.8,
                restitution: 0.05,
                hardness: 4.0,
                bond_strength: bond,
                flags: MaterialFlags(
                    MaterialFlags::OPAQUE.0
                        | MaterialFlags::COLLIDES.0
                        | MaterialFlags::STRUCTURAL.0,
                ),
            },
        }
    }

    fn manifest() -> MaterialManifest {
        MaterialManifest::validated(vec![
            MaterialDef {
                id: MaterialId::AIR,
                name: "air".into(),
                render: RenderProps {
                    albedo: [0.0; 3],
                    roughness: 1.0,
                    metalness: 0.0,
                    emissive: [0.0; 3],
                },
                sim: SimProps {
                    density_kg_m3: 0.0,
                    friction: 0.0,
                    restitution: 0.0,
                    hardness: 0.0,
                    bond_strength: 0.0,
                    flags: MaterialFlags::NONE,
                },
            },
            mat(1, "column", 4000.0, 60.0),
            mat(2, "beam_weak", 2000.0, 3.0),
            mat(3, "beam_strong", 2000.0, 60.0),
        ])
        .expect("hand-built manifest is valid")
    }

    fn params() -> StrengthParams {
        StrengthParams::from_manifest(&manifest(), CellSizeCode::Quarter)
    }

    fn vol() -> Volume {
        Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter)
    }

    fn fill(v: &mut Volume, a: GlobalCell, b: GlobalCell, m: MaterialId) {
        v.apply_edit(&EditPlan::filled_box(v.id(), a, b, m))
            .unwrap();
    }

    /// A strong column on the anchor plane (`y = 0`), 7 cells tall at x = 0.
    fn with_column(v: &mut Volume) {
        fill(
            v,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 6, 0),
            COLUMN,
        );
    }

    #[test]
    fn params_quantize_the_manifest() {
        let p = params();
        // 2000 kg/m³ · 0.25³ m³ · 1000 = 31 250 g.
        assert_eq!(p.weight_of(BEAM_WEAK), 31_250);
        assert_eq!(p.weight_of(COLUMN), 62_500);
        // bond_strength 3 · 100 000 = 300 000 g axial.
        assert_eq!(p.axial_cap_of(BEAM_WEAK), 300_000);
        assert_eq!(p.axial_cap_of(BEAM_STRONG), 6_000_000);
        assert_eq!(p.weight_of(MaterialId(99)), 0);
    }

    #[test]
    fn weak_cantilever_fails_and_sheds_its_span() {
        let mut v = vol();
        with_column(&mut v);
        // 10-cell weak beam off the column top, y = 6, x = 1..=10.
        fill(
            &mut v,
            GlobalCell::new(1, 6, 0),
            GlobalCell::new(10, 6, 0),
            BEAM_WEAK,
        );

        let r = evaluate(&v, AnchorPlane::at(0), &params(), &DamageState::new());

        assert!(r.stable, "cascade must reach a fixed point");
        assert!(!r.failures.is_empty(), "a long weak cantilever must fail");
        assert!(
            r.failures.iter().all(|f| f.lateral),
            "beam joints are lateral"
        );
        // Everything past the first retained cell sheds; the column stays.
        assert!(r.detached_cells() >= 8, "most of the span detaches");
        for c in &r.detached {
            for cell in &c.cells {
                assert_eq!(cell.y, 6, "only beam cells detach");
                assert!(cell.x >= 1);
            }
        }
        // The persisted damage grew by exactly the failure count.
        assert_eq!(r.damage.len(), r.failures.len());
    }

    #[test]
    fn strong_cantilever_of_the_same_geometry_holds() {
        let mut v = vol();
        with_column(&mut v);
        fill(
            &mut v,
            GlobalCell::new(1, 6, 0),
            GlobalCell::new(10, 6, 0),
            BEAM_STRONG,
        );

        let r = evaluate(&v, AnchorPlane::at(0), &params(), &DamageState::new());

        assert!(r.stable);
        assert!(
            r.failures.is_empty(),
            "material capacity changes the outcome"
        );
        assert!(r.detached.is_empty());
        assert!(r.damage.is_empty());
        assert!(r.evaluated_cells >= 10, "the whole beam was load-tested");
    }

    #[test]
    fn a_short_weak_stub_holds() {
        let mut v = vol();
        with_column(&mut v);
        // One weak cell only.
        fill(
            &mut v,
            GlobalCell::new(1, 6, 0),
            GlobalCell::new(1, 6, 0),
            BEAM_WEAK,
        );

        let r = evaluate(&v, AnchorPlane::at(0), &params(), &DamageState::new());

        assert!(r.stable);
        assert!(
            r.failures.is_empty(),
            "the model is span/load dependent, not material-fatal"
        );
        assert!(r.detached.is_empty());
    }

    #[test]
    fn undermining_the_ground_cascades_predictably() {
        // Wide weak slab at y = 1, x = 0..=9, resting on an anchor row at y = 0.
        let build = |undermine_from: i64| {
            let mut v = vol();
            fill(
                &mut v,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(9, 0, 0),
                COLUMN,
            );
            fill(
                &mut v,
                GlobalCell::new(0, 1, 0),
                GlobalCell::new(9, 1, 0),
                BEAM_WEAK,
            );
            // Remove the anchor cells from `undermine_from..=9`.
            fill(
                &mut v,
                GlobalCell::new(undermine_from, 0, 0),
                GlobalCell::new(9, 0, 0),
                MaterialId::AIR,
            );
            evaluate(&v, AnchorPlane::at(0), &params(), &DamageState::new())
        };

        let narrow = build(6); // undermine x = 6..=9
        let wide = build(3); // undermine x = 3..=9

        assert!(narrow.stable && wide.stable);
        assert!(!narrow.failures.is_empty());
        assert!(
            wide.detached_cells() >= narrow.detached_cells(),
            "removing more support detaches at least as much: {} vs {}",
            wide.detached_cells(),
            narrow.detached_cells()
        );
        // Failures are ordered — each iteration index is strictly increasing.
        for w in narrow.failures.windows(2) {
            assert!(w[0].iteration < w[1].iteration);
        }
    }

    #[test]
    fn a_pre_damaged_joint_changes_a_holding_bridge_into_a_failure() {
        // Beam bridged between two strong columns at x = 0 and x = 6.
        let bridge = || {
            let mut v = vol();
            fill(
                &mut v,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 6, 0),
                COLUMN,
            );
            fill(
                &mut v,
                GlobalCell::new(6, 0, 0),
                GlobalCell::new(6, 6, 0),
                COLUMN,
            );
            fill(
                &mut v,
                GlobalCell::new(1, 6, 0),
                GlobalCell::new(5, 6, 0),
                MaterialId(4),
            );
            v
        };
        // Beam material id 4: light, mid-strength — holds as a bridge, fails as
        // a one-sided cantilever.
        let m = MaterialManifest::validated(vec![
            MaterialDef {
                id: MaterialId::AIR,
                name: "air".into(),
                render: RenderProps {
                    albedo: [0.0; 3],
                    roughness: 1.0,
                    metalness: 0.0,
                    emissive: [0.0; 3],
                },
                sim: SimProps {
                    density_kg_m3: 0.0,
                    friction: 0.0,
                    restitution: 0.0,
                    hardness: 0.0,
                    bond_strength: 0.0,
                    flags: MaterialFlags::NONE,
                },
            },
            mat(1, "column", 4000.0, 60.0),
            mat(2, "beam_weak", 2000.0, 3.0),
            mat(3, "beam_strong", 2000.0, 60.0),
            mat(4, "beam_mid", 2000.0, 5.0),
        ])
        .unwrap();
        let p = StrengthParams::from_manifest(&m, CellSizeCode::Quarter);

        let clean = evaluate(&bridge(), AnchorPlane::at(0), &p, &DamageState::new());
        assert!(clean.stable && clean.failures.is_empty() && clean.detached.is_empty());

        // Pre-break the joint at the x = 6 column: (5,6,0)–(6,6,0).
        let damaged = DamageState::from_bonds([BrokenBond::new(
            GlobalCell::new(5, 6, 0),
            GlobalCell::new(6, 6, 0),
        )]);
        let after = evaluate(&bridge(), AnchorPlane::at(0), &p, &damaged);

        assert!(after.stable);
        assert!(
            !after.failures.is_empty(),
            "the one-sided cantilever must now fail"
        );
        assert!(!after.detached.is_empty());
        // The seeded bond is still present and is not re-counted as a failure.
        assert!(
            after
                .damage
                .contains(GlobalCell::new(5, 6, 0), GlobalCell::new(6, 6, 0))
        );
        assert!(
            after.failures.iter().all(|f| f.bond != damaged.bonds()[0]),
            "a pre-broken bond is not reported as a fresh failure"
        );
    }

    #[test]
    fn failure_order_is_deterministic_under_reordered_construction() {
        let forward = || {
            let mut v = vol();
            with_column(&mut v);
            fill(
                &mut v,
                GlobalCell::new(1, 6, 0),
                GlobalCell::new(10, 6, 0),
                BEAM_WEAK,
            );
            v
        };
        let piecewise = || {
            let mut v = vol();
            // Same cells, applied in a different order and in fragments.
            fill(
                &mut v,
                GlobalCell::new(10, 6, 0),
                GlobalCell::new(10, 6, 0),
                BEAM_WEAK,
            );
            fill(
                &mut v,
                GlobalCell::new(1, 6, 0),
                GlobalCell::new(5, 6, 0),
                BEAM_WEAK,
            );
            with_column(&mut v);
            fill(
                &mut v,
                GlobalCell::new(6, 6, 0),
                GlobalCell::new(9, 6, 0),
                BEAM_WEAK,
            );
            v
        };

        let a = evaluate(
            &forward(),
            AnchorPlane::at(0),
            &params(),
            &DamageState::new(),
        );
        let b = evaluate(
            &piecewise(),
            AnchorPlane::at(0),
            &params(),
            &DamageState::new(),
        );

        assert_eq!(a.failures, b.failures);
        assert_eq!(a.damage, b.damage);
        assert_eq!(a.detached, b.detached);
        assert_eq!(a.iterations, b.iterations);
    }

    #[test]
    fn restarting_from_a_reports_damage_heals_nothing() {
        let mut v = vol();
        with_column(&mut v);
        fill(
            &mut v,
            GlobalCell::new(1, 6, 0),
            GlobalCell::new(10, 6, 0),
            BEAM_WEAK,
        );

        let first = evaluate(&v, AnchorPlane::at(0), &params(), &DamageState::new());
        assert!(!first.failures.is_empty());

        // Feed the persisted damage straight back in — a restart.
        let second = evaluate(&v, AnchorPlane::at(0), &params(), &first.damage);
        assert!(second.stable);
        assert!(
            second.failures.is_empty(),
            "the fixed point is stable: nothing new breaks"
        );
        assert_eq!(
            second.damage, first.damage,
            "damage is neither healed nor grown"
        );
        assert_eq!(second.detached_cells(), first.detached_cells());
    }

    #[test]
    fn damage_state_canonical_bytes_are_order_independent() {
        let a = DamageState::from_bonds([
            BrokenBond::new(GlobalCell::new(2, 6, 0), GlobalCell::new(3, 6, 0)),
            BrokenBond::new(GlobalCell::new(1, 6, 0), GlobalCell::new(1, 7, 0)),
        ]);
        let b = DamageState::from_bonds([
            BrokenBond::new(GlobalCell::new(1, 7, 0), GlobalCell::new(1, 6, 0)),
            BrokenBond::new(GlobalCell::new(3, 6, 0), GlobalCell::new(2, 6, 0)),
        ]);
        assert_eq!(a, b);
        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
        assert_eq!(a.canonical_bytes().len(), 8 + 2 * 48);
    }

    #[test]
    fn broken_bond_endpoints_are_canonical_regardless_of_arg_order() {
        let p = GlobalCell::new(3, 6, 0);
        let q = GlobalCell::new(2, 6, 0); // key(q) < key(p)

        // `new` is the only constructor; it orders the endpoints either way.
        let forward = BrokenBond::new(q, p);
        let reversed = BrokenBond::new(p, q);
        assert_eq!(forward, reversed);
        assert_eq!(forward.a(), q);
        assert_eq!(forward.b(), p);

        // A state seeded from reversed raw endpoints still normalizes, so
        // `contains` (which normalizes its query) finds the bond and the
        // authoritative bytes match the forward-built state.
        let from_reversed = DamageState::from_bonds([BrokenBond::new(p, q)]);
        let from_forward = DamageState::from_bonds([BrokenBond::new(q, p)]);
        assert!(from_reversed.contains(p, q));
        assert!(from_reversed.contains(q, p));
        assert_eq!(
            from_reversed.canonical_bytes(),
            from_forward.canonical_bytes()
        );

        // `insert` agrees with `contains` on an already-recorded bond.
        let mut s = DamageState::new();
        assert!(s.insert(BrokenBond::new(p, q)));
        assert!(!s.insert(BrokenBond::new(q, p)));
        assert_eq!(s.len(), 1);
    }
}
