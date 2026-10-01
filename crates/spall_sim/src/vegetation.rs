//! Owner-thread integration. Growth uses the existing edit/structural pipeline;
//! no solver, invisible support, direct terrain write or extra ECS entity.
use crate::{EditIntent, EditKind, EditTarget, Simulation, TickError, TickReport};
use spall_core::{BRUSH_UNIT, BrushPoint, EntityId, SphereBrush};
use spall_ecology::living::{Growth, LivingState, VisualFrame};
use spall_protocol::RequestId;
use spall_worldgen::{ColumnMap, Preset, WorldGenSpec, WorldgenPalette};
use std::collections::BTreeMap;

pub(crate) struct AuthoritativeVegetation {
    pub state: LivingState,
    columns: ColumnMap,
    pending: BTreeMap<u64, Growth>,
    dirty: bool,
}
impl Simulation {
    pub fn install_vegetation(&mut self, state: LivingState) -> Result<(), String> {
        state.validate()?;
        let spec = WorldGenSpec::new(
            Preset::Showcase,
            state.seed,
            state.size,
            WorldgenPalette::sequential(1),
        );
        let columns = ColumnMap::compute(&spec).map_err(|e| e.to_string())?;
        self.vegetation = Some(AuthoritativeVegetation {
            state,
            columns,
            pending: BTreeMap::new(),
            dirty: true,
        });
        Ok(())
    }
    pub fn vegetation_state(&self) -> Option<&LivingState> {
        self.vegetation.as_ref().map(|v| &v.state)
    }
    pub fn vegetation_visual(&self) -> Option<VisualFrame> {
        self.vegetation
            .as_ref()
            .map(|v| v.state.visual(&self.world.terrain().volume))
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
        for (request, _) in &report.committed {
            if let Some(g) = v.pending.remove(&request.0) {
                v.state.acknowledge(g, &self.world.terrain().volume);
                v.dirty = true;
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
