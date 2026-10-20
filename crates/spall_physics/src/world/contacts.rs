use super::*;

impl PhysicsWorld {
    /// Diagnostic (wake-burst attribution): for each id in `ids`, the *other dynamic*
    /// bodies it currently has an active contact manifold with, as
    /// `(queried, partner)` pairs. Read-only; builds
    /// a handle -> id map per call, so call it only for bursts.
    pub fn touching_dynamic_bodies(&self, ids: &[BodyId]) -> Vec<(BodyId, BodyId)> {
        let by_handle: std::collections::HashMap<RigidBodyHandle, BodyId> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.retired && !e.dormant)
            .map(|(i, e)| {
                (
                    e.body,
                    BodyId((u64::from(self.id_namespace) << 32) | i as u64),
                )
            })
            .collect();
        let mut out = Vec::new();
        for &id in ids {
            let entry = &self.entries[entry_index(id, self.id_namespace)];
            if entry.retired || entry.dormant {
                continue;
            }
            for &c in self.bodies[entry.body].colliders() {
                for pair in self.narrow_phase.contact_pairs_with(c) {
                    if !pair.has_any_active_contact() {
                        continue;
                    }
                    let other = if pair.collider1 == c {
                        pair.collider2
                    } else {
                        pair.collider1
                    };
                    let Some(parent) = self.colliders.get(other).and_then(|o| o.parent()) else {
                        continue;
                    };
                    if !self.bodies[parent].is_dynamic() {
                        continue;
                    }
                    if let Some(&pid) = by_handle.get(&parent) {
                        out.push((id, pid));
                    }
                }
            }
        }
        out
    }

    /// Number of narrow-phase contact pairs currently tracked.
    pub fn contact_pair_count(&self) -> usize {
        self.narrow_phase.contact_pairs().count()
    }

    /// Deepest current contact penetration across all pairs, metres (0 if none).
    pub fn max_penetration_m(&self) -> f32 {
        let mut worst = 0.0_f32;
        for pair in self.narrow_phase.contact_pairs() {
            for manifold in &pair.manifolds {
                for point in &manifold.points {
                    if point.dist < 0.0 {
                        worst = worst.max(-point.dist);
                    }
                }
            }
        }
        worst
    }

    /// The [`RigidBodyHandle`] for `id`, mapped back to a [`BodyId`], if `id` is
    /// a live (non-retired) body this world owns.
    fn body_id_of(&self, handle: RigidBodyHandle) -> Option<BodyId> {
        self.entries
            .iter()
            .position(|e| !e.retired && e.body == handle)
            .map(|i| BodyId((u64::from(self.id_namespace) << 32) | i as u64))
            .or_else(|| {
                self.articulated.iter().find_map(|(id, assembly)| {
                    (assembly.kinematic_part_bodies
                        || assembly
                            .parts
                            .iter()
                            .any(|part| part.component_id.is_some()))
                    .then(|| assembly.parts.iter().any(|part| part.body == handle))
                    .filter(|owned| *owned)
                    .map(|_| *id)
                })
            })
    }

    fn source_part_of(&self, body: BodyId, collider: ColliderHandle) -> Option<u32> {
        self.articulated.get(&body).and_then(|assembly| {
            assembly
                .parts
                .iter()
                .find(|part| part.collider == collider)
                .map(|part| part.part_id)
        })
    }

    /// Copies solver-point contact data before the next global solver substep
    /// can recycle or discard it. The returned records contain no Rapier
    /// handles; published query pose tokens are updated only after the complete
    /// outer step.
    pub(super) fn capture_articulated_contact_witnesses(
        &mut self,
        substep: usize,
        substeps: usize,
    ) {
        let mut captured = Vec::new();
        let room =
            MAX_CONTACT_WITNESSES_PER_STEP.saturating_sub(self.articulated_contact_witnesses.len());
        let mut overflow = 0_u64;
        let fraction = (substep + 1) as f32 / substeps as f32;
        for pair in self.narrow_phase.contact_pairs() {
            if !pair.has_any_active_contact() {
                continue;
            }
            for manifold in pair.solver_manifolds() {
                let (Some(rb1), Some(rb2)) = (manifold.data.rigid_body1, manifold.data.rigid_body2)
                else {
                    continue;
                };
                let (Some(body1), Some(body2)) = (self.body_id_of(rb1), self.body_id_of(rb2))
                else {
                    continue;
                };
                let Some(collider1) = self.colliders.get(pair.collider1) else {
                    continue;
                };
                let Some(collider2) = self.colliders.get(pair.collider2) else {
                    continue;
                };
                let normal = manifold.data.normal;
                let normal_len = normal.length();
                if !normal_len.is_finite() || normal_len <= f32::EPSILON {
                    continue;
                }
                let normal = normal / normal_len;
                let pose1 = collider1.position();
                let pose2 = collider2.position();
                for contact in &manifold.data.solver_contacts {
                    let [contact_index] = contact.contact_indices();
                    let Some(point) = manifold.points.get(contact_index as usize) else {
                        continue;
                    };
                    // Solver anchors are COM-local and Rapier exposes the
                    // matching world witness transform. This remains correct
                    // for collider offsets and clustered compound subshapes.
                    let (p1, p2) = manifold
                        .data
                        .solver_contact_world_points(contact, &self.bodies);
                    let source_local1 = pose1.rotation.inverse() * (p1 - pose1.translation);
                    let source_local2 = pose2.rotation.inverse() * (p2 - pose2.translation);
                    if ![p1.x, p1.y, p1.z, p2.x, p2.y, p2.z, point.dist]
                        .iter()
                        .all(|value| value.is_finite())
                    {
                        continue;
                    }
                    let impulse = point.data.impulse;
                    let impulse = (impulse.is_finite() && impulse >= 0.0).then_some(impulse);
                    let source_sides = [
                        self.source_contact_witness(
                            body1,
                            pair.collider1,
                            source_local1,
                            p1,
                            normal,
                            point.dist,
                            impulse,
                        ),
                        self.source_contact_witness(
                            body2,
                            pair.collider2,
                            source_local2,
                            p2,
                            -normal,
                            point.dist,
                            impulse,
                        ),
                    ];
                    if source_sides.iter().all(Option::is_none) {
                        continue;
                    }
                    let record = ArticulatedContactWitness {
                        version: CONTACT_WITNESS_VERSION,
                        outer_step: self.step_count + 1,
                        solver_substep: substep as u16,
                        substep_fraction: fraction,
                        bodies: [body1, body2],
                        world_contact_point_m: [
                            0.5 * (p1.x + p2.x),
                            0.5 * (p1.y + p2.y),
                            0.5 * (p1.z + p2.z),
                        ],
                        source_sides,
                    };
                    if captured.len() < room {
                        captured.push(record);
                    } else {
                        overflow += 1;
                    }
                }
            }
        }
        self.contact_witness_overflow += overflow;
        self.articulated_contact_witnesses.extend(captured);
    }

    fn source_contact_witness(
        &self,
        body: BodyId,
        collider_handle: ColliderHandle,
        local_point: Vector,
        world_point: Vector,
        outward_world: Vector,
        separation_m: f32,
        impulse: Option<f32>,
    ) -> Option<SourceContactWitness> {
        let assembly = self.articulated.get(&body)?;
        let part = assembly
            .parts
            .iter()
            .find(|part| part.collider == collider_handle)?;
        let collider = self.colliders.get(collider_handle)?;
        let collider_pose = collider.position();
        let local_normal = collider_pose.rotation.inverse() * outward_world;
        let attribution = resolve_source_cell_witness(
            &part.grid,
            part.cell_m,
            local_point,
            local_normal,
            separation_m,
        );
        Some(SourceContactWitness {
            part_id: part.part_id,
            volume_id: part.volume_id,
            source_revision: part.source_revision,
            topology_token: assembly.topology_token,
            pose_token: assembly
                .pending_pose
                .as_ref()
                .map(|pending| pending.pose_token)
                .or(assembly.pending_root_pose_token)
                .unwrap_or(assembly.pose_token),
            source_witness_world_m: [world_point.x, world_point.y, world_point.z],
            source_outward_normal_world: [outward_world.x, outward_world.y, outward_world.z],
            separation_m,
            normal_impulse_n_s: impulse,
            attribution,
        })
    }

    /// Geometric witness records retained from the last completed outer step.
    /// Their authored pose token and substep fraction identify the source pose
    /// that generated them; the records do not mutate or replace ray-query
    /// provenance.
    pub fn articulated_contact_witnesses(&self) -> &[ArticulatedContactWitness] {
        &self.articulated_contact_witnesses
    }

    /// Number of source-bearing solver points omitted when the per-step
    /// witness cap was reached.
    pub fn contact_witness_overflow(&self) -> u64 {
        self.contact_witness_overflow
    }

    /// Every contact pair with a non-zero solved normal impulse this step, as
    /// [`ContactImpulse`] records in a deterministic order (by body-id pair).
    /// This is the T21 input surface: the caller thresholds these, applies
    /// per-region cooldowns, and converts survivors into bounded edit intents
    /// for a *later* tick — nothing here mutates world state.
    ///
    /// Call after [`Self::step`]. Pairs with no active solver contact, pairs
    /// touching a retired or world-detached body, and pairs whose accumulated
    /// normal impulse is not positive are omitted.
    pub fn contact_impulses(&self) -> Vec<ContactImpulse> {
        let mut out: Vec<ContactImpulse> = Vec::new();
        for pair in self.narrow_phase.contact_pairs() {
            if !pair.has_any_active_contact() {
                continue;
            }
            let impulse = pair.total_impulse_magnitude();
            if impulse <= 0.0 || !impulse.is_finite() {
                continue;
            }

            // Body handles + world contact normal live on the manifold data.
            let Some(manifold) = pair.manifolds.first() else {
                continue;
            };
            let (Some(rb1), Some(rb2)) = (manifold.data.rigid_body1, manifold.data.rigid_body2)
            else {
                continue;
            };
            let (Some(b1), Some(b2)) = (self.body_id_of(rb1), self.body_id_of(rb2)) else {
                continue;
            };
            let n = manifold.data.normal;
            let nlen = (n.x * n.x + n.y * n.y + n.z * n.z).sqrt();
            let normal = if nlen > f32::EPSILON {
                [n.x / nlen, n.y / nlen, n.z / nlen]
            } else {
                [0.0, 1.0, 0.0]
            };

            // Mean of every manifold contact point. In the pipeline's manifolds
            // `local_p1` / `local_p2` are the touch points on each body's surface
            // expressed relative to that body's centre of mass, in its local
            // frame; lift both to world space (world COM + body rotation · point)
            // and average — a stable brush centre for the hit.
            // A pair can outlive a body removed since the last step (a retired debris body):
            // skip it instead of indexing a stale handle.
            let (Some(rb1), Some(rb2)) = (self.bodies.get(rb1), self.bodies.get(rb2)) else {
                continue;
            };
            let (com1, rot1) = (rb1.center_of_mass(), *rb1.rotation());
            let (com2, rot2) = (rb2.center_of_mass(), *rb2.rotation());
            let mut point_sum = [0.0_f64; 3];
            let mut point_n = 0.0_f64;
            for m in &pair.manifolds {
                for p in &m.points {
                    let w1 = com1 + rot1 * p.local_p1;
                    let w2 = com2 + rot2 * p.local_p2;
                    point_sum[0] += 0.5 * f64::from(w1.x + w2.x);
                    point_sum[1] += 0.5 * f64::from(w1.y + w2.y);
                    point_sum[2] += 0.5 * f64::from(w1.z + w2.z);
                    point_n += 1.0;
                }
            }
            if point_n == 0.0 {
                continue;
            }
            let inv = 1.0 / point_n;
            out.push(ContactImpulse {
                bodies: [b1, b2],
                source_parts: [
                    self.source_part_of(b1, pair.collider1),
                    self.source_part_of(b2, pair.collider2),
                ],
                dynamic: [
                    self.bodies[self.entries[entry_index(b1, self.id_namespace)].body].is_dynamic(),
                    self.bodies[self.entries[entry_index(b2, self.id_namespace)].body].is_dynamic(),
                ],
                point_m: [
                    (point_sum[0] * inv) as f32,
                    (point_sum[1] * inv) as f32,
                    (point_sum[2] * inv) as f32,
                ],
                normal,
                normal_impulse_n_s: impulse,
                source_cells: [None, None],
            });
        }
        out.sort_by_key(|c| {
            (
                c.bodies[0].0.min(c.bodies[1].0),
                c.bodies[0].0.max(c.bodies[1].0),
            )
        });
        out
    }
}
