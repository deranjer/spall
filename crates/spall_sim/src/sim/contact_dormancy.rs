//! Contact damage and body dormancy.

use super::*;

impl Simulation {
    /// Converts this tick's solver contacts into bounded terrain-damage cuts and
    /// admits them (T21, increment 1). Call **after** [`Self::tick`], passing the
    /// [`TickReport`] it returned so a body created this tick is excluded from
    /// the recursion guard.
    ///
    /// This is deliberately *not* run by [`Self::tick`]: the integrator opts in,
    /// owns the [`ContactDamagePolicy`] (its cooldown state), and decides the
    /// cadence. Admitted cuts stage off-tick and commit on a later tick, exactly
    /// like a client edit — so a contact can never mutate world state inside the
    /// physics step, and a damage cut cannot cascade into another cut the same
    /// tick.
    ///
    /// Increment 1 damaged **terrain only**. Increment 3 adds **body-on-body**
    /// fracture: a `(dynamic, dynamic)` contact carves a bounded cut into the
    /// body being struck — the slower of the pair, tie-broken to the lower mass
    /// (`ContactDamageConfig::still_speed_m_s`) — through the same threshold,
    /// per-region cooldown, and per-tick cap. The contact point is resolved into
    /// the struck body's local cell frame here, so the pure policy stays in cell
    /// coordinates and a moving body's cooldown spot does not drift.
    pub fn apply_contact_damage(
        &mut self,
        policy: &mut ContactDamagePolicy,
        report: &TickReport,
    ) -> ContactDamageReport {
        let mut out = ContactDamageReport::default();
        if self.world.body_count() == 0 {
            return out;
        }

        let terrain_volume = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        let g = {
            let a = self.world.physics().gravity_m_s2();
            (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
        };
        let dt = TICK_DT_S;
        let still_speed = policy.config().still_speed_m_s;

        let born_this_tick: HashSet<u64> = report
            .committed
            .iter()
            .flat_map(|(_, c)| c.children.iter().map(|e| e.get()))
            .collect();

        let mut events: Vec<ContactEvent> = Vec::new();
        for contact in self.world.physics().contact_impulses() {
            if !contact.point_m.iter().all(|v| v.is_finite()) {
                continue;
            }
            let world_point =
                DVec3::from_array(self.world.physics_origin().to_world_f64(contact.point_m));
            match (contact.dynamic[0], contact.dynamic[1]) {
                // Exactly one side dynamic, the other terrain: a body striking
                // the world grid (increment 1).
                (true, false) | (false, true) => {
                    let striker_idx = if contact.dynamic[0] { 0 } else { 1 };
                    if !self
                        .world
                        .is_terrain_physics_body(contact.bodies[1 - striker_idx])
                    {
                        continue;
                    }
                    let striker_phys = contact.bodies[striker_idx];
                    let Some(striker) =
                        self.world.body_by_phys(striker_phys).and_then(|b| b.entity)
                    else {
                        continue;
                    };
                    let mass = self.world.physics().body_state(striker_phys).mass_kg;
                    let point_cell = (world_point / cell_m).to_array();
                    // Contact normals point from body 0 to body 1. Sampling
                    // steps against target-to-striker, so orient from target
                    // (terrain) toward the selected striker.
                    let normal =
                        target_to_striker_normal(contact.normal.map(f64::from), 1 - striker_idx);
                    let Some(target_material) =
                        sample_contact_material(&self.world, terrain_volume, point_cell, normal)
                    else {
                        continue;
                    };
                    events.push(ContactEvent {
                        target: EditTarget::Terrain,
                        target_volume: terrain_volume,
                        target_material,
                        point_cell,
                        normal,
                        impulse_n_s: contact.normal_impulse_n_s,
                        resting_impulse_n_s: mass * g * dt,
                        striker_born_this_tick: born_this_tick.contains(&striker.get()),
                    });
                }
                // Both sides dynamic: debris-on-debris (increment 3). Damage the
                // body being struck.
                (true, true) => {
                    let (pa, pb) = (contact.bodies[0], contact.bodies[1]);
                    let (Some(ba), Some(bb)) =
                        (self.world.body_by_phys(pa), self.world.body_by_phys(pb))
                    else {
                        continue;
                    };
                    let (Some(ea), Some(eb)) = (ba.entity, bb.entity) else {
                        continue;
                    };
                    debug_assert!(
                        !ba.dormant && !bb.dormant,
                        "a dormant body has no physics body, so it cannot appear in a live contact"
                    );
                    let sa = self.world.physics().body_state(pa);
                    let sb = self.world.physics().body_state(pb);
                    let (speed_a, speed_b) = (f64::from(sa.speed_m_s()), f64::from(sb.speed_m_s()));
                    // Struck = slower body; near-equal speeds -> lower mass;
                    // still equal -> lower entity id (fully deterministic).
                    let strike_a = if (speed_a - speed_b).abs() <= still_speed {
                        if (sa.mass_kg - sb.mass_kg).abs() <= f32::EPSILON {
                            ea.get() <= eb.get()
                        } else {
                            sa.mass_kg <= sb.mass_kg
                        }
                    } else {
                        speed_a < speed_b
                    };
                    let (struck_body, struck_entity, mass_struck) = if strike_a {
                        (ba, ea, sa.mass_kg)
                    } else {
                        (bb, eb, sb.mass_kg)
                    };
                    // World contact point -> the struck body's local cell frame,
                    // so the brush and the cooldown key travel with the body.
                    let point_cell = struck_body
                        .pose
                        .xform(struck_body.cell_size())
                        .world_to_local_cell(world_point)
                        .to_array();
                    let normal_target_to_striker = target_to_striker_normal(
                        contact.normal.map(f64::from),
                        if strike_a { 0 } else { 1 },
                    );
                    let normal_world = DVec3::from_array(normal_target_to_striker);
                    let normal_local = struck_body.pose.rotation.inverse() * normal_world;
                    let Some(target_material) = sample_contact_material(
                        &self.world,
                        struck_body.volume_id,
                        point_cell,
                        normal_local.to_array(),
                    ) else {
                        continue;
                    };
                    events.push(ContactEvent {
                        target: EditTarget::Body(struck_entity),
                        target_volume: struck_body.volume_id,
                        target_material,
                        point_cell,
                        normal: normal_target_to_striker,
                        impulse_n_s: contact.normal_impulse_n_s,
                        resting_impulse_n_s: mass_struck * g * dt,
                        // A body split out this tick is at its split instant, not
                        // a real impact — guard on either side of the pair.
                        striker_born_this_tick: born_this_tick.contains(&ea.get())
                            || born_this_tick.contains(&eb.get()),
                    });
                }
                // Only terrain is Fixed, so a fixed/fixed pair cannot occur.
                (false, false) => continue,
            }
        }

        let plan = policy.plan(self.tick.get(), &events);
        let actor = EntityId::new(CONTACT_DAMAGE_ACTOR_ID).expect("non-zero reserved actor id");
        for cut in &plan.damage {
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let mut intent = EditIntent::cut(request_id, actor, cut.target, cut.brush);
            if let Some(explosion) = cut.explosion {
                intent = intent.with_explosion(explosion);
            }
            // Route through `submit` so the dormancy-wake guard covers a
            // body-targeted cut uniformly with client edits (a struck body is
            // live here, so this is a no-op in practice — but consistent).
            match self.submit(intent) {
                Ok(_) => out.submitted += 1,
                Err(_) => out.rejected += 1,
            }
        }
        out.plan = plan;
        out
    }

    /// Runs the T21 region-dormancy policy for the current tick and applies its
    /// decisions: settled detached bodies with a quiet interaction region are
    /// deactivated (dropped from the physics step, record kept), and dormant
    /// bodies a player or an awake body has approached are reactivated.
    ///
    /// Opt-in, like [`Self::apply_contact_damage`]: the integrator owns the
    /// [`DormancyPolicy`] and the cadence, and it is *not* run by [`Self::tick`].
    /// Dormancy never changes authoritative geometry, ownership, or damage, so
    /// [`SimWorld::world_hash`](crate::world::SimWorld::world_hash),
    /// conservation, and the checkpoint set are unaffected. An edit that targets
    /// a dormant body still wakes it immediately through [`Self::submit`],
    /// independent of this pass.
    ///
    /// Pass the [`TickReport`] from the matching [`Self::tick`]: a **terrain**
    /// transaction committed this tick that lands within
    /// [`DormancyConfig::wake_margin_m`](crate::dormancy::DormancyConfig) of a
    /// dormant body hard-wakes it — the ground a settled body rests on just
    /// changed, so it must not wait out the hysteresis window
    /// (`docs/architecture.md`: "nearby edits wake affected neighbors").
    pub fn apply_dormancy(
        &mut self,
        policy: &mut DormancyPolicy,
        report: &TickReport,
    ) -> DormancyPlan {
        if self.world.body_count() == 0 {
            return DormancyPlan::default();
        }

        // Active interaction regions: every player capsule, plus every body that
        // is genuinely moving (an approaching / rolling piece — "active body
        // trajectories", docs/architecture.md).
        let still_speed = policy.config().still_speed_m_s;
        let mut regions: Vec<ActiveRegion> = Vec::new();
        for player in self.world.players() {
            let (lo, hi) = player.capsule_aabb_m();
            let centre = [
                0.5 * (lo[0] + hi[0]),
                0.5 * (lo[1] + hi[1]),
                0.5 * (lo[2] + hi[2]),
            ];
            let radius = 0.5
                * ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2))
                    .sqrt();
            regions.push(ActiveRegion {
                centre_m: centre,
                radius_m: radius,
            });
        }
        for body in self.world.bodies() {
            if body.dormant {
                continue;
            }
            let speed = {
                let v = body.linvel_m_s;
                (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
            };
            if speed > still_speed {
                let (centre_m, radius_m) = body.world_bounding_sphere();
                regions.push(ActiveRegion { centre_m, radius_m });
            }
        }

        // Terrain transactions committed this tick: their world boxes hard-wake
        // any dormant body resting within `wake_margin_m` of the cut (increment
        // 3). Order-independent — a body is woken iff *any* box is close enough.
        let wake_margin_m = policy.config().wake_margin_m;
        let terrain = self.world.terrain_volume_id();
        let terrain_cell_m = self.world.terrain().cell_size().metres();
        let terrain_edit_boxes: Vec<([f64; 3], [f64; 3])> = report
            .committed
            .iter()
            .filter_map(|(_, c)| transaction_world_box(&c.topology, terrain, terrain_cell_m))
            .collect();

        let inputs: Vec<BodyDormancyInput> = self
            .world
            .bodies()
            .filter_map(|body| {
                let entity = body.entity?;
                let (centre_m, radius_m) = body.world_bounding_sphere();
                let speed = {
                    let v = body.linvel_m_s;
                    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
                };
                // A body-targeted edit wakes its target through `submit` before
                // it is ever staged, so a dormant body never has a pending edit
                // against it here. A *terrain* cut under settled rubble is the
                // remaining case: hard-wake it so the support change is honoured
                // this tick, not after the hysteresis window.
                let hard_wake = body.dormant
                    && terrain_edit_boxes
                        .iter()
                        .any(|b| box_sphere_gap(*b, centre_m, radius_m) <= wake_margin_m);
                Some(BodyDormancyInput {
                    entity,
                    centre_m,
                    radius_m,
                    sleeping: body.sleeping,
                    speed_m_s: speed,
                    dormant: body.dormant,
                    hard_wake,
                })
            })
            .collect();

        let plan = policy.plan(self.tick.get(), &inputs, &regions);
        // A body with a queued or staged edit stays live until that edit commits (the commit
        // rebuilds its collider); it is retried on a later tick.
        let targeted = self.pipeline.targeted_bodies();
        for &entity in &plan.deactivate {
            if targeted.contains(&entity) {
                continue;
            }
            self.world.deactivate_body(entity);
        }
        for &entity in &plan.reactivate {
            self.world.reactivate_body(entity);
        }
        plan
    }
}
