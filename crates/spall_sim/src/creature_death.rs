//! Striking and killing creatures, and the terrain cells creatures edit.
//!
//! A tool strike hits a creature when the aim ray meets one of its posed voxels
//! (from the game's [`CreatureShape`]). Killing a creature despawns it and
//! leaves a carcass of those voxels in the terrain: each source voxel is
//! quantised to the terrain grid, the carcass is dropped straight down until it
//! rests on the ground, and every empty cell it covers is filled through the
//! ordinary committed edit path (one single-cell `Place` per cell, the way
//! vegetation grows). The carcass is then plain terrain: it replicates like any
//! edit and can be cut away cell by cell.
//!
//! A placed cell that touches no solid terrain is split off as a loose body,
//! and the edit pipeline does not keep commit order across retries (ENG-242).
//! So a carcass goes in by layers of support: first the cells touching the
//! ground, then, once that layer has committed, the cells touching it, and so
//! on. Every cell then lands next to terrain whatever order its own layer
//! commits in. Cells the carcass cannot reach from the ground are left out.
//!
//! A creature can also kill another ([`Simulation::kill_creature_by`]) and
//! fill or clear single terrain cells ([`Simulation::creature_edit_cell`]),
//! as a bear eating a carcass does. Each such cell takes the same committed
//! edit path under a server request id.
use std::collections::{BTreeMap, HashMap};

use super::*;
use spall_protocol::{ActionOutcome, CreatureRecord};

use crate::creatures::{CreatureShape, CreatureVoxel};

/// A creature whose root is farther than this beyond a strike's reach cannot
/// be hit, so its voxels are never sampled. Larger than any animal's extent
/// from its root.
const STRIKE_MARGIN_M: f64 = 4.0;
/// Most carcass cells placed for one death. The edit queue holds 256 pending
/// intents; a deer at 0.25 m cells is well under this.
const MAX_CARCASS_CELLS: usize = 192;
/// Furthest a carcass falls to reach the ground, in terrain cells (64 m).
const MAX_CARCASS_DROP_CELLS: i64 = 256;
/// Journal provenance for a cell the server edits on a creature's account: a
/// carcass, or a creature's own bite or placement.
const CREATURE_EDIT_ACTOR_ID: u64 = (1 << 40) + 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreatureKillError {
    #[error("creature is not hosted")]
    NotHosted,
    #[error("no creature shape is installed")]
    NoShape,
    #[error("the edit queue cannot take the carcass now")]
    QueueFull,
}

/// The six face neighbours of a cell.
const FACES: [[i64; 3]; 6] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];

/// A carcass still going in: the layers not yet queued, and the requests of
/// the layer in flight.
#[derive(Debug)]
pub(super) struct PendingCarcass {
    layers: VecDeque<Vec<(GlobalCell, MaterialId)>>,
    in_flight: Vec<RequestId>,
}

/// What a death left behind.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatureKill {
    /// The creature as it was when it died.
    pub record: CreatureRecord,
    /// Terrain cells planned for the carcass, in placement order. The first
    /// layer is queued at once; the rest follow as the layers below commit.
    pub carcass: Vec<(GlobalCell, MaterialId)>,
    /// Status of the first carcass cell, admitted under the caller's request
    /// id when one was given. `None` when nothing could be placed.
    pub first_status: Option<ActionStatus>,
}

impl Simulation {
    /// Installs the game's creature shape, used for strikes and carcasses.
    pub fn set_creature_shape(&mut self, shape: CreatureShape) -> Result<(), CreatureKillError> {
        self.creatures
            .as_mut()
            .ok_or(CreatureKillError::NotHosted)?
            .set_shape(shape);
        Ok(())
    }

    /// The nearest creature the ray meets within `reach_m`, and the distance
    /// to it. `None` without a shape or a hit.
    pub fn creature_struck(
        &self,
        origin_m: [f64; 3],
        dir: [f64; 3],
        reach_m: f64,
    ) -> Option<(u64, f64)> {
        let host = self.creatures.as_ref()?;
        let shape = host.shape()?;
        let origin = DVec3::from_array(origin_m);
        let dir = DVec3::from_array(dir).try_normalize()?;
        let mut best: Option<(u64, f64)> = None;
        for creature in host.iter() {
            let root = DVec3::from_array(creature.record.translation_m);
            if root.distance(origin) > reach_m + STRIKE_MARGIN_M {
                continue;
            }
            for voxel in shape.voxels(&creature.record) {
                // A rotated cube fits in this axis-aligned box.
                let half = f64::from(voxel.edge_m) * 0.5 * 3f64.sqrt();
                let centre = DVec3::from_array(voxel.centre_m);
                if let Some(t) = ray_box(origin, dir, centre - half, centre + half)
                    && t <= reach_m
                    && best.is_none_or(|(_, b)| t < b)
                {
                    best = Some((creature.record.creature_id, t));
                }
            }
        }
        best
    }

    /// Kills a creature: removes it and queues its carcass. The first carcass
    /// cell is admitted under `request_id` when given (the strike that killed
    /// it), the rest under server request ids. Only the layer touching the
    /// ground is queued now; each later layer is queued by the tick after the
    /// one below it finishes. Nothing changes if the first cell cannot be
    /// admitted.
    pub fn kill_creature(
        &mut self,
        creature_id: u64,
        request_id: Option<RequestId>,
        actor: EntityId,
    ) -> Result<CreatureKill, CreatureKillError> {
        let host = self
            .creatures
            .as_ref()
            .ok_or(CreatureKillError::NotHosted)?;
        let record = host
            .get(creature_id)
            .ok_or(CreatureKillError::NotHosted)?
            .record
            .clone();
        let voxels = host
            .shape()
            .ok_or(CreatureKillError::NoShape)?
            .voxels(&record);
        let mut layers = self.carcass_layers(&voxels);
        let carcass: Vec<_> = layers.iter().flatten().copied().collect();

        let mut first_status = None;
        if let Some(first) = layers.pop_front() {
            let mut in_flight = Vec::with_capacity(first.len());
            for (i, &(cell, material)) in first.iter().enumerate() {
                let (request, by) = match (i, request_id) {
                    (0, Some(request)) => (request, actor),
                    _ => (self.next_server_request(), creature_edit_actor()),
                };
                let intent = single_cell_intent(cell, EditKind::Place(material), request, by);
                match self.submit(intent) {
                    Ok(status) => {
                        first_status.get_or_insert(status);
                        in_flight.push(request);
                    }
                    Err(_) if i == 0 => return Err(CreatureKillError::QueueFull),
                    Err(_) => {
                        // The queue is full: the rest of this layer waits.
                        layers.push_front(first[i..].to_vec());
                        break;
                    }
                }
            }
            if !layers.is_empty() {
                self.carcasses.push(PendingCarcass { layers, in_flight });
            }
        }
        self.despawn_creature(creature_id);
        Ok(CreatureKill {
            record,
            carcass,
            first_status,
        })
    }

    /// Queues the next layer of every carcass whose layer in flight has
    /// finished (committed or rejected). A cell is queued only if it is still
    /// empty and touches solid terrain, so a layer whose support was cut away
    /// meanwhile places no floating cell. Runs after the edit pipeline each
    /// tick.
    pub(super) fn advance_carcasses(&mut self) {
        let mut carcasses = std::mem::take(&mut self.carcasses);
        carcasses.retain_mut(|carcass| {
            let busy = carcass.in_flight.iter().any(|request| {
                self.pipeline
                    .action_status(*request)
                    .is_some_and(|status| status.outcome == ActionOutcome::Queued)
            });
            if busy {
                return true;
            }
            carcass.in_flight.clear();
            while carcass.in_flight.is_empty()
                && let Some(layer) = carcass.layers.pop_front()
            {
                for (i, &(cell, material)) in layer.iter().enumerate() {
                    if !self.carcass_cell_supported(cell) {
                        continue;
                    }
                    let request = self.next_server_request();
                    let intent = single_cell_intent(
                        cell,
                        EditKind::Place(material),
                        request,
                        creature_edit_actor(),
                    );
                    if self.submit(intent).is_err() {
                        // The queue is full: the rest of this layer waits.
                        carcass.layers.push_front(layer[i..].to_vec());
                        return true;
                    }
                    carcass.in_flight.push(request);
                }
            }
            !carcass.layers.is_empty() || !carcass.in_flight.is_empty()
        });
        self.carcasses = carcasses;
    }

    /// Whether `cell` is empty and has a solid face neighbour.
    fn carcass_cell_supported(&self, cell: GlobalCell) -> bool {
        let volume = &self.world.terrain().volume;
        matches!(volume.sample(cell), Ok(Sample::Empty { .. }))
            && neighbours(cell)
                .into_iter()
                .any(|n| matches!(volume.sample(n), Ok(Sample::Filled(_))))
    }

    /// A creature kills another: `prey` dies exactly as from a strike, its
    /// whole carcass under server request ids. Fails if either is not hosted.
    pub fn kill_creature_by(
        &mut self,
        predator_id: u64,
        prey_id: u64,
    ) -> Result<CreatureKill, CreatureKillError> {
        if predator_id == prey_id || !self.hosts_creature(predator_id) {
            return Err(CreatureKillError::NotHosted);
        }
        self.kill_creature(prey_id, None, creature_edit_actor())
    }

    /// A creature changes one terrain cell: `place` fills it if it is empty,
    /// `None` clears it if it is solid. Queued as a single-cell edit under a
    /// server request id, the committed path a carcass takes. `Ok(false)` when
    /// the cell already is what was asked, so nothing was queued. An error
    /// when the creature is not hosted, the cell is not resident, or the edit
    /// queue is full.
    pub fn creature_edit_cell(
        &mut self,
        creature_id: u64,
        cell: GlobalCell,
        place: Option<MaterialId>,
    ) -> Result<bool, String> {
        if !self.hosts_creature(creature_id) {
            return Err(format!("creature {creature_id} is not hosted"));
        }
        let place = place.filter(|material| !material.is_air());
        let kind = match (self.world.terrain().volume.sample(cell), place) {
            (Ok(Sample::Filled(_)), None) => EditKind::Cut,
            (Ok(Sample::Empty { .. }), Some(material)) => EditKind::Place(material),
            (Ok(Sample::Filled(_) | Sample::Empty { .. }), _) => return Ok(false),
            _ => return Err(format!("cell {cell:?} is not resident")),
        };
        let request = self.next_server_request();
        let intent = single_cell_intent(cell, kind, request, creature_edit_actor());
        self.submit(intent).map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn hosts_creature(&self, creature_id: u64) -> bool {
        self.creatures
            .as_ref()
            .is_some_and(|host| host.get(creature_id).is_some())
    }

    fn next_server_request(&mut self) -> RequestId {
        let request = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
        self.next_damage_seq += 1;
        request
    }

    /// The terrain cells a carcass of `voxels` fills, in layers of support:
    /// each terrain cell any voxel centre falls in, with the most common
    /// material there, dropped onto the ground below and limited to cells that
    /// are empty now. The first layer touches solid terrain and each later one
    /// touches the layer before, each in ground-up order; cells not reached
    /// that way are left out.
    fn carcass_layers(&self, voxels: &[CreatureVoxel]) -> VecDeque<Vec<(GlobalCell, MaterialId)>> {
        let terrain = self.world.terrain();
        let cell_m = terrain.cell_size().metres();
        let mut counts: HashMap<GlobalCell, HashMap<MaterialId, u32>> = HashMap::new();
        for voxel in voxels {
            let [x, y, z] = voxel.centre_m.map(|v| (v / cell_m).floor() as i64);
            *counts
                .entry(GlobalCell::new(x, y, z))
                .or_default()
                .entry(voxel.material)
                .or_default() += 1;
        }
        let empty =
            |cell: GlobalCell| matches!(terrain.volume.sample(cell), Ok(Sample::Empty { .. }));

        // Fall as far as the column with the least room below it allows.
        let mut lowest: HashMap<(i64, i64), i64> = HashMap::new();
        for cell in counts.keys() {
            let y = lowest.entry((cell.x, cell.z)).or_insert(cell.y);
            *y = (*y).min(cell.y);
        }
        let drop = lowest
            .iter()
            .map(|(&(x, z), &y)| {
                (0..MAX_CARCASS_DROP_CELLS)
                    .find(|d| !empty(GlobalCell::new(x, y - 1 - d, z)))
                    .unwrap_or(MAX_CARCASS_DROP_CELLS)
            })
            .min()
            .unwrap_or(0);

        // Ground up, so the edit order and any truncation are stable.
        let cells: BTreeMap<(i64, i64, i64), MaterialId> = counts
            .into_iter()
            .map(|(cell, materials)| {
                let material = materials
                    .into_iter()
                    .max_by_key(|&(material, count)| (count, material.0))
                    .map(|(material, _)| material)
                    .expect("a counted cell has a material");
                ((cell.y - drop, cell.x, cell.z), material)
            })
            .collect();
        let cells: Vec<(GlobalCell, MaterialId)> = cells
            .into_iter()
            .map(|((y, x, z), material)| (GlobalCell::new(x, y, z), material))
            .filter(|&(cell, material)| !material.is_air() && empty(cell))
            .collect();

        // Breadth-first from the cells touching solid terrain (carcass cells
        // are all empty now, so none of them counts as support).
        let mut left: HashSet<GlobalCell> = cells.iter().map(|&(cell, _)| cell).collect();
        let mut layer: HashSet<GlobalCell> = left
            .iter()
            .copied()
            .filter(|&cell| {
                neighbours(cell)
                    .into_iter()
                    .any(|n| matches!(terrain.volume.sample(n), Ok(Sample::Filled(_))))
            })
            .collect();
        let mut layers = VecDeque::new();
        let mut placed = 0;
        while !layer.is_empty() && placed < MAX_CARCASS_CELLS {
            left.retain(|cell| !layer.contains(cell));
            let ordered: Vec<_> = cells
                .iter()
                .copied()
                .filter(|(cell, _)| layer.contains(cell))
                .take(MAX_CARCASS_CELLS - placed)
                .collect();
            placed += ordered.len();
            layers.push_back(ordered);
            layer = layer
                .iter()
                .flat_map(|&cell| neighbours(cell))
                .filter(|n| left.contains(n))
                .collect();
        }
        layers
    }
}

/// The six cells sharing a face with `cell`.
fn neighbours(cell: GlobalCell) -> [GlobalCell; 6] {
    FACES.map(|[dx, dy, dz]| GlobalCell::new(cell.x + dx, cell.y + dy, cell.z + dz))
}

fn creature_edit_actor() -> EntityId {
    EntityId::new(CREATURE_EDIT_ACTOR_ID).expect("non-zero reserved actor id")
}

/// An edit of exactly one terrain cell.
fn single_cell_intent(
    cell: GlobalCell,
    kind: EditKind,
    request_id: RequestId,
    actor: EntityId,
) -> EditIntent {
    let centre = BrushPoint::from_units(
        cell.x * BRUSH_UNIT + BRUSH_UNIT / 2,
        cell.y * BRUSH_UNIT + BRUSH_UNIT / 2,
        cell.z * BRUSH_UNIT + BRUSH_UNIT / 2,
    );
    EditIntent {
        request_id,
        actor,
        target: EditTarget::Terrain,
        kind,
        brush: SphereBrush::new(centre, 0).expect("a single-cell brush is valid"),
        explosion: None,
    }
}

/// Distance along a unit ray to an axis-aligned box, `0` from inside.
fn ray_box(origin: DVec3, dir: DVec3, lo: DVec3, hi: DVec3) -> Option<f64> {
    let inv = dir.recip();
    let a = (lo - origin) * inv;
    let b = (hi - origin) * inv;
    let near = a.min(b).max_element().max(0.0);
    let far = a.max(b).min_element();
    (near <= far).then_some(near)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ray_meets_a_box_at_its_near_face_and_misses_beside_it() {
        let (lo, hi) = (DVec3::splat(1.0), DVec3::splat(2.0));
        let hit = ray_box(DVec3::new(0.0, 1.5, 1.5), DVec3::X, lo, hi);
        assert_eq!(hit, Some(1.0));
        assert_eq!(ray_box(DVec3::new(0.0, 2.5, 1.5), DVec3::X, lo, hi), None);
        assert_eq!(ray_box(DVec3::new(3.0, 1.5, 1.5), DVec3::X, lo, hi), None);
        assert_eq!(ray_box(DVec3::splat(1.5), DVec3::Y, lo, hi), Some(0.0));
    }
}
