use super::*;

impl PhysicsWorld {
    /// Sweeps a capsule character through the current collider world one tick
    /// (T19). `position_m` and the returned translation are the capsule's
    /// **feet**; collision resolution (wall slide, autostep, slope limit, ground
    /// snap) is Rapier's `KinematicCharacterController` with the frozen
    /// [`crate::character::tuning`] constants.
    ///
    /// The query runs against the broad-phase BVH as last refreshed by
    /// [`Self::step`], so callers advance characters *after* stepping physics on
    /// a tick that rebuilt any collider.
    pub fn sweep_character(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
    ) -> crate::character::CharacterMove {
        self.sweep_character_with(params, position_m, desired_translation_m, dt_s, |_| {})
    }

    /// Same as [`Self::sweep_character`], but `on_collision` is called for
    /// every [`CharacterCollision`] Rapier's controller reports along the way
    /// — normally discarded (`sweep_character` passes an empty closure).
    /// ENG-69 round 15: added to directly confirm (not just infer from
    /// endpoint position diffs) that a representation-divergence event is a
    /// real contact against real geometry, not a coincidental integration
    /// difference — see `spall_physics::character::tests::
    /// strafing_the_g1_tower_wall_diverges_between_representations`'s
    /// per-tick trace, which found the divergence lands entirely within one
    /// tick.
    pub fn sweep_character_with(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        on_collision: impl FnMut(&CharacterCollision),
    ) -> crate::character::CharacterMove {
        self.sweep_character_impl(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &[],
            on_collision,
        )
    }

    /// Same as [`Self::sweep_character`], but every collider in `exclude`
    /// (by [`BodyId`] — a retired id is silently skipped) is invisible to
    /// this one sweep's query, as if it were not in the world at all. ENG-69
    /// round 18: a character's own [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache)
    /// window must be the *only* terrain-like collider it sees for its own
    /// movement — not the real whole-terrain collider (the window replaces
    /// it, exactly), and not another character's own window (each
    /// character's window is sized and centred for *that* character alone;
    /// nothing about it is meaningful to anyone else's sweep). Both need
    /// excluding explicitly, by identity, not by a collision-group category —
    /// see [`Self::set_query_only`]'s doc for why a coarse per-category
    /// exclusion isn't the right tool here (it would also have to reject the
    /// caller's *own* window, which a predicate lets back in individually).
    pub fn sweep_character_excluding(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        exclude: &[BodyId],
    ) -> crate::character::CharacterMove {
        let mut handles = Vec::new();
        for id in exclude {
            let entry = &self.entries[entry_index(*id, self.id_namespace)];
            if entry.retired || entry.dormant {
                continue;
            }
            handles.push(entry.collider);
        }
        self.sweep_character_impl(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &handles,
            |_| {},
        )
    }

    /// Authoritative character sweep that also transfers momentum into any
    /// dynamic rigid bodies the capsule contacts. The capsule itself remains
    /// kinematic, but Rapier's character impulse approximation treats it as a
    /// body of `params.mass_kg`, so a many-voxel body accelerates less than a
    /// small one under the same player movement.
    ///
    /// This is intentionally separate from [`Self::sweep_character_excluding`]:
    /// prediction clients use query-only replica colliders and must never
    /// become an authority for rigid-body impulses.
    pub fn sweep_character_pushing_excluding(
        &mut self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        exclude: &[BodyId],
    ) -> crate::character::CharacterMove {
        let base = self.exclusion_for(exclude.iter().copied());
        self.sweep_character_pushing_excluding_set(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &base,
            &[],
        )
    }

    /// The bodies `ids` as a reusable [`ColliderExclusion`].
    pub fn exclusion_for(&self, ids: impl IntoIterator<Item = BodyId>) -> ColliderExclusion {
        ColliderExclusion {
            bodies: ids.into_iter().collect(),
        }
    }

    /// Records which body owns `collider`, so a query can tell whose it is without the handle.
    pub(super) fn tag_collider(&mut self, collider: ColliderHandle, id: BodyId) {
        if let Some(c) = self.colliders.get_mut(collider) {
            c.user_data = u128::from(id.0) + 1;
        }
    }

    /// [`Self::sweep_character_pushing_excluding`] with the bulk of the exclusion prebuilt.
    /// Every terrain brick has its own collider, so a full-size world excludes hundreds of
    /// thousands of them from each character query; as a list that was tens of milliseconds per
    /// tick, as a hash set built once it is a lookup. `extra` is the few per-call additions.
    #[allow(clippy::too_many_arguments)]
    pub fn sweep_character_pushing_excluding_set(
        &mut self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        base: &ColliderExclusion,
        extra: &[BodyId],
    ) -> crate::character::CharacterMove {
        let extra_bodies = self.exclusion_for(extra.iter().copied());
        let excluded = CombinedExclusion {
            base,
            extra: &extra_bodies,
        };
        let controller = crate::character::controller();
        let shape = crate::character::capsule(params);
        let centre = params.centre_offset_m();
        let feet = Vector::new(
            position_m[0] as f32,
            position_m[1] as f32,
            position_m[2] as f32,
        );
        let pos = Pose::from_translation(feet + Vector::new(0.0, centre, 0.0));
        let excluded_predicate =
            |_handle: ColliderHandle, collider: &Collider| !excluded.excludes(collider);
        let filter = if excluded.is_empty() {
            QueryFilter::default()
        } else {
            QueryFilter::default().predicate(&excluded_predicate)
        };
        let desired = Vector::new(
            desired_translation_m[0],
            desired_translation_m[1],
            desired_translation_m[2],
        );
        let mut collisions = Vec::new();
        let moved = {
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                filter,
            );
            controller.move_shape(dt_s, &queries, &shape, &pos, desired, |collision| {
                collisions.push(collision)
            })
        };
        // Rapier's impulse solver keeps one manifold buffer across every
        // dynamic collider the capsule overlaps, but parry's
        // `contact_manifolds` may clear that buffer, after which Rapier
        // slices it from a stale index and panics ("range start index N out
        // of range for slice of length 0") as soon as the capsule touches a
        // second dynamic collider (e.g. two plinko balls at once). Solve one
        // dynamic collider per call so the buffer never spans colliders.
        let mut touched: Vec<ColliderHandle> = Vec::new();
        {
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                QueryFilter::default(),
            );
            for collision in &collisions {
                let aabb = shape.compute_aabb(&collision.character_pos).loosened(0.5);
                for (handle, collider) in queries.intersect_aabb_conservative(aabb) {
                    let dynamic = collider
                        .parent()
                        .and_then(|parent| self.bodies.get(parent))
                        .is_some_and(|body| body.is_dynamic());
                    if dynamic && !excluded.excludes(collider) && !touched.contains(&handle) {
                        touched.push(handle);
                    }
                }
            }
        }
        // The impulse is sized from the character's mass, so a heavy
        // character launches a body faster than it is walking; the body then
        // separates, friction slows it, and the character catches up and
        // kicks it again -- visible as a body jumping in increments. Cap the
        // horizontal speed a push can add at the character's own.
        let push_velocity = Vector::new(desired.x / dt_s, 0.0, desired.z / dt_s);
        let push_speed = push_velocity.length();
        for handle in touched {
            let body_handle = self.colliders.get(handle).and_then(|c| c.parent());
            let along_before = body_handle
                .and_then(|b| self.bodies.get(b))
                .map(|rb| rb.linvel().dot(push_velocity) / push_speed.max(f32::MIN_POSITIVE));
            let only = move |candidate: ColliderHandle, _collider: &Collider| candidate == handle;
            let mut queries = self.broad_phase.as_query_pipeline_mut(
                self.narrow_phase.query_dispatcher(),
                &mut self.bodies,
                &mut self.colliders,
                QueryFilter::default().predicate(&only),
            );
            controller.solve_character_collision_impulses(
                dt_s,
                &mut queries,
                &shape,
                params.mass_kg.max(f32::MIN_POSITIVE),
                &collisions,
            );
            if push_speed > 1.0e-3
                && let (Some(b), Some(before)) = (body_handle, along_before)
                && let Some(rb) = self.bodies.get_mut(b)
            {
                let dir = push_velocity / push_speed;
                let velocity = rb.linvel();
                let along = velocity.dot(dir);
                let limit = push_speed.max(before);
                if along > limit {
                    rb.set_linvel(velocity - dir * (along - limit), true);
                }
            }
        }
        // The controller stops the character a skin's width short of a body,
        // so on some ticks it reports no collision at all: no impulse, the
        // body slows against friction, and the next tick kicks it again --
        // the push alternates on and off. Carry every dynamic body that is
        // near and ahead of the character at the character's own speed
        // whether or not this tick registered a contact.
        if push_speed > CARRY_MIN_SPEED_M_S {
            let end = Pose::from_translation(pos.translation + moved.translation);
            let dir = push_velocity / push_speed;
            let candidates: Vec<(ColliderHandle, RigidBodyHandle)> = {
                let queries = self.broad_phase.as_query_pipeline(
                    self.narrow_phase.query_dispatcher(),
                    &self.bodies,
                    &self.colliders,
                    QueryFilter::default(),
                );
                let aabb = shape.compute_aabb(&end).loosened(CARRY_MARGIN_M);
                queries
                    .intersect_aabb_conservative(aabb)
                    .filter(|(_, collider)| !excluded.excludes(collider))
                    .filter_map(|(handle, collider)| {
                        let body = collider.parent()?;
                        self.bodies
                            .get(body)
                            .is_some_and(|rb| rb.is_dynamic())
                            .then_some((handle, body))
                    })
                    .collect()
            };
            for (handle, body) in candidates {
                let collider = &self.colliders[handle];
                let ahead = (collider.position().translation - end.translation).dot(dir) > 0.0;
                let near = rapier3d::parry::query::distance(
                    &end,
                    &shape,
                    collider.position(),
                    collider.shape(),
                )
                .is_ok_and(|gap| gap <= CARRY_MARGIN_M);
                if !(ahead && near) {
                    continue;
                }
                let rb = &mut self.bodies[body];
                let velocity = rb.linvel();
                let along = velocity.dot(dir);
                if along < push_speed {
                    rb.set_linvel(velocity + dir * (push_speed - along), true);
                }
            }
        }
        crate::character::CharacterMove {
            translation_m: [
                moved.translation.x,
                moved.translation.y,
                moved.translation.z,
            ],
            grounded: moved.grounded,
        }
    }

    fn sweep_character_impl(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        exclude: &[ColliderHandle],
        mut on_collision: impl FnMut(&CharacterCollision),
    ) -> crate::character::CharacterMove {
        let controller = crate::character::controller();
        let shape = crate::character::capsule(params);
        let centre = params.centre_offset_m();
        let feet = Vector::new(
            position_m[0] as f32,
            position_m[1] as f32,
            position_m[2] as f32,
        );
        let pos = Pose::from_translation(feet + Vector::new(0.0, centre, 0.0));
        let excluded_predicate =
            |handle: ColliderHandle, _collider: &Collider| !exclude.contains(&handle);
        let filter = if exclude.is_empty() {
            QueryFilter::default()
        } else {
            QueryFilter::default().predicate(&excluded_predicate)
        };
        let queries = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            filter,
        );
        let desired = Vector::new(
            desired_translation_m[0],
            desired_translation_m[1],
            desired_translation_m[2],
        );
        let moved = controller.move_shape(dt_s, &queries, &shape, &pos, desired, |c| {
            on_collision(&c);
        });
        crate::character::CharacterMove {
            translation_m: [
                moved.translation.x,
                moved.translation.y,
                moved.translation.z,
            ],
            grounded: moved.grounded,
        }
    }
}
