//! Authoritative kinematic movement of alive creatures.
//!
//! The game decides where a creature wants to go and supplies the sampled part
//! pose for this step; the engine sweeps that complete pose and root target
//! against fixed terrain through the physics articulated-transition API and
//! only then lets it stand. A blocked sample changes nothing. Presentation
//! state (locomotion selector, clip phase) is remembered per creature so the
//! server can publish it with the physically applied root; it is not part of
//! the ownership record and is not persisted.
use super::*;
use spall_physics::ArticulatedPose;
use spall_protocol::{
    CreatureLifecycleState, CreatureMotionEntry, CreatureMotionFrame, WIRE_SCHEMA_VERSION,
};

#[derive(Debug, Clone, PartialEq)]
pub struct CreatureMoveRequest {
    pub key: spall_core::CreatureKey,
    /// Desired root translation in world metres.
    pub target_translation_m: [f64; 3],
    pub target_rotation_xyzw: [f32; 4],
    /// The complete sampled local pose of every source part, in part-id order.
    pub parts: Vec<(u32, spall_physics::PartTransform)>,
    pub locomotion_tag: u8,
    pub clip_phase_tick: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreatureMoveOutcome {
    /// The pose and root were staged for the next physics step.
    Staged,
    /// Terrain blocks the swept path; nothing changed.
    Blocked {
        part_id: Option<u32>,
        time_of_impact: Option<f32>,
    },
    /// A previous pose has not been installed yet; try again next tick.
    Busy,
}

/// A ground creature's step, resolved with a character capsule around it.
///
/// The capsule is the same kinematic character controller players use (wall
/// slide, autostep over terrain stairs, ground snap). Its result moves the root;
/// the complete part pose is then staged without per-cell terrain casts, so
/// limbs may overlap terrain the capsule does not cover. This is the locomotion
/// collision proxy for walking animals; flying creatures use the exact swept path.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatureWalkRequest {
    pub key: spall_core::CreatureKey,
    /// Desired horizontal step this update, metres (x, z).
    pub desired_step_m: [f32; 2],
    pub target_rotation_xyzw: [f32; 4],
    /// The complete sampled local pose of every source part, in part-id order.
    pub parts: Vec<(u32, spall_physics::PartTransform)>,
    pub locomotion_tag: u8,
    pub clip_phase_tick: u64,
    /// Vertical offset from the root to the lowest point the creature stands on
    /// (negative), metres.
    pub feet_offset_m: f32,
    pub capsule_radius_m: f32,
    /// Half the height of the capsule's cylindrical segment.
    pub capsule_half_height_m: f32,
}

/// What a walk step achieved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreatureWalkOutcome {
    /// Root translation applied this step, metres.
    pub moved_m: [f64; 3],
    pub grounded: bool,
}

/// Where the swept-transition time of recent moves went, summed since the last
/// [`Simulation::take_creature_move_timing`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CreatureMoveTiming {
    pub moves: u64,
    pub blocked: u64,
    pub terrain_candidates: u64,
    pub cell_casts: u64,
    pub query_sync: std::time::Duration,
    pub candidate_query: std::time::Duration,
    pub exact_cast: std::time::Duration,
}

/// Current applied root of an alive creature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AliveCreature {
    pub key: spall_core::CreatureKey,
    pub translation_m: [f64; 3],
    pub rotation_xyzw: [f32; 4],
}

impl Simulation {
    /// Moves a walking creature with a character capsule (see
    /// [`CreatureWalkRequest`]). `Ok(None)` means a previous pose has not been
    /// installed yet; try again next tick.
    pub fn walk_creature(
        &mut self,
        request: CreatureWalkRequest,
    ) -> Result<Option<CreatureWalkOutcome>, String> {
        let state = self
            .creatures
            .as_ref()
            .and_then(|host| host.record(request.key.creature_id))
            .ok_or("creature is not hosted")?;
        if state.key != request.key || state.lifecycle != CreatureLifecycleState::Alive {
            return Err("creature is not alive".into());
        }
        let owner = self
            .world
            .creature_physics_owner(request.key)
            .ok_or("creature has no physical owner")?;
        let snapshot = self
            .world
            .physics()
            .articulated_owner_snapshot(owner)
            .ok_or("creature owner is missing from physics")?;
        if snapshot.pending_pose {
            return Ok(None);
        }
        let origin = self.world.physics_origin();
        let root = origin.to_world_f64(snapshot.root.translation_m);
        let feet = [root[0], root[1] + f64::from(request.feet_offset_m), root[2]];
        // Fall a little each step so the capsule follows the ground down.
        const FALL_M: f32 = 0.3;
        let swept = self.world.physics().sweep_character(
            spall_physics::CharacterParams {
                half_height_m: request.capsule_half_height_m,
                radius_m: request.capsule_radius_m,
                mass_kg: 60.0,
            },
            feet,
            [
                request.desired_step_m[0],
                -FALL_M,
                request.desired_step_m[1],
            ],
            0.1,
        );
        let moved = [
            f64::from(swept.translation_m[0]),
            f64::from(swept.translation_m[1]),
            f64::from(swept.translation_m[2]),
        ];
        let target_world = [root[0] + moved[0], root[1] + moved[1], root[2] + moved[2]];
        let target = origin
            .to_local_f32(target_world)
            .ok_or("target is outside the physics frame")?;
        let update = ArticulatedPose {
            topology_token: snapshot.topology_token,
            base_pose_token: snapshot.pose_token,
            pose_token: snapshot
                .pose_token
                .checked_add(1)
                .ok_or("pose token exhausted")?,
            parts: request.parts,
        };
        self.world
            .physics_mut()
            .update_articulated_pose_trusted(
                owner,
                update,
                spall_physics::PartTransform {
                    translation_m: target,
                    rotation_xyzw: request.target_rotation_xyzw,
                },
            )
            .map_err(|error| error.to_string())?;
        self.creature_presentation.insert(
            request.key,
            (request.locomotion_tag, request.clip_phase_tick),
        );
        Ok(Some(CreatureWalkOutcome {
            moved_m: moved,
            grounded: swept.grounded,
        }))
    }

    /// Returns and clears the accumulated swept-transition timing.
    pub fn take_creature_move_timing(&mut self) -> CreatureMoveTiming {
        std::mem::take(&mut self.creature_move_timing)
    }

    /// Every alive creature with its physically applied root pose.
    pub fn alive_creatures(&self) -> Vec<AliveCreature> {
        let Some(host) = self.creatures.as_ref() else {
            return Vec::new();
        };
        host.records()
            .filter(|state| state.lifecycle == CreatureLifecycleState::Alive)
            .filter_map(|state| {
                let owner = self.world.creature_physics_owner(state.key)?;
                let snapshot = self.world.physics().articulated_owner_snapshot(owner)?;
                Some(AliveCreature {
                    key: state.key,
                    translation_m: self
                        .world
                        .physics_origin()
                        .to_world_f64(snapshot.root.translation_m),
                    rotation_xyzw: snapshot.root.rotation,
                })
            })
            .collect()
    }

    pub fn move_creature(
        &mut self,
        request: CreatureMoveRequest,
    ) -> Result<CreatureMoveOutcome, String> {
        let state = self
            .creatures
            .as_ref()
            .and_then(|host| host.record(request.key.creature_id))
            .ok_or("creature is not hosted")?;
        if state.key != request.key || state.lifecycle != CreatureLifecycleState::Alive {
            return Err("creature is not alive".into());
        }
        let owner = self
            .world
            .creature_physics_owner(request.key)
            .ok_or("creature has no physical owner")?;
        let snapshot = self
            .world
            .physics()
            .articulated_owner_snapshot(owner)
            .ok_or("creature owner is missing from physics")?;
        if snapshot.pending_pose {
            return Ok(CreatureMoveOutcome::Busy);
        }
        let target = self
            .world
            .physics_origin()
            .to_local_f32(request.target_translation_m)
            .ok_or("target is outside the physics frame")?;
        let update = ArticulatedPose {
            topology_token: snapshot.topology_token,
            base_pose_token: snapshot.pose_token,
            pose_token: snapshot
                .pose_token
                .checked_add(1)
                .ok_or("pose token exhausted")?,
            parts: request.parts,
        };
        let transition = self
            .world
            .physics_mut()
            .update_articulated_pose_with_swept_root_pose(
                owner,
                update,
                spall_physics::PartTransform {
                    translation_m: target,
                    rotation_xyzw: request.target_rotation_xyzw,
                },
            )
            .map_err(|error| error.to_string())?;
        let timing = &mut self.creature_move_timing;
        timing.moves += 1;
        timing.terrain_candidates += transition.terrain_candidates as u64;
        timing.cell_casts += transition.cell_casts as u64;
        timing.query_sync += transition.query_sync_time;
        timing.candidate_query += transition.candidate_query_time;
        timing.exact_cast += transition.exact_cast_time;
        if transition.blocked {
            timing.blocked += 1;
            return Ok(CreatureMoveOutcome::Blocked {
                part_id: transition.part_id,
                time_of_impact: transition.time_of_impact,
            });
        }
        self.creature_presentation.insert(
            request.key,
            (request.locomotion_tag, request.clip_phase_tick),
        );
        Ok(CreatureMoveOutcome::Staged)
    }

    /// Motion entries for alive creatures whose root satisfies `include`,
    /// sorted by creature id. `None` when there is nothing to send.
    pub fn creature_motion_frame(
        &self,
        include: &dyn Fn([f64; 3]) -> bool,
    ) -> Option<CreatureMotionFrame> {
        let host = self.creatures.as_ref()?;
        let mut entries: Vec<CreatureMotionEntry> = host
            .records()
            .filter(|state| state.lifecycle == CreatureLifecycleState::Alive)
            .filter_map(|state| {
                let owner = self.world.creature_physics_owner(state.key)?;
                let root = self.world.physics().articulated_owner_snapshot(owner)?.root;
                let translation_m = self.world.physics_origin().to_world_f64(root.translation_m);
                if !include(translation_m) {
                    return None;
                }
                let (locomotion_tag, clip_phase_tick) = self
                    .creature_presentation
                    .get(&state.key)
                    .copied()
                    .unwrap_or((state.locomotion_tag, state.clip_phase_tick));
                Some(CreatureMotionEntry {
                    creature_id: state.key.creature_id.get(),
                    translation_m,
                    rotation_xyzw: root.rotation,
                    linear_velocity_m_s: root.linvel_m_s,
                    locomotion_tag,
                    clip_phase_tick,
                })
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        entries.sort_by_key(|entry| entry.creature_id);
        let world_epoch = self.creature_allocator_snapshot()?.world_epoch;
        Some(CreatureMotionFrame {
            schema: WIRE_SCHEMA_VERSION,
            world_epoch,
            server_tick: self.tick.get(),
            entries,
        })
    }
}
