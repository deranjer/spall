//! Owner-thread integration. Growth uses the existing edit/structural pipeline;
//! no solver, invisible support, direct terrain write or extra ECS entity.
use crate::{EditIntent, EditKind, EditTarget, Simulation, TickError, TickReport};
use spall_core::GlobalCell;
use spall_core::{BRUSH_UNIT, BrushPoint, EntityId, SphereBrush};
use spall_ecology::living::{Growth, LivingState, VisualFrame};
use spall_protocol::RequestId;
use spall_voxel::{Sample, Volume};
use spall_worldgen::{ColumnMap, Preset, WorldGenSpec, WorldgenPalette};
use std::collections::{BTreeMap, VecDeque};

/// Columns of destroyed-cover checks per tick; bigger edits finish over a few
/// ticks.
const CLEAR_COLUMNS_PER_TICK: usize = 6_000;

/// A committed edit's footprint, waiting to be checked column by column for
/// cover it destroyed (cells, inclusive).
struct ClearJob {
    x0: i64,
    x1: i64,
    z1: i64,
    y_lo: i64,
    y_hi: i64,
    next: (i64, i64),
}

pub(crate) struct AuthoritativeVegetation {
    pub state: LivingState,
    columns: ColumnMap,
    pending: BTreeMap<u64, Growth>,
    jobs: VecDeque<ClearJob>,
    dirty: bool,
}

/// The top solid cell of a column near `y_hint`: climbs out of solid, then
/// descends through open air. `None` where terrain is not resident or no
/// ground is found nearby.
fn surface_y(terrain: &Volume, x: i64, z: i64, y_hint: i64) -> Option<i64> {
    let mut y = y_hint;
    for _ in 0..64 {
        match terrain.sample(GlobalCell::new(x, y, z)).ok()? {
            Sample::Unknown(_) => return None,
            Sample::Filled(_) => y += 1,
            Sample::Empty { .. } => break,
        }
    }
    for _ in 0..160 {
        match terrain.sample(GlobalCell::new(x, y, z)).ok()? {
            Sample::Unknown(_) => return None,
            Sample::Filled(_) => return Some(y),
            Sample::Empty { .. } => y -= 1,
        }
    }
    None
}
impl Simulation {
    pub fn install_vegetation(&mut self, state: LivingState) -> Result<(), String> {
        state.validate()?;
        // The columns the world was generated from: its own preset, not
        // always the showcase (a natural world's climate differs).
        let preset = match state.preset {
            1 => Preset::Natural,
            2 => Preset::Gallery,
            _ => Preset::Showcase,
        };
        let spec = WorldGenSpec::new(
            preset,
            state.seed,
            state.size,
            WorldgenPalette::sequential(1),
        );
        let columns = ColumnMap::compute(&spec).map_err(|e| e.to_string())?;
        let mut state = state;
        state.ensure_field(&columns);
        self.vegetation = Some(AuthoritativeVegetation {
            state,
            columns,
            pending: BTreeMap::new(),
            jobs: VecDeque::new(),
            dirty: true,
        });
        Ok(())
    }
    /// Whether the climate columns the vegetation grows against are the
    /// natural preset's (`None` with no vegetation installed).
    pub fn vegetation_climate_is_natural(&self) -> Option<bool> {
        self.vegetation.as_ref().map(|v| v.columns.is_natural())
    }
    pub fn vegetation_state(&self) -> Option<&LivingState> {
        self.vegetation.as_ref().map(|v| &v.state)
    }
    pub fn vegetation_visual(&self) -> Option<VisualFrame> {
        self.vegetation
            .as_ref()
            .map(|v| v.state.visual(&self.world.terrain().volume))
    }
    /// What building the visual frame needs, detached from the simulation so the (milliseconds
    /// long) work can run on another thread: the living state and the terrain, both cheap to
    /// copy (the terrain shares its chunks).
    pub fn vegetation_visual_input(
        &self,
    ) -> Option<(spall_ecology::living::LivingState, spall_voxel::Volume)> {
        self.vegetation
            .as_ref()
            .map(|v| (v.state.clone(), self.world.terrain().volume.clone()))
    }
    pub fn take_vegetation_journal(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(v) = &mut self.vegetation else {
            return Ok(None);
        };
        if !v.dirty {
            return Ok(None);
        }
        let bytes = v.state.encode()?;
        v.dirty = false;
        Ok(Some(bytes))
    }
    pub fn restore_vegetation(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() {
            self.vegetation = None;
            return Ok(());
        }
        self.install_vegetation(LivingState::decode(bytes)?)
    }
    pub(crate) fn advance_vegetation(&mut self, report: &TickReport) -> Result<(), TickError> {
        let Some(mut v) = self.vegetation.take() else {
            return Ok(());
        };
        let terrain_id = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        for (request, committed) in &report.committed {
            if let Some(g) = v.pending.remove(&request.0) {
                v.state.acknowledge(g, &self.world.terrain().volume);
                v.dirty = true;
            } else {
                // An edit by a player or the world: whatever cover it reached
                // is destroyed (see `ClearJob`).
                for (lo, hi) in
                    crate::player::transaction_world_boxes(&committed.topology, terrain_id, cell_m)
                {
                    let cell = |m: f64| (m / cell_m).floor() as i64;
                    let (x0, x1) = (cell(lo[0]), cell(hi[0]));
                    let (z0, z1) = (cell(lo[2]), cell(hi[2]));
                    v.jobs.push_back(ClearJob {
                        x0,
                        x1,
                        z1,
                        y_lo: cell(lo[1]),
                        y_hi: cell(hi[1]),
                        next: (x0, z0),
                    });
                }
            }
        }
        let mut budget = CLEAR_COLUMNS_PER_TICK;
        while budget > 0 {
            let Some(job) = v.jobs.front_mut() else {
                break;
            };
            while budget > 0 && job.next.1 <= job.z1 {
                let (x, z) = job.next;
                job.next = if x >= job.x1 {
                    (job.x0, z + 1)
                } else {
                    (x + 1, z)
                };
                budget -= 1;
                // The cover is gone if the edit reached the surface band of the
                // column as it is now: a tunnel well below it leaves it be.
                if let Some(sy) = surface_y(&self.world.terrain().volume, x, z, job.y_hi + 4)
                    && job.y_hi >= sy - 3
                    && job.y_lo <= sy + 10
                {
                    v.state.clear_cover(x, z);
                    v.dirty = true;
                }
            }
            if job.next.1 > job.z1 {
                v.jobs.pop_front();
            }
        }
        for (request, _) in &report.rejected {
            v.pending.remove(&request.0);
        }
        let prior_time = v.state.time_ms;
        let growth = v.state.advance(
            &v.columns,
            &self.world.terrain().volume,
            16 + u64::from(self.tick.get() % 3 != 1),
        );
        v.dirty |= prior_time != v.state.time_ms;
        for (slot, g) in growth.into_iter().enumerate() {
            if v.pending.len() >= 4 {
                break;
            }
            if v.pending.values().any(|old| old.plant == g.plant) {
                continue;
            }
            let raw = self
                .tick
                .get()
                .checked_mul(8)
                .and_then(|n| {
                    n.checked_add(super::sim::SERVER_REQUEST_ID_BAND + (1 << 59) + slot as u64)
                })
                .ok_or(TickError::TickExhausted)?;
            let request = RequestId(raw);
            let center = BrushPoint::from_units(
                g.cell.x * BRUSH_UNIT + BRUSH_UNIT / 2,
                g.cell.y * BRUSH_UNIT + BRUSH_UNIT / 2,
                g.cell.z * BRUSH_UNIT + BRUSH_UNIT / 2,
            );
            let intent = EditIntent {
                request_id: request,
                actor: EntityId::new((1 << 40) + 2).unwrap(),
                target: EditTarget::Terrain,
                kind: EditKind::Place(g.material),
                brush: SphereBrush::new(center, 0).unwrap(),
                explosion: None,
            };
            if self.submit(intent).is_ok() {
                v.pending.insert(request.0, g);
            }
        }
        self.vegetation = Some(v);
        Ok(())
    }
}
