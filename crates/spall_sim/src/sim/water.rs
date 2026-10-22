//! Column-water and dam-gate methods.

use super::*;

impl Simulation {
    /// Read-only stacked-column water, when this world installed one.
    pub fn column_water(&self) -> Option<&crate::column_water::AuthoritativeColumnWater> {
        self.column_water.as_ref()
    }

    /// Installs recovered column water (checkpoint recovery). Replaces any
    /// water this simulation started with.
    pub fn restore_column_water(
        &mut self,
        state: &spall_protocol::column_water::ColumnWaterState,
        config: spall_fluid::columns::ColumnConfig,
    ) -> Result<(), crate::column_water::ColumnWaterError> {
        let mut water = crate::column_water::AuthoritativeColumnWater::restore(
            &self.world.terrain().volume,
            state,
            config,
        )?;
        if self.column_water_worker {
            water.start_worker();
        }
        self.column_water = Some(water);
        Ok(())
    }

    /// Runs the column water's fluid step on its own thread (see
    /// [`crate::column_water::AuthoritativeColumnWater::start_worker`]) for
    /// this water and any installed later by a restore or world reset.
    pub fn set_column_water_worker(&mut self, on: bool) {
        self.column_water_worker = on;
        if on && let Some(water) = &mut self.column_water {
            water.start_worker();
        }
    }

    /// The column-water delta owed to this tick's journal batch, if any.
    pub fn take_column_water_journal(
        &mut self,
    ) -> Option<spall_protocol::column_water::ColumnWaterDelta> {
        self.column_water.as_mut()?.take_journal_delta()
    }

    /// Sets this world's scene-authored gated spring(s) to `rate` (an admin
    /// control, e.g. "fill the reservoir"): `0` is off, `1..=3` selects an
    /// increasingly large authored footprint, i.e. a faster fill (clamped
    /// into range). A no-op — not an error — when this world has no water or
    /// the scene authored no gated spring.
    pub fn set_water_spring_rate(&self, rate: u8) {
        if let Some(water) = &self.column_water {
            water.set_gated_sources_rate(rate);
        }
    }

    /// The gated spring's current rate (`0` is off), `None` when this world
    /// has no water.
    pub fn water_spring_rate(&self) -> Option<u8> {
        self.column_water.as_ref().map(|w| w.gated_sources_rate())
    }

    /// Opens (cuts to air) or closes (refills with `closed_material`) a
    /// scene-authored dam gate: `cells` covered by a set of modest spheres
    /// (never one sphere sized to the whole shape's own bounding box — for a
    /// flat, elongated authored shape such as a notch, that would overshoot
    /// to the shape's diagonal and cut a crater far bigger than authored),
    /// each submitted as a server-authored edit exactly like a T21
    /// contact-damage cut — it goes through the normal commit pipeline
    /// (staged, then committed a few ticks later, exactly like a player's
    /// own dig), so it rebuilds the collider, replicates to clients, and
    /// dirties the water boundary on its own. Returns one admitted intent's
    /// status per covering sphere (empty when `cells` is empty — the scene
    /// authored no gate) — the caller decides whether admission alone is
    /// enough to report success, or whether to also inspect the eventual
    /// commit outcome of each.
    pub fn set_dam_gate(
        &mut self,
        cells: &[GlobalCell],
        open: bool,
        closed_material: MaterialId,
    ) -> Result<Vec<ActionStatus>, IntentError> {
        let actor = EntityId::new(DAM_GATE_ACTOR_ID).expect("non-zero reserved actor id");
        let kind = if open {
            EditKind::Cut
        } else {
            EditKind::Place(closed_material)
        };
        let mut statuses = Vec::new();
        for brush in covering_spheres(cells, DAM_GATE_MAX_SPHERE_RADIUS_CELLS) {
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let intent = EditIntent {
                request_id,
                actor,
                target: EditTarget::Terrain,
                kind,
                brush,
                explosion: None,
            };
            statuses.push(self.submit(intent)?);
        }
        Ok(statuses)
    }
}
