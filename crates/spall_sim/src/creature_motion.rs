//! Spawning, despawning and moving creatures.
//!
//! The game decides where a creature wants to go. The engine sweeps a capsule
//! around it against fixed terrain (the same kinematic controller players use:
//! wall slide, autostep over terrain stairs, ground snap) and keeps the resulting
//! root pose. Presentation state (locomotion selector, clip phase) is stored with
//! the pose so the server can publish it.
use super::*;
use spall_protocol::{
    CreatureMotionEntry, CreatureMotionFrame, CreatureRecord, WIRE_SCHEMA_VERSION,
};

use crate::creatures::{CreatureHost, CreatureHostInitError, CreatureSpawnError};

/// A flying creature's step: the capsule is swept from where it is to the target.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatureMoveRequest {
    pub creature_id: u64,
    /// Desired root translation in world metres.
    pub target_translation_m: [f64; 3],
    pub target_rotation_xyzw: [f32; 4],
    pub locomotion_tag: u8,
    pub clip_phase_tick: u64,
    /// Vertical offset from the root to the capsule's lowest point (negative), metres.
    pub feet_offset_m: f32,
    pub capsule_radius_m: f32,
    /// Half the height of the capsule's cylindrical segment.
    pub capsule_half_height_m: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreatureMoveOutcome {
    /// The creature reached the target.
    Moved,
    /// Terrain blocks the path; nothing changed.
    Blocked,
}

/// A ground creature's step, resolved with a character capsule around it.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatureWalkRequest {
    pub creature_id: u64,
    /// Desired horizontal step this update, metres (x, z).
    pub desired_step_m: [f32; 2],
    pub target_rotation_xyzw: [f32; 4],
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

/// Current root pose of a creature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreaturePose {
    pub creature_id: u64,
    pub translation_m: [f64; 3],
    pub rotation_xyzw: [f32; 4],
}

/// A flying step that the sweep shortens by more than this fraction is blocked.
const BLOCKED_FRACTION: f64 = 0.01;

impl Simulation {
    /// Installs an empty creature host.
    pub fn initialize_creature_host(&mut self) -> Result<(), CreatureHostInitError> {
        if self.creatures.is_some() {
            return Err(CreatureHostInitError::AlreadyInitialized);
        }
        self.creatures = Some(CreatureHost::default());
        Ok(())
    }

    /// Whether a creature host is installed. A world with animals cannot be saved.
    pub fn has_creature_host(&self) -> bool {
        self.creatures.is_some()
    }

    /// The first creature id not yet used, so a director can plan a spawn batch.
    pub fn next_creature_id(&self) -> Option<u64> {
        self.creatures.as_ref().map(CreatureHost::next_creature_id)
    }

    /// Spawns every record, or none. Ids must be unused and ascending.
    pub fn spawn_creatures(
        &mut self,
        records: Vec<CreatureRecord>,
    ) -> Result<(), CreatureSpawnError> {
        let host = self.creatures.as_mut().ok_or(CreatureSpawnError::NoHost)?;
        let mut candidate = host.clone();
        for record in records {
            candidate.spawn(record, self.tick.get())?;
        }
        *host = candidate;
        Ok(())
    }

    /// Removes a creature. `false` if it was not there.
    pub fn despawn_creature(&mut self, creature_id: u64) -> bool {
        self.creatures
            .as_mut()
            .is_some_and(|host| host.despawn(creature_id))
    }

    /// Every creature as it is now, sorted by id. This is the join list.
    pub fn creature_records(&self) -> Vec<CreatureRecord> {
        self.creatures
            .as_ref()
            .map(|host| host.iter().map(|c| c.record.clone()).collect())
            .unwrap_or_default()
    }

    /// Every creature's root pose.
    pub fn creature_poses(&self) -> Vec<CreaturePose> {
        let Some(host) = self.creatures.as_ref() else {
            return Vec::new();
        };
        host.iter()
            .map(|creature| CreaturePose {
                creature_id: creature.record.creature_id,
                translation_m: creature.record.translation_m,
                rotation_xyzw: creature.record.rotation_xyzw,
            })
            .collect()
    }

    /// Moves a walking creature with a character capsule (see
    /// [`CreatureWalkRequest`]).
    pub fn walk_creature(
        &mut self,
        request: CreatureWalkRequest,
    ) -> Result<CreatureWalkOutcome, String> {
        let here = self.creature_root(request.creature_id)?;
        let feet = [here[0], here[1] + f64::from(request.feet_offset_m), here[2]];
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
        let moved = swept.translation_m.map(f64::from);
        self.apply_creature_move(
            request.creature_id,
            [here[0] + moved[0], here[1] + moved[1], here[2] + moved[2]],
            request.target_rotation_xyzw,
            request.locomotion_tag,
            request.clip_phase_tick,
        )?;
        Ok(CreatureWalkOutcome {
            moved_m: moved,
            grounded: swept.grounded,
        })
    }

    /// Moves a flying creature straight to its target if the capsule fits along
    /// the way; otherwise leaves it where it is.
    pub fn move_creature(
        &mut self,
        request: CreatureMoveRequest,
    ) -> Result<CreatureMoveOutcome, String> {
        let here = self.creature_root(request.creature_id)?;
        let feet = [here[0], here[1] + f64::from(request.feet_offset_m), here[2]];
        let wanted = [
            request.target_translation_m[0] - here[0],
            request.target_translation_m[1] - here[1],
            request.target_translation_m[2] - here[2],
        ];
        let swept = self.world.physics().sweep_character(
            spall_physics::CharacterParams {
                half_height_m: request.capsule_half_height_m,
                radius_m: request.capsule_radius_m,
                mass_kg: 1.0,
            },
            feet,
            wanted.map(|v| v as f32),
            0.1,
        );
        let wanted_len = wanted.iter().map(|v| v * v).sum::<f64>().sqrt();
        let got = swept.translation_m.map(f64::from);
        let off = (0..3)
            .map(|i| (got[i] - wanted[i]).powi(2))
            .sum::<f64>()
            .sqrt();
        if off > wanted_len * BLOCKED_FRACTION + 1.0e-4 {
            return Ok(CreatureMoveOutcome::Blocked);
        }
        self.apply_creature_move(
            request.creature_id,
            request.target_translation_m,
            request.target_rotation_xyzw,
            request.locomotion_tag,
            request.clip_phase_tick,
        )?;
        Ok(CreatureMoveOutcome::Moved)
    }

    fn creature_root(&self, creature_id: u64) -> Result<[f64; 3], String> {
        self.creatures
            .as_ref()
            .and_then(|host| host.get(creature_id))
            .map(|creature| creature.record.translation_m)
            .ok_or_else(|| "creature is not hosted".to_owned())
    }

    fn apply_creature_move(
        &mut self,
        creature_id: u64,
        translation_m: [f64; 3],
        rotation_xyzw: [f32; 4],
        locomotion_tag: u8,
        clip_phase_tick: u64,
    ) -> Result<(), String> {
        let tick = self.tick.get();
        let creature = self
            .creatures
            .as_mut()
            .and_then(|host| host.get_mut(creature_id))
            .ok_or("creature is not hosted")?;
        let elapsed_s = (tick.saturating_sub(creature.moved_at_tick).max(1) as f64)
            / f64::from(1.0 / TICK_DT_S);
        let before = creature.record.translation_m;
        creature.velocity_m_s = [
            ((translation_m[0] - before[0]) / elapsed_s) as f32,
            ((translation_m[1] - before[1]) / elapsed_s) as f32,
            ((translation_m[2] - before[2]) / elapsed_s) as f32,
        ];
        creature.moved_at_tick = tick;
        creature.record.translation_m = translation_m;
        creature.record.rotation_xyzw = rotation_xyzw;
        creature.record.locomotion_tag = locomotion_tag;
        creature.record.clip_phase_tick = clip_phase_tick;
        Ok(())
    }

    /// Motion entries for creatures whose root satisfies `include`, sorted by
    /// creature id. `None` when there is nothing to send.
    pub fn creature_motion_frame(
        &self,
        include: &dyn Fn([f64; 3]) -> bool,
    ) -> Option<CreatureMotionFrame> {
        let host = self.creatures.as_ref()?;
        let entries: Vec<CreatureMotionEntry> = host
            .iter()
            .filter(|creature| include(creature.record.translation_m))
            .map(|creature| CreatureMotionEntry {
                creature_id: creature.record.creature_id,
                translation_m: creature.record.translation_m,
                rotation_xyzw: creature.record.rotation_xyzw,
                linear_velocity_m_s: creature.velocity_m_s,
                locomotion_tag: creature.record.locomotion_tag,
                clip_phase_tick: creature.record.clip_phase_tick,
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        Some(CreatureMotionFrame {
            schema: WIRE_SCHEMA_VERSION,
            server_tick: self.tick.get(),
            entries,
        })
    }
}
