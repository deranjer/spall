use super::*;

impl PhysicsWorld {
    /// Builds a Rapier rigid body + attached collider from `spec` and inserts
    /// both, returning their handles and the grid-origin collider offset. Shared
    /// by [`Self::add_body`] and [`Self::reactivate_body`].
    fn insert_rapier_body(
        &mut self,
        spec: &BodySpec,
    ) -> (RigidBodyHandle, ColliderHandle, [f32; 3]) {
        let prepared = PreparedCollider::new(&spec.grid, spec.cell_m, spec.representation);
        self.insert_prepared_rapier_body(
            spec.kind,
            spec.mass_properties,
            spec.density_kg_m3,
            spec.translation_m,
            spec.linvel_m_s,
            &prepared,
        )
    }

    fn insert_prepared_rapier_body(
        &mut self,
        kind: BodyKind,
        mass_properties: Option<BodyMassProperties>,
        density_kg_m3: f32,
        translation_m: [f32; 3],
        linvel_m_s: [f32; 3],
        prepared: &PreparedCollider,
    ) -> (RigidBodyHandle, ColliderHandle, [f32; 3]) {
        let rb = match kind {
            BodyKind::Fixed => RigidBodyBuilder::fixed(),
            BodyKind::Dynamic { ccd } => RigidBodyBuilder::dynamic()
                .linvel(Vector::new(linvel_m_s[0], linvel_m_s[1], linvel_m_s[2]))
                .ccd_enabled(ccd),
        }
        .translation(Vector::new(
            translation_m[0],
            translation_m[1],
            translation_m[2],
        ))
        .build();
        let body = self.bodies.insert(rb);

        let built = &prepared.build;
        let offset = prepared.offset_m;
        // With explicit mass properties the collision shape must not contribute
        // mass: the body carries the fine-grid mass / COM / inertia directly.
        let collider_density = if mass_properties.is_some() {
            0.0
        } else {
            density_kg_m3
        };
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        let collider = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);

        if let Some(props) = mass_properties {
            let rb = &mut self.bodies[body];
            rb.set_additional_mass_properties(rapier_mass_properties(props, offset), false);
            rb.recompute_mass_properties_from_colliders(&self.colliders);
        }
        self.pending_modified.push(collider);
        (body, collider, offset)
    }

    /// Adds a body whose collider was built ahead of time with [`PreparedCollider::new`] from
    /// the same grid, cell size and representation. Identical to [`Self::add_body`] with that
    /// grid, but the expensive shape construction has already happened, possibly on another
    /// thread. Ids and handles are assigned in call order, so adding prepared bodies in a fixed
    /// order gives the same world however they were built.
    pub fn add_prepared_body(
        &mut self,
        spec: PreparedBodySpec,
        prepared: PreparedCollider,
    ) -> BodyId {
        let (body, collider, offset) = self.insert_prepared_rapier_body(
            spec.kind,
            spec.mass_properties,
            spec.density_kg_m3,
            spec.translation_m,
            spec.linvel_m_s,
            &prepared,
        );
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            collider_offset_m: offset,
            mass_properties: spec.mass_properties,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        self.tag_collider(collider, id);
        id
    }

    /// Adds a body and returns its stable id.
    pub fn add_body(&mut self, spec: BodySpec) -> BodyId {
        let (body, collider, offset) = self.insert_rapier_body(&spec);

        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            collider_offset_m: offset,
            mass_properties: spec.mass_properties,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        self.tag_collider(collider, id);
        id
    }

    /// Swaps a body's collider for one rebuilt from `grid` in `rep` (an edit).
    /// Returns the **complete** rebuild cost: removing the old collider, the full
    /// occupancy → shape construction (native index extraction or greedy
    /// decomposition plus all shape allocation), the Rapier wrapping, and
    /// re-inserting the new collider under the same body.
    pub fn rebuild_collider(
        &mut self,
        id: BodyId,
        grid: &OccupancyGrid,
        rep: Representation,
    ) -> Duration {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        debug_assert!(!entry.retired, "rebuild_collider on a retired body");
        if entry.retired {
            return Duration::ZERO;
        }
        let (cell_m, density, body, old_collider, mass_properties, query_only, restitution) = (
            entry.cell_m,
            entry.density,
            entry.body,
            entry.collider,
            entry.mass_properties,
            entry.query_only,
            entry.restitution,
        );

        let start = Instant::now();
        self.colliders
            .remove(old_collider, &mut self.islands, &mut self.bodies, true);
        let built = build_collider(grid, cell_m, rep);
        let offset = grid_origin_offset_m(grid, cell_m);
        // The installed mass properties live on the rigid body and survive the
        // shape swap; the replacement collider stays massless so it cannot
        // perturb them. A geometry edit that changes the fine grid must follow
        // this with `set_mass_properties`.
        let collider_density = if mass_properties.is_some() {
            0.0
        } else {
            density
        };
        let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        if query_only {
            collider.set_solver_groups(query_only_solver_groups());
        }
        collider.set_restitution(restitution);
        collider.set_restitution_combine_rule(CoefficientCombineRule::Max);
        let handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        if mass_properties.is_some() {
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        }
        let total = start.elapsed();

        self.tag_collider(handle, id);
        self.entries[entry_index(id, self.id_namespace)].collider = handle;
        self.entries[entry_index(id, self.id_namespace)].representation = rep;
        self.entries[entry_index(id, self.id_namespace)].collider_offset_m = offset;
        self.pending_removed.push(old_collider);
        self.pending_modified.push(handle);
        total
    }

    /// Installs `props` — mass / COM / full inertia derived from a body's fine
    /// material grid — into the body, replacing whatever it carried. The
    /// collision shape is untouched and contributes no mass. Call this after a
    /// geometry edit has rebuilt the collider so the solver's mass properties
    /// track the new fine grid. Wakes the body.
    ///
    /// The body must have been added with [`BodySpec::mass_properties`] set (so
    /// its collider is already massless); otherwise the shape's own mass would
    /// be added on top of `props`.
    pub fn set_mass_properties(&mut self, id: BodyId, props: BodyMassProperties) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        debug_assert!(!entry.retired, "set_mass_properties on a retired body");
        if entry.retired {
            return;
        }
        debug_assert!(
            entry.mass_properties.is_some() || entry.density == 0.0,
            "set_mass_properties requires a body added with BodySpec::mass_properties = Some(_)"
        );
        entry.mass_properties = Some(props);
        let body = entry.body;
        let offset = entry.collider_offset_m;
        let rb = &mut self.bodies[body];
        rb.set_additional_mass_properties(rapier_mass_properties(props, offset), true);
        rb.recompute_mass_properties_from_colliders(&self.colliders);
    }

    /// Sets contact restitution and retains it across geometry rebuilds and
    /// dormancy. `Max` combination lets a deliberately bouncy body rebound
    /// from ordinary zero-restitution terrain.
    pub fn set_restitution(&mut self, id: BodyId, restitution: f32) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        entry.restitution = restitution.clamp(0.0, 1.0);
        if !entry.dormant {
            let collider = &mut self.colliders[entry.collider];
            collider.set_restitution(entry.restitution);
            collider.set_restitution_combine_rule(CoefficientCombineRule::Max);
        }
    }

    /// Retires a body whose authoritative volume became empty (`ENG-56`): its
    /// Rapier rigid body and attached collider are removed from the simulation
    /// so it can no longer collide with, rest on, or be swept against by any
    /// other body, nor be stepped or queried. The [`BodyId`] slot is kept (ids
    /// are indices — removing one would shift every later id) and marked
    /// retired; callers must drop the handle. Idempotent.
    pub fn retire_body(&mut self, id: BodyId) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        entry.retired = true;
        let body = entry.body;
        self.bodies.remove(
            body,
            &mut self.islands,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            true,
        );
        // Same hazard as `rebuild_collider`: the CCD solver caches fixed-target
        // collider handles and only refreshes that cache on a step where a body
        // is CCD-active, so a collider removed on a quiet step can be
        // dereferenced later ("No element at index"). Clearing the solver (it
        // holds nothing else) forces a rescan.
        self.ccd_solver = CCDSolver::new();
    }

    /// Whether `id` has been retired by [`Self::retire_body`].
    pub fn is_retired(&self, id: BodyId) -> bool {
        self.entries[entry_index(id, self.id_namespace)].retired
    }

    /// Deactivates a **dormant** body (T21): removes its Rapier rigid body and
    /// collider so it costs nothing to step and cannot be contacted or swept,
    /// but keeps its [`BodyId`] slot and every rebuild parameter
    /// (`cell_m` / representation / density / mass properties) so
    /// [`Self::reactivate_body`] can restore it. The caller owns the frozen
    /// pose / velocity and passes them back on reactivation. Idempotent; a no-op
    /// on a retired body.
    pub fn deactivate_body(&mut self, id: BodyId) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired || entry.dormant {
            return;
        }
        entry.dormant = true;
        let body = entry.body;
        self.bodies.remove(
            body,
            &mut self.islands,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            true,
        );
        // See `retire_body`: drop the CCD fixed-target cache so it cannot keep a
        // dangling handle to the collider just removed.
        self.ccd_solver = CCDSolver::new();
    }

    /// Restores a body deactivated by [`Self::deactivate_body`] into its
    /// original slot: rebuilds the Rapier rigid body + collider from `grid` and
    /// the stored rebuild parameters, at `translation_m` / `rotation` (xyzw)
    /// with `linvel_m_s` / `angvel_rad_s`. The body starts awake; the solver
    /// re-sleeps it on the next quiet step. A no-op on a retired body or one
    /// that is not dormant.
    #[allow(clippy::too_many_arguments)]
    pub fn reactivate_body(
        &mut self,
        id: BodyId,
        grid: &OccupancyGrid,
        translation_m: [f32; 3],
        rotation: [f32; 4],
        linvel_m_s: [f32; 3],
        angvel_rad_s: [f32; 3],
    ) {
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        if entry.retired || !entry.dormant {
            return;
        }
        let restitution = entry.restitution;
        let spec = BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: entry.representation,
            grid: grid.clone(),
            cell_m: entry.cell_m,
            density_kg_m3: entry.density,
            mass_properties: entry.mass_properties,
            translation_m,
            linvel_m_s,
        };
        let (body, collider, offset) = self.insert_rapier_body(&spec);
        self.tag_collider(collider, id);
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        entry.body = body;
        entry.collider = collider;
        entry.collider_offset_m = offset;
        entry.dormant = false;
        self.set_body_pose(id, translation_m, rotation);
        self.set_body_velocity(id, linvel_m_s, angvel_rad_s);
        self.set_restitution(id, restitution);
    }

    /// Whether `id` is currently dormant (deactivated by [`Self::deactivate_body`]).
    pub fn is_dormant(&self, id: BodyId) -> bool {
        self.entries[entry_index(id, self.id_namespace)].dormant
    }

    /// Bodies that are neither retired nor dormant — the set the solver actually
    /// steps.
    pub fn active_body_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.retired && !e.dormant)
            .count()
    }

    /// Removes just a body's attached collider, keeping the rigid body itself.
    /// Used when a *terrain* ownership's volume becomes empty (`ENG-56`): the
    /// fixed body stays so a later refill can rebuild a collider on it, but
    /// nothing collides with the obsolete solid shape in the meantime. A no-op
    /// on a retired body.
    pub fn remove_collider(&mut self, id: BodyId) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        let collider = entry.collider;
        self.colliders
            .remove(collider, &mut self.islands, &mut self.bodies, true);
        // See `retire_body`: drop the CCD fixed-target cache so it cannot keep a
        // dangling handle to the collider just removed.
        self.ccd_solver = CCDSolver::new();
        self.pending_removed.push(collider);
    }

    /// Whether `id` currently has an attached collider in Rapier.
    pub fn has_collider(&self, id: BodyId) -> bool {
        self.entries
            .get(entry_index(id, self.id_namespace))
            .is_some_and(|entry| !entry.retired && self.colliders.contains(entry.collider))
    }

    /// Excludes `id`'s collider from ever producing a rigid-body **solver**
    /// response — no dynamic body colliding with it is pushed, slowed, or
    /// stopped by it, ever, regardless of either side's own collision groups
    /// (`solver_groups` zeroed on both membership and filter, so Rapier's
    /// interaction test — `And` or `Or` — always fails). It stays fully
    /// visible to ordinary **queries** (`sweep_character`/`cast_shape`/...),
    /// which only ever consult `collision_groups`, a separate field this
    /// leaves untouched.
    ///
    /// For [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache)'s
    /// windows: a window collider must block/redirect the *character's own*
    /// query-based sweep exactly like real terrain would, while never once
    /// acting as a real physical obstacle for any dynamic body (debris, other
    /// players' own dynamics) that happens to pass through the same space —
    /// it is a query-time convenience, not a real object in the world.
    ///
    /// The setting is sticky: every later [`Self::rebuild_collider`] call on
    /// `id` reapplies it to the fresh collider automatically. A no-op on a
    /// retired body.
    pub fn set_query_only(&mut self, id: BodyId) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        entry.query_only = true;
        if let Some(collider) = self.colliders.get_mut(entry.collider) {
            collider.set_solver_groups(query_only_solver_groups());
        }
    }

    /// Refreshes the broad-phase spatial index for every collider added or
    /// rebuilt since the last call to this or [`Self::step`], for
    /// **queries only** (`sweep_character`/`cast_shape`/...) — without
    /// stepping the dynamics pipeline: no gravity or velocity integration, no
    /// contact resolution, for *any* body in this world. [`Self::step`]
    /// already refreshes the broad-phase as part of its own full pipeline
    /// step, so a caller that steps this world's simulation every tick
    /// regardless (the normal case) never needs this. It exists for a caller
    /// that adds/rebuilds colliders on a world it does **not** own the tick
    /// loop for — [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache),
    /// which must never advance a simulation another caller is driving.
    ///
    /// Uses [`BroadPhaseBvh::set_aabb`] per modified collider, **not**
    /// [`BroadPhaseBvh::update`]: `update` is the same call [`Self::step`]
    /// uses to detect newly-overlapping pairs and emit `AddPair` — and
    /// Rapier permanently remembers, per pair, that it already announced one
    /// ("no need to re-send an `AddPair` event... if no `RemovePair`
    /// happened since", `BroadPhaseBvh::update`'s own doc). Calling `update`
    /// from here — as this used to — consumes that one-time announcement
    /// for any dynamic body's pair that happens to be pending, with no
    /// narrow-phase registration ever created for it: `step`'s *own* later
    /// `update` call sees the pair as already-announced and never re-emits
    /// it, so that pair's contacts are never generated again for the rest of
    /// the simulation, however many times `step` runs afterwards. Confirmed
    /// with an isolated repro (`.local/analysis/pr109-probe`): a dynamic
    /// cube resting on a fixed one falls straight through it if a single
    /// `sync_queries` call happens before the first `step`. `set_aabb`
    /// refits the tree immediately (so queries see the new shape/position
    /// right away) but explicitly defers pair evaluation "to the next
    /// broad-phase update" (its own doc) — i.e. to `step`'s real one, which
    /// still runs the full detect-and-announce logic exactly once, correctly.
    /// A collider in `pending_removed` needs no action here: `remove_collider`/
    /// `retire_body` already removed it from `self.colliders` before pushing
    /// it there, so a query pipeline built from `self.colliders` naturally
    /// filters it out via the lookup miss regardless of the tree's own leaf
    /// cleanup timing, and `step`'s removal handling runs on Rapier's native
    /// dirty flags next time regardless of what this does.
    pub fn sync_queries(&mut self) {
        if self.pending_modified.is_empty() && self.pending_removed.is_empty() {
            return;
        }
        // `set_body_pose` updates the rigid body immediately, but attached
        // collider world poses are normally propagated by `step`. Query-only
        // mirrors do not step, so do that narrow propagation here before
        // computing their new AABBs.
        self.bodies
            .propagate_modified_body_positions_to_colliders(&mut self.colliders);
        for &handle in &self.pending_modified {
            if let Some(collider) = self.colliders.get(handle) {
                let aabb = collider.compute_aabb();
                self.broad_phase.set_aabb(&self.params, handle, aabb);
            }
        }
        self.pending_modified.clear();
        self.pending_removed.clear();
    }

    /// Current representation of a body.
    pub fn representation(&self, id: BodyId) -> Representation {
        self.entries[entry_index(id, self.id_namespace)].representation
    }

    /// Captures the creating world's identity, step/slot counts and selected
    /// body/collider state for a same-step commit fence. A fixed body slot may
    /// have no collider after an empty terrain-brick publication; that absence
    /// is captured and a collider appearing or disappearing makes the fence
    /// stale.
    pub fn snapshot_body_commit_fence(
        &self,
        ids: &[BodyId],
    ) -> Result<BodyCommitFence, BodyFenceError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut bodies = Vec::with_capacity(ids.len());
        for id in ids {
            if !seen.insert(id.0) || (id.0 >> 32) as u32 != self.id_namespace {
                return Err(BodyFenceError);
            }
            let index = (id.0 as u32) as usize;
            let entry = self.entries.get(index).ok_or(BodyFenceError)?;
            let body = self.bodies.get(entry.body).ok_or(BodyFenceError)?;
            let collider =
                self.colliders
                    .get(entry.collider)
                    .map(|collider| ColliderCommitFenceState {
                        handle: entry.collider,
                        shape: collider.shared_shape().clone(),
                        position: *collider.position(),
                        enabled: collider.is_enabled(),
                        sensor: collider.is_sensor(),
                        active_collision_types: collider.active_collision_types(),
                        collision_groups: collider.collision_groups(),
                        solver_groups: collider.solver_groups(),
                        friction: collider.friction(),
                        restitution: collider.restitution(),
                    });
            bodies.push(BodyCommitFenceEntry {
                id: *id,
                body: entry.body,
                body_type: body.body_type(),
                position: *body.position(),
                linvel: body.linvel(),
                angvel: body.angvel(),
                mass: body.mass(),
                center_of_mass: body.center_of_mass(),
                retired: entry.retired,
                dormant: entry.dormant,
                collider,
                representation: entry.representation,
                query_only: entry.query_only,
                density: entry.density,
                mass_properties: entry.mass_properties,
            });
        }
        Ok(BodyCommitFence {
            world_identity: self.preparation_identity.clone(),
            step_count: self.step_count,
            entry_count: self.entries.len(),
            body_count: self.bodies.len(),
            bodies,
        })
    }

    /// Revalidates a selected-body fence. Same-step pose/twist, collider
    /// rebuild/shape/config, or entry changes invalidate it.
    pub fn validate_body_commit_fence(&self, fence: &BodyCommitFence) -> bool {
        if !std::sync::Arc::ptr_eq(&fence.world_identity, &self.preparation_identity)
            || fence.step_count != self.step_count
            || fence.entry_count != self.entries.len()
            || fence.body_count != self.bodies.len()
        {
            return false;
        }
        fence.bodies.iter().all(|saved| {
            if (saved.id.0 >> 32) as u32 != self.id_namespace {
                return false;
            }
            let index = (saved.id.0 as u32) as usize;
            let Some(entry) = self.entries.get(index) else {
                return false;
            };
            if entry.body != saved.body
                || entry.retired != saved.retired
                || entry.dormant != saved.dormant
                || entry.representation != saved.representation
                || entry.query_only != saved.query_only
                || entry.density.to_bits() != saved.density.to_bits()
                || entry.mass_properties != saved.mass_properties
            {
                return false;
            }
            let Some(body) = self.bodies.get(saved.body) else {
                return false;
            };
            let collider_matches = match (&saved.collider, self.colliders.get(entry.collider)) {
                (None, None) => true,
                (Some(saved), Some(collider)) => {
                    saved.handle == entry.collider
                        && std::sync::Arc::ptr_eq(&saved.shape.0, &collider.shared_shape().0)
                        && *collider.position() == saved.position
                        && collider.is_enabled() == saved.enabled
                        && collider.is_sensor() == saved.sensor
                        && collider.active_collision_types() == saved.active_collision_types
                        && collider.collision_groups() == saved.collision_groups
                        && collider.solver_groups() == saved.solver_groups
                        && collider.friction().to_bits() == saved.friction.to_bits()
                        && collider.restitution().to_bits() == saved.restitution.to_bits()
                }
                _ => false,
            };
            collider_matches
                && body.body_type() == saved.body_type
                && *body.position() == saved.position
                && body.linvel() == saved.linvel
                && body.angvel() == saved.angvel
                && body.mass().to_bits() == saved.mass.to_bits()
                && body.center_of_mass() == saved.center_of_mass
        })
    }

    /// Kinematic snapshot of a body. A retired body (its volume became empty)
    /// or a dormant one (T21, deactivated pending reactivation) has no Rapier
    /// body left; it reports an all-zero, non-sleeping state and the caller is
    /// expected to hold the authoritative pose itself.
    pub fn body_state(&self, id: BodyId) -> BodyState {
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        debug_assert!(!entry.retired, "body_state on a retired body");
        debug_assert!(!entry.dormant, "body_state on a dormant body");
        if entry.retired || entry.dormant {
            return BodyState {
                translation_m: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                linvel_m_s: [0.0; 3],
                angvel_rad_s: [0.0; 3],
                sleeping: false,
                mass_kg: 0.0,
            };
        }
        let rb = &self.bodies[entry.body];
        let t = rb.translation();
        let q = rb.rotation();
        let lv = rb.linvel();
        let av = rb.angvel();
        BodyState {
            translation_m: [t.x, t.y, t.z],
            rotation: [q.x, q.y, q.z, q.w],
            linvel_m_s: [lv.x, lv.y, lv.z],
            angvel_rad_s: [av.x, av.y, av.z],
            sleeping: rb.is_sleeping(),
            mass_kg: entry
                .mass_properties
                .map(|p| p.mass_kg)
                .unwrap_or_else(|| rb.mass()),
        }
    }

    /// Sets a body's world pose. `rotation` is a quaternion `[x, y, z, w]`
    /// (renormalised by Rapier). Used by the authoritative sim (T08) to spawn a
    /// split child at its parent's transform so world geometry is unchanged at
    /// the split instant.
    pub fn set_body_pose(&mut self, id: BodyId, translation_m: [f32; 3], rotation: [f32; 4]) {
        let (body, primary) = {
            let entry = &self.entries[entry_index(id, self.id_namespace)];
            (entry.body, entry.collider)
        };
        let rb = &mut self.bodies[body];
        rb.set_translation(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            true,
        );
        rb.set_rotation(
            Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3]),
            true,
        );
        // A query-only mirror (for example, client-predicted collision against
        // replicated debris) may move without ever stepping this PhysicsWorld.
        // Mark its collider so `sync_queries` refits the broad-phase AABB.
        self.pending_modified.push(primary);
    }

    /// Applies a linear impulse (N·s) to a body at its centre of mass and wakes
    /// it if it was asleep. Models a documented "nearby interaction" — a blast
    /// impulse or an impact from an adjacent edit — for the sleep/wake
    /// feasibility scenario. No-op for `Fixed` bodies.
    pub fn apply_impulse(&mut self, id: BodyId, impulse_n_s: [f32; 3]) {
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.apply_impulse(
            Vector::new(impulse_n_s[0], impulse_n_s[1], impulse_n_s[2]),
            true,
        );
    }

    /// Applies an angular impulse (N·m·s) about a body's centre of mass and wakes
    /// it. The resulting change in angular velocity is `I⁻¹ · impulse` for the
    /// body's installed inertia tensor — the direct motion check for
    /// mixed-material inertia. No-op for `Fixed` bodies.
    pub fn apply_torque_impulse(&mut self, id: BodyId, torque_impulse_n_m_s: [f32; 3]) {
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.apply_torque_impulse(
            Vector::new(
                torque_impulse_n_m_s[0],
                torque_impulse_n_m_s[1],
                torque_impulse_n_m_s[2],
            ),
            true,
        );
    }

    /// Applies a linear impulse (N·s) at a world-space point, producing both a
    /// linear response (`impulse / mass`) and an angular response about the
    /// body's centre of mass (`I⁻¹ · ((point − com) × impulse)`) — so it
    /// exercises the installed COM and inertia together. Wakes the body. No-op
    /// for `Fixed` bodies.
    pub fn apply_impulse_at_point(
        &mut self,
        id: BodyId,
        impulse_n_s: [f32; 3],
        point_world_m: [f32; 3],
    ) {
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.apply_impulse_at_point(
            Vector::new(impulse_n_s[0], impulse_n_s[1], impulse_n_s[2]),
            Vector::new(point_world_m[0], point_world_m[1], point_world_m[2]),
            true,
        );
    }

    /// Sets a body's linear and angular velocity, m/s and rad/s. Used to hand a
    /// split child its inherited velocity.
    pub fn set_body_velocity(&mut self, id: BodyId, linvel_m_s: [f32; 3], angvel_rad_s: [f32; 3]) {
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.set_linvel(
            Vector::new(linvel_m_s[0], linvel_m_s[1], linvel_m_s[2]),
            true,
        );
        rb.set_angvel(
            Vector::new(angvel_rad_s[0], angvel_rad_s[1], angvel_rad_s[2]),
            true,
        );
    }

    /// The body-local collider offset applied for this body's occupancy-grid
    /// origin (`grid.origin() * cell_m`). Add it to
    /// [`Self::derived_mass_properties`]'s grid-local centre of mass to get the
    /// body-local centre of mass (`ENG-55`).
    pub fn collider_offset_m(&self, id: BodyId) -> [f32; 3] {
        self.entries[entry_index(id, self.id_namespace)].collider_offset_m
    }

    /// A body's authoritative mass properties: `(mass_kg, local centre of mass in
    /// metres, principal inertia diagonal)`. When fine-grid
    /// [`BodySpec::mass_properties`] were installed these are exactly those
    /// values (independent of the collision shape); otherwise they are what
    /// Rapier derived from the collider at its uniform density. Either way the
    /// centre of mass is in the collider shape's grid-local frame (cell
    /// `(0, 0, 0)` corner at the origin); [`Self::collider_offset_m`] shifts it
    /// to the body frame.
    pub fn derived_mass_properties(&self, id: BodyId) -> (f32, [f32; 3], [f32; 3]) {
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        if let Some(p) = entry.mass_properties {
            return (
                p.mass_kg,
                p.local_com_m,
                principal_inertia(p.inertia_com_kg_m2),
            );
        }
        let shape = self.colliders[entry.collider].shared_shape().clone();
        let mp = shape.mass_properties(entry.density);
        let com = mp.local_com;
        let inertia = mp.principal_inertia();
        (
            mp.mass(),
            [com.x, com.y, com.z],
            [inertia.x, inertia.y, inertia.z],
        )
    }

    /// Mass properties read straight off the live Rapier rigid body: `(mass_kg,
    /// local centre of mass in metres, principal inertia diagonal)`. This is the
    /// value the solver integrates — after the collider contribution and any
    /// installed override are folded together — so tests can prove the fine-grid
    /// properties reached the solver, not just this adapter's record of them.
    pub fn live_body_mass_properties(&self, id: BodyId) -> (f32, [f32; 3], [f32; 3]) {
        let rb = &self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        let mp = &rb.mass_properties().local_mprops;
        let com = mp.local_com;
        let pi = mp.principal_inertia();
        (mp.mass(), [com.x, com.y, com.z], [pi.x, pi.y, pi.z])
    }
}
